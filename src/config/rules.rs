//! Trading rules, measured on the reference wallet 8RvtT8189KpAq5MkXGVHFn6LdZpxq9PkvDeAvs1g5Yj4
//! (pump.fun, 8-23 September 2026: 813 tokens, 86% won, +517 SOL). The rules live here, not in
//! `.env`: `.env` holds credentials, connections and the stake per buy only.
//!
//! Entry: follow the deployers he trades, with his hard lines on the launch itself.
//! Buy: at the launch, sized by the dev buy, with his tight price guard.
//! Sell: a first slice into the buyers behind the fill, a check 2 s after the buy, then slices
//! on a clock that restarts when each sell lands, skipped while buyers keep arriving.

use {crate::config::Dex, solana_pubkey::Pubkey};

/// The wallet whose deployers the bot follows.
pub const REFERENCE_WALLET: Pubkey =
    solana_pubkey::pubkey!("8RvtT8189KpAq5MkXGVHFn6LdZpxq9PkvDeAvs1g5Yj4");
/// His trades per deployer and coin creator (`unix key lamports_delta`), seeded from the study
/// and appended with every trade of his the stream shows.
pub const REFERENCE_TRADES_FILE: &str = "reference_deployers.txt";

/// The rules are measured on pump.fun launches only.
pub const DEXES: &[Dex] = &[Dex::PumpFun];

// ---- entry ---------------------------------------------------------------------------------
/// He buys a deployer again when he traded it within this window (19% of such launches,
/// against 1.25% overall)...
pub const FOLLOW_RECENT_SECS: u64 = 41 * 3600;
/// ...or when the deployer made him at least this much over his history with it.
pub const FOLLOW_MIN_PROFIT_SOL: f64 = 0.77;
/// Dev buy in the create transaction: 92% of his attempts at 0.7 SOL or more. At most 2 SOL since
/// 2026-10-07 (the owner's yes, paper first; it was 20, 99% of his attempts): replayed with the
/// bot's real costs and its live landing positions (26 Sep), followed launches with a 2-20 SOL dev
/// buy lost money and 5+ SOL ones lost 6-13% a try even 100 positions in, while 0.7-2 SOL ones made
/// +4.4% a try on 25-27 Sep (90% interval +3.2..+5.5%, 0.5 SOL stake, with bundles of at most one
/// wallet) and +2.0% on 8-22 Sep. research/strategy-2026-10-07. Back to 20 SOL: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const DEV_BUY_MIN_SOL: f64 = 0.7;
pub const DEV_BUY_MAX_SOL: f64 = 20.0;
/// SOL the deployer held before the create: 89% of his attempts at 2 SOL or more.
pub const DEPLOYER_MIN_SOL: f64 = 2.0;
/// Deployer wallet age: 97% of his attempts are on wallets older than 24 h.
pub const DEPLOYER_MIN_AGE_SECS: u64 = 24 * 3600;
/// He never buys mayhem-mode launches.
pub const SKIP_MAYHEM: bool = true;

// ---- buy -----------------------------------------------------------------------------------
/// He fills in the launch slot (87%) or the next (13%), never later.
pub const MAX_LAUNCH_AGE_SLOTS: u64 = 1;
/// Dev buys under this get the smaller bet (his 1.9 SOL against 2.93 SOL).
pub const SMALL_BET_DEV_BUY_SOL: f64 = 1.0;
pub const SMALL_BET_SHARE: f64 = 0.65;
/// The buy fails when the price moved more than this past the quote (taken after the create's
/// bundle) before it lands. 1.25% until 29 Sep, the reference wallet's cap (1.0125x the quote):
/// about 0.2 SOL bought ahead of the bot moves a fresh curve that far, so it bought only when
/// nobody got in first, and lost 206 of 245 live races. 8% since 29 Sep (the owner's call): replay
/// of the launches the entry keeps (25-29 Sep, the bot's 47 real landing positions, current exit,
/// sells a slot late, 1 SOL): fills 43% of attempts against 34%, +13.8% a fill against +16.7%,
/// +5.92% an attempt against +5.63%; the added fills (behind 0.2-1.4 SOL of snipers) made about
/// +3.1% each. The replay has run about twice as kind to late fills as live, and on paper on 25
/// Sep buys behind a fast sniper went 1 win, 11 losses (BUY_AHEAD_MAX_SOL below): check the fills
/// that pass 1.25% only in live results. 13% on 30 Sep evening (the owner's call), back to 8% on 1
/// Oct: live, 22 of 30 buys lost the race at 13% anyway (a median 4.8 SOL landed ahead of them),
/// and with the step exit the extra fills a wider guard lets in lose: replay of the kept launches
/// (25-30 Sep, real landings, every sell's tip paid) +0.41 SOL a day at 8% against +0.31 at 13%,
/// +0.20 at 20%, -0.28 with no guard. With a one-sell exit about 20% did best (+1.03 to +1.11).
/// 1.25% again: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
/// 5% since 2026-10-14 (the owner's call, for the Frankfurt paper and live test; Germany stays 1.25%).
pub const BUY_SLIPPAGE_PERCENT: f64 = 5.0;
/// The buy accepts one fast buyer ahead of it: at most this much SOL put into the curve by others
/// (after fees) on top of the dev's bundle, which is already in the quote. The owner's rule (25
/// Sep). A single fast buyer puts in a median 2.93 SOL (p75 2.96: "3 SOL" less the fee). Replayed
/// on the 346 launches the entry rules pass, current exit: filled behind one buyer +7.3% a trade,
/// behind two -0.7%; this cap takes 80% of the one-buyer cases (+7.1%) and a third of the
/// two-buyer ones (+1.1%). None: the BUY_SLIPPAGE_PERCENT price guard instead.
/// Off since 25 Sep 22:00: live on paper (19:39-21:50) the buys it let in behind snipers went 1
/// win, 11 losses (-1.37 SOL) while the buys with nobody ahead made +2.37 SOL; a SOL cap also let
/// in fills behind 4 and 13 small snipers.
pub const BUY_AHEAD_MAX_SOL: Option<f64> = None;

/// Why a buy that met more than its guard allows was refused.
pub fn buy_guard_text() -> String {
    match BUY_AHEAD_MAX_SOL {
        Some(sol) => format!("more than {sol} SOL bought ahead of us"),
        None => format!("price moved past the {BUY_SLIPPAGE_PERCENT}% buy guard"),
    }
}

