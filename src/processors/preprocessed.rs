//! Helius preprocessed stream (`preprocessedSubscribe`, WebSocket): pump.fun transactions decoded
//! from shreds before they execute, 12-18 ms ahead of the processed stream from Frankfurt
//! (measured 24 Sep on 64 creates). Each binary frame is the version (u8), the slot (u64 LE), the
//! first signature (64 bytes), then the wire transaction. Launches go to the shred feed's
//! sighting, quoted after the buys that reach this stream within `BUNDLE_WINDOW` of the create
//! (its bundle: a bundle is atomic, so our buy always lands after it). Measured over 156
//! five-minute reports to 26 Sep 2026: bundle buys arrive a median 0.03-0.09 ms after their
//! create (p90 0.09), and the slowest in a report passed 0.5 ms in 29% of reports. Buys of a
//! launch that arrive after its window (its quote missed them) are counted as `LATE_BUYS`.

use {
    super::{
        shred_feed::{parse_launches, pump_buys, pump_buys_by, sighting, ShredLaunch},
        shred_wire::decode_transaction,
    },
    crate::instructions::pumpswap_buy_ix::{PUMP_FEE_PROGRAM_ID, PUMP_PROGRAM_ID},
    anyhow::{anyhow, bail, Result},
    futures::{SinkExt, StreamExt},
    log::{debug, info, warn},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant, SystemTime},
    },
    tokio_tungstenite::tungstenite::Message,
};

const FRAME_HEADER: usize = 1 + 8 + 64;
/// Wait after a create for its bundle's buys, on a precise timer: Tokio's 1 ms sleep took 2.3 ms.
/// Bundle buys arrive a median 0.04 ms after their create, p90 0.11-0.14 (26 Sep 2026); a buy
/// that comes later only makes the quote stale, and a stale buy fails nearly free (bundles).
pub const BUNDLE_WINDOW: Duration = Duration::from_micros(250);
/// How long after its create a buy of the same slot still counts as one the quote missed.
const LATE_BUY_HORIZON: Duration = Duration::from_millis(5);
const PING_EVERY: Duration = Duration::from_secs(20);

/// A create waiting out the bundle window.
struct Pending {
    launch: ShredLaunch,
    signature: Signature,
    slot: u64,
    received: Instant,
    received_at: SystemTime,
    fee_payer: Option<Pubkey>,
    later_buys: Vec<(u64, u64)>,
    /// The buyer of each buy in `later_buys` (the create's bundle).
    later_buyers: Vec<Option<Pubkey>>,
}

static PENDING: LazyLock<Mutex<HashMap<Pubkey, Pending>>> = LazyLock::new(Default::default);

/// Signatures already handled (a transaction can arrive more than once).
static SEEN: LazyLock<Mutex<(HashSet<Signature>, VecDeque<Signature>)>> = LazyLock::new(Default::default);
const SEEN_CAP: usize = 16_384;

/// How long after its create each bundle buy reached this stream (ms), for tuning the window.
pub static BUNDLE_GAPS_MS: LazyLock<Mutex<Vec<f64>>> = LazyLock::new(Default::default);

/// Launches already quoted, by mint: slot and when the create arrived, for counting late buys.
static QUOTED: LazyLock<Mutex<HashMap<Pubkey, (u64, Instant)>>> = LazyLock::new(Default::default);
/// Buys that reached this stream after their launch's window, within `LATE_BUY_HORIZON`, and how
/// long after the create (ms).
pub static LATE_BUYS_MS: LazyLock<Mutex<Vec<f64>>> = LazyLock::new(Default::default);

/// Every account the stream's pump.fun transactions named in the last `PRE_CREATE_HORIZON`: when
/// it first appeared, in whose transaction (fee payer) and slot. A create whose mint is already
/// here was announced by someone else's transaction before the create itself reached us (a
/// sniper spraying buys at a mint it learned early; 26 Sep: the reference wallet's buys landed
/// 1-379 positions ahead of 18 creates in their slots). Measurement only: nothing trades on it.
static NAMED: LazyLock<Mutex<(HashMap<Pubkey, (Instant, Pubkey, u64)>, VecDeque<(Instant, Pubkey)>)>> = LazyLock::new(Default::default);
const PRE_CREATE_HORIZON: Duration = Duration::from_secs(3);
/// Creates whose mint an earlier transaction named, and how much earlier (ms).
pub static PRE_CREATE_LEADS_MS: LazyLock<Mutex<Vec<f64>>> = LazyLock::new(Default::default);
pub static CREATES_SEEN: AtomicU64 = AtomicU64::new(0);

/// A create's mint named by an earlier transaction: (ms before, that transaction's fee payer, its slot).
fn named_before(mint: &Pubkey, received: Instant) -> Option<(f64, Pubkey, u64)> {
    let named = NAMED.lock().unwrap_or_else(|e| e.into_inner());
    named.0.get(mint).map(|(at, payer, slot)| (received.duration_since(*at).as_secs_f64() * 1000.0, *payer, *slot))
}

