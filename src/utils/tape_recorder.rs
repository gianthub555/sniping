//! Every pump.fun launch's first two minutes of trades, from the regular stream, for the strategy
//! research: one JSON line per launch in `logs/tapes/<UTC date>.jsonl`, in the layout `rules_check`
//! reads (`mint`, `bt`, `slot`, `dep`, `creator`, `uri`, `name`, `mayhem`, and `tape`: every trade
//! with its slot, block position, buyer, SOL, tokens and the curve's virtual reserves after it).
//! Launches of followed deployers are kept 30 minutes, with `fol` (`recent` / `profitable`), for the
//! ML study (research/ml-entry-exit-2026-10-13): their line is written when the 30 minutes end.
//! Logging only: nothing here decides or sends. `TAPE_RECORDER=false` turns it off.

use {
    super::parse::{read_pubkey, read_u64},
    crate::{
        instructions::{pumpfun_buy_ix::TRADE_EVENT_DISCRIMINATOR, pumpswap_buy_ix::PUMP_PROGRAM_ID},
        processors::common::cpi_event_payloads,
    },
    carbon_core::transaction::TransactionMetadata,
    log::{info, warn},
    serde::Serialize,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::{HashMap, HashSet},
        io::Write,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
};

/// How long after the create a launch's trades are kept.
const RECORD_FOR: Duration = Duration::from_secs(120);
/// ...and a followed deployer's launch's (about 1.5 a minute, so ~45 open at a time).
const RECORD_FOLLOWED_FOR: Duration = Duration::from_secs(30 * 60);
/// Launches recorded at once at most (about 60 are in their two minutes at a time).
const MAX_OPEN: usize = 2_000;
/// Trades kept per launch at most.
const MAX_TRADES: usize = 3_000;
/// ...for a followed launch's 30 minutes (p99 2,376 trades on 13 Oct, the survival recorder).
const MAX_TRADES_FOLLOWED: usize = 30_000;
pub const TAPES_DIR: &str = "logs/tapes";

#[derive(Serialize, Clone, Copy)]
struct Row {
    slot: u64,
    idx: u64,
    /// The trade's place among the transaction's trade events.
    k: u32,
    bt: i64,
    /// When the bot received this transaction from the stream (unix ms): with the create's `ms`,
    /// how long after the create the trade came (the stream's own delay cancels out).
    ms: u64,
    is_buy: bool,
    sol: u64,
    tok: u64,
    #[serde(serialize_with = "as_b58")]
    user: Pubkey,
    vsol: u64,
    vtok: u64,
}

fn as_b58<S: serde::Serializer>(k: &Pubkey, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&k.to_string())
}

#[derive(Serialize)]
struct Launch {
    mint: String,
    bt: i64,
    slot: u64,
    idx: u64,
    /// When the bot received the create from the stream (unix ms).
    ms: u64,
    dep: String,
    creator: String,
    uri: String,
    name: String,
    mayhem: bool,
    /// The bot's follow decision at the create (`recent` / `profitable`); absent when not followed.
    #[serde(skip_serializing_if = "Option::is_none")]
    fol: Option<&'static str>,
    tape: Vec<Row>,
}

struct Open {
    launch: Launch,
    started: Instant,
    seen: HashSet<Signature>,
    /// How long this launch is recorded and how many trades it keeps.
    record_for: Duration,
    max_trades: usize,
}

/// A launch's follow label, recording time and trade cap.
fn window_for(follow: Option<crate::config::rules::Follow>) -> (Option<&'static str>, Duration, usize) {
    use crate::config::rules::Follow;
    match follow {
        Some(Follow::Recent) => (Some("recent"), RECORD_FOLLOWED_FOR, MAX_TRADES_FOLLOWED),
        Some(Follow::Profitable) => (Some("profitable"), RECORD_FOLLOWED_FOR, MAX_TRADES_FOLLOWED),
        None => (None, RECORD_FOR, MAX_TRADES),
    }
}

