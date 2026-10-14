//! bundle10's shadow log (BUNDLE10_SHADOW=true; the owner's spec of 2026-10-13, section 3). For
//! every early-path launch with a dev buy, the engine hands over bundle10's view at its window's
//! end (`hot::bundle10::Rec`, one ring per decider, never waited on: a full ring drops the record
//! and counts it). Here, off the hot path:
//! - the regular stream's executed trades of each candidate (trade events at any CPI depth, with
//!   the block index): the exact count, whether each transaction counted at the decision executed
//!   (a phantom when not: the stream carries executed transactions only), the next trades' places;
//! - the slow path's count (`shred_feed::bundle_wallets`, noted in `shred_feed::snipe`);
//! - 300 s after the create, a simulated 0.1 SOL trade: entry e2 (the worst of the post-bundle
//!   state and the next two trades of the create slot), bundle10's exit with a 60 s and a 300 s
//!   deadline, the sell at the worst state of the slot after the decision; with the real costs
//!   (each trade's fee bps, the fee engine's bid and the 0slot tip, the sell's modeled cost) and
//!   with the research's (the create's fee bps both ways, 0.0011 SOL a transaction);
//! - what a limit of 1, 3 or 5 open positions and the day's loss stop would have done;
//! - one row in the store's `bundle10_shadow` (`STORE_DB=on`) for each launch where any of the
//!   four counts reaches 8. Daily summary: `tools/bundle10_summary.sql`.
//!
//! Nothing here signs or sends anything.

use {
    crate::{
        config::bundle10::settings,
        hot::{
            bundle10::{self as b10, Count, Exit, Facts, Miss, Rec, Sell, Stop, Trade},
            spsc,
        },
        instructions::{
            pumpfun_buy_ix::{CurveReserves, CREATE_EVENT_DISCRIMINATOR, TRADE_EVENT_DISCRIMINATOR},
            pumpswap_buy_ix::PUMP_PROGRAM_ID,
        },
        processors::common::EVENT_CPI_TAG,
        store::Val,
        utils::{
            dashboard::now_unix_ms,
            parse::{read_pubkey, read_u64},
        },
    },
    carbon_core::transaction::TransactionMetadata,
    log::{info, warn},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::{HashMap, VecDeque},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            LazyLock, Mutex,
        },
        time::Duration,
    },
};

/// Records a decider's ring holds.
const RING: usize = 1024;
/// The simulated trade's size.
pub const SIZE_LAMPORTS: u64 = 100_000_000;
/// The research's fixed cost a transaction.
pub const RESEARCH_TX_LAMPORTS: u64 = 1_100_000;
/// The long simulated hold; rows are written this long after the create, and a little more for
/// the sell's slot.
pub const LONG_HOLD_MS: u64 = 300_000;
const WRITE_AFTER_MS: u64 = LONG_HOLD_MS + 5_000;
/// When the create slot is long over on the regular stream: the counts are final, and a launch
/// whose four counts are all under 8 is let go.
const CHECK_AFTER_MS: u64 = 10_000;
/// Coins the regular stream saw created are kept this long for a record that comes after them.
const RECENT_MS: u64 = 10_000;
/// Trades kept per candidate at most.
const MAX_EVENTS: usize = 60_000;
/// The open-position limits simulated.
pub const LIMITS: [usize; 3] = [1, 3, 5];
/// No part of the rule missed (`Miss` codes start at 60).
const DONE: u8 = 0;

// ---- The rings.

static CONSUMERS: Mutex<Vec<spsc::Consumer<Rec>>> = Mutex::new(Vec::new());
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ROWS: AtomicU64 = AtomicU64::new(0);

/// A ring for one decider (its end for the engine; this log drains the other).
pub fn producer() -> spsc::Producer<Rec> {
    let (p, c) = spsc::channel::<Rec>(RING);
    CONSUMERS.lock().unwrap_or_else(|e| e.into_inner()).push(c);
    p
}

// ---- The regular stream.

/// One trade event of a watched coin on the regular stream (after execution).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ev {
    pub slot: u64,
    /// The transaction's block index.
    pub index: u32,
    /// The trade's place among the transaction's trade events.
    pub k: u16,
    /// When the bot received the transaction (unix ms).
    pub ms: u64,
    pub sig: [u8; 8],
    pub buy: bool,
    pub user: Pubkey,
    pub sol: u64,
    pub tok: u64,
    /// The curve after the trade.
    pub vsol: u64,
    pub vtok: u64,
    pub real_sol: u64,
    pub real_tok: u64,
    /// fee_basis_points + creator_fee_basis_points.
    pub fee_bps: u64,
}

impl Ev {
    pub fn state(&self) -> CurveReserves {
        CurveReserves { virtual_sol_reserves: self.vsol, virtual_token_reserves: self.vtok, real_sol_reserves: self.real_sol, real_token_reserves: self.real_tok }
    }

    pub fn price(&self) -> f64 {
        b10::spot(&self.state())
    }

    /// A pump.fun TradeEvent's body (offsets as `utils::tape_recorder` and `whale` read them).
    pub fn parse(body: &[u8], slot: u64, index: u32, k: u16, ms: u64, sig: [u8; 8]) -> Option<(Pubkey, Ev)> {
        let mint = read_pubkey(body, 0).ok()?;
        let fee = read_u64(body, 153).ok()? + read_u64(body, 201).unwrap_or(0);
        Some((
            mint,
            Ev {
                slot,
                index,
                k,
                ms,
                sig,
                buy: body.get(48) == Some(&1),
                user: read_pubkey(body, 49).ok()?,
                sol: read_u64(body, 32).ok()?,
                tok: read_u64(body, 40).ok()?,
                vsol: read_u64(body, 89).ok()?,
                vtok: read_u64(body, 97).ok()?,
                real_sol: read_u64(body, 105).ok()?,
                real_tok: read_u64(body, 113).ok()?,
                fee_bps: fee,
            },
        ))
    }
}

/// The create as the regular stream saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Created {
    pub slot: u64,
    pub index: u32,
    pub ms: u64,
    pub dev: Pubkey,
    pub creator: Pubkey,
    pub sol_quote: bool,
    pub mayhem: bool,
    /// is_cashback_enabled from the create's event, and from its instruction's arguments as the
    /// hot path would read them (`decode::create_cashback`; None: no top-level create).
    pub cashback_event: bool,
    pub cashback_ix: Option<bool>,
}

/// The regular stream's slot clock: (unix ms, the highest slot seen by then).
#[derive(Default)]
pub struct Clock {
    marks: VecDeque<(u64, u64)>,
}

impl Clock {
    pub fn note(&mut self, ms: u64, slot: u64) {
        if self.marks.back().is_none_or(|(_, s)| slot > *s) {
            self.marks.push_back((ms, slot));
        }
        while self.marks.front().is_some_and(|(t, _)| ms.saturating_sub(*t) > 2 * WRITE_AFTER_MS) {
            self.marks.pop_front();
        }
    }

