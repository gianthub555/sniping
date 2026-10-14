use {
    crate::{
        config::{buy_lamports, lamports_to_sol, rules, trading_wallet, RPC_CLIENT, TRADE_SETTING},
        error::SniperError,
        instructions::types::Market,
        service::Submitted,
        utils::{
            auto_sell::{spawn_position, Position},
            csv_logger::{log_trade, TradeRecord},
            creator_history::{human_duration, PriorLaunches},
            dashboard::{activity, bump, notify, ActivityKind, STATS},
            latency::{latest_slot, wait_own_tx, ReadyMark},
            utils::{
                confirm_signature, fee_payer_lamport_change, get_associated_token_address, get_token_balance,
                ConfirmOutcome,
            },
        },
    },
    anyhow::Result,
    carbon_core::transaction::TransactionMetadata,
    colored::Colorize,
    log::{debug, info, warn},
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config,
    solana_signature::Signature,
    std::{
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, LazyLock, Mutex,
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
};


const SIGNATURE_TTL: Duration = Duration::from_secs(3);

const BUY_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// A bundle-only buy lands within the next Jito leaders' slots or is dropped: past this, it lost.
/// A bundle lands within a slot or two of its delivery, or never (Nozomi delivers it once to each
/// builder; slots are about 0.27 s): past this it lost the race. Kept short so the position slot
/// is free for the next launch; a late landing is still caught by `watch_late_fill`.
const BUNDLE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
/// A dropped bundle is looked for again at these times after the wait, in case it landed late.
const LATE_FILL_CHECKS: [Duration; 2] = [Duration::from_secs(7), Duration::from_secs(45)];




pub const EVENT_CPI_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

static SEEN_SIGNATURES: LazyLock<Mutex<HashMap<(Signature, Pubkey), Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CLAIMED_MINTS: LazyLock<Mutex<HashSet<Pubkey>>> = LazyLock::new(|| Mutex::new(HashSet::new()));




pub fn first_sighting(signature: &Signature, mint: &Pubkey) -> bool {
    let mut seen = SEEN_SIGNATURES.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    seen.retain(|_, at| now.duration_since(*at) < SIGNATURE_TTL);
    seen.insert((*signature, *mint), now).is_none()
}


pub fn claim_mint(mint: &Pubkey) -> bool {
    CLAIMED_MINTS.lock().unwrap_or_else(|e| e.into_inner()).insert(*mint)
}


pub struct RawInstruction {
    pub program_id: Pubkey,
    pub accounts: Vec<Pubkey>,
    pub data: Vec<u8>,
}



pub fn transaction_instructions(tx: &TransactionMetadata) -> Vec<RawInstruction> {
    let static_keys = tx.message.static_account_keys();
    let loaded = &tx.meta.loaded_addresses;
    let key_at = |index: u8| -> Option<Pubkey> {
        let index = index as usize;
        static_keys
            .get(index)
            .or_else(|| loaded.writable.get(index.checked_sub(static_keys.len())?))
            .or_else(|| loaded.readonly.get(index.checked_sub(static_keys.len() + loaded.writable.len())?))
            .copied()
    };
    let resolve = |program_id_index: u8, accounts: &[u8], data: &[u8]| -> Option<RawInstruction> {
        Some(RawInstruction {
            program_id: key_at(program_id_index)?,
            accounts: accounts.iter().map(|i| key_at(*i)).collect::<Option<Vec<_>>>()?,
            data: data.to_vec(),
        })
    };

    let mut out = Vec::new();
    for (index, ix) in tx.message.instructions().iter().enumerate() {
        out.extend(resolve(ix.program_id_index, &ix.accounts, &ix.data));
        let inner = tx.meta.inner_instructions.iter().flatten().filter(|group| group.index as usize == index);
        for inner_ix in inner.flat_map(|group| group.instructions.iter()) {
            let c = &inner_ix.instruction;
            out.extend(resolve(c.program_id_index, &c.accounts, &c.data));
        }
    }
    out
}



pub fn cpi_event_payloads(
    tx: &TransactionMetadata,
    program: &Pubkey,
    event_discriminator: &[u8; 8],
) -> Vec<Vec<u8>> {
    transaction_instructions(tx)
        .into_iter()
        .filter(|ix| ix.program_id == *program)
        .filter(|ix| ix.data.len() >= 16 && ix.data[..8] == EVENT_CPI_TAG && &ix.data[8..16] == event_discriminator)
        .map(|ix| ix.data[16..].to_vec())
        .collect()
}


pub struct SnipeRequest {
    pub market: Market,
    pub signature: Signature,
    
    pub slot: u64,
    pub detected_at: Instant,
    
    
    pub needs_refresh: bool,
    
    pub prior_launches: Option<PriorLaunches>,
    
    pub deployer: Option<Pubkey>,
    
    pub mayhem: bool,

    pub metadata_host: Option<String>,
    /// SOL of the first buy in the create transaction (pump.fun; 0 elsewhere).
    pub dev_buy_lamports: u64,
    /// SOL the create's fee payer held before the transaction, when the chain says.
    pub deployer_lamports: Option<u64>,
    /// A shred sighting: the create has not executed yet, so the balance of this fee payer is
    /// fetched from RPC before the buy (`deployer_lamports` is `None`).
    pub balance_from_rpc: Option<Pubkey>,
    /// The launch fits the fast-rug profile (`rules::fast_rug_profile`).
    pub fast_rug: bool,
    /// Seen before the create executed (shreds or the preprocessed stream): quoted after its
    /// bundle already, and the chain may not have the curve yet.
    pub pre_execution: bool,
    /// Wallets in the create's bundle besides the dev (None: the path did not see the bundle).
    pub bundle_wallets: Option<u32>,
    /// The coin's name is in capitals only (None: the path did not read the name).
    pub name_all_caps: Option<bool>,
    /// The entry facts the launch passed on (set by the rules check), for the rules that need
    /// the deployer's balance once it is fetched.
    pub facts: Option<rules::LaunchFacts>,
}

/// What a create costs its fee payer besides the dev buy (rent for the mint, curve and token
/// accounts, fees and tips), added back when RPC already shows the create executed.
pub(crate) const CREATE_COST_LAMPORTS: u64 = 30_000_000;

/// The create's bundle (the deployer's own buys right behind it) reaches the stream within about
/// 1 ms of the create; the quote waits this long after the sighting so the price guard includes
/// it. A bundle is atomic, so our buy always lands after it.
pub(crate) const BUNDLE_WINDOW: Duration = Duration::from_millis(3);

/// Buy launches first seen on the regular (executed) stream. Off: only the early streams
/// (Helius preprocessed, pumpfeed's push) buy; the regular stream still confirms landings, feeds
/// the sells and keeps the deployer records. On: the 28 Sep rules restored on 2026-10-07 (the
/// owner's instruction, paper only).
const BUY_FROM_REGULAR_STREAM: bool = true;

/// Dry run on a launch seen before it executed: the simulation waits (at most this long) until
/// the regular stream shows the create, or RPC finds no curve yet. The paper fill does not wait.
const PRE_EXECUTION_SIM_WAIT: Duration = Duration::from_millis(600);

static OPEN_POSITIONS: AtomicUsize = AtomicUsize::new(0);

/// Longest wait for the live hot path's decision on a launch (it decides 0.25 ms after the create
/// arrives; this path gets here milliseconds later).
const HOT_DECISION_WAIT: Duration = Duration::from_millis(20);

/// With the hot path live, whether this path still buys the launches the hot path leaves to it
/// (and those seen only on the regular stream). LIVE_SLOW_PATH_BUYS=false: only the hot path buys,
/// so every live buy goes through its lanes and is counted by the live guard.
pub fn slow_path_live_buys() -> bool {
    !crate::config::env_opt("LIVE_SLOW_PATH_BUYS").is_some_and(|v| v.eq_ignore_ascii_case("false"))
}


#[derive(Debug)]
pub struct PositionSlot(());

impl Drop for PositionSlot {
    fn drop(&mut self) {
        OPEN_POSITIONS.fetch_sub(1, Ordering::SeqCst);
        notify();
    }
}


pub fn open_position_count() -> usize {
    OPEN_POSITIONS.load(Ordering::SeqCst)
}

pub fn try_reserve_position(max: usize) -> Option<PositionSlot> {
    let mut current = OPEN_POSITIONS.load(Ordering::SeqCst);
    loop {
        if max > 0 && current >= max {
            return None;
        }
        match OPEN_POSITIONS.compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                notify();
                return Some(PositionSlot(()));
            }
            Err(actual) => current = actual,
        }
    }
}



