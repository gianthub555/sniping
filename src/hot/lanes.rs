//! The send lanes. Every lane gets the same signed bytes; they fire in this order (B2), all
//! within a fraction of a millisecond:
//!
//! 1. **Staked gateways**, all at once: Helius Sender in SWQoS-only mode, Everstake SWQoS,
//!    AllenHark Relay (QUIC), Triton Jet. Each forwards over its own staked connection to the
//!    leader.
//! 2. **0slot** (`de1`) and **Nozomi** (`fra2`) plain sends.
//! 3. **Jito** bundle-only, as insurance.
//! 4. **Direct QUIC** to the TPU of the current and next leader from an unstaked identity: the
//!    last fallback.
//!
//! Plain-HTTP lanes (Helius, Everstake, 0slot, Nozomi; Triton when its URL is http) are written
//! by the decision thread onto warm keep-alive sockets: a pre-formatted request, no allocation,
//! one lock held for the write. HTTPS and QUIC lanes (AllenHark, Jito, Triton over https, direct
//! QUIC) run on their own threads, fed through rings. A `hot-lanes-io` thread (off the hot cores)
//! connects, reads and counts the HTTP answers, reconnects with back-off and pings idle
//! connections.
//!
//! Each lane comes from `LANE_<NAME>_ENABLED`, `_URL`, `_KEY`, `_TIP_LAMPORTS`. The buy carries
//! one tip transfer per enabled tipping lane (to one of its published accounts, rotated per
//! send), so a single transaction satisfies every lane. `LANE_SET=race` uses every enabled lane;
//! `LANE_SET=default` only those in `LANE_DEFAULT`.

use {
    super::{
        clock::now_ns,
        spsc,
        stamps::{self, LANES},
    },
    crate::config::{env_opt, TRADE_SETTING},
    log::{error, info, warn},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::{
        io::{Read, Write},
        net::{SocketAddr, TcpStream, ToSocketAddrs},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    },
};

// Lane indices: the firing order.
pub const LANE_HELIUS: usize = 0;
pub const LANE_EVERSTAKE: usize = 1;
pub const LANE_ALLENHARK: usize = 2;
pub const LANE_TRITON: usize = 3;
pub const LANE_ZEROSLOT: usize = 4;
pub const LANE_NOZOMI: usize = 5;
pub const LANE_JITO: usize = 6;
pub const LANE_QUIC: usize = 7;
pub const LANE_NAMES: [&str; LANES] = ["helius", "everstake", "allenhark", "triton", "0slot", "nozomi", "jito", "quic"];
/// The staked gateways (fired first, all at once).
pub const STAKED: [usize; 4] = [LANE_HELIUS, LANE_EVERSTAKE, LANE_ALLENHARK, LANE_TRITON];
/// `LANE_<NAME>_...` of each lane.
const ENV_NAMES: [&str; LANES] = ["HELIUS", "EVERSTAKE", "ALLENHARK", "TRITON", "ZEROSLOT", "NOZOMI", "JITO", "QUIC"];

/// AllenHark Relay's tip wallets (allenhark.com/docs/infrastructure/relay, 2026-10-07).
pub const ALLENHARK_TIP_ACCOUNTS: [Pubkey; 11] = [
    Pubkey::from_str_const("hark1zxc5Rz3K8Kquz79WPWFEgNCFeJnsMJ16f22uNP"),
    Pubkey::from_str_const("harkm2BTWxZuszoNpZnfe84jRbQTg6KGHaQBmWzDGQQ"),
    Pubkey::from_str_const("hark4CwtTnN2y9FaxjcFBAJdJqQrpouu5pgEixfqdEz"),
    Pubkey::from_str_const("harkoJfnM6dxrJydx5eVmDVwAgwC94KbhuxF69UbXwP"),
    Pubkey::from_str_const("hark6hUDUTekc1DGxWdJcuyDZwf6pJdCxd4SXAVtta6"),
    Pubkey::from_str_const("harkoTvFpKSrEQduYrNHXCurARVT19Ud3BnFhVxabos"),
    Pubkey::from_str_const("harkEpXoJv5qVzHaN7HSuUAd6PHjyMcFMcDYBMDJCEQ"),
    Pubkey::from_str_const("harkyXDdZSoJGyCxa24t2QXx1poPyp8YfghbtpzGSzK"),
    Pubkey::from_str_const("harkR2YJ4Dpt4UDJTcBirjnSPBhNpQFcoFkNpCkVqNk"),
    Pubkey::from_str_const("harkRBygM8pHYe4K8eBjfxyEX19oJn3LepFjvNbLbyi"),
    Pubkey::from_str_const("harkYFxB6DuUFNwDLvA5CQ66KpfRvFgUoVypMagNcmd"),
];

/// Tip accounts given in config (`LANE_<NAME>_TIP_ACCOUNTS`, comma-separated), per lane. Everstake's
/// come only from config (their RESOURCES.md); for the others config overrides the published lists
/// below. Read once.
static TIP_CONFIG: std::sync::LazyLock<[Option<&'static [Pubkey]>; LANES]> = std::sync::LazyLock::new(|| std::array::from_fn(config_tip_accounts));

/// `LANE_<NAME>_TIP_ACCOUNTS` of a lane, parsed (kept for the process's life).
fn config_tip_accounts(lane: usize) -> Option<&'static [Pubkey]> {
    let list = env_opt(&format!("LANE_{}_TIP_ACCOUNTS", ENV_NAMES[lane]))?;
    let keys: Vec<Pubkey> = list.split(',').map(str::trim).filter(|k| !k.is_empty()).filter_map(|k| k.parse().ok()).collect();
    (!keys.is_empty()).then(|| &*Box::leak(keys.into_boxed_slice()))
}

/// The published tip accounts in code (none for Everstake: config only).
fn default_tip_accounts(lane: usize) -> &'static [Pubkey] {
    use crate::config::ConfirmService::*;
    match lane {
        LANE_HELIUS => super::template::tip_accounts(Helius),
        LANE_ALLENHARK => &ALLENHARK_TIP_ACCOUNTS,
        LANE_ZEROSLOT => super::template::tip_accounts(ZeroSlot),
        LANE_NOZOMI => super::template::tip_accounts(Nozomi),
        LANE_JITO => super::template::tip_accounts(Jito),
        _ => &[],
    }
}

/// The smallest tip each provider accepts (their docs, 2026-10-07): a lane set below it is off.
pub const TIP_FLOOR: [u64; LANES] = [5_000, 1_000_000, 1_000_000, 0, 1_000_000, 1_000_000, 1_000, 0];

/// A lane's tip accounts as this process's config gives them (for recognizing rivals' tips; the
/// buy uses its `LaneSpec::tip_accounts`).
pub fn tip_accounts(lane: usize) -> &'static [Pubkey] {
    TIP_CONFIG[lane].unwrap_or_else(|| default_tip_accounts(lane))
}

/// The slot the early feeds last saw (the QUIC lane aims at its leader and the next one).
pub static FEED_SLOT: AtomicU64 = AtomicU64::new(0);

/// The Jito bundle id of each coin's last copy (`x-bundle-id`), the one way to tell which lane
/// landed a buy: every lane carries the same bytes, but Jito can say whether its bundle landed.
pub static JITO_BUNDLES: std::sync::LazyLock<Mutex<std::collections::HashMap<[u8; 32], String>>> = std::sync::LazyLock::new(Default::default);

/// Whether the Jito bundle of `mint`'s buy landed in `slot`: Some(true) when Jito says so,
/// Some(false) when it landed elsewhere or not at all, None when Jito cannot say.
pub async fn jito_landed(mint: &[u8; 32], slot: u64) -> Option<bool> {
    let id = JITO_BUNDLES.lock().unwrap_or_else(|e| e.into_inner()).get(mint).cloned()?;
    let base = TRADE_SETTING.jito_block_engine_url.clone()?;
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "getBundleStatuses", "params": [[id]]});
    let reply: serde_json::Value = crate::config::HTTP_CLIENT.post(format!("{}/api/v1/getBundleStatuses", base.trim_end_matches('/'))).json(&body).timeout(Duration::from_secs(5)).send().await.ok()?.json().await.ok()?;
    let status = &reply["result"]["value"][0];
    Some(status.is_object() && status["slot"].as_u64() == Some(slot))
}

/// One lane's settings.
#[derive(Clone, Debug, Default)]
pub struct LaneSpec {
    /// On in the configuration (`LANE_<NAME>_ENABLED`) and usable (it has what it needs).
    pub enabled: bool,
    /// Why it is off, when it is.
    pub off_because: Option<String>,
    pub url: Option<String>,
    pub key: Option<String>,
    /// Tip inside the transaction (lamports; 0: none), and the accounts it goes to (one per send,
    /// rotated): `LANE_<NAME>_TIP_ACCOUNTS`, else the published list.
    pub tip: u64,
    pub tip_accounts: &'static [Pubkey],
}

/// Lane settings (env).
#[derive(Clone, Debug)]
pub struct LaneConfig {
    pub lanes: [LaneSpec; LANES],
    /// "race" (every enabled lane) or "default" (`LANE_DEFAULT`).
    pub set: String,
    pub jito_uuid: Option<String>,
    pub quic_identity: Option<String>,
    /// Priority fee of a buy with no competition seen, and the cap on tips plus priority fee.
    pub fee_default: u64,
    pub fee_cap: u64,
    /// Bid this much (basis points) over the highest rival seen on the coin.
    pub outbid_bps: u64,
    /// DRY_RUN: send an unlandable copy (all-zero blockhash) of each decided buy through the
    /// lanes, to time them on the real connections without spending anything.
    pub dry_lanes: bool,
    /// LANE_ROTATE: each buy goes through one lane, the lanes that are on taking turns in
    /// `ROTATION` order, and pays only that lane's tip, so the lane that landed it is known.
    pub rotate: bool,
    /// LANE_ROTATE_LANES: the lanes that take turns (names; empty: every lane that is on). A lane
    /// left out still carries copies the fee engine asks for (the Jito bundle copy).
    pub rotate_lanes: [bool; LANES],
}

/// Tests (`hot::sell`): the lanes `on` with their tips, plain HTTP to a closed local port (nothing
/// connects, nothing is sent), the race set, no rotation.
#[cfg(test)]
pub(crate) fn test_config(on: &[(usize, u64)]) -> LaneConfig {
    let mut lanes: [LaneSpec; LANES] = Default::default();
    for &(i, tip) in on {
        lanes[i] = LaneSpec { enabled: true, url: Some("http://127.0.0.1:1/".into()), tip, tip_accounts: default_tip_accounts(i), ..Default::default() };
    }
    LaneConfig { lanes, set: "race".into(), jito_uuid: None, quic_identity: None, fee_default: 700_000, fee_cap: 5_000_000, outbid_bps: 1_000, dry_lanes: false, rotate: false, rotate_lanes: [false; LANES] }
}

/// The order lanes take turns in with LANE_ROTATE.
pub const ROTATION: [usize; LANES] = [LANE_ZEROSLOT, LANE_EVERSTAKE, LANE_HELIUS, LANE_ALLENHARK, LANE_TRITON, LANE_NOZOMI, LANE_JITO, LANE_QUIC];

fn sol(key: &str, default: f64) -> u64 {
    (env_opt(key).and_then(|v| v.parse::<f64>().ok()).unwrap_or(default) * 1e9).round() as u64
}

fn flag(key: &str) -> Option<bool> {
    env_opt(key).map(|v| v.eq_ignore_ascii_case("true"))
}

/// The plain-HTTP or HTTPS form of a lane URL, and whether this build can send on it.
fn usable_url(lane: usize, url: &str) -> bool {
    match lane {
        // Direct QUIC aims at the leaders' TPUs from the schedule: no URL.
        LANE_QUIC => true,
        LANE_EVERSTAKE => url.starts_with("http://") || url.starts_with("https://"),
        LANE_ALLENHARK => url.starts_with("quic://") || url.starts_with("https://"),
        LANE_JITO | LANE_TRITON => url.starts_with("https://") || parse_http(url).is_some(),
        _ => parse_http(url).is_some(),
    }
}

