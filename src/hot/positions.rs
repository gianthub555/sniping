//! B4: the position logger. For every hot-path buy (live, paper, or a lane-timing sample) it
//! follows the coin on the regular stream for a few slots and writes one row to
//! `logs/positions.csv`: where the create landed (slot, block position), the create's bundle,
//! where our buy landed (live) and how many other buys outside the bundle landed between the
//! two (`first_position`: none did), the first such rival (its position, priority bid and tip
//! service), the leader of the slot (identity, client version, city), and what we paid.
//! `logs/positions_daily.md` holds a table per UTC day, rebuilt after each row.
//!
//! Paper and sample rows have no landing of ours: they show the competition each launch had,
//! which is what a live buy has to beat. They are no evidence of where a live buy would land
//! (paper ignores fees and contention). Launches by followed deployers are tracked the same way
//! (`track_followed`, kind "followed"), bought or not.
//!
//! A2: `logs/rival_buys.csv` holds one row per buy on each tracked launch, ours included (a failed
//! live buy of ours too, its block position read from the chain): its slot and block position
//! against the create, whether it was part of the create's bundle (`bundle_end`), its order among
//! the buys outside the bundle, its priority fee per compute unit, compute limit and tip, the ms
//! after the create on the regular stream, and the slot leader's identity, client class and city.
//! The entry/tick offset is not on the regular stream; its column stays empty.

