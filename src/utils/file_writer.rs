//! Blocking file writes off the tokio runtime (ASYNC_FILE_WRITES, 2026-10-12, default off). The
//! runtime that runs the sells and the regular stream also ran file appends inline: a live buy's
//! trade row before its sell monitor started, the reference wallet's trades and the shred-lead
//! rows in the task that feeds the sells. With the switch on, each such write is a job for one
//! writer thread (off the hot cores) on a bounded queue: the caller only queues it. A full queue
//! drops the job and counts it, never waits. One thread, so rows keep their order and never
//! interleave. Off: the write runs where it is called, as before.

use {
    log::warn,
    std::{
        sync::{
            atomic::{AtomicU64, Ordering::Relaxed},
            mpsc::{sync_channel, SyncSender, TrySendError},
            LazyLock,
        },
        time::Instant,
    },
};

/// Jobs waiting at most (a few minutes of writes at the busiest).
pub const QUEUE: usize = 8_192;

type Job = Box<dyn FnOnce() + Send + 'static>;

static ON: LazyLock<bool> = LazyLock::new(|| crate::config::env_opt("ASYNC_FILE_WRITES").is_some_and(|v| v.eq_ignore_ascii_case("true")));

pub static QUEUED: AtomicU64 = AtomicU64::new(0);
pub static WRITTEN: AtomicU64 = AtomicU64::new(0);
pub static DROPPED: AtomicU64 = AtomicU64::new(0);
/// The longest single write (µs).
pub static MAX_JOB_US: AtomicU64 = AtomicU64::new(0);

static SENDER: LazyLock<Option<SyncSender<Job>>> = LazyLock::new(|| start(QUEUE));

fn start(capacity: usize) -> Option<SyncSender<Job>> {
    let (tx, rx) = sync_channel::<Job>(capacity);
    let spawned = std::thread::Builder::new().name("file-writer".into()).spawn(move || {
        crate::hot::affinity::keep_off_hot_cores();
        for job in rx {
            let t = Instant::now();
            job();
            WRITTEN.fetch_add(1, Relaxed);
            MAX_JOB_US.fetch_max(t.elapsed().as_micros() as u64, Relaxed);
        }
    });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            warn!("file writer not started ({e}); files are written where they are produced");
            None
        }
    }
}

pub fn enabled() -> bool {
    *ON
}

/// Runs `job` (a file write) on the writer thread (ASYNC_FILE_WRITES), else here.
pub fn run(job: impl FnOnce() + Send + 'static) {
    if !enabled() {
        job();
        return;
    }
    match SENDER.as_ref() {
        Some(tx) => submit(tx, Box::new(job)),
        None => job(),
    }
}

fn submit(tx: &SyncSender<Job>, job: Job) {
    match tx.try_send(job) {
        Ok(()) => {
            QUEUED.fetch_add(1, Relaxed);
        }
        Err(TrySendError::Full(_)) => {
            let n = DROPPED.fetch_add(1, Relaxed);
            if n < 5 || n % 1_000 == 0 {
                warn!("file writer: queue full ({QUEUE} waiting), a row was dropped ({} so far)", n + 1);
            }
        }
        // The writer is gone: write here rather than lose it.
        Err(TrySendError::Disconnected(job)) => job(),
    }
}

/// One line for the stats log.
pub fn report() -> String {
    format!(
        "file writer (ASYNC_FILE_WRITES {}): queued {}, written {}, dropped {}, longest write {} us",
        if enabled() { "on" } else { "off" },
        QUEUED.load(Relaxed),
        WRITTEN.load(Relaxed),
        DROPPED.load(Relaxed),
        MAX_JOB_US.load(Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::sync::{mpsc::channel, Arc, Barrier},
        std::time::Duration,
    };

    /// A full queue drops the job and counts it at once; the caller never waits on the writer.
    #[test]
    fn a_full_queue_drops_instead_of_blocking() {
        let (tx, rx) = sync_channel::<Job>(2);
        let before = DROPPED.load(Relaxed);
        for _ in 0..2 {
            submit(&tx, Box::new(|| {}));
        }
        let t = Instant::now();
        submit(&tx, Box::new(|| {}));
        assert!(t.elapsed() < Duration::from_millis(50), "did not wait");
        assert_eq!(DROPPED.load(Relaxed) - before, 1);
        drop(rx);
        // The writer gone: the job runs here.
        let (done_tx, done_rx) = channel();
        submit(&tx, Box::new(move || done_tx.send(()).unwrap()));
        assert!(done_rx.try_recv().is_ok());
    }

    /// One writer thread runs the jobs in the order they were queued.
    #[test]
    fn jobs_run_in_order_on_the_writer_thread() {
        let tx = start(64).expect("writer");
        let (out_tx, out_rx) = channel();
        let gate = Arc::new(Barrier::new(2));
        let g = gate.clone();
        submit(&tx, Box::new(move || {
            g.wait();
        }));
        for i in 0..10 {
            let out = out_tx.clone();
            submit(&tx, Box::new(move || out.send((i, std::thread::current().name().map(str::to_string))).unwrap()));
        }
        gate.wait();
        let got: Vec<(i32, Option<String>)> = (0..10).map(|_| out_rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), (0..10).collect::<Vec<_>>());
        assert!(got.iter().all(|g| g.1.as_deref() == Some("file-writer")));
    }
}
