







use {
    super::csv_logger::{mode_of_path, PAPER_TRADE_LOG_PATH, TRADE_LOG_PATH},
    crate::config::{
        buy_lamports, lamports_to_sol, set_buy_sol, trading_wallet, BuySizeChange, ConfirmService, Dex, PUBKEY,
        RPC_CLIENT, TRADE_SETTING,
    },
    anyhow::{Context, Result},
    axum::{
        extract::Request,
        http::{header, HeaderMap, StatusCode},
        middleware::{self, Next},
        response::{
            sse::{Event, KeepAlive, Sse},
            Html, IntoResponse, Response,
        },
        routing::{get, post},
        Json, Router,
    },
    futures::Stream,
    log::{info, warn},
    serde::{Deserialize, Serialize},
    std::{
        collections::{BTreeMap, HashMap, VecDeque},
        convert::Infallible,
        net::SocketAddr,
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
    tokio::sync::broadcast::{self, error::TryRecvError},
};

const PAGE: &str = include_str!("dashboard.html");
const BALANCE_REFRESH: Duration = Duration::from_secs(30);

const HEARTBEAT: Duration = Duration::from_secs(10);

const COALESCE: Duration = Duration::from_millis(150);
/// Token names (symbol and name) looked up once per mint and kept here across restarts.
const TOKEN_NAMES_PATH: &str = "token_names.json";
const NAME_LOOKUP_EVERY: Duration = Duration::from_secs(3);
/// A token the lookup does not know yet (brand new) is asked about again after this long.
const NAME_RETRY: Duration = Duration::from_secs(30);
const ACTIVITY_LIMIT: usize = 150;
const TRADES_LIMIT: usize = 5_000;

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Unix time in ms (no allocation: the hot path stamps its sends with it).
pub fn now_unix_ms() -> u64 {
    now_millis()
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

static CHANGES: LazyLock<broadcast::Sender<()>> = LazyLock::new(|| broadcast::channel(64).0);


pub fn notify() {
    let _ = CHANGES.send(());
}


#[derive(Default)]
pub struct Stats {
    
    pub detected: AtomicU64,
    
    pub filtered: AtomicU64,
    pub buys_ok: AtomicU64,
    pub buys_failed: AtomicU64,
    pub sells_ok: AtomicU64,
    pub sells_failed: AtomicU64,
}

pub static STATS: LazyLock<Stats> = LazyLock::new(Stats::default);
static STARTED: LazyLock<(Instant, u64)> = LazyLock::new(|| (Instant::now(), now_unix()));
static BALANCE_LAMPORTS: AtomicU64 = AtomicU64::new(u64::MAX);


pub fn wallet_balance_lamports() -> Option<u64> {
    match BALANCE_LAMPORTS.load(Ordering::Relaxed) {
        u64::MAX => None,
        lamports => Some(lamports),
    }
}


pub fn set_wallet_balance(lamports: u64) {
    BALANCE_LAMPORTS.store(lamports, Ordering::Relaxed);
}

pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
    notify();
}


#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Detected,
    Skipped,
    Bought,
    BuyFailed,
    Sold,
    SellFailed,
    Settings,
}

#[derive(Debug, Clone, Serialize)]
struct ActivityItem {
    id: u64,
    at_ms: u64,
    kind: ActivityKind,
    dex: Option<String>,
    mint: Option<String>,
    message: String,
}

static ACTIVITY: LazyLock<Mutex<VecDeque<ActivityItem>>> = LazyLock::new(Default::default);
static ACTIVITY_ID: AtomicU64 = AtomicU64::new(0);


