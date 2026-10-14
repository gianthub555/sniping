





use {
    crate::{
        config::TRADE_SETTING,
        service::utils::{
            services::{first_accepted, near_regions, send_json_rpc_transaction, spawn_keep_alive, Region},
            tip::tip_instruction,
        },
    },
    anyhow::{anyhow, Result},
    log::info,
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
    std::sync::OnceLock,
};


pub const HELIUS_SENDER_DEFAULT_URL: &str = "http://ewr-sender.helius-rpc.com/fast?swqos_only=true";

pub const HELIUS_SENDER_MIN_TIP_LAMPORTS: u64 = 5_000;

pub const HELIUS_SENDER_TIP_ACCOUNTS: [Pubkey; 10] = [
    Pubkey::from_str_const("4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE"),
    Pubkey::from_str_const("D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ"),
    Pubkey::from_str_const("9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta"),
    Pubkey::from_str_const("5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn"),
    Pubkey::from_str_const("2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD"),
    Pubkey::from_str_const("2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ"),
    Pubkey::from_str_const("wyvPkWjVZz1M8fHQnMMCDTQDbkManefNNhweYk5WkcF"),
    Pubkey::from_str_const("3KCKozbAaF75qEU33jtzozcJ29yJuaLJTy2jFdzUY8bT"),
    Pubkey::from_str_const("4vieeGHPYPG2MmyPRcYjdiDmmhN3ww7hsFNap8pVN3Ey"),
    Pubkey::from_str_const("4TQLFNWK8AovT1gFvda5jfw2oJeRMKEmw7aH6MGBJ3or"),
];

static URLS: OnceLock<Vec<String>> = OnceLock::new();

/// Regions besides the fastest that get every transaction too (within this much of it, at most
/// SENDER_REGIONS in all): free, every copy is the same transaction; the launch's producer may be
/// nearer another one (see `jito::jito_confirm::NEAR_EXTRA_MS`). The probe is plain HTTP (about two
/// round trips): from Frankfurt, Amsterdam answers 13 ms later, London 29 ms.
const NEAR_EXTRA_MS: f64 = 20.0;
const SENDER_REGIONS: usize = 2;

/// Helius Sender's regional endpoints (Salt Lake City, Newark, London, Frankfurt, Amsterdam,
/// Singapore, Tokyo).
const REGIONS: [&str; 7] = ["slc", "ewr", "lon", "fra", "ams", "sg", "tyo"];

/// HELIUS_SENDER_URL, or the fastest regional endpoint from here (SWQoS-only, whose minimum tip is
/// HELIUS_SENDER_MIN_TIP_LAMPORTS), or Newark when none answers.
pub async fn init_helius_sender() -> Result<()> {
    let urls: Vec<String> = match &TRADE_SETTING.helius_sender_url {
        Some(url) => vec![url.clone()],
        None => {
            let regions = REGIONS
                .iter()
                .map(|r| {
                    let base_url = format!("http://{r}-sender.helius-rpc.com");
                    Region { name: r, probe_url: format!("{base_url}/ping"), base_url }
                })
                .collect();
            let near = near_regions(regions, NEAR_EXTRA_MS, SENDER_REGIONS).await;
            if near.is_empty() {
                vec![HELIUS_SENDER_DEFAULT_URL.to_string()]
            } else {
                info!(
                    "helius-sender: every transaction to {}",
                    near.iter().map(|(l, r)| format!("{} ({:.1} ms)", r.name, l.as_secs_f64() * 1e3)).collect::<Vec<_>>().join(", ")
                );
                near.into_iter().map(|(_, r)| format!("{}/fast?swqos_only=true", r.base_url)).collect()
            }
        }
    };
    for url in &urls {
        let base = url.split("/fast").next().unwrap_or(url).trim_end_matches('/').to_string();
        spawn_keep_alive(format!("{base}/ping"));
    }
    let _ = URLS.set(urls);
    Ok(())
}

pub fn helius_sender_tip_instruction(payer: &Pubkey, lamports: u64) -> Instruction {
    tip_instruction(payer, &HELIUS_SENDER_TIP_ACCOUNTS, lamports)
}

/// Sends to every chosen region at once; accepted when one region accepts.
pub async fn send_helius_sender(tx_base64: &str) -> Result<String> {
    let urls = URLS.get().ok_or_else(|| anyhow!("Helius Sender is not initialised"))?;
    first_accepted(futures::future::join_all(urls.iter().map(|url| send_json_rpc_transaction("helius-sender", url, tx_base64, &[]))).await)
}