// ---- sell ----------------------------------------------------------------------------------
/// First slice: 30% once 1.1 SOL of other wallets' buys land behind the fill, or the price is
/// 16% over it (82% / 75% right on held-out days).
pub const FIRST_SELL_BEHIND_SOL: f64 = 1.1;
pub const FIRST_SELL_PRICE_PERCENT: f64 = 16.0;
pub const FIRST_SELL_SHARE: f64 = 0.30;
/// The first sell of a position, whatever triggers it (FIRST_ALL_UP_SOL, buyers behind, the pop,
/// the check's buyers), only while the price is more than this (%) over the entry, after fees;
/// until then the bot holds (MAX_HOLD_SECS still applies). None: at any price. The owner's rule
/// (30 Sep). Off: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const FIRST_SELL_MIN_UP_PERCENT: Option<f64> = None;
/// From 2 s after the buy with no sell yet: under +5% -> sell everything (75% of such cases);
/// buyers behind (1 SOL+) without a big run (under +30%) -> sell everything (57-67%); buyers
/// and a run -> the first slice; a quiet launch that is up -> hold (53%) and keep checking.
/// 4 s was tried on paper 26-27 Sep (with INSIDER_EXIT) and removed: on that night's 82 paper
/// fills it replayed +2.43 SOL against +3.35 at 2 s; quiet coins dumped between 2 and 4 s
/// (3GEomWYC -2.5% at 2 s, -46% at 4 s). 1.5 s and +3% since 29 Sep, with FIRST_ALL_UP_SOL 4:
/// replayed on the 653 launches the creator rules keep (25-29 Sep, sells a slot late) the three
/// together gave +4.52% an attempt at the bot's real landings against +4.33%, and helped most
/// where the bot is late: top of the next slot +1.46% against +0.50%, next-slot landings -0.85%
/// against -1.88%; better on the held-out days (+4.38% against +4.06%) and on all 4,348
/// followed launches (+2.20% against +2.00%).
///
/// NO_POP under the entry (0%, after fees) and never once the high since the buy reached +3%,
/// since 29 Sep evening. After a NO_POP sell at +3% a third of the coins rose 30%+ within 3
/// minutes, most of them ones that had touched +5% before the check (holding on those: +0.20 SOL
/// a trade on 309); the median coin still fell (-14% a minute later, -25% at 5 minutes), and
/// holding every one lost more (-239 against -219 SOL). Replay of the 4,749 followed launches of
/// 25-29 Sep (sells a slot late, 1 SOL): on the 602 the entry keeps, +28.5 SOL at the bot's real
/// landings against +26.4, next slot +7.4 against +6.3, next-slot landings -6.2 against -7.2,
/// held-out days +15.4 against +13.1; on all 4,749 next slot -24.2 against -32.1. No NO_POP at all
/// did best on the kept launches' landings (+30.0) but lost -52.8 at the next slot on all of them;
/// NO_POP as a stop under the entry (-3% to -30%) was worse than both; holding on while buyers
/// came in changed nothing (CHECK sells those).
///
/// NO_POP off since 30 Sep (the owner's rule): no sell at the check for being under the entry;
/// a coin that has not popped is held until the first sell comes due with the price up (see
/// FIRST_SELL_MIN_UP_PERCENT) or MAX_HOLD_SECS. Was Some(0.0): all out under the entry.
///
/// 2 s and all out under +5% again: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const CHECK_SECS: f64 = 2.0;
pub const CHECK_NO_POP_PERCENT: Option<f64> = Some(5.0);
/// NO_POP waits while the coin's high since the buy reached this rise (%), or other wallets
/// bought this much SOL behind the fill, until the price falls NO_POP_STOP_PERCENT under the
/// entry (None: off). Off: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const NO_POP_HOLD_HIGH_PERCENT: Option<f64> = None;
pub const NO_POP_HOLD_BEHIND_SOL: Option<f64> = None;
pub const NO_POP_STOP_PERCENT: Option<f64> = None;
pub const CHECK_BEHIND_SOL: f64 = 1.0;
pub const CHECK_RUN_PERCENT: f64 = 30.0;
/// Clock: the next check comes this long after the previous sell landed (2 s after the first).
pub const FIRST_STEP_SECS: f64 = 2.0;
pub const STEP_SECS: f64 = 1.0;
/// Each later slice: 33% of what is left (25% on bets of 5 SOL or more).
pub const STEP_SHARE: f64 = 0.33;
pub const BIG_BET_SOL: f64 = 5.0;
pub const BIG_BET_STEP_SHARE: f64 = 0.25;
/// Wait one more step (at most twice in a row) while the coin is running: the price within 2% of
/// its high since the buy, or other wallets bought 1+ SOL or sold 1+ SOL in the last second. No
/// wait on a drop, and no trailing stop. Measured on the 18,894 slots he held (landed sells only):
/// at his ~1 s tick he sells 49% of the time with no buys in the last second, 22% after 1-5 SOL
/// bought and 12% after 5+; 16% at the high against 32-35% under it; 40% under his entry; his
/// chance of selling is the same (15-19% a slot) at the high and 35% under it. Replay of his 749
/// positions: first-tick sells 57% (his 63%), result within 1% of the previous buy-count wait.
pub const WAIT_HIGH_PERCENT: Option<f64> = Some(2.0);
pub const WAIT_BUY_SOL: Option<f64> = Some(1.0);
pub const WAIT_SELL_SOL: Option<f64> = Some(1.0);
/// The previous wait's buy count and drop (off).
pub const WAIT_BUYS: Option<u32> = None;
pub const WAIT_DROP_PERCENT: Option<f64> = None;
/// Up to 4 one-second waits per step while the coin runs. His history: at his first tick after a
/// sell he sells 18% of the time while the price is rising against 32% when not (he holds about
/// one tick), and steady rises rarely last past 3 s. Replay: 4 waits beat 1, 2, 3 and 6 overall
/// (his 749 positions +348 SOL vs +340 with 2; the bot's fills with the current entry +17.0% vs
/// +15.0% a trade on the Sep sample, about equal on 24 Sep), and match his hold times (last sell
/// p90 21 s, his 20 s).
pub const MAX_WAITS: u32 = 4;
/// Sell the rest when a buying wave just stopped: others bought this much SOL in the last two
/// seconds but nobody for FADE_QUIET_SECS (None: off). With more than 9% left he sells everything
/// 48% of the time in that spot, 25% while buyers still arrive and 10% with no buyers at all.
pub const FADE_BUY_SOL: Option<f64> = None;
pub const FADE_QUIET_SECS: f64 = 0.75;
/// Trailing stop after the first sell (None: off), whether only while waiting, and whether it
/// sells everything or one step.
pub const TRAIL_PERCENT: Option<f64> = None;
pub const TRAIL_ONLY_WAITING: bool = true;
pub const TRAIL_SELLS_ALL: bool = true;
/// Sell everything once 10% or less of the tokens is left.
pub const CLOSE_OUT_SHARE: f64 = 0.10;
/// The price already under the entry when the first-sell trigger fires: sell everything, not a
/// slice (None keeps slicing).
pub const FIRST_DOWN_PERCENT: Option<f64> = None;
/// The price under the entry when a later sell comes due: sell everything (None keeps stepping).
pub const STEP_DOWN_PERCENT: Option<f64> = None;
/// Still falling with nobody buying: when a later sell comes due under the previous sell's price
/// and nobody bought in the last two seconds, sell everything instead of stepping down. With
/// buyers around, keep stepping (a bounce is likely). Replayed on the bot's own fills (Sep sample
/// and 24 Sep, fills behind the bundle and one slot later): better than stepping in 7 of 8
/// cases, e.g. +5.7% -> +6.7% a trade on the 302 affected dev-buy launches; selling everything
/// on any drop, buyers or not, was worse (stepping recovers the bounces).
pub const STEP_FALL_PERCENT: Option<f64> = Some(0.0);
/// "Nobody bought": less than this much SOL bought by others in the last two seconds.
pub const DOWN_QUIET_BUY_SOL: Option<f64> = Some(0.01);
/// His longest hold was 42 s.
pub const MAX_HOLD_SECS: f64 = 40.0;
/// The patient exit (`utils::auto_sell::Patient`) in place of every sell rule above (None: the
/// reference wallet's exit). Since 29 Sep: everything at +50% over the entry, from 30 s everything
/// once under the entry, everything at 60 s. Why: the reference wallet's exit cuts at 1.5-2 s a
/// coin that has not popped, and many of those rise later (after a NO_POP sell 16-24% of the coins
/// were back in profit between 5 and 30 s), while the winners get sold in slices into the first
/// buyers. Replay of the launches the entry keeps (25-29 Sep, the bot's 47 real landing positions
/// within the 8% guard, 0.1 SOL, sells a slot late), against the old exit: +15.5% a fill against
/// +12.3%, +6.66% an attempt against +5.29%, top of the next slot +3.6% against +2.8%, next-slot
/// landings +1.3% against +0.1%, held-out days (28-29 Sep) +15.5% against +11.2%. The price: bigger
/// losing trades (worst tenth of fills -21% or worse against -6%, 5% of fills lose over 30%
/// against 1%) and fewer winners. +30% and +100% take profits, a stop from 20 s, a 20% stop and no
/// stop were all tried and did no better.
///
/// Since 30 Sep: +100% and 180 s (were +50% and 60 s). The coins that grow peak late: of the
/// launches the entry keeps (25-30 Sep, study `grow_study.py`), those that reach +50% within 3
/// min peak at a median 57 s, those that reach x3 at 110 s, and 54% of the +50% ones go on to
/// x2; 5 minutes in they are back under the entry. Replay by this code (`rules_check dump`) at
/// the bot's real landings, 0.1 SOL, 8% guard, tips paid: +2.36 mSOL a try against +2.16 on
/// 25-28 Sep and +2.12 against +1.89 on 29-30 Sep (held out). Nearby rules (x2 with a 150 s
/// hold, the stop from 45-60 s) did about as well; trailing stops, a take profit at x1.5 or x3,
/// and stops before 30 s (under 0.8-0.9 at 5-15 s) did worse on one half or both.
///
/// Off on 30 Sep (the owner's step rule: the exit above without NO_POP, the first sell only on a
/// rise), back on since 1 Oct as one sell a coin, the owner's choice: everything at +50%, from 30 s
/// everything once under the entry, everything at 60 s. Replay of the kept launches (25-30 Sep, with
/// the dump-risk check, the bot's real landings, 8% guard, 0.1 SOL, every sell's tip and fee
/// paid): +0.94 SOL a day against +0.41 for the step rule (x2 / 180 s +1.02, x3 / 180 s +1.03),
/// and positive whatever the snipers bought ahead (+1.2 to +30.1 mSOL a fill, where the step rule
/// lost -4.1 and -0.5 on the lightly sniped launches it fills most); on the 43 coins tried live on
/// 30 Sep it stayed even (+0.03 SOL) where x2 and x3 lost (-0.15, -0.25). `None`: the step exit.
/// Off (the step exit): the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const PATIENT_EXIT: Option<crate::utils::auto_sell::Patient> = None;
pub const PATIENT_SETTINGS: crate::utils::auto_sell::Patient = crate::utils::auto_sell::Patient {
    take_profit_percent: 50.0,
    stop_after_secs: 30.0,
    stop_percent: Some(0.0),
    max_hold_secs: 60.0,
};
/// Insiders out, we're out (off): once the first-sell trigger has fired, sell everything as soon
/// as the dev or a wallet of the create's bundle (the buys right behind the create in its slot)
/// has sold anything, instead of slicing. Tried on paper 26-27 Sep with a 4 s check and removed
/// by the owner: it cut winners short (8r7mgjhW +0.60 -> +0.02 SOL). Replays, new vs old rules:
/// night of 25-26 Sep +9.7% vs +9.3% of stake, 26 Sep evening +3.3% vs -4.6% (the sample it was
/// chosen on), 8-23 Sep +27.7% vs +28.8%, 26-27 Sep paper fills +1.94 vs +3.35 SOL (82 fills;
/// the exit alone at 2 s: +2.84).
pub const INSIDER_EXIT: bool = false;
/// The first sell waits through the buy's slot and the next FIRST_WAIT_SLOTS: if the curve gains
/// FIRST_ALL_UP_SOL in them (every other wallet's buys less sells since our fill, however many
/// wallets), sell everything; if not, the usual first sell after them. The owner's rule (25 Sep).
/// Replayed: on the bot's own entries (fill behind the create's bundle) +67.2 -> +77.8 SOL, the
/// best of every variant tried (all out at +8 SOL any time: +68.6); on his 749 positions +348.2 ->
/// +301.8 SOL; on 25 Sep's paper buys +0.16 -> +0.58 SOL (SQQQ -0.08 -> +0.33). None: no wait,
/// the usual first sell. 4 SOL since 29 Sep (see CHECK_SECS): of 3, 4, 5, 6 and 10 SOL, 4 did best
/// on fills later than the bundle (6 SOL: +0.50% at the next slot, 4 SOL: +0.98% on its own).
/// 6 SOL again: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const FIRST_ALL_UP_SOL: Option<f64> = Some(6.0);
pub const FIRST_WAIT_SLOTS: u64 = 4;
/// Fast-rug profile, tagged in the log and on the dashboard only: the dev dumps within 10 s on
/// 73% of launches with launch-tool metadata or a 3-10 SOL dev buy (19% of the rest). It does
/// not change the exit: on the replay of his 749 positions a 4 s cap on these cut the result
/// from +356 to +326 SOL, because the exit above is usually out before the dev sells.
pub const FAST_RUG_HOSTS: &[&str] = &["j7tracker", "uxento", "rapidlaunch", "pump.mypinata"];
pub const FAST_RUG_DEV_BUY_MIN_SOL: f64 = 3.0;
pub const FAST_RUG_DEV_BUY_MAX_SOL: f64 = 10.0;

