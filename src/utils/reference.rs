use {
    crate::config::rules::{self, Follow},
    log::{info, warn},
    serde_json::{json, Value},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        io::Write,
        str::FromStr,
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex,
        },
    },
};

/// Unix time of the history file's latest line when it was loaded.
static LAST_RECORDED: AtomicU64 = AtomicU64::new(0);
/// ...and of its earliest: how far back the history the follow rule reads goes.
static FIRST_RECORDED: AtomicU64 = AtomicU64::new(u64::MAX);

/// A history this short at a start is a damaged file, not a quiet wallet (it once covered
/// 8 Sep on): on 13 Oct a file cut to one day made a restart follow ~60% fewer launches.
pub const MIN_HISTORY_DAYS: f64 = 7.0;

/// Days the loaded history covers up to `now` (0 when it is empty).
pub fn history_days(now: u64) -> f64 {
    let first = FIRST_RECORDED.load(Ordering::Relaxed);
    if first == u64::MAX { 0.0 } else { now.saturating_sub(first) as f64 / 86_400.0 }
}

/// The start check: Err when the history covers less than `MIN_HISTORY_DAYS`, so a live start
/// can refuse it (`FOLLOW_HISTORY_SHORT_OK=true` accepts it on purpose).
pub fn check_history(now: u64, keys: usize) -> Result<(), String> {
    let days = history_days(now);
    if days >= MIN_HISTORY_DAYS {
        return Ok(());
    }
    Err(format!(
        "the reference wallet's history ({}) covers only {days:.1} days ({keys} deployers and creators): the bot follows only what he traded in that span. Restore the full file before trading",
        rules::REFERENCE_TRADES_FILE
    ))
}

/// The reference wallet's history with one deployer or coin creator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Record {
    /// Unix time of his latest trade on one of its coins.
    pub last: u64,
    /// His SOL result over all its coins, in lamports.
    pub profit: i64,
    /// His trades on its coins.
    pub trades: u32,
}

impl Record {
    pub fn add(&mut self, at: u64, lamports: i64) {
        self.last = self.last.max(at);
        self.profit += lamports;
        self.trades += 1;
    }
}

/// Whether a key's history makes the bot follow it at `now`.
pub fn follow_of(record: &Record, now: u64) -> Option<Follow> {
    if now.saturating_sub(record.last) < rules::FOLLOW_RECENT_SECS {
        Some(Follow::Recent)
    } else if record.profit >= rules::lamports(rules::FOLLOW_MIN_PROFIT_SOL) as i64 {
        Some(Follow::Profitable)
    } else {
        None
    }
}

static RECORDS: LazyLock<Mutex<HashMap<Pubkey, Record>>> = LazyLock::new(Default::default);

/// Signatures of his transactions already noted: the pump.fun processor runs once per decoded
/// instruction, so one transaction can reach `note_trade` several times.
static NOTED: LazyLock<Mutex<std::collections::VecDeque<solana_signature::Signature>>> = LazyLock::new(Default::default);

/// True the first time a transaction signature is seen (keeps the last 256).
pub fn first_time(signature: &solana_signature::Signature) -> bool {
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    if noted.contains(signature) {
        return false;
    }
    if noted.len() >= 256 {
        noted.pop_front();
    }
    noted.push_back(*signature);
    true
}

/// One history line: `unix key lamports_delta`.
pub fn parse_line(line: &str) -> Option<(u64, Pubkey, i64)> {
    let mut parts = line.split_whitespace();
    let at = parts.next()?.parse().ok()?;
    let key = Pubkey::from_str(parts.next()?).ok()?;
    let lamports = parts.next()?.parse().ok()?;
    Some((at, key, lamports))
}

