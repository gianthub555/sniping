//! Per-launch stage stamps of the hot path (A1). A stamp is written into a pre-allocated ring with
//! plain atomic stores: no lock, no allocation, no formatting, no log. A separate thread drains the
//! ring, appends every stamp to `logs/hot_stamps.csv` and every 10 s writes p50/p90/p99 of each
//! stage, for the last 10 s and for the whole run, to `logs/hot_stats.log`.

use {
    super::clock::now_ns,
    std::{
        io::Write,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            LazyLock,
        },
        time::Duration,
    },
};

/// Send lanes (`hot::lanes`: Helius, Everstake, AllenHark, Triton, 0slot, Nozomi, Jito, QUIC).
pub const LANES: usize = 8;
const RING: usize = 1 << 14;
const WORDS: usize = 14 + LANES + 2;
const REPORT_EVERY: Duration = Duration::from_secs(10);
pub const STAMPS_CSV: &str = "logs/hot_stamps.csv";
pub const STATS_LOG: &str = "logs/hot_stats.log";

/// Which feed delivered the launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Feed {
    #[default]
    Bench = 0,
    Preprocessed = 1,
    /// pumpfeed's entry push (ENGINE_INPROC=false, and the parity runs).
    Udp = 2,
    /// The bot's own shred engine (ENGINE_INPROC).
    Inproc = 3,
    /// Helius LaserStream's executed transactions, while the preprocessed stream is silent
    /// (`hot::backup`).
    Grpc = 4,
    /// Not a launch: a sell handed to the lanes (`hot::sell`).
    Sell = 9,
}

/// How the decision was reached from the feed thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Mode {
    /// On the thread that decoded the message.
    #[default]
    Inline = 0,
    /// Over the SPSC ring to the pinned decision thread.
    Spsc = 1,
}

pub const FLAG_SHADOW: u16 = 1;
pub const FLAG_BUY: u16 = 2;
/// The decision needed something only the slow path has (an RPC answer): left to it.
pub const FLAG_UNKNOWN: u16 = 4;
/// DRY_RUN: built and sent as an unlandable copy to time the lanes (`HOT_DRY_SAMPLE_MS`).
pub const FLAG_SAMPLE: u16 = 8;
/// A sell from the slow path written on the lanes by the decision thread (`hot::sell`): only
/// `t_recv` (queued), `t_picked`, `t_ready` and the lane writes are set.
pub const FLAG_SELL: u16 = 16;

/// One launch through the hot path. Times are `clock::now_ns()`; 0 means "not reached".
#[derive(Debug, Clone, Copy, Default)]
pub struct Stamp {
    pub mint: [u8; 32],
    pub feed: Feed,
    pub mode: Mode,
    pub flags: u16,
    /// `decide::Reject` code (0: none).
    pub reject: u8,
    /// The deliberate wait for this feed (the 250 us bundle window), excluded from the totals.
    pub window_ns: u64,
    /// Feed frame received (in-process: the shred that completed the create's batch).
    pub t_recv: u64,
    /// In-process only: the packet that completed the create's batch came off the socket, and the
    /// batch was reassembled from its shreds.
    pub t_shred_recv: u64,
    pub t_deshred: u64,
    /// Create matched in the frame.
    pub t_matched: u64,
    /// Picked up by the deciding thread (same as `t_matched` inline).
    pub t_picked: u64,
    /// Per-coin accounts derived.
    pub t_derived: u64,
    /// Decision started (after the bundle window).
    pub t_decide_start: u64,
    pub t_decided: u64,
    pub t_signed: u64,
    /// Serialized and base64-encoded: bytes ready to write.
    pub t_ready: u64,
    /// Bytes written to each lane.
    pub t_sent: [u64; LANES],
}

impl Stamp {
    fn words(&self) -> [u64; WORDS] {
        let m = |i: usize| u64::from_le_bytes(self.mint[i * 8..i * 8 + 8].try_into().unwrap());
        let meta = self.feed as u64 | (self.mode as u64) << 8 | (self.flags as u64) << 16 | (self.reject as u64) << 32;
        [
            m(0), m(1), m(2), m(3), meta, self.window_ns, self.t_recv, self.t_matched, self.t_picked, self.t_derived,
            self.t_decide_start, self.t_decided, self.t_signed, self.t_ready, self.t_sent[0], self.t_sent[1], self.t_sent[2],
            self.t_sent[3], self.t_sent[4], self.t_sent[5], self.t_sent[6], self.t_sent[7], self.t_shred_recv, self.t_deshred,
        ]
    }