// ---- safety (not measured on him) ----------------------------------------------------------
/// Price guard on sells, so exits land in a falling curve.
pub const SELL_SLIPPAGE_PERCENT: f64 = 20.0;
pub const MAX_OPEN_POSITIONS: usize = 1;

/// The open-positions limit: `MAX_OPEN_POSITIONS` in the environment, else the constant above.
/// pXGQ's exit holds longer than the bot's 40 s (median 11 s, 90th percentile 268 s on the
/// research replay), so a second position avoids missing launches while one is held.
pub fn max_open_positions() -> usize {
    static N: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("MAX_OPEN_POSITIONS").ok().and_then(|v| v.trim().parse().ok()).filter(|n: &usize| *n >= 1).unwrap_or(MAX_OPEN_POSITIONS)
    });
    *N
}
/// Kept back from every buy for fees, tips and rent.
pub const BALANCE_RESERVE_SOL: f64 = 0.01;
/// How often an open position is re-checked (it also wakes on every trade on its curve).
pub const EXIT_TICK_MS: u64 = 100;

pub fn lamports(sol: f64) -> u64 {
    (sol * 1e9).round() as u64
}

/// Why the bot follows a launch's deployer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    /// He traded this deployer or creator within `FOLLOW_RECENT_SECS`.
    Recent,
    /// Over his history with it, it made him at least `FOLLOW_MIN_PROFIT_SOL`.
    Profitable,
}

/// What the bot knows about a pump.fun launch when it sees the create.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaunchFacts {
    pub mayhem: bool,
    /// SOL of the first buy in the create transaction.
    pub dev_buy_lamports: u64,
    /// SOL the create's fee payer held before the transaction.
    pub deployer_lamports: Option<u64>,
    pub follow: Option<Follow>,
    /// The reference wallet's SOL result over the launch's deployer and creator (lamports), when
    /// he has traded either (`utils::reference::profit`).
    pub reference_profit_lamports: Option<i64>,
    /// The reference wallet's trades on the launch's deployer and creator, when he has traded
    /// either (`utils::reference::trades`).
    pub reference_trades: Option<u32>,
    /// SOL the insiders put into the curve: the dev buy and, when the early stream showed it, the
    /// create's bundle (the curve's real SOL after them).
    pub insider_lamports: Option<u64>,
    /// The create's metadata URI is on a host in DUMP_RISK_HOSTS (None: no URI read).
    pub risky_host: Option<bool>,
    /// Seconds since the same deployer's previous launch, when the bot saw one.
    pub secs_since_previous_launch: Option<u64>,
    /// The same deployer's launches in the 24 h before this one, when the bot has its history.
    pub launches_last_day: Option<u32>,
    /// Wallets in the create's bundle besides the dev, when the bot saw the bundle.
    pub bundle_wallets: Option<u32>,
    /// The coin's name is in capitals only (`name_is_all_caps`), when the bot read the name.
    pub name_all_caps: Option<bool>,
    /// The deployer's recent launches the bot judged (`utils::deployer_record`): (launches, of
    /// which NO_POP for a buy right behind the bundle).
    pub deployer_record: (u32, u32),
    /// What only the scorecard reads (`config::scorecard`).
    pub scoring: Scoring,
}

/// The scorecard's facts that `LaunchFacts` has no other field for.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Scoring {
    /// The deployer's launches in the hour before this one.
    pub launches_last_hour: Option<u32>,
    /// The deployer wallet's first transaction: unix time, and the SOL it received
    /// (`utils::deployer_wallet`), when known.
    pub wallet_first_bt: Option<i64>,
    pub funded_sol: Option<f64>,
    /// The coin creator's earlier launches (`utils::launch_results`): count, mean result of a 0.5 SOL
    /// buy behind the bundle (SOL), share that went down.
    pub earlier: Option<(u32, f64, f64)>,
    /// SOL the create's bundle (besides the dev) put into the curve.
    pub bundle_lamports: Option<u64>,
    /// Unix time of the decision.
    pub now: u64,
}

/// The scorecard's facts from what the bot knows (`config::scorecard::ScoreFacts`), with missing
/// values as the training data had them.
pub fn score_facts(f: &LaunchFacts) -> crate::config::scorecard::ScoreFacts {
    let s = &f.scoring;
    let (launches, no_pops) = f.deployer_record;
    let (earlier_pnl, earlier_down) = match s.earlier {
        Some((n, pnl, down)) if n > 0 => (pnl, down),
        _ => (-9.0, -9.0),
    };
    [
        f.secs_since_previous_launch.map_or(1e7, |x| x as f64),
        f.launches_last_day.unwrap_or(0) as f64,
        s.launches_last_hour.unwrap_or(0) as f64,
        f.reference_profit_lamports.unwrap_or(0) as f64 / 1e9,
        f.reference_trades.unwrap_or(0) as f64,
        if launches == 0 { -9.0 } else { no_pops as f64 / launches as f64 },
        earlier_pnl,
        earlier_down,
        f.dev_buy_lamports as f64 / 1e9,
        crate::config::scorecard::dev_percent(f.dev_buy_lamports),
        f.bundle_wallets.unwrap_or(0) as f64,
        s.bundle_lamports.unwrap_or(0) as f64 / 1e9,
        f.deployer_lamports.map_or(f64::NEG_INFINITY, |l| l as f64 / 1e9),
        s.funded_sol.unwrap_or(f64::NEG_INFINITY),
        s.wallet_first_bt.map_or(f64::NEG_INFINITY, |t| (s.now as f64 - t as f64) / 3600.0),
    ]
}

/// The launch's score.
pub fn launch_score(f: &LaunchFacts) -> i32 {
    crate::config::scorecard::score(&score_facts(f))
}

/// Skip a deployer whose recent launches mostly got no buyers: of its last NO_POP_RECORD_LAST
/// launches seen within NO_POP_RECORD_DAYS (at least NO_POP_RECORD_MIN_LAUNCHES), this share or
/// more would have been sold NO_POP by a buy right behind the create's bundle. The bot wins the
/// race most easily where nobody else wants the coin, so these fill far more often than others.
/// Replay of the 1,005 launches the bot attempted 25-27 Sep (each judged only on the deployer's
/// earlier ones), fill at the top of the next slot within the 1.25% guard: the 98 skipped launches
/// (10%) were 56 of the 277 fills, 80% of them NO_POP, -1.26 SOL; the rest +26.0 SOL with 20%
/// NO_POP (32% before). On the 84 paper fills of 26-27 Sep night: 20 skipped, 12 of the 24
/// NO_POP sells among them, -1.23 SOL. The owner's call (27 Sep).
pub const NO_POP_RECORD_MIN_LAUNCHES: u32 = 2;
pub const NO_POP_RECORD_SHARE: f64 = 0.5;
pub const NO_POP_RECORD_LAST: usize = 10;
pub const NO_POP_RECORD_DAYS: u64 = 7;

/// The no-buyers record check (`NO_POP_RECORD_*`).
pub fn no_pop_record_verdict(record: (u32, u32)) -> Result<(), String> {
    if no_pop_record_rejects(record) {
        let (launches, no_pops) = record;
        return Err(format!(
            "the deployer's recent launches got no buyers: {no_pops} of its last {launches} would have been sold NO_POP"
        ));
    }
    Ok(())
}

/// `no_pop_record_verdict` without the message.
#[inline]
pub fn no_pop_record_rejects((launches, no_pops): (u32, u32)) -> bool {
    launches >= NO_POP_RECORD_MIN_LAUNCHES && no_pops as f64 >= NO_POP_RECORD_SHARE * launches as f64
}

