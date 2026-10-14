//! Live check of the hot path's decision against the slow path's, launch by launch: the entry
//! facts each assembled and the entry rules' verdict on them. Both paths note their side without
//! allocating, the hot path also the create's buys that reached it after its 250 us window
//! (`note_late`). The reporter thread (`flush`, every 10 s) compares the launches with both sides
//! in, writes one row each to `logs/hot_verify.csv`, counts them and logs every verdict
//! disagreement: nothing is compared or written on the hot threads.

use {
    crate::config::rules::LaunchFacts,
    log::warn,
    solana_pubkey::Pubkey,
    std::{
        io::Write,
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex,
        },
    },
};

pub const VERIFY_CSV: &str = "logs/hot_verify.csv";
const SLOTS: usize = 8192;
/// A launch is compared once both sides are in and this long has passed (its late buys are in).
const SETTLE_NS: u64 = 1_000_000_000;
const DISPLACED: usize = 256;

#[derive(Clone, Copy)]
struct Entry {
    mint: Pubkey,
    at: u64,
    fast: Option<(Option<LaunchFacts>, bool)>,
    slow: Option<(LaunchFacts, bool)>,
    /// Buys of the create's slot that reached the hot path after its window: (count, lamports).
    late: Option<(u32, u64)>,
    reported: bool,
}

struct Table {
    slots: Vec<Option<Entry>>,
    /// Entries pushed out by a newer coin before they were reported.
    displaced: [Option<Entry>; DISPLACED],
    n_displaced: usize,
}

/// Direct-mapped by mint: one lookup, no scan (a newer coin replaces an older one in its slot).
static TABLE: LazyLock<Mutex<Table>> =
    LazyLock::new(|| Mutex::new(Table { slots: vec![None; SLOTS], displaced: [None; DISPLACED], n_displaced: 0 }));

fn slot_of(mint: &Pubkey) -> usize {
    u64::from_le_bytes(mint.to_bytes()[..8].try_into().expect("8")) as usize & (SLOTS - 1)
}

pub mod counts {
    use std::sync::atomic::AtomicU64;
    pub static COMPARED: AtomicU64 = AtomicU64::new(0);
    pub static AGREE: AtomicU64 = AtomicU64::new(0);
    /// Same verdict, different facts.
    pub static FACTS_ONLY: AtomicU64 = AtomicU64::new(0);
    /// ... of which only the bundle facts differ and the hot path saw late buys.
    pub static FACTS_ONLY_LATE: AtomicU64 = AtomicU64::new(0);
    pub static VERDICT: AtomicU64 = AtomicU64::new(0);
    pub static VERDICT_LATE: AtomicU64 = AtomicU64::new(0);
}
pub use counts::{AGREE, COMPARED};

/// Builds the table now (startup), so the first launch does not.
pub fn init() {
    LazyLock::force(&TABLE);
}

fn with_entry(mint: &Pubkey, at: u64, f: impl FnOnce(&mut Entry)) {
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let i = slot_of(mint);
    match &mut t.slots[i] {
        Some(e) if e.mint == *mint => f(e),
        other => {
            let old = other.replace(Entry { mint: *mint, at, fast: None, slow: None, late: None, reported: false });
            if let Some(old) = old.filter(|o| !o.reported && o.fast.is_some() && o.slow.is_some()) {
                let k = t.n_displaced;
                if k < DISPLACED {
                    t.displaced[k] = Some(old);
                    t.n_displaced += 1;
                }
            }
            f(t.slots[i].as_mut().expect("just set"));
        }
    }
}

/// The hot path's side: the facts it decided on (None when it stopped before the entry rules) and
/// whether the entry rules passed. Allocation-free.
pub fn note_fast(mint: &Pubkey, facts: Option<LaunchFacts>, rules_ok: bool) {
    with_entry(mint, super::clock::now_ns(), |e| e.fast = Some((facts, rules_ok)));
}

/// The hot path's count of the create's buys that came after its window. Allocation-free.
pub fn note_late(mint: &Pubkey, buys: u32, lamports: u64) {
    with_entry(mint, super::clock::now_ns(), |e| e.late = Some((buys, lamports)));
}

/// The slow path's side, from `common::spawn_snipe`.
pub fn note_slow(mint: &Pubkey, facts: &LaunchFacts, rules_ok: bool) {
    with_entry(mint, super::clock::now_ns(), |e| e.slow = Some((*facts, rules_ok)));
}

/// The fields in which two sets of facts differ.
pub fn differing(a: &LaunchFacts, b: &LaunchFacts) -> Vec<&'static str> {
    let mut out = Vec::new();
    macro_rules! cmp {
        ($($f:ident),*) => { $( if a.$f != b.$f { out.push(stringify!($f)); } )* };
    }
    cmp!(
        mayhem,
        dev_buy_lamports,
        deployer_lamports,
        follow,
        reference_profit_lamports,
        reference_trades,
        insider_lamports,
        risky_host,
        secs_since_previous_launch,
        launches_last_day,
        bundle_wallets,
        name_all_caps,
        deployer_record
    );
    out
}

/// What one comparison found.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub agree: bool,
    pub verdict_differs: bool,
    pub fields: Vec<&'static str>,
    /// Only the bundle's facts differ (insider SOL, bundle wallets) and the hot path saw buys of
    /// the create's slot after its window: the slow path counted buys the hot path got too late.
    pub late_explains: bool,
}

