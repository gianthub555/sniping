use {
    super::{
        pumpswap_buy_ix::{pumpswap_config, PumpSwapMarket, PUMPSWAP_FEE_CONFIG, PUMPSWAP_PROGRAM_ID, PUMP_FEE_PROGRAM_ID},
        types::sol_sell_wrapper,
    },
    crate::utils::utils::{get_associated_token_address, TOKEN_PROGRAM_ID, WSOL_MINT},
    anyhow::Result,
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
};

const SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

impl PumpSwapMarket {
    pub fn sell_instructions(&self, payer: &Pubkey, tokens_in: u64, min_out: u64) -> Result<Vec<Instruction>> {
        let config = pumpswap_config()?;
        let mut accounts = self.common_accounts(payer, &config)?;
        accounts.extend([
            AccountMeta::new_readonly(PUMPSWAP_FEE_CONFIG, false),
            AccountMeta::new_readonly(PUMP_FEE_PROGRAM_ID, false),
        ]);
        if self.is_cashback_coin {
            let user_volume_accumulator = Self::user_volume_accumulator(payer);
            accounts.push(AccountMeta::new(
                get_associated_token_address(&user_volume_accumulator, &WSOL_MINT, &TOKEN_PROGRAM_ID),
                false,
            ));
            accounts.push(AccountMeta::new(user_volume_accumulator, false));
        }
        accounts.extend(self.pool_v2_and_buyback_accounts(&config)?);

        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&SELL_DISCRIMINATOR);
        data.extend_from_slice(&tokens_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        let swap = Instruction { program_id: PUMPSWAP_PROGRAM_ID, accounts, data };
        
        Ok(sol_sell_wrapper(payer, &self.base_mint, &self.base_token_program, true, swap))
    }
}
