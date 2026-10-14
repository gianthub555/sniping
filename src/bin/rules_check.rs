//! Checks the trading rules against recorded data from the reference-wallet study, using the
//! same code the bot runs (`config::rules`, `utils::reference`, `utils::auto_sell::RefExit`).
//!
//!   rules_check entry <facts.jsonl> <features2.json> <reference_deployers.txt> <baseline_weight>
//!       Entry rules on the launch sample: which launches pass, against the ones he attempted.
//!   rules_check exit <positions.json> <tapes_dir> [launch_dir]
//!       The exit on his 749 positions, bought at his fill, played over the real trade tape;
//!       its decisions against his.
//!   rules_check full <facts.jsonl> <features2.json> <reference_deployers.txt> <launch_dir>
//!       Entry, buy guard and exit together on every sampled launch the entry rules pass,
//!       bought where the bot's buy would land; split by whether he bought the launch.
//!   rules_check dump <launch_dir>
//!       One JSON line per launch file: the exit's result for a 1 SOL buy filled right behind the
//!       create's bundle, at the top of the next slot and at the top of the slot after, with the
//!       price move from the bundle to each fill (for testing entry filters and buy guards).
//!   rules_check pops <launch_dir>
//!       One CSV line per launch file (unix time, deployer, mint, no_pop): the bot's no-buyers
//!       test (`utils::deployer_record`), as `logs/deployer_launches.csv` holds it.
//!   rules_check results <launch_dir>
//!       One JSON line per launch file: the scorecard's launch result (`utils::launch_results`):
//!       a 0.5 SOL buy behind the bundle with the bot's exit, and whether it went down. Parity
//!       check against `dump` (fill "bundle", DUMP_STAKE=0.5).
//!   rules_check backfill
//!       The bot's startup catch-up of the reference wallet's trades, run on the history files in
//!       the working directory (use a scratch copy: it appends to reference_deployers.txt).
//!   rules_check play <launch_dir> <auto_sell_logs.csv> [since]
//!       The bot's own buys (live or dry run, from `since`, e.g. 2026-09-26T00:00) replayed from
//!       the curve state they filled at: the model's result next to the bot's (quoted, and the
//!       wallet's change with fees and tips). `./bot.sh compare` fetches the tapes and runs it.

use {
    anyhow::{anyhow, Context, Result},
    serde_json::Value,
    sniper_bot::{
        config::rules::{self, lamports, Follow, LaunchFacts},
        processors::position_feed::Flow,
        utils::{
            auto_sell::{Action, RefExit, SellReason, Tick, Tuning},
            reference::{follow_of, parse_line, Record},
        },
    },
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, HashSet},
        str::FromStr,
        time::Duration,
    },
};

const W: &str = "8RvtT8189KpAq5MkXGVHFn6LdZpxq9PkvDeAvs1g5Yj4";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("entry") if args.len() == 5 => entry(&args[1], &args[2], &args[3], args[4].parse()?),
        Some("exit") if args.len() == 3 => exit(&args[1], &args[2], None),
        Some("exit") if args.len() == 4 => exit(&args[1], &args[2], Some(&args[3])),
        Some("full") if args.len() == 5 => full(&args[1], &args[2], &args[3], &args[4]),
        Some("play") if args.len() == 3 => play_live(&args[1], &args[2], None),
        Some("play") if args.len() == 4 => play_live(&args[1], &args[2], Some(&args[3])),
        Some("dump") if args.len() == 2 => dump(&args[1]),
        Some("pops") if args.len() == 2 => pops(&args[1]),
        Some("results") if args.len() == 2 => results(&args[1]),
        // Runs the bot's startup catch-up of the reference wallet's trades on the history files in
        // the working directory (a scratch copy: it appends to them). RPC_ENDPOINT from the env.
        Some("backfill") if args.len() == 1 => {
            let known = sniper_bot::utils::reference::load();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
            let (trades, coins) = tokio::runtime::Runtime::new()?.block_on(sniper_bot::utils::reference::backfill(now))?;
            println!("{known} keys loaded; backfilled {trades} trades on {coins} coins");
            Ok(())
        }
        _ => Err(anyhow!("usage: rules_check entry <facts.jsonl> <features2.json> <reference_deployers.txt> <baseline_weight> | exit <positions.json> <tapes_dir> [launch_dir] | full <facts.jsonl> <features2.json> <reference_deployers.txt> <launch_dir>")),
    }
}

// ---- entry --------------------------------------------------------------------------------

/// Sampled launches (17-22 Sep, first buy 0.5+ SOL) in time order with the entry verdict the bot
/// would have given at the create, from his trades before it.
fn entry_verdicts(facts_path: &str, features_path: &str, history_path: &str) -> Result<Vec<(Value, Result<(), String>)>> {
    let mut history: Vec<(u64, Pubkey, i64)> = std::fs::read_to_string(history_path)?.lines().filter_map(parse_line).collect();
    history.sort_by_key(|h| h.0);
    let features: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(features_path)?)?;
    let age: HashMap<String, (f64, bool)> = features
        .iter()
        .map(|f| (f["m"].as_str().unwrap_or_default().to_string(), (f["dage"].as_f64().unwrap_or(0.0), f["age_known"].as_bool().unwrap_or(false))))
        .collect();
    let mut launches: Vec<Value> = std::fs::read_to_string(facts_path)?
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|f| f["err"].is_null() && f["create"].is_object() && f["stratum"] == "new")
        .collect();
    launches.sort_by(|a, b| a["ts"].as_f64().partial_cmp(&b["ts"].as_f64()).unwrap());

    // Walk launches in time order, adding his trades that happened before each launch.
    let mut records: HashMap<Pubkey, Record> = HashMap::new();
    let mut next = 0;
    let mut out = Vec::new();
    for f in launches {
        let ts = f["ts"].as_f64().unwrap_or(0.0) as u64;
        while next < history.len() && history[next].0 < ts {
            let (at, key, l) = history[next];
            records.entry(key).or_default().add(at, l);
            next += 1;
        }
        let key = |k: &str| Pubkey::from_str(f["create"][k].as_str().unwrap_or_default()).ok();
        let keys: Vec<Pubkey> = [key("user"), key("creator")].into_iter().flatten().collect();
        let follows: Vec<Follow> = keys.iter().filter_map(|k| records.get(k)).filter_map(|r| follow_of(r, ts)).collect();
        let follow = follows.iter().copied().find(|x| *x == Follow::Recent).or(follows.first().copied());
        let facts = LaunchFacts {
            mayhem: f["create"]["mayhem"].as_bool().unwrap_or(false),
            dev_buy_lamports: lamports(f["dev_buy"].as_f64().unwrap_or(0.0)),
            deployer_lamports: f["payer_pre"].as_f64().map(lamports),
            follow,
            reference_profit_lamports: None,
            reference_trades: None,
            insider_lamports: None,
            risky_host: None,
            // The launch sample has no deployer launch history.
            secs_since_previous_launch: None,
            launches_last_day: None,
            bundle_wallets: None,
            name_all_caps: None,
            deployer_record: (0, 0),
            scoring: Default::default(),
        };
        let mint = f["mint"].as_str().unwrap_or_default();
        let verdict = rules::launch_verdict(&facts).and_then(|_| match age.get(mint) {
            Some((hours, true)) if (*hours * 3600.0) < rules::DEPLOYER_MIN_AGE_SECS as f64 => Err("deployer wallet younger than 24 h".to_string()),
            _ => Ok(()),
        });
        out.push((f, verdict));
    }
    Ok(out)
}