    fn from_words(w: &[u64; WORDS]) -> Self {
        let mut mint = [0u8; 32];
        for i in 0..4 {
            mint[i * 8..i * 8 + 8].copy_from_slice(&w[i].to_le_bytes());
        }
        let feed = match w[4] & 0xff {
            1 => Feed::Preprocessed,
            2 => Feed::Udp,
            3 => Feed::Inproc,
            4 => Feed::Grpc,
            9 => Feed::Sell,
            _ => Feed::Bench,
        };
        let mode = if (w[4] >> 8) & 0xff == 1 { Mode::Spsc } else { Mode::Inline };
        Stamp {
            mint,
            feed,
            mode,
            flags: (w[4] >> 16) as u16,
            reject: (w[4] >> 32) as u8,
            window_ns: w[5],
            t_recv: w[6],
            t_matched: w[7],
            t_picked: w[8],
            t_derived: w[9],
            t_decide_start: w[10],
            t_decided: w[11],
            t_signed: w[12],
            t_ready: w[13],
            t_sent: std::array::from_fn(|i| w[14 + i]),
            t_shred_recv: w[14 + LANES],
            t_deshred: w[15 + LANES],
        }
    }
}

struct Slot {
    /// Index + 1 of the stamp in it once complete; 0 while being written.
    seq: AtomicU64,
    words: [AtomicU64; WORDS],
}

static SLOTS: LazyLock<Box<[Slot]>> = LazyLock::new(|| {
    (0..RING).map(|_| Slot { seq: AtomicU64::new(0), words: std::array::from_fn(|_| AtomicU64::new(0)) }).collect()
});
static HEAD: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Allocates the ring (call at startup, off the hot path).
pub fn init() {
    LazyLock::force(&SLOTS);
    LazyLock::force(&LANE_SLOTS);
}

/// Records a finished stamp. Lock-free and allocation-free; safe from several threads.
#[inline]
pub fn record(stamp: &Stamp) {
    let index = HEAD.fetch_add(1, Ordering::Relaxed);
    let slot = &SLOTS[(index as usize) & (RING - 1)];
    slot.seq.store(0, Ordering::Relaxed);
    for (cell, word) in slot.words.iter().zip(stamp.words()) {
        cell.store(word, Ordering::Relaxed);
    }
    slot.seq.store(index + 1, Ordering::Release);
}

/// Stamps recorded since `*next`, oldest first; `*next` moves past them. Stamps overwritten before
/// they were read are skipped.
pub fn drain(next: &mut u64, out: &mut Vec<Stamp>) {
    let head = HEAD.load(Ordering::Acquire);
    if head.saturating_sub(*next) > RING as u64 {
        *next = head - RING as u64;
    }
    while *next < head {
        let slot = &SLOTS[(*next as usize) & (RING - 1)];
        if slot.seq.load(Ordering::Acquire) != *next + 1 {
            // Still being written: try again on the next drain.
            break;
        }
        let words: [u64; WORDS] = std::array::from_fn(|i| slot.words[i].load(Ordering::Relaxed));
        if slot.seq.load(Ordering::Acquire) == *next + 1 {
            out.push(Stamp::from_words(&words));
        }
        *next += 1;
    }
}

/// Lane write times stamped by the lane threads (QUIC, Jito), which write after the decision
/// thread has recorded its stamp: (mint, lane, time), merged into the stamp by the reporter.
const LANE_RING: usize = 1 << 12;

struct LaneSlot {
    seq: AtomicU64,
    words: [AtomicU64; 6],
}

static LANE_SLOTS: LazyLock<Box<[LaneSlot]>> =
    LazyLock::new(|| (0..LANE_RING).map(|_| LaneSlot { seq: AtomicU64::new(0), words: std::array::from_fn(|_| AtomicU64::new(0)) }).collect());
static LANE_HEAD: AtomicU64 = AtomicU64::new(0);

