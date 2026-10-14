//! Pins the calling thread to one CPU core (Linux `sched_setaffinity`).

/// Pins the current thread to `core`. False when the core does not exist or the call fails.
pub fn pin_current_thread(core: usize) -> bool {
    if core >= libc::CPU_SETSIZE as usize {
        return false;
    }
    // SAFETY: `set` is a plain bitmask initialised by CPU_ZERO; pid 0 is the calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// Cores this machine has.
pub fn core_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Restricts the calling thread to every core except the hot path's (FAST_PATH with HOT_PIN):
/// called for the main thread and as the runtime's `on_thread_start`, so no Tokio worker or
/// blocking thread is scheduled onto a busy-polling hot core. Hot threads, started later, pin
/// themselves to their own core.
pub fn keep_off_hot_cores() {
    let cfg = &*super::engine::CONFIG;
    if !(cfg.enabled && cfg.pin) {
        return;
    }
    let n = core_count();
    // SAFETY: as in `pin_current_thread`.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        let mut any = false;
        // The in-process shred thread's core too, when it has one of its own.
        let inproc = super::engine::INPROC.enabled.then_some(cfg.inproc_core).flatten();
        let lane = lane_core();
        for core in (0..n).filter(|c| !cfg.cores.contains(c) && Some(*c) != inproc && Some(*c) != lane) {
            libc::CPU_SET(core, &mut set);
            any = true;
        }
        if any {
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        }
    }
}

/// The lane threads' own core (HOT_LANE_CORE, with HOT_PIN): the threads that hand our buy to a
/// lane (each HTTPS, relay and QUIC lane's sender) run there and nothing else does, so a buy never
/// waits behind a Tokio worker, the slow path or the shred proxy. Several lane threads share it:
/// a primed one gives the core up between looks at its ring (`spin_pause`).
pub fn lane_core() -> Option<usize> {
    static CORE: std::sync::LazyLock<Option<usize>> = std::sync::LazyLock::new(|| {
        let cfg = &*super::engine::CONFIG;
        let core = crate::config::env_opt("HOT_LANE_CORE").and_then(|v| v.trim().parse::<usize>().ok())?;
        (cfg.enabled && cfg.pin && core < core_count() && !cfg.cores.contains(&core)).then_some(core)
    });
    *CORE
}

/// Puts a lane thread on the lane core, else keeps it off the hot cores.
pub fn pin_lane_thread() {
    match lane_core() {
        Some(core) if pin_current_thread(core) => {}
        _ => keep_off_hot_cores(),
    }
}

/// One pause of a primed lane thread with nothing to send: the core goes to another lane thread
/// that has work (they share the lane core), and comes straight back when none has.
#[inline]
pub fn spin_pause() {
    if lane_core().is_some() {
        std::thread::yield_now();
    } else {
        std::hint::spin_loop();
    }
}