/// Good creators do not spam: skip a launch when the same deployer launched another coin less
/// than this long ago, or launched more than DEPLOYER_MAX_LAUNCHES_24H coins in the 24 h before
/// it (`utils::creator_history`). The bot cannot win the launch-slot race (live 26-29 Sep: 33 of
/// 259 buys filled, 206 lost to the price guard), so the entry keeps the launches a late fill
/// still pays on. Replay of the 4,348 followed launches of 25-29 Sep with the current exit (sells
/// a slot late), 1 SOL: at the bot's 47 real landing positions within the 1.25% guard the kept
/// 722 made +4.5% an attempt against +2.9% for the 3,318 the old 5-minute rule kept, and at the
/// top of the next slot +12.2 SOL against -6.8 (landings in the next slot -5.2 against -67.8
/// SOL). Held out (28-29 Sep, chosen on 25-27): +1.3 SOL against -17.0 at the next slot. Stable
/// around the cut: 3-15 launches and 5-60 min all gave +3.0% to +4.3% an attempt. Creator and
/// bundle features beyond these (bundle size, SOL, share, tips, who paid) did not hold out.
///
/// Loosened 30 Sep (20 min and 7 a day before): live, the pair left 3 tries in an hour (46
/// followed launches, 21 relaunches and 12 over the pace). Replay of 25-29 Sep with the patient
/// exit, 0.1 SOL, 8% guard, tips 0.0031 SOL a fill: 20 min / 7 a day 4.7 tries an hour, +16.5% a
/// fill, +0.56 SOL a day after tips; 10 min / no pace limit 12.9 tries an hour, +7.9% a fill,
/// +0.63 SOL a day, positive on each of the five days (5 min: 16.1, +6.7%, +0.62).
/// 5 min again: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const RELAUNCH_SKIP_SECS: u64 = 300;
/// None: off.
pub const DEPLOYER_MAX_LAUNCHES_24H: Option<u32> = None;

/// Skip a launch whose create came with a bundle of more than this many wallets besides the dev
/// (the buyers of the buys that reach the preprocessed stream within its bundle window; None:
/// off). The owner's rule (29 Sep). It costs on the bot's 40 s trades: replay of the 791
/// launches the creator rules kept (25-29 Sep, current exit), the 189 with 3+ bundle wallets made
/// +0.048 SOL an attempt at the bot's real landings (+0.026 at the next slot) against +0.044
/// (+0.010) for the rest, together +9.1 SOL of +35.5. Held longer they fall more: 5 minutes after
/// a NO_POP sell the price was a median -34% to -40% with 3+ bundle wallets, -11% with none.
/// Launches seen only on the regular stream carry no bundle count and are not judged by it.
///
/// Off for one live hour on 1 Oct, back on the same day (the owner's choice): the replay said the
/// launches only this rule removed made money (+1.0 and +1.8 mSOL a try, 25-28 / 29 Sep), but the
/// first live trade it let in, HoFg6fED (3 bundle wallets, 6 SOL behind a 3.95 SOL dev buy), lost
/// 40% to the insiders' dump. Turned off again only after a paper test.
///
/// One wallet since 2026-10-07 (the owner's yes, paper first; it was two): replayed with the bot's
/// real costs (priority fee, tips, sell tips) at its live landing positions, 0.7-20 SOL dev-buy
/// launches with exactly one bundle wallet made +6.8% / +9.8% a try 100 positions in (8-22 / 25-27
/// Sep) and 3+ wallets lost (-1.4% / -3.0%); with the 0.7-2 SOL dev-buy line, see DEV_BUY_MAX_SOL.
/// This differs from the replay above, which counted no tips or sell costs.
/// Off: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const MAX_BUNDLE_WALLETS: Option<u32> = None;

/// Skip a launch whose deployer or creator lost the reference wallet money: his SOL result over
/// their coins (his trades so far, an open buy counting as spent) is below zero. Followed
/// launches the entry kept, 25-30 Sep (study `grow_study.py`), the bot's exit replayed at its
/// real landings, 0.1 SOL, 8% guard, tips paid: these lost 6.4 mSOL a try on 25-28 Sep (77
/// tries) and 3.6 on 29-30 Sep (17), where the rest made +2.9 and +2.1; about 60% of them never
/// rose 10% after the entry, against 36% of all. Off: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const SKIP_REFERENCE_LOSERS: bool = false;

/// Dump risk: a launch whose price the insiders are likely to sell down in the first 30 s (dead or
/// down 15%+ half a minute after the bot's usual landing). One point each, and a launch with
/// DUMP_RISK_SKIP_POINTS or more is skipped (None: off); an input the bot does not have scores no
/// point:
///   the insiders (dev buy and create bundle) put DUMP_RISK_INSIDER_SOL+ into the curve,
///   the metadata is on a DUMP_RISK_HOSTS host,
///   the creator launched another coin within DUMP_RISK_RELAUNCH_SECS,
///   the create's fee payer held DUMP_RISK_CREATOR_SOL+,
///   the reference wallet traded the creator fewer than DUMP_RISK_REFERENCE_TRADES times.
/// The owner's request (30 Sep): no coins that nobody buys after the bot and that go down. Study
/// `dead_study.py`, followed launches the entry keeps, 25-30 Sep: outside buyers put under 3 SOL
/// in the 30 s after the entry on 23% of them (-5 to -13 mSOL a try) and the price was 15%+ down
/// on 28-34%, mostly the insiders selling (dev buy plus bundle 6+ SOL: 48-55% down, under 2 SOL:
/// 4-11%; j7tracker metadata 49-51%, ipfs.io 8-15%; creator 100+ SOL 41-42%, under 10 SOL
/// 18-23%). Skipping 3+ points, replayed at the bot's real landings (0.1 SOL, 8% guard, tips):
/// with the owner's exit +0.45 SOL a day against +0.20 on 25-28 Sep and +0.58 against +0.30 on
/// 29-30 Sep; with the patient exit +1.10 against +0.96 and +0.87 against +0.75; tries 9-11 an
/// hour, 15%+ down 25% and 17% (were 34% and 28%). Skipping 2+ points lost on the patient exit.
/// Off: the 28 Sep rules restored on 2026-10-07 (the owner's instruction, paper only).
pub const DUMP_RISK_SKIP_POINTS: Option<u32> = None;
pub const DUMP_RISK_INSIDER_SOL: f64 = 6.0;
pub const DUMP_RISK_HOSTS: &[&str] = &["j7tracker"];
pub const DUMP_RISK_RELAUNCH_SECS: u64 = 3600;
pub const DUMP_RISK_CREATOR_SOL: f64 = 100.0;
pub const DUMP_RISK_REFERENCE_TRADES: u32 = 10;

/// Whether a metadata host is one of DUMP_RISK_HOSTS.
pub fn risky_host(host: &str) -> bool {
    let host = host.to_lowercase();
    DUMP_RISK_HOSTS.iter().any(|h| host.contains(h))
}

/// What each dump-risk point is for, in `dump_risk_flags`' bit order.
const DUMP_RISK_LABELS: [&str; 5] = [
    "insiders 6+ SOL in",
    "j7tracker metadata",
    "relaunch within 1 h",
    "creator 100+ SOL",
    "reference traded the creator under 10 times",
];

/// The dump-risk points of a launch as bits (`DUMP_RISK_LABELS` order). Allocation-free.
#[inline]
pub fn dump_risk_flags(f: &LaunchFacts) -> u8 {
    let mut bits = 0u8;
    if f.insider_lamports.is_some_and(|l| l as f64 / 1e9 >= DUMP_RISK_INSIDER_SOL) {
        bits |= 1;
    }
    if f.risky_host == Some(true) {
        bits |= 2;
    }
    if f.secs_since_previous_launch.is_some_and(|s| s < DUMP_RISK_RELAUNCH_SECS) {
        bits |= 4;
    }
    if f.deployer_lamports.is_some_and(|l| l as f64 / 1e9 >= DUMP_RISK_CREATOR_SOL) {
        bits |= 8;
    }
    if f.follow.is_some() && f.reference_trades.unwrap_or(0) < DUMP_RISK_REFERENCE_TRADES {
        bits |= 16;
    }
    bits
}

/// The dump-risk points of a launch and what scored them.
pub fn dump_risk(f: &LaunchFacts) -> (u32, Vec<&'static str>) {
    let bits = dump_risk_flags(f);
    let why: Vec<&'static str> = DUMP_RISK_LABELS.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0).map(|(_, l)| *l).collect();
    (why.len() as u32, why)
}

/// Skip a launch whose name is written in capitals only (letters present, none lowercase: "PEPE
/// 2.0", "$WIF"). The owner's rule (29 Sep), from how long coins live: of the 4,594 followed
/// launches of 25-29 Sep, 1.2% of the all-caps ones were still trading at or above the launch
/// price 30 minutes later (3.1% of all) and 57% had no trade after 5 minutes (41% of all).
/// Replay on the launches the creator rules keep (current exit, sells a slot late, 1 SOL): at the
/// top of the next slot +20.0 SOL without them against +16.0, next-slot landings +2.2 against
/// -3.1, held-out days (28-29 Sep) +7.8 against +4.7; at the bot's real landings +34.3 against
/// +37.1 (they paid only on the earliest fills).
///
/// Off since 1 Oct (the owner's choice): with the dump-risk check on, the launches only this rule
/// removed (17-27 a day) made +1.8 and +1.5 mSOL a try (same replay as MAX_BUNDLE_WALLETS); both
/// off: +0.40 / +0.79 SOL a day against +0.30 / +0.58, about 60% more tries.
pub const SKIP_ALL_CAPS_NAMES: bool = false;

