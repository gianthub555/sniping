//! The pre-built buy transaction (A4). At startup the whole buy (compute budget, tips, the
//! loaded-data limit, the token-account create and pump.fun's `buy_exact_sol_in` with its 18
//! accounts, as `PumpFunMarket::buy_instructions` builds it) is compiled once into a legacy message
//! with a placeholder for every per-launch value, and the byte offset of each placeholder is kept.
//! A launch then only overwrites those bytes (keys, amounts, fees, blockhash), signs once and lays
//! the signed transaction into a stack buffer: no allocation, no key sorting, no hashing beyond
//! the signature.

use {
    crate::{
        instructions::{
            pumpfun_buy_ix::{PUMP_EVENT_AUTHORITY, PUMP_FEE_CONFIG, PUMP_GLOBAL, PUMP_GLOBAL_VOLUME_ACCUMULATOR},
            pumpswap_buy_ix::{PUMP_FEE_PROGRAM_ID, PUMP_PROGRAM_ID},
        },
        utils::utils::{
            create_associated_token_account_idempotent, set_compute_unit_limit, set_compute_unit_price,
            set_loaded_accounts_data_size_limit, transfer_sol, SYSTEM_PROGRAM_ID,
        },
    },
    anyhow::{anyhow, bail, Result},
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::legacy::Message,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
};

pub const PACKET: usize = 1232;
/// base64 of a full packet.
pub const PACKET_B64: usize = PACKET.div_ceil(3) * 4;
/// One tip per tipping lane (`hot::lanes`).
pub const MAX_TIPS: usize = 8;

const BUY_EXACT_SOL_IN: [u8; 8] = super::decode::BUY_EXACT_SOL_IN;

