
















use {
    anyhow::{anyhow, bail, Context, Result},
    futures::{SinkExt, StreamExt},
    solana_commitment_config::CommitmentConfig,
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    sniper_bot::{
        config::{env_opt, lamports_to_sol, ConfirmService, PUBKEY, RPC_CLIENT, TRADE_SETTING},
        instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID,
        service::{
            self,
            jito::{jito_probe_url, jito_regions},
            nozomi::{nozomi_probe_url, nozomi_regions},
            utils::{ping::http_latency, services::Region},
            zero_slot::{zero_slot_probe_url, zero_slot_regions},
        },
        utils::{
            blockhash::{recent_blockhash_handler, wait_for_blockhash},
            build_and_sign::build_and_sign,
            csv_logger::TRADE_LOG_PATH,
            utils::{MEMO_PROGRAM_ID, WSOL_MINT},
        },
    },
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        str::FromStr,
        sync::{Arc, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
    yellowstone_grpc_client::GeyserGrpcClient,
    yellowstone_grpc_proto::{
        prelude::{
            subscribe_update::UpdateOneof, CommitmentLevel, SlotStatus, SubscribeRequest,
            SubscribeRequestFilterSlots, SubscribeRequestFilterTransactions, SubscribeRequestPing,
            SubscribeUpdateTransaction, TokenBalance,
        },
        tonic::transport::ClientTlsConfig,
    },
};


const BUY_WINDOW_SLOTS: u64 = 20;
const LAND_TEST_TIMEOUT: Duration = Duration::from_secs(30);

struct Args {
    seconds: u64,
    land_tests: usize,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { seconds: 120, land_tests: 0 };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| anyhow!("{name} needs a value"));
        match arg.as_str() {
            "--seconds" => args.seconds = value("--seconds")?.parse().context("--seconds")?,
            "--land-test" => args.land_tests = value("--land-test")?.parse().context("--land-test")?,
            "-h" | "--help" => {
                println!("usage: latency [--seconds 120] [--land-test N]");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}; usage: latency [--seconds 120] [--land-test N]"),
        }
    }
    if args.seconds < 10 {
        bail!("--seconds must be at least 10");
    }
    Ok(args)
}

#[derive(Default)]
struct SlotTimes {
    
    first_shred: Option<Instant>,
    created_bank: Option<Instant>,
    processed: Option<Instant>,
    confirmed: Option<Instant>,
}

struct Launch {
    slot: u64,
    received: Instant,
    creator: Pubkey,
    
    buys: Vec<(u64, Instant)>,
}

#[derive(Default)]
struct Recorder {
    
    transport_ms: Vec<f64>,
    slot_transport_ms: Vec<f64>,
    slots: BTreeMap<u64, SlotTimes>,
    latest_slot: u64,
    launches: HashMap<Pubkey, Launch>,
    transactions: u64,
    
    own: HashMap<Signature, (u64, Instant)>,
}

impl Recorder {
    fn transport(&mut self, created_at: Option<&yellowstone_grpc_proto::prost_types::Timestamp>, is_slot: bool) {
        let Some(ts) = created_at else { return };
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else { return };
        let sent_ms = ts.seconds as f64 * 1000.0 + ts.nanos as f64 / 1e6;
        let delay = now.as_secs_f64() * 1000.0 - sent_ms;
        if is_slot { self.slot_transport_ms.push(delay) } else { self.transport_ms.push(delay) }
    }

    fn slot(&mut self, slot: u64, status: SlotStatus, at: Instant) {
        self.latest_slot = self.latest_slot.max(slot);
        let times = self.slots.entry(slot).or_default();
        let field = match status {
            SlotStatus::SlotFirstShredReceived => &mut times.first_shred,
            SlotStatus::SlotCreatedBank => &mut times.created_bank,
            SlotStatus::SlotProcessed => &mut times.processed,
            SlotStatus::SlotConfirmed => &mut times.confirmed,
            _ => return,
        };
        field.get_or_insert(at);
    }

