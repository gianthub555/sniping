//! The snipe test's graduations (SNIPE_TEST=1 with SNIPE_TEST_POOL=on, the owner's rule of
//! 2026-10-13): a pump.fun coin whose curve went from under 60 SOL real to complete within 2 s (seen
//! on bigboss's own pump.fun stream), migrated to its canonical PumpSwap pool, quoted in SOL, not
//! mayhem; when the migration slot has ended the pool is still under 1.5x its opening price (the
//! insiders' jump hasn't happened); bought at once (aiming to land 2 slots after the migration)
//! with a 10% price guard, all of it sold 240 s after the fill. Measured on 2,548 graduations of
//! Oct 3-7: +9.8 mSOL per 0.1 SOL after costs (gains capped at 10x); not proven yet.
//!
//! The pool's state comes from its trades' events on the stream (the PumpSwap program is in the
//! subscription while this is on): the vaults before each trade, its amounts, and the virtual quote
//! it was priced on (`instructions::pumpswap_events`). Paper fills at the landing slot's worst pool
//! state (the highest price for the buy, the lowest for the sell), on the same math as the chain
//! (`pool_check`: exact on real trades).
//!
//! Shared with the launches (`utils::snipe_test`): the size, the caps (an entry taken before the
//! buy), the log line (kind "graduation", with the pool's price at +30, +60, +120, +240 and +600 s
//! after the fill). Graduations have their own limit of open positions (MAX_OPEN), so a
//! graduation never blocks a launch's position slot (MAX_OPEN_POSITIONS) and the reverse.
//!
//! Off the hot path entirely: the launches' hot path is untouched.

use {
    super::common::cpi_event_payloads,
    crate::{
        config::{lamports_to_sol, trading_wallet, RPC_CLIENT, TRADE_SETTING},
        instructions::{
            pumpfun_buy_ix::TRADE_EVENT_DISCRIMINATOR,
            pumpswap_buy_ix::{pool_authority, PumpSwapMarket, CREATE_POOL_EVENT_DISCRIMINATOR, INIT_BOOST_EVENT_DISCRIMINATOR, PUMPSWAP_PROGRAM_ID, PUMP_PROGRAM_ID},
            pumpswap_events::{self, price},
        },
        service::SendProfile,
        utils::{
            csv_logger::{log_trade, TradeRecord},
            dashboard::{activity, now_unix, now_unix_ms, ActivityKind},
            parse::{read_pubkey, read_u128, read_u64},
            snipe_test,
            utils::{fee_payer_lamport_change, get_associated_token_address, get_token_balance, ConfirmOutcome, TOKEN_2022_PROGRAM_ID, WSOL_MINT},
        },
    },
    carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor, transaction::TransactionMetadata},
    carbon_pump_swap_decoder::instructions::PumpSwapInstruction,
    log::{info, warn},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            LazyLock, Mutex,
        },
        time::Duration,
    },
};

pub const FAST_FROM_LAMPORTS: u64 = 60_000_000_000;
pub const FAST_WITHIN_MS: u64 = 2_000;
pub const JUMP_X: f64 = 1.5;
pub const LAND_SLOTS: u64 = 2;
pub const PRICE_GUARD: f64 = 0.10;
pub const HOLD: Duration = Duration::from_secs(240);
/// Graduation positions open at once at most (their own, apart from MAX_OPEN_POSITIONS).
pub const MAX_OPEN: usize = 2;
/// Decide at the latest this long after the migration, even if the stream showed no later slot.
const SLOT_END_WAIT: Duration = Duration::from_millis(1_500);
/// A paper fill waits at most this long for the stream to pass its landing slot.
const LAND_WAIT: Duration = Duration::from_secs(3);
const SELL_TRIES: u32 = 5;
const DEX: &str = "PumpSwap";

pub fn on() -> bool {
    snipe_test::pool_on()
}

// ---- The curves (bigboss's pump.fun stream): when each was last under 60 SOL real, when it completed.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Curve {
    pub last_below_ms: Option<u64>,
    pub completed_ms: Option<u64>,
    seen_ms: u64,
}

