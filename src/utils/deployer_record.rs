//! Deployers' launch records for `rules::NO_POP_RECORD_*`: whether each recent launch of a followed
//! deployer got buyers in its first seconds, judged the way the replay does it (a 1 SOL buy right
//! behind the create's bundle, played through the exit rules until their first decision).
//! Kept in `logs/deployer_launches.csv` (unix time, deployer, mint, no_pop) across restarts.

use {
    super::auto_sell::{Action, RefExit, SellReason, Tick, Tuning},
    crate::{config::rules, processors::position_feed::Flow},
    log::warn,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, VecDeque},
        io::Write,
        str::FromStr,
        sync::{LazyLock, Mutex},
        time::Duration,
    },
};

pub const RECORD_PATH: &str = "logs/deployer_launches.csv";

/// Seconds a slot took on 26 Sep 2026, when the record was measured: the value until the slot
/// feed has measured the real one (slots were 0.219 s on 13 Oct).
pub const SLOT_SECS: f64 = 0.267;

/// The replay's own slot time for the launch it judges (rules_check: the fitted slot clock);
/// f64 bits, 0 = none.
static SLOT_SECS_OVERRIDE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The replay sets each launch's slot time before judging it; None goes back to the live value.
pub fn set_slot_secs(v: Option<f64>) {
    SLOT_SECS_OVERRIDE.store(v.map_or(0, f64::to_bits), std::sync::atomic::Ordering::Relaxed);
}

/// Seconds a slot takes now: the replay's value, else the slot feed's measurement, else SLOT_SECS.
pub fn slot_secs() -> f64 {
    let o = f64::from_bits(SLOT_SECS_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed));
    if o > 0.0 {
        return o;
    }
    super::slot_feed::secs_per_slot().unwrap_or(SLOT_SECS)
}

/// How long after the create a launch is watched: past the 2 s check with a second to spare for
/// a quiet coin that is up at the check (11 slots of 0.267 s when measured).
pub const OUTCOME_SECS: f64 = 11.0 * SLOT_SECS;

/// `OUTCOME_SECS` in slots at the current slot time (11 at 0.267 s, 14 at 0.219 s).
pub fn outcome_slots() -> u64 {
    (OUTCOME_SECS / slot_secs()).ceil() as u64
}

/// A trade after the create, with the curve after it.
#[derive(Debug, Clone, Copy)]
pub struct LaunchTrade {
    /// Slots after the create's slot.
    pub slot_off: u64,
    pub index: u64,
    pub is_buy: bool,
    pub sol: u64,
    pub vsol: u64,
    pub vtok: u64,
}

/// SOL out (after the 1.25% fee) for selling `amount` of the `held` tokens a position keeps on top
/// of a curve the stream shows at (vsol, vtok) without it.
fn sell_value(amount: u64, held: u64, vsol: u64, vtok: u64) -> f64 {
    let k = vsol as f64 * vtok as f64;
    let vt = vtok as f64 - held as f64;
    if vt <= 0.0 {
        return 0.0;
    }
    (k / vt - k / (vt + amount as f64)) * 0.9875
}

/// The exit this record judges with: the one the no-buyers rule was measured on (27 Sep: check
/// at 2 s, NO_POP under +5%, all out at 6 SOL), so changes to the live exit do not move the rule.
fn record_tuning() -> Tuning {
    Tuning {
        check_secs: 2.0,
        no_pop_percent: Some(5.0),
        first_all_up_sol: Some(6.0),
        no_pop_hold_high_percent: None,
        no_pop_hold_behind_sol: None,
        no_pop_stop_percent: None,
        first_up_percent: None,
        patient: None,
        ..Tuning::default()
    }
}

