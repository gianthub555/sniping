//! Launch-triggered landing probes: on each pump.fun create seen on the bot's gRPC stream, send a
//! small memo transaction through one landing path, with the buy's priority fee per compute unit
//! and that path's tip, and record where it lands relative to the create.
//!
//!   probe --via nozomi|zero_slot|jito|helius|all [--count 6] [--gap-secs 4] [--out probes.jsonl]
//!   probe --vias nozomi,jito [--cu 125000] ...   one probe per route on the same create (a
//!       head-to-head), each with its own route's tip; JITO_BUNDLE_ONLY=true sends Jito as bundles.
//!       Each bundle's id and its Jito status (getInflightBundleStatuses) are printed;
//!       JITO_SEND_BUNDLE=true uses sendBundle instead of sendTransaction?bundleOnly=true, and
//!       JITO_VARIANT=nofee|v0|tiponly|split builds the bundle differently (see `jito_variant`).
//!
//!   probe --via fanout ...   the bot's fanned-out buy path (NONCE_FANOUT): one memo copy per
//!       FANOUT_SERVICES service, each with its own tip, all advancing one nonce account from
//!       nonce_accounts.txt, so one copy lands; the line names the service whose copy landed.
//!       `--vias nozomi_both,fanout` puts it head to head with the old buy path on the same creates.
//!
//!   probe --vias nozomi_both#0,nozomi#0.002 --lock-curve --cu 120000 --producer-near Frankfurt
//!       Contended probes: `--lock-curve` adds 0-lamport transfers to the accounts a buy write-locks
//!       and shares with other buyers (the curve, its token account, a fee recipient and a buyback
//!       fee recipient), so each probe queues with the real buys (the plain memo
//!       probes compete with nothing, which is why tips and priority fees seemed not to matter on
//!       26 Sep). `#SOL` after a route is that probe's total priority fee, so bids race head to
//!       head on one create. `--producer-near` probes only creates whose block producer's city,
//!       country or continent code matches one of the names (unknown producers are skipped).
//!
//! Each probe is one line of JSON: the create, the probe, when each step happened (unix ms) and,
//! a few seconds later, both landing slots and block positions. `all` is the bot's buy path
//! (every CONFIRM_SERVICES service, all their tips).

use {
    anyhow::{anyhow, bail, Context, Result},
    futures::{SinkExt, StreamExt},
    serde_json::json,
    sniper_bot::{
        config::{ConfirmService, PAYER, RPC_CLIENT, TRADE_SETTING},
        service::{self, helius_sender, jito::send_jito, nozomi::send_nozomi, zero_slot::send_zero_slot},
        utils::{
            blockhash::{get_recent_blockhash, recent_blockhash_handler, wait_for_blockhash},
            utils::MEMO_PROGRAM_ID,
        },
    },
    solana_instruction::Instruction,
    solana_message::{Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        collections::HashMap,
        io::Write,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
    yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient},
    yellowstone_grpc_proto::geyser::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions,
        SubscribeRequestPing,
    },
};

/// The pump.fun mint authority: in every create, in no trade.
const MINT_AUTHORITY: &str = "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM";
const COMPUTE_BUDGET_ID: Pubkey = solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111");
/// Enough for the compute budget, memo and tip instructions (`--cu` raises it to a buy's limit,
/// so priority and tip per compute unit match a real buy).
const PROBE_CU_LIMIT: u32 = 20_000;

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