static CURVES: LazyLock<Mutex<HashMap<Pubkey, Curve>>> = LazyLock::new(Default::default);

/// One curve trade: its real SOL and real tokens after it, at `now_ms`.
pub fn note_curve(curves: &mut HashMap<Pubkey, Curve>, mint: Pubkey, real_sol: u64, real_tokens: u64, now_ms: u64) {
    if curves.len() > 50_000 {
        curves.retain(|_, c| now_ms.saturating_sub(c.seen_ms) < 600_000);
    }
    let c = curves.entry(mint).or_default();
    c.seen_ms = now_ms;
    if real_tokens == 0 {
        c.completed_ms.get_or_insert(now_ms);
    } else if real_sol < FAST_FROM_LAMPORTS {
        c.last_below_ms = Some(now_ms);
    }
}

/// Every pump.fun trade of a transaction (the curve's real reserves after it). Off: nothing.
pub fn observe_pump(tx: &TransactionMetadata) {
    if !on() {
        return;
    }
    let payloads = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    if payloads.is_empty() {
        return;
    }
    let now = now_unix_ms();
    let mut curves = CURVES.lock().unwrap_or_else(|e| e.into_inner());
    for body in payloads {
        if let (Ok(mint), Ok(real_sol), Ok(real_tokens)) = (read_pubkey(&body, 0), read_u64(&body, 105), read_u64(&body, 113)) {
            note_curve(&mut curves, mint, real_sol, real_tokens, now);
        }
    }
}

/// The completion rule: None when it passes, else why not.
pub fn completion_skip(c: Option<Curve>) -> Result<u64, String> {
    let Some(c) = c else { return Err("graduation: its curve's trades were not seen (since the bot started)".into()) };
    let Some(done) = c.completed_ms else { return Err("graduation: its curve's completing trade was not seen".into()) };
    let Some(below) = c.last_below_ms else { return Err("graduation: its curve was never seen under 60 SOL".into()) };
    let gap = done.saturating_sub(below);
    if gap > FAST_WITHIN_MS {
        return Err(format!("graduation: {:.1} s from under 60 SOL to complete (more than 2 s)", gap as f64 / 1000.0));
    }
    Ok(gap)
}

// ---- The pools at migration.

/// The pool after one trade (or at its creation): slot, block index, when the stream gave it,
/// base vault, quote vault, virtual quote.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct State {
    pub slot: u64,
    pub idx: u64,
    pub ms: u64,
    pub base: u64,
    pub quote: u64,
    pub vq: i128,
}

impl State {
    pub fn price(&self) -> f64 {
        price(self.base, self.quote, self.vq)
    }
}

struct Watch {
    mint: Pubkey,
    opened: State,
    states: Vec<State>,
    market: PumpSwapMarket,
}

static WATCHES: LazyLock<Mutex<HashMap<Pubkey, Watch>>> = LazyLock::new(Default::default);
static STREAM_SLOT: AtomicU64 = AtomicU64::new(0);
static OPEN: AtomicUsize = AtomicUsize::new(0);
static RECENT: LazyLock<Mutex<(HashSet<Signature>, VecDeque<Signature>)>> = LazyLock::new(Default::default);

/// Counts for the start line and the report: canonical migrations seen, bought.
pub static SEEN: AtomicU64 = AtomicU64::new(0);
pub static BOUGHT: AtomicU64 = AtomicU64::new(0);

fn first_time(sig: &Signature) -> bool {
    let mut r = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    if !r.0.insert(*sig) {
        return false;
    }
    r.1.push_back(*sig);
    if r.1.len() > 8_192 {
        if let Some(old) = r.1.pop_front() {
            r.0.remove(&old);
        }
    }
    true
}

/// The pool's state at the end of the migration slot (its last trade there, else its opening).
pub fn slot_end(opened: &State, states: &[State], slot: u64) -> State {
    states.iter().filter(|s| s.slot <= slot).max_by_key(|s| (s.slot, s.idx)).copied().unwrap_or(*opened)
}

