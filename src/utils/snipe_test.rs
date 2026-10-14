//! The snipe test (`SNIPE_TEST=1`, the owner's spec of 2026-10-13): a small, capped, measured
//! version of the bot as it trades, to find out whether its own filters (the follow rule and the
//! scorecard) and its landing position have an edge that a market recording could not show.
//!
//! Pre-registered rule: followed deployer + scorecard >= 61 (both unchanged, applied first) + the
//! four filter rules below + the exit below. Off (the default), nothing here runs and the bot
//! trades as before.
//!
//! Filter, on what the bot sees when it decides (the create transaction and the buys of its bundle
//! inside the bundle window; the owner's fix 2: never the whole create slot, which is not known
//! then):
//! 1. dev buy under 1 SOL;
//! 2. at most one buyer besides the dev in the bundle (an unreadable buyer: refused);
//! 3. real SOL in the curve after the bundle under 1 SOL;
//! 4. the curve not complete.
//!
//! Exit, whichever comes first: all out at +30% (the executable value of the tokens over what they
//! cost), or all out 15 s after the fill. No stop-loss, no trailing. The full sell closes the token
//! account in the same transaction (the pump.fun sell builder always does), so its rent comes back;
//! rent left unrecovered (a position not fully sold) is counted in the PnL.
//!
//! Size: `SNIPE_TEST_SOL` (0.1), set by hand in .env; never scaled, never changed by the dashboard.
//! Caps, each stopping new entries (open positions are still sold): `SNIPE_TEST_MAX_PER_DAY` (50)
//! buys a UTC day, `SNIPE_TEST_MAX_PER_HOUR` (2) a UTC hour (fix 3), and a realized loss of
//! `SNIPE_TEST_MAX_LOSS_SOL` (0.5) in the UTC day. The counts survive a restart (they are read back
//! from the log at start).
//!
//! One CSV line per snipe (`logs/snipe_test.csv`, and table `snipe_test` in the store) once it is
//! over; `tools/snipe_test_report.py` makes the daily report and the gate from it.
//!
//! The filter and the caps run on the hot path's decision thread: allocation-free (no allocation in
//! `filter`, `check_caps`, `try_enter`; a mutex without contention is a futex, no allocation).

use {
    crate::config::{env_opt, lamports_to_sol},
    log::{info, warn},
    std::{
        collections::HashMap,
        io::Write,
        sync::{LazyLock, Mutex},
        time::Duration,
    },
};

pub const LOG_PATH: &str = "logs/snipe_test.csv";
pub const DEV_BUY_MAX_LAMPORTS: u64 = 1_000_000_000;
pub const BUNDLE_MAX_WALLETS: u32 = 1;
pub const REAL_SOL_MAX_LAMPORTS: u64 = 1_000_000_000;
pub const TAKE_PROFIT_X: f64 = 1.30;
pub const MAX_HOLD: Duration = Duration::from_secs(15);
/// How long after the create the line waits for the regular stream's view of the whole create slot.
const FULL_SLOT_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub on: bool,
    /// SNIPE_TEST_RULES=on: the four launch rules and the +30% / 15 s exit (off: launches trade by
    /// bigboss's own rules and exit).
    pub rules_on: bool,
    /// SNIPE_TEST_POOL=on: PumpSwap graduations are bought too (`processors::graduation`).
    pub pool_on: bool,
    pub size_lamports: u64,
    pub max_loss_lamports: u64,
    pub max_per_day: u32,
    pub max_per_hour: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { on: false, rules_on: false, pool_on: false, size_lamports: 100_000_000, max_loss_lamports: 500_000_000, max_per_day: 100, max_per_hour: 4 }
    }
}

/// A SOL amount as typed, in lamports: above 0, at most 9 decimals.
fn sol_lamports(text: &str) -> Option<u64> {
    let t = text.trim();
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if whole.is_empty() && frac.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) || frac.len() > 9 || whole.len() > 6 {
        return None;
    }
    let l = whole.parse::<u64>().unwrap_or(0) * 1_000_000_000 + format!("{frac:0<9}").parse::<u64>().ok()?;
    (l > 0).then_some(l)
}

impl Settings {
    /// From the environment; an unreadable value is an error (the bot does not start: a size or a
    /// cap is never guessed).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Self::default();
        let flag = |key: &str| get(key).is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"));
        let on = flag("SNIPE_TEST");
        let sol = |key: &str, default: u64| match get(key) {
            None => Ok(default),
            Some(v) if v.trim().is_empty() => Ok(default),
            Some(v) => sol_lamports(&v).ok_or_else(|| format!("{key}={v:?} is not a SOL amount above 0")),
        };
        let count = |key: &str, default: u32| match get(key) {
            None => Ok(default),
            Some(v) if v.trim().is_empty() => Ok(default),
            Some(v) => v.trim().parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| format!("{key}={v:?} is not a whole number above 0")),
        };
        let s = Self {
            on,
            rules_on: flag("SNIPE_TEST_RULES"),
            pool_on: flag("SNIPE_TEST_POOL"),
            size_lamports: sol("SNIPE_TEST_SOL", d.size_lamports)?,
            max_loss_lamports: sol("SNIPE_TEST_MAX_LOSS_SOL", d.max_loss_lamports)?,
            max_per_day: count("SNIPE_TEST_MAX_PER_DAY", d.max_per_day)?,
            max_per_hour: count("SNIPE_TEST_MAX_PER_HOUR", d.max_per_hour)?,
        };
        if s.size_lamports as f64 / 1e9 > crate::config::MAX_RUNTIME_BUY_SOL {
            return Err(format!("SNIPE_TEST_SOL is above the {} SOL safety limit", crate::config::MAX_RUNTIME_BUY_SOL));
        }
        Ok(s)
    }
}

static SETTINGS: LazyLock<Result<Settings, String>> = LazyLock::new(|| Settings::from_lookup(env_opt));

/// The settings (the defaults, off, when they could not be read: `check_settings` stops the bot then).
pub fn settings() -> &'static Settings {
    static OFF: Settings = Settings { on: false, rules_on: false, pool_on: false, size_lamports: 0, max_loss_lamports: 0, max_per_day: 0, max_per_hour: 0 };
    SETTINGS.as_ref().unwrap_or(&OFF)
}

