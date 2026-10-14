//! Durable nonces for the buy fan-out (`TRADE_SETTING.nonce_fanout`). A buy goes out as one
//! transaction per landing service, each tipping only its own service and all advancing the same
//! nonce account first: at most one can land, and the moment it does the others no longer match
//! the nonce and are dropped without a fee. This is how the reference wallet sends (a copy per
//! service, the losing copies failing cheaply) and the first buyers of most launches too.
//!
//! The nonce accounts (authority: the bot's wallet) are listed in `nonce_accounts.txt`, one per
//! line, created with `cargo run --release --bin nonce -- create N`. Their values are read at
//! startup and kept in memory, so a buy never waits on RPC; after a buy the account it used is read
//! again in the background (a landed copy advanced it).

use {
    crate::config::{PUBKEY, RPC_CLIENT},
    anyhow::{bail, Context, Result},
    log::{info, warn},
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    std::{
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            LazyLock, Mutex,
        },
        time::Duration,
    },
};

pub const NONCE_FILE: &str = "nonce_accounts.txt";
/// A nonce account's data: version (u32), state (u32), authority, nonce value, fee calculator.
pub const NONCE_ACCOUNT_SIZE: u64 = 80;
pub const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
const RECENT_BLOCKHASHES: Pubkey = Pubkey::from_str_const("SysvarRecentB1ockHashes11111111111111111111");
const RENT: Pubkey = Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");
/// After a buy, how long its nonce account is watched for the advance a landed copy makes, and how
/// often. A copy lands within about a second or not at all (the buy flow's own timeouts).
const REFRESH_FOR: Duration = Duration::from_secs(5);
const REFRESH_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy)]
struct Entry {
    account: Pubkey,
    value: Option<Hash>,
    busy: bool,
}

static POOL: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(Default::default);
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// One nonce account handed to one buy: every copy of the buy advances `account` and carries
/// `value` as its blockhash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    pub account: Pubkey,
    pub value: Hash,
}

