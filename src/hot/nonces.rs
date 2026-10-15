//! The nonce accounts' manager (2026-10-12): drives `nonce_pool` for the fan-out of buys
//! (HOT_NONCE_FANOUT) and sells (HOT_SELL_FANOUT). Off the hot path, on the runtime:
//! - keeps a few free accounts reserved in a lock-free ring for the decision thread (`Grant`),
//!   which reports each one it used on a second ring (`Used`): no lock on the hot path;
//! - takes landings and re-bids from the after-send tasks (`landed`, `resent`) and hands accounts
//!   to the sells (`acquire`);
//! - reads accounts at `confirmed` from a node at the slot that matters, and sends the advances
//!   that cancel unlanded copies (live only), again every 2 s until they settle.
//!
//! Live: the accounts in `nonce_accounts.txt` (authority: the wallet), each advanced once at start.
//! DRY_RUN: synthetic accounts (nothing on chain); a paper copy can never land (signed by the
//! throwaway paper key on an account that doesn't exist), so each is released at once.

use {
    super::{clock::now_ns, nonce_pool::{NoncePool, Slot, Todo}, spsc},
    crate::{
        config::{RPC_CLIENT, TRADE_SETTING},
        utils::nonce,
    },
    log::{info, warn},
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex, OnceLock,
        },
        time::Duration,
    },
};

/// Reserved accounts kept in the decision thread's ring.
pub const GRANTS: usize = 2;
const TICK: Duration = Duration::from_millis(25);

/// A free account handed to the decision thread: its copies advance `account` and carry `value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub index: u8,
    pub account: Pubkey,
    pub value: Hash,
}

/// The decision thread used a grant for copy `copy`; `sent`: live copies went out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    pub index: u8,
    pub copy: u64,
    pub sent: bool,
}

#[derive(Debug)]
enum Event {
    Landed { index: u8, copy: u64, slot: u64 },
    Resent { index: u8, copy: u64 },
    Sent { index: u8, copy: u64 },
    Unsent { index: u8, copy: u64 },
}

struct Manager {
    live: bool,
    accounts: Vec<Pubkey>,
    pool: Mutex<NoncePool>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
}

static MANAGER: OnceLock<Manager> = OnceLock::new();
/// The decision thread's ends, taken once by the engine.
static DECIDER_ENDS: Mutex<Option<(spsc::Consumer<Grant>, spsc::Producer<Used>)>> = Mutex::new(None);
static COPIES: AtomicU64 = AtomicU64::new(1 << 40);

pub fn hot_fanout() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| crate::config::env_opt("HOT_NONCE_FANOUT").is_some_and(|v| v.eq_ignore_ascii_case("true")));
    *ON
}

pub fn sell_fanout() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| crate::config::env_opt("HOT_SELL_FANOUT").is_some_and(|v| v.eq_ignore_ascii_case("true")));
    *ON
}

/// A fresh copy id (the sells' and the cancels' own; the decision thread counts its own).
pub fn next_copy() -> u64 {
    COPIES.fetch_add(1, Ordering::Relaxed)
}

