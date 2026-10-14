//! The distance rule (the owner's, 8 Oct: Frankfurt only). Every 5 minutes a side task measures the
//! QUIC handshake time to the TPU of every leader in the current and the next epoch's schedule (each
//! TPU address once, the fastest of three; the best of its last three sweeps), cached per leader
//! identity. A launch is bought only when the leader of its slot answers under DISTANCE_MAX_RTT_MS
//! (30 ms since 8 Oct evening, so London counts; 20 ms before); when the create is in the last slot
//! of that leader's four and the next leader is farther, only when the create came in the first
//! DISTANCE_LAST_SLOT_MS (165 ms, ~62% of a 267 ms slot; 250 ms before) of the slot. On top of the
//! client routing (`fee`).
//!
//! The hot path reads two lock-free rings (`verdict`, allocation-free): each upcoming slot's leader
//! handshake time (filled by the side task from the schedule and the cache), and when each slot was
//! first seen on the early feeds (`note_seen`, the slot's start as seen from here). The slow path
//! logs every skipped launch with the leader's city, handshake time and client (`log_skip`) to
//! `logs/route_skips.csv`, and counts followed launches skipped for distance per day
//! (`logs/distance_daily.csv`). Each sweep's measurements: `logs/leader_rtt.csv`.

use {
    super::{clients, clock::now_ns},
    crate::{config::env_opt, utils::leaders},
    log::{info, warn},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        io::Write,
        net::SocketAddr,
        str::FromStr,
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub on: bool,
    pub max_rtt_us: u32,
    pub last_slot_ms: u64,
    pub every: Duration,
}

