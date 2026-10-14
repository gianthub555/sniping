//! The whale rule (WHALE_MODE=1, default off; the owner's spec of 2026-10-13), after the leader
//! wallet pXGQ6J2ziR's 7 days (985 positions, +456 SOL, 78% winners): buy BIG into a tiny, quiet,
//! very young coin (0.5-3x the SOL already in its curve, which lifts the price by itself), and sell
//! everything into the buyers who come behind at +23%, or after 90 s. What this measures: whether
//! follower bots buy behind US as they buy behind him. Paper can't show it (paper buys never reach
//! the chain): paper checks the mechanics, a small live run decides.
//!
//! Entry, on the coin's state from bigboss's pump.fun stream at every new slot (no RPC):
//! 1. age 60 s to 6.5 min;
//! 2. real SOL in the curve 0.3-5.4 SOL;
//! 3. price 1.0-1.4x the launch price;
//! 4. at most 71 distinct buyers so far;
//! 5. quiet: no trade for 5 slots, and net inflow over the last 10 slots <= 0.1 SOL;
//! 6. not mayhem, quoted in SOL, a known token program, the curve not complete;
//! 7. the dev buy at the launch under 1 SOL;
//! 8. not a coin we hold or held; at most WHALE_MAX_OPEN (3) open whale positions.
//! bigboss's follow rule and scorecard are not applied (logged as information).
//!
//! Size: min(WHALE_SIZE_SOL, max(0.5 SOL, 0.75 x the curve's real SOL)), never scaled otherwise.
//! No 1.25% price guard (our own buy moves the price; that is the point): the slippage limit is
//! 10% against our own quote at the decision. Exit (`auto_sell::TestExit::whale`): all out at the
//! first check at +23% over our fill, or 90 s after it; no stop. Sells go the bot's usual way
//! (live: the hot lanes' fan-out with re-bids, never giving up while tokens remain).
//!
//! Caps: WHALE_MAX_PER_HOUR (2) entries a UTC hour, WHALE_MAX_PER_DAY (30) a UTC day, and
//! WHALE_DAY_LOSS_SOL (3) realized loss in a UTC day stops whale buys. The live guard stays.
//! Log: one line per trade (`logs/whale.csv` + the store's `whale` table); report and gate:
//! `tools/whale_report.py`.
//!
//! Off the hot path: the launches' hot path and its allocation-free decision are untouched; the
//! filter (`check`) itself allocates nothing (audit test).

use {
    crate::{
        config::{env_opt, lamports_to_sol, rules, trading_wallet, RPC_CLIENT, TRADE_SETTING},
        instructions::{
            pumpfun_buy_ix::{bonding_curve_address, CurveReserves, PumpFunMarket, TRADE_EVENT_DISCRIMINATOR},
            pumpswap_buy_ix::PUMP_PROGRAM_ID,
            types::Market,
        },
        service::SendProfile,
        utils::{
            auto_sell::{spawn_position, Position},
            control,
            csv_logger::{log_trade, TradeRecord},
            dashboard::{activity, now_unix, now_unix_ms, wallet_balance_lamports, ActivityKind},
            parse::{read_pubkey, read_u64},
            utils::{get_associated_token_address, get_token_balance, ConfirmOutcome, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
        },
    },
    carbon_core::transaction::TransactionMetadata,
    log::{info, warn},
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, HashSet},
        io::Write,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
};

pub const LOG_PATH: &str = "logs/whale.csv";
pub const MIN_AGE_MS: u64 = 60_000;
pub const MAX_AGE_MS: u64 = 390_000;
pub const REAL_SOL_MIN: u64 = 300_000_000;
pub const REAL_SOL_MAX: u64 = 5_400_000_000;
pub const PRICE_X_MIN: f64 = 1.0;
pub const PRICE_X_MAX: f64 = 1.4;
pub const MAX_BUYERS: usize = 71;
pub const QUIET_SLOTS: u64 = 5;
pub const INFLOW_SLOTS: u64 = 10;
pub const INFLOW_MAX: i64 = 100_000_000;
pub const DEV_BUY_MAX: u64 = 1_000_000_000;
pub const SIZE_FLOOR: u64 = 500_000_000;
pub const SIZE_SHARE: f64 = 0.75;
pub const SLIPPAGE: f64 = 0.10;
pub const TAKE_PROFIT_X: f64 = 1.23;
pub const MAX_HOLD: Duration = Duration::from_secs(90);
/// The pool's price is logged this many slots after our fill.
pub const PRICE_SLOTS: [u64; 4] = [5, 20, 60, 214];
/// A paper buy lands this many slots after the decision, at that slot's worst state.
const LAND_SLOTS: u64 = 2;
const LAND_WAIT: Duration = Duration::from_secs(3);
/// The leader's follower bots, by the address prefixes the research named.
pub const FOLLOWERS: [&str; 8] = ["47du2vFg", "6MG3azei", "5QY3MBvL", "Bck5r87K", "GF66HXCC", "FkatEH7Y", "EZbw9MrM", "BUkpShoE"];

// ---- Settings.

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub on: bool,
    pub size_lamports: u64,
    pub max_open: usize,
    pub max_per_hour: u32,
    pub max_per_day: u32,
    pub day_loss_lamports: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { on: false, size_lamports: 1_500_000_000, max_open: 3, max_per_hour: 2, max_per_day: 30, day_loss_lamports: 3_000_000_000 }
    }
}

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
    /// From the environment; an unreadable value stops the start (a size or a cap is never guessed).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Self::default();
        let given = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let sol = |k: &str, def: u64| given(k).map_or(Ok(def), |v| sol_lamports(&v).ok_or_else(|| format!("{k}={v:?} is not a SOL amount above 0")));
        let count = |k: &str, def: u64| given(k).map_or(Ok(def), |v| v.trim().parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| format!("{k}={v:?} is not a whole number above 0")));
        let s = Self {
            on: given("WHALE_MODE").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")),
            size_lamports: sol("WHALE_SIZE_SOL", d.size_lamports)?,
            max_open: count("WHALE_MAX_OPEN", d.max_open as u64)? as usize,
            max_per_hour: count("WHALE_MAX_PER_HOUR", d.max_per_hour as u64)? as u32,
            max_per_day: count("WHALE_MAX_PER_DAY", d.max_per_day as u64)? as u32,
            day_loss_lamports: sol("WHALE_DAY_LOSS_SOL", d.day_loss_lamports)?,
        };
        if s.size_lamports as f64 / 1e9 > crate::config::MAX_RUNTIME_BUY_SOL {
            return Err(format!("WHALE_SIZE_SOL is above the {} SOL safety limit", crate::config::MAX_RUNTIME_BUY_SOL));
        }
        Ok(s)
    }
}

static SETTINGS: LazyLock<Result<Settings, String>> = LazyLock::new(|| Settings::from_lookup(env_opt));

