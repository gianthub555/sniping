//! Recent blockhash for every transaction the bot builds. Sources, each in a lock-free cell (A5):
//! RPC (refreshed every second, `confirmed`), and a backup: Helius LaserStream's block meta (two
//! blocks behind the newest, `utils::slot_feed`; since 8 Oct) or a shred-derived "safe" hash (a
//! couple of slots behind the newest, from the in-process engine or pumpfeed's local API),
//! whichever is fresher. The RPC hash is used while it is at most `FALLBACK_AFTER` old; past that (the RPC is
//! slow or down) the shred hash takes over while it is fresh; failing both, an RPC hash up to
//! `MAX_BLOCKHASH_AGE` old.

use {
    crate::hot::clock::now_ns,
    anyhow::{bail, Result},
    log::{info, warn},
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::{
        sync::{
            atomic::{fence, AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    },
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);
/// Past this an RPC hash is still valid on chain (150 slots, ~60 s) but suspiciously old.
const MAX_BLOCKHASH_AGE: Duration = Duration::from_secs(30);
/// The RPC hash is preferred while at most this old.
const FALLBACK_AFTER: Duration = Duration::from_secs(2);
/// A shred hash is used while at most this old (it is a few slots behind the tip when fetched).
const SHRED_HASH_MAX_AGE: Duration = Duration::from_secs(10);
const PUMPFEED_POLL: Duration = Duration::from_millis(200);

/// A 32-byte hash with the time it was current, written by one thread and read by any without a
/// lock (a sequence counter: odd while a write is in progress).
pub struct HashCell {
    seq: AtomicU64,
    words: [AtomicU64; 4],
    at_ns: AtomicU64,
    slot: AtomicU64,
}

impl HashCell {
    const fn new() -> Self {
        Self { seq: AtomicU64::new(0), words: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)], at_ns: AtomicU64::new(0), slot: AtomicU64::new(0) }
    }

    /// Stores `hash`, current at `at_ns` (`hot::clock::now_ns`). One writer per cell.
    pub fn store(&self, hash: &Hash, at_ns: u64, slot: u64) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let bytes: &[u8] = hash.as_ref();
        for (i, w) in self.words.iter().enumerate() {
            w.store(u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap()), Ordering::Relaxed);
        }
        self.at_ns.store(at_ns, Ordering::Relaxed);
        self.slot.store(slot, Ordering::Relaxed);
        self.seq.store(s + 2, Ordering::Release);
    }

    /// The hash, when it was current and its slot (0 if unknown); None before the first store.
    #[inline]
    pub fn load(&self) -> Option<(Hash, u64, u64)> {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None;
            }
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let mut bytes = [0u8; 32];
            for (i, w) in self.words.iter().enumerate() {
                bytes[i * 8..i * 8 + 8].copy_from_slice(&w.load(Ordering::Relaxed).to_le_bytes());
            }
            let at = self.at_ns.load(Ordering::Relaxed);
            let slot = self.slot.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Some((Hash::new_from_array(bytes), at, slot));
            }
        }
    }
}

pub static RPC_HASH: HashCell = HashCell::new();
pub static SHRED_HASH: HashCell = HashCell::new();
/// From Helius LaserStream's block meta (`utils::slot_feed`), two blocks behind the newest: the
/// backup in place of the shreds' (8 Oct).
pub static GRPC_HASH: HashCell = HashCell::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Rpc,
    Shreds,
    Grpc,
}

/// The blockhash to sign with now, and where it came from. Lock- and allocation-free.
#[inline]
pub fn current() -> Option<(Hash, Source, Duration)> {
    // The backup: the fresher of LaserStream's and the shreds'.
    let (grpc, shred) = (GRPC_HASH.load(), SHRED_HASH.load());
    let (backup, source) = match (grpc, shred) {
        (Some(g), Some(s)) if s.1 > g.1 => (Some(s), Source::Shreds),
        (Some(g), _) => (Some(g), Source::Grpc),
        (None, s) => (s, Source::Shreds),
    };
    choose(RPC_HASH.load(), backup, now_ns()).map(|(h, s, age)| (h, if s == Source::Shreds { source } else { s }, age))
}