use {
    super::live::{HotBuy, Kind, Landing},
    crate::{
        config::trading_wallet,
        instructions::{pumpfun_buy_ix::TRADE_EVENT_DISCRIMINATOR, pumpswap_buy_ix::PUMP_PROGRAM_ID},
        processors::common::cpi_event_payloads,
        utils::{
            leaders,
            parse::{read_pubkey, read_u64},
        },
    },
    carbon_core::transaction::TransactionMetadata,
    log::warn,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::HashMap,
        io::Write,
        sync::{
            atomic::{AtomicUsize, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
};

pub const POSITIONS_CSV: &str = "logs/positions.csv";
pub const POSITIONS_DAILY: &str = "logs/positions_daily.md";
/// Paper and sample rows: written once the stream is this many slots past the create (or after
/// `PAPER_MAX_WAIT`), long enough to see the first rivals.
const PAPER_SLOTS: u64 = 3;
const PAPER_MAX_WAIT: Duration = Duration::from_secs(8);
/// Live rows wait for the landing (the after-send task reports it) plus two slots.
const LIVE_MAX_WAIT: Duration = Duration::from_secs(75);
const MAX_TRACKED: usize = 512;

#[derive(Clone, Copy, Debug)]
struct Trade {
    slot: u64,
    index: u64,
    signature: Signature,
    user: Pubkey,
    is_buy: bool,
    sol: u64,
    cu_price: u64,
    cu_limit: u32,
    tip: &'static str,
    tip_lamports: u64,
    /// Unix ms the regular stream showed it.
    recv_ms: u64,
}

/// What kind of launch a row is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    Live,
    Paper,
    Sample,
    /// A followed deployer's launch (`track_followed`), bought or not.
    Followed,
}

impl RowKind {
    fn of(k: Kind) -> Self {
        match k {
            Kind::Live => RowKind::Live,
            Kind::Paper => RowKind::Paper,
            Kind::Sample => RowKind::Sample,
        }
    }

    fn name(self) -> &'static str {
        match self {
            RowKind::Live => "live",
            RowKind::Paper => "paper",
            RowKind::Sample => "sample",
            RowKind::Followed => "followed",
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

struct Tracked {
    kind: RowKind,
    create_signature: Signature,
    feed_slot: u64,
    our_signature: Signature,
    our_cu_price: u64,
    our_cu_limit: u32,
    our_fee: u64,
    raised: bool,
    rival_bid_seen: u64,
    lanes: [bool; super::stamps::LANES],
    started: Instant,
    create: Option<(u64, u64)>,
    trades: Vec<Trade>,
    landing: Option<(Option<u64>, Landing)>,
    /// Which lane landed it (live): "jito", "not jito" (one of the others: same bytes) or "unknown".
    landed_lane: &'static str,
    /// Paper/sample: the stream got this far past the create.
    latest_slot: u64,
    /// Unix ms the regular stream showed the create.
    create_recv_ms: Option<u64>,
    /// A live buy of ours that failed (no trade on the stream): its block position, from the chain.
    chain_index: Option<u64>,
    /// The launch's fee tier and, in the fee ladder, our arm.
    tier: Option<u8>,
    fee_arm: Option<u8>,
    /// Unix ms right after our bytes went out (0: none sent).
    sent_unix_ms: u64,
}

static TRACKED: LazyLock<Mutex<HashMap<Pubkey, Tracked>>> = LazyLock::new(Default::default);
static COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn is_tracking() -> bool {
    COUNT.load(Ordering::Relaxed) > 0
}

/// Starts following a hot buy's coin.
pub fn track(b: &HotBuy) {
    let mut t = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    if t.len() >= MAX_TRACKED {
        return;
    }
    let priority = (b.cu_price as u128 * b.cu_limit as u128 / 1_000_000) as u64;
    t.insert(
        b.launch.mint,
        Tracked {
            kind: RowKind::of(b.kind),
            create_signature: Signature::from(b.launch.signature),
            feed_slot: b.create_slot,
            our_signature: b.signature,
            our_cu_price: b.cu_price,
            our_cu_limit: b.cu_limit,
            our_fee: priority + b.tips + 5_000,
            raised: b.raised,
            rival_bid_seen: b.rival_cu_price,
            lanes: b.lanes,
            started: Instant::now(),
            create: None,
            trades: Vec::new(),
            landing: None,
            landed_lane: "",
            latest_slot: 0,
            create_recv_ms: None,
            chain_index: None,
            tier: Some(b.tier),
            fee_arm: b.fee_arm,
            sent_unix_ms: b.sent_unix_ms,
        },
    );
    COUNT.store(t.len(), Ordering::Relaxed);
}

/// Starts following a followed deployer's launch that no hot buy tracks (A2): its row and its
/// buyers' rows show the competition on the launches the bot would buy.
pub fn track_followed(mint: Pubkey, create_signature: Signature, slot: u64, tier: u8) {
    let mut t = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    if t.len() >= MAX_TRACKED || t.contains_key(&mint) {
        return;
    }
    t.insert(
        mint,
        Tracked {
            kind: RowKind::Followed,
            create_signature,
            feed_slot: slot,
            our_signature: Signature::default(),
            our_cu_price: 0,
            our_cu_limit: 0,
            our_fee: 0,
            raised: false,
            rival_bid_seen: 0,
            lanes: [false; super::stamps::LANES],
            started: Instant::now(),
            create: None,
            trades: Vec::new(),
            landing: None,
            landed_lane: "",
            latest_slot: 0,
            create_recv_ms: None,
            chain_index: None,
            tier: Some(tier),
            fee_arm: None,
            sent_unix_ms: 0,
        },
    );
    COUNT.store(t.len(), Ordering::Relaxed);
}

/// The after-send task's word on a live buy, and which lane landed it.
pub fn note_landing(mint: &Pubkey, _signature: &Signature, slot: Option<u64>, landing: Landing, lane: &'static str) {
    if let Some(t) = TRACKED.lock().unwrap_or_else(|e| e.into_inner()).get_mut(mint) {
        t.landing = Some((slot, landing));
        t.landed_lane = lane;
    }
}


/// A pump.fun transaction from the regular stream (successful ones only).
pub fn observe(tx: &TransactionMetadata) {
    if !is_tracking() {
        return;
    }
    let payloads = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    let mut t = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    for v in t.values_mut() {
        if tx.slot > v.latest_slot {
            v.latest_slot = tx.slot;
        }
    }
    let index = tx.index.unwrap_or(u64::MAX);
    let recv_ms = now_ms();
    let mut bid = None;
    for body in payloads {
        let Ok(mint) = read_pubkey(&body, 0) else { continue };
        let Some(v) = t.get_mut(&mint) else { continue };
        if tx.signature == v.create_signature && v.create.is_none() {
            v.create = Some((tx.slot, index));
            v.create_recv_ms = Some(recv_ms);
        }
        if v.trades.iter().any(|x| x.signature == tx.signature) {
            continue;
        }
        let (Ok(sol), Ok(user)) = (read_u64(&body, 32), read_pubkey(&body, 49)) else { continue };
        let (cu_price, cu_limit, tip_lamports, tip) = *bid.get_or_insert_with(|| super::bids::bid_of(tx));
        v.trades.push(Trade { slot: tx.slot, index, signature: tx.signature, user, is_buy: body.get(48) == Some(&1), sol, cu_price, cu_limit, tip, tip_lamports, recv_ms });
    }
}

/// One finished row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub date: String,
    pub kind: String,
    pub mint: String,
    pub create_slot: Option<u64>,
    pub create_index: Option<u64>,
    /// Buys at the block positions right after the create, in its slot (its bundle).
    pub bundle_buys: u64,
    pub leader: String,
    pub leader_version: String,
    pub leader_client: String,
    pub leader_city: String,
    pub our_status: String,
    pub our_slot: Option<u64>,
    pub our_index: Option<u64>,
    /// Other buys outside the bundle that landed between the create and ours.
    pub others_between: Option<u64>,
    pub first_position: Option<bool>,
    pub our_leader: String,
    pub our_leader_version: String,
    pub our_leader_client: String,
    pub our_leader_city: String,
    /// The first buy outside the bundle (not ours): slots after the create, block positions after
    /// the bundle when in the create's slot, its bid and tip service.
    pub rival_slot_offset: Option<u64>,
    pub rival_gap: Option<u64>,
    pub rival_cu_price: Option<u64>,
    pub rival_tip: String,
    pub rival_sol: Option<f64>,
    /// The client of the leader of the first rival's slot.
    pub rival_leader_client: String,
    pub rivals_in_create_slot: u64,
    pub our_cu_price: u64,
    pub our_fee_sol: f64,
    pub raised: bool,
    pub rival_bid_seen: u64,
    pub lanes: String,
    pub landed_lane: String,
    /// A2: our compute limit, the leaders' client classes, our order among the buys outside the
    /// bundle (the tipped run after the create, `rival_rows`), and the ms after the create on the
    /// regular stream.
    pub our_cu_limit: u32,
    pub leader_class: String,
    pub our_leader_class: String,
    pub our_rank: Option<u64>,
    pub our_ms: Option<u64>,
    /// The first and second buyers outside that bundle (not ours).
    pub nb: [Option<Nb>; 2],
    /// Part B: the launch's fee tier, and our arm in the fee ladder (and what it was).
    pub tier: Option<u8>,
    pub fee_arm: Option<u8>,
    pub fee_arm_label: String,
    /// Whether a Jito bundle or a gateway landed our buy ("bundle", "gateway", "unknown").
    pub landed_via: String,
    /// On the regular stream's clock, ms after the create: when our bytes went out (negative: the
    /// early feed was that far ahead of the regular stream), and when the next slot's first trade
    /// showed (the create's slot ended by then).
    pub our_sent_ms: Option<i64>,
    pub next_slot_ms: Option<u64>,
    /// The create slot's leader: our QUIC handshake time to its TPU (`distance`), ms.
    pub leader_rtt_ms: Option<f64>,
    /// Paper and samples: the rank our buy would have had among the buyers outside the bundle by
    /// time alone, if it reached the leader half a handshake after it left (each rival counted
    /// ahead when the regular stream showed it before then). Optimistic: it ignores the leader's
    /// ordering by fee and tip, and the regular stream shows rivals after they executed.
    pub would_rank_time: Option<u64>,
    /// Buyers outside the bundle in the create's slot that bid a higher fee per CU than ours.
    pub rivals_outbidding: Option<u64>,
}

/// A buyer outside the create's bundle: slots after the create, positions after it in its slot,
/// fee per CU, compute limit, tip lamports, ms after the create.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Nb {
    pub slot_offset: u64,
    pub after_create: Option<u64>,
    pub cu_price: u64,
    pub cu_limit: u32,
    pub tip: u64,
    pub ms: Option<u64>,
}

const HEADER: &str = "date,kind,mint,create_slot,create_index,bundle_buys,leader,leader_version,leader_client,leader_city,our_status,our_slot,our_index,others_between,first_position,our_leader,our_leader_version,our_leader_client,our_leader_city,rival_slot_offset,rival_gap,rival_cu_price,rival_tip,rival_sol,rival_leader_client,rivals_in_create_slot,our_cu_price,our_fee_sol,raised,rival_bid_seen,lanes,landed_lane,our_cu_limit,leader_class,our_leader_class,our_rank,our_ms,nb1_slot_offset,nb1_after_create,nb1_cu_price,nb1_cu_limit,nb1_tip,nb1_ms,nb2_slot_offset,nb2_after_create,nb2_cu_price,nb2_cu_limit,nb2_tip,nb2_ms,tier,fee_arm,fee_arm_label,landed_via,our_sent_ms,next_slot_ms,leader_rtt_ms,would_rank_time,rivals_outbidding";

/// One buy on a tracked launch (A2), ours included.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RivalRow {
    pub date: String,
    pub kind: String,
    pub mint: String,
    pub create_slot: u64,
    pub create_index: u64,
    pub slot: u64,
    pub index: u64,
    pub slot_delta: u64,
    pub after_create: Option<u64>,
    pub bundled: bool,
    pub rank: Option<u64>,
    pub ours: bool,
    pub status: &'static str,
    pub user: String,
    pub sol: f64,
    pub cu_price: u64,
    pub cu_limit: u32,
    pub tip_lane: String,
    pub tip_lamports: u64,
    pub ms_after_create: Option<u64>,
    pub leader: String,
    pub leader_client: String,
    pub leader_class: String,
    pub leader_city: String,
    pub signature: String,
}

