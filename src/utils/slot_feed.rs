//! Slot starts and a backup blockhash from Helius LaserStream (gRPC), in place of what the shred
//! stream (AllenHark) gave (the owner's, 8 Oct). One subscription, `processed`:
//!   - slot updates with the inter-slot statuses: the first update of a slot (normally "first shred
//!     received" from Helius's node, else "bank created" or "processed") marks the slot's start
//!     for the distance rule's last-slot clause (`hot::distance::note_seen`, which keeps the
//!     earliest of every feed's sighting);
//!   - block meta: each block's hash; the one two blocks behind the newest (as the shred book kept
//!     a safe depth) is the backup blockhash (`blockhash::GRPC_HASH`) when RPC's is over 2 s old.
//! Every 5 minutes a line compares the slot starts with the slot's first sighting on the other
//! feeds. HELIUS_SLOT_FEED=off turns it off.

use {
    crate::{
        hot::{clock::now_ns, distance},
        utils::blockhash,
    },
    anyhow::{anyhow, Result},
    futures::{SinkExt, StreamExt},
    log::{info, warn},
    solana_hash::Hash,
    std::{
        collections::{HashMap, VecDeque},
        str::FromStr,
        sync::{
            atomic::{AtomicU64, Ordering::Relaxed},
            LazyLock, Mutex,
        },
        time::Duration,
    },
    yellowstone_grpc_client::GeyserGrpcClient,
    yellowstone_grpc_proto::{
        geyser::{
            subscribe_update::UpdateOneof, CommitmentLevel, SlotStatus, SubscribeRequest, SubscribeRequestFilterBlocksMeta,
            SubscribeRequestFilterSlots, SubscribeRequestPing,
        },
        tonic::transport::ClientTlsConfig,
    },
};

/// Blocks the backup hash stays behind the newest.
const SAFE_DEPTH: u64 = 2;
/// No update for this long: the stream is reconnected.
const STALL: Duration = Duration::from_secs(10);
const REPORT_EVERY: Duration = Duration::from_secs(300);