    /// A clock from (ms, slot) marks in any order, nothing pruned (the replay's whole day).
    pub fn from_marks(mut marks: Vec<(u64, u64)>) -> Self {
        marks.sort_unstable();
        let mut c = Clock::default();
        for (ms, slot) in marks {
            if c.marks.back().is_none_or(|(_, s)| slot > *s) {
                c.marks.push_back((ms, slot));
            }
        }
        c
    }

    /// The time of the clock's last mark (how far the stream's data reaches).
    pub fn last_ms(&self) -> u64 {
        self.marks.back().map_or(0, |(t, _)| *t)
    }

    /// The highest slot seen by `ms`.
    pub fn slot_at(&self, ms: u64) -> Option<u64> {
        let i = self.marks.partition_point(|(t, _)| *t <= ms);
        (i > 0).then(|| self.marks[i - 1].1)
    }
}

struct Candidate {
    rec: Rec,
    created: Option<Created>,
    events: Vec<Ev>,
    slow: Option<Option<u32>>,
    checked: bool,
}

#[derive(Default)]
struct State {
    candidates: HashMap<Pubkey, Candidate>,
    recent: HashMap<Pubkey, (Created, Vec<Ev>)>,
    slow: HashMap<Pubkey, (Option<u32>, u64)>,
    clock: Clock,
    last_sigs: VecDeque<Signature>,
    book: Book,
    day: Day,
}

#[derive(Default, Clone, Copy)]
struct Day {
    day: u64,
    creates: u64,
    kept: u64,
    rows: u64,
    matches: u64,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The slow path's count of a create (`shred_feed::snipe`, before any of its skips).
pub fn note_slow_count(mint: &Pubkey, count: Option<u32>) {
    let mut st = state();
    if let Some(c) = st.candidates.get_mut(mint) {
        c.slow = Some(count);
    } else {
        st.slow.insert(*mint, (count, now_unix_ms()));
    }
}

/// A pump.fun transaction from the regular stream (called once per pump.fun instruction: each
/// transaction is taken once).
pub fn observe(tx: &TransactionMetadata) {
    let ms = now_unix_ms();
    let mut st = state();
    st.clock.note(ms, tx.slot);
    if st.last_sigs.contains(&tx.signature) {
        return;
    }
    st.last_sigs.push_back(tx.signature);
    if st.last_sigs.len() > 64 {
        st.last_sigs.pop_front();
    }
    let Some(index) = tx.index.and_then(|i| u32::try_from(i).ok()) else { return };
    let sig: [u8; 8] = tx.signature.as_ref()[..8].try_into().expect("8");
    // The transaction's instructions (inner ones at any depth) once, its pump.fun events from them.
    let ixs = super::common::transaction_instructions(tx);
    let events = |disc: &[u8; 8]| -> Vec<&[u8]> {
        ixs.iter()
            .filter(|ix| ix.program_id == PUMP_PROGRAM_ID && ix.data.len() >= 16 && ix.data[..8] == EVENT_CPI_TAG && ix.data[8..16] == *disc)
            .map(|ix| &ix.data[16..])
            .collect()
    };
    let creates = events(&CREATE_EVENT_DISCRIMINATOR);
    if !creates.is_empty() {
        let coins = super::pumpfun::created_coins(tx);
        for body in creates.iter().copied() {
            let Some((mint, cashback_event)) = super::pumpfun::create_event_cashback(body) else { continue };
            let Some(coin) = coins.iter().find(|c| c.mint == mint) else { continue };
            let cashback_ix = ixs.iter().filter(|ix| ix.program_id == PUMP_PROGRAM_ID && ix.accounts.first() == Some(&mint)).find_map(|ix| crate::hot::decode::create_cashback(&ix.data));
            let created = Created { slot: tx.slot, index, ms, dev: coin.user, creator: coin.creator, sol_quote: coin.sol_quote, mayhem: coin.mayhem, cashback_event, cashback_ix };
            match st.candidates.get_mut(&mint) {
                Some(c) => c.created = Some(created),
                None => {
                    st.recent.insert(mint, (created, Vec::new()));
                }
            }
        }
    }
    if st.candidates.is_empty() && st.recent.is_empty() {
        return;
    }
    for (k, body) in events(&TRADE_EVENT_DISCRIMINATOR).into_iter().enumerate() {
        let Some((mint, ev)) = Ev::parse(body, tx.slot, index, k as u16, ms, sig) else { continue };
        if let Some(c) = st.candidates.get_mut(&mint) {
            if c.events.len() < MAX_EVENTS {
                c.events.push(ev);
            }
        } else if let Some((_, evs)) = st.recent.get_mut(&mint) {
            evs.push(ev);
        }
    }
}

fn register(rec: Rec) {
    let mut st = state();
    let today = rec.recv_unix_ms / 86_400_000;
    if st.day.day != today {
        st.day = Day { day: today, ..Default::default() };
    }
    st.day.creates += 1;
    let (created, events) = st.recent.remove(&rec.mint).map_or((None, Vec::new()), |(c, e)| (Some(c), e));
    let slow = st.slow.remove(&rec.mint).map(|(c, _)| c);
    st.candidates.insert(rec.mint, Candidate { rec, created, events, slow, checked: false });
    ACTIVE.store(true, Ordering::Relaxed);
}

// ---- The exact view.

/// The regular stream's view of a create's bundle (`Count` with the block index) and around it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exact {
    pub count: u32,
    pub txs: u8,
    pub stop: u8,
    pub mixed: bool,
    pub overflow: bool,
    /// Position in the events of E0 (the bundle's last trade; the create's own last trade when
    /// nothing was counted).
    pub e0: usize,
    pub dev_buy_lamports: u64,
    pub bundle_lamports: u64,
    pub largest: u64,
    pub post_mult: f64,
    /// The create's first trade's fee bps (the research's cost model).
    pub create_fee_bps: u64,
    pub bundle_last_index: u32,
    /// The block indices of the next three transactions trading the coin in the create slot.
    pub next: [Option<u32>; 3],
}

/// `events` sorted by (slot, index, k), the coin's only.
pub fn exact(mint: &Pubkey, cr: &Created, events: &[Ev]) -> Option<Exact> {
    let first = events.iter().position(|e| e.slot == cr.slot && e.index == cr.index)?;
    let create_last = events.iter().rposition(|e| e.slot == cr.slot && e.index == cr.index)?;
    let mut count = Count::new(*mint, cr.dev, cr.creator, Some(cr.index));
    let mut last = None;
    let mut largest = 0;
    for (i, e) in events.iter().enumerate().filter(|(_, e)| e.slot == cr.slot && e.index > cr.index) {
        let t = Trade { mint: *mint, buy: e.buy, user: Some(e.user), tokens: e.tok, lamports: e.sol };
        if count.event(Some(e.index), e.sig, e.k, &t) {
            last = Some(i);
            if e.buy {
                largest = largest.max(e.sol);
            }
        }
    }
    let e0 = last.unwrap_or(create_last);
    let after_dev = &events[create_last];
    let bundle_last_index = events[e0].index;
    let mut next = [None; 3];
    let mut n = 0;
    for e in events.iter().filter(|e| e.slot == cr.slot && e.index > bundle_last_index) {
        if n < 3 && next[..n].iter().all(|x| *x != Some(e.index)) {
            next[n] = Some(e.index);
            n += 1;
        }
    }
    Some(Exact {
        count: count.wallets(),
        txs: count.txs() as u8,
        stop: Stop::code(count.stop()),
        mixed: count.mixed(),
        overflow: count.overflow(),
        e0,
        dev_buy_lamports: after_dev.real_sol,
        bundle_lamports: events[e0].real_sol.saturating_sub(after_dev.real_sol),
        largest,
        post_mult: events[e0].price() / b10::launch_price(),
        create_fee_bps: events[first].fee_bps,
        bundle_last_index,
        next,
    })
}