static OPEN: LazyLock<Mutex<HashMap<Pubkey, Open>>> = LazyLock::new(Default::default);
static COUNT: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

fn on() -> bool {
    !crate::config::env_opt("TAPE_RECORDER").is_some_and(|v| v.eq_ignore_ascii_case("false"))
}

fn unix_now() -> i64 {
    crate::utils::dashboard::now_unix() as i64
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A launch: its trades are recorded from now for two minutes (a followed deployer's for 30).
#[allow(clippy::too_many_arguments)]
pub fn start(mint: Pubkey, tx: &TransactionMetadata, deployer: Pubkey, creator: Pubkey, uri: &str, name: &str, mayhem: bool) {
    if !on() {
        return;
    }
    if !STARTED.swap(true, Ordering::SeqCst) {
        spawn_flusher();
    }
    // The same decision the entry makes (deployer or creator), from the reference wallet's records;
    // taken before the recorder's lock so the two locks are never held together.
    let (fol, record_for, max_trades) = window_for(crate::utils::reference::follow(&[deployer, creator], unix_now() as u64));
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if open.len() >= MAX_OPEN || open.contains_key(&mint) {
        return;
    }
    open.insert(
        mint,
        Open {
            launch: Launch {
                mint: mint.to_string(),
                bt: tx.block_time.unwrap_or_else(unix_now),
                slot: tx.slot,
                idx: tx.index.unwrap_or(u64::MAX),
                ms: unix_ms(),
                dep: deployer.to_string(),
                creator: creator.to_string(),
                uri: uri.chars().take(200).collect(),
                name: name.chars().take(64).collect(),
                mayhem,
                fol,
                tape: Vec::new(),
            },
            started: Instant::now(),
            seen: HashSet::new(),
            record_for,
            max_trades,
        },
    );
}

/// A pump.fun transaction from the regular stream: its trades on recorded launches.
pub fn observe(tx: &TransactionMetadata) {
    if !STARTED.load(Ordering::Relaxed) {
        return;
    }
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if open.is_empty() {
        return;
    }
    let payloads = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    // The processor runs once per pump.fun instruction and each run sees every event of the
    // transaction: a transaction is recorded once per launch.
    let mut fresh: Vec<Pubkey> = Vec::new();
    for body in &payloads {
        if let Ok(mint) = read_pubkey(body, 0) {
            if !fresh.contains(&mint) && open.get(&mint).is_some_and(|o| !o.seen.contains(&tx.signature)) {
                fresh.push(mint);
            }
        }
    }
    if fresh.is_empty() {
        return;
    }
    let ms = unix_ms();
    for (k, body) in payloads.iter().enumerate() {
        let Ok(mint) = read_pubkey(body, 0) else { continue };
        if !fresh.contains(&mint) {
            continue;
        }
        let Some(o) = open.get_mut(&mint) else { continue };
        let (Ok(sol), Ok(tok), Ok(vsol), Ok(vtok), Ok(user)) = (read_u64(body, 32), read_u64(body, 40), read_u64(body, 89), read_u64(body, 97), read_pubkey(body, 49)) else {
            continue;
        };
        if o.launch.tape.len() >= o.max_trades {
            continue;
        }
        o.launch.tape.push(Row {
            slot: tx.slot,
            idx: tx.index.unwrap_or(u64::MAX),
            k: k as u32,
            bt: tx.block_time.unwrap_or_else(unix_now),
            ms,
            is_buy: body.get(48) == Some(&1),
            sol,
            tok,
            user,
            vsol,
            vtok,
        });
    }
    for mint in fresh {
        if let Some(o) = open.get_mut(&mint) {
            o.seen.insert(tx.signature);
        }
    }
}

/// pump.fun's fresh curve: 30 SOL virtual, 1,073,000,000 tokens virtual of which 793,100,000 real
/// (6 decimals): real SOL = virtual - 30 SOL; the curve is complete when no real token is left.
const INITIAL_VIRTUAL_SOL: u64 = 30_000_000_000;
const VIRTUAL_NOT_REAL_TOKENS: u64 = 279_900_000_000_000;

/// The snipe test's look back at a recorded launch (the regular stream, a few seconds after the
/// create): the whole create slot (buyers besides the deployer and the creator, real SOL and
/// completeness after its last trade), and `wallet`'s first buy (slot, block index, its rank among
/// the coin's trades in that slot). None when the launch is not (or no longer) recorded.
pub fn create_slot_view(mint: &Pubkey, wallet: &Pubkey) -> Option<(crate::utils::snipe_test::FullSlot, Option<crate::utils::snipe_test::Landing>)> {
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    let o = open.get(mint)?;
    Some(slot_view(&o.launch, wallet))
}

fn slot_view(l: &Launch, wallet: &Pubkey) -> (crate::utils::snipe_test::FullSlot, Option<crate::utils::snipe_test::Landing>) {
    use crate::utils::snipe_test::{FullSlot, Landing};
    let mut full = FullSlot::default();
    let mut buyers: Vec<Pubkey> = Vec::new();
    let mut last: Option<&Row> = None;
    let insiders: Vec<String> = vec![l.dep.clone(), l.creator.clone()];
    for r in l.tape.iter().filter(|r| r.slot == l.slot) {
        if r.is_buy && !insiders.contains(&r.user.to_string()) && !buyers.contains(&r.user) {
            buyers.push(r.user);
        }
        if last.is_none_or(|x| (r.idx, r.k) >= (x.idx, x.k)) {
            last = Some(r);
        }
    }
    full.buyers = buyers.len() as u32;
    if let Some(r) = last {
        full.real_sol_lamports = r.vsol.saturating_sub(INITIAL_VIRTUAL_SOL);
        full.complete = r.vtok <= VIRTUAL_NOT_REAL_TOKENS;
    }
    let ours = l.tape.iter().filter(|r| r.is_buy && r.user == *wallet).min_by_key(|r| (r.slot, r.idx, r.k)).map(|r| Landing {
        slot: r.slot,
        index: (r.idx != u64::MAX).then_some(r.idx),
        rank: Some(1 + l.tape.iter().filter(|x| x.slot == r.slot && (x.idx, x.k) < (r.idx, r.k)).count() as u32),
        source: "chain",
    });
    (full, ours)
}

/// Writes every launch whose recording time is over, once a few seconds.
fn spawn_flusher() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let done: Vec<Launch> = {
                let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
                let keys: Vec<Pubkey> = open.iter().filter(|(_, o)| o.started.elapsed() >= o.record_for).map(|(k, _)| *k).collect();
                keys.into_iter().filter_map(|k| open.remove(&k)).map(|o| o.launch).collect()
            };
            if done.is_empty() {
                continue;
            }
            let n = done.len();
            let _ = tokio::task::spawn_blocking(move || write(done)).await;
            let total = COUNT.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
            if total == n as u64 {
                info!(
                    "tape recorder: writing every launch's first {} s of trades to {TAPES_DIR}/ (followed deployers' launches: {} min)",
                    RECORD_FOR.as_secs(),
                    RECORD_FOLLOWED_FOR.as_secs() / 60
                );
            }
        }
    });
}

