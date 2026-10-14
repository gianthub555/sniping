//! bigboss's SQLite store (2026-10-12, Part A), on the copy bot's model: one file
//! (`STORE_PATH`, default `data/bigboss.sqlite`), WAL, one writer thread on a bounded queue,
//! batched transactions, migrations in `PRAGMA user_version`; tapes in a file per UTC day
//! (`STORE_TAPES_DIR`, default `data/tapes`, 7 days kept). Readers (the dashboard, the scripts,
//! the startup loads) open it read-only.
//!
//! Switches (both default off):
//! - `STORE_DB=on`: every file's rows are written to the database too (dual-write), the trade log
//!   also as positions and orders, every launch the bot sees and its verdict, the activity feed.
//! - `STORE_FILES=off` (only with `STORE_DB=on`): the files are no longer written and the startup
//!   data is read from the database. Turn it off once `store compare` shows the counts and daily
//!   totals match.
//!
//! The scorecard's tables (`utils::db`) live here too: the store opens when `STORE_DB=on` or
//! `SCORECARD=true`; with only the scorecard on, nothing else is written.
//!
//! Hot threads never write here (they keep their rings, emptied by reporter threads); every
//! write is a non-blocking queue push from a reporter thread or a tokio task.

pub mod migrate;
pub mod read;
pub mod tee;
pub mod trades;
pub mod writer;

pub use writer::{field, Row, Val};

use {
    log::{info, warn},
    std::{
        path::PathBuf,
        sync::{atomic::Ordering::Relaxed, LazyLock, OnceLock},
    },
};

fn env_flag(key: &str, on: &[&str]) -> bool {
    std::env::var(key).is_ok_and(|v| on.iter().any(|o| v.trim().eq_ignore_ascii_case(o)))
}

static DB_ON: LazyLock<bool> = LazyLock::new(|| env_flag("STORE_DB", &["on", "true", "1"]));
static FILES_OFF: LazyLock<bool> = LazyLock::new(|| *DB_ON && env_flag("STORE_FILES", &["off", "false", "0"]));

#[cfg(test)]
static TEST_DB: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `STORE_DB=on`: the files' rows go to the database too.
pub fn db_on() -> bool {
    #[cfg(test)]
    if TEST_DB.load(Relaxed) {
        return HANDLE.get().is_some();
    }
    *DB_ON && HANDLE.get().is_some()
}

/// Tests: the store open on a temporary file, as with STORE_DB=on (once per test process).
#[cfg(test)]
pub fn test_store() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("bigboss-store-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = writer::Config::new(dir.join("db.sqlite"), dir.join("tapes"));
        cfg.batch_ms = 20;
        let (h, _t) = writer::spawn(cfg).expect("test store");
        let _ = RUN_ID.set("test-run".into());
        let _ = HANDLE.set(h);
        TEST_DB.store(true, Relaxed);
        dir.join("db.sqlite")
    })
    .clone()
}

