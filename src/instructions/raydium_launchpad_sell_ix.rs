use {
    super::{raydium_launchpad_buy_ix::{RaydiumLaunchpadMarket, RAYDIUM_LAUNCHPAD_PROGRAM_ID}, types::sol_sell_wrapper},
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
};

const SELL_EXACT_IN_DISCRIMINATOR: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26];

impl RaydiumLaunchpadMarket {
    pub fn sell_instructions(&self, payer: &Pubkey, tokens_in: u64, min_out: u64) -> Vec<Instruction> {
        let mut data = Vec::with_capacity(32);
        data.extend_from_slice(&SELL_EXACT_IN_DISCRIMINATOR);
        data.extend_from_slice(&tokens_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); 
        let swap = Instruction {
            program_id: RAYDIUM_LAUNCHPAD_PROGRAM_ID,
            accounts: self.swap_accounts(payer),
            data,
        };
        let close_token_account = self.base_transfer_fee.is_none();
        sol_sell_wrapper(payer, &self.base_mint, &self.base_token_program, close_token_account, swap)
    }
}
