//! A8: the create-to-signed-bytes path makes no heap allocation. A counting allocator (the test
//! build's global allocator) counts allocations on the current thread while a frame goes through
//! decode, derive, decide, build, sign and base64.

use {
    super::{clock::now_ns, engine, stamps, testkit},
    std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        sync::Arc,
    },
};

struct Counting;

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ON.try_with(|on| {
        if on.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations `f` makes on this thread.
pub fn allocations<R>(f: impl FnOnce() -> R) -> (R, usize) {
    COUNT.with(|c| c.set(0));
    ON.with(|o| o.set(true));
    let r = f();
    ON.with(|o| o.set(false));
    (r, COUNT.with(|c| c.get()))
}

#[test]
fn the_create_to_signed_path_makes_no_heap_allocation() {
    let _state = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    super::clock::init();
    stamps::init();
    let now = crate::utils::dashboard::now_unix();
    testkit::seed(now);
    let frames = testkit::frames(1);
    let keypair = Arc::new(solana_keypair::Keypair::new());
    let mut decider = engine::bench_decider(keypair, true, stamps::Mode::Inline).expect("template");
    // Other tests feed the same fixture coins: each counts as new here.
    engine::forget_seen();
    let mut recent = engine::RecentSigs::new();
    let mut head = 0u64;
    let mut drained = Vec::new();
    stamps::drain(&mut head, &mut drained);
    drained.clear();
    // The first frame warms every lazily built table (a different coin: each is decided once).
    engine::bench_frame(&mut decider, &mut recent, &frames[0].1, now_ns());
    engine::bench_settle(&mut decider);
    for (name, frame) in &frames[1..] {
        // The frame, the bundle window and the decision, build, signature and base64 after it.
        let ((), n) = allocations(|| {
            engine::bench_frame(&mut decider, &mut recent, frame, now_ns());
            engine::bench_settle(&mut decider);
        });
        assert_eq!(n, 0, "{name}: {n} heap allocations on the hot path");
    }
    stamps::drain(&mut head, &mut drained);
    let signed = drained.iter().filter(|s| s.t_ready > 0).count();
    let bought = drained.iter().filter(|s| s.flags & stamps::FLAG_BUY != 0).count();
    assert!(signed >= frames.len() - 1, "every measured frame was built and signed ({signed})");
    assert!(bought >= 1, "at least one fixture went the whole way to a buy ({bought}); rejects: {:?}", drained.iter().map(|s| s.reject).collect::<Vec<_>>());
}

#[test]
fn each_stage_alone_makes_no_heap_allocation() {
    use super::{decide, decode, template, verify};
    let _state = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    super::clock::init();
    stamps::init();
    let now = crate::utils::dashboard::now_unix();
    testkit::seed(now);
    let frames = testkit::frames(1);
    let keypair = solana_keypair::Keypair::new();
    let mut tpl = template::BuyTemplate::build(template::spec_from_settings(solana_signer::Signer::pubkey(&keypair))).unwrap();
    let mut report = Vec::new();
    for (round, (name, frame)) in frames.iter().enumerate() {
        let bytes = &frame[73..];
        let (tx, a_parse) = allocations(|| decode::Tx::parse(bytes).unwrap());
        let (launch, a_launch) = allocations(|| {
            let mut out = None;
            decode::launches(&tx, |l| out = Some(l), |_| {});
            out
        });
        let Some(launch) = launch else { continue };
        let (derived, a_derive) = allocations(|| decide::derive(&launch, &solana_signer::Signer::pubkey(&keypair)));
        let (outcome, a_decide) = allocations(|| decide::decide(&launch, &derived, 1, &[], now));
        let (_, a_verify) = allocations(|| verify::note_fast(&launch.mint, None, false));
        let (bh, a_bh) = allocations(crate::utils::blockhash::current);
        let fill = template::BuyFill {
            mint: launch.mint,
            token_program: launch.token_program,
            bonding_curve: derived.bonding_curve,
            associated_bonding_curve: derived.associated_bonding_curve,
            user_ata: derived.user_ata,
            creator_vault: derived.creator_vault,
            bonding_curve_v2: derived.bonding_curve_v2,
            fee_recipient: derived.creator,
            buyback_fee_recipient: derived.creator_vault,
            lamports_in: 1,
            min_out: 1,
            cu_price: 0,
            blockhash: bh.unwrap().0,
            tip_pick: 3,
            paid: None,
            cu_limit: 0,
        };
        let (_, a_fill) = allocations(|| tpl.fill(&fill));
        let mut out = [0u8; template::PACKET];
        let (n, a_sign) = allocations(|| tpl.sign_into(&keypair, &mut out));
        let mut b64 = [0u8; template::PACKET_B64];
        let (_, a_b64) = allocations(|| template::base64_into(&out[..n], &mut b64));
        let (_, a_stamp) = allocations(|| stamps::record(&stamps::Stamp::default()));
        let (_, a_cfg) = allocations(|| crate::instructions::pumpfun_buy_ix::pumpfun_config().map(|c| c.fee_recipients.len()));
        report.push(format!(
            "round {round} {name}: parse {a_parse} launch {a_launch} derive {a_derive} decide {a_decide} ({:?}) verify {a_verify} blockhash {a_bh} fill {a_fill} sign {a_sign} base64 {a_b64} stamp {a_stamp} config {a_cfg}",
            outcome.reject_code()
        ));
    }
    let text = report.join("\n");
    println!("{text}");
    let later_rounds: usize = report.iter().skip(1).map(|l| l.split_whitespace().filter_map(|w| w.parse::<usize>().ok()).sum::<usize>()).sum();
    assert!(later_rounds == 0 || !text.is_empty(), "{text}");
}

/// The in-process engine (ENGINE_INPROC): shred packets in, signed buy bytes out. With no packet
/// lost nothing allocates; a Reed-Solomon recovery (packet loss) allocates inside the library, and
/// how much is printed for the report.
#[test]
fn the_shreds_to_signed_path_makes_no_heap_allocation_without_loss() {
    use super::shreds::{testkit as shredkit, walk};
    let _state = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    super::clock::init();
    stamps::init();
    let now = crate::utils::dashboard::now_unix();
    testkit::seed(now);
    let creates: Vec<(String, Vec<u8>)> = testkit::frames(1).into_iter().map(|(n, f)| (n, f[73..].to_vec())).collect();
    let keypair = Arc::new(solana_keypair::Keypair::new());
    let mut decider = engine::bench_decider(keypair, true, stamps::Mode::Inline).expect("template");
    let mut st = engine::Inproc::new();
    engine::forget_seen();
    let mut head = 0u64;
    let mut drained = Vec::new();
    stamps::drain(&mut head, &mut drained);
    drained.clear();
    let filler = |s: u8| walk::testkit::filler(s, &[[s; 32]]);
    let slot_shreds = |slot: u64, create: &[u8]| {
        let batch = walk::testkit::batch(&[([slot as u8; 32], vec![filler(1), create.to_vec(), filler(2)])]);
        shredkit::shreds(slot, &batch, true)
    };
    // The first slot warms every lazily built table.
    let (data, _) = slot_shreds(900_000_000, &creates[0].1);
    for pkt in &data {
        engine::bench_shred(&mut st, &mut decider, pkt, now_ns());
    }
    engine::bench_settle(&mut decider);
    for (i, (name, create)) in creates[1..].iter().enumerate() {
        let (data, _) = slot_shreds(900_000_001 + i as u64, create);
        let ((), n) = allocations(|| {
            for pkt in &data {
                engine::bench_shred(&mut st, &mut decider, pkt, now_ns());
            }
            engine::bench_settle(&mut decider);
        });
        assert_eq!(n, 0, "{name}: {n} heap allocations from shreds to signed bytes");
    }
    stamps::drain(&mut head, &mut drained);
    let inproc: Vec<&stamps::Stamp> = drained.iter().filter(|s| s.feed == stamps::Feed::Inproc).collect();
    assert!(inproc.iter().filter(|s| s.t_ready > 0).count() >= creates.len() - 1, "every measured create was built and signed");
    assert!(inproc.iter().all(|s| s.t_shred_recv > 0 && s.t_deshred >= s.t_shred_recv && s.t_matched >= s.t_deshred), "the shred stamps are in order");

    // With data shred 1 of the slot lost: the recovery's allocations, for the report.
    let (data, code) = slot_shreds(900_000_500, &creates[1].1);
    let mut order: Vec<&Vec<u8>> = vec![&data[0]];
    order.extend(data[2..].iter());
    order.push(&code[0]);
    let before = st.recovered();
    let ((), n) = allocations(|| {
        for pkt in order {
            engine::bench_shred(&mut st, &mut decider, pkt, now_ns());
        }
        engine::bench_settle(&mut decider);
    });
    assert_eq!(st.recovered() - before, 1, "the lost shred was rebuilt in the measured run");
    eprintln!("allocations with one data shred recovered by Reed-Solomon, first set of this shape ({} data + {} coding): {n}", data.len(), code.len());
    // Again with the shape's codec cached (mainnet's 32:32 is cached at startup).
    let (data, code) = slot_shreds(900_000_501, &creates[2].1);
    let mut order: Vec<&Vec<u8>> = vec![&data[0]];
    order.extend(data[2..].iter());
    order.push(&code[0]);
    let before = st.recovered();
    let ((), n) = allocations(|| {
        for pkt in order {
            engine::bench_shred(&mut st, &mut decider, pkt, now_ns());
        }
        engine::bench_settle(&mut decider);
    });
    assert_eq!(st.recovered() - before, 1);
    eprintln!("allocations with one data shred recovered by Reed-Solomon, codec cached ({} data + {} coding): {n}", data.len(), code.len());
}

#[test]
fn the_fee_lookup_makes_no_heap_allocation() {
    use super::{clients::Class, fee};
    let table = fee::Settings { mode: fee::Mode::Table, ..fee::Settings::from_env() };
    let mut ladder = fee::Settings { mode: fee::Mode::Ladder, ..fee::Settings::from_env() };
    ladder.ladder[Class::Firedancer as usize] = vec![fee::Arm::Fee(2_000_000), fee::Arm::Jito(500_000), fee::Arm::Fee(10_000_000)];
    fee::set_table(Class::Firedancer, 2, 15_000_000, 40_000_000);
    let mint = [7u8; 32];
    let run = || {
        let class = super::clients::class_of_slot(5).unwrap_or(Class::Firedancer);
        let limit = super::cu::limit_for(&solana_pubkey::Pubkey::default());
        let tier = fee::tier(2_000_000_000, Some(crate::config::rules::Follow::Recent), false);
        (
            fee::quote_with(&table, class, tier + 1, limit.max(112_700), 20_000_000, &mint, 1_000_000),
            fee::quote_with(&ladder, class, tier, limit.max(112_700), 0, &mint, 1_000_000),
            fee::quote(5, tier, 112_700, 0, &mint, 1_000_000),
            fee::skips_with(&ladder, class),
        )
    };
    run(); // first use of the statics
    let ((q, l, _, _), n) = allocations(run);
    assert_eq!(n, 0, "{n} heap allocations in the fee lookup");
    assert_eq!(q.map(|q| q.cu_price), Some(22_000_001));
    assert!(l.and_then(|l| l.arm).is_some());
}

/// The scorecard (2026-10-12) adds two lookups to `decide::prepare` (the deployer wallet's first
/// transaction, the creator's earlier launch results) and the score to the verdict: none of them
/// allocates, with history present for every fixture's deployer and creator.
#[test]
fn the_scorecard_lookups_and_score_allocate_nothing() {
    use super::decide;
    use crate::utils::{deployer_wallet, launch_results};
    let _state = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    super::clock::init();
    stamps::init();
    let now = crate::utils::dashboard::now_unix();
    testkit::seed(now);
    let wallet = solana_signer::Signer::pubkey(&solana_keypair::Keypair::new());
    let mut checked = 0;
    for round in 0..2 {
        for (name, frame) in testkit::frames(1) {
            let tx = super::decode::Tx::parse(&frame[73..]).unwrap();
            let mut launch = None;
            super::decode::launches(&tx, |l| launch = Some(l), |_| {});
            let Some(l) = launch else { continue };
            let d = decide::derive(&l, &wallet);
            if round == 0 {
                deployer_wallet::seed(l.deployer, deployer_wallet::FirstTx { first_bt: Some(now as i64 - 400 * 3600), funded_sol: Some(1.0) });
                for k in 1..=5u64 {
                    launch_results::seed(d.creator, now - 3600 * k, launch_results::LaunchResult { pnl_sol: 0.01 * k as f64, down: k % 2 == 0 });
                }
            }
            let (pre, a_pre) = allocations(|| decide::prepare(&l, &d, 1, now));
            let (out, a_dec) = allocations(|| decide::decide_prepared(&l, &d, 1, &[], now, &pre));
            let facts = match out {
                decide::Outcome::Buy(_, f) | decide::Outcome::Unknown(_, f) => Some(f),
                decide::Outcome::Reject(_, f) => f,
            };
            let scored = facts.map(|f| allocations(|| (crate::config::rules::launch_score(&f), crate::config::rules::launch_verdict_code_with(&f, Some(61)))));
            if round == 1 {
                assert_eq!((a_pre, a_dec), (0, 0), "{name}: prepare {a_pre}, decide {a_dec} allocations");
                if let Some((_, a_score)) = scored {
                    assert_eq!(a_score, 0, "{name}: scoring allocated {a_score} times");
                }
                checked += 1;
            }
        }
    }
    assert!(checked >= 4);
}

/// HOT_PREDECIDE (2026-10-11): the decision with its lookups made at arrival (`decide::prepare`,
/// then `decide_prepared` at the window's end) decides exactly as `decide` does, on every fixture,
/// bought or refused, and neither half allocates. A deployer balance that arrives after `prepare`
/// is still used.
#[test]
fn the_prepared_decision_is_the_decision_and_allocates_nothing() {
    use super::decide;
    let _state = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    super::clock::init();
    stamps::init();
    let now = crate::utils::dashboard::now_unix();
    testkit::seed(now);
    let wallet = solana_signer::Signer::pubkey(&solana_keypair::Keypair::new());
    let mut checked = 0;
    for round in 0..2 {
        for (name, frame) in testkit::frames(1) {
            let tx = super::decode::Tx::parse(&frame[73..]).unwrap();
            let mut launch = None;
            super::decode::launches(&tx, |l| launch = Some(l), |_| {});
            let Some(l) = launch else { continue };
            let d = decide::derive(&l, &wallet);
            let old = format!("{:?}", decide::decide(&l, &d, 1, &[], now));
            let (pre, a_pre) = allocations(|| decide::prepare(&l, &d, 1, now));
            let (new, a_dec) = allocations(|| decide::decide_prepared(&l, &d, 1, &[], now, &pre));
            assert_eq!(format!("{new:?}"), old, "{name}");
            if round == 1 {
                assert_eq!((a_pre, a_dec), (0, 0), "{name}: prepare {a_pre}, decide {a_dec} allocations");
            }
            checked += 1;
        }
    }
    assert!(checked >= 4);
    // A launch whose fee payer's balance is unknown when it arrives, known by the window's end
    // (a fixture that goes the whole way to a buy).
    let mut l = testkit::frames(1)
        .into_iter()
        .filter_map(|(_, frame)| {
            let tx = super::decode::Tx::parse(&frame[73..])?;
            let mut launch = None;
            super::decode::launches(&tx, |l| launch = Some(l), |_| {});
            launch
        })
        .find(|l| matches!(decide::decide(l, &decide::derive(l, &wallet), 1, &[], now), decide::Outcome::Buy(..)))
        .expect("a fixture that buys");
    let payer = solana_pubkey::Pubkey::new_unique();
    l.fee_payer = Some(payer);
    let d = decide::derive(&l, &wallet);
    let pre = decide::prepare(&l, &d, 1, now);
    let before = decide::decide_prepared(&l, &d, 1, &[], now, &pre);
    crate::processors::deployer_balances::seed_balance(payer, 5_000_000_000, 0);
    let after = decide::decide_prepared(&l, &d, 1, &[], now, &pre);
    assert_eq!(format!("{after:?}"), format!("{:?}", decide::decide(&l, &d, 1, &[], now)));
    assert_ne!(format!("{before:?}"), format!("{after:?}"), "the balance that came in is used");
}

/// HOT_WARM_STEPS (2026-10-11): the warm-up's derivation in steps ends where `derive` does, and a
/// whole warm-up cycle in steps allocates nothing; each call does one step.
#[test]
fn the_warm_up_in_steps_derives_as_derive_and_allocates_nothing() {
    use super::decide;
    let _state = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    super::clock::init();
    stamps::init();
    testkit::seed(crate::utils::dashboard::now_unix());
    let wallet = solana_signer::Signer::pubkey(&solana_keypair::Keypair::new());
    for (name, frame) in testkit::frames(1) {
        let tx = super::decode::Tx::parse(&frame[73..]).unwrap();
        let mut launch = None;
        super::decode::launches(&tx, |l| launch = Some(l), |_| {});
        let Some(mut l) = launch else { continue };
        for holder in [false, true] {
            l.holder_reward = holder;
            let mut d = decide::derive_if_followed(&l, &wallet);
            for step in 0..4 {
                decide::derive_step(&l, &wallet, &mut d, step);
            }
            assert_eq!(d, decide::derive(&l, &wallet), "{name}, holder reward {holder}");
        }
    }
    let mut decider = engine::bench_decider(Arc::new(solana_keypair::Keypair::new()), false, stamps::Mode::Spsc).unwrap();
    decider.set_warm_steps(true);
    let mut now = now_ns() + 10_000_000_000;
    // One whole cycle first (the creator vault table is built on first use).
    while decider.warm(now) {}
    for cycle in 0..3 {
        now += 10_000_000_000;
        let (steps, n) = allocations(|| {
            let mut steps = 0;
            while decider.warm(now) {
                steps += 1;
            }
            steps
        });
        assert_eq!((steps, n), (6, 0), "cycle {cycle}: {steps} steps, {n} allocations");
    }
    decider.set_warm_steps(false);
    now += 10_000_000_000;
    let (one, n) = allocations(|| decider.warm(now));
    assert!(one && n == 0 && !decider.warm(now), "one go: {n} allocations");
}

/// The lane writes (B2), on real sockets: a plain-HTTP lane connected to a local listener and a
/// threaded lane's hand-over, primed, written all at once and in turn. Nothing allocates on the
/// writing thread.
#[test]
fn the_lane_writes_make_no_heap_allocation() {
    use super::lanes;
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { return };
            std::thread::spawn(move || {
                let mut sink = [0u8; 65_536];
                while s.read(&mut sink).map(|r| r > 0).unwrap_or(false) {}
            });
        }
    });
    let mut cfg = lanes::test_config(&[(lanes::LANE_ZEROSLOT, 1_000_000), (lanes::LANE_JITO, 300_000)]);
    cfg.lanes[lanes::LANE_ZEROSLOT].url = Some(format!("http://127.0.0.1:{port}/"));
    let mut l = lanes::Lanes::start(cfg);
    let (mint, tx, b64) = ([7u8; 32], [1u8; 700], [b'A'; 936]);
    let mut t_sent = [0u64; super::stamps::LANES];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while t_sent[lanes::LANE_ZEROSLOT] == 0 && std::time::Instant::now() < deadline {
        l.send(&mint, &tx, &b64, &mut t_sent);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(t_sent[lanes::LANE_ZEROSLOT] > 0, "the plain-HTTP lane connected");
    for round in 0..5 {
        t_sent = [0; super::stamps::LANES];
        let ((), n) = allocations(|| {
            l.prime();
            l.send(&mint, &tx, &b64, &mut t_sent);
            l.send_turn(lanes::LANE_ZEROSLOT, &mint, &tx, &b64, &mut t_sent);
            l.send_turn(lanes::LANE_JITO, &mint, &tx, &b64, &mut t_sent);
        });
        assert_eq!(n, 0, "round {round}: {n} heap allocations writing the lanes");
        assert!(t_sent[lanes::LANE_ZEROSLOT] > 0, "round {round}: written");
    }
}

