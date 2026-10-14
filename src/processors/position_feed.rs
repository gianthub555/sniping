










use {
    super::common::cpi_event_payloads,
    crate::{
        instructions::{
            pumpfun_buy_ix::{CurveReserves, TRADE_EVENT_DISCRIMINATOR},
            pumpswap_buy_ix::PUMP_PROGRAM_ID,
        },
        utils::parse::{read_pubkey, read_u64},
    },
    carbon_core::transaction::TransactionMetadata,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
    tokio::sync::Notify,
};


const FILL_TTL: Duration = Duration::from_secs(120);

/// Other wallets' trades are kept this long for the exit's flow counts.
const FLOW_TTL: Duration = Duration::from_secs(120);

/// Stream order of every trade the feeds record, so "behind our fill" means after it on the stream.
static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct FlowTrade {
    at: Instant,
    seq: u64,
    slot: u64,
    /// Block position (u64::MAX when the stream did not say).
    index: u64,
    is_buy: bool,
    sol: u64,
    user: Pubkey,
    /// The curve after this trade.
    after: CurveReserves,
}

/// Other wallets' buys and sells on a curve over some span.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flow {
    pub buys: u32,
    pub buy_lamports: u64,
    pub sells: u32,
    pub sell_lamports: u64,
}

impl Flow {
    fn add(&mut self, t: &FlowTrade) {
        if t.is_buy {
            self.buys += 1;
            self.buy_lamports += t.sol;
        } else {
            self.sells += 1;
            self.sell_lamports += t.sol;
        }
    }
}


#[derive(Debug, Clone, Copy)]
pub struct CurveState {
    pub reserves: CurveReserves,
    
    
    pub creator: Pubkey,
}


#[derive(Default)]
pub struct Feed {
    latest: Mutex<Option<(u64, CurveState)>>,
    
    
    dump: Mutex<f64>,
    changed: Notify,
    trades: Mutex<VecDeque<FlowTrade>>,
    own_fill_seq: Mutex<Option<u64>>,
    own_fill_slot: Mutex<Option<u64>>,
    /// Our fill's (slot, block position) when known: "behind the fill" is then by position, the
    /// order that decides who bought first, not by arrival on the stream.
    own_fill_pos: Mutex<Option<(u64, u64)>>,
    /// The coin's creator moved to a fee-sharing account (a separate transaction, not a trade):
    /// sells must name the new creator's vault or fail with 2006 (live, 26 Sep 2026: a sell 2 s
    /// after the buy failed that way and the retry a second later sold 30% lower).
    migrated_creator: Mutex<Option<Pubkey>>,
    /// The dev and the wallets of the create's bundle (`rules::INSIDER_EXIT`).
    insiders: Mutex<HashSet<Pubkey>>,
    /// Insiders' sells seen on the preprocessed stream, before they executed (their arrival order),
    /// for SELL_PREEXEC_TRIGGERS.
    pre_insider_sells: Mutex<Vec<u64>>,
}

impl Feed {
    