pub fn activity(kind: ActivityKind, dex: Option<&str>, mint: Option<&str>, message: impl Into<String>) {
    let item = ActivityItem {
        id: ACTIVITY_ID.fetch_add(1, Ordering::Relaxed),
        at_ms: now_millis(),
        kind,
        dex: dex.map(str::to_string),
        mint: mint.map(str::to_string),
        
        message: super::logging::redact_urls(&message.into()),
    };
    if crate::store::db_on() {
        let venue = item.dex.as_deref().map(|d| if d.to_ascii_lowercase().contains("swap") { "pumpswap" } else { "pumpfun" });
        // Every skip and every decision to buy goes through here: the launch's verdict.
        if let (Some(mint), Some(v)) = (item.mint.as_deref(), venue) {
            match item.kind {
                ActivityKind::Skipped => crate::store::launch_verdict(
                    mint,
                    v,
                    "skip",
                    Some(item.message.strip_prefix("Skipped: ").unwrap_or(&item.message).to_string()),
                    None,
                ),
                ActivityKind::Detected => crate::store::launch_verdict(mint, v, "buy", None, None),
                _ => {}
            }
        }
        crate::store::write(crate::store::Row::Exec {
            sql: "INSERT INTO activity (run_id, at_ns, kind, venue, dex, mint, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params: vec![
                crate::store::run_id().into(),
                (item.at_ms as i64 * 1_000_000).into(),
                format!("{:?}", item.kind).into(),
                venue.into(),
                item.dex.clone().into(),
                item.mint.clone().into(),
                item.message.clone().into(),
            ],
        });
    }
    let mut feed = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
    feed.push_front(item);
    feed.truncate(ACTIVITY_LIMIT);
    drop(feed);
    notify();
}


#[derive(Debug, Clone, Serialize)]
pub struct PositionView {
    pub mint: String,
    pub dex: String,
    pub paper: bool,
    pub entry_sol: f64,
    pub token_amount: u64,
    pub value_sol: Option<f64>,
    pub pnl_percent: Option<f64>,
    pub peak_pnl_percent: Option<f64>,
    pub take_profit_armed: bool,
    
    pub sold_percent: f64,
    pub ladder_step: u32,
    pub opened_at: u64,
    pub updated_at: u64,
}

static POSITIONS: LazyLock<Mutex<HashMap<String, PositionView>>> = LazyLock::new(Default::default);

pub fn upsert_position(view: PositionView) {
    POSITIONS.lock().unwrap_or_else(|e| e.into_inner()).insert(view.mint.clone(), view);
    notify();
}

pub fn remove_position(mint: &str) {
    POSITIONS.lock().unwrap_or_else(|e| e.into_inner()).remove(mint);
    notify();
}

type TradeRow = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenName {
    symbol: String,
    name: String,
}

static TOKEN_NAMES: LazyLock<Mutex<HashMap<String, TokenName>>> = LazyLock::new(Default::default);


static TRADES: LazyLock<Mutex<(u64, Vec<TradeRow>)>> = LazyLock::new(Default::default);


pub fn record_trade_row(row: TradeRow) {
    let mut trades = TRADES.lock().unwrap_or_else(|e| e.into_inner());
    trades.0 += 1;
    let mode = row.get("mode").cloned();
    trades.1.push(row);
    // TRADES_LIMIT rows per history (live, paper).
    if trades.1.iter().filter(|r| r.get("mode") == mode.as_ref()).count() > TRADES_LIMIT {
        if let Some(oldest) = trades.1.iter().position(|r| r.get("mode") == mode.as_ref()) {
            trades.1.remove(oldest);
        }
    }
    drop(trades);
    notify();
}

#[derive(Serialize)]
struct DashboardState {
    mode: &'static str,
    /// Whether new launches are being bought (the bot starts paused).
    trading: bool,
    wallet: String,
    trading_wallet: String,
    balance_sol: Option<f64>,
    confirm_service: &'static str,
    dexes: Vec<&'static str>,
    buy_sol: f64,
    max_buy_sol: f64,
    open_positions: usize,
    max_open_positions: usize,
    /// One line per trading rule (`config::rules::summary`).
    rules: Vec<String>,
    known_deployers: usize,
    deployer_history_secs: u64,
    slippage_percent: f64,
    /// The buy's guard as SOL others may put in ahead of it (None: the price guard).
    buy_ahead_max_sol: Option<f64>,
    started_at: u64,
    uptime_secs: u64,
    now: u64,
    counters: BTreeMap<&'static str, u64>,
    /// The in-process shred engine's feed (ENGINE_INPROC): what pumpfeed's dashboard showed.
    #[serde(skip_serializing_if = "Option::is_none")]
    shreds: Option<serde_json::Value>,
    positions: Vec<PositionView>,
    activity: Vec<ActivityItem>,
    trades_version: u64,
    
