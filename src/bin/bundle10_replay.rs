//! bundle10 on a recorded tape (`logs/tapes/<UTC day>.jsonl`: the regular stream's executed trades
//! of every launch's first 120 s, with block positions). Offline: nothing is sent, no RPC.
//!
//! 1. Counts and the simulated trade, launch by launch: the exact count (block index), the count
//!    without the index (what an early feed can see of the same executed trades: gaps invisible),
//!    the rule both ways, and the 60 s simulated 0.1 SOL trade per match (`bundle10_shadow`'s
//!    code). The tape has neither fee bps nor the fee engine's state: the research's cost model
//!    (125 bps both ways, 0.0011 SOL a transaction). Phantoms cannot show on executed trades.
//! 2. `--latency N`: the hot path's decision latency with the shadow off and on, on N launches
//!    made into preprocessed frames (the fixture create with the launch's coin, its bundle and
//!    next transactions with their real buyers and amounts), the two deciders taking turns.
//!
//!   bundle10_replay [tape] [--latency N] [--core C]

use {
    serde::Deserialize,
    sniper_bot::{
        config::bundle10::Settings,
        hot::{
            affinity,
            bundle10::{self as b10, Count, Facts, Miss, Rec, Trade},
            clock::now_ns,
            decode, engine, spsc, stamps, testkit,
        },
        processors::bundle10_shadow::{exact, simulate, Clock, Created, Ev, Fees, RESEARCH_TX_LAMPORTS, SIZE_LAMPORTS},
        utils::dashboard::now_unix,
    },
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        collections::HashMap,
        io::{BufRead, BufReader},
        str::FromStr,
        sync::{atomic::Ordering, Arc},
    },
};

#[derive(Deserialize)]
struct Line {
    mint: String,
    slot: u64,
    idx: u64,
    ms: u64,
    dep: String,
    creator: String,
    mayhem: bool,
    tape: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    slot: u64,
    idx: u64,
    k: u32,
    ms: u64,
    is_buy: bool,
    sol: u64,
    tok: u64,
    user: String,
    vsol: u64,
    vtok: u64,
}

/// pump.fun's standard curve: virtual SOL x virtual tokens.
const K: f64 = 30e9 * 1.073e15;
const VIRTUAL_NOT_REAL_TOKENS: u64 = 279_900_000_000_000;
const FEE_BPS: u64 = 125;

struct Launch {
    mint: Pubkey,
    created: Created,
    events: Vec<Ev>,
    count_exact: u32,
    count_early: u32,
    pass_exact: Result<(), Miss>,
    pass_early: Result<(), Miss>,
    bundle_sol: f64,
    post_mult: f64,
}

fn sig_of(slot: u64, idx: u64) -> [u8; 8] {
    (slot.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ idx.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)).to_le_bytes()
}

fn trade_of(mint: &Pubkey, e: &Ev) -> Trade {
    Trade { mint: *mint, buy: e.buy, user: Some(e.user), tokens: e.tok, lamports: e.sol }
}

