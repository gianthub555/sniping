//! Part B: the automatic priority fee. The hot path reads one table cell, `table[class][tier]`
//! (micro-lamports per compute unit), for the leader client class of the create's slot
//! (`clients::class_of_slot`) and the launch's tier (dev-buy bucket × deployer reputation × mayhem):
//! two atomic loads, no RPC, no allocation (`quote`). Everything else runs on a side task that
//! rewrites the table a few times a second (`spawn`):
//!
//! - Floor: the p92.5 fee per CU of pump.fun buys over the class's last ~50 slots (`bids`), times 3
//!   on contested tiers (where a buyer outside the bundle usually lands in the create's slot), never
//!   below Paladin's 10 lamports per CU.
//! - Slow controller on log(fee) per class: after a miss (not among the first P* = 2 buyers outside
//!   the bundle in the create's slot) raise by STEP × clamp(position − P*, −1, 3); after a win by a
//!   margin (our fee per CU over 1.5 × the second buyer's) lower by a quarter step; one update per
//!   launch, smoothed (EMA over about 8 launches), frozen below 10 outcomes per class.
//! - Cap (the owner's, 8 Oct): per class, the larger of the EV cap (κ × buy size − tip, κ 2%) and
//!   the class's median fee per CU of the first buyer outside the bundle in the create's slot (live
//!   from `bids`, A4's medians until 10 are seen), never under 10 lamports per CU; the global
//!   BUY_FEE_CAP_SOL stays the ceiling. A daily fee budget counts failed buys' fees (table mode).
//! - Route per leader class (FEE_ROUTE_<CLASS>, the owner's of 8 Oct): JitoLabs behind a relayer
//!   (and without one): only a one-transaction Jito bundle (sendBundle to the Frankfurt block
//!   engine), the class floor as priority fee, the tip inside the buy (0.0003 SOL to start, then the
//!   larger of rivals' median Jito tip on followed launches and Jito's tip floor p75, at most 0.001),
//!   jitodontfront named, no gateway copy: a bundle that fails costs nothing. BAM, Firedancer,
//!   Frankendancer and Agave: the table's fee through the class's gateways only (FEE_GATEWAYS_<CLASS>,
//!   each paid its own tip). Harmonic and unknown clients: not bought, the launch logged.
//! - Breakers: over 30% fill failures in the last 20 sends pauses buying and re-simulates the
//!   compute limit; 30 attempts at the cap with first-or-second position under the floor rate stops
//!   raising and flags the regime; a first-position fee jumping over 3× its rolling median widens
//!   exploration and is not followed.
//! - Tips (B3): the lane's floor, except on Harmonic leaders: clamp(Jito tip floor p75 × m, lane
//!   floor, tip cap). Tips stay inside the transaction, so a failed buy refunds them.
//! - The in-shred outbid (10% over a visible rival's bid) stays an override on top, under the caps.
//!
//! FEE_MODE: `off` (the default: the lanes' fixed default bid and outbid, as before), `table` (this
//! engine) or `ladder` (fee tests: per class, FEE_LADDER_<CLASS> lists arms, `fee:SOL` a total
//! priority fee, `jito:SOL` a Jito copy with that tip and the class floor as priority fee, `skip`;
//! a class without a list takes FEE_LADDER_SOL as fee arms; the arms balanced within each class,
//! ties broken by the mint's hash).

use {
    super::clients::{self, Class, CLASSES},
    crate::config::{env_opt, rules::Follow},
    log::{info, warn},
    std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
            LazyLock, Mutex,
        },
        time::Duration,
    },
};

/// Paladin's minimum: 10 lamports per CU, in micro-lamports per CU.
pub const PALADIN_MIN: u64 = 10_000_000;
const DEV_BUCKETS: usize = 3;
pub const TIERS: usize = DEV_BUCKETS * 2 * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Table,
    Ladder,
}

/// How a buy under a leader class goes out (table mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Not bought.
    Skip,
    /// The table's priority fee through the class's gateways.
    Fee,
    /// Only a one-transaction Jito bundle: the class floor as priority fee, the Jito tip inside.
    Jito,
    /// The table's priority fee through the gateways, and a Jito bundle copy.
    FeeJito,
    /// The table's fee, unless the class's median first-buyer fee is over its cap (then skipped).
    FeeUnderCap,
    /// Launch by launch in turn: a Jito bundle alone (as `Jito`), then one gateway copy (as `Fee`).
    /// The owner's for JitoLabs leaders without a relayer (8 Oct), to see which lands earlier.
    Alternate,
}

impl Route {
    fn parse(v: &str) -> Option<Route> {
        Some(match v.trim().to_ascii_lowercase().as_str() {
            "skip" => Route::Skip,
            "fee" => Route::Fee,
            "jito" => Route::Jito,
            "fee+jito" | "feejito" => Route::FeeJito,
            "fee-under-cap" => Route::FeeUnderCap,
            "alternate" | "jito|fee" | "jito/fee" => Route::Alternate,
            _ => return None,
        })
    }
}

/// One arm of a fee test (ladder mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// A total priority fee, lamports.
    Fee(u64),
    /// A Jito bundle copy with this tip (lamports), the class floor as priority fee.
    Jito(u64),
    Skip,
}

impl Arm {
    fn parse(v: &str) -> Option<Arm> {
        let v = v.trim().to_ascii_lowercase();
        let sol = |x: &str| x.trim().parse::<f64>().ok().map(|s| (s * 1e9).round() as u64);
        if v == "skip" {
            Some(Arm::Skip)
        } else if let Some(x) = v.strip_prefix("jito:") {
            sol(x).map(Arm::Jito)
        } else {
            sol(v.strip_prefix("fee:").unwrap_or(&v)).map(Arm::Fee)
        }
    }

    pub fn label(self) -> String {
        match self {
            Arm::Fee(l) => format!("fee {}", l as f64 / 1e9),
            Arm::Jito(t) => format!("jito tip {}", t as f64 / 1e9),
            Arm::Skip => "skip".into(),
        }
    }
}

/// The launch's tier: dev-buy bucket (under 1 SOL, 1-3, 3+), the deployer's reputation (it made the
/// reference wallet money, or only traded recently) and mayhem. Allocation-free.
#[inline]
pub fn tier(dev_buy_lamports: u64, follow: Option<Follow>, mayhem: bool) -> u8 {
    let dev = match dev_buy_lamports {
        l if l < 1_000_000_000 => 0,
        l if l < 3_000_000_000 => 1,
        _ => 2,
    };
    let rep = usize::from(follow == Some(Follow::Profitable));
    (dev + DEV_BUCKETS * (rep + 2 * usize::from(mayhem))) as u8
}

pub fn tier_name(t: u8) -> String {
    let t = t as usize;
    format!(
        "dev {} / {} / {}",
        ["<1", "1-3", "3+"][t % DEV_BUCKETS],
        if (t / DEV_BUCKETS) % 2 == 1 { "profitable" } else { "recent" },
        if t / (2 * DEV_BUCKETS) == 1 { "mayhem" } else { "plain" }
    )
}

