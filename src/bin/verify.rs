






use {
    anyhow::{anyhow, bail, Result},
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::config::{
        RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig, RpcTransactionConfig,
    },
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status_client_types::UiTransactionEncoding,
    sniper_bot::{
        config::{lamports_to_sol, rpc_endpoint, sol_to_lamports, Dex, RPC_CLIENT},
        instructions::{
            pumpfun_buy_ix::{init_pumpfun_config, PumpFunMarket},
            pumpswap_buy_ix::{init_pumpswap_config, PumpSwapMarket},
            raydium_amm_swap_base_in::RaydiumAmmMarket,
            raydium_clmm_swap::RaydiumClmmMarket,
            raydium_cpmm_swap_base_in::RaydiumCpmmMarket,
            raydium_launchpad_buy_ix::RaydiumLaunchpadMarket,
            types::Market,
        },
        utils::{
            blockhash::{recent_blockhash_handler, wait_for_blockhash},
            parse::read_u64,
            utils::{
                get_associated_token_address, set_compute_unit_limit, SYSTEM_PROGRAM_ID, TOKEN_2022_PROGRAM_ID,
                TOKEN_PROGRAM_ID, WSOL_MINT,
            },
        },
    },
    std::{str::FromStr, time::Duration},
};

const SIM_CU_LIMIT: u32 = 600_000;


const SIM_SLIPPAGE_BPS: u64 = 300;
const BASE_FEE: u64 = 5_000;

struct Args {
    dex: Dex,
    address: Pubkey,
    sol: f64,
    buyer: Option<Pubkey>,
    seller: Option<Pubkey>,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let usage = "usage: verify <dex> <pool|mint> [--sol 0.01] [--buyer <pubkey>] [--seller <pubkey>]";
    let dex: Dex = it.next().ok_or_else(|| anyhow!(usage))?.parse()?;
    let address = Pubkey::from_str(&it.next().ok_or_else(|| anyhow!(usage))?)?;
    let mut args = Args { dex, address, sol: 0.01, buyer: None, seller: None };
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| anyhow!("{flag} needs a value"))?;
        match flag.as_str() {
            "--sol" => args.sol = value.parse()?,
            "--buyer" => args.buyer = Some(Pubkey::from_str(&value)?),
            "--seller" => args.seller = Some(Pubkey::from_str(&value)?),
            _ => bail!("unknown flag {flag}\n{usage}"),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = parse_args()?;
    let rpc = RPC_CLIENT.clone();
    println!("rpc {}", rpc_endpoint().split('?').next().unwrap_or_default());

    tokio::spawn(recent_blockhash_handler(rpc.clone()));
    wait_for_blockhash(Duration::from_secs(20)).await?;
    
    tokio::time::sleep(Duration::from_millis(500)).await;
    if matches!(args.dex, Dex::PumpFun | Dex::PumpSwap) {
        init_pumpfun_config(rpc.clone()).await?;
        init_pumpswap_config(rpc.clone()).await?;
    }

    let mut market = load_market(&rpc, args.dex, args.address).await?;
    let mint = market.mint();
    let lamports = sol_to_lamports(args.sol);
    println!("{} market for mint {mint}, SOL liquidity {:.4}", market.dex(), lamports_to_sol(market.sol_liquidity()));
    println!("{market:#?}");

    
    let buyer = match args.buyer {
        Some(b) => b,
        None => find_buyer(&rpc, &args.address, lamports).await?,
    };
    market.refresh(&rpc).await?;
    let expected_tokens = market.quote_buy(lamports)?;
    let min_out = expected_tokens * (10_000 - SIM_SLIPPAGE_BPS) / 10_000;
    let buy_ixs = market.buy_instructions(&buyer, lamports, min_out)?;
    let buyer_ata = get_associated_token_address(&buyer, &mint, &market.token_program());
    let before = token_balance(&rpc, &buyer_ata).await?;
    println!("\nBUY {} SOL as {buyer}: quote {expected_tokens} tokens", args.sol);
    let sim = simulate(&rpc, &buyer, buy_ixs, &[buyer_ata]).await?;
    report(&sim);
    if sim.err.is_none() {
        let after = sim.accounts[0].as_ref().map_or(0, |d| read_u64(d, 64).unwrap_or(0));
        let received = after.saturating_sub(before);
        println!(
            "  received {received} tokens; quote {expected_tokens} ({:+.3} bps)",
            diff_bps(received, expected_tokens)
        );
    }

    
    let (seller, seller_balance) = match args.seller {
        Some(s) => (s, token_balance(&rpc, &get_associated_token_address(&s, &mint, &market.token_program())).await?),
        None => find_seller(&rpc, &args.address, &mint, &market.token_program(), &market).await?,
    };
    if seller_balance == 0 {
        bail!("seller {seller} holds no tokens");
    }
    
    let sell_amount = expected_tokens.clamp(1, seller_balance);
    market.refresh(&rpc).await?;
    let expected_sol = market.quote_sell(sell_amount)?;
    let min_sol = expected_sol * (10_000 - SIM_SLIPPAGE_BPS) / 10_000;
    let mut sell_ixs = market.sell_instructions(&seller, sell_amount, min_sol)?;
    let seller_ata = get_associated_token_address(&seller, &mint, &market.token_program());
    if sell_amount < seller_balance {
        
        sell_ixs.retain(|ix| !(is_token_program(&ix.program_id) && ix.data == [9] && ix.accounts[0].pubkey == seller_ata));
    }
    let lamports_before = rpc.get_balance_with_commitment(&seller, CommitmentConfig::processed()).await?.value;
    
    let mut closed_rent = if sell_amount == seller_balance {
        rpc.get_balance_with_commitment(&seller_ata, CommitmentConfig::processed()).await?.value
    } else {
        0
    };
    
    if market.dex() != Dex::PumpFun {
        let wsol = get_associated_token_address(&seller, &WSOL_MINT, &TOKEN_PROGRAM_ID);
        closed_rent += rpc.get_balance_with_commitment(&wsol, CommitmentConfig::processed()).await?.value;
    }
    println!("\nSELL {sell_amount} of {seller_balance} tokens as {seller}: quote {expected_sol} lamports");
    let sim = simulate(&rpc, &seller, sell_ixs, &[seller]).await?;
    report(&sim);
    if sim.err.is_none() {
        let lamports_after = sim.lamports[0];
        let received = (lamports_after + BASE_FEE).saturating_sub(lamports_before + closed_rent);
        println!(
            "  received {received} lamports (excluding {closed_rent} refunded from closed accounts); quote {expected_sol} ({:+.3} bps)",
            diff_bps(received, expected_sol)
        );
    }
    Ok(())
}