fn main() {
    // The bot's settings (the decider's trade settings; nothing here is sent).
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = "logs/tapes/2026-10-13.jsonl".to_string();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += 2;
        } else {
            path = args[i].clone();
            i += 1;
        }
    }
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<usize>().ok());
    let latency_n = opt("--latency");
    let core = opt("--core").unwrap_or(1);
    let s = Settings::DEFAULT;

    // ---- 1. Counts and the simulated trade.
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut marks: Vec<(u64, u64)> = Vec::new();
    let (mut lines, mut bad, mut standard, mut mayhem, mut no_idx) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut kept: Vec<Launch> = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let Ok(l) = serde_json::from_str::<Line>(&line) else {
            bad += 1;
            continue;
        };
        lines += 1;
        for r in &l.tape {
            marks.push((r.ms, r.slot));
        }
        if l.mayhem {
            mayhem += 1;
            continue;
        }
        let (Ok(mint), Ok(dev), Ok(creator)) = (Pubkey::from_str(&l.mint), Pubkey::from_str(&l.dep), Pubkey::from_str(&l.creator)) else { continue };
        if l.idx == u64::MAX || l.tape.iter().any(|r| r.idx == u64::MAX) {
            no_idx += 1;
            continue;
        }
        // A standard SOL curve: the dev buy's state on the 30 x 1,073M product.
        let Some(first) = l.tape.iter().min_by_key(|r| (r.slot, r.idx, r.k)) else { continue };
        if ((first.vsol as f64 * first.vtok as f64) / K - 1.0).abs() > 1e-6 {
            continue;
        }
        standard += 1;
        let mut events: Vec<Ev> = l
            .tape
            .iter()
            .filter_map(|r| {
                Some(Ev {
                    slot: r.slot,
                    index: u32::try_from(r.idx).ok()?,
                    k: r.k as u16,
                    ms: r.ms,
                    sig: sig_of(r.slot, r.idx),
                    buy: r.is_buy,
                    user: Pubkey::from_str(&r.user).ok()?,
                    sol: r.sol,
                    tok: r.tok,
                    vsol: r.vsol,
                    vtok: r.vtok,
                    real_sol: r.vsol.saturating_sub(b10::INITIAL_VSOL),
                    real_tok: r.vtok.saturating_sub(VIRTUAL_NOT_REAL_TOKENS),
                    fee_bps: FEE_BPS,
                })
            })
            .collect();
        events.sort_by_key(|e| (e.slot, e.index, e.k));
        let created = Created { slot: l.slot, index: l.idx as u32, ms: l.ms, dev, creator, sol_quote: true, mayhem: false, cashback_event: false, cashback_ix: None };
        let Some(x) = exact(&mint, &created, &events) else { continue };
        // Without the index: the coin's transactions of the create slot after the create, in
        // block order (an early feed's arrival order at best), gaps unseen.
        let mut early = Count::new(mint, dev, creator, None);
        let mut early_last = None;
        for (i, e) in events.iter().enumerate().filter(|(_, e)| e.slot == created.slot && e.index > created.index) {
            if early.event(None, e.sig, e.k, &trade_of(&mint, e)) {
                early_last = Some(i);
            }
        }
        if x.count < b10::SHADOW_MIN_COUNT && early.wallets() < b10::SHADOW_MIN_COUNT {
            continue;
        }
        let facts = |count: u32, bundle_lamports: u64, post_mult: f64, overflow: bool| Facts {
            layout_known: true,
            sol_quote: true,
            initial: (b10::INITIAL_VSOL, b10::INITIAL_VTOK),
            mayhem: false,
            cashback: false,
            dev_buy_lamports: x.dev_buy_lamports,
            count,
            overflow,
            bundle_lamports,
            post_bundle_mult: post_mult,
        };
        let e0_early = early_last.map_or(x.e0, |i| i);
        let early_bundle = events[e0_early].real_sol.saturating_sub(x.dev_buy_lamports);
        let early_mult = events[e0_early].price() / b10::launch_price();
        kept.push(Launch {
            mint,
            created,
            count_exact: x.count,
            count_early: early.wallets(),
            pass_exact: b10::rule(&facts(x.count, x.bundle_lamports, x.post_mult, x.overflow), &s),
            pass_early: b10::rule(&facts(early.wallets(), early_bundle, early_mult, early.overflow()), &s),
            bundle_sol: x.bundle_lamports as f64 / 1e9,
            post_mult: x.post_mult,
            events,
        });
    }
    let clock = Clock::from_marks(marks);
    println!("tape {path}: {lines} launches ({bad} unreadable lines skipped), {mayhem} mayhem, {no_idx} without block positions, {standard} standard SOL curves\n");
    let ge = |f: &dyn Fn(&Launch) -> bool| kept.iter().filter(|l| f(l)).count();
    let (both, exact_only, early_only) = (
        ge(&|l| l.count_exact >= 10 && l.count_early >= 10),
        ge(&|l| l.count_exact >= 10 && l.count_early < 10),
        ge(&|l| l.count_exact < 10 && l.count_early >= 10),
    );
    println!("count >= 10: exact {}, without the index {}; both {both}, exact only {exact_only}, without the index only {early_only} (agree {:.1}%)", both + exact_only, both + early_only, 100.0 * both as f64 / (both + exact_only + early_only).max(1) as f64);
    let passes_e = ge(&|l| l.pass_exact.is_ok());
    let passes_n = ge(&|l| l.pass_early.is_ok());
    let both_pass = ge(&|l| l.pass_exact.is_ok() && l.pass_early.is_ok());
    println!("would pass the rule: exact {passes_e}, without the index {passes_n}, both {both_pass}");
    let mut misses: HashMap<&str, usize> = HashMap::new();
    for l in kept.iter().filter(|l| l.count_exact >= 10) {
        *misses.entry(l.pass_exact.err().map_or("pass", Miss::name)).or_default() += 1;
    }
    let mut misses: Vec<_> = misses.into_iter().collect();
    misses.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("exact count >= 10, by the rule's verdict: {misses:?}");
    let blocked = ge(&|l| l.pass_exact.is_ok() && b10::blocked_by_15sol_cap((l.bundle_sol * 1e9) as u64));
    println!("of the exact passes, over the 15 SOL cap other paths keep: {blocked} ({:.0}%)", 100.0 * blocked as f64 / passes_e.max(1) as f64);
    println!("phantoms: none can show (the tape holds executed transactions only; the live shadow checks each counted transaction)\n");

    println!("the 60 s simulated 0.1 SOL trade per exact match (entry e2, trail 15% / 60 s, the sell at the worst state of the slot after; research costs: 125 bps both ways, 0.0011 SOL a transaction):\n");
    println!("{:<13} {:>8} {:>5} {:>5} {:>7} {:>6} {:>3} {:<11} {:>8}", "coin", "UTC", "exact", "early", "bundle", "x", "e2", "exit", "mSOL");
    let mut pnl = Vec::new();
    let mut e2s = [0usize; 3];
    let mut by_exit: HashMap<&str, (usize, f64)> = HashMap::new();
    for l in kept.iter().filter(|l| l.pass_exact.is_ok()) {
        let Some(x) = exact(&l.mint, &l.created, &l.events) else { continue };
        // The tape records each launch's first 120 s.
        let sim = simulate(&l.events, x.e0, l.created.slot, l.created.ms, s.max_hold_ms, s.trail, SIZE_LAMPORTS, Fees::Fixed(FEE_BPS), &clock, l.created.ms + 120_000);
        let m = (sim.proceeds as f64 - SIZE_LAMPORTS as f64 - 2.0 * RESEARCH_TX_LAMPORTS as f64) / 1e6;
        pnl.push(m);
        let e = by_exit.entry(sim.exit.name()).or_default();
        e.0 += 1;
        e.1 += m;
        let t = l.created.ms / 1000 % 86_400;
        e2s[sim.e2_from as usize] += 1;
        println!(
            "{:<13} {:02}:{:02}:{:02} {:>5} {:>5} {:>7.2} {:>6.2} {:>3} {:<11} {:>+8.2}",
            &l.mint.to_string()[..13],
            t / 3600,
            t / 60 % 60,
            t % 60,
            l.count_exact,
            l.count_early,
            l.bundle_sol,
            l.post_mult,
            sim.e2_from,
            sim.exit.name(),
            m
        );
    }
    if !pnl.is_empty() {
        let n = pnl.len() as f64;
        let mean = pnl.iter().sum::<f64>() / n;
        let sd = (pnl.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0).max(1.0)).sqrt();
        let h = 1.96 * sd / n.sqrt();
        println!(
            "\n{} matches: mean {:+.2} mSOL a trade [{:+.2}, {:+.2}], total {:+.4} SOL, {:.0}% positive",
            pnl.len(),
            mean,
            mean - h,
            mean + h,
            pnl.iter().sum::<f64>() / 1e3,
            100.0 * pnl.iter().filter(|x| **x > 0.0).count() as f64 / n
        );
        let mut exits: Vec<_> = by_exit.into_iter().collect();
        exits.sort_by_key(|(name, _)| *name);
        for (name, (k, sum)) in exits {
            println!("  exit {name:<11} {k:>4} trades, mean {:+.2} mSOL", sum / k as f64);
        }
        println!("  entry e2 from: the post-bundle state {}, the next trade {}, the one after {}", e2s[0], e2s[1], e2s[2]);
    }

    // ---- 2. Latency with the shadow off and on.
    let Some(n) = latency_n else { return };
    latency(&kept, n, core);
}

