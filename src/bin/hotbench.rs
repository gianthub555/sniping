//! D1: microbenchmarks of the hot path's stages on real pump.fun creates (the test fixtures),
//! next to the slow path's equivalents. Runs offline (no RPC, nothing sent): a seeded world, a
//! throwaway signing key, loopback sockets for the fan-out write.
//!
//!   hotbench [iterations, default 20000]

use {
    sniper_bot::{
        hot::{
            affinity, clock::now_ns, decide, decode, engine, spsc, stamps,
            template::{self, BuyFill, BuyTemplate, PACKET, PACKET_B64},
            testkit,
        },
        instructions::{pumpfun_buy_ix::PumpFunMarket, types::Market},
        processors::{shred_feed, shred_wire},
        utils::{blockhash, dashboard::now_unix},
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::Arc,
        time::Instant,
    },
};

fn pct(v: &mut [u64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    v.sort_unstable();
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize] as f64 / 1e3;
    (q(0.5), q(0.9), q(0.99))
}

fn row(name: &str, v: &mut [u64]) {
    let (a, b, c) = pct(v);
    println!("| {name:<44} | {a:>9.2} | {b:>9.2} | {c:>9.2} |");
}

fn time(n: usize, mut f: impl FnMut()) -> Vec<u64> {
    for _ in 0..n.min(1000) {
        f();
    }
    (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_nanos() as u64
        })
        .collect()
}

