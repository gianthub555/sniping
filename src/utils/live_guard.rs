//! Limits for a small live test, from `.env`. Buying stops for good (`control::stop_buying`, until
//! restart) after LIVE_MAX_BUYS buys that got tokens, after LIVE_MAX_INCLUDED buys included in a
//! block (tokens or a failure on chain), after LIVE_MAX_FAILED_IN_A_ROW buys in a row
//! that got none (lost the race, failed on chain or never landed), when the wallet holds less than
//! LIVE_MIN_WALLET_SOL, or when a hot feed has been silent for LIVE_FEED_STALL_SECS. Open
//! positions are still sold. Each limit is off unless set. The buy counts are live buys only (the
//! owner's rule, 2026-10-12): a paper buy never trips them, so the live limits stay set in paper.

use {
    crate::{
        config::{env_opt, RPC_CLIENT},
        utils::{
            control,
            dashboard::{activity, ActivityKind},
        },
    },
    log::{info, warn},
    std::{
        sync::{
            atomic::{AtomicU32, Ordering},
            LazyLock,
        },
        time::Duration,
    },
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub max_buys: Option<u32>,
    pub max_included: Option<u32>,
    pub max_failed_in_a_row: Option<u32>,
    pub min_wallet_lamports: Option<u64>,
    pub feed_stall_secs: Option<u64>,
}

impl Limits {
    pub fn from_env() -> Self {
        let num = |name: &str| env_opt(name).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v > 0.0);
        Limits {
            max_buys: num("LIVE_MAX_BUYS").map(|v| v as u32),
            max_included: num("LIVE_MAX_INCLUDED").map(|v| v as u32),
            max_failed_in_a_row: num("LIVE_MAX_FAILED_IN_A_ROW").map(|v| v as u32),
            min_wallet_lamports: num("LIVE_MIN_WALLET_SOL").map(|v| (v * 1e9).round() as u64),
            feed_stall_secs: num("LIVE_FEED_STALL_SECS").map(|v| v as u64),
        }
    }

    pub fn any(&self) -> bool {
        self.max_buys.is_some() || self.max_included.is_some() || self.max_failed_in_a_row.is_some() || self.min_wallet_lamports.is_some() || self.feed_stall_secs.is_some()
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = self.max_buys {
            parts.push(format!("stop after {n} buys that got tokens"));
        }
        if let Some(n) = self.max_included {
            parts.push(format!("stop after {n} buys included in a block"));
        }
        if let Some(n) = self.max_failed_in_a_row {
            parts.push(format!("stop after {n} failed buys in a row"));
        }
        if let Some(l) = self.min_wallet_lamports {
            parts.push(format!("stop when the wallet holds under {} SOL", l as f64 / 1e9));
        }
        if let Some(s) = self.feed_stall_secs {
            parts.push(format!("stop when a hot feed is silent for {s} s"));
        }
        parts.join("; ")
    }
}

pub static LIMITS: LazyLock<Limits> = LazyLock::new(Limits::from_env);

/// Buys that got tokens, buys included in a block, and buys in a row that got none.
static GOT: AtomicU32 = AtomicU32::new(0);
static INCLUDED: AtomicU32 = AtomicU32::new(0);
static FAILED_IN_A_ROW: AtomicU32 = AtomicU32::new(0);

/// The stop a buy result leads to, for the counts after it (pure).
pub fn verdict(limits: &Limits, got: u32, included: u32, failed_in_a_row: u32) -> Option<String> {
    if limits.max_buys.is_some_and(|n| got >= n) {
        return Some(format!("{got} buys got tokens (limit {})", limits.max_buys.unwrap_or(0)));
    }
    if limits.max_included.is_some_and(|n| included >= n) {
        return Some(format!("{included} buys included in a block (limit {})", limits.max_included.unwrap_or(0)));
    }
    if limits.max_failed_in_a_row.is_some_and(|n| failed_in_a_row >= n) {
        return Some(format!("{failed_in_a_row} buys in a row got no tokens (kill switch)"));
    }
    None
}

/// Whether the wallet is under the floor (pure).
pub fn wallet_too_low(limits: &Limits, lamports: u64) -> bool {
    limits.min_wallet_lamports.is_some_and(|floor| lamports < floor)
}

/// A buy's result: whether it got tokens and whether it was included in a block (tokens or a
/// failure on chain). `kind` names it in the log ("live", "paper").
pub fn on_buy_result(got_tokens: bool, included: bool, kind: &str) {
    let limits = &*LIMITS;
    if !limits.any() || !counts(kind) {
        return;
    }
    let included = if included { INCLUDED.fetch_add(1, Ordering::SeqCst) + 1 } else { INCLUDED.load(Ordering::SeqCst) };
    let (got, failed) = if got_tokens {
        FAILED_IN_A_ROW.store(0, Ordering::SeqCst);
        (GOT.fetch_add(1, Ordering::SeqCst) + 1, 0)
    } else {
        (GOT.load(Ordering::SeqCst), FAILED_IN_A_ROW.fetch_add(1, Ordering::SeqCst) + 1)
    };
    info!(
        "live guard: {kind} buy {}; {got} got tokens{}, {included} included{}, {failed} failed in a row{}",
        if got_tokens { "got tokens" } else { "got none" },
        limits.max_buys.map_or(String::new(), |n| format!(" of {n}")),
        limits.max_included.map_or(String::new(), |n| format!(" of {n}")),
        limits.max_failed_in_a_row.map_or(String::new(), |n| format!(" (kill at {n})"))
    );
    if let Some(reason) = verdict(limits, got, included, failed) {
        stop(&reason);
    }
}

