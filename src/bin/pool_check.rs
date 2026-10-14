//! Checks bigboss's PumpSwap buy and sell builders against the chain (2026-10-13, the snipe test's
//! graduations). Nothing is sent: only reads and `simulateTransaction` (signatures not verified).
//!
//!   pool_check <examples.json>
//!
//! `examples.json`: real mainnet PumpSwap trades, as `{"buy_exact_quote_in": {"sig", "accounts",
//! "data", "signer"}, "sell": {...}}` (research/snipe-test-2026-10-13/find_pool_trades.py writes it).
//! For each: our builder's PumpSwap instruction for the same pool, payer and amounts, compared account
//! by account with the real one (the protocol and buyback fee recipients are a random pick of the
//! program's lists: any listed one is accepted); the data compared byte for byte. Then the wallet's
//! buy of 0.001 SOL on the buy's pool and a sell of half the tokens it gets, simulated in one
//! transaction (both builders end to end on today's pool state).

use {
    anyhow::{anyhow, bail, Context, Result},
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_rpc_client_api::config::RpcSimulateTransactionConfig,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    sniper_bot::{
        config::{PUBKEY, RPC_CLIENT},
        instructions::pumpswap_buy_ix::{init_pumpswap_config, pumpswap_config, PumpSwapMarket, PUMPSWAP_PROGRAM_ID},
        utils::utils::{get_associated_token_address, set_compute_unit_limit, WSOL_MINT, TOKEN_PROGRAM_ID},
    },
    std::str::FromStr,
};

fn hex(s: &str) -> Result<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!("{e}"))).collect()
}

/// Compares our PumpSwap instruction with the real one, account by account. Returns the mismatches.
fn compare(ours: &[Pubkey], real: &[Pubkey], allowed_random: &[(usize, Vec<Pubkey>)]) -> Vec<String> {
    let mut out = Vec::new();
    if ours.len() != real.len() {
        out.push(format!("{} accounts ours, {} real", ours.len(), real.len()));
    }
    for (i, (a, b)) in ours.iter().zip(real).enumerate() {
        if a == b {
            continue;
        }
        if let Some((_, set)) = allowed_random.iter().find(|(j, _)| *j == i) {
            if set.contains(b) {
                continue;
            }
        }
        out.push(format!("#{i}: ours {a}, real {b}"));
    }
    out
}