/// For the start: an error when SNIPE_TEST's values cannot be read.
pub fn check_settings() -> Result<(), String> {
    SETTINGS.as_ref().map(|_| ()).map_err(Clone::clone)
}

#[inline]
pub fn on() -> bool {
    settings().on
}

/// The four launch rules and the +30% / 15 s exit (SNIPE_TEST_RULES=on).
#[inline]
pub fn rules_on() -> bool {
    let s = settings();
    s.on && s.rules_on
}

/// PumpSwap graduations (SNIPE_TEST_POOL=on).
#[inline]
pub fn pool_on() -> bool {
    let s = settings();
    s.on && s.pool_on
}

/// The test's size (lamports): every buy, never scaled.
#[inline]
pub fn size_lamports() -> u64 {
    settings().size_lamports
}

/// Why the test does not buy a launch the bot's own rules passed. Codes follow `hot::decide::Reject`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TestReject {
    DevBuy = 40,
    Bundle = 41,
    BundleUnknown = 42,
    RealSol = 43,
    Complete = 44,
    DayCap = 45,
    HourCap = 46,
    LossCap = 47,
    Wallet = 48,
}

impl TestReject {
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The skip reason (the activity feed, the launches table, the funnel): each starts "snipe test:".
    pub fn text(self) -> &'static str {
        match self {
            TestReject::DevBuy => "snipe test: dev buy 1 SOL or more",
            TestReject::Bundle => "snipe test: 2+ buyers besides the dev in the bundle",
            TestReject::BundleUnknown => "snipe test: a bundle buyer unreadable",
            TestReject::RealSol => "snipe test: 1+ SOL real in the curve after the bundle",
            TestReject::Complete => "snipe test: curve complete",
            TestReject::DayCap => "snipe test: the day's snipe cap is reached",
            TestReject::HourCap => "snipe test: the hour's snipe cap is reached",
            TestReject::LossCap => "snipe test: the day's loss cap is reached",
            TestReject::Wallet => "snipe test: the wallet can't pay the size",
        }
    }
}

/// What the decision sees of a launch: the create and its bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Visible {
    pub dev_buy_lamports: u64,
    /// Distinct buyers besides the deployer and the creator; None: one unreadable.
    pub bundle_wallets: Option<u32>,
    /// Real SOL in the curve after the bundle (the dev buy included).
    pub real_sol_lamports: u64,
    pub complete: bool,
}

/// The four rules, in order. Allocation-free.
#[inline]
pub fn filter(v: &Visible) -> Result<(), TestReject> {
    if v.dev_buy_lamports >= DEV_BUY_MAX_LAMPORTS {
        return Err(TestReject::DevBuy);
    }
    match v.bundle_wallets {
        None => return Err(TestReject::BundleUnknown),
        Some(n) if n > BUNDLE_MAX_WALLETS => return Err(TestReject::Bundle),
        Some(_) => {}
    }
    if v.real_sol_lamports >= REAL_SOL_MAX_LAMPORTS {
        return Err(TestReject::RealSol);
    }
    if v.complete {
        return Err(TestReject::Complete);
    }
    Ok(())
}

/// The caps' state: entries in the UTC day and hour, and the day's realized PnL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    day: u64,
    entries: u32,
    hour: u64,
    hour_entries: u32,
    pnl_lamports: i64,
}

impl Caps {
    fn roll(&mut self, now: u64) {
        let (day, hour) = (now / 86_400, now / 3_600);
        if day != self.day {
            *self = Caps { day, hour, ..Default::default() };
        } else if hour != self.hour {
            self.hour = hour;
            self.hour_entries = 0;
        }
    }

    /// Whether a new entry may go now (loss first, then the day, then the hour).
    pub fn check(&mut self, s: &Settings, now: u64) -> Result<(), TestReject> {
        self.roll(now);
        if self.pnl_lamports <= -(s.max_loss_lamports as i64) {
            return Err(TestReject::LossCap);
        }
        if self.entries >= s.max_per_day {
            return Err(TestReject::DayCap);
        }
        if self.hour_entries >= s.max_per_hour {
            return Err(TestReject::HourCap);
        }
        Ok(())
    }

    /// `check`, then counts the entry.
    pub fn enter(&mut self, s: &Settings, now: u64) -> Result<(), TestReject> {
        self.check(s, now)?;
        self.entries += 1;
        self.hour_entries += 1;
        Ok(())
    }

    /// A snipe's realized PnL (lamports), counted in the UTC day it ended (`at`).
    pub fn result(&mut self, pnl_lamports: i64, at: u64) {
        if at / 86_400 > self.day {
            self.roll(at);
        }
        if at / 86_400 == self.day {
            self.pnl_lamports += pnl_lamports;
        }
    }

    /// Entries today, this hour, and today's PnL (lamports), as of `now`.
    pub fn view(&mut self, now: u64) -> (u32, u32, i64) {
        self.roll(now);
        (self.entries, self.hour_entries, self.pnl_lamports)
    }
}

static CAPS: Mutex<Caps> = Mutex::new(Caps { day: 0, entries: 0, hour: 0, hour_entries: 0, pnl_lamports: 0 });

/// Read-only: may an entry go now? Allocation-free.
#[inline]
pub fn check_caps(now: u64) -> Result<(), TestReject> {
    CAPS.lock().unwrap_or_else(|e| e.into_inner()).check(settings(), now)
}

/// Takes an entry (at the buy's commit point: the position reserved, the bytes about to go out).
/// Allocation-free.
#[inline]
pub fn try_enter(now: u64) -> Result<(), TestReject> {
    CAPS.lock().unwrap_or_else(|e| e.into_inner()).enter(settings(), now)
}

/// Entries today, this hour, today's PnL (lamports).
pub fn caps_view(now: u64) -> (u32, u32, i64) {
    CAPS.lock().unwrap_or_else(|e| e.into_inner()).view(now)
}

/// The test's exit: +30% or 15 s, whichever first, all out. `x` is the executable value of the
/// tokens over their cost.
pub fn exit(held_for: Duration, x: f64) -> Option<&'static str> {
    if x >= TAKE_PROFIT_X {
        Some("TP")
    } else if held_for >= MAX_HOLD {
        Some("TIME")
    } else {
        None
    }
}

