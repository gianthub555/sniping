//! Part B: what follows a hot-path decision.
//!
//! - **Who buys.** With the hot path live, it is the only buyer of the launches it decides: it
//!   records every decision here (`note`), and the slow path, just before it would buy, waits for
//!   that record (`wait`) and buys only a launch the hot path left to it (one that needed an RPC
//!   answer, or whose create the hot decoder could not read). A launch is never bought twice.
//! - **After the send** (`run`, on the async runtime): a live buy is followed to its landing, logged
//!   like the slow path's (`trades.csv`, the dashboard) and handed to the sell side
//!   (`spawn_position`); every hot buy, live or not, goes to the position logger (`positions`).
//! - **The resend guard** (B3, off unless `HOT_RESEND_BEFORE_CREATE=true`): a buy that failed on
//!   chain because the coin did not exist yet (it executed before the create: `Custom(3012)`,
//!   AccountNotInitialized) is sent once more. A failed transaction can never land later, and
//!   nothing else is resent, so it cannot buy twice (`guard_step`, tested). Every such failure is
//!   counted, as a share of all live buys, whether the guard is on or not.

use {
    super::{
        decide::{Plan, Reject},
        decode::Launch,
        positions,
    },
    crate::{
        config::{lamports_to_sol, trading_wallet},
        instructions::{pumpfun_buy_ix::PumpFunMarket, types::Market},
        processors::common::{self, PositionSlot},
        utils::{
            auto_sell::{spawn_position, Position},
            csv_logger::{log_trade, TradeRecord},
            dashboard::{activity, bump, ActivityKind, STATS},
            utils::{fee_payer_lamport_change, get_associated_token_address, get_token_balance, ConfirmOutcome},
        },
    },
    log::{info, warn},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, LazyLock, Mutex, OnceLock,
        },
        time::{Duration, Instant},
    },
};

/// The hot path sends real buys (FAST_PATH, HOT_LIVE, DRY_RUN off): set once at start.
static LIVE: AtomicBool = AtomicBool::new(false);

pub fn set_live() {
    LIVE.store(true, Ordering::SeqCst);
}

pub fn is_live() -> bool {
    LIVE.load(Ordering::Relaxed)
}

// ---- who buys --------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Decision {
    /// The hot path sent the buy.
    Bought = 1,
    /// The hot path declined the launch (the rules, a limit, or too late).
    Declined = 2,
    /// The hot path could not decide (it needed an RPC answer): the slow path decides and buys.
    Left = 3,
}

impl Decision {
    /// What a decision outcome means for the slow path.
    pub fn of_reject(r: Reject) -> Decision {
        match r {
            // The hot decoder could not read what it needs: the slow path reads it its own way.
            Reject::LayoutUnknown | Reject::DevBuyHidden | Reject::ConfigMissing => Decision::Left,
            _ => Decision::Declined,
        }
    }
}

const DECISIONS: usize = 4096;

#[derive(Clone, Copy)]
struct DecisionSlot {
    mint: [u8; 32],
    decision: u8,
}

/// Direct-mapped by mint: a decision is read within milliseconds, long before 4096 launches pass.
static TABLE: LazyLock<Mutex<Box<[DecisionSlot]>>> =
    LazyLock::new(|| Mutex::new(vec![DecisionSlot { mint: [0; 32], decision: 0 }; DECISIONS].into_boxed_slice()));

fn slot_of(mint: &[u8; 32]) -> usize {
    (u64::from_le_bytes(mint[..8].try_into().expect("8 bytes")) as usize) & (DECISIONS - 1)
}

/// Builds the table (call at startup, off the hot path).
pub fn init() {
    LazyLock::force(&TABLE);
}

/// Records the hot path's decision on a launch. No allocation.
#[inline]
pub fn note(mint: &Pubkey, decision: Decision) {
    let m = mint.to_bytes();
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    t[slot_of(&m)] = DecisionSlot { mint: m, decision: decision as u8 };
}