fn note_named(keys: impl Iterator<Item = Pubkey>, payer: Pubkey, slot: u64, received: Instant) {
    let mut named = NAMED.lock().unwrap_or_else(|e| e.into_inner());
    let (map, order) = &mut *named;
    while order.front().is_some_and(|(at, _)| received.duration_since(*at) > PRE_CREATE_HORIZON) {
        if let Some((at, key)) = order.pop_front() {
            if map.get(&key).is_some_and(|(first, ..)| *first == at) {
                map.remove(&key);
            }
        }
    }
    for key in keys {
        if let std::collections::hash_map::Entry::Vacant(e) = map.entry(key) {
            e.insert((received, payer, slot));
            order.push_back((received, key));
        }
    }
}

pub static FRAMES: AtomicU64 = AtomicU64::new(0);
pub static BAD_FRAMES: AtomicU64 = AtomicU64::new(0);

fn first_time(signature: &Signature) -> bool {
    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let (set, order) = &mut *guard;
    if !set.insert(*signature) {
        return false;
    }
    order.push_back(*signature);
    if order.len() > SEEN_CAP {
        if let Some(old) = order.pop_front() {
            set.remove(&old);
        }
    }
    true
}

/// `url` carries the API key: it is never logged.
pub fn spawn(url: String) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&url).await {
                warn!("preprocessed stream: {e:#}; reconnecting in 2 s");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn run(url: &str) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async_with_config(url, None, true)
        .await
        .map_err(|e| anyhow!("connect: {}", e.to_string().replace(url, "<url>")))?;
    let (mut write, mut read) = ws.split();
    let subscribe = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "preprocessedSubscribe",
        // pump fees too: a creator's move to fee sharing names pump.fun only through a lookup table
        "params": [{ "accountInclude": [PUMP_PROGRAM_ID.to_string(), PUMP_FEE_PROGRAM_ID.to_string()] }],
    });
    write.send(Message::Text(subscribe.to_string().into())).await?;
    info!("preprocessed stream connected: pump.fun transactions before they execute");
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            message = read.next() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    // The launch's task waits in this worker's LIFO slot until this loop yields;
                    // with frames queued, that is after them. Yield now instead.
                    if frame(&bytes) {
                        tokio::task::yield_now().await;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if text.contains("\"error\"") {
                        bail!("subscription refused: {text}");
                    }
                    debug!("preprocessed stream: {text}");
                }
                Some(Ok(Message::Ping(payload))) => write.send(Message::Pong(payload)).await?,
                Some(Ok(Message::Close(close))) => bail!("closed by the server: {close:?}"),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => bail!("stream ended"),
            },
            _ = ping.tick() => write.send(Message::Ping(Vec::new().into())).await?,
        }
    }
}

