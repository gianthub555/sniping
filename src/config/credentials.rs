use {
    super::trade_setting::env_opt,
    anyhow::{anyhow, bail, Context, Result},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::sync::{Arc, LazyLock},
};


pub static PAYER: LazyLock<Arc<Keypair>> = LazyLock::new(|| {
    Arc::new(load_payer().unwrap_or_else(|e| panic!("invalid wallet configuration: {e:#}")))
});

pub static PUBKEY: LazyLock<Pubkey> = LazyLock::new(|| PAYER.pubkey());

/// Paper runs never sign with the wallet's key (the owner's rule, 2026-10-12): a key made fresh
/// at each start, kept in memory only, signs the hot path's unlandable lane copies of paper buys
/// and sells. Its account holds nothing, so nothing it signs can land.
pub static PAPER_KEY: LazyLock<Arc<Keypair>> = LazyLock::new(|| Arc::new(Keypair::new()));

/// The key the hot path signs with: the wallet's when live, the throwaway paper key in DRY_RUN.
pub fn hot_signer() -> Arc<Keypair> {
    signer_for(super::TRADE_SETTING.dry_run)
}

fn signer_for(dry_run: bool) -> Arc<Keypair> {
    if dry_run {
        PAPER_KEY.clone()
    } else {
        PAYER.clone()
    }
}


pub fn trading_wallet() -> Pubkey {
    let setting = &*super::TRADE_SETTING;
    match (setting.dry_run, setting.dry_run_wallet) {
        (true, Some(wallet)) => wallet,
        _ => *PUBKEY,
    }
}


pub fn check_wallet_config() -> Result<()> {
    load_payer().map(|_| ())
}

fn load_payer() -> Result<Keypair> {
    let raw = env_opt("PRIVATE_KEY").ok_or_else(|| anyhow!("PRIVATE_KEY is not set"))?;
    let keypair = parse_private_key(&raw)?;

    if let Some(expected) = env_opt("PUBKEY") {
        let expected: Pubkey = expected
            .parse()
            .map_err(|e| anyhow!("PUBKEY is not a valid public key: {e}"))?;
        if expected != keypair.pubkey() {
            bail!(
                "PUBKEY {expected} does not match the public key of PRIVATE_KEY ({})",
                keypair.pubkey()
            );
        }
    }
    Ok(keypair)
}


pub fn parse_private_key(raw: &str) -> Result<Keypair> {
    let raw = raw.trim();
    let bytes: Vec<u8> = if raw.starts_with('[') {
        serde_json::from_str(raw).context("PRIVATE_KEY looks like a JSON array but failed to parse")?
    } else {
        bs58::decode(raw)
            .into_vec()
            .context("PRIVATE_KEY is neither a JSON byte array nor valid base58")?
    };

    match bytes.len() {
        64 => Keypair::try_from(bytes.as_slice())
            .map_err(|e| anyhow!("PRIVATE_KEY is not a valid ed25519 keypair: {e}")),
        32 => {
            let secret: [u8; 32] = bytes.as_slice().try_into().expect("length checked");
            Ok(Keypair::new_from_array(secret))
        }
        n => bail!("PRIVATE_KEY decodes to {n} bytes; expected 64 (keypair) or 32 (secret key)"),
    }
}

#[cfg(test)]
mod paper_key_tests {
    use super::*;

    /// DRY_RUN's signer is the throwaway key, the same one for the whole run, never read from the
    /// environment.
    #[test]
    fn a_paper_run_signs_with_a_throwaway_key() {
        let a = signer_for(true);
        let b = signer_for(true);
        assert_eq!(a.pubkey(), b.pubkey());
        assert_eq!(a.pubkey(), PAPER_KEY.pubkey());
        if let Ok(wallet) = load_payer() {
            assert_ne!(a.pubkey(), wallet.pubkey(), "never the wallet's key");
        }
    }
}