pub fn decision(mint: &Pubkey) -> Option<Decision> {
    let m = mint.to_bytes();
    let t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let s = t[slot_of(&m)];
    if s.mint != m {
        return None;
    }
    match s.decision {
        1 => Some(Decision::Bought),
        2 => Some(Decision::Declined),
        3 => Some(Decision::Left),
        _ => None,
    }
}

/// The hot path's decision on `mint`, waiting up to `max` for it (it decides 0.25 ms after the
/// create arrives; the slow path gets there milliseconds later).
pub async fn wait(mint: &Pubkey, max: Duration) -> Option<Decision> {
    let until = Instant::now() + max;
    loop {
        if let Some(d) = decision(mint) {
            return Some(d);
        }
        if Instant::now() >= until {
            return None;
        }
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
}

// ---- the hand-off ----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A real buy.
    Live,
    /// DRY_RUN: a buy the decision made (the slow path paper-trades it), sent, if at all, as an
    /// unlandable copy.
    Paper,
    /// DRY_RUN: a launch the bot would not buy, built and sent as an unlandable copy to time the
    /// lanes (at most one every `HOT_DRY_SAMPLE_MS`).
    Sample,
}

/// A hot-path buy, handed over after its bytes went out.
pub struct HotBuy {
    pub kind: Kind,
    pub launch: Launch,
    pub plan: Option<Plan>,
    /// The launch's facts as the decision saw them (the snipe test's line).
    pub facts: Option<crate::config::rules::LaunchFacts>,
    /// The coin's accounts as derived by the hot path.
    pub bonding_curve: Pubkey,
    pub creator: Pubkey,
    pub create_slot: u64,
    pub signature: Signature,
    /// HOT_NONCE_FANOUT: each lane's own copy (signature) and its tip, and the nonce account and
    /// copy id they share (at most one lands).
    pub copies: [Option<Signature>; super::stamps::LANES],
    pub copy_tips: [u64; super::stamps::LANES],
    pub nonce: Option<(u8, u64)>,
    /// Priority fee per compute unit (micro-lamports), the compute limit, and the tips paid.
    pub cu_price: u64,
    pub cu_limit: u32,
    pub tips: u64,
    /// The launch's fee tier (`fee::tier`) and, in the fee ladder, its arm.
    pub tier: u8,
    pub fee_arm: Option<u8>,
    /// Unix ms right after the bytes went out (compared with the regular stream's receive times).
    pub sent_unix_ms: u64,
    /// The highest priority bid seen on rivals' buys before ours, and whether it raised ours.
    pub rival_cu_price: u64,
    pub raised: bool,
    /// Lanes that took the bytes.
    pub lanes: [bool; super::stamps::LANES],
    /// `clock::now_ns` at the create's arrival and at bytes ready.
    pub t_recv: u64,
    pub t_ready: u64,
    pub position_slot: Option<PositionSlot>,
}

static TX: OnceLock<tokio::sync::mpsc::UnboundedSender<HotBuy>> = OnceLock::new();

/// The sender the deciders use (None until `spawn` ran).
pub fn sender() -> Option<tokio::sync::mpsc::UnboundedSender<HotBuy>> {
    TX.get().cloned()
}

/// Starts the task that takes every hot buy after its send. Call on the runtime.
pub fn spawn() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HotBuy>();
    if TX.set(tx).is_err() {
        return;
    }
    tokio::spawn(async move {
        while let Some(b) = rx.recv().await {
            positions::track(&b);
            if b.kind == Kind::Live {
                tokio::spawn(after_send(b));
            }
        }
    });
}

// ---- after a live send -----------------------------------------------------------------------

/// Live buys sent, and how they ended.
pub mod counts {
    use std::sync::atomic::AtomicU64;
    pub static SENT: AtomicU64 = AtomicU64::new(0);
    pub static LANDED: AtomicU64 = AtomicU64::new(0);
    /// Failed on chain because the coin did not exist yet (executed before the create).
    pub static FAILED_BEFORE_CREATE: AtomicU64 = AtomicU64::new(0);
    pub static FAILED_OTHER: AtomicU64 = AtomicU64::new(0);
    pub static NOT_LANDED: AtomicU64 = AtomicU64::new(0);
    pub static RESENT: AtomicU64 = AtomicU64::new(0);
}