impl LaneConfig {
    pub fn from_env() -> Self {
        let s = &*TRADE_SETTING;
        let var = |lane: usize, what: &str| env_opt(&format!("LANE_{}_{what}", ENV_NAMES[lane]));
        let tip = |lane: usize, default: u64| var(lane, "TIP_LAMPORTS").and_then(|v| v.parse().ok()).unwrap_or(default);
        let mut lanes: [LaneSpec; LANES] = Default::default();

        // Helius Sender, SWQoS-only: the Frankfurt endpoint (plain HTTP, as Helius documents its
        // regional ones), 0.000005 SOL; a Helius API key raises the rate limit (1/s without).
        let helius_key = var(LANE_HELIUS, "KEY");
        let helius_url = var(LANE_HELIUS, "URL").unwrap_or_else(|| "http://fra-sender.helius-rpc.com/fast?swqos_only=true".into());
        lanes[LANE_HELIUS] = LaneSpec {
            enabled: flag("LANE_HELIUS_ENABLED").unwrap_or(true),
            url: Some(match &helius_key {
                Some(k) => format!("{helius_url}{}api-key={k}", if helius_url.contains('?') { '&' } else { '?' }),
                None => helius_url,
            }),
            key: helius_key,
            tip: tip(LANE_HELIUS, 5_000),
            ..Default::default()
        };
        // Everstake SWQoS pay-as-you-go: no key on the RPC path; one persistent HTTP/2 connection
        // (h2c on their plain-HTTP port, which they offer), at most 10 sends a second.
        lanes[LANE_EVERSTAKE] = LaneSpec {
            enabled: flag("LANE_EVERSTAKE_ENABLED").unwrap_or(false),
            url: Some(var(LANE_EVERSTAKE, "URL").unwrap_or_else(|| "http://fra-swqos.everstake.one".into())),
            key: var(LANE_EVERSTAKE, "KEY"),
            tip: tip(LANE_EVERSTAKE, 1_000_000),
            ..Default::default()
        };
        // AllenHark Relay over QUIC (Frankfurt), its API key on the stream's first line.
        let allenhark_key = var(LANE_ALLENHARK, "KEY");
        lanes[LANE_ALLENHARK] = LaneSpec {
            enabled: flag("LANE_ALLENHARK_ENABLED").unwrap_or(allenhark_key.is_some()),
            url: Some(var(LANE_ALLENHARK, "URL").unwrap_or_else(|| "quic://fra.relay.allenhark.com:4433".into())),
            key: allenhark_key,
            tip: tip(LANE_ALLENHARK, 1_000_000),
            ..Default::default()
        };
        // Triton Jet: sendTransaction on a Triton endpoint (its token in the URL); no tip.
        lanes[LANE_TRITON] = LaneSpec {
            enabled: flag("LANE_TRITON_ENABLED").unwrap_or(false),
            url: var(LANE_TRITON, "URL"),
            key: var(LANE_TRITON, "KEY"),
            tip: tip(LANE_TRITON, 0),
            ..Default::default()
        };
        let nozomi_base = var(LANE_NOZOMI, "URL").or_else(|| env_opt("HOT_NOZOMI_URL")).unwrap_or_else(|| "http://fra2.nozomi.temporal.xyz".into());
        let nozomi_key = var(LANE_NOZOMI, "KEY").or_else(|| env_opt("NOZOMI_API_KEY"));
        lanes[LANE_ZEROSLOT] = LaneSpec {
            enabled: flag("LANE_ZEROSLOT_ENABLED").unwrap_or(true),
            url: var(LANE_ZEROSLOT, "URL").or_else(|| env_opt("HOT_ZEROSLOT_URL")).or_else(|| s.zero_slot_api_url.clone()),
            key: None,
            tip: tip(LANE_ZEROSLOT, sol("HOT_TIP_ZEROSLOT_SOL", 0.001)),
            ..Default::default()
        };
        lanes[LANE_NOZOMI] = LaneSpec {
            enabled: flag("LANE_NOZOMI_ENABLED").unwrap_or(true),
            url: nozomi_key.as_ref().map(|k| format!("{}/api/sendTransaction2?c={k}", nozomi_base.trim_end_matches('/'))),
            key: nozomi_key,
            tip: tip(LANE_NOZOMI, sol("HOT_TIP_NOZOMI_SOL", 0.001)),
            ..Default::default()
        };
        lanes[LANE_JITO] = LaneSpec {
            enabled: flag("LANE_JITO_ENABLED").unwrap_or(true),
            url: var(LANE_JITO, "URL")
                .or_else(|| s.jito_block_engine_url.clone())
                .map(|u| format!("{}/api/v1/bundles", u.trim_end_matches('/'))),
            key: var(LANE_JITO, "KEY").or_else(|| s.jito_uuid.clone()),
            tip: tip(LANE_JITO, sol("HOT_TIP_JITO_SOL", 0.0001)),
            ..Default::default()
        };
        lanes[LANE_QUIC] = LaneSpec { enabled: flag("LANE_QUIC_ENABLED").unwrap_or(true), url: Some("tpu".into()), ..Default::default() };

        for (i, l) in lanes.iter_mut().enumerate() {
            l.tip_accounts = config_tip_accounts(i).unwrap_or_else(|| default_tip_accounts(i));
        }
        settle(&mut lanes);
        // LANE_SET=default: only the lanes in LANE_DEFAULT.
        let set = env_opt("LANE_SET").unwrap_or_else(|| "race".into()).to_lowercase();
        if set == "default" {
            match env_opt("LANE_DEFAULT") {
                Some(list) => {
                    let chosen: Vec<String> = list.split(',').map(|w| w.trim().to_lowercase()).collect();
                    for (i, l) in lanes.iter_mut().enumerate() {
                        if l.enabled && !chosen.iter().any(|c| c == LANE_NAMES[i]) {
                            l.enabled = false;
                            l.off_because = Some("not in LANE_DEFAULT".into());
                        }
                    }
                }
                None => warn!("LANE_SET=default but no LANE_DEFAULT: every enabled lane is used (as LANE_SET=race)"),
            }
        }
        if env_opt("HOT_LANES").is_some() {
            warn!("HOT_LANES is no longer read: each lane has LANE_<NAME>_ENABLED");
        }
        LaneConfig {
            lanes,
            set,
            jito_uuid: s.jito_uuid.clone(),
            quic_identity: env_opt("QUIC_IDENTITY_KEYPAIR"),
            fee_default: sol("BUY_PRIORITY_FEE_SOL", 0.0007),
            fee_cap: sol("BUY_FEE_CAP_SOL", 0.005),
            outbid_bps: env_opt("HOT_OUTBID_BPS").and_then(|v| v.parse().ok()).unwrap_or(1_000),
            dry_lanes: env_opt("HOT_DRY_LANES").is_some_and(|v| v.eq_ignore_ascii_case("true")),
            rotate: env_opt("LANE_ROTATE").is_some_and(|v| v.eq_ignore_ascii_case("true")),
            rotate_lanes: match env_opt("LANE_ROTATE_LANES").filter(|v| !v.trim().is_empty()) {
                Some(v) => {
                    let names: Vec<String> = v.split(',').map(|n| n.trim().to_ascii_lowercase()).collect();
                    std::array::from_fn(|i| names.iter().any(|n| n == LANE_NAMES[i]))
                }
                None => [true; LANES],
            },
        }
    }

    pub fn enabled(&self, lane: usize) -> bool {
        self.lanes[lane].enabled
    }

    /// The tips the buy carries: one transfer per enabled tipping lane, in firing order.
    pub fn tips(&self) -> Vec<(&'static [Pubkey], u64)> {
        self.lanes.iter().filter(|l| l.enabled && l.tip > 0 && !l.tip_accounts.is_empty()).map(|l| (l.tip_accounts, l.tip)).collect()
    }

    pub fn tips_total(&self) -> u64 {
        self.tips().iter().map(|(_, l)| l).sum()
    }

    /// Where `lane`'s tip sits among `tips()` (None: it tips nothing). No allocation.
    pub fn tip_index(&self, lane: usize) -> Option<u8> {
        let tips = |l: &LaneSpec| l.enabled && l.tip > 0 && !l.tip_accounts.is_empty();
        tips(&self.lanes[lane]).then(|| self.lanes[..lane].iter().filter(|l| tips(l)).count() as u8)
    }

    /// One line per lane for the startup log (no keys, no URLs with keys).
    pub fn describe(&self) -> Vec<String> {
        (0..LANES)
            .map(|i| {
                let l = &self.lanes[i];
                match &l.off_because {
                    None => format!("{}: on, tip {} SOL", LANE_NAMES[i], l.tip as f64 / 1e9),
                    Some(why) => format!("{}: off ({why})", LANE_NAMES[i]),
                }
            })
            .collect()
    }
}

/// Turns off every lane that lacks what it needs (its switch, a usable URL, AllenHark's key, tip
/// accounts, a tip at or above the provider's floor), saying why, so the buy never carries the tip
/// of a lane that cannot send.
pub fn settle(lanes: &mut [LaneSpec; LANES]) {
    for (i, l) in lanes.iter_mut().enumerate() {
        if !l.enabled && i == LANE_ALLENHARK && l.key.is_none() {
            l.off_because = Some("no LANE_ALLENHARK_KEY".into());
        } else if !l.enabled {
            l.off_because = Some(format!("LANE_{}_ENABLED is not true", ENV_NAMES[i]));
        } else if l.url.as_deref().is_none_or(|u| !usable_url(i, u)) {
            l.enabled = false;
            l.off_because = Some(format!("no usable LANE_{}_URL", ENV_NAMES[i]));
        } else if i == LANE_ALLENHARK && l.key.is_none() {
            l.enabled = false;
            l.off_because = Some("no LANE_ALLENHARK_KEY".into());
        } else if TIP_FLOOR[i] > 0 && l.tip_accounts.is_empty() {
            l.enabled = false;
            l.off_because = Some(format!("no LANE_{}_TIP_ACCOUNTS", ENV_NAMES[i]));
        } else if l.tip < TIP_FLOOR[i] {
            // Below the provider's floor the send is refused: off, and said so, rather than
            // quietly raising the fee.
            l.enabled = false;
            l.off_because = Some(format!("LANE_{}_TIP_LAMPORTS {} is below the provider's minimum of {}", ENV_NAMES[i], l.tip, TIP_FLOOR[i]));
        }
    }
}

/// The priority bid for a buy (B2): the default, or over the highest rival bid seen on the coin by
/// `outbid_bps`, never past the cap on tips plus priority fee. Micro-lamports per compute unit,
/// and whether the competition raised it.
pub fn bid(cfg: &LaneConfig, cu_limit: u32, rival_max: u64) -> (u64, bool) {
    let cu = cu_limit.max(1) as u128;
    let default = (cfg.fee_default as u128 * 1_000_000 / cu) as u64;
    let cap = (cfg.fee_cap.saturating_sub(cfg.tips_total()) as u128 * 1_000_000 / cu) as u64;
    let over = (rival_max as u128 * (10_000 + cfg.outbid_bps) as u128 / 10_000) as u64 + 1;
    let wanted = if rival_max > 0 && over > default { over } else { default };
    (wanted.min(cap), rival_max > 0 && over > default)
}

// ---- plain HTTP lanes ------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Body {
    /// Nozomi API v2: the base64 transaction as the body.
    Text,
    /// JSON-RPC sendTransaction (0slot).
    JsonRpc,
}

/// One keep-alive connection: the socket behind a lock (the decision thread writes buys, the I/O
/// thread writes pings and reconnects), and the write times of the requests still unanswered
/// (HTTP/1.1 answers in order), so each answer gives a round trip.
struct Conn {
    state: Mutex<ConnState>,
    last_write: AtomicU64,
}

const INFLIGHT: usize = 32;

struct ConnState {
    stream: Option<TcpStream>,
    /// (write time, was a ping), oldest first, a ring.
    inflight: [(u64, bool); INFLIGHT],
    head: usize,
    len: usize,
}

impl ConnState {
    fn new() -> Self {
        ConnState { stream: None, inflight: [(0, false); INFLIGHT], head: 0, len: 0 }
    }
    fn push(&mut self, t: u64, ping: bool) {
        if self.len == INFLIGHT {
            // Answers stopped coming: forget the oldest.
            self.head = (self.head + 1) % INFLIGHT;
            self.len -= 1;
        }
        self.inflight[(self.head + self.len) % INFLIGHT] = (t, ping);
        self.len += 1;
    }
    fn pop(&mut self) -> Option<(u64, bool)> {
        (self.len > 0).then(|| {
            let e = self.inflight[self.head];
            self.head = (self.head + 1) % INFLIGHT;
            self.len -= 1;
            e
        })
    }
    fn reset(&mut self, stream: Option<TcpStream>) {
        self.stream = stream;
        self.head = 0;
        self.len = 0;
    }
}

