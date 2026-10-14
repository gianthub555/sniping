//! Engine-merge parity: while pumpfeed's UDP push and the in-process shred engine both run, every
//! create each path saw, when it saw it, and what it saw of the create's batch (the buys after it
//! that reach the decider, their SOL, and the highest rival bid among them, the `HOT_OUTBID`
//! input). The feed threads note without allocating; the reporter thread (`flush`) writes
//! `logs/engine_parity.csv` once a create is 10 s old and keeps the totals.
//!
//! The feed race (8 Oct 2026): with Helius's preprocessed stream and the in-process shreds
//! (AllenHark's stream) both on, every create's arrival on each, and the slot leader's client, so
//! `research/feed-race-2026-10-08/race_report.py` can tell which is first, by how much, by client.

use {
    super::stamps::Feed,
    solana_pubkey::Pubkey,
    std::{
        io::Write,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
            LazyLock, Mutex,
        },
    },
};

pub const PARITY_CSV: &str = "logs/engine_parity.csv";
const SLOTS: usize = 8192;
const SETTLE_NS: u64 = 10_000_000_000;

static ON: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ON.store(true, Relaxed);
}

pub fn is_on() -> bool {
    ON.load(Relaxed)
}

/// What one shred path saw of a create's batch.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct View {
    /// Frame (UDP: datagram; in-process: the shred completing the batch) received, and the
    /// create decoded.
    pub t_recv: u64,
    pub t_matched: u64,
    /// Buys of the coin after the create in its batch that reach the decider, their SOL, and the
    /// highest priority bid among them that is not the deployer's, creator's or fee payer's.
    pub later: u32,
    pub later_lamports: u64,
    pub rival: u64,
}

#[derive(Clone, Copy)]
struct Entry {
    mint: [u8; 32],
    slot: u64,
    first_ns: u64,
    pp: u64,
    pp_recv: u64,
    udp: Option<View>,
    inproc: Option<View>,
    reported: bool,
}

static TABLE: LazyLock<Mutex<Box<[Option<Entry>]>>> = LazyLock::new(|| Mutex::new(vec![None; SLOTS].into_boxed_slice()));

pub mod counts {
    use std::sync::atomic::AtomicU64;
    pub static CREATES: AtomicU64 = AtomicU64::new(0);
    pub static BOTH: AtomicU64 = AtomicU64::new(0);
    pub static UDP_ONLY: AtomicU64 = AtomicU64::new(0);
    pub static INPROC_ONLY: AtomicU64 = AtomicU64::new(0);
    /// Of those seen by both: in-process decoded the create first.
    pub static INPROC_FIRST: AtomicU64 = AtomicU64::new(0);
    /// Of those seen by both: the same buys after the create, SOL and rival bid.
    pub static SAME_VIEW: AtomicU64 = AtomicU64::new(0);
    pub static RIVAL_DIFFERS: AtomicU64 = AtomicU64::new(0);
    /// Pushed out of the table by a newer create before it was written (none expected).
    pub static LOST: AtomicU64 = AtomicU64::new(0);
    /// Seen on the preprocessed stream only (no shred path decoded it).
    pub static PP_ONLY: AtomicU64 = AtomicU64::new(0);
}

pub fn init() {
    LazyLock::force(&TABLE);
}

fn index(mint: &[u8; 32]) -> usize {
    u64::from_le_bytes(mint[..8].try_into().expect("8")) as usize & (SLOTS - 1)
}

/// A feed saw a create. Allocation-free.
pub fn note(mint: &Pubkey, slot: u64, feed: Feed, view: View, now: u64) {
    if !is_on() {
        return;
    }
    let key = mint.to_bytes();
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let e = &mut t[index(&key)];
    if !e.is_some_and(|e| e.mint == key) {
        if e.is_some_and(|e| !e.reported) {
            counts::LOST.fetch_add(1, Relaxed);
        }
        *e = Some(Entry { mint: key, slot, first_ns: now, pp: 0, pp_recv: 0, udp: None, inproc: None, reported: false });
    }
    let e = e.as_mut().expect("just set");
    match feed {
        Feed::Preprocessed => {
            if e.pp == 0 {
                e.pp = view.t_matched;
                e.pp_recv = view.t_recv;
            }
        }
        Feed::Udp => {
            e.udp.get_or_insert(view);
        }
        Feed::Inproc => {
            e.inproc.get_or_insert(view);
        }
        Feed::Bench | Feed::Grpc | Feed::Sell => {}
    }
}

const HEADER: &str = "mint,slot,seen_by,pp_matched,udp_recv,udp_matched,inproc_recv,inproc_matched,inproc_lead_us,udp_later,inproc_later,udp_later_sol,inproc_later_sol,udp_rival,inproc_rival,same_view,pp_recv,leader,leader_client,leader_class";