pub fn spawn_snipe(request: SnipeRequest) {
    let mint = request.market.mint();
    if !first_sighting(&request.signature, &mint) {
        return;
    }
    bump(&STATS.detected);
    crate::utils::latency::probe_spawn_delay(request.detected_at);
    // Every followed deployer's launch is judged (bought, skipped or paused), so a deployer's
    // record (`rules::NO_POP_RECORD_*`) keeps up and can recover.
    let mut followed = false;
    if let Market::PumpFun(m) = &request.market {
        let keys: Vec<Pubkey> = request.deployer.into_iter().chain(std::iter::once(m.creator)).collect();
        let follow_kind = crate::utils::reference::follow(&keys, crate::utils::dashboard::now_unix()).map(|f| match f {
            rules::Follow::Recent => "recent",
            rules::Follow::Profitable => "profitable",
        });
        crate::store::launch_seen(
            &mint.to_string(),
            "pumpfun",
            "launch",
            request.slot,
            request.pre_execution,
            request.deployer.map(|d| d.to_string()),
            Some(m.creator.to_string()),
            request.dev_buy_lamports,
            follow_kind.is_some(),
            follow_kind,
        );
        if crate::utils::reference::follow(&keys, crate::utils::dashboard::now_unix()).is_some() {
            followed = true;
            super::launch_outcome::track(mint, request.deployer.unwrap_or(m.creator), m.creator, request.signature, request.slot);
            // The scorecard's funding and age facts for this deployer's next launches.
            let deployer = request.deployer.unwrap_or(m.creator);
            if crate::utils::deployer_wallet::get(&deployer).is_none() {
                tokio::spawn(crate::utils::deployer_wallet::ensure(deployer));
            }
            // A2: its buyers' positions, fees and tips (logs/rival_buys.csv), bought or not.
            let follow = crate::utils::reference::follow(&keys, crate::utils::dashboard::now_unix());
            let tier = crate::hot::fee::tier(request.dev_buy_lamports, follow, request.mayhem);
            crate::hot::positions::track_followed(mint, request.signature, request.slot, tier);
            crate::store::trades::note_context(
                &mint.to_string(),
                crate::store::trades::Context {
                    venue: "pumpfun",
                    deployer: Some(deployer.to_string()),
                    follow_kind,
                    tier: Some(tier.to_string()),
                    rent_lamports: crate::store::trades::rent(request.market.token_program() == crate::utils::utils::TOKEN_2022_PROGRAM_ID),
                },
            );
            crate::hot::distance::note_followed();
        }
    }
    // Buys come from the early streams only. A launch reaches here from the regular stream only
    // when no early stream handed it over first; bought from here it is ~4 ms later than an early
    // sighting plus a 3 ms bundle wait, and such buys landed in the launch slot 8 of 24 times
    // against 11 of 16 for early ones (live buys of 29-30 Sep). Its bookkeeping above still runs.
    if !request.pre_execution && !BUY_FROM_REGULAR_STREAM {
        if followed {
            info!("not bought {mint}: a followed deployer's launch reached only the regular stream (buys come from the early streams)");
            crate::store::launch_verdict(&mint.to_string(), "pumpfun", "skip", Some("reached only the regular stream".into()), None);
        }
        return;
    }
    if !crate::utils::control::trading_enabled() {
        crate::store::launch_verdict(&mint.to_string(), "pumpfun", "paused", None, None);
        return;
    }
    let dex_name = request.market.dex().name();
    let mint_text = mint.to_string();
    let skip = |reason: String| {
        bump(&STATS.filtered);
        info!("skip {mint}: {reason}");
        activity(ActivityKind::Skipped, Some(dex_name), Some(&mint_text), format!("Skipped: {reason}"));
    };
    let Market::PumpFun(m) = &request.market else {
        return skip(format!("{dex_name} is not traded (rules::DEXES)"));
    };
    if m.reserves.virtual_sol_reserves == 0 || m.reserves.virtual_token_reserves == 0 {
        return skip("non-standard bonding curve".into());
    }
    let keys: Vec<Pubkey> = request.deployer.into_iter().chain(std::iter::once(m.creator)).collect();
    let facts = rules::LaunchFacts {
        mayhem: request.mayhem,
        dev_buy_lamports: request.dev_buy_lamports,
        deployer_lamports: request.deployer_lamports,
        follow: crate::utils::reference::follow(&keys, crate::utils::dashboard::now_unix()),
        reference_profit_lamports: crate::utils::reference::profit(&keys),
        reference_trades: crate::utils::reference::trades(&keys),
        insider_lamports: Some(m.reserves.real_sol_reserves),
        risky_host: request.metadata_host.as_deref().map(rules::risky_host),
        secs_since_previous_launch: request.prior_launches.and_then(|p| p.secs_since_previous),
        launches_last_day: request.prior_launches.map(|p| p.last_day),
        bundle_wallets: request.bundle_wallets,
        name_all_caps: request.name_all_caps,
        deployer_record: crate::utils::deployer_record::record_of(&request.deployer.unwrap_or(m.creator), crate::utils::dashboard::now_unix()),
        scoring: {
            let now = crate::utils::dashboard::now_unix();
            let first = crate::utils::deployer_wallet::get(&request.deployer.unwrap_or(m.creator));
            rules::Scoring {
                launches_last_hour: request.prior_launches.map(|p| p.last_hour),
                wallet_first_bt: first.and_then(|f| f.first_bt),
                funded_sol: first.and_then(|f| f.funded_sol),
                earlier: crate::utils::launch_results::earlier(&m.creator, now),
                bundle_lamports: Some(m.reserves.real_sol_reserves.saturating_sub(request.dev_buy_lamports)),
                now,
            }
        },
    };
    let verdict = if request.balance_from_rpc.is_some() && request.deployer_lamports.is_none() {
        rules::verdict_before_balance(&facts)
    } else {
        rules::launch_verdict(&facts)
    };
    if crate::hot::engine::CONFIG.enabled {
        crate::hot::verify::note_slow(&mint, &facts, verdict.is_ok());
    }
    if crate::config::scorecard::enabled() && facts.follow.is_some() {
        log_score(&mint, request.deployer.unwrap_or(m.creator), m.creator, &facts, &verdict);
    }
    let follow = match verdict {
        Ok(follow) => follow,
        Err(reason) => return skip(reason),
    };
    // The snipe test: its four rules on what this path sees (the create and its bundle), then its
    // caps (read only; the entry is taken where the buy commits), in the hot path's order.
    if crate::utils::snipe_test::on() {
        let visible = crate::utils::snipe_test::Visible {
            dev_buy_lamports: request.dev_buy_lamports,
            bundle_wallets: request.bundle_wallets,
            real_sol_lamports: m.reserves.real_sol_reserves,
            complete: m.reserves.real_token_reserves == 0,
        };
        let now = crate::utils::dashboard::now_unix();
        let rules = if crate::utils::snipe_test::rules_on() { crate::utils::snipe_test::filter(&visible) } else { Ok(()) };
        if let Err(t) = rules.and_then(|()| crate::utils::snipe_test::check_caps(now)) {
            return skip(t.text().to_string());
        }
        let reserve = rules::lamports(rules::BALANCE_RESERVE_SOL);
        if crate::utils::dashboard::wallet_balance_lamports().is_some_and(|b| b.saturating_sub(reserve) < crate::utils::snipe_test::size_lamports()) {
            return skip(crate::utils::snipe_test::TestReject::Wallet.text().to_string());
        }
    }
    if open_position_count() >= rules::max_open_positions() {
        return skip(format!("{} positions already open (safety limit)", rules::max_open_positions()));
    }
    if !claim_mint(&mint) {
        return;
    }
    super::launch_watch::watch(mint, m.reserves, request.signature);
    activity(
        ActivityKind::Detected,
        Some(dex_name),
        Some(&mint_text),
        format!(
            "Followed deployer ({}), dev buy {:.2} SOL{}",
            match follow {
                rules::Follow::Recent => "reference traded it in the last 41 h",
                rules::Follow::Profitable => "made the reference 0.77+ SOL",
            },
            lamports_to_sol(request.dev_buy_lamports),
            if request.fast_rug { ", fast-rug profile" } else { "" }
        ) + &if crate::config::scorecard::enabled() && facts.deployer_lamports.is_some() {
            format!(", score {}", rules::launch_score(&facts))
        } else {
            String::new()
        },
    );
    tokio::spawn(async move {
        let dex = request.market.dex();
        let request = SnipeRequest { facts: Some(facts), ..request };
        if let Err(e) = snipe(request).await {
            warn!("snipe of {mint} on {dex} failed: {e:#}");
            activity(ActivityKind::BuyFailed, Some(dex.name()), Some(&mint.to_string()), format!("Buy not placed: {e:#}"));
        }
    });
}

