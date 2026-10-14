use {
    anyhow::{anyhow, bail, Result},
    std::{env, fmt, str::FromStr, sync::LazyLock, time::Duration},
};

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// How long a live Nozomi transaction from Frankfurt took to land, in ms from sending it: the 10
/// probes of 25 Sep 2026 (create seen on the stream to the probe on the shred clock, less the
/// 2 ms the probe took to send). Four landed in the launch slot, six in the next one.
pub const FRANKFURT_LANDING_MS: [u64; 10] = [32, 33, 46, 47, 52, 54, 110, 163, 183, 269];

/// Dry run: how long after sending a paper trade "lands". Each trade draws one of the delays, so
/// over many trades the paper fills spread like live ones. Empty: paper trades fill at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaperLatency(Vec<u64>);

impl PaperLatency {
    pub fn draw(&self) -> Option<Duration> {
        use rand::seq::SliceRandom;
        self.0.choose(&mut rand::thread_rng()).map(|ms| Duration::from_millis(*ms))
    }

    pub fn is_off(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for PaperLatency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_slice() {
            [] => write!(f, "at once"),
            [ms] => write!(f, "{ms} ms after sending"),
            all => {
                let mut sorted = all.to_vec();
                sorted.sort_unstable();
                let n = sorted.len();
                let median = (sorted[(n - 1) / 2] + sorted[n / 2]) / 2;
                write!(f, "{}-{} ms after sending (median {median})", sorted[0], sorted[n - 1])
            }
        }
    }
}

fn env_latency(key: &str) -> Result<PaperLatency> {
    parse_latency(key, env_opt(key))
}

/// `wss://beta.helius-rpc.com/?api-key=...` with the API key of RPC_ENDPOINT (a Helius URL).
fn helius_preprocessed_url() -> Result<String> {
    let rpc = env_opt("RPC_ENDPOINT").unwrap_or_default();
    let key = rpc
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("api-key="))
        .filter(|key| !key.is_empty())
        .ok_or_else(|| anyhow!("HELIUS_PREPROCESSED needs a Helius RPC_ENDPOINT with ?api-key=... (or set PREPROCESSED_WS_URL)"))?;
    Ok(format!("wss://beta.helius-rpc.com/?api-key={key}"))
}

/// "HH:MM-HH:MM" (UTC) as minutes since midnight; the end may be earlier than the start (the
/// window wraps past midnight).
pub fn parse_hours(raw: &str) -> Result<(u16, u16)> {
    let minutes = |t: &str| -> Result<u16> {
        let (h, m) = t.trim().split_once(':').ok_or_else(|| anyhow!("TRADING_HOURS_UTC={raw:?}: {t:?} is not HH:MM"))?;
        let (h, m): (u16, u16) = (h.parse()?, m.parse()?);
        if h > 24 || m > 59 || (h == 24 && m > 0) {
            bail!("TRADING_HOURS_UTC={raw:?}: {t:?} is not a time of day");
        }
        Ok(h * 60 + m)
    };
    let (start, end) = raw.split_once('-').ok_or_else(|| anyhow!("TRADING_HOURS_UTC={raw:?}: expected HH:MM-HH:MM"))?;
    let window = (minutes(start)?, minutes(end)?);
    if window.0 == window.1 {
        bail!("TRADING_HOURS_UTC={raw:?}: start and end are the same");
    }
    Ok(window)
}

/// Whether `minute` (UTC minutes since midnight) is inside the window.
pub fn in_window((start, end): (u16, u16), minute: u16) -> bool {
    if start < end { (start..end).contains(&minute) } else { minute >= start || minute < end }
}

