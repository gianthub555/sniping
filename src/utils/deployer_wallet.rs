//! Each deployer wallet's first transaction, for the scorecard's "funding" and "wallet age" facts
//! (2026-10-12): when it happened and how much SOL the wallet's balance changed by in it (the SOL it
//! was funded with). Looked up once per wallet over RPC (Helius `getTransactionsForAddress`, oldest
//! first), kept in SQLite (`deployer_wallet`) and in memory. The keys the bot can follow are
//! looked up ahead of their launches (`prefetch`), so the buy path only reads the map.
//!
//! Same definition as the scorecard's training data (research/filter-pXGQ-2026-10-12/enrich.py):
//! the first transaction of the create's fee payer; funded SOL = its own balance change there.

use {
    super::db,
    log::{debug, info, warn},
    serde_json::{json, Value},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        str::FromStr,
        sync::{LazyLock, Mutex},
    },
};

/// A wallet's first transaction: unix time and the SOL its balance changed by. Either is None when
/// RPC found no transaction (or no balance entry for the wallet in it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FirstTx {
    pub first_bt: Option<i64>,
    pub funded_sol: Option<f64>,
}

static CACHE: LazyLock<Mutex<HashMap<Pubkey, FirstTx>>> = LazyLock::new(Default::default);
static IN_FLIGHT: LazyLock<Mutex<std::collections::HashSet<Pubkey>>> = LazyLock::new(Default::default);

/// What is known about `wallet`, without RPC (None: not looked up yet).
pub fn get(wallet: &Pubkey) -> Option<FirstTx> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(wallet).copied()
}

/// Puts a value in memory only (tests and benchmarks).
#[doc(hidden)]
pub fn seed(wallet: Pubkey, first: FirstTx) {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(wallet, first);
}

/// Loads the cached wallets from the database. Returns how many.
pub fn load() -> usize {
    let rows = match db::read().and_then(|c| db::load_deployer_wallets(&c)) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("deployer wallets not loaded from {}: {e}", db::path());
            return 0;
        }
    };
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    for (key, first_bt, funded_sol) in rows {
        if let Ok(k) = Pubkey::from_str(&key) {
            cache.insert(k, FirstTx { first_bt, funded_sol });
        }
    }
    cache.len()
}

/// The first transaction of `wallet` from an RPC answer's page (pure, for tests).
pub fn first_tx_of(wallet: &str, page: &Value) -> FirstTx {
    let Some(tx) = page["data"].as_array().and_then(|d| d.first()) else {
        return FirstTx { first_bt: None, funded_sol: None };
    };
    let first_bt = tx["blockTime"].as_i64();
    let msg = &tx["transaction"]["message"];
    let mut keys: Vec<&str> = msg["accountKeys"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    for part in ["writable", "readonly"] {
        if let Some(a) = tx["meta"]["loadedAddresses"][part].as_array() {
            keys.extend(a.iter().filter_map(Value::as_str));
        }
    }
    let pre = tx["meta"]["preBalances"].as_array();
    let post = tx["meta"]["postBalances"].as_array();
    let funded_sol = match (pre, post, keys.iter().position(|k| *k == wallet)) {
        (Some(pre), Some(post), Some(i)) => match (pre.get(i).and_then(Value::as_i64), post.get(i).and_then(Value::as_i64)) {
            (Some(a), Some(b)) => Some((b - a) as f64 / 1e9),
            _ => None,
        },
        _ => None,
    };
    FirstTx { first_bt, funded_sol }
}

async fn fetch(wallet: &Pubkey) -> anyhow::Result<FirstTx> {
    let config = json!({ "transactionDetails": "full", "sortOrder": "asc", "limit": 1, "encoding": "json", "maxSupportedTransactionVersion": 1 });
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "getTransactionsForAddress", "params": [wallet.to_string(), config] });
    let reply: Value = crate::config::HTTP_CLIENT.post(crate::config::rpc_endpoint()).json(&body).send().await?.json().await?;
    if let Some(error) = reply.get("error") {
        anyhow::bail!("getTransactionsForAddress: {error}");
    }
    Ok(first_tx_of(&wallet.to_string(), &reply["result"]))
}

/// Looks `wallet` up once (RPC), caches and stores the answer. Does nothing when it is known or
/// already being looked up.
pub async fn ensure(wallet: Pubkey) {
    if get(&wallet).is_some() {
        return;
    }
    if !IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).insert(wallet) {
        return;
    }
    match fetch(&wallet).await {
        Ok(first) => {
            CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(wallet, first);
            db::write(db::Write::DeployerWallet {
                key: wallet.to_string(),
                first_bt: first.first_bt,
                funded_sol: first.funded_sol,
                funder: None,
                checked_at: crate::utils::dashboard::now_unix() as i64,
            });
        }
        Err(e) => debug!("first transaction of {wallet}: {e:#}"),
    }
    IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).remove(&wallet);
}

/// Looks up every wallet not cached yet, four at a time, in the background.
pub fn prefetch(wallets: Vec<Pubkey>) {
    tokio::spawn(async move {
        let todo: Vec<Pubkey> = wallets.into_iter().filter(|w| get(w).is_none()).collect();
        let n = todo.len();
        if n == 0 {
            return;
        }
        info!("looking up the first transaction of {n} deployer wallets (scorecard funding and age)");
        futures::StreamExt::for_each_concurrent(futures::stream::iter(todo), 4, ensure).await;
        info!("deployer wallets known: {}", CACHE.lock().unwrap_or_else(|e| e.into_inner()).len());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wallets_own_balance_change_is_its_funding() {
        let page = json!({ "data": [{
            "blockTime": 1700000000,
            "transaction": { "message": { "accountKeys": ["funder", "wallet", "11111111111111111111111111111111"] } },
            "meta": { "preBalances": [5_000_000_000i64, 0, 1], "postBalances": [3_499_995_000i64, 1_500_000_000, 1], "loadedAddresses": { "writable": [], "readonly": [] } }
        }]});
        assert_eq!(first_tx_of("wallet", &page), FirstTx { first_bt: Some(1700000000), funded_sol: Some(1.5) });
        assert_eq!(first_tx_of("other", &page), FirstTx { first_bt: Some(1700000000), funded_sol: None });
        assert_eq!(first_tx_of("wallet", &json!({ "data": [] })), FirstTx { first_bt: None, funded_sol: None });
    }
}