/// Whether a buy of `kind` ("live", "paper") counts toward the limits: live ones only.
fn counts(kind: &str) -> bool {
    kind == "live"
}

/// Stops buying for good, once, and says so everywhere.
pub fn stop(reason: &str) {
    if control::stop_buying(reason) {
        warn!("LIVE GUARD STOPPED BUYING: {reason}. Open positions are still sold; restart to buy again.");
        activity(ActivityKind::Skipped, None, None, format!("Buying stopped: {reason}. Open positions are still sold."));
    }
}

/// Reads the wallet and stops when it is under the floor.
pub async fn check_wallet() {
    let limits = &*LIMITS;
    if limits.min_wallet_lamports.is_none() {
        return;
    }
    match RPC_CLIENT.get_balance(&crate::config::trading_wallet()).await {
        Ok(lamports) if wallet_too_low(limits, lamports) => {
            stop(&format!("the wallet holds {:.6} SOL (floor {} SOL)", lamports as f64 / 1e9, limits.min_wallet_lamports.unwrap_or(0) as f64 / 1e9))
        }
        Ok(_) => {}
        Err(e) => warn!("live guard: wallet balance not read: {e}"),
    }
}

/// The feed a stall check found silent: its name and how long (pure). `feeds` holds each watched
/// feed's name and the age of its last message in ns (None: nothing yet); `uptime_ns` is the time
/// since start. A feed that never delivered counts as stalled once the start is that far behind.
pub fn stalled(feeds: &[(&'static str, Option<u64>)], uptime_ns: u64, stall_ns: u64) -> Option<(&'static str, u64)> {
    feeds.iter().find_map(|(name, age)| match age {
        Some(a) if *a >= stall_ns => Some((*name, *a)),
        None if uptime_ns >= stall_ns.max(30_000_000_000) => Some((*name, uptime_ns)),
        _ => None,
    })
}

/// Starts the guard's checks (on the runtime): the hot feeds once a second, the wallet every 30 s.
pub fn spawn() {
    let limits = *LIMITS;
    if !limits.any() {
        return;
    }
    info!("live guard: {}", limits.describe());
    tokio::spawn(async move {
        let start = crate::hot::clock::now_ns();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut n = 0u64;
        loop {
            tick.tick().await;
            n += 1;
            if let Some(secs) = limits.feed_stall_secs {
                let now = crate::hot::clock::now_ns();
                let feeds = crate::hot::engine::feed_ages(now);
                if let Some((name, age)) = stalled(&feeds, now.saturating_sub(start), secs * 1_000_000_000) {
                    stop(&format!("the {name} feed has been silent for {:.1} s (kill switch)", age as f64 / 1e9));
                }
            }
            if n % 30 == 1 {
                check_wallet().await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paper buys never count toward the live limits (2026-10-12); live ones do.
    #[test]
    fn only_live_buys_count() {
        assert!(counts("live"));
        assert!(!counts("paper"));
        assert!(!counts("sample"));
    }

    const L: Limits = Limits { max_buys: Some(10), max_included: None, max_failed_in_a_row: Some(3), min_wallet_lamports: Some(10_000_000), feed_stall_secs: Some(5) };

    #[test]
    fn stops_at_the_buy_limit_and_the_failure_streak() {
        assert_eq!(verdict(&L, 9, 9, 2), None);
        assert!(verdict(&L, 10, 10, 0).unwrap().contains("10 buys got tokens"));
        assert!(verdict(&L, 4, 7, 3).unwrap().contains("kill switch"));
        let c = Limits { max_buys: None, max_included: Some(15), max_failed_in_a_row: Some(6), ..L };
        assert_eq!(verdict(&c, 2, 14, 5), None);
        assert!(verdict(&c, 2, 15, 0).unwrap().contains("15 buys included"));
        assert!(verdict(&c, 2, 9, 6).unwrap().contains("kill switch"));
        let off = Limits { max_buys: None, max_included: None, max_failed_in_a_row: None, min_wallet_lamports: None, feed_stall_secs: None };
        assert_eq!(verdict(&off, 1000, 1000, 1000), None);
        assert!(!off.any());
    }

    #[test]
    fn the_wallet_floor_is_strict() {
        assert!(wallet_too_low(&L, 9_999_999));
        assert!(!wallet_too_low(&L, 10_000_000));
    }

    #[test]
    fn a_silent_feed_stops_and_a_missing_one_only_after_the_start_grace() {
        let s = 5_000_000_000;
        assert_eq!(stalled(&[("shreds", Some(1_000_000)), ("preprocessed", Some(200_000_000))], 60_000_000_000, s), None);
        assert_eq!(stalled(&[("shreds", Some(1_000_000)), ("preprocessed", Some(6_000_000_000))], 60_000_000_000, s), Some(("preprocessed", 6_000_000_000)));
        assert_eq!(stalled(&[("shreds", None)], 10_000_000_000, s), None, "still starting");
        assert_eq!(stalled(&[("shreds", None)], 31_000_000_000, s), Some(("shreds", 31_000_000_000)));
    }
}