/// The decision thread's ring ends (once).
pub fn take_decider_ends() -> Option<(spsc::Consumer<Grant>, spsc::Producer<Used>)> {
    DECIDER_ENDS.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Paper's synthetic accounts: distinct keys nothing on chain uses.
fn synthetic(i: usize) -> Pubkey {
    let mut b = [0xC3u8; 32];
    b[0] = i as u8;
    b[31] = 0x3C;
    Pubkey::new_from_array(b)
}

/// Starts the manager when a fan-out switch is on (on the runtime). Live: the listed accounts,
/// read and advanced once before use; none listed: the fan-out stays off (the old paths send).
pub async fn start() {
    if !(hot_fanout() || sell_fanout()) || MANAGER.get().is_some() {
        return;
    }
    let live = !TRADE_SETTING.dry_run;
    let accounts: Vec<Pubkey> = if live {
        match nonce::listed() {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => {
                warn!("nonce fan-out: no accounts in {} (the owner creates them): buys and sells go out the usual way", nonce::NONCE_FILE);
                return;
            }
            Err(e) => {
                warn!("nonce fan-out: {e:#}; off");
                return;
            }
        }
    } else {
        (0..8).map(synthetic).collect()
    };
    let mut pool = NoncePool::new(accounts.len());
    if !live {
        for i in 0..accounts.len() {
            pool.on_value(i as u8, [0; 32], 0);
        }
    }
    let (events, rx) = tokio::sync::mpsc::unbounded_channel();
    let (grant_tx, grant_rx) = spsc::channel::<Grant>(GRANTS * 2);
    let (used_tx, used_rx) = spsc::channel::<Used>(64);
    *DECIDER_ENDS.lock().unwrap_or_else(|e| e.into_inner()) = Some((grant_rx, used_tx));
    let n = accounts.len();
    if MANAGER.set(Manager { live, accounts, pool: Mutex::new(pool), events }).is_err() {
        return;
    }
    if live {
        // Read every account, then advance each once before its first use.
        read_all().await;
        MANAGER.get().expect("set").pool.lock().unwrap_or_else(|e| e.into_inner()).advance_first();
    }
    info!(
        "nonce fan-out: {n} {} accounts (buys {}, sells {}); a copy not landed within 2 s is cancelled{}",
        if live { "nonce" } else { "synthetic (DRY_RUN)" },
        if hot_fanout() { "on" } else { "off" },
        if sell_fanout() { "on" } else { "off" },
        if live { ", each account advanced once before its first use" } else { "" }
    );
    tokio::spawn(run(rx, grant_tx, used_rx));
}

async fn read_all() {
    let Some(m) = MANAGER.get() else { return };
    if let Ok(r) = RPC_CLIENT.get_multiple_accounts_with_commitment(&m.accounts, CommitmentConfig::confirmed()).await {
        let slot = r.context.slot;
        let mut pool = m.pool.lock().unwrap_or_else(|e| e.into_inner());
        for (i, acc) in r.value.iter().enumerate() {
            match acc.as_ref().and_then(|a| nonce::parse(&a.data)) {
                Some((authority, value)) if authority == *crate::config::PUBKEY => pool.on_value(i as u8, value.to_bytes(), slot),
                Some((authority, _)) => warn!("nonce account {} belongs to {authority}, not the wallet: left out", m.accounts[i]),
                None => warn!("{} is not an initialized nonce account: left out", m.accounts[i]),
            }
        }
    }
}

async fn run(mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>, mut grants: spsc::Producer<Grant>, mut used: spsc::Consumer<Used>) {
    let Some(m) = MANAGER.get() else { return };
    let mut tick = tokio::time::interval(TICK);
    let mut busy: HashSet<(u8, bool)> = HashSet::new();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<(u8, bool)>();
    let mut in_ring = 0usize;
    loop {
        tick.tick().await;
        let now = now_ns() as i64;
        while let Ok((index, cancel)) = done_rx.try_recv() {
            busy.remove(&(index, cancel));
        }
        let mut todo = Vec::new();
        {
            let mut pool = m.pool.lock().unwrap_or_else(|e| e.into_inner());
            // What the decision thread used first: a landing reported later finds it in use.
            while let Some(u) = used.pop() {
                in_ring = in_ring.saturating_sub(1);
                pool.used(u.index, u.copy, u.sent, now);
                if !u.sent {
                    pool.release_unsent(u.index, u.copy);
                }
            }
            while let Ok(e) = rx.try_recv() {
                match e {
                    Event::Landed { index, copy, slot } => pool.landed(index, copy, slot, now),
                    Event::Resent { index, copy } => pool.resent(index, copy, now),
                    Event::Sent { index, copy } => pool.mark_sent(index, copy),
                    Event::Unsent { index, copy } => {
                        pool.release_unsent(index, copy);
                    }
                }
            }
            if hot_fanout() {
                while in_ring < GRANTS {
                    let Some((index, value)) = pool.reserve() else { break };
                    if grants.push(Grant { index, account: m.accounts[index as usize], value: Hash::new_from_array(value) }) {
                        in_ring += 1;
                    } else {
                        // Ring full (cannot happen at this size): give it back.
                        pool.unreserve(index);
                        break;
                    }
                }
            }
            if m.live {
                pool.due(now, &mut todo);
            }
        }
        for t in todo {
            match t {
                Todo::Read { index, min_slot } if busy.insert((index, false)) => {
                    let done = done_tx.clone();
                    tokio::spawn(async move {
                        read(index, min_slot).await;
                        let _ = done.send((index, false));
                    });
                }
                Todo::Cancel { index, copy, value } if busy.insert((index, true)) => {
                    let done = done_tx.clone();
                    tokio::spawn(async move {
                        cancel(index, copy, value).await;
                        let _ = done.send((index, true));
                    });
                }
                _ => {}
            }
        }
    }
}

/// Account `index`'s value (confirmed) from a node at `min_slot` or later.
async fn fetch(index: u8, min_slot: u64) -> Option<([u8; 32], u64)> {
    let m = MANAGER.get()?;
    let config = solana_rpc_client_api::config::RpcAccountInfoConfig {
        encoding: Some(solana_account_decoder_client_types::UiAccountEncoding::Base64),
        commitment: Some(CommitmentConfig::confirmed()),
        min_context_slot: Some(min_slot),
        ..Default::default()
    };
    // A node not yet at `min_slot` answers "Minimum context slot has not been reached": asked again
    // every 200 ms for up to 5 s, it gets there (confirmed trails the landing by ~1 s). Giving up at
    // once left the account in use: on 15 Oct the pool had no free account 120 times in an hour.
    for attempt in 0..25 {
        match RPC_CLIENT.get_ui_account_with_config(&m.accounts[index as usize], config.clone()).await {
            Ok(r) => return r.value.and_then(|a| a.data.decode()).and_then(|d| nonce::parse(&d)).map(|(_, v)| (v.to_bytes(), r.context.slot)),
            Err(e) if attempt < 24 && min_slot_not_reached(&format!("{e:#}")) => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => {
                warn!("nonce account {}: {e:#}", m.accounts[index as usize]);
                return None;
            }
        }
    }
    None
}

/// Whether an RPC error is the node not having reached the request's minimum context slot yet.
fn min_slot_not_reached(error: &str) -> bool {
    error.contains("Minimum context slot has not been reached") || error.contains("-32016")
}

async fn read(index: u8, min_slot: u64) {
    if let (Some((value, slot)), Some(m)) = (fetch(index, min_slot).await, MANAGER.get()) {
        m.pool.lock().unwrap_or_else(|e| e.into_inner()).on_value(index, value, slot);
    }
}

/// Sends an advance for account `index` (a copy signed with `value` didn't land, or the live
/// start's), waits for it to land, and settles it with a read at or after its slot.
async fn cancel(index: u8, copy: u64, _value: [u8; 32]) {
    let Some(m) = MANAGER.get() else { return };
    if !m.live {
        return;
    }
    let account = m.accounts[index as usize];
    let Some((blockhash, _, _)) = crate::utils::blockhash::current() else { return };
    let ixs = vec![crate::utils::utils::set_compute_unit_price(100_000), nonce::advance_instruction(&account, &crate::config::PUBKEY)];
    let message = solana_message::Message::new_with_blockhash(&ixs, Some(&crate::config::PUBKEY), &blockhash);
    let tx = match solana_transaction::versioned::VersionedTransaction::try_new(solana_message::VersionedMessage::Legacy(message), &[crate::config::PAYER.as_ref()]) {
        Ok(tx) => tx,
        Err(e) => {
            warn!("nonce cancel for {account}: {e}");
            return;
        }
    };
    m.pool.lock().unwrap_or_else(|e| e.into_inner()).cancel_sent(index, copy, now_ns() as i64);
    let signature = match crate::service::send_rpc(&tx).await {
        Ok(s) => s,
        Err(e) => {
            warn!("nonce cancel for {account}: {e:#}");
            return;
        }
    };
    // Its landing (or not: the pool asks again in 2 s).
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let Ok(statuses) = RPC_CLIENT.get_signature_statuses(&[signature]).await else { continue };
        let Some(Some(status)) = statuses.value.first() else { continue };
        if status.satisfies_commitment(CommitmentConfig::confirmed()) {
            if let Some((read, _)) = fetch(index, status.slot).await {
                let mut pool = m.pool.lock().unwrap_or_else(|e| e.into_inner());
                if copy == 0 {
                    pool.stale_settled(index, read);
                } else {
                    pool.cancel_settled(index, copy, read);
                }
            }
            return;
        }
    }
}