/// Whether each transaction counted at the decision executed with a trade of the coin in the
/// create slot: 0 executed, 1 not (a phantom), 2 unknown (the create not seen on the stream).
pub fn phantoms(rec: &Rec, cr: Option<&Created>, events: &[Ev]) -> [u8; b10::MAX_TXS] {
    let mut out = [0u8; b10::MAX_TXS];
    for (i, sig) in rec.sigs[..rec.txs as usize].iter().enumerate() {
        out[i] = match cr {
            None => 2,
            Some(cr) => u8::from(!events.iter().any(|e| e.slot == cr.slot && e.sig == *sig)),
        };
    }
    out
}

// ---- The simulated trade.

/// The fee bps a simulated trade pays: each state's own (the real model) or one for all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fees {
    PerTrade,
    Fixed(u64),
}

impl Fees {
    fn at(self, e: &Ev) -> u64 {
        match self {
            Fees::PerTrade => e.fee_bps,
            Fees::Fixed(bps) => bps,
        }
    }
}

/// Why the simulated position sold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimExit {
    Trail,
    Deadline,
    /// The curve would complete (with our tokens out of it): valued at the last curve state before.
    Graduated,
    /// The data ends before the deadline (the replay's 120 s tapes).
    EndOfData,
}

impl SimExit {
    pub fn name(self) -> &'static str {
        match self {
            SimExit::Trail => "trail",
            SimExit::Deadline => "deadline",
            SimExit::Graduated => "graduated",
            SimExit::EndOfData => "end_of_data",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sim {
    /// Which state set e2: 0 the post-bundle state, 1 or 2 the next trades.
    pub e2_from: u8,
    pub e2_price: f64,
    pub fill: f64,
    pub tokens: u64,
    pub exit: SimExit,
    /// When the exit was decided (the stream's clock, unix ms).
    pub exit_ms: u64,
    pub sell_price: f64,
    pub peak: f64,
    pub armed: bool,
    /// SOL back from the sell, its fee out.
    pub proceeds: u64,
}

/// One simulated buy of `size` lamports at e2 after E0 (`events[e0]`), sold by bundle10's exit
/// with `hold_ms` from `create_ms` (the create's time on the same clock as the events'). `events`:
/// the coin's, sorted by (slot, index, k), every trade until `data_until_ms`. The price of a
/// state is its spot with our buy in the reserves; a trigger in slot s sells at the lowest of the
/// decision state and every state of slot s+1; the deadline's decision state is the last at or
/// before it, s the slot at it.
#[allow(clippy::too_many_arguments)]
pub fn simulate(events: &[Ev], e0: usize, create_slot: u64, create_ms: u64, hold_ms: u64, trail: f64, size: u64, fees: Fees, clock: &Clock, data_until_ms: u64) -> Sim {
    // Entry: the worst (highest) price of E0 and the next two trades of the create slot.
    let mut entry = e0;
    for i in (e0 + 1..events.len()).take(2) {
        if events[i].slot == create_slot && events[i].price() > events[entry].price() {
            entry = i;
        }
    }
    let e2_from = (entry - e0) as u8;
    let at = &events[entry];
    let (tokens, after) = b10::our_buy(at.state(), size, fees.at(at));
    let net = after.virtual_sol_reserves - at.vsol;
    let ours = |e: &Ev| CurveReserves {
        virtual_sol_reserves: e.vsol + net,
        virtual_token_reserves: e.vtok.saturating_sub(tokens).max(1),
        real_sol_reserves: e.real_sol + net,
        real_token_reserves: e.real_tok.saturating_sub(tokens),
    };
    let fill = b10::spot(&after);
    let s = crate::config::bundle10::Settings { max_hold_ms: hold_ms, trail, ..crate::config::bundle10::Settings::DEFAULT };
    let mut exit = Exit::new(fill, create_ms, &s);
    let deadline = exit.deadline_ms;
    // The lowest state among `decision` and the coin's states in slot `slot + 1` after `from`.
    let worst_next = |decision: (f64, CurveReserves, u64), from: usize, slot: u64| -> (f64, CurveReserves, u64) {
        let mut w = decision;
        for e in events[from..].iter().filter(|e| e.slot == slot + 1) {
            let r = ours(e);
            let p = b10::spot(&r);
            if p < w.0 {
                w = (p, r, fees.at(e));
            }
        }
        w
    };
    let mut decision = (fill, after, fees.at(at));
    let mut out = None;
    for i in entry + 1..events.len() {
        let e = &events[i];
        if e.ms > deadline {
            break;
        }
        if e.real_tok <= tokens {
            out = Some((SimExit::Graduated, e.ms, decision));
            break;
        }
        let r = ours(e);
        let p = b10::spot(&r);
        decision = (p, r, fees.at(e));
        if exit.on_trade(p, e.ms) == Some(Sell::Trail) {
            out = Some((SimExit::Trail, e.ms, worst_next(decision, i + 1, e.slot)));
            break;
        }
    }
    let (reason, exit_ms, (sell_price, state, bps)) = out.unwrap_or_else(|| match clock.slot_at(deadline).filter(|_| data_until_ms > deadline) {
        // Every state of slot s+1 comes after the decision state (s is at or past its slot).
        Some(slot) => (SimExit::Deadline, deadline, worst_next(decision, entry + 1, slot)),
        None => (SimExit::EndOfData, events.last().map_or(create_ms, |e| e.ms).min(deadline), decision),
    });
    Sim { e2_from, e2_price: at.price(), fill, tokens, exit: reason, exit_ms, sell_price, peak: exit.peak, armed: exit.armed, proceeds: b10::sell_proceeds(&state, tokens, bps) }
}

// ---- Positions and the day's loss stop, simulated.

/// Would-pass trades in decision order: which a limit of 1, 3 or 5 open positions takes, and the
/// day's realized total (the 60 s exit, real costs, limit 5) before each decision.
#[derive(Default)]
pub struct Book {
    open: [Vec<u64>; 3],
    all: Vec<u64>,
    /// Limit 5's closed positions today: (exit ms, P&L lamports).
    closed: Vec<(u64, i64)>,
    day: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Taken {
    pub open_at_decision: u32,
    pub taken: [bool; 3],
    pub day_running: i64,
    pub day_stopped: bool,
}

impl Book {
    pub fn take(&mut self, decision_ms: u64, exit_ms: u64, pnl: i64, day_loss: u64) -> Taken {
        let today = decision_ms / 86_400_000;
        if today != self.day {
            self.day = today;
            self.closed.retain(|(t, _)| t / 86_400_000 == today);
        }
        self.all.retain(|t| *t > decision_ms);
        let open_at_decision = self.all.len() as u32;
        self.all.push(exit_ms);
        let day_running: i64 = self.closed.iter().filter(|(t, _)| *t <= decision_ms).map(|(_, p)| p).sum();
        let mut taken = [false; 3];
        for (k, limit) in LIMITS.iter().enumerate() {
            self.open[k].retain(|t| *t > decision_ms);
            if self.open[k].len() < *limit {
                self.open[k].push(exit_ms);
                taken[k] = true;
            }
        }
        if taken[2] {
            self.closed.push((exit_ms, pnl));
        }
        Taken { open_at_decision, taken, day_running, day_stopped: day_running <= -(day_loss as i64) }
    }
}

// ---- The row.

/// The table's columns, in order (migration 0004).
pub const COLUMNS: &[&str] = &[
    "rx_ns", "run_id", "mint", "deployer", "creator", "create_slot", "create_index", "recv_unix_ms", "create_stream_unix_ms", "feed",
    "mayhem", "cashback_event", "cashback_ix", "token_2022", "sol_quote",
    "count_decision", "count_exact", "count_hot", "count_slow",
    "txs_decision", "txs_exact", "stop_decision", "stop_exact", "unreadable_decision", "trades_decision",
    "mixed_decision", "mixed_exact", "too_many_buys",
    "dev_buy_sol", "bundle_sol", "bundle_sol_exact", "bundle_sol_hot", "largest_buy_sol", "largest_buy_sol_exact",
    "post_mult", "post_mult_exact",
    "would_pass", "miss", "would_pass_exact", "miss_exact",
    "blocked_by_15sol_cap", "blocked_by_fee_route", "blocked_by_ceiling", "far_producer", "distance_go",
    "existing_reject", "existing_bought",
    "window_us", "compute_us", "decision_latency_us",
    "create_seen_exact", "phantom_flags", "phantoms",
    "bundle_last_index", "next_index_1", "next_index_2", "next_index_3",
    "open_at_decision", "taken_l1", "taken_l3", "taken_l5", "day_running_sol", "day_stopped",
    "e2_from", "e2_mult", "fill_mult", "tokens",
    "exit60", "exit60_unix_ms", "sell60_mult", "peak60_mult", "pnl60_real_sol", "pnl60_research_sol",
    "exit300", "exit300_unix_ms", "sell300_mult", "pnl300_real_sol", "pnl300_research_sol",
    "graduated", "pool_first_price", "buy_cost_sol", "sell_cost_sol", "create_fee_bps", "ring_drops",
];

static INSERT: LazyLock<&'static str> = LazyLock::new(|| {
    let cols = COLUMNS.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
    let marks = (1..=COLUMNS.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
    Box::leak(format!("INSERT INTO bundle10_shadow ({cols}) VALUES ({marks})").into_boxed_str())
});

fn sol(l: i64) -> Val {
    Val::F(l as f64 / 1e9)
}
fn int(v: impl Into<i64>) -> Val {
    Val::I(v.into())
}
fn flag(b: bool) -> Val {
    Val::I(b as i64)
}
fn opt<T: Into<i64>>(v: Option<T>) -> Val {
    v.map_or(Val::Null, |v| Val::I(v.into()))
}
fn real(x: f64) -> Val {
    if x.is_finite() { Val::F(x) } else { Val::Null }
}

/// The buy's real cost: base fee, the fee engine's bid on the compute limit, the 0slot tip.
pub fn buy_cost(rec: &Rec) -> u64 {
    5_000 + (rec.buy_cu_price as u128 * rec.buy_cu_limit as u128 / 1_000_000) as u64 + rec.buy_tip
}

/// The sell's real cost as the bot models a landed sell (base, priority on the limit, tips).
pub fn sell_cost() -> u64 {
    crate::service::modeled_cost_lamports(crate::service::SendProfile::Sell)
}

/// The rule on the decision's view, with the create event's cashback flag.
fn decision_rule(rec: &Rec, cashback: bool) -> u8 {
    if rec.miss == Miss::Config.code() || rec.miss == Miss::Layout.code() {
        return rec.miss;
    }
    let f = Facts {
        layout_known: true,
        sol_quote: true,
        initial: rec.initial,
        mayhem: rec.mayhem,
        cashback,
        dev_buy_lamports: rec.dev_buy_lamports,
        count: rec.count,
        overflow: rec.overflow,
        bundle_lamports: rec.bundle_lamports,
        post_bundle_mult: rec.post_mult,
    };
    b10::rule(&f, settings()).err().map_or(DONE, Miss::code)
}

/// Everything a row holds, in `COLUMNS` order. `None` when no count reaches 8.
fn row(c: &mut Candidate, book: &mut Book, clock: &Clock, now_ns: i64, sell_cost: u64) -> Option<(Vec<Val>, bool)> {
    c.events.sort_by_key(|e| (e.slot, e.index, e.k));
    let rec = &c.rec;
    let cr = c.created;
    let ex = cr.as_ref().and_then(|cr| exact(&rec.mint, cr, &c.events));
    let slow = c.slow.flatten();
    let counts = [Some(rec.count), ex.map(|e| e.count), rec.hot_count, slow];
    if !counts.iter().flatten().any(|n| *n >= b10::SHADOW_MIN_COUNT) {
        return None;
    }
    let cashback = cr.is_some_and(|c| c.cashback_event);
    let miss = decision_rule(rec, cashback);
    let would_pass = miss == DONE;
    let s = settings();
    let miss_exact = match (cr, ex) {
        (Some(cr), Some(ex)) => {
            let f = Facts {
                layout_known: rec.miss != Miss::Layout.code(),
                sol_quote: cr.sol_quote,
                initial: rec.initial,
                mayhem: cr.mayhem,
                cashback,
                dev_buy_lamports: ex.dev_buy_lamports,
                count: ex.count,
                overflow: ex.overflow,
                bundle_lamports: ex.bundle_lamports,
                post_bundle_mult: ex.post_mult,
            };
            Some(b10::rule(&f, s).err().map_or(DONE, Miss::code))
        }
        _ => None,
    };
    let flags = phantoms(rec, cr.as_ref(), &c.events);
    let shown = &flags[..rec.txs as usize];
    let phantom_text = shown.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",");
    let buy_cost = buy_cost(rec);
    let launch = b10::launch_price();
    // The simulated trade (the create seen on the stream, its trades after it).
    let sims = match (cr, ex) {
        (Some(cr), Some(ex)) => {
            let until = clock.last_ms();
            let run = |hold, fees| simulate(&c.events, ex.e0, cr.slot, cr.ms, hold, s.trail, SIZE_LAMPORTS, fees, clock, until);
            Some((
                run(s.max_hold_ms, Fees::PerTrade),
                run(s.max_hold_ms, Fees::Fixed(ex.create_fee_bps)),
                run(LONG_HOLD_MS, Fees::PerTrade),
                run(LONG_HOLD_MS, Fees::Fixed(ex.create_fee_bps)),
                cr,
            ))
        }
        _ => None,
    };
    let pnl_real = |sim: &Sim| sim.proceeds as i64 - SIZE_LAMPORTS as i64 - buy_cost as i64 - sell_cost as i64;
    let pnl_research = |sim: &Sim| sim.proceeds as i64 - SIZE_LAMPORTS as i64 - 2 * RESEARCH_TX_LAMPORTS as i64;
    // The open-position limits and the day's loss stop (would-pass trades, the hot path's clock).
    let taken = match (&sims, would_pass) {
        (Some((short, _, _, _, cr)), true) => {
            let exit_hot = short.exit_ms.saturating_sub(cr.ms.saturating_sub(rec.recv_unix_ms));
            Some(book.take(rec.recv_unix_ms, exit_hot, pnl_real(short), s.day_loss_lamports))
        }
        _ => None,
    };
    let v = vec![
        Val::I(now_ns),
        Val::T(crate::store::run_id().to_string()),
        Val::T(rec.mint.to_string()),
        Val::T(rec.deployer.to_string()),
        Val::T(rec.creator.to_string()),
        int(rec.slot as i64),
        opt(cr.map(|c| c.index)),
        int(rec.recv_unix_ms as i64),
        opt(cr.map(|c| c.ms as i64)),
        int(rec.feed),
        flag(rec.mayhem),
        cr.map_or(Val::Null, |c| flag(c.cashback_event)),
        cr.and_then(|c| c.cashback_ix).map_or(Val::Null, flag),
        flag(rec.token_2022),
        cr.map_or(Val::Null, |c| flag(c.sol_quote)),
        int(rec.count),
        opt(ex.map(|e| e.count)),
        opt(rec.hot_count),
        opt(slow),
        int(rec.txs),
        opt(ex.map(|e| e.txs)),
        Val::T(Stop::name(rec.stop).into()),
        ex.map_or(Val::Null, |e| Val::T(Stop::name(e.stop).into())),
        int(rec.unreadable),
        int(rec.trades),
        flag(rec.mixed),
        ex.map_or(Val::Null, |e| flag(e.mixed)),
        flag(rec.overflow),
        sol(rec.dev_buy_lamports as i64),
        sol(rec.bundle_lamports as i64),
        ex.map_or(Val::Null, |e| sol(e.bundle_lamports as i64)),
        sol(rec.hot_bundle_lamports as i64),
        sol(rec.largest as i64),
        ex.map_or(Val::Null, |e| sol(e.largest as i64)),
        real(rec.post_mult),
        ex.map_or(Val::Null, |e| real(e.post_mult)),
        flag(would_pass),
        Val::T(Miss::name_of(miss).into()),
        miss_exact.map_or(Val::Null, |m| flag(m == DONE)),
        miss_exact.map_or(Val::Null, |m| Val::T(Miss::name_of(m).into())),
        flag(b10::blocked_by_15sol_cap(rec.bundle_lamports)),
        flag(rec.fee_route_skip),
        flag(!(rec.post_mult <= s.max_post_bundle_mult)),
        flag(rec.far_producer),
        flag(rec.distance_go),
        int(rec.existing_reject),
        flag(rec.existing_bought),
        int((rec.window_ns / 1_000) as i64),
        int((rec.compute_ns / 1_000) as i64),
        int(((rec.window_ns + rec.compute_ns) / 1_000) as i64),
        flag(cr.is_some()),
        Val::T(phantom_text),
        int(shown.iter().filter(|f| **f == 1).count() as i64),
        opt(ex.map(|e| e.bundle_last_index)),
        opt(ex.and_then(|e| e.next[0])),
        opt(ex.and_then(|e| e.next[1])),
        opt(ex.and_then(|e| e.next[2])),
        opt(taken.map(|t| t.open_at_decision)),
        taken.map_or(Val::Null, |t| flag(t.taken[0])),
        taken.map_or(Val::Null, |t| flag(t.taken[1])),
        taken.map_or(Val::Null, |t| flag(t.taken[2])),
        taken.map_or(Val::Null, |t| sol(t.day_running)),
        taken.map_or(Val::Null, |t| flag(t.day_stopped)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| int(a.e2_from)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| real(a.e2_price / launch)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| real(a.fill / launch)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| int(a.tokens as i64)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| Val::T(a.exit.name().into())),
        sims.as_ref().map_or(Val::Null, |(a, ..)| int(a.exit_ms as i64)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| real(a.sell_price / launch)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| real(a.peak / launch)),
        sims.as_ref().map_or(Val::Null, |(a, ..)| sol(pnl_real(a))),
        sims.as_ref().map_or(Val::Null, |(_, b, ..)| sol(pnl_research(b))),
        sims.as_ref().map_or(Val::Null, |(_, _, l, ..)| Val::T(l.exit.name().into())),
        sims.as_ref().map_or(Val::Null, |(_, _, l, ..)| int(l.exit_ms as i64)),
        sims.as_ref().map_or(Val::Null, |(_, _, l, ..)| real(l.sell_price / launch)),
        sims.as_ref().map_or(Val::Null, |(_, _, l, ..)| sol(pnl_real(l))),
        sims.as_ref().map_or(Val::Null, |(_, _, _, m, _)| sol(pnl_research(m))),
        sims.as_ref().map_or(Val::Null, |(a, _, l, ..)| flag(a.exit == SimExit::Graduated || l.exit == SimExit::Graduated)),
        // PumpSwap's transactions are not on the stream unless SNIPE_TEST_POOL is on.
        Val::Null,
        sol(buy_cost as i64),
        sol(sell_cost as i64),
        opt(ex.map(|e| e.create_fee_bps as i64)),
        int(b10::DROPPED.load(Ordering::Relaxed) as i64),
    ];
    debug_assert_eq!(v.len(), COLUMNS.len());
    Some((v, would_pass))
}

// ---- The loop.

/// Rebuilds today's simulated positions from the store after a restart.
pub fn init() -> usize {
    if !crate::store::db_on() {
        warn!("bundle10 shadow: the store is off (STORE_DB=on writes the bundle10_shadow rows); rows are logged only");
        return 0;
    }
    let start = now_unix_ms() / 86_400_000 * 86_400_000;
    // The exit on the hot path's clock, as the book keeps it (the stream's clock less the create's
    // delay on the stream).
    let rows: Vec<(i64, i64, f64)> = crate::store::migrate::open_reader(&crate::store::path())
        .and_then(|c| {
            let mut q = c.prepare(
                "SELECT recv_unix_ms, exit60_unix_ms - (create_stream_unix_ms - recv_unix_ms), pnl60_real_sol FROM bundle10_shadow \
                 WHERE recv_unix_ms >= ?1 AND would_pass = 1 AND pnl60_real_sol IS NOT NULL AND create_stream_unix_ms IS NOT NULL ORDER BY recv_unix_ms",
            )?;
            let r = q.query_map([start as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>();
            r
        })
        .unwrap_or_default();
    let mut st = state();
    for (decided, exit, pnl) in &rows {
        st.book.take(*decided as u64, *exit as u64, (*pnl * 1e9).round() as i64, settings().day_loss_lamports);
    }
    rows.len()
}

/// The shadow log's task: drains the rings every 20 ms, finishes candidates once a second.
pub fn spawn() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut n: u64 = 0;
        let mut logged_drops = 0;
        loop {
            tick.tick().await;
            let recs: Vec<Rec> = {
                let mut cons = CONSUMERS.lock().unwrap_or_else(|e| e.into_inner());
                cons.iter_mut().flat_map(|c| std::iter::from_fn(move || c.pop())).collect()
            };
            for rec in recs {
                register(rec);
            }
            n += 1;
            if n % 50 != 0 {
                continue;
            }
            finish(now_unix_ms());
            let drops = b10::DROPPED.load(Ordering::Relaxed);
            if drops > logged_drops {
                warn!("bundle10 shadow: {} records dropped so far (a full ring; the engine never waits)", drops);
                logged_drops = drops;
            }
            if n % 3_000 == 0 {
                publish();
            }
        }
    });
}

