use {
    carbon_core::pipeline::{Pipeline, ShutdownStrategy},
    carbon_log_metrics::LogMetrics,
    carbon_pump_swap_decoder::PumpSwapDecoder,
    carbon_pumpfun_decoder::PumpfunDecoder,
    carbon_raydium_amm_v4_decoder::RaydiumAmmV4Decoder,
    carbon_raydium_clmm_decoder::RaydiumClmmDecoder,
    carbon_raydium_cpmm_decoder::RaydiumCpmmDecoder,
    carbon_raydium_launchpad_decoder::RaydiumLaunchpadDecoder,
    carbon_yellowstone_grpc_datasource::{
        BlockFilters, YellowstoneGrpcClientConfig, YellowstoneGrpcGeyserClient,
    },
    colored::Colorize,
    log::{info, warn},
    sniper_bot::{
        config::{
            rules,
            buy_lamports, check_wallet_config, env_opt, lamports_to_sol, rpc_endpoint, set_env_file, Dex, TradeSetting,
            PUBKEY, RPC_CLIENT,
            TRADE_SETTING,
        },
        instructions::{pumpfun_buy_ix::init_pumpfun_config, pumpswap_buy_ix::init_pumpswap_config},
        processors::{
            dex_program_id, PumpSwapProcess, PumpfunProcess, RayClmmProcess, RayCpmmProcess,
            RayLaunchPadProcess, RaydiumV4Process,
        },
        service,
        utils::{
            blockhash::{recent_blockhash_handler, wait_for_blockhash},
            creator_history,
            csv_logger,
            dashboard::{now_unix, spawn_dashboard},
            grpc::check_grpc_endpoint,
            logging::{init_logger, redact_urls},
        },
    },
    std::{collections::HashMap, sync::Arc, time::Duration},
    yellowstone_grpc_proto::geyser::{CommitmentLevel, SubscribeRequestFilterTransactions},
};

const MIN_RECOMMENDED_CU: u32 = 116_000;

fn main() {
    let env_file = dotenvy::dotenv().ok();
    // The runtime's threads (and every thread they start) keep off the hot path's cores.
    sniper_bot::hot::affinity::keep_off_hot_cores();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().on_thread_start(sniper_bot::hot::affinity::keep_off_hot_cores);
    // KEEP_BLOCKING_THREADS=true: the runtime's blocking threads stay once started. By default one
    // idle for 10 s exits and the next blocking call starts another; each start (stack guard,
    // mprotect) and exit (stack madvise + munmap) flushed the TLB of every core running this
    // process, the hot cores included: 6-7 a minute on 14 Oct, ~45 shootdown IPIs a minute on
    // cores 5 and 7 (bpftrace on the paper bot, Frankfurt).
    if env_opt("KEEP_BLOCKING_THREADS").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        builder.thread_keep_alive(std::time::Duration::from_secs(365 * 24 * 3600));
    }
    let runtime = builder.build().expect("tokio runtime");
    runtime.block_on(async_main(env_file));
}