fn event(e: Event) {
    if let Some(m) = MANAGER.get() {
        let _ = m.events.send(e);
    }
}

/// One of copy `copy`'s transactions (a buy or a sell on account `index`) landed in `slot`.
pub fn landed(index: u8, copy: u64, slot: u64) {
    event(Event::Landed { index, copy, slot });
}

/// A sell's copy on `index` was signed again with a higher fee (its cancel timer restarts).
pub fn resent(index: u8, copy: u64) {
    event(Event::Resent { index, copy });
}

/// A sell's copies on `index` went out (from now on cancelled if they don't land).
pub fn sent(index: u8, copy: u64) {
    event(Event::Sent { index, copy });
}

/// A sell's copies on `index` were never sent: the value is unused.
pub fn unsent(index: u8, copy: u64) {
    event(Event::Unsent { index, copy });
}

/// A free account for a sell's copies (copy `copy`, not yet sent): `(index, account, value)`.
pub fn acquire(copy: u64) -> Option<(u8, Pubkey, Hash)> {
    let m = MANAGER.get()?;
    let (index, value) = m.pool.lock().unwrap_or_else(|e| e.into_inner()).acquire(copy, false, now_ns() as i64)?;
    Some((index, m.accounts[index as usize], Hash::new_from_array(value)))
}

