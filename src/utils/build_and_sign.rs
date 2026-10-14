use {
    super::{
        blockhash::get_recent_blockhash,
        utils::{set_compute_unit_limit, set_compute_unit_price},
    },
    crate::{
        config::{PAYER, TRADE_SETTING},
        error::SniperError,
    },
    anyhow::Result,
    solana_instruction::Instruction,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
};


pub const PACKET_DATA_SIZE: usize = 1232;




pub fn build_and_sign(
    instructions: Vec<Instruction>,
    tips: Vec<Instruction>,
) -> Result<VersionedTransaction> {
    build_and_sign_with_fee(instructions, tips, TRADE_SETTING.priority_fee_micro_lamports)
}


pub fn build_and_sign_with_fee(
    instructions: Vec<Instruction>,
    tips: Vec<Instruction>,
    priority_fee: u64,
) -> Result<VersionedTransaction> {
    build_and_sign_with_limit(instructions, tips, priority_fee, TRADE_SETTING.compute_unit_limit)
}

/// `build_and_sign_with_fee` with an explicit compute limit (sells use their own).
pub fn build_and_sign_with_limit(
    instructions: Vec<Instruction>,
    tips: Vec<Instruction>,
    priority_fee: u64,
    compute_unit_limit: u32,
) -> Result<VersionedTransaction> {
    let payer = &*PAYER;
    let message = build_message(instructions, tips, &payer.pubkey(), priority_fee, compute_unit_limit)?;
    let transaction = sign_unless_paper(VersionedMessage::Legacy(message))?;
    check_size(&transaction)?;
    Ok(transaction)
}

/// The wallet's signature, live; in DRY_RUN a blank one (paper runs never sign with the wallet's
/// key, 2026-10-12): they only simulate, with signature checks off.
fn sign_unless_paper(message: VersionedMessage) -> Result<VersionedTransaction> {
    if TRADE_SETTING.dry_run {
        let n = message.header().num_required_signatures as usize;
        return Ok(VersionedTransaction { signatures: vec![Signature::default(); n], message });
    }
    Ok(VersionedTransaction::try_new(message, &[PAYER.as_ref()])?)
}



pub fn build_unsigned(
    instructions: Vec<Instruction>,
    tips: Vec<Instruction>,
    payer: &Pubkey,
) -> Result<VersionedTransaction> {
    build_unsigned_with_fee(instructions, tips, payer, TRADE_SETTING.priority_fee_micro_lamports)
}


pub fn build_unsigned_with_fee(
    instructions: Vec<Instruction>,
    tips: Vec<Instruction>,
    payer: &Pubkey,
    priority_fee: u64,
) -> Result<VersionedTransaction> {
    build_unsigned_with_limit(instructions, tips, payer, priority_fee, TRADE_SETTING.compute_unit_limit)
}

/// `build_unsigned_with_fee` with an explicit compute limit.
pub fn build_unsigned_with_limit(
    instructions: Vec<Instruction>,
    tips: Vec<Instruction>,
    payer: &Pubkey,
    priority_fee: u64,
    compute_unit_limit: u32,
) -> Result<VersionedTransaction> {
    let message = build_message(instructions, tips, payer, priority_fee, compute_unit_limit)?;
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default(); message.header.num_required_signatures as usize],
        message: VersionedMessage::Legacy(message),
    };
    check_size(&transaction)?;
    Ok(transaction)
}

/// One copy of a fanned-out buy (`utils::nonce`): `AdvanceNonceAccount` first (the runtime looks for
/// it there), the nonce value as the blockhash, then the usual compute budget, tips and
/// instructions. Every copy of a buy uses the same lease, so at most one can land.
pub fn build_and_sign_nonced(
    instructions: Vec<Instruction>,
    tips: Vec<Instruction>,
    priority_fee: u64,
    lease: &super::nonce::Lease,
) -> Result<VersionedTransaction> {
    let setting = &*TRADE_SETTING;
    let payer = &*PAYER;
    let mut all = Vec::with_capacity(instructions.len() + tips.len() + 3);
    all.push(super::nonce::advance_instruction(&lease.account, &payer.pubkey()));
    if setting.compute_unit_limit > 0 {
        all.push(set_compute_unit_limit(setting.compute_unit_limit));
    }
    if priority_fee > 0 {
        all.push(set_compute_unit_price(priority_fee));
    }
    all.extend(tips);
    all.extend(instructions);
    let message = Message::new_with_blockhash(&all, Some(&payer.pubkey()), &lease.value);
    let transaction = sign_unless_paper(VersionedMessage::Legacy(message))?;
    check_size(&transaction)?;
    Ok(transaction)
}

fn build_message(instructions: Vec<Instruction>, tips: Vec<Instruction>, payer: &Pubkey, priority_fee: u64, compute_unit_limit: u32) -> Result<Message> {
    let mut all = Vec::with_capacity(instructions.len() + tips.len() + 2);
    if compute_unit_limit > 0 {
        all.push(set_compute_unit_limit(compute_unit_limit));
    }
    if priority_fee > 0 {
        all.push(set_compute_unit_price(priority_fee));
    }
    all.extend(tips);
    all.extend(instructions);
    let blockhash = get_recent_blockhash()?;
    Ok(Message::new_with_blockhash(&all, Some(payer), &blockhash))
}

fn check_size(transaction: &VersionedTransaction) -> Result<()> {
    let size = bincode::serialized_size(transaction)? as usize;
    if size > PACKET_DATA_SIZE {
        return Err(SniperError::TransactionTooLarge(size).into());
    }
    Ok(())
}

pub fn serialize_base64(transaction: &VersionedTransaction) -> Result<String> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(bincode::serialize(transaction)?))
}
