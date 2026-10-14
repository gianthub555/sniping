use {
    super::nozomi_regions::{nozomi_probe_url, nozomi_regions},
    crate::{
        config::{env_opt, HTTP_CLIENT, TRADE_SETTING},
        service::utils::services::{fastest_region, ranked_regions, send_json_rpc_transaction, spawn_keep_alive},
    },
    anyhow::{anyhow, bail, Result},
    futures::stream::{FuturesUnordered, StreamExt},
    log::{info, warn},
    solana_transaction::versioned::VersionedTransaction,
    std::sync::OnceLock,
};

static NOZOMI_URL: OnceLock<String> = OnceLock::new();
/// `<region>/api/sendBundle?c=<KEY>` for the two fastest regions: Nozomi does not forward bundles
/// between regions, so each bundle goes to both (the same signed transaction lands at most once).
static NOZOMI_BUNDLE_URLS: OnceLock<Vec<String>> = OnceLock::new();
/// `<region>/api/sendTransaction2?c=<KEY>`: API v2, a base64 body with no JSON, which Nozomi
/// documents as its lowest-latency submission over a warm connection. No signature comes back
/// (the caller knows it).
static NOZOMI_V2_URL: OnceLock<String> = OnceLock::new();
/// A region that has not answered by then is not waited for (a launch is over in a slot or two).
const BUNDLE_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);



pub async fn init_nozomi() -> Result<()> {
    let url = match (&TRADE_SETTING.nozomi_api_url, env_opt("NOZOMI_API_KEY")) {
        (Some(url), _) => {
            if !url.contains("c=") {
                bail!("NOZOMI_API_URL must include the API key as ?c=<KEY>");
            }
            url.clone()
        }
        (None, Some(key)) => {
            let region = fastest_region("nozomi", nozomi_regions()).await?;
            format!("{}/?c={key}", region.base_url)
        }
        (None, None) => bail!("CONFIRM_SERVICE=NOZOMI needs NOZOMI_API_URL or NOZOMI_API_KEY"),
    };
    let base = url.split('?').next().unwrap_or(&url).trim_end_matches('/');
    spawn_keep_alive(nozomi_probe_url(base));
    let key = url.split_once("c=").map(|(_, k)| k.split('&').next().unwrap_or(k)).unwrap_or_default();
    let mut bundle_urls = vec![format!("{base}/api/sendBundle?c={key}")];
    if TRADE_SETTING.nozomi_bundle && TRADE_SETTING.nozomi_api_url.is_none() {
        let second = ranked_regions(nozomi_regions())
            .await
            .into_iter()
            .map(|(_, r)| r.base_url)
            .find(|b| b.trim_end_matches('/') != base);
        if let Some(second) = second {
            info!("nozomi bundles: also sent through {second}");
            // Its connections are kept warm too; without this every buy opened a new one.
            spawn_keep_alive(nozomi_probe_url(&second));
            bundle_urls.push(format!("{}/api/sendBundle?c={key}", second.trim_end_matches('/')));
        }
    }
    let _ = NOZOMI_BUNDLE_URLS.set(bundle_urls);
    let _ = NOZOMI_V2_URL.set(format!("{base}/api/sendTransaction2?c={key}"));
    let _ = NOZOMI_URL.set(url);
    Ok(())
}


/// Nozomi's Send Bundle body: each member as a big-endian u16 length, then its bytes.
pub fn nozomi_bundle_body(members: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::with_capacity(members.iter().map(|m| m.len() + 2).sum());
    for member in members {
        body.extend_from_slice(&(member.len() as u16).to_be_bytes());
        body.extend_from_slice(member);
    }
    body
}

/// Sends `txs` (1-4, signed with one blockhash) as a Nozomi bundle: all land in order in one slot,
/// or none does. Nozomi delivers it once to each block builder; it is not retried or forwarded to
/// other regions. One member must carry the Nozomi tip.
pub async fn send_nozomi_bundle(txs: &[&VersionedTransaction]) -> Result<()> {
    let urls = NOZOMI_BUNDLE_URLS.get().ok_or_else(|| anyhow!("Nozomi is not initialised"))?;
    let members = txs.iter().map(|tx| bincode::serialize(tx)).collect::<std::result::Result<Vec<_>, _>>()?;
    let body = nozomi_bundle_body(&members);
    // The first region to accept wins; a slow or refusing region never holds the buy up (the
    // bundle is out as soon as one has it). Each region gets its own short timeout.
    let mut pending: FuturesUnordered<_> = urls
        .iter()
        .map(|url| {
            let (url, body) = (url.clone(), body.clone());
            let host = url.split('/').nth(2).unwrap_or("nozomi").to_string();
            tokio::spawn(async move {
                let sent = HTTP_CLIENT
                    .post(&url)
                    .header("Content-Type", "application/octet-stream")
                    .body(body)
                    .timeout(BUNDLE_SEND_TIMEOUT)
                    .send()
                    .await;
                let result: Result<()> = match sent {
                    Ok(response) if response.status().is_success() => Ok(()),
                    Ok(response) => {
                        let status = response.status();
                        let text = response.text().await.unwrap_or_default();
                        Err(anyhow!("HTTP {status}: {}", text.trim()))
                    }
                    Err(e) => Err(e.into()),
                };
                if let Err(e) = &result {
                    warn!("nozomi {host}: bundle not accepted: {e:#}");
                }
                result
            })
        })
        .collect();
    let mut errors = Vec::new();
    while let Some(done) = pending.next().await {
        match done {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => errors.push(format!("{e:#}")),
            Err(e) => errors.push(e.to_string()),
        }
    }
    bail!("nozomi rejected the bundle: {}", errors.join("; "))
}

/// The plain send over API v2 (see `NOZOMI_V2_URL`).
pub async fn send_nozomi_fast(tx_base64: &str) -> Result<()> {
    let url = NOZOMI_V2_URL.get().ok_or_else(|| anyhow!("Nozomi is not initialised"))?;
    let response = HTTP_CLIENT
        .post(url)
        .header("Content-Type", "text/plain")
        .body(tx_base64.to_string())
        .timeout(BUNDLE_SEND_TIMEOUT)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("nozomi v2 rejected the transaction (HTTP {status}): {}", text.trim());
    }
    Ok(())
}

pub async fn send_nozomi(tx_base64: &str) -> Result<String> {
    let url = NOZOMI_URL.get().ok_or_else(|| anyhow!("Nozomi is not initialised"))?;
    send_json_rpc_transaction("nozomi", url, tx_base64, &[]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_body_is_length_prefixed_members() {
        let body = nozomi_bundle_body(&[vec![1, 2, 3], vec![9; 300]]);
        assert_eq!(&body[..5], &[0, 3, 1, 2, 3]);
        assert_eq!(&body[5..7], &[1, 44]);
        assert_eq!(body.len(), 2 + 3 + 2 + 300);
    }
}