/// Whether a 1 SOL buy right behind the create's bundle (the buys at the block positions right
/// after the create's, in its slot) would have been sold NO_POP: the exit rules' first decision
/// within `outcome_slots()`. `create_after` is the curve after the create; `trades` are the other
/// trades, in any order. Same test as `rules_check dump` (fill "bundle"), first reason NO_POP.
pub fn no_pop_behind_bundle(create_index: u64, create_after: (u64, u64), trades: &[LaunchTrade]) -> bool {
    let slot = slot_secs();
    let mut trades: Vec<LaunchTrade> = trades.to_vec();
    trades.sort_by_key(|t| (t.slot_off, t.index));
    // The bundle: consecutive positions right after the create, in its slot (a transaction with
    // several trades repeats its position).
    let mut at = 0;
    let mut prev = create_index;
    while at < trades.len() && trades[at].slot_off == 0 && trades[at].index <= prev + 1 {
        prev = trades[at].index;
        at += 1;
    }
    let (vsol0, vtok0) = if at == 0 { create_after } else { (trades[at - 1].vsol, trades[at - 1].vtok) };
    let after = &trades[at..];
    let bet = rules::lamports(1.0);
    let k = vsol0 as f64 * vtok0 as f64;
    let tokens0 = (vtok0 as f64 - k / (vsol0 as f64 + bet as f64 / 1.0125)) as u64;
    if tokens0 == 0 {
        return false;
    }
    let reference = (tokens0 / 20).max(1);
    let entry_price = (bet as u128 * reference as u128 / tokens0 as u128) as u64;
    // Trade times as the replay has them: the slot's start plus 1 ms per trade in it.
    let mut timed = Vec::with_capacity(after.len());
    let (mut order, mut last) = (0.0, u64::MAX);
    for t in after {
        if t.slot_off != last {
            order = 0.0;
            last = t.slot_off;
        }
        order += 0.001;
        timed.push((t.slot_off as f64 * slot + order, t));
    }
    let horizon = outcome_slots() as f64 * slot;
    let mut times: Vec<f64> = timed.iter().map(|(t, _)| *t).filter(|t| *t <= horizon).collect();
    let mut tick = 0.0;
    while tick <= horizon {
        times.push(tick);
        tick += 0.1;
    }
    times.sort_by(|a, b| a.total_cmp(b));
    let mut exit = RefExit::with_tuning(bet, record_tuning());
    let wait = exit.first_wait_slots();
    for now in times {
        let seen: Vec<&(f64, &LaunchTrade)> = timed.iter().filter(|(t, _)| *t <= now).collect();
        let (vsol, vtok) = seen.last().map(|(_, e)| (e.vsol, e.vtok)).unwrap_or((vsol0, vtok0));
        let mut flow = Flow::default();
        for (_, e) in seen.iter().filter(|(t, _)| *t > now - 1.0) {
            if e.is_buy {
                flow.buys += 1;
                flow.buy_lamports += e.sol;
            } else {
                flow.sells += 1;
                flow.sell_lamports += e.sol;
            }
        }
        let since_last_buy = seen.iter().filter(|(_, e)| e.is_buy).map(|(t, _)| *t).fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))));
        let action = exit.evaluate(&Tick {
            held_for: Duration::from_secs_f64(now),
            entry_price,
            price: sell_value(reference, tokens0, vsol, vtok) as u64,
            behind_lamports: seen.iter().filter(|(_, e)| e.is_buy).map(|(_, e)| e.sol).sum(),
            behind_sell_lamports: 0,
            last_second: flow,
            recent_buy_lamports: seen.iter().filter(|(t, e)| e.is_buy && *t > now - 2.0).map(|(_, e)| e.sol).sum(),
            recent_sell_lamports_3s: 0,
            since_last_buy: since_last_buy.map(|t| Duration::from_secs_f64((now - t).max(0.0))),
            left: 1.0,
            window_net_lamports: seen.iter().filter(|(_, e)| e.slot_off <= wait).map(|(_, e)| if e.is_buy { e.sol as i64 } else { -(e.sol as i64) }).sum(),
            in_wait_window: ((now / slot).floor() as u64) <= wait,
            insider_sold: false,
        });
        match action {
            Action::Hold => continue,
            Action::SellAll(reason) | Action::SellShare(_, reason) => return reason == SellReason::NoPop,
        }
    }
    false
}