/// Records that `lane` wrote the buy of `mint` at `t`. Lock-free.
pub fn record_lane(mint: &[u8; 32], lane: usize, t: u64) {
    let index = LANE_HEAD.fetch_add(1, Ordering::Relaxed);
    let slot = &LANE_SLOTS[(index as usize) & (LANE_RING - 1)];
    slot.seq.store(0, Ordering::Relaxed);
    for i in 0..4 {
        slot.words[i].store(u64::from_le_bytes(mint[i * 8..i * 8 + 8].try_into().unwrap()), Ordering::Relaxed);
    }
    slot.words[4].store(lane as u64, Ordering::Relaxed);
    slot.words[5].store(t, Ordering::Relaxed);
    slot.seq.store(index + 1, Ordering::Release);
}

/// Lane stamps since `*next`: (mint, lane, time).
pub fn drain_lanes(next: &mut u64, out: &mut Vec<([u8; 32], usize, u64)>) {
    let head = LANE_HEAD.load(Ordering::Acquire);
    if head.saturating_sub(*next) > LANE_RING as u64 {
        *next = head - LANE_RING as u64;
    }
    while *next < head {
        let slot = &LANE_SLOTS[(*next as usize) & (LANE_RING - 1)];
        if slot.seq.load(Ordering::Acquire) != *next + 1 {
            break;
        }
        let w: [u64; 6] = std::array::from_fn(|i| slot.words[i].load(Ordering::Relaxed));
        if slot.seq.load(Ordering::Acquire) == *next + 1 {
            let mut mint = [0u8; 32];
            for i in 0..4 {
                mint[i * 8..i * 8 + 8].copy_from_slice(&w[i].to_le_bytes());
            }
            out.push((mint, w[4] as usize, w[5]));
        }
        *next += 1;
    }
}

/// Round trips per lane, write to answer: transactions (`tx`) and keep-alive pings, the last 4096
/// of each (a ring of plain atomics; the reporter reads it).
pub mod rtt {
    use {
        super::LANES,
        std::sync::atomic::{AtomicU64, Ordering},
    };
    const N: usize = 4096;
    pub struct Samples {
        head: AtomicU64,
        v: [AtomicU64; N],
    }
    #[allow(clippy::declare_interior_mutable_const)]
    const EMPTY: Samples = Samples { head: AtomicU64::new(0), v: [const { AtomicU64::new(0) }; N] };
    pub static TX: [Samples; LANES] = [EMPTY; LANES];
    pub static PING: [Samples; LANES] = [EMPTY; LANES];
    /// Threaded lanes (2026-10-11): the bytes handed over to the lane thread taking them, and the
    /// lane thread taking them to the request's first poll (when it is written).
    pub static PICKUP: [Samples; LANES] = [EMPTY; LANES];
    pub static BUILD: [Samples; LANES] = [EMPTY; LANES];

    impl Samples {
        pub fn record(&self, ns: u64) {
            let i = self.head.fetch_add(1, Ordering::Relaxed) as usize;
            self.v[i % N].store(ns.max(1), Ordering::Relaxed);
        }
        pub fn snapshot(&self) -> Vec<u64> {
            self.v.iter().map(|a| a.load(Ordering::Relaxed)).filter(|&x| x > 0).collect()
        }
    }
}

/// Every answer to a lane write (and every write that got none), from the lane threads (never the
/// hot ones): written to `logs/hot_lane_answers.csv` by the reporter. `t_sent` is the same clock
/// reading as the stamp's `t_sent[lane]` for transactions, so the two join exactly.
pub mod answers {
    use std::sync::Mutex;

    #[derive(Clone, Copy, Debug)]
    pub struct Answer {
        pub lane: usize,
        pub ping: bool,
        /// Which connection or leader (0 or 1).
        pub conn: u8,
        pub t_sent: u64,
        /// 0: no answer.
        pub t_answer: u64,
        /// ok, refused, acked (QUIC: the leader acknowledged every byte), stopped (QUIC: the
        /// leader refused the stream), lost (the connection closed first), timeout.
        pub status: &'static str,
    }

    static PENDING: Mutex<Vec<Answer>> = Mutex::new(Vec::new());