/// Reads the reference wallet's trade history. Returns the number of deployers and creators.
pub fn load() -> usize {
    // The store (STORE_FILES=off): every line (a key's profit sums its whole history).
    if crate::store::read_from_db() {
        let started = std::time::Instant::now();
        let rows = crate::store::read::reference_trades();
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        for (at, key, lamports) in rows {
            let Ok(key) = Pubkey::from_str(&key) else { continue };
            records.entry(key).or_default().add(at, lamports);
            LAST_RECORDED.fetch_max(at, Ordering::Relaxed);
            FIRST_RECORDED.fetch_min(at, Ordering::Relaxed);
        }
        info!("reference trades: {} keys from the store in {:.1} ms", records.len(), started.elapsed().as_secs_f64() * 1e3);
        return records.len();
    }
    let text = match std::fs::read_to_string(rules::REFERENCE_TRADES_FILE) {
        Ok(text) => text,
        Err(_) => {
            warn!(
                "{} not found: only deployers the reference wallet trades from now on are followed",
                rules::REFERENCE_TRADES_FILE
            );
            return 0;
        }
    };
    let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    for (at, key, lamports) in text.lines().filter(|l| !l.trim_start().starts_with('#')).filter_map(parse_line) {
        records.entry(key).or_default().add(at, lamports);
        LAST_RECORDED.fetch_max(at, Ordering::Relaxed);
        FIRST_RECORDED.fetch_min(at, Ordering::Relaxed);
    }
    records.len()
}

/// Every deployer and creator in the history.
pub fn keys() -> Vec<Pubkey> {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect()
}

/// Deployers and creators the bot follows at `now`.
pub fn followed_keys(now: u64) -> Vec<Pubkey> {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(_, r)| follow_of(r, now).is_some()).map(|(k, _)| *k).collect()
}

/// How many deployers and creators the bot follows at `now`.
pub fn followed_count(now: u64) -> usize {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).values().filter(|r| follow_of(r, now).is_some()).count()
}

/// The follow decision for a launch, from its deployer and coin creator.
pub fn follow(keys: &[Pubkey], now: u64) -> Option<Follow> {
    let records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let found: Vec<Follow> = keys.iter().filter_map(|k| records.get(k)).filter_map(|r| follow_of(r, now)).collect();
    found.iter().copied().find(|f| *f == Follow::Recent).or(found.first().copied())
}

/// His SOL result (lamports) over a launch's deployer and creator together; None when he has
/// traded neither.
pub fn profit(keys: &[Pubkey]) -> Option<i64> {
    let records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let mut seen: Vec<&Pubkey> = Vec::with_capacity(keys.len());
    let mut total = None;
    for key in keys {
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if let Some(r) = records.get(key) {
            *total.get_or_insert(0) += r.profit;
        }
    }
    total
}

/// His trades on a launch's deployer and creator together (each key once); None when he has
/// traded neither.
pub fn trades(keys: &[Pubkey]) -> Option<u32> {
    let records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let mut seen: Vec<&Pubkey> = Vec::with_capacity(keys.len());
    let mut total = None;
    for key in keys {
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if let Some(r) = records.get(key) {
            *total.get_or_insert(0) += r.trades;
        }
    }
    total
}

/// Puts a record in memory only (benchmarks and tests; no file is written).
#[doc(hidden)]
pub fn seed_record(key: Pubkey, record: Record) {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, record);
}

/// `follow`, `profit` and `trades` for a launch's deployer and creator at once, under one lock and
/// without allocating (the hot path). Same answers as the three calls with `[deployer, creator]`.
pub fn pair_view(deployer: &Pubkey, creator: &Pubkey, now: u64) -> (Option<Follow>, Option<i64>, Option<u32>) {
    let records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let a = records.get(deployer);
    let b = if creator == deployer { None } else { records.get(creator) };
    let fa = a.and_then(|r| follow_of(r, now));
    let fb = records.get(creator).and_then(|r| follow_of(r, now));
    let follow = if fa == Some(Follow::Recent) || fb == Some(Follow::Recent) { Some(Follow::Recent) } else { fa.or(fb) };
    let sum = |f: fn(&Record) -> i64| match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.map_or(0, f) + y.map_or(0, f)),
    };
    (follow, sum(|r| r.profit), sum(|r| r.trades as i64).map(|t| t as u32))
}

/// A trade of the reference wallet seen on the stream: remember it for the coin's creator and
/// deployer, and append it to the history file.
pub fn note_trade(mint: &Pubkey, creator: &Pubkey, is_buy: bool, sol_lamports: u64, now: u64) {
    note_trade_with(mint, creator, None, is_buy, sol_lamports, now)
}