    #[serde(skip_serializing_if = "Option::is_none")]
    trades: Option<Vec<TradeRow>>,
    /// Symbol and name of every traded token whose name is known, by mint.
    names: HashMap<String, TokenName>,
    /// The snipe test (SNIPE_TEST=1): its size, caps and today's count.
    #[serde(skip_serializing_if = "Option::is_none")]
    snipe_test: Option<serde_json::Value>,
}

fn snapshot(last_trades_version: Option<u64>) -> DashboardState {
    let setting = &*TRADE_SETTING;
    let stats = &*STATS;
    let counters = BTreeMap::from([
        ("detected", stats.detected.load(Ordering::Relaxed)),
        ("filtered", stats.filtered.load(Ordering::Relaxed)),
        ("buys_ok", stats.buys_ok.load(Ordering::Relaxed)),
        ("buys_failed", stats.buys_failed.load(Ordering::Relaxed)),
        ("sells_ok", stats.sells_ok.load(Ordering::Relaxed)),
        ("sells_failed", stats.sells_failed.load(Ordering::Relaxed)),
    ]);
    let mut positions: Vec<PositionView> =
        POSITIONS.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    positions.sort_by_key(|p| std::cmp::Reverse(p.opened_at));
    let activity: Vec<ActivityItem> = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
    let (trades_version, trades) = {
        let trades = TRADES.lock().unwrap_or_else(|e| e.into_inner());
        let changed = last_trades_version != Some(trades.0);
        (trades.0, changed.then(|| trades.1.clone()))
    };
    let balance = BALANCE_LAMPORTS.load(Ordering::Relaxed);

    let shreds = crate::hot::engine::INPROC.enabled.then(|| {
        use {crate::hot::shreds::counters as c, std::sync::atomic::Ordering::Relaxed};
        let (ok, last_ms) = crate::hot::shreds::feed_ok(crate::hot::clock::now_ns());
        serde_json::json!({
            "status": if ok { "ok" } else { "down" },
            "last_shred_ms": last_ms,
            "packets": {"proxy_output": c::PACKETS[0].load(Relaxed), "capture": c::PACKETS[1].load(Relaxed)},
            "first_by_path": {"proxy_output": c::FIRST[0].load(Relaxed), "capture": c::FIRST[1].load(Relaxed)},
            "bad_packets": c::BAD_PACKETS.load(Relaxed),
            "batches": c::BATCHES.load(Relaxed),
            "pump_batches": c::PUMP_BATCHES.load(Relaxed),
            "bad_batches": c::BAD_BATCHES.load(Relaxed),
            "recovered_shreds": c::RECOVERED_SHREDS.load(Relaxed),
            "incomplete_slots": c::INCOMPLETE_SLOTS.load(Relaxed),
        })
    });
    DashboardState {
        shreds,
        mode: if setting.dry_run { "dry_run" } else { "live" },
        trading: super::control::trading_enabled(),
        wallet: PUBKEY.to_string(),
        trading_wallet: trading_wallet().to_string(),
        balance_sol: (balance != u64::MAX).then(|| lamports_to_sol(balance)),
        confirm_service: match setting.confirm_service {
            ConfirmService::Rpc => "RPC",
            ConfirmService::Jito => "Jito",
            ConfirmService::Nozomi => "Nozomi",
            ConfirmService::ZeroSlot => "0slot",
            ConfirmService::Helius => "helius-sender",
        },
        dexes: crate::config::rules::DEXES.iter().map(|d: &Dex| d.name()).collect(),
        buy_sol: lamports_to_sol(buy_lamports()),
        max_buy_sol: crate::config::MAX_RUNTIME_BUY_SOL,
        open_positions: crate::processors::common::open_position_count(),
        max_open_positions: crate::config::rules::max_open_positions(),
        rules: crate::config::rules::summary(),
        known_deployers: super::creator_history::known_deployers(),
        deployer_history_secs: super::creator_history::history_span_secs(now_unix()),
        slippage_percent: setting.buy_slippage_bps as f64 / 100.0,
        buy_ahead_max_sol: crate::config::rules::BUY_AHEAD_MAX_SOL,
        started_at: STARTED.1,
        uptime_secs: STARTED.0.elapsed().as_secs(),
        now: now_unix(),
        counters,
        positions,
        activity,
        trades_version,
        trades,
        names: TOKEN_NAMES.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        snipe_test: crate::utils::snipe_test::on().then(|| {
            let t = crate::utils::snipe_test::settings();
            let (today, hour, pnl) = crate::utils::snipe_test::caps_view(now_unix());
            serde_json::json!({
                "sol": lamports_to_sol(t.size_lamports),
                "max_per_day": t.max_per_day,
                "max_per_hour": t.max_per_hour,
                "max_loss_sol": lamports_to_sol(t.max_loss_lamports),
                "today": today,
                "this_hour": hour,
                "pnl_today_sol": pnl as f64 / 1e9,
            })
        }),
    }
}


