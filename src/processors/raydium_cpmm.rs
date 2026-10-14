use {
    super::common::{spawn_snipe, SnipeRequest},
    crate::instructions::{raydium_cpmm_swap_base_in::RaydiumCpmmMarket, types::Market},
    carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor},
    carbon_raydium_cpmm_decoder::instructions::RaydiumCpmmInstruction,
    std::time::Instant,
};




pub struct RayCpmmProcess;

impl<'a> Processor<InstructionProcessorInputType<'a, RaydiumCpmmInstruction>> for RayCpmmProcess {
    async fn process(&mut self, input: &InstructionProcessorInputType<'a, RaydiumCpmmInstruction>) -> CarbonResult<()> {
        crate::utils::latency::observe(&input.metadata.transaction_metadata);
        let market = match input.decoded_instruction {
            RaydiumCpmmInstruction::Initialize { data, accounts, .. } => RaydiumCpmmMarket::from_initialize(
                accounts.pool_state,
                accounts.amm_config,
                accounts.observation_state,
                accounts.token0_mint,
                accounts.token1_mint,
                accounts.token0_vault,
                accounts.token1_vault,
                accounts.token0_program,
                accounts.token1_program,
                data.open_time,
            ),
            RaydiumCpmmInstruction::InitializeWithPermission { data, accounts, .. } => {
                RaydiumCpmmMarket::from_initialize(
                    accounts.pool_state,
                    accounts.amm_config,
                    accounts.observation_state,
                    accounts.token0_mint,
                    accounts.token1_mint,
                    accounts.token0_vault,
                    accounts.token1_vault,
                    accounts.token0_program,
                    accounts.token1_program,
                    data.open_time,
                )
            }
            _ => None,
        };
        let Some(market) = market else {
            return Ok(());
        };
        spawn_snipe(SnipeRequest {
            market: Market::RaydiumCpmm(market),
            signature: input.metadata.transaction_metadata.signature,
            slot: input.metadata.transaction_metadata.slot,
            detected_at: Instant::now(),
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