/// `note_trade` with the coin's deployer when known from elsewhere (the backfill looks it up for
/// launches the bot did not see).
fn note_trade_with(mint: &Pubkey, creator: &Pubkey, deployer: Option<Pubkey>, is_buy: bool, sol_lamports: u64, now: u64) {
    let lamports = if is_buy { -(sol_lamports as i64) } else { sol_lamports as i64 };
    let mut keys = vec![*creator];
    if let Some(deployer) = deployer.or_else(|| crate::utils::creator_history::deployer_of(mint)) {
        if deployer != *creator {
            keys.push(deployer);
        }
    }
    let mut new = false;
    {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        for key in &keys {
            new |= !records.contains_key(key) || now.saturating_sub(records[key].last) >= rules::FOLLOW_RECENT_SECS;
            records.entry(*key).or_default().add(now, lamports);
        }
    }
    if crate::store::db_on() {
        for k in &keys {
            crate::store::write(crate::store::Row::Exec {
                sql: "INSERT INTO reference_trades (unix_time, key, lamports_delta) VALUES (?1, ?2, ?3)",
                params: vec![now.into(), k.to_string().into(), lamports.into()],
            });
        }
    }
    let lines: String = keys.iter().map(|k| format!("{now} {k} {lamports}\n")).collect();
    if !crate::store::files_on() {
        if is_buy && new {
            info!("reference wallet bought {mint}: following creator {creator} for {} h", rules::FOLLOW_RECENT_SECS / 3600);
            crate::processors::common::prefetch_wallet_ages(keys);
        }
        return;
    }
    // ASYNC_FILE_WRITES: the append on the writer thread, not in the stream's task.
    crate::utils::file_writer::run(move || {
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(rules::REFERENCE_TRADES_FILE)
            .and_then(|mut f| f.write_all(lines.as_bytes()));
        if let Err(e) = appended {
            warn!("could not append to {}: {e}", rules::REFERENCE_TRADES_FILE);
        }
    });
    if is_buy && new {
        info!("reference wallet bought {mint}: following creator {creator} for {} h", rules::FOLLOW_RECENT_SECS / 3600);
        crate::processors::common::prefetch_wallet_ages(keys);
    }
}

const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const EVENT_CPI_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
const TRADE_EVENT: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];
/// Longest stretch the backfill reaches back (older gaps stay gaps).
const BACKFILL_MAX_SECS: u64 = 3 * 86_400;

/// pump.fun trade events (mint, SOL, is_buy, user, creator) of a transaction in RPC's JSON
/// encoding: top-level and inner instructions of the pump.fun program carrying a trade event.
fn trade_events(tx: &Value) -> Vec<(Pubkey, u64, bool, Pubkey, Pubkey)> {
    let msg = &tx["transaction"]["message"];
    let loaded = &tx["meta"]["loadedAddresses"];
    let keys: Vec<&str> = [&msg["accountKeys"], &loaded["writable"], &loaded["readonly"]]
        .iter()
        .flat_map(|a| a.as_array().into_iter().flatten().filter_map(Value::as_str))
        .collect();
    let mut ixs: Vec<&Value> = msg["instructions"].as_array().into_iter().flatten().collect();
    for inner in tx["meta"]["innerInstructions"].as_array().into_iter().flatten() {
        ixs.extend(inner["instructions"].as_array().into_iter().flatten());
    }
    let key = |d: &[u8], at: usize| d.get(at..at + 32).and_then(|b| <[u8; 32]>::try_from(b).ok()).map(Pubkey::new_from_array);
    let mut out = Vec::new();
    for ix in ixs {
        let program = ix["programIdIndex"].as_u64().and_then(|i| keys.get(i as usize)).copied();
        if program != Some(PUMP_PROGRAM) {
            continue;
        }
        let Some(data) = ix["data"].as_str().and_then(|d| bs58::decode(d).into_vec().ok()) else { continue };
        if data.len() < 16 || data[..8] != EVENT_CPI_TAG || data[8..16] != TRADE_EVENT {
            continue;
        }
        let d = &data[16..];
        if let (Some(mint), Some(sol), Some(&flag), Some(user), Some(creator)) =
            (key(d, 0), d.get(32..40).map(|b| u64::from_le_bytes(b.try_into().unwrap())), d.get(48), key(d, 49), key(d, 169))
        {
            out.push((mint, sol, flag == 1, user, creator));
        }
    }
    out
}

async fn rpc(method: &str, params: Value) -> anyhow::Result<Value> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let reply: Value = crate::config::HTTP_CLIENT.post(crate::config::rpc_endpoint()).json(&body).send().await?.json().await?;
    if let Some(error) = reply.get("error") {
        anyhow::bail!("{method}: {error}");
    }
    Ok(reply["result"].clone())
}