/// Launches per deployer, oldest first: (unix time, mint, no_pop).
static RECORDS: LazyLock<Mutex<HashMap<Pubkey, VecDeque<(u64, Pubkey, bool)>>>> = LazyLock::new(|| Mutex::new(load()));

/// A well-formed row: exactly the four fields (rows written in pieces by two launches at once
/// used to interleave into merged ones).
fn parse_row(line: &str) -> Option<(u64, Pubkey, Pubkey, bool)> {
    let fields: Vec<&str> = line.split(',').collect();
    let [unix, deployer, mint, no_pop] = fields[..] else { return None };
    let no_pop = match no_pop {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    Some((unix.parse().ok()?, Pubkey::from_str(deployer).ok()?, Pubkey::from_str(mint).ok()?, no_pop))
}

fn load() -> HashMap<Pubkey, VecDeque<(u64, Pubkey, bool)>> {
    let mut out: HashMap<Pubkey, VecDeque<(u64, Pubkey, bool)>> = HashMap::new();
    let started = std::time::Instant::now();
    // The store (STORE_FILES=off): the record's window (NO_POP_RECORD_DAYS) by an indexed query.
    let mut rows: Vec<(u64, Pubkey, Pubkey, bool)> = if crate::store::read_from_db() {
        let since = crate::utils::dashboard::now_unix().saturating_sub(rules::NO_POP_RECORD_DAYS * 86_400);
        crate::store::read::deployer_launches(since)
            .into_iter()
            .filter_map(|(t, d, m, np)| Some((t, Pubkey::from_str(&d).ok()?, Pubkey::from_str(&m).ok()?, np)))
            .collect()
    } else {
        let Ok(text) = std::fs::read_to_string(RECORD_PATH) else {
            return out;
        };
        text.lines().skip(1).filter_map(parse_row).collect()
    };
    log::info!(
        "deployer launch records: {} rows from {} in {:.1} ms",
        rows.len(),
        if crate::store::read_from_db() { "the store" } else { RECORD_PATH },
        started.elapsed().as_secs_f64() * 1e3
    );
    rows.sort_by_key(|r| r.0);
    for (unix, deployer, mint, no_pop) in rows {
        let launches = out.entry(deployer).or_default();
        if !launches.iter().any(|(_, m, _)| *m == mint) {
            launches.push_back((unix, mint, no_pop));
        }
        while launches.len() > rules::NO_POP_RECORD_LAST {
            launches.pop_front();
        }
    }
    out
}

/// Loads the records (call at startup so the first launch does not wait on the file).
pub fn init() -> usize {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// A launch's outcome: kept in memory and appended to the file.
pub fn note(deployer: &Pubkey, mint: &Pubkey, unix: u64, no_pop: bool) {
    {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        let launches = records.entry(*deployer).or_default();
        if launches.iter().any(|(_, m, _)| m == mint) {
            return;
        }
        launches.push_back((unix, *mint, no_pop));
        while launches.len() > rules::NO_POP_RECORD_LAST {
            launches.pop_front();
        }
    }
    if crate::store::db_on() {
        crate::store::write(crate::store::Row::Exec {
            sql: "INSERT INTO deployer_launches (unix_time, deployer, mint, no_pop) VALUES (?1, ?2, ?3, ?4)",
            params: vec![unix.into(), deployer.to_string().into(), mint.to_string().into(), no_pop.into()],
        });
    }
    if !crate::store::files_on() {
        return;
    }
    // One write per row, so outcomes noted at the same moment do not interleave.
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(RECORD_PATH)?;
        let header = if file.metadata()?.len() == 0 { "unix_time,deployer,mint,no_pop\n" } else { "" };
        file.write_all(format!("{header}{unix},{deployer},{mint},{}\n", no_pop as u8).as_bytes())
    })();
    if let Err(e) = result {
        warn!("could not write {RECORD_PATH}: {e}");
    }
}