    pub fn push(a: Answer) {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).push(a);
    }

    pub fn drain() -> Vec<Answer> {
        std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

pub const ANSWERS_CSV: &str = "logs/hot_lane_answers.csv";

/// hot_stamps.csv's columns (the lanes' send times in the middle).
static STAMPS_HEADER: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "mint,feed,mode,flags,reject,window_ns,t_recv,t_matched,t_picked,t_derived,t_decide_start,t_decided,t_signed,t_ready,{},t_shred_recv,t_deshred",
        super::lanes::LANE_NAMES.iter().map(|n| format!("t_sent_{n}")).collect::<Vec<_>>().join(",")
    )
});

fn write_answers() {
    let fresh = answers::drain();
    if fresh.is_empty() {
        return;
    }
    let new_file = !std::path::Path::new(ANSWERS_CSV).exists();
    if let Some(mut f) = crate::store::tee::append(ANSWERS_CSV, "lane_answers", "lane,kind,conn,t_sent,t_answer,answer_us,status") {
        if new_file {
            let _ = f.write_all(b"lane,kind,conn,t_sent,t_answer,answer_us,status\n");
        }
        for a in fresh {
            let us = if a.t_answer >= a.t_sent && a.t_answer > 0 { format!("{:.1}", (a.t_answer - a.t_sent) as f64 / 1e3) } else { String::new() };
            let _ = writeln!(
                f,
                "{},{},{},{},{},{us},{}",
                super::lanes::LANE_NAMES[a.lane.min(LANES - 1)],
                if a.ping { "ping" } else { "tx" },
                a.conn,
                a.t_sent,
                a.t_answer,
                a.status
            );
        }
    }
}

/// A measured stage: its name and its duration in a stamp, when the stamp reached it.
pub struct Stage {
    pub name: &'static str,
    pub of: fn(&Stamp) -> Option<u64>,
}

fn span(from: u64, to: u64) -> Option<u64> {
    (from > 0 && to >= from).then(|| to - from)
}

pub const STAGES: &[Stage] = &[
    // In-process: shred off the socket to batch reassembled, then to the create decoded.
    Stage { name: "deshred", of: |s| span(s.t_shred_recv, s.t_deshred) },
    Stage { name: "decode", of: |s| span(s.t_recv, s.t_matched) },
    Stage { name: "handoff", of: |s| span(s.t_matched, s.t_picked) },
    Stage { name: "derive", of: |s| span(s.t_picked, s.t_derived) },
    // Time the decision started after it could have: past the end of the bundle window, or past
    // the derivation when that finished later.
    Stage {
        name: "late_start",
        of: |s| {
            let ready = s.t_derived.max(s.t_matched + s.window_ns);
            (s.t_decide_start > 0 && s.t_derived > 0).then(|| s.t_decide_start.saturating_sub(ready))
        },
    },
    Stage { name: "decide", of: |s| span(s.t_decide_start, s.t_decided) },
    Stage { name: "build_sign", of: |s| span(s.t_decided, s.t_signed) },
    Stage { name: "serialize", of: |s| span(s.t_signed, s.t_ready) },
    // Message in to bytes ready, less the deliberate bundle window.
    Stage { name: "total_excl_wait", of: |s| span(s.t_recv, s.t_ready).map(|t| t.saturating_sub(s.window_ns)) },
    // Bytes ready to each lane's write, in firing order (`hot::lanes::LANE_NAMES`).
    Stage { name: "sent_helius", of: |s| span(s.t_ready, s.t_sent[0]) },
    Stage { name: "sent_everstake", of: |s| span(s.t_ready, s.t_sent[1]) },
    Stage { name: "sent_allenhark", of: |s| span(s.t_ready, s.t_sent[2]) },
    Stage { name: "sent_triton", of: |s| span(s.t_ready, s.t_sent[3]) },
    Stage { name: "sent_0slot", of: |s| span(s.t_ready, s.t_sent[4]) },
    Stage { name: "sent_nozomi", of: |s| span(s.t_ready, s.t_sent[5]) },
    Stage { name: "sent_jito", of: |s| span(s.t_ready, s.t_sent[6]) },
    Stage { name: "sent_quic", of: |s| span(s.t_ready, s.t_sent[7]) },
    // First lane write to last: B2 wants every lane within 200 us.
    Stage {
        name: "lane_spread",
        of: |s| {
            let sent = s.t_sent.iter().filter(|&&t| t > 0);
            match (sent.clone().min(), sent.max()) {
                (Some(a), Some(b)) if b > a => Some(b - a),
                _ => None,
            }
        },
    },
];