/// The landing slot's worst state for a trade: the state the slot starts with and every state
/// after one of its trades; the highest price for a buy, the lowest for a sell.
pub fn worst(opened: &State, states: &[State], slot: u64, buy: bool) -> State {
    let start = states.iter().filter(|s| s.slot < slot).max_by_key(|s| (s.slot, s.idx)).copied().unwrap_or(*opened);
    let mut all: Vec<State> = vec![start];
    all.extend(states.iter().filter(|s| s.slot == slot).copied());
    let by = |a: &&State, b: &&State| a.price().partial_cmp(&b.price()).unwrap_or(std::cmp::Ordering::Equal);
    *if buy { all.iter().max_by(by) } else { all.iter().min_by(by) }.unwrap_or(&start)
}

/// The pool's price `at_ms` (its last state then).
fn price_at(opened: &State, states: &[State], at_ms: u64) -> f64 {
    states.iter().filter(|s| s.ms <= at_ms).max_by_key(|s| (s.slot, s.idx)).copied().unwrap_or(*opened).price()
}

fn market_at(m: &PumpSwapMarket, s: &State) -> PumpSwapMarket {
    PumpSwapMarket { base_reserve: s.base, quote_reserve: s.quote, virtual_quote_reserves: s.vq, ..m.clone() }
}

fn skip(mint: &Pubkey, reason: &str) {
    info!("graduation {mint}: skipped: {reason}");
    activity(ActivityKind::Skipped, Some(DEX), Some(&mint.to_string()), format!("Skipped: {reason}"));
}

pub struct GraduationProcess;

impl<'a> Processor<InstructionProcessorInputType<'a, PumpSwapInstruction>> for GraduationProcess {
    async fn process(&mut self, input: &InstructionProcessorInputType<'a, PumpSwapInstruction>) -> CarbonResult<()> {
        let tx = &input.metadata.transaction_metadata;
        STREAM_SLOT.fetch_max(tx.slot, Ordering::Relaxed);
        if first_time(&tx.signature) {
            observe_pool_trades(tx);
        }
        if let PumpSwapInstruction::CreatePool { data, accounts, .. } = input.decoded_instruction {
            // A migration creates the coin's canonical pool (pump's pool authority, index 0); any
            // other pool is not a graduation.
            if data.index != 0 || accounts.creator != pool_authority(&accounts.base_mint) {
                return Ok(());
            }
            let mint = accounts.base_mint;
            SEEN.fetch_add(1, Ordering::Relaxed);
            crate::store::launch_seen(&mint.to_string(), "pumpswap", "graduation", tx.slot, false, None, Some(data.coin_creator.to_string()), 0, false, None);
            if accounts.quote_mint != WSOL_MINT {
                return Ok(skip(&mint, "graduation: not quoted in SOL"));
            }
            if data.is_mayhem_mode {
                return Ok(skip(&mint, "graduation: mayhem"));
            }
            let curve = CURVES.lock().unwrap_or_else(|e| e.into_inner()).get(&mint).copied();
            let fast_ms = match completion_skip(curve) {
                Ok(gap) => gap,
                Err(why) => return Ok(skip(&mint, &why)),
            };
            let (base_amount, quote_amount) = cpi_event_payloads(tx, &PUMPSWAP_PROGRAM_ID, &CREATE_POOL_EVENT_DISCRIMINATOR)
                .iter()
                .find(|p| read_pubkey(p, 165).ok() == Some(accounts.pool))
                .and_then(|p| Some((read_u64(p, 124).ok()?, read_u64(p, 132).ok()?)))
                .unwrap_or((data.base_amount_in, data.quote_amount_in));
            let boost = cpi_event_payloads(tx, &PUMPSWAP_PROGRAM_ID, &INIT_BOOST_EVENT_DISCRIMINATOR)
                .iter()
                .find(|p| read_pubkey(p, 72).ok() == Some(accounts.pool))
                .and_then(|p| read_u128(p, 104).ok())
                .map_or(0, |v| v as i128);
            let market = PumpSwapMarket::from_create_pool(
                accounts.pool,
                accounts.creator,
                mint,
                accounts.base_token_program,
                accounts.pool_base_token_account,
                accounts.pool_quote_token_account,
                data.coin_creator,
                data.is_mayhem_mode,
                data.is_cashback_coin.0,
                base_amount,
                quote_amount,
                boost,
            );
            let opened = State {
                slot: tx.slot,
                idx: tx.index.unwrap_or(0),
                ms: now_unix_ms(),
                base: market.base_reserve,
                quote: market.quote_reserve,
                vq: market.virtual_quote_reserves,
            };
            info!(
                "graduation {mint}: canonical pool {} in slot {}, curve under 60 SOL {fast_ms} ms before it completed; opening price {:.3e}",
                accounts.pool,
                tx.slot,
                opened.price()
            );
            WATCHES.lock().unwrap_or_else(|e| e.into_inner()).insert(accounts.pool, Watch { mint, opened, states: Vec::new(), market });
            tokio::spawn(run(accounts.pool, tx.slot, fast_ms));
        }
        Ok(())
    }
}