pub fn settings() -> &'static Settings {
    static OFF: Settings = Settings { on: false, size_lamports: 0, max_open: 0, max_per_hour: 0, max_per_day: 0, day_loss_lamports: 0 };
    SETTINGS.as_ref().unwrap_or(&OFF)
}

pub fn check_settings() -> Result<(), String> {
    SETTINGS.as_ref().map(|_| ()).map_err(Clone::clone)
}

pub fn on() -> bool {
    settings().on
}

/// What we spend on a coin with `real_sol` lamports in its curve: min(size, max(0.5 SOL, 0.75 x real SOL)).
pub fn spend(size: u64, real_sol: u64) -> u64 {
    size.min(SIZE_FLOOR.max((real_sol as f64 * SIZE_SHARE) as u64))
}

// ---- The coins (bigboss's pump.fun stream).

/// A young coin's state.
#[derive(Debug, Clone)]
pub struct Coin {
    pub mint: Pubkey,
    pub deployer: Pubkey,
    pub creator: Pubkey,
    pub token_program: Pubkey,
    pub mayhem: bool,
    pub sol_quote: bool,
    pub created_ms: u64,
    pub launch_price: f64,
    pub dev_buy: u64,
    pub reserves: CurveReserves,
    pub buyers: Vec<Pubkey>,
    /// More than MAX_BUYERS distinct buyers (the list stops growing then).
    pub many_buyers: bool,
    pub last_trade_slot: u64,
    /// Net SOL in (buys less sells) per slot, the last 16 slots.
    pub flows: [(u64, i64); 16],
    /// The furthest rule it passed (the funnel).
    pub furthest: u8,
}

impl Coin {
    pub fn price(&self) -> f64 {
        let r = &self.reserves;
        if r.virtual_token_reserves == 0 { 0.0 } else { r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64 }
    }

    fn note_flow(&mut self, slot: u64, net: i64) {
        let i = (slot % 16) as usize;
        if self.flows[i].0 != slot {
            self.flows[i] = (slot, 0);
        }
        self.flows[i].1 += net;
    }

    /// Net SOL in over the slots (now - n, now].
    pub fn inflow(&self, now: u64, n: u64) -> i64 {
        self.flows.iter().filter(|(s, _)| *s + n > now && *s <= now).map(|(_, f)| f).sum()
    }
}

/// The rules, in order (the funnel counts the furthest each coin passed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Rule {
    Age = 1,
    RealSol = 2,
    Price = 3,
    Buyers = 4,
    Quiet = 5,
    Safety = 6,
    DevBuy = 7,
    Held = 8,
}

pub const RULES: [(Rule, &str); 8] = [
    (Rule::Age, "age 60 s - 6.5 min"),
    (Rule::RealSol, "real SOL 0.3-5.4"),
    (Rule::Price, "price 1.0-1.4x launch"),
    (Rule::Buyers, "<= 71 buyers"),
    (Rule::Quiet, "quiet (5 slots, <= 0.1 SOL in 10 slots)"),
    (Rule::Safety, "not mayhem, SOL quote, token program, not complete"),
    (Rule::DevBuy, "dev buy < 1 SOL"),
    (Rule::Held, "not held, open positions < max"),
];

/// The entry rules on a coin at `now_ms` in `slot`; `held`: we hold or held it, or the open limit
/// is reached. Ok, or the first rule it fails. Allocation-free.
pub fn check(c: &Coin, now_ms: u64, slot: u64, held: bool) -> Result<(), Rule> {
    let age = now_ms.saturating_sub(c.created_ms);
    if !(MIN_AGE_MS..=MAX_AGE_MS).contains(&age) {
        return Err(Rule::Age);
    }
    let real = c.reserves.real_sol_reserves;
    if !(REAL_SOL_MIN..=REAL_SOL_MAX).contains(&real) {
        return Err(Rule::RealSol);
    }
    let x = if c.launch_price > 0.0 { c.price() / c.launch_price } else { 0.0 };
    if !(PRICE_X_MIN..=PRICE_X_MAX).contains(&x) {
        return Err(Rule::Price);
    }
    if c.many_buyers || c.buyers.len() > MAX_BUYERS {
        return Err(Rule::Buyers);
    }
    if slot.saturating_sub(c.last_trade_slot) < QUIET_SLOTS || c.inflow(slot, INFLOW_SLOTS) > INFLOW_MAX {
        return Err(Rule::Quiet);
    }
    let known_program = c.token_program == TOKEN_PROGRAM_ID || c.token_program == TOKEN_2022_PROGRAM_ID;
    if c.mayhem || !c.sol_quote || !known_program || c.reserves.real_token_reserves == 0 {
        return Err(Rule::Safety);
    }
    if c.dev_buy >= DEV_BUY_MAX {
        return Err(Rule::DevBuy);
    }
    if held {
        return Err(Rule::Held);
    }
    Ok(())
}

/// A trade on a coin we bought, kept for the line (behind flow, followers, prices, paper fills).
#[derive(Debug, Clone, Copy)]
pub struct Trade {
    pub slot: u64,
    pub idx: u64,
    pub ms: u64,
    pub is_buy: bool,
    pub sol: u64,
    pub user: Pubkey,
    pub reserves: CurveReserves,
}

static COINS: LazyLock<Mutex<HashMap<Pubkey, Coin>>> = LazyLock::new(Default::default);
static WATCHED: LazyLock<Mutex<HashMap<Pubkey, Vec<Trade>>>> = LazyLock::new(Default::default);
static HELD: LazyLock<Mutex<HashSet<Pubkey>>> = LazyLock::new(Default::default);
static OPEN: AtomicUsize = AtomicUsize::new(0);
static STREAM_SLOT: AtomicU64 = AtomicU64::new(0);
/// Coins that left the window, by how many rules they passed (0: none, 8: all).
pub static FUNNEL: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// Is `mint` a whale position (its exit is the whale's)?
pub fn owns(mint: &Pubkey) -> bool {
    on() && WATCHED.lock().unwrap_or_else(|e| e.into_inner()).contains_key(mint)
}