pub fn judge(fast: (Option<LaunchFacts>, bool), slow: (LaunchFacts, bool), late: Option<(u32, u64)>) -> Verdict {
    let (fast_facts, fast_ok) = fast;
    let (slow_facts, slow_ok) = slow;
    let fields = match &fast_facts {
        Some(f) => differing(f, &slow_facts),
        None => vec!["(hot stopped before the rules)"],
    };
    let bundle_only = !fields.is_empty() && fields.iter().all(|f| matches!(*f, "insider_lamports" | "bundle_wallets"));
    let hot_saw_less = fast_facts.is_some_and(|f| f.bundle_wallets.unwrap_or(0) <= slow_facts.bundle_wallets.unwrap_or(0));
    Verdict {
        agree: fields.is_empty() && fast_ok == slow_ok,
        verdict_differs: fast_ok != slow_ok,
        late_explains: bundle_only && hot_saw_less && late.is_some_and(|(n, _)| n > 0),
        fields,
    }
}

const HEADER: &str = "mint,verdict_hot,verdict_slow,result,fields,late_buys,late_sol,bundle_wallets_hot,bundle_wallets_slow,insider_sol_hot,insider_sol_slow";

/// Compares every settled launch, writes its row, counts it. Called by the reporter thread.
pub fn flush() {
    let now = super::clock::now_ns();
    let ready: Vec<Entry> = {
        let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<Entry> = t.displaced[..t.n_displaced].iter().flatten().copied().collect();
        t.n_displaced = 0;
        for e in t.slots.iter_mut().flatten() {
            if !e.reported && e.fast.is_some() && e.slow.is_some() && now.saturating_sub(e.at) > SETTLE_NS {
                e.reported = true;
                out.push(*e);
            }
        }
        out
    };
    if ready.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all("logs");
    let new_file = !std::path::Path::new(VERIFY_CSV).exists();
    let mut file = crate::store::tee::append(VERIFY_CSV, "hot_verify", HEADER);
    if new_file {
        if let Some(f) = file.as_mut() {
            let _ = writeln!(f, "{HEADER}");
        }
    }
    let sol = |l: Option<u64>| l.map(|l| format!("{:.4}", l as f64 / 1e9)).unwrap_or_default();
    for e in ready {
        let (Some(fast), Some(slow)) = (e.fast, e.slow) else { continue };
        let v = judge(fast, slow, e.late);
        COMPARED.fetch_add(1, Ordering::Relaxed);
        let result = if v.agree {
            AGREE.fetch_add(1, Ordering::Relaxed);
            "agree"
        } else if v.verdict_differs {
            counts::VERDICT.fetch_add(1, Ordering::Relaxed);
            if v.late_explains {
                counts::VERDICT_LATE.fetch_add(1, Ordering::Relaxed);
            }
            warn!(
                "hot path verdict differs on {}: rules {} (hot) vs {} (slow); facts differ in {:?}; late buys {:?}; hot {:?} slow {:?}",
                e.mint, fast.1, slow.1, v.fields, e.late, fast.0, slow.0
            );
            "verdict"
        } else {
            counts::FACTS_ONLY.fetch_add(1, Ordering::Relaxed);
            if v.late_explains {
                counts::FACTS_ONLY_LATE.fetch_add(1, Ordering::Relaxed);
            }
            "facts"
        };
        if let Some(f) = file.as_mut() {
            let hot = fast.0;
            let _ = writeln!(
                f,
                "{},{},{},{},{},{},{},{},{},{},{}",
                e.mint,
                fast.1,
                slow.1,
                result,
                v.fields.join("+"),
                e.late.map_or(String::new(), |l| l.0.to_string()),
                sol(e.late.map(|l| l.1)),
                hot.and_then(|h| h.bundle_wallets).map_or(String::new(), |w| w.to_string()),
                slow.0.bundle_wallets.map_or(String::new(), |w| w.to_string()),
                sol(hot.and_then(|h| h.insider_lamports)),
                sol(slow.0.insider_lamports),
            );
        }
    }
}

/// One line for the stats log.
pub fn report() -> String {
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    format!(
        "cross-check: {} compared, {} agree, {} same verdict with different facts ({} of them only the bundle, with buys that reached the hot path after its window), {} verdicts differ ({} of them the same way)",
        g(&COMPARED),
        g(&AGREE),
        g(&counts::FACTS_ONLY),
        g(&counts::FACTS_ONLY_LATE),
        g(&counts::VERDICT),
        g(&counts::VERDICT_LATE)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(wallets: u32, insider: u64) -> LaunchFacts {
        LaunchFacts {
            mayhem: false,
            dev_buy_lamports: 1,
            deployer_lamports: None,
            follow: None,
            reference_profit_lamports: None,
            reference_trades: None,
            insider_lamports: Some(insider),
            risky_host: None,
            secs_since_previous_launch: None,
            launches_last_day: None,
            bundle_wallets: Some(wallets),
            name_all_caps: None,
            deployer_record: (0, 0),
            scoring: Default::default(),
        }
    }

    #[test]
    fn differences_are_told_apart() {
        let v = judge((Some(facts(2, 10)), false), (facts(2, 10), false), None);
        assert!(v.agree && !v.verdict_differs && v.fields.is_empty());
        let v = judge((Some(facts(2, 10)), false), (facts(4, 30), false), Some((2, 20)));
        assert!(!v.agree && !v.verdict_differs && v.late_explains, "{v:?}");
        assert_eq!(v.fields, vec!["insider_lamports", "bundle_wallets"]);
        let v = judge((Some(facts(2, 10)), true), (facts(4, 30), false), None);
        assert!(v.verdict_differs && !v.late_explains, "no late buys seen: not explained");
        let mut other = facts(2, 10);
        other.mayhem = true;
        let v = judge((Some(facts(2, 10)), false), (other, false), Some((1, 1)));
        assert!(!v.late_explains, "a non-bundle field differs");
    }
}