pub async fn spawn_dashboard(addr: SocketAddr) -> Result<()> {
    LazyLock::force(&STARTED);
    load_trade_log().await;
    load_token_names().await;
    tokio::spawn(resolve_token_names());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind dashboard to {addr}"))?;
    match (&TRADE_SETTING.dashboard_password, addr.ip().is_loopback()) {
        (Some(_), false) => info!(
            "dashboard on {addr} requires DASHBOARD_USER / DASHBOARD_PASSWORD. It is plain HTTP: over the internet, \
             reach it through an SSH tunnel or an HTTPS proxy so the password is not sent in the clear"
        ),
        (None, false) => warn!(
            "dashboard listens on {addr} but DASHBOARD_PASSWORD is not set, so it only answers requests from this machine; \
             set DASHBOARD_PASSWORD to use it from others"
        ),
        _ => {}
    }
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/state", get(|| async { Json(snapshot(None)) }))
        .route("/api/events", get(events))
        .route("/api/buy-size", post(update_buy_size))
        .route("/api/trading", post(update_trading))
        .route("/api/sell", post(sell_position))
        .route("/api/sell-all", post(sell_all_positions))
        .layer(middleware::from_fn(guard_requests));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!("dashboard server stopped: {e}");
        }
    });
    tokio::spawn(async {
        loop {
            if let Ok(balance) = RPC_CLIENT.get_balance(&trading_wallet()).await {
                if BALANCE_LAMPORTS.swap(balance, Ordering::Relaxed) != balance {
                    notify();
                }
            }
            tokio::time::sleep(BALANCE_REFRESH).await;
        }
    });
    info!("dashboard: http://{addr}");
    Ok(())
}