impl Settings {
    pub fn from_env() -> Self {
        let num = |k: &str, d: f64| env_opt(k).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(d);
        Settings {
            on: !env_opt("DISTANCE_RULE").is_some_and(|v| v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false")),
            max_rtt_us: (num("DISTANCE_MAX_RTT_MS", 30.0) * 1000.0) as u32,
            last_slot_ms: num("DISTANCE_LAST_SLOT_MS", 165.0) as u64,
            every: Duration::from_secs(num("DISTANCE_MEASURE_SECS", 300.0) as u64),
        }
    }
}

pub static SETTINGS: LazyLock<Settings> = LazyLock::new(Settings::from_env);

/// Slots the leader ring covers (about 1.8 hours).
const RING: usize = 16_384;
/// `slot << 32 | handshake µs` per slot (`u32::MAX`: no answer); 0: not filled.
static RTT_BY_SLOT: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
const UNKNOWN: u64 = u32::MAX as u64;
/// When each slot was first seen on the early feeds (`now_ns`), and which slot the cell holds.
const SEEN: usize = 4_096;
static SEEN_SLOT: [AtomicU64; SEEN] = [const { AtomicU64::new(0) }; SEEN];
static SEEN_NS: [AtomicU64; SEEN] = [const { AtomicU64::new(0) }; SEEN];

/// A frame or shred of `slot` arrived at `t` (`now_ns`). Allocation-free.
#[inline]
pub fn note_seen(slot: u64, t: u64) {
    let i = (slot as usize) % SEEN;
    if SEEN_SLOT[i].load(Ordering::Relaxed) == slot {
        SEEN_NS[i].fetch_min(t, Ordering::Relaxed);
    } else {
        SEEN_NS[i].store(t, Ordering::Relaxed);
        SEEN_SLOT[i].store(slot, Ordering::Relaxed);
    }
}

/// When `slot` was first seen, if the cell still holds it.
#[inline]
pub fn slot_seen(slot: u64) -> Option<u64> {
    let i = (slot as usize) % SEEN;
    (SEEN_SLOT[i].load(Ordering::Relaxed) == slot).then(|| SEEN_NS[i].load(Ordering::Relaxed))
}

/// When each create was first received (`now_ns`), by mint, for the slow path: it may see the
/// create later (the regular stream), and the rule needs the create's time in its slot.
const CREATES: usize = 4_096;
static CREATE_KEY: [AtomicU64; CREATES] = [const { AtomicU64::new(0) }; CREATES];
static CREATE_NS: [AtomicU64; CREATES] = [const { AtomicU64::new(0) }; CREATES];

fn mint_key(mint: &[u8; 32]) -> u64 {
    u64::from_le_bytes(mint[..8].try_into().expect("8")) | 1
}

/// The hot path received the create of `mint` at `t` (`now_ns`). Allocation-free.
#[inline]
pub fn note_create(mint: &[u8; 32], t: u64) {
    let key = mint_key(mint);
    let i = (key as usize >> 1) % CREATES;
    if CREATE_KEY[i].load(Ordering::Relaxed) == key {
        CREATE_NS[i].fetch_min(t, Ordering::Relaxed);
    } else {
        CREATE_KEY[i].store(0, Ordering::Relaxed);
        CREATE_NS[i].store(t, Ordering::Relaxed);
        CREATE_KEY[i].store(key, Ordering::Release);
    }
}

/// When the hot path received the create of `mint`, if it did.
pub fn create_seen(mint: &[u8; 32]) -> Option<u64> {
    let key = mint_key(mint);
    let i = (key as usize >> 1) % CREATES;
    (CREATE_KEY[i].load(Ordering::Acquire) == key).then(|| CREATE_NS[i].load(Ordering::Relaxed))
}

/// The handshake time (µs) to `slot`'s leader: Some(None) when it did not answer, None when the
/// ring does not hold the slot.
#[inline]
pub fn rtt_of_slot(slot: u64) -> Option<Option<u32>> {
    let v = RTT_BY_SLOT[(slot as usize) % RING].load(Ordering::Relaxed);
    (v != 0 && v >> 32 == slot).then(|| {
        let us = v & 0xFFFF_FFFF;
        (us != UNKNOWN).then_some(us as u32)
    })
}

/// Sets the handshake time of `slot`'s leader (the test kit and benchmarks: a near leader).
pub fn set_slot_rtt(slot: u64, rtt_us: Option<u32>) {
    let us = rtt_us.map_or(UNKNOWN, |u| u as u64);
    RTT_BY_SLOT[(slot as usize) % RING].store(slot << 32 | us, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Go,
    /// The slot's leader answered over the limit, or not at all (None).
    Far { rtt_us: Option<u32> },
    /// The create is in its leader's last slot, the next leader is far, and it came this late.
    LastSlotLate { next_rtt_us: Option<u32>, offset_ms: u64 },
}

impl Verdict {
    pub fn reason(self) -> &'static str {
        match self {
            Verdict::Go => "go",
            Verdict::Far { rtt_us: None } => "leader did not answer",
            Verdict::Far { .. } => "leader too far",
            Verdict::LastSlotLate { .. } => "last slot, next leader far, create late",
        }
    }
}

/// The distance rule for a create in `slot` received at `t_recv` (`now_ns`). Allocation-free.
#[inline]
pub fn verdict(slot: u64, t_recv: u64) -> Verdict {
    verdict_with(&SETTINGS, slot, t_recv, rtt_of_slot, slot_seen)
}

/// `verdict` with its inputs given (the tests).
#[inline]
pub fn verdict_with(s: &Settings, slot: u64, t_recv: u64, rtt: impl Fn(u64) -> Option<Option<u32>>, seen: impl Fn(u64) -> Option<u64>) -> Verdict {
    if !s.on {
        return Verdict::Go;
    }
    let near = |r: Option<Option<u32>>| matches!(r, Some(Some(us)) if us < s.max_rtt_us);
    let here = rtt(slot);
    if !near(here) {
        return Verdict::Far { rtt_us: here.flatten() };
    }
    // Every leader holds four slots from the epoch's start (a multiple of 4).
    if slot % 4 == 3 {
        let next = rtt(slot + 1);
        if !near(next) {
            let offset_ms = seen(slot).map_or(u64::MAX, |start| t_recv.saturating_sub(start) / 1_000_000);
            if offset_ms > s.last_slot_ms {
                return Verdict::LastSlotLate { next_rtt_us: next.flatten(), offset_ms };
            }
        }
    }
    Verdict::Go
}

// ---- the side task ----------------------------------------------------------------------------

/// Per leader identity: the last handshake time (µs; None: no answer) and when it was measured.
static CACHE: LazyLock<Mutex<HashMap<Pubkey, (Option<u32>, Instant)>>> = LazyLock::new(Default::default);
/// Per TPU address: its last three sweeps' handshake times (None: no answer).
static HISTORY: LazyLock<Mutex<HashMap<SocketAddr, std::collections::VecDeque<Option<u32>>>>> = LazyLock::new(Default::default);
/// The current and next epoch's schedules: (first slot, producer index per slot, producers).
type Schedule = (u64, Vec<u32>, Vec<Pubkey>);
static SCHEDULES: LazyLock<Mutex<Vec<Schedule>>> = LazyLock::new(Default::default);

fn leader_at(schedules: &[Schedule], slot: u64) -> Option<Pubkey> {
    schedules.iter().find_map(|(start, by_slot, producers)| {
        let i = slot.checked_sub(*start)? as usize;
        producers.get(*by_slot.get(i)? as usize).copied()
    })
}

/// The leader of `slot` from the side task's schedules (the next epoch's too).
pub fn leader_of(slot: u64) -> Option<Pubkey> {
    leader_at(&SCHEDULES.lock().unwrap_or_else(|e| e.into_inner()), slot)
}

/// The cached handshake time to a leader (µs; Some(None): it did not answer).
pub fn rtt_of(identity: &Pubkey) -> Option<Option<u32>> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(identity).map(|(r, _)| *r)
}