/// A transaction of pump.fun trades (the tape's), signature `sig`.
fn trade_tx(mint: &Pubkey, events: &[&Ev], sig: [u8; 8]) -> Vec<u8> {
    let ixs: Vec<Instruction> = events
        .iter()
        .map(|e| {
            let mut accounts: Vec<AccountMeta> = (0..16).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect();
            accounts[2] = AccountMeta::new_readonly(*mint, false);
            accounts[6] = AccountMeta::new(e.user, false);
            let mut data = if e.buy { decode::BUY_EXACT_SOL_IN.to_vec() } else { decode::SELL.to_vec() };
            data.extend_from_slice(&(if e.buy { e.sol } else { e.tok }).to_le_bytes());
            data.extend_from_slice(&1u64.to_le_bytes());
            if e.buy {
                data.push(1);
            }
            Instruction { program_id: sniper_bot::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID, accounts, data }
        })
        .collect();
    let msg = Message::new_with_blockhash(&ixs, Some(&events[0].user), &solana_hash::Hash::new_from_array([5; 32]));
    let mut s = [0u8; 64];
    s[..8].copy_from_slice(&sig);
    bincode::serialize(&VersionedTransaction { signatures: vec![solana_signature::Signature::from(s)], message: VersionedMessage::Legacy(msg) }).unwrap()
}