impl Conn {
    fn new() -> Self {
        Conn { state: Mutex::new(ConnState::new()), last_write: AtomicU64::new(0) }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, ConnState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct HttpTarget {
    name: &'static str,
    host: String,
    port: u16,
    path: String,
    body: Body,
    ping: String,
    /// Ping a connection after this long without a write.
    ping_every_ns: u64,
}

fn parse_http(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (hostport.to_string(), 80),
    };
    Some((host, port, path))
}

/// A plain-HTTP lane, written from the decision thread.
pub struct HttpLane {
    target: Arc<HttpTarget>,
    conns: Arc<[Conn; 2]>,
    next: usize,
    buf: Vec<u8>,
    /// The provider's per-second limit on transactions (0: none) and the lane it counts for.
    limit: usize,
    lane: usize,
    /// The write times of the last `limit` transactions, oldest first, a ring.
    recent: [u64; LIMIT_MAX],
    recent_head: usize,
    recent_len: usize,
}

/// The largest per-second limit a plain-HTTP lane keeps count of.
const LIMIT_MAX: usize = 16;

const JSON_HEAD: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":[""#;
const JSON_TAIL: &[u8] = br#"",{"encoding":"base64","skipPreflight":true,"maxRetries":0}]}"#;

fn push_number(buf: &mut Vec<u8>, mut n: usize) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&digits[i..]);
}

impl HttpLane {
    fn new(target: HttpTarget) -> Self {
        HttpLane {
            target: Arc::new(target),
            conns: Arc::new([Conn::new(), Conn::new()]),
            next: 0,
            buf: Vec::with_capacity(4096),
            limit: 0,
            lane: 0,
            recent: [0; LIMIT_MAX],
            recent_head: 0,
            recent_len: 0,
        }
    }

    /// At most `per_sec` transactions in any second on `lane` (at most `LIMIT_MAX`); a copy over
    /// it is not written and counts in `counters::LIMITED`, as on the threaded lanes.
    fn with_limit(mut self, lane: usize, per_sec: usize) -> Self {
        self.lane = lane;
        self.limit = per_sec.min(LIMIT_MAX);
        self
    }

    /// Room under the per-second limit now (and the window's old writes forgotten).
    #[inline]
    fn room(&mut self, now: u64) -> bool {
        if self.limit == 0 {
            return true;
        }
        while self.recent_len > 0 && now.saturating_sub(self.recent[self.recent_head]) >= 1_000_000_000 {
            self.recent_head = (self.recent_head + 1) % LIMIT_MAX;
            self.recent_len -= 1;
        }
        self.recent_len < self.limit
    }

    /// Writes one request carrying `b64`. Never allocates (the buffer is reserved). The write's
    /// clock reading when a connection took the bytes.
    #[inline]
    pub fn send(&mut self, b64: &[u8]) -> Option<u64> {
        if !self.room(now_ns()) {
            counters::LIMITED[self.lane].fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let t = &*self.target;
        let body_len = match t.body {
            Body::Text => b64.len(),
            Body::JsonRpc => JSON_HEAD.len() + b64.len() + JSON_TAIL.len(),
        };
        self.buf.clear();
        self.buf.extend_from_slice(b"POST ");
        self.buf.extend_from_slice(t.path.as_bytes());
        self.buf.extend_from_slice(b" HTTP/1.1\r\nHost: ");
        self.buf.extend_from_slice(t.host.as_bytes());
        self.buf.extend_from_slice(match t.body {
            Body::Text => b"\r\nContent-Type: text/plain\r\nContent-Length: ".as_slice(),
            Body::JsonRpc => b"\r\nContent-Type: application/json\r\nContent-Length: ".as_slice(),
        });
        push_number(&mut self.buf, body_len);
        self.buf.extend_from_slice(b"\r\n\r\n");
        if t.body == Body::JsonRpc {
            self.buf.extend_from_slice(JSON_HEAD);
        }
        self.buf.extend_from_slice(b64);
        if t.body == Body::JsonRpc {
            self.buf.extend_from_slice(JSON_TAIL);
        }
        for attempt in 0..2 {
            let conn = &self.conns[(self.next + attempt) % 2];
            let mut st = conn.lock();
            if let Some(s) = st.stream.as_mut() {
                // Non-blocking socket: a full send buffer fails the write instead of stalling the
                // decision thread, and the connection is dropped (a partial request poisons it).
                if s.write_all(&self.buf).is_ok() {
                    let t = now_ns();
                    st.push(t, false);
                    conn.last_write.store(t, Ordering::Relaxed);
                    self.next = (self.next + attempt + 1) % 2;
                    if self.limit > 0 {
                        if self.recent_len == LIMIT_MAX {
                            self.recent_head = (self.recent_head + 1) % LIMIT_MAX;
                            self.recent_len -= 1;
                        }
                        self.recent[(self.recent_head + self.recent_len) % LIMIT_MAX] = t;
                        self.recent_len += 1;
                    }
                    return Some(t);
                }
                // The I/O thread reconnects it (and counts its unanswered requests as lost).
                st.stream = None;
            }
        }
        None
    }
}

/// Responses and refusals per lane, for the report.
pub mod counters {
    use {
        super::LANES,
        std::sync::atomic::{AtomicBool, AtomicU64},
    };
    pub static OK: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
    pub static REFUSED: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
    pub static RECONNECTS: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
    pub static SENT: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
    /// Transaction answers in a row refused for authorization (HTTP 401/403), per lane.
    pub static AUTH_REFUSED_IN_A_ROW: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
    /// A lane whose key the service no longer accepts (`AUTH_DEAD_AFTER` such answers in a row):
    /// out of the rotation and every send until the bot restarts.
    pub static DEAD: [AtomicBool; LANES] = [const { AtomicBool::new(false) }; LANES];
    /// 0slot answered every transaction "api-key does not exist" (HTTP 403) for hours on 14 Oct
    /// after its key was rotated, while the rotation kept giving it every third buy.
    pub const AUTH_DEAD_AFTER: u64 = 3;

    /// A transaction answer (not a ping) from `lane`: true when it just made the lane dead.
    pub fn note_tx_answer(lane: usize, status: u16, bad: bool) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if bad && (status == 401 || status == 403) {
            let n = AUTH_REFUSED_IN_A_ROW[lane].fetch_add(1, Relaxed) + 1;
            n >= AUTH_DEAD_AFTER && !DEAD[lane].swap(true, Relaxed)
        } else {
            AUTH_REFUSED_IN_A_ROW[lane].store(0, Relaxed);
            false
        }
    }
    /// Connections open now (HTTP lanes, out of 2; QUIC, leaders).
    pub static WARM: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
    /// Lanes started.
    pub static ON: [AtomicBool; LANES] = [const { AtomicBool::new(false) }; LANES];
    /// Copies not sent on a lane because it was at its provider's per-second limit.
    pub static LIMITED: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
    /// Sends the provider refused for its rate limit (HTTP 429 or a "rate limit" error).
    pub static RATE_LIMITED: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
}

/// Splits complete HTTP responses off `buf`: (status, body). Content-Length or chunked.
fn take_response(buf: &mut Vec<u8>) -> Option<(u16, String)> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let lower = head.to_lowercase();
    let length = lower.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok());
    let (body_end, body) = if let Some(n) = length {
        if buf.len() < end + n {
            return None;
        }
        (end + n, String::from_utf8_lossy(&buf[end..end + n]).to_string())
    } else if lower.contains("transfer-encoding: chunked") {
        let rest = &buf[end..];
        let stop = rest.windows(5).position(|w| w == b"0\r\n\r\n")? + 5;
        (end + stop, String::from_utf8_lossy(&rest[..stop]).to_string())
    } else {
        (end, String::new())
    };
    buf.drain(..body_end);
    Some((status, body))
}

/// The I/O thread of the HTTP lanes: connect, read answers as they come (poll), reconnect with
/// back-off, ping when idle.
fn run_http_io(lanes: Vec<(usize, Arc<HttpTarget>, Arc<[Conn; 2]>)>) {
    use std::os::fd::AsRawFd;
    super::affinity::keep_off_hot_cores();
    struct Reader {
        stream: Option<TcpStream>,
        buf: Vec<u8>,
        failures: u32,
        retry_at: u64,
    }
    let mut readers: Vec<Vec<Reader>> = lanes.iter().map(|_| (0..2).map(|_| Reader { stream: None, buf: Vec::new(), failures: 0, retry_at: 0 }).collect()).collect();
    let mut chunk = vec![0u8; 16_384];
    let mut fds: Vec<libc::pollfd> = Vec::new();
    loop {
        let now = now_ns();
        // Connect what is down.
        for (li, (lane, target, conns)) in lanes.iter().enumerate() {
            for (ci, conn) in conns.iter().enumerate() {
                let r = &mut readers[li][ci];
                if conn.lock().stream.is_some() && r.stream.is_some() {
                    continue;
                }
                r.stream = None;
                // Requests still unanswered on the closed connection got no answer.
                {
                    let mut st = conn.lock();
                    st.stream = None;
                    while let Some((t, ping)) = st.pop() {
                        stamps::answers::push(stamps::answers::Answer { lane: *lane, ping, conn: ci as u8, t_sent: t, t_answer: 0, status: "lost" });
                    }
                }
                if now < r.retry_at {
                    continue;
                }
                let addr = (target.host.as_str(), target.port).to_socket_addrs().ok().and_then(|mut a| a.next());
                match addr.map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(2))) {
                    Some(Ok(s)) => {
                        let _ = s.set_nodelay(true);
                        // The clone shares the socket (and its non-blocking flag).
                        let _ = s.set_nonblocking(true);
                        r.stream = s.try_clone().ok();
                        r.buf.clear();
                        r.failures = 0;
                        conn.lock().reset(Some(s));
                        conn.last_write.store(now_ns(), Ordering::Relaxed);
                        counters::RECONNECTS[*lane].fetch_add(1, Ordering::Relaxed);
                    }
                    other => {
                        r.failures += 1;
                        // 200 ms, 400 ms, 800 ms ... at most 30 s (C3).
                        let ms = (100u64 << r.failures.min(9)).min(30_000);
                        r.retry_at = now + ms * 1_000_000;
                        if r.failures == 1 || r.failures % 30 == 0 {
                            warn!("hot lane {}: connect failed ({} in a row): {:?}", target.name, r.failures, other.map(|e| e.err()));
                        }
                    }
                }
            }
        }
        let warm: Vec<(usize, u64)> = lanes.iter().enumerate().map(|(li, (lane, _, _))| (*lane, readers[li].iter().filter(|r| r.stream.is_some()).count() as u64)).collect();
        for (lane, n) in warm {
            counters::WARM[lane].store(n, Ordering::Relaxed);
        }
        // Wait up to 5 ms for answers.
        fds.clear();
        for rs in &readers {
            for r in rs {
                fds.push(libc::pollfd { fd: r.stream.as_ref().map_or(-1, |s| s.as_raw_fd()), events: libc::POLLIN, revents: 0 });
            }
        }
        // SAFETY: `fds` is a valid array of pollfd for its length; negative fds are ignored.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 5) };
        let t_read = now_ns();
        let mut k = 0;
        for (li, (lane, target, conns)) in lanes.iter().enumerate() {
            for (ci, conn) in conns.iter().enumerate() {
                let ready = fds[k].revents != 0;
                k += 1;
                let r = &mut readers[li][ci];
                let Some(rs) = r.stream.as_mut() else { continue };
                if ready {
                    loop {
                        match rs.read(&mut chunk) {
                            Ok(0) => {
                                conn.lock().stream = None;
                                r.stream = None;
                                break;
                            }
                            Ok(n) => r.buf.extend_from_slice(&chunk[..n]),
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => {
                                conn.lock().stream = None;
                                r.stream = None;
                                break;
                            }
                        }
                    }
                    while let Some((status, body)) = take_response(&mut r.buf) {
                        let sent = conn.lock().pop();
                        let ping = sent.is_some_and(|(_, p)| p);
                        let bad = !(200..300).contains(&status) || body.contains("\"error\"");
                        if let Some((t, _)) = sent {
                            let rtt = t_read.saturating_sub(t);
                            if ping {
                                stamps::rtt::PING[*lane].record(rtt);
                            } else {
                                stamps::rtt::TX[*lane].record(rtt);
                            }
                            let status = if bad { "refused" } else { "ok" };
                            stamps::answers::push(stamps::answers::Answer { lane: *lane, ping, conn: ci as u8, t_sent: t, t_answer: t_read, status });
                        }
                        if ping {
                            continue;
                        }
                        if counters::note_tx_answer(*lane, status as u16, bad) {
                            error!(
                                "hot lane {}: {} transactions in a row refused for authorization (HTTP {status}): the lane is out of the rotation and every send until the bot restarts. Fix its key or set LANE_{}_ENABLED=false; without LANE_ROTATE each buy still carries its tip",
                                target.name,
                                counters::AUTH_DEAD_AFTER,
                                target.name.to_ascii_uppercase()
                            );
                        }
                        if bad {
                            let k = counters::REFUSED[*lane].fetch_add(1, Ordering::Relaxed);
                            if k < 5 || k % 100 == 0 {
                                warn!("hot lane {}: refused (HTTP {status}): {}", target.name, body.chars().take(200).collect::<String>());
                            }
                        } else {
                            counters::OK[*lane].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                // Keep it warm: a ping after the lane's idle interval.
                if r.stream.is_some() && t_read.saturating_sub(conn.last_write.load(Ordering::Relaxed)) > target.ping_every_ns {
                    let ping = format!("GET {} HTTP/1.1\r\nHost: {}\r\n\r\n", target.ping, target.host);
                    let mut st = conn.lock();
                    let ok = st.stream.as_mut().is_some_and(|s| s.write_all(ping.as_bytes()).is_ok());
                    if ok {
                        st.push(t_read, true);
                    } else {
                        st.stream = None;
                    }
                    conn.last_write.store(t_read, Ordering::Relaxed);
                }
            }
        }
    }
}

// ---- ring lanes (QUIC, Jito) ------------------------------------------------------------------

/// A signed transaction handed to a lane thread.
#[derive(Clone, Copy)]
pub struct TxMsg {
    pub mint: [u8; 32],
    pub len: u16,
    pub bytes: [u8; super::template::PACKET],
    /// When the hot thread handed it over (`now_ns`).
    pub t_queued: u64,
}

/// The producer side of a lane thread.
pub struct RingLane {
    tx: spsc::Producer<TxMsg>,
    wake: Arc<tokio::sync::Notify>,
    /// Until when (`now_ns`) the thread spins on its ring instead of sleeping (`prime`).
    awake_until: Arc<AtomicU64>,
}

/// How long a primed lane thread stays awake: a create's bundle window (0.25 ms) and decision,
/// with room to spare.
pub const PRIME_NS: u64 = 3_000_000;

impl RingLane {
    /// Wakes the thread now and keeps it spinning on its ring until `until`, so the buy that
    /// follows a create is picked up at once instead of after a wake-up from sleep.
    #[inline]
    fn prime(&self, until: u64) {
        self.awake_until.fetch_max(until, Ordering::Relaxed);
        self.wake.notify_one();
    }

    #[inline]
    fn send(&mut self, mint: &[u8; 32], bytes: &[u8]) -> bool {
        let mut msg = TxMsg { mint: *mint, len: bytes.len() as u16, bytes: [0; super::template::PACKET], t_queued: now_ns() };
        msg.bytes[..bytes.len()].copy_from_slice(bytes);
        let ok = self.tx.push(msg);
        self.wake.notify_one();
        ok
    }
}

/// What an HTTPS lane posts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HttpsKind {
    /// Jito `sendTransaction` (bundle-only by the URL): `[b64, {"encoding":"base64"}]`.
    Jito,
    /// Standard `sendTransaction` with `skipPreflight` and `maxRetries: 0` (Triton).
    JsonRpc,
    /// AllenHark Relay `/v1/sendTx`: `{"tx": b64, "simulate": false}`.
    AllenHark,
}

