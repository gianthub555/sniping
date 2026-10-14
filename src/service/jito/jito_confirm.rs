use {
    super::jito_regions::{jito_probe_url, jito_regions},
    crate::{
        config::TRADE_SETTING,
        service::utils::services::{first_accepted, near_regions, send_json_rpc_transaction, spawn_keep_alive},
    },
    anyhow::{anyhow, Result},
    log::info,
    std::sync::OnceLock,
};

static JITO_URLS: OnceLock<Vec<String>> = OnceLock::new();

/// Block-engine regions besides the fastest that get every transaction too: those within this
/// much of it, at most JITO_REGIONS in all. Free: every copy is the same transaction, so at most
/// one lands, and each region has its own rate limit. The launch's producer is often nearer
/// another one: of the EU launch slots of 25-26 Sep about 40% were produced in Frankfurt, 32% in
/// the Netherlands and 12% in London. The probe times include the TLS handshake (about three round
/// trips), so 60 ms takes in Amsterdam (+20 ms probed from Frankfurt) and London (+45 ms), not
/// Dublin (+71 ms) or farther.
const NEAR_EXTRA_MS: f64 = 60.0;
const JITO_REGIONS: usize = 3;

pub async fn init_jito() -> Result<()> {
    let bases: Vec<(String, String)> = match &TRADE_SETTING.jito_block_engine_url {
        Some(url) => vec![("JITO_BLOCK_ENGINE_URL".to_string(), url.trim_end_matches('/').to_string())],
        None => {
            let regions = jito_regions().into_iter().filter(|r| r.name != "global").collect();
            let near = near_regions(regions, NEAR_EXTRA_MS, JITO_REGIONS).await;
            if near.is_empty() {
                return Err(anyhow!("no jito region is reachable"));
            }
            near.into_iter().map(|(latency, r)| (format!("{} ({:.1} ms)", r.name, latency.as_secs_f64() * 1e3), r.base_url)).collect()
        }
    };
    for (_, base) in &bases {
        spawn_keep_alive(jito_probe_url(base));
    }
    info!("jito: every transaction to {}", bases.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", "));
    let query = if TRADE_SETTING.jito_bundle_only { "?bundleOnly=true" } else { "" };
    let _ = JITO_URLS.set(bases.iter().map(|(_, base)| format!("{base}/api/v1/transactions{query}")).collect());
    Ok(())
}

/// Sends to every chosen region at once; accepted when one region accepts.
pub async fn send_jito(tx_base64: &str) -> Result<String> {
    let urls = JITO_URLS.get().ok_or_else(|| anyhow!("Jito is not initialised"))?;
    let headers: Vec<(&str, &str)> = TRADE_SETTING.jito_uuid.as_deref().map(|uuid| vec![("x-jito-auth", uuid)]).unwrap_or_default();
    first_accepted(futures::future::join_all(urls.iter().map(|url| send_json_rpc_transaction("jito", url, tx_base64, &headers))).await)
}