/// Tests: waits (up to 3 s) until `sql` returns at least `n`.
#[cfg(test)]
pub fn wait_count(path: &std::path::Path, sql: &str, n: i64) -> i64 {
    let t = std::time::Instant::now();
    loop {
        let got = migrate::open_reader(path).and_then(|c| c.query_row(sql, [], |r| r.get::<_, i64>(0))).unwrap_or(0);
        if got >= n || t.elapsed() > std::time::Duration::from_secs(3) {
            return got;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The files are still written (always, unless `STORE_DB=on` and `STORE_FILES=off`).
pub fn files_on() -> bool {
    !*FILES_OFF
}

/// Startup data comes from the database (`STORE_FILES=off`).
pub fn read_from_db() -> bool {
    *FILES_OFF
}

pub fn path() -> PathBuf {
    PathBuf::from(std::env::var("STORE_PATH").unwrap_or_else(|_| "data/bigboss.sqlite".into()))
}

pub fn tapes_dir() -> PathBuf {
    PathBuf::from(std::env::var("STORE_TAPES_DIR").unwrap_or_else(|_| "data/tapes".into()))
}

static HANDLE: OnceLock<writer::Handle> = OnceLock::new();
static RUN_ID: OnceLock<String> = OnceLock::new();

/// This start's run id ("" before `init`).
pub fn run_id() -> &'static str {
    RUN_ID.get().map(String::as_str).unwrap_or("")
}

pub fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// Queues a row (no-op before `init` or when the store is closed). Never blocks.
pub fn write(row: Row) -> bool {
    HANDLE.get().is_some_and(|h| h.write(row))
}

/// Queues a mirrored file row when `STORE_DB=on`: the file's table, header and fields.
pub fn mirror(table: &'static str, header: &'static str, fields: &[&str]) {
    if db_on() {
        write(Row::Mirror { table, header, vals: fields.iter().map(|f| field(f)).collect(), rx_ns: now_ns() });
    }
}

/// `mirror` for a CSV line as the file gets it (the bot's CSV writers quote fields that need it).
pub fn mirror_line(table: &'static str, header: &'static str, line: &str) {
    if db_on() {
        let fields = split_csv(line.trim_end_matches(['\n', '\r']));
        write(Row::Mirror { table, header, vals: fields.iter().map(|f| field(f)).collect(), rx_ns: now_ns() });
    }
}

/// Splits one CSV line (double-quoted fields with "" escapes).
pub fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (false, '"') if cur.is_empty() => quoted = true,
            (false, ',') => out.push(std::mem::take(&mut cur)),
            (_, c) => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// A launch (or, Part C, a graduation) the bot saw: the `launches` row, verdict 'seen' until
/// decided. Venue 'pumpfun' kind 'launch'; venue 'pumpswap' kind 'graduation' or 'pool'.
#[allow(clippy::too_many_arguments)]
pub fn launch_seen(
    mint: &str,
    venue: &'static str,
    kind: &'static str,
    slot: u64,
    early: bool,
    deployer: Option<String>,
    creator: Option<String>,
    dev_buy_lamports: u64,
    followed: bool,
    follow_kind: Option<&'static str>,
) {
    if !db_on() {
        return;
    }
    write(Row::Exec {
        sql: "INSERT OR IGNORE INTO launches (mint, venue, kind, run_id, seen_ns, slot, source, deployer, creator, dev_buy_lamports, followed, follow_kind, verdict)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'seen')",
        params: vec![
            mint.into(),
            venue.into(),
            kind.into(),
            run_id().into(),
            now_ns().into(),
            slot.into(),
            (if early { "early" } else { "regular" }).into(),
            deployer.into(),
            creator.into(),
            dev_buy_lamports.into(),
            followed.into(),
            follow_kind.into(),
        ],
    });
}

/// What the bot decided about a seen launch: 'buy' (passed, buying), 'skip' with the reason,
/// 'paused', 'bought' / 'buy_failed' (the buy's result). Applies to the mint's row of `venue`.
pub fn launch_verdict(mint: &str, venue: &'static str, verdict: &'static str, reason: Option<String>, score: Option<i32>) {
    if !db_on() {
        return;
    }
    write(Row::Exec {
        sql: "UPDATE launches SET verdict = ?1, skip_reason = COALESCE(?2, skip_reason), score = COALESCE(?3, score), updated_ns = ?4
              WHERE mint = ?5 AND venue = ?6",
        params: vec![verdict.into(), reason.into(), score.map(|s| s as i64).into(), now_ns().into(), mint.into(), venue.into()],
    });
}

/// The switches in force, for the runs row (no secrets: switch names and values only).
fn settings_json() -> String {
    const KEYS: &[&str] = &[
        "DRY_RUN", "STORE_DB", "STORE_FILES", "SCORECARD", "SCORECARD_MIN", "SELL_RULE", "MAX_OPEN_POSITIONS", "BUY_SOL_AMOUNT",
        "HOT_LIVE", "SHRED_SNIPE", "HELIUS_PREPROCESSED", "ASYNC_FILE_WRITES", "HOT_NONCE_FANOUT", "HOT_SELL_FANOUT", "SNIPE_PUMPSWAP",
        "SNIPE_TEST", "SNIPE_TEST_SOL", "SNIPE_TEST_MAX_LOSS_SOL", "SNIPE_TEST_MAX_PER_DAY", "SNIPE_TEST_MAX_PER_HOUR",
    ];
    let pairs: Vec<String> = KEYS
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| format!("\"{k}\":{}", serde_json::Value::String(v.trim().to_string()))))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

/// Opens the store when `STORE_DB=on` or the scorecard needs it; registers this run. Returns
/// whether it is open. Call once at startup.
pub fn init(paper: bool) -> anyhow::Result<bool> {
    if HANDLE.get().is_some() {
        return Ok(true);
    }
    if !*DB_ON && !crate::config::scorecard::enabled() {
        return Ok(false);
    }
    let cfg = writer::Config::new(path(), tapes_dir());
    let (handle, _thread) = writer::spawn(cfg).map_err(|e| anyhow::anyhow!("store {}: {e}", path().display()))?;
    carry_over_legacy();
    let started = now_ns();
    let run = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        if paper { "paper" } else { "live" }
    );
    let _ = RUN_ID.set(run.clone());
    let _ = HANDLE.set(handle);
    write(Row::Exec {
        sql: "INSERT OR REPLACE INTO runs (run_id, started_ns, mode, exe, settings) VALUES (?1, ?2, ?3, ?4, ?5)",
        params: vec![
            run.into(),
            started.into(),
            (if paper { "paper" } else { "live" }).into(),
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default().into(),
            settings_json().into(),
        ],
    });
    info!(
        "store {} open (run {}): {}",
        path().display(),
        run_id(),
        if *DB_ON {
            if files_on() { "STORE_DB=on, files written too" } else { "STORE_DB=on, STORE_FILES=off: files no longer written" }
        } else {
            "the scorecard's tables only (STORE_DB off)"
        }
    );
    Ok(true)
}