/// Every PumpSwap trade of a transaction on a watched pool: the pool after it.
fn observe_pool_trades(tx: &TransactionMetadata) {
    let mut watches = WATCHES.lock().unwrap_or_else(|e| e.into_inner());
    if watches.is_empty() {
        return;
    }
    let trades = pumpswap_events::trades(tx);
    let ms = now_unix_ms();
    for t in trades {
        if let Some(w) = watches.get_mut(&t.pool) {
            let (base, quote, vq) = t.after();
            let vq = if t.virtual_quote.is_some() { vq } else { w.states.last().map_or(w.opened.vq, |s| s.vq) };
            w.states.push(State { slot: tx.slot, idx: tx.index.unwrap_or(0), ms, base, quote, vq });
        }
    }
}

/// The watch's opening, its states and its market, copied out.
fn snapshot(pool: &Pubkey) -> Option<(Pubkey, State, Vec<State>, PumpSwapMarket)> {
    let w = WATCHES.lock().unwrap_or_else(|e| e.into_inner());
    w.get(pool).map(|w| (w.mint, w.opened, w.states.clone(), w.market.clone()))
}

fn forget(pool: &Pubkey) {
    WATCHES.lock().unwrap_or_else(|e| e.into_inner()).remove(pool);
}

/// Waits until the stream is past `slot` (or `max` passed).
async fn past(slot: u64, max: Duration) {
    let until = tokio::time::Instant::now() + max;
    while STREAM_SLOT.load(Ordering::Relaxed) <= slot && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct OpenGuard;
impl Drop for OpenGuard {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One graduation: the slot-end check, the buy, the hold, the sell, the prices after.
async fn run(pool: Pubkey, mig_slot: u64, fast_ms: u64) {
    past(mig_slot, SLOT_END_WAIT).await;
    let Some((mint, opened, states, market)) = snapshot(&pool) else { return };
    let end = slot_end(&opened, &states, mig_slot);
    let open_price = opened.price();
    let x = if open_price > 0.0 { end.price() / open_price } else { 0.0 };
    if x >= JUMP_X {
        forget(&pool);
        return skip(&mint, &format!("graduation: the insiders' jump came in the migration slot (x{x:.2} of the opening)"));
    }
    if OPEN.load(Ordering::SeqCst) >= MAX_OPEN {
        forget(&pool);
        return skip(&mint, &format!("graduation: {MAX_OPEN} graduations already open"));
    }
    if let Err(t) = snipe_test::try_enter(now_unix()) {
        forget(&pool);
        return skip(&mint, t.text());
    }
    OPEN.fetch_add(1, Ordering::SeqCst);
    let _open = OpenGuard;
    let size = snipe_test::size_lamports();
    let decision = market_at(&market, &end);
    let expected = match decision.quote_buy(size) {
        Ok(t) if t > 0 => t,
        _ => {
            forget(&pool);
            return skip(&mint, "graduation: no quote on the pool");
        }
    };
    let min_out = (expected as f64 * (1.0 - PRICE_GUARD)) as u64;
    let paper = TRADE_SETTING.dry_run;
    snipe_test::note_entry(snipe_test::Entry {
        mint: mint.to_string(),
        mode: if paper { "paper" } else { "live" },
        kind: "graduation",
        path: "pool",
        deployer: market.coin_creator.to_string(),
        create_slot: mig_slot,
        decided_unix_ms: now_unix_ms(),
        price_after_bundle: end.price(),
        size_lamports: size,
        token_2022: market.base_token_program == TOKEN_2022_PROGRAM_ID,
        grad: Some(snipe_test::Grad { fast_ms, open_price, slot_end_x: x, ..Default::default() }),
        ..Default::default()
    });
    crate::store::trades::note_context(
        &mint.to_string(),
        crate::store::trades::Context {
            venue: "pumpswap",
            deployer: Some(market.coin_creator.to_string()),
            follow_kind: None,
            tier: None,
            rent_lamports: crate::store::trades::rent(market.base_token_program == TOKEN_2022_PROGRAM_ID),
        },
    );
    activity(
        ActivityKind::Detected,
        Some(DEX),
        Some(&mint.to_string()),
        format!("Graduation: curve under 60 SOL {fast_ms} ms before complete, pool x{x:.2} of its opening at the migration slot's end; buying {} SOL", lamports_to_sol(size)),
    );
    BOUGHT.fetch_add(1, Ordering::Relaxed);
    let bought = if paper { paper_buy(&pool, mint, mig_slot, size, min_out).await } else { live_buy(&pool, mint, mig_slot, &market, size, min_out).await };
    let Some((tokens, fill_ms)) = bought else {
        forget(&pool);
        return;
    };
    // Hold, then all out.
    let hold_until = fill_ms + HOLD.as_millis() as u64;
    let now = now_unix_ms();
    if hold_until > now {
        tokio::time::sleep(Duration::from_millis(hold_until - now)).await;
    }
    if paper { paper_sell(&pool, mint, tokens, size).await } else { live_sell(&pool, mint, tokens, size).await };
    snipe_test::count_result(&mint.to_string());
    // The pool's price at +30 ... +600 s after the fill, then the line.
    let last = fill_ms + snipe_test::PRICE_OFFSETS[4] * 1000 + 2_000;
    let now = now_unix_ms();
    if last > now {
        tokio::time::sleep(Duration::from_millis(last - now)).await;
    }
    if let Some((_, opened, states, _)) = snapshot(&pool) {
        let prices = snipe_test::PRICE_OFFSETS.map(|s| Some(price_at(&opened, &states, fill_ms + s * 1000)));
        snipe_test::note_grad(&mint.to_string(), |g| g.prices = prices);
    }
    forget(&pool);
    snipe_test::position_done(&mint.to_string());
}

fn record(action: &'static str, mint: &Pubkey) -> TradeRecord {
    TradeRecord { action, dex: "pumpswap".into(), mint: mint.to_string(), ..Default::default() }
}

/// Paper: the buy lands `LAND_SLOTS` after the migration (or the slot after the stream's, if the
/// decision came later) at that slot's worst pool state.
async fn paper_buy(pool: &Pubkey, mint: Pubkey, mig_slot: u64, size: u64, min_out: u64) -> Option<(u64, u64)> {
    let land = (mig_slot + LAND_SLOTS).max(STREAM_SLOT.load(Ordering::Relaxed) + 1);
    past(land, LAND_WAIT).await;
    let (_, opened, states, market) = snapshot(pool)?;
    let at = worst(&opened, &states, land, true);
    let tokens = market_at(&market, &at).quote_buy(size).unwrap_or(0);
    // The insiders' jump before our fill: the pool at 1.5x its opening in any state up to the landing slot.
    let max_x = states.iter().filter(|s| s.slot <= land).map(|s| s.price() / opened.price().max(f64::MIN_POSITIVE)).fold(0.0f64, f64::max);
    snipe_test::note_grad(&mint.to_string(), |g| {
        g.jump_before_fill = Some(max_x >= JUMP_X);
        g.max_x_before_fill = Some(max_x);
    });
    let mut r = record("BUY", &mint);
    r.sol_in = Some(lamports_to_sol(size));
    r.reason = "GRADUATION".into();
    r.launch_slot = Some(mig_slot);
    r.landed_slot = Some(land);
    let charge = TRADE_SETTING.dry_run_charge_fees;
    if tokens < min_out || tokens == 0 {
        r.status = format!("lost race: price moved past the {:.0}% guard before the buy landed (x{:.3} of the decision's price)", PRICE_GUARD * 100.0, at.price() / snapshot_price(&opened, &states, mig_slot));
        r.token_amount = Some(0);
        if charge {
            r.wallet_sol_change = Some(-(crate::service::modeled_failed_cost_lamports(SendProfile::Buy) as f64) / 1e9);
        }
        log_trade(&r);
        activity(ActivityKind::BuyFailed, Some(DEX), Some(&mint.to_string()), format!("Paper graduation buy failed: {}", r.status));
        return None;
    }
    r.status = "simulated".into();
    r.token_amount = Some(tokens);
    if charge {
        r.wallet_sol_change = Some(-((size + crate::service::modeled_cost_lamports(SendProfile::Buy)) as f64) / 1e9);
    }
    log_trade(&r);
    activity(ActivityKind::Bought, Some(DEX), Some(&mint.to_string()), format!("Paper graduation buy: {tokens} tokens for {} SOL in slot {land} (migration + {})", lamports_to_sol(size), land - mig_slot));
    info!("graduation {mint}: paper buy {tokens} tokens in slot {land} at {:.3e} (pool {pool})", at.price());
    Some((tokens, now_unix_ms()))
}

fn snapshot_price(opened: &State, states: &[State], mig_slot: u64) -> f64 {
    slot_end(opened, states, mig_slot).price().max(f64::MIN_POSITIVE)
}

/// Paper: the sell lands two slots after the stream's slot at that slot's worst pool state; a
/// sell past the sell guard is tried again (as a live one would be).
async fn paper_sell(pool: &Pubkey, mint: Pubkey, tokens: u64, cost: u64) {
    let charge = TRADE_SETTING.dry_run_charge_fees;
    for attempt in 1..=SELL_TRIES {
        let land = STREAM_SLOT.load(Ordering::Relaxed) + LAND_SLOTS;
        let Some((_, opened, states, market)) = snapshot(pool) else { return };
        let expected = market_at(&market, &slot_end(&opened, &states, land - 1)).quote_sell(tokens).unwrap_or(0);
        past(land, LAND_WAIT).await;
        let Some((_, opened, states, market)) = snapshot(pool) else { return };
        let at = worst(&opened, &states, land, false);
        let out = market_at(&market, &at).quote_sell(tokens).unwrap_or(0);
        let mut r = record("SELL", &mint);
        r.sol_in = Some(lamports_to_sol(cost));
        r.token_amount = Some(tokens);
        r.reason = "TIME240".into();
        r.landed_slot = Some(land);
        if out == 0 || out < TRADE_SETTING.min_out(expected) {
            r.status = format!("simulated: price fell past the sell guard before it landed (try {attempt})");
            if charge {
                r.wallet_sol_change = Some(-(crate::service::modeled_failed_cost_lamports(SendProfile::Sell) as f64) / 1e9);
            }
            log_trade(&r);
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        r.status = "paper".into();
        r.sol_out = Some(lamports_to_sol(out));
        r.pnl_percent = Some(crate::utils::auto_sell::pnl_percent(cost, out));
        if charge {
            r.wallet_sol_change = Some((out as f64 - crate::service::modeled_cost_lamports(SendProfile::Sell) as f64) / 1e9);
        }
        log_trade(&r);
        activity(ActivityKind::Sold, Some(DEX), Some(&mint.to_string()), format!("Paper graduation sell (240 s): {} SOL, {:+.2}%", lamports_to_sol(out), crate::utils::auto_sell::pnl_percent(cost, out)));
        return;
    }
    warn!("graduation {mint}: paper sell failed {SELL_TRIES} times; tokens left unsold");
}

/// Live: built and sent through the bot's services (SendProfile::Buy), landing awaited.
async fn live_buy(pool: &Pubkey, mint: Pubkey, mig_slot: u64, market: &PumpSwapMarket, size: u64, min_out: u64) -> Option<(u64, u64)> {
    let wallet = trading_wallet();
    let Some((_, opened, states, _)) = snapshot(pool) else { return None };
    let current = market_at(market, &slot_end(&opened, &states, u64::MAX));
    let ata = get_associated_token_address(&wallet, &mint, &market.base_token_program);
    let mut r = record("BUY", &mint);
    r.sol_in = Some(lamports_to_sol(size));
    r.reason = "GRADUATION".into();
    r.launch_slot = Some(mig_slot);
    let ixs = match current.buy_instructions(&wallet, size, min_out) {
        Ok(i) => i,
        Err(e) => {
            warn!("graduation {mint}: buy not built: {e:#}");
            return None;
        }
    };
    let signature = match crate::service::submit_watching(SendProfile::Buy, ixs, Some(mig_slot), Some(ata)).await {
        Ok(crate::service::Submitted::Sent { signature, .. }) => signature,
        Ok(_) => return None,
        Err(e) => {
            warn!("graduation {mint}: buy not sent: {e:#}");
            return None;
        }
    };
    r.signature = signature.to_string();
    let (outcome, _) = super::common::await_landing(&signature, Duration::from_secs(60)).await.unwrap_or((ConfirmOutcome::TimedOut, None));
    let tokens = get_token_balance(&RPC_CLIENT, &ata).await.unwrap_or(0);
    r.wallet_sol_change = fee_payer_lamport_change(&RPC_CLIENT, &signature).await.ok().map(|l| l as f64 / 1e9);
    r.token_amount = Some(tokens);
    let (status, landed) = match &outcome {
        ConfirmOutcome::Confirmed { slot } => (format!("confirmed (slot {slot})"), Some(*slot)),
        ConfirmOutcome::Failed { slot, error } => (format!("failed: {error}"), Some(*slot)),
        ConfirmOutcome::TimedOut if tokens > 0 => ("confirmed late".into(), None),
        ConfirmOutcome::TimedOut => ("not landed".into(), None),
    };
    r.status = status;
    r.landed_slot = landed;
    if let Some(land) = landed {
        let max_x = states.iter().filter(|s| s.slot <= land).map(|s| s.price() / opened.price().max(f64::MIN_POSITIVE)).fold(0.0f64, f64::max);
        snipe_test::note_grad(&mint.to_string(), |g| {
            g.jump_before_fill = Some(max_x >= JUMP_X);
            g.max_x_before_fill = Some(max_x);
        });
    }
    log_trade(&r);
    if tokens == 0 {
        activity(ActivityKind::BuyFailed, Some(DEX), Some(&mint.to_string()), format!("Graduation buy {}", r.status));
        return None;
    }
    activity(ActivityKind::Bought, Some(DEX), Some(&mint.to_string()), format!("Graduation buy: {tokens} tokens for {} SOL", lamports_to_sol(size)));
    Some((tokens, now_unix_ms()))
}

/// Live: all the tokens, on the pool as the stream has it now, sent through the bot's services;
/// tried again (on a fresh read of the pool) when it fails.
async fn live_sell(pool: &Pubkey, mint: Pubkey, tokens: u64, cost: u64) {
    let wallet = trading_wallet();
    for attempt in 1..=SELL_TRIES {
        let Some((_, opened, states, market)) = snapshot(pool) else { return };
        let mut current = market_at(&market, &slot_end(&opened, &states, u64::MAX));
        if attempt > 1 {
            let _ = current.refresh(&RPC_CLIENT).await;
        }
        let expected = current.quote_sell(tokens).unwrap_or(0);
        let mut r = record("SELL", &mint);
        r.sol_in = Some(lamports_to_sol(cost));
        r.token_amount = Some(tokens);
        r.reason = "TIME240".into();
        let ixs = match current.sell_instructions(&wallet, tokens, TRADE_SETTING.min_out(expected)) {
            Ok(i) => i,
            Err(e) => {
                warn!("graduation {mint}: sell not built: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let signature = match crate::service::submit_with(SendProfile::Sell, ixs, None).await {
            Ok(crate::service::Submitted::Sent { signature, .. }) => signature,
            Ok(_) => return,
            Err(e) => {
                warn!("graduation {mint}: sell not sent: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        r.signature = signature.to_string();
        let (outcome, _) = super::common::await_landing(&signature, Duration::from_secs(45)).await.unwrap_or((ConfirmOutcome::TimedOut, None));
        r.wallet_sol_change = fee_payer_lamport_change(&RPC_CLIENT, &signature).await.ok().map(|l| l as f64 / 1e9);
        let ok = matches!(outcome, ConfirmOutcome::Confirmed { .. });
        r.status = match &outcome {
            ConfirmOutcome::Confirmed { .. } => "confirmed".into(),
            ConfirmOutcome::Failed { error, .. } => format!("failed: {error}"),
            ConfirmOutcome::TimedOut => "timeout".into(),
        };
        if ok {
            // The SOL out: the wallet's change plus the fees (approximately; the rent comes back too).
            r.sol_out = r.wallet_sol_change.map(|w| w.max(0.0));
        }
        log_trade(&r);
        if ok {
            activity(ActivityKind::Sold, Some(DEX), Some(&mint.to_string()), "Graduation sold (240 s)");
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    warn!("graduation {mint}: sell failed {SELL_TRIES} times; tokens left in the wallet");
    activity(ActivityKind::SellFailed, Some(DEX), Some(&mint.to_string()), "Graduation sell failed 5 times: tokens left in the wallet");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fast_completion_passes_and_a_slow_one_is_named() {
        let mut c = HashMap::new();
        let m = Pubkey::new_unique();
        note_curve(&mut c, m, 50_000_000_000, 100, 1_000);
        note_curve(&mut c, m, 59_000_000_000, 100, 2_000);
        note_curve(&mut c, m, 70_000_000_000, 100, 2_500);
        note_curve(&mut c, m, 85_000_000_000, 0, 3_900);
        assert_eq!(completion_skip(c.get(&m).copied()), Ok(1_900), "under 60 SOL at 2.0 s, complete at 3.9 s");
        let slow = Pubkey::new_unique();
        note_curve(&mut c, slow, 59_000_000_000, 100, 1_000);
        note_curve(&mut c, slow, 80_000_000_000, 100, 2_000);
        note_curve(&mut c, slow, 85_000_000_000, 0, 3_100);
        assert!(completion_skip(c.get(&slow).copied()).unwrap_err().contains("2.1 s"));
        assert!(completion_skip(None).is_err());
        let never = Pubkey::new_unique();
        note_curve(&mut c, never, 85_000_000_000, 0, 5_000);
        assert!(completion_skip(c.get(&never).copied()).unwrap_err().contains("never seen under 60"));
        // Trades after the completion don't move it.
        note_curve(&mut c, m, 85_000_000_000, 0, 9_000);
        assert_eq!(c[&m].completed_ms, Some(3_900));
    }

    fn st(slot: u64, idx: u64, ms: u64, quote: u64) -> State {
        State { slot, idx, ms, base: 1_000_000, quote, vq: 0 }
    }

    #[test]
    fn the_slot_end_and_the_worst_state_of_a_landing_slot() {
        let opened = st(100, 5, 0, 100);
        let states = vec![st(100, 9, 10, 120), st(100, 40, 20, 140), st(101, 3, 30, 200), st(102, 1, 40, 150), st(102, 7, 50, 260), st(102, 9, 60, 180), st(103, 0, 70, 90)];
        assert_eq!(slot_end(&opened, &states, 100).quote, 140, "the migration slot's last trade");
        assert_eq!(slot_end(&opened, &[], 100).quote, 100, "no trade: the opening");
        // Landing in 102: it starts at 101's 200; its trades bring 150, 260, 180.
        assert_eq!(worst(&opened, &states, 102, true).quote, 260, "a buy fills at the highest price");
        assert_eq!(worst(&opened, &states, 102, false).quote, 150, "a sell at the lowest");
        assert_eq!(worst(&opened, &states, 104, true).quote, 90, "a quiet slot: the state it starts with");
        assert!((price_at(&opened, &states, 35) - 200.0 / 1e6).abs() < 1e-12);
        assert!((price_at(&opened, &states, 0) - 100.0 / 1e6).abs() < 1e-12, "before any trade: the opening");
    }
}
