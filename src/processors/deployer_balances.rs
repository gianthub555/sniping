//! Live SOL balances of the deployers and creators the bot follows, so a launch seen before it
//! executes (early stream) can be checked against the "deployer held 2+ SOL" rule at once instead
//! of asking RPC (about 60 ms from Frankfurt, more than the early stream gains). Seeded from RPC,
//! then kept current by an account subscription on the gRPC stream (processed commitment).

use {
    crate::{config::RPC_CLIENT, utils::reference},
    anyhow::{anyhow, Result},
    futures::{SinkExt, StreamExt},
    log::{info, warn},
    solana_commitment_config::CommitmentConfig,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, HashSet},
        sync::{LazyLock, Mutex},
        time::Duration,
    },
    yellowstone_grpc_client::GeyserGrpcClient,
    yellowstone_grpc_proto::{
        geyser::{
            subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestFilterAccounts,
            SubscribeRequestPing,
        },
        tonic::transport::ClientTlsConfig,
    },
};

/// How often the followed list is checked for new wallets to subscribe to.
const REFRESH: Duration = Duration::from_secs(60);
/// getMultipleAccounts takes at most 100 keys.
const RPC_BATCH: usize = 100;

/// Balance and the slot it was seen at, by wallet.
static BALANCES: LazyLock<Mutex<HashMap<Pubkey, (u64, u64)>>> = LazyLock::new(Default::default);

fn store(key: Pubkey, lamports: u64, slot: u64) {
    let mut balances = BALANCES.lock().unwrap_or_else(|e| e.into_inner());
    let entry = balances.entry(key).or_insert((lamports, slot));
    if slot >= entry.1 {
        *entry = (lamports, slot);
    }
}

/// The wallet's balance before a create in `create_slot` that spends `create_spend` lamports
/// (dev buy, rent and fees), when the wallet is followed. A balance seen after the create's slot
/// may already have paid for it, so the spend is added back.
pub fn balance_before(key: &Pubkey, create_slot: u64, create_spend: u64) -> Option<u64> {
    let (lamports, slot) = *BALANCES.lock().unwrap_or_else(|e| e.into_inner()).get(key)?;
    Some(if slot > create_slot { lamports + create_spend } else { lamports })
}

/// Puts a balance in memory only (benchmarks and tests).
#[doc(hidden)]
pub fn seed_balance(key: Pubkey, lamports: u64, slot: u64) {
    store(key, lamports, slot);
}

pub fn known() -> usize {
    BALANCES.lock().unwrap_or_else(|e| e.into_inner()).len()
}

fn followed() -> HashSet<Pubkey> {
    reference::followed_keys(crate::utils::dashboard::now_unix()).into_iter().collect()
}

/// Current balances of `keys` from RPC.
async fn seed(keys: &[Pubkey]) {
    for chunk in keys.chunks(RPC_BATCH) {
        match RPC_CLIENT.get_multiple_accounts_with_commitment(chunk, CommitmentConfig::processed()).await {
            Ok(response) => {
                for (key, account) in chunk.iter().zip(response.value) {
                    store(*key, account.map_or(0, |a| a.lamports), response.context.slot);
                }
            }
            Err(e) => warn!("deployer balances: RPC seed failed: {e}"),
        }
    }
}

fn request(keys: &HashSet<Pubkey>) -> SubscribeRequest {
    SubscribeRequest {
        accounts: HashMap::from([(
            "followed_deployers".to_string(),
            SubscribeRequestFilterAccounts { account: keys.iter().map(|k| k.to_string()).collect(), ..Default::default() },
        )]),
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    }
}

pub fn spawn(url: String, token: Option<String>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&url, token.clone()).await {
                warn!("deployer balances: {e:#}; reconnecting in 2 s");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn run(url: &str, token: Option<String>) -> Result<()> {
    let mut keys = followed();
    let seeded: Vec<Pubkey> = keys.iter().copied().collect();
    seed(&seeded).await;
    let mut client = GeyserGrpcClient::build_from_shared(url.to_string())
        .map_err(|e| anyhow!("invalid gRPC url: {e}"))?
        .x_token(token)
        .map_err(|e| anyhow!("invalid gRPC token: {e}"))?
        .tls_config(ClientTlsConfig::new().with_enabled_roots())?
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .connect()
        .await
        .map_err(|e| anyhow!("connect: {e}"))?;
    let (mut sink, mut stream) = client.subscribe_with_request(Some(request(&keys))).await.map_err(|e| anyhow!("subscribe: {e}"))?;
    info!("deployer balances: watching {} followed wallets", keys.len());
    let mut refresh = tokio::time::interval(REFRESH);
    refresh.tick().await;
    loop {
        tokio::select! {
            update = stream.next() => {
                let update = update.ok_or_else(|| anyhow!("stream ended"))?.map_err(|e| anyhow!("{e}"))?;
                match update.update_oneof {
                    Some(UpdateOneof::Account(a)) => {
                        if let Some(info) = a.account {
                            if let Ok(key) = Pubkey::try_from(info.pubkey.as_slice()) {
                                store(key, info.lamports, a.slot);
                            }
                        }
                    }
                    Some(UpdateOneof::Ping(_)) => {
                        sink.send(SubscribeRequest { ping: Some(SubscribeRequestPing { id: 1 }), ..Default::default() })
                            .await
                            .map_err(|e| anyhow!("ping: {e}"))?;
                    }
                    _ => {}
                }
            }
            _ = refresh.tick() => {
                let now = followed();
                if now != keys {
                    let added: Vec<Pubkey> = now.difference(&keys).copied().collect();
                    seed(&added).await;
                    sink.send(request(&now)).await.map_err(|e| anyhow!("resubscribe: {e}"))?;
                    keys = now;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_balance_seen_after_the_create_gets_its_spend_back() {
        let key = Pubkey::new_unique();
        assert_eq!(balance_before(&key, 100, 5), None, "not followed");
        store(key, 3_000, 99);
        assert_eq!(balance_before(&key, 100, 500), Some(3_000));
        store(key, 2_500, 101);
        assert_eq!(balance_before(&key, 100, 500), Some(3_000));
        // An older update does not overwrite a newer one.
        store(key, 9_999, 90);
        assert_eq!(balance_before(&key, 102, 500), Some(2_500));
    }
}
