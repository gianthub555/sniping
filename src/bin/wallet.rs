//! Wallet rotation. Secrets are never printed: only public keys, balances and signatures.
//!
//!   wallet new <keyfile>
//!       A new keypair, written to <keyfile> (Solana CLI JSON, mode 600; an existing file is never
//!       overwritten). Prints its public key.
//!   wallet sweep <keyfile> [--nonce <address>]... [--yes]
//!       Moves everything the current wallet (PRIVATE_KEY in .env) controls to <keyfile>'s wallet:
//!       closes its empty token accounts (rent to the new wallet), withdraws each nonce account
//!       given whose authority it is, then transfers the rest of its SOL. Without --yes it only
//!       simulates and prints what it would do.

use {
    anyhow::{anyhow, bail, Context, Result},
    sniper_bot::{
        config::{PAYER, RPC_CLIENT},
        utils::{
            nonce,
            utils::{transfer_sol, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
        },
    },
    solana_commitment_config::CommitmentConfig,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    std::io::Write,
};

const FEE: u64 = 5_000;

fn sol(l: u64) -> f64 {
    l as f64 / 1e9
}

fn read_keyfile(path: &str) -> Result<Keypair> {
    let bytes: Vec<u8> = serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("read {path}"))?)?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow!("{path} is not a keypair: {e}"))
}

fn new_keyfile(path: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let keypair = Keypair::new();
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).with_context(|| format!("create {path} (it must not exist yet)"))?;
    file.write_all(serde_json::to_string(&keypair.to_bytes().to_vec())?.as_bytes())?;
    println!("new wallet {} written to {path} (mode 600)", keypair.pubkey());
    Ok(())
}

async fn run(ixs: Vec<Instruction>, what: &str, yes: bool) -> Result<()> {
    let old = &*PAYER;
    let blockhash = RPC_CLIENT.get_latest_blockhash().await?;
    let message = Message::new_with_blockhash(&ixs, Some(&old.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[old.as_ref()])?;
    let sim = RPC_CLIENT.simulate_transaction(&tx).await?.value;
    if let Some(err) = sim.err {
        bail!("{what}: simulation failed: {err:?}; logs {:?}", sim.logs.unwrap_or_default());
    }
    println!("{what}: simulation ok ({} CU)", sim.units_consumed.unwrap_or_default());
    if !yes {
        return Ok(());
    }
    let signature = RPC_CLIENT.send_and_confirm_transaction_with_spinner_and_commitment(&tx, CommitmentConfig::confirmed()).await?;
    println!("{what}: confirmed {signature}");
    Ok(())
}

async fn sweep(keyfile: &str, nonces: &[Pubkey], yes: bool) -> Result<()> {
    let old = PAYER.pubkey();
    let new = read_keyfile(keyfile)?.pubkey();
    if new == old {
        bail!("the new wallet is the current one");
    }
    println!("from {old} (current, burned) to {new} ({keyfile}){}", if yes { "" } else { ": dry run, add --yes to send" });

    // Empty token accounts: closed, rent to the new wallet. Any with tokens stops the sweep.
    let mut ixs = Vec::new();
    for program in [TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID] {
        let accounts = RPC_CLIENT
            .get_token_accounts_by_owner(&old, solana_rpc_client_api::request::TokenAccountsFilter::ProgramId(program))
            .await?;
        for a in accounts {
            let address: Pubkey = a.pubkey.parse()?;
            let amount = match &a.account.data {
                solana_account_decoder_client_types::UiAccountData::Json(parsed) => parsed.parsed["info"]["tokenAmount"]["amount"].as_str().unwrap_or("?").to_string(),
                _ => "?".into(),
            };
            if amount != "0" {
                bail!("token account {address} holds {amount} tokens: sell or move them first");
            }
            println!("  close token account {address} ({} SOL rent)", sol(a.account.lamports));
            ixs.push(Instruction {
                program_id: program,
                accounts: vec![AccountMeta::new(address, false), AccountMeta::new(new, false), AccountMeta::new_readonly(old, true)],
                data: vec![9],
            });
        }
    }
    // Nonce accounts: only those whose authority is the current wallet.
    for address in nonces {
        let account = RPC_CLIENT.get_account(address).await.with_context(|| format!("nonce account {address}"))?;
        let (authority, _) = nonce::parse(&account.data).ok_or_else(|| anyhow!("{address} is not a nonce account"))?;
        if authority != old {
            bail!("nonce account {address} belongs to {authority}, not the current wallet");
        }
        println!("  withdraw nonce account {address} ({} SOL)", sol(account.lamports));
        ixs.push(nonce::withdraw_instruction(address, &old, &new, account.lamports));
    }
    if !ixs.is_empty() {
        run(ixs, "close token and nonce accounts", yes).await?;
    }

    // The rest of the SOL, less the fee of this last transaction.
    // Read after the closes landed (with --yes); a dry run simulates the transfer on today's balance.
    let balance = RPC_CLIENT.get_balance_with_commitment(&old, CommitmentConfig::confirmed()).await?.value;
    if balance <= FEE {
        println!("nothing left to transfer ({} SOL)", sol(balance));
    } else {
        // Everything but this transaction's fee: the old account ends at 0 and is gone.
        let amount = balance - FEE;
        println!("  transfer {} SOL", sol(amount));
        run(vec![transfer_sol(&old, &new, amount)], "transfer SOL", yes).await?;
    }
    if yes {
        let c = CommitmentConfig::confirmed();
        let (a, b) = (RPC_CLIENT.get_balance_with_commitment(&old, c).await?.value, RPC_CLIENT.get_balance_with_commitment(&new, c).await?.value);
        println!("balances now: old {} SOL, new {} SOL", sol(a), sol(b));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("new") if args.len() == 2 => new_keyfile(&args[1]),
        Some("sweep") if args.len() >= 2 => {
            let yes = args.iter().any(|a| a == "--yes");
            let mut nonces = Vec::new();
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                if a == "--nonce" {
                    nonces.push(it.next().ok_or_else(|| anyhow!("--nonce needs an address"))?.parse()?);
                }
            }
            sweep(&args[1], &nonces, yes).await
        }
        _ => bail!("usage: wallet new <keyfile> | wallet sweep <keyfile> [--nonce <address>]... [--yes]"),
    }
}