/// Unset: the Frankfurt landing delays. One number or a comma list: those delays. 0: at once.
fn parse_latency(key: &str, raw: Option<String>) -> Result<PaperLatency> {
    let Some(raw) = raw else {
        return Ok(PaperLatency(FRANKFURT_LANDING_MS.to_vec()));
    };
    let delays = raw
        .split(',')
        .map(|v| v.trim().replace('_', "").parse::<u64>().map_err(|e| anyhow!("{key}={raw:?}: {v:?} is not a number of ms: {e}")))
        .collect::<Result<Vec<_>>>()?;
    Ok(PaperLatency(delays.into_iter().filter(|ms| *ms > 0).collect()))
}


pub static TRADE_SETTING: LazyLock<TradeSetting> = LazyLock::new(|| {
    TradeSetting::from_env().unwrap_or_else(|e| panic!("invalid configuration: {e:#}"))
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmService {
    
    Rpc,
    Jito,
    Nozomi,
    ZeroSlot,
    
    Helius,
}

impl FromStr for ConfirmService {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_uppercase().replace('-', "_").as_str() {
            "" | "RPC" | "NONE" => Ok(Self::Rpc),
            "JITO" => Ok(Self::Jito),
            "NOZOMI" => Ok(Self::Nozomi),
            "ZERO_SLOT" | "ZEROSLOT" | "0SLOT" => Ok(Self::ZeroSlot),
            "HELIUS" | "HELIUS_SENDER" => Ok(Self::Helius),
            other => bail!("CONFIRM_SERVICE={other:?}: expected JITO, NOZOMI, ZERO_SLOT, HELIUS or empty"),
        }
    }
}


fn parse_services(raw: &str) -> Result<Vec<ConfirmService>> {
    let mut out: Vec<ConfirmService> = Vec::new();
    for name in raw.split(',').filter(|n| !n.trim().is_empty()) {
        let service: ConfirmService = name.parse()?;
        if !out.contains(&service) {
            out.push(service);
        }
    }
    Ok(out)
}


fn confirm_services() -> Result<Vec<ConfirmService>> {
    let mut out = parse_services(&env_opt("CONFIRM_SERVICES").unwrap_or_default())?;
    if out.is_empty() {
        out.push(env_opt("CONFIRM_SERVICE").unwrap_or_default().parse()?);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dex {
    PumpFun,
    PumpSwap,
    RaydiumAmm,
    RaydiumCpmm,
    RaydiumClmm,
    RaydiumLaunchpad,
}

impl Dex {
    pub const ALL: [Dex; 6] = [
        Dex::PumpFun,
        Dex::PumpSwap,
        Dex::RaydiumAmm,
        Dex::RaydiumCpmm,
        Dex::RaydiumClmm,
        Dex::RaydiumLaunchpad,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Dex::PumpFun => "pumpfun",
            Dex::PumpSwap => "pumpswap",
            Dex::RaydiumAmm => "raydium_amm",
            Dex::RaydiumCpmm => "raydium_cpmm",
            Dex::RaydiumClmm => "raydium_clmm",
            Dex::RaydiumLaunchpad => "raydium_launchpad",
        }
    }
}

impl fmt::Display for Dex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Dex {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let key = s.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Dex::ALL
            .into_iter()
            .find(|d| d.name() == key)
            .or(match key.as_str() {
                "pump" | "pump_fun" => Some(Dex::PumpFun),
                "pump_swap" | "pumpamm" | "pump_amm" => Some(Dex::PumpSwap),
                "amm" | "raydium_amm_v4" | "raydium_v4" => Some(Dex::RaydiumAmm),
                "cpmm" => Some(Dex::RaydiumCpmm),
                "clmm" => Some(Dex::RaydiumClmm),
                "launchpad" | "launchlab" | "raydium_launchlab" => Some(Dex::RaydiumLaunchpad),
                _ => None,
            })
            .ok_or_else(|| anyhow!("unknown DEX {s:?} in SNIPE_DEXES"))
    }
}

/// Connections, credentials, landing services and the stake. The trading rules are in
/// `config::rules`, not here: nothing in `.env` changes what the bot buys or when it sells.
#[derive(Debug, Clone)]
pub struct TradeSetting {
    /// Stake per buy (BUY_SOL_AMOUNT); the rules scale it by the dev buy.
    pub buy_lamports: u64,
    /// Price guards, from `rules::SELL_SLIPPAGE_PERCENT` and `rules::BUY_SLIPPAGE_PERCENT`.
    pub slippage_bps: u64,
    pub buy_slippage_bps: u64,
    pub compute_unit_limit: u32,
    pub priority_fee_micro_lamports: u64,
    /// Account data a buy may load (LOADED_DATA_LIMIT_MIB), stated so block producers rank it by
    /// its real cost instead of the 64 MiB default. A pump.fun buy loads about 12.6 MiB, mostly
    /// program code (pump.fun's program data is 10 MiB; measured 26 Sep 2026). Buys only: a sell
    /// must never fail because a program grew.
    pub loaded_data_limit_bytes: Option<u32>,
    /// Compute limit of sells (SELL_COMPUTE_UNIT_LIMIT; measured 51-68k): a tighter limit ranks the
    /// same fee higher.
    pub sell_compute_unit_limit: u32,
    /// Buy only launches whose block producer is in these continents (LEADER_REGIONS, e.g.
    /// "EU"; empty: every launch). See `utils::leaders`.
    pub leader_regions: Vec<String>,
    /// Buy only inside this daily window, UTC minutes since midnight [start, end); it may wrap
    /// past midnight (TRADING_HOURS_UTC, e.g. "01:00-07:00"). Sells always run.
    pub trading_hours: Option<(u16, u16)>,
    pub confirm_service: ConfirmService,
    /// Send each transaction to all of these at once; the first to land wins.
    pub confirm_services: Vec<ConfirmService>,
    pub sell_confirm_services: Vec<ConfirmService>,
    pub sell_priority_fee_micro_lamports: u64,
    pub third_party_fee_lamports: u64,
    /// Nozomi / 0slot tip on buys (BUY_THIRD_PARTY_FEE, default THIRD_PARTY_FEE). With Nozomi the
    /// tip is the bid: Nozomi orders by it and bids it with the block builders, and it is only
    /// paid when the buy lands.
    pub buy_third_party_fee_lamports: u64,
    /// Buys go as Nozomi bundles (NOZOMI_BUNDLE): delivered to block builders (Jito, Harmonic) as
    /// an all-or-nothing unit, so a buy that would fail (a lost race) never lands and costs
    /// nothing. Sells keep SELL_CONFIRM_SERVICES. Tested 26 Sep 2026: our key may send bundles.
    pub nozomi_bundle: bool,
    /// With NOZOMI_BUNDLE, also send each buy as a Jito bundle (JITO_COPY; needs JITO_UUID and
    /// JITO_BUNDLE_ONLY=true): the same transaction, it lands once, on whichever builder is
    /// building. Costs the Jito tip on landed buys. Paired probes 26 Sep 2026: Jito ahead on 1 of
    /// 5 launches, Nozomi on 3.
    pub jito_copy: bool,
    /// Buys go out as one transaction per landing service in `fanout_services`, each tipping only
    /// its own service and all advancing one durable nonce, so at most one lands and the others are
    /// dropped for free (NONCE_FANOUT; `utils::nonce`). Takes precedence over NOZOMI_BUNDLE /
    /// JITO_COPY / CONFIRM_SERVICES for buys; sells are unchanged.
    pub nonce_fanout: bool,
    /// The services a fanned-out buy goes through (FANOUT_SERVICES, default NOZOMI,JITO,HELIUS,
    /// ZERO_SLOT). A Nozomi copy goes both as a Nozomi bundle and a plain Nozomi send when
    /// NOZOMI_BUNDLE is on; a Jito copy as a bundle when JITO_BUNDLE_ONLY is on.
    pub fanout_services: Vec<ConfirmService>,
    pub jito_tip_lamports: u64,
    pub jito_block_engine_url: Option<String>,
    /// Jito auth key (UUID), sent as `x-jito-auth` on every Jito request. Jito support, 26 Sep
    /// 2026: bundles are hard to land without one. Never logged.
    pub jito_uuid: Option<String>,
    /// Send through Jito as a one-transaction bundle only (`bundleOnly=true`): a transaction that
    /// would fail is never included, so it costs nothing. Jito runs on 98.9% of stake (26 Sep).
    pub jito_bundle_only: bool,
    pub nozomi_api_url: Option<String>,
    pub zero_slot_api_url: Option<String>,
    pub dry_run: bool,
    /// Dry run: when a paper buy / sell lands after it is sent, like a live one.
    /// Paper buys land at a block position drawn from real landings (DRY_RUN_FILL_BY_POSITION,
    /// default true) instead of at a time: the position decides who bought first. The time model
    /// filled 42% of races where live filled 8-17% (26 Sep 2026).
    pub dry_run_fill_by_position: bool,
    pub dry_run_buy_latency: PaperLatency,
    pub dry_run_sell_latency: PaperLatency,
    pub dry_run_charge_fees: bool,
    /// Dry run: simulate as this funded wallet instead of PRIVATE_KEY's.
    pub dry_run_wallet: Option<solana_pubkey::Pubkey>,
    pub birdeye_api_key: Option<String>,
    pub dashboard_addr: Option<std::net::SocketAddr>,
    pub dashboard_user: String,
    pub dashboard_password: Option<String>,
    pub shred_grpc_url: Option<String>,
    pub helius_sender_url: Option<String>,
    pub helius_sender_tip_lamports: u64,
    pub shred_token: Option<String>,
    /// Local Jito ShredStream proxy's entries service (`--grpc-service-port`), e.g.
    /// `http://127.0.0.1:9999`. Takes precedence over SHRED_GRPC_URL.
    pub shred_proxy_url: Option<String>,
    /// Deshredded entries pushed to us over UDP (SHRED_UDP_PORT): each datagram is the slot
    /// (u64 LE) then a bincode `Vec<Entry>`, as AllenHark's Direct UDP delivers (26 Sep 2026).
    pub shred_udp_port: Option<u16>,
    /// Buy from shred sightings (false = only measure the head start).
    pub shred_snipe: bool,
    /// Helius preprocessed stream (WebSocket). Carries the API key: never log it.
    pub preprocessed_url: Option<String>,
    /// Buys also wait for the balance cache (deployers followed) instead of RPC.
    pub helius_preprocessed: bool,
}

impl TradeSetting {
    pub fn from_env() -> Result<Self> {
        let third_party_fee_lamports = sol_to_lamports(env_num("THIRD_PARTY_FEE", 0.0)?);
        let jito_tip_lamports = match env_opt("JITO_TIP_VALUE") {
            Some(_) => sol_to_lamports(env_num("JITO_TIP_VALUE", 0.0)?),
            None => third_party_fee_lamports,
        };
        let buy_sol: f64 = env_num("BUY_SOL_AMOUNT", 0.0)?;
        if buy_sol <= 0.0 {
            bail!("BUY_SOL_AMOUNT must be greater than 0");
        }
        let setting = Self {
            buy_lamports: sol_to_lamports(buy_sol),
            slippage_bps: (super::rules::SELL_SLIPPAGE_PERCENT * 100.0).round() as u64,
            buy_slippage_bps: (super::rules::BUY_SLIPPAGE_PERCENT * 100.0).round() as u64,
            compute_unit_limit: env_num("CU", 0u32)?,
            priority_fee_micro_lamports: env_num("PRIORITY_FEE_MICRO_LAMPORT", 0u64)?,
            sell_compute_unit_limit: env_num("SELL_COMPUTE_UNIT_LIMIT", 80_000u32)?,
            loaded_data_limit_bytes: match env_num("LOADED_DATA_LIMIT_MIB", 0u32)? {
                0 => None,
                mib @ 1..=64 => Some(mib * 1024 * 1024),
                mib => bail!("LOADED_DATA_LIMIT_MIB={mib}: at most 64 (the network maximum)"),
            },
            trading_hours: env_opt("TRADING_HOURS_UTC").map(|v| parse_hours(&v)).transpose()?,
            leader_regions: env_opt("LEADER_REGIONS")
                .unwrap_or_default()
                .split(',')
                .map(|c| c.trim().to_uppercase())
                .filter(|c| !c.is_empty())
                .collect(),
            confirm_service: env_opt("CONFIRM_SERVICE").unwrap_or_default().parse()?,
            confirm_services: confirm_services()?,
            sell_confirm_services: match env_opt("SELL_CONFIRM_SERVICES") {
                Some(raw) => parse_services(&raw)?,
                None => confirm_services()?,
            },
            sell_priority_fee_micro_lamports: match env_opt("SELL_PRIORITY_FEE_MICRO_LAMPORT") {
                Some(_) => env_num("SELL_PRIORITY_FEE_MICRO_LAMPORT", 0u64)?,
                None => env_num("PRIORITY_FEE_MICRO_LAMPORT", 0u64)?,
            },
            third_party_fee_lamports,
            buy_third_party_fee_lamports: match env_opt("BUY_THIRD_PARTY_FEE") {
                Some(_) => sol_to_lamports(env_num("BUY_THIRD_PARTY_FEE", 0.0)?),
                None => third_party_fee_lamports,
            },
            nozomi_bundle: env_bool("NOZOMI_BUNDLE", false)?,
            jito_copy: env_bool("JITO_COPY", false)?,
            nonce_fanout: env_bool("NONCE_FANOUT", false)?,
            fanout_services: {
                let services = parse_services(&env_opt("FANOUT_SERVICES").unwrap_or_else(|| "NOZOMI,JITO,HELIUS,ZERO_SLOT".into()))?;
                if services.contains(&ConfirmService::Rpc) {
                    anyhow::bail!("FANOUT_SERVICES cannot include RPC: a fanned-out buy goes only through landing services");
                }
                services
            },
            jito_tip_lamports,
            jito_block_engine_url: env_opt("JITO_BLOCK_ENGINE_URL"),
            jito_uuid: env_opt("JITO_UUID"),
            jito_bundle_only: env_bool("JITO_BUNDLE_ONLY", false)?,
            nozomi_api_url: env_opt("NOZOMI_API_URL"),
            zero_slot_api_url: env_opt("ZERO_SLOT_API_URL"),
            dry_run: env_bool("DRY_RUN", false)?,
            dry_run_fill_by_position: env_bool("DRY_RUN_FILL_BY_POSITION", true)?,
            dry_run_buy_latency: env_latency("DRY_RUN_BUY_LATENCY_MS")?,
            dry_run_sell_latency: env_latency("DRY_RUN_SELL_LATENCY_MS")?,
            dry_run_charge_fees: env_bool("DRY_RUN_CHARGE_FEES", false)?,
            dry_run_wallet: env_opt("DRY_RUN_WALLET")
                .map(|v| v.parse().map_err(|e| anyhow!("DRY_RUN_WALLET is not a valid public key: {e}")))
                .transpose()?,
            birdeye_api_key: env_opt("BIRDEYE_API_KEY"),
            dashboard_user: env_opt("DASHBOARD_USER").unwrap_or_else(|| "admin".to_string()),
            shred_grpc_url: env_opt("SHRED_GRPC_URL"),
            helius_sender_url: env_opt("HELIUS_SENDER_URL"),
            helius_sender_tip_lamports: sol_to_lamports(non_negative("HELIUS_SENDER_TIP_SOL", 0.000005)?),
            shred_token: env_opt("SHRED_TOKEN"),
            shred_proxy_url: env_opt("SHRED_PROXY_URL"),
            shred_udp_port: env_opt("SHRED_UDP_PORT").map(|v| v.parse()).transpose().map_err(|e| anyhow!("SHRED_UDP_PORT: {e}"))?,
            shred_snipe: env_bool("SHRED_SNIPE", false)?,
            helius_preprocessed: env_bool("HELIUS_PREPROCESSED", false)?,
            preprocessed_url: match env_opt("PREPROCESSED_WS_URL") {
                Some(url) => Some(url),
                None if env_bool("HELIUS_PREPROCESSED", false)? => Some(helius_preprocessed_url()?),
                None => None,
            },
            dashboard_password: match env_opt("DASHBOARD_PASSWORD") {
                Some(p) if p.chars().count() < 12 => bail!("DASHBOARD_PASSWORD must be at least 12 characters"),
                p => p,
            },
            dashboard_addr: if env_bool("DASHBOARD_ENABLED", true)? {
                let addr = env_opt("DASHBOARD_ADDR").unwrap_or_else(|| "127.0.0.1:8787".to_string());
                Some(addr.parse().map_err(|e| anyhow!("DASHBOARD_ADDR={addr:?} is not host:port: {e}"))?)
            } else {
                None
            },
        };
        Ok(setting)
    }

    pub fn is_enabled(&self, dex: Dex) -> bool {
        super::rules::DEXES.contains(&dex)
    }

    pub fn min_out(&self, amount: u64) -> u64 {
        apply_slippage_down(amount, self.slippage_bps)
    }

    pub fn buy_min_out(&self, amount: u64) -> u64 {
        apply_slippage_down(amount, self.buy_slippage_bps)
    }

    pub fn max_in(&self, amount: u64) -> u64 {
        apply_slippage_up(amount, self.slippage_bps)
    }
}

pub fn apply_slippage_down(amount: u64, slippage_bps: u64) -> u64 {
    let bps = slippage_bps.min(10_000) as u128;
    (amount as u128 * (10_000 - bps) / 10_000) as u64
}

pub fn apply_slippage_up(amount: u64, slippage_bps: u64) -> u64 {
    let v = amount as u128 * (10_000 + slippage_bps as u128) / 10_000;
    v.min(u64::MAX as u128) as u64
}

pub fn sol_to_lamports(sol: f64) -> u64 {
    (sol * LAMPORTS_PER_SOL as f64).round() as u64
}

pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL as f64
}