struct Args {
    via: String,
    count: usize,
    gap: Duration,
    out: String,
    /// Only probe creates in these slots (a JSON array), e.g. slots led by nearby validators.
    slots: Option<std::collections::HashSet<u64>>,
    /// Only probe creates whose fee payer spent at least this much SOL (dev buy plus ~0.03 SOL of
    /// rent and fees), like the launches the entry rules consider.
    min_spend_sol: f64,
    /// Fire a Nozomi-only probe and a race probe (every path) on the same create.
    pair: bool,
    /// One probe per route on the same create.
    vias: Vec<String>,
    cu: u32,
    /// Take the coin's bonding-curve write lock, like a buy.
    lock_curve: bool,
    /// Probe only creates whose producer is in one of these places.
    producer_near: Vec<String>,
    /// Simulate each probe on RPC instead of sending it (costs nothing; checks the transactions).
    simulate: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { via: String::new(), count: 6, gap: Duration::from_secs(4), out: "probes.jsonl".into(), slots: None, pair: false, min_spend_sol: 0.0, vias: Vec::new(), cu: PROBE_CU_LIMIT, lock_curve: false, producer_near: Vec::new(), simulate: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{a} needs a value"));
        match a.as_str() {
            "--via" => args.via = value()?,
            "--count" => args.count = value()?.parse()?,
            "--gap-secs" => args.gap = Duration::from_secs(value()?.parse()?),
            "--out" => args.out = value()?,
            "--slots" => args.slots = Some(serde_json::from_str(&std::fs::read_to_string(value()?)?)?),
            "--pair" => args.pair = true,
            "--vias" => args.vias = value()?.split(',').map(|s| s.trim().to_string()).collect(),
            "--cu" => args.cu = value()?.parse()?,
            "--min-dev-sol" => args.min_spend_sol = value()?.parse::<f64>()? + 0.03,
            "--lock-curve" => args.lock_curve = true,
            "--simulate" => args.simulate = true,
            "--producer-near" => args.producer_near = value()?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            _ => bail!("usage: probe --via nozomi|zero_slot|jito|helius|all|race [--pair] [--count 6] [--gap-secs 4] [--out probes.jsonl] [--slots slots.json] [--lock-curve] [--producer-near Frankfurt,EU]"),
        }
    }
    if args.pair {
        args.via = "pair".into();
        args.vias = vec!["nozomi".into(), "race".into()];
    } else if !args.vias.is_empty() {
        if let Some(bad) = args.vias.iter().find(|v| !["nozomi", "nozomi_bundle", "nozomi_both", "zero_slot", "jito", "helius", "all", "race", "fanout"].contains(&route_of(v))) {
            bail!("unknown route {bad}");
        }
        args.via = args.vias.join("+");
    } else if !["nozomi", "nozomi_bundle", "nozomi_both", "zero_slot", "jito", "helius", "all", "race", "fanout"].contains(&args.via.as_str()) {
        bail!("--via must be nozomi, zero_slot, jito, helius, all, race or fanout");
    }
    Ok(args)
}

/// JITO_VARIANT (with JITO_BUNDLE_ONLY=true) builds the Jito bundle differently, to find which
/// bundles Jito keeps: `nofee` (no compute-budget instructions), `v0` (a v0 message), `tiponly`
/// (only the tip transfer) or `split` (the memo transaction without a tip, then a second
/// transaction with only the tip, sent together with sendBundle).
fn jito_variant() -> Option<String> {
    std::env::var("JITO_VARIANT").ok().filter(|v| !v.is_empty() && TRADE_SETTING.jito_bundle_only)
}

/// A via may carry its own Nozomi tip and its own total priority fee: `nozomi_bundle@0.02#0.002`
/// (SOL). Same route, other tip or bid, for ladders on one launch.
fn route_of(via: &str) -> &str {
    via.split(['@', '#']).next().unwrap_or(via)
}

fn tip_of(via: &str) -> Option<u64> {
    via.split_once('@').and_then(|(_, t)| t.split('#').next()?.parse::<f64>().ok()).map(|sol| (sol * 1e9) as u64)
}

/// The probe's priority fee in micro-lamports per compute unit: the via's total (`#SOL`) spread
/// over the CU limit, else the bot's setting.
fn priority_of(via: &str, cu: u32) -> u64 {
    match via.split_once('#').and_then(|(_, p)| p.parse::<f64>().ok()) {
        Some(sol) => ((sol * 1e9) as u128 * 1_000_000 / cu.max(1) as u128) as u64,
        None => TRADE_SETTING.priority_fee_micro_lamports,
    }
}

