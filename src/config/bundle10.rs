//! bundle10's switches (the owner's spec of 2026-10-13: research on 165k launches, searched on
//! Oct 3-7, tested on Oct 10-13). Everything defaults off, and with both switches off nothing of
//! bundle10 runs. The numbers are the research's and are not tuned here. The rule, the count and
//! the exit are in `hot::bundle10`.
//!
//! - `BUNDLE10_ENABLED` (false): the live path (decide and buy).
//! - `BUNDLE10_SHADOW` (false): every candidate goes to the store's `bundle10_shadow`; never signs
//!   or sends anything.
//! - `BUNDLE10_SIZE_SOL` (0.1): the buy, fixed (never scaled to the dev buy).
//! - `BUNDLE10_MAX_BUNDLE_SOL` (100): bundle10's own cap on the bundle's SOL; every other path
//!   keeps `shred_feed::MAX_EARLY_BUNDLE_LAMPORTS` (15 SOL).
//! - `BUNDLE10_MAX_POST_BUNDLE_MULT` (6): skip when the post-bundle price is more than this many
//!   times the launch price (a ceiling on a quote inflated by a bundle transaction that fails).
//! - `BUNDLE10_MAX_HOLD_S` (60): sell all this long after the create was received.
//! - `BUNDLE10_TRAIL` (0.15): once armed (1.05x the fill), sell all this far under the peak.
//! - `BUNDLE10_DAY_LOSS_SOL` (0.5): realized bundle10 loss since 00:00 UTC that stops bundle10
//!   buys until the next UTC day (open positions still exit).
//! - `BUNDLE10_MAX_OPEN` (5): open bundle10 positions, apart from MAX_OPEN_POSITIONS.

use {super::env_opt, std::sync::LazyLock};

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub shadow: bool,
    pub size_lamports: u64,
    pub max_bundle_lamports: u64,
    pub max_post_bundle_mult: f64,
    pub max_hold_ms: u64,
    pub trail: f64,
    pub day_loss_lamports: u64,
    pub max_open: usize,
}

impl Settings {
    /// The spec's values, both switches off.
    pub const DEFAULT: Settings = Settings {
        enabled: false,
        shadow: false,
        size_lamports: 100_000_000,
        max_bundle_lamports: 100_000_000_000,
        max_post_bundle_mult: 6.0,
        max_hold_ms: 60_000,
        trail: 0.15,
        day_loss_lamports: 500_000_000,
        max_open: 5,
    };

    /// What runs when the settings did not read (the start is refused then, `check_settings`).
    pub const OFF: Settings = Settings { enabled: false, shadow: false, ..Settings::DEFAULT };

    /// From the environment; an unreadable value is an error (the bot does not start: a size or a
    /// limit is never guessed).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Self::DEFAULT;
        let given = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let flag = |k: &str| match given(k).map(|v| v.trim().to_ascii_lowercase()) {
            None => Ok(false),
            Some(v) if matches!(v.as_str(), "1" | "true" | "on" | "yes") => Ok(true),
            Some(v) if matches!(v.as_str(), "0" | "false" | "off" | "no") => Ok(false),
            Some(v) => Err(format!("{k}={v:?} must be true or false")),
        };
        let sol = |k: &str, def: u64| given(k).map_or(Ok(def), |v| sol_lamports(&v).ok_or_else(|| format!("{k}={v:?} is not a SOL amount above 0")));
        let whole = |k: &str, def: u64| given(k).map_or(Ok(def), |v| v.trim().parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| format!("{k}={v:?} is not a whole number above 0")));
        let number = |k: &str, def: f64, ok: fn(f64) -> bool, what: &str| {
            given(k).map_or(Ok(def), |v| v.trim().parse::<f64>().ok().filter(|x| x.is_finite() && ok(*x)).ok_or_else(|| format!("{k}={v:?} {what}")))
        };
        let s = Self {
            enabled: flag("BUNDLE10_ENABLED")?,
            shadow: flag("BUNDLE10_SHADOW")?,
            size_lamports: sol("BUNDLE10_SIZE_SOL", d.size_lamports)?,
            max_bundle_lamports: sol("BUNDLE10_MAX_BUNDLE_SOL", d.max_bundle_lamports)?,
            max_post_bundle_mult: number("BUNDLE10_MAX_POST_BUNDLE_MULT", d.max_post_bundle_mult, |x| x > 1.0, "must be a number above 1")?,
            max_hold_ms: whole("BUNDLE10_MAX_HOLD_S", d.max_hold_ms / 1000)?.saturating_mul(1000),
            trail: number("BUNDLE10_TRAIL", d.trail, |x| x > 0.0 && x < 1.0, "must be a number between 0 and 1")?,
            day_loss_lamports: sol("BUNDLE10_DAY_LOSS_SOL", d.day_loss_lamports)?,
            max_open: whole("BUNDLE10_MAX_OPEN", d.max_open as u64)? as usize,
        };
        if s.size_lamports as f64 / 1e9 > super::MAX_RUNTIME_BUY_SOL {
            return Err(format!("BUNDLE10_SIZE_SOL is above the {} SOL safety limit", super::MAX_RUNTIME_BUY_SOL));
        }
        Ok(s)
    }
}