async fn load_schedules() -> anyhow::Result<()> {
    let rpc = &crate::config::RPC_CLIENT;
    let epoch = rpc.get_epoch_info().await?;
    let start = epoch.absolute_slot - epoch.slot_index;
    let mut out = Vec::new();
    for first in [start, start + epoch.slots_in_epoch] {
        let Some(schedule) = rpc.get_leader_schedule(Some(first)).await? else { continue };
        let mut by_slot = vec![u32::MAX; epoch.slots_in_epoch as usize];
        let mut producers = Vec::with_capacity(schedule.len());
        for (identity, slots) in schedule {
            let Ok(key) = Pubkey::from_str(&identity) else { continue };
            let index = producers.len() as u32;
            producers.push(key);
            for s in slots {
                if let Some(e) = by_slot.get_mut(s) {
                    *e = index;
                }
            }
        }
        out.push((first, by_slot, producers));
    }
    *SCHEDULES.lock().unwrap_or_else(|e| e.into_inner()) = out;
    Ok(())
}

/// The QUIC handshake time to `addr` (µs): to an established connection, or to the peer's refusal
/// (it answered); None when nothing came back within 600 ms.
async fn handshake_us(endpoint: &quinn::Endpoint, addr: SocketAddr) -> Option<u32> {
    let t = Instant::now();
    let connecting = endpoint.connect(addr, "connect").ok()?;
    match tokio::time::timeout(Duration::from_millis(600), connecting).await {
        Ok(Ok(conn)) => {
            let us = t.elapsed().as_micros() as u32;
            conn.close(0u32.into(), b"");
            Some(us)
        }
        Ok(Err(quinn::ConnectionError::TimedOut)) | Err(_) => None,
        Ok(Err(_)) => Some(t.elapsed().as_micros() as u32),
    }
}

pub const SNAPSHOT_CSV: &str = "logs/leader_rtt.csv";

/// The last sweep, one row per leader (rewritten each sweep): its slots this epoch and next, client,
/// class, city and handshake time.
fn write_snapshot(results: &[(Pubkey, Option<u32>)]) {
    let slots: HashMap<Pubkey, (u64, u64)> = {
        let sch = SCHEDULES.lock().unwrap_or_else(|e| e.into_inner());
        let mut m: HashMap<Pubkey, (u64, u64)> = HashMap::new();
        for (k, (_, by_slot, producers)) in sch.iter().enumerate() {
            for p in by_slot.iter().filter_map(|i| producers.get(*i as usize)) {
                let e = m.entry(*p).or_default();
                if k == 0 { e.0 += 1 } else { e.1 += 1 }
            }
        }
        m
    };
    let mut out = String::from("leader,slots_this_epoch,slots_next_epoch,client,class,city,country,rtt_ms\n");
    for (id, rtt) in results {
        let (a, b) = slots.get(id).copied().unwrap_or_default();
        let place = leaders::place(id);
        out.push_str(&format!(
            "{id},{a},{b},{},{},{},{},{}\n",
            leaders::client(id).unwrap_or_default(),
            clients::of_identity(id).map_or("", |c| c.name()),
            place.as_ref().map_or(String::new(), |p| p.city.replace(',', " ")),
            place.as_ref().map_or(String::new(), |p| p.country.clone()),
            rtt.map_or("none".into(), |u| format!("{:.1}", u as f64 / 1000.0))
        ));
    }
    if crate::store::db_on() {
        let mut lines = out.lines();
        let header = "leader,slots_this_epoch,slots_next_epoch,client,class,city,country,rtt_ms";
        lines.next();
        let rows = lines.map(|l| crate::store::split_csv(l).iter().map(|f| crate::store::field(f)).collect()).collect();
        crate::store::write(crate::store::Row::Snapshot { table: "leader_rtt", header, rows, rx_ns: Some(crate::store::now_ns()) });
    }
    if crate::store::files_on() {
        let _ = std::fs::write(SNAPSHOT_CSV, out);
    }
}

