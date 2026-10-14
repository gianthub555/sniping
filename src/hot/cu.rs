//! A1: the buy's compute limit, per buy path. Schedulers rank by fee per compute unit, so the limit
//! asked for should be what the buy uses plus a margin: measured usage × 1.10. The paths are the
//! two kinds of pump.fun coin: SPL Token mints (`create`) and Token-2022 mints (`create_v2`), whose
//! token-account create and transfers cost different compute.
//!
//! Usage is measured by simulating the hot path's own buy (the compute budget, the lanes' tips, the
//! loaded-data limit, the token-account create and `buy_exact_sol_in`, as `template::instructions`
//! lays them out) against launches the stream just showed, through RPC with signature checks off.
//! The limit of each path is the largest usage measured under the current program deployment ×
//! 1.10 (coins of one path differ), kept in `logs/cu_calibration.json` across restarts. A side task re-simulates when pump.fun's program is upgraded (its program-data
//! account's deploy slot changes) and every six hours, and alerts (log and dashboard) when a path's
//! usage moves more than 15%. The hot path reads the limits with one atomic load (`limit_for`).

use {
    crate::{
        config::{trading_wallet, RPC_CLIENT, TRADE_SETTING},
        instructions::{pumpfun_buy_ix::PumpFunMarket, pumpswap_buy_ix::PUMP_PROGRAM_ID},
        utils::{
            dashboard::{activity, ActivityKind},
            utils::{set_compute_unit_limit, set_compute_unit_price, set_loaded_accounts_data_size_limit, transfer_sol, TOKEN_2022_PROGRAM_ID},
        },
    },
    log::{info, warn},
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicU32, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
};

pub const CALIBRATION_FILE: &str = "logs/cu_calibration.json";
/// Measured usage × MARGIN, rounded up to 100 CU.
pub const MARGIN: f64 = 1.10;
/// Alert when a path's measured usage moves more than this between calibrations.
pub const ALERT_MOVE: f64 = 0.15;
/// The priority fee the before/after comparison is quoted at (the default bid, lamports).
const QUOTE_FEE_LAMPORTS: u64 = 700_000;
const PATHS: [&str; 2] = ["SPL Token (create)", "Token-2022 (create_v2)"];
const MIN_SAMPLES: usize = 3;
const MAX_SAMPLES: usize = 12;

/// Per path (0: SPL Token, 1: Token-2022): the limit in force; 0 until calibrated (the spec's then).
static LIMITS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PathCal {
    pub measured_max: u32,
    pub measured_p50: u32,
    pub samples: u32,
    pub limit: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Calibration {
    pub paths: [PathCal; 2],
    /// pump.fun's program-data deploy slot the calibration ran under.
    pub program_slot: u64,
    pub at: String,
}

static CAL: LazyLock<Mutex<Calibration>> = LazyLock::new(Default::default);
/// Launches the stream just showed: (mint, path, when).
static RECENT: LazyLock<Mutex<VecDeque<(Pubkey, usize, Instant)>>> = LazyLock::new(Default::default);

fn path_of(token_program: &Pubkey) -> usize {
    usize::from(*token_program == TOKEN_2022_PROGRAM_ID)
}

/// The compute limit for a buy of a coin under `token_program`; 0 until calibrated. One load.
#[inline]
pub fn limit_for(token_program: &Pubkey) -> u32 {
    LIMITS[path_of(token_program)].load(Ordering::Relaxed)
}

/// A create the regular stream showed (its curve exists): a candidate to simulate against.
pub fn note_launch(mint: Pubkey, token_program: Pubkey) {
    let mut r = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    if r.len() >= 64 {
        r.pop_front();
    }
    r.push_back((mint, path_of(&token_program), Instant::now()));
}

/// Measured usage × MARGIN, rounded up to 100 CU (pure).
pub fn limit_from(measured: u32) -> u32 {
    // × 11 / 10 in integers (MARGIN), then up to the next 100.
    ((measured as u64 * 11).div_ceil(1_000) * 100) as u32
}

/// Micro-lamports per CU that `fee` lamports buy at `limit` (pure).
pub fn fee_per_cu(fee: u64, limit: u32) -> u64 {
    (fee as u128 * 1_000_000 / limit.max(1) as u128) as u64
}

/// Whether usage moved more than ALERT_MOVE between two measurements (pure).
pub fn moved(before: u32, after: u32) -> bool {
    before > 0 && (after as f64 - before as f64).abs() / before as f64 > ALERT_MOVE
}

/// Compute units the hot path's buy of `mint` uses, by simulation.
async fn simulate(mint: Pubkey) -> anyhow::Result<u32> {
    let market = PumpFunMarket::from_mint(&RPC_CLIENT, mint).await?;
    let payer = trading_wallet();
    let lanes = super::lanes::LaneConfig::from_env();
    let mut ixs = vec![set_compute_unit_limit(1_400_000), set_compute_unit_price(1)];
    for (list, lamports) in lanes.tips() {
        if let Some(account) = list.first() {
            ixs.push(transfer_sol(&payer, account, lamports));
        }
    }
    if let Some(bytes) = TRADE_SETTING.loaded_data_limit_bytes {
        ixs.push(set_loaded_accounts_data_size_limit(bytes));
    }
    ixs.extend(market.buy_instructions(&payer, 100_000, 1)?);
    let message = solana_message::Message::new(&ixs, Some(&payer));
    let tx = solana_transaction::versioned::VersionedTransaction {
        signatures: vec![solana_signature::Signature::default()],
        message: solana_message::VersionedMessage::Legacy(message),
    };
    let result = RPC_CLIENT
        .simulate_transaction_with_config(
            &tx,
            solana_rpc_client_api::config::RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                commitment: Some(solana_commitment_config::CommitmentConfig::processed()),
                ..Default::default()
            },
        )
        .await?
        .value;
    if let Some(err) = result.err {
        anyhow::bail!("simulation failed: {err:?}");
    }
    result.units_consumed.map(|u| u as u32).ok_or_else(|| anyhow::anyhow!("no units in the simulation"))
}