    fn transaction(&mut self, update: &SubscribeUpdateTransaction, at: Instant) {
        self.transactions += 1;
        self.latest_slot = self.latest_slot.max(update.slot);
        let Some(info) = &update.transaction else { return };
        let (Some(tx), Some(meta)) = (&info.transaction, &info.meta) else { return };
        let Some(message) = &tx.message else { return };
        let Some(fee_payer) = message.account_keys.first().and_then(|k| Pubkey::try_from(k.as_slice()).ok()) else {
            return;
        };
        if fee_payer == *PUBKEY {
            if let Ok(signature) = Signature::try_from(info.signature.as_slice()) {
                self.own.entry(signature).or_insert((update.slot, at));
            }
        }
        let logs = &meta.log_messages;
        let is_create = logs.iter().any(|l| l == "Program log: Instruction: Create" || l == "Program log: Instruction: CreateV2");
        let is_buy = logs.iter().any(|l| l == "Program log: Instruction: Buy" || l == "Program log: Instruction: BuyExactSolIn");
        if is_create {
            
            let before: HashSet<&str> = meta.pre_token_balances.iter().map(|b| b.mint.as_str()).collect();
            let mint = meta
                .post_token_balances
                .iter()
                .map(|b| b.mint.as_str())
                .find(|m| !before.contains(m) && *m != WSOL_MINT.to_string());
            if let Some(mint) = mint.and_then(|m| Pubkey::from_str(m).ok()) {
                self.launches.entry(mint).or_insert(Launch { slot: update.slot, received: at, creator: fee_payer, buys: Vec::new() });
            }
        }
        if is_buy {
            let owner = fee_payer.to_string();
            for post in meta.post_token_balances.iter().filter(|b| b.owner == owner) {
                let Ok(mint) = Pubkey::from_str(&post.mint) else { continue };
                let Some(launch) = self.launches.get_mut(&mint) else { continue };
                if launch.creator == fee_payer || update.slot > launch.slot + BUY_WINDOW_SLOTS {
                    continue;
                }
                if amount(Some(post)) > amount(meta.pre_token_balances.iter().find(|b| b.account_index == post.account_index)) {
                    launch.buys.push((update.slot, at));
                }
            }
        }
    }
}

fn amount(balance: Option<&TokenBalance>) -> u128 {
    balance.and_then(|b| b.ui_token_amount.as_ref()).and_then(|a| a.amount.parse().ok()).unwrap_or(0)
}

fn ms(from: Instant, to: Instant) -> f64 {
    to.checked_duration_since(from).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(-(from.duration_since(to).as_secs_f64() * 1000.0))
}

struct Stats {
    n: usize,
    p10: f64,
    median: f64,
    p90: f64,
}