/// Whether copy `copy` on account `index` can no longer land: the account moved on (its cancel
/// settled, or a landing was read), or it holds another copy now.
pub fn settled(index: u8, copy: u64) -> bool {
    let Some(m) = MANAGER.get() else { return true };
    match m.pool.lock().unwrap_or_else(|e| e.into_inner()).slot(index) {
        Slot::InUse { copy: c, .. } | Slot::Cancelling { copy: c, .. } | Slot::Advanced { copy: c, .. } => c != copy,
        _ => true,
    }
}

/// Whether the manager runs with real accounts (live).
pub fn is_live() -> bool {
    MANAGER.get().is_some_and(|m| m.live)
}

/// One line for the stats log.
pub fn report() -> Option<String> {
    let m = MANAGER.get()?;
    let pool = m.pool.lock().unwrap_or_else(|e| e.into_inner());
    let s = pool.stats;
    let states = (0..pool.len()).map(|i| match pool.slot(i as u8) {
        Slot::Unknown => 'u',
        Slot::Stale => 's',
        Slot::Free { .. } => 'f',
        Slot::Reserved { .. } => 'r',
        Slot::InUse { .. } => 'i',
        Slot::Cancelling { .. } => 'c',
        Slot::Advanced { .. } => 'a',
    });
    Some(format!(
        "nonces ({}): [{}] acquired {}, none free {}, landed {}, cancels sent {}, cancelled {}, released unsent {}",
        if m.live { "live" } else { "paper" },
        states.collect::<String>(),
        s.acquired,
        s.exhausted,
        s.landed,
        s.cancels_sent,
        s.cancelled,
        s.released_unsent
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_behind_the_slot_is_asked_again_and_other_errors_are_not() {
        assert!(min_slot_not_reached("AccountNotFound: pubkey=72G2d1: RPC response error -32016: Minimum context slot has not been reached; "));
        assert!(min_slot_not_reached("RPC response error -32016: whatever"));
        assert!(!min_slot_not_reached("RPC response error -32602: Invalid param: could not find account"));
        assert!(!min_slot_not_reached("error sending request: connection refused"));
    }
}