/// A name in capitals only: some letter, and none lowercase (Python's `str.isupper`).
pub fn name_is_all_caps(name: &str) -> bool {
    name.chars().any(char::is_uppercase) && !name.chars().any(char::is_lowercase)
}

/// Why the entry rules refused a launch, without the message (`describe` writes it). The hot path
/// decides on these codes; the messages come later, off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reject {
    Mayhem = 1,
    AllCaps,
    NotFollowed,
    ReferenceLost,
    DevBuyOutside,
    Relaunch,
    TooManyLaunches,
    BigBundle,
    DumpRisk,
    NoPopRecord,
    DeployerPoor,
    BalanceUnknown,
    /// Under the scorecard's threshold (`config::scorecard`).
    LowScore,
}

impl Reject {
    /// The skip reason as the log and the dashboard show it, for the facts it was decided on.
    pub fn describe(self, f: &LaunchFacts) -> String {
        match self {
            Reject::Mayhem => "mayhem-mode launch".into(),
            Reject::AllCaps => "name in capitals only".into(),
            Reject::NotFollowed => "deployer not followed: the reference wallet has not traded it in 41 h and it never made him 0.77 SOL".into(),
            Reject::ReferenceLost => {
                let lost = f.reference_profit_lamports.unwrap_or(0);
                format!("the reference wallet lost {:.2} SOL on this deployer's coins", -lost as f64 / 1e9)
            }
            Reject::DevBuyOutside => {
                let dev = f.dev_buy_lamports as f64 / 1e9;
                format!("dev buy {dev:.2} SOL outside {DEV_BUY_MIN_SOL}-{DEV_BUY_MAX_SOL} SOL")
            }
            Reject::Relaunch => format!(
                "deployer launched another coin {} s ago (relaunches within {} min are skipped)",
                f.secs_since_previous_launch.unwrap_or(0),
                RELAUNCH_SKIP_SECS / 60
            ),
            Reject::TooManyLaunches => format!(
                "deployer launched {} coins in the last 24 h (more than {} are skipped)",
                f.launches_last_day.unwrap_or(0),
                DEPLOYER_MAX_LAUNCHES_24H.unwrap_or(0)
            ),
            Reject::BigBundle => format!(
                "{} wallets in the create's bundle (more than {} are skipped)",
                f.bundle_wallets.unwrap_or(0),
                MAX_BUNDLE_WALLETS.unwrap_or(0)
            ),
            Reject::DumpRisk => {
                let (points, why) = dump_risk(f);
                format!("dump risk {points} points ({})", why.join(", "))
            }
            Reject::NoPopRecord => {
                let (launches, no_pops) = f.deployer_record;
                format!("the deployer's recent launches got no buyers: {no_pops} of its last {launches} would have been sold NO_POP")
            }
            Reject::DeployerPoor => format!(
                "deployer held {:.2} SOL before the create (minimum {DEPLOYER_MIN_SOL})",
                f.deployer_lamports.unwrap_or(0) as f64 / 1e9
            ),
            Reject::BalanceUnknown => "deployer balance before the create unknown".into(),
            Reject::LowScore => {
                let facts = score_facts(f);
                format!(
                    "score {} under {} (lowest: {})",
                    crate::config::scorecard::score(&facts),
                    crate::config::scorecard::min(),
                    crate::config::scorecard::worst(&facts, 3)
                )
            }
        }
    }
}

/// The entry rules that need no RPC. The deployer's wallet age is checked afterwards.
pub fn launch_verdict(f: &LaunchFacts) -> Result<Follow, String> {
    launch_verdict_code(f).map_err(|r| r.describe(f))
}

/// `launch_verdict` without the message. Allocation-free.
#[inline]
pub fn launch_verdict_code(f: &LaunchFacts) -> Result<Follow, Reject> {
    launch_verdict_code_with(f, card())
}

/// The scorecard's threshold when it is on.
#[inline]
fn card() -> Option<i32> {
    crate::config::scorecard::enabled().then(crate::config::scorecard::min)
}

/// `launch_verdict_code` with the scorecard's threshold given (None: off).
#[inline]
pub fn launch_verdict_code_with(f: &LaunchFacts, card: Option<i32>) -> Result<Follow, Reject> {
    let follow = verdict_code_before_balance_with(f, card)?;
    if card.is_some() {
        // The score needs the balance; any balance is allowed (it is one of the facts).
        return if f.deployer_lamports.is_none() { Err(Reject::BalanceUnknown) } else { Ok(follow) };
    }
    deployer_balance_code(f.deployer_lamports)?;
    Ok(follow)
}

/// Every entry rule except the deployer's balance, which a shred sighting fetches afterwards.
pub fn verdict_before_balance(f: &LaunchFacts) -> Result<Follow, String> {
    verdict_code_before_balance(f).map_err(|r| r.describe(f))
}

/// `verdict_before_balance` without the message. Allocation-free.
#[inline]
pub fn verdict_code_before_balance(f: &LaunchFacts) -> Result<Follow, Reject> {
    verdict_code_before_balance_with(f, card())
}

/// `verdict_code_before_balance` with the scorecard's threshold given (None: off).
#[inline]
pub fn verdict_code_before_balance_with(f: &LaunchFacts, card: Option<i32>) -> Result<Follow, Reject> {
    if SKIP_MAYHEM && f.mayhem {
        return Err(Reject::Mayhem);
    }
    if SKIP_ALL_CAPS_NAMES && f.name_all_caps == Some(true) {
        return Err(Reject::AllCaps);
    }
    let Some(follow) = f.follow else {
        return Err(Reject::NotFollowed);
    };
    if let Some(min) = card {
        // The score replaces the rules below. Without the balance it is decided once the balance
        // is in (`launch_verdict_code`), as the minimum-balance rule was.
        if f.deployer_lamports.is_some() && launch_score(f) < min {
            return Err(Reject::LowScore);
        }
        return Ok(follow);
    }
    if f.reference_profit_lamports.is_some_and(|p| SKIP_REFERENCE_LOSERS && p < 0) {
        return Err(Reject::ReferenceLost);
    }
    let dev = f.dev_buy_lamports as f64 / 1e9;
    if dev < DEV_BUY_MIN_SOL || dev > DEV_BUY_MAX_SOL {
        return Err(Reject::DevBuyOutside);
    }
    if f.secs_since_previous_launch.is_some_and(|s| s < RELAUNCH_SKIP_SECS) {
        return Err(Reject::Relaunch);
    }
    if let (Some(max), Some(n)) = (DEPLOYER_MAX_LAUNCHES_24H, f.launches_last_day) {
        if n > max {
            return Err(Reject::TooManyLaunches);
        }
    }
    if let (Some(max), Some(n)) = (MAX_BUNDLE_WALLETS, f.bundle_wallets) {
        if n > max {
            return Err(Reject::BigBundle);
        }
    }
    if let Some(max) = DUMP_RISK_SKIP_POINTS {
        if dump_risk_flags(f).count_ones() >= max {
            return Err(Reject::DumpRisk);
        }
    }
    if no_pop_record_rejects(f.deployer_record) {
        return Err(Reject::NoPopRecord);
    }
    Ok(follow)
}

/// The deployer's balance before the create.
pub fn deployer_balance_verdict(deployer_lamports: Option<u64>) -> Result<(), String> {
    deployer_balance_code(deployer_lamports).map_err(|r| match r {
        Reject::DeployerPoor => format!(
            "deployer held {:.2} SOL before the create (minimum {DEPLOYER_MIN_SOL})",
            deployer_lamports.unwrap_or(0) as f64 / 1e9
        ),
        _ => "deployer balance before the create unknown".into(),
    })
}

/// `deployer_balance_verdict` without the message.
#[inline]
pub fn deployer_balance_code(deployer_lamports: Option<u64>) -> Result<(), Reject> {
    match deployer_lamports {
        Some(l) if (l as f64 / 1e9) < DEPLOYER_MIN_SOL => Err(Reject::DeployerPoor),
        None => Err(Reject::BalanceUnknown),
        Some(_) => Ok(()),
    }
}

/// Lamports to spend on a launch: the stake, or its smaller share on a small dev buy.
pub fn bet_lamports(stake: u64, dev_buy_lamports: u64) -> u64 {
    if (dev_buy_lamports as f64 / 1e9) < SMALL_BET_DEV_BUY_SOL {
        ((stake as f64) * SMALL_BET_SHARE).round().max(1.0) as u64
    } else {
        stake
    }
}

/// Launches whose dev usually dumps within seconds.
pub fn fast_rug_profile(metadata_host: Option<&str>, dev_buy_lamports: u64) -> bool {
    let tool_host = metadata_host.is_some_and(|h| {
        let h = h.to_lowercase();
        FAST_RUG_HOSTS.iter().any(|t| h.contains(t))
    });
    let dev = dev_buy_lamports as f64 / 1e9;
    tool_host || (FAST_RUG_DEV_BUY_MIN_SOL..FAST_RUG_DEV_BUY_MAX_SOL).contains(&dev)
}