    pub fn state(&self) -> Option<CurveState> {
        let migrated = *self.migrated_creator.lock().unwrap_or_else(|e| e.into_inner());
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).map(|(_, s)| CurveState { creator: migrated.unwrap_or(s.creator), ..s })
    }

    
    pub fn take_dump(&self) -> f64 {
        std::mem::take(&mut *self.dump.lock().unwrap_or_else(|e| e.into_inner()))
    }

    
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    /// Other wallets' trades seen at or after `since`.
    pub fn flow_since(&self, since: Instant) -> Flow {
        let mut flow = Flow::default();
        for t in self.trades.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|t| t.at >= since) {
            flow.add(t);
        }
        flow
    }

    /// When another wallet last bought.
    pub fn last_buy_at(&self) -> Option<Instant> {
        self.trades.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|t| t.is_buy).map(|t| t.at).max()
    }

    /// A paper buy lands now, in `slot` (the stream's latest): trades the stream shows from here
    /// on count as behind it, the way trades after a live fill do.
    pub fn mark_paper_fill(&self, slot: Option<u64>, index: Option<u64>) {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        self.own_fill_seq.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(seq);
        if let Some(slot) = slot {
            self.own_fill_slot.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(slot);
            if let Some(index) = index {
                self.own_fill_pos.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert((slot, index));
            }
        }
    }

    /// Whether `t` landed behind our fill: by block position when ours is known, else by arrival.
    fn behind(&self, own_seq: u64, t: &FlowTrade) -> bool {
        match *self.own_fill_pos.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(pos) if t.index != u64::MAX => (t.slot, t.index) > pos,
            _ => t.seq > own_seq,
        }
    }

    /// The dev and the create's bundle wallets, once the launch stream has shown them.
    pub fn set_insiders(&self, insiders: impl IntoIterator<Item = Pubkey>) {
        self.insiders.lock().unwrap_or_else(|e| e.into_inner()).extend(insiders);
    }

    pub fn insider_count(&self) -> usize {
        self.insiders.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether the dev or a bundle wallet sold behind our fill. With SELL_PREEXEC_TRIGGERS, a sell of
    /// theirs seen on the preprocessed stream (before it executed) after our fill counts too.
    pub fn insider_sold(&self) -> bool {
        let insiders = self.insiders.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if insiders.is_empty() {
            return false;
        }
        let own = *self.own_fill_seq.lock().unwrap_or_else(|e| e.into_inner());
        if preexec_triggers() && self.pre_insider_sells.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|seq| own.is_none_or(|o| *seq > o)) {
            return true;
        }
        self.trades
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|t| !t.is_buy && insiders.contains(&t.user) && own.is_none_or(|seq| self.behind(seq, t)))
    }

    /// The curve as a transaction landing at (`slot`, `index`) meets it: after the last recorded
    /// trade before that position (None: no trade recorded before it). Trades without a block
    /// position count as the end of their slot.
    pub fn state_before(&self, slot: u64, index: u64) -> Option<CurveReserves> {
        self.trades
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|t| (t.slot, t.index) < (slot, index))
            .max_by_key(|t| (t.slot, t.index, t.seq))
            .map(|t| t.after)
    }

    /// The slot our fill landed in, once the stream has shown it (or the paper fill is marked).
    pub fn fill_slot(&self) -> Option<u64> {
        *self.own_fill_slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Other wallets' trades behind our fill up to `slots` slots after its slot.
    pub fn behind_within_slots(&self, slots: u64) -> Flow {
        let own = *self.own_fill_seq.lock().unwrap_or_else(|e| e.into_inner());
        let mut flow = Flow::default();
        let (Some(seq), Some(fill_slot)) = (own, self.fill_slot()) else {
            return flow;
        };
        for t in self.trades.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if self.behind(seq, t) && t.slot <= fill_slot + slots {
                flow.add(t);
            }
        }
        flow
    }

    /// Other wallets' trades that landed behind our own fill. Until the fill shows up on the
    /// stream (or a paper fill is marked) this counts from `opened`.
    pub fn behind_fill(&self, opened: Instant) -> Flow {
        let own = *self.own_fill_seq.lock().unwrap_or_else(|e| e.into_inner());
        let mut flow = Flow::default();
        for t in self.trades.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if own.map_or(t.at >= opened, |seq| self.behind(seq, t)) {
                flow.add(t);
            }
        }
        flow
    }
}


#[derive(Debug, Clone, Copy)]
pub struct Fill {
    pub slot: u64,
    
    pub tokens: u64,
    
    pub wallet_lamports: i64,
    pub seen_at: Instant,
}

static FEEDS: LazyLock<Mutex<HashMap<Pubkey, Arc<Feed>>>> = LazyLock::new(Default::default);
static OWN_FILLS: LazyLock<Mutex<HashMap<Signature, Fill>>> = LazyLock::new(Default::default);

/// Transactions already recorded. The processor runs once per pump.fun instruction, a trade
/// carries two (the trade and its event record), and each call sees all the transaction's events:
/// without this every trade counted twice.
static RECORDED: LazyLock<Mutex<(HashSet<Signature>, VecDeque<Signature>)>> = LazyLock::new(Default::default);
const RECORDED_CAP: usize = 4096;