fn probe_tx(via: &str, note: &str, cu: u32, locks: &[Pubkey]) -> Result<VersionedTransaction> {
    let tip = tip_of(via);
    let priority = priority_of(via, cu);
    let via = route_of(via);
    let payer = PAYER.pubkey();
    let budget = |data: Vec<u8>| Instruction { program_id: COMPUTE_BUDGET_ID, accounts: vec![], data };
    let mut instructions = vec![
        budget([&[2u8][..], &cu.to_le_bytes()].concat()),
        budget([&[3u8][..], &priority.to_le_bytes()].concat()),
        Instruction { program_id: MEMO_PROGRAM_ID, accounts: vec![], data: note.as_bytes().to_vec() },
    ];
    // Buy-like probes also declare the bot's loaded-data limit (16 MiB), which the producer's
    // cost model counts when it ranks them.
    if !locks.is_empty() {
        instructions.insert(2, budget([&[4u8][..], &(16u32 * 1024 * 1024).to_le_bytes()].concat()));
    }
    // Nothing moves, but each transfer takes a write lock a buy takes.
    for account in locks {
        instructions.push(sniper_bot::utils::utils::transfer_sol(&payer, account, 0));
    }
    let variant = if via == "jito" { jito_variant() } else { None };
    match variant.as_deref() {
        None | Some("v0") => {}
        Some("nofee") => instructions.drain(..2).for_each(drop),
        Some("tiponly") => instructions.clear(),
        Some("split") => return signed(&instructions, false),
        Some(other) => bail!("unknown JITO_VARIANT {other}: nofee, v0, tiponly or split"),
    }
    match via {
        "all" => instructions.extend(service::tip_instructions(&payer)),
        "nozomi" | "nozomi_bundle" | "nozomi_both" => match tip {
            Some(lamports) => instructions.push(service::nozomi::nozomi_tip_instruction(&payer, lamports)),
            None => instructions.extend(service::tip_instruction(ConfirmService::Nozomi, &payer)),
        },
        "zero_slot" => instructions.extend(service::tip_instruction(ConfirmService::ZeroSlot, &payer)),
        "jito" => instructions.extend(service::tip_instruction(ConfirmService::Jito, &payer)),
        "helius" => instructions.extend(service::tip_instruction(ConfirmService::Helius, &payer)),
        // One transaction carrying every path's tip, sent through every path: the first copy to
        // land wins and all the tips are paid.
        "race" => {
            for service in [ConfirmService::Nozomi, ConfirmService::ZeroSlot, ConfirmService::Jito, ConfirmService::Helius] {
                instructions.extend(service::tip_instruction(service, &payer));
            }
        }
        _ => unreachable!(),
    }
    signed(&instructions, variant.as_deref() == Some("v0"))
}

/// The route a fanned-out copy for `service` is sent on (the bot's own way for each service).
fn fanout_route(service: ConfirmService) -> &'static str {
    match service {
        ConfirmService::Nozomi if TRADE_SETTING.nozomi_bundle => "nozomi_both",
        ConfirmService::Nozomi => "nozomi",
        ConfirmService::Jito => "jito",
        ConfirmService::Helius => "helius",
        ConfirmService::ZeroSlot => "zero_slot",
        ConfirmService::Rpc => "all",
    }
}

