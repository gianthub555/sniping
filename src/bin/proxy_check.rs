//! Reads a ShredStream proxy's entries service for a while and counts, per slot, the
//! transactions it delivered and when the slot's first batch arrived, then compares the counts
//! with the chain's blocks: is the deshredded feed complete?
//!
//!   proxy_check [http://127.0.0.1:9999] [seconds]

use {
    anyhow::Result,
    sniper_bot::{config::RPC_CLIENT, processors::shred_wire::entry_transactions},
    std::{collections::BTreeMap, time::{Duration, Instant}},
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

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9999".into());
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let channel = tonic::transport::Endpoint::from_shared(url.clone())?.connect().await?;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await?;
    let path = tonic::codegen::http::uri::PathAndQuery::from_static("/shredstream.ShredstreamProxy/SubscribeEntries");
    let codec = tonic_prost::ProstCodec::<SubscribeEntriesRequest, EntryBatch>::default();
    let mut stream = grpc.server_streaming(tonic::Request::new(SubscribeEntriesRequest {}), path, codec).await?.into_inner();
    // slot -> (batches, txs, unreadable batches, first seen)
    let mut slots: BTreeMap<u64, (u32, usize, u32, Instant)> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        let Ok(Ok(Some(batch))) = tokio::time::timeout(Duration::from_secs(2), stream.message()).await else { continue };
        let e = slots.entry(batch.slot).or_insert((0, 0, 0, Instant::now()));
        e.0 += 1;
        match entry_transactions(&batch.entries) {
            Some(txs) => e.1 += txs.len(),
            None => e.2 += 1,
        }
    }
    let keys: Vec<u64> = slots.keys().copied().collect();
    // Skip the first and last slots (partially covered by the window).
    for slot in keys.iter().skip(2).take(keys.len().saturating_sub(4)).take(6) {
        let (batches, txs, bad, _) = slots[slot];
        let chain = RPC_CLIENT
            .get_block_with_config(
                *slot,
                solana_rpc_client_api::config::RpcBlockConfig {
                    transaction_details: Some(solana_transaction_status_client_types::TransactionDetails::Signatures),
                    rewards: Some(false),
                    max_supported_transaction_version: Some(1),
                    ..Default::default()
                },
            )
            .await
            .ok()
            .and_then(|b| b.signatures.map(|s| s.len()));
        println!(
            "slot {slot}: feed {txs} txs in {batches} batches ({bad} unreadable); chain {}",
            chain.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
        );
    }
    println!("{} slots seen in {secs} s", keys.len());
    Ok(())
}
