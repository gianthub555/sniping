use {
    super::common::{cpi_event_payloads, spawn_snipe, SnipeRequest},
    crate::{
        instructions::{
            pumpswap_buy_ix::{
                pool_authority, PumpSwapMarket, CREATE_POOL_EVENT_DISCRIMINATOR,
                INIT_BOOST_EVENT_DISCRIMINATOR, PUMPSWAP_PROGRAM_ID,
            },
            types::Market,
        },
        utils::{
            parse::{read_pubkey, read_u128, read_u64},
            utils::{TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor},
    carbon_pump_swap_decoder::instructions::PumpSwapInstruction,
    log::debug,
    solana_pubkey::Pubkey,
    std::time::Instant,
};



pub struct PumpSwapProcess;

impl<'a> Processor<InstructionProcessorInputType<'a, PumpSwapInstruction>> for PumpSwapProcess {
    async fn process(&mut self, input: &InstructionProcessorInputType<'a, PumpSwapInstruction>) -> CarbonResult<()> {
        crate::utils::latency::observe(&input.metadata.transaction_metadata);
        let PumpSwapInstruction::CreatePool { data, accounts, .. } = input.decoded_instruction else {
            return Ok(());
        };
        let detected_at = Instant::now();
        if accounts.quote_mint != WSOL_MINT || accounts.base_mint == WSOL_MINT {
            return Ok(());
        }
        let canonical = data.index == 0 && accounts.creator == pool_authority(&accounts.base_mint);
        if !canonical && accounts.base_token_program != TOKEN_PROGRAM_ID {
            debug!("skip PumpSwap pool {}: non-canonical Token-2022 base", accounts.pool);
            return Ok(());
        }

        let tx = &input.metadata.transaction_metadata;
        let (base_amount, quote_amount) =
            cpi_event_payloads(tx, &PUMPSWAP_PROGRAM_ID, &CREATE_POOL_EVENT_DISCRIMINATOR)
                .iter()
                .find(|p| read_pubkey(p, 165).ok() == Some(accounts.pool))
                .and_then(|p| Some((read_u64(p, 124).ok()?, read_u64(p, 132).ok()?)))
                .unwrap_or((data.base_amount_in, data.quote_amount_in));
        let boost = cpi_event_payloads(tx, &PUMPSWAP_PROGRAM_ID, &INIT_BOOST_EVENT_DISCRIMINATOR)
            .iter()
            .find(|p| read_pubkey(p, 72).ok() == Some(accounts.pool))
            .and_then(|p| read_u128(p, 104).ok())
            .map_or(0, |v| v as i128);

        let market = PumpSwapMarket::from_create_pool(
            accounts.pool,
            accounts.creator,
            accounts.base_mint,
            accounts.base_token_program,
            accounts.pool_base_token_account,
            accounts.pool_quote_token_account,
            if canonical { data.coin_creator } else { Pubkey::default() },
            data.is_mayhem_mode,
            data.is_cashback_coin.0,
            base_amount,
            quote_amount,
            boost,
        );
        spawn_snipe(SnipeRequest {
            market: Market::PumpSwap(market),
            signature: tx.signature,
            slot: tx.slot,
            detected_at,
            needs_refresh: false,
            prior_launches: None,
            deployer: None,
            mayhem: false,
            metadata_host: None,
            dev_buy_lamports: 0,
            deployer_lamports: None,
            balance_from_rpc: None,
            fast_rug: false,
            pre_execution: false,
            bundle_wallets: None,
            name_all_caps: None,
            facts: None,
        });
        Ok(())
    }
}
