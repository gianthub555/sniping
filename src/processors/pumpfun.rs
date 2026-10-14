use {
    super::{
        common::{cpi_event_payloads, spawn_snipe, SnipeRequest},
        launch_watch,
    },
    crate::{
        config::rules,
        instructions::{
            pumpfun_buy_ix::{CurveReserves, PumpFunMarket, CREATE_EVENT_DISCRIMINATOR, TRADE_EVENT_DISCRIMINATOR},
            pumpswap_buy_ix::PUMP_PROGRAM_ID,
            types::Market,
        },
        utils::{
            creator_history::observe_launch,
            dashboard::now_unix,
            parse::{read_pubkey, read_u64, BorshReader},
            utils::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    anyhow::Result,
    carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor},
    carbon_pumpfun_decoder::instructions::PumpfunInstruction,
    log::debug,
    solana_pubkey::Pubkey,
    std::time::Instant,
};



pub struct PumpfunProcess;

struct CreateEvent {
    mint: Pubkey,
    
    user: Pubkey,
    creator: Pubkey,
    reserves: CurveReserves,
    token_program: Pubkey,
    is_mayhem_mode: bool,
    quote_mint: Pubkey,
    name: String,
    uri: String,
}



/// The coins a transaction creates (their CreateEvents) and each one's token program.
pub fn created_mints(tx: &carbon_core::transaction::TransactionMetadata) -> Vec<(Pubkey, Pubkey)> {
    cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &CREATE_EVENT_DISCRIMINATOR)
        .iter()
        .filter_map(|body| parse_create_event(body).ok())
        .map(|e| (e.mint, e.token_program))
        .collect()
}

/// A coin a transaction creates, as the whale rule reads it (`processors::whale`).
#[derive(Debug, Clone, Copy)]
pub struct CreatedCoin {
    pub mint: Pubkey,
    /// The create's signer (the deployer) and the coin's creator.
    pub user: Pubkey,
    pub creator: Pubkey,
    /// The curve as created (before the dev buy).
    pub reserves: CurveReserves,
    pub token_program: Pubkey,
    pub mayhem: bool,
    /// Quoted in SOL (no quote mint, or wrapped SOL).
    pub sol_quote: bool,
}

/// Every coin a transaction creates (its CreateEvents).
pub fn created_coins(tx: &carbon_core::transaction::TransactionMetadata) -> Vec<CreatedCoin> {
    cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &CREATE_EVENT_DISCRIMINATOR)
        .iter()
        .filter_map(|body| parse_create_event(body).ok())
        .map(|e| CreatedCoin {
            mint: e.mint,
            user: e.user,
            creator: e.creator,
            reserves: e.reserves,
            token_program: e.token_program,
            mayhem: e.is_mayhem_mode,
            sol_quote: e.quote_mint == Pubkey::default() || e.quote_mint == WSOL_MINT,
        })
        .collect()
}

fn parse_create_event(body: &[u8]) -> Result<CreateEvent> {
    let mut r = BorshReader::new(body);
    let (name, _symbol, uri) = (r.string()?, r.string()?, r.string()?);
    let mint = r.pubkey()?;
    let _bonding_curve = r.pubkey()?;
    let user = r.pubkey()?;
    let creator = r.pubkey()?;
    let _timestamp = r.i64()?;
    let virtual_token_reserves = r.u64()?;
    let virtual_sol_reserves = r.u64()?;
    let real_token_reserves = r.u64()?;
    let _token_total_supply = r.u64()?;
    let token_program = r.pubkey().unwrap_or(TOKEN_PROGRAM_ID);
    let is_mayhem_mode = r.bool().unwrap_or(false);
    let _is_cashback_enabled = r.bool().unwrap_or(false);
    let quote_mint = r.pubkey().unwrap_or_default();
    Ok(CreateEvent {
        mint,
        user,
        creator,
        reserves: CurveReserves {
            virtual_token_reserves,
            virtual_sol_reserves,
            real_token_reserves,
            real_sol_reserves: 0,
        },
        token_program,
        is_mayhem_mode,
        quote_mint,
        name,
        uri,
    })
}