pub const RIVALS_CSV: &str = "logs/rival_buys.csv";
const RIVALS_HEADER: &str = "date,kind,mint,create_slot,create_index,slot,index,slot_delta,after_create,bundled,rank,ours,status,user,sol,cu_price,cu_limit,priority_lamports,tip_lane,tip_lamports,ms_after_create,entry_offset,leader,leader_client,leader_class,leader_city,signature";

impl RivalRow {
    fn csv(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.6},{},{},{},{},{},{},,{},{},{},{},{}\n",
            self.date,
            self.kind,
            self.mint,
            self.create_slot,
            self.create_index,
            self.slot,
            self.index,
            self.slot_delta,
            opt(self.after_create),
            self.bundled,
            opt(self.rank),
            self.ours,
            self.status,
            self.user,
            self.sol,
            self.cu_price,
            self.cu_limit,
            self.cu_price as u128 * self.cu_limit as u128 / 1_000_000,
            self.tip_lane,
            self.tip_lamports,
            opt(self.ms_after_create),
            self.leader,
            self.leader_client,
            self.leader_class,
            self.leader_city.replace(',', " "),
            self.signature
        )
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

impl Row {
    fn csv(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.9},{},{},{},{}\n",
            self.date,
            self.kind,
            self.mint,
            opt(self.create_slot),
            opt(self.create_index),
            self.bundle_buys,
            self.leader,
            self.leader_version,
            self.leader_client,
            self.leader_city.replace(',', " "),
            self.our_status,
            opt(self.our_slot),
            opt(self.our_index),
            opt(self.others_between),
            opt(self.first_position),
            self.our_leader,
            self.our_leader_version,
            self.our_leader_client,
            self.our_leader_city.replace(',', " "),
            opt(self.rival_slot_offset),
            opt(self.rival_gap),
            opt(self.rival_cu_price),
            self.rival_tip,
            self.rival_sol.map(|x| format!("{x:.4}")).unwrap_or_default(),
            self.rival_leader_client,
            self.rivals_in_create_slot,
            self.our_cu_price,
            self.our_fee_sol,
            self.raised,
            self.rival_bid_seen,
            self.lanes,
            self.landed_lane
        )
        .trim_end()
        .to_string()
            + &format!(
                ",{},{},{},{},{}{},{},{},{},{},{},{},{},{},{}\n",
                self.our_cu_limit,
                self.leader_class,
                self.our_leader_class,
                opt(self.our_rank),
                opt(self.our_ms),
                self.nb
                    .iter()
                    .map(|n| match n {
                        Some(n) => format!(",{},{},{},{},{},{}", n.slot_offset, opt(n.after_create), n.cu_price, n.cu_limit, n.tip, opt(n.ms)),
                        None => ",,,,,,".to_string(),
                    })
                    .collect::<String>(),
                opt(self.tier),
                opt(self.fee_arm),
                self.fee_arm_label.replace(',', " "),
                self.landed_via,
                opt(self.our_sent_ms),
                opt(self.next_slot_ms),
                self.leader_rtt_ms.map(|x| format!("{x:.1}")).unwrap_or_default(),
                opt(self.would_rank_time),
                opt(self.rivals_outbidding)
            )
    }
}

/// The leader of `slot`: identity, version, client, city.
type Leader = (String, String, String, String);