/// Fills the leader ring for the slots ahead of the feed.
fn fill_ring(from: u64) -> (usize, usize) {
    let schedules = SCHEDULES.lock().unwrap_or_else(|e| e.into_inner());
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut filled, mut near) = (0, 0);
    for slot in from..from + RING as u64 {
        let Some(id) = leader_at(&schedules, slot) else { continue };
        let Some((rtt, _)) = cache.get(&id) else { continue };
        let us = rtt.map_or(UNKNOWN, |u| u as u64);
        RTT_BY_SLOT[(slot as usize) % RING].store(slot << 32 | us, Ordering::Relaxed);
        filled += 1;
        near += usize::from(rtt.is_some_and(|u| u < SETTINGS.max_rtt_us));
    }
    (filled, near)
}

/// Measures every leader of the current and next epoch every `every`, and keeps the ring filled
/// (on the runtime).
pub fn spawn() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let s = *SETTINGS;
    if !s.on {
        info!("distance rule: off (DISTANCE_RULE=off)");
        return;
    }
    leaders::ensure_spawned();
    info!(
        "distance rule: buy only when the slot's leader answers a QUIC handshake under {} ms (measured every {} s); in a leader's last slot with a far next leader, only in the slot's first {} ms",
        s.max_rtt_us / 1000,
        s.every.as_secs(),
        s.last_slot_ms
    );
    tokio::spawn(async move {
        let endpoint = match super::lanes::quic_endpoint(&solana_keypair::Keypair::new()) {
            Ok(e) => e,
            Err(e) => {
                warn!("distance rule: no QUIC endpoint ({e:#}): every launch counts as far");
                return;
            }
        };
        let mut last_measure: Option<Instant> = None;
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            if last_measure.is_none_or(|t| t.elapsed() >= s.every) {
                if let Err(e) = load_schedules().await {
                    warn!("distance rule: leader schedules: {e:#}");
                }
                let ids: Vec<Pubkey> = {
                    let sch = SCHEDULES.lock().unwrap_or_else(|e| e.into_inner());
                    let mut v: Vec<Pubkey> = sch.iter().flat_map(|(_, _, p)| p.iter().copied()).collect();
                    v.sort();
                    v.dedup();
                    v
                };
                // One measurement per TPU address: BAM and Harmonic leaders share a few (the
                // BAM or Harmonic node), and hundreds of handshakes to one node in a few seconds
                // go unanswered.
                let addr_of: Vec<(Pubkey, Option<SocketAddr>)> = ids.iter().map(|id| (*id, leaders::tpu_quic(id))).collect();
                let mut targets: Vec<SocketAddr> = addr_of.iter().filter_map(|(_, a)| *a).collect();
                targets.sort();
                targets.dedup();
                let t0 = Instant::now();
                use futures::stream::{self, StreamExt};
                let measured: Vec<(SocketAddr, Option<u32>)> = stream::iter(targets)
                    .map(|addr| {
                        let endpoint = endpoint.clone();
                        async move {
                            // The fastest of three, 100 ms apart: one handshake can be held up
                            // by this host's own load (the crypto and the runtime share cores 0-3).
                            let mut best: Option<u32> = None;
                            for k in 0..3 {
                                if k > 0 {
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                                if let Some(us) = handshake_us(&endpoint, addr).await {
                                    best = Some(best.map_or(us, |b| b.min(us)));
                                }
                            }
                            (addr, best)
                        }
                    })
                    .buffer_unordered(16)
                    .collect()
                    .await;
                // Each address keeps its last three sweeps; it counts as its best answer among
                // them (no answer only when none of the three answered).
                let by_addr: HashMap<SocketAddr, Option<u32>> = {
                    let mut history = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
                    measured
                        .iter()
                        .map(|(addr, rtt)| {
                            let h = history.entry(*addr).or_default();
                            h.push_back(*rtt);
                            while h.len() > 3 {
                                h.pop_front();
                            }
                            (*addr, h.iter().flatten().min().copied())
                        })
                        .collect()
                };
                let addresses = measured.len();
                let results: Vec<(Pubkey, Option<u32>)> = addr_of.iter().map(|(id, a)| (*id, a.and_then(|a| by_addr.get(&a).copied().flatten()))).collect();
                let now = Instant::now();
                let (answered, near) = {
                    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                    for (id, rtt) in &results {
                        cache.insert(*id, (*rtt, now));
                    }
                    (results.iter().filter(|(_, r)| r.is_some()).count(), results.iter().filter(|(_, r)| r.is_some_and(|u| u < s.max_rtt_us)).count())
                };
                let (followed, checked, skipped) = today_counts();
                info!(
                    "distance rule: {} leaders of this epoch and the next ({addresses} TPU addresses) measured in {:.1} s: {answered} answered, {near} under {} ms; today {skipped} of {checked} followed launches past the entry rules skipped for distance ({followed} followed)",
                    results.len(),
                    t0.elapsed().as_secs_f64(),
                    s.max_rtt_us / 1000
                );
                last_measure = Some(now);
                let _ = tokio::task::spawn_blocking(move || write_snapshot(&results)).await;
            }
            let mut slot = super::lanes::FEED_SLOT.load(Ordering::Relaxed);
            if slot == 0 {
                slot = crate::config::RPC_CLIENT.get_slot().await.unwrap_or(0);
            }
            if slot > 0 {
                let _ = tokio::task::spawn_blocking(move || fill_ring(slot.saturating_sub(64))).await;
            }
        }
    });
}

