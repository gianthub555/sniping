//! Races two early launch feeds on the same launches with one clock: a ShredStream proxy's
//! entries service and Helius's preprocessed WebSocket. For every pump.fun create both show,
//! prints how much earlier the proxy saw it (negative: Helius was first).
//!
//!   feed_race [proxy url, default http://127.0.0.1:9999] [seconds, default 600]
//!
//! The Helius URL comes from the bot's settings (HELIUS_PREPROCESSED / PREPROCESSED_WS_URL).

use {
    anyhow::{anyhow, Result},
    futures::{SinkExt, StreamExt},
    sniper_bot::{
        config::TRADE_SETTING,
        instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID,
        processors::{shred_feed::parse_launches, shred_wire},
    },
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    },
    tokio_tungstenite::tungstenite::Message,
};

#[derive(Clone, PartialEq, prost::Message)]
struct SubscribeEntriesRequest {}

#[derive(Clone, PartialEq, prost::Message)]
struct EntryBatch {
    #[prost(uint64, tag = "1")]
    slot: u64,
    #[prost(bytes = "vec", tag = "2")]
    entries: Vec<u8>,
}

/// First sighting of each mint, per feed.
type Seen = Arc<Mutex<HashMap<Pubkey, Instant>>>;

fn note(seen: &Seen, mints: impl Iterator<Item = Pubkey>) {
    let now = Instant::now();
    let mut seen = seen.lock().unwrap();
    for mint in mints {
        seen.entry(mint).or_insert(now);
    }
}

async fn proxy(url: String, seen: Seen) -> Result<()> {
    let channel = tonic::transport::Endpoint::from_shared(url)?.tcp_nodelay(true).connect().await?;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await?;
    let path = tonic::codegen::http::uri::PathAndQuery::from_static("/shredstream.ShredstreamProxy/SubscribeEntries");
    let codec = tonic_prost::ProstCodec::<SubscribeEntriesRequest, EntryBatch>::default();
    let mut stream = grpc.server_streaming(tonic::Request::new(SubscribeEntriesRequest {}), path, codec).await?.into_inner();
    while let Some(batch) = stream.message().await? {
        if let Some(txs) = shred_wire::entry_transactions(&batch.entries) {
            let mints: Vec<Pubkey> = txs
                .iter()
                .filter(|t| t.mentions(&PUMP_PROGRAM_ID))
                .flat_map(|t| parse_launches(&t.top_instructions()).into_iter().map(|l| l.mint))
                .collect();
            note(&seen, mints.into_iter());
        }
    }
    Err(anyhow!("proxy stream ended"))
}

async fn helius(url: String, seen: Seen) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async_with_config(&url, None, true).await.map_err(|e| anyhow!("connect: {}", e.to_string().replace(&url, "<url>")))?;
    let (mut write, mut read) = ws.split();
    let subscribe = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "preprocessedSubscribe",
        "params": [{ "accountInclude": [PUMP_PROGRAM_ID.to_string()] }],
    });
    write.send(Message::Text(subscribe.to_string().into())).await?;
    while let Some(message) = read.next().await {
        match message? {
            Message::Binary(bytes) if bytes.len() > 73 => {
                if let Some(tx) = shred_wire::decode_transaction(&bytes[73..]) {
                    note(&seen, parse_launches(&tx.top_instructions()).into_iter().map(|l| l.mint));
                }
            }
            Message::Ping(p) => write.send(Message::Pong(p)).await?,
            _ => {}
        }
    }
    Err(anyhow!("helius stream ended"))
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let proxy_url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9999".into());
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(600);
    let helius_url = TRADE_SETTING.preprocessed_url.clone().ok_or_else(|| anyhow!("set HELIUS_PREPROCESSED=true"))?;
    let (a, b): (Seen, Seen) = (Default::default(), Default::default());
    let (pa, pb) = (a.clone(), b.clone());
    tokio::spawn(async move { if let Err(e) = proxy(proxy_url, pa).await { eprintln!("proxy: {e:#}") } });
    tokio::spawn(async move { if let Err(e) = helius(helius_url, pb).await { eprintln!("helius: {e:#}") } });
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let (a, b) = (a.lock().unwrap().clone(), b.lock().unwrap().clone());
    let mut leads: Vec<f64> = a
        .iter()
        .filter_map(|(mint, pa)| b.get(mint).map(|hb| hb.duration_since(*pa).as_secs_f64() * 1e3 - pa.duration_since(*hb).as_secs_f64() * 1e3))
        .collect();
    leads.sort_by(|x, y| x.total_cmp(y));
    let q = |p: f64| leads.get(((leads.len().max(1) - 1) as f64 * p).round() as usize).copied().unwrap_or(0.0);
    println!(
        "{} launches on both feeds in {secs} s ({} only on the proxy, {} only on Helius)",
        leads.len(),
        a.keys().filter(|m| !b.contains_key(m)).count(),
        b.keys().filter(|m| !a.contains_key(m)).count()
    );
    println!(
        "proxy ahead of Helius: median {:+.1} ms, p10 {:+.1}, p25 {:+.1}, p75 {:+.1}, p90 {:+.1}; proxy first on {:.0}%",
        q(0.5),
        q(0.1),
        q(0.25),
        q(0.75),
        q(0.9),
        100.0 * leads.iter().filter(|l| **l > 0.0).count() as f64 / leads.len().max(1) as f64
    );
    Ok(())
}