// ---- settings ----------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub mode: Mode,
    pub floor_quantile: f64,
    pub floor_slots: usize,
    pub contested_mult: f64,
    pub paladin_min: u64,
    /// The controller's step (log of 1 + this), the target position, outcomes before it moves, the
    /// smoothing (launches).
    pub step: f64,
    pub target: u32,
    pub min_samples: u32,
    pub smooth: f64,
    pub kappa: f64,
    pub buy_lamports: u64,
    pub global_cap_lamports: u64,
    pub daily_budget_lamports: u64,
    /// Per class, the median first-buyer fee per CU used until 10 are seen live (A4, 8 Oct).
    pub first_fee_prior: [u64; CLASSES],
    pub route: [Route; CLASSES],
    /// Per class, the gateway lanes a buy goes through (a bit per lane index), each paid its tip.
    pub gateways: [u8; CLASSES],
    pub harmonic_tip_mult: f64,
    pub tip_cap_lamports: u64,
    pub jito_tip_start_lamports: u64,
    pub jito_tip_cap_lamports: u64,
    /// Fee-test arms per class (ladder mode).
    pub ladder: [Vec<Arm>; CLASSES],
    pub fill_fail_rate: f64,
    pub regime_attempts: usize,
    pub regime_floor: f64,
    pub jump_mult: f64,
    pub outbid_bps: u64,
}

impl Settings {
    pub fn from_env() -> Self {
        let num = |k: &str, d: f64| env_opt(k).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(d);
        let lamports_per_cu = |k: &str, d: f64| (num(k, d) * 1e6) as u64;
        let sol = |k: &str, d: f64| (num(k, d) * 1e9).round() as u64;
        Settings {
            mode: match env_opt("FEE_MODE").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
                "table" => Mode::Table,
                "ladder" => Mode::Ladder,
                _ => Mode::Off,
            },
            floor_quantile: num("FEE_FLOOR_QUANTILE", 0.925),
            floor_slots: num("FEE_FLOOR_SLOTS", 50.0) as usize,
            contested_mult: num("FEE_CONTESTED_MULT", 3.0),
            paladin_min: lamports_per_cu("FEE_MIN_LAMPORTS_PER_CU", 10.0),
            step: num("FEE_STEP", 0.2).clamp(0.125, 0.25),
            target: num("FEE_TARGET_POSITION", 2.0) as u32,
            min_samples: num("FEE_MIN_SAMPLES", 10.0) as u32,
            smooth: num("FEE_SMOOTH_LAUNCHES", 8.0).clamp(5.0, 10.0),
            kappa: num("FEE_KAPPA", 0.02),
            buy_lamports: sol("BUY_SOL_AMOUNT", 0.1),
            global_cap_lamports: sol("BUY_FEE_CAP_SOL", 0.005),
            daily_budget_lamports: sol("FEE_DAILY_BUDGET_SOL", 0.05),
            // A4: Jito-Agave 5.0, Firedancer 37.4, Harmonic 25.4, BAM 37.4 (Agave had 3 launches:
            // Jito-Agave's; Frankendancer none: Firedancer's; other: Jito-Agave's).
            first_fee_prior: std::array::from_fn(|c| {
                let name = clients::NAMES[c].to_ascii_uppercase();
                let d = [5.0, 5.0, 37.4, 37.4, 25.4, 37.4, 5.0, 5.0][c];
                lamports_per_cu(&format!("FEE_FIRST_FEE_{name}"), d)
            }),
            route: std::array::from_fn(|c| {
                let name = clients::NAMES[c].to_ascii_uppercase();
                // Agave, JitoAgave (no relayer), Firedancer, Frankendancer, Harmonic, BAM, other,
                // JitoRelayer.
                let d = [Route::Fee, Route::Alternate, Route::Fee, Route::Fee, Route::Skip, Route::Fee, Route::Skip, Route::Jito][c];
                env_opt(&format!("FEE_ROUTE_{name}")).and_then(|v| Route::parse(&v)).unwrap_or(d)
            }),
            gateways: std::array::from_fn(|c| {
                let name = clients::NAMES[c].to_ascii_uppercase();
                let names = env_opt(&format!("FEE_GATEWAYS_{name}")).unwrap_or_else(|| "0slot,everstake".into());
                names
                    .split(',')
                    .filter_map(|n| super::lanes::LANE_NAMES.iter().position(|l| l.eq_ignore_ascii_case(n.trim())))
                    .filter(|l| *l != super::lanes::LANE_JITO)
                    .fold(0u8, |m, l| m | 1 << l)
            }),
            harmonic_tip_mult: num("FEE_HARMONIC_TIP_MULT", 1.0),
            tip_cap_lamports: sol("FEE_TIP_CAP_SOL", 0.002),
            jito_tip_start_lamports: sol("FEE_JITO_TIP_START_SOL", 0.0003),
            jito_tip_cap_lamports: sol("FEE_JITO_TIP_CAP_SOL", 0.001),
            ladder: {
                let fallback: Vec<Arm> = env_opt("FEE_LADDER_SOL")
                    .unwrap_or_else(|| "0.002,0.005,0.010".into())
                    .split(',')
                    .filter_map(Arm::parse)
                    .collect();
                std::array::from_fn(|c| {
                    let name = clients::NAMES[c].to_ascii_uppercase();
                    match env_opt(&format!("FEE_LADDER_{name}")) {
                        Some(v) => v.split(',').filter_map(Arm::parse).take(8).collect(),
                        None => fallback.iter().copied().take(8).collect(),
                    }
                })
            },
            fill_fail_rate: num("FEE_FILL_FAIL_RATE", 0.30),
            regime_attempts: num("FEE_REGIME_ATTEMPTS", 30.0) as usize,
            regime_floor: num("FEE_REGIME_FLOOR", 0.2),
            jump_mult: num("FEE_JUMP_MULT", 3.0),
            outbid_bps: num("HOT_OUTBID_BPS", 1_000.0) as u64,
        }
    }
}

pub static SETTINGS: LazyLock<Settings> = LazyLock::new(Settings::from_env);

// ---- the table the hot path reads ----------------------------------------------------------------

/// Fee per CU per class and tier (0: not computed yet), and the cap the outbid may reach.
static TABLE: [[AtomicU64; TIERS]; CLASSES] = [const { [const { AtomicU64::new(0) }; TIERS] }; CLASSES];
static CAP: [[AtomicU64; TIERS]; CLASSES] = [const { [const { AtomicU64::new(0) }; TIERS] }; CLASSES];
/// The class floor per class and tier, under the cap (micro-lamports per CU).
static FLOOR: [[AtomicU64; TIERS]; CLASSES] = [const { [const { AtomicU64::new(0) }; TIERS] }; CLASSES];
/// The Harmonic tip (lamports, before the lane's floor and the cap): Jito's tip floor p75 × m.
static HARMONIC_TIP: AtomicU64 = AtomicU64::new(0);
/// The Jito bundle copy's tip (lamports): rivals' Jito tips and Jito's tip floor, capped.
static JITO_TIP: AtomicU64 = AtomicU64::new(0);
/// Table mode: classes skipped (FeeUnderCap with the median first-buyer fee over the cap).
static SKIP_CLASS: [AtomicBool; CLASSES] = [const { AtomicBool::new(false) }; CLASSES];
/// Jito's tip floor, 75th percentile of landed tips (lamports; 0: not read yet).
static JITO_FLOOR_P75: AtomicU64 = AtomicU64::new(0);
/// Fill-failure breaker: buying paused until the compute limit is re-simulated.
static PAUSED: AtomicBool = AtomicBool::new(false);
/// Alternate routes sent, per class: even turns go as a Jito bundle, odd ones through a gateway.
static ALT_SENT: [AtomicU32; CLASSES] = [const { AtomicU32::new(0) }; CLASSES];
/// Ladder arms sent, per class and arm (Part C balance).
static LADDER_SENT: [[AtomicU32; 8]; CLASSES] = [const { [const { AtomicU32::new(0) }; 8] }; CLASSES];

