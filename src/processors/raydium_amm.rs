use {
    super::common::{spawn_snipe, SnipeRequest},
    crate::instructions::{raydium_amm_swap_base_in::RaydiumAmmMarket, types::Market},
    carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor},
    carbon_raydium_amm_v4_decoder::instructions::RaydiumAmmV4Instruction,
    std::time::Instant,
};



pub struct RaydiumV4Process;

impl<'a> Processor<InstructionProcessorInputType<'a, RaydiumAmmV4Instruction>> for RaydiumV4Process {
    async fn process(&mut self, input: &InstructionProcessorInputType<'a, RaydiumAmmV4Instruction>) -> CarbonResult<()> {
        crate::utils::latency::observe(&input.metadata.transaction_metadata);
        let RaydiumAmmV4Instruction::Initialize2 { data, accounts, .. } = input.decoded_instruction else {
            return Ok(());
        };
        let Some(market) = RaydiumAmmMarket::from_initialize(
            accounts.amm,
            accounts.amm_coin_mint,
            accounts.amm_pc_mint,
            accounts.amm_coin_vault,
            accounts.amm_pc_vault,
            data.init_coin_amount,
            data.init_pc_amount,
            data.open_time,
        ) else {
            return Ok(());
        };
        spawn_snipe(SnipeRequest {
            market: Market::RaydiumAmm(market),
            signature: input.metadata.transaction_metadata.signature,
            slot: input.metadata.transaction_metadata.slot,
            detected_at: Instant::now(),
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