/// An HTTPS lane's thread: one warm client, a keep-alive request while idle, each transaction
/// posted as it comes off the ring, every answer timed and counted.
#[allow(clippy::too_many_arguments)]
fn https_thread(
    lane: usize,
    kind: HttpsKind,
    url: String,
    header: Option<(&'static str, String)>,
    ping: (reqwest::Method, String, Option<serde_json::Value>),
    ping_every: Duration,
    h2: bool,
    max_per_sec: usize,
    mut rx: spsc::Consumer<TxMsg>,
    wake: Arc<tokio::sync::Notify>,
    awake_until: Arc<AtomicU64>,
) {
    super::affinity::pin_lane_thread();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("https lane runtime");
    let name = LANE_NAMES[lane];
    rt.block_on(async move {
        let mut builder = reqwest::Client::builder().tcp_nodelay(true).pool_idle_timeout(Duration::from_secs(90));
        if h2 {
            // One persistent HTTP/2 connection, kept up with HTTP/2 pings: h2c (prior knowledge)
            // on a plain-HTTP URL, h2 over TLS on an https one.
            builder = builder.pool_max_idle_per_host(1).http2_keep_alive_interval(Duration::from_secs(10)).http2_keep_alive_while_idle(true);
            if url.starts_with("http://") {
                builder = builder.http2_prior_knowledge();
            }
        }
        let client = builder.build().expect("client");
        // The provider's limit: the times of the last `max_per_sec` requests (pings included).
        let mut recent: std::collections::VecDeque<u64> = std::collections::VecDeque::with_capacity(max_per_sec + 1);
        let mut room = move |now: u64| -> bool {
            if max_per_sec == 0 {
                return true;
            }
            while recent.front().is_some_and(|&t| now.saturating_sub(t) >= 1_000_000_000) {
                recent.pop_front();
            }
            if recent.len() >= max_per_sec {
                return false;
            }
            recent.push_back(now);
            true
        };
        let mut keepalive = tokio::time::interval(ping_every);
        let mut warm_failures = 0u32;
        loop {
            tokio::select! {
                _ = wake.notified() => {}
                _ = keepalive.tick() => {
                    let t = now_ns();
                    if !room(t) {
                        continue;
                    }
                    let mut req = client.request(ping.0.clone(), &ping.1);
                    if let Some(body) = &ping.2 {
                        req = req.json(body);
                    }
                    if let Some((h, v)) = &header {
                        req = req.header(*h, v);
                    }
                    match req.send().await {
                        Ok(_) => {
                            let ta = now_ns();
                            stamps::rtt::PING[lane].record(ta - t);
                            stamps::answers::push(stamps::answers::Answer { lane, ping: true, conn: 0, t_sent: t, t_answer: ta, status: "ok" });
                            counters::WARM[lane].store(1, Ordering::Relaxed);
                            warm_failures = 0;
                        }
                        Err(e) => {
                            counters::WARM[lane].store(0, Ordering::Relaxed);
                            warm_failures += 1;
                            if warm_failures == 1 || warm_failures % 20 == 0 {
                                warn!("hot lane {name}: keep-alive failed ({warm_failures} in a row): {}", e.without_url());
                            }
                        }
                    }
                    continue;
                }
            }
            loop {
                // While primed (a create is on its way to a buy), look again instead of sleeping.
                let Some(msg) = rx.pop() else {
                    if now_ns() < awake_until.load(Ordering::Relaxed) {
                        super::affinity::spin_pause();
                        tokio::task::yield_now().await;
                        continue;
                    }
                    break;
                };
                let t_pop = now_ns();
                stamps::rtt::PICKUP[lane].record(t_pop.saturating_sub(msg.t_queued));
                if !room(now_ns()) {
                    // Over the provider's per-second limit: this copy is not sent on this lane.
                    counters::LIMITED[lane].fetch_add(1, Ordering::Relaxed);
                    stamps::answers::push(stamps::answers::Answer { lane, ping: false, conn: 0, t_sent: now_ns(), t_answer: 0, status: "limited" });
                    continue;
                }
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&msg.bytes[..msg.len as usize]);
                let body = match kind {
                    // A one-transaction bundle: it lands whole or not at all (a failing buy costs
                    // nothing), its tip inside the buy.
                    HttpsKind::Jito => serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "sendBundle", "params": [[b64], {"encoding": "base64"}]}),
                    HttpsKind::JsonRpc => serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "sendTransaction", "params": [b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]}),
                    HttpsKind::AllenHark => serde_json::json!({"tx": b64, "simulate": false}),
                };
                let mut req = client.post(&url).json(&body);
                if let Some((h, v)) = &header {
                    req = req.header(*h, v);
                }
                let fut = req.send();
                counters::SENT[lane].fetch_add(1, Ordering::Relaxed);
                let mint = msg.mint;
                tokio::spawn(async move {
                    // The request's first poll writes it (a warm pooled connection): the lane's
                    // write time from here (2026-10-11; it was stamped when the request was built,
                    // before the task ran).
                    let t = now_ns();
                    stamps::record_lane(&mint, lane, t);
                    stamps::rtt::BUILD[lane].record(t.saturating_sub(t_pop));
                    let answer = fut.await;
                    let ta = now_ns();
                    stamps::rtt::TX[lane].record(ta - t);
                    let note = |status: &'static str| {
                        stamps::answers::push(stamps::answers::Answer { lane, ping: false, conn: 0, t_sent: t, t_answer: if status == "lost" { 0 } else { ta }, status })
                    };
                    let refused = |what: String| {
                        let k = counters::REFUSED[lane].fetch_add(1, Ordering::Relaxed);
                        if k < 5 || k % 100 == 0 {
                            warn!("hot lane {}: refused {}: {what}", LANE_NAMES[lane], bs58::encode(mint).into_string());
                        }
                    };
                    // A rate limit (HTTP 429 or a "rate limit" error): counted and logged apart.
                    let rate_limited = |text: &str| {
                        let k = counters::RATE_LIMITED[lane].fetch_add(1, Ordering::Relaxed);
                        if k < 20 || k % 50 == 0 {
                            warn!("hot lane {}: RATE LIMITED ({} so far) on {}: {}", LANE_NAMES[lane], k + 1, bs58::encode(mint).into_string(), text.chars().take(160).collect::<String>());
                        }
                    };
                    match answer {
                        Ok(r) if r.status().is_success() => {
                            let header_id = r.headers().get("x-bundle-id").and_then(|v| v.to_str().ok()).map(str::to_string);
                            let text = r.text().await.unwrap_or_default();
                            if kind == HttpsKind::Jito {
                                // sendBundle answers the bundle's id as its result.
                                let id = header_id.or_else(|| serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["result"].as_str().map(str::to_string)));
                                if let Some(id) = id {
                                    let mut b = JITO_BUNDLES.lock().unwrap_or_else(|e| e.into_inner());
                                    if b.len() > 4_096 {
                                        b.clear();
                                    }
                                    b.insert(mint, id);
                                }
                            }
                            if text.to_ascii_lowercase().contains("rate limit") {
                                note("limited");
                                rate_limited(&text);
                            } else if text.contains("\"error\"") || text.contains("\"rejected\"") {
                                note("refused");
                                refused(text.chars().take(200).collect());
                            } else {
                                note("ok");
                                counters::OK[lane].fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Ok(r) if r.status().as_u16() == 429 => {
                            note("limited");
                            rate_limited(&format!("HTTP 429: {}", r.text().await.unwrap_or_default()));
                        }
                        Ok(r) => {
                            note("refused");
                            let status = r.status();
                            refused(format!("HTTP {status}: {}", r.text().await.unwrap_or_default().chars().take(200).collect::<String>()));
                        }
                        Err(e) => {
                            note("lost");
                            refused(e.without_url().to_string())
                        }
                    }
                });
            }
        }
    });
}

// ---- AllenHark Relay over QUIC -------------------------------------------------------------------

/// A QUIC client for AllenHark Relay: TLS 1.3 verified against the web roots, server name the
/// region's host, no ALPN, no client certificate (their docs).
fn relay_endpoint() -> anyhow::Result<quinn::Endpoint> {
    use quinn::{crypto::rustls::QuicClientConfig, ClientConfig, IdleTimeout, TransportConfig};
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let crypto = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(IdleTimeout::try_from(Duration::from_secs(15))?));
    transport.keep_alive_interval(Some(Duration::from_secs(3)));
    config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// `quic://host:port` -> (address, server name).
fn quic_target(url: &str) -> Option<(SocketAddr, String)> {
    let hostport = url.strip_prefix("quic://")?.trim_end_matches('/');
    let host = hostport.rsplit_once(':')?.0.to_string();
    let addr = hostport.to_socket_addrs().ok()?.next()?;
    Some((addr, host))
}

