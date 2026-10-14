use {
    super::common::{spawn_snipe, transaction_instructions, SnipeRequest},
    crate::instructions::{
        raydium_clmm_swap::{RaydiumClmmMarket, RAYDIUM_CLMM_PROGRAM_ID},
        types::Market,
    },
    carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor},
    carbon_raydium_clmm_decoder::instructions::RaydiumClmmInstruction,
    log::debug,
    std::time::Instant,
};


const OPEN_POSITION_INSTRUCTIONS: [([u8; 8], usize); 3] = [
    ([0x4d, 0xff, 0xae, 0x52, 0x7d, 0x1d, 0xc9, 0x2e], 4), 
    ([0x4d, 0xb8, 0x4a, 0xd6, 0x70, 0x56, 0xf1, 0xc7], 5), 
    ([0x87, 0x80, 0x2f, 0x4d, 0x0f, 0x98, 0xf0, 0x31], 5), 
];



pub struct RayClmmProcess;

impl<'a> Processor<InstructionProcessorInputType<'a, RaydiumClmmInstruction>> for RayClmmProcess {
    async fn process(&mut self, input: &InstructionProcessorInputType<'a, RaydiumClmmInstruction>) -> CarbonResult<()> {
        crate::utils::latency::observe(&input.metadata.transaction_metadata);
        let detected_at = Instant::now();
        let market = match input.decoded_instruction {
            RaydiumClmmInstruction::CreatePool { accounts: a, .. } => RaydiumClmmMarket::from_create(
                a.pool_state,
                a.amm_config,
                a.observation_state,
                a.token_mint0,
                a.token_mint1,
                a.token_vault0,
                a.token_vault1,
                a.token_program0,
                a.token_program1,
            ),
            RaydiumClmmInstruction::CreateCustomizablePool { accounts: a, .. } => RaydiumClmmMarket::from_create(
                a.pool_state,
                a.amm_config,
                a.observation_state,
                a.token_mint0,
                a.token_mint1,
                a.token_vault0,
                a.token_vault1,
                a.token_program0,
                a.token_program1,
            ),
            _ => None,
        };
        let Some(market) = market else {
            return Ok(());
        };

        let tx = &input.metadata.transaction_metadata;
        let opens_position = transaction_instructions(tx).iter().any(|ix| {
            ix.program_id == RAYDIUM_CLMM_PROGRAM_ID
                && OPEN_POSITION_INSTRUCTIONS
                    .iter()
                    .any(|(disc, pool_index)| ix.data.starts_with(disc) && ix.accounts.get(*pool_index) == Some(&market.pool))
        });
        if !opens_position {
            debug!("skip CLMM pool {}: created without liquidity", market.pool);
            return Ok(());
        }
        spawn_snipe(SnipeRequest {
            market: Market::RaydiumClmm(market),
            signature: tx.signature,
            slot: tx.slot,
            detected_at,
            needs_refresh: true,
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