/// pump.fun's program-data deploy slot (it changes on every upgrade).
async fn program_slot() -> anyhow::Result<u64> {
    let program = RPC_CLIENT.get_account(&PUMP_PROGRAM_ID).await?;
    let data_address: [u8; 32] = program.data.get(4..36).ok_or_else(|| anyhow::anyhow!("not an upgradeable program"))?.try_into()?;
    let data = RPC_CLIENT.get_account_data(&Pubkey::new_from_array(data_address)).await?;
    Ok(u64::from_le_bytes(data.get(4..12).ok_or_else(|| anyhow::anyhow!("short program data"))?.try_into()?))
}

fn save(cal: &Calibration) {
    if let Ok(json) = serde_json::to_string_pretty(cal) {
        if crate::store::db_on() {
            crate::store::write(crate::store::Row::Exec {
                sql: "INSERT INTO kv (key, value, updated_ns) VALUES ('cu_calibration', ?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?1, updated_ns = ?2",
                params: vec![json.clone().into(), crate::store::now_ns().into()],
            });
        }
        if crate::store::files_on() {
            let _ = std::fs::write(CALIBRATION_FILE, json);
        }
    }
}

fn load() {
    let text = if crate::store::read_from_db() { crate::store::read::kv("cu_calibration") } else { std::fs::read_to_string(CALIBRATION_FILE).ok() };
    let Some(cal) = text.and_then(|t| serde_json::from_str::<Calibration>(&t).ok()) else { return };
    for (i, p) in cal.paths.iter().enumerate() {
        if p.limit > 0 {
            LIMITS[i].store(p.limit, Ordering::Relaxed);
        }
    }
    info!(
        "compute limits from {CALIBRATION_FILE} ({}): {}",
        cal.at,
        PATHS.iter().zip(cal.paths).map(|(n, p)| format!("{n} {}", if p.limit > 0 { p.limit.to_string() } else { "not measured".into() })).collect::<Vec<_>>().join(", ")
    );
    *CAL.lock().unwrap_or_else(|e| e.into_inner()) = cal;
}