/// One memo copy per FANOUT_SERVICES service, each tipping only its service (the bot's fan-out
/// tip), all advancing `lease`'s nonce.
fn fanout_copies(note: &str, cu: u32, lease: &sniper_bot::utils::nonce::Lease) -> Result<Vec<(ConfirmService, VersionedTransaction)>> {
    let payer = PAYER.pubkey();
    let budget = |data: Vec<u8>| Instruction { program_id: COMPUTE_BUDGET_ID, accounts: vec![], data };
    TRADE_SETTING
        .fanout_services
        .iter()
        .map(|&service| {
            let mut ixs = vec![
                sniper_bot::utils::nonce::advance_instruction(&lease.account, &payer),
                budget([&[2u8][..], &cu.to_le_bytes()].concat()),
                budget([&[3u8][..], &TRADE_SETTING.priority_fee_micro_lamports.to_le_bytes()].concat()),
            ];
            ixs.extend(service::tip_instruction_for_amount(service, &payer, service::fanout_tip(service)));
            ixs.push(Instruction { program_id: MEMO_PROGRAM_ID, accounts: vec![], data: format!("{note} {service:?}").into_bytes() });
            let message = VersionedMessage::Legacy(Message::new_with_blockhash(&ixs, Some(&payer), &lease.value));
            Ok((service, VersionedTransaction::try_new(message, &[PAYER.as_ref()])?))
        })
        .collect()
}

/// The accounts a pump.fun buy of `mint` write-locks that other buyers share: the curve, its token
/// account, one of pump.fun's fee recipients and one of its buyback fee recipients (each picked at
/// random, as the bot's buy does; every pump.fun buy on the network writes one of these).
fn buy_locks(mint: &Pubkey, token_program: &Pubkey) -> Result<Vec<Pubkey>> {
    use rand::seq::SliceRandom;
    let config = sniper_bot::instructions::pumpfun_buy_ix::pumpfun_config()?;
    let curve = sniper_bot::instructions::pumpfun_buy_ix::bonding_curve_address(mint);
    let mut rng = rand::thread_rng();
    Ok(vec![
        curve,
        sniper_bot::utils::utils::get_associated_token_address(&curve, mint, token_program),
        *config.fee_recipients.choose(&mut rng).ok_or_else(|| anyhow!("no fee recipients"))?,
        *config.buyback_fee_recipients.choose(&mut rng).ok_or_else(|| anyhow!("no buyback fee recipients"))?,
    ])
}

fn signed(instructions: &[Instruction], v0: bool) -> Result<VersionedTransaction> {
    let payer = PAYER.pubkey();
    let blockhash = get_recent_blockhash()?;
    let message = if v0 {
        VersionedMessage::V0(solana_message::v0::Message::try_compile(&payer, instructions, &[], blockhash)?)
    } else {
        VersionedMessage::Legacy(Message::new_with_blockhash(instructions, Some(&payer), &blockhash))
    };
    Ok(VersionedTransaction::try_new(message, &[PAYER.as_ref()])?)
}

async fn send(via: &str, tx: &VersionedTransaction) -> Result<()> {
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bincode::serialize(tx)?);
    match route_of(via) {
        "all" => service::send(tx).await,
        "nozomi" => send_nozomi(&encoded).await.map(|_| ()),
        // The bot's own bundle path (both Nozomi regions when NOZOMI_BUNDLE=true).
        "nozomi_bundle" => service::nozomi::send_nozomi_bundle(&[tx]).await,
        // The bot's dual path: bundle and plain send of the same transaction.
        "nozomi_both" => {
            let members = [tx];
            let (b, p) = tokio::join!(service::nozomi::send_nozomi_bundle(&members), service::nozomi::send_nozomi_fast(&encoded));
            if b.is_err() && p.is_err() { bail!("both refused") } else { Ok(()) }
        }
        "zero_slot" => send_zero_slot(&encoded).await.map(|_| ()),
        "jito" if TRADE_SETTING.jito_bundle_only => {
            let tip = match jito_variant().as_deref() {
                Some("split") => {
                    let tip_tx = signed(&[service::tip_instruction(ConfirmService::Jito, &PAYER.pubkey()).expect("Jito tip")], false)?;
                    Some(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bincode::serialize(&tip_tx)?))
                }
                _ => None,
            };
            send_jito_bundle(&encoded, tip).await
        }
        "jito" => send_jito(&encoded).await.map(|_| ()),
        "helius" => helius_sender::send_helius_sender(&encoded).await.map(|_| ()),
        "race" => {
            let (a, b, c, d) = tokio::join!(
                send_nozomi(&encoded),
                send_zero_slot(&encoded),
                send_jito(&encoded),
                helius_sender::send_helius_sender(&encoded)
            );
            let accepted = [a.is_ok(), b.is_ok(), c.is_ok(), d.is_ok()];
            if accepted.iter().any(|ok| *ok) {
                Ok(())
            } else {
                Err(anyhow!("every path rejected the race probe: {:?}", [a.err(), b.err(), c.err(), d.err()]))
            }
        }
        _ => unreachable!(),
    }
}

