//! The lane race (ticket 5.1): which send lane lands first, with real transactions.
//!
//!   lane_race [--rounds N] [--period-ms MS] --nonce ADDRESS [--yes]
//!   lane_race --create-nonce --yes
//!
//! Every enabled lane (LANE_SET=race) gets its own copy of a 1-lamport transfer to self each round,
//! all on the same durable nonce (`--nonce`, authority: the wallet in .env), each tipping only its
//! lane; at most one copy lands per round. Rounds every 1.6 s, 1,000 by default, counted at
//! `confirmed`; misses and failures stay in the denominator. Without `--yes` it only prints the
//! plan and the most it can cost, and sends nothing. The table goes to the terminal and to
//! `logs/lane_race_<unix>.md`.

use {
    anyhow::{anyhow, bail, Context, Result},
    base64::Engine,
    sniper_bot::{
        config::{PAYER, RPC_CLIENT, TRADE_SETTING},
        hot::{
            lanes::{LaneConfig, Lanes},
            race::{self, Tally, RACE_CU_LIMIT, SIGNATURE_FEE},
            stamps::LANES,
        },
        utils::nonce,
    },
    solana_commitment_config::CommitmentConfig,
    solana_keypair::Keypair,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status_client_types::TransactionConfirmationStatus,
    std::time::{Duration, Instant},
};

const ROUND_WAIT: Duration = Duration::from_secs(20);

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let rounds: u64 = value("--rounds").map(|v| v.parse()).transpose()?.unwrap_or(1_000);
    let period = Duration::from_millis(value("--period-ms").map(|v| v.parse()).transpose()?.unwrap_or(1_600));
    let yes = args.iter().any(|a| a == "--yes");
    // The race uses every enabled lane.
    std::env::set_var("LANE_SET", "race");
    let cfg = LaneConfig::from_env();
    let payer: &Keypair = &PAYER;
    // The bot's own priority per compute unit (its default fee at the buy's limit).
    let cu_price = cfg.fee_default * 1_000_000 / TRADE_SETTING.compute_unit_limit.max(1) as u64;

    if args.iter().any(|a| a == "--create-nonce") {
        if !yes {
            bail!("--create-nonce sends a transaction: add --yes");
        }
        let account = Keypair::new();
        let rent = RPC_CLIENT.get_minimum_balance_for_rent_exemption(80).await?;
        let ixs = nonce::create_instructions(&payer.pubkey(), &account.pubkey(), &payer.pubkey(), rent);
        let blockhash = RPC_CLIENT.get_latest_blockhash().await?;
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &blockhash)), &[payer, &account])?;
        let sig = RPC_CLIENT.send_and_confirm_transaction(&tx).await?;
        println!("nonce account {} created ({} SOL rent): {sig}", account.pubkey(), rent as f64 / 1e9);
        return Ok(());
    }

    if !yes {
        print!("{}", race::dry_plan(payer, &cfg, rounds, cu_price)?);
        println!("dry: nothing sent. Add --nonce ADDRESS --yes to race.");
        return Ok(());
    }
    let nonce_account: Pubkey = value("--nonce").ok_or_else(|| anyhow!("--nonce ADDRESS (or --create-nonce --yes first)"))?.parse()?;
    let (authority, _) = nonce::parse(&RPC_CLIENT.get_account(&nonce_account).await.context("nonce account")?.data).ok_or_else(|| anyhow!("{nonce_account} is not a nonce account"))?;
    if authority != payer.pubkey() {
        bail!("nonce account {nonce_account} belongs to {authority}, not the wallet in .env");
    }
    let mut lanes = Lanes::start(cfg.clone());
    // Let the connections open before the first round.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut tallies: [Tally; LANES] = Default::default();
    let priority = cu_price * RACE_CU_LIMIT as u64 / 1_000_000;
    let started = Instant::now();
    let mut next = Instant::now();
    for round in 0..rounds {
        tokio::time::sleep_until(next.into()).await;
        next += period;
        let account = RPC_CLIENT.get_account_with_commitment(&nonce_account, CommitmentConfig::confirmed()).await?.value;
        let Some((_, value)) = account.and_then(|a| nonce::parse(&a.data)) else {
            eprintln!("round {round}: nonce account unreadable; skipped");
            continue;
        };
        let slot = RPC_CLIENT.get_slot_with_commitment(CommitmentConfig::processed()).await?;
        let txs = race::round_txs(payer, &nonce_account, &value, &cfg, round, cu_price)?;
        for t in &txs {
            tallies[t.lane].rounds += 1;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&t.bytes);
            lanes.send_lane(t.lane, &[0; 32], &t.bytes, b64.as_bytes());
        }
        let sigs: Vec<_> = txs.iter().map(|t| t.signature).collect();
        let deadline = Instant::now() + ROUND_WAIT;
        'wait: while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let Ok(statuses) = RPC_CLIENT.get_signature_statuses(&sigs).await else { continue };
            for (t, s) in txs.iter().zip(statuses.value) {
                let Some(s) = s else { continue };
                if matches!(s.confirmation_status, Some(TransactionConfirmationStatus::Confirmed | TransactionConfirmationStatus::Finalized)) {
                    tallies[t.lane].land(s.slot.saturating_sub(slot), s.err.is_none(), SIGNATURE_FEE + priority + t.tip);
                    break 'wait;
                }
            }
        }
        if (round + 1) % 50 == 0 {
            println!("{} rounds in {:.0} s", round + 1, started.elapsed().as_secs_f64());
        }
    }
    let table = race::table(&tallies);
    println!("{table}");
    let path = format!("logs/lane_race_{}.md", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs());
    std::fs::write(&path, format!("# Lane race ({rounds} rounds, every {} ms)\n\n{table}", period.as_millis()))?;
    println!("written to {path}");
    Ok(())
}
