//! Fixtures and in-memory state for the hot path's benchmark and allocation test: real pump.fun
//! create transactions (`processors/testdata`) as preprocessed frames, and a seeded world in which
//! their deployers are followed, funded and old enough, so the whole buy path runs. Nothing here
//! writes a file.

use {
    super::decode::{self, Tx},
    crate::{
        processors::{deployer_balances, lookup_tables},
        utils::{blockhash, control, creator_history, reference},
    },
    base64::Engine,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
};

/// Held by tests that change process-wide state (the trading switch), so they do not race.
#[cfg(test)]
pub static GLOBAL_STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// (name, preprocessed frame): version 0, slot, first signature, then the transaction.
pub fn frames(slot: u64) -> Vec<(String, Vec<u8>)> {
    let decode = |b: &str| base64::engine::general_purpose::STANDARD.decode(b).expect("fixture base64");
    let mut out = Vec::new();
    let all: serde_json::Value = serde_json::from_str(include_str!("../processors/testdata/pump_creates.json")).expect("fixtures");
    for (name, f) in all.as_object().expect("object") {
        out.push((format!("create {name}"), decode(f["tx_base64"].as_str().expect("tx"))));
    }
    let lookup: serde_json::Value = serde_json::from_str(include_str!("../processors/testdata/pump_create_lookup.json")).expect("fixture");
    for (table, addresses) in lookup["tables"].as_object().expect("tables") {
        let addresses = addresses.as_array().expect("array").iter().map(|a| a.as_str().expect("key").parse().expect("pubkey")).collect();
        lookup_tables::insert(table.parse().expect("table"), addresses);
    }
    out.push(("create with a lookup table".into(), decode(lookup["tx_base64"].as_str().expect("tx"))));
    out.into_iter()
        .map(|(name, tx)| {
            let sig = Tx::parse(&tx).expect("fixture parses").signature();
            let mut frame = vec![0u8];
            frame.extend(slot.to_le_bytes());
            frame.extend(sig);
            frame.extend(tx);
            (name, frame)
        })
        .collect()
}

/// A world where every fixture launch passes the entry rules: trading on, pump.fun config, a fresh
/// blockhash, each deployer followed (recent, profitable, traded 20 times), its fee payer holding
/// 5 SOL, and a first launch two days ago.
pub fn seed(now_unix: u64) {
    // Settings come from .env in the bot; a test process has none, so give the stake one.
    if std::env::var("BUY_SOL_AMOUNT").is_err() {
        std::env::set_var("BUY_SOL_AMOUNT", "0.1");
    }
    // Built once at startup in the bot; here before anything is measured.
    std::sync::LazyLock::force(&crate::config::TRADE_SETTING);
    // main.rs loads the deployer records at startup.
    let _ = crate::utils::deployer_record::init();
    crate::instructions::pumpfun_buy_ix::seed_test_config();
    control::set_trading(true);
    blockhash::RPC_HASH.store(&Hash::new_from_array([7; 32]), super::clock::now_ns(), 0);
    // The fixtures' slot has a near leader (the distance rule) of a client the routes buy on
    // (unknown clients are skipped in table mode).
    for slot in 0..8 {
        super::distance::set_slot_rtt(slot, Some(1_000));
        super::clients::set_slot_class(slot, super::clients::Class::Firedancer);
    }
    for (_, frame) in frames(1) {
        let Some(tx) = Tx::parse(&frame[73..]) else { continue };
        decode::launches(
            &tx,
            |l| {
                reference::seed_record(l.deployer, reference::Record { last: now_unix - 60, profit: 1_000_000_000, trades: 20 });
                if let Some(payer) = l.fee_payer {
                    deployer_balances::seed_balance(payer, 5_000_000_000, 0);
                }
                creator_history::seed_launch(&l.deployer, &Pubkey::new_unique(), now_unix - 2 * 86_400);
            },
            |_| {},
        );
    }
}

/// Follows the deployer of the launch in `frame` (a preprocessed frame) and funds its fee payer:
/// for benchmarks that feed many fresh coins of one fixture.
pub fn seed_follow(frame: &[u8]) {
    let now = crate::utils::dashboard::now_unix();
    let Some(tx) = Tx::parse(&frame[73..]) else { return };
    decode::launches(
        &tx,
        |l| {
            reference::seed_record(l.deployer, reference::Record { last: now - 60, profit: 1_000_000_000, trades: 20 });
            if let Some(payer) = l.fee_payer {
                deployer_balances::seed_balance(payer, 5_000_000_000, 0);
            }
        },
        |_| {},
    );
}