/// The wallet that launched `mint`: the user of the first trade in its create transaction (the
/// dev buy), else the create's fee payer, as the bot records deployers at launch.
async fn deployer_from_chain(mint: &Pubkey) -> Option<Pubkey> {
    let config = json!({ "transactionDetails": "full", "sortOrder": "asc", "limit": 20, "encoding": "json", "maxSupportedTransactionVersion": 1 });
    let page = rpc("getTransactionsForAddress", json!([mint.to_string(), config])).await.ok()?;
    let create = page["data"].as_array()?.iter().find(|tx| {
        tx["meta"]["logMessages"].as_array().is_some_and(|logs| logs.iter().filter_map(Value::as_str).any(|l| l.contains("Instruction: Create")))
    })?;
    trade_events(create)
        .into_iter()
        .find(|(m, ..)| m == mint)
        .map(|(_, _, _, user, _)| user)
        .or_else(|| create["transaction"]["message"]["accountKeys"][0].as_str().and_then(|k| Pubkey::from_str(k).ok()))
}

/// His trades the stream could not show while the bot was down: everything since the history
/// file's last line (at most 3 days back), from RPC (Helius getTransactionsForAddress), recorded
/// like live ones at their block time. Without it, a deployer he traded only while the bot was
/// stopped is not followed (26-27 Sep: 85 of his 1,343 trades in 48 h were missed that way, and a
/// deployer he traded 9 times was skipped as "not followed"). Returns (trades, coins).
pub async fn backfill(now: u64) -> anyhow::Result<(usize, usize)> {
    let last = LAST_RECORDED.load(Ordering::Relaxed);
    if last == 0 {
        return Ok((0, 0));
    }
    let since = (last + 1).max(now.saturating_sub(BACKFILL_MAX_SECS));
    // Every reference wallet (REFERENCE_WALLETS), each from the same point.
    let (mut trades, mut coins) = (0, 0);
    for &wallet in rules::reference_wallets() {
        let (t, c) = backfill_wallet(wallet, since, now).await?;
        trades += t;
        coins += c;
    }
    Ok((trades, coins))
}