/// A buy's priority bid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    /// Micro-lamports per CU.
    pub cu_price: u64,
    /// The outbid raised it over the table.
    pub raised: bool,
    pub class: Class,
    /// Ladder: which arm (index into the class's FEE_LADDER list).
    pub arm: Option<u8>,
    /// The paid tip's lamports when it differs from the lane's floor (Harmonic).
    pub tip: Option<u64>,
    /// A Jito bundle of the same signed buy, with this tip (lamports).
    pub jito_tip: Option<u64>,
    /// The lanes the buy goes through, a bit per lane index (the gateways, Jito).
    pub lanes: u8,
}

/// A lane's bit in `Quote::lanes`.
pub const fn lane_bit(lane: usize) -> u8 {
    1 << lane
}

pub fn mode() -> Mode {
    SETTINGS.mode
}

pub fn paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

/// Whether the route of `slot`'s leader class says not to buy. Allocation-free.
#[inline]
pub fn skips(slot: u64) -> bool {
    skips_with(&SETTINGS, clients::class_of_slot(slot).unwrap_or(Class::Other))
}

#[inline]
pub fn skips_with(s: &Settings, class: Class) -> bool {
    let c = class as usize;
    match s.mode {
        Mode::Off => false,
        Mode::Ladder => s.ladder[c].iter().all(|a| *a == Arm::Skip),
        Mode::Table => match s.route[c] {
            Route::Skip => true,
            Route::FeeUnderCap => SKIP_CLASS[c].load(Ordering::Relaxed),
            _ => false,
        },
    }
}

/// The label of a class's ladder arm, for the logs.
pub fn arm_label(class: Class, arm: u8) -> String {
    if SETTINGS.mode == Mode::Table && SETTINGS.route[class as usize] == Route::Alternate {
        return if arm == 0 { "jito bundle".into() } else { "gateway".into() };
    }
    SETTINGS.ladder[class as usize].get(arm as usize).map_or_else(|| format!("arm {arm}"), |a| a.label())
}

/// The hot path's bid for a buy in `slot` of tier `tier` at `cu_limit`, over a rival bid seen in the
/// bundle window (0: none), paying `lane_tip` lamports by default. None: FEE_MODE=off (the caller
/// keeps its own bid) or the table has no value yet. Allocation-free.
#[inline]
pub fn quote(slot: u64, tier: u8, cu_limit: u32, rival_cu_price: u64, mint: &[u8; 32], lane_tip: u64) -> Option<Quote> {
    quote_with(&SETTINGS, clients::class_of_slot(slot).unwrap_or(Class::Other), tier, cu_limit, rival_cu_price, mint, lane_tip)
}

/// `quote` for given settings and class. Allocation-free.
#[inline]
pub fn quote_with(s: &Settings, class: Class, tier: u8, cu_limit: u32, rival_cu_price: u64, mint: &[u8; 32], lane_tip: u64) -> Option<Quote> {
    let (c, t) = (class as usize, (tier as usize).min(TIERS - 1));
    // The class floor (under the cap), at least Paladin's minimum before the table is computed.
    let floor = match FLOOR[c][t].load(Ordering::Relaxed) {
        0 => s.paladin_min,
        f => f,
    };
    match s.mode {
        Mode::Off => None,
        Mode::Ladder => {
            let arms = &s.ladder[c];
            let n = arms.len().min(8);
            if n == 0 {
                return None;
            }
            // The arm this class has sent least (skip arms never), ties broken by the mint's hash.
            let h = mint[..8].iter().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(*b as u64));
            let mut best = (u32::MAX, usize::MAX);
            for k in 0..n {
                let arm = (k + h as usize) % n;
                if arms[arm] == Arm::Skip {
                    continue;
                }
                let sent = LADDER_SENT[c][arm].load(Ordering::Relaxed);
                if sent < best.0 {
                    best = (sent, arm);
                }
            }
            if best.1 == usize::MAX {
                return None;
            }
            LADDER_SENT[c][best.1].fetch_add(1, Ordering::Relaxed);
            let (cu_price, jito_tip, lanes) = match arms[best.1] {
                Arm::Fee(lamports) => ((lamports as u128 * 1_000_000 / cu_limit.max(1) as u128) as u64, None, s.gateways[c]),
                Arm::Jito(tip) => (floor, Some(tip), lane_bit(super::lanes::LANE_JITO)),
                Arm::Skip => return None,
            };
            Some(Quote { cu_price, raised: false, class, arm: Some(best.1 as u8), tip: None, jito_tip, lanes })
        }
        Mode::Table => {
            let base = TABLE[c][t].load(Ordering::Relaxed);
            if base == 0 {
                return None;
            }
            // Alternate: this launch's turn (arm 0: the Jito bundle, arm 1: a gateway copy).
            let (route, arm) = match s.route[c] {
                Route::Alternate => match ALT_SENT[c].fetch_add(1, Ordering::Relaxed) % 2 {
                    0 => (Route::Jito, Some(0)),
                    _ => (Route::Fee, Some(1)),
                },
                r => (r, None),
            };
            let jito_tip = matches!(route, Route::Jito | Route::FeeJito).then(|| JITO_TIP.load(Ordering::Relaxed).max(1_000));
            let jito = lane_bit(super::lanes::LANE_JITO);
            if route == Route::Jito {
                return Some(Quote { cu_price: floor, raised: false, class, arm, tip: None, jito_tip, lanes: jito });
            }
            let lanes = if route == Route::FeeJito { s.gateways[c] | jito } else { s.gateways[c] };
            let cap = CAP[c][t].load(Ordering::Relaxed).max(base);
            let over = (rival_cu_price as u128 * (10_000 + s.outbid_bps) as u128 / 10_000) as u64 + 1;
            let raised = rival_cu_price > 0 && over > base;
            let cu_price = if raised { over.min(cap) } else { base };
            let tip = (class == Class::Harmonic).then(|| HARMONIC_TIP.load(Ordering::Relaxed).clamp(lane_tip, s.tip_cap_lamports.max(lane_tip)));
            Some(Quote { cu_price, raised: raised && cu_price > base, class, arm, tip: tip.filter(|t| *t != lane_tip), jito_tip, lanes })
        }
    }
}

/// What `quote` would bid in `slot` for tier `tier` (micro-lamports per CU) and the tip it would
/// pay, without taking a turn: the ladder's and the alternate route's counters are not touched, so
/// no buy's arm changes. bundle10's shadow log prices its buy with it. The gateway arm (the
/// table's fee, the lane's tip; Harmonic's tip for a Harmonic leader); a Jito-only route: the class
/// floor and the Jito tip. None in ladder mode, with FEE_MODE=off or before the table has a value.
pub fn peek(slot: u64, tier: u8, rival_cu_price: u64, lane_tip: u64) -> Option<(u64, u64)> {
    let s = &*SETTINGS;
    let class = clients::class_of_slot(slot).unwrap_or(Class::Other);
    let (c, t) = (class as usize, (tier as usize).min(TIERS - 1));
    if s.mode != Mode::Table {
        return None;
    }
    let base = TABLE[c][t].load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    if s.route[c] == Route::Jito {
        let floor = match FLOOR[c][t].load(Ordering::Relaxed) {
            0 => s.paladin_min,
            f => f,
        };
        return Some((floor, JITO_TIP.load(Ordering::Relaxed).max(1_000)));
    }
    let cap = CAP[c][t].load(Ordering::Relaxed).max(base);
    let over = (rival_cu_price as u128 * (10_000 + s.outbid_bps) as u128 / 10_000) as u64 + 1;
    let cu_price = if rival_cu_price > 0 && over > base { over.min(cap) } else { base };
    let tip = if class == Class::Harmonic { HARMONIC_TIP.load(Ordering::Relaxed).clamp(lane_tip, s.tip_cap_lamports.max(lane_tip)) } else { lane_tip };
    Some((cu_price, tip))
}