fn first_time(signature: &Signature) -> bool {
    let mut guard = RECORDED.lock().unwrap_or_else(|e| e.into_inner());
    let (set, order) = &mut *guard;
    if !set.insert(*signature) {
        return false;
    }
    order.push_back(*signature);
    if order.len() > RECORDED_CAP {
        if let Some(old) = order.pop_front() {
            set.remove(&old);
        }
    }
    true
}

static ACTIVE: AtomicUsize = AtomicUsize::new(0);


/// Starts recording a curve's trades. Subscribe before sending the buy so the trades that
/// land right behind the fill are counted.
/// The creator of `mint` moved to `creator` (a fee-sharing account): every later state names it.
pub fn observe_creator_migration(mint: &Pubkey, creator: &Pubkey) {
    if let Some(feed) = FEEDS.lock().unwrap_or_else(|e| e.into_inner()).get(mint) {
        *feed.migrated_creator.lock().unwrap_or_else(|e| e.into_inner()) = Some(*creator);
        feed.changed.notify_one();
    }
}

pub fn subscribe(mint: Pubkey) -> FeedGuard {
    let feed = FEEDS.lock().unwrap_or_else(|e| e.into_inner()).entry(mint).or_default().clone();
    ACTIVE.fetch_add(1, Ordering::Relaxed);
    FeedGuard { mint, feed }
}

pub struct FeedGuard {
    mint: Pubkey,
    pub feed: Arc<Feed>,
}

impl std::fmt::Debug for FeedGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedGuard").field("mint", &self.mint).finish()
    }
}

impl Drop for FeedGuard {
    fn drop(&mut self) {
        FEEDS.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.mint);
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}


pub struct PendingFill;

impl PendingFill {
    pub fn new() -> Self {
        ACTIVE.fetch_add(1, Ordering::Relaxed);
        PendingFill
    }
}

impl Default for PendingFill {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PendingFill {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed) > 0
}


pub async fn wait_fill(signature: &Signature, timeout: Duration) -> Option<Fill> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(fill) = OWN_FILLS.lock().unwrap_or_else(|e| e.into_inner()).get(signature) {
            return Some(*fill);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}



// ---- insiders' sells before they execute (2026-10-12) ------------------------------------------

/// SELL_PREEXEC_TRIGGERS (default off): an insider's sell seen on the preprocessed stream triggers
/// our sell then, 4-10 ms before the regular stream shows it executed. Off: the sightings are only
/// recorded and checked (how often a sighting never executes), not acted on.
pub fn preexec_triggers() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| crate::config::env_opt("SELL_PREEXEC_TRIGGERS").is_some_and(|v| v.eq_ignore_ascii_case("true")));
    *ON
}

/// A sighting is "did not execute" when the regular stream (successful transactions only) has not
/// shown it this long after.
const PRE_CHECK_AFTER: Duration = Duration::from_secs(10);
pub const PRE_CSV: &str = "logs/preexec_sightings.csv";

struct PreSighting {
    mint: Pubkey,
    seller: Pubkey,
    slot: u64,
    at: Instant,
    unix_ms: u64,
}

static PRE_SIGHTINGS: LazyLock<Mutex<HashMap<Signature, PreSighting>>> = LazyLock::new(Default::default);

pub mod pre_counts {
    use std::sync::atomic::AtomicU64;
    pub static SEEN: AtomicU64 = AtomicU64::new(0);
    pub static EXECUTED: AtomicU64 = AtomicU64::new(0);
    pub static NOT_EXECUTED: AtomicU64 = AtomicU64::new(0);
}

