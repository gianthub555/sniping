use {
    super::{
        csv_logger::{log_trade, TradeRecord},
        dashboard::{activity, bump, now_unix, remove_position, upsert_position, ActivityKind, PositionView, STATS},
        token_price::birdeye_value_lamports,
        utils::{fee_payer_lamport_change, get_associated_token_address, get_token_balance, ConfirmOutcome},
    },
    crate::{
        config::{
            lamports_to_sol,
            rules::{self, lamports},
            trading_wallet, RPC_CLIENT, TRADE_SETTING,
        },
        instructions::types::Market,
        processors::{
            common::await_landing,
            position_feed::{self, Flow},
        },
        service::{submit_with, SendProfile, Submitted},
        utils::control,
    },
    anyhow::Result,
    colored::Colorize,
    log::{debug, info, warn},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::{Arc, LazyLock, Mutex},
        time::{Duration, Instant},
    },
};

const SELL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(45);
/// Longest wait for the stream to get past a paper sell's landing slot.
const PAPER_SELL_WAIT: Duration = Duration::from_millis(1_500);

/// Where a paper sell lands: a block position drawn from real landings (the draw paper buys use),
/// counted from where the stream is at the decision, as a live sell sent then would land behind
/// everything already executed. Waits for the stream to reach that position and returns it and
/// the curve a sell there meets (after the trades that landed ahead of it).
async fn paper_sell_landing(feed: &position_feed::Feed) -> Option<((u64, u64), position_feed::CurveState)> {
    let (slot, index) = crate::utils::latency::stream_position()?;
    let (offset, position) = crate::processors::common::draw_fill_position();
    let landing = if offset == 0 { (slot, index + position) } else { (slot + offset, position) };
    // Landed once the stream reaches the position (a live sell is seen landing then).
    crate::utils::latency::wait_for_stream(landing.0, || Some(landing.1), PAPER_SELL_WAIT).await;
    let latest = feed.state()?;
    let reserves = feed.state_before(landing.0, landing.1).unwrap_or(latest.reserves);
    Some((landing, position_feed::CurveState { reserves, ..latest }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellReason {
    /// First slice, sold into the buyers behind the fill (or on a pop).
    First,
    /// A later slice on the clock.
    Step,
    /// At the check, no pop: everything.
    NoPop,
    /// At the check, buyers came but no big run: everything.
    Check,
    /// 10% or less left: everything.
    CloseOut,
    /// The price is under the entry when a sell comes due: everything, as the reference wallet does.
    Down,
    /// A buying wave just stopped when a sell came due: the rest, as the reference wallet does.
    Fade,
    /// The price fell the trailing distance under its high since the last sell.
    Trail,
    /// The first sell, taking everything: the curve gained FIRST_ALL_UP_SOL in the wait window.
    FirstAll,
    /// MAX_HOLD_SECS passed.
    Lifetime,
    /// The patient exit's take profit (`rules::PATIENT_EXIT`).
    TakeProfit,
    /// The dev or a bundle wallet sold once the first sell was due (`rules::INSIDER_EXIT`).
    Insider,
    Manual,
    /// pXGQ's exit (`SELL_RULE=pxgq`): the position fell to the stop.
    Stop,
    /// pXGQ's exit: no wave of buyers behind the fill in the first seconds.
    NoWave,
    /// pXGQ's exit: up and near its high with buyers behind: a share, the rest runs.
    Scale,
    /// pXGQ's exit: no scale-out, rolled over from its peak while still in profit.
    Dump,
    /// pXGQ's exit: under cost while others sell hard.
    Pressure,
    /// The snipe test's exit: 15 s after the fill.
    Time,
}

impl SellReason {
    pub fn code(self) -> &'static str {
        match self {
            SellReason::First => "FIRST",
            SellReason::Step => "STEP",
            SellReason::NoPop => "NO_POP",
            SellReason::Check => "CHECK",
            SellReason::CloseOut => "CLOSE",
            SellReason::Down => "DOWN",
            SellReason::Fade => "FADE",
            SellReason::Trail => "TRAIL",
            SellReason::FirstAll => "FIRST_ALL",
            SellReason::Lifetime => "LIFETIME",
            SellReason::TakeProfit => "TP",
            SellReason::Insider => "INSIDER",
            SellReason::Manual => "MANUAL",
            SellReason::Stop => "STOP",
            SellReason::NoWave => "NO_WAVE",
            SellReason::Scale => "SCALE",
            SellReason::Dump => "DUMP",
            SellReason::Pressure => "PRESSURE",
            SellReason::Time => "TIME",
        }
    }
}

/// What the exit wants done after one check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Hold,
    /// Sell this share of the current balance.
    SellShare(f64, SellReason),
    SellAll(SellReason),
}

/// The market as the exit sees it at one check. Prices are the executable value of a fixed
/// reference amount of tokens, so they stay comparable as the balance shrinks.
#[derive(Debug, Clone, Copy)]
pub struct Tick {
    /// Time since the buy.
    pub held_for: Duration,
    pub entry_price: u64,
    pub price: u64,
    /// SOL other wallets bought behind our fill.
    pub behind_lamports: u64,
    /// SOL other wallets sold behind our fill.
    pub behind_sell_lamports: u64,
    /// Other wallets' trades in the last second.
    pub last_second: Flow,
    /// SOL other wallets bought in the last two seconds.
    pub recent_buy_lamports: u64,
    /// SOL other wallets sold in the last three seconds.
    pub recent_sell_lamports_3s: u64,
    /// Time since another wallet last bought (None: no buy seen).
    pub since_last_buy: Option<Duration>,
    /// Share of the bought tokens still held.
    pub left: f64,
    /// SOL other wallets put into the curve behind our fill (buys less sells) in the first-sell
    /// wait window: the buy's slot and the next `first_wait_slots`.
    pub window_net_lamports: i64,
    /// The stream is still inside that window.
    pub in_wait_window: bool,
    /// The dev or a wallet of the create's bundle sold behind our fill.
    pub insider_sold: bool,
}

/// The reference wallet's exit (see `config::rules`). Free of I/O so every branch is tested.
#[derive(Debug, Clone, Default)]
pub struct RefExit {
    pub sells: u32,
    tuning: Tuning,
    /// Time since the buy at which the last sell landed; the clock restarts there.
    last_sell_at: Duration,
    last_sell_price: u64,
    waits: u32,
    /// Highest price seen since the buy.
    peak_price: u64,
    /// Highest price seen since the last sell.
    peak_since_sell: u64,
    big_bet: bool,
}

/// The exit's price-driven settings; the bot runs `Tuning::default()`, the constants in
/// `config::rules`. Replays try other values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// When the first-sell trigger fires this far (%) under the entry, sell everything.
    pub first_down_percent: Option<f64>,
    /// When a later sell comes due this far (%) under the entry, sell everything.
    pub step_down_percent: Option<f64>,
    /// `step_down_percent` only while nobody is buying (`down_quiet_buy_sol`); false: always.
    pub step_down_quiet: bool,
    /// When a later sell comes due this far (%) under the previous sell, sell everything.
    pub step_fall_percent: Option<f64>,
    /// The sell-everything-when-down switches above fire only while others bought less than this
    /// much SOL in the last two seconds (no buyers to bring the price back); None: always.
    pub down_quiet_buy_sol: Option<f64>,
    /// Sell everything once this share of the position or less is left.
    pub close_out_share: f64,
    /// Wait when the price fell this far (%) under the last sell (None: no wait on drops).
    pub wait_drop_percent: Option<f64>,
    /// Wait while the price is within this much (%) of its high since the buy.
    pub wait_high_percent: Option<f64>,
    /// Wait on this many buys by others in the last second.
    pub wait_buys: Option<u32>,
    /// Wait on this much SOL bought by others in the last second.
    pub wait_buy_sol: Option<f64>,
    /// Wait on this much SOL sold by others in the last second.
    pub wait_sell_sol: Option<f64>,
    /// Waits allowed per step.
    pub max_waits: u32,
    /// Sell the rest when others bought this much SOL in the last two seconds but nobody bought
    /// for `fade_quiet_secs` (a buying wave that just stopped).
    pub fade_buy_sol: Option<f64>,
    pub fade_quiet_secs: f64,
    /// Trailing stop after the first sell: the price this far (%) under its high since the last
    /// sell. Checked on every tick, not only when a sell comes due.
    pub trail_percent: Option<f64>,
    /// The trailing stop only while a wait is running (the bot is holding past a due sell).
    pub trail_only_waiting: bool,
    /// The trailing stop sells everything (false: one step).
    pub trail_sells_all: bool,
    /// Share of the position the first sell takes.
    pub first_share: f64,
    /// The first sell (whole or a slice, whatever triggers it) only while the price is more than
    /// this (%) over the entry; until then hold. None: at any price.
    pub first_up_percent: Option<f64>,
    /// The first sell waits `first_wait_slots` slots after the buy's slot, and takes everything
    /// when the curve gains this much SOL in them. None: no wait.
    pub first_all_up_sol: Option<f64>,
    pub first_wait_slots: u64,
    /// Hold the first slice in the window (true), or slice as usual and sell the rest once the
    /// window reaches `first_all_up_sol` (false, replay variant).
    pub first_wait: bool,
    /// Seconds after the buy of the check (no first sell yet: NO_POP / CHECK).
    pub check_secs: f64,
    /// At the check, under this rise (%) over the entry: NO_POP. None: no NO_POP sell.
    pub no_pop_percent: Option<f64>,
    /// Share of the position a NO_POP sell takes (1: everything).
    pub no_pop_share: f64,
    /// NO_POP only while nobody is buying (`down_quiet_buy_sol` in the last two seconds).
    pub no_pop_quiet: bool,
    /// No NO_POP while the high since the buy reached this rise (%) over the entry: a coin that
    /// popped before the check keeps its chance (the rest of the exit still runs).
    pub no_pop_hold_high_percent: Option<f64>,
    /// No NO_POP while other wallets bought this much SOL behind the fill.
    pub no_pop_hold_behind_sol: Option<f64>,
    /// A coin held past NO_POP by the two above is sold whole once the price falls this far (%)
    /// under the entry (None: only the rest of the exit).
    pub no_pop_stop_percent: Option<f64>,
    /// Once the first-sell trigger has fired, everything goes as soon as an insider has sold.
    pub insider_exit: bool,
    /// Instead of all of the above: everything at a take profit, a stop only after a wait, and a
    /// longer hold (`rules::PATIENT_EXIT`).
    pub patient: Option<Patient>,
}

/// The patient exit: sell everything once the price is `take_profit_percent` over the entry; from
/// `stop_after_secs` on, sell everything when it is `stop_percent` under the entry (None: no
/// stop); sell everything at `max_hold_secs`. Nothing is sold in slices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Patient {
    pub take_profit_percent: f64,
    pub stop_after_secs: f64,
    pub stop_percent: Option<f64>,
    pub max_hold_secs: f64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            first_down_percent: rules::FIRST_DOWN_PERCENT,
            step_down_percent: rules::STEP_DOWN_PERCENT,
            step_down_quiet: true,
            step_fall_percent: rules::STEP_FALL_PERCENT,
            down_quiet_buy_sol: rules::DOWN_QUIET_BUY_SOL,
            close_out_share: rules::CLOSE_OUT_SHARE,
            wait_drop_percent: rules::WAIT_DROP_PERCENT,
            wait_high_percent: rules::WAIT_HIGH_PERCENT,
            wait_buys: rules::WAIT_BUYS,
            wait_buy_sol: rules::WAIT_BUY_SOL,
            wait_sell_sol: rules::WAIT_SELL_SOL,
            max_waits: rules::MAX_WAITS,
            fade_buy_sol: rules::FADE_BUY_SOL,
            fade_quiet_secs: rules::FADE_QUIET_SECS,
            trail_percent: rules::TRAIL_PERCENT,
            trail_only_waiting: rules::TRAIL_ONLY_WAITING,
            trail_sells_all: rules::TRAIL_SELLS_ALL,
            first_share: rules::FIRST_SELL_SHARE,
            first_up_percent: rules::FIRST_SELL_MIN_UP_PERCENT,
            first_all_up_sol: rules::FIRST_ALL_UP_SOL,
            first_wait_slots: rules::FIRST_WAIT_SLOTS,
            first_wait: true,
            check_secs: rules::CHECK_SECS,
            no_pop_percent: rules::CHECK_NO_POP_PERCENT,
            no_pop_share: 1.0,
            no_pop_quiet: false,
            no_pop_hold_high_percent: rules::NO_POP_HOLD_HIGH_PERCENT,
            no_pop_hold_behind_sol: rules::NO_POP_HOLD_BEHIND_SOL,
            no_pop_stop_percent: rules::NO_POP_STOP_PERCENT,
            insider_exit: rules::INSIDER_EXIT,
            patient: rules::PATIENT_EXIT,
        }
    }
}