fn leader_of(slot: u64) -> Leader {
    match leaders::producer(slot) {
        Some((id, place)) => (
            id.to_string(),
            leaders::version(&id).unwrap_or_default(),
            leaders::client(&id).unwrap_or_default(),
            place.map(|p| p.to_string()).unwrap_or_default(),
        ),
        None => Default::default(),
    }
}

pub(crate) fn today() -> String {
    civil((crate::utils::dashboard::now_unix() / 86_400) as i64)
}

/// The date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The row of a finished coin. Pure, for the tests.
fn row(mint: &Pubkey, v: &Tracked, own: &Pubkey, date: String, leader: impl Fn(u64) -> Leader) -> Row {
    let mut trades = v.trades.clone();
    trades.sort_by_key(|t| (t.slot, t.index));
    let ours = |t: &Trade| t.signature == v.our_signature || t.user == *own;
    let mut r = Row {
        date,
        kind: v.kind.name().into(),
        mint: mint.to_string(),
        our_cu_price: v.our_cu_price,
        our_cu_limit: v.our_cu_limit,
        tier: v.tier,
        fee_arm: v.fee_arm,
        landed_via: match v.landed_lane {
            "" => String::new(),
            "jito" => "bundle".into(),
            "unknown" | "not jito" => "unknown".into(),
            _ => "gateway".into(),
        },
        our_fee_sol: v.our_fee as f64 / 1e9,
        raised: v.raised,
        rival_bid_seen: v.rival_bid_seen,
        lanes: super::lanes::LANE_NAMES.iter().zip(v.lanes).filter(|(_, on)| *on).map(|(n, _)| *n).collect::<Vec<_>>().join("+"),
        rival_tip: String::new(),
        landed_lane: v.landed_lane.into(),
        ..Default::default()
    };
    let Some((cslot, cindex)) = v.create else {
        // The create never showed on the stream: the leader of the slot the feed named.
        (r.leader, r.leader_version, r.leader_client, r.leader_city) = leader(v.feed_slot);
        r.leader_class = class_name(&r.leader, &r.leader_client);
        r.our_status = match v.landing {
            Some((_, l)) => format!("{l:?}"),
            None if v.kind == RowKind::Live => "unknown".into(),
            None => "-".into(),
        };
        return r;
    };
    r.create_slot = Some(cslot);
    r.create_index = Some(cindex);
    if let Some(c) = v.create_recv_ms {
        r.our_sent_ms = (v.sent_unix_ms > 0).then(|| v.sent_unix_ms as i64 - c as i64);
        r.next_slot_ms = trades.iter().filter(|t| t.slot > cslot).map(|t| t.recv_ms).min().map(|m| m.saturating_sub(c));
    }
    (r.leader, r.leader_version, r.leader_client, r.leader_city) = leader(cslot);
    r.leader_class = class_name(&r.leader, &r.leader_client);
    r.leader_rtt_ms = r.leader.parse::<Pubkey>().ok().and_then(|id| super::distance::rtt_of(&id)).flatten().map(|us| us as f64 / 1000.0);
    // The create's bundle: buys at the positions right after it, in its slot.
    let mut next = cindex + 1;
    while trades.iter().any(|t| t.slot == cslot && t.index == next && t.is_buy && !ours(t)) {
        next += 1;
    }
    r.bundle_buys = next - cindex - 1;
    let bundle_end = next; // first position after the bundle
    let outside = |t: &&Trade| t.is_buy && (t.slot, t.index) > (cslot, cindex) && !(t.slot == cslot && t.index < bundle_end);
    let rivals: Vec<&Trade> = trades.iter().filter(outside).filter(|t| !ours(t)).collect();
    r.rivals_in_create_slot = rivals.iter().filter(|t| t.slot == cslot).count() as u64;
    if let Some(first) = rivals.first() {
        r.rival_slot_offset = Some(first.slot - cslot);
        r.rival_gap = (first.slot == cslot).then(|| first.index - bundle_end);
        r.rival_cu_price = Some(first.cu_price);
        r.rival_tip = first.tip.into();
        r.rival_sol = Some(first.sol as f64 / 1e9);
        r.rival_leader_client = leader(first.slot).2;
    }
    if v.kind == RowKind::Live {
        let mine = trades.iter().find(|t| ours(t) && t.is_buy);
        r.our_status = match (mine, v.landing) {
            (Some(_), _) => "Landed".into(),
            (None, Some((_, l))) => format!("{l:?}"),
            (None, None) => "unknown".into(),
        };
        if let Some(m) = mine {
            r.our_slot = Some(m.slot);
            r.our_index = Some(m.index);
            let between = rivals.iter().filter(|t| (t.slot, t.index) < (m.slot, m.index)).count() as u64;
            r.others_between = Some(between);
            r.first_position = Some(between == 0);
            (r.our_leader, r.our_leader_version, r.our_leader_client, r.our_leader_city) = leader(m.slot);
        } else if let Some((Some(slot), _)) = v.landing {
            r.our_slot = Some(slot);
            r.our_index = v.chain_index;
            if let Some(i) = v.chain_index {
                let between = rivals.iter().filter(|t| (t.slot, t.index) < (slot, i)).count() as u64;
                r.others_between = Some(between);
                r.first_position = Some(between == 0);
            }
            (r.our_leader, r.our_leader_version, r.our_leader_client, r.our_leader_city) = leader(slot);
        }
        r.our_leader_class = class_name(&r.our_leader, &r.our_leader_client);
    } else {
        r.our_status = "-".into();
    }
    // A2: the buyers outside the bundle, in block order (ours included).
    let bundled_until = create_bundle_end(&trades, cslot, cindex, &ours);
    let outside_nb = |t: &&Trade| t.is_buy && (t.slot, t.index) > (cslot, cindex) && !(t.slot == cslot && t.index <= bundled_until);
    let since = |ms: u64| v.create_recv_ms.map(|c| ms.saturating_sub(c));
    let mut rank = 0u64;
    let mut k = 0;
    for t in trades.iter().filter(outside_nb) {
        rank += 1;
        if ours(t) {
            r.our_rank = Some(rank);
            r.our_ms = since(t.recv_ms);
        } else if k < 2 {
            r.nb[k] = Some(Nb {
                slot_offset: t.slot - cslot,
                after_create: (t.slot == cslot).then(|| t.index - cindex),
                cu_price: t.cu_price,
                cu_limit: t.cu_limit,
                tip: t.tip_lamports,
                ms: since(t.recv_ms),
            });
            k += 1;
        }
    }
    // Paper and samples: where our buy would have stood (see the fields).
    if v.kind != RowKind::Live && v.sent_unix_ms > 0 {
        let others: Vec<&Trade> = trades.iter().filter(outside_nb).filter(|t| !ours(t)).collect();
        if let (Some(sent), Some(rtt)) = (r.our_sent_ms, r.leader_rtt_ms) {
            let at = sent as f64 + rtt / 2.0;
            r.would_rank_time = Some(others.iter().filter(|t| since(t.recv_ms).is_some_and(|ms| (ms as f64) < at)).count() as u64 + 1);
        }
        r.rivals_outbidding = Some(others.iter().filter(|t| t.slot == cslot && t.cu_price > v.our_cu_price).count() as u64);
    }
    // A failed live buy of ours (no trade): its order from the chain position.
    if r.our_rank.is_none() {
        if let (Some(slot), Some(i)) = (r.our_slot, v.chain_index) {
            r.our_rank = Some(trades.iter().filter(outside_nb).filter(|t| (t.slot, t.index) < (slot, i)).count() as u64 + 1);
        }
    }
    r
}