/// The AllenHark Relay lane: one kept-alive QUIC connection; each transaction on its own stream
/// (`api-key: KEY` line, then the JSON body, then the end of the stream), its answer read back.
fn relay_quic_thread(lane: usize, url: String, key: String, mut rx: spsc::Consumer<TxMsg>, wake: Arc<tokio::sync::Notify>, awake_until: Arc<AtomicU64>) {
    super::affinity::pin_lane_thread();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("relay lane runtime");
    rt.block_on(async move {
        let endpoint = match relay_endpoint() {
            Ok(e) => e,
            Err(e) => {
                warn!("hot lane allenhark: no QUIC endpoint: {e:#}; the lane stays off");
                return;
            }
        };
        let Some((addr, server_name)) = quic_target(&url) else {
            warn!("hot lane allenhark: cannot resolve the relay address; the lane stays off");
            return;
        };
        let header = format!("api-key: {key}\n");
        let mut conn: Option<quinn::Connection> = None;
        let mut failures = 0u32;
        let mut retry_at = 0u64;
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                _ = wake.notified() => {}
                _ = tick.tick() => {}
            }
            // Keep one connection up; reconnect with back-off (C3).
            if conn.as_ref().is_none_or(|c| c.close_reason().is_some()) {
                counters::WARM[lane].store(0, Ordering::Relaxed);
                let now = now_ns();
                if now >= retry_at {
                    let t = now_ns();
                    match endpoint.connect(addr, &server_name) {
                        Ok(connecting) => match tokio::time::timeout(Duration::from_secs(3), connecting).await {
                            Ok(Ok(c)) => {
                                stamps::rtt::PING[lane].record(now_ns() - t);
                                counters::RECONNECTS[lane].fetch_add(1, Ordering::Relaxed);
                                counters::WARM[lane].store(1, Ordering::Relaxed);
                                failures = 0;
                                conn = Some(c);
                            }
                            other => {
                                failures += 1;
                                retry_at = now + (200u64 << failures.min(7)).min(30_000) * 1_000_000;
                                if failures == 1 || failures % 20 == 0 {
                                    warn!("hot lane allenhark: connect failed ({failures} in a row): {:?}", other.map(|r| r.err()));
                                }
                            }
                        },
                        Err(e) => {
                            failures += 1;
                            retry_at = now + 5_000_000_000;
                            warn!("hot lane allenhark: connect: {e}");
                        }
                    }
                }
            }
            loop {
                // While primed (a create is on its way to a buy), look again instead of sleeping.
                let Some(msg) = rx.pop() else {
                    if now_ns() < awake_until.load(Ordering::Relaxed) {
                        super::affinity::spin_pause();
                        tokio::task::yield_now().await;
                        continue;
                    }
                    break;
                };
                let Some(c) = conn.clone() else {
                    counters::REFUSED[lane].fetch_add(1, Ordering::Relaxed);
                    stamps::answers::push(stamps::answers::Answer { lane, ping: false, conn: 0, t_sent: now_ns(), t_answer: 0, status: "lost" });
                    continue;
                };
                use base64::Engine;
                let body = serde_json::json!({"tx": base64::engine::general_purpose::STANDARD.encode(&msg.bytes[..msg.len as usize]), "simulate": false}).to_string();
                let header = header.clone();
                let mint = msg.mint;
                tokio::spawn(async move {
                    let t = now_ns();
                    let sent = async {
                        let (mut send, recv) = c.open_bi().await?;
                        send.write_all(header.as_bytes()).await?;
                        send.write_all(body.as_bytes()).await?;
                        send.finish()?;
                        anyhow::Ok(recv)
                    }
                    .await;
                    let tw = now_ns();
                    let mut recv = match sent {
                        Ok(r) => {
                            stamps::record_lane(&mint, lane, tw);
                            counters::SENT[lane].fetch_add(1, Ordering::Relaxed);
                            r
                        }
                        Err(e) => {
                            counters::REFUSED[lane].fetch_add(1, Ordering::Relaxed);
                            stamps::answers::push(stamps::answers::Answer { lane, ping: false, conn: 0, t_sent: t, t_answer: 0, status: "lost" });
                            warn!("hot lane allenhark: stream: {e}");
                            return;
                        }
                    };
                    let answer = tokio::time::timeout(Duration::from_secs(3), recv.read_to_end(8_192)).await;
                    let ta = now_ns();
                    let status = match &answer {
                        Ok(Ok(b)) if String::from_utf8_lossy(b).contains("accepted") => "ok",
                        Ok(Ok(_)) => "refused",
                        _ => "lost",
                    };
                    stamps::rtt::TX[lane].record(ta - tw);
                    stamps::answers::push(stamps::answers::Answer { lane, ping: false, conn: 0, t_sent: tw, t_answer: if status == "lost" { 0 } else { ta }, status });
                    if status == "ok" {
                        counters::OK[lane].fetch_add(1, Ordering::Relaxed);
                    } else {
                        let k = counters::REFUSED[lane].fetch_add(1, Ordering::Relaxed);
                        if k < 5 || k % 100 == 0 {
                            let text = match answer {
                                Ok(Ok(b)) => String::from_utf8_lossy(&b).chars().take(200).collect(),
                                Ok(Err(e)) => e.to_string(),
                                Err(_) => "no answer in 3 s".into(),
                            };
                            warn!("hot lane allenhark: refused {}: {text}", bs58::encode(mint).into_string());
                        }
                    }
                });
            }
        }
    });
}

/// The QUIC lane's connections.
pub mod quic_state {
    use std::sync::atomic::AtomicU64;
    pub static CONNECTED: AtomicU64 = AtomicU64::new(0);
    pub static CONNECT_FAILED: AtomicU64 = AtomicU64::new(0);
    pub static STREAMS_OK: AtomicU64 = AtomicU64::new(0);
    pub static STREAMS_FAILED: AtomicU64 = AtomicU64::new(0);
    pub static NO_TARGET: AtomicU64 = AtomicU64::new(0);
    /// Connections the leader closed while it was still a target.
    pub static CLOSED_EARLY: AtomicU64 = AtomicU64::new(0);
}

pub(crate) fn quic_endpoint(identity: &Keypair) -> anyhow::Result<quinn::Endpoint> {
    use {
        quinn::{crypto::rustls::QuicClientConfig, ClientConfig, IdleTimeout, TransportConfig},
        solana_tls_utils::{tls_client_config_builder, QuicClientCertificate},
    };
    let cert = QuicClientCertificate::new(Some(identity));
    let mut crypto = tls_client_config_builder().with_client_auth_cert(vec![cert.certificate.clone()], cert.key.clone_key())?;
    crypto.enable_early_data = true;
    crypto.alpn_protocols = vec![b"solana-tpu".to_vec()];
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(IdleTimeout::try_from(Duration::from_secs(2))?));
    transport.keep_alive_interval(Some(Duration::from_secs(1)));
    transport.send_fairness(false);
    config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// The TPU addresses the QUIC lane aims at: the leader of the slot the feeds see now and the