impl RefExit {
    pub fn new(bet_lamports: u64) -> Self {
        Self::with_tuning(bet_lamports, Tuning::default())
    }

    pub fn with_tuning(bet_lamports: u64, tuning: Tuning) -> Self {
        Self { big_bet: bet_lamports >= lamports(rules::BIG_BET_SOL), tuning, ..Default::default() }
    }

    /// Slots after the buy's slot that the first sell waits through.
    pub fn first_wait_slots(&self) -> u64 {
        self.tuning.first_wait_slots
    }

    fn down(x: f64, percent: Option<f64>) -> bool {
        percent.is_some_and(|p| x < 1.0 - p / 100.0)
    }

    /// NO_POP waits: the coin popped or drew buyers before the check, and has not fallen through
    /// the stop.
    fn no_pop_waits(&self, t: &Tick, x: f64) -> bool {
        let u = &self.tuning;
        let popped = u.no_pop_hold_high_percent.is_some_and(|h| self.peak_price as f64 >= t.entry_price as f64 * (1.0 + h / 100.0));
        let buyers = u.no_pop_hold_behind_sol.is_some_and(|sol| t.behind_lamports >= lamports(sol));
        (popped || buyers) && !Self::down(x, u.no_pop_stop_percent)
    }

    /// Nobody (or next to nobody) is buying: nothing to bring a falling price back.
    fn quiet(&self, t: &Tick) -> bool {
        self.tuning.down_quiet_buy_sol.is_none_or(|sol| t.recent_buy_lamports < lamports(sol))
    }

    pub fn evaluate(&mut self, t: &Tick) -> Action {
        let action = self.decide(t);
        // The first sell's trigger with an insider already out: everything, not a slice.
        if self.tuning.insider_exit && t.insider_sold && self.sells == 0 {
            if let Action::SellShare(_, SellReason::First) = action {
                return Action::SellAll(SellReason::Insider);
            }
        }
        action
    }

    fn decide(&mut self, t: &Tick) -> Action {
        let secs = t.held_for.as_secs_f64();
        if let Some(p) = self.tuning.patient {
            let x = t.price as f64 / t.entry_price.max(1) as f64;
            if secs >= p.max_hold_secs {
                return Action::SellAll(SellReason::Lifetime);
            }
            if x >= 1.0 + p.take_profit_percent / 100.0 {
                return Action::SellAll(SellReason::TakeProfit);
            }
            if secs >= p.stop_after_secs && Self::down(x, p.stop_percent) {
                return Action::SellAll(SellReason::Down);
            }
            return Action::Hold;
        }
        if secs >= rules::MAX_HOLD_SECS {
            return Action::SellAll(SellReason::Lifetime);
        }
        // Every tick counts toward the highs, the first-sell window's included (it was skipped
        // until 26 Sep: a spike in the window was missed by the wait-at-the-high rule).
        self.peak_price = self.peak_price.max(t.price);
        self.peak_since_sell = self.peak_since_sell.max(t.price);
        if self.tuning.insider_exit && t.insider_sold && self.sells > 0 {
            return Action::SellAll(SellReason::Insider);
        }
        let x = t.price as f64 / t.entry_price.max(1) as f64;
        // The first sell only on a rise (`first_up_percent`): below it every first-sell trigger
        // holds.
        let first_ok = self.tuning.first_up_percent.is_none_or(|p| x > 1.0 + p / 100.0);
        if let Some(sol) = self.tuning.first_all_up_sol {
            let up = t.window_net_lamports >= lamports(sol) as i64;
            if up && ((self.sells == 0 && first_ok) || (self.sells > 0 && !self.tuning.first_wait)) {
                return Action::SellAll(SellReason::FirstAll);
            }
            if self.sells == 0 && t.in_wait_window && self.tuning.first_wait {
                return Action::Hold;
            }
        }
        if self.sells == 0 {
            if !first_ok {
                return Action::Hold;
            }
            let behind = t.behind_lamports >= lamports(rules::FIRST_SELL_BEHIND_SOL);
            let popped = x >= 1.0 + rules::FIRST_SELL_PRICE_PERCENT / 100.0;
            if secs < self.tuning.check_secs {
                if behind || popped {
                    if Self::down(x, self.tuning.first_down_percent) && self.quiet(t) {
                        return Action::SellAll(SellReason::Down);
                    }
                    return Action::SellShare(self.tuning.first_share, SellReason::First);
                }
                return Action::Hold;
            }
            // From the check on, every tick until the first sell.
            if self.tuning.no_pop_percent.is_some_and(|p| x < 1.0 + p / 100.0)
                && (!self.tuning.no_pop_quiet || self.quiet(t))
                && !self.no_pop_waits(t, x)
            {
                if self.tuning.no_pop_share >= 1.0 {
                    return Action::SellAll(SellReason::NoPop);
                }
                return Action::SellShare(self.tuning.no_pop_share, SellReason::NoPop);
            }
            if t.behind_lamports >= lamports(rules::CHECK_BEHIND_SOL) {
                if x < 1.0 + rules::CHECK_RUN_PERCENT / 100.0 {
                    return Action::SellAll(SellReason::Check);
                }
                return Action::SellShare(self.tuning.first_share, SellReason::First);
            }
            if popped {
                return Action::SellShare(self.tuning.first_share, SellReason::First);
            }
            return Action::Hold;
        }
        if let Some(p) = self.tuning.trail_percent {
            let fell = (t.price as f64) < self.peak_since_sell as f64 * (1.0 - p / 100.0);
            if fell && (!self.tuning.trail_only_waiting || self.waits > 0) {
                if self.tuning.trail_sells_all || t.left <= self.tuning.close_out_share {
                    return Action::SellAll(SellReason::Trail);
                }
                let share = if self.big_bet { rules::BIG_BET_STEP_SHARE } else { rules::STEP_SHARE };
                return Action::SellShare(share, SellReason::Trail);
            }
        }
        let step = if self.sells == 1 { rules::FIRST_STEP_SECS } else { rules::STEP_SECS };
        let due = self.last_sell_at + Duration::from_secs_f64(step + self.waits as f64 * rules::STEP_SECS);
        if t.held_for < due {
            return Action::Hold;
        }
        if t.left <= self.tuning.close_out_share {
            return Action::SellAll(SellReason::CloseOut);
        }
        if Self::down(x, self.tuning.step_down_percent) && (!self.tuning.step_down_quiet || self.quiet(t)) {
            return Action::SellAll(SellReason::Down);
        }
        let since_last = t.price as f64 / self.last_sell_price.max(1) as f64;
        if Self::down(since_last, self.tuning.step_fall_percent) && self.quiet(t) {
            return Action::SellAll(SellReason::Down);
        }
        let faded = self.tuning.fade_buy_sol.is_some_and(|sol| t.recent_buy_lamports >= lamports(sol))
            && t.since_last_buy.is_some_and(|d| d.as_secs_f64() >= self.tuning.fade_quiet_secs);
        if faded {
            return Action::SellAll(SellReason::Fade);
        }
        let u = &self.tuning;
        let dropped = u.wait_drop_percent.is_some_and(|p| (t.price as f64) < self.last_sell_price as f64 * (1.0 - p / 100.0));
        let at_high = u.wait_high_percent.is_some_and(|p| (t.price as f64) >= self.peak_price as f64 * (1.0 - p / 100.0));
        let busy = u.wait_buys.is_some_and(|n| t.last_second.buys >= n)
            || u.wait_buy_sol.is_some_and(|sol| t.last_second.buy_lamports >= lamports(sol))
            || u.wait_sell_sol.is_some_and(|sol| t.last_second.sell_lamports >= lamports(sol))
            || dropped
            || at_high;
        if busy && self.waits < u.max_waits {
            self.waits += 1;
            return Action::Hold;
        }
        let share = if self.big_bet { rules::BIG_BET_STEP_SHARE } else { rules::STEP_SHARE };
        Action::SellShare(share, SellReason::Step)
    }

    /// A partial sell landed at `held_for` after the buy, decided at `price`.
    pub fn sold(&mut self, price: u64, held_for: Duration) {
        self.sells += 1;
        self.last_sell_at = held_for;
        self.last_sell_price = price;
        self.peak_since_sell = price;
        self.waits = 0;
    }
}

/// pXGQ's exit (`SELL_RULE=pxgq`, 2026-10-12; research/filter-pXGQ-2026-10-12/SELL_STUDY.md and
/// pxsell.py): price and the flow behind the fill decide, not a clock. Checked on every tick:
/// - stop: the whole position's value at `stop_x` of its cost or less: everything;
/// - no wave: from `wave_min_secs` to `wave_secs` after the buy, others' net buying behind the fill
///   under `wave_sol` while the value is under x1.05: everything;
/// - scale out: the value at `scale_at_x` or more and within 5% of its peak: `scale_share` once,
///   the rest runs;
/// - after the scale-out: `trail` under the peak, or x3: the rest;
/// - dump: no scale-out yet, `dump_dd` under the peak while still at cost or better: everything;
/// - pressure: under `pressure_x` while others sold `pressure_sol` SOL or more in the last 3 s:
///   everything;
/// - `max_hold_secs`: everything (a safety cap, not part of his rule).
/// "Value" is the whole position (sold shares at their sale prices plus what is held, at the
/// executable price) over its cost, so it starts near x0.975 (fees both ways).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PxSettings {
    pub stop_x: f64,
    pub wave_min_secs: f64,
    pub wave_secs: f64,
    pub wave_sol: f64,
    pub scale_at_x: f64,
    pub scale_share: f64,
    pub trail: f64,
    pub take_profit_x: f64,
    pub dump_dd: f64,
    pub pressure_x: f64,
    pub pressure_sol: f64,
    pub max_hold_secs: f64,
}

impl Default for PxSettings {
    /// The parameters replayed in the research (pxsell.py, base): on followed launches of Oct 8-12
    /// scoring 69+ on the scorecard, +0.082 SOL a 0.5 SOL try against the bot's exit's +0.104.
    fn default() -> Self {
        Self {
            stop_x: 0.85,
            wave_min_secs: 2.0,
            wave_secs: 8.0,
            wave_sol: 1.0,
            scale_at_x: 1.25,
            scale_share: 0.5,
            trail: 0.20,
            take_profit_x: 3.0,
            dump_dd: 0.15,
            pressure_x: 0.95,
            pressure_sol: 4.0,
            max_hold_secs: 600.0,
        }
    }
}

