








use {
    anyhow::{anyhow, bail, Result},
    solana_pubkey::Pubkey,
    sniper_bot::{
        config::{apply_slippage_down, lamports_to_sol, trading_wallet, TRADE_SETTING, RPC_CLIENT, PUBKEY},
        instructions::{
            pumpfun_buy_ix::{init_pumpfun_config, PumpFunMarket},
            pumpswap_buy_ix::init_pumpswap_config,
            types::Market,
        },
        service::{self, send_via, simulate, tip_instructions_for, SendProfile, Submitted},
        utils::{
            blockhash::{recent_blockhash_handler, wait_for_blockhash},
            build_and_sign::{build_and_sign_with_fee, build_unsigned_with_fee},
            utils::{confirm_signature, get_associated_token_address, get_token_balance, ConfirmOutcome},
        },
    },
    std::{str::FromStr, time::Duration},
};



const SELL_SLIPPAGE_BPS: u64 = 9_000;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut args = std::env::args().skip(1);
    let usage = "usage: sell <mint> [--yes]";
    let mint = Pubkey::from_str(&args.next().ok_or_else(|| anyhow!(usage))?)?;
    let send_for_real = match args.next().as_deref() {
        None => false,
        Some("--yes") => true,
        Some(other) => bail!("unknown argument {other}\n{usage}"),
    };

    let rpc = RPC_CLIENT.clone();
    tokio::spawn(recent_blockhash_handler(rpc.clone()));
    wait_for_blockhash(Duration::from_secs(20)).await?;
    init_pumpfun_config(rpc.clone()).await?;
    init_pumpswap_config(rpc.clone()).await?;

    let mut market = Market::PumpFun(PumpFunMarket::from_mint(&rpc, mint).await?);
    if let Some(next) = market.migrated(&rpc).await? {
        println!("{mint} has moved to {}", next.dex());
        market = next;
    }
    let wallet = trading_wallet();
    let token_account = get_associated_token_address(&wallet, &mint, &market.token_program());
    let balance = get_token_balance(&rpc, &token_account).await?;
    if balance == 0 {
        bail!("wallet {wallet} holds no {mint}");
    }
    let expected = market.quote_sell(balance)?;
    let min_out = apply_slippage_down(expected, SELL_SLIPPAGE_BPS);
    println!(
        "{mint} on {}: {balance} tokens, quote {:.6} SOL (accepting down to {:.6})",
        market.dex(),
        lamports_to_sol(expected),
        lamports_to_sol(min_out)
    );
    let instructions = market.sell_instructions(&wallet, balance, min_out)?;

    if !send_for_real {
        let tx = build_unsigned_with_fee(
            instructions,
            tip_instructions_for(SendProfile::Sell, &wallet),
            &wallet,
            SendProfile::Sell.priority_fee(),
        )?;
        match simulate(&tx, None).await? {
            Submitted::Simulated { error: None, .. } => println!("simulation OK; run again with --yes to sell"),
            Submitted::Simulated { error: Some(e), logs, .. } => {
                for line in logs.iter().filter(|l| l.contains("Error") || l.contains("error")) {
                    println!("  {line}");
                }
                bail!("simulation failed: {e}");
            }
            Submitted::Sent { .. } => unreachable!("simulate never sends"),
        }
        return Ok(());
    }

    if TRADE_SETTING.dry_run {
        bail!("DRY_RUN is on in .env; set DRY_RUN=false to send");
    }
    service::init().await?;
    let tx = build_and_sign_with_fee(instructions, tip_instructions_for(SendProfile::Sell, &PUBKEY), SendProfile::Sell.priority_fee())?;
    let signature = tx.signatures[0];
    send_via(SendProfile::Sell, &tx).await?;
    println!("sent {signature}");
    match confirm_signature(&rpc, &signature, Duration::from_secs(45)).await? {
        ConfirmOutcome::Confirmed { slot } => println!("sold in slot {slot}"),
        ConfirmOutcome::Failed { error, .. } => bail!("sell failed on chain: {error}"),
        ConfirmOutcome::TimedOut => bail!("sell not confirmed within 45 s; check {signature}"),
    }
    Ok(())
}
