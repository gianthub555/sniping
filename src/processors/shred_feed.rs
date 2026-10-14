














use {
    super::common::{spawn_snipe, uri_host, SnipeRequest},
    crate::{
        config::TRADE_SETTING,
        instructions::{
            pumpfun_buy_ix::{pumpfun_config, CurveReserves, PumpFunMarket},
            pumpswap_buy_ix::PUMP_PROGRAM_ID,
            types::Market,
        },
        utils::{
            creator_history::observe_launch,
            dashboard::now_unix,
            utils::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    carbon_pumpfun_decoder::instructions::{
        buy::Buy, buy_exact_quote_in_v2::BuyExactQuoteInV2, buy_exact_sol_in::BuyExactSolIn, buy_v2::BuyV2, create::Create, create_v2::CreateV2,
    },
    decoded_shredstream::{Filter, GrpcClient, GrpcConfig, TransactionUpdate},
    log::{info, warn},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::HashMap,
        io::Write,
        sync::{LazyLock, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
};


pub type TopInstruction<'a> = (Pubkey, Vec<Option<Pubkey>>, &'a [u8]);

const LEAD_CSV: &str = "logs/shred_lead.csv";

const SIGHTING_TTL: Duration = Duration::from_secs(120);

/// Buys of the coin seen right behind its create are the dev's bundle, but a stream of
/// transactions before they run also carries ones that will fail (TIFFANY, 25 Sep: a failed
/// 37 SOL buy made the quote 4x too high). Real bundles on followed launches: median 1.6 SOL,
/// p99 8.6 SOL. Above this the launch is left to the regular stream, which shows executed trades.
pub(crate) const MAX_EARLY_BUNDLE_LAMPORTS: u64 = 15_000_000_000;


#[derive(Debug, Clone, PartialEq)]
pub struct ShredLaunch {
    pub mint: Pubkey,
    pub deployer: Pubkey,
    pub creator: Pubkey,
    pub token_program: Pubkey,
    pub mayhem: bool,
    
    pub dev_buy_lamports: u64,
    
    pub dev_buy_tokens: u64,
    
    
    
    /// The create's args end in a layout the parser does not know, so the coin's creator on the
    /// curve cannot be told before the create executes: left to the regular stream.
    pub creator_set_onchain: bool,
    
    pub metadata_host: Option<String>,
    pub name: String,
}


/// A create_v2's args after `creator` (pump.fun IDL, 30 Sep 2026): is_mayhem_mode (bool),
/// is_cashback_enabled (OptionBool), creator_fee_bps (OptionU64, newer clients) and
/// is_holder_reward (OptionBool): 1, 2, 3 (no fee bps) or 10 / 11 bytes.
const CREATE_V2_TAILS: [usize; 5] = [1, 2, 3, 10, 11];

/// The create_v2 makes a holder-reward coin (its creator fees go to the holders): the last arg,
/// is_holder_reward, is set. Such a coin's creator on the curve is pump.fun's holder-rewards
/// account for the mint (`holder_rewards_account`), not the creator the args name: on 400 followed
/// launches of 25-30 Sep all 157 holder-reward coins (3- and 11-byte tails) traded with it and 229
/// of the 234 others with the named creator. Until 30 Sep these went to the regular stream, 33 of
/// the 38 buys there that evening.
fn holder_reward(data: &[u8]) -> bool {
    create_v2_tail(data).is_some_and(|t| matches!(t.len(), 3 | 11) && t[t.len() - 1] == 1)
}

/// The create_v2's args end in a layout the parser does not know (a newer IDL).
fn unknown_create_v2_tail(data: &[u8]) -> bool {
    create_v2_tail(data).is_some_and(|t| !CREATE_V2_TAILS.contains(&t.len()))
}

/// A holder-reward coin's creator on the curve (seeds "holder-rewards", mint; the pump.fun program).
pub fn holder_rewards_account(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"holder-rewards", mint.as_ref()], &PUMP_PROGRAM_ID).0
}

