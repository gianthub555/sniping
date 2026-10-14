//! Every followed launch's replayed result, for the scorecard's "deployer's earlier launches" facts
//! (2026-10-12): a 0.5 SOL buy right behind the create's bundle, the bot's exit (`RefExit`, default
//! tuning) played over the launch's trades, and whether that position fell under 0.9x of its cost
//! before its best within 30 s. Same as the scorecard's training data, which took them from
//! `rules_check dump` (fill "bundle", DUMP_STAKE=0.5) and research/filter-pXGQ-2026-10-12/slot0.py.
//! Kept in SQLite (`launch_result`) and in memory, keyed by the coin's creator.

use {
    super::{
        auto_sell::{Action, RefExit, Tick},
        db,
        deployer_record::{self, LaunchTrade, SLOT_SECS},
    },
    crate::{config::rules::lamports, processors::position_feed::Flow},
    log::warn,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        str::FromStr,
        sync::{LazyLock, Mutex},
        time::Duration,
    },
};

/// The stake the facts are measured with.
pub const STAKE_SOL: f64 = 0.5;
/// How long a launch is watched for its result: the exit's 40 s lifetime and a little more
/// (160 slots of 0.267 s when it was measured).
pub const RESULT_SECS: f64 = 160.0 * SLOT_SECS;

/// `RESULT_SECS` in slots at the current slot time (`deployer_record::slot_secs`).
pub fn result_slots() -> u64 {
    (RESULT_SECS / deployer_record::slot_secs()).ceil() as u64
}
/// The "down" window.
const DOWN_SECS: f64 = 30.0;
const DOWN_X: f64 = 0.9;

/// SOL out (after the 1.25% fee) for selling `amount` of the `held` tokens a position keeps on top
/// of a curve the stream shows at (vsol, vtok) without it (as `rules_check`).
fn sell_value(amount: u64, held: u64, vsol: u64, vtok: u64) -> f64 {
    let k = vsol as f64 * vtok as f64;
    let vt = vtok as f64 - held as f64;
    if vt <= 0.0 {
        return 0.0;
    }
    (k / vt - k / (vt + amount as f64)) * 0.9875
}

/// The result of a launch: SOL won or lost by the replayed buy, and whether it went down first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaunchResult {
    pub pnl_sol: f64,
    pub down: bool,
}

