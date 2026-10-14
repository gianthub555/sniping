use {
    super::{
        pumpfun_buy_ix::{pumpfun_config, PumpFunMarket, PUMP_EVENT_AUTHORITY, PUMP_FEE_CONFIG, PUMP_GLOBAL},
        pumpswap_buy_ix::{PUMP_FEE_PROGRAM_ID, PUMP_PROGRAM_ID},
    },
    crate::utils::utils::{close_token_account, get_associated_token_address, SYSTEM_PROGRAM_ID},
    anyhow::Result,
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
};

const SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

impl PumpFunMarket {
    
    
    pub fn sell_instructions(&self, payer: &Pubkey, tokens_in: u64, min_out: u64) -> Result<Vec<Instruction>> {
        let config = pumpfun_config()?;
        let user_token_account = get_associated_token_address(payer, &self.mint, &self.token_program);
        let mut accounts = vec![
            AccountMeta::new_readonly(PUMP_GLOBAL, false),
            AccountMeta::new(self.fee_recipient(&config)?, false),
            AccountMeta::new_readonly(self.mint, false),
            AccountMeta::new(self.bonding_curve, false),
            AccountMeta::new(self.associated_bonding_curve(), false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new(self.creator_vault(), false),
            AccountMeta::new_readonly(self.token_program, false),
            AccountMeta::new_readonly(PUMP_EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
            AccountMeta::new_readonly(PUMP_FEE_CONFIG, false),
            AccountMeta::new_readonly(PUMP_FEE_PROGRAM_ID, false),
        ];
        if self.is_cashback_coin {
            accounts.push(AccountMeta::new(Self::user_volume_accumulator(payer), false));
        }
        accounts.push(AccountMeta::new_readonly(self.bonding_curve_v2(), false));
        accounts.push(AccountMeta::new(Self::buyback_fee_recipient(&config)?, false));

        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&SELL_DISCRIMINATOR);
        data.extend_from_slice(&tokens_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        Ok(vec![
            Instruction { program_id: PUMP_PROGRAM_ID, accounts, data },
            
            close_token_account(&user_token_account, payer, payer, &self.token_program),
        ])
    }
}
