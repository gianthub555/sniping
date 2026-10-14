//! The LaserStream backup feed (the owner's, 8 Oct, before AllenHark is cancelled). When Helius's
//! preprocessed stream has been silent for HOT_BACKUP_AFTER_MS (2000 ms), the regular stream's
//! pump.fun transactions (Helius LaserStream gRPC, `processed`) go to the hot path as frames in
//! the preprocessed format, through a feed thread of their own (`engine::run_backup`), until the
//! preprocessed stream delivers again. They come after execution, so a launch bought from them is
//! later than one bought from the preprocessed stream; without them nothing is bought at all. The
//! live guard counts the feed as stalled only when both are silent (`engine::feed_ages`).
//! HOT_BACKUP_FEED=off turns it off.

use {
    super::{clock::now_ns, engine::feeds},
    crate::config::env_opt,
    carbon_core::transaction::TransactionMetadata,
    log::{info, warn},
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
            mpsc::{sync_channel, Receiver, SyncSender},
            LazyLock, Mutex, OnceLock,
        },
        time::Duration,
    },
};

struct Settings {
    on: bool,
    after_ns: u64,
}

static SETTINGS: LazyLock<Settings> = LazyLock::new(|| Settings {
    on: !env_opt("HOT_BACKUP_FEED").is_some_and(|v| v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false")),
    after_ns: env_opt("HOT_BACKUP_AFTER_MS").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(2_000) * 1_000_000,
});

static SENDER: OnceLock<SyncSender<(Vec<u8>, u64)>> = OnceLock::new();
/// The regular stream's last pump.fun transaction (`now_ns`).
static STREAM_LAST_NS: AtomicU64 = AtomicU64::new(0);
/// A failover is on: the preprocessed stream is silent.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static INSTALLED_NS: AtomicU64 = AtomicU64::new(0);
static HANDED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// The last transactions handed over (8-byte signature prefixes), direct-mapped: the processor
/// runs once per pump.fun instruction, the frame goes once per transaction.
static RECENT: Mutex<[u64; 1024]> = Mutex::new([0; 1024]);

pub fn enabled() -> bool {
    SETTINGS.on
}

pub fn active() -> bool {
    ACTIVE.load(Relaxed)
}

pub fn stream_last_ns() -> u64 {
    STREAM_LAST_NS.load(Relaxed)
}

/// Sets the backup up (the hot path's start, on the runtime): the receiver goes to its feed
/// thread; a watcher turns the failover on and off.
pub fn install() -> Receiver<(Vec<u8>, u64)> {
    let (tx, rx) = sync_channel(4_096);
    let _ = SENDER.set(tx);
    INSTALLED_NS.store(now_ns(), Relaxed);
    info!("hot backup: LaserStream feeds the hot path when the preprocessed stream is silent for {} ms (HOT_BACKUP_AFTER_MS)", SETTINGS.after_ns / 1_000_000);
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut since = 0u64;
        loop {
            tick.tick().await;
            let now = now_ns();
            let last = feeds::PP_LAST_NS.load(Relaxed).max(INSTALLED_NS.load(Relaxed));
            let silent = now.saturating_sub(last) >= SETTINGS.after_ns;
            if silent && !ACTIVE.swap(true, Relaxed) {
                since = now;
                warn!(
                    "HOT BACKUP ON: the preprocessed stream has been silent {:.1} s; LaserStream's transactions feed the hot path",
                    now.saturating_sub(last) as f64 / 1e9
                );
            } else if !silent && ACTIVE.swap(false, Relaxed) {
                info!(
                    "hot backup off: the preprocessed stream is back after {:.1} s; LaserStream handed over {} transactions ({} dropped, queue full)",
                    now.saturating_sub(since) as f64 / 1e9,
                    HANDED.load(Relaxed),
                    DROPPED.load(Relaxed)
                );
            }
        }
    });
    rx
}

/// A pump.fun transaction on the regular stream: it marks the stream alive, and during a failover
/// it goes to the hot path.
pub fn offer(tx: &TransactionMetadata) {
    let t = now_ns();
    STREAM_LAST_NS.store(t, Relaxed);
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let Some(sender) = SENDER.get() else { return };
    if !first_time(&tx.signature) {
        return;
    }
    let Some(frame) = frame(tx.slot, &tx.signature, &tx.message) else { return };
    match sender.try_send((frame, t)) {
        Ok(()) => HANDED.fetch_add(1, Relaxed),
        Err(_) => DROPPED.fetch_add(1, Relaxed),
    };
}

fn first_time(sig: &Signature) -> bool {
    let key = u64::from_le_bytes(sig.as_ref()[..8].try_into().expect("8")) | 1;
    let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    let cell = &mut recent[(key >> 1) as usize % 1024];
    if *cell == key {
        return false;
    }
    *cell = key;
    true
}

/// A transaction as the preprocessed stream frames it: version 0, the slot, the first signature,
/// then the transaction's wire bytes, v1 included (the stream gives only the first signature: the
/// others are left zero, which the decoder does not read).
pub fn frame(slot: u64, signature: &Signature, message: &solana_message::VersionedMessage) -> Option<Vec<u8>> {
    let n = (message.header().num_required_signatures as usize).max(1);
    let mut signatures = vec![Signature::default(); n];
    signatures[0] = *signature;
    let bytes = wincode::serialize(&VersionedTransaction { signatures, message: message.clone() }).ok()?;
    let mut f = Vec::with_capacity(1 + 8 + 64 + bytes.len());
    f.push(0);
    f.extend_from_slice(&slot.to_le_bytes());
    f.extend_from_slice(signature.as_ref());
    f.extend_from_slice(&bytes);
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixture_create_framed_from_the_stream_reads_as_the_preprocessed_frame_did() {
        // A preprocessed fixture frame, taken apart as the regular stream gives it, framed again.
        for (name, pp) in super::super::testkit::frames(42) {
            let tx: VersionedTransaction = wincode::deserialize(&pp[73..]).expect("a transaction");
            let rebuilt = frame(42, &tx.signatures[0], &tx.message).expect("framed");
            assert_eq!(&rebuilt[..73], &pp[..73], "{name}: header");
            let a = super::super::decode::Tx::parse(&pp[73..]).expect("parses");
            let b = super::super::decode::Tx::parse(&rebuilt[73..]).expect("parses");
            assert_eq!(a.signature(), b.signature(), "{name}: the same first signature");
            let (mut la, mut lb) = (Vec::new(), Vec::new());
            super::super::decode::launches(&a, |l| la.push(l.mint), |_| {});
            super::super::decode::launches(&b, |l| lb.push(l.mint), |_| {});
            assert_eq!(la, lb, "{name}: the same launches");
        }
    }
}