/// The scheduler class of a leader, as the logs write it: from its identity when gossip has it
/// (JitoLabs behind a relayer is its own class), else from its client name.
fn class_name(identity: &str, client: &str) -> String {
    use std::str::FromStr;
    if client.is_empty() {
        return String::new();
    }
    Pubkey::from_str(identity)
        .ok()
        .and_then(|id| super::clients::of_identity(&id))
        .unwrap_or_else(|| super::clients::Class::of_client(client))
        .name()
        .to_string()
}

/// The most buys a create's bundle can hold: a Jito bundle is at most five transactions.
const MAX_BUNDLE_BUYS: u64 = 4;

/// The last block position of the create's bundle: the run of buys right after the create in its
/// slot (not ours), at most `MAX_BUNDLE_BUYS`, tipped or not. `cindex` when there is none. A bundle
/// executes contiguously and carries one tip, often not in the buys: on 223 followed launches of
/// 7-8 Oct the run right after the create was 1-4 buys (never more), 206 of the runs had no tipped
/// buy, and those buys bid a median 0.3 lamports per CU against 23 for the first two buys after
/// the run (research/fee-engine-2026-10-08). Requiring a tip on each buy, as first planned, called
/// 92% of these bundles rivals.
fn create_bundle_end(trades: &[Trade], cslot: u64, cindex: u64, ours: &impl Fn(&Trade) -> bool) -> u64 {
    let mut end = cindex;
    while end - cindex < MAX_BUNDLE_BUYS && trades.iter().any(|t| t.slot == cslot && t.index == end + 1 && t.is_buy && !ours(t)) {
        end += 1;
    }
    end
}

/// Every buy on a finished launch (A2), ours included: a failed live buy of ours appears with its
/// chain position. Pure, for the tests.
fn rival_rows(mint: &Pubkey, v: &Tracked, own: &Pubkey, date: &str, leader: impl Fn(u64) -> Leader) -> Vec<RivalRow> {
    let Some((cslot, cindex)) = v.create else { return Vec::new() };
    let mut trades = v.trades.clone();
    trades.sort_by_key(|t| (t.slot, t.index));
    let ours = |t: &Trade| t.signature == v.our_signature || t.user == *own;
    let bundled_until = create_bundle_end(&trades, cslot, cindex, &ours);
    let mut out = Vec::new();
    let mut rank = 0u64;
    let mut failed_ours = match (v.kind, v.landing, v.chain_index) {
        (RowKind::Live, Some((Some(slot), _)), Some(i)) if !trades.iter().any(|t| ours(t)) => Some((slot, i)),
        _ => None,
    };
    let leader_row = |slot: u64, r: &mut RivalRow| {
        let (id, _, client, city) = leader(slot);
        r.leader_class = class_name(&id, &client);
        r.leader = id;
        r.leader_client = client;
        r.leader_city = city;
    };
    let failed_row = |slot: u64, i: u64, rank: u64| {
        let mut r = RivalRow {
            date: date.into(),
            kind: v.kind.name().into(),
            mint: mint.to_string(),
            create_slot: cslot,
            create_index: cindex,
            slot,
            index: i,
            slot_delta: slot - cslot,
            after_create: (slot == cslot).then(|| i - cindex),
            rank: Some(rank),
            ours: true,
            status: "failed",
            user: own.to_string(),
            cu_price: v.our_cu_price,
            cu_limit: v.our_cu_limit,
            tip_lane: v.landed_lane.into(),
            signature: v.our_signature.to_string(),
            ..Default::default()
        };
        leader_row(slot, &mut r);
        r
    };
    for t in trades.iter().filter(|t| t.is_buy && (t.slot, t.index) > (cslot, cindex)) {
        // Our failed buy goes in its place in block order.
        if let Some((slot, i)) = failed_ours.filter(|&(s, i)| (s, i) < (t.slot, t.index)) {
            rank += 1;
            out.push(failed_row(slot, i, rank));
            failed_ours = None;
        }
        let bundled = t.slot == cslot && t.index <= bundled_until;
        if !bundled {
            rank += 1;
        }
        let mut r = RivalRow {
            date: date.into(),
            kind: v.kind.name().into(),
            mint: mint.to_string(),
            create_slot: cslot,
            create_index: cindex,
            slot: t.slot,
            index: t.index,
            slot_delta: t.slot - cslot,
            after_create: (t.slot == cslot).then(|| t.index - cindex),
            bundled,
            rank: (!bundled).then_some(rank),
            ours: ours(t),
            status: "executed",
            user: t.user.to_string(),
            sol: t.sol as f64 / 1e9,
            cu_price: t.cu_price,
            cu_limit: t.cu_limit,
            tip_lane: t.tip.into(),
            tip_lamports: t.tip_lamports,
            ms_after_create: v.create_recv_ms.map(|c| t.recv_ms.saturating_sub(c)),
            signature: t.signature.to_string(),
            ..Default::default()
        };
        leader_row(t.slot, &mut r);
        out.push(r);
    }
    if let Some((slot, i)) = failed_ours {
        rank += 1;
        out.push(failed_row(slot, i, rank));
    }
    out
}