/// The snipe test's record of a slow-path buy (paper, or live when this path buys): what the
/// decision saw.
fn snipe_entry(request: &SnipeRequest) -> crate::utils::snipe_test::Entry {
    use crate::utils::snipe_test::{Entry, Visible};
    let mut e = Entry {
        mint: request.market.mint().to_string(),
        mode: if TRADE_SETTING.dry_run { "paper" } else { "live" },
        path: "slow",
        create_slot: request.slot,
        decided_unix_ms: crate::utils::dashboard::now_unix_ms(),
        size_lamports: crate::utils::snipe_test::size_lamports(),
        token_2022: request.market.token_program() == crate::utils::utils::TOKEN_2022_PROGRAM_ID,
        mayhem: request.mayhem,
        ..Default::default()
    };
    if let Market::PumpFun(m) = &request.market {
        let r = m.reserves;
        e.deployer = request.deployer.unwrap_or(m.creator).to_string();
        e.visible = Visible {
            dev_buy_lamports: request.dev_buy_lamports,
            bundle_wallets: request.bundle_wallets,
            real_sol_lamports: r.real_sol_reserves,
            complete: r.real_token_reserves == 0,
        };
        e.bundle_lamports = r.real_sol_reserves.saturating_sub(request.dev_buy_lamports);
        e.price_after_bundle = if r.virtual_token_reserves > 0 { r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64 } else { 0.0 };
    }
    if let Some(f) = &request.facts {
        crate::utils::snipe_test::entry_of_facts(&mut e, f);
    }
    e
}

/// A followed launch's score, its facts' points and the verdict, to the database (`score`).
/// Without the deployer's balance yet (a shred sighting), the score is logged once RPC gave it
/// (`deployer_balance_skip`).
fn log_score(mint: &Pubkey, deployer: Pubkey, creator: Pubkey, facts: &rules::LaunchFacts, verdict: &Result<rules::Follow, String>) {
    if facts.deployer_lamports.is_none() {
        return;
    }
    let f = rules::score_facts(facts);
    let score = crate::config::scorecard::score(&f);
    if crate::store::db_on() {
        crate::store::write(crate::store::Row::Exec {
            sql: "UPDATE launches SET score = ?1 WHERE mint = ?2 AND venue = 'pumpfun'",
            params: vec![(score as i64).into(), mint.to_string().into()],
        });
    }
    let min = crate::config::scorecard::min();
    crate::utils::db::write(crate::utils::db::Write::Score {
        mint: mint.to_string(),
        t: facts.scoring.now as i64,
        deployer: deployer.to_string(),
        creator: creator.to_string(),
        path: "slow",
        score,
        threshold: min,
        passed: score >= min,
        verdict: match verdict {
            Ok(_) => "pass".into(),
            Err(reason) => reason.clone(),
        },
        facts: crate::config::scorecard::breakdown(&f),
    });
}

/// The create's receive time for the distance rule: the hot path's, else this path's sighting.
fn distance_t(request: &SnipeRequest) -> u64 {
    crate::hot::distance::create_seen(&request.market.mint().to_bytes()).unwrap_or_else(|| crate::hot::distance::ns_of(request.detected_at))
}

fn setting_leader_regions() -> &'static [String] {
    &crate::config::TRADE_SETTING.leader_regions
}

async fn snipe(mut request: SnipeRequest) -> Result<()> {
    let dex = request.market.dex();
    let mint = request.market.mint();
    crate::utils::latency::mark(&mint, "task");
    info!(
        "{} {dex} pool for {mint} (tx {}){}",
        "NEW".green().bold(),
        request.signature,
        if request.pre_execution { ", seen early (before it executed)" } else { "" }
    );

    if let Some(window) = crate::config::TRADE_SETTING.trading_hours {
        let minute = ((crate::utils::dashboard::now_unix() % 86_400) / 60) as u16;
        if !crate::config::trade_setting::in_window(window, minute) {
            bump(&STATS.filtered);
            info!("skip {mint}: outside trading hours ({:02}:{:02}-{:02}:{:02} UTC)", window.0 / 60, window.0 % 60, window.1 / 60, window.1 % 60);
            crate::store::launch_verdict(&mint.to_string(), "pumpfun", "skip", Some("outside trading hours".into()), None);
            return Ok(());
        }
    }
    if !setting_leader_regions().is_empty() {
        if let Some(reason) = crate::utils::leaders::far_producer(request.slot, setting_leader_regions()) {
            bump(&STATS.filtered);
            activity(ActivityKind::Skipped, Some(dex.name()), Some(&mint.to_string()), format!("Skipped: {reason}"));
            info!("skip {mint}: {reason}");
            return Ok(());
        }
    }
    // The leader class's route (Harmonic and unknown clients: log the launch, no buy), then the
    // distance rule, both as the hot path decides them; every skip is logged with the leader's
    // city, handshake time and client (logs/route_skips.csv).
    if crate::hot::fee::skips(request.slot) {
        let class = crate::hot::clients::class_of_slot(request.slot).map_or("unknown", |c| c.name());
        bump(&STATS.filtered);
        activity(ActivityKind::Skipped, Some(dex.name()), Some(&mint.to_string()), format!("Skipped: {class} leader (route: no buy)"));
        let v = crate::hot::distance::verdict(request.slot, distance_t(&request));
        crate::hot::distance::log_skip(&mint, true, request.slot, &format!("route: {class} leader"), v);
        return Ok(());
    }
    let v = crate::hot::distance::verdict(request.slot, distance_t(&request));
    crate::hot::distance::note_checked(v != crate::hot::distance::Verdict::Go);
    if v != crate::hot::distance::Verdict::Go {
        bump(&STATS.filtered);
        activity(ActivityKind::Skipped, Some(dex.name()), Some(&mint.to_string()), format!("Skipped: distance ({})", v.reason()));
        crate::hot::distance::log_skip(&mint, true, request.slot, &format!("distance: {}", v.reason()), v);
        return Ok(());
    }
    request.market.prepare(&RPC_CLIENT).await?;
    // Both lookups run at once: the deployer's age and, for a shred sighting, its balance.
    let (age_skip, balance_skip) = tokio::join!(wallet_age_skip(&request), deployer_balance_skip(&request));
    if let Some(reason) = balance_skip.or(age_skip) {
        bump(&STATS.filtered);
        activity(ActivityKind::Skipped, Some(dex.name()), Some(&mint.to_string()), format!("Skipped: {reason}"));
        info!("skip {mint}: {reason}");
        return Ok(());
    }
    crate::utils::latency::mark(&mint, "checks");
    // A shred sighting already quotes after the bundle; the regular stream needs a moment.
    if !request.pre_execution {
        crate::utils::latency::precise_sleep(BUNDLE_WINDOW.saturating_sub(request.detected_at.elapsed())).await;
    }
    // A live hot path buys the launches it decides; this path buys only those it left over.
    if crate::hot::live::is_live() {
        use crate::hot::live::Decision;
        match crate::hot::live::wait(&mint, HOT_DECISION_WAIT).await {
            Some(Decision::Bought) => {
                info!("{mint}: bought by the hot path");
                return Ok(());
            }
            Some(Decision::Declined) => {
                info!("{mint}: declined by the hot path (it decides the launches it can read)");
                return Ok(());
            }
            Some(Decision::Left) => {}
            None => info!("{mint}: no hot-path decision within {HOT_DECISION_WAIT:?}; this path decides"),
        }
        if !slow_path_live_buys() {
            info!("not bought {mint}: the hot path left it to this path, and LIVE_SLOW_PATH_BUYS=false (only the hot path buys)");
            return Ok(());
        }
    }
    let Some(slot) = try_reserve_position(rules::max_open_positions()) else {
        bump(&STATS.filtered);
        activity(
            ActivityKind::Skipped,
            Some(dex.name()),
            Some(&mint.to_string()),
            format!("Skipped: {} positions already open (safety limit)", rules::max_open_positions()),
        );
        return Ok(());
    };
    // The snipe test's caps: the entry is taken here, before the buy goes out.
    if crate::utils::snipe_test::on() {
        if let Err(t) = crate::utils::snipe_test::try_enter(crate::utils::dashboard::now_unix()) {
            bump(&STATS.filtered);
            activity(ActivityKind::Skipped, Some(dex.name()), Some(&mint.to_string()), format!("Skipped: {}", t.text()));
            return Ok(());
        }
        crate::utils::snipe_test::note_entry(snipe_entry(&request));
    }
    let mut slot = Some(slot);
    let _watch = super::launch_watch::WatchGuard(mint);
    apply_stream_view(&mut request.market);
    // One buy per launch: a failed buy is not sent again (the owner's rule, 27 Sep). Until then a
    // buy that failed on the creator's fee-sharing switch (2006) was rebuilt and sent once more.
    if let BuyAttempt::StaleAccounts = buy_once(&request, &mut slot).await? {
        info!("{mint}: the buy failed because the coin's creator moved to a fee-sharing account before it landed; no second attempt");
    }
    Ok(())
}