/// The frame with its mint replaced by `mint` everywhere it appears (a fresh coin for each run).
fn with_mint(frame: &[u8], old: &Pubkey, mint: &Pubkey) -> Vec<u8> {
    let mut f = frame.to_vec();
    let (o, m) = (old.to_bytes(), mint.to_bytes());
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

fn main() {
    dotenvy::dotenv().ok();
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(20_000);
    sniper_bot::hot::clock::init();
    stamps::init();
    let cores = affinity::core_count();
    // BENCH_CORES=a,b: the consumer/timing core and the producer core (keep them off a running bot's
    // hot cores, or the two time-share them).
    let bench: Vec<usize> = std::env::var("BENCH_CORES").unwrap_or_else(|_| "7,6".into()).split(',').filter_map(|c| c.trim().parse().ok()).collect();
    let (main_core, producer_core) = (*bench.first().unwrap_or(&7), *bench.get(1).unwrap_or(&6));
    affinity::pin_current_thread(main_core);
    let now = now_unix();
    testkit::seed(now);
    let frames = testkit::frames(1);
    let (name, frame) = frames.iter().find(|(name, _)| name.contains("legacy")).expect("the legacy create").clone();
    let tx_bytes = &frame[73..];
    let tx = decode::Tx::parse(tx_bytes).expect("parses");
    let mut launch = None;
    decode::launches(&tx, |l| launch = Some(l), |_| {});
    let launch = launch.expect("a launch");
    let keypair = Arc::new(Keypair::new());
    let wallet = keypair.pubkey();
    let derived = decide::derive(&launch, &wallet);
    println!("hot path microbenchmarks, {n} runs each, fixture `{name}`, {} cores, timing pinned to core {main_core}\n", cores);
    println!("| stage                                        |  p50 (us) |  p90 (us) |  p99 (us) |");
    println!("|----------------------------------------------|-----------|-----------|-----------|");

    // Decode
    row("decode: minimal (hot)", &mut time(n, || {
        let tx = decode::Tx::parse(tx_bytes).unwrap();
        decode::launches(&tx, |l| { std::hint::black_box(l); }, |_| {});
    }));
    row("decode: full parser (slow path)", &mut time(n, || {
        let tx = shred_wire::decode_transaction(tx_bytes).unwrap();
        std::hint::black_box(shred_feed::parse_launches(&tx.top_instructions()));
    }));

    // Handoff
    let (mut prod, mut cons) = spsc::channel::<u64>(1024);
    let producer = std::thread::spawn(move || {
        affinity::pin_current_thread(producer_core);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let t = now_ns();
            while !prod.push(t) {}
            // Let the consumer drain, so each push is timed alone.
            let until = now_ns() + 20_000;
            while now_ns() < until {
                std::hint::spin_loop();
            }
            out.push(());
        }
    });
    let mut handoff = Vec::with_capacity(n);
    while handoff.len() < n {
        if let Some(t) = cons.pop() {
            handoff.push(now_ns() - t);
        }
    }
    producer.join().unwrap();
    row("handoff: SPSC ring to a busy-polling thread", &mut handoff);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let mut spawned: Vec<u64> = rt.block_on(async {
        let mut v = Vec::with_capacity(n.min(5_000));
        for _ in 0..n.min(5_000) {
            let t = now_ns();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _ = tx.send(now_ns() - t);
            });
            v.push(rx.await.unwrap());
        }
        v
    });
    row("handoff: tokio::spawn (slow path)", &mut spawned);

    // The decision thread's hand-off while it warms up when idle (HOT_WARM_US): a producer on its
    // own core pushes events at random 0.2-2 ms gaps; the consumer polls the ring and, when idle,
    // warms up in one go (as before) or one step per look (HOT_WARM_STEPS).
    for steps in [false, true] {
        let mut d = engine::bench_decider(Arc::new(Keypair::new()), false, stamps::Mode::Spsc).unwrap();
        d.set_warm_steps(steps);
        let events = n.min(4_000);
        let (mut prod, mut cons) = spsc::channel::<u64>(1024);
        let producer = std::thread::spawn(move || {
            affinity::pin_current_thread(producer_core);
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
            for _ in 0..events {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let until = now_ns() + 200_000 + x % 1_800_000;
                while now_ns() < until {
                    std::hint::spin_loop();
                }
                let t = now_ns();
                while !prod.push(t) {}
            }
        });
        let mut waits = Vec::with_capacity(events);
        while waits.len() < events {
            if let Some(t) = cons.pop() {
                waits.push(now_ns() - t);
            } else if !d.warm(now_ns()) {
                std::hint::spin_loop();
            }
        }
        producer.join().unwrap();
        row(if steps { "handoff: busy ring, warm-ups in steps (HOT_WARM_STEPS)" } else { "handoff: busy ring, warm-ups in one go (before)" }, &mut waits);
    }

    // Derive and decide
    row("derive: 3-4 addresses, allocation-free (hot)", &mut time(n, || {
        std::hint::black_box(decide::derive(&launch, &wallet));
    }));
    // The creator vault (HOT_VAULT_CACHE, 2026-10-11): from the table for a repeat creator.
    let creator = derived.creator;
    row("creator vault: find_program_address (cache off)", &mut time(n, || {
        std::hint::black_box(sniper_bot::hot::pda::find_program_address(&[b"creator-vault", creator.as_ref()], &sniper_bot::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID));
    }));
    row("creator vault: from the table (HOT_VAULT_CACHE)", &mut time(n, || {
        std::hint::black_box(decide::creator_vault_of(&creator));
    }));
    row("derive: library find_program_address", &mut time(n, || {
        let m = PumpFunMarket::new(launch.mint, launch.token_program, launch.named_creator, false, Default::default());
        std::hint::black_box(m.buy_instructions(&wallet, 1, 1).unwrap());
    }));
    let mut outcome = None;
    row("decide: entry rules + checks + quote", &mut time(n, || {
        outcome = Some(decide::decide(&launch, &derived, 1, &[], now));
    }));
    println!("|   (decided: {:<32}) |           |           |           |", format!("{:?}", outcome.map(|o| matches!(o, decide::Outcome::Buy(..)))).replace("Some(true)", "buy").replace("Some(false)", "no buy"));
    // HOT_PREDECIDE (2026-10-11): the lookups when the create arrives, inside the window; at the
    // window's end only the arithmetic.
    row("decide: lookups at arrival (in the window)", &mut time(n, || {
        std::hint::black_box(decide::prepare(&launch, &derived, 1, now));
    }));
    let pre = decide::prepare(&launch, &derived, 1, now);
    row("decide: at the window's end, prepared", &mut time(n, || {
        outcome = Some(decide::decide_prepared(&launch, &derived, 1, &[], now, &pre));
    }));

    // Build, sign, serialize
    let mut tpl = BuyTemplate::build(template::spec_from_settings(wallet)).expect("template");
    let (bh, _, _) = blockhash::current().expect("seeded blockhash");
    let fill = BuyFill {
        mint: launch.mint,
        token_program: launch.token_program,
        bonding_curve: derived.bonding_curve,
        associated_bonding_curve: derived.associated_bonding_curve,
        user_ata: derived.user_ata,
        creator_vault: derived.creator_vault,
        bonding_curve_v2: derived.bonding_curve_v2,
        fee_recipient: Pubkey::new_unique(),
        buyback_fee_recipient: Pubkey::new_unique(),
        lamports_in: 100_000_000,
        min_out: 1,
        cu_price: 0,
        blockhash: bh,
        tip_pick: 1,
        paid: None,
        cu_limit: 0,
    };
    let mut out = [0u8; PACKET];
    let mut b64 = [0u8; PACKET_B64];
    let mut len = 0;
    row("build+sign: patch template, ed25519 (hot)", &mut time(n, || {
        tpl.fill(&fill);
        len = tpl.sign_into(&keypair, &mut out);
    }));
    // HOT_PRESIGN (2026-10-11): the signature finished from a nonce made ahead of time.
    let hot_key = Arc::new(sniper_bot::hot::presign::HotKey::of(&keypair));
    let (mut pre, mut refill) = sniper_bot::hot::presign::pair(hot_key, (n * 2).next_power_of_two());
    refill.fill();
    row("build+sign: patch template, presigned nonce (hot)", &mut time(n, || {
        tpl.fill(&fill);
        len = tpl.sign_into_with(|m| pre.sign(m), &mut out);
    }));
    // HOT_NONCE_FANOUT (2026-10-12): one lane's copy on a durable nonce, as the hot thread makes
    // each (fill with only that lane's tip, the nonce account, presigned, base64).
    let mut nonced = BuyTemplate::build_nonced(template::spec_from_settings(wallet)).expect("nonced template");
    let nonce_account = Pubkey::new_unique();
    row("fan-out buy: one nonced copy, presigned + base64 (hot)", &mut time(n, || {
        nonced.fill(&BuyFill { paid: Some([0; template::MAX_TIPS]), ..fill });
        nonced.set_nonce_account(&nonce_account);
        len = nonced.sign_into_with(|m| pre.sign(m), &mut out);
        std::hint::black_box(template::base64_into(&out[..len], &mut b64));
    }));
    // HOT_SELL_FANOUT: one lane's sell copy, built on the slow path (a message, presigned).
    let sell_ixs = PumpFunMarket::new(launch.mint, launch.token_program, launch.named_creator, false, Default::default()).sell_instructions(&wallet, 1_000_000, 1).expect("sell");
    let sell_tips: &'static [Pubkey] = Box::leak(vec![Pubkey::new_unique()].into_boxed_slice());
    let (nonce_value, mint_key) = (solana_hash::Hash::new_from_array([3; 32]), launch.mint);
    row("fan-out sell: one copy built + presigned (slow path)", &mut time(n.min(5_000), || {
        let mut sign = |m: &[u8]| pre.sign(m);
        std::hint::black_box(sniper_bot::hot::sell::fanout_copy(&mint_key, &sell_ixs, &wallet, 0, Some((sell_tips, 1_000_000)), 0, (&nonce_account, &nonce_value), 100_000, 90_000, &mut sign).unwrap());
    }));
    row("serialize: base64 into a stack buffer (hot)", &mut time(n, || {
        std::hint::black_box(template::base64_into(&out[..len], &mut b64));
    }));
    let market = Market::PumpFun(PumpFunMarket::new(launch.mint, launch.token_program, launch.named_creator, false, Default::default()));
    row("build+sign+serialize: slow path builder", &mut time(n.min(5_000), || {
        let ixs = market.buy_instructions(&sniper_bot::config::PAYER.pubkey(), 100_000_000, 1).unwrap();
        let tx = sniper_bot::utils::build_and_sign::build_and_sign_with_fee(ixs, vec![], 0).unwrap();
        std::hint::black_box(sniper_bot::utils::build_and_sign::serialize_base64(&tx).unwrap());
    }));

    // Fan-out write: one request (headers + JSON body around the base64) to 4 warm loopback sockets.
    let m = template::base64_into(&out[..len], &mut b64);
    let mut request = Vec::new();
    request.extend_from_slice(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: ");
    let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"sendTransaction\",\"params\":[\"{}\",{{\"encoding\":\"base64\",\"skipPreflight\":true,\"maxRetries\":0}}]}}", std::str::from_utf8(&b64[..m]).unwrap());
    request.extend_from_slice(format!("{}\r\n\r\n{body}", body.len()).as_bytes());
    let mut lanes = Vec::new();
    for _ in 0..4 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut sink = [0u8; 65_536];
            while s.read(&mut sink).map(|r| r > 0).unwrap_or(false) {}
        });
        let s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        lanes.push(s);
    }
    let mut spread = Vec::with_capacity(n);
    let mut each = Vec::with_capacity(4 * n);
    for _ in 0..n.min(10_000) {
        let start = now_ns();
        let mut last = start;
        for s in lanes.iter_mut() {
            s.write_all(&request).unwrap();
            let t = now_ns();
            each.push(t - last);
            last = t;
        }
        spread.push(last - start);
    }
    row(&format!("fan-out: one {}-byte write to a warm socket", request.len()), &mut each);
    row("fan-out: all 4 lanes written (first to last)", &mut spread);

    // The whole hot path, inline: frame in to bytes ready, bundle window excluded.
    let mut decider = engine::bench_decider(keypair.clone(), false, stamps::Mode::Inline).unwrap();
    let mut recent = engine::RecentSigs::new();
    let mut head = 0u64;
    let mut drained = Vec::new();
    stamps::drain(&mut head, &mut drained);
    drained.clear();
    for i in 0..n.min(3_000) {
        let mint = Pubkey::new_unique();
        let mut f = with_mint(&frame, &launch.mint, &mint);
        f[9..17].copy_from_slice(&(i as u64).to_le_bytes());
        testkit::seed_follow(&f);
        engine::bench_frame(&mut decider, &mut recent, &f, now_ns());
        engine::bench_settle(&mut decider);
    }
    stamps::drain(&mut head, &mut drained);
    let stage = |name: &str| -> Vec<u64> { drained.iter().filter_map(|s| (stamps::STAGES.iter().find(|st| st.name == name).unwrap().of)(s)).collect() };
    let bought = drained.iter().filter(|s| s.flags & stamps::FLAG_BUY != 0).count();
    println!("|   (end to end: {} frames, {bought} decided to buy)          |           |           |           |", drained.len());
    for name in ["decode", "derive", "late_start", "decide", "build_sign", "serialize", "total_excl_wait"] {
        row(&format!("end to end, inline: {name}"), &mut stage(name));
    }
}