/// The choice `current` makes between an RPC and a shred hash at time `now`.
#[inline]
fn choose(rpc: Option<(Hash, u64, u64)>, shred: Option<(Hash, u64, u64)>, now: u64) -> Option<(Hash, Source, Duration)> {
    let age = |at: u64| Duration::from_nanos(now.saturating_sub(at));
    if let Some((hash, at, _)) = rpc {
        if age(at) <= FALLBACK_AFTER {
            return Some((hash, Source::Rpc, age(at)));
        }
    }
    if let Some((hash, at, _)) = shred {
        if age(at) <= SHRED_HASH_MAX_AGE {
            return Some((hash, Source::Shreds, age(at)));
        }
    }
    rpc.filter(|(_, at, _)| age(*at) <= MAX_BLOCKHASH_AGE).map(|(hash, at, _)| (hash, Source::Rpc, age(at)))
}

static CURRENT_EPOCH: AtomicU64 = AtomicU64::new(0);
const EPOCH_REFRESH_EVERY: u32 = 60;

pub fn current_epoch() -> u64 {
    CURRENT_EPOCH.load(Ordering::Relaxed)
}

pub async fn recent_blockhash_handler(rpc_client: Arc<RpcClient>) {
    let mut iteration = 0u32;
    let mut failures = 0u32;
    loop {
        if iteration.is_multiple_of(EPOCH_REFRESH_EVERY) {
            match rpc_client.get_epoch_info().await {
                Ok(info) => CURRENT_EPOCH.store(info.epoch, Ordering::Relaxed),
                Err(e) => warn!("failed to refresh epoch: {e}"),
            }
        }
        iteration = iteration.wrapping_add(1);
        let asked = now_ns();
        match rpc_client.get_latest_blockhash_with_commitment(CommitmentConfig::confirmed()).await {
            Ok((hash, _)) => {
                failures = 0;
                RPC_HASH.store(&hash, asked, 0);
            }
            Err(e) => {
                failures += 1;
                // One warning at the first failure, then one a minute (it was 3,600 an hour while
                // the RPC was down on 1 Oct).
                if failures == 1 || failures % 60 == 0 {
                    warn!("failed to refresh blockhash ({failures} in a row): {e}");
                }
            }
        }
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

#[derive(serde::Deserialize)]
struct PumpfeedHash {
    slot: u64,
    hash: String,
    /// Milliseconds, with a fraction (pumpfeed sends e.g. 160.347354).
    age_ms: f64,
}

#[derive(serde::Deserialize)]
struct PumpfeedBlockhash {
    safe: Option<PumpfeedHash>,
}

/// Polls pumpfeed's `GET /blockhash` (its `safe` hash, from shreds) into `SHRED_HASH`. With
/// `compare_only` (the engine-merge parity run: the in-process engine writes `SHRED_HASH`) it only
/// checks pumpfeed's hash against the in-process one for the same slot.
pub async fn pumpfeed_blockhash_handler(api_addr: String, compare_only: bool) {
    let url = format!("http://{api_addr}/blockhash");
    let mut failures = 0u32;
    let first = AtomicBool::new(true);
    loop {
        let asked = now_ns();
        let result = async {
            let reply: PumpfeedBlockhash = crate::config::HTTP_CLIENT.get(&url).timeout(Duration::from_secs(2)).send().await?.json().await?;
            let safe = reply.safe.ok_or_else(|| anyhow::anyhow!("no safe hash yet"))?;
            let bytes: [u8; 32] = bs58::decode(&safe.hash).into_vec()?.try_into().map_err(|_| anyhow::anyhow!("hash is not 32 bytes"))?;
            Ok::<_, anyhow::Error>((Hash::new_from_array(bytes), safe.slot, safe.age_ms))
        }
        .await;
        match result {
            Ok((hash, slot, age_ms)) => {
                failures = 0;
                if compare_only {
                    crate::hot::shreds::book::compare(slot, &hash.to_bytes());
                } else {
                    SHRED_HASH.store(&hash, asked.saturating_sub((age_ms.max(0.0) * 1e6) as u64), slot);
                }
                if first.swap(false, Ordering::Relaxed) {
                    info!("blockhash fallback: pumpfeed's shred hash at {url} (slot {slot})");
                }
            }
            Err(e) => {
                failures += 1;
                if failures == 1 || failures % 300 == 0 {
                    warn!("blockhash fallback: pumpfeed at {url} not answering ({failures} in a row): {e:#}");
                }
            }
        }
        // Back off while it is down: 200 ms, then up to 5 s.
        let wait = if failures < 3 { PUMPFEED_POLL } else { Duration::from_secs(5) };
        tokio::time::sleep(wait).await;
    }
}

pub fn get_recent_blockhash() -> Result<Hash> {
    match current() {
        Some((hash, _, _)) => Ok(hash),
        None => match RPC_HASH.load() {
            Some((_, at, _)) => bail!(
                "cached blockhash is stale ({:.1}s old) and pumpfeed has none; check RPC_ENDPOINT",
                (now_ns().saturating_sub(at)) as f64 / 1e9
            ),
            None => bail!("no blockhash fetched yet"),
        },
    }
}

pub async fn wait_for_blockhash(timeout: Duration) -> Result<Hash> {
    let started = Instant::now();
    loop {
        if let Ok(hash) = get_recent_blockhash() {
            return Ok(hash);
        }
        if started.elapsed() > timeout {
            bail!("could not fetch a blockhash within {timeout:?}; check RPC_ENDPOINT");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rpc_hash_wins_while_fresh_then_the_shred_hash_takes_over() {
        let cell_rpc = HashCell::new();
        let cell_shred = HashCell::new();
        assert!(cell_rpc.load().is_none());
        let (a, b) = (Hash::new_from_array([1; 32]), Hash::new_from_array([2; 32]));
        cell_rpc.store(&a, 100, 0);
        cell_shred.store(&b, 200, 7);
        assert_eq!(cell_rpc.load(), Some((a, 100, 0)));
        assert_eq!(cell_shred.load(), Some((b, 200, 7)));
        // The choice between them, at a moment 100 s into the run.
        let now = 100_000_000_000u64;
        let sec = 1_000_000_000u64;
        let (r, sh) = (|at: u64| Some((a, at, 0)), |at: u64| Some((b, at, 9)));
        assert_eq!(choose(r(now), sh(now), now).map(|c| (c.0, c.1)), Some((a, Source::Rpc)));
        assert_eq!(choose(r(now - 3 * sec), sh(now), now).map(|c| (c.0, c.1)), Some((b, Source::Shreds)), "RPC 3 s old: the shred hash");
        assert_eq!(choose(r(now - 3 * sec), sh(now - 20 * sec), now).map(|c| (c.0, c.1)), Some((a, Source::Rpc)), "both old: the RPC hash while under 30 s");
        assert_eq!(choose(r(now - 40 * sec), sh(now - 20 * sec), now), None, "nothing usable");
        assert_eq!(choose(None, sh(now - sec), now).map(|c| c.1), Some(Source::Shreds), "RPC down from the start");
    }

    #[test]
    fn readers_never_see_a_torn_hash() {
        static CELL: HashCell = HashCell::new();
        let writer = std::thread::spawn(|| {
            for i in 0..200_000u64 {
                let b = (i as u8).wrapping_add(1);
                CELL.store(&Hash::new_from_array([b; 32]), i, i);
            }
        });
        for _ in 0..200_000 {
            if let Some((h, at, slot)) = CELL.load() {
                let bytes: &[u8] = h.as_ref();
                assert!(bytes.iter().all(|x| *x == bytes[0]), "torn read");
                assert_eq!(at, slot);
            }
        }
        writer.join().unwrap();
    }
}
