//! B6: replays recorded launches through the fee engine and prints the fee it would have chosen
//! per launch and per leader client class, next to what the first buyer outside the bundle paid.
//!
//!     cargo run --release --bin fee_replay -- [rows.csv ...]
//!
//! Rows: the per-buy logs (`logs/rival_buys_backfill.csv`, `logs/rival_buys.csv`). The engine's
//! floor is the p92.5 fee per CU over each class's last 50 slots; live it is taken over every
//! pump.fun buy (`hot::bids`), here over the buys on the recorded launches only, which leans toward
//! the contested coins. Tiers come from the tapes (dev buy, mayhem) and the follow list. No outcomes
//! of ours feed the controller here (the bot did not buy these), so it stays at the floor.

use {
    sniper_bot::{
        config::rules::Follow,
        hot::{
            clients::{Class, ALL, CLASSES},
            fee::{self, Engine, Mode, Route, Settings},
        },
        utils::reference,
    },
    solana_pubkey::Pubkey,
    std::{collections::HashMap, str::FromStr},
};

#[derive(Clone, Debug)]
struct Buy {
    mint: String,
    kind: String,
    create_slot: u64,
    slot: u64,
    slot_delta: u64,
    rank: Option<u32>,
    cu_price: u64,
    class: Class,
    jito_tip: Option<u64>,
}

fn class_of(name: &str) -> Class {
    ALL.iter().copied().find(|c| c.name() == name).unwrap_or(Class::Other)
}