impl PxSettings {
    /// The defaults, each overridable from the environment (PX_STOP_X, PX_WAVE_MIN_SECS,
    /// PX_WAVE_SECS, PX_WAVE_SOL, PX_SCALE_AT_X, PX_SCALE_SHARE, PX_TRAIL, PX_TAKE_PROFIT_X,
    /// PX_DUMP_DD, PX_PRESSURE_X, PX_PRESSURE_SOL, PX_MAX_HOLD_SECS).
    pub fn from_env() -> Self {
        let d = Self::default();
        let f = |key: &str, default: f64| std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default);
        Self {
            stop_x: f("PX_STOP_X", d.stop_x),
            wave_min_secs: f("PX_WAVE_MIN_SECS", d.wave_min_secs),
            wave_secs: f("PX_WAVE_SECS", d.wave_secs),
            wave_sol: f("PX_WAVE_SOL", d.wave_sol),
            scale_at_x: f("PX_SCALE_AT_X", d.scale_at_x),
            scale_share: f("PX_SCALE_SHARE", d.scale_share),
            trail: f("PX_TRAIL", d.trail),
            take_profit_x: f("PX_TAKE_PROFIT_X", d.take_profit_x),
            dump_dd: f("PX_DUMP_DD", d.dump_dd),
            pressure_x: f("PX_PRESSURE_X", d.pressure_x),
            pressure_sol: f("PX_PRESSURE_SOL", d.pressure_sol),
            max_hold_secs: f("PX_MAX_HOLD_SECS", d.max_hold_secs),
        }
    }
}

pub static PX_SETTINGS: LazyLock<PxSettings> = LazyLock::new(PxSettings::from_env);

#[derive(Debug, Clone, PartialEq)]
pub struct PxExit {
    pub sells: u32,
    s: PxSettings,
    scaled: bool,
    /// A scale-out was asked for and has not landed yet.
    scale_pending: bool,
    peak_x: f64,
    value_x: f64,
    /// Value of the shares already sold, over the position's cost.
    realized_x: f64,
    /// Share still held at the last check, and the price multiple of the last decision.
    last_left: f64,
    last_x: f64,
    /// Price multiple at the last sell that landed.
    sold_x: f64,
}

impl PxExit {
    pub fn new(s: PxSettings) -> Self {
        Self { sells: 0, s, scaled: false, scale_pending: false, peak_x: 0.0, value_x: 0.0, realized_x: 0.0, last_left: 1.0, last_x: 1.0, sold_x: 1.0 }
    }

    /// The whole position's value over its cost and its peak, at the last check.
    pub fn value_peak(&self) -> (f64, f64) {
        (self.value_x, self.peak_x)
    }

    pub fn evaluate(&mut self, t: &Tick) -> Action {
        let s = self.s;
        let x = t.price as f64 / t.entry_price.max(1) as f64;
        self.last_x = x;
        // Shares sold since the last check count at the price their sell was decided at.
        if t.left < self.last_left - 1e-9 {
            self.realized_x += (self.last_left - t.left) * self.sold_x;
            self.last_left = t.left;
        }
        let value = self.realized_x + t.left * x;
        self.value_x = value;
        self.peak_x = self.peak_x.max(value);
        let peak = self.peak_x;
        let secs = t.held_for.as_secs_f64();
        let net_sol = (t.behind_lamports as f64 - t.behind_sell_lamports as f64) / 1e9;
        if secs >= s.max_hold_secs {
            return Action::SellAll(SellReason::Lifetime);
        }
        if value <= s.stop_x {
            return Action::SellAll(SellReason::Stop);
        }
        if secs >= s.wave_min_secs && secs <= s.wave_secs && net_sol < s.wave_sol && value < 1.05 {
            return Action::SellAll(SellReason::NoWave);
        }
        if !self.scaled && value >= s.scale_at_x && value >= peak * 0.95 {
            self.scale_pending = true;
            return Action::SellShare(s.scale_share, SellReason::Scale);
        }
        if self.scaled && value >= s.take_profit_x {
            return Action::SellAll(SellReason::TakeProfit);
        }
        if self.scaled && value <= peak * (1.0 - s.trail) {
            return Action::SellAll(SellReason::Trail);
        }
        if !self.scaled && value <= peak * (1.0 - s.dump_dd) && value >= 1.0 {
            return Action::SellAll(SellReason::Dump);
        }
        if value < s.pressure_x && t.recent_sell_lamports_3s as f64 / 1e9 >= s.pressure_sol {
            return Action::SellAll(SellReason::Pressure);
        }
        Action::Hold
    }

    /// A sell landed (decided at the last check's price).
    pub fn sold(&mut self) {
        self.sells += 1;
        self.sold_x = self.last_x;
        if self.scale_pending {
            self.scaled = true;
            self.scale_pending = false;
        }
    }
}

/// The exit a position runs: the bot's (`RefExit`) or pXGQ's (`PxExit`), by `SELL_RULE`
/// (`ref` default, `pxgq`).
#[derive(Debug, Clone)]
pub enum Exit {
    Ref(RefExit),
    Px(PxExit),
    /// The snipe test's (`utils::snipe_test`, SNIPE_TEST=1): all out at +30% or 15 s.
    Test(TestExit),
}

/// A take-profit-or-time exit, all out at the first of +X (the tokens' executable value over their
/// cost) or a hold time after the fill. No stop-loss, no trailing. The snipe test's: +30% or 15 s;
/// the whale rule's: +23% or 90 s.
#[derive(Debug, Clone, PartialEq)]
pub struct TestExit {
    pub sells: u32,
    pub take_profit_x: f64,
    pub hold: Duration,
}

impl Default for TestExit {
    fn default() -> Self {
        Self { sells: 0, take_profit_x: crate::utils::snipe_test::TAKE_PROFIT_X, hold: crate::utils::snipe_test::MAX_HOLD }
    }
}

impl TestExit {
    /// The whale rule's: +23% or 90 s.
    pub fn whale() -> Self {
        Self { sells: 0, take_profit_x: crate::processors::whale::TAKE_PROFIT_X, hold: crate::processors::whale::MAX_HOLD }
    }

    pub fn evaluate(&mut self, t: &Tick) -> Action {
        let x = t.price as f64 / t.entry_price.max(1) as f64;
        if x >= self.take_profit_x {
            Action::SellAll(SellReason::TakeProfit)
        } else if t.held_for >= self.hold {
            Action::SellAll(SellReason::Time)
        } else {
            Action::Hold
        }
    }
}

/// `SELL_RULE=pxgq` selects pXGQ's exit.
pub fn px_exit_on() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var("SELL_RULE").is_ok_and(|v| v.trim().eq_ignore_ascii_case("pxgq")));
    *ON
}

impl Exit {
    pub fn new(bet_lamports: u64) -> Self {
        if crate::utils::snipe_test::rules_on() {
            Exit::Test(TestExit::default())
        } else if px_exit_on() {
            Exit::Px(PxExit::new(*PX_SETTINGS))
        } else {
            Exit::Ref(RefExit::new(bet_lamports))
        }
    }

    pub fn evaluate(&mut self, t: &Tick) -> Action {
        match self {
            Exit::Ref(e) => e.evaluate(t),
            Exit::Px(e) => e.evaluate(t),
            Exit::Test(e) => e.evaluate(t),
        }
    }

    pub fn sold(&mut self, price: u64, held_for: Duration) {
        match self {
            Exit::Ref(e) => e.sold(price, held_for),
            Exit::Px(e) => e.sold(),
            Exit::Test(e) => e.sells += 1,
        }
    }

    pub fn first_wait_slots(&self) -> u64 {
        match self {
            Exit::Ref(e) => e.first_wait_slots(),
            Exit::Px(_) | Exit::Test(_) => 0,
        }
    }

    pub fn sells(&self) -> u32 {
        match self {
            Exit::Ref(e) => e.sells,
            Exit::Px(e) => e.sells,
            Exit::Test(e) => e.sells,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Exit::Ref(_) => "the bot's exit",
            Exit::Px(_) => "pXGQ's exit",
            Exit::Test(e) => if e.hold == crate::processors::whale::MAX_HOLD { "the whale rule's exit (+23% or 90 s)" } else { "the snipe test's exit (+30% or 15 s)" },
        }
    }
}

pub fn pnl_percent(entry_lamports: u64, value_lamports: u64) -> f64 {
    if entry_lamports == 0 {
        return 0.0;
    }
    (value_lamports as f64 - entry_lamports as f64) / entry_lamports as f64 * 100.0
}

/// An open position handed over by the buy flow.
#[derive(Debug, Clone)]
pub struct Position {
    pub market: Market,
    /// Lamports that went into the swap (exact-input buy amount).
    pub entry_lamports: u64,
    /// Paper positions (DRY_RUN) track a simulated balance and never send sells.
    pub paper_token_amount: Option<u64>,
    /// Paper positions on pump.fun: SOL the paper buy put into the curve, less what paper
    /// sells took out. The chain never sees paper trades, so every curve state read from
    /// the stream or RPC gets them added back (`apply_paper_trades`) before pricing.
    pub paper_sol_in_curve: u64,
    /// Live positions: the token balance as the bot's own fills reported it.
    pub known_tokens: Option<u64>,
    pub opened_at: Instant,
    pub slot: Option<std::sync::Arc<crate::processors::common::PositionSlot>>,
    /// The curve's trade feed, subscribed before the buy was sent.
    pub feed: Option<Arc<position_feed::FeedGuard>>,
    pub fast_rug: bool,
}

pub fn spawn_position(position: Position) {
    if let Some(guard) = &position.feed {
        note_insiders(guard, &position.market.mint());
    }
    tokio::spawn(async move {
        let mint = position.market.mint();
        // HOT_SELL_FANOUT: the sell's instructions built now, at the buy.
        if position.paper_token_amount.is_none() {
            prebuild_sell(&position.market);
        }
        let result = monitor(position).await;
        crate::utils::snipe_test::position_done(&mint.to_string());
        crate::processors::whale::position_done(&mint);
        SELL_TEMPLATES.lock().unwrap_or_else(|e| e.into_inner()).remove(&mint);
        STUCK_WATCH.lock().unwrap_or_else(|e| e.into_inner()).remove(&mint);
        remove_position(&mint.to_string());
        control::forget_sell_request(&mint);
        if let Err(e) = result {
            warn!("auto-sell for {mint} stopped: {e:#}");
        }
    });
}

/// Hands the launch stream's dev and bundle wallets to the position's feed (`rules::INSIDER_EXIT`).
/// Call while the launch is still watched (the snipe that bought it is running).
pub fn note_insiders(guard: &position_feed::FeedGuard, mint: &solana_pubkey::Pubkey) {
    if let Some(view) = crate::processors::launch_watch::view(mint) {
        guard.feed.set_insiders(view.insiders());
    }
}

struct Holding {
    initial_tokens: u64,
}

impl Holding {
    fn sold_percent(&self, balance: u64) -> f64 {
        if self.initial_tokens == 0 {
            0.0
        } else {
            100.0 * (1.0 - balance as f64 / self.initial_tokens as f64)
        }
    }

    fn cost_of(&self, entry_lamports: u64, tokens: u64) -> u64 {
        if self.initial_tokens == 0 {
            entry_lamports
        } else {
            (entry_lamports as u128 * tokens as u128 / self.initial_tokens as u128) as u64
        }
    }
}

fn publish(position: &Position, balance: u64, value: Option<u64>, peak: u64, holding: &Holding, sells: u32, opened_at: u64) {
    let cost = holding.cost_of(position.entry_lamports, balance);
    let pnl = |v: u64| pnl_percent(cost, v);
    upsert_position(PositionView {
        mint: position.market.mint().to_string(),
        dex: position.market.dex().to_string(),
        paper: position.paper_token_amount.is_some(),
        entry_sol: lamports_to_sol(cost),
        token_amount: balance,
        value_sol: value.map(lamports_to_sol),
        pnl_percent: value.map(pnl),
        peak_pnl_percent: (peak > 0).then(|| pnl(peak)),
        take_profit_armed: false,
        sold_percent: holding.sold_percent(balance),
        ladder_step: sells,
        opened_at,
        updated_at: now_unix(),
    });
}