fn entry(facts_path: &str, features_path: &str, history_path: &str, weight: f64) -> Result<()> {
    let verdicts = entry_verdicts(facts_path, features_path, history_path)?;
    let mut table: HashMap<(bool, &'static str), usize> = HashMap::new();
    let mut reasons: HashMap<String, usize> = HashMap::new();
    for (f, verdict) in &verdicts {
        let attempted = f["group"] == "attempted";
        let label = match &verdict {
            Ok(()) => "pass",
            Err(_) => "skip",
        };
        *table.entry((attempted, label)).or_default() += 1;
        if let Err(r) = verdict {
            let short = r.split(':').next().unwrap_or(&r).split(" outside").next().unwrap_or(&r).to_string();
            *reasons.entry(format!("{} {short}", if attempted { "attempted" } else { "baseline " })).or_default() += 1;
        }
    }
    let n = |a: bool, l: &'static str| *table.get(&(a, l)).unwrap_or(&0) as f64;
    let (ap, as_, bp, bs) = (n(true, "pass"), n(true, "skip"), n(false, "pass"), n(false, "skip"));
    println!("ENTRY RULES on {} launches with a first buy of 0.5+ SOL (17-22 Sep)", verdicts.len());
    println!("  his attempts:     {ap:.0} pass, {as_:.0} skipped  -> the rules keep {:.0}% of his attempts", 100.0 * ap / (ap + as_));
    println!("  random launches:  {bp:.0} pass, {bs:.0} skipped  -> {:.1}% of all launches pass", 100.0 * bp / (bp + bs));
    println!("  of the launches that pass, he attempted {:.1}% (weighted; he attempts {:.2}% of all launches)",
        100.0 * ap / (ap + bp * weight), 100.0 * (ap + as_) / (ap + as_ + (bp + bs) * weight));
    let mut r: Vec<_> = reasons.into_iter().collect();
    r.sort_by(|a, b| b.1.cmp(&a.1));
    println!("  skip reasons:");
    for (k, v) in r {
        println!("    {v:5}  {k}");
    }
    Ok(())
}

// ---- exit ---------------------------------------------------------------------------------

struct Ev {
    t: f64,
    /// Slots after his fill's slot.
    slot_off: u64,
    is_buy: bool,
    mine: bool,
    sol: u64,
    vsol: u64,
    vtok: u64,
}

struct Decision {
    t: f64,
    all: bool,
    reason: SellReason,
}

fn slot_secs(date: &str) -> f64 {
    if date <= "09-17" {
        0.317
    } else if date == "09-18" {
        0.276
    } else {
        0.267
    }
}

/// Slot time from the fitted slot clock (`SLOT_CLOCK`, default `data/slot_clock.tsv`: `slot<TAB>unix`
/// rows, the median time of every 1,000-slot bucket of pump.fun creates, research/ml-entry-exit-2026-10-13
/// `load.py export-clock`). Slots got shorter over the month (317 ms on 7-14 Sep, 268 ms late Sep,
/// 219 ms on 13 Oct), so one number per date no longer fits.
struct SlotClock {
    slots: Vec<f64>,
    times: Vec<f64>,
}

impl SlotClock {
    fn parse(text: &str) -> Option<Self> {
        let mut rows: Vec<(f64, f64)> = text
            .lines()
            .filter_map(|l| {
                let mut p = l.split('\t');
                Some((p.next()?.trim().parse().ok()?, p.next()?.trim().parse().ok()?))
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        (rows.len() >= 2).then(|| Self { slots: rows.iter().map(|r| r.0).collect(), times: rows.iter().map(|r| r.1).collect() })
    }

    /// Seconds a slot takes around `slot`: the slope over the buckets within 10 of it. None outside
    /// the table (more than 10 buckets past either end).
    fn secs_per_slot(&self, slot: u64) -> Option<f64> {
        let s = slot as f64;
        if s < self.slots[0] - 10_000.0 || s > self.slots[self.slots.len() - 1] + 10_000.0 {
            return None;
        }
        let i = self.slots.partition_point(|&x| x < s);
        let lo = i.saturating_sub(10);
        let hi = (i + 10).min(self.slots.len() - 1);
        let (ds, dt) = (self.slots[hi] - self.slots[lo], self.times[hi] - self.times[lo]);
        (ds > 0.0 && dt > 0.0).then(|| dt / ds).filter(|v| (0.1..1.0).contains(v))
    }
}

static SLOT_CLOCK: std::sync::LazyLock<Option<SlotClock>> = std::sync::LazyLock::new(|| {
    let path = std::env::var("SLOT_CLOCK").unwrap_or_else(|_| "data/slot_clock.tsv".into());
    std::fs::read_to_string(&path).ok().and_then(|t| SlotClock::parse(&t))
});

/// The tape's own block times against its slots, when they span a minute or more.
fn tape_secs_per_slot(tape: &[Value]) -> Option<f64> {
    let pts: Vec<(f64, f64)> =
        tape.iter().filter_map(|e| Some((e["slot"].as_u64()? as f64, e["bt"].as_u64()? as f64))).filter(|p| p.1 > 0.0).collect();
    let (first, last) = (pts.first()?, pts.last()?);
    (last.1 - first.1 >= 60.0 && last.0 > first.0).then(|| (last.1 - first.1) / (last.0 - first.0)).filter(|v| (0.1..1.0).contains(v))
}

/// A launch's slot time: the fitted clock at its create slot, else its tape's own block times,
/// else the old per-date value.
fn launch_slot_secs(s0: u64, bt: u64, tape: &[Value]) -> f64 {
    clock_or_tape(SLOT_CLOCK.as_ref(), s0, tape).unwrap_or_else(|| slot_secs(&chrono_date(bt)))
}

fn clock_or_tape(clock: Option<&SlotClock>, s0: u64, tape: &[Value]) -> Option<f64> {
    clock.and_then(|c| c.secs_per_slot(s0)).or_else(|| tape_secs_per_slot(tape))
}

/// Executable value of `tokens` on the curve (fees 1.25%).
fn quote(tokens: u64, vsol: u64, vtok: u64) -> u64 {
    ((tokens as u128 * vsol as u128 / (vtok as u128 + tokens as u128)) as f64 * 0.9875) as u64
}

fn simulate(evs: &[Ev], bet: u64, tokens0: u64, slot: f64) -> (Vec<Decision>, f64) {
    let mut exit = RefExit::with_tuning(bet, tuning());
    let reference = (tokens0 / 20).max(1);
    let entry_price = (bet as u128 * reference as u128 / tokens0 as u128) as u64;
    let mut held = tokens0;
    let mut proceeds = 0.0;
    let mut out = Vec::new();
    let mut times: Vec<f64> = evs.iter().map(|e| e.t).collect();
    let mut t = 0.0;
    while t <= 60.0 {
        times.push(t);
        t += 0.1;
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    for now in times {
        if held == 0 {
            break;
        }
        let seen: Vec<&Ev> = evs.iter().filter(|e| e.t <= now).collect();
        let Some(last) = seen.last() else { continue };
        let others = seen.iter().filter(|e| !e.mine);
        let behind: u64 = others.clone().filter(|e| e.is_buy).map(|e| e.sol).sum();
        let mut flow = Flow::default();
        for e in others.filter(|e| e.t > now - 1.0) {
            if e.is_buy {
                flow.buys += 1;
                flow.buy_lamports += e.sol;
            } else {
                flow.sells += 1;
                flow.sell_lamports += e.sol;
            }
        }
        let price = quote(reference, last.vsol, last.vtok);
        let recent_buy_lamports: u64 = seen.iter().filter(|e| !e.mine && e.is_buy && e.t > now - 2.0).map(|e| e.sol).sum();
        let since_last_buy = seen.iter().filter(|e| !e.mine && e.is_buy).map(|e| e.t).fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))))
            .map(|t| Duration::from_secs_f64((now - t).max(0.0)));
        let wait = exit.first_wait_slots();
        let window_net = seen.iter().filter(|e| !e.mine && e.slot_off <= wait).map(|e| if e.is_buy { e.sol as i64 } else { -(e.sol as i64) }).sum();
        let action = exit.evaluate(&Tick {
            held_for: Duration::from_secs_f64(now),
            entry_price,
            price,
            behind_lamports: behind,
            behind_sell_lamports: 0,
            last_second: flow,
            recent_buy_lamports,
            recent_sell_lamports_3s: 0,
            since_last_buy,
            left: held as f64 / tokens0 as f64,
            window_net_lamports: window_net,
            in_wait_window: ((now / slot).floor() as u64) <= wait,
            insider_sold: false,
        });
        let (amount, all, reason) = match action {
            Action::Hold => continue,
            Action::SellAll(r) => (held, true, r),
            Action::SellShare(share, r) => {
                let a = (held as f64 * share).round() as u64;
                if a == 0 || held - a < reference { (held, true, r) } else { (a, false, r) }
            }
        };
        proceeds += quote(amount, last.vsol, last.vtok) as f64 / 1e9;
        held -= amount;
        out.push(Decision { t: now, all, reason });
        // A live sell lands about a slot after the decision; the clock restarts there.
        exit.sold(price, Duration::from_secs_f64(now + slot));
    }
    (out, proceeds - bet as f64 / 1e9)
}

