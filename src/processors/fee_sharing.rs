//! Coins whose creator moves to a fee-sharing account as the early feeds show it, before it
//! executes: pump fees' create_fee_sharing_config (it calls pump.fun's
//! migrate_bonding_curve_creator), often the transaction right after the create, in its bundle.
//! The curve's creator then becomes the coin's sharing config, and a buy that pays the old
//! creator's vault fails (pump.fun 2006: two followed launches on 13 Oct, both of one deployer
//! whose creates are followed at create+1 by the move). The slow path's buy takes the new creator
//! from here (`common::apply_stream_view`); the regular stream's view, when it has one, is the
//! chain's and comes after it.

use {
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::{LazyLock, Mutex},
        time::{Duration, Instant},
    },
};

/// How long a sighting is kept (a launch is bought within seconds of its create).
const TTL: Duration = Duration::from_secs(300);

static SEEN: LazyLock<Mutex<HashMap<Pubkey, Instant>>> = LazyLock::new(Default::default);

/// An early feed showed `mint`'s creator moving to fee sharing.
pub fn note(mint: Pubkey) {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() > 10_000 {
        seen.retain(|_, at| at.elapsed() < TTL);
    }
    seen.insert(mint, Instant::now());
}

/// The coin's creator after its move to fee sharing (its sharing config), when an early feed
/// showed the move.
pub fn creator_of(mint: &Pubkey) -> Option<Pubkey> {
    let seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.get(mint).filter(|at| at.elapsed() < TTL).map(|_| crate::hot::decode::sharing_config_of(mint))
}

/// Tests: forget a sighting (tests share the map; take `TEST_LOCK` around a forget-and-check).
#[cfg(test)]
pub fn forget(mint: &Pubkey) {
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).remove(mint);
}

#[cfg(test)]
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use {super::*, std::str::FromStr};

    #[test]
    fn a_noted_coin_takes_its_sharing_config_as_creator() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 5Dio's sharing config, as its create_fee_sharing_config named it on 13 Oct.
        let mint = Pubkey::from_str("5DioReSZLHQsCuZXiREHwTiGcvts9TXmvGx6RJyu8Zx7").unwrap();
        forget(&mint);
        assert_eq!(creator_of(&mint), None);
        note(mint);
        assert_eq!(creator_of(&mint), Some(Pubkey::from_str("3Z9mtCi8ebunhQ8fHTUHEutRvhCUHHZ7yAWHZkVp6695").unwrap()));
    }
}