async fn monitor(mut position: Position) -> Result<()> {
    let mint = position.market.mint();
    let token_account = get_associated_token_address(&trading_wallet(), &mint, &position.market.token_program());
    // A whale position runs the whale rule's exit; every other the bot's (or the snipe test's).
    let mut exit = if crate::processors::whale::owns(&mint) { Exit::Test(TestExit::whale()) } else { Exit::new(position.entry_lamports) };
    let mut holding = Holding { initial_tokens: 0 };
    let mut peak = 0u64;
    let mut ticker = tokio::time::interval(Duration::from_millis(rules::EXIT_TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let feed = position.feed.clone().or_else(|| {
        matches!(position.market, Market::PumpFun(_)).then(|| Arc::new(position_feed::subscribe(mint)))
    });
    // After a failed sell the stream may lag the chain, so value from RPC until one succeeds.
    let mut failed_sells: u32 = 0;
    // One instant retry per position after a creator change; further failures back off.
    let mut stale_retried = false;
    let opened_at = now_unix();
    publish(&position, position.paper_token_amount.unwrap_or(0), None, peak, &holding, 0, opened_at);
    let insiders = if rules::INSIDER_EXIT { feed.as_ref().map_or(0, |g| g.feed.insider_count()) } else { usize::MAX };
    info!(
        "{} {} on {} with {} (entry {:.6} SOL{}, {})",
        "auto-sell watching".cyan(),
        mint,
        position.market.dex(),
        exit.name(),
        lamports_to_sol(position.entry_lamports),
        if position.fast_rug { ", fast-rug profile" } else { "" },
        match insiders {
            usize::MAX => "insider exit off".to_string(),
            0 => "insiders unknown".to_string(),
            n => format!("watching the dev and {} bundle wallet{}", n - 1, if n == 2 { "" } else { "s" }),
        }
    );

    loop {
        match &feed {
            Some(guard) => tokio::select! {
                _ = ticker.tick() => {}
                _ = guard.feed.changed() => {}
                _ = control::sell_requested() => {}
            },
            None => tokio::select! {
                _ = ticker.tick() => {}
                _ = control::sell_requested() => {}
            },
        }

        let balance = match (position.paper_token_amount, position.known_tokens) {
            (Some(amount), _) | (None, Some(amount)) => amount,
            (None, None) => match get_token_balance(&RPC_CLIENT, &token_account).await {
                Ok(balance) => {
                    position.known_tokens = Some(balance);
                    balance
                }
                Err(e) => {
                    warn!("balance check for {mint} failed: {e:#}");
                    continue;
                }
            },
        };
        if balance == 0 {
            info!("position {mint} has no tokens left; stopping auto-sell");
            return Ok(());
        }
        if holding.initial_tokens == 0 {
            holding.initial_tokens = balance;
        }

        let stream = if failed_sells > 0 { None } else { feed.as_ref().and_then(|guard| guard.feed.state()) };
        let Some(value) = valuation(&mut position, balance, stream).await else {
            continue;
        };
        peak = peak.max(value);
        let cost = holding.cost_of(position.entry_lamports, balance);
        let pnl = pnl_percent(cost, value);
        let held_for = position.opened_at.elapsed();
        let reference = (holding.initial_tokens / REFERENCE_DIVISOR).max(1);
        let entry_price = holding.cost_of(position.entry_lamports, reference);
        let Ok(price) = position.market.quote_sell(reference) else {
            continue;
        };
        let now = Instant::now();
        let insider_sold = feed.as_ref().is_some_and(|g| g.feed.insider_sold());
        let recent_sell_lamports_3s = feed.as_ref().map_or(0, |g| g.feed.flow_since(now - Duration::from_secs(3)).sell_lamports);
        let (behind, last_second, recent_buy_lamports, since_last_buy, window, in_wait_window) = match &feed {
            Some(guard) => {
                let slots = exit.first_wait_slots();
                let in_window = match (guard.feed.fill_slot(), crate::utils::latency::latest_slot()) {
                    (Some(fill), Some(now)) => now <= fill + slots,
                    _ => false,
                };
                (
                    guard.feed.behind_fill(position.opened_at),
                    guard.feed.flow_since(now - Duration::from_secs(1)),
                    guard.feed.flow_since(now - Duration::from_secs(2)).buy_lamports,
                    guard.feed.last_buy_at().map(|at| now.saturating_duration_since(at)),
                    guard.feed.behind_within_slots(slots),
                    in_window,
                )
            }
            None => (Flow::default(), Flow::default(), 0, None, Flow::default(), false),
        };
        let action = if control::take_sell_request(&mint) {
            Action::SellAll(SellReason::Manual)
        } else {
            exit.evaluate(&Tick {
                held_for,
                entry_price,
                price,
                behind_lamports: behind.buy_lamports,
                behind_sell_lamports: behind.sell_lamports,
                last_second,
                recent_buy_lamports,
                recent_sell_lamports_3s,
                since_last_buy,
                left: balance as f64 / holding.initial_tokens as f64,
                window_net_lamports: window.buy_lamports as i64 - window.sell_lamports as i64,
                in_wait_window,
                insider_sold,
            })
        };
        publish(&position, balance, Some(value), peak, &holding, exit.sells(), opened_at);
        let (amount, reason) = match action {
            Action::Hold => continue,
            Action::SellAll(reason) => (balance, reason),
            Action::SellShare(share, reason) => {
                let amount = ((balance as f64) * share).round() as u64;
                // A slice that would leave dust sells everything.
                if amount == 0 || balance - amount < reference {
                    (balance, reason)
                } else {
                    (amount, reason)
                }
            }
        };
        if let Exit::Px(px) = &exit {
            let (value_x, peak_x) = px.value_peak();
            crate::utils::db::write(crate::utils::db::Write::ExitDecision {
                mint: mint.to_string(),
                t: now_unix() as i64,
                paper: position.paper_token_amount.is_some(),
                reason: reason.code(),
                share: amount as f64 / balance.max(1) as f64,
                value_x,
                peak_x,
                held_secs: held_for.as_secs_f64(),
                net_behind_sol: (behind.buy_lamports as f64 - behind.sell_lamports as f64) / 1e9,
            });
        }
        // The stream's position at the decision: with the sell's landing it calibrates where paper
        // sells land.
        let decided_at = crate::utils::latency::stream_position().map(|(s, i)| format!(", decided at slot {s} index {i}")).unwrap_or_default();
        info!(
            "{} {mint}: {} {} of {} tokens at x{:.2} ({pnl:+.2}%) after {:.1}s, {:.2} SOL bought behind the fill, curve {:+.2} SOL since it{}{decided_at}",
            "SELL".yellow().bold(),
            reason.code(),
            amount,
            balance,
            price as f64 / entry_price.max(1) as f64,
            held_for.as_secs_f64(),
            lamports_to_sol(behind.buy_lamports),
            (behind.buy_lamports as f64 - behind.sell_lamports as f64) / 1e9,
            if insider_sold { ", an insider has sold" } else { "" }
        );
        let mut retry_now = false;
        match sell(&mut position, balance, amount, reason, &holding, feed.as_ref().map(|g| &*g.feed)).await {
            Ok(true) if amount == balance => return Ok(()),
            Ok(true) => {
                failed_sells = 0;
                stale_retried = false;
                // The clock restarts when the sell has landed (or the paper sell is done).
                exit.sold(price, position.opened_at.elapsed());
            }
            Ok(false) => failed_sells += 1,
            // The next pass values from RPC (failed_sells > 0), which reads the new creator.
            Err(e) if e.is::<StaleAccounts>() && !stale_retried => {
                failed_sells += 1;
                retry_now = true;
                stale_retried = true;
            }
            Err(e) => {
                warn!("sell of {mint} failed: {e:#}");
                failed_sells += 1;
            }
        }
        // HOT_SELL_FANOUT never stops selling, except for a coin that can't be sold (STUCK).
        if failed_sells > 0 && crate::hot::sell::fanout_on() {
            let value = position.market.quote_sell(balance).ok();
            if let Some(why) = stuck_verdict(&mint, value) {
                mark_stuck(&mint, &why);
                return Ok(());
            }
        }
        if failed_sells > 0 && !(retry_now && failed_sells < MAX_FAILED_SELLS) && give_up_or_back_off(&mint, failed_sells).await {
            return Ok(());
        }
    }
}

/// A position the fan-out could not sell (2026-10-12, the owner's rule): 3 fresh sells in a row
/// failed with the same program error that a retry can't fix, or the fees its sells spent exceed
/// what its tokens are worth now. Temporary failures (not landed, the price guard, the creator's
/// account moving, a blockhash or nonce) never count toward it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StuckWatch {
    last_error: Option<String>,
    same: u32,
    pub fees: u64,
}

/// Fresh sells failing the same way before a position is STUCK.
pub const STUCK_AFTER: u32 = 3;

impl StuckWatch {
    /// One fresh sell's outcome and the fees it spent.
    pub fn note(&mut self, outcome: &ConfirmOutcome, fees: u64) {
        self.fees = self.fees.saturating_add(fees);
        if let ConfirmOutcome::Failed { error, .. } = outcome {
            if temporary(error) {
                return;
            }
            if self.last_error.as_deref() == Some(error.as_str()) {
                self.same += 1;
            } else {
                self.last_error = Some(error.clone());
                self.same = 1;
            }
        }
    }

    /// Why the position is STUCK now (`value`: its tokens' worth, lamports), if it is.
    pub fn verdict(&self, value: Option<u64>) -> Option<String> {
        if self.same >= STUCK_AFTER {
            return Some(format!("{} fresh sells failed with the same error ({})", self.same, self.last_error.as_deref().unwrap_or("")));
        }
        match value {
            Some(v) if self.fees > v => Some(format!("its sells spent {:.6} SOL in fees, more than its tokens are worth now ({:.6} SOL)", self.fees as f64 / 1e9, v as f64 / 1e9)),
            _ => None,
        }
    }
}

/// An on-chain error a fresh sell (new quote, new accounts) can get past: the price guard
/// (pump.fun 6002, 6003, 6042), the creator's account moving (2006, rebuilt), a blockhash or a
/// nonce that moved on.
fn temporary(error: &str) -> bool {
    ["Custom(6002)", "Custom(6003)", "Custom(6042)", STALE_ACCOUNTS, "BlockhashNotFound", "AlreadyProcessed", "Nonce"].iter().any(|t| error.contains(t))
}

static STUCK_WATCH: LazyLock<Mutex<HashMap<Pubkey, StuckWatch>>> = LazyLock::new(Default::default);

fn note_stuck(mint: &Pubkey, outcome: &ConfirmOutcome, fees: u64) {
    STUCK_WATCH.lock().unwrap_or_else(|e| e.into_inner()).entry(*mint).or_default().note(outcome, fees);
}

fn stuck_verdict(mint: &Pubkey, value: Option<u64>) -> Option<String> {
    STUCK_WATCH.lock().unwrap_or_else(|e| e.into_inner()).get(mint).and_then(|w| w.verdict(value))
}

/// Stops selling a coin that can't be sold: the tokens stay in the wallet, the dashboard says so.
fn mark_stuck(mint: &Pubkey, why: &str) {
    warn!("{} {mint}: {why}; stopped selling it, the tokens stay in the wallet", "STUCK".red().bold());
    activity(ActivityKind::SellFailed, None, Some(&mint.to_string()), format!("STUCK: {why}. Selling stopped; the tokens are still in the wallet."));
}

const MAX_FAILED_SELLS: u32 = 5;

/// pump.fun's seeds check (the creator vault named in the sell no longer matches the curve's
/// creator, which moved to a fee-sharing account).
const STALE_ACCOUNTS: &str = "Custom(2006)";

/// A sell failed because the coin's creator changed: retry at once, not after the back-off.
#[derive(Debug)]
struct StaleAccounts;

impl std::fmt::Display for StaleAccounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the coin's creator changed before the sell landed")
    }
}

impl std::error::Error for StaleAccounts {}