/// Exit settings under test: the bot's defaults, overridden by EXIT_FIRST_DOWN, EXIT_STEP_DOWN
/// (percent under the entry, or "none"), EXIT_CLOSE (share left), EXIT_CHECK_SECS, EXIT_NO_POP,
/// EXIT_INSIDER_EXIT (0/1) and the rest below.
fn tuning() -> Tuning {
    let mut t = Tuning::default();
    let pct = |key: &str| std::env::var(key).ok().map(|v| if v == "none" { None } else { v.parse().ok() });
    if let Some(v) = pct("EXIT_FIRST_DOWN") {
        t.first_down_percent = v;
    }
    if let Some(v) = pct("EXIT_STEP_DOWN") {
        t.step_down_percent = v;
    }
    if let Ok(v) = std::env::var("EXIT_STEP_DOWN_QUIET") {
        t.step_down_quiet = v == "1";
    }
    if let Some(v) = pct("EXIT_STEP_FALL") {
        t.step_fall_percent = v;
    }
    if let Some(v) = pct("EXIT_DOWN_QUIET") {
        t.down_quiet_buy_sol = v;
    }
    if let Some(v) = std::env::var("EXIT_CLOSE").ok().and_then(|v| v.parse().ok()) {
        t.close_out_share = v;
    }
    if let Some(v) = pct("EXIT_WAIT_DROP") {
        t.wait_drop_percent = v;
    }
    if let Some(v) = pct("EXIT_WAIT_HIGH") {
        t.wait_high_percent = v;
    }
    if let Some(v) = pct("EXIT_WAIT_BUY_SOL") {
        t.wait_buy_sol = v;
    }
    if let Some(v) = pct("EXIT_WAIT_SELL_SOL") {
        t.wait_sell_sol = v;
    }
    if let Some(v) = pct("EXIT_WAIT_BUYS") {
        t.wait_buys = v.map(|n| n as u32);
    }
    if let Some(v) = std::env::var("EXIT_MAX_WAITS").ok().and_then(|v| v.parse().ok()) {
        t.max_waits = v;
    }
    if let Some(v) = pct("EXIT_TRAIL") {
        t.trail_percent = v;
    }
    if let Ok(v) = std::env::var("EXIT_TRAIL_WAITING") {
        t.trail_only_waiting = v == "1";
    }
    if let Ok(v) = std::env::var("EXIT_TRAIL_ALL") {
        t.trail_sells_all = v == "1";
    }
    if let Some(v) = pct("EXIT_FADE_BUY_SOL") {
        t.fade_buy_sol = v;
    }
    if let Some(v) = std::env::var("EXIT_FADE_QUIET").ok().and_then(|v| v.parse().ok()) {
        t.fade_quiet_secs = v;
    }
    if let Some(v) = pct("EXIT_FIRST_ALL_UP") {
        t.first_all_up_sol = v;
    }
    if let Some(v) = std::env::var("EXIT_FIRST_WAIT_SLOTS").ok().and_then(|v| v.parse().ok()) {
        t.first_wait_slots = v;
    }
    if let Ok(v) = std::env::var("EXIT_FIRST_WAIT") {
        t.first_wait = v == "1";
    }
    if let Some(v) = std::env::var("EXIT_FIRST_SHARE").ok().and_then(|v| v.parse().ok()) {
        t.first_share = v;
    }
    // EXIT_FIRST_UP=<% or none>: the first sell only while the price is over the entry by this.
    if let Some(v) = pct("EXIT_FIRST_UP") {
        t.first_up_percent = v;
    }
    if let Some(v) = std::env::var("EXIT_CHECK_SECS").ok().and_then(|v| v.parse().ok()) {
        t.check_secs = v;
    }
    if let Some(v) = pct("EXIT_NO_POP") {
        t.no_pop_percent = v;
    }
    if let Some(v) = std::env::var("EXIT_NO_POP_SHARE").ok().and_then(|v| v.parse().ok()) {
        t.no_pop_share = v;
    }
    if let Ok(v) = std::env::var("EXIT_NO_POP_QUIET") {
        t.no_pop_quiet = v == "1";
    }
    if let Some(v) = pct("EXIT_NO_POP_HOLD_HIGH") {
        t.no_pop_hold_high_percent = v;
    }
    if let Some(v) = pct("EXIT_NO_POP_HOLD_BEHIND") {
        t.no_pop_hold_behind_sol = v;
    }
    if let Some(v) = pct("EXIT_NO_POP_STOP") {
        t.no_pop_stop_percent = v;
    }
    // EXIT_PATIENT=<take profit %>,<stop after s>,<stop % or none>,<max hold s>
    if let Ok(v) = std::env::var("EXIT_PATIENT") {
        let f: Vec<&str> = v.split(',').collect();
        let num = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok());
        t.patient = (v != "none").then(|| sniper_bot::utils::auto_sell::Patient {
            take_profit_percent: num(0).unwrap_or(50.0),
            stop_after_secs: num(1).unwrap_or(30.0),
            stop_percent: num(2),
            max_hold_secs: num(3).unwrap_or(60.0),
        });
    }
    if let Ok(v) = std::env::var("EXIT_INSIDER_EXIT") {
        t.insider_exit = v == "1";
    }
    t
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 { "-".into() } else { format!("{:.0}%", 100.0 * n as f64 / d as f64) }
}

/// The fast-rug flag the bot would have set at the create: metadata host and dev buy.
fn fast_rug_of(launch_dir: Option<&str>, mint: &str) -> Option<bool> {
    let launch: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/{mint}.json", launch_dir?)).ok()?).ok()?;
    let host = launch["uri"].as_str().and_then(sniper_bot::processors::common::uri_host);
    let dev = launch["dev_trades"].as_array()?.iter().find(|e| e["is_buy"] == true).and_then(|e| e["sol"].as_u64()).unwrap_or(0);
    Some(rules::fast_rug_profile(host.as_deref(), dev))
}

fn exit(positions_path: &str, tapes: &str, launch_dir: Option<&str>) -> Result<()> {
    let (mut flagged, mut flagged_pnl, mut flagged_his) = (0, 0.0, 0.0);
    let positions: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(positions_path)?)?;
    let (mut p1, mut p1_ok, mut p1_he, mut p1_bot) = (0, 0, 0, 0);
    let mut check: HashMap<(&'static str, &'static str), usize> = HashMap::new();
    let (mut bot_gaps, mut his_gaps, mut bot_last, mut his_last) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // Later steps (after the second sell): how many waits each took.
    let mut bot_waits = [0usize; 4];
    let mut reasons: HashMap<&'static str, usize> = HashMap::new();
    let (mut bot_pnl, mut his_pnl, mut n) = (0.0, 0.0, 0);
    for p in &positions {
        let mint = p["mint"].as_str().unwrap_or_default();
        let date = p["date"].as_str().unwrap_or_default();
        let slot = slot_secs(date);
        // Prefer the launch data's tape (the first 2 minutes of the token, longer than any hold);
        // the study's tape stops 30 slots after his last sell.
        let long = launch_dir
            .and_then(|d| std::fs::read_to_string(format!("{d}/{mint}.json")).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|l| l["tape"].as_array().cloned())
            .filter(|t| t.iter().any(|e| e["user"] == W && e["is_buy"] == true));
        let events = match long {
            Some(t) => t,
            None => {
                let tape: Value = serde_json::from_str(&std::fs::read_to_string(format!("{tapes}/{mint}.json")).context(mint.to_string())?)?;
                tape["events"].as_array().cloned().unwrap_or_default()
            }
        };
        let Some(bi) = events.iter().position(|e| e["user"] == W && e["is_buy"] == true) else { continue };
        let b = &events[bi];
        let bslot = b["slot"].as_u64().unwrap_or(0);
        let bet = b["sol"].as_u64().unwrap_or(0);
        let tokens0 = b["tok"].as_u64().unwrap_or(0);
        if tokens0 == 0 {
            continue;
        }
        let mut evs = vec![Ev {
            t: 0.0,
            slot_off: 0,
            is_buy: true,
            mine: true,
            sol: bet,
            vsol: b["vsol"].as_u64().unwrap_or(0),
            vtok: b["vtok"].as_u64().unwrap_or(1),
        }];
        let mut order = 0.0;
        let mut last_slot = bslot;
        let mut his_in = bet as f64;
        let mut his_out = 0.0;
        for e in &events[bi + 1..] {
            let s = e["slot"].as_u64().unwrap_or(0);
            if s != last_slot {
                order = 0.0;
                last_slot = s;
            }
            order += 0.001;
            let mine = e["user"] == W;
            let sol = e["sol"].as_u64().unwrap_or(0);
            if mine {
                if e["is_buy"] == true { his_in += sol as f64 } else { his_out += sol as f64 * 0.9875 }
            }
            evs.push(Ev {
                t: (s - bslot) as f64 * slot + order,
                slot_off: s - bslot,
                is_buy: e["is_buy"] == true,
                mine,
                sol,
                vsol: e["vsol"].as_u64().unwrap_or(0),
                vtok: e["vtok"].as_u64().unwrap_or(1),
            });
        }
        let fast_rug = fast_rug_of(launch_dir, mint).unwrap_or(false);
        let (decisions, pnl) = simulate(&evs, bet, tokens0, slot);
        if fast_rug {
            flagged += 1;
            flagged_pnl += pnl;
            flagged_his += (his_out - his_in) / 1e9;
        }
        n += 1;
        bot_pnl += pnl;
        his_pnl += (his_out - his_in) / 1e9;
        for d in &decisions {
            *reasons.entry(d.reason.code()).or_default() += 1;
        }
        let his_slots: Vec<u64> = p["dec_slots"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64()).collect()).unwrap_or_default();
        let his_pcts: Vec<u64> = p["pcts"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64()).collect()).unwrap_or_default();
        let his_first = his_slots.first().copied().unwrap_or(999) as f64 * slot;
        // Phase 1: a first slice inside the buy slot.
        p1 += 1;
        let he = his_slots.first() == Some(&0);
        let bot = decisions.first().is_some_and(|d| d.t < slot);
        p1_ok += (he == bot) as usize;
        p1_he += he as usize;
        p1_bot += bot as usize;
        // Phase 2: positions neither of them sold before the check window.
        let bot_first = decisions.first().map(|d| d.t).unwrap_or(999.0);
        if his_first >= 1.6 && bot_first >= 1.6 {
            let his = if his_first > 3.2 { "held" } else if his_pcts.first() == Some(&100) { "sold all" } else { "sold part" };
            let b = match decisions.first() {
                Some(d) if d.t <= 3.2 && d.all => "sells all",
                Some(d) if d.t <= 3.2 => "sells part",
                _ => "holds",
            };
            *check.entry((his, b)).or_default() += 1;
        }
        for w in decisions.windows(2) {
            bot_gaps.push(w[1].t - w[0].t);
        }
        for (i, w) in decisions.windows(2).enumerate().skip(1) {
            if w[1].reason == SellReason::Step {
                let waited = ((w[1].t - w[0].t - slot - rules::STEP_SECS) / rules::STEP_SECS).round().clamp(0.0, 3.0) as usize;
                bot_waits[waited] += 1;
                let _ = i;
            }
        }
        for w in his_slots.windows(2) {
            his_gaps.push((w[1] - w[0]) as f64 * slot);
        }
        if let Some(d) = decisions.last() {
            bot_last.push(d.t);
        }
        if let Some(s) = his_slots.last() {
            his_last.push(*s as f64 * slot);
        }
    }
    let med = |v: &mut Vec<f64>, q: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() { f64::NAN } else { v[((v.len() - 1) as f64 * q) as usize] }
    };
    let near = |v: &[f64], x: f64| v.iter().filter(|g| (**g - x).abs() <= 0.35).count();
    println!("EXIT on {n} of his positions, bought at his fill, played over the real tape");
    println!("  first slice inside the buy slot: agrees with him on {} ({p1_ok}/{p1}); he did it on {}, the rule on {}", pct(p1_ok, p1), pct(p1_he, p1), pct(p1_bot, p1));
    println!("  2 s check (neither sold before 1.6 s):");
    for his in ["sold all", "sold part", "held"] {
        let row: Vec<String> = ["sells all", "sells part", "holds"].iter().map(|b| format!("rule {b}: {:3}", check.get(&(his, b)).unwrap_or(&0))).collect();
        println!("    he {his:9} | {}", row.join(" | "));
    }
    println!("  gap between sells: rule median {:.2} s ({} within 0.35 s of 1 s, {} of 2 s); his median {:.2} s ({} / {})",
        med(&mut bot_gaps, 0.5), pct(near(&bot_gaps, 1.0), bot_gaps.len()), pct(near(&bot_gaps, 2.0), bot_gaps.len()),
        med(&mut his_gaps, 0.5), pct(near(&his_gaps, 1.0), his_gaps.len()), pct(near(&his_gaps, 2.0), his_gaps.len()));
    println!("  last sell: rule median {:.1} s, p90 {:.1} s; his median {:.1} s, p90 {:.1} s",
        med(&mut bot_last, 0.5), med(&mut bot_last, 0.9), med(&mut his_last, 0.5), med(&mut his_last, 0.9));
    let steps: usize = bot_waits.iter().sum();
    println!("  later steps: sold on the first tick {}, after one wait {}, after two {} (his later sells: 63% / 23% / 14% on ticks 1 / 2 / 3+)",
        pct(bot_waits[0], steps), pct(bot_waits[1], steps), pct(bot_waits[2] + bot_waits[3], steps));
    let mut r: Vec<_> = reasons.into_iter().collect();
    r.sort_by(|a, b| b.1.cmp(&a.1));
    println!("  rule decisions by reason: {}", r.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "));
    if launch_dir.is_some() {
        println!("  fast-rug profile: {flagged} positions, rule {flagged_pnl:+.1} SOL on them, his real exits {flagged_his:+.1} SOL");
    }
    println!("  curve-quoted result on the same entries: rule {bot_pnl:+.1} SOL, his real exits {his_pnl:+.1} SOL (fees 1.25%, no tips)");
    Ok(())
}