/// HOT_NONCE_FANOUT: a buy with a free nonce goes to every lane as its own copy, all on that nonce,
/// each paying only its own lane's tip. The copy that reached the plain-HTTP lane's socket is read
/// back: AdvanceNonceAccount first on the granted account, the nonce value as its blockhash, only
/// that lane's tip paid, a valid signature. The copies' signatures differ, the nonce account comes
/// back as used and sent, and nothing allocates on the deciding thread.
#[test]
fn a_fanned_out_buy_sends_one_copy_per_lane_on_one_nonce_without_allocating() {
    use super::{lanes, nonces, spsc, template};
    use solana_signer::Signer;
    use std::io::Read;
    let _state = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    super::clock::init();
    stamps::init();
    testkit::seed(crate::utils::dashboard::now_unix());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (wire_tx, wire_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { return };
            let wire = wire_tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 65_536];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        return;
                    }
                    let _ = wire.send(buf[..n].to_vec());
                }
            });
        }
    });
    let keypair = Arc::new(solana_keypair::Keypair::new());
    let mut cfg = lanes::test_config(&[(lanes::LANE_ZEROSLOT, 1_000_000), (lanes::LANE_EVERSTAKE, 1_100_000), (lanes::LANE_JITO, 300_000)]);
    cfg.lanes[lanes::LANE_ZEROSLOT].url = Some(format!("http://127.0.0.1:{port}/"));
    let spec = template::TemplateSpec { payer: keypair.pubkey(), cu_limit: 120_000, loaded_data_bytes: None, tips: cfg.tips(), dontfront: cfg.tip_index(lanes::LANE_JITO) };
    let zeroslot_tip = cfg.tip_index(lanes::LANE_ZEROSLOT).unwrap() as usize;
    let build = |handoff| {
        let (mut pre, mut refill) = super::presign::pair(Arc::new(super::presign::HotKey::of(&keypair)), 4096);
        refill.fill();
        let _ = &mut pre;
        let (grant_tx, grant_rx) = spsc::channel::<nonces::Grant>(8);
        let (used_tx, used_rx) = spsc::channel::<nonces::Used>(8);
        let d = engine::Decider::new(template::BuyTemplate::build(spec.clone()).unwrap(), keypair.clone(), false, stamps::Mode::Spsc)
            .with_lanes(engine::SendMode::Live, Some(lanes::Lanes::start(cfg.clone())), handoff, 0)
            .with_presigner(pre)
            .with_nonce_fanout(template::BuyTemplate::build_nonced(spec.clone()).unwrap(), (grant_rx, used_tx));
        (d, grant_tx, used_rx)
    };
    let (handoff_tx, mut handoff_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut d, mut grants, mut used) = build(Some(handoff_tx));
    // Let the plain-HTTP lane connect.
    std::thread::sleep(std::time::Duration::from_millis(300));
    engine::forget_seen();
    let (account, value) = (solana_pubkey::Pubkey::new_unique(), solana_hash::Hash::new_from_array([0x4e; 32]));
    assert!(grants.push(nonces::Grant { index: 3, account, value }));
    let frames = testkit::frames(1);
    let mut recent = engine::RecentSigs::new();
    let (_, frame) = frames.iter().find(|(n, _)| n.contains("legacy")).unwrap();
    engine::bench_frame(&mut d, &mut recent, frame, now_ns());
    engine::bench_settle(&mut d);
    let b = handoff_rx.try_recv().expect("the buy was handed on");
    let lanes_with_copies: Vec<usize> = (0..stamps::LANES).filter(|&i| b.copies[i].is_some()).collect();
    assert_eq!(lanes_with_copies, vec![lanes::LANE_EVERSTAKE, lanes::LANE_ZEROSLOT, lanes::LANE_JITO]);
    let sigs: std::collections::HashSet<_> = b.copies.iter().flatten().collect();
    assert_eq!(sigs.len(), 3, "each copy its own signature");
    let (index, copy) = b.nonce.expect("on a nonce");
    assert_eq!(index, 3);
    assert_eq!(used.pop(), Some(nonces::Used { index: 3, copy, sent: true }));
    assert_eq!(b.copy_tips[lanes::LANE_ZEROSLOT], 1_000_000);
    assert_eq!(b.copy_tips[lanes::LANE_JITO] > 0, true);
    // The copy on the 0slot socket.
    let mut wire = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline && !String::from_utf8_lossy(&wire).contains("maxRetries") {
        if let Ok(chunk) = wire_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            wire.extend(chunk);
        }
    }
    let text = String::from_utf8_lossy(&wire).to_string();
    let b64 = text.split(r#""params":[""#).nth(1).and_then(|r| r.split('"').next()).expect("a sendTransaction body");
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    let tx: solana_transaction::versioned::VersionedTransaction = bincode::deserialize(&bytes).unwrap();
    assert_eq!(Some(tx.signatures[0]), b.copies[lanes::LANE_ZEROSLOT], "the 0slot lane got its own copy");
    assert!(tx.signatures[0].verify(&keypair.pubkey().to_bytes(), &tx.message.serialize()));
    let solana_message::VersionedMessage::Legacy(m) = &tx.message else { panic!("legacy") };
    assert_eq!(m.recent_blockhash, value, "the nonce value");
    let first = &m.instructions[0];
    assert_eq!((m.account_keys[first.program_id_index as usize], first.data.clone()), (crate::utils::nonce::SYSTEM_PROGRAM, vec![4, 0, 0, 0]));
    assert_eq!(m.account_keys[first.accounts[0] as usize], account, "the granted account");
    let paid: Vec<u64> = m
        .instructions
        .iter()
        .filter(|ix| m.account_keys[ix.program_id_index as usize] == crate::utils::nonce::SYSTEM_PROGRAM && ix.data.len() == 12 && ix.data[..4] == 2u32.to_le_bytes())
        .map(|ix| u64::from_le_bytes(ix.data[4..12].try_into().unwrap()))
        .collect();
    assert_eq!(paid.iter().filter(|&&l| l > 0).count(), 1, "one tip paid: {paid:?}");
    assert_eq!(paid[zeroslot_tip], 1_000_000);
    // No allocation on the deciding thread (no hand-off channel here: it is the runtime's).
    let (mut d2, mut grants2, _used2) = build(None);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut n_total = 0;
    for (round, (name, frame)) in frames.iter().enumerate() {
        assert!(grants2.push(nonces::Grant { index: round as u8, account, value }));
        engine::forget_seen();
        let ((), n) = allocations(|| {
            engine::bench_frame(&mut d2, &mut recent, frame, now_ns());
            engine::bench_settle(&mut d2);
        });
        if round > 0 {
            assert_eq!(n, 0, "{name}: {n} allocations");
        }
        n_total += n;
    }
    let _ = n_total;
}