async fn give_up_or_back_off(mint: &solana_pubkey::Pubkey, failed_sells: u32) -> bool {
    // HOT_SELL_FANOUT (the owner's rule): never stop selling while tokens remain; a fresh sell (new
    // nonce, fresh quote) goes out at once (the cancel already took its seconds).
    if crate::hot::sell::fanout_on() {
        tokio::time::sleep(Duration::from_millis(100)).await;
        return false;
    }
    if failed_sells >= MAX_FAILED_SELLS {
        warn!(
            "{} {mint}: {failed_sells} sells failed in a row; stopped selling it to stop paying fees. \
             The tokens are still in the wallet - sell them by hand.",
            "GAVE UP".red().bold()
        );
        activity(
            ActivityKind::SellFailed,
            None,
            Some(&mint.to_string()),
            format!("{failed_sells} sells failed in a row; stopped selling. The tokens are still in the wallet."),
        );
        return true;
    }
    tokio::time::sleep(Duration::from_millis(500 << (failed_sells - 1).min(4))).await;
    false
}


/// Sell instructions built once per position (HOT_SELL_FANOUT, 2026-10-12): the pump.fun sell's
/// accounts are derived at the first build (at the buy, `prebuild_sell`), then each sell only
/// patches its amounts (the sell's data: tokens at 8..16, minimum SOL out at 16..24). A creator
/// change (the creator vault moves) builds it again. Other venues, or the switch off: built fresh.
static SELL_TEMPLATES: LazyLock<Mutex<HashMap<Pubkey, (Pubkey, Vec<solana_instruction::Instruction>)>>> = LazyLock::new(Default::default);

fn sell_instructions_cached(market: &Market, wallet: &Pubkey, amount: u64, min_out: u64) -> Result<Vec<solana_instruction::Instruction>> {
    let Market::PumpFun(m) = market else { return market.sell_instructions(wallet, amount, min_out) };
    if !crate::hot::sell::fanout_on() {
        return market.sell_instructions(wallet, amount, min_out);
    }
    let mut templates = SELL_TEMPLATES.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = !matches!(templates.get(&m.mint), Some((creator, _)) if *creator == m.creator);
    if fresh {
        templates.insert(m.mint, (m.creator, market.sell_instructions(wallet, 0, 0)?));
    }
    let mut ixs = templates[&m.mint].1.clone();
    patch_sell_amounts(&mut ixs, amount, min_out);
    Ok(ixs)
}

/// Writes `amount` and `min_out` into the pump.fun sell instruction of `ixs`.
fn patch_sell_amounts(ixs: &mut [solana_instruction::Instruction], amount: u64, min_out: u64) {
    if let Some(sell) = ixs.iter_mut().find(|ix| ix.program_id == crate::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID && ix.data.len() >= 24) {
        sell.data[8..16].copy_from_slice(&amount.to_le_bytes());
        sell.data[16..24].copy_from_slice(&min_out.to_le_bytes());
    }
}

/// At the buy (a live position opening): the sell's instructions built now, off the hot cores.
pub fn prebuild_sell(market: &Market) {
    if TRADE_SETTING.dry_run || !crate::hot::sell::fanout_on() {
        return;
    }
    if let Err(e) = sell_instructions_cached(market, &trading_wallet(), 0, 0) {
        warn!("sell template for {}: {e:#}", market.mint());
    }
}

/// Prices compare the value of 1/20 of the initial tokens, so they stay comparable as slices sell.
const REFERENCE_DIVISOR: u64 = 20;


fn keep_token_account(
    instructions: &mut Vec<solana_instruction::Instruction>,
    token_account: &solana_pubkey::Pubkey,
) {
    instructions.retain(|ix| {
        let closes = ix.data == [9]
            && ix
                .accounts
                .first()
                .is_some_and(|a| a.pubkey == *token_account);
        !closes
    });
}





fn apply_paper_trades(position: &mut Position) {
    let (Some(tokens), Market::PumpFun(m)) = (position.paper_token_amount, &mut position.market)
    else {
        return;
    };
    let r = &mut m.reserves;
    r.virtual_sol_reserves += position.paper_sol_in_curve;
    r.real_sol_reserves += position.paper_sol_in_curve;
    r.virtual_token_reserves = r.virtual_token_reserves.saturating_sub(tokens);
    r.real_token_reserves = r.real_token_reserves.saturating_sub(tokens);
}


fn curve_sol_out(market: &Market, tokens: u64) -> u64 {
    match market {
        Market::PumpFun(m) => {
            let r = &m.reserves;
            (tokens as u128 * r.virtual_sol_reserves as u128
                / (r.virtual_token_reserves as u128 + tokens as u128)) as u64
        }
        _ => 0,
    }
}




async fn valuation(
    position: &mut Position,
    balance: u64,
    stream: Option<position_feed::CurveState>,
) -> Option<u64> {
    let mint = position.market.mint();
    if let (Market::PumpFun(m), Some(state)) = (&mut position.market, stream) {
        m.reserves = state.reserves;
        m.creator = state.creator;
        
        if state.reserves.real_token_reserves > 0 {
            apply_paper_trades(position);
            if let Ok(value) = position.market.quote_sell(balance) {
                return Some(value);
            }
        }
    }
    let refreshed = position.market.refresh(&RPC_CLIENT).await;
    if refreshed.is_ok() {
        apply_paper_trades(position);
    }
    match refreshed {
        Ok(()) => match position.market.quote_sell(balance) {
            Ok(value) => return Some(value),
            Err(e) => warn!("sell quote for {mint} failed: {e:#}"),
        },
        Err(e) => match position.market.migrated(&RPC_CLIENT).await {
            Ok(Some(next)) => {
                info!(
                    "{mint} moved from {} to {}",
                    position.market.dex(),
                    next.dex()
                );
                position.market = next;
                return position.market.quote_sell(balance).ok();
            }
            Ok(None) => warn!("pool refresh for {mint} failed: {e:#}"),
            Err(migration_err) => {
                warn!("pool refresh for {mint} failed: {e:#} ({migration_err:#})")
            }
        },
    }

    if TRADE_SETTING.birdeye_api_key.is_some() {
        match birdeye_value_lamports(&mint, balance).await {
            Ok(value) => return Some(value),
            Err(e) => warn!("Birdeye price for {mint} failed: {e:#}"),
        }
    }
    None
}