fn done(v: &Tracked) -> bool {
    let waited = v.started.elapsed();
    match v.kind {
        RowKind::Live => {
            // The landing is known and the stream is two slots past it (or past the create).
            let past = v.landing.map(|(s, _)| s.unwrap_or(0)).unwrap_or(u64::MAX);
            (v.landing.is_some() && v.latest_slot >= past.max(v.create.map_or(0, |c| c.0)) + 2) || waited > LIVE_MAX_WAIT
        }
        _ => v.create.is_some_and(|(s, _)| v.latest_slot >= s + PAPER_SLOTS) || waited > PAPER_MAX_WAIT,
    }
}

/// Appends `lines` to `path` under `header`; a file with another header (an older format) is moved
/// aside first (`<path>.<date>.old`).
fn append_csv(path: &str, table: &'static str, header: &'static str, lines: impl Iterator<Item = String>) {
    let lines: Vec<String> = lines.collect();
    for l in &lines {
        crate::store::mirror_line(table, header, l);
    }
    if !crate::store::files_on() {
        return;
    }
    let _ = std::fs::create_dir_all("logs");
    // Only the first line: the whole file was read here on every batch (4 MB for rival_buys.csv).
    let current = std::fs::File::open(path).ok().and_then(|f| {
        let mut first = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut first).ok()?;
        Some(first.trim_end_matches(['\n', '\r']).to_string()).filter(|l| !l.is_empty())
    });
    if current.as_deref().is_some_and(|h| h != header) {
        let mut aside = format!("{path}.{}.old", today());
        for n in 2.. {
            if !std::path::Path::new(&aside).exists() {
                break;
            }
            aside = format!("{path}.{}.{n}.old", today());
        }
        if let Err(e) = std::fs::rename(path, &aside) {
            warn!("positions: moving {path} aside: {e}");
        }
    }
    let new_file = !std::path::Path::new(path).exists();
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            if new_file {
                let _ = writeln!(f, "{header}");
            }
            for l in &lines {
                let _ = f.write_all(l.as_bytes());
            }
        }
        Err(e) => warn!("positions: {path}: {e}"),
    }
}

fn write_rows(rows: &[Row], rivals: &[RivalRow]) {
    append_csv(POSITIONS_CSV, "positions_log", HEADER, rows.iter().map(Row::csv));
    if !rivals.is_empty() {
        append_csv(RIVALS_CSV, "rival_buys", RIVALS_HEADER, rivals.iter().map(RivalRow::csv));
    }
    // The daily table from the file, or from the store once the files are off.
    let text = if crate::store::files_on() { std::fs::read_to_string(POSITIONS_CSV).ok() } else { Some(crate::store::read::csv_text("positions_log", HEADER)) };
    if let Some(text) = text {
        let _ = std::fs::write(POSITIONS_DAILY, daily_table(&text));
    }
}

/// Where `signature` sits in `slot`'s block (a failed transaction of ours: the stream shows only
/// successful ones).
async fn block_index(slot: u64, signature: &Signature) -> Option<u64> {
    let block = crate::config::RPC_CLIENT
        .get_block_with_config(
            slot,
            solana_rpc_client_api::config::RpcBlockConfig {
                transaction_details: Some(solana_transaction_status_client_types::TransactionDetails::Signatures),
                rewards: Some(false),
                max_supported_transaction_version: Some(1),
                commitment: Some(solana_commitment_config::CommitmentConfig::confirmed()),
                ..Default::default()
            },
        )
        .await
        .ok()?;
    let text = signature.to_string();
    block.signatures?.iter().position(|s| *s == text).map(|i| i as u64)
}

fn median(mut v: Vec<u64>) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_unstable();
    v[v.len() / 2].to_string()
}