/// Simulates recent launches of each path and sets its limit. `why` names the occasion.
pub async fn calibrate(why: &str, program: u64) {
    let default = TRADE_SETTING.compute_unit_limit;
    let candidates: Vec<(Pubkey, usize)> = {
        let r = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        r.iter().rev().filter(|(_, _, at)| (Duration::from_secs(3)..Duration::from_secs(300)).contains(&at.elapsed())).map(|(m, p, _)| (*m, *p)).collect()
    };
    let mut cal = CAL.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for path in 0..2 {
        let mut units = Vec::new();
        for (mint, _) in candidates.iter().filter(|(_, p)| *p == path).take(MAX_SAMPLES) {
            match simulate(*mint).await {
                Ok(u) => units.push(u),
                Err(e) => info!("compute limit: simulating a buy of {mint}: {e:#}"),
            }
        }
        if units.len() < MIN_SAMPLES {
            if !units.is_empty() || path == 1 {
                info!("compute limit {}: {} simulations ({why}), {MIN_SAMPLES} needed; limit unchanged", PATHS[path], units.len());
            }
            continue;
        }
        units.sort_unstable();
        let (sample_max, p50) = (*units.last().expect("non-empty"), units[units.len() / 2]);
        let before = cal.paths[path];
        // Coins of one path differ (Token-2022 buys used 102.4k CU in one sample and 91.3k in the
        // next, same program): within a program deployment the limit follows the largest usage
        // ever measured; an upgrade starts over.
        let same_program = program != 0 && program == cal.program_slot;
        let max = if same_program { sample_max.max(before.measured_max) } else { sample_max };
        let new = limit_from(max);
        let old = if before.limit > 0 { before.limit } else { default };
        info!(
            "compute limit {} ({why}): measured {} simulations, p50 {p50} CU, max {sample_max} CU (largest under this program {max}); limit {old} -> {new} CU; at {} SOL of priority fee the bid goes {:.2} -> {:.2} lamports/CU ({:+.1}%)",
            PATHS[path],
            units.len(),
            QUOTE_FEE_LAMPORTS as f64 / 1e9,
            fee_per_cu(QUOTE_FEE_LAMPORTS, old) as f64 / 1e6,
            fee_per_cu(QUOTE_FEE_LAMPORTS, new) as f64 / 1e6,
            100.0 * (old as f64 / new as f64 - 1.0)
        );
        if moved(before.measured_max, sample_max) {
            let text = format!(
                "pump.fun buy usage moved {:+.0}% ({} -> {sample_max} CU, {}{}): compute limit now {new}",
                100.0 * (sample_max as f64 / before.measured_max as f64 - 1.0),
                before.measured_max,
                PATHS[path],
                if same_program { ", same program: another kind of coin" } else { "" }
            );
            warn!("COMPUTE ALERT: {text}");
            activity(ActivityKind::Settings, None, None, format!("Alert: {text}"));
        }
        cal.paths[path] = PathCal { measured_max: max, measured_p50: p50, samples: units.len() as u32, limit: new };
        LIMITS[path].store(new, Ordering::Relaxed);
    }
    cal.program_slot = program;
    cal.at = format!("{} {}", super::positions::today(), why);
    save(&cal);
    *CAL.lock().unwrap_or_else(|e| e.into_inner()) = cal;
}

/// The calibration in force.
pub fn calibration() -> Calibration {
    CAL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Loads the last calibration, then keeps the limits measured (on the runtime): a first run a
/// minute after start, again on every pump.fun upgrade and every six hours.
pub fn spawn() {
    load();
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut program = program_slot().await.unwrap_or(0);
        calibrate("start", program).await;
        let mut last = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            match program_slot().await {
                Ok(slot) if slot != program && program != 0 => {
                    warn!("pump.fun's program was upgraded (deploy slot {program} -> {slot}): re-simulating the buy");
                    program = slot;
                    calibrate("program upgrade", slot).await;
                    last = Instant::now();
                }
                Ok(slot) => program = slot,
                Err(e) => info!("compute limit: reading pump.fun's program data: {e:#}"),
            }
            if last.elapsed() > Duration::from_secs(6 * 3600) {
                calibrate("six-hourly", program).await;
                last = Instant::now();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_usage_plus_ten_percent_rounded_up() {
        assert_eq!(limit_from(93_230), 102_600);
        assert_eq!(limit_from(100_000), 110_000);
        assert_eq!(fee_per_cu(700_000, 120_000), 5_833_333);
        assert_eq!(fee_per_cu(700_000, 102_600), 6_822_612);
    }

    #[test]
    fn an_alert_needs_a_move_over_fifteen_percent() {
        assert!(!moved(0, 200_000), "nothing to compare with");
        assert!(!moved(100_000, 115_000));
        assert!(moved(100_000, 115_001));
        assert!(moved(100_000, 84_000));
    }
}