/// Every pump.fun transaction: new coins, and every trade on a coin in the window or bought.
pub fn observe(tx: &TransactionMetadata) {
    if !on() {
        return;
    }
    STREAM_SLOT.fetch_max(tx.slot, Ordering::Relaxed);
    let now = now_unix_ms();
    let created = super::pumpfun::created_coins(tx);
    let trades = crate::processors::common::cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    let mut coins = COINS.lock().unwrap_or_else(|e| e.into_inner());
    for c in &created {
        if coins.contains_key(&c.mint) {
            continue;
        }
        let r = c.reserves;
        coins.insert(
            c.mint,
            Coin {
                mint: c.mint,
                deployer: c.user,
                creator: c.creator,
                token_program: c.token_program,
                mayhem: c.mayhem,
                sol_quote: c.sol_quote,
                created_ms: now,
                launch_price: if r.virtual_token_reserves > 0 { r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64 } else { 0.0 },
                dev_buy: 0,
                reserves: r,
                buyers: Vec::new(),
                many_buyers: false,
                last_trade_slot: tx.slot,
                flows: [(0, 0); 16],
                furthest: 0,
            },
        );
    }
    if trades.is_empty() {
        return;
    }
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    for body in trades {
        let (Ok(mint), Ok(sol), Ok(user), Ok(vsol), Ok(vtok), Ok(real_sol), Ok(real_tok)) =
            (read_pubkey(&body, 0), read_u64(&body, 32), read_pubkey(&body, 49), read_u64(&body, 89), read_u64(&body, 97), read_u64(&body, 105), read_u64(&body, 113))
        else {
            continue;
        };
        let is_buy = body.get(48) == Some(&1);
        let reserves = CurveReserves { virtual_token_reserves: vtok, virtual_sol_reserves: vsol, real_token_reserves: real_tok, real_sol_reserves: real_sol };
        if let Some(c) = coins.get_mut(&mint) {
            // The dev buy: the create transaction's buy by the deployer or the creator.
            if is_buy && (user == c.deployer || user == c.creator) && created.iter().any(|k| k.mint == mint) {
                c.dev_buy += sol;
            }
            if is_buy && user != c.deployer && user != c.creator && !c.buyers.contains(&user) {
                if c.buyers.len() <= MAX_BUYERS {
                    c.buyers.push(user);
                } else {
                    c.many_buyers = true;
                }
            }
            c.reserves = reserves;
            c.last_trade_slot = c.last_trade_slot.max(tx.slot);
            c.note_flow(tx.slot, if is_buy { sol as i64 } else { -(sol as i64) });
        }
        if let Some(list) = watched.get_mut(&mint) {
            if list.len() < 20_000 {
                list.push(Trade { slot: tx.slot, idx: tx.index.unwrap_or(0), ms: now, is_buy, sol, user, reserves });
            }
        }
    }
}

/// Starts the rule's loop (on the runtime) when WHALE_MODE=1: at every new slot each coin in the
/// window is checked; coins past it leave (their furthest rule counted).
pub fn spawn() {
    if !on() {
        return;
    }
    tokio::spawn(async {
        let mut last = 0u64;
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut report_at = Instant::now() + Duration::from_secs(600);
        loop {
            tick.tick().await;
            let slot = crate::utils::latency::latest_slot().unwrap_or(0).max(STREAM_SLOT.load(Ordering::Relaxed));
            if slot <= last {
                continue;
            }
            last = slot;
            evaluate(slot);
            if Instant::now() >= report_at {
                report_at = Instant::now() + Duration::from_secs(600);
                info!("whale: {}", funnel_text());
                publish_funnel();
            }
        }
    });
}

fn evaluate(slot: u64) {
    let now = now_unix_ms();
    let s = settings();
    let full = OPEN.load(Ordering::SeqCst) >= s.max_open;
    let mut picks: Vec<Coin> = Vec::new();
    {
        let held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let mut coins = COINS.lock().unwrap_or_else(|e| e.into_inner());
        coins.retain(|mint, c| {
            if now.saturating_sub(c.created_ms) > MAX_AGE_MS {
                FUNNEL[c.furthest as usize].fetch_add(1, Ordering::Relaxed);
                return false;
            }
            match check(c, now, slot, full || held.contains(mint)) {
                Ok(()) => {
                    c.furthest = 8;
                    picks.push(c.clone());
                }
                Err(r) => c.furthest = c.furthest.max(r as u8 - 1),
            }
            true
        });
    }
    for c in picks {
        // At most one entry per slot; the rest are checked again next slot.
        if try_enter(&c, slot) {
            break;
        }
    }
}

fn funnel_text() -> String {
    let n: Vec<u64> = FUNNEL.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    let total: u64 = n.iter().sum();
    let mut out = format!("{total} coins left the window");
    for (i, (_, name)) in RULES.iter().enumerate() {
        let passed: u64 = n[i + 1..].iter().sum();
        out += &format!("; {name}: {passed}");
    }
    out
}

fn publish_funnel() {
    if !crate::store::db_on() {
        return;
    }
    let n: Vec<u64> = FUNNEL.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    let (today, hour, pnl) = CAPS.lock().unwrap_or_else(|e| e.into_inner()).view(now_unix());
    crate::store::write(crate::store::Row::Exec {
        sql: "INSERT OR REPLACE INTO kv (key, value, updated_ns) VALUES ('whale_funnel', ?1, ?2)",
        params: vec![
            format!("{{\"furthest\":{n:?},\"entries_today\":{today},\"entries_hour\":{hour},\"pnl_today_lamports\":{pnl}}}").into(),
            crate::store::now_ns().into(),
        ],
    });
}

// ---- Caps.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    day: u64,
    entries: u32,
    hour: u64,
    hour_entries: u32,
    pnl: i64,
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

    /// Takes an entry, or says which cap stops it.
    pub fn enter(&mut self, s: &Settings, now: u64) -> Result<(), &'static str> {
        self.roll(now);
        if self.pnl <= -(s.day_loss_lamports as i64) {
            return Err("the day's loss cap is reached");
        }
        if self.entries >= s.max_per_day {
            return Err("the day's entry cap is reached");
        }
        if self.hour_entries >= s.max_per_hour {
            return Err("the hour's entry cap is reached");
        }
        self.entries += 1;
        self.hour_entries += 1;
        Ok(())
    }

    pub fn result(&mut self, pnl: i64, at: u64) {
        if at / 86_400 > self.day {
            self.roll(at);
        }
        if at / 86_400 == self.day {
            self.pnl += pnl;
        }
    }

    pub fn view(&mut self, now: u64) -> (u32, u32, i64) {
        self.roll(now);
        (self.entries, self.hour_entries, self.pnl)
    }
}

static CAPS: Mutex<Caps> = Mutex::new(Caps { day: 0, entries: 0, hour: 0, hour_entries: 0, pnl: 0 });
static CAP_SKIPS: AtomicU64 = AtomicU64::new(0);

pub fn caps_view() -> (u32, u32, i64) {
    CAPS.lock().unwrap_or_else(|e| e.into_inner()).view(now_unix())
}

// ---- Entry.