// ---- full ---------------------------------------------------------------------------------

/// A trade by someone else after the bot's fill.
struct Trade {
    t: f64,
    /// Slots after the fill's slot.
    slot_off: u64,
    is_buy: bool,
    /// The dev or a wallet of the create's bundle.
    insider: bool,
    sol: u64,
    vsol: u64,
    vtok: u64,
}

/// SOL out (after the 1.25% fee) for selling `amount` of the `held` tokens the bot keeps off a
/// curve the tape shows at (vsol, vtok) without it. Others' token amounts are taken as given, so
/// the bot's tokens sit on top of the real curve.
fn sell_value(amount: u64, held: u64, vsol: u64, vtok: u64) -> f64 {
    let k = vsol as f64 * vtok as f64;
    let vt = vtok as f64 - held as f64;
    (k / vt - k / (vt + amount as f64)) * 0.9875
}

thread_local! {
    /// TRACE_MINT matched the token being replayed: print its sells.
    static TRACE: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
    /// FILL_POSITION tally: attempts, guarded out, filled, pnl, wins, staked.
    static CAL: std::rc::Rc<std::cell::RefCell<(usize, usize, usize, f64, usize, f64)>> = std::rc::Rc::new(std::cell::RefCell::new((0, 0, 0, 0.0, 0, 0.0)));
}

/// The exit played over the tape after a fill at (vsol0, vtok0). Returns the SOL result and the
/// sell reasons.
fn play(trades: &[Trade], bet: u64, vsol0: u64, vtok0: u64, slot: f64, dev_exit: bool) -> (f64, Vec<SellReason>) {
    let k = vsol0 as f64 * vtok0 as f64;
    let spent = bet as f64 / 1.0125;
    let tokens0 = (vtok0 as f64 - k / (vsol0 as f64 + spent)) as u64;
    if tokens0 == 0 {
        // No curve state to fill against (a tape entry without reserves).
        return (0.0, Vec::new());
    }
    let reference = (tokens0 / 20).max(1);
    let entry_price = (bet as u128 * reference as u128 / tokens0 as u128) as u64;
    let mut exit = RefExit::with_tuning(bet, tuning());
    // EXIT_SELL_LAG_SLOTS=<n>: every sell lands at the top of the n-th slot after the decision's,
    // filled at the curve after every trade before it (1: the rest of the decision's slot), not
    // at the decision's curve. 0 (default): at the decision's curve.
    let lag: u64 = std::env::var("EXIT_SELL_LAG_SLOTS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut held = tokens0;
    let mut proceeds = 0.0;
    let mut reasons = Vec::new();
    let mut times: Vec<f64> = trades.iter().map(|e| e.t).collect();
    let mut t = 0.0;
    while t <= 180.0 {
        times.push(t);
        t += 0.1;
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    for now in times {
        if held == 0 {
            break;
        }
        let seen: Vec<&Trade> = trades.iter().filter(|e| e.t <= now).collect();
        let (vsol, vtok) = seen.last().map(|e| (e.vsol, e.vtok)).unwrap_or((vsol0, vtok0));
        let behind: u64 = seen.iter().filter(|e| e.is_buy).map(|e| e.sol).sum();
        let mut flow = Flow::default();
        for e in seen.iter().filter(|e| e.t > now - 1.0) {
            if e.is_buy {
                flow.buys += 1;
                flow.buy_lamports += e.sol;
            } else {
                flow.sells += 1;
                flow.sell_lamports += e.sol;
            }
        }
        let price = sell_value(reference, held, vsol, vtok) as u64;
        // The dev or a bundle wallet sold behind the fill: the exit's insider rule decides. Variant
        // `dev_exit`: everything goes at once, first sell or not (reported as MANUAL).
        let insider_sold = seen.iter().any(|e| e.insider && !e.is_buy);
        let recent_buy_lamports: u64 = seen.iter().filter(|e| e.is_buy && e.t > now - 2.0).map(|e| e.sol).sum();
        let since_last_buy = seen.iter().filter(|e| e.is_buy).map(|e| e.t).fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))))
            .map(|t| Duration::from_secs_f64((now - t).max(0.0)));
        let wait = exit.first_wait_slots();
        let window_net = seen.iter().filter(|e| e.slot_off <= wait).map(|e| if e.is_buy { e.sol as i64 } else { -(e.sol as i64) }).sum();
        let action = if dev_exit && insider_sold { Action::SellAll(SellReason::Manual) } else { exit.evaluate(&Tick {
            held_for: Duration::from_secs_f64(now),
            entry_price,
            price,
            behind_lamports: behind,
            behind_sell_lamports: 0,
            last_second: flow,
            recent_buy_lamports,
            recent_sell_lamports_3s: 0,
            since_last_buy,
            left: held as f64 / tokens0 as f64,
            window_net_lamports: window_net,
            in_wait_window: ((now / slot).floor() as u64) <= wait,
            insider_sold,
        }) };
        let (amount, reason) = match action {
            Action::Hold => continue,
            Action::SellAll(r) => (held, r),
            Action::SellShare(share, r) => {
                let a = (held as f64 * share).round() as u64;
                if a == 0 || held - a < reference { (held, r) } else { (a, r) }
            }
        };
        if TRACE.with(|t| *t.borrow()) {
            eprintln!(
                "      {now:5.2} s  {:<9} x{:.2}  others' buys in the last 2 s {:.3} SOL, last 1 s: bought {:.2} / sold {:.2} SOL",
                reason.code(),
                price as f64 / entry_price.max(1) as f64,
                recent_buy_lamports as f64 / 1e9,
                flow.buy_lamports as f64 / 1e9,
                flow.sell_lamports as f64 / 1e9
            );
        }
        let lands = (now / slot).floor() as u64 + lag;
        let (fvsol, fvtok) = if lag > 0 {
            trades.iter().filter(|e| e.t <= now || e.slot_off < lands).last().map(|e| (e.vsol, e.vtok)).unwrap_or((vsol, vtok))
        } else {
            (vsol, vtok)
        };
        proceeds += sell_value(amount, held, fvsol, fvtok);
        held -= amount;
        reasons.push(reason);
        exit.sold(price, Duration::from_secs_f64(now + slot));
    }
    ((proceeds - bet as f64) / 1e9, reasons)
}