fn stats(values: &[f64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    Some(Stats { n: v.len(), p10: at(0.1), median: at(0.5), p90: at(0.9) })
}

fn fmt_ms(v: f64) -> String {
    if v.abs() < 10.0 { format!("{v:.2} ms") } else if v.abs() < 1000.0 { format!("{v:.0} ms") } else { format!("{:.2} s", v / 1000.0) }
}

fn row(stage: &str, values: &[f64], note: &str) {
    row_with(stage, values, note, fmt_ms)
}

fn fmt_slots(v: f64) -> String {
    format!("{v:+.0} slots")
}

fn row_with(stage: &str, values: &[f64], note: &str, fmt: fn(f64) -> String) {
    match stats(values) {
        Some(s) => println!(
            "  {stage:<44} {:>10}   (p10 {}, p90 {}, n={})  {note}",
            fmt(s.median),
            fmt(s.p10),
            fmt(s.p90),
            s.n
        ),
        None => println!("  {stage:<44} {:>10}   {note}", "no data"),
    }
}

async fn connect_stream(
    recorder: Arc<Mutex<Recorder>>,
    wallet_filter: bool,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let url = env_opt("LOCAL_GRPC_URL").ok_or_else(|| anyhow!("LOCAL_GRPC_URL is not set"))?;
    let mut client = GeyserGrpcClient::build_from_shared(url)?
        .x_token(env_opt("GRPC_TOKEN"))?
        .tls_config(ClientTlsConfig::new().with_enabled_roots())?
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await
        .map_err(|e| anyhow!("cannot connect to the gRPC stream: {e}"))?;

    let mut transactions = HashMap::from([(
        "pump".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: vec![PUMP_PROGRAM_ID.to_string()],
            ..Default::default()
        },
    )]);
    if wallet_filter {
        transactions.insert(
            "own".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                account_include: vec![PUBKEY.to_string()],
                ..Default::default()
            },
        );
    }
    let request = SubscribeRequest {
        slots: HashMap::from([(
            "slots".to_string(),
            SubscribeRequestFilterSlots { filter_by_commitment: Some(false), interslot_updates: Some(true) },
        )]),
        transactions,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    let (mut sink, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .map_err(|e| anyhow!("gRPC subscription failed: {e}"))?;

    Ok(tokio::spawn(async move {
        while let Some(update) = stream.next().await {
            let at = Instant::now();
            let update = update.map_err(|status| anyhow!("gRPC stream error: {status}"))?;
            match update.update_oneof {
                Some(UpdateOneof::Slot(slot)) => {
                    let mut r = recorder.lock().unwrap();
                    r.transport(update.created_at.as_ref(), true);
                    if let Ok(status) = SlotStatus::try_from(slot.status) {
                        r.slot(slot.slot, status, at);
                    }
                }
                Some(UpdateOneof::Transaction(tx)) => {
                    let mut r = recorder.lock().unwrap();
                    r.transport(update.created_at.as_ref(), false);
                    r.transaction(&tx, at);
                }
                Some(UpdateOneof::Ping(_)) => {
                    let ping = SubscribeRequest { ping: Some(SubscribeRequestPing { id: 1 }), ..Default::default() };
                    sink.send(ping).await.map_err(|e| anyhow!("gRPC ping failed: {e}"))?;
                }
                _ => {}
            }
        }
        Ok(())
    }))
}


fn bot_reaction_ms() -> Vec<f64> {
    let Ok(text) = std::fs::read_to_string(TRADE_LOG_PATH) else { return Vec::new() };
    let mut lines = text.lines();
    let Some(header) = lines.next() else { return Vec::new() };
    let columns: Vec<&str> = header.split(',').collect();
    let (Some(action), Some(ready)) = (
        columns.iter().position(|c| *c == "action"),
        columns.iter().position(|c| *c == "detect_to_ready_ms"),
    ) else {
        return Vec::new();
    };
    lines
        .map(split_csv_line)
        .filter(|f| f.get(action).map(String::as_str) == Some("BUY"))
        .filter_map(|f| f.get(ready).and_then(|v| v.parse().ok()))
        .collect()
}

fn split_csv_line(line: &str) -> Vec<String> {
    let (mut fields, mut field, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}

async fn service_round_trip() -> (String, Vec<f64>) {
    let setting = &*TRADE_SETTING;
    let region = |regions: Vec<Region>, explicit: &Option<String>, probe: fn(&str) -> String| -> Vec<Region> {
        match explicit {
            Some(url) => vec![Region { name: "configured", base_url: url.clone(), probe_url: probe(url) }],
            None => regions,
        }
    };
    let (name, regions) = match setting.confirm_service {
        ConfirmService::Rpc => return ("RPC (no landing service)".into(), Vec::new()),
        ConfirmService::Jito => ("Jito", region(jito_regions(), &setting.jito_block_engine_url, jito_probe_url)),
        ConfirmService::Nozomi => ("Nozomi", region(nozomi_regions(), &setting.nozomi_api_url, nozomi_probe_url)),
        ConfirmService::ZeroSlot => ("0slot", region(zero_slot_regions(), &setting.zero_slot_api_url, zero_slot_probe_url)),
        ConfirmService::Helius => return ("Helius Sender (Newark)".into(), Vec::new()),
    };
    let mut best: Option<(&str, Vec<f64>)> = None;
    for region in &regions {
        let mut samples = Vec::new();
        for _ in 0..3 {
            if let Some(d) = http_latency(&region.probe_url).await {
                samples.push(d.as_secs_f64() * 1000.0);
            }
        }
        let median = stats(&samples).map(|s| s.median).unwrap_or(f64::MAX);
        if best.as_ref().and_then(|(_, b)| stats(b)).is_none_or(|b| median < b.median) && !samples.is_empty() {
            best = Some((region.name, samples));
        }
    }
    match best {
        Some((region, samples)) => (format!("{name} ({region})"), samples),
        None => (format!("{name} (unreachable)"), Vec::new()),
    }
}

async fn rpc_round_trip() -> Vec<f64> {
    let mut samples = Vec::new();
    for _ in 0..8 {
        let started = Instant::now();
        if RPC_CLIENT.get_slot_with_commitment(CommitmentConfig::processed()).await.is_ok() {
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    samples.drain(..samples.len().min(1)); 
    samples
}

struct LandResult {
    send_ms: f64,
    slots_after_send: Option<i64>,
    landed_ms: Option<f64>,
}

async fn land_tests(recorder: &Arc<Mutex<Recorder>>, count: usize) -> Result<Vec<LandResult>> {
    service::init().await?;
    tokio::spawn(recent_blockhash_handler(RPC_CLIENT.clone()));
    wait_for_blockhash(Duration::from_secs(20)).await?;
    let mut results = Vec::new();
    for i in 0..count {
        let memo = Instruction {
            program_id: MEMO_PROGRAM_ID,
            accounts: vec![],
            data: format!("latency test {} {}", i + 1, SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()).into_bytes(),
        };
        let tx = build_and_sign(vec![memo], service::tip_instructions(&PUBKEY))?;
        let signature = tx.signatures[0];
        let slot_at_send = recorder.lock().unwrap().latest_slot;
        let started = Instant::now();
        service::send(&tx).await?;
        let send_ms = started.elapsed().as_secs_f64() * 1000.0;
        let deadline = Instant::now() + LAND_TEST_TIMEOUT;
        let seen = loop {
            if let Some(seen) = recorder.lock().unwrap().own.get(&signature).copied() {
                break Some(seen);
            }
            if Instant::now() > deadline {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let result = LandResult {
            send_ms,
            slots_after_send: seen.map(|(slot, _)| slot as i64 - slot_at_send as i64),
            landed_ms: seen.map(|(_, at)| ms(started, at)),
        };
        match (&result.slots_after_send, &result.landed_ms) {
            (Some(slots), Some(landed)) => {
                println!("  test {}: landed {slots:+} slots after the send slot, {} after sending ({signature})", i + 1, fmt_ms(*landed))
            }
            _ => println!("  test {}: not seen within {LAND_TEST_TIMEOUT:?} ({signature})", i + 1),
        }
        results.push(result);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(results)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    sniper_bot::utils::logging::init_logger();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = parse_args()?;
    let setting = &*TRADE_SETTING;

    if args.land_tests > 0 {
        let tip = match setting.confirm_service {
            ConfirmService::Rpc => 0,
            ConfirmService::Jito => setting.jito_tip_lamports,
            _ => setting.third_party_fee_lamports,
        };
        println!(
            "Land test: sending {} memo transaction(s) from {} through {:?}; cost about {:.6} SOL each (tip + fees).",
            args.land_tests,
            *PUBKEY,
            setting.confirm_service,
            lamports_to_sol(tip + 5_000 + setting.priority_fee_micro_lamports * setting.compute_unit_limit as u64 / 1_000_000)
        );
    }

    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let stream = connect_stream(recorder.clone(), args.land_tests > 0).await?;
    println!("Listening to the gRPC stream for {} s (pump.fun transactions and slot updates)...", args.seconds);

    let (service_name, service_ms) = service_round_trip().await;
    let rpc_ms = rpc_round_trip().await;

    let measure_until = Instant::now() + Duration::from_secs(args.seconds);
    let land_results = if args.land_tests > 0 {
        println!("Sending land tests...");
        land_tests(&recorder, args.land_tests).await?
    } else {
        Vec::new()
    };
    while Instant::now() < measure_until {
        if stream.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    
    tokio::time::sleep(Duration::from_secs(3)).await;
    if stream.is_finished() {
        match stream.await {
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(anyhow!("stream task failed: {e}")),
            Ok(Ok(())) => bail!("the gRPC stream closed early"),
        }
    }
    stream.abort();

    let r = recorder.lock().unwrap();
    let slots: Vec<(&u64, &SlotTimes)> = r.slots.iter().collect();
    let slot_duration: Vec<f64> = slots
        .windows(2)
        .filter(|w| *w[1].0 == *w[0].0 + 1)
        .filter_map(|w| Some(ms(w[0].1.first_shred?, w[1].1.first_shred?)))
        .collect();
    let shred_to_processed: Vec<f64> = r.slots.values().filter_map(|t| Some(ms(t.first_shred?, t.processed?))).collect();
    let processed_to_confirmed: Vec<f64> = r.slots.values().filter_map(|t| Some(ms(t.processed?, t.confirmed?))).collect();
    let slot_start = |slot: u64| r.slots.get(&slot).and_then(|t| t.first_shred.or(t.created_bank));

    let launches: Vec<&Launch> = r.launches.values().filter(|l| slot_start(l.slot).is_some()).collect();
    let launch_in_slot: Vec<f64> = launches.iter().filter_map(|l| Some(ms(slot_start(l.slot)?, l.received))).collect();
    let mut first_buy_slots: BTreeMap<u64, usize> = BTreeMap::new();
    let mut first_buy_ms = Vec::new();
    let mut next_slot_buy_ms = Vec::new();
    let mut without_buys = 0;
    for launch in r.launches.values() {
        let Some(first) = launch.buys.iter().min_by_key(|(slot, at)| (*slot, *at)) else {
            without_buys += 1;
            continue;
        };
        *first_buy_slots.entry(first.0 - launch.slot).or_default() += 1;
        first_buy_ms.push(ms(launch.received, first.1));
        if let Some(next) = launch.buys.iter().filter(|(slot, _)| *slot > launch.slot).min_by_key(|(slot, at)| (*slot, *at)) {
            next_slot_buy_ms.push(ms(launch.received, next.1));
        }
    }
    let reaction = bot_reaction_ms();

    println!();
    println!("=== Latency report ({} s, {} pump.fun transactions, {} slots, {} launches) ===", args.seconds, r.transactions, r.slots.len(), r.launches.len());
    println!();
    println!("Chain");
    row("slot length (first shred to next first shred)", &slot_duration, "");
    row("slot first shred -> processed at Helius", &shred_to_processed, "");
    row("processed -> confirmed", &processed_to_confirmed, "");
    println!();
    println!("Launch to your buy, stage by stage (medians)");
    row("1. launch slot starts -> launch reaches you", &launch_in_slot, "includes block production and Helius replay");
    row("   of which Helius -> this server (transport)", &r.transport_ms, "wall-clock vs server timestamp; NTP error applies");
    row("2. bot: launch received -> buy tx ready", &reaction, "from auto_sell_logs.csv buys");
    row(&format!("3. send: HTTP round trip to {service_name}"), &service_ms, "");
    row("   RPC round trip (getSlot)", &rpc_ms, "used for confirmations, not for sending");
    if land_results.is_empty() {
        println!("  {:<44} {:>10}   run with --land-test N to time real landings (costs the tip per test)", "4. sent -> included in a block", "not run");
    } else {
        let landed: Vec<f64> = land_results.iter().filter_map(|l| l.landed_ms).collect();
        let sent: Vec<f64> = land_results.iter().map(|l| l.send_ms).collect();
        row("   send request (measured in land test)", &sent, "");
        row("4. sent -> seen landed in the stream", &landed, "");
        let slots: Vec<f64> = land_results.iter().filter_map(|l| l.slots_after_send.map(|s| s as f64)).collect();
        row_with("   landed slot vs. the slot at send time", &slots, "", fmt_slots);
        println!("     {} of {} test transactions landed", landed.len(), land_results.len());
    }
    println!();
    println!("Competition on the same launches (other wallets' buys; may include the creator's own bundle wallets)");
    let with_buys: usize = first_buy_slots.values().sum();
    for (offset, count) in &first_buy_slots {
        println!(
            "  first other buy in launch slot +{offset}: {count} launches ({:.0}%)",
            *count as f64 * 100.0 / with_buys.max(1) as f64
        );
    }
    println!("  launches with no other buy within {BUY_WINDOW_SLOTS} slots: {without_buys}");
    row("first other buy received after the launch", &first_buy_ms, "");
    row("first other buy from a later slot", &next_slot_buy_ms, "the earliest a reaction to the launch can land");
    if slot_duration.is_empty() {
        println!();
        println!("Note: the stream sent no first-shred slot updates, so slot timings are missing.");
    }
    Ok(())
}
