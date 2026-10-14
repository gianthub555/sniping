use {
    super::ping::http_latency,
    crate::config::HTTP_CLIENT,
    anyhow::{anyhow, bail, Result},
    futures::future::join_all,
    log::{debug, info},
    serde_json::{json, Value},
    std::time::Duration,
};


#[derive(Debug, Clone)]
pub struct Region {
    pub name: &'static str,
    pub base_url: String,
    pub probe_url: String,
}

/// Probes every region concurrently and returns the one with the lowest latency.
/// Every reachable region of `service`, fastest first.
pub async fn ranked_regions(regions: Vec<Region>) -> Vec<(Duration, Region)> {
    let latencies = join_all(regions.iter().map(|r| http_latency(&r.probe_url))).await;
    let mut ranked: Vec<(Duration, Region)> =
        latencies.into_iter().zip(regions).filter_map(|(latency, region)| latency.map(|l| (l, region))).collect();
    ranked.sort_by_key(|(latency, _)| *latency);
    ranked
}

/// The fastest reachable region and the others within `extra_ms` of it, at most `max`.
pub async fn near_regions(regions: Vec<Region>, extra_ms: f64, max: usize) -> Vec<(Duration, Region)> {
    let ranked = ranked_regions(regions).await;
    let Some(best) = ranked.first().map(|(latency, _)| latency.as_secs_f64() * 1e3) else {
        return Vec::new();
    };
    ranked.into_iter().filter(|(latency, _)| latency.as_secs_f64() * 1e3 <= best + extra_ms).take(max.max(1)).collect()
}

/// The first accepted send of several (the same transaction to several endpoints), or every
/// refusal when none accepted.
pub fn first_accepted(results: Vec<Result<String>>) -> Result<String> {
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(id) => return Ok(id),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    bail!("{}", errors.join("; "))
}

pub async fn fastest_region(service: &str, regions: Vec<Region>) -> Result<Region> {
    let latencies = join_all(regions.iter().map(|r| http_latency(&r.probe_url))).await;
    let mut ranked: Vec<(Duration, Region)> = latencies
        .into_iter()
        .zip(regions)
        .filter_map(|(latency, region)| latency.map(|l| (l, region)))
        .collect();
    ranked.sort_by_key(|(latency, _)| *latency);
    for (latency, region) in &ranked {
        debug!("{service} {}: {:.1} ms", region.name, latency.as_secs_f64() * 1e3);
    }
    let (latency, region) = ranked
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no {service} region is reachable"))?;
    info!(
        "{service}: using {} ({}, {:.1} ms)",
        region.name,
        region.base_url,
        latency.as_secs_f64() * 1e3
    );
    Ok(region)
}

/// Connections kept open to each send host. A buy sends several requests to one host at once
/// (Nozomi: the bundle and the plain send) and HTTP/1.1 carries one at a time per connection, so
/// with one warm connection the others paid a DNS lookup and a TCP handshake first.
const WARM_CONNECTIONS: usize = 3;

/// Keeps `WARM_CONNECTIONS` pooled connections to `probe_url` alive (services drop idle
/// connections at ~65 s): that many probes at once, every 20 s, so the pool holds that many.
pub fn spawn_keep_alive(probe_url: String) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(20));
        loop {
            ticker.tick().await;
            let probes = (0..WARM_CONNECTIONS).map(|_| HTTP_CLIENT.get(&probe_url).timeout(Duration::from_secs(5)).send());
            let _ = futures::future::join_all(probes).await;
        }
    });
}

/// POSTs a JSON-RPC `sendTransaction` with a base64 transaction and returns the `result` string.
pub async fn send_json_rpc_transaction(
    service: &str,
    url: &str,
    tx_base64: &str,
    extra_headers: &[(&str, &str)],
) -> Result<String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [tx_base64, {"encoding": "base64", "skipPreflight": true}],
    });
    let mut request = HTTP_CLIENT.post(url).json(&body);
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    let parsed: Option<Value> = serde_json::from_str(&text).ok();

    if let Some(error) = parsed.as_ref().and_then(|v| v.get("error")) {
        bail!("{service} rejected the transaction (HTTP {status}): {error}");
    }
    if !status.is_success() {
        bail!("{service} returned HTTP {status}: {}", text.trim());
    }
    match parsed.as_ref().and_then(|v| v.get("result")).and_then(Value::as_str) {
        Some(signature) => Ok(signature.to_string()),
        None => bail!("{service} returned an unexpected response: {}", text.trim()),
    }
}