#[derive(Default)]
struct Tally {
    passed: usize,
    no_tape: usize,
    lost_race: usize,
    filled: usize,
    wins: usize,
    pnl: f64,
    staked: f64,
    results: Vec<f64>,
}

impl Tally {
    fn add(&mut self, pnl: f64, bet: u64) {
        self.filled += 1;
        self.wins += (pnl > 0.0) as usize;
        self.pnl += pnl;
        self.staked += bet as f64 / 1e9;
        self.results.push(pnl / (bet as f64 / 1e9));
    }

    fn line(&mut self, label: &str) -> String {
        self.results.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = if self.results.is_empty() { f64::NAN } else { self.results[self.results.len() / 2] * 100.0 };
        format!(
            "{label:<22} passed {:4} | lost to the guard {:4} | bought {:4} | won {:>4} | result {:+7.2} SOL on {:6.1} staked ({:+.1}%), median trade {:+.1}%",
            self.passed - self.no_tape,
            self.lost_race,
            self.filled,
            pct(self.wins, self.filled),
            self.pnl,
            self.staked,
            100.0 * self.pnl / self.staked.max(1e-9),
            med
        )
    }
}

fn full(facts_path: &str, features_path: &str, history_path: &str, launch_dir: &str) -> Result<()> {
    let verdicts = entry_verdicts(facts_path, features_path, history_path)?;
    let stake = lamports(1.0);
    let guard = 1.0 + rules::BUY_SLIPPAGE_PERCENT / 100.0;
    // [his][model]: model 0 lands right behind the create's bundle, model 1 at the top of the
    // next slot (after every trade of the create slot).
    let mut tallies: [[Tally; 2]; 2] = Default::default();
    let mut reasons: [HashMap<&'static str, usize>; 2] = Default::default();
    for (f, verdict) in &verdicts {
        if verdict.is_err() {
            continue;
        }
        let mint = f["mint"].as_str().unwrap_or_default();
        let launch: Option<Value> = std::fs::read_to_string(format!("{launch_dir}/{mint}.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
        let tape: Vec<Value> = launch.as_ref().and_then(|l| l["tape"].as_array().cloned()).unwrap_or_default();
        let his = f["group"] == "attempted" || tape.iter().any(|e| e["user"] == W && e["is_buy"] == true);
        for m in 0..2 {
            tallies[his as usize][m].passed += 1;
        }
        let Some(first) = tape.first() else {
            for m in 0..2 {
                tallies[his as usize][m].no_tape += 1;
            }
            continue;
        };
        let s0 = first["slot"].as_u64().unwrap_or(0);
        let slot = launch_slot_secs(s0, f["bt"].as_u64().unwrap_or(0), &tape);
        // The create's bundle: the create slot's trades in consecutive block positions from the first.
        let mut bundle = 1;
        while bundle < tape.len()
            && tape[bundle]["slot"].as_u64() == Some(s0)
            && tape[bundle]["idx"].as_u64().unwrap_or(0) <= tape[bundle - 1]["idx"].as_u64().unwrap_or(0) + 1
        {
            bundle += 1;
        }
        let in_slot0 = tape.iter().take_while(|e| e["slot"].as_u64() == Some(s0)).count();
        let reserves = |i: usize| (tape[i - 1]["vsol"].as_u64().unwrap_or(0), tape[i - 1]["vtok"].as_u64().unwrap_or(1));
        let (qs, qt) = reserves(bundle);
        let quote_price = qs as f64 / qt as f64;
        let bet = rules::bet_lamports(stake, lamports(f["dev_buy"].as_f64().unwrap_or(0.0)));
        let insiders: HashSet<&str> = tape[..bundle].iter().filter_map(|e| e["user"].as_str()).collect();
        for (m, at) in [bundle, in_slot0].into_iter().enumerate() {
            let (vs, vt) = reserves(at);
            // BUY_AHEAD=none (or a SOL amount) overrides rules::BUY_AHEAD_MAX_SOL for comparisons.
            let ahead_cap = match std::env::var("BUY_AHEAD").ok().as_deref() {
                Some("none") => None,
                Some(v) => v.parse().ok(),
                None => rules::BUY_AHEAD_MAX_SOL,
            };
            let lost = match ahead_cap {
                Some(sol) => vs.saturating_sub(qs) > lamports(sol),
                None => vs as f64 / vt as f64 > quote_price * guard,
            };
            if lost {
                tallies[his as usize][m].lost_race += 1;
                continue;
            }
            let fill_slot = tape[at - 1]["slot"].as_u64().unwrap_or(s0) + (m == 1) as u64;
            let mut trades = Vec::new();
            let (mut order, mut last) = (0.0, 0);
            for e in &tape[at..] {
                let s = e["slot"].as_u64().unwrap_or(0);
                if s != last {
                    order = 0.0;
                    last = s;
                }
                order += 0.001;
                trades.push(Trade {
                    t: s.saturating_sub(fill_slot) as f64 * slot + order,
                    slot_off: s.saturating_sub(fill_slot),
                    is_buy: e["is_buy"] == true,
                    insider: insiders.contains(e["user"].as_str().unwrap_or_default()),
                    sol: e["sol"].as_u64().unwrap_or(0),
                    vsol: e["vsol"].as_u64().unwrap_or(0),
                    vtok: e["vtok"].as_u64().unwrap_or(1),
                });
            }
            let (pnl, why) = play(&trades, bet, vs, vt, slot, false);
            tallies[his as usize][m].add(pnl, bet);
            if m == 1 {
                for r in why {
                    *reasons[his as usize].entry(r.code()).or_default() += 1;
                }
            }
        }
    }
    println!("FULL RULES (entry, 1.25% buy guard, exit) on the sampled launches that pass the entry rules, 1 SOL stake");
    for (m, label) in ["fill right behind the create's bundle (best case)", "fill at the top of the next slot"].iter().enumerate() {
        println!("  {label}:");
        println!("    {}", tallies[1][m].line("launches he bought"));
        println!("    {}", tallies[0][m].line("launches he skipped"));
    }
    for (h, label) in [(1, "he bought"), (0, "he skipped")] {
        let mut r: Vec<_> = reasons[h].iter().collect();
        r.sort_by(|a, b| b.1.cmp(a.1));
        println!("  sell reasons, next-slot fill, {label}: {}", r.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

/// "MM-DD" (UTC) of a unix time, for `slot_secs`.
fn chrono_date(unix: u64) -> String {
    let days = unix / 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{m:02}-{d:02}")
}

/// The tape after position `at`, timed from a fill in `fill_slot`.
fn trades_after(tape: &[Value], at: usize, fill_slot: u64, slot: f64, insiders: &HashSet<String>) -> Vec<Trade> {
    let mut trades = Vec::new();
    let (mut order, mut last) = (0.0, 0);
    for e in &tape[at..] {
        let s = e["slot"].as_u64().unwrap_or(0);
        if s != last {
            order = 0.0;
            last = s;
        }
        order += 0.001;
        trades.push(Trade {
            t: s.saturating_sub(fill_slot) as f64 * slot + order,
            slot_off: s.saturating_sub(fill_slot),
            is_buy: e["is_buy"] == true,
            insider: insiders.contains(e["user"].as_str().unwrap_or_default()),
            sol: e["sol"].as_u64().unwrap_or(0),
            vsol: e["vsol"].as_u64().unwrap_or(0),
            vtok: e["vtok"].as_u64().unwrap_or(1),
        });
    }
    trades
}

/// The tape without `wallets`' trades, the curve put back as if they never happened: every
/// later state gets the tokens those wallets held at that point returned to the curve (constant
/// product). A live buy's tape holds the bot's own buy and sells; `play` puts the modeled
/// position on top of the curve itself, so leaving them in counts the bot twice (its buy as a
/// buyer behind the fill, the curve lifted by its tokens) and its sells as other wallets' sells.
fn without_wallets(tape: &[Value], wallets: &HashSet<String>) -> Vec<Value> {
    let mut held: u128 = 0;
    let mut out = Vec::with_capacity(tape.len());
    for e in tape {
        if wallets.contains(e["user"].as_str().unwrap_or_default()) {
            let tok = e["tok"].as_u64().unwrap_or(0) as u128;
            held = if e["is_buy"] == true { held + tok } else { held.saturating_sub(tok) };
            continue;
        }
        let mut e = e.clone();
        if held > 0 {
            let (vs, vt) = (e["vsol"].as_u64().unwrap_or(0) as u128, e["vtok"].as_u64().unwrap_or(1) as u128);
            let vt2 = vt + held;
            e["vsol"] = Value::from((vs * vt / vt2) as u64);
            e["vtok"] = Value::from(vt2 as u64);
        }
        out.push(e);
    }
    out
}

/// The bot's wallets: the buyers of exactly the tokens its confirmed (live) buys got.
fn own_wallets(rows: &[Vec<&str>], launch_dir: &str, c: (usize, usize, usize, usize)) -> HashSet<String> {
    let (c_action, c_mint, c_status, c_tok) = c;
    let mut out = HashSet::new();
    for r in rows.iter().filter(|r| r.get(c_action) == Some(&"BUY") && r.get(c_status) == Some(&"confirmed")) {
        let tokens: u64 = r.get(c_tok).and_then(|t| t.parse().ok()).unwrap_or(0);
        let Some(launch) = r.get(c_mint).and_then(|m| std::fs::read_to_string(format!("{launch_dir}/{m}.json")).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        let own = launch["tape"].as_array().and_then(|t| t.iter().find(|e| e["is_buy"] == true && e["tok"].as_u64() == Some(tokens)).and_then(|e| e["user"].as_str()).map(String::from));
        out.extend(own);
    }
    out
}

fn play_live(launch_dir: &str, log_path: &str, since: Option<&str>) -> Result<()> {
    let log = std::fs::read_to_string(log_path)?;
    let mut lines = log.lines();
    let header: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).unwrap_or(usize::MAX);
    let (c_action, c_mint, c_status, c_in, c_out, c_tok) = (col("action"), col("mint"), col("status"), col("sol_in"), col("sol_out"), col("token_amount"));
    let (c_ts, c_wallet) = (col("timestamp"), col("wallet_sol_change"));
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).filter(|r: &Vec<&str>| since.is_none_or(|s| r.get(c_ts).is_some_and(|t| *t >= s))).collect();
    fn get<'a>(r: &[&'a str], c: usize) -> &'a str {
        r.get(c).copied().unwrap_or_default()
    }
    let (mut model_sum, mut paper_sum) = (0.0, 0.0);
    // By mode (live, paper): buys, model, bot's quoted result, wallet change.
    let mut by_mode: HashMap<&str, (usize, f64, f64, f64)> = HashMap::new();
    let mut missing = 0;
    // Found over every row (not only those since `since`): the wallet is the same.
    let all_rows: Vec<Vec<&str>> = log.lines().skip(1).map(|l| l.split(',').collect()).collect();
    let own = own_wallets(&all_rows, launch_dir, (c_action, c_mint, c_status, c_tok));
    println!(
        "BOT BUYS replayed from the state they filled at (tape position matched by token count){}; the bot's own trades taken off the tapes ({} wallet{})",
        since.map(|s| format!(", since {s}")).unwrap_or_default(),
        own.len(),
        if own.len() == 1 { "" } else { "s" }
    );
    for r in rows.iter().filter(|r| get(r, c_action) == "BUY") {
        let mint = get(r, c_mint);
        let Ok(text) = std::fs::read_to_string(format!("{launch_dir}/{mint}.json")) else {
            missing += 1;
            continue;
        };
        let launch: Value = serde_json::from_str(&text)?;
        let tape = without_wallets(launch["tape"].as_array().map(Vec::as_slice).unwrap_or_default(), &own);
        if tape.is_empty() {
            continue;
        }
        let bet = lamports(get(r, c_in).parse().unwrap_or(0.0));
        let s0 = tape[0]["slot"].as_u64().unwrap_or(0);
        let slot = launch_slot_secs(s0, launch["bt"].as_u64().unwrap_or(0), &tape);
        let in_slot0 = tape.iter().take_while(|e| e["slot"].as_u64() == Some(s0)).count();
        let slot0_buys: u64 = tape[1..in_slot0].iter().filter(|e| e["is_buy"] == true).map(|e| e["sol"].as_u64().unwrap_or(0)).sum();
        let slot0_sells: u64 = tape[1..in_slot0].iter().filter(|e| e["is_buy"] != true).map(|e| e["sol"].as_u64().unwrap_or(0)).sum();
        let dev = tape[0]["user"].as_str().unwrap_or_default().to_string();
        let dev_sold_at = tape.iter().find(|e| e["user"] == dev.as_str() && e["is_buy"] != true).map(|e| (e["slot"].as_u64().unwrap_or(0) - s0) as f64 * slot);
        let status = get(r, c_status);
        // The dev's bundle: consecutive indexes right after the create. Its wallets and the dev
        // are the insiders (EXIT_INSIDER).
        let ci = tape[0]["idx"].as_u64().unwrap_or(0);
        let mut bundle_end = 1;
        while bundle_end < in_slot0 && tape[bundle_end]["idx"].as_u64() == Some(ci + bundle_end as u64) {
            bundle_end += 1;
        }
        let insiders: HashSet<String> = tape[..bundle_end].iter().filter_map(|e| e["user"].as_str().map(String::from)).collect();
        // FILL_POSITION=<n> (launch slot, n transactions after the create) or next:<n> (the next
        // slot, at index n): every attempt fills there instead of where the bot put it, if the
        // price is within the 1.25% guard of the post-bundle quote; a calibration of live fills.
        if let Ok(spec) = std::env::var("FILL_POSITION") {
            let (at, fill_slot) = match spec.strip_prefix("next:") {
                Some(n) => {
                    let n: u64 = n.parse().unwrap_or(0);
                    (tape.iter().take_while(|e| e["slot"].as_u64() < Some(s0 + 1) || (e["slot"].as_u64() == Some(s0 + 1) && e["idx"].as_u64().unwrap_or(0) < n)).count(), s0 + 1)
                }
                None => {
                    let n: u64 = spec.parse().unwrap_or(0);
                    (tape.iter().take_while(|e| e["slot"].as_u64() == Some(s0) && e["idx"].as_u64().unwrap_or(0) < ci + n).count(), s0)
                }
            };
            let at = at.max(1);
            let price = |i: usize| tape[i - 1]["vsol"].as_f64().unwrap_or(0.0) / tape[i - 1]["vtok"].as_f64().unwrap_or(1.0);
            let guarded_out = price(at) > price(bundle_end) * 1.0125;
            let tally = CAL.with(|c| c.clone());
            let mut c = tally.borrow_mut();
            c.0 += 1;
            if guarded_out {
                c.1 += 1;
                continue;
            }
            let (vs, vt) = (tape[at - 1]["vsol"].as_u64().unwrap_or(0), tape[at - 1]["vtok"].as_u64().unwrap_or(1));
            let (pnl, _) = play(&trades_after(&tape, at, fill_slot, slot, &insiders), bet, vs, vt, slot, false);
            c.2 += 1;
            c.3 += pnl;
            c.4 += (pnl > 0.0) as usize;
            c.5 += bet as f64 / 1e9;
            continue;
        }
        let filled = !status.starts_with("lost") && !status.contains("fail");
        let head = format!(
            "{} dev buy {:.2} SOL, slot 0: {} trades, others bought {:.2} / sold {:.2} SOL; dev sold {}",
            &mint[..8],
            tape[0]["sol"].as_u64().unwrap_or(0) as f64 / 1e9,
            in_slot0,
            slot0_buys as f64 / 1e9,
            slot0_sells as f64 / 1e9,
            dev_sold_at.map(|t| format!("at {t:.1} s")).unwrap_or("never".into())
        );
        if !filled {
            // What a fill at the top of the next slot would have made, guard aside.
            let (vs, vt) = (tape[in_slot0 - 1]["vsol"].as_u64().unwrap_or(0), tape[in_slot0 - 1]["vtok"].as_u64().unwrap_or(1));
            let (pnl, _) = play(&trades_after(&tape, in_slot0, s0 + 1, slot, &insiders), bet, vs, vt, slot, false);
            println!("  {head}\n      lost to the guard; a next-slot fill would have made {pnl:+.3} SOL");
            continue;
        }
        let tokens: u64 = get(r, c_tok).parse().unwrap_or(0);
        // The fill position: the state after tape[i-1] whose buy of `bet` gives the dry run's tokens.
        // The curve often comes back to the same state a few trades later (a buy and a sell of the
        // same size), so the earliest state within 0.05% of the best match is the fill.
        let spent = bet as f64 / 1.0125;
        let miss = |i: usize| {
            let (vs, vt) = (tape[i - 1]["vsol"].as_f64().unwrap_or(0.0), tape[i - 1]["vtok"].as_f64().unwrap_or(1.0));
            ((vt - vs * vt / (vs + spent)) - tokens as f64).abs()
        };
        let best = (1..=tape.len()).map(miss).fold(f64::MAX, f64::min);
        let at = (1..=tape.len()).find(|&i| miss(i) <= best + tokens as f64 * 0.0005).unwrap_or(1);
        let (vs, vt) = (tape[at - 1]["vsol"].as_u64().unwrap_or(0), tape[at - 1]["vtok"].as_u64().unwrap_or(1));
        let fill_slot = tape[at - 1]["slot"].as_u64().unwrap_or(s0);
        // TRACE_MINT=<prefix>: the replay's sells for that token, on stderr.
        TRACE.with(|t| *t.borrow_mut() = std::env::var("TRACE_MINT").is_ok_and(|p| !p.is_empty() && mint.starts_with(&p)));
        let (pnl, why) = play(&trades_after(&tape, at, fill_slot, slot, &insiders), bet, vs, vt, slot, false);
        TRACE.with(|t| *t.borrow_mut() = false);
        // Sells that went through (paper or landed): a failed sell's quote was never received.
        let paper: f64 = rows
            .iter()
            .filter(|x| get(x, c_mint) == mint && get(x, c_action) != "BUY" && matches!(get(x, c_status), "paper" | "confirmed"))
            .map(|x| get(x, c_out).parse::<f64>().unwrap_or(0.0))
            .sum::<f64>()
            - bet as f64 / 1e9;
        let wallet: f64 = rows.iter().filter(|x| get(x, c_mint) == mint).map(|x| get(x, c_wallet).parse::<f64>().unwrap_or(0.0)).sum();
        let mode = if status == "confirmed" { "live" } else { "paper" };
        model_sum += pnl;
        paper_sum += paper;
        let m = by_mode.entry(mode).or_default();
        m.0 += 1;
        m.1 += pnl;
        m.2 += paper;
        m.3 += wallet;
        println!(
            "  {head}\n      {mode}, filled after tape trade {at} of {}: model {pnl:+.3} SOL ({}), bot {paper:+.3} SOL quoted, wallet {wallet:+.3} SOL",
            tape.len(),
            why.iter().map(|r| r.code()).collect::<Vec<_>>().join(" ")
        );
    }
    println!("  total on the filled buys: model {model_sum:+.3} SOL, bot {paper_sum:+.3} SOL quoted");
    for (mode, (n, model, bot, wallet)) in &by_mode {
        println!(
            "  {mode}: {n} buys, model {model:+.3} SOL, bot {bot:+.3} SOL quoted (gap {:+.3} SOL, {:+.1}% of the stake a trade), wallet {wallet:+.3} SOL with fees and tips",
            bot - model,
            100.0 * (bot - model) / *n as f64
        );
    }
    if missing > 0 {
        println!("  {missing} buy rows without a tape (run tools/fetch_tapes.py)");
    }
    if std::env::var("FILL_POSITION").is_ok() {
        let (n, out, filled, pnl, wins, staked) = *CAL.with(|c| c.clone()).borrow();
        println!(
            "  CALIBRATED fill at {}: {n} attempts, {out} lost to the guard ({:.0}%), {filled} filled, won {:.0}%, model {pnl:+.3} SOL on {staked:.1} staked ({:+.1}%)",
            std::env::var("FILL_POSITION").unwrap_or_default(),
            100.0 * out as f64 / n.max(1) as f64,
            100.0 * wins as f64 / filled.max(1) as f64,
            100.0 * pnl / staked.max(0.01)
        );
    }
    Ok(())
}

fn pops(launch_dir: &str) -> Result<()> {
    use sniper_bot::utils::deployer_record::{no_pop_behind_bundle, outcome_slots, set_slot_secs, LaunchTrade};
    let mut out = Vec::new();
    for entry in std::fs::read_dir(launch_dir)?.filter_map(|e| e.ok()) {
        let Ok(launch) = serde_json::from_str::<Value>(&std::fs::read_to_string(entry.path())?) else { continue };
        let tape = launch["tape"].as_array().cloned().unwrap_or_default();
        let (Some(first), Some(mint)) = (tape.first(), launch["mint"].as_str()) else { continue };
        let u = |e: &Value, k: &str| e[k].as_u64().unwrap_or(0);
        let (s0, create) = (u(first, "slot"), u(first, "idx"));
        // The launch's own slot time (the fitted clock), as the live record uses the measured one.
        set_slot_secs(Some(launch_slot_secs(s0, launch["bt"].as_u64().unwrap_or(0), &tape)));
        let outcome_slots = outcome_slots();
        // The create transaction's trades (one position) set the curve after it.
        let mut after = (u(first, "vsol"), u(first, "vtok"));
        let mut trades = Vec::new();
        for e in &tape[1..] {
            let (slot, idx) = (u(e, "slot"), u(e, "idx"));
            if slot == s0 && idx == create {
                after = (u(e, "vsol"), u(e, "vtok"));
            } else if slot <= s0 + outcome_slots {
                trades.push(LaunchTrade { slot_off: slot - s0, index: idx, is_buy: e["is_buy"] == true, sol: u(e, "sol"), vsol: u(e, "vsol"), vtok: u(e, "vtok") });
            }
        }
        let unix = launch["bt"].as_u64().unwrap_or_else(|| u(first, "bt"));
        out.push((unix, first["user"].as_str().unwrap_or_default().to_string(), mint.to_string(), no_pop_behind_bundle(create, after, &trades)));
    }
    out.sort();
    println!("unix_time,deployer,mint,no_pop");
    for (unix, deployer, mint, no_pop) in out {
        println!("{unix},{deployer},{mint},{}", no_pop as u8);
    }
    Ok(())
}

/// Every launch under `path`: one JSON file per launch (the study's layout), or JSON lines (the
/// bot's tape recorder, `logs/tapes/<date>.jsonl`), as a directory or a single file.
fn launch_values(path: &str) -> Result<Vec<Value>> {
    let p = std::path::Path::new(path);
    let mut files: Vec<std::path::PathBuf> = if p.is_dir() { std::fs::read_dir(p)?.filter_map(|e| e.ok()).map(|e| e.path()).collect() } else { vec![p.to_path_buf()] };
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f)?;
        if f.extension().is_some_and(|e| e == "jsonl") {
            out.extend(text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()));
        } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
            out.push(v);
        }
    }
    Ok(out)
}