// ---- the engine (pure: the side task, the tests and the replay drive it) ---------------------------

#[derive(Clone, Debug, Default)]
pub struct Controller {
    /// log of the multiplier over the floor (never under 0).
    pub u: f64,
    pub ema: f64,
    pub samples: u32,
    /// Raising stopped: at the cap for `regime_attempts` attempts with too few first-or-second.
    pub regime: bool,
    /// The last attempts: (at the cap, first or second in the create's slot).
    pub recent: VecDeque<(bool, bool)>,
    /// Updates left with a widened exploration (a jump in the first position's fee).
    pub explore: u32,
}

/// One live buy's result, as the position log saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome {
    pub class: Class,
    /// Our order among the buyers outside the bundle, when in the create's slot (None: later slot
    /// or never landed).
    pub rank_in_create_slot: Option<u32>,
    pub our_fee_per_cu: u64,
    /// The second buyer's fee per CU, when there was one.
    pub second_fee_per_cu: Option<u64>,
    pub at_cap: bool,
}

#[derive(Clone, Debug)]
pub struct Engine {
    pub s: Settings,
    pub ctl: Vec<Controller>,
    /// Contested share per tier: (launches, with a buyer outside the bundle in the create's slot).
    pub contested: Vec<VecDeque<bool>>,
    /// Rolling first-position fee per CU per class (for the jump breaker).
    pub first_fees: Vec<VecDeque<u64>>,
    /// The fill-failure window (true: a failed fill).
    pub fills: VecDeque<bool>,
    /// Fees spent today (lamports) and the UTC day.
    pub spent: u64,
    pub day: u64,
    /// Per class, the live median first-buyer fee per CU (None: under 10 seen, the prior applies).
    pub first_median: [Option<u64>; CLASSES],
    /// Rivals' Jito tips on followed launches (lamports), newest last.
    pub rival_jito_tips: VecDeque<u64>,
}

impl Engine {
    pub fn new(s: Settings) -> Self {
        Engine {
            s,
            ctl: vec![Controller::default(); CLASSES],
            contested: vec![VecDeque::new(); TIERS],
            first_fees: vec![VecDeque::new(); CLASSES],
            fills: VecDeque::new(),
            spent: 0,
            day: 0,
            first_median: [None; CLASSES],
            rival_jito_tips: VecDeque::new(),
        }
    }

    /// The median first-buyer fee per CU a class's cap uses: live once 10 are seen, else the prior.
    pub fn first_fee(&self, class: Class) -> u64 {
        self.first_median[class as usize].unwrap_or(self.s.first_fee_prior[class as usize])
    }

    /// A rival's Jito tip on a followed launch.
    pub fn note_rival_jito_tip(&mut self, lamports: u64) {
        self.rival_jito_tips.push_back(lamports);
        if self.rival_jito_tips.len() > 200 {
            self.rival_jito_tips.pop_front();
        }
    }

    /// The Jito bundle's tip: the start value (0.0003 SOL) until 10 rivals' Jito tips are seen,
    /// then the larger of their median and Jito's tip floor p75, never under the start value, at most
    /// the cap (0.001 SOL). Also the rivals' p50 and p75.
    pub fn jito_tip(&self, floor_p75: Option<u64>) -> (u64, Option<u64>, Option<u64>) {
        let mut v: Vec<u64> = self.rival_jito_tips.iter().copied().collect();
        v.sort_unstable();
        let at = |q: f64| (v.len() >= 10).then(|| v[((v.len() - 1) as f64 * q).round() as usize]);
        let (p50, p75) = (at(0.5), at(0.75));
        let start = self.s.jito_tip_start_lamports.max(1_000);
        let cap = self.s.jito_tip_cap_lamports.max(start);
        let tip = p50.unwrap_or(0).max(floor_p75.unwrap_or(0)).clamp(start, cap);
        (tip, p50, p75)
    }

    /// Whether a tier is contested: half or more of its last 20 launches had a buyer outside the
    /// bundle in the create's slot (with 10 or more seen; until then: not).
    pub fn is_contested(&self, tier: usize) -> bool {
        let v = &self.contested[tier];
        v.len() >= 10 && v.iter().filter(|c| **c).count() * 2 >= v.len()
    }

    pub fn note_launch(&mut self, tier: u8, contested: bool) {
        let v = &mut self.contested[(tier as usize).min(TIERS - 1)];
        v.push_back(contested);
        if v.len() > 20 {
            v.pop_front();
        }
    }

    /// The first-position fee of a launch under `class`: kept for the jump breaker, which widens
    /// exploration (and does not follow) when it is over JUMP × the rolling median. True on a jump.
    pub fn note_first_fee(&mut self, class: Class, fee: u64) -> bool {
        let v = &mut self.first_fees[class as usize];
        let jump = v.len() >= 10 && {
            let mut sorted: Vec<u64> = v.iter().copied().collect();
            sorted.sort_unstable();
            fee as f64 > self.s.jump_mult * sorted[sorted.len() / 2].max(1) as f64
        };
        if jump {
            self.ctl[class as usize].explore = 20;
        } else {
            v.push_back(fee);
            if v.len() > 30 {
                v.pop_front();
            }
        }
        jump
    }

    /// The controller's update for one launch's outcome: one step at most.
    pub fn on_outcome(&mut self, o: Outcome) {
        let s = &self.s;
        let c = &mut self.ctl[o.class as usize];
        let p_star = s.target as f64;
        let hit = o.rank_in_create_slot.is_some_and(|r| r <= s.target);
        let error = match (hit, o.rank_in_create_slot) {
            (true, _) if o.second_fee_per_cu.is_some_and(|f| o.our_fee_per_cu as f64 > 1.5 * f as f64) => -0.25,
            (true, _) => 0.0,
            (false, Some(r)) => (r as f64 - p_star).clamp(-1.0, 3.0),
            (false, None) => 3.0,
        };
        let alpha = 2.0 / (s.smooth + 1.0);
        c.ema += alpha * (error - c.ema);
        c.samples += 1;
        c.recent.push_back((o.at_cap, hit));
        if c.recent.len() > s.regime_attempts {
            c.recent.pop_front();
        }
        if c.recent.len() == s.regime_attempts && c.recent.iter().all(|(cap, _)| *cap) {
            let rate = c.recent.iter().filter(|(_, h)| *h).count() as f64 / c.recent.len() as f64;
            if rate < s.regime_floor && !c.regime {
                c.regime = true;
                warn!("fee engine {}: {} attempts at the cap, first or second on {:.0}%: raising stopped (regime flag)", o.class.name(), c.recent.len(), 100.0 * rate);
            }
        }
        if c.samples < s.min_samples {
            return;
        }
        let delta = (1.0 + s.step).ln() * c.ema;
        if delta > 0.0 && c.regime {
            return;
        }
        // At most 50x the floor (the caps bind long before).
        c.u = (c.u + delta).clamp(0.0, 50f64.ln());
    }

    /// A live send's fill: true when it failed for another reason than losing the race. Over
    /// FILL_FAIL_RATE of the last 20: true (pause and re-simulate).
    pub fn on_fill(&mut self, failed: bool) -> bool {
        self.fills.push_back(failed);
        if self.fills.len() > 20 {
            self.fills.pop_front();
        }
        self.fills.len() >= 10 && self.fills.iter().filter(|f| **f).count() as f64 / self.fills.len() as f64 > self.s.fill_fail_rate
    }