fn write(done: Vec<Launch>) {
    // The store (STORE_DB=on): each launch's line to that day's tapes file (data/tapes).
    if crate::store::db_on() {
        let day = super::super::hot::positions::today();
        for l in &done {
            let mut l2 = Launch { tape: l.tape.clone(), mint: l.mint.clone(), dep: l.dep.clone(), creator: l.creator.clone(), uri: l.uri.clone(), name: l.name.clone(), ..*l };
            l2.tape.sort_by_key(|r| (r.slot, r.idx, r.k));
            if let Ok(json) = serde_json::to_string(&l2) {
                crate::store::write(crate::store::Row::Tape { day: day.clone(), mint: l.mint.clone(), bt: l.bt, slot: l.slot as i64, json, rx_ns: crate::store::now_ns() });
            }
        }
    }
    if !crate::store::files_on() {
        return;
    }
    let _ = std::fs::create_dir_all(TAPES_DIR);
    let path = format!("{TAPES_DIR}/{}.jsonl", super::super::hot::positions::today());
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let mut buf = String::new();
            for mut l in done {
                l.tape.sort_by_key(|r| (r.slot, r.idx, r.k));
                if let Ok(line) = serde_json::to_string(&l) {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
            if let Err(e) = f.write_all(buf.as_bytes()) {
                warn!("tape recorder: {path}: {e}");
            }
        }
        Err(e) => warn!("tape recorder: {path}: {e}"),
    }
}