fn results(launch_dir: &str) -> Result<()> {
    use sniper_bot::utils::{deployer_record::LaunchTrade, launch_results};
    for launch in launch_values(launch_dir)? {
        let tape = launch["tape"].as_array().cloned().unwrap_or_default();
        let Some(first) = tape.first() else { continue };
        let s0 = first["slot"].as_u64().unwrap_or(0);
        let u = |e: &Value, k: &str| e[k].as_u64().unwrap_or(0);
        let trades: Vec<LaunchTrade> = tape[1..]
            .iter()
            .map(|e| LaunchTrade { slot_off: u(e, "slot") - s0, index: u(e, "idx"), is_buy: e["is_buy"] == true, sol: u(e, "sol"), vsol: u(e, "vsol"), vtok: u(e, "vtok") })
            .collect();
        sniper_bot::utils::deployer_record::set_slot_secs(Some(launch_slot_secs(s0, launch["bt"].as_u64().unwrap_or(0), &tape)));
        let r = launch_results::replay(u(first, "idx"), (u(first, "vsol"), u(first, "vtok")), &trades);
        println!("{}", serde_json::json!({ "mint": launch["mint"], "pnl": r.map(|r| r.pnl_sol), "down": r.map(|r| r.down) }));
    }
    Ok(())
}