/// One frame of the stream. True when it held a launch (a task to run soon was spawned).
pub(crate) fn frame(bytes: &[u8]) -> bool {
    let received = Instant::now();
    let received_at = SystemTime::now();
    FRAMES.fetch_add(1, Ordering::Relaxed);
    let (Some(slot), Some(signature)) = (
        bytes.get(1..9).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes),
        bytes.get(9..FRAME_HEADER).and_then(|b| <[u8; 64]>::try_from(b).ok()).map(Signature::from),
    ) else {
        BAD_FRAMES.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    if !first_time(&signature) {
        return false;
    }
    let Some(tx) = decode_transaction(&bytes[FRAME_HEADER..]) else {
        BAD_FRAMES.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    // A creator's move to fee sharing (pump fees' create_fee_sharing_config) names pump.fun only
    // through its lookup table: read it before the pump.fun filter below, which sees static keys.
    if tx.mentions(&PUMP_FEE_PROGRAM_ID) && !tx.mentions(&PUMP_PROGRAM_ID) {
        for mint in super::shred_feed::creator_migrations_in(&tx.top_instructions()) {
            super::fee_sharing::note(mint);
        }
        return false;
    }
    if !tx.mentions(&PUMP_PROGRAM_ID) {
        return false;
    }
    let mut launched = false;
    let instructions = tx.top_instructions();
    {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        for p in pending.values_mut().filter(|p| p.slot == slot) {
            let buys = pump_buys_by(&instructions, &p.launch.mint);
            if !buys.is_empty() {
                BUNDLE_GAPS_MS.lock().unwrap_or_else(|e| e.into_inner()).push(received.duration_since(p.received).as_secs_f64() * 1000.0);
            }
            for (tokens, lamports, buyer) in buys {
                p.later_buys.push((tokens, lamports));
                p.later_buyers.push(buyer);
            }
        }
    }
    {
        let mut quoted = QUOTED.lock().unwrap_or_else(|e| e.into_inner());
        quoted.retain(|_, (_, at)| at.elapsed() < LATE_BUY_HORIZON);
        for (mint, (quoted_slot, at)) in quoted.iter() {
            if *quoted_slot == slot && !pump_buys(&instructions, mint).is_empty() {
                LATE_BUYS_MS.lock().unwrap_or_else(|e| e.into_inner()).push(received.duration_since(*at).as_secs_f64() * 1000.0);
            }
        }
    }
    // A creator's move to fee sharing (often the transaction right after its create): the buy
    // pays the new creator's vault (`fee_sharing`).
    for mint in super::shred_feed::creator_migrations_in(&instructions) {
        super::fee_sharing::note(mint);
    }
    let payer = tx.keys.first().copied().unwrap_or_default();
    for launch in parse_launches(&instructions) {
        let mint = launch.mint;
        CREATES_SEEN.fetch_add(1, Ordering::Relaxed);
        if let Some((lead_ms, by, by_slot)) = named_before(&mint, received) {
            PRE_CREATE_LEADS_MS.lock().unwrap_or_else(|e| e.into_inner()).push(lead_ms);
            info!(
                "PRE-CREATE {mint}: named {lead_ms:.1} ms before its create by a transaction of {by} (slot {by_slot}, create slot {slot}{})",
                if crate::config::rules::is_reference_wallet(&by) { ", a reference wallet" } else { "" }
            );
        }
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(
            mint,
            Pending {
                launch,
                signature,
                slot,
                received,
                received_at,
                fee_payer: tx.keys.first().copied(),
                later_buys: Vec::new(),
                later_buyers: Vec::new(),
            },
        );
        tokio::spawn(async move {
            crate::utils::latency::precise_sleep(BUNDLE_WINDOW.saturating_sub(received.elapsed())).await;
            let Some(p) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).remove(&mint) else { return };
            QUOTED.lock().unwrap_or_else(|e| e.into_inner()).insert(mint, (p.slot, p.received));
            sighting(&p.launch, p.signature, p.slot, p.received_at, p.received, &p.later_buys, Some(&p.later_buyers), p.fee_payer);
        });
        launched = true;
    }
    // After the launch handling (the time-critical part): every account this transaction names.
    let named = tx.keys.iter().copied().chain(instructions.iter().flat_map(|(_, accounts, _)| accounts.iter().flatten().copied()));
    note_named(named, payer, slot, received);
    launched
}

#[cfg(test)]
mod pre_create_tests {
    /// The real create_fee_sharing_config of 13 Oct names pump.fun only through its lookup table:
    /// the frame still reaches the fee-sharing map (the pump.fun filter used to drop it).
    #[test]
    fn a_creators_move_to_fee_sharing_is_noted_from_a_frame() {
        use base64::Engine;
        let _serial = super::super::fee_sharing::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f: serde_json::Value = serde_json::from_str(include_str!("testdata/pump_fee_sharing.json")).unwrap();
        let mint: Pubkey = f["mint"].as_str().unwrap().parse().unwrap();
        let tx = base64::engine::general_purpose::STANDARD.decode(f["tx_base64"].as_str().unwrap()).unwrap();
        let view = decode_transaction(&tx).unwrap();
        assert!(!view.mentions(&PUMP_PROGRAM_ID) && view.mentions(&PUMP_FEE_PROGRAM_ID), "pump.fun only through the table");
        let mut bytes = vec![0u8];
        bytes.extend(f["slot"].as_u64().unwrap().to_le_bytes());
        bytes.extend(&tx[1..65]);
        bytes.extend(&tx);
        super::super::fee_sharing::forget(&mint);
        assert_eq!(super::super::fee_sharing::creator_of(&mint), None);
        assert!(!frame(&bytes), "no launch in it");
        assert_eq!(
            super::super::fee_sharing::creator_of(&mint).map(|c| c.to_string()).as_deref(),
            Some("3Z9mtCi8ebunhQ8fHTUHEutRvhCUHHZ7yAWHZkVp6695")
        );
    }

    use super::*;

    #[test]
    fn a_mint_named_before_its_create_is_found_with_the_lead_and_the_namer() {
        let (mint, other, payer) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let t0 = Instant::now();
        note_named([mint, other].into_iter(), payer, 7, t0);
        // A later mention does not move the first sighting.
        note_named([mint].into_iter(), Pubkey::new_unique(), 8, t0 + Duration::from_millis(5));
        let (lead, by, slot) = named_before(&mint, t0 + Duration::from_millis(12)).unwrap();
        assert!((lead - 12.0).abs() < 0.01);
        assert_eq!((by, slot), (payer, 7));
        assert!(named_before(&Pubkey::new_unique(), t0).is_none());
        // Past the horizon the entry goes (on the next note).
        note_named(std::iter::empty(), payer, 9, t0 + PRE_CREATE_HORIZON + Duration::from_millis(1));
        assert!(named_before(&mint, t0).is_none());
    }
}