/// p50, p90, p99 of `values` (sorted in place), in nanoseconds.
pub fn percentiles(values: &mut [u64]) -> Option<(u64, u64, u64)> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let q = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    Some((q(0.5), q(0.9), q(0.99)))
}

/// One report line per stage with samples: "stage n p50 p90 p99" in microseconds.
pub fn report(stamps: &[Stamp], label: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for feed in [Feed::Preprocessed, Feed::Udp, Feed::Inproc, Feed::Grpc, Feed::Bench, Feed::Sell] {
        let of_feed: Vec<&Stamp> = stamps.iter().filter(|s| s.feed == feed).collect();
        if of_feed.is_empty() {
            continue;
        }
        for stage in STAGES {
            let mut v: Vec<u64> = of_feed.iter().filter_map(|s| (stage.of)(s)).collect();
            if let Some((p50, p90, p99)) = percentiles(&mut v) {
                lines.push(format!(
                    "{label} {:?} {:<16} n={:<6} p50={:>8.1}us p90={:>8.1}us p99={:>8.1}us",
                    feed,
                    stage.name,
                    v.len(),
                    p50 as f64 / 1e3,
                    p90 as f64 / 1e3,
                    p99 as f64 / 1e3
                ));
            }
        }
    }
    lines
}

/// Round-trip percentiles per lane (write to answer), transactions and pings.
pub fn rtt_report() -> Vec<String> {
    let mut lines = Vec::new();
    for lane in 0..LANES {
        for (kind, samples) in [("tx", &rtt::TX[lane]), ("ping", &rtt::PING[lane]), ("pickup", &rtt::PICKUP[lane]), ("build", &rtt::BUILD[lane])] {
            let mut v = samples.snapshot();
            if let Some((p50, p90, p99)) = percentiles(&mut v) {
                lines.push(format!(
                    "rtt {:<7} {:<5} n={:<6} p50={:>8.1}us p90={:>8.1}us p99={:>8.1}us",
                    super::lanes::LANE_NAMES[lane],
                    kind,
                    v.len(),
                    p50 as f64 / 1e3,
                    p90 as f64 / 1e3,
                    p99 as f64 / 1e3
                ));
            }
        }
    }
    lines
}

fn csv_row(s: &Stamp) -> String {
    let sent: Vec<String> = s.t_sent.iter().map(|t| t.to_string()).collect();
    format!(
        "{},{:?},{:?},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        bs58::encode(s.mint).into_string(),
        s.feed,
        s.mode,
        s.flags,
        s.reject,
        s.window_ns,
        s.t_recv,
        s.t_matched,
        s.t_picked,
        s.t_derived,
        s.t_decide_start,
        s.t_decided,
        s.t_signed,
        s.t_ready,
        sent.join(","),
        s.t_shred_recv,
        s.t_deshred
    )
}

