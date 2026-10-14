pub mod common;
pub mod deployer_balances;
pub mod fee_sharing;
pub mod graduation;
pub mod whale;
pub mod bundle10_shadow;
pub mod preprocessed;
pub mod launch_outcome;
pub mod launch_watch;
pub mod lookup_tables;
pub mod position_feed;
pub mod pumpfun;
pub mod shred_feed;
pub mod shred_wire;
pub mod pumpswap;
pub mod raydium_amm;
pub mod raydium_clmm;
pub mod raydium_cpmm;
pub mod raydium_launchpad;

pub use {
    pumpfun::PumpfunProcess, pumpswap::PumpSwapProcess, raydium_amm::RaydiumV4Process, raydium_clmm::RayClmmProcess,
    raydium_cpmm::RayCpmmProcess, raydium_launchpad::RayLaunchPadProcess,
};

use {
    crate::{
        config::Dex,
        instructions::{
            pumpswap_buy_ix::{PUMPSWAP_PROGRAM_ID, PUMP_PROGRAM_ID},
            raydium_amm_swap_base_in::RAYDIUM_AMM_V4_PROGRAM_ID,
            raydium_clmm_swap::RAYDIUM_CLMM_PROGRAM_ID,
            raydium_cpmm_swap_base_in::RAYDIUM_CPMM_PROGRAM_ID,
            raydium_launchpad_buy_ix::RAYDIUM_LAUNCHPAD_PROGRAM_ID,
        },
    },
    solana_pubkey::Pubkey,
};


pub fn dex_program_id(dex: Dex) -> Pubkey {
    match dex {
        Dex::PumpFun => PUMP_PROGRAM_ID,
        Dex::PumpSwap => PUMPSWAP_PROGRAM_ID,
        Dex::RaydiumAmm => RAYDIUM_AMM_V4_PROGRAM_ID,
        Dex::RaydiumCpmm => RAYDIUM_CPMM_PROGRAM_ID,
        Dex::RaydiumClmm => RAYDIUM_CLMM_PROGRAM_ID,
        Dex::RaydiumLaunchpad => RAYDIUM_LAUNCHPAD_PROGRAM_ID,
    }
}