/// The per-day table from the CSV.
pub fn daily_table(csv: &str) -> String {
    let mut lines = csv.lines();
    let Some(head) = lines.next() else { return String::new() };
    let cols: Vec<&str> = head.split(',').collect();
    let col = |name: &str| cols.iter().position(|c| *c == name);
    let (Some(c_date), Some(c_kind), Some(c_cs), Some(c_status), Some(c_first), Some(c_between), Some(c_off), Some(c_gap), Some(c_in), Some(c_bundle), Some(c_fee)) = (
        col("date"),
        col("kind"),
        col("create_slot"),
        col("our_status"),
        col("first_position"),
        col("others_between"),
        col("rival_slot_offset"),
        col("rival_gap"),
        col("rivals_in_create_slot"),
        col("bundle_buys"),
        col("our_fee_sol"),
    ) else {
        return String::new();
    };
    let mut groups: std::collections::BTreeMap<(String, String), Vec<Vec<String>>> = Default::default();
    for line in lines {
        let f: Vec<String> = line.split(',').map(String::from).collect();
        if f.len() < cols.len() {
            continue;
        }
        groups.entry((f[c_date].clone(), f[c_kind].clone())).or_default().push(f);
    }
    let mut out = String::from(
        "# Hot-path positions per day\n\nFrom `logs/positions.csv`. *first rival*: the first buy outside the create's bundle (not ours). Paper and sample rows show the competition only; they are no evidence of where a live buy lands.\n\n\
         | date | kind | rows | create seen | median bundle | rival in create slot | median rival gap (positions after the bundle) | median rivals in create slot | landed | first position | median others between | failed before create | median fee SOL |\n\
         |---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for ((date, kind), rows) in groups {
        let n = rows.len();
        let num = |f: &Vec<String>, c: usize| f[c].parse::<u64>().ok();
        let seen: Vec<&Vec<String>> = rows.iter().filter(|f| !f[c_cs].is_empty()).collect();
        let with_rival: Vec<&&Vec<String>> = seen.iter().filter(|f| !f[c_off].is_empty()).collect();
        let in_slot = with_rival.iter().filter(|f| f[c_off] == "0").count();
        let landed = rows.iter().filter(|f| f[c_status] == "Landed").count();
        let first = rows.iter().filter(|f| f[c_first] == "true").count();
        let before = rows.iter().filter(|f| f[c_status] == "FailedBeforeCreate").count();
        let fees: Vec<u64> = rows.iter().filter_map(|f| f[c_fee].parse::<f64>().ok()).map(|x| (x * 1e9) as u64).collect();
        let live = kind == "live";
        let pct = |a: usize, b: usize| if b == 0 { "-".to_string() } else { format!("{a} ({:.0}%)", 100.0 * a as f64 / b as f64) };
        out.push_str(&format!(
            "| {date} | {kind} | {n} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            seen.len(),
            median(seen.iter().filter_map(|f| num(f, c_bundle)).collect()),
            pct(in_slot, with_rival.len()),
            median(with_rival.iter().filter_map(|f| num(f, c_gap)).collect()),
            median(seen.iter().filter_map(|f| num(f, c_in)).collect()),
            if live { pct(landed, n) } else { "-".into() },
            if live { pct(first, landed) } else { "-".into() },
            if live { median(rows.iter().filter_map(|f| num(f, c_between)).collect()) } else { "-".into() },
            if live { pct(before, n) } else { "-".into() },
            median(fees).parse::<u64>().map_or("-".into(), |l| format!("{:.6}", l as f64 / 1e9)),
        ));
    }
    out
}

