//! The nonce accounts of the buy fan-out (NONCE_FANOUT, `utils::nonce`), listed in
//! `nonce_accounts.txt`, their authority the bot's wallet.
//!
//!   nonce create <N> [--yes]   create N nonce accounts (rent about 0.0015 SOL each, refundable)
//!   nonce list                 each account's balance, authority and current value
//!   nonce close [--yes]        withdraw every account's balance back to the wallet and forget it
//!
//! Without --yes, create and close only print what they would do.

use {
    anyhow::{anyhow, bail, Result},
    solana_commitment_config::CommitmentConfig,
    solana_keypair::Keypair,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    sniper_bot::{
        config::{lamports_to_sol, PAYER, PUBKEY, RPC_CLIENT},
        utils::nonce::{create_instructions, listed, parse, withdraw_instruction, NONCE_ACCOUNT_SIZE, NONCE_FILE},
    },
    std::io::Write,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let yes = args.iter().any(|a| a == "--yes");
    match args.first().map(String::as_str) {
        Some("create") => create(args.get(1).and_then(|n| n.parse().ok()).ok_or_else(|| anyhow!("usage: nonce create <N> [--yes]"))?, yes).await,
        Some("list") => list().await,
        Some("close") => close(yes).await,
        _ => bail!("usage: nonce create <N> [--yes] | list | close [--yes]"),
    }
}

async fn send(instructions: &[solana_instruction::Instruction], signers: &[&Keypair]) -> Result<String> {
    let blockhash = RPC_CLIENT.get_latest_blockhash().await?;
    let message = Message::new_with_blockhash(instructions, Some(&PUBKEY), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), signers)?;
    Ok(RPC_CLIENT.send_and_confirm_transaction(&tx).await?.to_string())
}

async fn create(n: usize, yes: bool) -> Result<()> {
    let rent = RPC_CLIENT.get_minimum_balance_for_rent_exemption(NONCE_ACCOUNT_SIZE as usize).await?;
    let balance = RPC_CLIENT.get_balance(&PUBKEY).await?;
    println!(
        "{n} nonce accounts for {}: {} SOL rent each ({} SOL in all, refundable with `nonce close`), plus fees; wallet holds {} SOL",
        *PUBKEY,
        lamports_to_sol(rent),
        lamports_to_sol(rent * n as u64),
        lamports_to_sol(balance)
    );
    if !yes {
        println!("dry run: add --yes to create them");
        return Ok(());
    }
    for i in 0..n {
        let nonce = Keypair::new();
        let ixs = create_instructions(&PUBKEY, &nonce.pubkey(), &PUBKEY, rent);
        let signature = send(&ixs, &[PAYER.as_ref(), &nonce]).await?;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(NONCE_FILE)?;
        writeln!(file, "{}", nonce.pubkey())?;
        println!("  {}/{n} {} ({signature})", i + 1, nonce.pubkey());
    }
    println!("listed in {NONCE_FILE}; set NONCE_FANOUT=true to use them");
    Ok(())
}

async fn list() -> Result<()> {
    let accounts = listed()?;
    if accounts.is_empty() {
        println!("no nonce accounts in {NONCE_FILE}");
        return Ok(());
    }
    let infos = RPC_CLIENT.get_multiple_accounts_with_commitment(&accounts, CommitmentConfig::confirmed()).await?.value;
    for (account, info) in accounts.iter().zip(infos) {
        match info.as_ref().map(|a| (a.lamports, parse(&a.data))) {
            Some((lamports, Some((authority, value)))) => println!(
                "{account}  {} SOL  authority {}  value {value}",
                lamports_to_sol(lamports),
                if authority == *PUBKEY { "the bot's wallet".to_string() } else { authority.to_string() }
            ),
            Some((lamports, None)) => println!("{account}  {} SOL  not an initialized nonce account", lamports_to_sol(lamports)),
            None => println!("{account}  does not exist"),
        }
    }
    Ok(())
}

async fn close(yes: bool) -> Result<()> {
    let accounts = listed()?;
    let infos = RPC_CLIENT.get_multiple_accounts_with_commitment(&accounts, CommitmentConfig::confirmed()).await?.value;
    let closing: Vec<(Pubkey, u64)> = accounts
        .iter()
        .zip(infos)
        .filter_map(|(account, info)| info.filter(|a| parse(&a.data).is_some_and(|(auth, _)| auth == *PUBKEY)).map(|a| (*account, a.lamports)))
        .collect();
    println!(
        "closing {} nonce accounts, {} SOL back to {}",
        closing.len(),
        lamports_to_sol(closing.iter().map(|(_, l)| l).sum()),
        *PUBKEY
    );
    if !yes {
        println!("dry run: add --yes to close them");
        return Ok(());
    }
    for (account, lamports) in &closing {
        let signature = send(&[withdraw_instruction(account, &PUBKEY, &PUBKEY, *lamports)], &[PAYER.as_ref()]).await?;
        println!("  {account} closed ({signature})");
    }
    let left: Vec<String> = accounts.iter().filter(|a| !closing.iter().any(|(c, _)| c == *a)).map(|a| a.to_string()).collect();
    std::fs::write(NONCE_FILE, if left.is_empty() { String::new() } else { left.join("\n") + "\n" })?;
    Ok(())
}