struct OpenGuard;
impl Drop for OpenGuard {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The checks that need the bot (trading on, the caps, the wallet), then the buy. True when it
/// went for the buy.
fn try_enter(c: &Coin, slot: u64) -> bool {
    if !control::trading_enabled() {
        return false;
    }
    let s = settings();
    let size = spend(s.size_lamports, c.reserves.real_sol_reserves);
    let reserve = rules::lamports(rules::BALANCE_RESERVE_SOL);
    if wallet_balance_lamports().is_some_and(|b| b.saturating_sub(reserve) < size) {
        return false;
    }
    if let Err(why) = CAPS.lock().unwrap_or_else(|e| e.into_inner()).enter(s, now_unix()) {
        if CAP_SKIPS.fetch_add(1, Ordering::Relaxed) % 50 == 0 {
            info!("whale: {} passed but not bought: {why}", c.mint);
        }
        return false;
    }
    if !HELD.lock().unwrap_or_else(|e| e.into_inner()).insert(c.mint) {
        return false;
    }
    OPEN.fetch_add(1, Ordering::SeqCst);
    WATCHED.lock().unwrap_or_else(|e| e.into_inner()).insert(c.mint, Vec::new());
    let c = c.clone();
    tokio::spawn(async move {
        let _open = OpenGuard;
        buy(c, slot, size).await;
    });
    true
}

fn market_of(c: &Coin, reserves: CurveReserves) -> PumpFunMarket {
    PumpFunMarket::with_curve(c.mint, bonding_curve_address(&c.mint), c.token_program, c.creator, c.mayhem, reserves)
}

fn price_of(r: &CurveReserves) -> f64 {
    if r.virtual_token_reserves == 0 { 0.0 } else { r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64 }
}

/// The landing slot's worst state for a buy: the state the slot starts with, and after each of
/// its trades; the highest price.
pub fn worst_buy_state(start: CurveReserves, trades: &[Trade], slot: u64) -> CurveReserves {
    let begin = trades.iter().filter(|t| t.slot < slot).max_by_key(|t| (t.slot, t.idx)).map_or(start, |t| t.reserves);
    let mut worst = begin;
    for t in trades.iter().filter(|t| t.slot == slot) {
        if price_of(&t.reserves) > price_of(&worst) {
            worst = t.reserves;
        }
    }
    worst
}

fn watched(mint: &Pubkey) -> Vec<Trade> {
    WATCHED.lock().unwrap_or_else(|e| e.into_inner()).get(mint).cloned().unwrap_or_default()
}

async fn buy(c: Coin, slot: u64, size: u64) {
    let paper = TRADE_SETTING.dry_run;
    let decision = c.reserves;
    let market = market_of(&c, decision);
    let expected = match market.quote_buy(size) {
        Ok(t) if t > 0 => t,
        _ => return finish_failed(&c.mint, "no quote"),
    };
    let min_out = (expected as f64 * (1.0 - SLIPPAGE)) as u64;
    let follow = crate::utils::reference::follow(&[c.deployer, c.creator], now_unix()).map_or("", |f| match f {
        rules::Follow::Recent => "recent",
        rules::Follow::Profitable => "profitable",
    });
    let mut rec = Record {
        mode: if paper { "paper" } else { "live" },
        mint: c.mint,
        deployer: c.deployer,
        follow_kind: follow,
        decided_unix_ms: now_unix_ms(),
        decision_slot: slot,
        age_s: now_unix_ms().saturating_sub(c.created_ms) as f64 / 1000.0,
        real_sol_before: decision.real_sol_reserves,
        buyers_before: c.buyers.len() as u32 + c.many_buyers as u32,
        dev_buy: c.dev_buy,
        our_sol: size,
        price_before: price_of(&decision),
        token_2022: c.token_program == TOKEN_2022_PROGRAM_ID,
        ..Default::default()
    };
    activity(
        ActivityKind::Detected,
        Some("PumpFun"),
        Some(&c.mint.to_string()),
        format!(
            "Whale: {:.2} SOL into a {:.0} s old coin with {:.2} SOL real ({:.2}x), {} buyers, quiet",
            lamports_to_sol(size),
            rec.age_s,
            lamports_to_sol(decision.real_sol_reserves),
            size as f64 / decision.real_sol_reserves.max(1) as f64,
            rec.buyers_before
        ),
    );
    crate::store::trades::note_context(
        &c.mint.to_string(),
        crate::store::trades::Context { venue: "pumpfun", deployer: Some(c.deployer.to_string()), follow_kind: None, tier: None, rent_lamports: crate::store::trades::rent(rec.token_2022) },
    );
    let mut r = TradeRecord { action: "BUY", dex: "pumpfun".into(), mint: c.mint.to_string(), sol_in: Some(lamports_to_sol(size)), reason: "WHALE".into(), launch_slot: Some(slot), ..Default::default() };
    if paper {
        let land = slot + LAND_SLOTS;
        let until = tokio::time::Instant::now() + LAND_WAIT;
        while STREAM_SLOT.load(Ordering::Relaxed) <= land && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let at = worst_buy_state(decision, &watched(&c.mint), land);
        let fill_market = market_of(&c, at);
        let tokens = fill_market.quote_buy(size).unwrap_or(0);
        r.landed_slot = Some(land);
        rec.landed_slot = Some(land);
        if tokens < min_out || tokens == 0 {
            r.status = format!("lost race: price moved past the 10% limit before the buy landed ({} < {min_out} tokens)", tokens);
            r.token_amount = Some(0);
            if TRADE_SETTING.dry_run_charge_fees {
                r.wallet_sol_change = Some(-(crate::service::modeled_failed_cost_lamports(SendProfile::Buy) as f64) / 1e9);
            }
            rec.buy_status = r.status.clone();
            rec.buy_wallet = r.wallet_sol_change.map(|w| (w * 1e9).round() as i64);
            log_trade(&r);
            return finish_with(rec);
        }
        let in_curve = fill_market.after_buy(size).map(|a| a.reserves.virtual_sol_reserves.saturating_sub(at.virtual_sol_reserves)).unwrap_or(0);
        r.status = "simulated".into();
        r.token_amount = Some(tokens);
        if TRADE_SETTING.dry_run_charge_fees {
            r.wallet_sol_change = Some(-((size + crate::service::modeled_cost_lamports(SendProfile::Buy)) as f64) / 1e9);
        }
        rec.tokens = tokens;
        rec.landed = true;
        rec.buy_status = r.status.clone();
        rec.buy_wallet = r.wallet_sol_change.map(|w| (w * 1e9).round() as i64);
        rec.fill_slot = land;
        rec.fill_ms = now_unix_ms();
        log_trade(&r);
        activity(ActivityKind::Bought, Some("PumpFun"), Some(&c.mint.to_string()), format!("Paper whale buy: {tokens} tokens for {} SOL", lamports_to_sol(size)));
        RECORDS.lock().unwrap_or_else(|e| e.into_inner()).insert(c.mint, rec);
        spawn_position(Position {
            market: Market::PumpFun(fill_market),
            entry_lamports: size,
            paper_token_amount: Some(tokens),
            paper_sol_in_curve: in_curve,
            known_tokens: None,
            opened_at: Instant::now(),
            slot: None,
            feed: Some(Arc::new(super::position_feed::subscribe(c.mint))),
            fast_rug: false,
        });
        return;
    }
    // Live: through the bot's services; the sells go the usual way (the hot lanes' fan-out).
    let wallet = trading_wallet();
    let feed = Arc::new(super::position_feed::subscribe(c.mint));
    let ata = get_associated_token_address(&wallet, &c.mint, &c.token_program);
    let ixs = match market.buy_instructions(&wallet, size, min_out) {
        Ok(i) => i,
        Err(e) => return finish_failed(&c.mint, &format!("buy not built: {e:#}")),
    };
    let signature = match crate::service::submit_watching(SendProfile::Buy, ixs, Some(slot), Some(ata)).await {
        Ok(crate::service::Submitted::Sent { signature, .. }) => signature,
        Ok(_) => return finish_failed(&c.mint, "not sent"),
        Err(e) => return finish_failed(&c.mint, &format!("buy not sent: {e:#}")),
    };
    r.signature = signature.to_string();
    let (outcome, fill) = super::common::await_landing(&signature, Duration::from_secs(60)).await.unwrap_or((ConfirmOutcome::TimedOut, None));
    let tokens = match &fill {
        Some(f) => f.tokens,
        None => get_token_balance(&RPC_CLIENT, &ata).await.unwrap_or(0),
    };
    r.wallet_sol_change = match (&fill, &outcome) {
        (Some(f), _) => Some(f.wallet_lamports as f64 / 1e9),
        (None, ConfirmOutcome::TimedOut) => None,
        (None, _) => crate::utils::utils::fee_payer_lamport_change(&RPC_CLIENT, &signature).await.ok().map(|l| l as f64 / 1e9),
    };
    r.token_amount = Some(tokens);
    r.status = match &outcome {
        ConfirmOutcome::Confirmed { slot } => format!("confirmed (slot {slot})"),
        ConfirmOutcome::Failed { error, .. } => format!("failed: {error}"),
        ConfirmOutcome::TimedOut if tokens > 0 => "confirmed late".into(),
        ConfirmOutcome::TimedOut => "not landed".into(),
    };
    r.landed_slot = match &outcome {
        ConfirmOutcome::Confirmed { slot } | ConfirmOutcome::Failed { slot, .. } => Some(*slot),
        ConfirmOutcome::TimedOut => fill.as_ref().map(|f| f.slot),
    };
    rec.landed_slot = r.landed_slot;
    rec.buy_status = r.status.clone();
    rec.buy_wallet = r.wallet_sol_change.map(|w| (w * 1e9).round() as i64);
    crate::utils::live_guard::on_buy_result(tokens > 0, !matches!(outcome, ConfirmOutcome::TimedOut), "whale");
    tokio::spawn(crate::utils::live_guard::check_wallet());
    log_trade(&r);
    if tokens == 0 {
        activity(ActivityKind::BuyFailed, Some("PumpFun"), Some(&c.mint.to_string()), format!("Whale buy {}", r.status));
        return finish_with(rec);
    }
    rec.tokens = tokens;
    rec.landed = true;
    rec.fill_slot = r.landed_slot.unwrap_or(slot);
    rec.fill_ms = now_unix_ms();
    activity(ActivityKind::Bought, Some("PumpFun"), Some(&c.mint.to_string()), format!("Whale buy: {tokens} tokens for {} SOL", lamports_to_sol(size)));
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).insert(c.mint, rec);
    spawn_position(Position {
        market: Market::PumpFun(market),
        entry_lamports: size,
        paper_token_amount: None,
        paper_sol_in_curve: 0,
        known_tokens: Some(tokens),
        opened_at: Instant::now(),
        slot: None,
        feed: Some(feed),
        fast_rug: false,
    });
}