/// next different leader (leaders hold 4 slots each).
fn quic_targets() -> Vec<SocketAddr> {
    let slot = FEED_SLOT.load(Ordering::Relaxed).max(crate::utils::latency::latest_slot().unwrap_or(0));
    let mut out = Vec::new();
    if slot == 0 {
        return out;
    }
    let mut leaders: Vec<Pubkey> = Vec::new();
    for s in slot..slot + 12 {
        if let Some(l) = crate::utils::leaders::leader(s) {
            if !leaders.contains(&l) {
                leaders.push(l);
            }
        }
        if leaders.len() == 2 {
            break;
        }
    }
    for l in leaders {
        if let Some(addr) = crate::utils::leaders::tpu_quic(&l) {
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    out
}

fn quic_thread(identity: Keypair, staked: bool, mut rx: spsc::Consumer<TxMsg>, wake: Arc<tokio::sync::Notify>, awake_until: Arc<AtomicU64>) {
    super::affinity::pin_lane_thread();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("quic lane runtime");
    rt.block_on(async move {
        let endpoint = match quic_endpoint(&identity) {
            Ok(e) => e,
            Err(e) => {
                warn!("hot lane quic: no endpoint: {e:#}; the lane stays off");
                return;
            }
        };
        info!("hot lane quic: identity {} ({})", identity.pubkey(), if staked { "from QUIC_IDENTITY_KEYPAIR" } else { "unstaked placeholder until the staked identity exists" });
        type Conns = Arc<Mutex<Vec<(SocketAddr, Option<quinn::Connection>)>>>;
        let conns: Conns = Default::default();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = wake.notified() => {}
                _ = tick.tick() => {
                    // Keep connections to the current and next leaders; drop the rest. `None`
                    // marks a connect in progress.
                    let targets = quic_targets();
                    let mut list = conns.lock().unwrap_or_else(|e| e.into_inner());
                    list.retain(|(addr, c)| {
                        let closed = c.as_ref().and_then(|c| c.close_reason());
                        if let Some(reason) = &closed {
                            let k = quic_state::CLOSED_EARLY.fetch_add(1, Ordering::Relaxed);
                            if k < 5 || k % 500 == 0 {
                                info!("hot lane quic: {addr} closed the connection ({k} so far): {reason}");
                            }
                        }
                        targets.contains(addr) && closed.is_none()
                    });
                    counters::WARM[LANE_QUIC].store(list.iter().filter(|(_, c)| c.is_some()).count() as u64, Ordering::Relaxed);
                    for addr in targets {
                        if list.iter().any(|(a, _)| *a == addr) {
                            continue;
                        }
                        let Ok(connecting) = endpoint.connect(addr, &solana_tls_utils::socket_addr_to_quic_server_name(addr)) else {
                            quic_state::CONNECT_FAILED.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        list.push((addr, None));
                        let conns = conns.clone();
                        tokio::spawn(async move {
                            let result = tokio::time::timeout(Duration::from_secs(2), connecting).await;
                            let mut list = conns.lock().unwrap_or_else(|e| e.into_inner());
                            match result {
                                Ok(Ok(c)) => {
                                    quic_state::CONNECTED.fetch_add(1, Ordering::Relaxed);
                                    if let Some(e) = list.iter_mut().find(|(a, _)| *a == addr) {
                                        e.1 = Some(c);
                                    }
                                }
                                _ => {
                                    quic_state::CONNECT_FAILED.fetch_add(1, Ordering::Relaxed);
                                    list.retain(|(a, _)| *a != addr);
                                }
                            }
                        });
                    }
                    continue;
                }
            }
            loop {
                // While primed (a create is on its way to a buy), look again instead of sleeping.
                let Some(msg) = rx.pop() else {
                    if now_ns() < awake_until.load(Ordering::Relaxed) {
                        super::affinity::spin_pause();
                        tokio::task::yield_now().await;
                        continue;
                    }
                    break;
                };
                let targets: Vec<quinn::Connection> = conns.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(|(_, c)| c.clone()).collect();
                if targets.is_empty() {
                    quic_state::NO_TARGET.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let bytes = &msg.bytes[..msg.len as usize];
                let t0 = now_ns();
                let mut first = true;
                for (k, c) in targets.into_iter().enumerate() {
                    let sent = match c.open_uni().await {
                        Ok(mut s) => {
                            let ok = s.write_all(bytes).await.is_ok() && s.finish().is_ok();
                            if ok {
                                // The leader's acknowledgement of every byte (transport level; it
                                // includes the peer's ack delay).
                                let (t_write, acked) = (now_ns(), s.stopped());
                                tokio::spawn(async move {
                                    let status = match tokio::time::timeout(Duration::from_secs(3), acked).await {
                                        Ok(Ok(None)) => "acked",
                                        Ok(Ok(Some(_))) => "stopped",
                                        Ok(Err(_)) => "lost",
                                        Err(_) => "timeout",
                                    };
                                    let ta = if matches!(status, "acked" | "stopped") { now_ns() } else { 0 };
                                    if status == "acked" {
                                        counters::OK[LANE_QUIC].fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        counters::REFUSED[LANE_QUIC].fetch_add(1, Ordering::Relaxed);
                                    }
                                    stamps::answers::push(stamps::answers::Answer { lane: LANE_QUIC, ping: false, conn: k as u8, t_sent: t_write, t_answer: ta, status });
                                });
                            }
                            ok
                        }
                        Err(_) => false,
                    };
                    if sent {
                        quic_state::STREAMS_OK.fetch_add(1, Ordering::Relaxed);
                        if first {
                            let t = now_ns();
                            stamps::record_lane(&msg.mint, LANE_QUIC, t);
                            // No answer on QUIC: the "round trip" is the hand-off to the socket.
                            stamps::rtt::TX[LANE_QUIC].record(t - t0);
                            counters::SENT[LANE_QUIC].fetch_add(1, Ordering::Relaxed);
                            first = false;
                        }
                    } else {
                        quic_state::STREAMS_FAILED.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    });
}

// ---- the lanes together -----------------------------------------------------------------------

/// How a lane is driven: written from the decision thread, or handed to the lane's thread.
enum Driver {
    Http(HttpLane),
    Ring(RingLane),
}

/// Every lane, owned by the decision thread.
pub struct Lanes {
    /// The started lanes, by index (the firing order).
    drivers: [Option<Driver>; LANES],
    pub cfg: LaneConfig,
    /// LANE_ROTATE: the next turn in `ROTATION`.
    turn: usize,
    /// Tests: the order the lanes were fired in.
    #[cfg(test)]
    pub fired: Vec<usize>,
}

/// Everstake's per-second request limit (pings included).
const EVERSTAKE_PER_SEC: usize = 10;

/// HOT_EVERSTAKE_DIRECT=true: Everstake as a plain-HTTP lane (read once, at the lanes' start).
fn everstake_direct() -> bool {
    env_opt("HOT_EVERSTAKE_DIRECT").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// The ping a plain-HTTP lane sends while idle, and how often.
fn http_ping(lane: usize) -> (&'static str, u64) {
    match lane {
        // Helius: a ping after 5 s idle (its docs).
        LANE_HELIUS => ("/ping", 5_000_000_000),
        // Everstake documents no health route; its keep-alive interval is 10 s.
        LANE_EVERSTAKE => ("/", 10_000_000_000),
        LANE_ZEROSLOT => ("/health", 15_000_000_000),
        LANE_NOZOMI => ("/ping", 15_000_000_000),
        _ => ("/", 15_000_000_000),
    }
}

fn ring() -> (RingLane, spsc::Consumer<TxMsg>, Arc<tokio::sync::Notify>, Arc<AtomicU64>) {
    let (tx, rx) = spsc::channel::<TxMsg>(64);
    let wake = Arc::new(tokio::sync::Notify::new());
    let awake_until = Arc::new(AtomicU64::new(0));
    (RingLane { tx, wake: wake.clone(), awake_until: awake_until.clone() }, rx, wake, awake_until)
}

impl Lanes {
    /// Opens the enabled lanes and starts their threads, all off the hot cores. Off the hot path.
    pub fn start(cfg: LaneConfig) -> Self {
        let mut drivers: [Option<Driver>; LANES] = std::array::from_fn(|_| None);
        let mut http_io = Vec::new();
        for lane in 0..LANES {
            let spec = cfg.lanes[lane].clone();
            if !spec.enabled {
                continue;
            }
            let url = spec.url.clone().unwrap_or_default();
            let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
                std::thread::Builder::new().name(format!("hot-lane-{name}")).spawn(f).expect("lane thread");
            };
            drivers[lane] = match lane {
                LANE_QUIC => {
                    crate::utils::leaders::ensure_spawned();
                    let staked = match cfg.quic_identity.as_deref().map(std::fs::read_to_string) {
                        Some(Ok(text)) => serde_json::from_str::<Vec<u8>>(&text).ok().and_then(|b| Keypair::try_from(b.as_slice()).ok()),
                        Some(Err(e)) => {
                            warn!("hot lane quic: QUIC_IDENTITY_KEYPAIR unreadable ({e}); using an unstaked identity");
                            None
                        }
                        None => None,
                    };
                    let is_staked = staked.is_some();
                    let identity = staked.unwrap_or_else(Keypair::new);
                    let (r, rx, wake, awake) = ring();
                    spawn("quic", Box::new(move || quic_thread(identity, is_staked, rx, wake, awake)));
                    Some(Driver::Ring(r))
                }
                LANE_ALLENHARK if url.starts_with("quic://") => {
                    let (r, rx, wake, awake) = ring();
                    let key = spec.key.clone().unwrap_or_default();
                    spawn("allenhark", Box::new(move || relay_quic_thread(lane, url, key, rx, wake, awake)));
                    Some(Driver::Ring(r))
                }
                LANE_ALLENHARK => {
                    let base = url.trim_end_matches('/').trim_end_matches("/v1/sendTx").to_string();
                    let (r, rx, wake, awake) = ring();
                    let header = spec.key.clone().map(|k| ("x-api-key", k));
                    let ping = (reqwest::Method::GET, format!("{base}/keepalive"), None);
                    let send_url = format!("{base}/v1/sendTx");
                    spawn("allenhark", Box::new(move || https_thread(lane, HttpsKind::AllenHark, send_url, header, ping, Duration::from_secs(5), false, 10, rx, wake, awake)));
                    Some(Driver::Ring(r))
                }
                LANE_JITO => {
                    let (r, rx, wake, awake) = ring();
                    let header = spec.key.clone().map(|k| ("x-jito-auth", k));
                    let base = url.split("/api/").next().unwrap_or(&url).to_string();
                    let ping = (reqwest::Method::POST, format!("{base}/api/v1/getTipAccounts"), Some(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "getTipAccounts", "params": []})));
                    spawn("jito", Box::new(move || https_thread(lane, HttpsKind::Jito, url, header, ping, Duration::from_secs(15), false, 0, rx, wake, awake)));
                    Some(Driver::Ring(r))
                }
                // HOT_EVERSTAKE_DIRECT=true: Everstake as a plain-HTTP lane, written by the decision
                // thread on a kept-alive HTTP/1.1 connection like 0slot (its endpoint answers
                // HTTP/1.1, checked 14 Oct), under its 10/s limit less one for the keep-alive ping.
                // The threaded reqwest lane below took ~105 us (p99 1.1 ms) from the hand-over to
                // the write (Frankfurt paper run, 14 Oct); 0slot's direct write ~50 us.
                LANE_EVERSTAKE if everstake_direct() && url.starts_with("http://") => match parse_http(&url) {
                    Some((host, port, path)) => {
                        let (ping, every) = http_ping(lane);
                        let l = HttpLane::new(HttpTarget { name: LANE_NAMES[lane], host, port, path, body: Body::JsonRpc, ping: ping.into(), ping_every_ns: every })
                            .with_limit(lane, EVERSTAKE_PER_SEC - 1);
                        info!("hot lane everstake: direct plain-HTTP writes from the decision thread (HOT_EVERSTAKE_DIRECT)");
                        http_io.push((lane, l.target.clone(), l.conns.clone()));
                        Some(Driver::Http(l))
                    }
                    None => None,
                },
                LANE_EVERSTAKE => {
                    let (r, rx, wake, awake) = ring();
                    // No health route: a getHealth (answered "method not found") times the connection.
                    let ping = (reqwest::Method::POST, url.clone(), Some(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "getHealth"})));
                    spawn("everstake", Box::new(move || https_thread(lane, HttpsKind::JsonRpc, url, None, ping, Duration::from_secs(10), true, EVERSTAKE_PER_SEC, rx, wake, awake)));
                    Some(Driver::Ring(r))
                }
                LANE_TRITON if url.starts_with("https://") => {
                    let (r, rx, wake, awake) = ring();
                    let ping = (reqwest::Method::POST, url.clone(), Some(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "getHealth"})));
                    spawn("triton", Box::new(move || https_thread(lane, HttpsKind::JsonRpc, url, None, ping, Duration::from_secs(15), false, 0, rx, wake, awake)));
                    Some(Driver::Ring(r))
                }
                _ => match parse_http(&url) {
                    Some((host, port, path)) => {
                        let (ping, every) = http_ping(lane);
                        let body = if lane == LANE_NOZOMI { Body::Text } else { Body::JsonRpc };
                        let l = HttpLane::new(HttpTarget { name: LANE_NAMES[lane], host, port, path, body, ping: ping.into(), ping_every_ns: every });
                        http_io.push((lane, l.target.clone(), l.conns.clone()));
                        Some(Driver::Http(l))
                    }
                    None => None,
                },
            };
        }
        if let Some(core) = super::affinity::lane_core() {
            info!("hot lanes: the lane threads run on core {core} of their own (HOT_LANE_CORE); nothing else is scheduled there");
        }
        if !http_io.is_empty() {
            std::thread::Builder::new().name("hot-lanes-io".into()).spawn(move || run_http_io(http_io)).expect("lanes io thread");
        }
        for (i, d) in drivers.iter().enumerate() {
            counters::ON[i].store(d.is_some(), Ordering::Relaxed);
        }
        info!(
            "hot lanes (LANE_SET={}): {}; tips {} SOL per buy{}, priority {} SOL default, cap {} SOL with tips{}",
            cfg.set,
            cfg.describe().join("; "),
            cfg.tips_total() as f64 / 1e9,
            if cfg.rotate { " with every lane (LANE_ROTATE: one lane per buy, in turn, paying only its own tip; a Jito copy when the fee engine asks)" } else { "" },
            cfg.fee_default as f64 / 1e9,
            cfg.fee_cap as f64 / 1e9,
            if cfg.dry_lanes { "; DRY_RUN: unlandable copies (all-zero blockhash) go out to time the lanes" } else { "" }
        );
        Lanes {
            drivers,
            cfg,
            turn: 0,
            #[cfg(test)]
            // Reserved, so the allocation audit sees the lanes' writes, not this test record.
            fired: Vec::with_capacity(4096),
        }
    }

    /// One lane: written now (plain HTTP, the write's clock reading) or handed to its thread.
    #[inline]
    fn fire(&mut self, lane: usize, mint: &[u8; 32], tx: &[u8], b64: &[u8], t_sent: &mut [u64; LANES]) {
        if counters::DEAD[lane].load(Ordering::Relaxed) {
            return;
        }
        match self.drivers[lane].as_mut() {
            Some(Driver::Http(l)) => {
                if let Some(t) = l.send(b64) {
                    t_sent[lane] = t;
                    counters::SENT[lane].fetch_add(1, Ordering::Relaxed);
                }
            }
            Some(Driver::Ring(r)) => {
                r.send(mint, tx);
            }
            None => return,
        }
        #[cfg(test)]
        self.fired.push(lane);
    }

    /// Sends the same signed bytes on every lane (B2): the staked gateways first and all at once
    /// (the threaded ones handed over first, so they start while the plain-HTTP ones are written),
    /// then 0slot and Nozomi, then Jito, then direct QUIC. Stamps the plain-HTTP lanes' write
    /// times; the threaded lanes stamp theirs when they write. No allocation.
    #[inline]
    pub fn send(&mut self, mint: &[u8; 32], tx: &[u8], b64: &[u8], t_sent: &mut [u64; LANES]) {
        for lane in STAKED {
            if matches!(self.drivers[lane], Some(Driver::Ring(_))) {
                self.fire(lane, mint, tx, b64, t_sent);
            }
        }
        for lane in STAKED {
            if matches!(self.drivers[lane], Some(Driver::Http(_))) {
                self.fire(lane, mint, tx, b64, t_sent);
            }
        }
        for lane in [LANE_ZEROSLOT, LANE_NOZOMI, LANE_JITO, LANE_QUIC] {
            self.fire(lane, mint, tx, b64, t_sent);
        }
    }

    /// A create is on its way to a possible buy: wakes the threaded lanes now and keeps them
    /// spinning for `PRIME_NS`, so their write follows the plain-HTTP ones closely (B2) instead of
    /// waiting out a wake-up from sleep. A notify per threaded lane; no allocation.
    #[inline]
    pub fn prime(&self) {
        let until = now_ns() + PRIME_NS;
        for d in self.drivers.iter().flatten() {
            if let Driver::Ring(r) = d {
                r.prime(until);
            }
        }
    }

    /// LANE_ROTATE: the lane whose turn it is (the next one that is on, in `ROTATION` order) and
    /// the index of its tip in the buy (None: it tips nothing). None when no lane is on.
    #[inline]
    pub fn next_turn(&mut self) -> Option<(usize, Option<u8>)> {
        for _ in 0..LANES {
            let lane = ROTATION[self.turn % LANES];
            self.turn = self.turn.wrapping_add(1);
            if self.is_on(lane) && self.cfg.rotate_lanes[lane] {
                return Some((lane, self.cfg.tip_index(lane)));
            }
        }
        None
    }

    /// `next_turn` among the lanes of `mask` (a bit per lane index) only.
    #[inline]
    pub fn next_turn_in(&mut self, mask: u8) -> Option<(usize, Option<u8>)> {
        for _ in 0..LANES {
            let lane = ROTATION[self.turn % LANES];
            self.turn = self.turn.wrapping_add(1);
            if mask & (1 << lane) != 0 && self.is_on(lane) {
                return Some((lane, self.cfg.tip_index(lane)));
            }
        }
        None
    }

    /// LANE_ROTATE: the buy's bytes on its turn's lane only, stamped like `send`.
    #[inline]
    pub fn send_turn(&mut self, lane: usize, mint: &[u8; 32], tx: &[u8], b64: &[u8], t_sent: &mut [u64; LANES]) {
        self.fire(lane, mint, tx, b64, t_sent);
    }

    /// One lane only (the lane race tool sends each lane its own transaction).
    pub fn send_lane(&mut self, lane: usize, mint: &[u8; 32], tx: &[u8], b64: &[u8]) -> bool {
        let mut t = [0u64; LANES];
        let on = self.drivers[lane].is_some();
        self.fire(lane, mint, tx, b64, &mut t);
        on
    }

    /// Configured and its key still accepted (`counters::DEAD`).
    pub fn is_on(&self, lane: usize) -> bool {
        self.drivers[lane].is_some() && !counters::DEAD[lane].load(Ordering::Relaxed)
    }

    /// Whether `lane` is written by its own thread (a hand-over to it is a push).
    pub fn is_threaded(&self, lane: usize) -> bool {
        matches!(self.drivers[lane], Some(Driver::Ring(_)))
    }
}