// ---- The per-snipe record and its line.

/// What the decision knew (`note_entry`).
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub mint: String,
    pub mode: &'static str,
    /// "launch" (a pump.fun launch) or "graduation" (a PumpSwap pool at migration).
    pub kind: &'static str,
    pub grad: Option<Grad>,
    pub path: &'static str,
    pub deployer: String,
    pub follow_kind: &'static str,
    pub score: Option<i32>,
    /// The scorecard's facts, "name=value:points;..." (no commas).
    pub facts: String,
    pub wallet_age_h: Option<f64>,
    pub funded_sol: Option<f64>,
    pub mayhem: bool,
    pub create_slot: u64,
    pub decided_unix_ms: u64,
    pub visible: Visible,
    pub bundle_lamports: u64,
    /// The curve's price after the bundle, lamports per token unit.
    pub price_after_bundle: f64,
    pub size_lamports: u64,
    pub token_2022: bool,
}

/// A graduation's facts: how fast the curve completed, the pool's opening price, its price when the
/// migration slot ended (over the opening), whether the insiders' jump (1.5x the opening) came
/// before our fill, and the pool's price at +30, +60, +120, +240 and +600 s after our fill.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grad {
    pub fast_ms: u64,
    pub open_price: f64,
    pub slot_end_x: f64,
    pub jump_before_fill: Option<bool>,
    pub max_x_before_fill: Option<f64>,
    pub prices: [Option<f64>; 5],
}

/// The seconds after the fill at which a graduation's pool price is logged.
pub const PRICE_OFFSETS: [u64; 5] = [30, 60, 120, 240, 600];

/// The whole create slot as the regular stream recorded it (`tape_recorder::create_slot_view`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FullSlot {
    pub buyers: u32,
    pub real_sol_lamports: u64,
    pub complete: bool,
}

/// Where our buy landed: slot, block index, its rank among the coin's trades in that slot (1 =
/// first), and where that came from ("chain" from the stream, "paper" the drawn landing).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Landing {
    pub slot: u64,
    pub index: Option<u64>,
    pub rank: Option<u32>,
    pub source: &'static str,
}

#[derive(Clone, Debug, Default)]
struct Snipe {
    entry: Entry,
    buy_status: String,
    landed: bool,
    tokens: u64,
    sol_in: u64,
    buy_wallet: Option<i64>,
    landed_slot: Option<u64>,
    landing: Option<Landing>,
    priority_lamports: Option<u64>,
    tip_lamports: Option<u64>,
    tokens_left: u64,
    sells: u32,
    sol_out: u64,
    sell_wallet: i64,
    sell_wallet_known: bool,
    reasons: Vec<String>,
    exit_at_ms: Option<u64>,
    buy_at_ms: Option<u64>,
    full: Option<FullSlot>,
    /// Its PnL is in the caps already (`count_result`).
    counted: bool,
}

static SNIPES: LazyLock<Mutex<HashMap<String, Snipe>>> = LazyLock::new(Default::default);

/// The decision to buy (the commit point took an entry).
pub fn note_entry(entry: Entry) {
    if !on() {
        return;
    }
    let mut s = SNIPES.lock().unwrap_or_else(|e| e.into_inner());
    if s.len() > 1_000 {
        s.clear();
    }
    s.insert(entry.mint.clone(), Snipe { entry, ..Default::default() });
}

/// The buy's priority fee and tip (live, from the hot path's send).
pub fn note_fees(mint: &str, priority_lamports: u64, tip_lamports: u64) {
    if let Some(s) = SNIPES.lock().unwrap_or_else(|e| e.into_inner()).get_mut(mint) {
        s.priority_lamports = Some(priority_lamports);
        s.tip_lamports = Some(tip_lamports);
    }
}

/// A paper buy's drawn landing (slot, block index).
pub fn note_paper_landing(mint: &str, slot: u64, index: u64) {
    if let Some(s) = SNIPES.lock().unwrap_or_else(|e| e.into_inner()).get_mut(mint) {
        s.landing = Some(Landing { slot, index: Some(index), rank: None, source: "paper" });
    }
}

fn lamports_of(v: Option<f64>) -> Option<i64> {
    v.map(|x| (x * 1e9).round() as i64)
}

/// Every trade-log row (`csv_logger::log_trade`): a buy's result, a sell. A failed buy ends the
/// snipe at once.
pub fn on_trade(r: &super::csv_logger::TradeRecord, mode: &'static str) {
    if !on() {
        return;
    }
    let landed = |action: &str, status: &str| match action {
        "BUY" => status.starts_with("simulated") || status.starts_with("confirmed"),
        _ => status.starts_with("paper") || status.starts_with("confirmed"),
    };
    let ok = landed(r.action, &r.status);
    let now = crate::utils::dashboard::now_unix_ms();
    let finish_failed = {
        let mut map = SNIPES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = map.get_mut(&r.mint) else { return };
        if s.entry.mode != mode {
            return;
        }
        if r.action == "BUY" {
            s.buy_status = r.status.clone();
            s.landed = ok;
            s.sol_in = lamports_of(r.sol_in).unwrap_or(0).max(0) as u64;
            s.tokens = if ok { r.token_amount.unwrap_or(0) } else { 0 };
            s.tokens_left = s.tokens;
            s.buy_wallet = lamports_of(r.wallet_sol_change);
            s.landed_slot = r.landed_slot;
            s.buy_at_ms = Some(now);
            !ok
        } else {
            s.sells += 1;
            s.reasons.push(r.reason.clone());
            if ok {
                s.tokens_left = s.tokens_left.saturating_sub(r.token_amount.unwrap_or(0));
                s.sol_out += lamports_of(r.sol_out).unwrap_or(0).max(0) as u64;
                s.exit_at_ms = Some(now);
            }
            if let Some(w) = lamports_of(r.wallet_sol_change) {
                s.sell_wallet += w;
                s.sell_wallet_known = true;
            }
            false
        }
    };
    if finish_failed {
        finish(&r.mint);
    }
}

/// A graduation's facts, as they come in (the jump, the prices after the fill).
pub fn note_grad(mint: &str, f: impl FnOnce(&mut Grad)) {
    if let Some(s) = SNIPES.lock().unwrap_or_else(|e| e.into_inner()).get_mut(mint) {
        f(s.entry.grad.get_or_insert_with(Grad::default));
    }
}