pub fn env_opt(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}


pub fn env_num<T>(key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match env_opt(key) {
        None => Ok(default),
        Some(raw) => raw
            .replace('_', "")
            .parse::<T>()
            .map_err(|e| anyhow!("{key}={raw:?} is not a valid number: {e}")),
    }
}

pub fn env_bool(key: &str, default: bool) -> Result<bool> {
    match env_opt(key).map(|v| v.to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) => match v.as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => bail!("{key}={v:?} must be true or false"),
        },
    }
}

fn non_negative(key: &str, default: f64) -> Result<f64> {
    let value: f64 = env_num(key, default)?;
    if value < 0.0 || !value.is_finite() {
        bail!("{key}={value} must be a non-negative number");
    }
    Ok(value)
}


pub const MAX_RUNTIME_BUY_SOL: f64 = 100.0;

static BUY_LAMPORTS: LazyLock<std::sync::atomic::AtomicU64> =
    LazyLock::new(|| std::sync::atomic::AtomicU64::new(TRADE_SETTING.buy_lamports));


static ENV_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_env_file(path: std::path::PathBuf) {
    let _ = ENV_FILE.set(path);
}


pub fn buy_lamports() -> u64 {
    BUY_LAMPORTS.load(std::sync::atomic::Ordering::Relaxed)
}