/// Lets go of candidates with every count under 8, writes those 300 s old.
fn finish(now: u64) {
    let mut out = Vec::new();
    {
        let mut st = state();
        let State { candidates, recent, slow, clock, book, day, .. } = &mut *st;
        recent.retain(|_, (c, _)| now.saturating_sub(c.ms) < RECENT_MS);
        slow.retain(|_, (_, t)| now.saturating_sub(*t) < 60_000);
        // The counts are final: keep only launches where one reaches 8.
        let mut gone = Vec::new();
        for (mint, c) in candidates.iter_mut().filter(|(_, c)| !c.checked && now >= c.rec.recv_unix_ms + CHECK_AFTER_MS) {
            c.checked = true;
            if c.slow.is_none() {
                c.slow = slow.remove(mint).map(|(n, _)| n);
            }
            c.events.sort_by_key(|e| (e.slot, e.index, e.k));
            let ex = c.created.as_ref().and_then(|cr| exact(mint, cr, &c.events)).map(|e| e.count);
            if [Some(c.rec.count), ex, c.rec.hot_count, c.slow.flatten()].iter().flatten().any(|n| *n >= b10::SHADOW_MIN_COUNT) {
                day.kept += 1;
            } else {
                gone.push(*mint);
            }
        }
        for m in gone {
            candidates.remove(&m);
        }
        let mut due: Vec<Pubkey> = candidates.iter().filter(|(_, c)| now >= c.rec.recv_unix_ms + WRITE_AFTER_MS).map(|(m, _)| *m).collect();
        due.sort_by_key(|m| candidates[m].rec.recv_unix_ms);
        let ns = crate::store::now_ns();
        let sell = sell_cost();
        for m in due {
            let Some(mut c) = candidates.remove(&m) else { continue };
            if let Some((vals, pass)) = row(&mut c, book, clock, ns, sell) {
                day.rows += 1;
                day.matches += pass as u64;
                out.push((m, vals, pass));
            }
        }
        ACTIVE.store(!candidates.is_empty(), Ordering::Relaxed);
    }
    for (mint, vals, pass) in out {
        ROWS.fetch_add(1, Ordering::Relaxed);
        if pass {
            info!("bundle10 shadow: {mint} would pass; row written");
        }
        if crate::store::db_on() {
            crate::store::write(crate::store::Row::Exec { sql: *INSERT, params: vals });
        }
    }
}