/// The frame with `old` replaced by `new` everywhere (a fresh coin for each run).
fn with_mint(frame: &[u8], old: &Pubkey, new: &Pubkey) -> Vec<u8> {
    let mut f = frame.to_vec();
    let (o, m) = (old.to_bytes(), new.to_bytes());
    let mut i = 0;
    while i + 32 <= f.len() {
        if f[i..i + 32] == o {
            f[i..i + 32].copy_from_slice(&m);
            i += 32;
        } else {
            i += 1;
        }
    }
    f
}

fn pct(v: &mut [u64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    v.sort_unstable();
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize] as f64 / 1e3;
    (q(0.5), q(0.9), q(0.99))
}

fn latency(kept: &[Launch], n: usize, core: usize) {
    affinity::pin_current_thread(core);
    sniper_bot::hot::clock::init();
    stamps::init();
    testkit::seed(now_unix());
    let fixtures = testkit::frames(1);
    let (_, create) = fixtures.iter().find(|(name, _)| name.contains("legacy")).expect("the legacy create").clone();
    let mut fixture_mint = None;
    decode::launches(&decode::Tx::parse(&create[73..]).unwrap(), |l| fixture_mint = Some(l.mint), |_| {});
    let fixture_mint = fixture_mint.unwrap();
    // The launches with a bundle: the create, its counted transactions and the slot's next two.
    let launches: Vec<&Launch> = kept.iter().filter(|l| l.count_exact >= 1).cycle().take(n).collect();
    let (p_on, mut c_on) = spsc::channel::<Rec>(1 << 12);
    let mut off = engine::bench_decider(Arc::new(Keypair::new()), false, stamps::Mode::Inline).expect("decider");
    let mut on = engine::bench_decider(Arc::new(Keypair::new()), false, stamps::Mode::Inline).expect("decider").with_bundle10(p_on, 1_000_000);
    let (mut recent_off, mut recent_on) = (engine::RecentSigs::new(), engine::RecentSigs::new());
    let mut head = 0u64;
    let mut drained = Vec::new();
    stamps::drain(&mut head, &mut drained);
    drained.clear();
    // Per run: frame handling (each bundle frame), the stamps (by mint), the window end to the decider done.
    let mut frame_ns: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
    let mut settle_ns: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
    let mut stamp_of: [HashMap<[u8; 32], stamps::Stamp>; 2] = [HashMap::new(), HashMap::new()];
    let mut seq: u64 = 0;
    let mut recs = 0usize;
    // Each round's coin per run, to compare the two decisions launch by launch.
    let mut coins: Vec<[[u8; 32]; 2]> = Vec::new();
    for (round, l) in launches.iter().enumerate() {
        let cr = &l.created;
        let after: Vec<&Ev> = l.events.iter().filter(|e| e.slot == cr.slot && e.index > cr.index).collect();
        let mut by_tx: Vec<Vec<&Ev>> = Vec::new();
        for e in after {
            match by_tx.last_mut() {
                Some(t) if t[0].index == e.index => t.push(e),
                _ => by_tx.push(vec![e]),
            }
        }
        by_tx.truncate(6);
        let slot = (round % 8) as u64;
        // Each run its own coin (a coin is decided once), the two deciders in turn.
        let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
        coins.push([[0; 32]; 2]);
        for run in order {
            let mint = Pubkey::new_unique();
            coins[round][run] = mint.to_bytes();
            let mut frames = Vec::new();
            let mut c = with_mint(&create, &fixture_mint, &mint);
            c[1..9].copy_from_slice(&slot.to_le_bytes());
            frames.push(c);
            for tx in &by_tx {
                seq += 1;
                let sig = (seq | 0xB10 << 48).to_le_bytes();
                let bytes = trade_tx(&mint, tx, sig);
                let mut f = vec![0u8];
                f.extend(slot.to_le_bytes());
                f.extend_from_slice(&bytes[1..65]);
                f.extend_from_slice(&bytes);
                frames.push(f);
            }
            seq += 1;
            frames[0][9..17].copy_from_slice(&(seq | 0xC0 << 48).to_le_bytes());
            testkit::seed_follow(&frames[0]);
            b10::FEED.store(run == 1, Ordering::Relaxed);
            let (d, recent) = if run == 0 { (&mut off, &mut recent_off) } else { (&mut on, &mut recent_on) };
            let t0 = now_ns();
            for (i, f) in frames.iter().enumerate() {
                let t = now_ns();
                engine::bench_frame(d, recent, f, t0 + i as u64 * 2_000);
                if i > 0 {
                    frame_ns[run].push(now_ns() - t);
                }
            }
            let deadline = d.next_deadline().unwrap_or(0);
            engine::bench_settle(d);
            settle_ns[run].push(now_ns().saturating_sub(deadline));
            stamps::drain(&mut head, &mut drained);
            for st in drained.drain(..) {
                stamp_of[run].insert(st.mint, st);
            }
            while c_on.pop().is_some() {
                recs += 1;
            }
        }
    }
    b10::FEED.store(false, Ordering::Relaxed);
    println!("\nhot path latency, {n} launches from the tape (the fixture create with each launch's coin, its bundle and next transactions with their real buyers), the deciders taking turns, pinned to core {core}; {recs} shadow records\n");
    println!("| stage (us)                                   | off p50 | off p99 |  on p50 |  on p99 | p99 diff |");
    println!("|----------------------------------------------|---------|---------|---------|---------|----------|");
    let row = |name: &str, off: &mut Vec<u64>, on: &mut Vec<u64>| {
        let (a50, _, a99) = pct(off);
        let (b50, _, b99) = pct(on);
        println!("| {name:<44} | {a50:>7.2} | {a99:>7.2} | {b50:>7.2} | {b99:>7.2} | {:>+8.2} |", b99 - a99);
    };
    for st in ["decide", "build_sign", "serialize", "total_excl_wait", "late_start"] {
        let stage = stamps::STAGES.iter().find(|s| s.name == st).unwrap();
        let mut v: [Vec<u64>; 2] = [stamp_of[0].values().filter_map(|s| (stage.of)(s)).collect(), stamp_of[1].values().filter_map(|s| (stage.of)(s)).collect()];
        let [a, b] = &mut v;
        row(&format!("existing decision: {st}"), a, b);
    }
    let bought = |run: usize| stamp_of[run].values().filter(|s| s.flags & stamps::FLAG_BUY != 0).count();
    let [a, b] = &mut frame_ns;
    row("feed: one bundle frame handled", a, b);
    let [a, b] = &mut settle_ns;
    row("window end to the decider done (incl. shadow)", a, b);
    println!("\n(decided: off {} launches, {} bought; on {} launches, {} bought)", stamp_of[0].len(), bought(0), stamp_of[1].len(), bought(1));
    // Launch by launch: the same decision with the shadow off and on?
    let verdict = |run: usize, m: &[u8; 32]| stamp_of[run].get(m).map(|s| (s.reject, s.flags & stamps::FLAG_BUY != 0));
    let differ: Vec<(usize, Option<(u8, bool)>, Option<(u8, bool)>)> =
        coins.iter().enumerate().map(|(r, c)| (r, verdict(0, &c[0]), verdict(1, &c[1]))).filter(|(_, a, b)| a != b).collect();
    println!("launches decided differently with the shadow off and on: {}", differ.len());
    for (r, a, b) in differ.iter().take(10) {
        println!("  round {r}, slot {} ({} first): off {a:?}, on {b:?}", r % 8, if r % 2 == 0 { "off" } else { "on" });
    }
}