#[derive(Debug, Clone, serde::Serialize)]
pub struct BuySizeChange {
    pub previous_sol: f64,
    pub buy_sol: f64,
    
    pub saved_to_env: bool,
}



pub fn set_buy_sol(sol: f64) -> Result<BuySizeChange> {
    if !sol.is_finite() || sol <= 0.0 {
        bail!("buy size must be a positive number of SOL");
    }
    if sol > MAX_RUNTIME_BUY_SOL {
        bail!("buy size {sol} SOL is above the {MAX_RUNTIME_BUY_SOL} SOL safety limit; edit BUY_SOL_AMOUNT in .env to go higher");
    }
    let lamports = sol_to_lamports(sol);
    if lamports == 0 {
        bail!("buy size {sol} SOL is less than one lamport");
    }
    let previous = BUY_LAMPORTS.swap(lamports, std::sync::atomic::Ordering::Relaxed);
    let saved_to_env = match ENV_FILE.get() {
        Some(path) => match update_env_value(path, "BUY_SOL_AMOUNT", &lamports_to_sol(lamports).to_string()) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("buy size changed but could not be saved to {}: {e:#}", path.display());
                false
            }
        },
        None => false,
    };
    Ok(BuySizeChange { previous_sol: lamports_to_sol(previous), buy_sol: lamports_to_sol(lamports), saved_to_env })
}