/// Launches written so far.
pub fn written() -> u64 {
    COUNT.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slot: u64, idx: u64, is_buy: bool, user: Pubkey, vsol: u64, vtok: u64) -> Row {
        Row { slot, idx, k: 0, bt: 0, ms: 0, is_buy, sol: 0, tok: 0, user, vsol, vtok }
    }

    /// The whole create slot: buyers besides the deployer, real SOL and completeness after its last
    /// trade; our first buy with its block index and rank in its slot.
    #[test]
    fn the_create_slot_and_our_landing_from_the_tape() {
        let (dep, a, b, us) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let launch = Launch {
            mint: String::new(),
            bt: 0,
            slot: 10,
            idx: 5,
            ms: 0,
            dep: dep.to_string(),
            creator: dep.to_string(),
            uri: String::new(),
            name: String::new(),
            mayhem: false,
            fol: None,
            tape: vec![
                row(10, 5, true, dep, 30_500_000_000, 1_050_000_000_000_000),
                row(10, 6, true, a, 31_000_000_000, 1_030_000_000_000_000),
                row(10, 90, true, b, 31_400_000_000, 1_020_000_000_000_000),
                row(10, 91, true, a, 31_600_000_000, 1_010_000_000_000_000),
                row(10, 7, false, b, 31_200_000_000, 1_025_000_000_000_000),
                row(11, 3, true, b, 32_000_000_000, 1_000_000_000_000_000),
                row(11, 8, true, us, 32_100_000_000, 990_000_000_000_000),
            ],
        };
        let (full, ours) = slot_view(&launch, &us);
        assert_eq!(full.buyers, 2, "a and b; the deployer and a sell don't count");
        assert_eq!(full.real_sol_lamports, 1_600_000_000, "after the slot's last trade (index 91)");
        assert!(!full.complete);
        let l = ours.unwrap();
        assert_eq!((l.slot, l.index, l.rank, l.source), (11, Some(8), Some(2), "chain"));
        let (_, none) = slot_view(&launch, &Pubkey::new_unique());
        assert!(none.is_none());
        let done = Launch { tape: vec![row(10, 5, true, dep, 115_000_000_000, VIRTUAL_NOT_REAL_TOKENS)], ..launch };
        assert!(slot_view(&done, &us).0.complete);
    }

    /// Followed launches are kept 30 minutes with a larger trade cap and their label; others two.
    #[test]
    fn followed_launches_are_recorded_for_thirty_minutes() {
        use crate::config::rules::Follow;
        assert_eq!(window_for(None), (None, Duration::from_secs(120), MAX_TRADES));
        assert_eq!(window_for(Some(Follow::Recent)), (Some("recent"), Duration::from_secs(1800), MAX_TRADES_FOLLOWED));
        assert_eq!(window_for(Some(Follow::Profitable)), (Some("profitable"), Duration::from_secs(1800), MAX_TRADES_FOLLOWED));
        let mut l = Launch { mint: "m".into(), bt: 1, slot: 2, idx: 3, ms: 4, dep: "d".into(), creator: "c".into(), uri: String::new(), name: String::new(), mayhem: false, fol: None, tape: vec![] };
        assert!(!serde_json::to_string(&l).unwrap().contains("fol"), "unfollowed lines keep the old layout");
        l.fol = Some("recent");
        assert!(serde_json::to_string(&l).unwrap().contains(r#""fol":"recent""#));
    }
}