/// A CreateEvent's mint and is_cashback_enabled (bundle10's shadow log; the fields before it as
/// `parse_create_event` reads them).
pub fn create_event_cashback(body: &[u8]) -> Option<(Pubkey, bool)> {
    let mut r = BorshReader::new(body);
    let (_name, _symbol, _uri) = (r.string().ok()?, r.string().ok()?, r.string().ok()?);
    let mint = r.pubkey().ok()?;
    let (_bonding_curve, _user, _creator) = (r.pubkey().ok()?, r.pubkey().ok()?, r.pubkey().ok()?);
    let _timestamp = r.i64().ok()?;
    for _ in 0..4 {
        r.u64().ok()?;
    }
    let _token_program = r.pubkey().ok()?;
    let _mayhem = r.bool().ok()?;
    Some((mint, r.bool().unwrap_or(false)))
}

impl<'a> Processor<InstructionProcessorInputType<'a, PumpfunInstruction>> for PumpfunProcess {
    async fn process(&mut self, input: &InstructionProcessorInputType<'a, PumpfunInstruction>) -> CarbonResult<()> {
        let tx = &input.metadata.transaction_metadata;
        crate::utils::latency::observe(tx);
        // The hot path's backup feed while the preprocessed stream is silent (`hot::backup`).
        crate::hot::backup::offer(tx);
        let detected_at = Instant::now();
        // Launches first: the buy path is the latency-critical one, observers can wait.
        let mint = match input.decoded_instruction {
            PumpfunInstruction::CreateV2 { accounts, .. } => {
                // A quote mint other than WSOL in the remaining accounts marks a non-SOL coin.
                if accounts.remaining.first().is_some_and(|quote| quote.pubkey != WSOL_MINT) {
                    None
                } else {
                    Some(accounts.mint)
                }
            }
            PumpfunInstruction::Create { accounts, .. } => Some(accounts.mint),
            _ => None,
        };
        if let Some(mint) = mint {
            handle_launch(tx, mint, detected_at);
        }
        if let PumpfunInstruction::MigrateBondingCurveCreator { accounts, .. } = input.decoded_instruction {
            // Open positions too: their sells must name the new creator's vault.
            if super::position_feed::is_active() {
                super::position_feed::observe_creator_migration(&accounts.mint, &accounts.sharing_config);
            }
        }
        if launch_watch::is_watching() {
            if let PumpfunInstruction::MigrateBondingCurveCreator { accounts, .. } = input.decoded_instruction {
                launch_watch::observe_creator_migration(&accounts.mint, &accounts.sharing_config, tx.slot, tx.index);
            }
            launch_watch::observe_trades(tx, &crate::config::trading_wallet());
        }
        if super::position_feed::is_active() {
            super::position_feed::observe_trades(tx, &crate::config::trading_wallet());
        }
        if super::launch_outcome::is_tracking() {
            super::launch_outcome::observe_trades(tx);
        }
        if crate::hot::positions::is_tracking() {
            crate::hot::positions::observe(tx);
        }
        // Every pump.fun buy's block position and bundle flag, for the bid rings (hot::bids).
        crate::hot::bids::note_executed(tx);
        // Research: every launch's first two minutes of trades (logs/tapes).
        crate::utils::tape_recorder::observe(tx);
        // The snipe test's graduations (SNIPE_TEST_POOL=on): each curve's last moment under 60 SOL
        // and its completion.
        super::graduation::observe_pump(tx);
        // The whale rule (WHALE_MODE=1): every young coin's state.
        super::whale::observe(tx);
        // bundle10's shadow log (BUNDLE10_SHADOW): its candidates' create slots and trades.
        if crate::config::bundle10::shadow() {
            super::bundle10_shadow::observe(tx);
        }
        // The reference wallets' own trades keep the followed-deployer list current (REFERENCE_WALLETS).
        // They sign them, so a key check skips every other transaction cheaply; the signature
        // check counts each one once.
        if tx.message.static_account_keys().iter().any(rules::is_reference_wallet) && crate::utils::reference::first_time(&tx.signature) {
            for body in cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR) {
                if !read_pubkey(&body, 49).is_ok_and(|user| rules::is_reference_wallet(&user)) {
                    continue;
                }
                if let (Ok(coin), Ok(sol), Ok(creator)) = (read_pubkey(&body, 0), read_u64(&body, 32), read_pubkey(&body, 169)) {
                    crate::utils::reference::note_trade(&coin, &creator, body.get(48) == Some(&1), sol, now_unix());
                }
            }
        }
        Ok(())
    }
}