/// Brings a pump.fun launch up to the stream's latest curve state and creator before the buy is
/// built, so the quote and the creator-vault account match the chain.
fn apply_stream_view(market: &mut Market) {
    let Market::PumpFun(m) = market else { return };
    // The creator's move to fee sharing as an early feed showed it (in the create's bundle); the
    // regular stream's view below, when there is one, is the chain's.
    if let Some(creator) = super::fee_sharing::creator_of(&m.mint) {
        m.creator = creator;
    }
    let Some(view) = super::launch_watch::view(&m.mint) else { return };
    if let Some(creator) = view.creator {
        m.creator = creator;
    }
    m.reserves = view.latest;
}


/// For a shred sighting: the fee payer's balance before the create. RPC may already have
/// executed the create by the time it answers; then the create's cost is added back.
async fn deployer_balance_skip(request: &SnipeRequest) -> Option<String> {
    let payer = request.balance_from_rpc.filter(|_| request.deployer_lamports.is_none())?;
    let commitment = solana_commitment_config::CommitmentConfig::processed();
    let signatures = [request.signature];
    let (balance, status) = tokio::join!(
        RPC_CLIENT.get_balance_with_commitment(&payer, commitment),
        RPC_CLIENT.get_signature_statuses(&signatures)
    );
    let balance = match balance {
        Ok(b) => b.value,
        Err(e) => return Some(format!("deployer balance unavailable: {e}")),
    };
    let executed = status.ok().is_some_and(|s| s.value.first().is_some_and(|v| v.is_some()));
    let before = if executed { balance + request.dev_buy_lamports + CREATE_COST_LAMPORTS } else { balance };
    // Every rule again with the balance in (the dump-risk points count the creator's SOL).
    match request.facts {
        Some(facts) => {
            let facts = rules::LaunchFacts { deployer_lamports: Some(before), ..facts };
            let verdict = rules::launch_verdict(&facts);
            if crate::config::scorecard::enabled() && facts.follow.is_some() {
                if let crate::instructions::types::Market::PumpFun(m) = &request.market {
                    log_score(&request.market.mint(), request.deployer.unwrap_or(m.creator), m.creator, &facts, &verdict);
                }
            }
            verdict.err()
        }
        None => rules::deployer_balance_verdict(Some(before)).err(),
    }
}

async fn wallet_age_skip(request: &SnipeRequest) -> Option<String> {
    // With the scorecard on, the wallet's age is one of the score's facts, not a rule.
    if crate::config::scorecard::enabled() {
        return None;
    }
    let min_age_secs = rules::DEPLOYER_MIN_AGE_SECS;
    let deployer = request.deployer?;
    let age_known = request.prior_launches.and_then(|p| p.first_seen_secs_ago).is_some_and(|secs| secs >= min_age_secs);
    if age_known {
        return None;
    }
    match creator_wallet_age_secs(&deployer).await {
        Ok(Some(age)) if age < min_age_secs => Some(format!(
            "deployer wallet is only {} old (minimum {})",
            human_duration(age),
            human_duration(min_age_secs)
        )),
        Ok(_) => None,
        Err(e) => {
            warn!("could not check the age of deployer {deployer}: {e:#}");
            None
        }
    }
}



/// The wallet-age rule from what is already known, without RPC (the hot path): Some(true) skip,
/// Some(false) pass, None when only RPC could tell (`wallet_age_skip` then asks it).
pub fn wallet_age_skip_cached(deployer: &Pubkey, prior: Option<PriorLaunches>, now: u64) -> Option<bool> {
    let min_age_secs = rules::DEPLOYER_MIN_AGE_SECS;
    if prior.and_then(|p| p.first_seen_secs_ago).is_some_and(|secs| secs >= min_age_secs) {
        return Some(false);
    }
    if OLD_WALLETS.lock().unwrap_or_else(|e| e.into_inner()).contains(deployer) {
        return Some(false);
    }
    let first = YOUNG_WALLETS.lock().unwrap_or_else(|e| e.into_inner()).get(deployer).copied()?;
    (now.saturating_sub(first) < min_age_secs).then_some(true)
}

const WALLET_AGE_PAGE: usize = 100;


static OLD_WALLETS: LazyLock<Mutex<HashSet<Pubkey>>> = LazyLock::new(Default::default);

/// Wallets younger than the minimum age, with the unix time of their oldest transaction, so the
/// buy path can judge them without RPC until they come of age.
static YOUNG_WALLETS: LazyLock<Mutex<HashMap<Pubkey, u64>>> = LazyLock::new(Default::default);

/// Works out deployer ages ahead of any buy (the followed deployers and creators), so the
/// wallet-age rule costs the buy path no RPC round trip (it took about 17 ms).
pub fn prefetch_wallet_ages(wallets: Vec<Pubkey>) {
    tokio::spawn(async move {
        let todo: Vec<Pubkey> = {
            let old = OLD_WALLETS.lock().unwrap_or_else(|e| e.into_inner());
            wallets.into_iter().filter(|w| !old.contains(w)).collect()
        };
        let count = todo.len();
        futures::StreamExt::for_each_concurrent(futures::stream::iter(todo), 4, |wallet| async move {
            if let Err(e) = creator_wallet_age_secs(&wallet).await {
                debug!("wallet age of {wallet}: {e:#}");
            }
        })
        .await;
        if count > 1 {
            info!("wallet ages known ahead of time for {count} followed deployers and creators");
        }
    });
}