async fn events() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let receiver = CHANGES.subscribe();
    let stream = futures::stream::unfold((receiver, None::<u64>, true), |(mut receiver, sent_version, first)| async move {
        if !first {
            tokio::select! {
                changed = receiver.recv() => {
                    if matches!(changed, Err(broadcast::error::RecvError::Closed)) {
                        return None;
                    }
                }
                _ = tokio::time::sleep(HEARTBEAT) => {}
            }
            tokio::time::sleep(COALESCE).await;
            while let Ok(()) | Err(TryRecvError::Lagged(_)) = receiver.try_recv() {}
        }
        let state = snapshot(sent_version);
        let version = Some(state.trades_version);
        let event = Event::default()
            .event("state")
            .json_data(&state)
            .unwrap_or_else(|_| Event::default().comment("snapshot failed"));
        Some((Ok(event), (receiver, version, false)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct BuySizeRequest {
    sol: f64,
}

async fn update_buy_size(Json(request): Json<BuySizeRequest>) -> Response {
    // The snipe test's size is SNIPE_TEST_SOL, set by hand in .env only.
    if crate::utils::snipe_test::on() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "snipe test mode: the size is SNIPE_TEST_SOL in .env, set by hand (not from the dashboard)" })),
        )
            .into_response();
    }
    match set_buy_sol(request.sol) {
        Ok(change) => {
            let BuySizeChange { previous_sol, buy_sol, saved_to_env } = change;
            info!("buy size changed from {previous_sol} to {buy_sol} SOL (saved to .env: {saved_to_env})");
            activity(
                ActivityKind::Settings,
                None,
                None,
                format!(
                    "Buy size changed from {previous_sol} to {buy_sol} SOL{}",
                    if saved_to_env { "" } else { " (not saved to .env)" }
                ),
            );
            Json(change).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

#[derive(Deserialize)]
struct TradingRequest {
    enabled: bool,
}


async fn update_trading(Json(request): Json<TradingRequest>) -> Response {
    let was = super::control::set_trading(request.enabled);
    if was != request.enabled {
        let (verb, detail) = if request.enabled {
            ("started", "new launches will be bought")
        } else {
            ("paused", "no new buys; open positions are still managed")
        };
        info!("trading {verb} from the dashboard: {detail}");
        activity(ActivityKind::Settings, None, None, format!("Trading {verb}: {detail}"));
        notify();
    }
    Json(serde_json::json!({ "trading": request.enabled })).into_response()
}

#[derive(Deserialize)]
struct SellRequest {
    mint: String,
}


async fn sell_position(Json(request): Json<SellRequest>) -> Response {
    let open = POSITIONS.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&request.mint);
    let mint = match request.mint.parse::<solana_pubkey::Pubkey>() {
        Ok(mint) if open => mint,
        Ok(_) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no open position for that token" }))).into_response(),
        Err(_) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "not a token address" }))).into_response(),
    };
    info!("manual sell of {mint} requested from the dashboard");
    activity(ActivityKind::Settings, None, Some(&request.mint), "Manual sell requested");
    super::control::request_sell(mint);
    Json(serde_json::json!({ "requested": [request.mint] })).into_response()
}


async fn sell_all_positions() -> Response {
    let mints: Vec<String> = POSITIONS.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
    for mint in &mints {
        if let Ok(pubkey) = mint.parse::<solana_pubkey::Pubkey>() {
            super::control::request_sell(pubkey);
        }
    }
    if !mints.is_empty() {
        info!("manual sell of all {} open positions requested from the dashboard", mints.len());
        activity(ActivityKind::Settings, None, None, format!("Manual sell of all {} open positions requested", mints.len()));
    }
    Json(serde_json::json!({ "requested": mints })).into_response()
}


const FAILED_LOGIN_DELAY: std::time::Duration = std::time::Duration::from_millis(750);









async fn guard_requests(request: Request, next: Next) -> Response {
    let Some(password) = TRADE_SETTING.dashboard_password.as_deref() else {
        return local_requests_only(request, next).await;
    };
    let headers = request.headers();
    let host = header_host(headers, header::HOST);
    let same_origin = match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin_host(origin).is_some_and(|o| Some(o) == host),
    };
    if !same_origin {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    if !basic_auth_ok(headers, &TRADE_SETTING.dashboard_user, password) {
        if headers.contains_key(header::AUTHORIZATION) {
            warn!("dashboard: wrong user or password (Host {})", host.unwrap_or("?"));
            tokio::time::sleep(FAILED_LOGIN_DELAY).await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"sniper bot\", charset=\"UTF-8\"")],
            "login required",
        )
            .into_response();
    }
    next.run(request).await
}

fn origin_host(origin: &header::HeaderValue) -> Option<&str> {
    origin.to_str().ok().and_then(|o| o.split("://").nth(1)).map(|rest| rest.split('/').next().unwrap_or(rest))
}


