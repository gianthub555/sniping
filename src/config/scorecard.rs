//! The slot-0 entry scorecard (2026-10-12; research/filter-pXGQ-2026-10-12/FILTER_SPEC.md, card
//! `scorecard_bot.json`). Every followed launch gets points from 15 facts known in the create's
//! slot; it is bought at `SCORECARD_MIN` or more. With `SCORECARD=true` the score replaces the
//! dev-buy range, the deployer's minimum balance and wallet age, the relaunch skip and the
//! no-buyers record (those facts are in the score); the follow rule and the mayhem skip stay.
//!
//! Fitted on Oct 2-7 followed launches (ridge on binned facts, the bot's own exit replayed at
//! position 0, 0.5 SOL), tested Oct 7-8, then on Oct 8 20:13 - Oct 12 19:00 (6,354 launches it
//! never saw): score >= 61 (top 20%) kept 8.7 launches an hour, +0.102 SOL a try net of tips,
//! 80% won, 9% went under 0.9x before their best.
//!
//! Bins are [lo, hi); a value under the first cut takes the first bin, at or over the last the
//! last. A fact the bot does not know takes the value the training data gave it (see `ScoreFacts`).

use std::sync::LazyLock;

pub const INTERCEPT: i32 = 22;
/// The card's top-20% threshold (the precision mode).
pub const DEFAULT_MIN: i32 = 61;

/// Index of each fact in `CARD`.
pub const FACTS: [&str; 15] = [
    "secs_since_last_launch",
    "launches_24h",
    "launches_1h",
    "reference_profit_sol",
    "reference_trades",
    "no_pop_rate",
    "earlier_pnl_sol",
    "earlier_down_share",
    "dev_buy_sol",
    "dev_pct",
    "bundle_wallets",
    "bundle_sol",
    "deployer_sol",
    "funded_sol",
    "wallet_age_h",
];

const INF: f64 = f64::INFINITY;

/// (cuts, points): bin i is [cuts[i], cuts[i+1]); `points[i]` its points.
pub const CARD: [(&[f64], &[i32]); 15] = [
    (&[0.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 21600.0, 86400.0, INF], &[-23, -10, 4, 7, 9, 3, 3, 8]),
    (&[0.0, 1.0, 3.0, 6.0, 11.0, 21.0, 31.0, INF], &[8, 12, -5, 2, -4, -8, -4]),
    (&[0.0, 1.0, 2.0, 4.0, INF], &[14, -1, 0, -13]),
    (&[-INF, 0.0, 0.77, 1.5, 3.0, 6.0, INF], &[-6, -10, 3, -9, 4, 18]),
    (&[0.0, 3.0, 6.0, 11.0, 31.0, INF], &[-1, -3, -8, 9, 3]),
    (&[-10.0, -0.5, 0.001, 0.25, 0.5, 1.01], &[2, 5, 1, -1, -7]),
    (&[-10.0, -1.0, -0.02, 0.0, 0.02, 0.1, INF], &[4, -7, -5, -2, -3, 13]),
    (&[-10.0, -1.0, 0.2, 0.34, 0.5, 1.01], &[4, 8, -4, -1, -7]),
    (&[0.0, 0.001, 0.7, 1.5, 3.0, 5.0, 10.0, INF], &[-1, 0, 1, 0, 3, 4, -7]),
    (&[0.0, 0.001, 3.5, 10.0, 20.0, INF], &[-1, -2, 6, 5, -7]),
    (&[0.0, 1.0, 2.0, 3.0, 5.0, INF], &[1, 6, -1, -1, -4]),
    (&[0.0, 0.001, 1.0, 3.0, 6.0, INF], &[0, 0, 10, 0, -10]),
    (&[0.0, 1.0, 2.0, 5.0, 10.0, 30.0, 100.0, INF], &[-1, 3, 0, -5, 6, 6, -9]),
    (&[-INF, 0.01, 0.5, 2.0, 10.0, INF], &[-5, 8, 6, 8, -17]),
    (&[-INF, 24.0, 168.0, 720.0, 4320.0, INF], &[-2, 1, 2, -3, 2]),
];

/// The facts as the card reads them, in `FACTS` order. Missing values as in the training data:
/// no earlier launch seen: 1e7 s since it; no reference record: profit 0, trades 0; no no-buyers
/// record and no earlier results: -9 (their own bins); wallet funding or age unknown: -inf (the
/// first bin).
pub type ScoreFacts = [f64; 15];