/// `AdvanceNonceAccount`: must be the transaction's first instruction.
pub fn advance_instruction(nonce: &Pubkey, authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: SYSTEM_PROGRAM,
        accounts: vec![
            AccountMeta::new(*nonce, false),
            AccountMeta::new_readonly(RECENT_BLOCKHASHES, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data: 4u32.to_le_bytes().to_vec(),
    }
}

/// `CreateAccount` for a nonce account and `InitializeNonceAccount` with `authority`.
pub fn create_instructions(payer: &Pubkey, nonce: &Pubkey, authority: &Pubkey, rent_lamports: u64) -> Vec<Instruction> {
    let mut create = 0u32.to_le_bytes().to_vec();
    create.extend(rent_lamports.to_le_bytes());
    create.extend(NONCE_ACCOUNT_SIZE.to_le_bytes());
    create.extend(SYSTEM_PROGRAM.to_bytes());
    let mut initialize = 6u32.to_le_bytes().to_vec();
    initialize.extend(authority.to_bytes());
    vec![
        Instruction {
            program_id: SYSTEM_PROGRAM,
            accounts: vec![AccountMeta::new(*payer, true), AccountMeta::new(*nonce, true)],
            data: create,
        },
        Instruction {
            program_id: SYSTEM_PROGRAM,
            accounts: vec![
                AccountMeta::new(*nonce, false),
                AccountMeta::new_readonly(RECENT_BLOCKHASHES, false),
                AccountMeta::new_readonly(RENT, false),
            ],
            data: initialize,
        },
    ]
}

/// `WithdrawNonceAccount`: all of `lamports` closes the account.
pub fn withdraw_instruction(nonce: &Pubkey, authority: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 5u32.to_le_bytes().to_vec();
    data.extend(lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM,
        accounts: vec![
            AccountMeta::new(*nonce, false),
            AccountMeta::new(*to, false),
            AccountMeta::new_readonly(RECENT_BLOCKHASHES, false),
            AccountMeta::new_readonly(RENT, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// An initialized nonce account's authority and value.
pub fn parse(data: &[u8]) -> Option<(Pubkey, Hash)> {
    if data.len() < 72 || u32::from_le_bytes(data[4..8].try_into().ok()?) != 1 {
        return None;
    }
    let authority = Pubkey::new_from_array(data[8..40].try_into().ok()?);
    let value = Hash::new_from_array(data[40..72].try_into().ok()?);
    Some((authority, value))
}

/// The accounts listed in `nonce_accounts.txt` (blank lines and `#` comments skipped).
pub fn listed() -> Result<Vec<Pubkey>> {
    let Ok(text) = std::fs::read_to_string(NONCE_FILE) else { return Ok(Vec::new()) };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| Pubkey::from_str(l).with_context(|| format!("{NONCE_FILE}: bad address {l}")))
        .collect()
}

/// Reads every listed account's value; an account not owned by the bot's wallet is an error.
/// Returns how many are ready.
pub async fn init() -> Result<usize> {
    let accounts = listed()?;
    if accounts.is_empty() {
        bail!("NONCE_FANOUT needs nonce accounts: run `cargo run --release --bin nonce -- create 4` ({NONCE_FILE})");
    }
    let values = read_values(&accounts).await?;
    let mut pool = Vec::with_capacity(accounts.len());
    for (account, value) in accounts.iter().zip(values) {
        match value {
            Some((authority, value)) if authority == *PUBKEY => pool.push(Entry { account: *account, value: Some(value), busy: false }),
            Some((authority, _)) => bail!("nonce account {account} belongs to {authority}, not the bot's wallet"),
            None => bail!("{account} in {NONCE_FILE} is not an initialized nonce account"),
        }
    }
    let ready = pool.len();
    *POOL.lock().unwrap_or_else(|e| e.into_inner()) = pool;
    info!("nonce fan-out: {ready} nonce accounts ready");
    Ok(ready)
}

async fn read_values(accounts: &[Pubkey]) -> Result<Vec<Option<(Pubkey, Hash)>>> {
    let response = RPC_CLIENT.get_multiple_accounts_with_commitment(accounts, CommitmentConfig::processed()).await?;
    Ok(response.value.into_iter().map(|a| a.and_then(|a| parse(&a.data))).collect())
}

/// The next free account with a known value, marked busy until `release` has read it again.
/// None when every account is in use (the buy then goes out the ordinary way).
pub fn lease() -> Option<Lease> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let n = pool.len();
    for k in 0..n {
        let i = (NEXT.fetch_add(1, Ordering::Relaxed) + k) % n;
        let e = &mut pool[i];
        if let (false, Some(value)) = (e.busy, e.value) {
            e.busy = true;
            return Some(Lease { account: e.account, value });
        }
    }
    None
}

/// After a buy: watch the leased account for the advance a landed copy makes, keep its new value,
/// and free it. Unchanged after `REFRESH_FOR` (no copy landed): the old value stays good.
pub fn release(lease: Lease) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + REFRESH_FOR;
        let mut value = Some(lease.value);
        loop {
            tokio::time::sleep(REFRESH_EVERY).await;
            match read_values(&[lease.account]).await {
                Ok(v) => match v.first().copied().flatten() {
                    Some((_, now)) if now != lease.value => {
                        value = Some(now);
                        break;
                    }
                    Some(_) => {}
                    None => {
                        warn!("nonce account {} is no longer a nonce account; left out", lease.account);
                        value = None;
                        break;
                    }
                },
                Err(e) => warn!("nonce account {}: {e:#}", lease.account),
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        set(lease.account, value);
    });
}

fn set(account: Pubkey, value: Option<Hash>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = pool.iter_mut().find(|e| e.account == account) {
        e.value = value;
        e.busy = false;
    }
}

#[cfg(test)]
pub(crate) fn install_for_test(entries: &[(Pubkey, Hash)]) {
    *POOL.lock().unwrap_or_else(|e| e.into_inner()) =
        entries.iter().map(|(account, value)| Entry { account: *account, value: Some(*value), busy: false }).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_initialized_nonce_account_is_read() {
        let (authority, value) = (Pubkey::new_unique(), Hash::new_from_array([7; 32]));
        let mut data = vec![0u8; 80];
        data[0..4].copy_from_slice(&1u32.to_le_bytes());
        data[4..8].copy_from_slice(&1u32.to_le_bytes());
        data[8..40].copy_from_slice(&authority.to_bytes());
        data[40..72].copy_from_slice(&value.to_bytes());
        assert_eq!(parse(&data), Some((authority, value)));
        data[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse(&data), None, "uninitialized");
        assert_eq!(parse(&data[..40]), None, "too short");
    }

    #[test]
    fn the_advance_instruction_is_the_system_programs() {
        let (nonce, authority) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ix = advance_instruction(&nonce, &authority);
        assert_eq!(ix.program_id, SYSTEM_PROGRAM);
        assert_eq!(ix.data, vec![4, 0, 0, 0]);
        assert_eq!(ix.accounts[0], AccountMeta::new(nonce, false));
        assert_eq!(ix.accounts[1].pubkey, RECENT_BLOCKHASHES);
        assert!(ix.accounts[2].is_signer && ix.accounts[2].pubkey == authority);
    }

    #[test]
    fn leases_go_round_and_skip_busy_accounts() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        install_for_test(&[(a, Hash::new_from_array([1; 32])), (b, Hash::new_from_array([2; 32]))]);
        let first = lease().expect("one free");
        let second = lease().expect("the other free");
        assert_ne!(first.account, second.account);
        assert!(lease().is_none(), "both busy");
        set(first.account, Some(Hash::new_from_array([9; 32])));
        let again = lease().expect("freed");
        assert_eq!((again.account, again.value), (first.account, Hash::new_from_array([9; 32])));
    }
}