async fn async_main(env_file: Option<std::path::PathBuf>) {
    init_logger();
    if let Some(path) = env_file {
        set_env_file(path);
    }
    if let Err(e) = run().await {
        
        eprintln!("Error: {}", redact_urls(&format!("{e:#}")));
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {

    
    let _ = rustls::crypto::ring::default_provider().install_default();

    
    TradeSetting::from_env().map_err(|e| anyhow::anyhow!("invalid configuration: {e:#}"))?;
    check_wallet_config().map_err(|e| anyhow::anyhow!("invalid wallet configuration: {e:#}"))?;
    let setting = &*TRADE_SETTING;
    let grpc_url = env_opt("LOCAL_GRPC_URL")
        .ok_or_else(|| anyhow::anyhow!("LOCAL_GRPC_URL is not set"))?;

    println!("{}", "Solana sniper bot".bold());
    println!("  payer            {}", *PUBKEY);
    println!("  rpc              {}", redact_urls(&rpc_endpoint()));
    println!("  grpc             {}", redact_urls(&grpc_url));
    println!(
        "  buy              stake {} SOL, CU {}, priority fee {} micro-lamports/CU ({} SOL){}",
        lamports_to_sol(buy_lamports()),
        setting.compute_unit_limit,
        setting.priority_fee_micro_lamports,
        lamports_to_sol(setting.priority_fee_micro_lamports * setting.compute_unit_limit as u64 / 1_000_000),
        match setting.loaded_data_limit_bytes {
            Some(bytes) => format!(", loaded data limit {} MiB", bytes / 1024 / 1024),
            None => String::new(),
        }
    );
    println!(
        "  send            {}",
        match setting.confirm_services.as_slice() {
            _ if setting.nozomi_bundle => "Nozomi bundle (buys; see below)".to_string(),
            [one] => format!("{one:?}"),
            many => format!(
                "{} in parallel, one transaction, first to accept wins",
                many.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(" + ")
            ),
        }
    );
    if setting.sell_confirm_services != setting.confirm_services
        || setting.sell_priority_fee_micro_lamports != setting.priority_fee_micro_lamports
    {
        println!(
            "  send sells       {}, priority fee {} micro-lamports/CU",
            setting.sell_confirm_services.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(" + "),
            setting.sell_priority_fee_micro_lamports
        );
    }
    if setting.nonce_fanout {
        println!(
            "  buys             one copy per service ({}), each tipping only its own service, all advancing one durable nonce: the first to land is the buy, the others are void; tips {} ({}); a failing copy costs {} SOL",
            setting.fanout_services.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(" + "),
            setting
                .fanout_services
                .iter()
                .map(|s| format!("{s:?} {} SOL", lamports_to_sol(sniper_bot::service::fanout_tip(*s))))
                .collect::<Vec<_>>()
                .join(", "),
            "fixed",
            lamports_to_sol(5_000 + setting.priority_fee_micro_lamports * setting.compute_unit_limit as u64 / 1_000_000),
        );
    } else if setting.nozomi_bundle {
        let priority = lamports_to_sol(setting.priority_fee_micro_lamports * setting.compute_unit_limit as u64 / 1_000_000);
        println!(
            "  buys             Nozomi bundle + plain send{} of the same transaction (first path to land wins); a failing buy costs {} SOL; tips {} SOL{} and priority fee {priority} SOL only on landed buys",
            if setting.jito_copy { " + Jito bundle" } else { "" },
            lamports_to_sol(5_000 + setting.priority_fee_micro_lamports * setting.compute_unit_limit as u64 / 1_000_000),
            lamports_to_sol(setting.buy_third_party_fee_lamports),
            if setting.jito_copy { format!(" + {} SOL Jito", lamports_to_sol(setting.jito_tip_lamports)) } else { String::new() }
        );
        if setting.priority_fee_micro_lamports > 0 {
            warn!("NOZOMI_BUNDLE with a priority fee: the plain copy of a failing buy pays it ({priority} SOL); paired probes on 26 Sep 2026 showed neither the priority fee nor the tip moves the block position, set PRIORITY_FEE_MICRO_LAMPORT=0");
        }
        if setting.jito_bundle_only && !setting.jito_copy {
            warn!("NOZOMI_BUNDLE and JITO_BUNDLE_ONLY are both on: buys go as Nozomi bundles (set JITO_COPY=true to add a Jito bundle copy)");
        }
    }
    if setting.jito_bundle_only && !setting.nozomi_bundle && !setting.jito_copy {
        if setting.jito_uuid.is_none() {
            warn!("JITO_BUNDLE_ONLY without JITO_UUID: Jito dropped every bundle sent without an auth key in tests on 26 Sep 2026 (0 of 30 landed); check with `probe --vias jito` before relying on it");
        }
        if sniper_bot::service::buys_are_bundles() {
            println!("  buys             Jito bundles only: a buy that would fail is dropped, no fee (tip {} SOL on landed buys)", lamports_to_sol(setting.jito_tip_lamports));
        } else if setting.confirm_services.contains(&sniper_bot::config::ConfirmService::Jito) {
            warn!("JITO_BUNDLE_ONLY is on but CONFIRM_SERVICES has other routes too: they can land a buy that fails and charge its fee; set CONFIRM_SERVICES=JITO for no-fail buys");
        }
        if setting.sell_confirm_services.contains(&sniper_bot::config::ConfirmService::Jito) {
            warn!("JITO_BUNDLE_ONLY also applies to sells through Jito: a sell that would fail is dropped and retried, and bundles cannot land in the ~1% of slots without Jito; keep Nozomi or RPC for sells too");
        }
    }
    if setting.dry_run {
        println!("  {}", "DRY_RUN: transactions are simulated, never sent".yellow().bold());
        println!(
            "  live timing      paper buys land {}, sells {}; fees and tips {}",
            if setting.dry_run_fill_by_position { sniper_bot::processors::common::fill_positions_summary() } else { setting.dry_run_buy_latency.to_string() },
            if setting.dry_run_fill_by_position { "at a position from the same draw, counted from the stream's position at the decision".to_string() } else { setting.dry_run_sell_latency.to_string() },
            if setting.dry_run_charge_fees { "charged" } else { "not charged" }
        );
        if let Some(wallet) = setting.dry_run_wallet {
            println!("  simulating as     {wallet}");
        }
    }
    if setting.compute_unit_limit > 0 && setting.compute_unit_limit < MIN_RECOMMENDED_CU {
        warn!(
            "CU={} is below what pump.fun buys can use (260 simulated buys: median 94k, max 115k, \
             including 20k to create the token account; sells 51k-68k); some will fail with \
             ComputationalBudgetExceeded. Try 120000.",
            setting.compute_unit_limit
        );
    }

    // The snipe test (SNIPE_TEST=1): its values must read, or the bot does not start.
    if let Err(e) = sniper_bot::utils::snipe_test::check_settings() {
        anyhow::bail!("snipe test: {e}");
    }
    if sniper_bot::utils::snipe_test::on() {
        let mode = if setting.dry_run { "paper" } else { "live" };
        let (today, hour, pnl) = sniper_bot::utils::snipe_test::init(mode).await;
        println!(
            "  {}",
            format!(
                "SNIPE TEST on ({mode}): {} SOL a buy; today {today} buys ({hour} this hour), realized {:+.6} SOL ({})",
                sniper_bot::config::lamports_to_sol(sniper_bot::utils::snipe_test::size_lamports()),
                pnl as f64 / 1e9,
                sniper_bot::utils::snipe_test::LOG_PATH
            )
            .yellow()
            .bold()
        );
    }
    // bundle10 (BUNDLE10_*): its values must read, or the bot does not start.
    if let Err(e) = sniper_bot::config::bundle10::check_settings() {
        anyhow::bail!("bundle10: {e}");
    }
    // The whale rule (WHALE_MODE=1): its values must read, or the bot does not start.
    if let Err(e) = sniper_bot::processors::whale::check_settings() {
        anyhow::bail!("whale rule: {e}");
    }
    if sniper_bot::processors::whale::on() {
        let mode = if setting.dry_run { "paper" } else { "live" };
        let (held, today, pnl) = sniper_bot::processors::whale::init(mode);
        println!(
            "  {}",
            format!(
                "WHALE on ({mode}): {} SOL size; {held} coins held before (never bought again); today {today} entries, realized {:+.6} SOL ({})",
                sniper_bot::config::lamports_to_sol(sniper_bot::processors::whale::settings().size_lamports),
                pnl as f64 / 1e9,
                sniper_bot::processors::whale::LOG_PATH
            )
            .yellow()
            .bold()
        );
    }
    for line in rules::summary() {
        println!("  rule             {line}");
    }
    creator_history::load(now_unix());
    let known = sniper_bot::utils::reference::load();
    // A cut history file means far fewer followed launches (13 Oct): loud always, refused live.
    if let Err(problem) = sniper_bot::utils::reference::check_history(now_unix(), known) {
        log::error!("{problem}");
        println!("  {}", format!("follow history   {problem}").red().bold());
        let accepted = std::env::var("FOLLOW_HISTORY_SHORT_OK").is_ok_and(|v| v.eq_ignore_ascii_case("true"));
        if !setting.dry_run && !accepted {
            anyhow::bail!("{problem} (refusing to trade live; FOLLOW_HISTORY_SHORT_OK=true accepts it on purpose)");
        }
    }
    // His trades while the bot was down, from RPC, alongside the stream (each noted once).
    tokio::spawn(async {
        match sniper_bot::utils::reference::backfill(now_unix()).await {
            Ok((0, _)) => log::info!("reference wallet: no trades of his missed while the bot was down"),
            Ok((n, coins)) => log::info!("reference wallet: {n} trades of his on {coins} coins while the bot was down, recorded from RPC"),
            Err(e) => log::warn!("reference wallet: could not fetch the trades missed while the bot was down: {e:#}"),
        }
    });
    sniper_bot::processors::common::prefetch_wallet_ages(sniper_bot::utils::reference::keys());
    // The store (SQLite, data/bigboss.sqlite): open with STORE_DB=on or the scorecard on.
    sniper_bot::utils::db::init()?;
    // bundle10's shadow log (BUNDLE10_SHADOW=true): nothing signed or sent, rows to the store.
    if sniper_bot::config::bundle10::settings().enabled {
        println!("  {}", "bundle10         BUNDLE10_ENABLED=true is ignored: the live path is not built yet (shadow only)".yellow());
    }
    if sniper_bot::config::bundle10::shadow() {
        let today = sniper_bot::processors::bundle10_shadow::init();
        sniper_bot::processors::bundle10_shadow::spawn();
        let fast = sniper_bot::hot::engine::CONFIG.enabled;
        println!(
            "  bundle10 shadow  on: every early-path launch with a dev buy and a count >= 8 to the store's bundle10_shadow, simulated at 0.1 SOL; nothing is sent ({today} would-pass rows today){}",
            if fast { "" } else { "; FAST_PATH is off, so no launch reaches it" }
        );
    }
    if sniper_bot::store::db_on() {
        tokio::spawn(async {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                log::info!("{}", sniper_bot::store::report());
                sniper_bot::store::publish_counters();
            }
        });
    }
    let wallets = sniper_bot::utils::deployer_wallet::load();
    let results = sniper_bot::utils::launch_results::load();
    if sniper_bot::config::scorecard::enabled() {
        sniper_bot::utils::deployer_wallet::prefetch(sniper_bot::utils::reference::keys());
    }
    println!("  store            {}", sniper_bot::store::report());
    println!(
        "  scorecard        {}; database {} ({wallets} deployer wallets, {results} launch results)",
        if sniper_bot::config::scorecard::enabled() {
            format!("on: buy a followed launch at score {}+ (config::scorecard)", sniper_bot::config::scorecard::min())
        } else {
            "off (SCORECARD=true to use it)".to_string()
        },
        sniper_bot::utils::db::path()
    );
    println!(
        "  exit             {}",
        if sniper_bot::utils::auto_sell::px_exit_on() {
            let s = *sniper_bot::utils::auto_sell::PX_SETTINGS;
            format!(
                "pXGQ's (SELL_RULE=pxgq): stop x{}, no wave {}-{} s under {} SOL net, scale {:.0}% at x{}, then trail {:.0}% or x{}, dump {:.0}% off the peak, pressure {} SOL/3 s under x{}, cap {} s; {} open positions max",
                s.stop_x, s.wave_min_secs, s.wave_secs, s.wave_sol, s.scale_share * 100.0, s.scale_at_x, s.trail * 100.0, s.take_profit_x,
                s.dump_dd * 100.0, s.pressure_sol, s.pressure_x, s.max_hold_secs, rules::max_open_positions()
            )
        } else {
            "the bot's (SELL_RULE=pxgq for pXGQ's)".to_string()
        }
    );
    if sniper_bot::utils::snipe_test::rules_on() {
        println!("  exit (test)      SNIPE_TEST_RULES=on overrides the exit above for launches: all out at +30% or 15 s after the fill");
    }
    println!(
        "  reference        {known} deployers and creators in {}, {} followed right now",
        rules::REFERENCE_TRADES_FILE,
        sniper_bot::utils::reference::followed_count(now_unix())
    );
    csv_logger::upgrade_log_columns();
    println!(
        "  launch records   {} deployers in {} (no-buyers record: skip at {:.0}%+ of the last {} launches, {}+ launches)",
        sniper_bot::utils::deployer_record::init(),
        sniper_bot::utils::deployer_record::RECORD_PATH,
        rules::NO_POP_RECORD_SHARE * 100.0,
        rules::NO_POP_RECORD_LAST,
        rules::NO_POP_RECORD_MIN_LAUNCHES
    );

    tokio::spawn(recent_blockhash_handler(RPC_CLIENT.clone()));
    // The blockhash from shreds, a fallback when RPC's is old or missing: the in-process engine's
    // (ENGINE_INPROC), and pumpfeed's while its UDP push runs (SHRED_UDP_PORT; with the in-process
    // engine too, the parity run, it is only compared). LaserStream's block hashes are the other
    // fallback (`utils::slot_feed`, started below).
    let inproc = sniper_bot::hot::engine::INPROC.enabled && sniper_bot::hot::engine::CONFIG.enabled;
    if setting.shred_udp_port.is_some() {
        tokio::spawn(sniper_bot::utils::blockhash::pumpfeed_blockhash_handler(
            env_opt("PUMPFEED_API_ADDR").unwrap_or_else(|| "127.0.0.1:7878".into()),
            inproc,
        ));
    }
    sniper_bot::hot::clock::init();
    wait_for_blockhash(Duration::from_secs(15)).await?;

    
    match RPC_CLIENT.get_balance(&sniper_bot::config::trading_wallet()).await {
        Ok(balance) => {
            println!("  balance          {:.6} SOL", lamports_to_sol(balance));
            sniper_bot::utils::dashboard::set_wallet_balance(balance);
            let reserve = rules::lamports(rules::BALANCE_RESERVE_SOL);
            if balance < buy_lamports() + reserve {
                warn!(
                    "balance {:.4} SOL cannot cover the stake ({:.4} SOL plus {:.4} SOL reserve for fees, tips and rent); buys will be capped at what the wallet can afford",
                    lamports_to_sol(balance),
                    lamports_to_sol(buy_lamports()),
                    lamports_to_sol(reserve)
                );
            }
            if balance < buy_lamports() && !setting.dry_run {
                warn!("wallet balance is below BUY_SOL_AMOUNT; buys will fail");
            }
        }
        Err(e) => warn!("could not read wallet balance: {e}"),
    }

    if setting.is_enabled(Dex::PumpFun) || setting.is_enabled(Dex::PumpSwap) {
        
        init_pumpfun_config(RPC_CLIENT.clone()).await?;
        init_pumpswap_config(RPC_CLIENT.clone()).await?;
    }
    service::init().await?;
    sniper_bot::utils::latency::spawn_probe_reporter();
    if let Some((start, end)) = setting.trading_hours {
        println!("  trading hours    buys only {:02}:{:02}-{:02}:{:02} UTC (TRADING_HOURS_UTC); sells always", start / 60, start % 60, end / 60, end % 60);
    }
    if !setting.leader_regions.is_empty() {
        sniper_bot::utils::leaders::ensure_spawned();
        println!(
            "  block producers  buy only when the launch's producer is in {} (LEADER_REGIONS; unknown producers pass)",
            setting.leader_regions.join(",")
        );
    }
    sniper_bot::hot::distance::spawn();
    if sniper_bot::hot::distance::SETTINGS.on {
        let d = *sniper_bot::hot::distance::SETTINGS;
        println!(
            "  distance rule    buy only when the slot's leader answers a QUIC handshake under {} ms; in its last slot with a far next leader, only in the slot's first {} ms (DISTANCE_*)",
            d.max_rtt_us / 1000,
            d.last_slot_ms
        );
    }
    // The nonce accounts' manager for the fan-out (HOT_NONCE_FANOUT / HOT_SELL_FANOUT), before the
    // hot path starts and takes its rings.
    sniper_bot::hot::nonces::start().await;
    // Slot starts and the backup blockhash from LaserStream (in place of the shred stream).
    sniper_bot::utils::slot_feed::spawn(grpc_url.clone(), env_opt("GRPC_TOKEN"));
    if sniper_bot::processors::shred_feed::enabled() {
        sniper_bot::processors::shred_feed::spawn();
        // Launches seen before they execute carry no balances: keep the followed deployers' ones.
        sniper_bot::processors::deployer_balances::spawn(grpc_url.clone(), env_opt("GRPC_TOKEN"));
        let mode = if setting.shred_snipe { "buys from early sightings (SHRED_SNIPE)" } else { "measuring the head start only (logs/shred_lead.csv)" };
        if setting.preprocessed_url.is_some() {
            println!("  early stream     Helius preprocessed (before execution), {mode}; followed deployers' balances kept live");
        } else if let Some(port) = setting.shred_udp_port.filter(|_| setting.shred_proxy_url.is_none()) {
            println!("  early stream     deshredded entries over UDP port {port} (SHRED_UDP_PORT), {mode}");
        } else {
            println!("  shred stream     on ({}), {mode}", if setting.shred_token.is_some() { "token set" } else { "SHRED_TOKEN missing" });
        }
    }
    check_grpc_endpoint(&grpc_url, env_opt("GRPC_TOKEN")).await?;
    // The whale rule's loop (every new slot, the young coins in its window).
    sniper_bot::processors::whale::spawn();
    if let Some(addr) = setting.dashboard_addr {
        spawn_dashboard(addr).await?;
        println!(
            "  {}",
            format!("trading PAUSED: nothing is bought until you press \"Start trading\" on the dashboard (port {})", addr.port())
                .yellow()
                .bold()
        );
    } else {
        
        sniper_bot::utils::control::set_trading(true);
        println!("  trading          on (no dashboard to start it from)");
    }
    let guard = *sniper_bot::utils::live_guard::LIMITS;
    if guard.any() {
        println!("  live guard       {}", guard.describe());
        sniper_bot::utils::live_guard::spawn();
    }
    if sniper_bot::hot::live::is_live() && !sniper_bot::processors::common::slow_path_live_buys() {
        println!("  slow path        does not buy (LIVE_SLOW_PATH_BUYS=false): only the hot path buys, through its lanes");
    }

    let mut programs: Vec<String> = rules::DEXES
        .iter()
        .map(|dex| dex_program_id(*dex).to_string())
        .collect();
    // The snipe test's graduations: the PumpSwap program's transactions too (migrations, the pools'
    // trades).
    let graduations = sniper_bot::processors::graduation::on();
    if graduations {
        programs.push(sniper_bot::instructions::pumpswap_buy_ix::PUMPSWAP_PROGRAM_ID.to_string());
    }
    let mut transaction_filters = HashMap::new();
    transaction_filters.insert(
        "sniper_dex_programs".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: programs,
            account_exclude: vec![],
            account_required: vec![],
            signature: None,
            ..Default::default()
        },
    );

    let datasource = YellowstoneGrpcGeyserClient::new(
        grpc_url,
        env_opt("GRPC_TOKEN"),
        Some(CommitmentLevel::Processed),
        HashMap::new(),
        transaction_filters,
        BlockFilters::default(),
        YellowstoneGrpcClientConfig { tcp_nodelay: Some(true), ..Default::default() },
        None,
        None,
    );

    let mut builder = Pipeline::builder()
        .datasource(datasource)
        .metrics(Arc::new(LogMetrics::new_with_flush_interval(60)))
        .shutdown_strategy(ShutdownStrategy::Immediate);
    for dex in rules::DEXES {
        builder = match dex {
            Dex::PumpFun => builder.instruction(PumpfunDecoder, PumpfunProcess),
            Dex::PumpSwap => builder.instruction(PumpSwapDecoder, PumpSwapProcess),
            Dex::RaydiumAmm => builder.instruction(RaydiumAmmV4Decoder, RaydiumV4Process),
            Dex::RaydiumCpmm => builder.instruction(RaydiumCpmmDecoder, RayCpmmProcess),
            Dex::RaydiumClmm => builder.instruction(RaydiumClmmDecoder, RayClmmProcess),
            Dex::RaydiumLaunchpad => builder.instruction(RaydiumLaunchpadDecoder, RayLaunchPadProcess),
        };
    }
    if graduations {
        builder = builder.instruction(PumpSwapDecoder, sniper_bot::processors::graduation::GraduationProcess);
        println!("  graduations      PumpSwap in the stream: canonical pools at migration (SNIPE_TEST_POOL=on)");
    }

    info!("{}", "Starting sniper bot...".green().bold());
    let result = builder.build()?.run().await;
    // The store: the run's stop time, and a moment for the writer to flush its last batch.
    sniper_bot::store::mark_stopped();
    tokio::time::sleep(Duration::from_millis(600)).await;
    log::info!("{}", sniper_bot::store::report());
    result?;
    info!("Sniper bot stopped.");
    Ok(())
}