fn create_v2_tail(data: &[u8]) -> Option<&[u8]> {
    let mut o = 8usize;
    for _ in 0..3 {
        let len = u32::from_le_bytes(data.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4 + len;
    }
    data.get(o + 32..)
}

struct Sighting {
    at: Instant,
    unix_ms: u128,
    slot: u64,
}

#[derive(Default)]
struct Stats {
    seen: u64,
    matched: u64,
    regular_only: u64,
    
    shred_late: u64,
    
    creator_unknown: u64,
    /// Launches whose dev buy goes through another program: left to the regular stream.
    dev_buy_hidden: u64,
    /// Launches whose bundle looked too big to be real: left to the regular stream.
    bundle_unbelievable: u64,
    /// Proxy batches that did not parse.
    bad_batches: u64,
    leads_ms: Vec<f64>,
}

static SIGHTINGS: LazyLock<Mutex<HashMap<Pubkey, Sighting>>> = LazyLock::new(Default::default);

static REGULAR_FIRST: LazyLock<Mutex<HashMap<Pubkey, (Instant, u64)>>> = LazyLock::new(Default::default);
static STATS: LazyLock<Mutex<Stats>> = LazyLock::new(Default::default);

fn unix_ms(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}


pub fn enabled() -> bool {
    TRADE_SETTING.shred_proxy_url.is_some()
        || TRADE_SETTING.shred_udp_port.is_some()
        || TRADE_SETTING.shred_grpc_url.is_some()
        || TRADE_SETTING.preprocessed_url.is_some()
}


pub fn spawn() {
    if enabled() {
        super::lookup_tables::warm();
    }
    // FAST_PATH: the hot path's own threads read the early feeds and forward every frame here.
    if enabled() && crate::hot::engine::start() {
        tokio::spawn(report_loop());
        return;
    }
    if let Some(url) = TRADE_SETTING.preprocessed_url.clone() {
        super::preprocessed::spawn(url);
        // pumpfeed pushes each block piece's pump.fun transactions here (its ENTRY_PUSH): a second
        // early feed next to the preprocessed stream, merged in sighting() (first sighting wins).
        if let Some(port) = TRADE_SETTING.shred_udp_port {
            tokio::spawn(async move {
                loop {
                    if let Err(e) = run_udp(port).await {
                        warn!("shred udp: {e:#}; rebinding in 2 s");
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });
        }
        tokio::spawn(report_loop());
        return;
    }
    if let Some(url) = TRADE_SETTING.shred_proxy_url.clone() {
        tokio::spawn(async move {
            loop {
                if let Err(e) = run_proxy(&url).await {
                    warn!("shred proxy: {e:#}; reconnecting in 2 s");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        tokio::spawn(report_loop());
        return;
    }
    if let Some(port) = TRADE_SETTING.shred_udp_port {
        tokio::spawn(async move {
            loop {
                if let Err(e) = run_udp(port).await {
                    warn!("shred udp: {e:#}; rebinding in 2 s");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        tokio::spawn(report_loop());
        return;
    }
    let (Some(url), Some(token)) = (TRADE_SETTING.shred_grpc_url.clone(), TRADE_SETTING.shred_token.clone()) else {
        if TRADE_SETTING.shred_grpc_url.is_some() {
            warn!("SHRED_GRPC_URL is set but SHRED_TOKEN is not; the shred stream stays off");
        }
        return;
    };
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&url, &token).await {
                warn!("shred stream: {e}; reconnecting in 2 s");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    tokio::spawn(report_loop());
}

async fn run(url: &str, token: &str) -> anyhow::Result<()> {
    let filter = Filter::new().include([PUMP_PROGRAM_ID.to_string()]).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut client = GrpcClient::connect(GrpcConfig::new(url, token).filter("pump", filter))
        .await
        .map_err(|e| anyhow::anyhow!("connect: {e}"))?;
    info!("shred stream connected (pump.fun transactions before execution)");
    while let Some(update) = client.next_update().await {
        let update = update.map_err(|e| anyhow::anyhow!("{e}"))?;
        handle(&update);
    }
    anyhow::bail!("stream ended")
}


/// Jito ShredStream proxy entries service (proto `shredstream.ShredstreamProxy`).
#[derive(Clone, PartialEq, prost::Message)]
struct SubscribeEntriesRequest {}

#[derive(Clone, PartialEq, prost::Message)]
struct EntryBatch {
    #[prost(uint64, tag = "1")]
    slot: u64,
    /// bincode `Vec<Entry>`.
    #[prost(bytes = "vec", tag = "2")]
    entries: Vec<u8>,
}

async fn run_proxy(url: &str) -> anyhow::Result<()> {
    let channel = tonic::transport::Endpoint::from_shared(url.to_string())?.tcp_nodelay(true).connect().await?;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.map_err(|e| anyhow::anyhow!("not ready: {e}"))?;
    let path = tonic::codegen::http::uri::PathAndQuery::from_static("/shredstream.ShredstreamProxy/SubscribeEntries");
    let codec = tonic_prost::ProstCodec::<SubscribeEntriesRequest, EntryBatch>::default();
    let mut stream = grpc.server_streaming(tonic::Request::new(SubscribeEntriesRequest {}), path, codec).await?.into_inner();
    info!("shred stream connected (Jito ShredStream proxy at {url}): pump.fun transactions before execution");
    while let Some(batch) = stream.message().await? {
        handle_batch(batch.slot, &batch.entries);
    }
    anyhow::bail!("stream ended")
}

/// Deshredded entries pushed over UDP: one datagram per batch, the slot (u64 LE) then a bincode
/// `Vec<Entry>` (AllenHark Direct UDP). Datagrams run to 65 KB, so the socket buffer is large.
async fn run_udp(port: u16) -> anyhow::Result<()> {
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", port)).await?;
    info!("shred stream listening on UDP port {port}: deshredded entries pushed to us");
    let mut buf = vec![0u8; 65_535];
    loop {
        let (n, _) = socket.recv_from(&mut buf).await?;
        match udp_batch(&buf[..n]) {
            Some((slot, payload)) => {
                // A buy task spawned from here waits in this worker's LIFO slot until this loop
                // yields; with datagrams queued, that is after the next ones. Yield now instead.
                if handle_batch(slot, payload) {
                    tokio::task::yield_now().await;
                }
            }
            None => {
                STATS.lock().unwrap_or_else(|e| e.into_inner()).bad_batches += 1;
            }
        }
    }
}

/// Splits a UDP batch into its slot and the entries payload.
pub fn udp_batch(datagram: &[u8]) -> Option<(u64, &[u8])> {
    let slot = u64::from_le_bytes(datagram.get(..8)?.try_into().ok()?);
    Some((slot, &datagram[8..]))
}

/// One deshredded batch: its launches, each quoted after the buys of the coin later in the same
/// batch (the create's bundle and the fastest snipers land before any buy of ours). True when
/// the batch held a launch.
pub(crate) fn handle_batch(slot: u64, payload: &[u8]) -> bool {
    let received = Instant::now();
    let received_at = SystemTime::now();
    let Some(txs) = super::shred_wire::entry_transactions(payload) else {
        STATS.lock().unwrap_or_else(|e| e.into_inner()).bad_batches += 1;
        return false;
    };
    let pump: Vec<_> = txs.iter().filter(|t| t.mentions(&PUMP_PROGRAM_ID)).map(|t| (t, t.top_instructions())).collect();
    // A creator's move to fee sharing in the batch, before any of its launches is bought. Pump
    // fees' create_fee_sharing_config names pump.fun only through a lookup table, so those
    // transactions are read here too.
    use crate::instructions::pumpswap_buy_ix::PUMP_FEE_PROGRAM_ID;
    for (_, instructions) in &pump {
        for mint in creator_migrations_in(instructions) {
            super::fee_sharing::note(mint);
        }
    }
    for t in txs.iter().filter(|t| t.mentions(&PUMP_FEE_PROGRAM_ID) && !t.mentions(&PUMP_PROGRAM_ID)) {
        for mint in creator_migrations_in(&t.top_instructions()) {
            super::fee_sharing::note(mint);
        }
    }
    let mut launched = false;
    for (i, (tx, instructions)) in pump.iter().enumerate() {
        for launch in parse_launches(instructions) {
            let later: Vec<(u64, u64, Option<Pubkey>)> = pump[i + 1..].iter().flat_map(|(_, ixs)| pump_buys_by(ixs, &launch.mint)).collect();
            let amounts: Vec<(u64, u64)> = later.iter().map(|(t, l, _)| (*t, *l)).collect();
            let buyers: Vec<Option<Pubkey>> = later.iter().map(|(_, _, b)| *b).collect();
            sighting(&launch, Signature::from(tx.signature), slot, received_at, received, &amounts, Some(&buyers), tx.keys.first().copied());
            launched = true;
        }
    }
    launched
}

pub fn launches_in_tx(bytes: &[u8]) -> Vec<ShredLaunch> {
    super::shred_wire::decode_transaction(bytes).map(|tx| parse_launches(&tx.top_instructions())).unwrap_or_default()
}

fn handle(update: &TransactionUpdate) {
    let received = Instant::now();
    let payer = super::shred_wire::decode_transaction(update.bytes()).and_then(|tx| tx.keys.first().copied());
    let signature = Signature::from(*update.signature().as_bytes());
    for launch in launches_in_tx(update.bytes()) {
        sighting(&launch, signature, update.slot(), update.received_at(), received, &[], None, payer);
    }
}

/// A launch seen in shreds: the head-start bookkeeping against the regular stream, then the buy.
pub(crate) fn sighting(
    launch: &ShredLaunch,
    signature: Signature,
    slot: u64,
    received_at: SystemTime,
    received: Instant,
    later_buys: &[(u64, u64)],
    bundle_buyers: Option<&[Option<Pubkey>]>,
    fee_payer: Option<Pubkey>,
) {
    {
        let first = {
            let mut sightings = SIGHTINGS.lock().unwrap_or_else(|e| e.into_inner());
            sightings.retain(|_, s| s.at.elapsed() < SIGHTING_TTL);
            match sightings.entry(launch.mint) {
                std::collections::hash_map::Entry::Occupied(_) => false,
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(Sighting { at: received, unix_ms: unix_ms(received_at), slot });
                    true
                }
            }
        };
        if !first {
            return;
        }
        
        if let Some((at, regular_slot)) = REGULAR_FIRST.lock().unwrap_or_else(|e| e.into_inner()).remove(&launch.mint) {
            let lag = at.elapsed().as_secs_f64() * 1000.0;
            SIGHTINGS.lock().unwrap_or_else(|e| e.into_inner()).remove(&launch.mint);
            {
                let mut stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
                stats.regular_only = stats.regular_only.saturating_sub(1);
                stats.shred_late += 1;
                stats.leads_ms.push(-lag);
            }
            write_row(&format!(
                "{},{},{},{},{regular_slot},{:.1},regular-first\n",
                unix_ms(SystemTime::now()),
                launch.mint,
                unix_ms(received_at),
                slot,
                -lag
            ));
            return;
        }
        STATS.lock().unwrap_or_else(|e| e.into_inner()).seen += 1;
        if TRADE_SETTING.shred_snipe {
            snipe(launch, signature, slot, received, later_buys, bundle_buyers, fee_payer);
        }
    }
}




pub fn parse_launches(instructions: &[TopInstruction]) -> Vec<ShredLaunch> {
    let mut launches = Vec::new();
    for (program, accounts, data) in instructions {
        if *program != PUMP_PROGRAM_ID {
            continue;
        }
        let at = |i: usize| accounts.get(i).copied().flatten();
        let launch = if let Some(create) = CreateV2::decode(data) {
            
            if accounts.len() > 16 && at(16) != Some(WSOL_MINT) {
                if at(16).is_none() {
                    // The quote mint comes from a lookup table not cached yet: not a non-SOL coin.
                    info!("early skip {:?}: quote mint in an unresolved lookup table", at(0));
                }
                continue;
            }
            at(0).zip(at(5)).map(|(mint, deployer)| ShredLaunch {
                mint,
                deployer,
                creator: if holder_reward(data) { holder_rewards_account(&mint) } else { create.creator },
                token_program: at(7).unwrap_or(TOKEN_2022_PROGRAM_ID),
                mayhem: create.is_mayhem_mode,
                dev_buy_lamports: 0,
                dev_buy_tokens: 0,
                creator_set_onchain: unknown_create_v2_tail(data),
                metadata_host: uri_host(&create.uri),
                name: create.name.clone(),
            })
        } else if let Some(create) = Create::decode(data) {
            at(0).zip(at(7)).map(|(mint, deployer)| ShredLaunch {
                mint,
                deployer,
                creator: create.creator,
                token_program: TOKEN_PROGRAM_ID,
                mayhem: false,
                dev_buy_lamports: 0,
                dev_buy_tokens: 0,
                creator_set_onchain: false,
                metadata_host: uri_host(&create.uri),
                name: create.name.clone(),
            })
        } else {
            None
        };
        if let Some(launch) = launch {
            launches.push(launch);
        }
    }
    
    for launch in &mut launches {
        for (tokens, lamports) in pump_buys(instructions, &launch.mint) {
            launch.dev_buy_tokens += tokens;
            launch.dev_buy_lamports += lamports;
        }
    }
    launches
}

/// Top-level pump.fun buys of `mint`: (exact tokens or 0, SOL in or the SOL cap).
pub fn pump_buys(instructions: &[TopInstruction], mint: &Pubkey) -> Vec<(u64, u64)> {
    pump_buys_by(instructions, mint).into_iter().map(|(tokens, lamports, _)| (tokens, lamports)).collect()
}

/// `pump_buys` with each buy's buyer (the `user` account; None when the account comes from a
/// lookup table the decoder could not resolve).
pub fn pump_buys_by(instructions: &[TopInstruction], mint: &Pubkey) -> Vec<(u64, u64, Option<Pubkey>)> {
    let mut buys = Vec::new();
    for (program, accounts, data) in instructions {
        if *program != PUMP_PROGRAM_ID {
            continue;
        }
        let at = |i: usize| accounts.get(i).copied().flatten();
        if let Some(buy) = BuyExactSolIn::decode(data) {
            if at(2) == Some(*mint) {
                buys.push((0, buy.spendable_sol_in, at(6)));
            }
        } else if let Some(buy) = Buy::decode(data) {
            if at(2) == Some(*mint) {
                buys.push((buy.amount, buy.max_sol_cost, at(6)));
            }
        } else if let Some(buy) = BuyExactQuoteInV2::decode(data) {
            if at(1) == Some(*mint) {
                buys.push((0, buy.spendable_quote_in, at(13)));
            }
        } else if let Some(buy) = BuyV2::decode(data) {
            if at(1) == Some(*mint) {
                buys.push((buy.amount, buy.max_sol_cost, at(13)));
            }
        } else if let Some((tokens, lamports)) = buy_v3(data) {
            // The v3 buys (`hot::decode::BUY_V3`): the coin at account 1, the user at 8.
            if at(1) == Some(*mint) {
                buys.push((tokens, lamports, at(8)));
            }
        }
    }
    buys
}

/// A pump.fun buy_v3 (tokens exact, the SOL cap) or buy_exact_quote_in_v3 (0, the SOL in): the
/// two u64 after the discriminator, as `hot::decode::buy_of` reads them.
fn buy_v3(data: &[u8]) -> Option<(u64, u64)> {
    use crate::hot::decode::{BUY_EXACT_QUOTE_IN_V3, BUY_V3};
    let disc: [u8; 8] = data.get(..8)?.try_into().ok()?;
    let a = u64::from_le_bytes(data.get(8..16)?.try_into().ok()?);
    let b = u64::from_le_bytes(data.get(16..24)?.try_into().ok()?);
    match disc {
        BUY_V3 => Some((a, b)),
        BUY_EXACT_QUOTE_IN_V3 => Some((0, a)),
        _ => None,
    }
}

/// Every coin whose creator these instructions move to a fee-sharing account, as
/// `hot::decode::creator_migrations` reads them (pump fees' create_fee_sharing_config: the coin at
/// account 4; pump.fun's migrate_bonding_curve_creator: at 0).
pub fn creator_migrations_in(instructions: &[TopInstruction]) -> Vec<Pubkey> {
    use crate::{
        hot::decode::{CREATE_FEE_SHARING_CONFIG, MIGRATE_BONDING_CURVE_CREATOR},
        instructions::pumpswap_buy_ix::PUMP_FEE_PROGRAM_ID,
    };
    instructions
        .iter()
        .filter_map(|(program, accounts, data)| {
            let disc = data.get(..8)?;
            let at = if *program == PUMP_FEE_PROGRAM_ID && disc == CREATE_FEE_SHARING_CONFIG {
                4
            } else if *program == PUMP_PROGRAM_ID && disc == MIGRATE_BONDING_CURVE_CREATOR {
                0
            } else {
                return None;
            };
            accounts.get(at).copied().flatten()
        })
        .collect()
}

/// Wallets of a create's bundle: the distinct buyers of its bundle buys, without the deployer and
/// the coin's creator (their own buys are the dev buy). None when a buyer could not be read.
pub fn bundle_wallets(launch: &ShredLaunch, buyers: &[Option<Pubkey>]) -> Option<u32> {
    let mut wallets = std::collections::HashSet::new();
    for buyer in buyers {
        let buyer = (*buyer)?;
        if buyer != launch.deployer && buyer != launch.creator {
            wallets.insert(buyer);
        }
    }
    Some(wallets.len() as u32)
}


pub fn curve_after(launch: &ShredLaunch, initial: CurveReserves, fee_bps: u64) -> CurveReserves {
    apply_buy(initial, launch.dev_buy_tokens, launch.dev_buy_lamports, fee_bps)
}

/// The curve after one buy: exact `tokens` when known, else `lamports` of SOL in (fee included).
pub fn apply_buy(mut r: CurveReserves, tokens: u64, lamports: u64, fee_bps: u64) -> CurveReserves {
    let (tokens, sol) = if tokens > 0 {
        let t = tokens.min(r.real_token_reserves);
        let sol = (r.virtual_sol_reserves as u128 * t as u128 / (r.virtual_token_reserves - t).max(1) as u128) as u64 + 1;
        (t, sol)
    } else if lamports > 0 {
        let net = (lamports as u128 * 10_000 / (10_000 + fee_bps as u128)) as u64;
        let t = (net as u128 * r.virtual_token_reserves as u128 / (r.virtual_sol_reserves as u128 + net as u128)) as u64;
        (t.min(r.real_token_reserves), net)
    } else {
        (0, 0)
    };
    r.virtual_token_reserves -= tokens;
    r.real_token_reserves -= tokens;
    r.virtual_sol_reserves += sol;
    r.real_sol_reserves += sol;
    r
}

fn snipe(
    launch: &ShredLaunch,
    signature: Signature,
    slot: u64,
    received: Instant,
    later_buys: &[(u64, u64)],
    bundle_buyers: Option<&[Option<Pubkey>]>,
    fee_payer: Option<Pubkey>,
) {
    // Each early exit is logged with the coin: a launch not bought here is not bought at all
    // (the regular stream no longer buys, `common::BUY_FROM_REGULAR_STREAM`).
    let mint = launch.mint;
    // bundle10's shadow log (BUNDLE10_SHADOW): this path's count, before any skip below.
    if crate::config::bundle10::shadow() {
        super::bundle10_shadow::note_slow_count(&mint, bundle_buyers.and_then(|b| bundle_wallets(launch, b)));
    }
    if launch.creator_set_onchain {
        STATS.lock().unwrap_or_else(|e| e.into_inner()).creator_unknown += 1;
        info!("early skip {mint}: create args in an unknown layout");
        return;
    }
    // A dev buy through another program is invisible at the top level; the regular stream sees it.
    if launch.dev_buy_lamports == 0 {
        STATS.lock().unwrap_or_else(|e| e.into_inner()).dev_buy_hidden += 1;
        info!("early skip {mint}: no top-level dev buy (tokens {})", launch.dev_buy_tokens);
        return;
    }
    let Ok(config) = pumpfun_config() else {
        info!("early skip {mint}: pump.fun config not loaded");
        return;
    };
    let fee_bps = config.fee_tiers.first().map(|(_, f)| f.protocol_bps + f.creator_bps).unwrap_or(125);
    let after_dev = curve_after(launch, config.initial_reserves, fee_bps);
    // What the dev's buy puts into the curve (a Buy names the tokens and only a maximum cost),
    // the same measure the regular stream takes from the trade event.
    let dev_buy_lamports = after_dev.real_sol_reserves;
    let reserves = later_buys.iter().fold(after_dev, |r, (tokens, lamports)| apply_buy(r, *tokens, *lamports, fee_bps));
    if reserves.real_sol_reserves.saturating_sub(after_dev.real_sol_reserves) > MAX_EARLY_BUNDLE_LAMPORTS {
        STATS.lock().unwrap_or_else(|e| e.into_inner()).bundle_unbelievable += 1;
        info!(
            "early skip {mint}: bundle of {} buys puts {:.2} SOL in after a {:.2} SOL dev buy (limit {:.0})",
            later_buys.len(),
            reserves.real_sol_reserves.saturating_sub(after_dev.real_sol_reserves) as f64 / 1e9,
            dev_buy_lamports as f64 / 1e9,
            MAX_EARLY_BUNDLE_LAMPORTS as f64 / 1e9
        );
        return;
    }
    let market = PumpFunMarket::new(launch.mint, launch.token_program, launch.creator, launch.mayhem, reserves);
    let prior = observe_launch(&launch.deployer, &launch.mint, reserves.real_sol_reserves, now_unix());
    // The fee payer's balance before the create: from the followed wallets' live balances, or
    // (not followed, or the cache is off) from RPC before the buy.
    let spend = dev_buy_lamports + super::common::CREATE_COST_LAMPORTS;
    let cached = fee_payer.and_then(|payer| super::deployer_balances::balance_before(&payer, slot, spend));
    spawn_snipe(SnipeRequest {
        market: Market::PumpFun(market),
        signature,
        slot,
        detected_at: received,
        needs_refresh: false,
        prior_launches: Some(prior),
        deployer: Some(launch.deployer),
        mayhem: launch.mayhem,
        metadata_host: launch.metadata_host.clone(),
        dev_buy_lamports,
        // Shreds carry no balances.
        deployer_lamports: cached,
        balance_from_rpc: if cached.is_some() { None } else { fee_payer },
        fast_rug: crate::config::rules::fast_rug_profile(launch.metadata_host.as_deref(), dev_buy_lamports),
        pre_execution: true,
        bundle_wallets: bundle_buyers.and_then(|b| bundle_wallets(launch, b)),
        name_all_caps: Some(crate::config::rules::name_is_all_caps(&launch.name)),
        facts: None,
    });
}



pub fn regular_saw(mint: &Pubkey, regular_slot: u64) {
    if !enabled() {
        return;
    }
    {
        let mut seen = REGULAR_SEEN.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, at| at.elapsed() < SIGHTING_TTL);
        seen.insert(*mint, Instant::now());
    }
    let now_ms = unix_ms(SystemTime::now());
    let sighting = SIGHTINGS.lock().unwrap_or_else(|e| e.into_inner()).remove(mint);
    let mut stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
    let row = match sighting {
        Some(s) => {
            let lead = s.at.elapsed().as_secs_f64() * 1000.0;
            stats.matched += 1;
            stats.leads_ms.push(lead);
            format!("{now_ms},{mint},{},{},{regular_slot},{lead:.1},shred\n", s.unix_ms, s.slot)
        }
        None => {
            stats.regular_only += 1;
            let mut first = REGULAR_FIRST.lock().unwrap_or_else(|e| e.into_inner());
            first.retain(|_, (at, _)| at.elapsed() < SIGHTING_TTL);
            first.insert(*mint, (Instant::now(), regular_slot));
            return;
        }
    };
    drop(stats);
    // ASYNC_FILE_WRITES: on the writer thread, not in the regular stream's task.
    crate::utils::file_writer::run(move || write_row(&row));
}

/// Launches the regular (executed) stream has shown.
static REGULAR_SEEN: LazyLock<Mutex<HashMap<Pubkey, Instant>>> = LazyLock::new(Default::default);

/// Waits until the regular stream shows the launch, at most `timeout`.
pub async fn wait_regular(mint: &Pubkey, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if REGULAR_SEEN.lock().unwrap_or_else(|e| e.into_inner()).contains_key(mint) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn write_row(row: &str) {
    let new = !std::path::Path::new(LEAD_CSV).exists();
    if let Some(mut f) = crate::store::tee::append(LEAD_CSV, "shred_lead", "regular_seen_unix_ms,mint,shred_seen_unix_ms,shred_slot,regular_slot,lead_ms,first_seen_by") {
        if new {
            let _ = f.write_all(b"regular_seen_unix_ms,mint,shred_seen_unix_ms,shred_slot,regular_slot,lead_ms,first_seen_by\n");
        }
        let _ = f.write_all(row.as_bytes());
    }
}

async fn report_loop() {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        let mut s = STATS.lock().unwrap_or_else(|e| e.into_inner());
        let mut leads = std::mem::take(&mut s.leads_ms);
        leads.sort_by(|a, b| a.total_cmp(b));
        let pick = |q: f64| leads.get(((leads.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0.0);
        info!(
            "shred stream, last 5 min: {} frames ({} unreadable), {} followed-wallet balances cached; {} launches seen from shreds, {} earlier than the regular stream, {} later (head start median {:.0} ms, p10 {:.0}, p90 {:.0}), {} only on the regular stream, {} not bought from shreds (create args in an unknown layout), {} left to the regular stream (dev buy through another program), {} left to it (bundle too big to be real), {} unreadable batches",
            super::preprocessed::FRAMES.swap(0, std::sync::atomic::Ordering::Relaxed),
            super::preprocessed::BAD_FRAMES.swap(0, std::sync::atomic::Ordering::Relaxed),
            super::deployer_balances::known(),
            s.seen,
            s.matched,
            s.shred_late,
            pick(0.5),
            pick(0.1),
            pick(0.9),
            s.regular_only,
            s.creator_unknown,
            s.dev_buy_hidden,
            s.bundle_unbelievable,
            s.bad_batches
        );
        *s = Stats::default();
        drop(s);
        let mut gaps = std::mem::take(&mut *super::preprocessed::BUNDLE_GAPS_MS.lock().unwrap_or_else(|e| e.into_inner()));
        let late = std::mem::take(&mut *super::preprocessed::LATE_BUYS_MS.lock().unwrap_or_else(|e| e.into_inner()));
        if !gaps.is_empty() || !late.is_empty() {
            gaps.sort_by(|a, b| a.total_cmp(b));
            let q = |p: f64| gaps.get(((gaps.len().max(1) as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0);
            info!(
                "preprocessed stream, last 5 min: {} bundle buys reached it after their create: median {:.2} ms, p90 {:.2}, max {:.2} (window {:.1} ms); {} buys of the same slot came after the window, within 5 ms (quote missed them{})",
                gaps.len(),
                q(0.5),
                q(0.9),
                gaps.last().copied().unwrap_or(0.0),
                super::preprocessed::BUNDLE_WINDOW.as_secs_f64() * 1000.0,
                late.len(),
                if late.is_empty() { String::new() } else { format!(": {:.2}-{:.2} ms after the create", late.iter().copied().fold(f64::MAX, f64::min), late.iter().copied().fold(0.0, f64::max)) }
            );
        }
        let creates = super::preprocessed::CREATES_SEEN.swap(0, std::sync::atomic::Ordering::Relaxed);
        let mut pre = std::mem::take(&mut *super::preprocessed::PRE_CREATE_LEADS_MS.lock().unwrap_or_else(|e| e.into_inner()));
        pre.sort_by(|a, b| a.total_cmp(b));
        let q = |p: f64| pre.get(((pre.len().max(1) as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0);
        info!(
            "preprocessed stream, last 5 min: {creates} creates, {} of them named first by another transaction (lead median {:.1} ms, p10 {:.1}, p90 {:.1})",
            pre.len(),
            q(0.5),
            q(0.1),
            q(0.9)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_v2_data(creator: &Pubkey, tail: &[u8]) -> Vec<u8> {
        let mut d = vec![214, 144, 76, 236, 95, 139, 49, 180];
        for s in ["inuflation", "INUFLATION", "https://metadata.j7tracker.io/metadata/K6STEcYNCE.json"] {
            d.extend((s.len() as u32).to_le_bytes());
            d.extend(s.as_bytes());
        }
        d.extend(creator.to_bytes());
        d.extend(tail);
        d
    }

    #[test]
    fn a_holder_reward_coin_names_the_holder_rewards_account_as_its_creator() {
        let mint = Pubkey::from_str_const("BFAX2TCzfLGfjSCSLPQtWaRVdp4tPvMPXqB39UpUncMJ");
        let dev = Pubkey::from_str_const("8YcbyX92UHTU23HZv3ccP9o13qibErQkKaUjoxqxd7SJ");
        let mut accounts = vec![Some(Pubkey::new_unique()); 16];
        accounts[0] = Some(mint);
        accounts[5] = Some(dev);
        let parse = |tail: &[u8]| {
            let data = create_v2_data(&dev, tail);
            parse_launches(&[(PUMP_PROGRAM_ID, accounts.clone(), &data[..])]).pop().expect("a launch")
        };
        // BFAX (30 Sep): is_holder_reward set; its trades on chain named 75rnzs... as the creator.
        let mut tail = [0u8; 11];
        tail[10] = 1;
        let launch = parse(&tail);
        assert_eq!(launch.creator, Pubkey::from_str_const("75rnzsFyFgnDHA62QqRzerkrFkNDAPxCGnb9FRvtxpVt"));
        assert!(!launch.creator_set_onchain);
        assert_eq!(parse(&[0, 0, 1]).creator, launch.creator, "the layout without creator_fee_bps");
        assert_eq!(parse(&[0u8; 11]).creator, dev, "not a holder-reward coin");
        assert_eq!(parse(&[0u8; 10]).creator, dev);
        assert_eq!(parse(&[0, 0]).creator, dev);
        assert!(parse(&[0u8; 12]).creator_set_onchain, "an unknown layout is left to the regular stream");
    }

    #[test]
    fn dev_buy_moves_the_curve_like_a_buy() {
        let initial = CurveReserves {
            virtual_token_reserves: 1_073_000_000_000_000,
            virtual_sol_reserves: 30_000_000_000,
            real_token_reserves: 793_100_000_000_000,
            real_sol_reserves: 0,
        };
        let launch = ShredLaunch {
            mint: Pubkey::new_unique(),
            deployer: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            token_program: TOKEN_2022_PROGRAM_ID,
            mayhem: false,
            dev_buy_lamports: 1_012_500_000,
            dev_buy_tokens: 0,
            creator_set_onchain: false,
            metadata_host: None,
            name: String::new(),
        };
        let r = curve_after(&launch, initial, 125);
        assert_eq!(r.real_sol_reserves, 1_000_000_000);
        
        let expected_vt = (30_000_000_000u128 * 1_073_000_000_000_000 / 31_000_000_000) as u64;
        assert!((r.virtual_token_reserves as i128 - expected_vt as i128).abs() < 1_000_000);
        assert_eq!(curve_after(&ShredLaunch { dev_buy_lamports: 0, ..launch.clone() }, initial, 125), initial);
    }

    #[test]
    fn bundle_wallets_count_distinct_buyers_without_the_dev() {
        let launch = ShredLaunch {
            mint: Pubkey::new_unique(),
            deployer: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            token_program: TOKEN_2022_PROGRAM_ID,
            mayhem: false,
            dev_buy_lamports: 1_000_000_000,
            dev_buy_tokens: 0,
            creator_set_onchain: false,
            metadata_host: None,
            name: String::new(),
        };
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        assert_eq!(bundle_wallets(&launch, &[]), Some(0));
        assert_eq!(bundle_wallets(&launch, &[Some(a), Some(a), Some(b)]), Some(2), "a wallet buying twice counts once");
        assert_eq!(bundle_wallets(&launch, &[Some(launch.deployer), Some(launch.creator), Some(a)]), Some(1), "the dev's own buys are not bundle wallets");
        assert_eq!(bundle_wallets(&launch, &[Some(a), None]), None, "an unreadable buyer leaves the count unknown");
    }
}
