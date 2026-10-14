//! Monotonic nanoseconds since the process started: one clock for every hot-path stamp, cheap to
//! read (a vDSO `clock_gettime`) and never allocating.

use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Nanoseconds since the first call in this process.
#[inline]
pub fn now_ns() -> u64 {
    START.elapsed().as_nanos() as u64
}

/// `now_ns()` of an `Instant` taken earlier (0 if it predates the clock's start).
#[inline]
pub fn ns_of(at: Instant) -> u64 {
    at.saturating_duration_since(*START).as_nanos() as u64
}

/// Starts the clock (call once at startup, so the first stamp does not pay for it).
pub fn init() {
    LazyLock::force(&START);
}

pub fn micros(ns: u64) -> f64 {
    ns as f64 / 1_000.0
}

pub fn duration(ns: u64) -> Duration {
    Duration::from_nanos(ns)
}