/// Starts the task that writes finished rows. Call on the runtime.
pub fn spawn() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            let finished: Vec<(Pubkey, Tracked)> = {
                let mut t = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
                let keys: Vec<Pubkey> = t.iter().filter(|(_, v)| done(v)).map(|(k, _)| *k).collect();
                let out = keys.into_iter().filter_map(|k| t.remove(&k).map(|v| (k, v))).collect();
                COUNT.store(t.len(), Ordering::Relaxed);
                out
            };
            if finished.is_empty() {
                continue;
            }
            let own = trading_wallet();
            let mut finished = finished;
            for (_, v) in finished.iter_mut() {
                if v.kind == RowKind::Live && !v.trades.iter().any(|t| t.signature == v.our_signature) {
                    if let Some((Some(slot), _)) = v.landing {
                        v.chain_index = block_index(slot, &v.our_signature).await;
                    }
                }
            }
            let date = today();
            let rows: Vec<Row> = finished.iter().map(|(mint, v)| row(mint, v, &own, date.clone(), leader_of)).collect();
            let rivals: Vec<RivalRow> = finished.iter().flat_map(|(mint, v)| rival_rows(mint, v, &own, &date, leader_of)).collect();
            let mut rows = rows;
            for r in rows.iter_mut() {
                if let Some(arm) = r.fee_arm {
                    r.fee_arm_label = super::fee::arm_label(super::clients::Class::from_name(&r.leader_class).unwrap_or(super::clients::Class::Other), arm);
                }
            }
            // Rivals' Jito tips on followed launches size the Jito copy's tip.
            super::fee::note_rival_jito_tips(rivals.iter().filter(|x| !x.ours && x.tip_lane == "jito" && x.tip_lamports > 0 && x.kind != "sample").map(|x| x.tip_lamports));
            // Part B: our outcomes steer the fee controller; every launch tells which tiers are
            // contested and what the first buyer paid.
            for (r, (_, v)) in rows.iter().zip(&finished) {
                if v.kind == RowKind::Live {
                    super::fee::on_live_row(r);
                } else {
                    super::fee::on_launch_row(r, v.tier);
                }
            }
            tokio::task::spawn_blocking(move || write_rows(&rows, &rivals));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(slot: u64, index: u64, user: Pubkey, sig: u8) -> Trade {
        Trade {
            slot,
            index,
            signature: Signature::from([sig; 64]),
            user,
            is_buy: true,
            sol: 1,
            cu_price: 1_000 + index,
            cu_limit: 100_000,
            tip: "none",
            tip_lamports: 0,
            recv_ms: 10_000 + index,
        }
    }

    fn tracked(kind: Kind) -> Tracked {
        Tracked {
            kind: RowKind::of(kind),
            create_signature: Signature::from([1; 64]),
            feed_slot: 100,
            our_signature: Signature::from([9; 64]),
            our_cu_price: 5_833,
            our_cu_limit: 101_000,
            our_fee: 2_805_000,
            raised: false,
            rival_bid_seen: 0,
            lanes: [false, false, false, false, true, true, false, true],
            started: Instant::now(),
            create: Some((100, 50)),
            trades: Vec::new(),
            landing: None,
            landed_lane: "",
            latest_slot: 0,
            create_recv_ms: Some(10_000),
            chain_index: None,
            tier: Some(1),
            fee_arm: None,
            sent_unix_ms: 0,
        }
    }

    fn none(_: u64) -> Leader {
        ("L".into(), "4.3.0".into(), "JitoLabs".into(), "Frankfurt, DE".into())
    }

    #[test]
    fn a_live_row_counts_the_bundle_and_the_buys_between() {
        let own = Pubkey::new_unique();
        let (dev2, dev3, rival, late) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let mut v = tracked(Kind::Live);
        v.trades = vec![
            trade(100, 50, Pubkey::new_unique(), 1), // the create (dev buy)
            trade(100, 51, dev2, 2),                 // bundle
            trade(100, 52, dev3, 3),                 // bundle
            trade(100, 60, rival, 4),                // first rival
            trade(100, 70, own, 9),                  // ours
            trade(101, 3, late, 5),
        ];
        let r = row(&Pubkey::new_unique(), &v, &own, "2026-10-07".into(), none);
        assert_eq!(r.bundle_buys, 2);
        assert_eq!((r.our_slot, r.our_index, r.others_between, r.first_position), (Some(100), Some(70), Some(1), Some(false)));
        assert_eq!((r.rival_slot_offset, r.rival_gap, r.rival_cu_price), (Some(0), Some(7), Some(1_060)));
        assert_eq!(r.rivals_in_create_slot, 1);
        assert_eq!(r.lanes, "0slot+nozomi+quic");
        assert_eq!(r.our_status, "Landed");

        // Ours right after the bundle: first.
        v.trades[4] = trade(100, 53, own, 9);
        let r = row(&Pubkey::new_unique(), &v, &own, "d".into(), none);
        assert_eq!((r.others_between, r.first_position), (Some(0), Some(true)));
        assert_eq!(r.bundle_buys, 2, "our buy right after the bundle is not part of it");
    }

    #[test]
    fn a_paper_row_shows_the_competition_and_the_table_adds_up() {
        let own = Pubkey::new_unique();
        let mut v = tracked(Kind::Paper);
        v.trades = vec![trade(100, 50, Pubkey::new_unique(), 1), trade(101, 7, Pubkey::new_unique(), 4)];
        let r = row(&Pubkey::new_unique(), &v, &own, "2026-10-07".into(), none);
        assert_eq!((r.rival_slot_offset, r.rival_gap, r.our_status.as_str(), r.first_position), (Some(1), None, "-", None));
        let csv = format!("{HEADER}\n{}{}", r.csv(), r.csv());
        let table = daily_table(&csv);
        assert!(table.contains("| 2026-10-07 | paper | 2 | 2 | 0 | 0 (0%) | - | 0 |"), "{table}");
        assert_eq!(r.csv().split(',').count(), HEADER.split(',').count(), "one value per column");
    }

    #[test]
    fn the_bundle_ranks_and_rival_rows_follow_block_order() {
        let own = Pubkey::new_unique();
        let mut v = tracked(Kind::Live);
        let tipped = |mut t: Trade| {
            t.tip_lamports = 1_000_000;
            t
        };
        let rival = Pubkey::new_unique();
        v.trades = vec![
            trade(100, 50, Pubkey::new_unique(), 1),         // the create (dev buy)
            trade(100, 51, Pubkey::new_unique(), 2),         // bundle (right after, untipped)
            trade(100, 52, Pubkey::new_unique(), 3),         // bundle
            tipped(trade(100, 56, Pubkey::new_unique(), 4)), // a gap: outside, rank 1
            trade(100, 60, rival, 5),                        // rank 3 (ours is 2)
            trade(101, 3, Pubkey::new_unique(), 6),          // rank 4, next slot
        ];
        // Our buy failed at position 55 of the create's slot: rank 1, ahead of everyone outside.
        v.landing = Some((Some(100), Landing::FailedOther));
        v.chain_index = Some(55);
        let r = row(&Pubkey::new_unique(), &v, &own, "d".into(), none);
        assert_eq!(r.leader_class, "JitoAgave");
        assert_eq!((r.our_slot, r.our_index, r.our_rank), (Some(100), Some(55), Some(1)));
        assert_eq!(r.nb[0].map(|n| (n.after_create, n.cu_price, n.tip, n.ms)), Some((Some(6), 1_056, 1_000_000, Some(56))));
        assert_eq!(r.nb[1].map(|n| n.after_create), Some(Some(10)));
        assert_eq!(r.csv().split(',').count(), HEADER.split(',').count(), "one value per column");
        let rows = rival_rows(&Pubkey::new_unique(), &v, &own, "d", none);
        let view: Vec<(u64, bool, Option<u64>, bool, &str)> = rows.iter().map(|x| (x.index, x.bundled, x.rank, x.ours, x.status)).collect();
        assert_eq!(
            view,
            vec![
                (51, true, None, false, "executed"),
                (52, true, None, false, "executed"),
                (55, false, Some(1), true, "failed"),
                (56, false, Some(2), false, "executed"),
                (60, false, Some(3), false, "executed"),
                (3, false, Some(4), false, "executed"),
            ]
        );
        assert_eq!(rows[0].csv().split(',').count(), RIVALS_HEADER.split(',').count());
        // A bundle holds at most four buys: a fifth right after is outside.
        v.trades = (0..6).map(|i| trade(100, 50 + i, Pubkey::new_unique(), 10 + i as u8)).collect();
        v.chain_index = None;
        v.landing = None;
        let rows = rival_rows(&Pubkey::new_unique(), &v, &own, "d", none);
        assert_eq!(rows.iter().filter(|x| x.bundled).count(), 4, "51-54");
        assert_eq!(rows.iter().find(|x| x.index == 55).and_then(|x| x.rank), Some(1));
    }

    #[test]
    fn dates_are_civil() {
        assert_eq!(civil(0), "1970-01-01");
        assert_eq!(civil(11_016), "2000-02-29");
        assert_eq!(civil(20_733), "2026-10-07");
    }
}