/// One wallet's pump.fun trades from `since` (unix seconds) to now, from RPC, recorded like live
/// ones at their block time (each signature once). The startup backfill above, and the
/// `reference_backfill` tool for a wallet newly added to REFERENCE_WALLETS. Returns (trades, coins).
pub async fn backfill_wallet(wallet: Pubkey, since: u64, now: u64) -> anyhow::Result<(usize, usize)> {
    let (mut trades, mut coins, mut token): (usize, std::collections::HashSet<Pubkey>, Option<String>) = (0, Default::default(), None);
    let mut deployers: HashMap<Pubkey, Option<Pubkey>> = HashMap::new();
    loop {
        let mut config = json!({
            "transactionDetails": "full", "sortOrder": "asc", "limit": 100, "encoding": "json", "maxSupportedTransactionVersion": 1,
            "filters": { "blockTime": { "gte": since }, "status": "succeeded" }
        });
        if let Some(t) = &token {
            config["paginationToken"] = json!(t);
        }
        let page = rpc("getTransactionsForAddress", json!([wallet.to_string(), config])).await?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        for tx in &data {
            let Some(signature) = tx["transaction"]["signatures"][0].as_str().and_then(|s| solana_signature::Signature::from_str(s).ok()) else { continue };
            // Also noted live (the stream started meanwhile): once only.
            if !first_time(&signature) {
                continue;
            }
            let at = tx["blockTime"].as_u64().unwrap_or(now);
            for (mint, sol, is_buy, user, creator) in trade_events(tx) {
                if user != wallet {
                    continue;
                }
                let deployer = match crate::utils::creator_history::deployer_of(&mint) {
                    Some(d) => Some(d),
                    None => match deployers.get(&mint) {
                        Some(d) => *d,
                        None => {
                            let d = deployer_from_chain(&mint).await;
                            deployers.insert(mint, d);
                            d
                        }
                    },
                };
                note_trade_with(&mint, &creator, deployer, is_buy, sol, at);
                trades += 1;
                coins.insert(mint);
            }
        }
        token = page["paginationToken"].as_str().map(String::from);
        if token.is_none() || data.is_empty() {
            break;
        }
    }
    Ok((trades, coins.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_history_fails_the_start_check() {
        let now = 1_792_000_000;
        FIRST_RECORDED.store(now - 86_400, Ordering::Relaxed);
        assert!((history_days(now) - 1.0).abs() < 1e-9);
        assert!(check_history(now, 199).unwrap_err().contains("covers only 1.0 days"));
        FIRST_RECORDED.store(now - 36 * 86_400, Ordering::Relaxed);
        assert!(check_history(now, 1_980).is_ok());
        FIRST_RECORDED.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(history_days(now), 0.0);
    }

    #[test]
    fn the_pair_view_answers_like_the_three_lookups() {
        let now = 2_000_000_000;
        let keys: Vec<Pubkey> = (0..4).map(|_| Pubkey::new_unique()).collect();
        {
            let mut records = RECORDS.lock().unwrap();
            records.insert(keys[0], Record { last: now - 10, profit: -5, trades: 2 });
            records.insert(keys[1], Record { last: now - 100 * 3600, profit: 900_000_000, trades: 7 });
            records.insert(keys[2], Record { last: now - 100 * 3600, profit: 3, trades: 1 });
        }
        for a in &keys {
            for b in &keys {
                let pair = [*a, *b];
                assert_eq!(pair_view(a, b, now), (follow(&pair, now), profit(&pair), trades(&pair)), "{a} {b}");
            }
        }
    }

    #[test]
    fn trade_events_are_read_from_rpc_json() {
        let (mint, user, creator, other) = (Pubkey::new_unique(), rules::REFERENCE_WALLET, Pubkey::new_unique(), Pubkey::new_unique());
        let mut d = Vec::new();
        d.extend(EVENT_CPI_TAG);
        d.extend(TRADE_EVENT);
        d.extend(mint.to_bytes());
        d.extend(1_500_000_000u64.to_le_bytes());
        d.extend(7u64.to_le_bytes());
        d.push(1);
        d.extend(user.to_bytes());
        d.resize(16 + 169, 0);
        d.extend(creator.to_bytes());
        let event = bs58::encode(&d).into_string();
        let tx = json!({
            "transaction": { "message": { "accountKeys": [user.to_string(), PUMP_PROGRAM], "instructions": [{ "programIdIndex": 1, "data": "3" }] } },
            "meta": {
                "loadedAddresses": { "writable": [other.to_string()], "readonly": [] },
                "innerInstructions": [{ "index": 0, "instructions": [
                    { "programIdIndex": 1, "data": event },
                    { "programIdIndex": 2, "data": event }
                ] }]
            }
        });
        // Only the pump.fun program's event counts; the same bytes from another program do not.
        assert_eq!(trade_events(&tx), vec![(mint, 1_500_000_000, true, user, creator)]);
    }

    #[test]
    fn recent_trades_are_followed_for_41_hours() {
        let r = Record { last: 1_000, profit: -5, trades: 1 };
        assert_eq!(follow_of(&r, 1_000 + rules::FOLLOW_RECENT_SECS - 1), Some(Follow::Recent));
        assert_eq!(follow_of(&r, 1_000 + rules::FOLLOW_RECENT_SECS), None);
    }

    #[test]
    fn old_but_profitable_deployers_stay_followed() {
        let profit = rules::lamports(0.77) as i64;
        let r = Record { last: 0, profit, trades: 1 };
        assert_eq!(follow_of(&r, 10_000_000), Some(Follow::Profitable));
        let r = Record { last: 0, profit: profit - 1, trades: 1 };
        assert_eq!(follow_of(&r, 10_000_000), None);
    }

    #[test]
    fn history_lines_parse_and_add_up() {
        let key = "4F6ZbYX2ZRd7zV14zmY3dciTnm14hWD9prskde3dnPSa";
        let (at, k, l) = parse_line(&format!("1788874348 {key} -2926383171")).unwrap();
        assert_eq!((at, k.to_string(), l), (1788874348, key.to_string(), -2926383171));
        let mut r = Record::default();
        r.add(1788874348, -2926383171);
        r.add(1788874350, 3047744152);
        assert_eq!(r, Record { last: 1788874350, profit: 121360981, trades: 2 });
        assert!(parse_line("# comment").is_none());
    }
}