/// One preprocessed frame (the slow path's copy): a top-level pump.fun sell (sell, sell_v2 or
/// sell_v3, as `hot::decode::sell_of` reads them) by an insider of a coin with a position is
/// recorded (and, with SELL_PREEXEC_TRIGGERS, wakes its sell monitor). Sells routed through
/// another program are not visible here (no inner instructions before execution).
pub fn observe_preprocessed(frame: &[u8]) {
    if !is_active() {
        return;
    }
    let Some(slot) = frame.get(1..9).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes) else { return };
    let Some(tx) = frame.get(1 + 8 + 64..).and_then(crate::hot::decode::Tx::parse) else { return };
    let Some(pump) = tx.key_index(&PUMP_PROGRAM_ID) else { return };
    for ix in tx.instructions() {
        if ix.program != pump {
            continue;
        }
        let Some(sell) = crate::hot::decode::sell_of(&tx, ix) else { continue };
        let (mint, Some(seller)) = (sell.mint, sell.user) else { continue };
        let Some(feed) = FEEDS.lock().unwrap_or_else(|e| e.into_inner()).get(&mint).cloned() else { continue };
        if !feed.insiders.lock().unwrap_or_else(|e| e.into_inner()).contains(&seller) {
            continue;
        }
        let signature = Signature::from(tx.signature());
        let unix_ms = crate::utils::dashboard::now_unix_ms();
        PRE_SIGHTINGS.lock().unwrap_or_else(|e| e.into_inner()).entry(signature).or_insert_with(|| {
            pre_counts::SEEN.fetch_add(1, Ordering::Relaxed);
            PreSighting { mint, seller, slot, at: Instant::now(), unix_ms }
        });
        feed.pre_insider_sells.lock().unwrap_or_else(|e| e.into_inner()).push(SEQ.fetch_add(1, Ordering::Relaxed));
        if preexec_triggers() {
            feed.changed.notify_one();
        }
    }
}

/// A transaction the regular stream showed (it executed): a preprocessed sighting of it is a hit.
fn pre_executed(signature: &Signature) {
    let Some(s) = PRE_SIGHTINGS.lock().unwrap_or_else(|e| e.into_inner()).remove(signature) else { return };
    pre_counts::EXECUTED.fetch_add(1, Ordering::Relaxed);
    pre_row(signature, &s, true);
}

fn pre_row(signature: &Signature, s: &PreSighting, executed: bool) {
    let line = format!("{},{},{},{},{},{executed},{}\n", s.unix_ms, signature, s.mint, s.seller, s.slot, if executed { s.at.elapsed().as_millis().to_string() } else { String::new() });
    crate::utils::file_writer::run(move || {
        use std::io::Write;
        let new = !std::path::Path::new(PRE_CSV).exists();
        if let Some(mut f) = crate::store::tee::append(PRE_CSV, "preexec_sightings", "seen_unix_ms,signature,mint,seller,slot,executed,ms_to_executed") {
            if new {
                let _ = f.write_all(b"seen_unix_ms,signature,mint,seller,slot,executed,ms_to_executed\n");
            }
            let _ = f.write_all(line.as_bytes());
        }
    });
}

/// Sightings the regular stream never showed within `PRE_CHECK_AFTER`: did not execute (failed,
/// or never landed). Called by the reporter.
pub fn sweep_preexec() {
    sweep_older_than(PRE_CHECK_AFTER);
}

fn sweep_older_than(age: Duration) {
    let mut gone = Vec::new();
    PRE_SIGHTINGS.lock().unwrap_or_else(|e| e.into_inner()).retain(|sig, s| {
        let keep = s.at.elapsed() < age;
        if !keep {
            gone.push((*sig, PreSighting { mint: s.mint, seller: s.seller, slot: s.slot, at: s.at, unix_ms: s.unix_ms }));
        }
        keep
    });
    for (sig, s) in gone {
        pre_counts::NOT_EXECUTED.fetch_add(1, Ordering::Relaxed);
        pre_row(&sig, &s, false);
    }
}

/// One line for the stats log.
pub fn preexec_report() -> String {
    use pre_counts::*;
    let (seen, ok, not) = (SEEN.load(Ordering::Relaxed), EXECUTED.load(Ordering::Relaxed), NOT_EXECUTED.load(Ordering::Relaxed));
    format!(
        "insider sells seen before execution (SELL_PREEXEC_TRIGGERS {}): {seen}, executed {ok}, did not execute {not}{}",
        if preexec_triggers() { "on" } else { "off: recorded only" },
        if ok + not > 0 { format!(" ({:.1}% did not)", 100.0 * not as f64 / (ok + not) as f64) } else { String::new() }
    )
}