async fn sell(
    position: &mut Position,
    balance: u64,
    amount: u64,
    reason: SellReason,
    holding: &Holding,
    feed: Option<&position_feed::Feed>,
) -> Result<bool> {
    let market = &position.market;
    let mint = market.mint();
    let expected = market.quote_sell(amount)?;
    let min_out = TRADE_SETTING.min_out(expected);
    let cost = holding.cost_of(position.entry_lamports, amount);
    let pnl = pnl_percent(cost, expected);
    let partial = amount < balance;
    let what = if partial {
        format!(
            "{:.0}% of the tokens",
            100.0 * amount as f64 / balance as f64
        )
    } else {
        "all tokens".to_string()
    };

    let mut record = TradeRecord {
        action: "SELL",
        dex: market.dex().to_string(),
        mint: mint.to_string(),
        sol_in: Some(lamports_to_sol(cost)),
        sol_out: Some(lamports_to_sol(expected)),
        token_amount: Some(amount),
        pnl_percent: Some(pnl),
        reason: reason.code().to_string(),
        ..Default::default()
    };

    let dex_name = market.dex().name();
    let mint_text = mint.to_string();
    if position.paper_token_amount.is_some() {
        // The hot sell path in a paper run (HOT_SELL_LANES, the lanes timing paper trades): an
        // unlandable copy of this sell goes out on the lanes now, as a live sell would.
        if crate::hot::sell::enabled() {
            let paper_key = solana_signer::Signer::pubkey(&*crate::config::hot_signer());
            if let Err(e) = market.sell_instructions(&paper_key, amount, min_out).and_then(|ixs| crate::hot::sell::send_dry(&mint, ixs)) {
                debug!("hot sell (paper copy) of {mint}: {e:#}");
            }
        }
        let setting = &*TRADE_SETTING;
        let mut filled = expected;
        // Where a live sell sent now would land: at a block position drawn from real landings
        // (DRY_RUN_FILL_BY_POSITION, the default), or a landing delay after sending.
        let by_position = match (setting.dry_run_fill_by_position, feed, &position.market) {
            (true, Some(feed), Market::PumpFun(_)) => paper_sell_landing(feed).await,
            _ => None,
        };
        let delay = if by_position.is_none() { setting.dry_run_sell_latency.draw() } else { None };
        if by_position.is_some() || delay.is_some() {
            let state = match by_position {
                Some((landing, state)) => {
                    info!("paper sell {mint} lands at slot {} index {}", landing.0, landing.1);
                    Some(state)
                }
                None => {
                    tokio::time::sleep(delay.unwrap_or_default()).await;
                    feed.and_then(|f| f.state())
                }
            };
            if let (Market::PumpFun(m), Some(state)) = (&mut position.market, state) {
                if state.reserves.real_token_reserves > 0 {
                    m.reserves = state.reserves;
                    m.creator = state.creator;
                    apply_paper_trades(position);
                }
            }
            if let Ok(value) = position.market.quote_sell(amount) {
                filled = value;
            }
            if filled < min_out {
                record.status = format!(
                    "simulated: price fell past the {}% sell slippage before a live sell lands",
                    setting.slippage_bps as f64 / 100.0
                );
                // A live sell that fails on chain still pays its fees.
                if setting.dry_run_charge_fees {
                    record.wallet_sol_change = Some(-(crate::service::modeled_failed_cost_lamports(SendProfile::Sell) as f64) / 1e9);
                }
                log_trade(&record);
                bump(&STATS.sells_failed);
                warn!("paper sell of {mint} would have failed its slippage check ({filled} < {min_out} lamports)");
                return Ok(false);
            }
        }
        let pnl = pnl_percent(cost, filled);
        record.sol_out = Some(lamports_to_sol(filled));
        record.pnl_percent = Some(pnl);
        if setting.dry_run_charge_fees {
            let fee = crate::service::modeled_cost_lamports(SendProfile::Sell);
            record.wallet_sol_change = Some((filled as f64 - fee as f64) / 1e9);
        }
        let expected = filled;
        record.status = "paper".into();
        log_trade(&record);
        bump(&STATS.sells_ok);
        activity(
            ActivityKind::Sold,
            Some(dex_name),
            Some(&mint_text),
            format!(
                "Paper sell ({}): {what} for {:.6} SOL, {pnl:+.2}%",
                reason.code(),
                lamports_to_sol(expected)
            ),
        );
        info!(
            "paper sell {mint}: {what} for {:.6} SOL ({pnl:+.2}%)",
            lamports_to_sol(expected)
        );
        
        let taken = curve_sol_out(&position.market, amount);
        position.paper_sol_in_curve = position.paper_sol_in_curve.saturating_sub(taken);
        if let Market::PumpFun(m) = &mut position.market {
            m.reserves.virtual_sol_reserves = m.reserves.virtual_sol_reserves.saturating_sub(taken);
            m.reserves.real_sol_reserves = m.reserves.real_sol_reserves.saturating_sub(taken);
            m.reserves.virtual_token_reserves += amount;
            m.reserves.real_token_reserves += amount;
        }
        position.paper_token_amount = Some(balance - amount);
        return Ok(true);
    }

    let mut instructions = sell_instructions_cached(market, &trading_wallet(), amount, min_out)?;
    if partial {
        let token_account =
            get_associated_token_address(&trading_wallet(), &mint, &market.token_program());
        keep_token_account(&mut instructions, &token_account);
    }
    // 2026-10-11: through the hot lanes when they run (presigned, warm sockets, no HTTP client);
    // else the slow path's services, as before. The landing is awaited the same way.
    // HOT_SELL_FANOUT (2026-10-12): one copy per lane on one durable nonce, re-bid every slot,
    // cancelled if none lands; its outcome replaces the wait below.
    let fanout = if !TRADE_SETTING.dry_run && crate::hot::sell::fanout_on() {
        crate::hot::sell::fanout_sell(&mint, instructions.clone()).await.unwrap_or_else(|e| {
            warn!("hot sell fan-out: {e:#}; the other path sends");
            None
        })
    } else {
        None
    };
    if let Some(f) = &fanout {
        note_stuck(&mint, &f.outcome, f.fees_lamports);
        info!(
            "sell {mint} (fan-out): {} after {} re-bid(s), last fee {} micro-lamports/CU{}",
            match &f.outcome {
                ConfirmOutcome::Confirmed { .. } => "landed".to_string(),
                ConfirmOutcome::Failed { error, .. } => format!("landed and failed ({error})"),
                ConfirmOutcome::TimedOut => "not landed, cancelled".to_string(),
            },
            f.rebids,
            f.cu_price,
            f.lane.map(|l| format!(", lane {} (tip {} lamports)", crate::hot::lanes::LANE_NAMES[l], f.tip)).unwrap_or_default()
        );
    }
    let hot = if TRADE_SETTING.dry_run {
        None
    } else if let Some(f) = &fanout {
        Some(f.signature)
    } else {
        crate::hot::sell::send(&mint, instructions.clone()).unwrap_or_else(|e| {
            warn!("hot sell: {e:#}; the slow path sends");
            None
        })
    };
    let signature = match hot {
        Some(signature) => signature,
        None => match submit_with(SendProfile::Sell, instructions, None).await? {
        Submitted::Sent { signature, .. } => signature,
        Submitted::Simulated { error, .. } => {
            bump(if error.is_none() {
                &STATS.sells_ok
            } else {
                &STATS.sells_failed
            });
            match &error {
                None => activity(
                    ActivityKind::Sold,
                    Some(dex_name),
                    Some(&mint_text),
                    format!("Simulated sell ({})", reason.code()),
                ),
                Some(e) => activity(
                    ActivityKind::SellFailed,
                    Some(dex_name),
                    Some(&mint_text),
                    format!("Simulated sell failed: {e}"),
                ),
            }
            record.status = format!(
                "simulated{}",
                error.map(|e| format!(": {e}")).unwrap_or_default()
            );
            log_trade(&record);
            return Ok(true);
        }
        },
    };
    record.signature = signature.to_string();

    let (outcome, fill) = match fanout {
        Some(f) => (f.outcome, f.fill),
        None => await_landing(&signature, SELL_CONFIRM_TIMEOUT).await?,
    };
    record.wallet_sol_change = match (fill, &outcome) {
        (Some(fill), _) => Some(fill.wallet_lamports as f64 / 1e9),
        (None, ConfirmOutcome::TimedOut) => None,
        (None, _) => fee_payer_lamport_change(&RPC_CLIENT, &signature)
            .await
            .map(|l| l as f64 / 1e9)
            .ok(),
    };
    bump(if matches!(outcome, ConfirmOutcome::Confirmed { .. }) {
        &STATS.sells_ok
    } else {
        &STATS.sells_failed
    });
    record.status = match &outcome {
        ConfirmOutcome::Confirmed { .. } => "confirmed".into(),
        ConfirmOutcome::Failed { error, .. } => format!("failed: {error}"),
        ConfirmOutcome::TimedOut => "timeout".into(),
    };
    log_trade(&record);

    match &outcome {
        ConfirmOutcome::Confirmed { .. } => activity(
            ActivityKind::Sold,
            Some(dex_name),
            Some(&mint_text),
            format!(
                "Sold {what} ({}){}",
                reason.code(),
                record
                    .wallet_sol_change
                    .map(|v| format!(", wallet {v:+.6} SOL"))
                    .unwrap_or_default()
            ),
        ),
        ConfirmOutcome::Failed { error, .. } => activity(
            ActivityKind::SellFailed,
            Some(dex_name),
            Some(&mint_text),
            format!("Sell failed on chain: {error}"),
        ),
        ConfirmOutcome::TimedOut => activity(
            ActivityKind::SellFailed,
            Some(dex_name),
            Some(&mint_text),
            "Sell not confirmed in time",
        ),
    }
    match outcome {
        ConfirmOutcome::Confirmed { .. } => {
            info!("{} {mint} ({signature})", "sold".green().bold());
            position.known_tokens = Some(balance - amount);
            Ok(true)
        }
        ConfirmOutcome::Failed { error, .. } if error.contains(STALE_ACCOUNTS) => {
            warn!("sell {signature} failed on chain: {error}: the creator's account changed; retrying at once with the creator read from chain");
            Err(StaleAccounts.into())
        }
        ConfirmOutcome::Failed { error, .. } => {
            warn!("sell {signature} failed on chain: {error}");
            Ok(false)
        }
        ConfirmOutcome::TimedOut => {
            warn!("sell {signature} not confirmed within {SELL_CONFIRM_TIMEOUT:?}");
            
            position.known_tokens = None;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STUCK (the owner's rule): three fresh sells failing with the same lasting error stop the
    /// selling; temporary failures never count; a different error starts the count again; fees over
    /// the tokens' worth stop it too.
    #[test]
    fn a_coin_that_cannot_be_sold_is_stuck() {
        let failed = |e: &str| ConfirmOutcome::Failed { slot: 1, error: e.into() };
        let complete = "InstructionError(4, Custom(6005))";
        let mut w = StuckWatch::default();
        for _ in 0..5 {
            w.note(&failed("InstructionError(4, Custom(6003))"), 10_000);
            w.note(&ConfirmOutcome::TimedOut, 5_600);
            w.note(&failed(STALE_ACCOUNTS), 10_000);
        }
        assert_eq!(w.verdict(Some(10_000_000)), None, "slippage, not landing, the creator moving: temporary");
        w.note(&failed(complete), 10_000);
        w.note(&failed(complete), 10_000);
        assert_eq!(w.verdict(Some(10_000_000)), None, "two");
        w.note(&failed("InstructionError(4, Custom(6017))"), 10_000);
        w.note(&failed(complete), 10_000);
        w.note(&failed(complete), 10_000);
        assert_eq!(w.verdict(Some(10_000_000)), None, "a different error in between starts again");
        w.note(&ConfirmOutcome::TimedOut, 5_600);
        w.note(&failed(complete), 10_000);
        assert!(w.verdict(Some(10_000_000)).unwrap().contains("3 fresh sells failed with the same error"));
        // Fees over the worth.
        let mut w = StuckWatch::default();
        w.note(&ConfirmOutcome::TimedOut, 5_600);
        assert_eq!(w.verdict(Some(10_000)), None);
        w.note(&ConfirmOutcome::TimedOut, 5_600);
        assert!(w.verdict(Some(10_000)).unwrap().contains("more than its tokens are worth"));
        assert_eq!(w.verdict(None), None, "worth unknown: not on fees");
    }

    /// HOT_SELL_FANOUT: the sell built once and patched equals the sell built fresh for the same
    /// amounts (the fee recipients are a random valid pick on every build: checked as such).
    #[test]
    fn a_patched_sell_template_is_the_fresh_sell() {
        use crate::instructions::pumpfun_buy_ix::{pumpfun_config, set_test_config, PumpFunMarket};
        set_test_config();
        let config = pumpfun_config().unwrap();
        let wallet = Pubkey::new_unique();
        for cashback in [false, true] {
            let mut m = PumpFunMarket::new(Pubkey::new_unique(), crate::utils::utils::TOKEN_PROGRAM_ID, Pubkey::new_unique(), false, Default::default());
            m.is_cashback_coin = cashback;
            let market = Market::PumpFun(m);
            let base = market.sell_instructions(&wallet, 0, 0).unwrap();
            for (amount, min_out) in [(1u64, 0u64), (123_456_789, 42), (u64::MAX, u64::MAX)] {
                let mut patched = base.clone();
                patch_sell_amounts(&mut patched, amount, min_out);
                let fresh = market.sell_instructions(&wallet, amount, min_out).unwrap();
                assert_eq!(patched.len(), fresh.len());
                for (a, b) in patched.iter().zip(&fresh) {
                    assert_eq!((a.program_id, &a.data), (b.program_id, &b.data), "amounts {amount} {min_out}");
                    assert_eq!(a.accounts.len(), b.accounts.len());
                    for (i, (x, y)) in a.accounts.iter().zip(&b.accounts).enumerate() {
                        if x.pubkey != y.pubkey {
                            assert!(config.fee_recipients.contains(&x.pubkey) || config.buyback_fee_recipients.contains(&x.pubkey), "account {i} differs and is not a fee recipient");
                        }
                        assert_eq!((x.is_signer, x.is_writable), (y.is_signer, y.is_writable));
                    }
                }
            }
        }
    }

    /// The reference wallet's exit as measured on him: NO_POP at the entry, the first sell at any
    /// price (the bot ran without both from 30 Sep to 7 Oct).
    fn reference_tuning() -> Tuning {
        Tuning { patient: None, no_pop_percent: Some(0.0), first_up_percent: None, ..Tuning::default() }
    }

    fn reference(bet: u64) -> RefExit {
        RefExit::with_tuning(bet, reference_tuning())
    }

    const ENTRY: u64 = 1_000_000;

    fn tick(ms: u64, x: f64, behind_sol: f64) -> Tick {
        Tick {
            held_for: Duration::from_millis(ms),
            entry_price: ENTRY,
            price: (ENTRY as f64 * x) as u64,
            behind_lamports: lamports(behind_sol),
            behind_sell_lamports: 0,
            last_second: Flow::default(),
            recent_buy_lamports: 0,
            recent_sell_lamports_3s: 0,
            since_last_buy: None,
            left: 1.0,
            window_net_lamports: 0,
            in_wait_window: false,
            insider_sold: false,
        }
    }

    /// pXGQ's exit at the research's settings.
    fn px() -> PxExit {
        PxExit::new(PxSettings::default())
    }

    fn net(t: Tick, buy_sol: f64, sell_sol: f64) -> Tick {
        Tick { behind_lamports: lamports(buy_sol), behind_sell_lamports: lamports(sell_sol), ..t }
    }

    #[test]
    fn px_sells_everything_when_no_wave_comes() {
        let mut e = px();
        // Inside the first 2 s nothing is judged on the wave.
        assert_eq!(e.evaluate(&net(tick(1_000, 0.98, 0.0), 0.0, 0.0)), Action::Hold);
        assert_eq!(e.evaluate(&net(tick(3_000, 0.98, 0.0), 0.4, 0.0)), Action::SellAll(SellReason::NoWave));
        // A wave of 2 SOL net: held.
        let mut e = px();
        assert_eq!(e.evaluate(&net(tick(3_000, 1.02, 0.0), 2.0, 0.0)), Action::Hold);
        // After 8 s the wave rule is over.
        let mut e = px();
        assert_eq!(e.evaluate(&net(tick(9_000, 1.0, 0.0), 0.0, 0.0)), Action::Hold);
    }

    #[test]
    fn px_scales_out_once_then_trails_the_rest() {
        let mut e = px();
        let up = |ms, x| net(tick(ms, x, 0.0), 3.0, 0.0);
        assert_eq!(e.evaluate(&up(4_000, 1.30)), Action::SellShare(0.5, SellReason::Scale));
        // The scale-out has not landed: asked again.
        assert_eq!(e.evaluate(&up(4_100, 1.30)), Action::SellShare(0.5, SellReason::Scale));
        e.sold();
        // Half sold at x1.30; the rest at x1.50: whole position (0.65 + 0.75) = x1.40.
        assert_eq!(e.evaluate(&Tick { left: 0.5, ..up(10_000, 1.50) }), Action::Hold);
        assert!((e.value_peak().0 - 1.40).abs() < 1e-9, "{:?}", e.value_peak());
        // 20% under the x1.40 peak is x1.12: the rest at x0.94 (0.65 + 0.47) goes.
        assert_eq!(e.evaluate(&Tick { left: 0.5, ..up(12_000, 0.95) }), Action::Hold);
        assert_eq!(e.evaluate(&Tick { left: 0.5, ..up(13_000, 0.90) }), Action::SellAll(SellReason::Trail));
    }

    #[test]
    fn px_dumps_a_roll_over_in_profit_and_stops_a_loser() {
        let mut e = px();
        let t = |ms, x| net(tick(ms, x, 0.0), 3.0, 0.0);
        assert_eq!(e.evaluate(&t(9_000, 1.20)), Action::Hold);
        // 15% off x1.20 is x1.02: still at cost or better: everything.
        assert_eq!(e.evaluate(&t(20_000, 1.01)), Action::SellAll(SellReason::Dump));
        let mut e = px();
        assert_eq!(e.evaluate(&t(9_000, 0.90)), Action::Hold);
        assert_eq!(e.evaluate(&t(10_000, 0.85)), Action::SellAll(SellReason::Stop));
        // Under x0.95 with 4 SOL sold in 3 s: everything.
        let mut e = px();
        assert_eq!(e.evaluate(&Tick { recent_sell_lamports_3s: lamports(4.0), ..t(9_000, 0.93) }), Action::SellAll(SellReason::Pressure));
        // The cap.
        let mut e = px();
        assert_eq!(e.evaluate(&t(600_000, 1.1)), Action::SellAll(SellReason::Lifetime));
    }

    /// The exit the bot runs since 30 Sep: the reference wallet's, without NO_POP and with the
    /// first sell only on a rise.
    /// The owner's step rule of 30 Sep: no NO_POP, the first sell only on a rise, 4 SOL.
    fn live(bet: u64) -> RefExit {
        RefExit::with_tuning(
            bet,
            Tuning { patient: None, no_pop_percent: None, first_up_percent: Some(0.0), first_all_up_sol: Some(4.0), ..Tuning::default() },
        )
    }

    #[test]
    fn without_no_pop_a_coin_under_the_entry_is_held_to_the_time_limit() {
        let at = (rules::CHECK_SECS * 1000.0) as u64;
        let mut e = live(lamports(1.0));
        assert_eq!(e.evaluate(&tick(at, 0.95, 0.0)), Action::Hold, "no NO_POP at the check");
        assert_eq!(e.evaluate(&tick(20_000, 0.70, 0.0)), Action::Hold);
        let end = (rules::MAX_HOLD_SECS * 1000.0) as u64;
        assert_eq!(e.evaluate(&tick(end, 0.70, 0.0)), Action::SellAll(SellReason::Lifetime));
    }

    #[test]
    fn the_first_sell_waits_until_the_price_is_up() {
        let at = (rules::CHECK_SECS * 1000.0) as u64;
        // Buyers behind the fill before the check, price under the entry: hold (the reference
        // exit would sell its first slice).
        let mut e = live(lamports(1.0));
        assert_eq!(e.evaluate(&tick(500, 0.97, 2.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(600, 1.0, 2.0)), Action::Hold, "at the entry is not up");
        assert_eq!(e.evaluate(&tick(700, 1.05, 2.0)), Action::SellShare(rules::FIRST_SELL_SHARE, SellReason::First));
        // The check's "buyers, no run": all out only when up.
        let mut e = live(lamports(1.0));
        assert_eq!(e.evaluate(&tick(at, 0.97, 1.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(at + 100, 1.10, 1.0)), Action::SellAll(SellReason::Check));
        // The curve up 4+ SOL in the first slots: all out only when the price is up too.
        let mut e = live(lamports(1.0));
        let window = |x, sol: f64| Tick { window_net_lamports: (sol * 1e9) as i64, ..tick(2_000, x, 0.0) };
        assert_eq!(e.evaluate(&window(0.9, 5.0)), Action::Hold);
        assert_eq!(e.evaluate(&window(1.2, 5.0)), Action::SellAll(SellReason::FirstAll));
    }

    #[test]
    fn a_high_inside_the_first_sell_window_counts_for_the_wait_at_the_high() {
        let wait = Tuning { first_all_up_sol: Some(6.0), first_wait_slots: 4, ..reference_tuning() };
        let mut e = RefExit::with_tuning(lamports(1.0), wait);
        // The price spikes to x1.5 while the first sell waits out its slots.
        assert_eq!(e.evaluate(&Tick { in_wait_window: true, ..tick(300, 1.5, 0.5) }), Action::Hold);
        // After the window, buyers behind the fill: the first slice at x1.2.
        assert_eq!(e.evaluate(&tick(1_300, 1.2, 2.0)), Action::SellShare(rules::FIRST_SELL_SHARE, SellReason::First));
        e.sold((ENTRY as f64 * 1.2) as u64, Duration::from_millis(1_400));
        // At the next due sell x1.2 is 20% under the high: not "at the high", so no wait.
        assert_eq!(e.evaluate(&tick(3_500, 1.2, 2.0)), Action::SellShare(rules::STEP_SHARE, SellReason::Step));
    }

    #[test]
    fn the_first_sell_waits_for_the_curve_in_the_first_slots() {
        let wait = Tuning { first_all_up_sol: Some(6.0), first_wait_slots: 4, ..reference_tuning() };
        let e = RefExit::with_tuning(lamports(1.0), wait);
        let at = |sol: f64, ms: u64, in_window: bool| Tick {
            window_net_lamports: (sol * 1e9) as i64,
            in_wait_window: in_window,
            ..tick(ms, 1.6, sol.max(0.0))
        };
        // Inside the window: no 30% slice on buyers behind the fill, everything once up 6 SOL.
        assert_eq!(e.clone().evaluate(&at(4.9, 100, true)), Action::Hold);
        assert_eq!(e.clone().evaluate(&at(6.2, 1_600, true)), Action::SellAll(SellReason::FirstAll));
        // Buys less sells: 8 SOL bought and 3 sold is up 5.
        assert_eq!(e.clone().evaluate(&at(5.0, 1_600, true)), Action::Hold);
        // After the window without 6 SOL: the usual first sell.
        assert_eq!(e.clone().evaluate(&at(4.9, 1_700, false)), Action::SellShare(rules::FIRST_SELL_SHARE, SellReason::First));
        // A window reached 6 SOL but the tick came after it: still everything.
        assert_eq!(e.clone().evaluate(&at(7.0, 1_800, false)), Action::SellAll(SellReason::FirstAll));
        // Only the first sell: after it the window does not matter.
        let mut later = e.clone();
        later.sold(ENTRY, Duration::from_millis(1_700));
        assert_eq!(later.evaluate(&at(7.0, 1_900, false)), Action::Hold);
        let off = Tuning { first_all_up_sol: None, ..wait };
        assert_eq!(RefExit::with_tuning(lamports(1.0), off).evaluate(&at(4.9, 100, true)), Action::SellShare(rules::FIRST_SELL_SHARE, SellReason::First));
    }

    #[test]
    fn price_driven_variants_sell_everything_when_down() {
        // Off by default: the first trigger slices even 10% under the entry.
        let mut e = reference(lamports(1.0));
        assert_eq!(e.evaluate(&tick(500, 0.9, 1.2)), Action::SellShare(rules::FIRST_SELL_SHARE, SellReason::First));
        let down = Tuning { first_down_percent: Some(5.0), step_down_percent: Some(5.0), close_out_share: 0.35, ..reference_tuning() };
        let mut e = RefExit::with_tuning(lamports(1.0), down);
        assert_eq!(e.evaluate(&tick(500, 0.9, 1.2)), Action::SellAll(SellReason::Down));
        // A later step 10% under the entry sells everything; one above it steps, and 35% left closes.
        let mut e = RefExit::with_tuning(lamports(1.0), down);
        e.sold(ENTRY, Duration::from_millis(500));
        assert_eq!(e.evaluate(&tick(1_000, 1.3, 1.2)), Action::Hold, "not due; sets the high");
        assert_eq!(e.evaluate(&tick(2_600, 0.9, 1.2)), Action::SellAll(SellReason::Down));
        assert_eq!(e.evaluate(&tick(2_600, 1.1, 1.2)), Action::SellShare(rules::STEP_SHARE, SellReason::Step));
        assert_eq!(e.evaluate(&Tick { left: 0.3, ..tick(2_600, 1.1, 1.2) }), Action::SellAll(SellReason::CloseOut));
    }

    #[test]
    fn a_faded_buying_wave_sells_the_rest_when_switched_on() {
        let fade = Tuning { fade_buy_sol: Some(1.0), fade_quiet_secs: 0.75, ..reference_tuning() };
        let mut e = RefExit::with_tuning(lamports(3.0), fade);
        e.sold(ENTRY, Duration::from_millis(0));
        assert_eq!(e.evaluate(&tick(1_000, 1.3, 5.0)), Action::Hold, "not due; sets the high");
        let wave = |since_ms: u64, sol: f64| Tick {
            recent_buy_lamports: lamports(sol),
            since_last_buy: Some(Duration::from_millis(since_ms)),
            ..tick(2_000, 1.1, 5.0)
        };
        assert_eq!(e.clone().evaluate(&wave(800, 1.5)), Action::SellAll(SellReason::Fade), "wave stopped 0.8 s ago");
        assert_eq!(e.clone().evaluate(&wave(300, 1.5)), Action::SellShare(rules::STEP_SHARE, SellReason::Step), "still buying");
        assert_eq!(e.clone().evaluate(&wave(800, 0.5)), Action::SellShare(rules::STEP_SHARE, SellReason::Step), "no real wave");
        // Off by default.
        let mut d = reference(lamports(3.0));
        d.sold(ENTRY, Duration::from_millis(0));
        assert_eq!(d.evaluate(&tick(1_000, 1.3, 5.0)), Action::Hold);
        assert_eq!(d.evaluate(&wave(800, 1.5)), Action::SellShare(rules::STEP_SHARE, SellReason::Step));
    }

    #[test]
    fn first_slice_sells_into_buyers_behind_the_fill() {
        let mut e = reference(lamports(3.0));
        assert_eq!(e.evaluate(&tick(100, 1.02, 1.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(150, 1.02, 1.1)), Action::SellShare(0.30, SellReason::First));
    }

    #[test]
    fn first_slice_also_sells_on_a_16_percent_pop() {
        let mut e = reference(lamports(3.0));
        assert_eq!(e.evaluate(&tick(500, 1.159, 0.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(500, 1.16, 0.0)), Action::SellShare(0.30, SellReason::First));
    }

    #[test]
    fn the_check_picks_the_branch() {
        let at = (rules::CHECK_SECS * 1000.0) as u64;
        let flat = 1.0 + reference_tuning().no_pop_percent.expect("NO_POP in the reference exit") / 100.0 - 0.01;
        let mut e = reference(lamports(3.0));
        assert_eq!(e.evaluate(&tick(at - 1, flat, 0.5)), Action::Hold);
        assert_eq!(e.evaluate(&tick(at, flat, 0.5)), Action::SellAll(SellReason::NoPop));
        let mut e = reference(lamports(3.0));
        assert_eq!(e.evaluate(&tick(at, 1.10, 1.0)), Action::SellAll(SellReason::Check));
        let mut e = reference(lamports(3.0));
        assert_eq!(e.evaluate(&tick(at, 1.30, 5.0)), Action::SellShare(0.30, SellReason::First), "buyers and a run");
        // quiet and up: hold, then react to what comes
        let mut e = RefExit::with_tuning(lamports(3.0), Tuning { no_pop_hold_high_percent: Some(3.0), ..reference_tuning() });
        assert_eq!(e.evaluate(&tick(at, 1.10, 0.5)), Action::Hold);
        // It touched +10% before falling back: no NO_POP (NO_POP_HOLD_HIGH_PERCENT at 3%), the rest of the exit runs.
        assert_eq!(e.clone().evaluate(&tick(at + 1_000, flat, 0.5)), Action::Hold, "fell back after a pop");
        assert_eq!(e.clone().evaluate(&tick(at + 1_000, 1.12, 1.0)), Action::SellAll(SellReason::Check), "buyers, no run");
        assert_eq!(e.clone().evaluate(&tick(at + 1_000, 1.16, 0.5)), Action::SellShare(0.30, SellReason::First), "popped");
        assert_eq!(e.evaluate(&tick(at + 3_000, 1.10, 0.9)), Action::Hold);
        // +4% is up (the no-pop line is under it): quiet and up holds.
        let mut e = reference(lamports(3.0));
        assert_eq!(e.evaluate(&tick(at, 1.04, 0.5)), Action::Hold);
    }

    #[test]
    fn the_patient_exit_takes_profit_waits_out_dips_and_stops_late() {
        let patient = Tuning {
            patient: Some(Patient { take_profit_percent: 50.0, stop_after_secs: 30.0, stop_percent: Some(0.0), max_hold_secs: 60.0 }),
            ..reference_tuning()
        };
        let mut e = RefExit::with_tuning(lamports(0.1), patient);
        // A dip before 30 s is waited out, buyers or not; no slices, no NO_POP.
        assert_eq!(e.evaluate(&tick(2_000, 0.80, 0.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(10_000, 1.30, 3.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(12_000, 1.50, 3.0)), Action::SellAll(SellReason::TakeProfit));
        // From 30 s under the entry: out.
        let mut e = RefExit::with_tuning(lamports(0.1), patient);
        assert_eq!(e.evaluate(&tick(29_900, 0.95, 0.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(30_000, 0.95, 0.0)), Action::SellAll(SellReason::Down));
        // Above the entry but short of the take profit: until the longest hold.
        let mut e = RefExit::with_tuning(lamports(0.1), patient);
        assert_eq!(e.evaluate(&tick(45_000, 1.20, 0.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(60_000, 1.20, 0.0)), Action::SellAll(SellReason::Lifetime));
    }

    #[test]
    fn no_pop_waits_for_a_coin_that_touched_the_hold_high() {
        let at = (rules::CHECK_SECS * 1000.0) as u64;
        let hold = Tuning { no_pop_percent: Some(0.0), no_pop_hold_high_percent: Some(3.0), no_pop_stop_percent: Some(20.0), ..reference_tuning() };
        // Never above +2%: under the entry at the check is NO_POP.
        let mut e = RefExit::with_tuning(lamports(1.0), hold);
        assert_eq!(e.evaluate(&tick(500, 1.02, 0.2)), Action::Hold);
        assert_eq!(e.evaluate(&tick(at, 0.98, 0.2)), Action::SellAll(SellReason::NoPop));
        // Touched +4% first: the same price holds, until the stop.
        let mut e = RefExit::with_tuning(lamports(1.0), hold);
        assert_eq!(e.evaluate(&tick(500, 1.04, 0.2)), Action::Hold);
        assert_eq!(e.evaluate(&tick(at, 0.98, 0.2)), Action::Hold);
        assert_eq!(e.evaluate(&tick(at + 500, 0.79, 0.2)), Action::SellAll(SellReason::NoPop), "through the 20% stop");
        // Between the no-pop line and the old +3% it is not NO_POP any more.
        let mut e = RefExit::with_tuning(lamports(1.0), Tuning { no_pop_hold_high_percent: None, ..hold });
        assert_eq!(e.evaluate(&tick(at, 1.02, 0.2)), Action::Hold);
    }

    #[test]
    fn an_insider_sell_turns_the_first_slice_into_everything_and_ends_later_steps() {
        let out = |t: Tick| Tick { insider_sold: true, ..t };
        let on = Tuning { insider_exit: true, check_secs: 4.0, ..reference_tuning() };
        // Before the first sell is due an insider's sell alone does nothing.
        let mut e = RefExit::with_tuning(lamports(1.0), on);
        assert_eq!(e.evaluate(&out(tick(500, 1.02, 0.5))), Action::Hold);
        // The first-sell trigger with an insider out: everything, not 30%.
        assert_eq!(e.evaluate(&out(tick(600, 1.05, 1.2))), Action::SellAll(SellReason::Insider));
        // Without one: the usual slice. Then an insider sells before the next step is due: the rest.
        let mut e = RefExit::with_tuning(lamports(1.0), on);
        assert_eq!(e.evaluate(&tick(600, 1.05, 1.2)), Action::SellShare(rules::FIRST_SELL_SHARE, SellReason::First));
        e.sold(ENTRY, Duration::from_millis(900));
        assert_eq!(e.evaluate(&tick(1_200, 1.2, 1.2)), Action::Hold);
        assert_eq!(e.evaluate(&out(tick(1_300, 1.2, 1.2))), Action::SellAll(SellReason::Insider));
        // At the check: NO_POP and CHECK already sell everything.
        let mut e = RefExit::with_tuning(lamports(1.0), on);
        assert_eq!(e.evaluate(&out(tick(4_000, 0.99, 0.2))), Action::SellAll(SellReason::NoPop));
        // Off (the bot's default since 27 Sep): slices as before.
        let mut e = reference(lamports(1.0));
        assert_eq!(e.evaluate(&out(tick(600, 1.05, 1.2))), Action::SellShare(rules::FIRST_SELL_SHARE, SellReason::First));
        e.sold(ENTRY, Duration::from_millis(900));
        assert_eq!(e.evaluate(&out(tick(1_300, 1.2, 1.2))), Action::Hold);
    }

    #[test]
    fn slices_follow_the_clock_from_each_landed_sell() {
        let mut e = reference(lamports(3.0));
        e.sold(ENTRY, Duration::from_millis(300));
        // The price is off its high (1.2), so the clock alone decides.
        assert_eq!(e.evaluate(&tick(1_000, 1.2, 5.0)), Action::Hold);
        // after the first slice: 2 s
        assert_eq!(e.evaluate(&tick(2_299, 1.0, 5.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(2_300, 1.0, 5.0)), Action::SellShare(0.33, SellReason::Step));
        e.sold(ENTRY, Duration::from_millis(2_600));
        // after later slices: 1 s
        assert_eq!(e.evaluate(&tick(3_599, 1.0, 5.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(3_600, 1.0, 5.0)), Action::SellShare(0.33, SellReason::Step));
    }

    #[test]
    fn busy_ticks_wait_one_second_up_to_max_waits() {
        let mut e = reference(lamports(3.0));
        e.sold(ENTRY, Duration::from_millis(0));
        e.sold(ENTRY, Duration::from_millis(2_000));
        assert_eq!(e.evaluate(&tick(2_500, 1.2, 5.0)), Action::Hold, "not due; sets the high");
        let mut busy = tick(3_000, 1.0, 5.0);
        busy.last_second = Flow { buy_lamports: lamports(1.0), ..Default::default() };
        busy.recent_buy_lamports = lamports(1.0);
        for wait in 0..rules::MAX_WAITS {
            busy.held_for = Duration::from_millis(3_000 + 1_000 * wait as u64);
            assert_eq!(e.evaluate(&busy), Action::Hold, "wait {}", wait + 1);
            busy.held_for += Duration::from_millis(500);
            assert_eq!(e.evaluate(&busy), Action::Hold, "not due between ticks");
        }
        busy.held_for = Duration::from_millis(3_000 + 1_000 * rules::MAX_WAITS as u64);
        assert_eq!(e.evaluate(&busy), Action::SellShare(0.33, SellReason::Step), "sells once the waits are used up");
    }

    #[test]
    fn sellers_and_a_price_at_its_high_wait_but_a_drop_does_not() {
        let mut e = reference(lamports(3.0));
        e.sold(ENTRY, Duration::from_millis(0));
        e.sold(ENTRY, Duration::from_millis(2_000));
        assert_eq!(e.evaluate(&tick(2_500, 1.2, 5.0)), Action::Hold, "not due; sets the high");
        let mut t = tick(3_000, 1.0, 5.0);
        t.last_second = Flow { sell_lamports: lamports(1.0), ..Default::default() };
        assert_eq!(e.clone().evaluate(&t), Action::Hold, "1 SOL sold");
        t.last_second = Flow { buys: 30, buy_lamports: lamports(0.99), sell_lamports: lamports(0.99), ..Default::default() };
        assert_eq!(e.clone().evaluate(&t), Action::SellShare(0.33, SellReason::Step), "many small buys do not wait");
        // A drop under the last sell does not wait: with nobody buying for 2 s it sells everything,
        // with buyers around it steps.
        assert_eq!(e.clone().evaluate(&tick(3_000, 0.8, 5.0)), Action::SellAll(SellReason::Down));
        let buyers = Tick { recent_buy_lamports: lamports(0.5), ..tick(3_000, 0.8, 5.0) };
        assert_eq!(e.clone().evaluate(&buyers), Action::SellShare(0.33, SellReason::Step));
        // Within 2% of the high since the buy: wait; further under it: sell.
        assert_eq!(e.clone().evaluate(&tick(3_000, 1.181, 5.0)), Action::Hold);
        assert_eq!(e.evaluate(&tick(3_000, 1.17, 5.0)), Action::SellShare(0.33, SellReason::Step));
    }

    #[test]
    fn big_bets_step_in_quarters() {
        let mut e = reference(lamports(5.0));
        e.sold(ENTRY, Duration::from_millis(0));
        assert_eq!(e.evaluate(&tick(1_000, 1.2, 0.0)), Action::Hold, "not due; sets the high");
        assert_eq!(e.evaluate(&tick(2_000, 1.0, 0.0)), Action::SellShare(0.25, SellReason::Step));
    }

    #[test]
    fn close_out_when_ten_percent_or_less_is_left() {
        let mut e = reference(lamports(3.0));
        e.sold(ENTRY, Duration::from_millis(0));
        e.sold(ENTRY, Duration::from_millis(2_000));
        let mut t = tick(3_000, 1.0, 5.0);
        t.left = 0.10;
        assert_eq!(e.evaluate(&t), Action::SellAll(SellReason::CloseOut));
    }

    #[test]
    fn everything_goes_after_forty_seconds() {
        let mut e = reference(lamports(3.0));
        e.sold(ENTRY, Duration::from_millis(0));
        assert_eq!(e.evaluate(&tick(39_999, 2.0, 9.0)).eq(&Action::SellAll(SellReason::Lifetime)), false);
        assert_eq!(e.evaluate(&tick(40_000, 2.0, 9.0)), Action::SellAll(SellReason::Lifetime));
    }

    /// The snipe test's exit: +30% at once, else all out at 15 s; nothing in between, whatever the
    /// price does.
    #[test]
    fn the_snipe_test_exit_is_take_profit_or_time() {
        let t = |ms: u64, x: f64| Tick {
            held_for: Duration::from_millis(ms),
            entry_price: 1_000_000,
            price: (1_000_000.0 * x) as u64,
            behind_lamports: 0,
            behind_sell_lamports: 0,
            last_second: Flow::default(),
            recent_buy_lamports: 0,
            recent_sell_lamports_3s: 0,
            since_last_buy: None,
            left: 1.0,
            window_net_lamports: 0,
            in_wait_window: false,
            insider_sold: true,
        };
        let mut e = TestExit::default();
        assert_eq!(e.evaluate(&t(500, 1.29)), Action::Hold);
        assert_eq!(e.evaluate(&t(900, 0.30)), Action::Hold, "no stop-loss");
        assert_eq!(e.evaluate(&t(1_000, 1.30)), Action::SellAll(SellReason::TakeProfit));
        assert_eq!(e.evaluate(&t(14_999, 1.10)), Action::Hold, "no trailing, no insider exit");
        assert_eq!(e.evaluate(&t(15_000, 1.10)), Action::SellAll(SellReason::Time));
        assert_eq!(SellReason::Time.code(), "TIME");
        // The whale rule's: +23% or 90 s, nothing in between.
        let mut w = TestExit::whale();
        assert_eq!(w.evaluate(&t(30_000, 1.229)), Action::Hold);
        assert_eq!(w.evaluate(&t(30_000, 1.23)), Action::SellAll(SellReason::TakeProfit));
        assert_eq!(w.evaluate(&t(89_999, 0.40)), Action::Hold, "no stop: a loser is sold on the clock");
        assert_eq!(w.evaluate(&t(90_000, 0.40)), Action::SellAll(SellReason::Time));
        assert_eq!(SellReason::TakeProfit.code(), "TP");
    }

}