/// The snipe's PnL into the caps now (a graduation is sold at 240 s, its line written at 600 s).
pub fn count_result(mint: &str) {
    let mut map = SNIPES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = map.get_mut(mint) {
        if !s.counted {
            s.counted = true;
            let pnl = pnl_lamports(s, rent_of(s.entry.token_2022));
            CAPS.lock().unwrap_or_else(|e| e.into_inner()).result(pnl, crate::utils::dashboard::now_unix());
        }
    }
}

/// The position's monitor has stopped (sold out, or given up): the snipe is over.
pub fn position_done(mint: &str) {
    if on() && SNIPES.lock().unwrap_or_else(|e| e.into_inner()).contains_key(mint) {
        finish(mint);
    }
}

/// Writes the snipe's line once the regular stream has the whole create slot (a few seconds after
/// the create), and counts its PnL in the caps.
fn finish(mint: &str) {
    let mint = mint.to_string();
    let run = async move {
        let decided = SNIPES.lock().unwrap_or_else(|e| e.into_inner()).get(&mint).map(|s| s.entry.decided_unix_ms).unwrap_or(0);
        let ready_at = decided + FULL_SLOT_WAIT.as_millis() as u64;
        let now = crate::utils::dashboard::now_unix_ms();
        if ready_at > now {
            tokio::time::sleep(Duration::from_millis(ready_at - now)).await;
        }
        let Some(mut s) = SNIPES.lock().unwrap_or_else(|e| e.into_inner()).remove(&mint) else { return };
        let wallet = crate::config::trading_wallet();
        // A launch: the whole create slot and our landing from the tape (a graduation has neither).
        if s.entry.kind != "graduation" {
            if let Some((full, ours)) = mint.parse::<solana_pubkey::Pubkey>().ok().and_then(|key| super::tape_recorder::create_slot_view(&key, &wallet)) {
                s.full = Some(full);
                if s.entry.mode == "live" {
                    if let Some(l) = ours {
                        s.landing = Some(l);
                    }
                }
            }
        }
        let line = line_of(&s, rent_of(s.entry.token_2022));
        if !s.counted {
            let pnl = pnl_lamports(&s, rent_of(s.entry.token_2022));
            CAPS.lock().unwrap_or_else(|e| e.into_inner()).result(pnl, crate::utils::dashboard::now_unix());
        }
        info!("snipe test {mint}: {}", line);
        write_line(&line);
    };
    match RUNTIME.get().cloned().or_else(|| tokio::runtime::Handle::try_current().ok()) {
        Some(h) => {
            h.spawn(run);
        }
        None => warn!("snipe test: no runtime to write the line of a snipe"),
    }
}

/// The runtime the lines are written on (`log_trade` may run on the file writer's thread).
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// At start, on the runtime: keeps its handle, reads the rents from the chain and today's caps from
/// the log. Returns (entries today, this hour, today's PnL in lamports).
pub async fn init(mode: &str) -> (u32, u32, i64) {
    let _ = RUNTIME.set(tokio::runtime::Handle::current());
    load_rent().await;
    load_caps(mode, crate::utils::dashboard::now_unix())
}

/// The token-account rent (lamports): the chain's value, read at start (`load_rent`), else today's.
pub fn rent_of(token_2022: bool) -> u64 {
    let r = RENT.lock().unwrap_or_else(|e| e.into_inner());
    if token_2022 { r.1 } else { r.0 }
}

/// Classic SPL (165 bytes) and Token-2022 with immutable owner (170 bytes), from the chain on
/// 2026-10-13: 1,488,440 and 1,513,840 lamports.
static RENT: Mutex<(u64, u64)> = Mutex::new((1_488_440, 1_513_840));

/// Reads the two rents from the chain (getMinimumBalanceForRentExemption); kept as they are on failure.
pub async fn load_rent() {
    let rpc = &*crate::config::RPC_CLIENT;
    if let (Ok(spl), Ok(t22)) = (rpc.get_minimum_balance_for_rent_exemption(165).await, rpc.get_minimum_balance_for_rent_exemption(170).await) {
        *RENT.lock().unwrap_or_else(|e| e.into_inner()) = (spl, t22);
        crate::store::trades::set_rent(spl, t22);
        info!("token account rent: {spl} lamports (SPL), {t22} lamports (Token-2022)");
    }
}

/// Rent left in the token account: a live position whose tokens were not all sold (the close goes
/// with the full sell only). Paper never pays the rent (the paper buy's modeled cost has none).
fn rent_unrecovered(s: &Snipe, rent: u64) -> u64 {
    if s.entry.mode == "live" && s.landed && s.tokens_left > 0 { rent } else { 0 }
}

/// The snipe's realized PnL (lamports) after every cost: the wallet's change over the buy and its
/// sells (live: measured; paper: modeled fees). The rent paid at the buy is in it, and so is its
/// refund by the closing sell; without a measured wallet change, SOL out - SOL in - the rent left.
fn pnl_lamports(s: &Snipe, rent: u64) -> i64 {
    match s.buy_wallet {
        Some(b) if !s.landed || s.sell_wallet_known || s.sells == 0 => b + s.sell_wallet,
        _ => s.sol_out as i64 - s.sol_in as i64 - rent_unrecovered(s, rent) as i64,
    }
}

pub const HEADER: &str = "date,mode,kind,path,mint,deployer,follow_kind,score,create_slot,decided_unix_ms,dev_buy_sol,bundle_wallets,bundle_sol,real_sol_visible,complete_visible,full_slot_buyers,full_slot_real_sol,full_slot_complete,filter_full_slot,buy_status,landed_slot,landed_index,landed_rank,slot_offset,landing_source,size_sol,tokens,fill_price,price_after_bundle,fill_vs_bundle_pct,priority_fee_sol,tip_sol,buy_fees_sol,exit_trigger,exit_reasons,exit_sol,sell_fees_sol,hold_secs,rent_unrecovered_sol,pnl_sol,pnl_msol,wallet_age_h,funded_sol,mayhem,facts,grad_fast_ms,open_price,slot_end_x,jump_before_fill,max_x_before_fill,px_30s,px_60s,px_120s,px_240s,px_600s";

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn sol9(l: i64) -> String {
    format!("{:.9}", l as f64 / 1e9)
}

