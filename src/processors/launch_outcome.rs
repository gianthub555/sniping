//! Watches every followed deployer's launch (bought, skipped or lost) for its first
//! `deployer_record::outcome_slots()` slots on the stream, then records whether a buy right behind
//! the create's bundle would have been sold NO_POP (`rules::NO_POP_RECORD_*`); then on to
//! `launch_results::result_slots()` for the scorecard's "earlier launches" facts: the replayed result
//! of a 0.5 SOL buy behind the bundle and whether it went down (`utils::launch_results`).

use {
    super::common::cpi_event_payloads,
    crate::{
        instructions::{pumpfun_buy_ix::TRADE_EVENT_DISCRIMINATOR, pumpswap_buy_ix::PUMP_PROGRAM_ID},
        utils::{
            dashboard::now_unix,
            deployer_record::{self, LaunchTrade},
            launch_results,
            parse::{read_pubkey, read_u64},
        },
    },
    carbon_core::transaction::TransactionMetadata,
    log::info,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicUsize, Ordering},
            LazyLock, Mutex,
        },
        time::Duration,
    },
};

/// Longest wait for the stream to get past a launch's window.
const OUTCOME_WAIT: Duration = Duration::from_secs(10);
const RESULT_WAIT: Duration = Duration::from_secs(70);

struct Tracked {
    deployer: Pubkey,
    creator: Pubkey,
    launch_signature: Signature,
    /// The create's (slot, block position) and the curve after it, once the stream showed it.
    create: Option<(u64, u64, (u64, u64))>,
    /// Other trades: slot, position, is_buy, SOL, curve after.
    trades: Vec<(u64, u64, bool, u64, u64, u64)>,
    seen: HashSet<Signature>,
}

static TRACKED: LazyLock<Mutex<HashMap<Pubkey, Tracked>>> = LazyLock::new(Default::default);
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Starts watching a followed deployer's launch (once per mint).
pub fn track(mint: Pubkey, deployer: Pubkey, creator: Pubkey, launch_signature: Signature, launch_slot: u64) {
    {
        let mut tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
        if tracked.contains_key(&mint) {
            return;
        }
        tracked.insert(mint, Tracked { deployer, creator, launch_signature, create: None, trades: Vec::new(), seen: HashSet::new() });
        COUNT.store(tracked.len(), Ordering::Relaxed);
    }
    tokio::spawn(async move {
        let created = now_unix();
        // The windows in slots at the measured slot time (their real-time length stays fixed).
        let (outcome_slots, result_slots) = (deployer_record::outcome_slots(), launch_results::result_slots());
        // Past the no-buyers window: every trade of its slots has been seen.
        crate::utils::latency::wait_for_stream(launch_slot + outcome_slots + 1, || Some(0), OUTCOME_WAIT).await;
        let Some((deployer, create, trades)) = snapshot(&mint) else { return };
        let Some((create_slot, create_index, create_after)) = create else {
            let mut tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
            tracked.remove(&mint);
            COUNT.store(tracked.len(), Ordering::Relaxed);
            info!("launch outcome {mint}: the stream never showed the create; not recorded");
            return;
        };
        let no_pop = deployer_record::no_pop_behind_bundle(create_index, create_after, &window(&trades, create_slot, outcome_slots));
        let now = now_unix();
        deployer_record::note(&deployer, &mint, now, no_pop);
        let (launches, no_pops) = deployer_record::record_of(&deployer, now);
        info!(
            "launch outcome {mint}: {} (deployer {}: {no_pops} of its last {launches} launches got no buyers)",
            if no_pop { "no buyers (NO_POP)" } else { "buyers came" },
            deployer
        );
        // On to the result window (the exit's lifetime), then the replayed result.
        crate::utils::latency::wait_for_stream(create_slot + result_slots + 1, || Some(0), RESULT_WAIT).await;
        let done = {
            let mut tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
            let done = tracked.remove(&mint);
            COUNT.store(tracked.len(), Ordering::Relaxed);
            done
        };
        let Some(t) = done else { return };
        let trades = window(&t.trades, create_slot, result_slots);
        if let Some(result) = launch_results::replay(create_index, create_after, &trades) {
            launch_results::note(&mint, &t.deployer, &t.creator, created, result, no_pop);
            info!(
                "launch result {mint}: {:+.4} SOL for 0.5 SOL behind the bundle{} (creator {})",
                result.pnl_sol,
                if result.down { ", went under 0.9x before its best" } else { "" },
                t.creator
            );
        }
    });
}

/// The tracked launch's deployer, create and trades so far (it stays tracked).
#[allow(clippy::type_complexity)]
fn snapshot(mint: &Pubkey) -> Option<(Pubkey, Option<(u64, u64, (u64, u64))>, Vec<(u64, u64, bool, u64, u64, u64)>)> {
    let tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    let t = tracked.get(mint)?;
    Some((t.deployer, t.create, t.trades.clone()))
}

/// The trades of the create's slot and the `slots` after it.
fn window(trades: &[(u64, u64, bool, u64, u64, u64)], create_slot: u64, slots: u64) -> Vec<LaunchTrade> {
    trades
        .iter()
        .filter(|(slot, ..)| *slot >= create_slot && *slot <= create_slot + slots)
        .map(|&(slot, index, is_buy, sol, vsol, vtok)| LaunchTrade { slot_off: slot - create_slot, index, is_buy, sol, vsol, vtok })
        .collect()
}

pub fn is_tracking() -> bool {
    COUNT.load(Ordering::Relaxed) > 0
}

pub fn observe_trades(tx: &TransactionMetadata) {
    if !is_tracking() {
        return;
    }
    let payloads = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    if payloads.is_empty() {
        return;
    }
    let mut tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    let mut counted: HashSet<Pubkey> = HashSet::new();
    for body in payloads {
        let Ok(mint) = read_pubkey(&body, 0) else { continue };
        let Some(t) = tracked.get_mut(&mint) else { continue };
        let (Ok(sol), Ok(vq), Ok(vt)) = (read_u64(&body, 32), read_u64(&body, 89), read_u64(&body, 97)) else { continue };
        let index = tx.index.unwrap_or(u64::MAX);
        if tx.signature == t.launch_signature {
            // The create's own trades: where the launch landed and the curve after the last one.
            t.create = Some((tx.slot, index, (vq, vt)));
            continue;
        }
        // One trade per transaction and mint (a transaction can reach us more than once).
        if t.seen.contains(&tx.signature) && !counted.contains(&mint) {
            continue;
        }
        t.seen.insert(tx.signature);
        counted.insert(mint);
        t.trades.push((tx.slot, index, body.get(48) == Some(&1), sol, vq, vt));
    }
}