fn basic_auth_ok(headers: &HeaderMap, user: &str, password: &str) -> bool {
    use base64::Engine;
    let Some(encoded) = header_host(headers, header::AUTHORIZATION).and_then(|v| v.strip_prefix("Basic ")) else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let expected = format!("{user}:{password}");
    constant_time_eq(&decoded, expected.as_bytes())
}


fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}



async fn local_requests_only(request: Request, next: Next) -> Response {
    let headers = request.headers();
    let configured = TRADE_SETTING.dashboard_addr;
    let host_ok = header_host(headers, header::HOST).is_some_and(|h| is_local_host(h, configured));
    let origin_ok = match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .ok()
            .and_then(|o| o.split("://").nth(1))
            .map(|rest| rest.split('/').next().unwrap_or(rest))
            .is_some_and(|h| is_local_host(h, configured)),
    };
    if host_ok && origin_ok {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "dashboard only accepts requests from this machine").into_response()
    }
}

fn header_host(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}


fn is_local_host(host_port: &str, configured: Option<SocketAddr>) -> bool {
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host_port.rsplit_once(':').map_or(host_port, |(host, _)| host)
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        || configured.is_some_and(|addr| !addr.ip().is_unspecified() && addr.ip().to_string() == host)
}


async fn load_token_names() {
    if crate::store::read_from_db() {
        let rows = tokio::task::spawn_blocking(crate::store::read::token_names).await.unwrap_or_default();
        let mut names = TOKEN_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        for (mint, symbol, name) in rows {
            names.insert(mint, TokenName { symbol: symbol.unwrap_or_default(), name: name.unwrap_or_default() });
        }
        return;
    }
    let Ok(text) = tokio::fs::read_to_string(TOKEN_NAMES_PATH).await else {
        return;
    };
    match serde_json::from_str::<HashMap<String, TokenName>>(&text) {
        Ok(names) => TOKEN_NAMES.lock().unwrap_or_else(|e| e.into_inner()).extend(names),
        Err(e) => warn!("{TOKEN_NAMES_PATH} is not valid JSON, names will be looked up again: {e}"),
    }
}

/// Looks up the symbol and name of every traded or held token the dashboard has no name for,
/// through the RPC's asset API (Helius `getAssetBatch`), and keeps them in TOKEN_NAMES_PATH.
async fn resolve_token_names() {
    let mut asked: HashMap<String, Instant> = HashMap::new();
    loop {
        tokio::time::sleep(NAME_LOOKUP_EVERY).await;
        let wanted: Vec<String> = {
            let names = TOKEN_NAMES.lock().unwrap_or_else(|e| e.into_inner());
            let trades = TRADES.lock().unwrap_or_else(|e| e.into_inner());
            let positions = POSITIONS.lock().unwrap_or_else(|e| e.into_inner());
            let mut wanted: Vec<String> = trades.1.iter().filter_map(|row| row.get("mint").cloned()).chain(positions.keys().cloned()).collect();
            wanted.sort_unstable();
            wanted.dedup();
            wanted.retain(|mint| !mint.is_empty() && !names.contains_key(mint) && asked.get(mint).is_none_or(|at| at.elapsed() >= NAME_RETRY));
            wanted.truncate(100);
            wanted
        };
        if wanted.is_empty() {
            continue;
        }
        for mint in &wanted {
            asked.insert(mint.clone(), Instant::now());
        }
        let found = match lookup_token_names(&wanted).await {
            Ok(found) => found,
            Err(e) => {
                log::debug!("token name lookup failed: {e:#}");
                continue;
            }
        };
        if found.is_empty() {
            continue;
        }
        if crate::store::db_on() {
            for (mint, n) in &found {
                crate::store::write(crate::store::Row::Exec {
                    sql: "INSERT INTO token_names (mint, symbol, name) VALUES (?1, ?2, ?3) ON CONFLICT(mint) DO UPDATE SET symbol = ?2, name = ?3",
                    params: vec![mint.as_str().into(), n.symbol.as_str().into(), n.name.as_str().into()],
                });
            }
        }
        let json = {
            let mut names = TOKEN_NAMES.lock().unwrap_or_else(|e| e.into_inner());
            names.extend(found);
            serde_json::to_string_pretty(&*names)
        };
        if let (true, Ok(json)) = (crate::store::files_on(), json) {
            if let Err(e) = tokio::fs::write(TOKEN_NAMES_PATH, json).await {
                warn!("cannot save {TOKEN_NAMES_PATH}: {e}");
            }
        }
        notify();
    }
}