/// A POST to Jito carrying the auth key (JITO_UUID) when there is one.
fn jito_request(url: &str) -> reqwest::RequestBuilder {
    let request = sniper_bot::config::HTTP_CLIENT.post(url);
    match &TRADE_SETTING.jito_uuid {
        Some(uuid) => request.header("x-jito-auth", uuid),
        None => request,
    }
}

/// Jito as a one-transaction bundle, printing the bundle id Jito assigns so its fate can be
/// looked up (getInflightBundleStatuses, 5 minutes). JITO_SEND_BUNDLE=true uses sendBundle
/// instead of sendTransaction?bundleOnly=true.
async fn send_jito_bundle(encoded: &str, tip_tx: Option<String>) -> Result<()> {
    let base = TRADE_SETTING.jito_block_engine_url.as_deref().ok_or_else(|| anyhow!("set JITO_BLOCK_ENGINE_URL"))?;
    let base = base.trim_end_matches('/');
    let (url, method, params) = if let Some(tip_tx) = tip_tx {
        (format!("{base}/api/v1/bundles"), "sendBundle", json!([[encoded, tip_tx], {"encoding": "base64"}]))
    } else if std::env::var("JITO_SEND_BUNDLE").is_ok_and(|v| v == "true") {
        (format!("{base}/api/v1/bundles"), "sendBundle", json!([[encoded], {"encoding": "base64"}]))
    } else {
        (format!("{base}/api/v1/transactions?bundleOnly=true"), "sendTransaction", json!([encoded, {"encoding": "base64"}]))
    };
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let response = jito_request(&url).json(&body).send().await?;
    let bundle_id = response.headers().get("x-bundle-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let status = response.status();
    let text = response.text().await?;
    println!("    jito {method}: HTTP {status}, bundle id {bundle_id:?}, reply {}", text.trim());
    if !status.is_success() || text.contains("\"error\"") {
        bail!("jito rejected the bundle: {}", text.trim());
    }
    let id = bundle_id.or_else(|| serde_json::from_str::<serde_json::Value>(&text).ok()?.get("result")?.as_str().map(str::to_string));
    if let Some(id) = id {
        let status_url = format!("{base}/api/v1/getInflightBundleStatuses");
        tokio::spawn(async move {
            let started = Instant::now();
            for wait_ms in [1200u64, 4000] {
                tokio::time::sleep(Duration::from_millis(wait_ms).saturating_sub(started.elapsed())).await;
                let body = json!({"jsonrpc": "2.0", "id": 1, "method": "getInflightBundleStatuses", "params": [[id]]});
                let reply = match jito_request(&status_url).json(&body).send().await {
                    Ok(r) => r.text().await.unwrap_or_default(),
                    Err(e) => e.to_string(),
                };
                println!("    bundle {} at +{wait_ms} ms: {}", &id[..8], reply.trim());
            }
        });
    }
    Ok(())
}

/// Slot and block position of each signature, once confirmed.
async fn positions(signatures: &[solana_signature::Signature]) -> Result<Vec<Option<(u64, usize)>>> {
    let statuses = RPC_CLIENT.get_signature_statuses_with_history(signatures).await?.value;
    let mut blocks: HashMap<u64, Vec<String>> = HashMap::new();
    let mut out = Vec::new();
    for (signature, status) in signatures.iter().zip(statuses) {
        let Some(slot) = status.map(|s| s.slot) else {
            out.push(None);
            continue;
        };
        if !blocks.contains_key(&slot) {
            let config = solana_rpc_client_api::config::RpcBlockConfig {
                transaction_details: Some(solana_transaction_status_client_types::TransactionDetails::Signatures),
                rewards: Some(false),
                max_supported_transaction_version: Some(1),
                commitment: Some(solana_commitment_config::CommitmentConfig::confirmed()),
                ..Default::default()
            };
            let block = RPC_CLIENT.get_block_with_config(slot, config).await.context("getBlock")?;
            blocks.insert(slot, block.signatures.unwrap_or_default());
        }
        let text = signature.to_string();
        out.push(blocks[&slot].iter().position(|s| *s == text).map(|i| (slot, i)));
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    sniper_bot::utils::logging::init_logger();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = parse_args()?;
    if TRADE_SETTING.dry_run {
        println!("note: DRY_RUN is on for the bot; probes are real transactions regardless");
    }
    service::init().await?;
    helius_sender::init_helius_sender().await?;
    let uses = |route: &str| args.via == route || args.vias.iter().any(|v| route_of(v) == route || v == "race") || args.via == "race" || args.via == "pair";
    if uses("zero_slot") {
        service::zero_slot::init_zero_slot().await?;
    }
    if uses("nozomi_bundle") || uses("nozomi_both") {
        service::nozomi::init_nozomi().await?;
    }
    if uses("jito") {
        service::jito::init_jito().await?;
    }
    if uses("fanout") {
        for s in &TRADE_SETTING.fanout_services {
            match s {
                ConfirmService::ZeroSlot => service::zero_slot::init_zero_slot().await?,
                ConfirmService::Nozomi => service::nozomi::init_nozomi().await?,
                ConfirmService::Jito => service::jito::init_jito().await?,
                _ => {}
            }
        }
        sniper_bot::utils::nonce::init().await?;
    }
    tokio::spawn(recent_blockhash_handler(RPC_CLIENT.clone()));
    wait_for_blockhash(Duration::from_secs(20)).await?;
    if args.lock_curve {
        sniper_bot::instructions::pumpfun_buy_ix::init_pumpfun_config(RPC_CLIENT.clone()).await?;
    }
    if !args.producer_near.is_empty() {
        sniper_bot::utils::leaders::spawn();
        println!("probing only creates produced in {}; loading the leader schedule...", args.producer_near.join(", "));
    }

    let url = env_opt("LOCAL_GRPC_URL").ok_or_else(|| anyhow!("LOCAL_GRPC_URL is not set"))?;
    let mut client = GeyserGrpcClient::build_from_shared(url)?
        .x_token(env_opt("GRPC_TOKEN"))?
        .tls_config(ClientTlsConfig::new().with_enabled_roots())?
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await
        .map_err(|e| anyhow!("cannot connect to the gRPC stream: {e}"))?;
    let request = SubscribeRequest {
        transactions: HashMap::from([(
            "creates".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: vec![MINT_AUTHORITY.to_string()],
                ..Default::default()
            },
        )]),
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    let (mut sink, mut stream) = client.subscribe_with_request(Some(request)).await.map_err(|e| anyhow!("subscribe: {e}"))?;
    println!(
        "probing via {} on {} creates (priority {} micro-lamports/CU, CU limit {}, Jito tip {} SOL{})",
        args.via,
        args.count,
        TRADE_SETTING.priority_fee_micro_lamports,
        args.cu,
        TRADE_SETTING.jito_tip_lamports as f64 / 1e9,
        match (TRADE_SETTING.jito_bundle_only, TRADE_SETTING.jito_uuid.is_some()) {
            (true, true) => ", Jito as bundles with the auth key",
            (true, false) => ", Jito as bundles without an auth key",
            (false, true) => ", Jito auth key set",
            (false, false) => "",
        }
    );
    let per_create = if args.vias.is_empty() { 1 } else { args.vias.len() };

    let mut sent = Vec::new();
    let mut next_allowed = std::time::Instant::now();
    while sent.len() < args.count * per_create {
        let Some(update) = stream.next().await else { bail!("stream ended") };
        let update = update.map_err(|s| anyhow!("stream: {s}"))?;
        let tx = match update.update_oneof {
            Some(UpdateOneof::Transaction(tx)) => tx,
            Some(UpdateOneof::Ping(_)) => {
                let _ = sink.send(SubscribeRequest { ping: Some(SubscribeRequestPing { id: 1 }), ..Default::default() }).await;
                continue;
            }
            _ => continue,
        };
        let detected_ms = now_ms();
        if std::time::Instant::now() < next_allowed {
            continue;
        }
        let spent = tx.transaction.as_ref().and_then(|t| t.meta.as_ref()).and_then(|m| {
            Some(m.pre_balances.first()?.saturating_sub(*m.post_balances.first()?) as f64 / 1e9)
        });
        if args.min_spend_sol > 0.0 && spent.is_none_or(|sol| sol < args.min_spend_sol) {
            continue;
        }
        if args.slots.as_ref().is_some_and(|s| !s.contains(&tx.slot)) {
            continue;
        }
        let place = sniper_bot::utils::leaders::producer(tx.slot).and_then(|(_, p)| p);
        if !args.producer_near.is_empty() {
            let near = place.as_ref().is_some_and(|p| {
                args.producer_near.iter().any(|n| {
                    p.continent.eq_ignore_ascii_case(n) || p.city.to_lowercase().contains(&n.to_lowercase()) || p.country.eq_ignore_ascii_case(n)
                })
            });
            if !near {
                continue;
            }
        }
        // The coin: every create mints its supply to the curve, so the mint is in the token balances.
        let mint = tx
            .transaction
            .as_ref()
            .and_then(|t| t.meta.as_ref())
            .and_then(|m| m.post_token_balances.first())
            .and_then(|b| b.mint.parse::<Pubkey>().ok());
        if args.lock_curve && mint.is_none() {
            continue;
        }
        let token_program = tx
            .transaction
            .as_ref()
            .and_then(|t| t.meta.as_ref())
            .and_then(|m| m.post_token_balances.first())
            .and_then(|b| b.program_id.parse::<Pubkey>().ok());
        let locks = match (mint.filter(|_| args.lock_curve), token_program) {
            (Some(m), Some(program)) => buy_locks(&m, &program)?,
            (Some(_), None) => continue,
            _ => Vec::new(),
        };
        let Some(info) = tx.transaction else { continue };
        let Ok(create_sig) = solana_signature::Signature::try_from(info.signature.as_slice()) else { continue };
        let n = sent.len() / per_create + 1;
        let vias: Vec<String> = if args.vias.is_empty() { vec![args.via.clone()] } else { args.vias.clone() };
        // Each via is a group of (route, transaction): one for the plain routes, one copy per
        // service for `fanout`.
        let mut leases = Vec::new();
        let mut groups: Vec<Vec<(String, VersionedTransaction)>> = Vec::new();
        for v in &vias {
            let note = format!("probe {n} {v} {}", &create_sig.to_string()[..8]);
            if route_of(v) == "fanout" {
                let Some(lease) = sniper_bot::utils::nonce::lease() else {
                    println!("  every nonce account is in use; skipping this create");
                    groups.push(Vec::new());
                    continue;
                };
                leases.push(lease);
                groups.push(fanout_copies(&note, args.cu, &lease)?.into_iter().map(|(s, t)| (fanout_route(s).to_string(), t)).collect());
            } else {
                groups.push(vec![(v.clone(), probe_tx(v, &note, args.cu, &locks)?)]);
            }
        }
        let built_ms = now_ms();
        if args.simulate {
            for (route, t) in groups.iter().flatten() {
                match RPC_CLIENT.simulate_transaction(t).await {
                    Ok(r) => println!(
                        "  simulated {route} on {} (producer {}): err {:?}, {} CU, priority {} micro-lamports/CU, accounts {}",
                        mint.map(|m| m.to_string()).unwrap_or_default(),
                        place.as_ref().map(|p| p.to_string()).unwrap_or_else(|| "unknown".into()),
                        r.value.err,
                        r.value.units_consumed.unwrap_or_default(),
                        priority_of(route, args.cu),
                        t.message.static_account_keys().len()
                    ),
                    Err(e) => println!("  simulate {route}: {e:#}"),
                }
            }
            sent.extend((0..per_create).map(|_| Default::default()));
            for lease in leases {
                sniper_bot::utils::nonce::release(lease);
            }
            next_allowed = std::time::Instant::now() + args.gap;
            continue;
        }
        let results = futures::future::join_all(groups.iter().map(|g| futures::future::join_all(g.iter().map(|(route, t)| send(route, t))))).await;
        let sent_ms = now_ms();
        for lease in leases {
            sniper_bot::utils::nonce::release(lease);
        }
        for ((via, group), results) in vias.iter().zip(&groups).zip(results) {
            if group.is_empty() {
                continue;
            }
            for ((route, _), r) in group.iter().zip(&results) {
                if let Err(e) = r {
                    println!("  send via {route} failed: {e:#}");
                }
            }
            let signatures: Vec<solana_signature::Signature> = group.iter().map(|(_, t)| t.signatures[0]).collect();
            let routes: Vec<String> = group.iter().map(|(r, _)| r.clone()).collect();
            println!("  create {create_sig} (slot {}) -> {via} probe {} sent in {} ms", tx.slot, signatures[0], sent_ms - detected_ms);
            let extra = (mint, place.as_ref().map(|p| format!("{p} ({})", p.continent)));
            sent.push((create_sig, tx.slot, signatures, routes, detected_ms, built_ms, sent_ms, results.iter().any(|r| r.is_ok()), via.clone(), n, extra));
        }
        next_allowed = std::time::Instant::now() + args.gap;
    }

    if args.simulate {
        return Ok(());
    }
    println!("waiting 20 s for confirmations...");
    tokio::time::sleep(Duration::from_secs(20)).await;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&args.out)?;
    for (create_sig, create_slot, signatures, routes, detected_ms, built_ms, sent_ms, ok, via, n, (mint, producer)) in sent {
        let all: Vec<solana_signature::Signature> = std::iter::once(create_sig).chain(signatures.iter().copied()).collect();
        let found = positions(&all).await.unwrap_or_else(|e| {
            println!("  positions for {}: {e:#}", signatures[0]);
            vec![None; all.len()]
        });
        // The copy that landed (a fan-out lands one; the plain routes have one copy).
        let landed = (1..all.len()).find(|&i| found[i].is_some()).unwrap_or(1);
        let probe_sig = all[landed];
        let landed_route = routes.get(landed - 1).cloned();
        let pos = vec![found[0], found[landed]];
        let line = json!({
            "via": via,
            "pair": n,
            "create": create_sig.to_string(),
            "create_stream_slot": create_slot,
            "probe": probe_sig.to_string(),
            "landed_route": landed_route,
            "copies": signatures.len(),
            "detected_ms": detected_ms as u64,
            "built_ms": built_ms as u64,
            "sent_ms": sent_ms as u64,
            "send_ok": ok,
            "create_at": pos[0].map(|(s, i)| json!([s, i])),
            "probe_at": pos[1].map(|(s, i)| json!([s, i])),
            "mint": mint.map(|m| m.to_string()),
            "producer": producer,
            "lock_curve": args.lock_curve,
            "cu": args.cu,
        });
        writeln!(file, "{line}")?;
        match (pos[0], pos[1]) {
            (Some((cs, ci)), Some((ps, pi))) => println!(
                "  {probe_sig}: landed slot +{} {}",
                ps.saturating_sub(cs),
                if ps == cs { format!("{} positions after the create", pi as i64 - ci as i64) } else { format!("at position {pi}") }
            ),
            _ => println!("  {probe_sig}: not landed (or create not found)"),
        }
    }
    Ok(())
}