/// The CSV line of a finished snipe (fields never contain commas).
fn line_of(s: &Snipe, rent: u64) -> String {
    let e = &s.entry;
    let clean = |t: &str| t.replace([',', '\n', '\r'], ";");
    let date = chrono::DateTime::from_timestamp_millis(e.decided_unix_ms as i64).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    let full_verdict = s.full.map(|f| {
        let v = Visible { dev_buy_lamports: e.visible.dev_buy_lamports, bundle_wallets: Some(f.buyers), real_sol_lamports: f.real_sol_lamports, complete: f.complete };
        match filter(&v) {
            Ok(()) => "pass".to_string(),
            Err(r) => format!("{:?}", r),
        }
    });
    let landing = s.landing.or_else(|| s.landed_slot.map(|slot| Landing { slot, index: None, rank: None, source: "confirm" }));
    let fill_price = (s.tokens > 0).then(|| s.sol_in as f64 / s.tokens as f64);
    let vs = fill_price.filter(|_| e.price_after_bundle > 0.0).map(|p| (p / e.price_after_bundle - 1.0) * 100.0);
    let buy_fees = s.buy_wallet.map(|w| -w - s.sol_in as i64 * s.landed as i64 - if s.entry.mode == "live" && s.landed { rent as i64 } else { 0 });
    let sell_fees = (s.sells > 0 && s.sell_wallet_known).then(|| s.sol_out as i64 + if s.entry.mode == "live" && s.tokens_left == 0 && s.landed { rent as i64 } else { 0 } - s.sell_wallet);
    let trigger = s.reasons.first().cloned().unwrap_or_default();
    let hold = match (s.buy_at_ms, s.exit_at_ms) {
        (Some(b), Some(x)) => Some(format!("{:.1}", x.saturating_sub(b) as f64 / 1000.0)),
        _ => None,
    };
    let pnl = pnl_lamports(s, rent);
    let g = e.grad.clone().unwrap_or_default();
    let is_grad = e.kind == "graduation";
    let fields: Vec<String> = vec![
        date,
        e.mode.into(),
        (if is_grad { "graduation" } else { "launch" }).into(),
        e.path.into(),
        e.mint.clone(),
        e.deployer.clone(),
        e.follow_kind.into(),
        opt(e.score),
        e.create_slot.to_string(),
        e.decided_unix_ms.to_string(),
        format!("{:.9}", lamports_to_sol(e.visible.dev_buy_lamports)),
        opt(e.visible.bundle_wallets),
        format!("{:.9}", lamports_to_sol(e.bundle_lamports)),
        format!("{:.9}", lamports_to_sol(e.visible.real_sol_lamports)),
        (e.visible.complete as u8).to_string(),
        opt(s.full.map(|f| f.buyers)),
        opt(s.full.map(|f| format!("{:.9}", lamports_to_sol(f.real_sol_lamports)))),
        opt(s.full.map(|f| f.complete as u8)),
        opt(full_verdict),
        clean(&s.buy_status),
        opt(landing.map(|l| l.slot)),
        opt(landing.and_then(|l| l.index)),
        opt(landing.and_then(|l| l.rank)),
        opt(landing.map(|l| l.slot.saturating_sub(e.create_slot))),
        opt(landing.map(|l| l.source)),
        format!("{:.9}", lamports_to_sol(e.size_lamports)),
        s.tokens.to_string(),
        opt(fill_price.map(|p| format!("{p:.12}"))),
        format!("{:.12}", e.price_after_bundle),
        opt(vs.map(|v| format!("{v:.3}"))),
        opt(s.priority_lamports.map(|l| sol9(l as i64))),
        opt(s.tip_lamports.map(|l| sol9(l as i64))),
        opt(buy_fees.map(sol9)),
        trigger,
        s.reasons.join("|"),
        format!("{:.9}", lamports_to_sol(s.sol_out)),
        opt(sell_fees.map(sol9)),
        opt(hold),
        sol9(rent_unrecovered(s, rent) as i64),
        sol9(pnl),
        format!("{:.3}", pnl as f64 / 1e6),
        opt(e.wallet_age_h.map(|h| format!("{h:.1}"))),
        opt(e.funded_sol.map(|f| format!("{f:.3}"))),
        (e.mayhem as u8).to_string(),
        clean(&e.facts),
        if is_grad { g.fast_ms.to_string() } else { String::new() },
        if is_grad { format!("{:.12}", g.open_price) } else { String::new() },
        if is_grad { format!("{:.4}", g.slot_end_x) } else { String::new() },
        opt(g.jump_before_fill.map(|j| j as u8)),
        opt(g.max_x_before_fill.map(|x| format!("{x:.4}"))),
        opt(g.prices[0].map(|p| format!("{p:.12}"))),
        opt(g.prices[1].map(|p| format!("{p:.12}"))),
        opt(g.prices[2].map(|p| format!("{p:.12}"))),
        opt(g.prices[3].map(|p| format!("{p:.12}"))),
        opt(g.prices[4].map(|p| format!("{p:.12}"))),
    ];
    fields.into_iter().map(|f| clean(&f)).collect::<Vec<_>>().join(",")
}

fn write_line(line: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = std::fs::metadata(LOG_PATH).map_or(true, |m| m.len() == 0);
    let Some(mut f) = crate::store::tee::append(LOG_PATH, "snipe_test", HEADER) else { return };
    let r = if fresh { writeln!(f, "{HEADER}\n{line}") } else { writeln!(f, "{line}") };
    if let Err(e) = r {
        warn!("snipe test: {LOG_PATH} not written: {e}");
    }
}

/// At start: today's entries and PnL from the log (this mode's lines), so a restart keeps the caps.
pub fn load_caps(mode: &str, now: u64) -> (u32, u32, i64) {
    let text = std::fs::read_to_string(LOG_PATH).unwrap_or_default();
    let mut caps = Caps::default();
    caps.roll(now);
    caps_from_lines(&mut caps, &text, mode, now);
    let view = caps.view(now);
    *CAPS.lock().unwrap_or_else(|e| e.into_inner()) = caps;
    view
}