pub fn enabled() -> bool {
    !crate::config::env_opt("HELIUS_SLOT_FEED").is_some_and(|v| v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false"))
}

/// The last update of any kind (`now_ns`).
pub static LAST_NS: AtomicU64 = AtomicU64::new(0);

/// Seconds a slot takes, measured: the time between slot starts over the slots between them,
/// averaged (EWMA, 1/500 a slot); f64 bits, 0 until measured. Slots shortened from 0.317 s
/// (early Sep) to 0.219 s (13 Oct), so no constant holds.
static SECS_PER_SLOT: AtomicU64 = AtomicU64::new(0);

/// The measured slot time, once at least a few hundred slot starts were seen.
pub fn secs_per_slot() -> Option<f64> {
    let (v, n) = (f64::from_bits(SECS_PER_SLOT.load(Relaxed)), SLOT_SAMPLES.load(Relaxed));
    (n >= 300 && v > 0.0).then_some(v)
}
static SLOT_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// One slot start at `t_ns` after the previous one (`prev`): folds it into the average.
fn note_slot_start(prev: Option<(u64, u64)>, slot: u64, t_ns: u64) {
    let Some((ps, pt)) = prev else { return };
    if slot <= ps || slot - ps > 32 || t_ns <= pt {
        return;
    }
    let d = (t_ns - pt) as f64 / 1e9 / (slot - ps) as f64;
    if !(0.05..1.0).contains(&d) {
        return;
    }
    let old = f64::from_bits(SECS_PER_SLOT.load(Relaxed));
    let new = if old == 0.0 { d } else { old + (d - old) / 500.0 };
    SECS_PER_SLOT.store(new.to_bits(), Relaxed);
    SLOT_SAMPLES.fetch_add(1, Relaxed);
}

#[derive(Default)]
struct Stats {
    /// Slot starts by the status that marked them.
    starts: HashMap<&'static str, u64>,
    /// Slot start minus the slot's first sighting on the other feeds, when they saw it first (ms).
    lags: Vec<f64>,
    /// Slots this feed saw before any other.
    first_here: u64,
    hashes: u64,
}

static STATS: LazyLock<Mutex<Stats>> = LazyLock::new(Default::default);

pub fn spawn(url: String, token: Option<String>) {
    if !enabled() {
        info!("slot feed: off (HELIUS_SLOT_FEED=off)");
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&url, token.clone()).await {
                warn!("slot feed: {e:#}; reconnecting in 2 s");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    tokio::spawn(async {
        let mut tick = tokio::time::interval(REPORT_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            info!("{}", report());
        }
    });
}

fn request() -> SubscribeRequest {
    SubscribeRequest {
        slots: HashMap::from([("slots".to_string(), SubscribeRequestFilterSlots { filter_by_commitment: Some(false), interslot_updates: Some(true) })]),
        blocks_meta: HashMap::from([("blocks".to_string(), SubscribeRequestFilterBlocksMeta {})]),
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    }
}

async fn run(url: &str, token: Option<String>) -> Result<()> {
    let mut client = GeyserGrpcClient::build_from_shared(url.to_string())
        .map_err(|e| anyhow!("invalid gRPC url: {e}"))?
        .x_token(token)
        .map_err(|e| anyhow!("invalid gRPC token: {e}"))?
        .tls_config(ClientTlsConfig::new().with_enabled_roots())?
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .connect()
        .await
        .map_err(|e| anyhow!("connect: {e}"))?;
    let (mut sink, mut stream) = client.subscribe_with_request(Some(request())).await.map_err(|e| anyhow!("subscribe: {e}"))?;
    info!("slot feed: LaserStream slot starts and block hashes subscribed");
    // The slots whose start is noted, and the last blocks' hashes.
    let mut started: VecDeque<u64> = VecDeque::with_capacity(256);
    let mut last_start: Option<(u64, u64)> = None;
    let mut blocks: VecDeque<(u64, Hash)> = VecDeque::with_capacity(16);
    loop {
        let update = tokio::time::timeout(STALL, stream.next())
            .await
            .map_err(|_| anyhow!("no update for {} s", STALL.as_secs()))?
            .ok_or_else(|| anyhow!("stream ended"))?
            .map_err(|e| anyhow!("{e}"))?;
        let t = now_ns();
        LAST_NS.store(t, Relaxed);
        match update.update_oneof {
            Some(UpdateOneof::Slot(u)) => {
                if started.contains(&u.slot) {
                    continue;
                }
                let status = match SlotStatus::try_from(u.status) {
                    Ok(SlotStatus::SlotFirstShredReceived) => "first shred",
                    Ok(SlotStatus::SlotCreatedBank) => "bank created",
                    Ok(SlotStatus::SlotProcessed) => "processed",
                    Ok(SlotStatus::SlotCompleted) => "completed",
                    // Confirmed, finalized and dead slots are old news or no slot at all.
                    _ => continue,
                };
                if started.len() == 256 {
                    started.pop_front();
                }
                started.push_back(u.slot);
                note_slot_start(last_start, u.slot, t);
                if last_start.is_none_or(|(s, _)| u.slot > s) {
                    last_start = Some((u.slot, t));
                }
                let seen = distance::slot_seen(u.slot);
                distance::note_seen(u.slot, t);
                let mut s = STATS.lock().unwrap_or_else(|e| e.into_inner());
                *s.starts.entry(status).or_default() += 1;
                match seen {
                    Some(seen) if s.lags.len() < 100_000 => s.lags.push((t.saturating_sub(seen)) as f64 / 1e6),
                    Some(_) => {}
                    None => s.first_here += 1,
                }
            }
            Some(UpdateOneof::BlockMeta(b)) => {
                let Ok(hash) = Hash::from_str(&b.blockhash) else { continue };
                if blocks.len() == 16 {
                    blocks.pop_front();
                }
                blocks.push_back((b.slot, hash));
                let newest = blocks.iter().map(|(s, _)| *s).max().unwrap_or(b.slot);
                if let Some((slot, h)) = blocks.iter().filter(|(s, _)| *s + SAFE_DEPTH <= newest).max_by_key(|(s, _)| *s) {
                    blockhash::GRPC_HASH.store(h, t, *slot);
                    STATS.lock().unwrap_or_else(|e| e.into_inner()).hashes += 1;
                }
            }
            Some(UpdateOneof::Ping(_)) => {
                sink.send(SubscribeRequest { ping: Some(SubscribeRequestPing { id: 1 }), ..Default::default() }).await.map_err(|e| anyhow!("ping: {e}"))?;
            }
            _ => {}
        }
    }
}

/// One line for the log (and resets the window).
pub fn report() -> String {
    let mut s = STATS.lock().unwrap_or_else(|e| e.into_inner());
    let mut lags = std::mem::take(&mut s.lags);
    let starts = std::mem::take(&mut s.starts);
    let hashes = std::mem::take(&mut s.hashes);
    let first_here = std::mem::take(&mut s.first_here);
    lags.sort_by(f64::total_cmp);
    let q = |p: f64| lags.get(((lags.len() as f64 - 1.0) * p).round() as usize).copied();
    let total: u64 = starts.values().sum();
    let mut by: Vec<String> = starts.iter().map(|(k, v)| format!("{k} {v}")).collect();
    by.sort();
    format!(
        "slot feed, last 5 min: {total} slot starts ({}), {hashes} block hashes; first sighting of the slot here on {first_here}, after another feed on {} (behind by median {:.1} ms, p10 {:.1}, p90 {:.1})",
        by.join(", "),
        lags.len(),
        q(0.5).unwrap_or(0.0),
        q(0.1).unwrap_or(0.0),
        q(0.9).unwrap_or(0.0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot starts 219 ms apart give 0.219 s a slot; gaps of several slots count per slot; a
    /// late or repeated start does not move it.
    #[test]
    fn the_slot_time_is_measured_from_slot_starts() {
        let mut prev = None;
        let (mut slot, mut t) = (1_000u64, 1_000_000_000u64);
        for i in 0..900u64 {
            let step = if i % 10 == 0 { 2 } else { 1 }; // a skipped slot now and then
            slot += step;
            t += 219_000_000 * step;
            note_slot_start(prev, slot, t);
            prev = Some((slot, t));
        }
        let v = secs_per_slot().expect("measured");
        assert!((v - 0.219).abs() < 0.01, "{v}");
        note_slot_start(prev, prev.unwrap().0, prev.unwrap().1 + 5_000_000_000);
        assert!((secs_per_slot().unwrap() - v).abs() < 1e-12, "the same slot again changes nothing");
    }
}