async fn load_market(rpc: &RpcClient, dex: Dex, address: Pubkey) -> Result<Market> {
    Ok(match dex {
        Dex::PumpFun => Market::PumpFun(PumpFunMarket::from_mint(rpc, address).await?),
        Dex::PumpSwap => Market::PumpSwap(PumpSwapMarket::from_pool(rpc, address).await?),
        Dex::RaydiumAmm => Market::RaydiumAmm(RaydiumAmmMarket::from_pool(rpc, address).await?),
        Dex::RaydiumCpmm => Market::RaydiumCpmm(RaydiumCpmmMarket::from_pool(rpc, address).await?),
        Dex::RaydiumLaunchpad => Market::RaydiumLaunchpad(RaydiumLaunchpadMarket::from_pool(rpc, address).await?),
        Dex::RaydiumClmm => Market::RaydiumClmm(RaydiumClmmMarket::from_pool(rpc, address).await?),
    })
}

struct Simulation {
    err: Option<String>,
    logs: Vec<String>,
    units: Option<u64>,
    
    accounts: Vec<Option<Vec<u8>>>,
    lamports: Vec<u64>,
}

async fn simulate(rpc: &RpcClient, payer: &Pubkey, instructions: Vec<Instruction>, watch: &[Pubkey]) -> Result<Simulation> {
    let mut all = vec![set_compute_unit_limit(SIM_CU_LIMIT)];
    all.extend(instructions);
    let message = Message::new_with_blockhash(&all, Some(payer), &Hash::default());
    let num_signers = message.header.num_required_signatures as usize;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_signers],
        message: VersionedMessage::Legacy(message),
    };
    let size = bincode::serialized_size(&tx)?;
    println!("  tx {} bytes, {} instructions", size, all.len());
    let config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        accounts: Some(RpcSimulateTransactionAccountsConfig {
            encoding: Some(UiAccountEncoding::Base64),
            addresses: watch.iter().map(|k| k.to_string()).collect(),
        }),
        ..Default::default()
    };
    let result = rpc.simulate_transaction_with_config(&tx, config).await?.value;
    let returned = result.accounts.unwrap_or_default();
    Ok(Simulation {
        err: result.err.map(|e| format!("{e:?}")),
        logs: result.logs.unwrap_or_default(),
        units: result.units_consumed,
        accounts: returned.iter().map(|a| a.as_ref().and_then(|a| a.data.decode())).collect(),
        lamports: returned.iter().map(|a| a.as_ref().map_or(0, |a| a.lamports)).collect(),
    })
}