async fn lookup_token_names(mints: &[String]) -> Result<Vec<(String, TokenName)>> {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "getAssetBatch", "params": { "ids": mints } });
    let response: serde_json::Value =
        crate::config::clients::HTTP_CLIENT.post(crate::config::clients::rpc_endpoint()).json(&body).send().await?.json().await?;
    let assets = response["result"].as_array().with_context(|| format!("no result: {}", response["error"]))?;
    Ok(assets
        .iter()
        .filter_map(|asset| {
            let mint = asset["id"].as_str()?;
            let meta = &asset["content"]["metadata"];
            let text = |key: &str| meta[key].as_str().unwrap_or_default().trim().to_string();
            let (symbol, name) = (text("symbol"), text("name"));
            (!symbol.is_empty() || !name.is_empty()).then(|| (mint.to_string(), TokenName { symbol, name }))
        })
        .collect())
}

/// Both histories, each row tagged with its mode ("live" or "paper"); the page shows one at a time.
async fn load_trade_log() {
    let mut rows: Vec<TradeRow> = Vec::new();
    for path in [TRADE_LOG_PATH, PAPER_TRADE_LOG_PATH] {
        let Ok(text) = tokio::fs::read_to_string(path).await else {
            continue;
        };
        let mut lines = text.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let columns = split_csv_line(header);
        let mut file_rows: Vec<TradeRow> = lines
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut row: TradeRow = columns.iter().cloned().zip(split_csv_line(line)).collect();
                row.insert("mode".into(), mode_of_path(path).into());
                row
            })
            .collect();
        let excess = file_rows.len().saturating_sub(TRADES_LIMIT);
        file_rows.drain(..excess);
        rows.append(&mut file_rows);
    }
    let mut trades = TRADES.lock().unwrap_or_else(|e| e.into_inner());
    rows.append(&mut trades.1);
    trades.1 = rows;
    trades.0 += 1;
}

fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
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

#[cfg(test)]
mod tests {
    use super::{basic_auth_ok, header, is_local_host, split_csv_line, HeaderMap};

    fn auth(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn basic_auth_accepts_only_the_exact_credentials() {
        use base64::Engine;
        let encode = |s: &str| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(s));
        assert!(basic_auth_ok(&auth(&encode("admin:correct horse")), "admin", "correct horse"));
        assert!(!basic_auth_ok(&auth(&encode("admin:correct hors")), "admin", "correct horse"));
        assert!(!basic_auth_ok(&auth(&encode("admin:correct horse ")), "admin", "correct horse"));
        assert!(!basic_auth_ok(&auth(&encode("root:correct horse")), "admin", "correct horse"));
        assert!(!basic_auth_ok(&auth("Bearer x"), "admin", "correct horse"));
        assert!(!basic_auth_ok(&HeaderMap::new(), "admin", "correct horse"));
    }

    #[test]
    fn splits_quoted_fields() {
        assert_eq!(
            split_csv_line(r#"a,"b, ""c""",,d"#),
            vec!["a".to_string(), r#"b, "c""#.to_string(), String::new(), "d".to_string()]
        );
    }

    #[test]
    fn only_local_hosts_pass() {
        assert!(is_local_host("localhost:8787", None));
        assert!(is_local_host("127.0.0.1:8787", None));
        assert!(is_local_host("[::1]:9000", None));
        assert!(!is_local_host("evil.example:8787", None));
        assert!(!is_local_host("192.168.1.5:8787", None));
        assert!(is_local_host("192.168.1.5:8787", Some("192.168.1.5:8787".parse().unwrap())));
    }
}