fn dump(launch_dir: &str) -> Result<()> {
    // DUMP_STAKE=<SOL>: the buy size replayed (default 1 SOL).
    let stake = lamports(std::env::var("DUMP_STAKE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0));
    for launch in launch_values(launch_dir)? {
        let tape = launch["tape"].as_array().cloned().unwrap_or_default();
        let mint = launch["mint"].as_str().unwrap_or_default();
        let bt = launch["bt"].as_u64().or(launch["create"]["bt"].as_u64()).unwrap_or(0);
        if tape.is_empty() || bt == 0 {
            continue;
        }
        let s0 = tape[0]["slot"].as_u64().unwrap_or(0);
        let slot = launch_slot_secs(s0, bt, &tape);
        let mut bundle = 1;
        while bundle < tape.len()
            && tape[bundle]["slot"].as_u64() == Some(s0)
            && tape[bundle]["idx"].as_u64().unwrap_or(0) <= tape[bundle - 1]["idx"].as_u64().unwrap_or(0) + 1
        {
            bundle += 1;
        }
        let upto = |slots: u64| tape.iter().take_while(|e| e["slot"].as_u64().unwrap_or(0) < s0 + slots).count().max(1);
        let reserves = |i: usize| (tape[i - 1]["vsol"].as_u64().unwrap_or(0), tape[i - 1]["vtok"].as_u64().unwrap_or(1));
        let (qs, qt) = reserves(bundle);
        let quote_price = qs as f64 / qt as f64;
        let mut fills = Vec::new();
        // Copying him: quoted right after his first buy lands, filled at the top of the next slot
        // (or the one after).
        let his = tape.iter().position(|e| e["user"] == W && e["is_buy"] == true);
        let mut plans = vec![("bundle", bundle, s0, quote_price), ("next", upto(1), s0 + 1, quote_price), ("next2", upto(2), s0 + 2, quote_price)];
        // Inside the launch slot, behind the first k other trades of that slot.
        let in_slot0 = upto(1);
        for (name, k) in [("s0k1", 1), ("s0k2", 2), ("s0k3", 3), ("s0k5", 5)] {
            plans.push((name, (bundle + k).min(in_slot0), s0, quote_price));
        }
        if let Some(h) = his {
            let hs = tape[h]["slot"].as_u64().unwrap_or(s0);
            let (ps, pt) = reserves(h + 1);
            let after = |slots: u64| tape.iter().take_while(|e| e["slot"].as_u64().unwrap_or(0) < hs + slots).count().max(h + 1);
            plans.push(("copy", after(1), hs + 1, ps as f64 / pt as f64));
        }
        let insiders: HashSet<String> = tape[..bundle].iter().filter_map(|e| e["user"].as_str().map(String::from)).collect();
        let mut plans: Vec<(String, usize, u64, f64, Value)> =
            plans.into_iter().map(|(n, at, fs, q)| (n.to_string(), at, fs, q, Value::Null)).collect();
        // At a block position, where live buys land: behind every trade before position `pos` of
        // the launch slot (counted from the create) or of the slot after (from the top).
        let create_idx = tape[0]["idx"].as_u64().unwrap_or(0);
        for off in 0..=1u64 {
            for pos in [0u64, 50, 100, 150, 200, 250, 300, 400, 600, 900] {
                let (s, i) = (s0 + off, if off == 0 { create_idx + pos } else { pos });
                let at = tape
                    .iter()
                    .take_while(|e| {
                        let es = e["slot"].as_u64().unwrap_or(0);
                        es < s || (es == s && e["idx"].as_u64().unwrap_or(0) < i)
                    })
                    .count()
                    .max(bundle);
                plans.push((format!("p{off}_{pos}"), at, s, quote_price, Value::Null));
            }
        }
        // Delayed entry: decide at the end of slot d on what the first slots showed, fill after
        // every trade of slot d+1 (a full slot of latency), guarded against the price at the decision.
        for d in 0..=8u64 {
            let seen = upto(d + 1);
            let at = upto(d + 2);
            let window = &tape[bundle.min(seen)..seen];
            let outsiders = window.iter().filter(|e| !insiders.contains(e["user"].as_str().unwrap_or_default()));
            let buyers: HashSet<&str> = outsiders.clone().filter(|e| e["is_buy"] == true).filter_map(|e| e["user"].as_str()).collect();
            let sol = |buy: bool| outsiders.clone().filter(|e| (e["is_buy"] == true) == buy).map(|e| e["sol"].as_u64().unwrap_or(0)).sum::<u64>() as f64 / 1e9;
            let insider_sold = window.iter().any(|e| e["is_buy"] != true && insiders.contains(e["user"].as_str().unwrap_or_default()));
            let (ds, dt) = reserves(seen);
            let seen_price = ds as f64 / dt as f64;
            let features = serde_json::json!({
                "buyers": buyers.len(),
                "buy_sol": sol(true),
                "sell_sol": sol(false),
                "insider_sold": insider_sold,
                "rise": (seen_price / quote_price - 1.0) * 100.0,
            });
            plans.push((format!("d{d}"), at, s0 + d + 2, seen_price, features));
        }
        for (name, at, fill_slot, quote, features) in plans {
            let (vs, vt) = reserves(at);
            let trades = trades_after(&tape, at, fill_slot, slot, &insiders);
            let (pnl, why) = play(&trades, stake, vs, vt, slot, false);
            let (pnl_dev, _) = play(&trades, stake, vs, vt, slot, true);
            fills.push(serde_json::json!({
                "fill": name,
                "at": at,
                "move": (vs as f64 / vt as f64 / quote - 1.0) * 100.0,
                "pnl": pnl,
                "pnl_dev": pnl_dev,
                "reasons": why.iter().map(|r| r.code()).collect::<Vec<_>>(),
                "seen": features,
            }));
        }
        println!("{}", serde_json::json!({ "mint": mint, "bundle": bundle, "fills": fills }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tape entry after a trade of `sol` for `tok` on a curve that was at (vs, vt).
    fn trade(user: &str, is_buy: bool, sol: u64, tok: u64, vs: &mut u64, vt: &mut u64) -> Value {
        if is_buy {
            *vs += sol;
            *vt -= tok;
        } else {
            *vs -= sol;
            *vt += tok;
        }
        serde_json::json!({ "user": user, "is_buy": is_buy, "sol": sol, "tok": tok, "vsol": *vs, "vtok": *vt })
    }

    /// Tokens out for `sol` in on a constant-product curve at (vs, vt).
    fn tokens_for(sol: u64, vs: u64, vt: u64) -> u64 {
        (vt as u128 - vs as u128 * vt as u128 / (vs as u128 + sol as u128)) as u64
    }

    /// The fitted clock gives each launch its own slot time; outside the table the tape's block
    /// times decide; with neither, the old per-date value.
    #[test]
    fn slot_time_comes_from_the_fitted_clock_then_the_tape() {
        // 317 ms a slot up to slot 100,000, 219 ms after (buckets of 1,000 slots)
        let mut text = String::new();
        let mut t = 1_788_800_000.0;
        for b in 0..200u64 {
            text += &format!("{}\t{t}\n", b * 1000);
            t += if b < 100 { 317.0 } else { 219.0 };
        }
        let clock = SlotClock::parse(&text).unwrap();
        assert!((clock.secs_per_slot(50_000).unwrap() - 0.317).abs() < 1e-9);
        assert!((clock.secs_per_slot(180_000).unwrap() - 0.219).abs() < 1e-9);
        assert!(clock.secs_per_slot(5_000_000).is_none(), "far past the table");
        let tape: Vec<Value> = (0..300u64).map(|i| serde_json::json!({ "slot": 5_000_000 + i, "bt": 1_791_900_000 + i * 22 / 100 })).collect();
        let v = clock_or_tape(Some(&clock), 5_000_000, &tape).unwrap();
        assert!((v - 0.22).abs() < 0.005, "the tape's own block times: {v}");
        let short: Vec<Value> = (0..10u64).map(|i| serde_json::json!({ "slot": 5_000_000 + i, "bt": 1_791_900_000 })).collect();
        assert!(clock_or_tape(None, 5_000_000, &short).is_none());
        assert_eq!(slot_secs(&chrono_date(1_791_900_000)), 0.267, "the old value stays the last resort");
    }

    #[test]
    fn the_bots_own_trades_come_off_the_tape_and_the_curve() {
        let (mut vs, mut vt) = (30_000_000_000u64, 1_000_000_000_000_000u64);
        let mut tape = Vec::new();
        let t = tokens_for(2_000_000_000, vs, vt);
        tape.push(trade("dev", true, 2_000_000_000, t, &mut vs, &mut vt));
        let (before_vs, before_vt) = (vs, vt);
        let mine = tokens_for(1_000_000_000, vs, vt);
        tape.push(trade("bot", true, 1_000_000_000, mine, &mut vs, &mut vt));
        let t = tokens_for(500_000_000, vs, vt);
        tape.push(trade("x", true, 500_000_000, t, &mut vs, &mut vt));
        // The bot sells everything back.
        let out = (vs as u128 - vs as u128 * vt as u128 / (vt as u128 + mine as u128)) as u64;
        tape.push(trade("bot", false, out, mine, &mut vs, &mut vt));
        let t = tokens_for(300_000_000, vs, vt);
        tape.push(trade("y", true, 300_000_000, t, &mut vs, &mut vt));

        let clean = without_wallets(&tape, &HashSet::from(["bot".to_string()]));
        assert_eq!(clean.iter().map(|e| e["user"].as_str().unwrap()).collect::<Vec<_>>(), ["dev", "x", "y"]);
        // The dev's entry is untouched; the curve before x's buy is the one the bot bought from.
        assert_eq!((clean[0]["vsol"].as_u64(), clean[0]["vtok"].as_u64()), (Some(before_vs), Some(before_vt)));
        // After x's buy, without the bot: x's tokens off the pre-bot curve (constant product,
        // integer rounding aside).
        let (xs, xt) = (clean[1]["vsol"].as_u64().unwrap() as f64, clean[1]["vtok"].as_u64().unwrap() as f64);
        let k = before_vs as f64 * before_vt as f64;
        assert!(((xs * xt) / k - 1.0).abs() < 1e-6);
        assert!(xs > before_vs as f64 && xs < before_vs as f64 + 510_000_000.0);
        // After the bot's sell the real curve holds none of its tokens: y's entry is as recorded.
        assert_eq!(clean[2]["vtok"], tape[4]["vtok"]);
    }
}
