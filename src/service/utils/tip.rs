use {
    crate::utils::utils::transfer_sol,
    rand::seq::SliceRandom,
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
};



pub fn tip_instruction(payer: &Pubkey, tip_accounts: &[Pubkey], lamports: u64) -> Instruction {
    let account = tip_accounts
        .choose(&mut rand::thread_rng())
        .expect("tip account list is never empty");
    transfer_sol(payer, account, lamports)
}