/// The day's counters to the store's kv table (the dashboard's and the scripts').
fn publish() {
    let d = state().day;
    info!("bundle10 shadow today: {} launches with a dev buy, {} kept (a count >= 8), {} rows, {} would pass; {} records dropped", d.creates, d.kept, d.rows, d.matches, b10::DROPPED.load(Ordering::Relaxed));
    if !crate::store::db_on() {
        return;
    }
    crate::store::write(crate::store::Row::Exec {
        sql: "INSERT OR REPLACE INTO kv (key, value, updated_ns) VALUES ('bundle10_shadow', ?1, ?2)",
        params: vec![
            format!(
                "{{\"day\":{},\"launches\":{},\"kept\":{},\"rows\":{},\"would_pass\":{},\"ring_drops\":{}}}",
                d.day,
                d.creates,
                d.kept,
                d.rows,
                d.matches,
                b10::DROPPED.load(Ordering::Relaxed)
            )
            .into(),
            crate::store::now_ns().into(),
        ],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 1_791_900_000_000;
    /// A landed sell's cost in the tests (the bot's model needs its settings).
    const SELL: u64 = 1_005_000;

    /// A trade after which the curve has `vsol` virtual SOL (tokens from the constant product).
    fn ev(slot: u64, index: u32, k: u16, ms: u64, buy: bool, user: Pubkey, sol: u64, vsol: u64) -> Ev {
        let vtok = (b10::INITIAL_VSOL as u128 * b10::INITIAL_VTOK as u128 / vsol as u128) as u64;
        Ev {
            slot,
            index,
            k,
            ms,
            // One signature a transaction (its trades share it).
            sig: [(index % 251) as u8, (index / 251 % 251) as u8, (slot % 251) as u8, 1, 2, 3, 4, 5],
            buy,
            user,
            sol,
            tok: 0,
            vsol,
            vtok,
            real_sol: vsol - b10::INITIAL_VSOL,
            real_tok: vtok - 279_900_000_000_000,
            fee_bps: 125,
        }
    }

    fn created(dev: Pubkey, creator: Pubkey) -> Created {
        Created { slot: 100, index: 10, ms: START, dev, creator, sol_quote: true, mayhem: false, cashback_event: false, cashback_ix: Some(false) }
    }

    /// A create at index 10 of slot 100 (dev buy 2 SOL), a bundle of `txs` transactions of 3
    /// wallets each at 11.., then `after`.
    fn coin(txs: u32, after: &[Ev]) -> (Pubkey, Created, Vec<Ev>) {
        let (mint, dev, creator) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let mut v = vec![ev(100, 10, 0, START, true, dev, 2_000_000_000, 32_000_000_000)];
        let mut vsol = 32_000_000_000;
        for i in 0..txs {
            for k in 0..3 {
                vsol += 1_000_000_000;
                v.push(ev(100, 11 + i, k, START, true, Pubkey::new_unique(), 1_000_000_000, vsol));
            }
        }
        v.extend_from_slice(after);
        (mint, created(dev, creator), v)
    }

    fn clock() -> Clock {
        let mut c = Clock::default();
        for i in 0..1_000u64 {
            c.note(START + i * 400, 100 + i);
        }
        c
    }

    #[test]
    fn the_exact_count_takes_the_contiguous_transactions_after_the_create() {
        let after = [ev(100, 16, 0, START, true, Pubkey::new_unique(), 1, 45_000_000_000)];
        let (mint, cr, events) = coin(4, &after);
        let x = exact(&mint, &cr, &events).unwrap();
        assert_eq!((x.count, x.txs, x.stop, x.e0, x.bundle_last_index), (12, 4, 3, 12, 14));
        assert_eq!(x.dev_buy_lamports, 2_000_000_000);
        assert_eq!(x.bundle_lamports, 12_000_000_000);
        assert_eq!(x.next, [Some(16), None, None]);
        assert_eq!(x.create_fee_bps, 125);
        // A gap at 13: the count is the first two transactions.
        let (mint, cr, mut events) = coin(4, &[]);
        events.retain(|e| e.index != 13);
        let x = exact(&mint, &cr, &events).unwrap();
        assert_eq!((x.count, x.txs, x.stop, x.next[0]), (6, 2, 2, Some(14)));
    }

    #[test]
    fn a_counted_transaction_that_never_executed_is_a_phantom() {
        let (mint, cr, events) = coin(2, &[]);
        let mut rec = test_rec(mint);
        rec.txs = 3;
        rec.sigs[0] = events[1].sig;
        rec.sigs[1] = events[4].sig;
        rec.sigs[2] = [9; 8];
        assert_eq!(phantoms(&rec, Some(&cr), &events), [0, 0, 1, 0]);
        assert_eq!(phantoms(&rec, None, &events), [2, 2, 2, 0]);
    }

    #[test]
    fn e2_is_the_worst_of_the_post_bundle_state_and_the_next_two_trades_of_the_slot() {
        let u = Pubkey::new_unique();
        // E0 at 44 SOL virtual; then 46 (worse) and 43 in slot 100, 50 in slot 101 (not the create slot).
        let after = [ev(100, 20, 0, START, true, u, 1, 46_000_000_000), ev(100, 21, 0, START, false, u, 1, 43_000_000_000), ev(101, 1, 0, START + 400, true, u, 1, 50_000_000_000)];
        let (_, cr, events) = coin(4, &after);
        let sim = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &clock(), u64::MAX);
        assert_eq!(sim.e2_from, 1);
        assert_eq!(sim.e2_price, events[13].price());
        assert!(sim.fill > sim.e2_price);
    }

    #[test]
    fn the_trail_sells_at_the_worst_state_of_the_next_slot() {
        let u = Pubkey::new_unique();
        // Up to 60 SOL (armed, peak), down to 48 (under 0.85 x the peak with our buy in): sold; the
        // next slot has 47 and 49: the sale is at 47.
        let after = [
            ev(101, 1, 0, START + 1_000, true, u, 1, 60_000_000_000),
            ev(102, 1, 0, START + 2_000, false, u, 1, 48_000_000_000),
            ev(103, 1, 0, START + 2_400, false, u, 1, 49_000_000_000),
            ev(103, 2, 0, START + 2_400, false, u, 1, 47_000_000_000),
            ev(104, 1, 0, START + 2_800, false, u, 1, 40_000_000_000),
        ];
        let (_, cr, events) = coin(4, &after);
        let sim = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &clock(), u64::MAX);
        assert_eq!((sim.exit, sim.exit_ms, sim.armed), (SimExit::Trail, START + 2_000, true));
        let net = (SIZE_LAMPORTS as u128 * 10_000 / 10_125) as u64;
        let at47 = b10::spot(&CurveReserves { virtual_sol_reserves: 47_000_000_000 + net, virtual_token_reserves: events[16].vtok - sim.tokens, ..Default::default() });
        assert!((sim.sell_price - at47).abs() < 1e-12, "{} vs {}", sim.sell_price, at47);
        assert!(sim.proceeds < SIZE_LAMPORTS * 2);
    }

    #[test]
    fn the_deadline_sells_at_the_worst_of_the_last_state_and_the_slot_after_the_deadline() {
        let u = Pubkey::new_unique();
        // Never armed; the deadline at START + 60 s falls in slot 250 (the clock); slot 251 has a
        // lower state.
        let after = [
            ev(120, 1, 0, START + 8_000, true, u, 1, 45_000_000_000),
            ev(251, 1, 0, START + 60_500, false, u, 1, 40_000_000_000),
            ev(251, 2, 0, START + 60_500, true, u, 1, 44_000_000_000),
            ev(300, 1, 0, START + 80_000, false, u, 1, 30_000_000_000),
        ];
        let (_, cr, events) = coin(4, &after);
        let c = clock();
        assert_eq!(c.slot_at(START + 60_000), Some(250));
        let sim = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &c, u64::MAX);
        assert_eq!((sim.exit, sim.exit_ms, sim.armed), (SimExit::Deadline, START + 60_000, false));
        let lowest = b10::spot(&CurveReserves {
            virtual_sol_reserves: 40_000_000_000 + (SIZE_LAMPORTS as u128 * 10_000 / 10_125) as u64,
            virtual_token_reserves: events[14].vtok - sim.tokens,
            ..Default::default()
        });
        assert!((sim.sell_price - lowest).abs() < 1e-12);
        // Nothing traded after the entry: sold at the post-buy state.
        let (_, cr, events) = coin(4, &[ev(400, 1, 0, START + 200_000, true, u, 1, 45_000_000_000)]);
        let sim = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &c, u64::MAX);
        assert_eq!((sim.exit, sim.sell_price), (SimExit::Deadline, sim.fill));
        // A coin that stops trading before the deadline is sold at the deadline; only data that
        // ends before the deadline (a 300 s hold on a 120 s tape) is "end of data".
        let (_, cr, events) = coin(4, &[ev(120, 1, 0, START + 8_000, true, u, 1, 45_000_000_000)]);
        let sim = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &c, START + 120_000);
        assert_eq!((sim.exit, sim.exit_ms), (SimExit::Deadline, START + 60_000));
        let sim = simulate(&events, 12, cr.slot, cr.ms, 300_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &c, START + 120_000);
        assert_eq!(sim.exit, SimExit::EndOfData);
    }

    #[test]
    fn a_curve_that_completes_is_valued_at_its_last_state_before_and_flagged() {
        let u = Pubkey::new_unique();
        let mut done = ev(130, 1, 0, START + 9_000, true, u, 1, 115_000_000_000);
        done.real_tok = 0;
        let after = [ev(120, 1, 0, START + 8_000, true, u, 1, 60_000_000_000), done];
        let (_, cr, events) = coin(4, &after);
        let sim = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &clock(), u64::MAX);
        assert_eq!((sim.exit, sim.exit_ms), (SimExit::Graduated, START + 9_000));
        let at60 = b10::spot(&CurveReserves {
            virtual_sol_reserves: 60_000_000_000 + (SIZE_LAMPORTS as u128 * 10_000 / 10_125) as u64,
            virtual_token_reserves: events[13].vtok - sim.tokens,
            ..Default::default()
        });
        assert!((sim.sell_price - at60).abs() < 1e-12);
    }

    #[test]
    fn the_research_cost_model_uses_the_creates_fee_both_ways() {
        let u = Pubkey::new_unique();
        let mut after = [ev(101, 1, 0, START + 1_000, true, u, 1, 50_000_000_000)];
        after[0].fee_bps = 300;
        let (_, cr, mut events) = coin(4, &after);
        events[12].fee_bps = 300;
        let per = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::PerTrade, &clock(), u64::MAX);
        let fixed = simulate(&events, 12, cr.slot, cr.ms, 60_000, 0.15, SIZE_LAMPORTS, Fees::Fixed(125), &clock(), u64::MAX);
        assert!(fixed.tokens > per.tokens, "a lower fee buys more");
        assert!(fixed.proceeds > per.proceeds);
    }

    #[test]
    fn the_book_takes_positions_under_each_limit_and_keeps_the_days_total() {
        let mut b = Book::default();
        let t = START;
        let a = b.take(t, t + 30_000, -200_000_000, 500_000_000);
        assert_eq!((a.open_at_decision, a.taken, a.day_running), (0, [true, true, true], 0));
        let c = b.take(t + 1_000, t + 50_000, -400_000_000, 500_000_000);
        assert_eq!((c.open_at_decision, c.taken), (1, [false, true, true]));
        for i in 2..5 {
            b.take(t + i * 1_000, t + 50_000, 0, 500_000_000);
        }
        let full = b.take(t + 6_000, t + 50_000, 0, 500_000_000);
        assert_eq!((full.open_at_decision, full.taken), (5, [false, false, false]));
        // After the first two closed: -0.6 SOL realized, the day's stop (0.5) hit.
        let later = b.take(t + 55_000, t + 70_000, 0, 500_000_000);
        assert_eq!((later.open_at_decision, later.taken, later.day_running, later.day_stopped), (0, [true, true, true], -600_000_000, true));
    }

    fn test_rec(mint: Pubkey) -> Rec {
        Rec {
            mint,
            deployer: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            slot: 100,
            feed: 1,
            recv_unix_ms: START - 600,
            window_ns: 250_000,
            compute_ns: 3_000,
            mayhem: false,
            token_2022: true,
            initial: (b10::INITIAL_VSOL, b10::INITIAL_VTOK),
            fee_bps: 125,
            dev_buy_lamports: 2_000_000_000,
            count: 12,
            txs: 4,
            stop: 3,
            unreadable: 0,
            trades: 12,
            overflow: false,
            mixed: false,
            sigs: [[0; 8]; b10::MAX_TXS],
            bundle_lamports: 12_000_000_000,
            largest: 1_000_000_000,
            post: CurveReserves::default(),
            post_mult: 2.1,
            miss: 0,
            hot_count: Some(4),
            hot_bundle_lamports: 4_000_000_000,
            hot_buys: 4,
            existing_reject: 1,
            existing_bought: false,
            fee_route_skip: false,
            far_producer: false,
            distance_go: true,
            buy_cu_price: 1_000_000,
            buy_cu_limit: 100_000,
            buy_tip: 1_000_000,
        }
    }

    #[test]
    fn a_row_has_every_column_and_launches_under_eight_everywhere_are_let_go() {
        let u = Pubkey::new_unique();
        let after = [ev(101, 1, 0, START + 1_000, true, u, 1, 50_000_000_000), ev(400, 1, 0, START + 200_000, true, u, 1, 45_000_000_000)];
        let (mint, cr, events) = coin(4, &after);
        let mut rec = test_rec(mint);
        rec.sigs[..4].copy_from_slice(&[events[1].sig, events[4].sig, events[7].sig, events[10].sig]);
        let mut c = Candidate { rec, created: Some(cr), events: events.clone(), slow: Some(Some(5)), checked: true };
        let mut book = Book::default();
        let (vals, pass) = row(&mut c, &mut book, &clock(), 1, SELL).unwrap();
        assert_eq!(vals.len(), COLUMNS.len());
        assert!(pass);
        let col = |name: &str| vals[COLUMNS.iter().position(|c| *c == name).unwrap()].clone();
        assert_eq!((col("count_decision"), col("count_exact"), col("count_hot"), col("count_slow")), (Val::I(12), Val::I(12), Val::I(4), Val::I(5)));
        assert_eq!((col("phantoms"), col("phantom_flags")), (Val::I(0), Val::T("0,0,0,0".into())));
        assert_eq!((col("taken_l1"), col("blocked_by_15sol_cap")), (Val::I(1), Val::I(0)));
        assert!(matches!(col("pnl60_real_sol"), Val::F(_)));
        // A cashback coin (from its create event) does not pass.
        let mut cash = Candidate { rec, created: Some(Created { cashback_event: true, ..cr }), events: events.clone(), slow: None, checked: true };
        let (vals, pass) = row(&mut cash, &mut book, &clock(), 1, SELL).unwrap();
        assert!(!pass);
        assert_eq!(vals[COLUMNS.iter().position(|c| *c == "miss").unwrap()], Val::T("cashback".into()));
        // Every count under 8: no row.
        let (mint, cr, events) = coin(2, &[]);
        let mut small = Candidate { rec: Rec { count: 6, hot_count: Some(2), ..test_rec(mint) }, created: Some(cr), events, slow: Some(Some(6)), checked: true };
        assert!(row(&mut small, &mut book, &clock(), 1, SELL).is_none());
    }

    #[test]
    fn the_migration_has_every_column_in_order_and_a_row_reaches_the_store() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::migrate::migrate(&mut c).unwrap();
        let cols: Vec<String> = c.prepare("SELECT name FROM pragma_table_info('bundle10_shadow')").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(cols[0], "id");
        assert_eq!(&cols[1..], COLUMNS);
        let db = crate::store::test_store();
        let after = [ev(101, 1, 0, START + 1_000, true, Pubkey::new_unique(), 1, 50_000_000_000)];
        let (mint, cr, events) = coin(4, &after);
        let mut cand = Candidate { rec: test_rec(mint), created: Some(cr), events, slow: None, checked: true };
        let (vals, _) = row(&mut cand, &mut Book::default(), &clock(), 1, SELL).unwrap();
        assert!(crate::store::write(crate::store::Row::Exec { sql: *INSERT, params: vals }));
        let n = crate::store::wait_count(&db, &format!("SELECT COUNT(*) FROM bundle10_shadow WHERE mint = '{mint}' AND count_exact = 12 AND would_pass = 1"), 1);
        assert_eq!(n, 1);
    }

    #[test]
    fn the_insert_names_every_column() {
        assert!(INSERT.contains("\"pnl300_research_sol\""));
        assert!(INSERT.ends_with(&format!("?{})", COLUMNS.len())));
    }
}