/// Starts the drain-and-report thread (once).
pub fn spawn_reporter() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    init();
    std::thread::Builder::new()
        .name("hot-stamps".into())
        .spawn(|| {
            let _ = std::fs::create_dir_all("logs");
            let mut next = 0u64;
            let mut next_lane = 0u64;
            let mut all: Vec<Stamp> = Vec::new();
            // Stamps with a send wait one round for the lane threads' write times.
            let mut held: Vec<Stamp> = Vec::new();
            let mut lane_times: std::collections::HashMap<[u8; 32], [u64; LANES]> = Default::default();
            loop {
                std::thread::sleep(REPORT_EVERY);
                super::verify::flush();
                super::parity::flush(now_ns());
                write_answers();
                let mut drained = Vec::new();
                drain(&mut next, &mut drained);
                let mut lanes = Vec::new();
                drain_lanes(&mut next_lane, &mut lanes);
                for (mint, lane, t) in lanes {
                    let e = lane_times.entry(mint).or_default();
                    if lane < LANES && (e[lane] == 0 || t < e[lane]) {
                        e[lane] = t;
                    }
                }
                let mut fresh = std::mem::take(&mut held);
                for s in drained {
                    if s.t_ready > 0 {
                        held.push(s);
                    } else {
                        fresh.push(s);
                    }
                }
                for s in fresh.iter_mut() {
                    if let Some(t) = lane_times.remove(&s.mint) {
                        for lane in 0..LANES {
                            // The earliest write after the bytes were ready (a mint seen twice keeps its own).
                            if s.t_sent[lane] == 0 && t[lane] >= s.t_ready {
                                s.t_sent[lane] = t[lane];
                            }
                        }
                    }
                }
                if lane_times.len() > 10_000 {
                    lane_times.clear();
                }
                if fresh.is_empty() {
                    continue;
                }
                let new_file = !std::path::Path::new(STAMPS_CSV).exists();
                if let Some(mut f) = crate::store::tee::append(STAMPS_CSV, "hot_stamps", STAMPS_HEADER.as_str()) {
                    if new_file {
                        let _ = f.write_all(format!("{}\n", STAMPS_HEADER.as_str()).as_bytes());
                    }
                    for s in &fresh {
                        let _ = f.write_all(csv_row(s).as_bytes());
                    }
                }
                all.extend_from_slice(&fresh);
                let at = now_ns();
                let mut lines = report(&fresh, "last10s");
                lines.extend(report(&all, "run"));
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(STATS_LOG) {
                    let _ = writeln!(f, "--- {:.1} s after start: {} stamps in the last 10 s, {} in the run", at as f64 / 1e9, fresh.len(), all.len());
                    let _ = writeln!(f, "{}", super::engine::feed_report());
                    let _ = writeln!(f, "{}", super::lanes::report());
                    let _ = writeln!(f, "{}", super::verify::report());
                    if super::engine::INPROC.enabled {
                        let _ = writeln!(f, "{}", super::shreds::report(at));
                        let _ = writeln!(f, "{}", super::shreds::book::report());
                        if let Some(line) = super::shreds::source_race::report() {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                    if super::parity::is_on() {
                        let _ = writeln!(f, "{}", super::parity::report());
                    }
                    let _ = writeln!(f, "{}", super::live::report());
                    let _ = writeln!(f, "{}", crate::utils::file_writer::report());
                    crate::processors::position_feed::sweep_preexec();
                    let _ = writeln!(f, "{}", crate::processors::position_feed::preexec_report());
                    if let Some(line) = super::nonces::report() {
                        let _ = writeln!(f, "{line}");
                    }
                    for line in rtt_report() {
                        let _ = writeln!(f, "{line}");
                    }
                    for line in lines {
                        let _ = writeln!(f, "{line}");
                    }
                }
            }
        })
        .expect("hot-stamps thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_survives_the_ring_and_its_stages_add_up() {
        init();
        let mut next = HEAD.load(Ordering::Acquire);
        let s = Stamp {
            mint: [7; 32],
            feed: Feed::Preprocessed,
            mode: Mode::Spsc,
            flags: FLAG_SHADOW,
            reject: 3,
            window_ns: 250_000,
            t_recv: 1_000,
            t_matched: 3_000,
            t_picked: 3_500,
            t_derived: 60_000,
            t_decide_start: 253_100,
            t_decided: 255_000,
            t_signed: 300_000,
            t_ready: 302_000,
            t_sent: [0; LANES],
            t_shred_recv: 900,
            t_deshred: 950,
        };
        record(&s);
        let mut out = Vec::new();
        drain(&mut next, &mut out);
        let got = out.iter().find(|x| x.mint == [7; 32]).expect("drained");
        assert_eq!((got.feed, got.mode, got.flags, got.reject, got.t_ready), (Feed::Preprocessed, Mode::Spsc, FLAG_SHADOW, 3, 302_000));
        let stage = |name: &str| (STAGES.iter().find(|st| st.name == name).unwrap().of)(got);
        assert_eq!(stage("decode"), Some(2_000));
        assert_eq!(stage("deshred"), Some(50));
        assert_eq!(stage("handoff"), Some(500));
        assert_eq!(stage("late_start"), Some(100), "window ends at 253_000");
        assert_eq!(stage("total_excl_wait"), Some(51_000));
        assert_eq!(stage("sent_helius"), None, "no lane yet");
    }
}