/// A pump.fun create seen on the stream: records it and hands it to the buy path.
fn handle_launch(tx: &carbon_core::transaction::TransactionMetadata, mint: Pubkey, detected_at: Instant) {
    {
        let Some(event) = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &CREATE_EVENT_DISCRIMINATOR)
            .iter()
            .filter_map(|body| parse_create_event(body).ok())
            .find(|event| event.mint == mint)
        else {
            debug!("no CreateEvent for {mint} in {}", tx.signature);
            return;
        };
        if event.quote_mint != Pubkey::default() && event.quote_mint != WSOL_MINT {
            return;
        }
        if event.token_program != TOKEN_PROGRAM_ID && event.token_program != TOKEN_2022_PROGRAM_ID {
            return;
        }

        // A dev buy in the same transaction moves the curve; its TradeEvent has the
        // reserves after the trade and the effective creator.
        let mut reserves = event.reserves;
        let mut creator = event.creator;
        let mut first_buy = None;
        for body in cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR) {
            if read_pubkey(&body, 0).ok() != Some(mint) {
                continue;
            }
            observe_first_buy(&mut first_buy, &body);
            let (Ok(vq), Ok(vt), Ok(rq), Ok(rt), Ok(trade_creator)) = (
                read_u64(&body, 89),
                read_u64(&body, 97),
                read_u64(&body, 105),
                read_u64(&body, 113),
                read_pubkey(&body, 169),
            ) else {
                continue;
            };
            reserves = CurveReserves {
                virtual_token_reserves: vt,
                virtual_sol_reserves: vq,
                real_token_reserves: rt,
                real_sol_reserves: rq,
            };
            creator = trade_creator;
        }

        super::shred_feed::regular_saw(&mint, tx.slot);
        // Every launch goes into the deployer history, including ones the rules skip.
        let prior_launches = observe_launch(&event.user, &mint, reserves.real_sol_reserves, now_unix());
        let dev_buy_lamports = first_buy.unwrap_or(0);
        crate::utils::tape_recorder::start(mint, tx, event.user, creator, &event.uri, &event.name, event.is_mayhem_mode);
        // The fee payer's balance before the create (the study's "deployer SOL before").
        let deployer_lamports = tx.meta.pre_balances.first().copied();
        let metadata_host = super::common::uri_host(&event.uri);
        let market = PumpFunMarket::new(mint, event.token_program, creator, event.is_mayhem_mode, reserves);
        spawn_snipe(SnipeRequest {
            market: Market::PumpFun(market),
            signature: tx.signature,
            slot: tx.slot,
            detected_at,
            needs_refresh: false,
            prior_launches: Some(prior_launches),
            deployer: Some(event.user),
            mayhem: event.is_mayhem_mode,
            fast_rug: rules::fast_rug_profile(metadata_host.as_deref(), dev_buy_lamports),
            metadata_host,
            dev_buy_lamports,
            deployer_lamports,
            balance_from_rpc: None,
            pre_execution: false,
            bundle_wallets: None,
            name_all_caps: Some(rules::name_is_all_caps(&event.name)),
            facts: None,
        });
    }
}

/// First buy TradeEvent's SOL amount, excluding subsequent bundled buys.
fn observe_first_buy(first: &mut Option<u64>, body: &[u8]) {
    if first.is_none() && body.get(48) == Some(&1) {
        *first = read_u64(body, 32).ok();
    }
}

#[cfg(test)]
mod first_buy_tests {
    use super::*;
    #[test]
    fn bundled_buys_do_not_replace_small_first_buy() {
        let mut first = None;
        let mut body = vec![0; 49];
        body[32..40].copy_from_slice(&499_999_999u64.to_le_bytes());
        observe_first_buy(&mut first, &body); 
        assert_eq!(first, None);
        body[48] = 1;
        observe_first_buy(&mut first, &body);
        body[32..40].copy_from_slice(&3_000_000_000u64.to_le_bytes());
        observe_first_buy(&mut first, &body);
        assert_eq!(first, Some(499_999_999));
        assert!(first.unwrap() < 500_000_000);
        let mut exact = None;
        body[32..40].copy_from_slice(&500_000_000u64.to_le_bytes());
        observe_first_buy(&mut exact, &body);
        assert_eq!(exact, Some(500_000_000));
    }
}