pub fn observe_trades(tx: &TransactionMetadata, own_wallet: &Pubkey) {
    if !is_active() {
        return;
    }
    pre_executed(&tx.signature);
    let payloads = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    if payloads.is_empty() || !first_time(&tx.signature) {
        return;
    }
    let feeds = FEEDS.lock().unwrap_or_else(|e| e.into_inner());
    for body in payloads {
        let (Ok(mint), Ok(sol), Ok(tokens), Ok(vq), Ok(vt), Ok(rq), Ok(rt), Ok(user), Ok(creator)) = (
            read_pubkey(&body, 0),
            read_u64(&body, 32),
            read_u64(&body, 40),
            read_u64(&body, 89),
            read_u64(&body, 97),
            read_u64(&body, 105),
            read_u64(&body, 113),
            read_pubkey(&body, 49),
            read_pubkey(&body, 169),
        ) else {
            continue;
        };
        if let Some(feed) = feeds.get(&mint) {
            let state = CurveState {
                reserves: CurveReserves {
                    virtual_token_reserves: vt,
                    virtual_sol_reserves: vq,
                    real_token_reserves: rt,
                    real_sol_reserves: rq,
                },
                creator,
            };
            let is_buy = body.get(48) == Some(&1);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            if user == *own_wallet {
                if is_buy {
                    feed.own_fill_seq.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(seq);
                    feed.own_fill_slot.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(tx.slot);
                    if let Some(index) = tx.index {
                        feed.own_fill_pos.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert((tx.slot, index));
                    }
                }
            } else {
                let now = Instant::now();
                let mut trades = feed.trades.lock().unwrap_or_else(|e| e.into_inner());
                while trades.front().is_some_and(|t| now.duration_since(t.at) > FLOW_TTL) {
                    trades.pop_front();
                }
                trades.push_back(FlowTrade { at: now, seq, slot: tx.slot, index: tx.index.unwrap_or(u64::MAX), is_buy, sol, user, after: state.reserves });
            }
            if !is_buy && user != *own_wallet && vt > tokens && vt > 0 {
                
                let before = (vq as f64 + sol as f64) / (vt - tokens) as f64;
                let after = vq as f64 / vt as f64;
                let drop = (1.0 - after / before) * 100.0;
                let mut dump = feed.dump.lock().unwrap_or_else(|e| e.into_inner());
                *dump = dump.max(drop);
            }
            let mut latest = feed.latest.lock().unwrap_or_else(|e| e.into_inner());
            
            if latest.is_none_or(|(slot, _)| tx.slot >= slot) {
                *latest = Some((tx.slot, state));
                drop(latest);
                feed.changed.notify_one();
            }
        }
        if user == *own_wallet {
            let wallet_lamports = match (tx.meta.pre_balances.first(), tx.meta.post_balances.first()) {
                (Some(&pre), Some(&post)) => post as i64 - pre as i64,
                _ => 0,
            };
            let mut fills = OWN_FILLS.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            fills.retain(|_, f| now.duration_since(f.seen_at) < FILL_TTL);
            fills.entry(tx.signature).or_insert(Fill { slot: tx.slot, tokens, wallet_lamports, seen_at: now });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_creator_migration_overrides_the_creator_trades_reported() {
        let mint = Pubkey::new_unique();
        let guard = subscribe(mint);
        let (old, new) = (Pubkey::new_unique(), Pubkey::new_unique());
        *guard.feed.latest.lock().unwrap() = Some((10, CurveState { reserves: CurveReserves::default(), creator: old }));
        assert_eq!(guard.feed.state().unwrap().creator, old);
        observe_creator_migration(&mint, &new);
        assert_eq!(guard.feed.state().unwrap().creator, new);
        // A later trade event (still naming the old creator, e.g. from the same slot) does not undo it.
        *guard.feed.latest.lock().unwrap() = Some((11, CurveState { reserves: CurveReserves::default(), creator: old }));
        assert_eq!(guard.feed.state().unwrap().creator, new);
    }

    fn trade(feed: &Feed, is_buy: bool, sol: u64) {
        trade_in(feed, 100, is_buy, sol);
    }

    fn trade_in(feed: &Feed, slot: u64, is_buy: bool, sol: u64) {
        trade_at(feed, slot, u64::MAX, is_buy, sol);
    }

    fn lamports(sol: f64) -> u64 {
        (sol * 1e9) as u64
    }

    fn trade_at(feed: &Feed, slot: u64, index: u64, is_buy: bool, sol: u64) {
        trade_by(feed, slot, index, is_buy, sol, Pubkey::default(), CurveReserves::default());
    }

    fn trade_by(feed: &Feed, slot: u64, index: u64, is_buy: bool, sol: u64, user: Pubkey, after: CurveReserves) {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        feed.trades.lock().unwrap().push_back(FlowTrade { at: Instant::now(), seq, slot, index, is_buy, sol, user, after });
    }

    #[test]
    fn an_insiders_sell_seen_before_execution_is_recorded_and_checked() {
        use solana_signer::Signer;
        crate::instructions::pumpfun_buy_ix::set_test_config();
        let mint = Pubkey::new_unique();
        let guard = subscribe(mint);
        let dev = solana_keypair::Keypair::new();
        let other = solana_keypair::Keypair::new();
        guard.feed.set_insiders([dev.pubkey()]);
        let market = crate::instructions::pumpfun_buy_ix::PumpFunMarket::new(mint, crate::utils::utils::TOKEN_PROGRAM_ID, Pubkey::new_unique(), false, Default::default());
        let frame = |seller: &solana_keypair::Keypair| {
            let ixs = market.sell_instructions(&seller.pubkey(), 1_000, 1).unwrap();
            let message = solana_message::Message::new(&ixs, Some(&seller.pubkey()));
            let tx = solana_transaction::versioned::VersionedTransaction::try_new(solana_message::VersionedMessage::Legacy(message), &[seller]).unwrap();
            let bytes = bincode::serialize(&tx).unwrap();
            let mut f = vec![0u8];
            f.extend(77u64.to_le_bytes());
            f.extend(tx.signatures[0].as_ref());
            f.extend(bytes);
            (f, tx.signatures[0])
        };
        let before = pre_counts::SEEN.load(Ordering::Relaxed);
        let (f, _) = frame(&other);
        observe_preprocessed(&f);
        assert_eq!(pre_counts::SEEN.load(Ordering::Relaxed), before, "not an insider");
        let (f, sig) = frame(&dev);
        observe_preprocessed(&f);
        observe_preprocessed(&f);
        assert_eq!(pre_counts::SEEN.load(Ordering::Relaxed), before + 1, "an insider's sell, once");
        assert!(!guard.feed.pre_insider_sells.lock().unwrap().is_empty());
        assert_eq!(guard.feed.insider_sold(), preexec_triggers(), "acted on only with the switch");
        // The regular stream shows it: executed.
        let executed = pre_counts::EXECUTED.load(Ordering::Relaxed);
        pre_executed(&sig);
        assert_eq!(pre_counts::EXECUTED.load(Ordering::Relaxed), executed + 1);
        // Another sell of the dev's that never executes: swept as not executed.
        let not = pre_counts::NOT_EXECUTED.load(Ordering::Relaxed);
        let ixs = market.sell_instructions(&dev.pubkey(), 2_000, 1).unwrap();
        let tx = solana_transaction::versioned::VersionedTransaction::try_new(
            solana_message::VersionedMessage::Legacy(solana_message::Message::new(&ixs, Some(&dev.pubkey()))),
            &[&dev],
        )
        .unwrap();
        let mut g = vec![0u8];
        g.extend(78u64.to_le_bytes());
        g.extend(tx.signatures[0].as_ref());
        g.extend(bincode::serialize(&tx).unwrap());
        observe_preprocessed(&g);
        sweep_older_than(Duration::ZERO);
        assert!(pre_counts::NOT_EXECUTED.load(Ordering::Relaxed) > not);
        drop(guard);
    }

    #[test]
    fn an_insider_sell_behind_the_fill_counts_and_other_trades_do_not() {
        let feed = Feed::default();
        let (dev, bundle, other) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let none = CurveReserves::default();
        // The bundle's buys and a sell by the dev before our fill (landed ahead of it).
        trade_by(&feed, 100, 11, true, lamports(1.5), bundle, none);
        trade_by(&feed, 100, 12, false, lamports(0.1), dev, none);
        feed.mark_paper_fill(Some(100), Some(90));
        feed.set_insiders([dev, bundle]);
        // Ahead of our fill (block position 12 < 90): does not count.
        assert!(!feed.insider_sold());
        trade_by(&feed, 101, 5, false, lamports(2.0), other, none);
        trade_by(&feed, 101, 6, true, lamports(1.0), bundle, none);
        assert!(!feed.insider_sold(), "another wallet's sell and an insider's buy");
        trade_by(&feed, 102, 3, false, lamports(0.01), bundle, none);
        assert!(feed.insider_sold(), "a bundle wallet sold, however little");
        assert!(!Feed::default().insider_sold(), "no insiders known");
    }

    #[test]
    fn a_landing_position_meets_the_curve_after_the_trades_ahead_of_it() {
        let feed = Feed::default();
        let r = |sol: u64| CurveReserves { virtual_sol_reserves: sol, virtual_token_reserves: 1_000, ..Default::default() };
        let who = Pubkey::default();
        trade_by(&feed, 101, 5, true, 1, who, r(31));
        trade_by(&feed, 100, 40, true, 1, who, r(30));
        trade_by(&feed, 101, u64::MAX, true, 1, who, r(33));
        trade_by(&feed, 101, 700, false, 1, who, r(32));
        assert_eq!(feed.state_before(100, 40), None, "nothing ahead");
        assert_eq!(feed.state_before(101, 6), Some(r(31)));
        assert_eq!(feed.state_before(101, 701), Some(r(32)));
        // A trade without a block position sits at the end of its slot.
        assert_eq!(feed.state_before(102, 0), Some(r(33)));
    }

    #[test]
    fn a_fill_with_a_block_position_counts_trades_by_position_not_arrival() {
        let feed = Feed::default();
        // Seen before the fill was marked, but at a later block position: behind the fill.
        trade_at(&feed, 100, 300, true, lamports(2.0));
        // Seen before, earlier position: ahead.
        trade_at(&feed, 100, 50, true, lamports(5.0));
        feed.mark_paper_fill(Some(100), Some(200));
        // Seen after the mark, earlier position (the stream showed it late): still ahead.
        trade_at(&feed, 100, 120, true, lamports(7.0));
        trade_at(&feed, 101, 4, true, lamports(1.0));
        let behind = feed.behind_within_slots(4);
        assert_eq!(behind.buy_lamports, lamports(3.0));
        assert_eq!(feed.behind_fill(Instant::now()).buy_lamports, lamports(3.0));
    }

    #[test]
    fn a_paper_fill_counts_only_the_trades_after_it() {
        let feed = Feed::default();
        let opened = Instant::now();
        trade(&feed, true, 2_000_000_000);
        feed.mark_paper_fill(Some(100), None);
        trade(&feed, true, 1_500_000_000);
        trade(&feed, false, 300_000_000);
        let behind = feed.behind_fill(opened);
        assert_eq!((behind.buys, behind.buy_lamports), (1, 1_500_000_000));
        assert_eq!((behind.sells, behind.sell_lamports), (1, 300_000_000));
        // A second mark does not move the fill.
        feed.mark_paper_fill(Some(100), None);
        assert_eq!(feed.behind_fill(opened), behind);
    }

    #[test]
    fn the_wait_window_counts_the_buy_slot_and_the_next_ones() {
        let feed = Feed::default();
        assert_eq!(feed.behind_within_slots(4), Flow::default(), "no fill yet");
        trade_in(&feed, 100, true, 5_000_000_000);
        feed.mark_paper_fill(Some(100), None);
        trade_in(&feed, 100, true, 1_000_000_000);
        trade_in(&feed, 104, true, 2_000_000_000);
        trade_in(&feed, 104, false, 500_000_000);
        trade_in(&feed, 105, true, 9_000_000_000);
        let early = feed.behind_within_slots(4);
        assert_eq!((early.buy_lamports, early.sell_lamports), (3_000_000_000, 500_000_000));
        assert_eq!(feed.fill_slot(), Some(100));
    }

    #[test]
    fn a_transaction_is_recorded_once() {
        let signature = Signature::from([7u8; 64]);
        assert!(first_time(&signature));
        assert!(!first_time(&signature), "the event record's call sees the same transaction");
        assert!(first_time(&Signature::from([8u8; 64])));
    }
}