/// A SOL amount as typed, in lamports: above 0, at most 9 decimals.
fn sol_lamports(text: &str) -> Option<u64> {
    let t = text.trim();
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if whole.is_empty() && frac.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) || frac.len() > 9 || whole.len() > 6 {
        return None;
    }
    let l = whole.parse::<u64>().unwrap_or(0) * 1_000_000_000 + format!("{frac:0<9}").parse::<u64>().ok()?;
    (l > 0).then_some(l)
}

static SETTINGS: LazyLock<Result<Settings, String>> = LazyLock::new(|| Settings::from_lookup(env_opt));

/// The settings read once; both switches off when they did not read.
pub fn settings() -> &'static Settings {
    SETTINGS.as_ref().unwrap_or(&Settings::OFF)
}

/// For the start: an unreadable value stops it.
pub fn check_settings() -> Result<(), String> {
    SETTINGS.as_ref().map(|_| ()).map_err(Clone::clone)
}

/// BUNDLE10_ENABLED: the live path decides and buys.
#[inline]
pub fn enabled() -> bool {
    settings().enabled
}

/// BUNDLE10_SHADOW: candidates are logged, nothing is sent.
#[inline]
pub fn shadow() -> bool {
    settings().shadow
}

/// Either switch: the engine counts bundle10's view of each create's bundle.
#[inline]
pub fn on() -> bool {
    let s = settings();
    s.enabled || s.shadow
}

#[cfg(test)]
mod tests {
    use {super::*, std::collections::HashMap};

    fn read(pairs: &[(&str, &str)]) -> Result<Settings, String> {
        let env: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Settings::from_lookup(|k| env.get(k).cloned())
    }

    #[test]
    fn an_empty_environment_leaves_bundle10_off_with_the_specs_numbers() {
        let s = read(&[]).unwrap();
        assert_eq!(s, Settings::DEFAULT);
        assert!(!s.enabled && !s.shadow);
        assert_eq!(
            (s.size_lamports, s.max_bundle_lamports, s.max_post_bundle_mult, s.max_hold_ms, s.trail, s.day_loss_lamports, s.max_open),
            (100_000_000, 100_000_000_000, 6.0, 60_000, 0.15, 500_000_000, 5)
        );
    }

    #[test]
    fn values_are_read_and_bad_ones_refuse_the_start() {
        let s = read(&[("BUNDLE10_SHADOW", "true"), ("BUNDLE10_MAX_HOLD_S", "90"), ("BUNDLE10_SIZE_SOL", "0.25"), ("BUNDLE10_MAX_OPEN", "3")]).unwrap();
        assert!(s.shadow && !s.enabled);
        assert_eq!((s.max_hold_ms, s.size_lamports, s.max_open), (90_000, 250_000_000, 3));
        for bad in [
            ("BUNDLE10_ENABLED", "maybe"),
            ("BUNDLE10_TRAIL", "1.5"),
            ("BUNDLE10_TRAIL", "0"),
            ("BUNDLE10_SIZE_SOL", "abc"),
            ("BUNDLE10_SIZE_SOL", "101"),
            ("BUNDLE10_MAX_BUNDLE_SOL", "0"),
            ("BUNDLE10_MAX_POST_BUNDLE_MULT", "1"),
            ("BUNDLE10_MAX_HOLD_S", "-5"),
            ("BUNDLE10_MAX_OPEN", "0"),
            ("BUNDLE10_DAY_LOSS_SOL", "x"),
        ] {
            assert!(read(&[bad]).is_err(), "{bad:?} should be refused");
        }
        assert!(read(&[("BUNDLE10_ENABLED", "off")]).is_ok_and(|s| !s.enabled));
    }
}