fn report(sim: &Simulation) {
    match &sim.err {
        None => println!("  simulation OK, {} CU", sim.units.unwrap_or_default()),
        Some(err) => {
            println!("  simulation FAILED: {err}");
            for line in sim.logs.iter().rev().take(15).rev() {
                println!("    {line}");
            }
        }
    }
}

fn diff_bps(actual: u64, expected: u64) -> f64 {
    if expected == 0 {
        return 0.0;
    }
    (actual as f64 - expected as f64) / expected as f64 * 10_000.0
}

fn is_token_program(program: &Pubkey) -> bool {
    *program == TOKEN_PROGRAM_ID || *program == TOKEN_2022_PROGRAM_ID
}

async fn token_balance(rpc: &RpcClient, account: &Pubkey) -> Result<u64> {
    sniper_bot::utils::utils::get_token_balance(rpc, account).await
}


async fn recent_fee_payers(rpc: &RpcClient, address: &Pubkey, limit: usize) -> Result<Vec<Pubkey>> {
    let signatures = rpc.get_signatures_for_address(address).await?;
    let mut payers = Vec::new();
    for status in signatures.iter().filter(|s| s.err.is_none()).take(limit) {
        let signature = Signature::from_str(&status.signature)?;
        let config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(1),
        };
        match rpc.get_transaction_with_config(&signature, config).await {
            Ok(tx) => match tx.transaction.transaction.decode() {
                Some(decoded) => {
                    let payer = decoded.message.static_account_keys()[0];
                    if !payers.contains(&payer) {
                        payers.push(payer);
                    }
                }
                None => eprintln!("  could not decode {signature}"),
            },
            Err(e) => eprintln!("  getTransaction {signature}: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Ok(payers)
}


async fn find_buyer(rpc: &RpcClient, address: &Pubkey, lamports: u64) -> Result<Pubkey> {
    for payer in recent_fee_payers(rpc, address, 25).await? {
        if is_funded_wallet(rpc, &payer, lamports + sol_to_lamports(0.05)).await? {
            println!("buyer {payer} (recent fee payer)");
            return Ok(payer);
        }
    }
    bail!("no funded recent fee payer found for {address}; pass --buyer")
}



async fn find_seller(
    rpc: &RpcClient,
    pool_or_mint: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    market: &Market,
) -> Result<(Pubkey, u64)> {
    let mut candidates = Vec::new();
    match rpc.get_token_largest_accounts(mint).await {
        Ok(holders) => {
            for holder in holders {
                let token_account = Pubkey::from_str(&holder.address)?;
                if let Ok(account) = rpc.get_account(&token_account).await {
                    if account.data.len() >= 72 {
                        candidates.push(Pubkey::new_from_array(account.data[32..64].try_into()?));
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("  getTokenLargestAccounts unavailable ({e}); trying recent traders");
            candidates = recent_fee_payers(rpc, pool_or_mint, 40).await?;
        }
    }
    for owner in candidates {
        let token_account = get_associated_token_address(&owner, mint, token_program);
        let balance = token_balance(rpc, &token_account).await.unwrap_or(0);
        if balance == 0 {
            continue;
        }
        
        let wsol = get_associated_token_address(&owner, &WSOL_MINT, &TOKEN_PROGRAM_ID);
        if market.dex() != Dex::PumpFun && rpc.get_account(&wsol).await.is_ok() {
            continue;
        }
        if is_funded_wallet(rpc, &owner, sol_to_lamports(0.01)).await? {
            println!("seller {owner} holding {balance}");
            return Ok((owner, balance));
        }
    }
    bail!("no wallet holder of {mint} found; pass --seller")
}

async fn is_funded_wallet(rpc: &RpcClient, key: &Pubkey, min_lamports: u64) -> Result<bool> {
    match rpc.get_account(key).await {
        Ok(account) => Ok(account.owner == SYSTEM_PROGRAM_ID && account.lamports >= min_lamports),
        Err(_) => Ok(false),
    }
}