/// Counts the log's lines of `mode` into `caps`: entries by their decision time (today and this
/// hour), PnL of today's lines.
fn caps_from_lines(caps: &mut Caps, text: &str, mode: &str, now: u64) {
    let header: Vec<&str> = HEADER.split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).expect("column");
    let (c_mode, c_ms, c_pnl) = (col("mode"), col("decided_unix_ms"), col("pnl_sol"));
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() != header.len() || f[c_mode] != mode {
            continue;
        }
        let Ok(ms) = f[c_ms].parse::<u64>() else { continue };
        let t = ms / 1000;
        if t / 86_400 == now / 86_400 {
            caps.entries += 1;
            if t / 3_600 == now / 3_600 {
                caps.hour_entries += 1;
            }
            caps.pnl_lamports += f[c_pnl].parse::<f64>().map(|v| (v * 1e9).round() as i64).unwrap_or(0);
        }
    }
}

/// The scorecard's facts as "name=value:points;..." for the line.
pub fn facts_text(f: &crate::config::rules::LaunchFacts) -> String {
    use crate::config::scorecard::{points, FACTS};
    let v = crate::config::rules::score_facts(f);
    FACTS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let x = v[i];
            let shown = if x.is_finite() { format!("{x:.4}") } else { "na".into() };
            format!("{name}={shown}:{}", points(i, if x.is_nan() { f64::NEG_INFINITY } else { x }))
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// The bits of an entry the launch's facts give.
pub fn entry_of_facts(e: &mut Entry, f: &crate::config::rules::LaunchFacts) {
    e.follow_kind = match f.follow {
        Some(crate::config::rules::Follow::Recent) => "recent",
        Some(crate::config::rules::Follow::Profitable) => "profitable",
        None => "",
    };
    e.score = Some(crate::config::rules::launch_score(f));
    e.facts = facts_text(f);
    e.wallet_age_h = f.scoring.wallet_first_bt.map(|bt| (f.scoring.now as f64 - bt as f64) / 3600.0);
    e.funded_sol = f.scoring.funded_sol;
    e.mayhem = f.mayhem;
}

/// The rules summary's lines.
pub fn summary() -> Vec<String> {
    let s = settings();
    if !s.on {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "SNIPE TEST (SNIPE_TEST=1), one test of the whole bot: {} SOL every buy (SNIPE_TEST_SOL, by hand, never scaled); caps for all buys together: {} a UTC hour, {} a UTC day, {} SOL realized loss a UTC day stops all buying",
        lamports_to_sol(s.size_lamports),
        s.max_per_hour,
        s.max_per_day,
        lamports_to_sol(s.max_loss_lamports)
    )];
    lines.push(if s.rules_on {
        format!("snipe test, launches (SNIPE_TEST_RULES=on): on top of the rules above, dev buy under 1 SOL, at most {BUNDLE_MAX_WALLETS} buyer besides the dev in the bundle, under 1 SOL real after it, curve not complete; exit all out at +30% or {} s after the fill", MAX_HOLD.as_secs())
    } else {
        "snipe test, launches: bigboss's own rules and exit, as above (SNIPE_TEST_RULES=on adds the four test rules and the +30% / 15 s exit)".to_string()
    });
    lines.push(if s.pool_on {
        "snipe test, graduations (SNIPE_TEST_POOL=on): a curve from under 60 SOL real to complete within 2 s; canonical pool, SOL quote, not mayhem; at the migration slot's end the pool under 1.5x its opening price; buy at once (aiming at the migration slot + 2), 10% price guard; all out 240 s after the fill; paper fills at the landing slot's worst pool state".to_string()
    } else {
        "snipe test, graduations: off (SNIPE_TEST_POOL=on to buy them)".to_string()
    });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The caps' logic at 2 an hour and 50 a day (the defaults are 4 and 100).
    fn s() -> Settings {
        Settings { on: true, max_per_hour: 2, max_per_day: 50, ..Default::default() }
    }

    #[test]
    fn settings_come_from_the_environment_and_bad_values_stop_the_start() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(Settings::from_lookup(env(&[])).unwrap(), Settings::default());
        assert!(!Settings::from_lookup(env(&[])).unwrap().on, "off by default");
        let x = Settings::from_lookup(env(&[("SNIPE_TEST", "1"), ("SNIPE_TEST_SOL", "0.05"), ("SNIPE_TEST_MAX_PER_HOUR", "3")])).unwrap();
        assert_eq!((x.on, x.size_lamports, x.max_per_hour, x.max_per_day), (true, 50_000_000, 3, 100));
        assert!(!x.rules_on && !x.pool_on, "the launch rules and the graduations are switched on apart");
        let both = Settings::from_lookup(env(&[("SNIPE_TEST", "1"), ("SNIPE_TEST_RULES", "on"), ("SNIPE_TEST_POOL", "on")])).unwrap();
        assert!(both.rules_on && both.pool_on);
        let d = Settings::default();
        assert_eq!((d.max_per_hour, d.max_per_day, d.max_loss_lamports, d.size_lamports), (4, 100, 500_000_000, 100_000_000));
        for bad in ["0", "-1", "abc", "1e3", "0.0000000001"] {
            assert!(Settings::from_lookup(|k: &str| (k == "SNIPE_TEST_SOL").then(|| bad.to_string())).is_err(), "{bad}");
        }
        assert!(Settings::from_lookup(|k: &str| (k == "SNIPE_TEST_SOL").then(|| "101".to_string())).is_err(), "over the safety limit");
        assert!(Settings::from_lookup(|k: &str| (k == "SNIPE_TEST_MAX_PER_DAY").then(|| "0".to_string())).is_err());
    }

    #[test]
    fn the_filter_takes_only_small_quiet_launches() {
        let ok = Visible { dev_buy_lamports: 500_000_000, bundle_wallets: Some(1), real_sol_lamports: 900_000_000, complete: false };
        assert_eq!(filter(&ok), Ok(()));
        assert_eq!(filter(&Visible { dev_buy_lamports: 999_999_999, bundle_wallets: Some(0), ..ok }), Ok(()));
        assert_eq!(filter(&Visible { dev_buy_lamports: 1_000_000_000, ..ok }), Err(TestReject::DevBuy));
        assert_eq!(filter(&Visible { bundle_wallets: Some(2), ..ok }), Err(TestReject::Bundle));
        assert_eq!(filter(&Visible { bundle_wallets: None, ..ok }), Err(TestReject::BundleUnknown));
        assert_eq!(filter(&Visible { real_sol_lamports: 1_000_000_000, ..ok }), Err(TestReject::RealSol));
        assert_eq!(filter(&Visible { complete: true, ..ok }), Err(TestReject::Complete));
        // The first rule broken is the one named.
        assert_eq!(filter(&Visible { dev_buy_lamports: 2_000_000_000, bundle_wallets: Some(5), real_sol_lamports: 9_000_000_000, complete: true }), Err(TestReject::DevBuy));
        for r in [TestReject::DevBuy, TestReject::Bundle, TestReject::BundleUnknown, TestReject::RealSol, TestReject::Complete, TestReject::DayCap, TestReject::HourCap, TestReject::LossCap, TestReject::Wallet] {
            assert!(r.text().starts_with("snipe test: "));
            assert!(r.code() >= 40);
        }
    }

    #[test]
    fn the_exit_is_thirty_percent_or_fifteen_seconds() {
        assert_eq!(exit(Duration::from_secs(1), 1.0), None);
        assert_eq!(exit(Duration::from_millis(14_999), 1.299), None);
        assert_eq!(exit(Duration::from_secs(2), 1.30), Some("TP"));
        assert_eq!(exit(Duration::from_secs(15), 0.2), Some("TIME"), "no stop-loss: a loss is held to 15 s");
        assert_eq!(exit(Duration::from_secs(16), 1.5), Some("TP"));
        assert_eq!(exit(Duration::from_secs(10), 0.01), None, "no stop, no trail");
    }

    #[test]
    fn the_caps_stop_entries_by_hour_day_and_loss() {
        let s = s();
        let day = 20_000 * 86_400;
        let mut c = Caps::default();
        assert_eq!(c.enter(&s, day + 10), Ok(()));
        assert_eq!(c.enter(&s, day + 20), Ok(()));
        assert_eq!(c.enter(&s, day + 30), Err(TestReject::HourCap), "2 an hour");
        assert_eq!(c.enter(&s, day + 3_600), Ok(()), "a new hour");
        // 50 a day.
        let mut c = Caps::default();
        for h in 0..24u64 {
            let _ = c.enter(&s, day + h * 3_600);
            let _ = c.enter(&s, day + h * 3_600 + 1);
        }
        assert_eq!(c.view(day + 23 * 3_600).0, 48);
        let s3 = Settings { max_per_hour: 10, ..s.clone() };
        assert_eq!(c.enter(&s3, day + 23 * 3_600 + 2), Ok(()));
        assert_eq!(c.enter(&s3, day + 23 * 3_600 + 3), Ok(()));
        assert_eq!(c.enter(&s3, day + 23 * 3_600 + 4), Err(TestReject::DayCap), "50 a day");
        assert_eq!(c.enter(&s3, day + 86_400), Ok(()), "a new day");
        // The loss: 0.5 SOL realized in the day stops entries; a gain doesn't count against it.
        let mut c = Caps::default();
        c.roll(day);
        c.result(-300_000_000, day + 5);
        assert_eq!(c.check(&s, day + 6), Ok(()));
        c.result(-200_000_000, day + 7);
        assert_eq!(c.check(&s, day + 8), Err(TestReject::LossCap));
        assert_eq!(c.check(&s, day + 86_400), Ok(()), "the next day starts over");
        let mut c = Caps::default();
        c.roll(day);
        c.result(-600_000_000, day + 1);
        c.result(200_000_000, day + 2);
        assert_eq!(c.check(&s, day + 3), Ok(()), "net -0.4 SOL");
    }

    #[test]
    fn the_caps_survive_a_restart_from_the_log() {
        let now = 20_000 * 86_400 + 5 * 3_600 + 100;
        let line = |mode: &str, ms: u64, pnl: &str| {
            let mut f = vec![String::new(); HEADER.split(',').count()];
            let h: Vec<&str> = HEADER.split(',').collect();
            f[h.iter().position(|x| *x == "mode").unwrap()] = mode.into();
            f[h.iter().position(|x| *x == "decided_unix_ms").unwrap()] = ms.to_string();
            f[h.iter().position(|x| *x == "pnl_sol").unwrap()] = pnl.into();
            f.join(",")
        };
        let text = [
            HEADER.to_string(),
            line("live", (now - 86_400) * 1000, "-1.0"),
            line("live", (now - 3_600) * 1000, "-0.1"),
            line("live", (now - 10) * 1000, "-0.25"),
            line("paper", (now - 20) * 1000, "-5"),
            "garbage".into(),
        ]
        .join("\n");
        let mut c = Caps::default();
        c.roll(now);
        caps_from_lines(&mut c, &text, "live", now);
        assert_eq!(c.view(now), (2, 1, -350_000_000));
        assert_eq!(c.check(&s(), now), Ok(()), "one this hour: one more may go");
    }

    fn snipe(mode: &'static str) -> Snipe {
        Snipe {
            entry: Entry {
                mint: "Mint1111".into(),
                mode,
                path: "hot",
                deployer: "Dep".into(),
                follow_kind: "recent",
                score: Some(66),
                facts: "a=1:2;b=na:-5".into(),
                create_slot: 100,
                decided_unix_ms: 1_791_850_000_000,
                visible: Visible { dev_buy_lamports: 500_000_000, bundle_wallets: Some(1), real_sol_lamports: 800_000_000, complete: false },
                bundle_lamports: 300_000_000,
                price_after_bundle: 0.000_03,
                size_lamports: 100_000_000,
                token_2022: true,
                ..Default::default()
            },
            buy_status: "confirmed (slot 100)".into(),
            landed: true,
            tokens: 3_000_000,
            sol_in: 100_000_000,
            // 0.1 SOL in, 1,513,840 rent, 2,000,000 tip + 105,000 priority.
            buy_wallet: Some(-(100_000_000 + 1_513_840 + 2_105_000)),
            landed_slot: Some(101),
            landing: Some(Landing { slot: 101, index: Some(42), rank: Some(3), source: "chain" }),
            priority_lamports: Some(105_000),
            tip_lamports: Some(2_000_000),
            tokens_left: 0,
            sells: 1,
            sol_out: 130_000_000,
            // Its sell's wallet change: 0.13 SOL, the rent back, less 0.000505 SOL fees.
            sell_wallet: 130_000_000 + 1_513_840 - 505_000,
            sell_wallet_known: true,
            reasons: vec!["TP".into()],
            exit_at_ms: Some(1_791_850_004_000),
            buy_at_ms: Some(1_791_850_000_500),
            full: Some(FullSlot { buyers: 2, real_sol_lamports: 1_200_000_000, complete: false }),
            counted: false,
        }
    }

    #[test]
    fn the_line_has_every_field_and_the_pnl_after_all_costs() {
        let rent = 1_513_840;
        let s = snipe("live");
        let pnl = pnl_lamports(&s, rent);
        assert_eq!(pnl, 130_000_000 - 505_000 - 100_000_000 - 2_105_000, "the rent went out and came back");
        let line = line_of(&s, rent);
        let h: Vec<&str> = HEADER.split(',').collect();
        let f: Vec<&str> = line.split(',').collect();
        assert_eq!(f.len(), h.len(), "{line}");
        let get = |name: &str| f[h.iter().position(|x| *x == name).unwrap()];
        assert_eq!(get("mode"), "live");
        assert_eq!(get("create_slot"), "100");
        assert_eq!(get("landed_slot"), "101");
        assert_eq!(get("landed_index"), "42");
        assert_eq!(get("landed_rank"), "3");
        assert_eq!(get("slot_offset"), "1");
        assert_eq!(get("dev_buy_sol"), "0.500000000");
        assert_eq!(get("bundle_wallets"), "1");
        assert_eq!(get("bundle_sol"), "0.300000000");
        assert_eq!(get("real_sol_visible"), "0.800000000");
        assert_eq!(get("full_slot_buyers"), "2");
        assert_eq!(get("filter_full_slot"), "Bundle", "the whole slot would have been refused");
        assert_eq!(get("fill_price"), "33.333333333333");
        assert_eq!(get("price_after_bundle"), "0.000030000000");
        assert_eq!(get("tip_sol"), "0.002000000");
        assert_eq!(get("priority_fee_sol"), "0.000105000");
        assert_eq!(get("buy_fees_sol"), "0.002105000");
        assert_eq!(get("sell_fees_sol"), "0.000505000");
        assert_eq!(get("exit_trigger"), "TP");
        assert_eq!(get("exit_sol"), "0.130000000");
        assert_eq!(get("hold_secs"), "3.5");
        assert_eq!(get("rent_unrecovered_sol"), "0.000000000");
        assert_eq!(get("pnl_sol"), "0.027390000");
        assert_eq!(get("pnl_msol"), "27.390");
        assert_eq!(get("score"), "66");
        assert_eq!(get("facts"), "a=1:2;b=na:-5");
        assert_eq!(get("date"), "2026-10-13");
    }

    #[test]
    fn rent_not_recovered_is_a_cost_and_a_failed_buy_costs_its_fee() {
        let rent = 1_513_840;
        // Stuck: no sell landed, the wallet's change is the buy's, the rent with it.
        let mut stuck = snipe("live");
        stuck.sells = 0;
        stuck.sol_out = 0;
        stuck.sell_wallet = 0;
        stuck.sell_wallet_known = false;
        stuck.tokens_left = stuck.tokens;
        stuck.reasons.clear();
        assert_eq!(pnl_lamports(&stuck, rent), -(100_000_000 + 1_513_840 + 2_105_000));
        let line = line_of(&stuck, rent);
        assert!(line.contains(",0.001513840,"), "{line}");
        // Without a measured wallet change: SOL out - SOL in - the rent left.
        let mut unknown = stuck.clone();
        unknown.buy_wallet = None;
        assert_eq!(pnl_lamports(&unknown, rent), -(100_000_000 + 1_513_840));
        // A failed buy: its fee only.
        let mut failed = snipe("live");
        failed.landed = false;
        failed.tokens = 0;
        failed.tokens_left = 0;
        failed.sells = 0;
        failed.sol_out = 0;
        failed.sell_wallet = 0;
        failed.sell_wallet_known = false;
        failed.reasons.clear();
        failed.buy_status = "failed: slippage".into();
        failed.buy_wallet = Some(-105_000);
        assert_eq!(pnl_lamports(&failed, rent), -105_000);
        // Paper never pays rent.
        let mut paper = stuck.clone();
        paper.entry.mode = "paper";
        assert_eq!(rent_unrecovered(&paper, rent), 0);
    }

    #[test]
    fn a_graduation_line_carries_its_kind_and_prices() {
        let mut g = snipe("paper");
        g.entry.kind = "graduation";
        g.entry.grad = Some(Grad {
            fast_ms: 1_400,
            open_price: 0.0000275,
            slot_end_x: 1.12,
            jump_before_fill: Some(false),
            max_x_before_fill: Some(1.31),
            prices: [Some(0.00003), Some(0.000031), None, Some(0.000029), Some(0.00002)],
        });
        g.full = None;
        g.landing = Some(Landing { slot: 102, index: None, rank: None, source: "paper" });
        let line = line_of(&g, 1_513_840);
        let h: Vec<&str> = HEADER.split(',').collect();
        let f: Vec<&str> = line.split(',').collect();
        assert_eq!(f.len(), h.len());
        let get = |name: &str| f[h.iter().position(|x| *x == name).unwrap()];
        assert_eq!(get("kind"), "graduation");
        assert_eq!(get("grad_fast_ms"), "1400");
        assert_eq!(get("slot_end_x"), "1.1200");
        assert_eq!(get("jump_before_fill"), "0");
        assert_eq!(get("max_x_before_fill"), "1.3100");
        assert_eq!(get("px_30s"), "0.000030000000");
        assert_eq!(get("px_120s"), "");
        assert_eq!(get("px_600s"), "0.000020000000");
        assert_eq!(get("slot_offset"), "2");
        let launch = line_of(&snipe("live"), 1_513_840);
        assert!(launch.split(',').nth(2) == Some("launch"));
    }

}