fn q(mut v: Vec<u64>, p: f64) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[((v.len() - 1) as f64 * p).round() as usize])
}

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let files: Vec<String> = std::env::args().skip(1).collect();
    let files = if files.is_empty() { vec!["logs/rival_buys_backfill.csv".to_string(), "logs/rival_buys.csv".to_string()] } else { files };
    let mut buys = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        let mut lines = text.lines();
        let head: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
        let col = |n: &str| head.iter().position(|h| *h == n);
        let (Some(c_mint), Some(c_kind), Some(c_cslot), Some(c_slot), Some(c_delta), Some(c_rank), Some(c_price), Some(c_class), Some(c_sig), Some(c_status), Some(c_lane), Some(c_tip)) = (
            col("mint"),
            col("kind"),
            col("create_slot"),
            col("slot"),
            col("slot_delta"),
            col("rank"),
            col("cu_price"),
            col("leader_class"),
            col("signature"),
            col("status"),
            col("tip_lane"),
            col("tip_lamports"),
        ) else {
            continue;
        };
        for l in lines {
            let v: Vec<&str> = l.split(',').collect();
            if v.len() < head.len() || v[c_status] != "executed" || !seen.insert(v[c_sig].to_string()) {
                continue;
            }
            buys.push(Buy {
                mint: v[c_mint].into(),
                kind: v[c_kind].into(),
                create_slot: v[c_cslot].parse()?,
                slot: v[c_slot].parse()?,
                slot_delta: v[c_delta].parse()?,
                rank: v[c_rank].parse().ok(),
                cu_price: v[c_price].parse()?,
                class: class_of(v[c_class]),
                jito_tip: (v[c_lane] == "jito").then(|| v[c_tip].parse().ok()).flatten().filter(|t| *t > 0),
            });
        }
    }
    buys.sort_by_key(|b| (b.slot, b.create_slot));
    // Tiers: the tape's dev buy and mayhem flag, the follow list at launch time.
    reference::load();
    let mut tiers: HashMap<String, u8> = HashMap::new();
    for entry in std::fs::read_dir("logs/tapes")? {
        let text = std::fs::read_to_string(entry?.path())?;
        for line in text.lines() {
            let Ok(t) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let mint = t["mint"].as_str().unwrap_or_default().to_string();
            let dev = t["tape"].get(0).and_then(|x| x["sol"].as_u64()).unwrap_or(0);
            let at = t["ms"].as_u64().unwrap_or(0) / 1000;
            let keys: Vec<Pubkey> = [t["dep"].as_str(), t["creator"].as_str()].into_iter().flatten().filter_map(|k| Pubkey::from_str(k).ok()).collect();
            let follow = reference::follow(&keys, at).or(Some(Follow::Recent));
            tiers.insert(mint, fee::tier(dev, follow, t["mayhem"].as_bool().unwrap_or(false)));
        }
    }
    let settings = Settings { mode: Mode::Table, ..Settings::from_env() };
    let cu_limit: u32 = std::env::var("REPLAY_CU_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(112_700);
    let tip: u64 = 1_000_000;
    let mut engine = Engine::new(settings.clone());
    // Per class: (slot, fee per CU) of the buys seen so far.
    let mut history: Vec<Vec<(u64, u64)>> = vec![Vec::new(); CLASSES];
    let mut done = std::collections::HashSet::new();
    // Per class, the first buyers' fees seen so far (the cap's median).
    let mut firsts: Vec<Vec<u64>> = vec![Vec::new(); CLASSES];
    let mut out: Vec<(Class, u8, Option<u64>, Option<u64>, Option<u64>, Option<u64>)> = Vec::new();
    println!("| launch | kind | class | tier | route | floor (p92.5, 50 slots) | cap | chosen fee | Jito tip (SOL) | first buyer outside the bundle (create slot) | chosen ≥ first? |");
    println!("|---|---|---|---|---|---:|---:|---:|---:|---:|---|");
    let by_mint: HashMap<String, Vec<Buy>> = buys.iter().fold(HashMap::new(), |mut m, b| {
        m.entry(b.mint.clone()).or_default().push(b.clone());
        m
    });
    for b in &buys {
        // At a launch's first buy, decide its fee from the history up to its create.
        if done.insert(b.mint.clone()) {
            let class = by_mint[&b.mint].iter().find(|x| x.slot_delta == 0).map_or(b.class, |x| x.class);
            let h = &history[class as usize];
            let mut fees = Vec::new();
            let mut slots = 0;
            let mut last = u64::MAX;
            for (slot, fee) in h.iter().rev().filter(|(s, _)| *s < b.create_slot) {
                if *slot != last {
                    if slots == settings.floor_slots {
                        break;
                    }
                    slots += 1;
                    last = *slot;
                }
                fees.push(*fee);
            }
            let floor = q(fees, settings.floor_quantile);
            let tier = tiers.get(&b.mint).copied().unwrap_or(0);
            let f = &firsts[class as usize];
            engine.first_median[class as usize] = (f.len() >= 10).then(|| q(f[f.len().saturating_sub(50)..].to_vec(), 0.5)).flatten();
            let (fee_t, cap, floor_t) = engine.row(class, floor, cu_limit, tip, 1.0)[tier as usize];
            let route = settings.route[class as usize];
            let skipped = route == Route::Skip || (route == Route::FeeUnderCap && engine.first_fee(class) > cap);
            let chosen = match route {
                _ if skipped => None,
                Route::Jito => Some(floor_t),
                _ => Some(fee_t),
            };
            let jito = matches!(route, Route::Jito | Route::FeeJito).then(|| engine.jito_tip(None).0).filter(|_| !skipped);
            let first = by_mint[&b.mint].iter().find(|x| x.rank == Some(1) && x.slot_delta == 0).map(|x| x.cu_price);
            let contested = first.is_some();
            println!(
                "| {}… | {} | {} | {} | {:?} | {} | {:.1} | {} | {} | {} | {} |",
                &b.mint[..8],
                b.kind,
                class.name(),
                fee::tier_name(tier),
                route,
                floor.map_or("-".into(), |f| format!("{:.1}", f as f64 / 1e6)),
                cap as f64 / 1e6,
                chosen.map_or("skip".into(), |c| format!("{:.1}", c as f64 / 1e6)),
                jito.map_or("-".into(), |t| format!("{}", t as f64 / 1e9)),
                first.map_or("none".into(), |f| format!("{:.1}", f as f64 / 1e6)),
                match (first, chosen) {
                    (Some(f), Some(c)) => if c >= f { "yes" } else { "no" },
                    _ => "-",
                }
            );
            out.push((class, tier, chosen, first, floor, Some(cap)));
            engine.note_launch(tier, contested);
            if let Some(f) = first {
                engine.note_first_fee(class, f);
                firsts[class as usize].push(f);
            }
        }
        if let Some(t) = b.jito_tip.filter(|_| b.kind != "sample") {
            engine.note_rival_jito_tip(t);
        }
        history[b.class as usize].push((b.slot, b.cu_price));
    }
    println!("\nper class (lamports/CU; at {cu_limit} CU a lamport/CU costs {:.6} SOL):", cu_limit as f64 / 1e9);
    println!("| class | route | launches | skipped | chosen p50 | chosen p90 | cap at the end | first buyer p50 | chosen ≥ first buyer | at the cap |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for c in ALL {
        let v: Vec<&(Class, u8, Option<u64>, Option<u64>, Option<u64>, Option<u64>)> = out.iter().filter(|x| x.0 == c).collect();
        if v.is_empty() {
            continue;
        }
        let chosen: Vec<u64> = v.iter().filter_map(|x| x.2).collect();
        let first_fees: Vec<u64> = v.iter().filter_map(|x| x.3).collect();
        let beat = v.iter().filter(|x| matches!((x.2, x.3), (Some(c), Some(f)) if c >= f)).count();
        let with_both = v.iter().filter(|x| x.2.is_some() && x.3.is_some()).count();
        let capped = v.iter().filter(|x| matches!((x.2, x.5), (Some(c), Some(cap)) if c >= cap)).count();
        println!(
            "| {} | {:?} | {} | {} | {} | {} | {:.1} | {} | {} of {} | {} |",
            c.name(),
            settings.route[c as usize],
            v.len(),
            v.iter().filter(|x| x.2.is_none()).count(),
            q(chosen.clone(), 0.5).map_or("-".into(), |f| format!("{:.1}", f as f64 / 1e6)),
            q(chosen, 0.9).map_or("-".into(), |f| format!("{:.1}", f as f64 / 1e6)),
            engine.cap(c, cu_limit, tip) as f64 / 1e6,
            q(first_fees, 0.5).map_or("-".into(), |f| format!("{:.1}", f as f64 / 1e6)),
            beat,
            with_both,
            capped
        );
    }
    let (jito, p50, p75) = engine.jito_tip(None);
    println!(
        "\nsettings: cap = max(κ {} × {} SOL less a {} SOL tip, the class's median first-buyer fee), at least {} lamports/CU, at most {} SOL; Jito copy tip at the end {} SOL (rivals' p50 {:?} p75 {:?} lamports, no tip-floor stream in the replay)",
        settings.kappa,
        settings.buy_lamports as f64 / 1e9,
        tip as f64 / 1e9,
        settings.paladin_min as f64 / 1e6,
        settings.global_cap_lamports as f64 / 1e9,
        jito as f64 / 1e9,
        p50,
        p75
    );
    Ok(())
}
