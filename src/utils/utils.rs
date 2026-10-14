use {
    anyhow::{bail, Result},
    solana_commitment_config::CommitmentConfig,
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_signature::Signature,
    solana_rpc_client_api::config::RpcTransactionConfig,
    solana_transaction_status_client_types::{TransactionConfirmationStatus, UiTransactionEncoding},
    std::time::{Duration, Instant},
};

pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
pub const WSOL_MINT: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
pub const MEMO_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

pub fn get_associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}


pub fn create_associated_token_account_idempotent(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(get_associated_token_address(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

pub fn transfer_sol(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction { program_id: COMPUTE_BUDGET_PROGRAM_ID, accounts: vec![], data }
}

/// Caps the account data (programs included) the transaction may load. Block producers rank by
/// fee over cost, and the cost counts 8 CU per 32 KiB of this limit: 16,384 CU at the 64 MiB
/// default, 4,096 at 16 MiB.
pub fn set_loaded_accounts_data_size_limit(bytes: u32) -> Instruction {
    let mut data = vec![4u8];
    data.extend_from_slice(&bytes.to_le_bytes());
    Instruction { program_id: COMPUTE_BUDGET_PROGRAM_ID, accounts: vec![], data }
}

pub fn set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction { program_id: COMPUTE_BUDGET_PROGRAM_ID, accounts: vec![], data }
}


pub fn sync_native(account: &Pubkey) -> Instruction {
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*account, false)],
        data: vec![17],
    }
}


pub fn close_token_account(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9],
    }
}


pub fn wrap_sol_instructions(payer: &Pubkey, lamports: u64) -> Vec<Instruction> {
    let wsol_ata = get_associated_token_address(payer, &WSOL_MINT, &TOKEN_PROGRAM_ID);
    vec![
        create_associated_token_account_idempotent(payer, payer, &WSOL_MINT, &TOKEN_PROGRAM_ID),
        transfer_sol(payer, &wsol_ata, lamports),
        sync_native(&wsol_ata),
    ]
}


pub fn unwrap_sol_instruction(payer: &Pubkey) -> Instruction {
    let wsol_ata = get_associated_token_address(payer, &WSOL_MINT, &TOKEN_PROGRAM_ID);
    close_token_account(&wsol_ata, payer, payer, &TOKEN_PROGRAM_ID)
}



pub async fn get_token_balance(rpc: &RpcClient, token_account: &Pubkey) -> Result<u64> {
    let account = rpc
        .get_account_with_commitment(token_account, CommitmentConfig::processed())
        .await?
        .value;
    match account {
        None => Ok(0),
        Some(account) => super::parse::read_u64(&account.data, 64),
    }
}



pub async fn fee_payer_lamport_change(rpc: &RpcClient, signature: &Signature) -> Result<i64> {
    let mut attempt = 0;
    loop {
        let config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(1),
        };
        match rpc.get_transaction_with_config(signature, config).await {
            Ok(tx) => {
                let meta = tx.transaction.meta.ok_or_else(|| anyhow::anyhow!("{signature} has no status meta"))?;
                let (Some(pre), Some(post)) = (meta.pre_balances.first(), meta.post_balances.first()) else {
                    bail!("{signature} has no balances");
                };
                return Ok(*post as i64 - *pre as i64);
            }
            
            Err(_) if attempt < 6 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmOutcome {
    Confirmed { slot: u64 },
    Failed { slot: u64, error: String },
    TimedOut,
}


pub async fn confirm_signature(
    rpc: &RpcClient,
    signature: &Signature,
    timeout: Duration,
) -> Result<ConfirmOutcome> {
    let started = Instant::now();
    let mut consecutive_errors = 0;
    loop {
        match rpc.get_signature_statuses(&[*signature]).await {
            Ok(response) => {
                consecutive_errors = 0;
                if let Some(Some(status)) = response.value.into_iter().next() {
                    // On chain, landed or failed: stop re-sending it.
                    crate::utils::latency::mark_own_tx_landed(signature, status.slot);
                    if let Some(err) = status.err {
                        return Ok(ConfirmOutcome::Failed { slot: status.slot, error: format!("{err:?}") });
                    }
                    if matches!(
                        status.confirmation_status,
                        Some(TransactionConfirmationStatus::Confirmed)
                            | Some(TransactionConfirmationStatus::Finalized)
                    ) {
                        return Ok(ConfirmOutcome::Confirmed { slot: status.slot });
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    bail!("signature status polling keeps failing: {e}");
                }
            }
        }
        if started.elapsed() > timeout {
            return Ok(ConfirmOutcome::TimedOut);
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_data_limit_is_compute_budget_instruction_4() {
        let ix = set_loaded_accounts_data_size_limit(16 * 1024 * 1024);
        assert_eq!(ix.program_id, COMPUTE_BUDGET_PROGRAM_ID);
        assert_eq!(ix.data, [4, 0, 0, 0, 1]);
    }
}