fn finish_failed(mint: &Pubkey, why: &str) {
    warn!("whale {mint}: {why}");
    WATCHED.lock().unwrap_or_else(|e| e.into_inner()).remove(mint);
}

// ---- The record and its line.

#[derive(Debug, Clone, Default)]
struct Record {
    mode: &'static str,
    mint: Pubkey,
    deployer: Pubkey,
    follow_kind: &'static str,
    decided_unix_ms: u64,
    decision_slot: u64,
    age_s: f64,
    real_sol_before: u64,
    buyers_before: u32,
    dev_buy: u64,
    our_sol: u64,
    price_before: f64,
    token_2022: bool,
    landed: bool,
    landed_slot: Option<u64>,
    buy_status: String,
    buy_wallet: Option<i64>,
    tokens: u64,
    fill_slot: u64,
    fill_ms: u64,
    sells: u32,
    tokens_left: Option<u64>,
    sol_out: u64,
    sell_wallet: i64,
    sell_wallet_known: bool,
    reasons: Vec<String>,
    exit_ms: Option<u64>,
}

static RECORDS: LazyLock<Mutex<HashMap<Pubkey, Record>>> = LazyLock::new(Default::default);

/// Sells of whale positions (from `csv_logger::log_trade`).
pub fn on_trade(r: &TradeRecord, mode: &'static str) {
    if !on() || r.action != "SELL" {
        return;
    }
    let Ok(mint) = r.mint.parse::<Pubkey>() else { return };
    let mut map = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rec) = map.get_mut(&mint) else { return };
    if rec.mode != mode {
        return;
    }
    let ok = r.status.starts_with("paper") || r.status.starts_with("confirmed");
    rec.sells += 1;
    rec.reasons.push(r.reason.clone());
    if ok {
        let left = rec.tokens_left.unwrap_or(rec.tokens).saturating_sub(r.token_amount.unwrap_or(0));
        rec.tokens_left = Some(left);
        rec.sol_out += r.sol_out.map_or(0, |s| (s * 1e9).round() as u64);
        rec.exit_ms = Some(now_unix_ms());
    }
    if let Some(w) = r.wallet_sol_change {
        rec.sell_wallet += (w * 1e9).round() as i64;
        rec.sell_wallet_known = true;
    }
}

/// The position's monitor stopped: the line once the price at +214 slots is known.
pub fn position_done(mint: &Pubkey) {
    if !on() {
        return;
    }
    let Some(rec) = RECORDS.lock().unwrap_or_else(|e| e.into_inner()).remove(mint) else { return };
    finish_with(rec);
}

fn finish_with(rec: Record) {
    tokio::spawn(async move {
        // +214 slots (~86 s) and the 90 s window after the fill.
        let ready = rec.fill_ms.max(rec.decided_unix_ms) + 92_000;
        let now = now_unix_ms();
        if rec.landed && ready > now {
            tokio::time::sleep(Duration::from_millis(ready - now)).await;
        }
        let trades = WATCHED.lock().unwrap_or_else(|e| e.into_inner()).remove(&rec.mint).unwrap_or_default();
        let rent = crate::store::trades::rent(rec.token_2022);
        let after = Behind::of(&trades, &rec, &trading_wallet());
        let pnl = pnl_of(&rec, rent);
        CAPS.lock().unwrap_or_else(|e| e.into_inner()).result(pnl, now_unix());
        let line = line_of(&rec, &after, rent);
        info!("whale {}: {line}", rec.mint);
        write_line(&line);
    });
}

/// What happened behind our fill: others' buys within 90 s (SOL, wallets), the known followers
/// among them, the price at +5/+20/+60/+214 slots.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Behind {
    pub sol: u64,
    pub wallets: u32,
    pub followers: Vec<&'static str>,
    pub follower_sol: u64,
    pub prices: [Option<f64>; 4],
}