/// Writes the creates that have settled. Called by the reporter thread.
pub fn flush(now: u64) {
    if !is_on() {
        return;
    }
    let ready: Vec<Entry> = {
        let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        t.iter_mut()
            .flatten()
            .filter(|e| !e.reported && now.saturating_sub(e.first_ns) > SETTLE_NS && (e.pp > 0 || e.udp.is_some() || e.inproc.is_some()))
            .map(|e| {
                e.reported = true;
                *e
            })
            .collect()
    };
    if ready.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all("logs");
    // A file with other columns is set aside, not appended to.
    if crate::store::tee::first_line(PARITY_CSV).is_some_and(|l| l != HEADER) {
        let _ = std::fs::rename(PARITY_CSV, format!("{PARITY_CSV}.{}.old", super::positions::today()));
    }
    let new_file = !std::path::Path::new(PARITY_CSV).exists();
    let Some(mut f) = crate::store::tee::append(PARITY_CSV, "engine_parity", HEADER) else { return };
    if new_file {
        let _ = writeln!(f, "{HEADER}");
    }
    let c = |a: &AtomicU64| a.fetch_add(1, Relaxed);
    for e in ready {
        c(&counts::CREATES);
        let seen_by = match (e.udp, e.inproc) {
            (Some(_), Some(_)) => "both",
            (Some(_), None) => "udp_only",
            (None, Some(_)) => "inproc_only",
            (None, None) => {
                c(&counts::PP_ONLY);
                "pp_only"
            }
        };
        let mut lead = String::new();
        let mut same = String::new();
        match (e.udp, e.inproc) {
            (Some(u), Some(i)) => {
                c(&counts::BOTH);
                if i.t_matched < u.t_matched {
                    c(&counts::INPROC_FIRST);
                }
                lead = format!("{:.1}", (u.t_matched as f64 - i.t_matched as f64) / 1e3);
                let s = u.later == i.later && u.later_lamports == i.later_lamports && u.rival == i.rival;
                if s {
                    c(&counts::SAME_VIEW);
                }
                if u.rival != i.rival {
                    c(&counts::RIVAL_DIFFERS);
                }
                same = s.to_string();
            }
            (Some(_), None) => {
                c(&counts::UDP_ONLY);
            }
            (None, Some(_)) => {
                c(&counts::INPROC_ONLY);
            }
            (None, None) => {}
        }
        let leader = crate::utils::leaders::leader(e.slot);
        let client = leader.and_then(|l| crate::utils::leaders::client(&l)).unwrap_or_default();
        let class = leader.and_then(|l| super::clients::of_identity(&l)).map_or("", |c| c.name());
        let v = |x: Option<View>, g: fn(&View) -> String| x.as_ref().map(g).unwrap_or_default();
        let _ = writeln!(
            f,
            "{},{},{seen_by},{},{},{},{},{},{lead},{},{},{},{},{},{},{same},{},{},{client},{class}",
            Pubkey::new_from_array(e.mint),
            e.slot,
            if e.pp > 0 { e.pp.to_string() } else { String::new() },
            v(e.udp, |x| x.t_recv.to_string()),
            v(e.udp, |x| x.t_matched.to_string()),
            v(e.inproc, |x| x.t_recv.to_string()),
            v(e.inproc, |x| x.t_matched.to_string()),
            v(e.udp, |x| x.later.to_string()),
            v(e.inproc, |x| x.later.to_string()),
            v(e.udp, |x| format!("{:.4}", x.later_lamports as f64 / 1e9)),
            v(e.inproc, |x| format!("{:.4}", x.later_lamports as f64 / 1e9)),
            v(e.udp, |x| x.rival.to_string()),
            v(e.inproc, |x| x.rival.to_string()),
            if e.pp_recv > 0 { e.pp_recv.to_string() } else { String::new() },
            leader.map_or(String::new(), |l| l.to_string()),
        );
    }
}

pub fn report() -> String {
    let g = |a: &AtomicU64| a.load(Relaxed);
    use counts::*;
    format!(
        "parity: {} creates, both paths {} (in-process first on {}, same buys and rival bid on {}, rival bid differs on {}), pumpfeed UDP only {}, in-process only {}, preprocessed only {}, lost {}",
        g(&CREATES),
        g(&BOTH),
        g(&INPROC_FIRST),
        g(&SAME_VIEW),
        g(&RIVAL_DIFFERS),
        g(&UDP_ONLY),
        g(&INPROC_ONLY),
        g(&PP_ONLY),
        g(&LOST)
    )
}