/// What the template is built for (fixed until rebuilt).
#[derive(Clone, Debug)]
pub struct TemplateSpec {
    pub payer: Pubkey,
    pub cu_limit: u32,
    /// Loaded-accounts data limit (bytes), or none.
    pub loaded_data_bytes: Option<u32>,
    /// One tip transfer per entry: the lane's tip accounts (one is picked per send) and lamports.
    pub tips: Vec<(&'static [Pubkey], u64)>,
    /// The tip (index into `tips`) whose transfer also names `JITO_DONTFRONT` read-only: Jito's
    /// block engine then never places a bundle in front of it (the Jito tip's transfer).
    pub dontfront: Option<u8>,
}

/// Jito's don't-front marker: any read-only account starting `jitodontfront` in a transaction keeps
/// bundles from being placed before it.
pub const JITO_DONTFRONT: Pubkey = Pubkey::from_str_const("jitodontfront111111111111111111111111111111");

/// The per-launch values.
#[derive(Clone, Copy, Debug)]
pub struct BuyFill {
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub user_ata: Pubkey,
    pub creator_vault: Pubkey,
    pub bonding_curve_v2: Pubkey,
    pub fee_recipient: Pubkey,
    pub buyback_fee_recipient: Pubkey,
    pub lamports_in: u64,
    pub min_out: u64,
    pub cu_price: u64,
    pub blockhash: Hash,
    /// Index into each tip lane's account list (rotated per send).
    pub tip_pick: u32,
    /// Each tip's lamports, by its index in the spec's tips (0: written as a 0-lamport transfer,
    /// unpaid): LANE_ROTATE pays only the lane whose turn it is, a Jito bundle copy adds Jito's tip.
    /// None: every tip at the spec's amount.
    pub paid: Option<[u64; MAX_TIPS]>,
    /// This buy's compute limit (`hot::cu`, per token program); 0: the spec's.
    pub cu_limit: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct Offsets {
    fee_recipient: usize,
    mint: usize,
    bonding_curve: usize,
    associated_bonding_curve: usize,
    user_ata: usize,
    token_program: usize,
    creator_vault: usize,
    bonding_curve_v2: usize,
    buyback_fee_recipient: usize,
    /// The nonce account's key (a nonced template only; 0 otherwise).
    nonce_account: usize,
    blockhash: usize,
    cu_limit: usize,
    cu_price: usize,
    lamports_in: usize,
    min_out: usize,
}

pub struct BuyTemplate {
    msg: [u8; PACKET],
    len: usize,
    off: Offsets,
    tip_keys: [usize; MAX_TIPS],
    tip_amounts: [usize; MAX_TIPS],
    tip_lists: [&'static [Pubkey]; MAX_TIPS],
    n_tips: usize,
    /// Built with an AdvanceNonceAccount first (`build_nonced`): `blockhash` carries the nonce value.
    nonced: bool,
    pub spec: TemplateSpec,
}

/// Placeholders: distinct keys nothing on chain uses, found again after compilation.
fn placeholder(tag: u8) -> Pubkey {
    let mut b = [0xA5u8; 32];
    b[0] = tag;
    b[31] = 0x5A;
    Pubkey::new_from_array(b)
}

const P_FEE: u8 = 1;
const P_MINT: u8 = 2;
const P_CURVE: u8 = 3;
const P_CURVE_ATA: u8 = 4;
const P_USER_ATA: u8 = 5;
const P_TOKEN_PROGRAM: u8 = 6;
const P_CREATOR_VAULT: u8 = 7;
const P_CURVE_V2: u8 = 8;
const P_BUYBACK: u8 = 9;
const P_NONCE: u8 = 30;
const P_TIP: u8 = 16;

fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PUMP_PROGRAM_ID).0
}

/// The buy's instructions, in the order `build_and_sign::build_message` puts them, for `fill`'s
/// values (placeholders at startup; real values in the equivalence test).
pub fn instructions(spec: &TemplateSpec, fill: &BuyFill, tip_accounts: &[Pubkey]) -> Vec<Instruction> {
    instructions_nonced(spec, fill, tip_accounts, None)
}

/// `instructions` with an AdvanceNonceAccount of `nonce` (authority: the payer) first, as a durable
/// nonce transaction needs (the fan-out, 2026-10-12).
pub fn instructions_nonced(spec: &TemplateSpec, fill: &BuyFill, tip_accounts: &[Pubkey], nonce: Option<&Pubkey>) -> Vec<Instruction> {
    let payer = spec.payer;
    let mut all = Vec::with_capacity(8 + spec.tips.len());
    if let Some(account) = nonce {
        all.push(crate::utils::nonce::advance_instruction(account, &payer));
    }
    all.push(set_compute_unit_limit(spec.cu_limit));
    all.push(set_compute_unit_price(fill.cu_price));
    for (i, ((_, lamports), account)) in spec.tips.iter().zip(tip_accounts).enumerate() {
        let mut ix = transfer_sol(&payer, account, *lamports);
        if spec.dontfront == Some(i as u8) {
            ix.accounts.push(AccountMeta::new_readonly(JITO_DONTFRONT, false));
        }
        all.push(ix);
    }
    if let Some(bytes) = spec.loaded_data_bytes {
        all.push(set_loaded_accounts_data_size_limit(bytes));
    }
    // The token-account create derives the ATA itself; its account list names it.
    let mut ata = create_associated_token_account_idempotent(&payer, &payer, &fill.mint, &fill.token_program);
    ata.accounts[1].pubkey = fill.user_ata;
    all.push(ata);
    let accounts = vec![
        AccountMeta::new_readonly(PUMP_GLOBAL, false),
        AccountMeta::new(fill.fee_recipient, false),
        AccountMeta::new_readonly(fill.mint, false),
        AccountMeta::new(fill.bonding_curve, false),
        AccountMeta::new(fill.associated_bonding_curve, false),
        AccountMeta::new(fill.user_ata, false),
        AccountMeta::new(payer, true),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        AccountMeta::new_readonly(fill.token_program, false),
        AccountMeta::new(fill.creator_vault, false),
        AccountMeta::new_readonly(PUMP_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
        AccountMeta::new_readonly(PUMP_GLOBAL_VOLUME_ACCUMULATOR, false),
        AccountMeta::new(user_volume_accumulator(&payer), false),
        AccountMeta::new_readonly(PUMP_FEE_CONFIG, false),
        AccountMeta::new_readonly(PUMP_FEE_PROGRAM_ID, false),
        AccountMeta::new_readonly(fill.bonding_curve_v2, false),
        AccountMeta::new(fill.buyback_fee_recipient, false),
    ];
    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&BUY_EXACT_SOL_IN);
    data.extend_from_slice(&fill.lamports_in.to_le_bytes());
    data.extend_from_slice(&fill.min_out.to_le_bytes());
    data.push(1);
    all.push(Instruction { program_id: PUMP_PROGRAM_ID, accounts, data });
    all
}

/// Offsets of each instruction's data inside a serialized legacy message.
fn instruction_data_offsets(msg: &[u8]) -> Result<Vec<usize>> {
    let short = |b: &[u8], at: &mut usize| -> Result<usize> {
        let mut v = 0usize;
        for i in 0..3 {
            let x = *b.get(*at).ok_or_else(|| anyhow!("short read"))?;
            *at += 1;
            v |= ((x & 0x7f) as usize) << (7 * i);
            if x & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!("bad compact-u16")
    };
    let mut at = 3;
    let n_keys = short(msg, &mut at)?;
    at += 32 * n_keys + 32;
    let n_ixs = short(msg, &mut at)?;
    let mut out = Vec::with_capacity(n_ixs);
    for _ in 0..n_ixs {
        at += 1;
        let n = short(msg, &mut at)?;
        at += n;
        let n = short(msg, &mut at)?;
        out.push(at);
        at += n;
    }
    if at != msg.len() {
        bail!("message walk ended at {at} of {}", msg.len());
    }
    Ok(out)
}

impl BuyTemplate {
    pub fn build(spec: TemplateSpec) -> Result<Self> {
        Self::build_with(spec, false)
    }

    /// The template of a durable-nonce copy (HOT_NONCE_FANOUT): AdvanceNonceAccount first, the
    /// nonce account set per buy (`set_nonce_account`), the nonce value as the fill's blockhash.
    pub fn build_nonced(spec: TemplateSpec) -> Result<Self> {
        Self::build_with(spec, true)
    }

    fn build_with(spec: TemplateSpec, nonced: bool) -> Result<Self> {
        if spec.tips.len() > MAX_TIPS {
            bail!("at most {MAX_TIPS} tips");
        }
        let fill = BuyFill {
            mint: placeholder(P_MINT),
            token_program: placeholder(P_TOKEN_PROGRAM),
            bonding_curve: placeholder(P_CURVE),
            associated_bonding_curve: placeholder(P_CURVE_ATA),
            user_ata: placeholder(P_USER_ATA),
            creator_vault: placeholder(P_CREATOR_VAULT),
            bonding_curve_v2: placeholder(P_CURVE_V2),
            fee_recipient: placeholder(P_FEE),
            buyback_fee_recipient: placeholder(P_BUYBACK),
            lamports_in: 0,
            min_out: 0,
            cu_price: 0,
            blockhash: Hash::default(),
            tip_pick: 0,
            paid: None,
            cu_limit: 0,
        };
        let tips: Vec<Pubkey> = (0..spec.tips.len()).map(|i| placeholder(P_TIP + i as u8)).collect();
        let nonce_key = placeholder(P_NONCE);
        let ixs = instructions_nonced(&spec, &fill, &tips, nonced.then_some(&nonce_key));
        let message = Message::new_with_blockhash(&ixs, Some(&spec.payer), &Hash::default());
        let bytes = message.serialize();
        if bytes.len() + 65 > PACKET {
            bail!("buy template is {} bytes, more than a packet", bytes.len() + 65);
        }
        let key_at = |key: Pubkey| -> Result<usize> {
            let i = message.account_keys.iter().position(|k| *k == key).ok_or_else(|| anyhow!("placeholder {key} not in the message"))?;
            // header (3) + compact-u16 key count (1 byte below 128 keys) + 32 per key
            Ok(4 + 32 * i)
        };
        if message.account_keys.len() >= 128 {
            bail!("too many keys");
        }
        let data = instruction_data_offsets(&bytes)?;
        let n_tips = spec.tips.len();
        // [advance nonce?, limit, price, tips.., loaded-data limit?, token-account create, buy];
        // the loaded-data limit's value is fixed by the spec and already in the compiled bytes.
        let base = nonced as usize;
        let buy_ix = base + 3 + n_tips + spec.loaded_data_bytes.is_some() as usize;
        let mut tip_keys = [0; MAX_TIPS];
        let mut tip_amounts = [0; MAX_TIPS];
        let mut tip_lists: [&'static [Pubkey]; MAX_TIPS] = [&[]; MAX_TIPS];
        for (i, (list, _)) in spec.tips.iter().enumerate() {
            tip_keys[i] = key_at(tips[i])?;
            // System transfer data: u32 instruction (2), then u64 lamports.
            tip_amounts[i] = data[base + 2 + i] + 4;
            tip_lists[i] = list;
        }
        let off = Offsets {
            fee_recipient: key_at(fill.fee_recipient)?,
            mint: key_at(fill.mint)?,
            bonding_curve: key_at(fill.bonding_curve)?,
            associated_bonding_curve: key_at(fill.associated_bonding_curve)?,
            user_ata: key_at(fill.user_ata)?,
            token_program: key_at(fill.token_program)?,
            creator_vault: key_at(fill.creator_vault)?,
            bonding_curve_v2: key_at(fill.bonding_curve_v2)?,
            buyback_fee_recipient: key_at(fill.buyback_fee_recipient)?,
            nonce_account: if nonced { key_at(nonce_key)? } else { 0 },
            blockhash: 4 + 32 * message.account_keys.len(),
            cu_limit: data[base] + 1,
            cu_price: data[base + 1] + 1,
            lamports_in: data[buy_ix] + 8,
            min_out: data[buy_ix] + 16,
        };
        let mut msg = [0u8; PACKET];
        msg[..bytes.len()].copy_from_slice(&bytes);
        let mut template = BuyTemplate { msg, len: bytes.len(), off, tip_keys, tip_amounts, tip_lists, n_tips, nonced, spec };
        // The tip amounts and the compute limit are fixed by the spec.
        for (i, (_, lamports)) in template.spec.tips.clone().iter().enumerate() {
            template.put(template.tip_amounts[i], &lamports.to_le_bytes());
        }
        let cu = template.spec.cu_limit;
        template.put(template.off.cu_limit, &cu.to_le_bytes());
        Ok(template)
    }

    #[inline]
    fn put(&mut self, at: usize, bytes: &[u8]) {
        self.msg[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// Writes one launch's values into the message.
    #[inline]
    pub fn fill(&mut self, f: &BuyFill) {
        let o = self.off;
        self.put(o.fee_recipient, f.fee_recipient.as_ref());
        self.put(o.mint, f.mint.as_ref());
        self.put(o.bonding_curve, f.bonding_curve.as_ref());
        self.put(o.associated_bonding_curve, f.associated_bonding_curve.as_ref());
        self.put(o.user_ata, f.user_ata.as_ref());
        self.put(o.token_program, f.token_program.as_ref());
        self.put(o.creator_vault, f.creator_vault.as_ref());
        self.put(o.bonding_curve_v2, f.bonding_curve_v2.as_ref());
        self.put(o.buyback_fee_recipient, f.buyback_fee_recipient.as_ref());
        self.put(o.blockhash, f.blockhash.as_ref());
        self.put(o.cu_price, &f.cu_price.to_le_bytes());
        self.put(o.lamports_in, &f.lamports_in.to_le_bytes());
        self.put(o.min_out, &f.min_out.to_le_bytes());
        for i in 0..self.n_tips {
            let list = self.tip_lists[i];
            if !list.is_empty() {
                let key = list[f.tip_pick as usize % list.len()];
                self.put(self.tip_keys[i], key.as_ref());
            }
            let lamports = match &f.paid {
                Some(paid) => paid[i],
                None => self.spec.tips[i].1,
            };
            self.put(self.tip_amounts[i], &lamports.to_le_bytes());
        }
        let cu = if f.cu_limit > 0 { f.cu_limit } else { self.spec.cu_limit };
        self.put(o.cu_limit, &cu.to_le_bytes());
    }

    /// A nonced template's nonce account for this buy (its value goes in the fill's blockhash).
    #[inline]
    pub fn set_nonce_account(&mut self, account: &Pubkey) {
        if self.nonced {
            let at = self.off.nonce_account;
            self.put(at, account.as_ref());
        }
    }

    pub fn is_nonced(&self) -> bool {
        self.nonced
    }

    /// The message bytes as filled.
    pub fn message(&self) -> &[u8] {
        &self.msg[..self.len]
    }

    /// Signs the filled message and lays the transaction (one signature) into `out`; its length.
    #[inline]
    pub fn sign_into(&self, keypair: &Keypair, out: &mut [u8; PACKET]) -> usize {
        self.sign_into_with(
            |m| {
                let mut sig = [0u8; 64];
                sig.copy_from_slice(keypair.sign_message(m).as_ref());
                sig
            },
            out,
        )
    }

    /// `sign_into` with `sign` making the signature (the hot path's presigned signer,
    /// `hot::presign`).
    #[inline]
    pub fn sign_into_with(&self, sign: impl FnOnce(&[u8]) -> [u8; 64], out: &mut [u8; PACKET]) -> usize {
        let signature = sign(self.message());
        out[0] = 1;
        out[1..65].copy_from_slice(&signature);
        out[65..65 + self.len].copy_from_slice(self.message());
        65 + self.len
    }
}

/// base64 of `tx` into `out`; its length. Never allocates.
#[inline]
pub fn base64_into(tx: &[u8], out: &mut [u8; PACKET_B64]) -> usize {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode_slice(tx, out).expect("a packet fits")
}

/// A spec for `payer` with the bot's buy settings: its compute limit, loaded-data limit and the
/// tips of the current buy lanes.
pub fn spec_from_settings(payer: Pubkey) -> TemplateSpec {
    let setting = &*crate::config::TRADE_SETTING;
    let tips = crate::service::tip_amounts(crate::service::SendProfile::Buy)
        .into_iter()
        .map(|(service, lamports)| (tip_accounts(service), lamports))
        .collect();
    TemplateSpec {
        payer,
        cu_limit: setting.compute_unit_limit,
        loaded_data_bytes: setting.loaded_data_limit_bytes.map(|b| b as u32),
        tips,
        dontfront: None,
    }
}

/// A landing service's published tip accounts.
pub fn tip_accounts(service: crate::config::ConfirmService) -> &'static [Pubkey] {
    use crate::config::ConfirmService::*;
    match service {
        Jito => &crate::service::jito::JITO_TIP_ACCOUNTS,
        Nozomi => &crate::service::nozomi::NOZOMI_TIP_ACCOUNTS,
        ZeroSlot => &crate::service::zero_slot::ZERO_SLOT_TIP_ACCOUNTS,
        Helius => &crate::service::helius_sender::HELIUS_SENDER_TIP_ACCOUNTS,
        Rpc => &[],
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::utils::utils::{get_associated_token_address, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
        solana_transaction::versioned::VersionedTransaction,
    };

    /// (program, [(key, signer, writable)], data) of every instruction, writability from the header.
    fn decompiled(m: &Message) -> Vec<(Pubkey, Vec<(Pubkey, bool, bool)>, Vec<u8>)> {
        let h = m.header;
        let n = m.account_keys.len();
        let writable = |i: usize| {
            let signers = h.num_required_signatures as usize;
            if i < signers {
                i < signers - h.num_readonly_signed_accounts as usize
            } else {
                i < n - h.num_readonly_unsigned_accounts as usize
            }
        };
        m.instructions
            .iter()
            .map(|ix| {
                (
                    m.account_keys[ix.program_id_index as usize],
                    ix.accounts.iter().map(|&a| (m.account_keys[a as usize], m.is_signer(a as usize), writable(a as usize))).collect(),
                    ix.data.clone(),
                )
            })
            .collect()
    }

    fn real_fill(payer: &Pubkey, token_program: Pubkey) -> BuyFill {
        let mint = Pubkey::new_unique();
        let curve = crate::instructions::pumpfun_buy_ix::bonding_curve_address(&mint);
        BuyFill {
            mint,
            token_program,
            bonding_curve: curve,
            associated_bonding_curve: get_associated_token_address(&curve, &mint, &token_program),
            user_ata: get_associated_token_address(payer, &mint, &token_program),
            creator_vault: Pubkey::new_unique(),
            bonding_curve_v2: Pubkey::new_unique(),
            fee_recipient: Pubkey::new_unique(),
            buyback_fee_recipient: Pubkey::new_unique(),
            lamports_in: 100_000_000,
            min_out: 2_000_000_000_000,
            cu_price: 16_666_666,
            blockhash: Hash::new_from_array([3; 32]),
            tip_pick: 5,
            paid: None,
            cu_limit: 0,
        }
    }

    #[test]
    fn paid_tips_are_written_and_the_others_are_zero() {
        let keypair = Keypair::new();
        let spec = TemplateSpec {
            payer: keypair.pubkey(),
            cu_limit: 120_000,
            loaded_data_bytes: Some(16 * 1024 * 1024),
            tips: vec![(&crate::service::nozomi::NOZOMI_TIP_ACCOUNTS, 5_000), (&crate::service::jito::JITO_TIP_ACCOUNTS, 1_000_000), (&crate::service::nozomi::NOZOMI_TIP_ACCOUNTS, 2_000_000)],
            // The Jito tip's transfer names jitodontfront.
            dontfront: Some(1),
        };
        let mut template = BuyTemplate::build(spec.clone()).unwrap();
        let tip_accounts: Vec<Pubkey> = spec.tips.iter().map(|(list, _)| list[5 % list.len()]).collect();
        let one = |i: usize, lamports: u64| {
            let mut a = [0u64; MAX_TIPS];
            a[i] = lamports;
            Some(a)
        };
        let two = {
            let mut a = [0u64; MAX_TIPS];
            (a[0], a[2]) = (7_000, 2_500_000);
            Some(a)
        };
        for paid in [one(1, 1_000_000), one(0, 5_000), None, two] {
            let fill = BuyFill { paid, ..real_fill(&keypair.pubkey(), TOKEN_PROGRAM_ID) };
            template.fill(&fill);
            let mut out = [0u8; PACKET];
            let n = template.sign_into(&keypair, &mut out);
            let tx: VersionedTransaction = bincode::deserialize(&out[..n]).expect("a valid transaction");
            let solana_message::VersionedMessage::Legacy(got) = &tx.message else { panic!("legacy") };
            let with = TemplateSpec {
                tips: spec.tips.iter().enumerate().map(|(i, (list, l))| (*list, paid.map_or(*l, |a| a[i]))).collect(),
                ..spec.clone()
            };
            let expected = Message::new_with_blockhash(&instructions(&with, &fill, &tip_accounts), Some(&spec.payer), &fill.blockhash);
            assert_eq!(decompiled(got), decompiled(&expected), "paid {paid:?}: those tips paid, the others 0");
            assert!(got.account_keys.contains(&JITO_DONTFRONT), "jitodontfront is in the transaction");
        }
    }

    /// HOT_NONCE_FANOUT: a nonced template, filled for one lane's copy (only its tip paid), is the
    /// builder's message with AdvanceNonceAccount first and the nonce value as its blockhash.
    #[test]
    fn a_nonced_copy_is_the_builders_nonce_transaction() {
        let keypair = Keypair::new();
        let spec = TemplateSpec {
            payer: keypair.pubkey(),
            cu_limit: 120_000,
            loaded_data_bytes: Some(16 * 1024 * 1024),
            tips: vec![(&crate::service::nozomi::NOZOMI_TIP_ACCOUNTS, 1_000_000), (&crate::service::jito::JITO_TIP_ACCOUNTS, 300_000)],
            dontfront: Some(1),
        };
        let mut template = BuyTemplate::build_nonced(spec.clone()).unwrap();
        assert!(template.is_nonced() && !BuyTemplate::build(spec.clone()).unwrap().is_nonced());
        let tip_accounts: Vec<Pubkey> = spec.tips.iter().map(|(list, _)| list[5 % list.len()]).collect();
        for (lane_tip, account) in [(0usize, Pubkey::new_unique()), (1, Pubkey::new_unique())] {
            let mut paid = [0u64; MAX_TIPS];
            paid[lane_tip] = spec.tips[lane_tip].1;
            let value = Hash::new_from_array([lane_tip as u8 + 40; 32]);
            let fill = BuyFill { paid: Some(paid), blockhash: value, ..real_fill(&keypair.pubkey(), TOKEN_PROGRAM_ID) };
            template.fill(&fill);
            template.set_nonce_account(&account);
            let mut out = [0u8; PACKET];
            let n = template.sign_into(&keypair, &mut out);
            let tx: VersionedTransaction = bincode::deserialize(&out[..n]).expect("a valid transaction");
            assert!(tx.signatures[0].verify(keypair.pubkey().as_ref(), &tx.message.serialize()));
            let solana_message::VersionedMessage::Legacy(got) = &tx.message else { panic!("legacy") };
            assert_eq!(got.recent_blockhash, value, "the nonce value is the blockhash");
            let with = TemplateSpec { tips: spec.tips.iter().enumerate().map(|(i, (list, _))| (*list, paid[i])).collect(), ..spec.clone() };
            let expected = Message::new_with_blockhash(&instructions_nonced(&with, &fill, &tip_accounts, Some(&account)), Some(&spec.payer), &value);
            assert_eq!(decompiled(got), decompiled(&expected), "copy for tip {lane_tip}");
            let first = &got.instructions[0];
            assert_eq!(got.account_keys[first.program_id_index as usize], crate::utils::nonce::SYSTEM_PROGRAM);
            assert_eq!(first.data, vec![4, 0, 0, 0], "AdvanceNonceAccount first");
            assert_eq!(got.account_keys[first.accounts[0] as usize], account);
        }
    }

    #[test]
    fn a_filled_template_signs_into_the_same_buy_the_builder_makes() {
        let keypair = Keypair::new();
        let spec = TemplateSpec {
            payer: keypair.pubkey(),
            cu_limit: 120_000,
            loaded_data_bytes: Some(16 * 1024 * 1024),
            tips: vec![(&crate::service::nozomi::NOZOMI_TIP_ACCOUNTS, 1_000_000), (&crate::service::jito::JITO_TIP_ACCOUNTS, 1_000_000)],
            dontfront: None,
        };
        let mut template = BuyTemplate::build(spec.clone()).unwrap();
        for token_program in [TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID] {
            let fill = real_fill(&keypair.pubkey(), token_program);
            template.fill(&fill);
            let mut out = [0u8; PACKET];
            let n = template.sign_into(&keypair, &mut out);
            let tx: VersionedTransaction = bincode::deserialize(&out[..n]).expect("a valid transaction");
            assert!(tx.signatures[0].verify(keypair.pubkey().as_ref(), &tx.message.serialize()), "signature verifies");
            let solana_message::VersionedMessage::Legacy(got) = &tx.message else { panic!("legacy") };
            assert_eq!(got.recent_blockhash, fill.blockhash);
            let tip_accounts = [
                crate::service::nozomi::NOZOMI_TIP_ACCOUNTS[5 % crate::service::nozomi::NOZOMI_TIP_ACCOUNTS.len()],
                crate::service::jito::JITO_TIP_ACCOUNTS[5 % crate::service::jito::JITO_TIP_ACCOUNTS.len()],
            ];
            let expected = Message::new_with_blockhash(&instructions(&spec, &fill, &tip_accounts), Some(&spec.payer), &fill.blockhash);
            assert_eq!(decompiled(got), decompiled(&expected), "same instructions, accounts, flags and data");
            assert_eq!(got.header, expected.header);
            let mut b64 = [0u8; PACKET_B64];
            let m = base64_into(&out[..n], &mut b64);
            use base64::Engine;
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(&b64[..m]).unwrap(), out[..n].to_vec());
        }
    }

    #[test]
    fn the_template_matches_the_market_builder_for_the_same_coin() {
        // The 18 accounts and the data of PumpFunMarket::buy_instructions, the bot's live builder.
        let keypair = Keypair::new();
        let payer = keypair.pubkey();
        let mint = Pubkey::new_unique();
        let creator = Pubkey::new_unique();
        let market = crate::instructions::pumpfun_buy_ix::PumpFunMarket::new(mint, TOKEN_2022_PROGRAM_ID, creator, false, Default::default());
        crate::instructions::pumpfun_buy_ix::set_test_config();
        let built = market.buy_instructions(&payer, 50_000_000, 7).unwrap();
        let buy = &built[1];
        let fill = BuyFill {
            mint,
            token_program: TOKEN_2022_PROGRAM_ID,
            bonding_curve: buy.accounts[3].pubkey,
            associated_bonding_curve: buy.accounts[4].pubkey,
            user_ata: buy.accounts[5].pubkey,
            creator_vault: buy.accounts[9].pubkey,
            bonding_curve_v2: buy.accounts[16].pubkey,
            fee_recipient: buy.accounts[1].pubkey,
            buyback_fee_recipient: buy.accounts[17].pubkey,
            lamports_in: 50_000_000,
            min_out: 7,
            cu_price: 0,
            blockhash: Hash::default(),
            tip_pick: 0,
            paid: None,
            cu_limit: 0,
        };
        let spec = TemplateSpec { payer, cu_limit: 120_000, loaded_data_bytes: None, tips: vec![], dontfront: None };
        let ours = instructions(&spec, &fill, &[]);
        assert_eq!(&ours[ours.len() - 2..], &built[..], "token-account create and buy, account for account");
        assert_eq!(buy.accounts.len(), 18);
    }
}