// ---- the skip log --------------------------------------------------------------------------------

pub const SKIPS_CSV: &str = "logs/route_skips.csv";
pub const DAILY_CSV: &str = "logs/distance_daily.csv";
const SKIPS_HEADER: &str = "time,mint,followed,slot,reason,leader,client,class,city,rtt_ms,next_rtt_ms,offset_ms";

/// Per UTC day: followed launches, those that passed the entry rules and reached the distance
/// rule, and those it skipped.
#[derive(Default)]
struct Day {
    date: String,
    followed: u64,
    checked: u64,
    skipped: u64,
}

static DAY: LazyLock<Mutex<Day>> = LazyLock::new(Default::default);

fn with_today(f: impl FnOnce(&mut Day)) {
    let today = super::positions::today();
    let mut d = DAY.lock().unwrap_or_else(|e| e.into_inner());
    if d.date != today {
        if !d.date.is_empty() {
            info!(
                "distance rule, {}: {} followed launches, {} passed the entry rules, {} of those skipped for distance",
                d.date, d.followed, d.checked, d.skipped
            );
            append(DAILY_CSV, "date,followed,passed_rules,skipped_for_distance", &format!("{},{},{},{}\n", d.date, d.followed, d.checked, d.skipped));
        }
        *d = Day { date: today, ..Default::default() };
    }
    f(&mut d);
}

/// A followed deployer's launch (the slow path's count).
pub fn note_followed() {
    with_today(|d| d.followed += 1);
}

/// A followed launch that passed the entry rules met the distance rule, and whether it skipped it.
pub fn note_checked(skipped: bool) {
    with_today(|d| {
        d.checked += 1;
        d.skipped += u64::from(skipped);
    });
}

/// Today's (followed launches, passed the rules, skipped for distance).
pub fn today_counts() -> (u64, u64, u64) {
    let d = DAY.lock().unwrap_or_else(|e| e.into_inner());
    (d.followed, d.checked, d.skipped)
}

fn append(path: &str, header: &'static str, line: &str) {
    let table = if path == SKIPS_CSV { "route_skips" } else { "distance_daily" };
    crate::store::mirror_line(table, header, line);
    if !crate::store::files_on() {
        return;
    }
    let new = !std::path::Path::new(path).exists();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        if new {
            let _ = writeln!(f, "{header}");
        }
        let _ = f.write_all(line.as_bytes());
    }
}