/// One line on the lanes for the stats log.
pub fn report() -> String {
    let c = |a: &[AtomicU64; LANES], i: usize| a[i].load(Ordering::Relaxed);
    let mut parts = Vec::new();
    for (i, name) in LANE_NAMES.iter().enumerate() {
        if !counters::ON[i].load(Ordering::Relaxed) {
            continue;
        }
        let extra = match i {
            LANE_QUIC => format!(
                ", {} leader connections now, {} opened, {} failed, {} closed by the leader, streams {} ok / {} failed, {} sends with no leader connected",
                c(&counters::WARM, i),
                quic_state::CONNECTED.load(Ordering::Relaxed),
                quic_state::CONNECT_FAILED.load(Ordering::Relaxed),
                quic_state::CLOSED_EARLY.load(Ordering::Relaxed),
                quic_state::STREAMS_OK.load(Ordering::Relaxed),
                quic_state::STREAMS_FAILED.load(Ordering::Relaxed),
                quic_state::NO_TARGET.load(Ordering::Relaxed)
            ),
            LANE_JITO | LANE_ALLENHARK | LANE_TRITON | LANE_EVERSTAKE => format!(
                ", connection {}, reconnects {}, held back by the provider's rate limit {}, refused for its rate limit {}",
                if c(&counters::WARM, i) > 0 { "warm" } else { "down" },
                c(&counters::RECONNECTS, i),
                c(&counters::LIMITED, i),
                c(&counters::RATE_LIMITED, i)
            ),
            _ => format!(", {}/2 connections warm, reconnects {}", c(&counters::WARM, i), c(&counters::RECONNECTS, i)),
        };
        parts.push(format!("{name}: sent {}, ok {}, refused {}{extra}", c(&counters::SENT, i), c(&counters::OK, i), c(&counters::REFUSED, i)));
    }
    if parts.is_empty() {
        return "lanes: none".into();
    }
    format!("lanes: {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(on: &[(usize, u64)]) -> LaneConfig {
        let mut lanes: [LaneSpec; LANES] = Default::default();
        for &(i, tip) in on {
            lanes[i] = LaneSpec { enabled: true, url: Some("http://127.0.0.1:1/".into()), tip, tip_accounts: default_tip_accounts(i), ..Default::default() };
        }
        LaneConfig { lanes, set: "race".into(), jito_uuid: None, quic_identity: None, fee_default: 700_000, fee_cap: 5_000_000, outbid_bps: 1_000, dry_lanes: false, rotate: false, rotate_lanes: [true; LANES] }
    }

    fn cfg() -> LaneConfig {
        cfg_with(&[(LANE_ZEROSLOT, 1_000_000), (LANE_NOZOMI, 1_000_000), (LANE_JITO, 100_000)])
    }

    /// Everstake as configured: its accounts from config (here, stand-ins), `tip` lamports.
    fn everstake(tip: u64) -> LaneSpec {
        let accounts: Vec<Pubkey> = (0..10).map(|_| Pubkey::new_unique()).collect();
        LaneSpec { enabled: true, url: Some("http://fra-swqos.everstake.one".into()), tip, tip_accounts: Box::leak(accounts.into_boxed_slice()), ..Default::default() }
    }

    /// Every lane this round, with the tips of the spec.
    fn race_set() -> LaneConfig {
        cfg_with(&[(LANE_HELIUS, 5_000), (LANE_ALLENHARK, 1_000_000), (LANE_ZEROSLOT, 1_000_000), (LANE_NOZOMI, 1_000_000), (LANE_JITO, 100_000), (LANE_QUIC, 0)])
    }

    #[test]
    fn the_bid_is_the_default_or_over_the_rival_and_never_past_the_cap() {
        let c = cfg();
        let cu = 120_000;
        let default = 700_000u64 * 1_000_000 / 120_000;
        assert_eq!(bid(&c, cu, 0), (default, false), "no rival: 0.0007 SOL");
        assert_eq!(bid(&c, cu, default / 2), (default, false), "a lower rival changes nothing");
        let (p, raised) = bid(&c, cu, default * 2);
        assert!(raised && p == default * 2 * 11 / 10 + 1, "10% over the rival: {p}");
        let cap = (5_000_000u64 - 2_100_000) * 1_000_000 / 120_000;
        assert_eq!(bid(&c, cu, u64::MAX / 100_000).0, cap, "tips plus priority stay under 0.005 SOL");
        assert!(cap as u128 * 120_000 / 1_000_000 + 2_100_000 <= 5_000_000);
    }

    #[test]
    fn urls_split_into_host_port_and_path() {
        assert_eq!(parse_http("http://de1.0slot.trade?api-key=K"), Some(("de1.0slot.trade".into(), 80, "/?api-key=K".into())));
        assert_eq!(parse_http("http://fra2.nozomi.temporal.xyz/api/sendTransaction2?c=K"), Some(("fra2.nozomi.temporal.xyz".into(), 80, "/api/sendTransaction2?c=K".into())));
        assert_eq!(parse_http("http://h:8080/x"), Some(("h".into(), 8080, "/x".into())));
        assert_eq!(parse_http("https://h/x"), None);
    }

    #[test]
    fn requests_are_well_formed_http_with_the_right_length() {
        let mut lane = HttpLane::new(HttpTarget { name: "t", host: "h".into(), port: 80, path: "/p".into(), body: Body::JsonRpc, ping: "/".into(), ping_every_ns: 1 });
        let cap = lane.buf.capacity();
        // No connection: nothing is written, but the request is laid out.
        assert!(lane.send(b"QUJD").is_none());
        let text = String::from_utf8(lane.buf.clone()).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /p HTTP/1.1\r\nHost: h\r\nContent-Type: application/json"));
        let len: usize = head.lines().find_map(|l| l.strip_prefix("Content-Length: ")).unwrap().parse().unwrap();
        assert_eq!(len, body.len());
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["params"][0], "QUJD");
        assert_eq!(json["params"][1]["maxRetries"], 0);
        assert_eq!(lane.buf.capacity(), cap, "no reallocation");
    }

    #[test]
    fn responses_are_split_by_length_or_chunks() {
        let mut b = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokHTTP/1.1 429 Too Many\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nHTTP/1.1 200".to_vec();
        assert_eq!(take_response(&mut b).map(|r| r.0), Some(200));
        assert_eq!(take_response(&mut b).map(|r| r.0), Some(429));
        assert_eq!(take_response(&mut b), None, "incomplete");
        assert!(b.starts_with(b"HTTP/1.1 200"));
    }

    #[test]
    fn one_transaction_carries_every_enabled_lanes_tip_in_firing_order() {
        let c = race_set();
        let tips = c.tips();
        let order: Vec<u64> = tips.iter().map(|t| t.1).collect();
        assert_eq!(order, vec![5_000, 1_000_000, 1_000_000, 1_000_000, 100_000], "helius, allenhark, 0slot, nozomi, jito; no tip for direct QUIC");
        assert_eq!(c.tips_total(), 3_105_000);
        assert!(tips[1].0.contains(&ALLENHARK_TIP_ACCOUNTS[0]));
        // With Everstake too, the buy still fits in one packet.
        let mut all = race_set();
        all.lanes[LANE_EVERSTAKE] = everstake(1_000_000);
        let spec = super::super::template::TemplateSpec { payer: Pubkey::new_unique(), cu_limit: 120_000, loaded_data_bytes: Some(16 << 20), tips: all.tips(), dontfront: None };
        let t = super::super::template::BuyTemplate::build(spec).expect("a buy with six tips fits in a packet");
        eprintln!("buy with six tips: {} bytes signed", t.message().len() + 65);
        // The priority bid's room under the cap shrinks with the tips (reported, not changed).
        let cu = 120_000;
        let cap_now = bid(&race_set(), cu, u64::MAX / 1_000_000).0 as u128 * cu as u128 / 1_000_000;
        assert_eq!(cap_now as u64, 5_000_000 - 3_105_000 - 1, "the cap left for priority with the race set");
    }

    /// Three transactions in a row refused for the key (not pings, not other errors) take a lane
    /// out of the rotation; an accepted answer in between starts the count over.
    #[test]
    fn a_lane_whose_key_is_refused_leaves_the_rotation() {
        let lane = LANE_TRITON; // no other test uses it: DEAD is process-wide
        counters::DEAD[lane].store(false, Ordering::Relaxed);
        counters::AUTH_REFUSED_IN_A_ROW[lane].store(0, Ordering::Relaxed);
        assert!(!counters::note_tx_answer(lane, 403, true));
        assert!(!counters::note_tx_answer(lane, 200, false), "an accepted answer resets the count");
        assert!(!counters::note_tx_answer(lane, 403, true));
        assert!(!counters::note_tx_answer(lane, 500, true), "a server error is not a key problem");
        assert!(!counters::note_tx_answer(lane, 401, true));
        assert!(!counters::note_tx_answer(lane, 403, true));
        assert!(counters::note_tx_answer(lane, 403, true), "the third in a row");
        assert!(!counters::note_tx_answer(lane, 403, true), "dead once: reported once");
        let mut cfg = cfg_with(&[(LANE_EVERSTAKE, 1_000_000), (LANE_TRITON, 1_000_000)]);
        cfg.lanes[LANE_EVERSTAKE].tip_accounts = default_tip_accounts(LANE_NOZOMI);
        cfg.lanes[LANE_TRITON].tip_accounts = default_tip_accounts(LANE_NOZOMI);
        cfg.rotate = true;
        let mut drivers: [Option<Driver>; LANES] = std::array::from_fn(|_| None);
        for l in [LANE_EVERSTAKE, LANE_TRITON] {
            let (r, _rx, _wake, _awake) = ring();
            drivers[l] = Some(Driver::Ring(r));
        }
        let mut lanes = Lanes { drivers, cfg, turn: 0, fired: Vec::new() };
        assert!(!lanes.is_on(LANE_TRITON) && lanes.is_on(LANE_EVERSTAKE));
        for _ in 0..4 {
            assert_eq!(lanes.next_turn().map(|t| t.0), Some(LANE_EVERSTAKE), "only the live lane gets turns");
        }
        lanes.send_lane(LANE_TRITON, &[1; 32], &[0; 10], b"");
        assert!(lanes.fired.is_empty(), "nothing goes to a dead lane");
        counters::DEAD[lane].store(false, Ordering::Relaxed);
        counters::AUTH_REFUSED_IN_A_ROW[lane].store(0, Ordering::Relaxed);
    }

    #[test]
    fn rotation_sends_each_buy_on_one_lane_in_turn_and_names_its_tip() {
        let mut cfg = cfg_with(&[(LANE_HELIUS, 5_000), (LANE_EVERSTAKE, 1_000_000), (LANE_ZEROSLOT, 1_000_000)]);
        // Everstake's tip accounts come from config; any list stands in here.
        cfg.lanes[LANE_EVERSTAKE].tip_accounts = default_tip_accounts(LANE_NOZOMI);
        cfg.rotate = true;
        assert_eq!(cfg.tips().len(), 3);
        assert_eq!([cfg.tip_index(LANE_HELIUS), cfg.tip_index(LANE_EVERSTAKE), cfg.tip_index(LANE_ZEROSLOT), cfg.tip_index(LANE_NOZOMI)], [Some(0), Some(1), Some(2), None]);
        let mut drivers: [Option<Driver>; LANES] = std::array::from_fn(|_| None);
        let mut rxs = Vec::new();
        for lane in [LANE_HELIUS, LANE_EVERSTAKE, LANE_ZEROSLOT] {
            let (r, rx, _wake, _awake) = ring();
            drivers[lane] = Some(Driver::Ring(r));
            rxs.push(rx);
        }
        let mut lanes = Lanes { drivers, cfg, turn: 0, fired: Vec::new() };
        let mut turns = Vec::new();
        for _ in 0..6 {
            let (lane, tip) = lanes.next_turn().expect("a lane is on");
            lanes.send_turn(lane, &[1; 32], &[0; 10], b"", &mut [0; LANES]);
            turns.push((lane, tip));
        }
        let order = [(LANE_ZEROSLOT, Some(2)), (LANE_EVERSTAKE, Some(1)), (LANE_HELIUS, Some(0))];
        assert_eq!(turns, [order, order].concat(), "0slot, Everstake, Helius in turn, each with its own tip");
        assert_eq!(lanes.fired, turns.iter().map(|t| t.0).collect::<Vec<_>>(), "each buy on its turn's lane only");
        let mut none = Lanes { drivers: std::array::from_fn(|_| None), cfg: cfg_with(&[]), turn: 0, fired: Vec::new() };
        assert_eq!(none.next_turn(), None);
    }

    #[test]
    fn a_send_fires_the_staked_gateways_first_and_every_lane_gets_the_same_bytes() {
        use std::net::TcpListener;
        // Local stand-ins for the plain-HTTP gateways.
        let http_lanes = [LANE_HELIUS, LANE_ZEROSLOT, LANE_NOZOMI];
        let mut listeners = Vec::new();
        let mut drivers: [Option<Driver>; LANES] = std::array::from_fn(|_| None);
        for lane in http_lanes {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let body = if lane == LANE_NOZOMI { Body::Text } else { Body::JsonRpc };
            let h = HttpLane::new(HttpTarget { name: LANE_NAMES[lane], host: "127.0.0.1".into(), port, path: "/p".into(), body, ping: "/".into(), ping_every_ns: u64::MAX });
            h.conns[0].lock().reset(Some(TcpStream::connect(("127.0.0.1", port)).unwrap()));
            drivers[lane] = Some(Driver::Http(h));
            listeners.push((lane, l));
        }
        // The threaded lanes: their rings, read here instead of by their threads.
        let mut rings = Vec::new();
        for lane in [LANE_EVERSTAKE, LANE_ALLENHARK, LANE_JITO, LANE_QUIC] {
            let (r, rx, _wake, _awake) = ring();
            drivers[lane] = Some(Driver::Ring(r));
            rings.push((lane, rx));
        }
        let mut lanes = Lanes { drivers, cfg: race_set(), turn: 0, fired: Vec::new() };
        let tx: Vec<u8> = (0..400u32).map(|i| (i * 7) as u8).collect();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&tx);
        let mut t_sent = [0u64; LANES];
        lanes.send(&[9; 32], &tx, b64.as_bytes(), &mut t_sent);
        assert_eq!(
            lanes.fired,
            vec![LANE_EVERSTAKE, LANE_ALLENHARK, LANE_HELIUS, LANE_ZEROSLOT, LANE_NOZOMI, LANE_JITO, LANE_QUIC],
            "staked gateways first (threaded ones handed over first), then 0slot and Nozomi, Jito, direct QUIC"
        );
        let written: Vec<u64> = http_lanes.iter().map(|&l| t_sent[l]).collect();
        assert!(written.iter().all(|&t| t > 0), "every plain-HTTP lane was written");
        let spread = written.iter().max().unwrap() - written.iter().min().unwrap();
        eprintln!("plain-HTTP writes spread: {:.1} us", spread as f64 / 1e3);
        assert!(spread < 200_000, "the decision thread's writes within 200 us ({spread} ns)");
        for (lane, l) in listeners {
            let (mut sock, _) = l.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while take_response_like_request(&buf).is_none() {
                let n = sock.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = take_response_like_request(&buf).unwrap();
            let sent = if lane == LANE_NOZOMI {
                body
            } else {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                v["params"][0].as_str().unwrap().to_string()
            };
            assert_eq!(sent, b64, "{}: the same signed bytes", LANE_NAMES[lane]);
        }
        for (lane, mut rx) in rings {
            let m = rx.pop().unwrap_or_else(|| panic!("{} got nothing", LANE_NAMES[lane]));
            assert_eq!(&m.bytes[..m.len as usize], &tx[..], "{}: the same signed bytes", LANE_NAMES[lane]);
        }
    }

    /// The body of one complete HTTP request in `buf` (Content-Length framed).
    fn take_response_like_request(buf: &[u8]) -> Option<String> {
        let end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
        let n: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok())?;
        (buf.len() >= end + n).then(|| String::from_utf8_lossy(&buf[end..end + n]).to_string())
    }

    #[test]
    fn lanes_come_from_their_variables_and_turn_off_without_what_they_need() {
        let c = race_set();
        assert!(c.enabled(LANE_HELIUS) && !c.enabled(LANE_EVERSTAKE) && !c.enabled(LANE_TRITON));
        assert_eq!(usable_url(LANE_ALLENHARK, "quic://fra.relay.allenhark.com:4433"), true);
        assert_eq!(usable_url(LANE_HELIUS, "http://fra-sender.helius-rpc.com/fast?swqos_only=true"), true);
        assert_eq!(usable_url(LANE_HELIUS, "https://fra-sender.helius-rpc.com/fast"), false, "plain-HTTP lanes need http://");
        assert!(default_tip_accounts(LANE_EVERSTAKE).is_empty(), "Everstake's tip accounts come from config only");
        assert_eq!(tip_accounts(LANE_ALLENHARK).len(), 11);
        assert!(tip_accounts(LANE_QUIC).is_empty() && tip_accounts(LANE_TRITON).is_empty());
        assert!(usable_url(LANE_QUIC, "tpu"), "direct QUIC needs no URL");
    }

    #[test]
    fn the_buy_carries_everstakes_tip_above_its_floor_through_a_static_key() {
        use {
            super::super::template::{BuyFill, BuyTemplate, TemplateSpec, PACKET},
            solana_transaction::versioned::VersionedTransaction,
        };
        // Helius Sender, Everstake, 0slot and Nozomi.
        let mut cfg = cfg_with(&[(LANE_HELIUS, 5_000), (LANE_ZEROSLOT, 1_000_000), (LANE_NOZOMI, 1_000_000)]);
        cfg.lanes[LANE_EVERSTAKE] = everstake(1_000_000);
        settle(&mut cfg.lanes);
        assert!(cfg.enabled(LANE_EVERSTAKE), "{:?}", cfg.lanes[LANE_EVERSTAKE].off_because);
        assert_eq!(cfg.tips_total(), 3_005_000, "per attempt: 0.000005 + 0.001 + 0.001 + 0.001 SOL");
        assert!(cfg.tips_total() + cfg.fee_default < cfg.fee_cap, "under the 0.005 SOL cap with the default priority fee");
        let payer = solana_keypair::Keypair::new();
        let mut template = BuyTemplate::build(TemplateSpec { payer: payer.pubkey(), cu_limit: 120_000, loaded_data_bytes: Some(16 << 20), tips: cfg.tips(), dontfront: None }).unwrap();
        let k = Pubkey::new_unique;
        for pick in 0..3u32 {
            template.fill(&BuyFill {
                mint: k(),
                token_program: k(),
                bonding_curve: k(),
                associated_bonding_curve: k(),
                user_ata: k(),
                creator_vault: k(),
                bonding_curve_v2: k(),
                fee_recipient: k(),
                buyback_fee_recipient: k(),
                lamports_in: 1,
                min_out: 1,
                cu_price: 5_833,
                blockhash: solana_hash::Hash::new_from_array([3; 32]),
                tip_pick: pick,
                paid: None,
                cu_limit: 0,
            });
            let mut buf = [0u8; PACKET];
            let n = template.sign_into(&payer, &mut buf);
            // Through the fan-out: what the Everstake lane is handed.
            let (r, mut rx, _w, _a) = ring();
            let mut drivers: [Option<Driver>; LANES] = std::array::from_fn(|_| None);
            drivers[LANE_EVERSTAKE] = Some(Driver::Ring(r));
            let mut lanes = Lanes { drivers, cfg: cfg.clone(), turn: 0, fired: Vec::new() };
            lanes.send(&[1; 32], &buf[..n], b"", &mut [0; LANES]);
            let got = rx.pop().expect("the Everstake lane got the buy");
            let tx: VersionedTransaction = bincode::deserialize(&got.bytes[..got.len as usize]).unwrap();
            assert!(matches!(tx.message, solana_message::VersionedMessage::Legacy(_)), "a legacy message: no lookup tables");
            let keys = tx.message.static_account_keys();
            let tip = tx
                .message
                .instructions()
                .iter()
                .filter(|ix| keys[ix.program_id_index as usize] == Pubkey::default() && ix.data.len() == 12 && ix.data[..4] == [2, 0, 0, 0])
                .find_map(|ix| {
                    let to = keys[ix.accounts[1] as usize];
                    cfg.lanes[LANE_EVERSTAKE].tip_accounts.contains(&to).then(|| (to, u64::from_le_bytes(ix.data[4..12].try_into().unwrap())))
                })
                .expect("a transfer to an Everstake tip account");
            assert!(tip.1 >= TIP_FLOOR[LANE_EVERSTAKE], "{} lamports, floor {}", tip.1, TIP_FLOOR[LANE_EVERSTAKE]);
            assert_eq!(tip.0, cfg.lanes[LANE_EVERSTAKE].tip_accounts[pick as usize], "rotated per send");
        }
    }

    #[test]
    fn a_lane_below_its_providers_floor_or_without_tip_accounts_is_off() {
        let mut lanes: [LaneSpec; LANES] = Default::default();
        lanes[LANE_EVERSTAKE] = everstake(500_000);
        settle(&mut lanes);
        assert!(!lanes[LANE_EVERSTAKE].enabled);
        assert!(lanes[LANE_EVERSTAKE].off_because.as_deref().unwrap().contains("below the provider's minimum of 1000000"));
        let mut lanes: [LaneSpec; LANES] = Default::default();
        lanes[LANE_EVERSTAKE] = LaneSpec { tip_accounts: &[], ..everstake(1_000_000) };
        settle(&mut lanes);
        assert_eq!(lanes[LANE_EVERSTAKE].off_because.as_deref(), Some("no LANE_EVERSTAKE_TIP_ACCOUNTS"));
    }

    #[test]
    fn a_plain_http_lane_over_its_per_second_limit_skips_the_copy_and_counts_it() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut h = HttpLane::new(HttpTarget { name: "triton", host: "127.0.0.1".into(), port, path: "/".into(), body: Body::JsonRpc, ping: "/".into(), ping_every_ns: u64::MAX })
            .with_limit(LANE_TRITON, 3);
        for c in h.conns.iter() {
            c.lock().reset(Some(TcpStream::connect(("127.0.0.1", port)).unwrap()));
        }
        let before = counters::LIMITED[LANE_TRITON].load(Ordering::Relaxed);
        for i in 0..3 {
            assert!(h.send(b"AAAA").is_some(), "write {i} is under the limit");
        }
        assert!(h.send(b"AAAA").is_none(), "the fourth in the same second is over it");
        assert!(counters::LIMITED[LANE_TRITON].load(Ordering::Relaxed) > before, "counted as limited");
        // A second later the window has room again.
        let now = now_ns() + 1_000_000_001;
        assert!(h.room(now));
    }

    #[test]
    fn everstakes_direct_request_carries_the_threaded_lanes_json() {
        use std::{io::Read, net::TcpListener};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut h = HttpLane::new(HttpTarget { name: "everstake", host: "fra-swqos.everstake.one".into(), port, path: "/".into(), body: Body::JsonRpc, ping: "/".into(), ping_every_ns: u64::MAX })
            .with_limit(LANE_EVERSTAKE, EVERSTAKE_PER_SEC - 1);
        h.conns[0].lock().reset(Some(TcpStream::connect(("127.0.0.1", port)).unwrap()));
        let (mut server, _) = l.accept().unwrap();
        let b64 = "AQIDBAUGBwg=";
        assert!(h.send(b64.as_bytes()).is_some());
        let mut got = vec![0u8; 4096];
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let n = server.read(&mut got).unwrap();
        let text = String::from_utf8_lossy(&got[..n]).to_string();
        let (head, body) = text.split_once("\r\n\r\n").expect("a complete request");
        assert!(head.starts_with("POST / HTTP/1.1\r\nHost: fra-swqos.everstake.one\r\n"), "{head}");
        assert!(head.contains(&format!("Content-Length: {}", body.len())), "{head}");
        let sent: serde_json::Value = serde_json::from_str(body).expect("JSON body");
        let threaded = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "sendTransaction", "params": [b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]});
        assert_eq!(sent, threaded, "the same request as the threaded lane sent");
    }
}