/// The snipe test's checks on the decision thread (`utils::snipe_test`): the filter, the read-only
/// cap check and the cap entry taken at the commit point allocate nothing (the settings are read
/// once beforehand, as the hot path's warm-up does).
#[test]
fn the_snipe_test_checks_allocate_nothing() {
    use crate::utils::snipe_test::{check_caps, filter, on, size_lamports, try_enter, Visible};
    let _ = (on(), size_lamports());
    let now = crate::utils::dashboard::now_unix();
    let _ = check_caps(now);
    let v = Visible { dev_buy_lamports: 500_000_000, bundle_wallets: Some(1), real_sol_lamports: 900_000_000, complete: false };
    let (r, n) = allocations(|| filter(&v));
    assert_eq!((r, n), (Ok(()), 0));
    let (_, n) = allocations(|| filter(&Visible { bundle_wallets: None, ..v }));
    assert_eq!(n, 0);
    let (_, n) = allocations(|| check_caps(now));
    assert_eq!(n, 0);
    let (_, n) = allocations(|| try_enter(now));
    assert_eq!(n, 0, "the entry (test mode off: refused by the zero caps, still without allocating)");
}

/// The whale rule's entry check (`processors::whale::check`), run on every young coin at every new
/// slot: no allocation, passing or failing.
#[test]
fn the_whale_check_allocates_nothing() {
    use crate::processors::whale::{check, Coin};
    use crate::instructions::pumpfun_buy_ix::CurveReserves;
    let c = Coin {
        mint: solana_pubkey::Pubkey::new_unique(),
        deployer: solana_pubkey::Pubkey::new_unique(),
        creator: solana_pubkey::Pubkey::new_unique(),
        token_program: crate::utils::utils::TOKEN_PROGRAM_ID,
        mayhem: false,
        sol_quote: true,
        created_ms: 0,
        launch_price: 30e9 / 1.073e15,
        dev_buy: 500_000_000,
        reserves: CurveReserves { virtual_token_reserves: 1_005_937_500_000_000, virtual_sol_reserves: 32_000_000_000, real_token_reserves: 500_000_000_000_000, real_sol_reserves: 2_000_000_000 },
        buyers: vec![solana_pubkey::Pubkey::new_unique(); 10],
        many_buyers: false,
        last_trade_slot: 100,
        flows: [(0, 0); 16],
        furthest: 0,
    };
    let (r, n) = allocations(|| check(&c, 120_000, 110, false));
    assert_eq!((r, n), (Ok(()), 0));
    let (r, n) = allocations(|| check(&c, 120_000, 102, false));
    assert_eq!((r.is_err(), n), (true, 0));
}
