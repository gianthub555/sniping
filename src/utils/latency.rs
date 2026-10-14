






use {
    crate::config::PUBKEY,
    carbon_core::transaction::TransactionMetadata,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
};


const OWN_TX_TTL: Duration = Duration::from_secs(300);

static LATEST_SLOT: AtomicU64 = AtomicU64::new(0);
/// The furthest (slot, block position) the stream has shown.
static LATEST_POSITION: Mutex<(u64, u64)> = Mutex::new((0, 0));


static OWN_TXS: LazyLock<Mutex<HashMap<Signature, (u64, Instant)>>> = LazyLock::new(Default::default);


pub fn observe(tx: &TransactionMetadata) {
    LATEST_SLOT.fetch_max(tx.slot, Ordering::Relaxed);
    if let Some(index) = tx.index {
        let mut latest = LATEST_POSITION.lock().unwrap_or_else(|e| e.into_inner());
        *latest = (*latest).max((tx.slot, index));
    }
    if tx.fee_payer == *PUBKEY {
        let now = Instant::now();
        let mut own = OWN_TXS.lock().unwrap_or_else(|e| e.into_inner());
        own.retain(|_, (_, at)| now.duration_since(*at) < OWN_TX_TTL);
        own.entry(tx.signature).or_insert((tx.slot, now));
    }
}


pub fn latest_slot() -> Option<u64> {
    Some(LATEST_SLOT.load(Ordering::Relaxed)).filter(|slot| *slot > 0)
}

/// Waits until the stream has reached (`slot`, `index()`), or has left `slot` when the index is
/// unknown, or until `timeout`. `index` is asked again on every check (it may become known).
pub async fn wait_for_stream(slot: u64, index: impl Fn() -> Option<u64>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let reached = match (stream_position(), index()) {
            (Some(at), Some(i)) => at >= (slot, i),
            (Some(at), None) => at.0 > slot,
            (None, _) => false,
        };
        if reached {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Where the stream is now: the furthest (slot, block position) it has shown, or the start of the
/// latest slot when that slot's transactions came without positions.
pub fn stream_position() -> Option<(u64, u64)> {
    let slot = latest_slot()?;
    let seen = *LATEST_POSITION.lock().unwrap_or_else(|e| e.into_inner());
    Some(if seen.0 == slot { seen } else { (slot, 0) })
}


#[derive(Debug, Clone, Copy)]
pub struct ReadyMark {
    pub at: Instant,
    pub stream_slot: Option<u64>,
}

impl ReadyMark {
    pub fn now() -> Self {
        Self { at: Instant::now(), stream_slot: latest_slot() }
    }
}


/// Our transaction is on chain in `slot` (the RPC status check found it, success or failure):
/// the stream never shows a failed one, so without this its re-sends ran their full 1.5 s and
/// hit Nozomi's rate limit (HTTP 429, four times per lost race live on 26 Sep 2026).
pub fn mark_own_tx_landed(signature: &Signature, slot: u64) {
    let now = Instant::now();
    OWN_TXS.lock().unwrap_or_else(|e| e.into_inner()).entry(*signature).or_insert((slot, now));
}

pub fn own_tx_seen(signature: &Signature) -> bool {
    OWN_TXS.lock().unwrap_or_else(|e| e.into_inner()).contains_key(signature)
}



pub async fn wait_own_tx(signature: &Signature, timeout: Duration) -> Option<(u64, Instant)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(seen) = OWN_TXS.lock().unwrap_or_else(|e| e.into_inner()).get(signature) {
            return Some(*seen);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}


type StageMarks = HashMap<Pubkey, Vec<(&'static str, Instant)>>;
static STAGES: LazyLock<Mutex<StageMarks>> = LazyLock::new(Default::default);

/// Records that `mint`'s snipe reached `stage`.
pub fn mark(mint: &Pubkey, stage: &'static str) {
    let mut stages = STAGES.lock().unwrap_or_else(|e| e.into_inner());
    if stages.len() > 256 {
        stages.clear();
    }
    stages.entry(*mint).or_default().push((stage, Instant::now()));
}

/// "stage +x.xx ms, ..." from `start`, and forgets the marks.
pub fn take_marks(mint: &Pubkey, start: Instant) -> String {
    let marks = STAGES.lock().unwrap_or_else(|e| e.into_inner()).remove(mint).unwrap_or_default();
    let mut last = start;
    marks
        .iter()
        .map(|(stage, at)| {
            let d = at.saturating_duration_since(last).as_secs_f64() * 1000.0;
            last = *at;
            format!("{stage} +{d:.2}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Scheduler delay probe: time from a launch's detection until a freshly spawned task runs,

static SPAWN_DELAYS: LazyLock<Mutex<Vec<f64>>> = LazyLock::new(Default::default);

pub fn probe_spawn_delay(detected_at: Instant) {
    tokio::spawn(async move {
        let ms = detected_at.elapsed().as_secs_f64() * 1000.0;
        SPAWN_DELAYS.lock().unwrap_or_else(|e| e.into_inner()).push(ms);
    });
}


pub fn spawn_probe_reporter() {
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let mut v = std::mem::take(&mut *SPAWN_DELAYS.lock().unwrap_or_else(|e| e.into_inner()));
            if v.is_empty() {
                continue;
            }
            v.sort_by(|a, b| a.total_cmp(b));
            let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
            log::info!(
                "latency probe, last 5 min: {} launches, detection -> spawned task median {:.2} ms, p90 {:.2}, p99 {:.2}, max {:.2}",
                v.len(), q(0.5), q(0.9), q(0.99), v[v.len() - 1]
            );
        }
    });
}

/// Sleeps `duration` to within about 0.1 ms. Tokio's timer ticks in whole milliseconds, so its
/// 1 ms sleep took 2.3 ms (median, 335 snipes to 26 Sep 2026); a blocking-pool thread sleeping
/// on the OS timer does not round up.
pub async fn precise_sleep(duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || std::thread::sleep(duration)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_landing_waits_for_the_stream_to_reach_its_position() {
        // The stream is in slot 900 at block position 50.
        LATEST_SLOT.fetch_max(900_000_000, Ordering::Relaxed);
        *LATEST_POSITION.lock().unwrap() = (900_000_000, 50);
        assert_eq!(stream_position(), Some((900_000_000, 50)));
        let quick = |index: Option<u64>, slot: u64| async move {
            let started = Instant::now();
            wait_for_stream(slot, || index, Duration::from_millis(40)).await;
            started.elapsed() < Duration::from_millis(30)
        };
        assert!(quick(Some(40), 900_000_000).await, "position already passed");
        assert!(quick(Some(50), 900_000_000).await, "position reached");
        assert!(!quick(Some(60), 900_000_000).await, "not reached: waits out the timeout");
        assert!(!quick(None, 900_000_000).await, "index unknown: waits for the slot to end");
        assert!(quick(None, 899_999_999).await, "an earlier slot has ended");
    }
}