fn update_env_value(path: &std::path::Path, key: &str, value: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let prefix = format!("{key}=");
    let mut replaced = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            if !replaced && line.trim_start().starts_with(&prefix) {
                replaced = true;
                format!("{key}={value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        lines.push(format!("{key}={value}"));
    }
    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_latency, update_env_value, PaperLatency, FRANKFURT_LANDING_MS};

    #[test]
    fn paper_landing_delays_parse() {
        let parse = |raw: Option<&str>| parse_latency("DRY_RUN_BUY_LATENCY_MS", raw.map(String::from)).unwrap();
        assert_eq!(parse(None), PaperLatency(FRANKFURT_LANDING_MS.to_vec()));
        assert_eq!(parse(Some("55")), PaperLatency(vec![55]));
        assert_eq!(parse(Some("30, 1_000")), PaperLatency(vec![30, 1000]));
        assert!(parse(Some("0")).is_off());
        assert_eq!(parse(Some("0")).draw(), None);
        let drawn = parse(None).draw().unwrap().as_millis() as u64;
        assert!(FRANKFURT_LANDING_MS.contains(&drawn));
        assert!(parse_latency("DRY_RUN_BUY_LATENCY_MS", Some("fast".into())).is_err());
    }

    #[test]
    fn env_value_is_replaced_in_place() {
        let dir = std::env::temp_dir().join(format!("sniper-env-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "# comment\nRPC_ENDPOINT=x?y=1\nBUY_SOL_AMOUNT=0.001\nSLIPPAGE=1\n").unwrap();
        update_env_value(&path, "BUY_SOL_AMOUNT", "0.05").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# comment\nRPC_ENDPOINT=x?y=1\nBUY_SOL_AMOUNT=0.05\nSLIPPAGE=1\n"
        );
        update_env_value(&path, "NEW_KEY", "1").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("SLIPPAGE=1\nNEW_KEY=1\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod hours_tests {
    use super::*;

    #[test]
    fn trading_hours_parse_and_wrap_past_midnight() {
        assert_eq!(parse_hours("01:00-07:00").unwrap(), (60, 420));
        let night = parse_hours("22:30-03:00").unwrap();
        assert!(in_window(night, 23 * 60) && in_window(night, 60) && in_window(night, 22 * 60 + 30));
        assert!(!in_window(night, 3 * 60) && !in_window(night, 12 * 60));
        let day = parse_hours("01:00-07:00").unwrap();
        assert!(in_window(day, 60) && in_window(day, 419) && !in_window(day, 420) && !in_window(day, 59));
        assert!(parse_hours("7-9").is_err() && parse_hours("01:00-01:00").is_err() && parse_hours("25:00-01:00").is_err());
    }
}