async fn creator_wallet_age_secs(wallet: &Pubkey) -> Result<Option<u64>> {
    if OLD_WALLETS.lock().unwrap_or_else(|e| e.into_inner()).contains(wallet) {
        return Ok(None);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if let Some(first) = YOUNG_WALLETS.lock().unwrap_or_else(|e| e.into_inner()).get(wallet).copied() {
        let age = now.saturating_sub(first);
        if age < rules::DEPLOYER_MIN_AGE_SECS {
            return Ok(Some(age));
        }
    }
    let config = GetConfirmedSignaturesForAddress2Config {
        commitment: Some(solana_commitment_config::CommitmentConfig::confirmed()),
        limit: Some(WALLET_AGE_PAGE),
        ..Default::default()
    };
    let signatures = RPC_CLIENT.get_signatures_for_address_with_config(wallet, config).await?;
    let age = if signatures.len() >= WALLET_AGE_PAGE {
        None
    } else {
        let oldest = signatures.last().and_then(|s| s.block_time).unwrap_or(now as i64);
        Some(now.saturating_sub(oldest.max(0) as u64))
    };
    let min_age = rules::DEPLOYER_MIN_AGE_SECS;
    match age {
        Some(a) if a < min_age => {
            YOUNG_WALLETS.lock().unwrap_or_else(|e| e.into_inner()).insert(*wallet, now - a);
        }
        _ => {
            YOUNG_WALLETS.lock().unwrap_or_else(|e| e.into_inner()).remove(wallet);
            OLD_WALLETS.lock().unwrap_or_else(|e| e.into_inner()).insert(*wallet);
        }
    }
    Ok(age)
}


/// The stake, scaled by the dev buy, capped by what the wallet can spend.
fn bet_size(request: &SnipeRequest, stake: u64) -> (u64, &'static str) {
    // The snipe test: its size, never scaled, never cut to what the wallet holds (the decision
    // checked the wallet).
    if crate::utils::snipe_test::on() {
        return (crate::utils::snipe_test::size_lamports(), "snipe test");
    }
    let lamports = rules::bet_lamports(stake, request.dev_buy_lamports);
    affordable(lamports, if lamports < stake { "small" } else { "standard" })
}

fn affordable(lamports: u64, tier: &'static str) -> (u64, &'static str) {
    let reserve = rules::lamports(rules::BALANCE_RESERVE_SOL);
    let Some(balance) = crate::utils::dashboard::wallet_balance_lamports() else {
        return (lamports, tier);
    };
    let spendable = balance.saturating_sub(reserve);
    if lamports <= spendable {
        return (lamports, tier);
    }
    if spendable == 0 {
        warn!(
            "wallet holds {:.4} SOL, less than the {:.4} SOL reserve for fees, tips and rent; buying anyway may fail",
            lamports_to_sol(balance),
            lamports_to_sol(reserve)
        );
        return (lamports, tier);
    }
    warn!(
        "{tier} bet of {:.4} SOL is more than the wallet can spend ({:.4} SOL of {:.4} SOL balance); buying {:.4} SOL instead",
        lamports_to_sol(lamports),
        lamports_to_sol(spendable),
        lamports_to_sol(balance),
        lamports_to_sol(spendable)
    );
    (spendable, "capped")
}


const OWN_TX_WAIT: Duration = Duration::from_secs(2);


struct BuyTiming {
    launch_slot: u64,
    
    ready_slot: Option<u64>,
    detect_to_ready_ms: f64,
    
    landed_slot: Option<u64>,
    
    launch_to_landed_ms: Option<u64>,
}

impl BuyTiming {
    fn ready(request: &SnipeRequest, ready: ReadyMark) -> Self {
        Self {
            launch_slot: request.slot,
            ready_slot: ready.stream_slot,
            detect_to_ready_ms: ready.at.saturating_duration_since(request.detected_at).as_secs_f64() * 1000.0,
            landed_slot: None,
            launch_to_landed_ms: None,
        }
    }

    fn apply(&self, record: &mut TradeRecord) {
        record.launch_slot = Some(self.launch_slot);
        record.ready_slot = self.ready_slot;
        record.landed_slot = self.landed_slot;
        record.detect_to_ready_ms = Some(self.detect_to_ready_ms);
        record.launch_to_landed_ms = self.launch_to_landed_ms;
    }

    fn summary(&self) -> String {
        let offset = |slot: u64| format!("{:+}", slot as i64 - self.launch_slot as i64);
        let mut text = format!("ready {:.2} ms after the launch arrived", self.detect_to_ready_ms);
        if let Some(slot) = self.ready_slot {
            text.push_str(&format!(" (chain at launch slot {})", offset(slot)));
        }
        match (self.landed_slot, self.launch_to_landed_ms) {
            (Some(slot), ms) => {
                text.push_str(&format!(", landed in launch slot {}", offset(slot)));
                if let Some(ms) = ms {
                    text.push_str(&format!(", {ms} ms after the launch"));
                }
            }
            (None, Some(ms)) => text.push_str(&format!(", paper buy lands {ms} ms after the launch")),
            (None, None) => {}
        }
        text
    }
}

/// Where a paper buy landed: when, and the stream's view of the curve at that moment.
struct PaperLanding {
    at: Instant,
    view: Option<super::launch_watch::StreamView>,
    /// Where the paper buy landed (slot, block position), when the fill is by position.
    fill: Option<(u64, u64)>,
}

/// Where real buys from this server landed, as (slots after the launch slot, position): in the
/// launch slot the position counts transactions after the create; in the next slot it is the
/// block index. 47 live buys of 26 Sep 2026 (plain Nozomi sends and Nozomi bundles, tips
/// 0.001-0.005 SOL, 60% in the launch slot). A paper buy draws one and is priced from the trades
/// that landed before it, the order that decides who bought first.
const LIVE_FILL_POSITIONS: &[(u64, u64)] = &[
    (0, 70), (0, 83), (0, 91), (0, 93), (0, 95), (0, 99), (0, 137), (0, 140), (0, 178), (0, 186), (0, 190), (0, 192), (0, 204),
    (0, 239), (0, 262), (0, 263), (0, 278), (0, 283), (0, 332), (0, 355), (0, 359), (0, 461), (0, 504), (0, 527), (0, 588),
    (0, 672), (0, 743), (0, 848), (0, 1036),
    (1, 1), (1, 3), (1, 24), (1, 27), (1, 36), (1, 64), (1, 114), (1, 193), (1, 211), (1, 222), (1, 315), (1, 320), (1, 323),
    (1, 330), (1, 359), (1, 474), (1, 496), (1, 688),
];
/// Longest wait for the stream to get past the fill slot (slots are about 0.27 s).
const PAPER_FILL_WAIT: Duration = Duration::from_millis(1_500);

pub fn draw_fill_position() -> (u64, u64) {
    use rand::seq::SliceRandom;
    *LIVE_FILL_POSITIONS.choose(&mut rand::thread_rng()).expect("positions")
}

pub fn fill_positions_summary() -> String {
    let next = LIVE_FILL_POSITIONS.iter().filter(|(s, _)| *s > 0).count();
    format!("a block position drawn from {} real landings ({}% in the next slot)", LIVE_FILL_POSITIONS.len(), 100 * next / LIVE_FILL_POSITIONS.len())
}

fn millis_between(from: Instant, to: Instant) -> u64 {
    to.saturating_duration_since(from).as_millis() as u64
}


fn slippage_context(err: &str, mint: &Pubkey) -> String {
    if !err.contains("Custom(6042)") {
        return String::new();
    }
    match super::launch_watch::view(mint) {
        Some(v) if v.other_buys > 0 => format!(
            " (price moved: {} other buy{} for {:.2} SOL, {:+.0}% so far)",
            v.other_buys,
            if v.other_buys == 1 { "" } else { "s" },
            lamports_to_sol(v.other_buy_lamports),
            v.price_move_percent()
        ),
        _ => " (price moved: other buys landed before this one)".to_string(),
    }
}




enum BuyAttempt {
    Done,
    /// The accounts changed under the buy (the creator's fee-sharing switch); it is not retried.
    StaleAccounts,
}


const STALE_ACCOUNTS_ERROR: &str = "Custom(2006)";

/// pump.fun's slippage error: the price moved past the buy's guard.
const SLIPPAGE_ERROR: &str = "Custom(6042)";


async fn buy_once(request: &SnipeRequest, slot: &mut Option<PositionSlot>) -> Result<BuyAttempt> {
    let setting = &*TRADE_SETTING;
    let dex = request.market.dex();
    let mint = request.market.mint();
    {
        if let Some(latest) = latest_slot() {
            if latest > request.slot + rules::MAX_LAUNCH_AGE_SLOTS {
                bump(&STATS.filtered);
                activity(
                    ActivityKind::Skipped,
                    Some(dex.name()),
                    Some(&mint.to_string()),
                    format!("Skipped: chain is {} slots past the launch (max {})", latest - request.slot, rules::MAX_LAUNCH_AGE_SLOTS),
                );
                info!("skip {mint}: {} slots past the launch (rules::MAX_LAUNCH_AGE_SLOTS)", latest - request.slot);
                return Ok(BuyAttempt::Done);
            }
        }
    }
    
    let (buy_lamports, tier) = bet_size(request, buy_lamports());
    let tier_note = if tier == "standard" { String::new() } else { format!(" ({tier} bet)") };
    
    
    let entry_note = match super::launch_watch::view(&mint) {
        Some(view) => format!(
            "{} other buy{} ({:.2} SOL), {} sell{}, price {:+.0}%, dev buy {:.2} SOL",
            view.other_buys,
            if view.other_buys == 1 { "" } else { "s" },
            lamports_to_sol(view.other_buy_lamports),
            view.sells,
            if view.sells == 1 { "" } else { "s" },
            view.price_move_percent(),
            lamports_to_sol(view.launch.real_sol_reserves)
        ),
        None => format!("no stream view, dev buy {:.2} SOL", lamports_to_sol(request.market.sol_liquidity())),
    };
    let expected = request.market.quote_buy(buy_lamports)?;
    if expected == 0 {
        return Err(SniperError::ZeroOutput(mint).into());
    }
    crate::utils::latency::mark(&mint, "quote");
    
    
    
    
    // The guard: at most rules::BUY_AHEAD_MAX_SOL put into the curve by others ahead of us (one
    // fast buyer; the dev's bundle is already in the quote), or the rules::BUY_SLIPPAGE_PERCENT price guard.
    let min_out = match (rules::BUY_AHEAD_MAX_SOL, &request.market) {
        (Some(sol), Market::PumpFun(m)) => {
            // `after_buy` takes SOL before the fee; the cap is SOL into the curve.
            let gross = (rules::lamports(sol) as u128 * 10_125 / 10_000) as u64;
            m.after_buy(gross).and_then(|ahead| ahead.quote_buy(buy_lamports)).unwrap_or_else(|_| setting.buy_min_out(expected))
        }
        _ => setting.buy_min_out(expected),
    };
    let instructions = request.market.buy_instructions(&trading_wallet(), buy_lamports, min_out)?;
    crate::utils::latency::mark(&mint, "instructions");

    let mut record = TradeRecord {
        action: "BUY",
        dex: dex.to_string(),
        mint: mint.to_string(),
        sol_in: Some(lamports_to_sol(buy_lamports)),
        token_amount: Some(expected),
        reason: if tier == "standard" { "SNIPE".to_string() } else { format!("SNIPE {tier}") },
        ..Default::default()
    };

    
    
    let _pending_fill = super::position_feed::PendingFill::new();
    // Record the curve's trades from before the send, so the exit sees the buyers behind the fill.
    let feed = matches!(request.market, Market::PumpFun(_)).then(|| Arc::new(super::position_feed::subscribe(mint)));
    let own_token_account = get_associated_token_address(&trading_wallet(), &mint, &request.market.token_program());
    // A paper buy lands where a live one would: on the stream's curve a live landing delay after
    // sending. The simulation reads RPC state about 190 ms after the launch, well after that.
    let pre_ready = crate::utils::latency::ReadyMark::now();
    let launch_slot = request.slot;
    let paper_landing = setting.dry_run.then(|| {
        let by_position = setting.dry_run_fill_by_position.then(draw_fill_position);
        let delay = setting.dry_run_buy_latency.draw();
        (by_position.is_some() || delay.is_some()).then(|| {
            let feed = feed.clone();
            tokio::spawn(async move {
                let (at, fill) = match by_position {
                    // Fill by block position: once the stream is past the fill slot, the buy sits
                    // after the trades that landed before its position.
                    // The buy lands when the stream reaches its position: every trade ahead of it
                    // has been seen, and a live bot would see its own fill then (its exit clock
                    // starts there too; waiting for the whole slot started paper clocks up to a
                    // slot late, 26 Sep: 2TB9ZxHa's check caught a wave a live one would not).
                    Some((offset, position)) => {
                        let fill_slot = launch_slot + offset;
                        let index = || {
                            if offset == 0 {
                                super::launch_watch::view(&mint).and_then(|v| v.launch_index).map(|(_, i)| i + position)
                            } else {
                                Some(position)
                            }
                        };
                        crate::utils::latency::wait_for_stream(fill_slot, index, PAPER_FILL_WAIT).await;
                        (Instant::now(), index().map(|i| (fill_slot, i)))
                    }
                    None => {
                        let at = Instant::now() + delay.unwrap_or_default();
                        tokio::time::sleep_until(at.into()).await;
                        (at, None)
                    }
                };
                if let Some(guard) = &feed {
                    guard.feed.mark_paper_fill(fill.map(|f| f.0).or(crate::utils::latency::latest_slot()), fill.map(|f| f.1));
                }
                PaperLanding { at, view: super::launch_watch::view(&mint), fill }
            })
        })
    }).flatten();
    if setting.dry_run && request.pre_execution {
        super::shred_feed::wait_regular(&mint, PRE_EXECUTION_SIM_WAIT).await;
    }
    match crate::service::submit_watching(crate::service::SendProfile::Buy, instructions, Some(request.slot), Some(own_token_account)).await? {
        Submitted::Simulated { units_consumed, error, logs, ready, watched_tokens, .. } => {
            // A live buy would have gone out before the wait for the regular stream.
            let ready = if request.pre_execution { pre_ready } else { ready };
            let landing = match paper_landing {
                Some(task) => task.await.ok(),
                None => None,
            };
            if let Some((slot, index)) = landing.as_ref().and_then(|l| l.fill) {
                crate::utils::snipe_test::note_paper_landing(&mint.to_string(), slot, index);
            }
            let mut timing = BuyTiming::ready(request, ready);
            timing.launch_to_landed_ms = landing.as_ref().map(|l| millis_between(request.detected_at, l.at));
            timing.apply(&mut record);
            info!("{} {mint}: {} [{}, signed]", "TIMING".blue().bold(), timing.summary(), crate::utils::latency::take_marks(&mint, request.detected_at));

            let mut error = error;
            let mut lost_race = error.as_deref().is_some_and(|e| e.contains(SLIPPAGE_ERROR));
            let mut paper_market = request.market.clone();
            let paper_tokens = match &landing {
                Some(landing) => {
                    // The simulation ran on later state than a live buy meets: its slippage check
                    // and the creator it saw are the chain's after the landing, not at it. A
                    // deployer's switch to a fee-sharing account (always its own transaction)
                    // fails the buy only if the stream showed it before the landing.
                    let switched = match (&request.market, landing.view.as_ref()) {
                        (Market::PumpFun(m), Some(v)) => match (landing.fill, v.creator_migration) {
                            // By position: only a move that landed before the fill fails it.
                            (Some(fill), Some((slot, index, creator))) => (slot, index) < fill && creator != m.creator,
                            (Some(_), None) => false,
                            (None, _) => v.creator.is_some_and(|c| c != m.creator),
                        },
                        _ => false,
                    };
                    let stale = error.as_deref().is_some_and(|e| e.contains(STALE_ACCOUNTS_ERROR));
                    // A 2006 the simulation met because the creator moved after the landing would
                    // not fail the live buy: cleared. With no move seen at all its cause is
                    // something else (a seeds mismatch live would hit too): kept as a failed buy.
                    let moved_later = match (&request.market, landing.view.as_ref()) {
                        (Market::PumpFun(m), Some(v)) => v.creator_migration.is_some() || v.creator.is_some_and(|c| c != m.creator),
                        _ => false,
                    };
                    if lost_race || (stale && !switched && moved_later) {
                        error = None;
                    }
                    if switched && error.is_none() {
                        error = Some(format!("{STALE_ACCOUNTS_ERROR}: the creator moved to a fee-sharing account before the buy landed"));
                    }
                    // The curve the buy meets: at its block position when the fill is by
                    // position, else the stream's latest at the landing time.
                    let met = landing.view.as_ref().map(|v| match landing.fill {
                        Some((slot, index)) => v.state_before(slot, index),
                        None => (v.latest, v.other_buys, v.other_buy_lamports),
                    });
                    if let (Market::PumpFun(m), Some((reserves, _, _))) = (&mut paper_market, &met) {
                        m.reserves = *reserves;
                    }
                    let tokens = paper_market.quote_buy(buy_lamports).unwrap_or(0);
                    let ahead = met
                        .as_ref()
                        .map(|(reserves, buys, lamports)| {
                            let price = |r: &crate::instructions::pumpfun_buy_ix::CurveReserves| r.virtual_sol_reserves as f64 / r.virtual_token_reserves.max(1) as f64;
                            let launch = landing.view.as_ref().map(|v| price(&v.launch)).unwrap_or(0.0);
                            let moved = if launch > 0.0 { (price(reserves) / launch - 1.0) * 100.0 } else { 0.0 };
                            let at = landing
                                .fill
                                .map(|(s, i)| {
                                    let create = landing.view.as_ref().and_then(|v| v.launch_index).map(|(_, c)| c).unwrap_or(0);
                                    if s == launch_slot { format!(" at {} after the create", i - create) } else { format!(" at slot +{} index {i}", s - launch_slot) }
                                })
                                .unwrap_or_default();
                            format!("; {} other buy{} for {:.2} SOL landed first, price {moved:+.1}%{at}", buys, if *buys == 1 { "" } else { "s" }, lamports_to_sol(*lamports))
                        })
                        .unwrap_or_default();
                    lost_race = error.is_none() && tokens < min_out;
                    if lost_race {
                        error = Some(format!("{} before the buy landed{ahead}", rules::buy_guard_text()));
                    } else if error.is_none() {
                        info!(
                            "{mint}: paper fill {tokens} tokens at the landing ({:+.1}% vs the {expected} quoted at decision{ahead})",
                            (tokens as f64 / expected.max(1) as f64 - 1.0) * 100.0
                        );
                    }
                    tokens
                }
                None => {
                    let tokens = match watched_tokens {
                        Some(tokens) if error.is_none() && tokens > 0 && tokens <= expected + expected / 1000 => {
                            if tokens < expected {
                                info!(
                                    "{mint}: simulated fill {tokens} tokens ({:+.1}% vs the {expected} quoted at decision: others bought ahead)",
                                    (tokens as f64 / expected.max(1) as f64 - 1.0) * 100.0
                                );
                            }
                            tokens
                        }
                        _ => expected,
                    };
                    if error.is_none() && tokens < min_out {
                        lost_race = true;
                        error = Some(format!("{} before a live buy lands", rules::buy_guard_text()));
                    }
                    tokens
                }
            };
            if setting.dry_run_charge_fees {
                record.wallet_sol_change = match &error {
                    None => {
                        let cost = crate::service::modeled_cost_lamports(crate::service::SendProfile::Buy);
                        Some(-((buy_lamports + cost) as f64) / 1e9)
                    }
                    // A live buy that fails on chain still pays its fees; the tip reverts with it.
                    Some(err) if lost_race || err.contains(STALE_ACCOUNTS_ERROR) => {
                        Some(-(crate::service::modeled_failed_cost_lamports(crate::service::SendProfile::Buy) as f64) / 1e9)
                    }
                    Some(_) => None,
                };
            }
            record.token_amount = Some(paper_tokens);
            match &error {
                None => {
                    record.status = "simulated".into();
                    activity(
                        ActivityKind::Bought,
                        Some(dex.name()),
                        Some(&mint.to_string()),
                        format!(
                            "Simulated buy: {paper_tokens} tokens for {} SOL{tier_note} · {}",
                            lamports_to_sol(buy_lamports),
                            timing.summary()
                        ),
                    );
                    info!(
                        "{} {mint} on {dex}: {paper_tokens} tokens for {} SOL ({} CU) · {entry_note}",
                        "SIMULATED BUY".cyan().bold(),
                        lamports_to_sol(buy_lamports),
                        units_consumed.unwrap_or_default()
                    );
                }
                Some(err) => {
                    
                    
                    record.status = if lost_race {
                        format!("lost race: {}", rules::buy_guard_text())
                    } else {
                        format!("simulation failed: {err}")
                    };
                    let context = if landing.is_some() { String::new() } else { slippage_context(err, &mint) };
                    activity(
                        ActivityKind::BuyFailed,
                        Some(dex.name()),
                        Some(&mint.to_string()),
                        format!("Simulated buy failed: {err}{context} · {}", timing.summary()),
                    );
                    warn!("simulated buy of {mint} failed: {err}");
                    for line in logs.iter().rev().take(8).rev() {
                        debug!("  {line}");
                    }
                }
            }
            log_trade(&record);
            bump(if error.is_none() { &STATS.buys_ok } else { &STATS.buys_failed });
            // A paper fill or a race lost on chain: both would have been included in a block.
            crate::utils::live_guard::on_buy_result(error.is_none(), true, "paper");
            match error {
                None => {
                    
                    
                    let paper_sol_in_curve = match &paper_market {
                        Market::PumpFun(m) => m
                            .after_buy(buy_lamports)
                            .map(|after| after.reserves.virtual_sol_reserves.saturating_sub(m.reserves.virtual_sol_reserves))
                            .unwrap_or(0),
                        _ => 0,
                    };
                    spawn_position(Position {
                        market: request.market.clone(),
                        entry_lamports: buy_lamports,
                        paper_token_amount: Some(paper_tokens),
                        paper_sol_in_curve,
                        known_tokens: None,
                        opened_at: landing.as_ref().map_or_else(Instant::now, |l| l.at),
                        slot: slot.take().map(Arc::new),
                        feed: feed.clone(),
                        fast_rug: request.fast_rug,
                    });
                    Ok(BuyAttempt::Done)
                }
                Some(err) if err.contains(STALE_ACCOUNTS_ERROR) => Ok(BuyAttempt::StaleAccounts),
                Some(_) => Ok(BuyAttempt::Done),
            }
        }
        Submitted::Sent { signature, alternates, ready } => {
            let mut timing = BuyTiming::ready(request, ready);
            info!(
                "{} {mint} on {dex}: {} SOL, min {min_out} tokens, ready {:.2} ms and sent {} ms after detection ({signature})",
                "BUY SENT".yellow().bold(),
                lamports_to_sol(buy_lamports),
                timing.detect_to_ready_ms,
                request.detected_at.elapsed().as_millis()
            );
            let bundles = crate::service::buys_are_bundles();
            // No copy delivered in time costs nothing (a dropped bundle, or no fanned-out copy landed).
            let free_timeout = crate::service::buy_timeout_is_free();
            let confirm_timeout = if bundles { BUNDLE_CONFIRM_TIMEOUT } else { BUY_CONFIRM_TIMEOUT };
            let (outcome, fill, signature) = await_landing_any(signature, &alternates, confirm_timeout).await?;
            record.signature = signature.to_string();
            if let ConfirmOutcome::Confirmed { slot } | ConfirmOutcome::Failed { slot, .. } = &outcome {
                timing.landed_slot = Some(*slot);
                match fill {
                    Some(fill) => timing.launch_to_landed_ms = Some(millis_between(request.detected_at, fill.seen_at)),
                    None => {
                        let wait = if matches!(outcome, ConfirmOutcome::Confirmed { .. }) { OWN_TX_WAIT } else { Duration::ZERO };
                        if let Some((_, seen_at)) = wait_own_tx(&signature, wait).await {
                            timing.launch_to_landed_ms = Some(millis_between(request.detected_at, seen_at));
                        }
                    }
                }
            }
            timing.apply(&mut record);
            info!("{} {mint}: {} [{}, signed]", "TIMING".blue().bold(), timing.summary(), crate::utils::latency::take_marks(&mint, request.detected_at));
            
            
            
            let balance = match fill {
                Some(fill) => fill.tokens,
                None => {
                    let token_account = get_associated_token_address(&trading_wallet(), &mint, &request.market.token_program());
                    get_token_balance(&RPC_CLIENT, &token_account).await.unwrap_or(0)
                }
            };
            if balance > 0 {
                spawn_position(Position {
                    market: request.market.clone(),
                    entry_lamports: buy_lamports,
                    paper_token_amount: None,
                    paper_sol_in_curve: 0,
                    known_tokens: Some(balance),
                    opened_at: Instant::now(),
                    slot: slot.take().map(Arc::new),
                    feed: feed.clone(),
                    fast_rug: request.fast_rug,
                });
            }
            record.token_amount = Some(balance);
            record.wallet_sol_change = match (fill, &outcome) {
                (Some(fill), _) => Some(fill.wallet_lamports as f64 / 1e9),
                (None, ConfirmOutcome::TimedOut) => None,
                (None, _) => fee_payer_lamport_change(&RPC_CLIENT, &signature).await.map(|lamports| lamports as f64 / 1e9).ok(),
            };
            bump(if balance > 0 { &STATS.buys_ok } else { &STATS.buys_failed });
            let mint_text = mint.to_string();
            match &outcome {
                ConfirmOutcome::Confirmed { .. } => activity(
                    ActivityKind::Bought,
                    Some(dex.name()),
                    Some(&mint_text),
                    format!("Bought {balance} tokens for {} SOL{tier_note} · {}", lamports_to_sol(buy_lamports), timing.summary()),
                ),
                ConfirmOutcome::Failed { error, .. } => activity(
                    ActivityKind::BuyFailed,
                    Some(dex.name()),
                    Some(&mint_text),
                    format!("Buy failed on chain: {error} · {}", timing.summary()),
                ),
                ConfirmOutcome::TimedOut if free_timeout => activity(
                    ActivityKind::BuyFailed,
                    Some(dex.name()),
                    Some(&mint_text),
                    if bundles {
                        "Buy not included: the bundle was dropped (it would have failed or lost the auction), no fee"
                    } else {
                        "Buy not included: no copy landed in time, no fee"
                    },
                ),
                ConfirmOutcome::TimedOut => {
                    activity(ActivityKind::BuyFailed, Some(dex.name()), Some(&mint_text), "Buy not confirmed in time")
                }
            }
            record.status = match &outcome {
                ConfirmOutcome::Confirmed { slot } => {
                    let via = crate::service::route_of(&signature).map(|s| format!(", landed via {s:?}")).unwrap_or_default();
                    info!("{} {mint}: {balance} tokens (slot {slot}{via})", "BOUGHT".green().bold());
                    "confirmed".into()
                }
                ConfirmOutcome::Failed { error, .. } => {
                    let via = crate::service::route_of(&signature).map(|s| format!(" (landed via {s:?})")).unwrap_or_default();
                    warn!("buy {signature} failed on chain{via}: {error}");
                    format!("failed: {error}")
                }
                ConfirmOutcome::TimedOut if free_timeout && balance == 0 => {
                    info!("{mint}: buy {signature} not included within {confirm_timeout:?}: lost the race, no fee");
                    watch_late_fill(request.market.clone(), buy_lamports, feed.clone(), request.fast_rug);
                    if bundles { "not included: bundle dropped, no fee".into() } else { "not included: no copy landed, no fee".into() }
                }
                ConfirmOutcome::TimedOut => {
                    warn!("buy {signature} not confirmed within {confirm_timeout:?}");
                    "timeout".into()
                }
            };
            log_trade(&record);
            match outcome {
                ConfirmOutcome::Failed { error, .. } if balance == 0 && error.contains(STALE_ACCOUNTS_ERROR) => Ok(BuyAttempt::StaleAccounts),
                _ => Ok(BuyAttempt::Done),
            }
        }
    }
}








/// `await_landing` over every copy of a buy (NONCE_FANOUT): the first copy seen on chain, success
/// or failure, and its signature. At most one copy can land, so the others simply time out.
pub(crate) async fn await_landing_any(
    primary: Signature,
    alternates: &[Signature],
    timeout: Duration,
) -> Result<(ConfirmOutcome, Option<super::position_feed::Fill>, Signature)> {
    if alternates.is_empty() {
        let (outcome, fill) = await_landing(&primary, timeout).await?;
        return Ok((outcome, fill, primary));
    }
    use futures::stream::{FuturesUnordered, StreamExt};
    let mut pending: FuturesUnordered<_> = std::iter::once(primary)
        .chain(alternates.iter().copied())
        .map(|signature| async move { (signature, await_landing(&signature, timeout).await) })
        .collect();
    while let Some((signature, result)) = pending.next().await {
        match result {
            Ok((ConfirmOutcome::TimedOut, _)) => {}
            Ok((outcome, fill)) => return Ok((outcome, fill, signature)),
            Err(e) => warn!("copy {signature}: {e:#}"),
        }
    }
    Ok((ConfirmOutcome::TimedOut, None, primary))
}

pub(crate) async fn await_landing(
    signature: &Signature,
    timeout: Duration,
) -> Result<(ConfirmOutcome, Option<super::position_feed::Fill>)> {
    tokio::select! {
        fill = super::position_feed::wait_fill(signature, timeout) => Ok(match fill {
            Some(fill) => (ConfirmOutcome::Confirmed { slot: fill.slot }, Some(fill)),
            None => (ConfirmOutcome::TimedOut, None),
        }),
        outcome = confirm_signature(&RPC_CLIENT, signature, timeout) => Ok((outcome?, None)),
    }
}



/// A dropped bundle-only buy can still land in a later Jito leader's slot: if its tokens show up
/// after all, open the position so they get sold.
fn watch_late_fill(market: Market, entry_lamports: u64, feed: Option<Arc<super::position_feed::FeedGuard>>, fast_rug: bool) {
    // The launch stops being watched when the snipe returns: take its insiders now.
    if let Some(guard) = &feed {
        crate::utils::auto_sell::note_insiders(guard, &market.mint());
    }
    let token_account = get_associated_token_address(&trading_wallet(), &market.mint(), &market.token_program());
    tokio::spawn(async move {
        for wait in LATE_FILL_CHECKS {
            tokio::time::sleep(wait).await;
            let balance = get_token_balance(&RPC_CLIENT, &token_account).await.unwrap_or(0);
            if balance > 0 {
                warn!("{}: the dropped bundle landed after all ({balance} tokens); selling it", market.mint());
                spawn_position(Position {
                    market,
                    entry_lamports,
                    paper_token_amount: None,
                    paper_sol_in_curve: 0,
                    known_tokens: Some(balance),
                    opened_at: Instant::now(),
                    slot: None,
                    feed,
                    fast_rug,
                });
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_slots_respect_the_limit_and_release_on_drop() {
        let first = try_reserve_position(2).expect("first slot");
        let second = try_reserve_position(2).expect("second slot");
        assert!(try_reserve_position(2).is_none(), "limit of 2 reached");
        drop(first);
        let third = try_reserve_position(2).expect("slot freed by drop");
        assert_eq!(open_position_count(), 2);
        let unlimited = try_reserve_position(0).expect("0 means no limit");
        drop((second, third, unlimited));
        assert_eq!(open_position_count(), 0);
    }
    /// A creator's move to fee sharing seen on an early feed: the buy is built for the sharing
    /// config (its creator vault), not the creator the create named.
    #[test]
    fn an_early_move_to_fee_sharing_sets_the_buys_creator() {
        let (mint, named) = (Pubkey::new_unique(), Pubkey::new_unique());
        let market = || Market::PumpFun(crate::instructions::pumpfun_buy_ix::PumpFunMarket::new(mint, crate::utils::utils::TOKEN_2022_PROGRAM_ID, named, false, Default::default()));
        let mut m = market();
        apply_stream_view(&mut m);
        let Market::PumpFun(p) = &m else { unreachable!() };
        assert_eq!(p.creator, named);
        super::super::fee_sharing::note(mint);
        let mut m = market();
        apply_stream_view(&mut m);
        let Market::PumpFun(p) = &m else { unreachable!() };
        assert_eq!(p.creator, crate::hot::decode::sharing_config_of(&mint));
    }
}

/// Host of a metadata URI ("https://ipfs.io/ipfs/..." -> "ipfs.io"), lower case.
pub fn uri_host(uri: &str) -> Option<String> {
    let rest = uri.trim().split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?.to_lowercase();
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod metadata_host_tests {
    use super::uri_host;

    #[test]
    fn host_is_taken_from_the_uri() {
        assert_eq!(uri_host("https://meta.uxento.io/abc.json").as_deref(), Some("meta.uxento.io"));
        assert_eq!(uri_host("https://IPFS.io/ipfs/Qm").as_deref(), Some("ipfs.io"));
        assert_eq!(uri_host("usepaid.app/x"), None);
        assert_eq!(uri_host(""), None);
    }
}