/// Marks the run stopped (a clean shutdown).
pub fn mark_stopped() {
    write(Row::Exec { sql: "UPDATE runs SET stopped_ns = ?1, clean = 1 WHERE run_id = ?2", params: vec![now_ns().into(), run_id().into()] });
}

/// The scorecard's tables were in `DB_PATH` (default `bigboss.db`) before the store: copied over
/// once, when the store's are still empty. The old file is left as it is.
fn carry_over_legacy() {
    let legacy = std::env::var("DB_PATH").unwrap_or_else(|_| "bigboss.db".into());
    if !std::path::Path::new(&legacy).exists() || std::path::Path::new(&legacy) == path() {
        return;
    }
    let result = (|| -> rusqlite::Result<Vec<(String, usize)>> {
        let conn = rusqlite::Connection::open(path())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let empty: i64 = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM deployer_wallet) + (SELECT COUNT(*) FROM launch_result) + (SELECT COUNT(*) FROM score) + (SELECT COUNT(*) FROM exit_decision)",
            [],
            |r| r.get(0),
        )?;
        if empty > 0 {
            return Ok(vec![]);
        }
        conn.execute("ATTACH DATABASE ?1 AS legacy", [&legacy])?;
        let mut copied = Vec::new();
        for (table, cols) in [
            ("deployer_wallet", "key, first_bt, funded_sol, funder, checked_at"),
            ("launch_result", "mint, deployer, creator, created, pnl_sol, down, no_pop"),
            ("score", "mint, t, deployer, creator, path, score, threshold, passed, verdict, facts"),
            ("exit_decision", "mint, t, paper, reason, share, value_x, peak_x, held_secs, net_behind_sol"),
        ] {
            let n = conn.execute(&format!("INSERT OR IGNORE INTO main.{table} ({cols}) SELECT {cols} FROM legacy.{table}"), []).unwrap_or(0);
            copied.push((table.to_string(), n));
        }
        conn.execute("DETACH DATABASE legacy", [])?;
        Ok(copied)
    })();
    match result {
        Ok(c) if !c.is_empty() => info!(
            "store: the scorecard's data copied from {legacy} ({})",
            c.iter().map(|(t, n)| format!("{t} {n}")).collect::<Vec<_>>().join(", ")
        ),
        Ok(_) => {}
        Err(e) => warn!("store: the scorecard's data in {legacy} was not copied: {e}"),
    }
}