impl Behind {
    fn of(trades: &[Trade], rec: &Record, ours: &Pubkey) -> Self {
        Self::compute(trades, rec.fill_slot, rec.fill_ms, ours, rec.landed)
    }

    pub fn compute(trades: &[Trade], fill_slot: u64, fill_ms: u64, ours: &Pubkey, landed: bool) -> Self {
        let mut b = Behind::default();
        if !landed {
            return b;
        }
        let mut wallets: Vec<Pubkey> = Vec::new();
        for t in trades.iter().filter(|t| t.slot >= fill_slot && t.ms <= fill_ms + MAX_HOLD.as_millis() as u64 && t.user != *ours && t.is_buy) {
            b.sol += t.sol;
            if !wallets.contains(&t.user) {
                wallets.push(t.user);
            }
            let key = t.user.to_string();
            if let Some(f) = FOLLOWERS.iter().find(|p| key.starts_with(*p)) {
                b.follower_sol += t.sol;
                if !b.followers.contains(f) {
                    b.followers.push(f);
                }
            }
        }
        b.wallets = wallets.len() as u32;
        for (i, n) in PRICE_SLOTS.iter().enumerate() {
            b.prices[i] = trades.iter().filter(|t| t.slot <= fill_slot + n).max_by_key(|t| (t.slot, t.idx)).map(|t| price_of(&t.reserves));
        }
        b
    }
}

fn pnl_of(rec: &Record, rent: u64) -> i64 {
    let left_rent = if rec.mode == "live" && rec.landed && rec.tokens_left.unwrap_or(rec.tokens) > 0 { rent } else { 0 };
    match rec.buy_wallet {
        Some(b) if !rec.landed || rec.sell_wallet_known || rec.sells == 0 => b + rec.sell_wallet,
        _ => rec.sol_out as i64 - rec.our_sol as i64 - left_rent as i64,
    }
}

pub const HEADER: &str = "date,mode,mint,deployer,follow_kind,decided_unix_ms,decision_slot,age_s,real_sol_before,buyers_before,dev_buy_sol,our_sol,ratio,buy_status,landed_slot,tokens,fill_price,price_before,impact_pct,px_5,px_20,px_60,px_214,behind_sol_90s,behind_wallets_90s,followers,follower_sol,exit_trigger,exit_reasons,exit_sol,buy_fees_sol,sell_fees_sol,rent_sol,rent_unrecovered_sol,hold_secs,pnl_sol,pnl_msol";

fn line_of(rec: &Record, b: &Behind, rent: u64) -> String {
    let opt = |v: Option<String>| v.unwrap_or_default();
    let sol9 = |l: i64| format!("{:.9}", l as f64 / 1e9);
    let date = chrono::DateTime::from_timestamp_millis(rec.decided_unix_ms as i64).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    let fill_price = (rec.tokens > 0).then(|| rec.our_sol as f64 / rec.tokens as f64);
    let impact = fill_price.filter(|_| rec.price_before > 0.0).map(|p| (p / rec.price_before - 1.0) * 100.0);
    let live = rec.mode == "live";
    let rent_paid = if live && rec.landed { rent } else { 0 };
    let left = if live && rec.landed && rec.tokens_left.unwrap_or(rec.tokens) > 0 { rent } else { 0 };
    let buy_fees = rec.buy_wallet.map(|w| -w - if rec.landed { rec.our_sol as i64 + rent_paid as i64 } else { 0 });
    let sell_fees = (rec.sells > 0 && rec.sell_wallet_known).then(|| rec.sol_out as i64 + rent_paid as i64 - left as i64 - rec.sell_wallet);
    let pnl = pnl_of(rec, rent);
    let clean = |t: &str| t.replace([',', '\n', '\r'], ";");
    [
        date,
        rec.mode.to_string(),
        rec.mint.to_string(),
        rec.deployer.to_string(),
        rec.follow_kind.to_string(),
        rec.decided_unix_ms.to_string(),
        rec.decision_slot.to_string(),
        format!("{:.1}", rec.age_s),
        format!("{:.9}", lamports_to_sol(rec.real_sol_before)),
        rec.buyers_before.to_string(),
        format!("{:.9}", lamports_to_sol(rec.dev_buy)),
        format!("{:.9}", lamports_to_sol(rec.our_sol)),
        format!("{:.3}", rec.our_sol as f64 / rec.real_sol_before.max(1) as f64),
        clean(&rec.buy_status),
        opt(rec.landed_slot.map(|s| s.to_string())),
        rec.tokens.to_string(),
        opt(fill_price.map(|p| format!("{p:.12}"))),
        format!("{:.12}", rec.price_before),
        opt(impact.map(|v| format!("{v:.2}"))),
        opt(b.prices[0].map(|p| format!("{p:.12}"))),
        opt(b.prices[1].map(|p| format!("{p:.12}"))),
        opt(b.prices[2].map(|p| format!("{p:.12}"))),
        opt(b.prices[3].map(|p| format!("{p:.12}"))),
        format!("{:.9}", lamports_to_sol(b.sol)),
        b.wallets.to_string(),
        b.followers.join("|"),
        format!("{:.9}", lamports_to_sol(b.follower_sol)),
        rec.reasons.first().cloned().unwrap_or_default(),
        rec.reasons.join("|"),
        format!("{:.9}", lamports_to_sol(rec.sol_out)),
        opt(buy_fees.map(sol9)),
        opt(sell_fees.map(sol9)),
        sol9(rent_paid as i64),
        sol9(left as i64),
        opt(rec.exit_ms.filter(|_| rec.fill_ms > 0).map(|x| format!("{:.1}", x.saturating_sub(rec.fill_ms) as f64 / 1000.0))),
        sol9(pnl),
        format!("{:.3}", pnl as f64 / 1e6),
    ]
    .iter()
    .map(|f| clean(f))
    .collect::<Vec<_>>()
    .join(",")
}

fn write_line(line: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = std::fs::metadata(LOG_PATH).map_or(true, |m| m.len() == 0);
    let Some(mut f) = crate::store::tee::append(LOG_PATH, "whale", HEADER) else { return };
    let r = if fresh { writeln!(f, "{HEADER}\n{line}") } else { writeln!(f, "{line}") };
    if let Err(e) = r {
        warn!("whale: {LOG_PATH} not written: {e}");
    }
}

/// At start: every coin we ever bought (never bought again), and today's entries and PnL.
pub fn init(mode: &str) -> (usize, u32, i64) {
    let text = std::fs::read_to_string(LOG_PATH).unwrap_or_default();
    let now = now_unix();
    let (held, caps) = from_log(&text, mode, now);
    let n = held.len();
    *HELD.lock().unwrap_or_else(|e| e.into_inner()) = held;
    let mut c = CAPS.lock().unwrap_or_else(|e| e.into_inner());
    *c = caps;
    let (today, _, pnl) = c.view(now);
    (n, today, pnl)
}