/// One line for the stats log.
pub fn report() -> String {
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let sent = g(&counts::SENT);
    let before = g(&counts::FAILED_BEFORE_CREATE);
    format!(
        "live buys: sent {sent}, landed {}, failed before the create {before} ({:.1}% of buys sent), failed otherwise {}, not landed {}, resent {} (resend guard {})",
        g(&counts::LANDED),
        if sent > 0 { 100.0 * before as f64 / sent as f64 } else { 0.0 },
        g(&counts::FAILED_OTHER),
        g(&counts::NOT_LANDED),
        g(&counts::RESENT),
        if resend_enabled() { "on" } else { "off" }
    )
}

/// The error of a buy that executed before the coin's create: its accounts do not exist yet.
pub const BEFORE_CREATE_ERROR: &str = "Custom(3012)";

pub fn resend_enabled() -> bool {
    crate::config::env_opt("HOT_RESEND_BEFORE_CREATE").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// How a live buy ended on chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    Landed,
    FailedBeforeCreate,
    FailedOther,
    NotLanded,
}

impl Landing {
    pub fn of(outcome: &ConfirmOutcome, tokens: u64) -> Landing {
        match outcome {
            _ if tokens > 0 => Landing::Landed,
            ConfirmOutcome::Confirmed { .. } => Landing::Landed,
            ConfirmOutcome::Failed { error, .. } if error.contains(BEFORE_CREATE_ERROR) => Landing::FailedBeforeCreate,
            ConfirmOutcome::Failed { .. } => Landing::FailedOther,
            ConfirmOutcome::TimedOut => Landing::NotLanded,
        }
    }
}

/// The resend guard: after attempt number `attempt` (1 = the first send) ended in `landing`,
/// whether to send once more. Only a definite on-chain failure before the create is resent (that
/// transaction can never land), only once, and only with the guard on.
pub fn guard_step(enabled: bool, attempt: u32, landing: Landing) -> bool {
    enabled && attempt == 1 && landing == Landing::FailedBeforeCreate
}

const BUY_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// The snipe test's record of a live hot buy: what the decision saw.
fn snipe_entry(b: &HotBuy, plan: &Plan) -> crate::utils::snipe_test::Entry {
    use crate::utils::snipe_test::{Entry, Visible};
    let r = plan.reserves;
    let mut e = Entry {
        mint: b.launch.mint.to_string(),
        mode: "live",
        path: "hot",
        deployer: b.launch.deployer.to_string(),
        create_slot: b.create_slot,
        decided_unix_ms: b.sent_unix_ms,
        visible: Visible { dev_buy_lamports: plan.dev_buy_lamports, bundle_wallets: plan.bundle_wallets, real_sol_lamports: r.real_sol_reserves, complete: r.real_token_reserves == 0 },
        bundle_lamports: plan.bundle_lamports,
        price_after_bundle: if r.virtual_token_reserves > 0 { r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64 } else { 0.0 },
        size_lamports: plan.lamports_in,
        token_2022: b.launch.token_program == crate::utils::utils::TOKEN_2022_PROGRAM_ID,
        mayhem: b.launch.mayhem,
        ..Default::default()
    };
    if let Some(f) = &b.facts {
        crate::utils::snipe_test::entry_of_facts(&mut e, f);
    }
    e
}

fn market_of(b: &HotBuy) -> Market {
    let l = &b.launch;
    let reserves = b.plan.map(|p| p.reserves).unwrap_or_default();
    Market::PumpFun(PumpFunMarket::with_curve(l.mint, b.bonding_curve, l.token_program, b.creator, l.mayhem, reserves))
}