/// One line per rule, for the startup summary and the dashboard.
pub fn summary() -> Vec<String> {
    let mut lines: Vec<String> = vec![
        format!("markets: pump.fun launches"),
        format!(
            "entry: {}deployer traded by {} in the last {} h or made it {FOLLOW_MIN_PROFIT_SOL} SOL; dev buy {DEV_BUY_MIN_SOL}-{DEV_BUY_MAX_SOL} SOL; deployer held {DEPLOYER_MIN_SOL}+ SOL; deployer wallet {}+ h old; no mayhem;{} not within {} min of the same deployer's previous launch{};{}{} not when {:.0}%+ of the deployer's last {NO_POP_RECORD_LAST} launches ({NO_POP_RECORD_MIN_LAUNCHES}+, {NO_POP_RECORD_DAYS} days) got no buyers",
            if SKIP_ALL_CAPS_NAMES { "name not all capitals; " } else { "" },
            short(&REFERENCE_WALLET),
            FOLLOW_RECENT_SECS / 3600,
            DEPLOYER_MIN_AGE_SECS / 3600,
            if SKIP_REFERENCE_LOSERS { " not when the reference wallet lost SOL on the deployer's coins;" } else { "" },
            RELAUNCH_SKIP_SECS / 60,
            DEPLOYER_MAX_LAUNCHES_24H.map(|n| format!(" or after more than {n} of its launches in 24 h")).unwrap_or_default(),
            MAX_BUNDLE_WALLETS.map(|n| format!(" at most {n} wallets in the create's bundle besides the dev;")).unwrap_or_default(),
            DUMP_RISK_SKIP_POINTS.map(|n| format!(" not at {n}+ dump-risk points (insiders {DUMP_RISK_INSIDER_SOL}+ SOL in, j7tracker metadata, relaunch within 1 h, creator {DUMP_RISK_CREATOR_SOL}+ SOL, reference traded the creator under {DUMP_RISK_REFERENCE_TRADES} times);")).unwrap_or_default(),
            NO_POP_RECORD_SHARE * 100.0
        ),
        format!(
            "buy: at the launch (at most {MAX_LAUNCH_AGE_SLOTS} slot late), full stake, {:.0}% of it when the dev buy is under {SMALL_BET_DEV_BUY_SOL} SOL, {}",
            SMALL_BET_SHARE * 100.0,
            match BUY_AHEAD_MAX_SOL {
                Some(sol) => format!("at most {sol} SOL bought ahead of it after the dev's bundle (one fast buyer)"),
                None => format!("{BUY_SLIPPAGE_PERCENT}% price guard"),
            }
        ),
        format!(
            "sell: {:.0}% once {FIRST_SELL_BEHIND_SOL} SOL lands behind the fill or +{FIRST_SELL_PRICE_PERCENT}%; from {CHECK_SECS} s with no sell: {}all out with {CHECK_BEHIND_SOL}+ SOL behind under +{CHECK_RUN_PERCENT}%, first slice on a run, hold while quiet{}",
            FIRST_SELL_SHARE * 100.0,
            match CHECK_NO_POP_PERCENT {
                Some(p) => format!("all out under +{p}%{} (NO_POP) or ", NO_POP_HOLD_HIGH_PERCENT.map(|h| format!(" unless it touched +{h}%")).unwrap_or_default()),
                None => "no NO_POP; ".to_string(),
            },
            match FIRST_SELL_MIN_UP_PERCENT {
                Some(p) => format!("; the first sell only while the price is over +{p}% (else hold)"),
                None => String::new(),
            }
        ),
        format!(
            "sell: {:.0}% of the rest {STEP_SECS} s after each sell lands ({FIRST_STEP_SECS} s after the first; {:.0}% on bets of {BIG_BET_SOL}+ SOL), wait while the price is within {}% of its high or others bought {} SOL+ or sold {} SOL+ in the last second, not on drops (max {MAX_WAITS} waits); all out when a sell comes due under the last sell's price with nobody buying for 2 s; all out at {:.0}% left or {MAX_HOLD_SECS} s",
            STEP_SHARE * 100.0,
            BIG_BET_STEP_SHARE * 100.0,
            WAIT_HIGH_PERCENT.unwrap_or(0.0),
            WAIT_BUY_SOL.unwrap_or(0.0),
            WAIT_SELL_SOL.unwrap_or(0.0),
            CLOSE_OUT_SHARE * 100.0
        ),
        match FIRST_ALL_UP_SOL {
            Some(sol) => format!("first sell: waits through the buy's slot and the next {FIRST_WAIT_SLOTS}; all out if the curve gains {sol}+ SOL in them, else the usual first sell"),
            None => "first sell: no wait".to_string(),
        },
    ]
    .into_iter()
    .chain(INSIDER_EXIT.then(|| "insiders: once the first sell is due, all out as soon as the dev or a wallet of the create's bundle has sold".to_string()))
    .chain([
        format!("fast-rug profile (launch-tool metadata or a {FAST_RUG_DEV_BUY_MIN_SOL}-{FAST_RUG_DEV_BUY_MAX_SOL} SOL dev buy): tagged only"),
        format!("safety: at most {} open positions, {BALANCE_RESERVE_SOL} SOL kept back, {SELL_SLIPPAGE_PERCENT}% sell price guard", max_open_positions()),
    ])
    .collect();
    if let Some(p) = PATIENT_EXIT {
        lines.retain(|l| !l.starts_with("sell:") && !l.starts_with("first sell:") && !l.starts_with("insiders:"));
        lines.insert(
            3,
            format!(
                "sell (patient exit): all out at +{}%; from {} s all out {}; all out at {} s; no slices",
                p.take_profit_percent,
                p.stop_after_secs,
                p.stop_percent.map(|s| if s == 0.0 { "under the entry".to_string() } else { format!("{s}% under the entry") }).unwrap_or_else(|| "never on a loss".to_string()),
                p.max_hold_secs
            ),
        );
    }
    if crate::config::scorecard::enabled() {
        if let Some(entry) = lines.iter_mut().find(|l| l.starts_with("entry:")) {
            *entry = format!(
                "entry (scorecard, SCORECARD=true): deployer traded by {} in the last {} h or made it {FOLLOW_MIN_PROFIT_SOL} SOL; no mayhem; score {}+ from 15 facts (relaunch gap, launches in 1 h / 24 h, reference profit and trades, no-buyers record, the creator's earlier results, dev buy and share, bundle, deployer SOL, funding, wallet age; config::scorecard); replaces the dev-buy range, deployer minimum SOL and age, relaunch skip and no-buyers record",
                short(&REFERENCE_WALLET),
                FOLLOW_RECENT_SECS / 3600,
                crate::config::scorecard::min()
            );
        }
    }
    if crate::utils::auto_sell::px_exit_on() {
        let s = *crate::utils::auto_sell::PX_SETTINGS;
        lines.retain(|l| !l.starts_with("sell:") && !l.starts_with("first sell:") && !l.starts_with("insiders:"));
        lines.insert(
            3,
            format!(
                "sell (pXGQ's exit, SELL_RULE=pxgq): all out at x{} of cost; {}-{} s after the buy all out under x1.05 with under {} SOL net bought behind; {:.0}% at x{} near the high, then the rest {:.0}% off the peak or at x{}; without that, all out {:.0}% off the peak while at cost or better; all out under x{} when others sold {}+ SOL in 3 s; all out at {} s",
                s.stop_x, s.wave_min_secs, s.wave_secs, s.wave_sol, s.scale_share * 100.0, s.scale_at_x, s.trail * 100.0, s.take_profit_x,
                s.dump_dd * 100.0, s.pressure_x, s.pressure_sol, s.max_hold_secs
            ),
        );
    }
    // The snipe test: its size and caps; with SNIPE_TEST_RULES its launch rules replace the exit.
    if crate::utils::snipe_test::on() {
        lines.retain(|l| !l.starts_with("buy:"));
        if crate::utils::snipe_test::rules_on() {
            lines.retain(|l| !l.starts_with("sell") && !l.starts_with("first sell:") && !l.starts_with("insiders:"));
        }
        lines.extend(crate::utils::snipe_test::summary());
    }
    lines.extend(crate::processors::whale::summary());
    lines
}