/// Replays a buy right behind the create's bundle (`create_index`, the curve after the create
/// `create_after`, the launch's other trades in any order) with the bot's exit. `rules_check`'s
/// `play` for fill "bundle", without a sell lag.
pub fn replay(create_index: u64, create_after: (u64, u64), trades: &[LaunchTrade]) -> Option<LaunchResult> {
    let slot = deployer_record::slot_secs();
    let mut trades: Vec<LaunchTrade> = trades.to_vec();
    trades.sort_by_key(|t| (t.slot_off, t.index));
    let mut at = 0;
    let mut prev = create_index;
    while at < trades.len() && trades[at].slot_off == 0 && trades[at].index <= prev + 1 {
        prev = trades[at].index;
        at += 1;
    }
    let (vsol0, vtok0) = if at == 0 { create_after } else { (trades[at - 1].vsol, trades[at - 1].vtok) };
    let after = &trades[at..];
    let bet = lamports(STAKE_SOL);
    let k = vsol0 as f64 * vtok0 as f64;
    let tokens0 = (vtok0 as f64 - k / (vsol0 as f64 + bet as f64 / 1.0125)) as u64;
    if tokens0 == 0 {
        return None;
    }
    // Trade times as the replay has them: the slot's start plus 1 ms per trade in it.
    let mut timed: Vec<(f64, &LaunchTrade)> = Vec::with_capacity(after.len());
    let (mut order, mut last) = (0.0, u64::MAX);
    for t in after {
        if t.slot_off != last {
            order = 0.0;
            last = t.slot_off;
        }
        order += 0.001;
        timed.push((t.slot_off as f64 * slot + order, t));
    }
    // Down: the whole position's value at each trade within 30 s, under 0.9x before its best.
    let mut best = sell_value(tokens0, tokens0, vsol0, vtok0) / bet as f64;
    let mut worst_before_best = best;
    let mut worst_so_far = best;
    for (_, e) in timed.iter().filter(|(t, _)| *t <= DOWN_SECS) {
        let v = sell_value(tokens0, tokens0, e.vsol, e.vtok) / bet as f64;
        worst_so_far = worst_so_far.min(v);
        if v > best {
            best = v;
            worst_before_best = worst_so_far;
        }
    }
    let down = worst_before_best < DOWN_X;
    // The exit, every trade and every 0.1 s.
    let reference = (tokens0 / 20).max(1);
    let entry_price = (bet as u128 * reference as u128 / tokens0 as u128) as u64;
    let mut exit = RefExit::new(bet);
    let mut times: Vec<f64> = timed.iter().map(|(t, _)| *t).collect();
    let mut tick = 0.0;
    while tick <= 180.0 {
        times.push(tick);
        tick += 0.1;
    }
    times.sort_by(|a, b| a.total_cmp(b));
    let mut held = tokens0;
    let mut proceeds = 0.0;
    let wait = exit.first_wait_slots();
    for now in times {
        if held == 0 {
            break;
        }
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
        let price = sell_value(reference, held, vsol, vtok) as u64;
        let action = exit.evaluate(&Tick {
            held_for: Duration::from_secs_f64(now),
            entry_price,
            price,
            behind_lamports: seen.iter().filter(|(_, e)| e.is_buy).map(|(_, e)| e.sol).sum(),
            behind_sell_lamports: seen.iter().filter(|(_, e)| !e.is_buy).map(|(_, e)| e.sol).sum(),
            last_second: flow,
            recent_buy_lamports: seen.iter().filter(|(t, e)| e.is_buy && *t > now - 2.0).map(|(_, e)| e.sol).sum(),
            recent_sell_lamports_3s: seen.iter().filter(|(t, e)| !e.is_buy && *t > now - 3.0).map(|(_, e)| e.sol).sum(),
            since_last_buy: since_last_buy.map(|t| Duration::from_secs_f64((now - t).max(0.0))),
            left: held as f64 / tokens0 as f64,
            window_net_lamports: seen.iter().filter(|(_, e)| e.slot_off <= wait).map(|(_, e)| if e.is_buy { e.sol as i64 } else { -(e.sol as i64) }).sum(),
            in_wait_window: ((now / slot).floor() as u64) <= wait,
            insider_sold: false,
        });
        let amount = match action {
            Action::Hold => continue,
            Action::SellAll(_) => held,
            Action::SellShare(share, _) => {
                let a = (held as f64 * share).round() as u64;
                if a == 0 || held - a < reference { held } else { a }
            }
        };
        proceeds += sell_value(amount, held, vsol, vtok);
        held -= amount;
        exit.sold(price, Duration::from_secs_f64(now + slot));
    }
    Some(LaunchResult { pnl_sol: (proceeds - bet as f64) / 1e9, down })
}

/// A creator's earlier launches: (unix time, result), oldest first.
static BY_CREATOR: LazyLock<Mutex<HashMap<Pubkey, Vec<(u64, LaunchResult)>>>> = LazyLock::new(Default::default);

/// The scorecard's facts from a creator's launches before `now`: (count, mean SOL result, share
/// that went down). None when there are none.
/// Allocation-free: the hot path's `decide::prepare` reads it.
pub fn earlier(creator: &Pubkey, now: u64) -> Option<(u32, f64, f64)> {
    let map = BY_CREATOR.lock().unwrap_or_else(|e| e.into_inner());
    let rows = map.get(creator)?;
    let (mut n, mut pnl, mut down) = (0u32, 0.0f64, 0u32);
    for (t, r) in rows {
        if *t < now {
            n += 1;
            pnl += r.pnl_sol;
            down += r.down as u32;
        }
    }
    (n > 0).then(|| (n, pnl / n as f64, down as f64 / n as f64))
}