/// Follows a live buy to its landing: the trade log, the dashboard, the sell side. Mirrors the
/// slow path's sent-buy branch (`common::buy_once`).
async fn after_send(mut b: HotBuy) {
    let mint = b.launch.mint;
    let Some(plan) = b.plan else { return };
    counts::SENT.fetch_add(1, Ordering::Relaxed);
    let market = market_of(&b);
    let feed = Some(Arc::new(crate::processors::position_feed::subscribe(mint)));
    let ready_ms = b.t_ready.saturating_sub(b.t_recv) as f64 / 1e6;
    info!(
        "HOT BUY SENT {mint}: {} SOL, min {} tokens, bytes ready {:.3} ms after the create arrived, priority {} micro-lamports/CU{} ({})",
        lamports_to_sol(plan.lamports_in),
        plan.min_out,
        ready_ms,
        b.cu_price,
        if b.raised { format!(", raised over a rival's {}", b.rival_cu_price) } else { String::new() },
        b.signature
    );
    if crate::utils::snipe_test::on() && matches!(b.kind, Kind::Live) {
        crate::utils::snipe_test::note_entry(snipe_entry(&b, &plan));
    }
    let mut signature = b.signature;
    // HOT_NONCE_FANOUT: every lane's own copy is watched; the one that landed tells the lane.
    let alternates: Vec<Signature> = b.copies.iter().flatten().copied().filter(|s| *s != b.signature).collect();
    let mut attempt = 1;
    loop {
        let result = common::await_landing_any(signature, &alternates, BUY_CONFIRM_TIMEOUT).await;
        let (outcome, fill) = match result {
            Ok((outcome, fill, landed)) => {
                signature = landed;
                (outcome, fill)
            }
            Err(e) => {
                warn!("hot buy {signature}: {e:#}");
                (ConfirmOutcome::TimedOut, None)
            }
        };
        let tokens = match &fill {
            Some(f) => f.tokens,
            None => {
                let ata = get_associated_token_address(&trading_wallet(), &mint, &b.launch.token_program);
                get_token_balance(&crate::config::RPC_CLIENT, &ata).await.unwrap_or(0)
            }
        };
        let landing = Landing::of(&outcome, tokens);
        match landing {
            Landing::Landed => counts::LANDED.fetch_add(1, Ordering::Relaxed),
            Landing::FailedBeforeCreate => counts::FAILED_BEFORE_CREATE.fetch_add(1, Ordering::Relaxed),
            Landing::FailedOther => counts::FAILED_OTHER.fetch_add(1, Ordering::Relaxed),
            Landing::NotLanded => counts::NOT_LANDED.fetch_add(1, Ordering::Relaxed),
        };
        let landed_slot = match &outcome {
            ConfirmOutcome::Confirmed { slot } | ConfirmOutcome::Failed { slot, .. } => Some(*slot),
            ConfirmOutcome::TimedOut => None,
        };
        // Which lane landed it: with one lane carrying the buy (LANE_ROTATE), that lane; with a
        // gateway and a Jito bundle copy, Jito's bundle status tells which; with every lane carrying
        // the same bytes, only Jito can say it was its.
        let carried: Vec<usize> = (0..super::stamps::LANES).filter(|&i| b.lanes[i]).collect();
        let gateway = carried.iter().copied().find(|&i| i != super::lanes::LANE_JITO);
        // A fanned-out buy: the landed copy is the lane's own, and only its tip was paid.
        let copy_lane = b.copies.iter().position(|c| *c == Some(signature));
        if let (Some((index, copy)), Some(slot)) = (b.nonce, landed_slot) {
            super::nonces::landed(index, copy, slot);
        }
        if let (Some(l), true) = (copy_lane, b.nonce.is_some()) {
            b.tips = b.copy_tips[l];
        }
        let lane = match (landing, landed_slot) {
            (Landing::Landed | Landing::FailedBeforeCreate | Landing::FailedOther, Some(_)) if b.nonce.is_some() && copy_lane.is_some() => super::lanes::LANE_NAMES[copy_lane.unwrap_or(0)],
            (Landing::Landed | Landing::FailedBeforeCreate | Landing::FailedOther, Some(_)) if carried.len() == 1 => super::lanes::LANE_NAMES[carried[0]],
            (Landing::Landed | Landing::FailedBeforeCreate | Landing::FailedOther, Some(slot)) => match super::lanes::jito_landed(&mint.to_bytes(), slot).await {
                Some(true) => "jito",
                Some(false) if carried.len() == 2 => gateway.map_or("not jito", |g| super::lanes::LANE_NAMES[g]),
                Some(false) => "not jito",
                None => "unknown",
            },
            _ => "",
        };
        positions::note_landing(&mint, &signature, landed_slot, landing, lane);
        let mut record = TradeRecord {
            action: "BUY",
            dex: market.dex().to_string(),
            mint: mint.to_string(),
            signature: signature.to_string(),
            sol_in: Some(lamports_to_sol(plan.lamports_in)),
            token_amount: Some(tokens),
            reason: if plan.small_bet { "SNIPE hot small".into() } else { "SNIPE hot".into() },
            launch_slot: Some(b.create_slot),
            landed_slot,
            detect_to_ready_ms: Some(ready_ms),
            ..Default::default()
        };
        record.wallet_sol_change = match (&fill, &outcome) {
            (Some(f), _) => Some(f.wallet_lamports as f64 / 1e9),
            (None, ConfirmOutcome::TimedOut) => None,
            (None, _) => fee_payer_lamport_change(&crate::config::RPC_CLIENT, &signature).await.map(|l| l as f64 / 1e9).ok(),
        };
        record.status = match &outcome {
            ConfirmOutcome::Confirmed { slot } => format!("confirmed (slot {slot})"),
            ConfirmOutcome::Failed { error, .. } => format!("failed: {error}"),
            ConfirmOutcome::TimedOut if tokens > 0 => "confirmed late".into(),
            ConfirmOutcome::TimedOut => "not landed".into(),
        };
        // ASYNC_FILE_WRITES: the trade row is queued after the sell monitor starts (below), not
        // written here first.
        let async_writes = crate::utils::file_writer::enabled();
        if !async_writes {
            log_trade(&record);
        }
        bump(if tokens > 0 { &STATS.buys_ok } else { &STATS.buys_failed });
        let mint_text = mint.to_string();
        let final_attempt = tokens > 0 || !guard_step(resend_enabled(), attempt, landing);
        if final_attempt {
            crate::utils::live_guard::on_buy_result(tokens > 0, landing != Landing::NotLanded, "live");
            tokio::spawn(crate::utils::live_guard::check_wallet());
            // Fees spent (a failed buy pays its priority fee; its tips revert) and the fill breaker.
            let priority = (b.cu_price as u128 * b.cu_limit as u128 / 1_000_000) as u64 + 5_000;
            let spent = match landing {
                Landing::Landed => priority + b.tips,
                Landing::FailedBeforeCreate | Landing::FailedOther => priority,
                Landing::NotLanded => 0,
            };
            let error = match &outcome {
                ConfirmOutcome::Failed { error, .. } => Some(error.as_str()),
                _ => None,
            };
            super::fee::on_send(error, spent);
            if crate::utils::snipe_test::on() {
                let tip = if landing == Landing::Landed { b.tips } else { 0 };
                crate::utils::snipe_test::note_fees(&mint.to_string(), priority, tip);
            }
        }
        if tokens > 0 {
            info!(
                "HOT BOUGHT {mint}: {tokens} tokens (slot {}{})",
                landed_slot.map_or("?".into(), |s| s.to_string()),
                if lane.is_empty() { String::new() } else { format!(", lane {lane}") }
            );
            activity(ActivityKind::Bought, Some(market.dex().name()), Some(&mint_text), format!("Bought {tokens} tokens for {} SOL (hot path)", lamports_to_sol(plan.lamports_in)));
            spawn_position(Position {
                market: market.clone(),
                entry_lamports: plan.lamports_in,
                paper_token_amount: None,
                paper_sol_in_curve: 0,
                known_tokens: Some(tokens),
                opened_at: Instant::now(),
                slot: b.position_slot.take().map(Arc::new),
                feed: feed.clone(),
                fast_rug: b.launch.fast_rug_host,
            });
            if async_writes {
                crate::utils::file_writer::run(move || log_trade(&record));
            }
            return;
        }
        if async_writes {
            let row = record.clone();
            crate::utils::file_writer::run(move || log_trade(&row));
        }
        activity(ActivityKind::BuyFailed, Some(market.dex().name()), Some(&mint_text), format!("Hot buy {}", record.status));
        warn!("hot buy {signature} of {mint}: {}", record.status);
        if !guard_step(resend_enabled(), attempt, landing) {
            return;
        }
        // The resend guard: once more, built and sent the slow path's way (it is off by default).
        attempt += 1;
        counts::RESENT.fetch_add(1, Ordering::Relaxed);
        let Market::PumpFun(m) = &market else { return };
        let ixs = match m.buy_instructions(&trading_wallet(), plan.lamports_in, plan.min_out) {
            Ok(ixs) => ixs,
            Err(e) => {
                warn!("hot buy of {mint}: resend not built: {e:#}");
                return;
            }
        };
        let ata = get_associated_token_address(&trading_wallet(), &mint, &b.launch.token_program);
        match crate::service::submit_watching(crate::service::SendProfile::Buy, ixs, Some(b.create_slot), Some(ata)).await {
            Ok(crate::service::Submitted::Sent { signature: s, .. }) => {
                info!("hot buy of {mint}: resent once after it executed before the create ({s})");
                signature = s;
            }
            Ok(_) => return,
            Err(e) => {
                warn!("hot buy of {mint}: resend failed: {e:#}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resend_guard_resends_only_a_first_failure_before_the_create() {
        use Landing::*;
        for landing in [Landed, FailedBeforeCreate, FailedOther, NotLanded] {
            assert!(!guard_step(false, 1, landing), "off: never");
            for attempt in 2..5 {
                assert!(!guard_step(true, attempt, landing), "never a second resend");
            }
        }
        assert!(guard_step(true, 1, FailedBeforeCreate));
        assert!(!guard_step(true, 1, Landed), "a landed buy is never resent");
        assert!(!guard_step(true, 1, NotLanded), "a buy not seen may still land: never resent");
        assert!(!guard_step(true, 1, FailedOther));
    }

    #[test]
    fn at_most_one_buy_lands_whatever_the_chain_does() {
        // Every sequence of outcomes the chain can give: each attempt either lands, fails before
        // the create, fails otherwise or is not seen. A failed attempt never lands later; one not
        // seen may. Count the attempts that can hold tokens.
        use Landing::*;
        let all = [Landed, FailedBeforeCreate, FailedOther, NotLanded];
        for first in all {
            for second in all {
                let mut attempt = 1;
                let mut can_land = 0;
                let mut landing = first;
                loop {
                    if matches!(landing, Landed | NotLanded) {
                        can_land += 1;
                    }
                    if landing == Landed || !guard_step(true, attempt, landing) {
                        break;
                    }
                    attempt += 1;
                    landing = second;
                }
                assert!(can_land <= 1, "{first:?} then {second:?}: {can_land} buys could land");
                assert!(attempt <= 2);
            }
        }
    }

    #[test]
    fn failures_are_told_apart() {
        let failed = |e: &str| ConfirmOutcome::Failed { slot: 1, error: e.into() };
        assert_eq!(Landing::of(&failed("InstructionError(3, Custom(3012))"), 0), Landing::FailedBeforeCreate);
        assert_eq!(Landing::of(&failed("InstructionError(3, Custom(6042))"), 0), Landing::FailedOther);
        assert_eq!(Landing::of(&ConfirmOutcome::TimedOut, 0), Landing::NotLanded);
        assert_eq!(Landing::of(&ConfirmOutcome::TimedOut, 5), Landing::Landed, "tokens arrived after the wait");
        assert_eq!(Landing::of(&ConfirmOutcome::Confirmed { slot: 1 }, 0), Landing::Landed);
    }

    #[test]
    fn decisions_are_found_by_mint_and_newer_ones_replace_older() {
        init();
        let a = Pubkey::new_unique();
        assert_eq!(decision(&a), None);
        note(&a, Decision::Left);
        assert_eq!(decision(&a), Some(Decision::Left));
        note(&a, Decision::Bought);
        assert_eq!(decision(&a), Some(Decision::Bought));
        assert_eq!(Decision::of_reject(Reject::LayoutUnknown), Decision::Left);
        assert_eq!(Decision::of_reject(Reject::TooLate), Decision::Declined);
    }
}