fn from_log(text: &str, mode: &str, now: u64) -> (HashSet<Pubkey>, Caps) {
    let h: Vec<&str> = HEADER.split(',').collect();
    let col = |n: &str| h.iter().position(|x| *x == n).expect("column");
    let (cm, cmint, cms, cpnl) = (col("mode"), col("mint"), col("decided_unix_ms"), col("pnl_sol"));
    let mut held = HashSet::new();
    let mut caps = Caps::default();
    caps.roll(now);
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() != h.len() {
            continue;
        }
        if let Ok(m) = f[cmint].parse::<Pubkey>() {
            held.insert(m);
        }
        if f[cm] != mode {
            continue;
        }
        let Ok(ms) = f[cms].parse::<u64>() else { continue };
        let t = ms / 1000;
        if t / 86_400 == now / 86_400 {
            caps.entries += 1;
            if t / 3_600 == now / 3_600 {
                caps.hour_entries += 1;
            }
            caps.pnl += f[cpnl].parse::<f64>().map(|v| (v * 1e9).round() as i64).unwrap_or(0);
        }
    }
    (held, caps)
}

/// The rules summary's lines.
pub fn summary() -> Vec<String> {
    let s = settings();
    if !s.on {
        return Vec::new();
    }
    vec![
        format!(
            "WHALE (WHALE_MODE=1): coins 60 s - 6.5 min old, 0.3-5.4 SOL real, price 1.0-1.4x launch, <= {MAX_BUYERS} buyers, quiet ({QUIET_SLOTS} slots without a trade, <= 0.1 SOL in over {INFLOW_SLOTS} slots), not mayhem, SOL quote, dev buy < 1 SOL, never one held before; follow rule and scorecard not applied"
        ),
        format!(
            "whale size: min({} SOL, max(0.5 SOL, 0.75 x the curve's real SOL)); 10% slippage against our own quote, no 1.25% guard; exit all out at +23% over the fill or 90 s; at most {} open; caps {} an hour, {} a day, {} SOL realized loss a day",
            lamports_to_sol(s.size_lamports),
            s.max_open,
            s.max_per_hour,
            s.max_per_day,
            lamports_to_sol(s.day_loss_lamports)
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = 1_000_000_000;

    fn coin() -> Coin {
        // Fresh curve: 30 SOL / 1,073,000,000 tokens virtual; 2 SOL real now.
        let vsol = 32 * SOL;
        let vtok = 30 * SOL as u128 * 1_073_000_000_000_000u128 / vsol as u128;
        Coin {
            mint: Pubkey::new_unique(),
            deployer: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            token_program: TOKEN_2022_PROGRAM_ID,
            mayhem: false,
            sol_quote: true,
            created_ms: 1_000_000,
            launch_price: 30.0 * SOL as f64 / 1_073_000_000_000_000.0,
            dev_buy: SOL / 2,
            reserves: CurveReserves { virtual_token_reserves: vtok as u64, virtual_sol_reserves: vsol, real_token_reserves: 500_000_000_000_000, real_sol_reserves: 2 * SOL },
            buyers: vec![Pubkey::new_unique(); 10],
            many_buyers: false,
            last_trade_slot: 100,
            flows: [(0, 0); 16],
            furthest: 0,
        }
    }

    #[test]
    fn the_filter_at_its_edges() {
        let c = coin();
        let at = |age_s: u64| 1_000_000 + age_s * 1000;
        assert_eq!(check(&c, at(120), 110, false), Ok(()));
        assert_eq!(check(&c, at(59), 110, false), Err(Rule::Age));
        assert_eq!(check(&c, at(60), 110, false), Ok(()));
        assert_eq!(check(&c, at(390), 110, false), Ok(()));
        assert_eq!(check(&c, at(391), 110, false), Err(Rule::Age));
        let real = |l: u64| {
            let mut x = coin();
            x.reserves.real_sol_reserves = l;
            x.reserves.virtual_sol_reserves = 30 * SOL + l;
            x.reserves.virtual_token_reserves = (30 * SOL as u128 * 1_073_000_000_000_000u128 / (30 * SOL + l) as u128) as u64;
            x
        };
        assert_eq!(check(&real(300_000_000), at(120), 110, false), Ok(()));
        assert_eq!(check(&real(299_999_999), at(120), 110, false), Err(Rule::RealSol));
        assert_eq!(check(&real(5_400_000_000), at(120), 110, false), Ok(()));
        assert_eq!(check(&real(5_400_000_001), at(120), 110, false), Err(Rule::RealSol));
        let mut pricey = coin();
        pricey.launch_price /= 2.0;
        assert_eq!(check(&pricey, at(120), 110, false), Err(Rule::Price), "2.1x the launch price");
        let mut crowded = coin();
        crowded.buyers = vec![Pubkey::default(); 72];
        assert_eq!(check(&crowded, at(120), 110, false), Err(Rule::Buyers));
        crowded.buyers.truncate(71);
        assert_eq!(check(&crowded, at(120), 110, false), Ok(()));
        // Quiet: 5 slots without a trade; at most 0.1 SOL net in over the last 10 slots.
        assert_eq!(check(&c, at(120), 104, false), Err(Rule::Quiet));
        assert_eq!(check(&c, at(120), 105, false), Ok(()));
        let mut busy = coin();
        busy.note_flow(97, 60_000_000);
        busy.note_flow(99, 50_000_000);
        assert_eq!(check(&busy, at(120), 106, false), Err(Rule::Quiet), "0.11 SOL in over slots 97-106");
        assert_eq!(check(&busy, at(120), 108, false), Ok(()), "slot 97 left the window: 0.05 SOL");
        busy.note_flow(105, -100_000_000);
        assert_eq!(check(&busy, at(120), 106, false), Ok(()), "net: a sell offsets");
        assert_eq!(check(&Coin { mayhem: true, ..coin() }, at(120), 110, false), Err(Rule::Safety));
        assert_eq!(check(&Coin { sol_quote: false, ..coin() }, at(120), 110, false), Err(Rule::Safety));
        assert_eq!(check(&Coin { token_program: Pubkey::new_unique(), ..coin() }, at(120), 110, false), Err(Rule::Safety));
        let mut done = coin();
        done.reserves.real_token_reserves = 0;
        assert_eq!(check(&done, at(120), 110, false), Err(Rule::Safety));
        assert_eq!(check(&Coin { dev_buy: SOL, ..coin() }, at(120), 110, false), Err(Rule::DevBuy));
        assert_eq!(check(&Coin { dev_buy: SOL - 1, ..coin() }, at(120), 110, false), Ok(()));
        assert_eq!(check(&c, at(120), 110, true), Err(Rule::Held));
    }

    #[test]
    fn the_size_rule() {
        let size = 1_500_000_000;
        assert_eq!(spend(size, 4 * SOL), size, "0.75 x 4 = 3 SOL: the size");
        assert_eq!(spend(size, SOL), 750_000_000, "0.75 x 1 SOL");
        assert_eq!(spend(size, 300_000_000), 500_000_000, "the floor");
        assert_eq!(spend(400_000_000, 300_000_000), 400_000_000, "never above the size");
    }

    #[test]
    fn the_caps_stop_whale_buys() {
        let s = Settings { on: true, ..Default::default() };
        let day = 20_000 * 86_400;
        let mut c = Caps::default();
        assert!(c.enter(&s, day + 1).is_ok());
        assert!(c.enter(&s, day + 2).is_ok());
        assert_eq!(c.enter(&s, day + 3), Err("the hour's entry cap is reached"));
        for h in 1..15u64 {
            let _ = c.enter(&s, day + h * 3_600);
            let _ = c.enter(&s, day + h * 3_600 + 1);
        }
        assert_eq!(c.enter(&s, day + 15 * 3_600), Err("the day's entry cap is reached"), "30 a day");
        let mut c = Caps::default();
        c.roll(day);
        c.result(-2_000_000_000, day + 10);
        assert!(c.enter(&s, day + 11).is_ok());
        c.result(-1_000_000_000, day + 12);
        assert_eq!(c.enter(&s, day + 3_700), Err("the day's loss cap is reached"));
        assert!(c.enter(&s, day + 86_400).is_ok(), "a new day");
    }

    #[test]
    fn settings_and_their_errors() {
        let get = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(Settings::from_lookup(get(&[])).unwrap(), Settings::default());
        assert!(!Settings::default().on);
        let s = Settings::from_lookup(get(&[("WHALE_MODE", "1"), ("WHALE_SIZE_SOL", "0.75"), ("WHALE_MAX_OPEN", "2")])).unwrap();
        assert_eq!((s.on, s.size_lamports, s.max_open), (true, 750_000_000, 2));
        assert!(Settings::from_lookup(get(&[("WHALE_SIZE_SOL", "abc")])).is_err());
        assert!(Settings::from_lookup(get(&[("WHALE_SIZE_SOL", "0")])).is_err());
        assert!(Settings::from_lookup(get(&[("WHALE_MAX_PER_HOUR", "0")])).is_err());
    }

    fn trade(slot: u64, idx: u64, ms: u64, user: Pubkey, sol: u64, vsol: u64) -> Trade {
        Trade { slot, idx, ms, is_buy: true, sol, user, reserves: CurveReserves { virtual_token_reserves: 1_000_000, virtual_sol_reserves: vsol, real_token_reserves: 1, real_sol_reserves: vsol } }
    }

    #[test]
    fn the_landing_slots_worst_state_and_what_came_behind() {
        let start = CurveReserves { virtual_token_reserves: 1_000_000, virtual_sol_reserves: 100, real_token_reserves: 1, real_sol_reserves: 100 };
        let (a, us) = (Pubkey::new_unique(), Pubkey::new_unique());
        // A key whose address starts with a follower's prefix (padded with base58 '1's).
        let follower: Pubkey = format!("47du2vFg{}", "1".repeat(36)).parse().unwrap();
        assert!(follower.to_string().starts_with("47du2vFg"));
        let trades = vec![
            trade(10, 1, 0, a, 5, 120),
            trade(12, 3, 1_000, a, 5, 150),
            trade(12, 9, 1_100, a, 5, 130),
            trade(13, 2, 2_000, us, 50, 300),
            trade(14, 1, 3_000, follower, 2 * SOL, 400),
            trade(20, 1, 50_000, a, SOL, 500),
            trade(300, 1, 200_000, a, 9 * SOL, 900),
        ];
        assert_eq!(worst_buy_state(start, &trades, 12).virtual_sol_reserves, 150, "the slot's highest price");
        assert_eq!(worst_buy_state(start, &trades, 11).virtual_sol_reserves, 120, "a quiet slot: its starting state");
        let b = Behind::compute(&trades, 13, 2_000, &us, true);
        assert_eq!(b.sol, 3 * SOL, "the follower's 2 SOL and a's 1 SOL; ours not; a's 9 SOL came after 90 s");
        assert_eq!(b.wallets, 2);
        assert_eq!(b.followers, vec!["47du2vFg"]);
        assert_eq!(b.follower_sol, 2 * SOL);
        assert_eq!(b.prices[0].map(|p| (p * 1e6).round()), Some(400.0), "+5 slots: slot 18's last state is slot 14's");
        assert_eq!(b.prices[3].map(|p| (p * 1e6).round()), Some(500.0), "+214 slots: slot 227; slot 300 is later");
        assert_eq!(Behind::compute(&trades, 13, 2_000, &us, false), Behind::default());
    }

    #[test]
    fn the_line_and_the_log_read_back() {
        let rec = Record {
            mode: "paper",
            mint: Pubkey::new_unique(),
            deployer: Pubkey::new_unique(),
            decided_unix_ms: 1_791_850_000_000,
            decision_slot: 500,
            age_s: 95.0,
            real_sol_before: 2 * SOL,
            buyers_before: 12,
            dev_buy: SOL / 2,
            our_sol: 1_500_000_000,
            price_before: 0.00003,
            landed: true,
            landed_slot: Some(502),
            buy_status: "simulated".into(),
            buy_wallet: Some(-(1_500_000_000 + 2_105_000)),
            tokens: 40_000_000_000,
            fill_slot: 502,
            fill_ms: 1_791_850_001_000,
            sells: 1,
            tokens_left: Some(0),
            sol_out: 1_800_000_000,
            sell_wallet: 1_800_000_000 - 105_000,
            sell_wallet_known: true,
            reasons: vec!["TP".into()],
            exit_ms: Some(1_791_850_031_000),
            ..Default::default()
        };
        let b = Behind { sol: 11 * SOL, wallets: 8, followers: vec!["47du2vFg", "6MG3azei"], follower_sol: 6 * SOL, prices: [Some(0.00004), None, None, Some(0.00002)] };
        let line = line_of(&rec, &b, 1_513_840);
        let h: Vec<&str> = HEADER.split(',').collect();
        let f: Vec<&str> = line.split(',').collect();
        assert_eq!(f.len(), h.len(), "{line}");
        let get = |n: &str| f[h.iter().position(|x| *x == n).unwrap()];
        assert_eq!(get("ratio"), "0.750");
        assert_eq!(get("fill_price"), "0.037500000000");
        assert_eq!(get("followers"), "47du2vFg|6MG3azei");
        assert_eq!(get("behind_sol_90s"), "11.000000000");
        assert_eq!(get("exit_trigger"), "TP");
        assert_eq!(get("hold_secs"), "30.0");
        assert_eq!(get("buy_fees_sol"), "0.002105000");
        assert_eq!(get("sell_fees_sol"), "0.000105000");
        assert_eq!(get("rent_sol"), "0.000000000", "paper pays no rent");
        assert_eq!(get("pnl_sol"), "0.297790000");
        // Read back at a start: held, and today's entries and PnL.
        let text = format!("{HEADER}\n{line}\n");
        let (held, mut caps) = from_log(&text, "paper", 1_791_850_100);
        assert!(held.contains(&rec.mint));
        assert_eq!(caps.view(1_791_850_100), (1, 1, 297_790_000));
    }
}