/// The deployer's recent launches (the last `NO_POP_RECORD_LAST` within `NO_POP_RECORD_DAYS`
/// before `now`): (launches, of which NO_POP).
pub fn record_of(deployer: &Pubkey, now: u64) -> (u32, u32) {
    let records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(launches) = records.get(deployer) else {
        return (0, 0);
    };
    let since = now.saturating_sub(rules::NO_POP_RECORD_DAYS * 86_400);
    let recent = launches.iter().filter(|(t, _, _)| *t >= since && *t <= now);
    let (mut n, mut no_pops) = (0, 0);
    for (_, _, no_pop) in recent {
        n += 1;
        no_pops += *no_pop as u32;
    }
    (n, no_pops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_rows_are_left_out() {
        let good = "1790575328,6xmnqr5Fgj9Schwv18iJev1Ao31YZzs944bxh5nGfnA8,Fb9jf7DNorHDSNg8hRcgT4cLJTNEYTDrtqT7QyRACzmM,1";
        assert!(parse_row(good).is_some_and(|r| r.3));
        for bad in [
            "17905753281790575328,,6xmnqr5Fgj9Schwv18iJev1Ao31YZzs944bxh5nGfnA8,,1",
            "1790575328,6xmnqr5Fgj9Schwv18iJev1Ao31YZzs944bxh5nGfnA8,Fb9jf7DNorHDSNg8hRcgT4cLJTNEYTDrtqT7QyRACzmM,1,",
            "1790575328,6xmnqr5Fgj9Schwv18iJev1Ao31YZzs944bxh5nGfnA8,Fb9jf7DNorHDSNg8hRcgT4cLJTNEYTDrtqT7QyRACzmM,17905",
            "1,",
        ] {
            assert_eq!(parse_row(bad), None, "{bad}");
        }
    }

    /// A curve of 30 SOL / 1.073e15 tokens (pump.fun's start) after `sol` more came in.
    fn curve(sol: f64) -> (u64, u64) {
        let (vs, vt) = (30e9, 1_073_000_000e6);
        let vs2 = vs + sol * 1e9;
        (vs2 as u64, (vs * vt / vs2) as u64)
    }

    fn trade(slot_off: u64, index: u64, is_buy: bool, sol: f64, curve_sol: f64) -> LaunchTrade {
        let (vsol, vtok) = curve(curve_sol);
        LaunchTrade { slot_off, index, is_buy, sol: (sol * 1e9) as u64, vsol, vtok }
    }

    #[test]
    fn a_launch_nobody_buys_behind_the_bundle_is_a_no_pop() {
        // Dev 3 SOL, a 2-wallet bundle of 3 SOL, then a few crumbs.
        let bundle = [trade(0, 11, true, 1.5, 4.5), trade(0, 12, true, 1.5, 6.0)];
        let mut trades = bundle.to_vec();
        trades.push(trade(3, 40, true, 0.05, 6.05));
        assert!(no_pop_behind_bundle(10, curve(3.0), &trades));
    }

    #[test]
    fn buyers_behind_the_fill_are_a_pop() {
        let mut trades = vec![trade(0, 11, true, 1.5, 4.5), trade(0, 12, true, 1.5, 6.0)];
        // 3 SOL of buys in the next slots: the first-sell trigger fires before the check.
        trades.push(trade(2, 100, true, 1.0, 7.0));
        trades.push(trade(5, 200, true, 2.0, 9.0));
        assert!(!no_pop_behind_bundle(10, curve(3.0), &trades));
    }

    #[test]
    fn the_bundle_ends_at_the_first_gap_in_block_positions() {
        // Position 13 is missing: the buy at 14 is a sniper behind the fill, not the bundle. It
        // is 1.2 SOL behind the fill, enough for the first sell.
        let trades = vec![trade(0, 11, true, 1.5, 4.5), trade(0, 12, true, 1.5, 6.0), trade(0, 14, true, 1.2, 7.2)];
        assert!(!no_pop_behind_bundle(10, curve(3.0), &trades));
        // As part of the bundle (position 13) it is ahead of the fill: nothing behind, no pop.
        let trades = vec![trade(0, 11, true, 1.5, 4.5), trade(0, 12, true, 1.5, 6.0), trade(0, 13, true, 1.2, 7.2)];
        assert!(no_pop_behind_bundle(10, curve(3.0), &trades));
    }
}