/// Records a launch's result (memory and database).
pub fn note(mint: &Pubkey, deployer: &Pubkey, creator: &Pubkey, created: u64, result: LaunchResult, no_pop: bool) {
    BY_CREATOR.lock().unwrap_or_else(|e| e.into_inner()).entry(*creator).or_default().push((created, result));
    db::write(db::Write::LaunchResult {
        mint: mint.to_string(),
        deployer: deployer.to_string(),
        creator: creator.to_string(),
        created: created as i64,
        pnl_sol: result.pnl_sol,
        down: result.down,
        no_pop,
    });
}

/// Puts a result in memory only (tests).
#[doc(hidden)]
pub fn seed(creator: Pubkey, created: u64, result: LaunchResult) {
    BY_CREATOR.lock().unwrap_or_else(|e| e.into_inner()).entry(creator).or_default().push((created, result));
}

/// Loads the recorded results from the database. Returns how many.
pub fn load() -> usize {
    let rows = match db::read().and_then(|c| db::load_launch_results(&c)) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("launch results not loaded from {}: {e}", db::path());
            return 0;
        }
    };
    let n = rows.len();
    let mut map = BY_CREATOR.lock().unwrap_or_else(|e| e.into_inner());
    for (creator, created, pnl_sol, down) in rows {
        if let Ok(c) = Pubkey::from_str(&creator) {
            map.entry(c).or_default().push((created.max(0) as u64, LaunchResult { pnl_sol, down }));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const V0: u64 = 30_000_000_000;
    const T0: u64 = 1_073_000_000_000_000;

    /// A trade of `sol` on a curve at (vs, vt), the curve after it.
    fn trade(slot_off: u64, index: u64, is_buy: bool, sol: u64, vs: &mut u64, vt: &mut u64) -> LaunchTrade {
        let k = *vs as u128 * *vt as u128;
        if is_buy {
            *vs += sol;
        } else {
            *vs -= sol;
        }
        *vt = (k / *vs as u128) as u64;
        LaunchTrade { slot_off, index, is_buy, sol, vsol: *vs, vtok: *vt }
    }

    #[test]
    fn nobody_buying_loses_the_fees_and_is_not_down() {
        let r = replay(10, (V0 + 1_000_000_000, T0 - 34_000_000_000_000), &[]).unwrap();
        assert!(r.pnl_sol < 0.0 && r.pnl_sol > -0.02, "{r:?}");
        assert!(!r.down);
    }

    #[test]
    fn a_wave_of_buyers_wins_and_a_dump_first_is_down() {
        let (mut vs, mut vt) = (V0 + 1_000_000_000, T0 - 34_000_000_000_000);
        let create = (vs, vt);
        let mut up = Vec::new();
        for i in 0..20 {
            up.push(trade(1 + i / 4, i, true, 1_000_000_000, &mut vs, &mut vt));
        }
        let r = replay(10, create, &up).unwrap();
        assert!(r.pnl_sol > 0.05, "{r:?}");
        assert!(!r.down);
        // A dev that bought 5 SOL sells 3 of them right behind us: the curve falls ~16%.
        let (mut vs, mut vt) = (V0, T0);
        let created = trade(0, 9, true, 5_000_000_000, &mut vs, &mut vt);
        let create = (created.vsol, created.vtok);
        let mut dump = vec![trade(1, 0, false, 3_000_000_000, &mut vs, &mut vt)];
        for i in 0..20 {
            dump.push(trade(5 + i / 4, i, true, 1_000_000_000, &mut vs, &mut vt));
        }
        assert!(replay(10, create, &dump).unwrap().down);
    }

    #[test]
    fn earlier_counts_only_launches_before_now() {
        let c = Pubkey::new_unique();
        seed(c, 100, LaunchResult { pnl_sol: 0.2, down: false });
        seed(c, 200, LaunchResult { pnl_sol: -0.1, down: true });
        assert_eq!(earlier(&c, 50), None);
        assert_eq!(earlier(&c, 150), Some((1, 0.2, 0.0)));
        let (n, pnl, down) = earlier(&c, 300).unwrap();
        assert_eq!(n, 2);
        assert!((pnl - 0.05).abs() < 1e-12 && (down - 0.5).abs() < 1e-12);
    }
}
