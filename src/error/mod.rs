use solana_pubkey::Pubkey;

#[derive(Debug, thiserror::Error)]
pub enum SniperError {
    #[error("transaction is {0} bytes, above the 1232-byte limit")]
    TransactionTooLarge(usize),
    #[error("account {0} not found")]
    AccountNotFound(Pubkey),
    #[error("account {account} has unexpected data: {reason}")]
    InvalidAccountData { account: Pubkey, reason: String },
    #[error("quote for {0} returned zero output")]
    ZeroOutput(Pubkey),
    #[error("pool for {mint} is no longer tradable on {venue}")]
    VenueClosed { mint: Pubkey, venue: &'static str },
}