/// The points of `value` on fact `i`.
#[inline]
pub fn points(i: usize, value: f64) -> i32 {
    let (cuts, pts) = CARD[i];
    let mut bin = 0;
    while bin + 1 < pts.len() && value >= cuts[bin + 1] {
        bin += 1;
    }
    pts[bin]
}

/// The score. Allocation-free.
#[inline]
pub fn score(f: &ScoreFacts) -> i32 {
    let mut s = INTERCEPT;
    for (i, v) in f.iter().enumerate() {
        s += points(i, if v.is_nan() { -INF } else { *v });
    }
    s
}

/// Each fact's value and points, as JSON (logs and the database).
pub fn breakdown(f: &ScoreFacts) -> String {
    let parts: Vec<String> = FACTS
        .iter()
        .zip(f.iter())
        .enumerate()
        .map(|(i, (name, v))| {
            let shown = if v.is_finite() { format!("{v:.4}") } else { "null".into() };
            format!("\"{name}\":{{\"value\":{shown},\"points\":{}}}", points(i, if v.is_nan() { -INF } else { *v }))
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// The facts with the most negative points, for a skip message: "name -23, name -17".
pub fn worst(f: &ScoreFacts, n: usize) -> String {
    let mut p: Vec<(i32, &str)> = FACTS.iter().enumerate().map(|(i, name)| (points(i, f[i]), *name)).collect();
    p.sort();
    p.iter().take(n).filter(|(pts, _)| *pts < 0).map(|(pts, name)| format!("{name} {pts}")).collect::<Vec<_>>().join(", ")
}

/// Dev tokens as % of the supply for a dev buy of `lamports` SOL into a fresh curve (30 SOL and
/// 1.073 billion tokens virtual; 1 billion supply).
pub fn dev_percent(dev_buy_lamports: u64) -> f64 {
    let (v0, t0) = (30_000_000_000f64, 1_073_000_000_000_000f64);
    let tokens = t0 - v0 * t0 / (v0 + dev_buy_lamports as f64);
    100.0 * tokens / 1e15
}

pub struct Settings {
    pub enabled: bool,
    pub min: i32,
}

/// `SCORECARD=true|false` (default false) and `SCORECARD_MIN` (default 61), read once.
pub static SETTINGS: LazyLock<Settings> = LazyLock::new(|| Settings {
    enabled: std::env::var("SCORECARD").is_ok_and(|v| matches!(v.trim(), "true" | "1" | "on")),
    min: std::env::var("SCORECARD_MIN").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_MIN),
});

#[inline]
pub fn enabled() -> bool {
    SETTINGS.enabled
}

#[inline]
pub fn min() -> i32 {
    SETTINGS.min
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fact_has_one_more_cut_than_points() {
        for (i, (cuts, pts)) in CARD.iter().enumerate() {
            assert_eq!(cuts.len(), pts.len() + 1, "{}", FACTS[i]);
            assert!(cuts.windows(2).all(|w| w[0] < w[1]), "{}", FACTS[i]);
        }
    }

    #[test]
    fn bins_are_closed_below_and_open_above() {
        // secs_since_last_launch: [0,120) -23, [120,300) -10
        assert_eq!(points(0, 119.9), -23);
        assert_eq!(points(0, 120.0), -10);
        assert_eq!(points(0, 1e7), 8);
        // under the first cut: first bin; unknown funding (-inf): first bin
        assert_eq!(points(13, f64::NEG_INFINITY), -5);
        assert_eq!(points(13, 10.0), -17);
        // the -9 sentinel of "no earlier launches" has its own bin
        assert_eq!(points(6, -9.0), 4);
        assert_eq!(points(7, -9.0), 4);
        assert_eq!(points(5, -9.0), 2);
    }

    /// One launch scored by hand against the research's apply_card.py arithmetic.
    #[test]
    fn a_launch_scores_as_the_research_card() {
        let f: ScoreFacts = [7200.0, 2.0, 0.0, 8.0, 40.0, 0.0, 0.15, 0.1, 2.0, dev_percent(2_000_000_000), 1.0, 1.5, 20.0, 1.0, 400.0];
        // 22 + 3 + 12 + 14 + 18 + 3 + 5 + 13 + 8 + 0 + 6 + 6 + 10 + 6 + 6 + 2
        assert_eq!(score(&f), 22 + 3 + 12 + 14 + 18 + 3 + 5 + 13 + 8 + 0 + 6 + 6 + 10 + 6 + 6 + 2);
        assert!((dev_percent(2_000_000_000) - 6.71).abs() < 0.01, "{}", dev_percent(2_000_000_000));
        assert!(breakdown(&f).starts_with("{\"secs_since_last_launch\":{\"value\":7200.0000,\"points\":3}"));
    }
}