/// Simulates `ixs` paid by `payer` (signatures not verified, nothing sent). 1 when it failed.
async fn simulate(rpc: &solana_rpc_client::nonblocking::rpc_client::RpcClient, payer: &Pubkey, ixs: Vec<solana_instruction::Instruction>, what: &str) -> Result<u32> {
    let message = Message::new_with_blockhash(&ixs, Some(payer), &Hash::default());
    let tx = VersionedTransaction { signatures: vec![Signature::default(); message.header.num_required_signatures as usize], message: VersionedMessage::Legacy(message) };
    let config = RpcSimulateTransactionConfig { sig_verify: false, replace_recent_blockhash: true, commitment: Some(CommitmentConfig::processed()), ..Default::default() };
    let sim = rpc.simulate_transaction_with_config(&tx, config).await?.value;
    match sim.err {
        None => {
            println!("simulated (nothing sent): {what}: OK, {} CU", sim.units_consumed.unwrap_or_default());
            Ok(0)
        }
        Some(e) => {
            println!("simulated (nothing sent): {what}: FAILED {e:?}");
            for l in sim.logs.unwrap_or_default().iter().rev().take(12).rev() {
                println!("    {l}");
            }
            Ok(1)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let path = std::env::args().nth(1).ok_or_else(|| anyhow!("usage: pool_check <examples.json>"))?;
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let rpc = RPC_CLIENT.clone();
    init_pumpswap_config(rpc.clone()).await?;
    let config = pumpswap_config()?;
    let protocol: Vec<Pubkey> = config.protocol_fee_recipients.iter().chain(&config.reserved_fee_recipients).copied().collect();
    let protocol_atas: Vec<Pubkey> = protocol.iter().map(|r| get_associated_token_address(r, &WSOL_MINT, &TOKEN_PROGRAM_ID)).collect();
    let buyback: Vec<Pubkey> = config.buyback_fee_recipients.clone();
    let buyback_atas: Vec<Pubkey> = buyback.iter().map(|r| get_associated_token_address(r, &WSOL_MINT, &TOKEN_PROGRAM_ID)).collect();
    let mut failures = 0;

    for (kind, discriminator) in [("buy_exact_quote_in", "c62e1552b4d9e870"), ("sell", "33e685a4017f83ad")] {
        let Some(ex) = doc.get(kind) else {
            println!("{kind}: no real example in {path}");
            continue;
        };
        let real: Vec<Pubkey> = ex["accounts"].as_array().ok_or_else(|| anyhow!("accounts"))?.iter().map(|k| Pubkey::from_str(k.as_str().unwrap_or_default())).collect::<Result<_, _>>()?;
        let data = hex(ex["data"].as_str().unwrap_or_default())?;
        if !ex["data"].as_str().unwrap_or_default().starts_with(discriminator) {
            bail!("{kind}: the example is not a {kind}");
        }
        let signer = real[1];
        let mut market = PumpSwapMarket::from_pool(&rpc, real[0]).await.with_context(|| format!("pool {}", real[0]))?;
        // The amounts of the real instruction (an exact-in buy: SOL in, min out; a sell: tokens in, min out).
        let a = u64::from_le_bytes(data[8..16].try_into()?);
        let b = u64::from_le_bytes(data[16..24].try_into()?);
        let built = if kind == "sell" { market.sell_instructions(&signer, a, b)? } else { market.buy_instructions(&signer, a, b)? };
        let ix = built.iter().find(|ix| ix.program_id == PUMPSWAP_PROGRAM_ID).ok_or_else(|| anyhow!("no PumpSwap instruction built"))?;
        let ours: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        // Random picks: the protocol fee recipient (9) and its wSOL account (10); the buyback
        // recipient and its wSOL account (the last two).
        let n = ours.len();
        let allowed = vec![(9, protocol.clone()), (10, protocol_atas.clone()), (n - 2, buyback.clone()), (n - 1, buyback_atas.clone())];
        let mismatches = compare(&ours, &real, &allowed);
        let data_ok = ix.data == data || (kind == "buy_exact_quote_in" && ix.data[..24] == data[..24]);
        println!(
            "{kind} on pool {} ({}…, slot {}): {} accounts ours, {} real; accounts {}; data {}",
            real[0],
            &ex["sig"].as_str().unwrap_or_default()[..12],
            ex["slot"],
            ours.len(),
            real.len(),
            if mismatches.is_empty() { "all match".to_string() } else { format!("MISMATCH: {}", mismatches.join("; ")) },
            if data_ok { "matches".to_string() } else { format!("DIFFERS: ours {}, real {}", ix.data.iter().map(|b| format!("{b:02x}")).collect::<String>(), ex["data"]) }
        );
        if !mismatches.is_empty() || !data_ok {
            failures += 1;
        }
        // The math: on the pool as the real trade found it (its event: the reserves before it),
        // our quote must give what the trader got.
        if let Some(ev) = ex["events"].as_array().and_then(|e| e.first()).and_then(|e| e.as_str()) {
            let e = hex(ev)?;
            let u = |at: usize| -> Result<u64> { Ok(u64::from_le_bytes(e[8 + at..16 + at].try_into()?)) };
            let t = sniper_bot::instructions::pumpswap_events::parse(kind != "sell", &e[8..])?;
            let at_trade = PumpSwapMarket {
                base_reserve: t.pool_base,
                quote_reserve: t.pool_quote,
                virtual_quote_reserves: t.virtual_quote.unwrap_or(market.virtual_quote_reserves),
                ..market.clone()
            };
            let (ours, theirs, what) = if kind == "sell" {
                (at_trade.quote_sell(a)?, t.user_quote, "lamports out")
            } else {
                (at_trade.quote_buy(a)?, t.base_amount, "tokens out")
            };
            let fee_bps = (u(64)?, u(80)?);
            println!(
                "  math on the trade's pool state (lp {} bps, protocol {} bps): ours {ours} {what}, the chain's {theirs}: {}",
                fee_bps.0,
                fee_bps.1,
                if ours == theirs { "exact".to_string() } else { format!("DIFFERS by {}", ours as i128 - theirs as i128) }
            );
            if ours != theirs {
                failures += 1;
            }
        }
        market.refresh(&rpc).await?;
        if kind == "buy_exact_quote_in" {
            // End to end: our wallet buys 0.001 SOL on this pool, simulated.
            let payer = *PUBKEY;
            let lamports = 1_000_000;
            let expected = market.quote_buy(lamports)?;
            let mut ixs = vec![set_compute_unit_limit(300_000)];
            ixs.extend(market.buy_instructions(&payer, lamports, expected * 9 / 10)?);
            failures += simulate(&rpc, &payer, ixs, &format!("our wallet {}… buys 0.001 SOL of {} (expects {expected} tokens, at least {})", &payer.to_string()[..6], market.base_mint, expected * 9 / 10)).await?;
        } else {
            // End to end: a wallet that holds the coin now sells 1% of it with our builder, simulated
            // (signatures aren't verified, so any holder can be the payer).
            let holders = rpc.get_token_largest_accounts(&market.base_mint).await?;
            let mut done = false;
            for h in holders.iter().take(8) {
                let account = Pubkey::from_str(&h.address)?;
                if account == market.pool_base_token_account {
                    continue;
                }
                let data = rpc.get_account_data(&account).await?;
                let owner = Pubkey::new_from_array(data[32..64].try_into()?);
                let amount = u64::from_le_bytes(data[64..72].try_into()?);
                if amount == 0 || get_associated_token_address(&owner, &market.base_mint, &market.base_token_program) != account || rpc.get_balance(&owner).await? < 10_000_000 {
                    continue;
                }
                let tokens = amount / 100;
                let min = market.quote_sell(tokens)? * 9 / 10;
                let mut sell = market.sell_instructions(&owner, tokens, min)?;
                // The holder keeps the rest: their coin account is not closed.
                sell.retain(|ix| !(ix.data == [9] && ix.accounts.first().is_some_and(|m| m.pubkey == account)));
                let mut ixs = vec![set_compute_unit_limit(300_000)];
                ixs.extend(sell);
                failures += simulate(&rpc, &owner, ixs, &format!("holder {}… sells {tokens} of {} (at least {min} lamports)", &owner.to_string()[..6], market.base_mint)).await?;
                done = true;
                break;
            }
            if !done {
                println!("sell: no holder with SOL found to simulate a sell");
            }
        }
    }
    if failures > 0 {
        bail!("{failures} check(s) failed");
    }
    println!("all checks passed");
    Ok(())
}