/// One line for the stats log and the dashboard: enqueued, dropped, written, errors, batches.
pub fn report() -> String {
    match HANDLE.get() {
        None => "store: closed".into(),
        Some(h) => {
            let s = &h.stats;
            format!(
                "store ({}): enqueued {}, dropped {}, written {}, errors {}, batches {}, last batch {} us, longest {} us",
                if *DB_ON { if files_on() { "dual-write" } else { "database only" } } else { "scorecard only" },
                s.enqueued.load(Relaxed),
                s.dropped.load(Relaxed),
                s.written.load(Relaxed),
                s.write_errors.load(Relaxed),
                s.batches.load(Relaxed),
                s.last_batch_us.load(Relaxed),
                s.max_batch_us.load(Relaxed)
            )
        }
    }
}

/// (enqueued, dropped, written, errors) for the dashboard's status bar.
pub fn counters() -> Option<(u64, u64, u64, u64)> {
    HANDLE.get().map(|h| {
        let s = &h.stats;
        (s.enqueued.load(Relaxed), s.dropped.load(Relaxed), s.written.load(Relaxed), s.write_errors.load(Relaxed))
    })
}

/// The counters into kv 'store_stats' (JSON), for the dashboard's "Store drops" (it reads the
/// store, never the bot's memory). Called with the minute's report; only with STORE_DB=on.
pub fn publish_counters() {
    if !db_on() {
        return;
    }
    if let Some((enqueued, dropped, written, errors)) = counters() {
        write(Row::Exec {
            sql: "INSERT OR REPLACE INTO kv (key, value, updated_ns) VALUES ('store_stats', ?1, ?2)",
            params: vec![
                format!("{{\"enqueued\":{enqueued},\"dropped\":{dropped},\"written\":{written},\"errors\":{errors}}}").into(),
                now_ns().into(),
            ],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trade log's hooks: a buy and two sells of one mint become one position (closed when its
    /// tokens are gone) and three orders (sells numbered); the launch row follows the buy.
    #[test]
    fn a_buy_and_its_sells_become_a_position_and_orders() {
        let db = test_store();
        let mint = "TestMint1111111111111111111111111111111111";
        launch_seen(mint, "pumpfun", "launch", 5, true, Some("Dep".into()), Some("Cre".into()), 1_000_000_000, true, Some("recent"));
        trades::note_context(mint, trades::Context { venue: "pumpfun", deployer: Some("Dep".into()), follow_kind: Some("recent"), tier: Some("1".into()), rent_lamports: trades::RENT_TOKEN_2022 });
        let row = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            move |k: &str| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default()
        };
        trades::on_trade(row(&[("action", "BUY"), ("mint", mint), ("dex", "pumpfun"), ("status", "simulated"), ("sol_in", "0.500000000"), ("token_amount", "1000"), ("reason", "SNIPE"), ("wallet_sol_change", "-0.502074080")]), "paper");
        trades::on_trade(row(&[("action", "SELL"), ("mint", mint), ("status", "paper"), ("sol_in", "0.25"), ("sol_out", "0.300000000"), ("token_amount", "500"), ("reason", "SCALE"), ("wallet_sol_change", "0.300000000")]), "paper");
        trades::on_trade(row(&[("action", "SELL"), ("mint", mint), ("status", "paper"), ("sol_in", "0.25"), ("sol_out", "0.200000000"), ("token_amount", "500"), ("reason", "TRAIL"), ("wallet_sol_change", "0.200000000")]), "paper");
        let n = wait_count(&db, &format!("SELECT COUNT(*) FROM orders WHERE mint = '{mint}'"), 3);
        assert_eq!(n, 3);
        assert_eq!(wait_count(&db, &format!("SELECT COUNT(*) FROM positions WHERE mint = '{mint}' AND state = 'CLOSED'"), 1), 1);
        let c = migrate::open_reader(&db).unwrap();
        let (sol_in, sol_out, wallet, rent, exit, deployer): (i64, i64, i64, i64, String, String) = c
            .query_row(&format!("SELECT sol_in, sol_out, wallet_delta_lamports, rent_locked_lamports, exit_reason, deployer FROM positions WHERE mint = '{mint}'"), [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .unwrap();
        assert_eq!((sol_in, sol_out, wallet, rent, exit.as_str(), deployer.as_str()), (500_000_000, 500_000_000, -2_074_080, 0, "TRAIL", "Dep"), "paper locks no rent, and a closed position has none left");
        let steps: Vec<Option<i64>> = c
            .prepare(&format!("SELECT sell_step FROM orders WHERE mint = '{mint}' ORDER BY client_id"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(steps, [None, Some(1), Some(2)]);
        assert_eq!(wait_count(&db, &format!("SELECT COUNT(*) FROM launches WHERE mint = '{mint}' AND verdict = 'bought'"), 1), 1);
    }

    /// A teed file and its table get the same rows; the header goes to the file only.
    #[test]
    fn a_teed_file_and_its_table_get_the_same_rows() {
        use std::io::Write;
        let db = test_store();
        let file = std::env::temp_dir().join(format!("bigboss-tee-{}.csv", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let header = "seen_unix_ms,signature,mint,seller,slot,executed,ms_to_executed";
        let mut t = tee::append(file.to_str().unwrap(), "preexec_sightings", header).unwrap();
        writeln!(t, "{header}").unwrap();
        write!(t, "1,SigA,MintTee,Seller,7,true,").unwrap();
        writeln!(t, "12").unwrap();
        writeln!(t, "2,SigB,\"Mint,Tee\",Seller,8,false,").unwrap();
        drop(t);
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 3);
        assert_eq!(wait_count(&db, "SELECT COUNT(*) FROM preexec_sightings WHERE seller = 'Seller'", 2), 2);
        let c = migrate::open_reader(&db).unwrap();
        let mint2: String = c.query_row("SELECT mint FROM preexec_sightings WHERE signature = 'SigB'", [], |r| r.get(0)).unwrap();
        assert_eq!(mint2, "Mint,Tee");
        let ms: Option<i64> = c.query_row("SELECT ms_to_executed FROM preexec_sightings WHERE signature = 'SigA'", [], |r| r.get(0)).unwrap();
        assert_eq!(ms, Some(12));
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn csv_lines_split_as_the_writers_quote_them() {
        assert_eq!(split_csv("a,,\"b,c\",\"say \"\"hi\"\"\",d"), ["a", "", "b,c", "say \"hi\"", "d"]);
        assert_eq!(split_csv(""), [""]);
    }

    /// The minute's counters land in kv 'store_stats' as JSON the dashboard reads.
    #[test]
    fn counters_are_published_for_the_dashboard() {
        let db = test_store();
        publish_counters();
        assert_eq!(wait_count(&db, "SELECT COUNT(*) FROM kv WHERE key = 'store_stats'", 1), 1);
        let c = migrate::open_reader(&db).unwrap();
        let v: String = c.query_row("SELECT value FROM kv WHERE key = 'store_stats'", [], |r| r.get(0)).unwrap();
        let j: serde_json::Value = serde_json::from_str(&v).unwrap();
        for k in ["enqueued", "dropped", "written", "errors"] {
            assert!(j[k].is_u64(), "{k} in {v}");
        }
    }
}
