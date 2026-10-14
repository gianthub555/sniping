use {
    super::common::{cpi_event_payloads, spawn_snipe, SnipeRequest},
    crate::{
        instructions::{
            raydium_launchpad_buy_ix::{LaunchpadCurve, RaydiumLaunchpadMarket, RAYDIUM_LAUNCHPAD_PROGRAM_ID},
            types::Market,
        },
        utils::{
            parse::{read_pubkey, read_u64, TransferFee},
            utils::{TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor},
    carbon_raydium_launchpad_decoder::{instructions::RaydiumLaunchpadInstruction, types::CurveParams},
    log::debug,
    solana_pubkey::Pubkey,
    std::time::Instant,
};

const TRADE_EVENT_DISCRIMINATOR: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];


pub struct RayLaunchPadProcess;

struct Launch {
    pool: Pubkey,
    global_config: Pubkey,
    platform_config: Pubkey,
    creator: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    base_token_program: Pubkey,
    quote_token_program: Pubkey,
    constant_curve: bool,
    transfer_fee: Option<TransferFee>,
}

impl<'a> Processor<InstructionProcessorInputType<'a, RaydiumLaunchpadInstruction>> for RayLaunchPadProcess {
    async fn process(&mut self, input: &InstructionProcessorInputType<'a, RaydiumLaunchpadInstruction>) -> CarbonResult<()> {
        crate::utils::latency::observe(&input.metadata.transaction_metadata);
        let detected_at = Instant::now();
        let is_constant = |curve: &CurveParams| matches!(curve, CurveParams::Constant { .. });
        let launch = match input.decoded_instruction {
            RaydiumLaunchpadInstruction::InitializeV2 { data, accounts, .. } => Launch {
                pool: accounts.pool_state,
                global_config: accounts.global_config,
                platform_config: accounts.platform_config,
                creator: accounts.creator,
                base_mint: accounts.base_mint,
                quote_mint: accounts.quote_mint,
                base_vault: accounts.base_vault,
                quote_vault: accounts.quote_vault,
                base_token_program: accounts.base_token_program,
                quote_token_program: accounts.quote_token_program,
                constant_curve: is_constant(&data.curve_param),
                transfer_fee: None,
            },
            RaydiumLaunchpadInstruction::InitializeWithToken2022 { data, accounts, .. } => Launch {
                pool: accounts.pool_state,
                global_config: accounts.global_config,
                platform_config: accounts.platform_config,
                creator: accounts.creator,
                base_mint: accounts.base_mint,
                quote_mint: accounts.quote_mint,
                base_vault: accounts.base_vault,
                quote_vault: accounts.quote_vault,
                base_token_program: accounts.base_token_program,
                quote_token_program: accounts.quote_token_program,
                constant_curve: is_constant(&data.curve_param),
                transfer_fee: data
                    .transfer_fee_extension_param
                    .as_ref()
                    .map(|p| TransferFee::fixed(p.transfer_fee_basis_points, p.maximum_fee)),
            },
            RaydiumLaunchpadInstruction::Initialize { data, accounts, .. } => Launch {
                pool: accounts.pool_state,
                global_config: accounts.global_config,
                platform_config: accounts.platform_config,
                creator: accounts.creator,
                base_mint: accounts.base_mint,
                quote_mint: accounts.quote_mint,
                base_vault: accounts.base_vault,
                quote_vault: accounts.quote_vault,
                base_token_program: accounts.base_token_program,
                quote_token_program: accounts.quote_token_program,
                constant_curve: is_constant(&data.curve_param),
                transfer_fee: None,
            },
            _ => return Ok(()),
        };
        if launch.quote_mint != WSOL_MINT || launch.quote_token_program != TOKEN_PROGRAM_ID {
            return Ok(());
        }
        if !launch.constant_curve {
            debug!("skip LaunchLab pool {}: only constant-product curves are supported", launch.pool);
            return Ok(());
        }

        // The creator usually buys in the same transaction; its TradeEvent carries the
        // curve state after that buy. Without one, the state is loaded over RPC.
        let tx = &input.metadata.transaction_metadata;
        let curve = cpi_event_payloads(tx, &RAYDIUM_LAUNCHPAD_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR)
            .iter()
            .rfind(|p| read_pubkey(p, 0).ok() == Some(launch.pool))
            .and_then(|p| {
                Some(LaunchpadCurve {
                    total_base_sell: read_u64(p, 32).ok()?,
                    virtual_base: read_u64(p, 40).ok()?,
                    virtual_quote: read_u64(p, 48).ok()?,
                    real_base: read_u64(p, 72).ok()?,
                    real_quote: read_u64(p, 80).ok()?,
                })
            });
        let needs_refresh = curve.is_none();
        let curve = curve.unwrap_or(LaunchpadCurve {
            virtual_base: 0,
            virtual_quote: 0,
            real_base: 0,
            real_quote: 0,
            total_base_sell: 0,
        });

        let market = RaydiumLaunchpadMarket::new(
            launch.pool,
            launch.global_config,
            launch.platform_config,
            launch.creator,
            launch.base_mint,
            launch.base_vault,
            launch.quote_vault,
            launch.base_token_program,
            curve,
            launch.transfer_fee,
        );
        spawn_snipe(SnipeRequest {
            market: Market::RaydiumLaunchpad(market),
            signature: tx.signature,
            slot: tx.slot,
            detected_at,
            needs_refresh,
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