    /// Fees spent (failed buys too); true when today's budget is used up.
    pub fn on_spend(&mut self, lamports: u64, day: u64) -> bool {
        if day != self.day {
            (self.day, self.spent) = (day, 0);
        }
        self.spent += lamports;
        self.spent >= self.s.daily_budget_lamports
    }

    /// The cap for one class at `cu_limit` with `tip` lamports, micro-lamports per CU: the larger
    /// of the EV cap (κ × buy size − tip) and the class's median first-buyer fee, never under
    /// Paladin's minimum, under the global BUY_FEE_CAP_SOL.
    pub fn cap(&self, class: Class, cu_limit: u32, tip: u64) -> u64 {
        let per_cu = |lamports: u64| (lamports as u128 * 1_000_000 / cu_limit.max(1) as u128) as u64;
        let ev = per_cu(((self.s.kappa * self.s.buy_lamports as f64).round() as u64).saturating_sub(tip));
        let global = per_cu(self.s.global_cap_lamports.saturating_sub(tip));
        ev.max(self.first_fee(class)).max(self.s.paladin_min).min(global)
    }

    /// The table row of one class: per tier, (fee, cap, floor), the floor under the cap too.
    /// `floor_fee` is the class's recent quantile (None: no bids seen), `jitter` a factor near 1
    /// (Harmonic: never a round number; exploration: wider).
    pub fn row(&self, class: Class, floor_fee: Option<u64>, cu_limit: u32, tip: u64, jitter: f64) -> [(u64, u64, u64); TIERS] {
        let c = &self.ctl[class as usize];
        let base = floor_fee.unwrap_or(0);
        let cap = self.cap(class, cu_limit, tip);
        std::array::from_fn(|t| {
            let mult = if self.is_contested(t) { self.s.contested_mult } else { 1.0 };
            let floor = ((base as f64 * mult) as u64).max(self.s.paladin_min);
            let fee = (floor as f64 * c.u.exp() * jitter) as u64;
            (fee.min(cap).max(1), cap, floor.min(cap))
        })
    }
}

pub static ENGINE: LazyLock<Mutex<Engine>> = LazyLock::new(|| Mutex::new(Engine::new(SETTINGS.clone())));

// ---- outcomes from the logs ----------------------------------------------------------------------

/// A finished live row (the position log): the controller's outcome and the first-position fee.
pub fn on_live_row(r: &super::positions::Row) {
    let class = super::clients::Class::from_name(&r.leader_class).unwrap_or(super::clients::Class::Other);
    let in_create_slot = r.create_slot.is_some() && r.our_slot == r.create_slot;
    let cap_now = CAP[class as usize].iter().map(|a| a.load(Ordering::Relaxed)).max().unwrap_or(0);
    let o = Outcome {
        class,
        rank_in_create_slot: if in_create_slot { r.our_rank.map(|x| x as u32) } else { None },
        our_fee_per_cu: r.our_cu_price,
        second_fee_per_cu: r.nb[1].map(|n| n.cu_price).or(r.nb[0].map(|n| n.cu_price)),
        at_cap: cap_now > 0 && r.our_cu_price >= cap_now,
    };
    ENGINE.lock().unwrap_or_else(|e| e.into_inner()).on_outcome(o);
    info!("fee engine: outcome {}: rank in the create's slot {:?}, our {:.2} lamports/CU", class.name(), o.rank_in_create_slot, o.our_fee_per_cu as f64 / 1e6);
}

/// A finished followed or sample row: whether its tier is contested, and the first-position fee.
pub fn on_launch_row(r: &super::positions::Row, tier: Option<u8>) {
    let contested = r.nb[0].is_some_and(|n| n.slot_offset == 0);
    let mut e = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = tier {
        e.note_launch(t, contested);
    }
    if let Some(n) = r.nb[0].filter(|n| n.slot_offset == 0) {
        let class = super::clients::Class::from_name(&r.leader_class).unwrap_or(super::clients::Class::Other);
        if e.note_first_fee(class, n.cu_price) {
            warn!("fee engine {}: first-position fee {:.1} lamports/CU is over {}x its rolling median: exploring wider, not following", class.name(), n.cu_price as f64 / 1e6, e.s.jump_mult);
        }
    }
}

/// Rivals' Jito tips on a followed launch (the position log's buyer rows), for the Jito copy's tip.
pub fn note_rival_jito_tips(tips: impl Iterator<Item = u64>) {
    let mut e = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    for t in tips {
        e.note_rival_jito_tip(t);
    }
}

/// A live send's end (after-send task): the fill-failure breaker and the daily budget (table mode
/// only: a fee test has its own stops).
pub fn on_send(error: Option<&str>, fee_lamports: u64) {
    let failed_fill = error.is_some_and(|e| !e.contains("6042") && !e.contains("3012"));
    let day = crate::utils::dashboard::now_unix() / 86_400;
    let (pause, broke) = {
        let mut e = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let broke = e.on_spend(fee_lamports, day) && e.s.mode == Mode::Table;
        (e.on_fill(failed_fill), broke)
    };
    if pause && !PAUSED.swap(true, Ordering::SeqCst) {
        warn!("fee engine: over {:.0}% of the last sends failed to fill: buying paused, re-simulating the compute limit", 100.0 * SETTINGS.fill_fail_rate);
        tokio::spawn(async {
            super::cu::calibrate("fill failures", 0).await;
            ENGINE.lock().unwrap_or_else(|e| e.into_inner()).fills.clear();
            PAUSED.store(false, Ordering::SeqCst);
            info!("fee engine: compute limit re-simulated; buying resumes");
        });
    }
    if broke {
        crate::utils::live_guard::stop(&format!("the daily fee budget of {} SOL is spent", SETTINGS.daily_budget_lamports as f64 / 1e9));
    }
}

// ---- the side task ----------------------------------------------------------------------------------

/// Jito's tip floor, 75th percentile of landed tips (lamports).
async fn jito_tip_p75() -> Option<u64> {
    let v: serde_json::Value = crate::config::HTTP_CLIENT.get("https://bundles.jito.wtf/api/v1/bundles/tip_floor").send().await.ok()?.json().await.ok()?;
    let sol = v.get(0)?.get("landed_tips_75th_percentile")?.as_f64()?;
    Some((sol * 1e9) as u64)
}