fn short(key: &Pubkey) -> String {
    let s = key.to_string();
    format!("{}…", &s[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> LaunchFacts {
        LaunchFacts {
            mayhem: false,
            dev_buy_lamports: lamports(2.0),
            deployer_lamports: Some(lamports(5.0)),
            follow: Some(Follow::Recent),
            reference_profit_lamports: Some(lamports(1.0) as i64),
            reference_trades: Some(20),
            insider_lamports: Some(lamports(2.0)),
            risky_host: Some(false),
            secs_since_previous_launch: None,
            launches_last_day: Some(3),
            bundle_wallets: Some(1),
            name_all_caps: Some(false),
            deployer_record: (0, 0),
            scoring: Default::default(),
        }
    }

    /// With the scorecard on, a followed launch is decided by its score alone: a dev buy outside
    /// 0.7-20 SOL, a poor deployer or a quick relaunch no longer skip it; a low score does; the
    /// balance is still needed; mayhem and unfollowed deployers still skip.
    #[test]
    fn the_scorecard_replaces_the_quality_rules() {
        let good = LaunchFacts {
            dev_buy_lamports: lamports(0.0),
            deployer_lamports: Some(lamports(20.0)),
            reference_profit_lamports: Some(lamports(8.0) as i64),
            reference_trades: Some(40),
            secs_since_previous_launch: Some(7200),
            launches_last_day: Some(2),
            bundle_wallets: Some(1),
            deployer_record: (3, 0),
            scoring: Scoring {
                launches_last_hour: Some(0),
                wallet_first_bt: Some(0),
                funded_sol: Some(1.0),
                earlier: Some((3, 0.15, 0.1)),
                bundle_lamports: Some(lamports(1.5)),
                now: 400 * 3600,
            },
            ..facts()
        };
        let score = launch_score(&good);
        assert!(score >= 61, "{score}");
        // Off: the dev buy of 0 SOL is outside the range.
        assert_eq!(launch_verdict_code_with(&good, None), Err(Reject::DevBuyOutside));
        assert!(launch_verdict_code_with(&good, Some(61)).is_ok());
        assert_eq!(launch_verdict_code_with(&good, Some(score + 1)), Err(Reject::LowScore));
        let poor_quick = LaunchFacts { deployer_lamports: Some(lamports(0.5)), secs_since_previous_launch: Some(400), ..good };
        assert!(launch_verdict_code_with(&poor_quick, Some(0)).is_ok());
        // A relaunch within 2 minutes costs 33 points against one 2 hours on.
        let relaunch = LaunchFacts { secs_since_previous_launch: Some(60), ..good };
        assert_eq!(launch_score(&good) - launch_score(&relaunch), 3 + 23);
        // The balance is needed for the score: before it, nothing is decided on the score.
        let no_balance = LaunchFacts { deployer_lamports: None, ..good };
        assert!(verdict_code_before_balance_with(&no_balance, Some(1000)).is_ok());
        assert_eq!(launch_verdict_code_with(&no_balance, Some(0)), Err(Reject::BalanceUnknown));
        assert_eq!(launch_verdict_code_with(&LaunchFacts { follow: None, ..good }, Some(0)), Err(Reject::NotFollowed));
        assert_eq!(launch_verdict_code_with(&LaunchFacts { mayhem: true, ..good }, Some(0)), Err(Reject::Mayhem));
        assert!(Reject::LowScore.describe(&relaunch).contains("secs_since_last_launch -23"));
    }

    /// Unknown facts take the values the training data gave them.
    #[test]
    fn missing_score_facts_take_the_training_datas_values() {
        let f = score_facts(&LaunchFacts { secs_since_previous_launch: None, deployer_record: (0, 0), ..facts() });
        assert_eq!(f[0], 1e7);
        assert_eq!(f[5], -9.0);
        assert_eq!((f[6], f[7]), (-9.0, -9.0));
        assert_eq!(f[13], f64::NEG_INFINITY);
        assert_eq!(f[14], f64::NEG_INFINITY);
    }

    /// The entry rules exactly as they were before the code/message split (baseline-2026-10-07),
    /// kept as the oracle for `launch_verdict` / `verdict_before_balance`.
    mod before_split {
        use super::super::*;

        pub fn dump_risk(f: &LaunchFacts) -> (u32, Vec<&'static str>) {
            let mut why = Vec::new();
            if f.insider_lamports.is_some_and(|l| l as f64 / 1e9 >= DUMP_RISK_INSIDER_SOL) {
                why.push("insiders 6+ SOL in");
            }
            if f.risky_host == Some(true) {
                why.push("j7tracker metadata");
            }
            if f.secs_since_previous_launch.is_some_and(|s| s < DUMP_RISK_RELAUNCH_SECS) {
                why.push("relaunch within 1 h");
            }
            if f.deployer_lamports.is_some_and(|l| l as f64 / 1e9 >= DUMP_RISK_CREATOR_SOL) {
                why.push("creator 100+ SOL");
            }
            if f.follow.is_some() && f.reference_trades.unwrap_or(0) < DUMP_RISK_REFERENCE_TRADES {
                why.push("reference traded the creator under 10 times");
            }
            (why.len() as u32, why)
        }

        pub fn no_pop_record_verdict((launches, no_pops): (u32, u32)) -> Result<(), String> {
            if launches >= NO_POP_RECORD_MIN_LAUNCHES && no_pops as f64 >= NO_POP_RECORD_SHARE * launches as f64 {
                return Err(format!(
                    "the deployer's recent launches got no buyers: {no_pops} of its last {launches} would have been sold NO_POP"
                ));
            }
            Ok(())
        }

        pub fn launch_verdict(f: &LaunchFacts) -> Result<Follow, String> {
            let follow = verdict_before_balance(f)?;
            deployer_balance_verdict(f.deployer_lamports)?;
            Ok(follow)
        }

        pub fn verdict_before_balance(f: &LaunchFacts) -> Result<Follow, String> {
            if SKIP_MAYHEM && f.mayhem {
                return Err("mayhem-mode launch".into());
            }
            if SKIP_ALL_CAPS_NAMES && f.name_all_caps == Some(true) {
                return Err("name in capitals only".into());
            }
            let Some(follow) = f.follow else {
                return Err("deployer not followed: the reference wallet has not traded it in 41 h and it never made him 0.77 SOL".into());
            };
            if let Some(lost) = f.reference_profit_lamports.filter(|p| SKIP_REFERENCE_LOSERS && *p < 0) {
                return Err(format!("the reference wallet lost {:.2} SOL on this deployer's coins", -lost as f64 / 1e9));
            }
            let dev = f.dev_buy_lamports as f64 / 1e9;
            if dev < DEV_BUY_MIN_SOL || dev > DEV_BUY_MAX_SOL {
                return Err(format!("dev buy {dev:.2} SOL outside {DEV_BUY_MIN_SOL}-{DEV_BUY_MAX_SOL} SOL"));
            }
            if let Some(secs) = f.secs_since_previous_launch.filter(|s| *s < RELAUNCH_SKIP_SECS) {
                return Err(format!(
                    "deployer launched another coin {secs} s ago (relaunches within {} min are skipped)",
                    RELAUNCH_SKIP_SECS / 60
                ));
            }
            if let (Some(max), Some(n)) = (DEPLOYER_MAX_LAUNCHES_24H, f.launches_last_day) {
                if n > max {
                    return Err(format!("deployer launched {n} coins in the last 24 h (more than {max} are skipped)"));
                }
            }
            if let (Some(max), Some(n)) = (MAX_BUNDLE_WALLETS, f.bundle_wallets) {
                if n > max {
                    return Err(format!("{n} wallets in the create's bundle (more than {max} are skipped)"));
                }
            }
            if let Some(max) = DUMP_RISK_SKIP_POINTS {
                let (points, why) = dump_risk(f);
                if points >= max {
                    return Err(format!("dump risk {points} points ({})", why.join(", ")));
                }
            }
            no_pop_record_verdict(f.deployer_record)?;
            Ok(follow)
        }

        pub fn deployer_balance_verdict(deployer_lamports: Option<u64>) -> Result<(), String> {
            match deployer_lamports {
                Some(l) if (l as f64 / 1e9) < DEPLOYER_MIN_SOL => Err(format!(
                    "deployer held {:.2} SOL before the create (minimum {DEPLOYER_MIN_SOL})",
                    l as f64 / 1e9
                )),
                None => Err("deployer balance before the create unknown".into()),
                Some(_) => Ok(()),
            }
        }
    }

    /// Facts spread over every rule's edges, from a fixed seed.
    fn random_facts(seed: &mut u64) -> LaunchFacts {
        let mut next = || {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        };
        let pick = |r: u64, n: u64| r % n;
        let sol = |r: u64| lamports(match r % 8 { 0 => 0.0, 1 => 0.69, 2 => 0.7, 3 => 2.0, 4 => 5.99, 5 => 6.0, 6 => 20.0, _ => 120.0 });
        LaunchFacts {
            mayhem: pick(next(), 10) == 0,
            dev_buy_lamports: sol(next()),
            deployer_lamports: (next() % 4 != 0).then(|| sol(next())),
            follow: match pick(next(), 3) { 0 => None, 1 => Some(Follow::Recent), _ => Some(Follow::Profitable) },
            reference_profit_lamports: (next() % 4 != 0).then(|| [-5_000_000i64, -1, 0, 1, 900_000_000][pick(next(), 5) as usize]),
            reference_trades: (next() % 4 != 0).then(|| [0u32, 3, 9, 10, 40][pick(next(), 5) as usize]),
            insider_lamports: (next() % 4 != 0).then(|| sol(next())),
            risky_host: [None, Some(false), Some(true)][pick(next(), 3) as usize],
            secs_since_previous_launch: (next() % 4 != 0).then(|| [0u64, 59, 599, 600, 3599, 3600, 90_000][pick(next(), 7) as usize]),
            launches_last_day: (next() % 4 != 0).then(|| pick(next(), 30) as u32),
            bundle_wallets: (next() % 4 != 0).then(|| pick(next(), 6) as u32),
            name_all_caps: [None, Some(false), Some(true)][pick(next(), 3) as usize],
            deployer_record: (pick(next(), 11) as u32, 0).map_second(|n| (next() % (n as u64 + 1)) as u32),
            scoring: Default::default(),
        }
    }

    trait MapSecond {
        fn map_second(self, f: impl FnMut(u32) -> u32) -> (u32, u32);
    }
    impl MapSecond for (u32, u32) {
        fn map_second(self, mut f: impl FnMut(u32) -> u32) -> (u32, u32) {
            (self.0, f(self.0))
        }
    }

    #[test]
    fn the_coded_verdicts_decide_and_read_exactly_as_before() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let (mut kept, mut skipped) = (0, 0);
        for _ in 0..200_000 {
            let f = random_facts(&mut seed);
            assert_eq!(launch_verdict(&f), before_split::launch_verdict(&f), "{f:?}");
            assert_eq!(verdict_before_balance(&f), before_split::verdict_before_balance(&f), "{f:?}");
            assert_eq!(launch_verdict_code(&f).is_ok(), launch_verdict(&f).is_ok(), "{f:?}");
            assert_eq!(dump_risk(&f), before_split::dump_risk(&f), "{f:?}");
            assert_eq!(dump_risk_flags(&f).count_ones(), dump_risk(&f).0);
            if launch_verdict(&f).is_ok() { kept += 1 } else { skipped += 1 }
        }
        assert!(kept > 100 && skipped > 100_000, "both outcomes covered: {kept} kept, {skipped} skipped");
    }

    #[test]
    fn all_caps_names_are_read_like_python_isupper() {
        for (name, caps) in [("PEPE", true), ("PEPE 2.0", true), ("$WIF", true), ("X", true), ("Pepe", false), ("pepe", false),
                             ("iPHONE", false), ("123", false), ("", false), ("🚀🚀", false)] {
            assert_eq!(name_is_all_caps(name), caps, "{name:?}");
        }
    }

    #[test]
    fn an_all_caps_name_is_skipped() {
        let with = |caps| launch_verdict(&LaunchFacts { name_all_caps: caps, ..facts() });
        assert!(with(None).is_ok(), "name not read");
        assert!(with(Some(false)).is_ok());
        if SKIP_ALL_CAPS_NAMES {
            assert!(with(Some(true)).unwrap_err().contains("capitals"));
        } else {
            assert!(with(Some(true)).is_ok(), "the rule is off");
        }
    }

    #[test]
    fn a_launch_with_a_big_bundle_is_skipped() {
        let with = |n| launch_verdict(&LaunchFacts { bundle_wallets: n, ..facts() });
        assert!(with(None).is_ok(), "bundle not seen");
        assert!(with(Some(0)).is_ok());
        match MAX_BUNDLE_WALLETS {
            Some(max) => {
                assert!(with(Some(max)).is_ok(), "the limit itself passes");
                assert!(with(Some(max + 1)).unwrap_err().contains("bundle"));
            }
            None => assert!(with(Some(50)).is_ok(), "the rule is off"),
        }
    }

    #[test]
    fn a_deployer_with_too_many_launches_in_a_day_is_skipped() {
        let with = |n| launch_verdict(&LaunchFacts { launches_last_day: n, ..facts() });
        assert!(with(None).is_ok(), "no history");
        assert!(with(Some(0)).is_ok());
        match DEPLOYER_MAX_LAUNCHES_24H {
            Some(max) => {
                assert!(with(Some(max)).is_ok(), "the limit itself passes");
                assert!(with(Some(max + 1)).unwrap_err().contains("in the last 24 h"));
            }
            None => assert!(with(Some(1000)).is_ok(), "the rule is off"),
        }
    }

    #[test]
    fn a_deployer_whose_recent_launches_got_no_buyers_is_skipped() {
        let with = |launches, no_pops| launch_verdict(&LaunchFacts { deployer_record: (launches, no_pops), ..facts() });
        assert!(with(0, 0).is_ok(), "unknown deployer");
        assert!(with(1, 1).is_ok(), "one launch is not a record");
        assert!(with(2, 1).unwrap_err().contains("no buyers"), "half");
        assert!(with(3, 1).is_ok(), "a third");
        assert!(with(10, 5).is_err());
        assert!(with(10, 4).is_ok());
    }

    #[test]
    fn followed_launch_inside_every_line_passes() {
        assert_eq!(launch_verdict(&facts()), Ok(Follow::Recent));
    }

    #[test]
    fn quick_relaunch_by_the_same_deployer_is_skipped() {
        let quick = LaunchFacts {
            secs_since_previous_launch: Some(RELAUNCH_SKIP_SECS - 1),
            ..facts()
        };
        assert!(launch_verdict(&quick)
            .unwrap_err()
            .contains("launched another coin"));
        let later = LaunchFacts {
            secs_since_previous_launch: Some(RELAUNCH_SKIP_SECS),
            ..facts()
        };
        assert!(launch_verdict(&later).is_ok());
    }

    #[test]
    fn unfollowed_deployer_is_skipped() {
        let f = LaunchFacts {
            follow: None,
            ..facts()
        };
        assert!(launch_verdict(&f).unwrap_err().contains("not followed"));
    }

    #[test]
    fn a_deployer_the_reference_wallet_lost_on_is_skipped() {
        let with = |p| launch_verdict(&LaunchFacts { reference_profit_lamports: p, ..facts() });
        assert!(with(None).is_ok(), "no trades of his");
        assert!(with(Some(0)).is_ok(), "even");
        match SKIP_REFERENCE_LOSERS {
            true => assert!(with(Some(-1)).unwrap_err().contains("lost")),
            false => assert!(with(Some(-1)).is_ok(), "the rule is off"),
        }
    }

    #[test]
    fn dump_risk_skips_at_three_points() {
        let risky = LaunchFacts {
            insider_lamports: Some(lamports(6.0)),
            risky_host: Some(true),
            secs_since_previous_launch: Some(DUMP_RISK_RELAUNCH_SECS - 1),
            ..facts()
        };
        assert_eq!(dump_risk(&risky).0, 3);
        match DUMP_RISK_SKIP_POINTS {
            Some(_) => assert!(launch_verdict(&risky).unwrap_err().contains("dump risk 3 points")),
            None => assert!(launch_verdict(&risky).is_ok(), "the check is off"),
        }
        let two = LaunchFacts { risky_host: Some(false), ..risky };
        assert_eq!(dump_risk(&two).0, 2);
        assert!(launch_verdict(&two).is_ok());
        let rich = LaunchFacts { deployer_lamports: Some(lamports(100.0)), reference_trades: Some(3), ..two };
        assert_eq!(dump_risk(&rich).0, 4, "creator 100+ SOL and few reference trades");
        // Unknown inputs score nothing.
        let unknown = LaunchFacts { insider_lamports: None, risky_host: None, secs_since_previous_launch: None, deployer_lamports: None, ..facts() };
        assert_eq!(dump_risk(&unknown).0, 0);
        assert!(risky_host("metadata.j7tracker.io") && !risky_host("ipfs.io"));
    }

    #[test]
    fn mayhem_is_skipped_even_for_followed_deployers() {
        let f = LaunchFacts {
            mayhem: true,
            ..facts()
        };
        assert!(launch_verdict(&f).unwrap_err().contains("mayhem"));
    }

    #[test]
    fn dev_buy_band_is_inclusive_at_both_ends() {
        for (sol, ok) in [(DEV_BUY_MIN_SOL - 0.01, false), (DEV_BUY_MIN_SOL, true), (DEV_BUY_MAX_SOL, true), (DEV_BUY_MAX_SOL + 0.01, false)] {
            let f = LaunchFacts {
                dev_buy_lamports: lamports(sol),
                ..facts()
            };
            assert_eq!(launch_verdict(&f).is_ok(), ok, "dev buy {sol}");
        }
    }

    #[test]
    fn deployer_needs_two_sol_and_a_known_balance() {
        let poor = LaunchFacts {
            deployer_lamports: Some(lamports(1.99)),
            ..facts()
        };
        assert!(launch_verdict(&poor).is_err());
        let unknown = LaunchFacts {
            deployer_lamports: None,
            ..facts()
        };
        assert!(launch_verdict(&unknown).is_err());
        let enough = LaunchFacts {
            deployer_lamports: Some(lamports(2.0)),
            ..facts()
        };
        assert!(launch_verdict(&enough).is_ok());
    }

    #[test]
    fn small_dev_buys_get_the_smaller_bet() {
        let stake = lamports(2.93);
        assert_eq!(bet_lamports(stake, lamports(0.8)), lamports(2.93 * 0.65));
        assert_eq!(bet_lamports(stake, lamports(1.0)), stake);
        assert_eq!(bet_lamports(stake, lamports(15.0)), stake);
    }

    #[test]
    fn fast_rug_profile_matches_tool_hosts_and_the_dev_buy_band() {
        assert!(fast_rug_profile(
            Some("metadata.j7tracker.io"),
            lamports(1.0)
        ));
        assert!(fast_rug_profile(Some("META.UXENTO.IO"), lamports(1.0)));
        assert!(fast_rug_profile(Some("ipfs.io"), lamports(3.0)));
        assert!(fast_rug_profile(Some("ipfs.io"), lamports(9.99)));
        assert!(!fast_rug_profile(Some("ipfs.io"), lamports(10.0)));
        assert!(!fast_rug_profile(Some("ipfs.io"), lamports(2.99)));
        assert!(!fast_rug_profile(None, lamports(1.0)));
    }
}