/// Logs a launch the route or the distance rule skipped: the leader's city, handshake time and
/// client (log and `logs/route_skips.csv`).
pub fn log_skip(mint: &Pubkey, followed: bool, slot: u64, reason: &str, v: Verdict) {
    let id = leader_of(slot).or_else(|| leaders::leader(slot));
    let client = id.and_then(|i| leaders::client(&i)).unwrap_or_default();
    let class = clients::lookup(slot).map_or("", |c| c.name());
    let city = leaders::producer(slot).and_then(|(_, p)| p).map(|p| p.to_string()).unwrap_or_default();
    let ms = |us: Option<u32>| us.map_or("none".to_string(), |u| format!("{:.1}", u as f64 / 1000.0));
    let (rtt, next, offset) = match v {
        Verdict::Far { rtt_us } => (ms(rtt_us), String::new(), String::new()),
        Verdict::LastSlotLate { next_rtt_us, offset_ms } => (id.and_then(|i| rtt_of(&i)).map_or("-".into(), ms), ms(next_rtt_us), offset_ms.to_string()),
        Verdict::Go => (id.and_then(|i| rtt_of(&i)).map_or("-".into(), ms), String::new(), String::new()),
    };
    info!(
        "skip {mint}: {reason}: leader {} ({class}, {city}), handshake {rtt} ms{}{}",
        id.map_or("?".into(), |i| i.to_string()),
        if next.is_empty() { String::new() } else { format!(", next leader {next} ms") },
        if offset.is_empty() { String::new() } else { format!(", create {offset} ms into the slot") }
    );
    let line = format!(
        "{},{mint},{followed},{slot},{reason},{},{client},{class},{},{rtt},{next},{offset}\n",
        crate::utils::dashboard::now_unix(),
        id.map_or(String::new(), |i| i.to_string()),
        city.replace(',', " ")
    );
    append(SKIPS_CSV, SKIPS_HEADER, &line);
}

/// `now_ns` of an `Instant` (the slow path's receive time on the hot path's clock).
pub fn ns_of(at: Instant) -> u64 {
    now_ns().saturating_sub(at.elapsed().as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Settings = Settings { on: true, max_rtt_us: 20_000, last_slot_ms: 250, every: Duration::from_secs(300) };

    #[test]
    fn near_leaders_go_far_ones_and_silent_ones_do_not() {
        let rtt = |near: bool| move |_: u64| Some(Some(if near { 5_000 } else { 40_000 }));
        let never = |_: u64| None;
        assert_eq!(verdict_with(&S, 100, 0, rtt(true), never), Verdict::Go);
        assert_eq!(verdict_with(&S, 100, 0, rtt(false), never), Verdict::Far { rtt_us: Some(40_000) });
        assert_eq!(verdict_with(&S, 100, 0, |_| Some(None), never), Verdict::Far { rtt_us: None });
        assert_eq!(verdict_with(&S, 100, 0, |_| None, never), Verdict::Far { rtt_us: None }, "not measured: far");
        let off = Settings { on: false, ..S };
        assert_eq!(verdict_with(&off, 100, 0, rtt(false), never), Verdict::Go);
    }

    #[test]
    fn the_last_slot_with_a_far_next_leader_needs_an_early_create() {
        // Slot 103 is its leader's last (103 % 4 == 3); 104 belongs to a far leader.
        let rtt = |slot: u64| Some(Some(if slot == 104 { 60_000 } else { 3_000 }));
        let started = |_: u64| Some(1_000_000_000u64);
        assert_eq!(verdict_with(&S, 103, 1_000_000_000 + 200_000_000, rtt, started), Verdict::Go, "200 ms in");
        assert_eq!(
            verdict_with(&S, 103, 1_000_000_000 + 300_000_000, rtt, started),
            Verdict::LastSlotLate { next_rtt_us: Some(60_000), offset_ms: 300 }
        );
        assert!(matches!(verdict_with(&S, 103, 5, rtt, |_| None), Verdict::LastSlotLate { .. }), "slot start unknown: late");
        // Not the last slot, or a near next leader: no timing rule.
        assert_eq!(verdict_with(&S, 102, u64::MAX, rtt, started), Verdict::Go);
        assert_eq!(verdict_with(&S, 103, u64::MAX, |_| Some(Some(3_000)), started), Verdict::Go);
    }

    #[test]
    fn the_rings_answer_only_for_their_slot() {
        let slot = 900_000_007u64;
        RTT_BY_SLOT[(slot as usize) % RING].store(slot << 32 | 12_345, Ordering::Relaxed);
        assert_eq!(rtt_of_slot(slot), Some(Some(12_345)));
        RTT_BY_SLOT[(slot as usize) % RING].store(slot << 32 | UNKNOWN, Ordering::Relaxed);
        assert_eq!(rtt_of_slot(slot), Some(None));
        assert_eq!(rtt_of_slot(slot + RING as u64), None);
        note_seen(slot, 500);
        note_seen(slot, 400);
        note_seen(slot, 900);
        assert_eq!(slot_seen(slot), Some(400), "the earliest");
        assert_eq!(slot_seen(slot + SEEN as u64), None);
    }
}