/// A factor near 1 that keeps a fee off round numbers (Harmonic) and, while exploring, wider.
fn jitter(class: Class, explore: u32, n: u64) -> f64 {
    let r = ((n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as f64) / (1u64 << 24) as f64; // 0..1
    let mut j = 1.0;
    if class == Class::Harmonic {
        j *= 1.013 + 0.031 * r;
    }
    if explore > 0 {
        j *= 0.8 + 0.7 * r;
    }
    j
}

/// Rewrites the table every 400 ms (on the runtime), refreshes the Harmonic tip every minute.
pub fn spawn() {
    let s = &*SETTINGS;
    if s.mode == Mode::Off {
        return;
    }
    info!(
        "fee engine: FEE_MODE={:?}; floor p{:.1} over {} slots ×{} when contested, min {} lamports/CU; cap max(κ {} × {} SOL, class median first-buyer fee), at most {} SOL; daily budget {} SOL (table mode); {}",
        s.mode,
        s.floor_quantile * 100.0,
        s.floor_slots,
        s.contested_mult,
        s.paladin_min as f64 / 1e6,
        s.kappa,
        s.buy_lamports as f64 / 1e9,
        s.global_cap_lamports as f64 / 1e9,
        s.daily_budget_lamports as f64 / 1e9,
        clients::ALL
            .iter()
            .map(|c| {
                let k = *c as usize;
                match s.mode {
                    Mode::Ladder => format!("{} [{}]", c.name(), s.ladder[k].iter().map(|a| a.label()).collect::<Vec<_>>().join(", ")),
                    _ => format!("{} {:?}", c.name(), s.route[k]),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    );
    tokio::spawn(async {
        let mut n = 0u64;
        let mut tick = tokio::time::interval(Duration::from_millis(400));
        loop {
            tick.tick().await;
            n += 1;
            if n % 150 == 1 {
                let p75 = jito_tip_p75().await;
                if let Some(p75) = p75 {
                    HARMONIC_TIP.store((p75 as f64 * SETTINGS.harmonic_tip_mult) as u64, Ordering::Relaxed);
                    JITO_FLOOR_P75.store(p75, Ordering::Relaxed);
                }
            }
            let cu_limit = match super::cu::limit_for(&crate::utils::utils::TOKEN_2022_PROGRAM_ID) {
                0 => crate::config::TRADE_SETTING.compute_unit_limit,
                l => l,
            };
            let tip = super::lanes::LaneConfig::from_env().tips().iter().map(|(_, l)| *l).max().unwrap_or(0);
            let floors: Vec<Option<u64>> = clients::ALL.iter().map(|c| super::bids::fee_quantile(*c, SETTINGS.floor_slots, SETTINGS.floor_quantile).map(|(f, _)| f)).collect();
            // The median first-buyer fee per class (the cap), live once 10 are seen.
            let firsts: Vec<Option<u64>> = clients::ALL
                .iter()
                .map(|c| {
                    let mut v = super::bids::rank_fees(*c, 1, 50);
                    v.sort_unstable();
                    (v.len() >= 10).then(|| v[v.len() / 2])
                })
                .collect();
            let mut e = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            let floor_p75 = match JITO_FLOOR_P75.load(Ordering::Relaxed) {
                0 => None,
                v => Some(v),
            };
            JITO_TIP.store(e.jito_tip(floor_p75).0, Ordering::Relaxed);
            for class in clients::ALL {
                let c = class as usize;
                e.first_median[c] = firsts[c];
                let explore = e.ctl[c].explore;
                let row = e.row(class, floors[c], cu_limit, tip, jitter(class, explore, n.wrapping_add(c as u64)));
                for (t, (fee, cap, floor)) in row.iter().enumerate() {
                    TABLE[c][t].store(*fee, Ordering::Relaxed);
                    CAP[c][t].store(*cap, Ordering::Relaxed);
                    FLOOR[c][t].store(*floor, Ordering::Relaxed);
                }
                // Harmonic's rule: not bought while its median first-buyer fee is over its cap.
                let over = e.first_fee(class) > row[0].1;
                if SKIP_CLASS[c].swap(over, Ordering::Relaxed) != over && e.s.route[c] == Route::FeeUnderCap {
                    info!("fee engine {}: median first-buyer fee {:.1} lamports/CU {} its cap {:.1}: {}", class.name(), e.first_fee(class) as f64 / 1e6, if over { "over" } else { "under" }, row[0].1 as f64 / 1e6, if over { "not bought" } else { "bought" });
                }
                if explore > 0 && n % 10 == 0 {
                    e.ctl[c].explore -= 1;
                }
            }
            if n % 150 == 2 {
                info!("fee engine: {}", report_with(&e));
            }
        }
    });
}

fn report_with(e: &Engine) -> String {
    clients::ALL
        .iter()
        .map(|c| {
            let k = *c as usize;
            let fees: Vec<u64> = (0..TIERS).map(|t| TABLE[k][t].load(Ordering::Relaxed)).collect();
            let (lo, hi) = (fees.iter().min().copied().unwrap_or(0), fees.iter().max().copied().unwrap_or(0));
            let ctl = &e.ctl[k];
            format!(
                "{} {:.1}-{:.1} lamports/CU (floor {:.1}, cap {:.1}, median first buyer {:.1}{}, ×{:.2} over the floor, {} outcomes{}{})",
                c.name(),
                lo as f64 / 1e6,
                hi as f64 / 1e6,
                FLOOR[k][0].load(Ordering::Relaxed) as f64 / 1e6,
                CAP[k][0].load(Ordering::Relaxed) as f64 / 1e6,
                e.first_fee(*c) as f64 / 1e6,
                if e.first_median[k].is_some() { "" } else { " prior" },
                ctl.u.exp(),
                ctl.samples,
                if ctl.regime { ", regime flag" } else { "" },
                if SKIP_CLASS[k].load(Ordering::Relaxed) && e.s.route[k] == Route::FeeUnderCap { ", skipped" } else { "" }
            )
        })
        .chain(std::iter::once({
            let floor = JITO_FLOOR_P75.load(Ordering::Relaxed);
            let (tip, p50, p75) = e.jito_tip((floor > 0).then_some(floor));
            format!(
                "Jito copy tip {} SOL (rivals' p50 {} p75 {} over {}, tip floor p75 {} SOL)",
                tip as f64 / 1e9,
                p50.map_or("-".into(), |v| (v as f64 / 1e9).to_string()),
                p75.map_or("-".into(), |v| (v as f64 / 1e9).to_string()),
                e.rival_jito_tips.len(),
                floor as f64 / 1e9
            )
        }))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One line for the stats log.
pub fn report() -> String {
    report_with(&ENGINE.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
pub(crate) fn set_table(class: Class, tier: u8, fee: u64, cap: u64) {
    TABLE[class as usize][tier as usize].store(fee, Ordering::Relaxed);
    CAP[class as usize][tier as usize].store(cap, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn set_floor(class: Class, tier: u8, floor: u64) {
    FLOOR[class as usize][tier as usize].store(floor, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        let mut s = Settings::from_env();
        s.mode = Mode::Table;
        s.buy_lamports = 100_000_000; // 0.1 SOL: an EV cap of 0.002 SOL
        s.global_cap_lamports = 5_000_000;
        s
    }

    #[test]
    fn tiers_cover_dev_reputation_and_mayhem() {
        assert_eq!(tier(500_000_000, Some(Follow::Recent), false), 0);
        assert_eq!(tier(2_000_000_000, Some(Follow::Recent), false), 1);
        assert_eq!(tier(5_000_000_000, Some(Follow::Profitable), false), 5);
        assert_eq!(tier(5_000_000_000, Some(Follow::Profitable), true), 11);
        assert_eq!(tier_name(5), "dev 3+ / profitable / plain");
        assert!((0..TIERS as u8).all(|t| (t as usize) < TIERS));
    }

    #[test]
    fn the_controller_moves_at_most_one_step_per_launch_and_waits_for_ten_outcomes() {
        let mut e = Engine::new(settings());
        let miss = Outcome { class: Class::Firedancer, rank_in_create_slot: None, our_fee_per_cu: 10_000_000, second_fee_per_cu: None, at_cap: false };
        for _ in 0..9 {
            e.on_outcome(miss);
        }
        assert_eq!(e.ctl[Class::Firedancer as usize].u, 0.0, "frozen below 10 outcomes");
        let step = (1.0 + e.s.step).ln();
        for _ in 0..50 {
            let before = e.ctl[Class::Firedancer as usize].u;
            e.on_outcome(miss);
            let moved = e.ctl[Class::Firedancer as usize].u - before;
            assert!(moved >= 0.0 && moved <= 3.0 * step + 1e-12, "one step at most ({moved})");
        }
        assert!(e.ctl[Class::Firedancer as usize].u > 0.0, "misses raise");
        // Wins by a margin bring it down, a quarter step at most a launch once the smoothing caught up.
        let win = Outcome { rank_in_create_slot: Some(1), our_fee_per_cu: 100_000_000, second_fee_per_cu: Some(10_000_000), ..miss };
        let high = e.ctl[Class::Firedancer as usize].u;
        for _ in 0..200 {
            let before = e.ctl[Class::Firedancer as usize].u;
            e.on_outcome(win);
            let moved = e.ctl[Class::Firedancer as usize].u - before;
            assert!(moved <= 3.0 * step + 1e-12 && moved >= -0.25 * step - 1e-12, "one step at most ({moved})");
        }
        assert!(e.ctl[Class::Firedancer as usize].u < high, "it came down");
        assert_eq!(e.ctl[Class::Agave as usize].samples, 0, "other classes untouched");
    }

    #[test]
    fn the_cap_is_the_larger_of_ev_and_the_median_first_fee_never_under_paladin() {
        let mut e = Engine::new(settings());
        // EV 8.87 lamports/CU (2% of 0.1 SOL less a 0.001 SOL tip over 112,700 CU) against
        // Firedancer's prior median 37.4: 37.4, held at the global cap's 35.49 (0.004 SOL).
        assert_eq!(e.cap(Class::Firedancer, 112_700, 1_000_000), 35_492_457);
        e.s.global_cap_lamports = 1_000_000_000;
        assert_eq!(e.cap(Class::Firedancer, 112_700, 1_000_000), 37_400_000);
        // The live median replaces the prior once there is one.
        e.first_median[Class::Firedancer as usize] = Some(50_000_000);
        assert_eq!(e.cap(Class::Firedancer, 112_700, 1_000_000), 50_000_000);
        // Jito-Agave: median 5.0 and EV 8.87, never under Paladin's 10.
        assert_eq!(e.cap(Class::JitoAgave, 112_700, 1_000_000), PALADIN_MIN);
        // A 10 SOL buy: the EV cap (0.199 SOL) is the larger.
        e.s.buy_lamports = 10_000_000_000;
        assert_eq!(e.cap(Class::JitoAgave, 112_700, 1_000_000), 1_765_749_778);
        // The table: a quiet market sits on Paladin's floor, contested tiers ×3, all under the cap.
        let row = e.row(Class::JitoAgave, Some(100_000), 112_700, 1_000_000, 1.0);
        assert!(row.iter().all(|(fee, _, floor)| *fee == PALADIN_MIN && *floor == PALADIN_MIN));
        for _ in 0..12 {
            e.note_launch(4, true);
        }
        let row = e.row(Class::Firedancer, Some(20_000_000), 112_700, 1_000_000, 1.0);
        assert_eq!((row[4].0, row[4].2), (60_000_000, 60_000_000));
        assert_eq!(row[0].0, 20_000_000);
        // A wild market: everything at the cap, the floor too.
        let e = Engine::new(settings());
        let row = e.row(Class::Firedancer, Some(1_000_000_000), 112_700, 1_000_000, 1.0);
        assert!(row.iter().all(|(fee, cap, floor)| *fee == *cap && *floor == *cap && *cap == 35_492_457));
    }

    #[test]
    fn the_jito_tip_starts_at_0_0003_and_follows_rivals_under_0_001() {
        let mut e = Engine::new(settings());
        assert_eq!(e.jito_tip(None).0, 300_000, "the start before anything is seen");
        assert_eq!(e.jito_tip(Some(25_000)).0, 300_000, "a low tip floor: the start");
        assert_eq!(e.jito_tip(Some(500_000)).0, 500_000, "the tip floor's p75 over the start");
        for t in [50_000u64, 60_000, 70_000, 80_000, 90_000, 400_000, 400_000, 500_000, 600_000, 700_000] {
            e.note_rival_jito_tip(t);
        }
        let (tip, p50, p75) = e.jito_tip(Some(25_000));
        assert_eq!((p50, p75, tip), (Some(400_000), Some(500_000), 400_000), "the rivals' median");
        for _ in 0..20 {
            e.note_rival_jito_tip(9_000_000);
        }
        assert_eq!(e.jito_tip(None).0, 1_000_000, "at most 0.001 SOL");
    }

    #[test]
    fn breakers_hold_under_synthetic_streams() {
        let mut e = Engine::new(settings());
        // Fill failures: 3 of 10 is not over 30%, 4 of 10 is.
        assert!(!(0..7).map(|_| e.on_fill(false)).any(|x| x));
        assert!(!(0..3).map(|_| e.on_fill(true)).fold(false, |_, x| x));
        assert!(e.on_fill(true), "4 of 11");
        // Regime: 30 attempts at the cap, never first or second: raising stops.
        let capped = Outcome { class: Class::JitoAgave, rank_in_create_slot: Some(9), our_fee_per_cu: 20_000_000, second_fee_per_cu: None, at_cap: true };
        for _ in 0..30 {
            e.on_outcome(capped);
        }
        assert!(e.ctl[Class::JitoAgave as usize].regime);
        let u = e.ctl[Class::JitoAgave as usize].u;
        e.on_outcome(capped);
        assert_eq!(e.ctl[Class::JitoAgave as usize].u, u, "no raise under the regime flag");
        // Jump: a first-position fee over 3× the rolling median is not followed.
        for _ in 0..12 {
            assert!(!e.note_first_fee(Class::Bam, 30_000_000));
        }
        assert!(e.note_first_fee(Class::Bam, 100_000_000));
        assert_eq!(e.ctl[Class::Bam as usize].explore, 20);
        assert_eq!(e.first_fees[Class::Bam as usize].back(), Some(&30_000_000), "the jump is not in the median");
        // Budget: counts failed buys' fees too and resets each day.
        e.s.daily_budget_lamports = 1_000_000;
        assert!(!e.on_spend(705_000, 1));
        assert!(e.on_spend(705_000, 1));
        assert!(!e.on_spend(705_000, 2), "a new day");
    }

    #[test]
    fn the_quote_reads_the_table_and_outbids_under_the_cap() {
        let s = settings();
        let off = Settings { mode: Mode::Off, ..s.clone() };
        assert_eq!(quote_with(&off, Class::Agave, 0, 100_000, 0, &[1; 32], 0), None, "off: the caller keeps its bid");
        set_table(Class::Frankendancer, 3, 12_000_000, 40_000_000);
        let q = quote_with(&s, Class::Frankendancer, 3, 100_000, 0, &[1; 32], 1_000_000).unwrap();
        assert_eq!((q.cu_price, q.raised, q.tip), (12_000_000, false, None));
        // A visible rival at 20: 10% over it.
        let q = quote_with(&s, Class::Frankendancer, 3, 100_000, 20_000_000, &[1; 32], 1_000_000).unwrap();
        assert_eq!((q.cu_price, q.raised), (22_000_001, true));
        // A rival at 50: capped at 40.
        let q = quote_with(&s, Class::Frankendancer, 3, 100_000, 50_000_000, &[1; 32], 1_000_000).unwrap();
        assert_eq!(q.cu_price, 40_000_000);
        // A rival under the table: the table.
        let q = quote_with(&s, Class::Frankendancer, 3, 100_000, 5_000_000, &[1; 32], 1_000_000).unwrap();
        assert_eq!((q.cu_price, q.raised), (12_000_000, false));
        // Harmonic, when bought: the Jito-based tip, at least the lane's floor, at most the tip cap.
        let mut harmonic = s.clone();
        harmonic.route[Class::Harmonic as usize] = Route::Fee;
        set_table(Class::Harmonic, 0, 12_000_000, 40_000_000);
        HARMONIC_TIP.store(1_500_000, Ordering::Relaxed);
        let q = quote_with(&harmonic, Class::Harmonic, 0, 100_000, 0, &[1; 32], 1_000_000).unwrap();
        assert_eq!(q.tip, Some(1_500_000));
        HARMONIC_TIP.store(100, Ordering::Relaxed);
        let q = quote_with(&harmonic, Class::Harmonic, 0, 100_000, 0, &[1; 32], 1_000_000).unwrap();
        assert_eq!(q.tip, None, "the lane's floor");
        // Not computed yet: the caller keeps its bid.
        assert_eq!(quote_with(&s, Class::Other, 7, 100_000, 0, &[1; 32], 0), None);
        // Routes (the owner's): JitoLabs behind a relayer, a Jito bundle only at the class floor;
        // BAM and Firedancer, the table's fee through the gateways only; Harmonic and unknown
        // clients skipped.
        let jito = lane_bit(super::super::lanes::LANE_JITO);
        let gateways = lane_bit(super::super::lanes::LANE_ZEROSLOT) | lane_bit(super::super::lanes::LANE_EVERSTAKE);
        JITO_TIP.store(300_000, Ordering::Relaxed);
        set_table(Class::JitoRelayer, 2, 30_000_000, 40_000_000);
        set_floor(Class::JitoRelayer, 2, 11_000_000);
        let q = quote_with(&s, Class::JitoRelayer, 2, 100_000, 50_000_000, &[1; 32], 1_000_000).unwrap();
        assert_eq!((q.cu_price, q.jito_tip, q.raised, q.lanes), (11_000_000, Some(300_000), false, jito), "a bundle only, no outbid");
        set_table(Class::Bam, 2, 30_000_000, 40_000_000);
        let q = quote_with(&s, Class::Bam, 2, 100_000, 0, &[1; 32], 1_000_000).unwrap();
        assert_eq!((q.cu_price, q.jito_tip, q.lanes), (30_000_000, None, gateways));
        set_table(Class::Firedancer, 2, 30_000_000, 40_000_000);
        let q = quote_with(&s, Class::Firedancer, 2, 100_000, 0, &[1; 32], 1_000_000).unwrap();
        assert_eq!((q.jito_tip, q.lanes), (None, gateways));
        assert!(skips_with(&s, Class::Harmonic) && skips_with(&s, Class::Other));
        assert!(!skips_with(&s, Class::Firedancer) && !skips_with(&s, Class::JitoRelayer));
        // Fee-under-cap: skipped while the class's median first-buyer fee is over its cap.
        let mut under = s.clone();
        under.route[Class::Harmonic as usize] = Route::FeeUnderCap;
        SKIP_CLASS[Class::Harmonic as usize].store(true, Ordering::Relaxed);
        assert!(skips_with(&under, Class::Harmonic));
        SKIP_CLASS[Class::Harmonic as usize].store(false, Ordering::Relaxed);
        assert!(!skips_with(&under, Class::Harmonic));
        // A class's own gateways: only those, each paid its tip.
        let mut one = s.clone();
        one.gateways[Class::Bam as usize] = lane_bit(super::super::lanes::LANE_EVERSTAKE);
        assert_eq!(quote_with(&one, Class::Bam, 2, 100_000, 0, &[1; 32], 1_000_000).unwrap().lanes, lane_bit(super::super::lanes::LANE_EVERSTAKE));
        // JitoLabs without a relayer (the owner's, 8 Oct): launch by launch in turn, a Jito bundle
        // alone at the class floor, then one gateway copy at the table's fee.
        assert_eq!(s.route[Class::JitoAgave as usize], Route::Alternate);
        set_table(Class::JitoAgave, 2, 30_000_000, 40_000_000);
        set_floor(Class::JitoAgave, 2, 11_000_000);
        let turns: Vec<(u64, u8, Option<u8>)> = (0..4)
            .map(|_| quote_with(&s, Class::JitoAgave, 2, 100_000, 0, &[1; 32], 1_000_000).unwrap())
            .map(|q| (q.cu_price, q.lanes, q.arm))
            .collect();
        let first = turns[0].2.unwrap() as usize;
        for (k, t) in turns.iter().enumerate() {
            let jito_turn = (first + k) % 2 == 0;
            assert_eq!(*t, if jito_turn { (11_000_000, jito, Some(0)) } else { (30_000_000, gateways, Some(1)) }, "turn {k}");
        }
    }

    #[test]
    fn ladder_arms_stay_balanced_within_a_class() {
        let mut s = Settings { mode: Mode::Ladder, ..settings() };
        let fees = [2_000_000u64, 5_000_000, 10_000_000];
        s.ladder[Class::Frankendancer as usize] = fees.iter().map(|l| Arm::Fee(*l)).collect();
        s.ladder[Class::Agave as usize] = vec![Arm::Jito(100_000), Arm::Jito(500_000), Arm::Jito(2_000_000)];
        s.ladder[Class::Other as usize] = vec![Arm::Fee(10_000_000), Arm::Jito(500_000)];
        s.ladder[Class::Harmonic as usize] = vec![Arm::Skip];
        set_floor(Class::Agave, 0, 12_000_000);
        let mint = |m: u8| {
            let mut mint = [0u8; 32];
            mint[0] = m.wrapping_mul(37);
            mint[3] = m;
            mint
        };
        let mut picks = [0u32; 3];
        for m in 0..30u8 {
            let q = quote_with(&s, Class::Frankendancer, 0, 100_000, 0, &mint(m), 0).unwrap();
            let arm = q.arm.unwrap() as usize;
            picks[arm] += 1;
            assert_eq!((q.cu_price, q.jito_tip), (fees[arm] * 1_000_000 / 100_000, None), "the arm's total fee at the limit");
        }
        assert_eq!(picks, [10, 10, 10]);
        // Jito arms: a bundle only, the class floor as priority fee, the arm's tip.
        let mut tips = std::collections::BTreeMap::new();
        for m in 0..30u8 {
            let q = quote_with(&s, Class::Agave, 0, 100_000, 0, &mint(m), 0).unwrap();
            assert_eq!((q.cu_price, q.lanes), (12_000_000, lane_bit(super::super::lanes::LANE_JITO)));
            *tips.entry(q.jito_tip.unwrap()).or_insert(0) += 1;
        }
        assert_eq!(tips.values().copied().collect::<Vec<_>>(), vec![10, 10, 10]);
        // Two kinds alternate; a skip class is never bought.
        let kinds: Vec<bool> = (0..10u8).map(|m| quote_with(&s, Class::Other, 0, 100_000, 0, &mint(m), 0).unwrap().jito_tip.is_some()).collect();
        assert_eq!(kinds.iter().filter(|j| **j).count(), 5);
        assert!(skips_with(&s, Class::Harmonic));
        assert_eq!(quote_with(&s, Class::Harmonic, 0, 100_000, 0, &mint(1), 0), None);
        assert_eq!(Arm::parse("jito:0.0005"), Some(Arm::Jito(500_000)));
        assert_eq!(Arm::parse("fee:0.010"), Some(Arm::Fee(10_000_000)));
        assert_eq!(Arm::parse("0.002"), Some(Arm::Fee(2_000_000)));
        assert_eq!(Arm::parse("skip"), Some(Arm::Skip));
    }
}
