//! The store's tool (2026-10-12, Part A): import the old files, export tables in their files'
//! formats for the scripts, and counts.
//!
//!   store import [--force]        every old file into the store (STORE_PATH, default
//!                                 data/bigboss.sqlite; tapes into STORE_TAPES_DIR). The files are
//!                                 left as they are. A source already imported is skipped (kv
//!                                 "import:<file>"); --force imports it again (its rows are added,
//!                                 not replaced). A file whose header differs from its table (an
//!                                 older format) is skipped and reported.
//!   store export <table> [--mode paper|live]
//!                                 the table to stdout in its file's format (trade_log: the trade
//!                                 log's columns, one mode; reference_trades: the text file's lines)
//!   store stats                   tables and row counts, schema version, file sizes
//!   store timings                 each startup load from its file against its query, timed
//!
//! Run the import with the bot stopped (or before turning STORE_DB on): rows the bot appends to a
//! file after the import are in the database only once STORE_DB=on writes them.

use {
    anyhow::{anyhow, Context, Result},
    rusqlite::Connection,
    sniper_bot::store::{
        self, field, migrate,
        trades::{self, Book, Context as TradeContext},
        writer::{self, Row, Stats, Val},
    },
    std::{
        collections::HashMap,
        io::{BufRead, Write},
        path::Path,
        sync::atomic::Ordering::Relaxed,
        time::Instant,
    },
};

const TRADE_HEADER: &str = "timestamp,action,dex,mint,signature,status,sol_in,sol_out,token_amount,pnl_percent,reason,\
wallet_sol_change,launch_slot,ready_slot,landed_slot,detect_to_ready_ms,launch_to_landed_ms";
const TRADE_STORE_HEADER: &str = "mode,timestamp,action,dex,mint,signature,status,sol_in,sol_out,token_amount,pnl_percent,reason,\
wallet_sol_change,launch_slot,ready_slot,landed_slot,detect_to_ready_ms,launch_to_landed_ms";

/// Mirrored measurement logs: (table, file).
const MIRRORS: &[(&str, &str)] = &[
    ("positions_log", "logs/positions.csv"),
    ("rival_buys", "logs/rival_buys.csv"),
    ("route_skips", "logs/route_skips.csv"),
    ("hot_stamps", "logs/hot_stamps.csv"),
    ("lane_answers", "logs/hot_lane_answers.csv"),
    ("hot_verify", "logs/hot_verify.csv"),
    ("engine_parity", "logs/engine_parity.csv"),
    ("shred_lead", "logs/shred_lead.csv"),
    ("leader_rtt", "logs/leader_rtt.csv"),
    ("preexec_sightings", "logs/preexec_sightings.csv"),
    ("distance_daily", "logs/distance_daily.csv"),
];

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("import") => import(args.iter().any(|a| a == "--force")),
        Some("export") if args.len() >= 2 => {
            let mode = args.windows(2).find(|w| w[0] == "--mode").map(|w| w[1].clone());
            export(&args[1], mode.as_deref())
        }
        Some("stats") => stats(),
        Some("timings") => timings(),
        _ => Err(anyhow!("usage: store import [--force] | export <table> [--mode paper|live] | stats | timings")),
    }
}

/// Batches rows into the store with the writer's own code (`writer::flush`), 5,000 at a time.
struct Sink {
    conn: Connection,
    tapes: writer::Tapes,
    cache: HashMap<(&'static str, &'static str), String>,
    batch: Vec<Row>,
    stats: Stats,
}

impl Sink {
    fn push(&mut self, row: Row) {
        self.batch.push(row);
        if self.batch.len() >= 5_000 {
            self.flush();
        }
    }

    fn flush(&mut self) {
        writer::flush(&mut self.conn, &mut self.tapes, &mut self.cache, &mut self.batch, &self.stats);
    }
}

fn imported(conn: &Connection, source: &str) -> Option<String> {
    conn.query_row("SELECT value FROM kv WHERE key = ?1", [format!("import:{source}")], |r| r.get(0)).ok()
}

fn mark(sink: &mut Sink, source: &str, rows: usize) {
    let bytes = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
    sink.push(Row::Exec {
        sql: "INSERT INTO kv (key, value, updated_ns) VALUES (?1, ?2, ?3) ON CONFLICT(key) DO UPDATE SET value = ?2, updated_ns = ?3",
        params: vec![
            format!("import:{source}").into(),
            format!("{rows} rows, {bytes} bytes, {}", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).into(),
            store::now_ns().into(),
        ],
    });
    sink.flush();
}

/// The columns of a table (but id and rx_ns), comma-separated: a mirrored file's header.
fn columns(conn: &Connection, table: &str) -> Result<String> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols: Vec<String> = st.query_map([], |r| r.get::<_, String>(1))?.filter_map(|c| c.ok()).filter(|c| c != "id" && c != "rx_ns").collect();
    Ok(cols.join(","))
}

fn lines(path: &str) -> Result<impl Iterator<Item = String>> {
    let f = std::fs::File::open(path).with_context(|| path.to_string())?;
    Ok(std::io::BufReader::new(f).lines().map_while(|l| l.ok()))
}

fn import(force: bool) -> Result<()> {
    let path = store::path();
    let conn = migrate::open_writer(&path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let mut sink = Sink {
        conn,
        tapes: writer::tapes_sink(store::tapes_dir(), 7),
        cache: HashMap::new(),
        batch: Vec::new(),
        stats: Stats::default(),
    };
    println!("importing into {} (schema version {})", path.display(), migrate::latest_version());
    let skip = |sink: &Sink, source: &str| -> bool {
        if !Path::new(source).exists() {
            println!("  {source}: not found");
            return true;
        }
        if let (false, Some(done)) = (force, imported(&sink.conn, source)) {
            println!("  {source}: already imported ({done}); --force to import it again");
            return true;
        }
        false
    };
    let t0 = Instant::now();

    // creator_history.csv first: the trade log's positions take their deployer from it.
    let mut deployer_of: HashMap<String, String> = HashMap::new();
    let source = sniper_bot::utils::creator_history::CREATOR_HISTORY_PATH;
    if !skip(&sink, source) {
        let mut n = 0;
        for line in lines(source)?.skip(1) {
            let f: Vec<&str> = line.split(',').collect();
            let [t, d, m, dev] = f[..] else { continue };
            let (Ok(t), Ok(dev)) = (t.parse::<i64>(), dev.parse::<i64>()) else { continue };
            deployer_of.insert(m.to_string(), d.to_string());
            sink.push(Row::Exec {
                sql: "INSERT INTO creator_history (unix_time, deployer, mint, dev_buy_lamports) VALUES (?1, ?2, ?3, ?4)",
                params: vec![t.into(), d.into(), m.into(), dev.into()],
            });
            n += 1;
        }
        mark(&mut sink, source, n);
        println!("  {source}: {n} rows");
    } else if let Ok(it) = lines(source) {
        for line in it.skip(1) {
            let f: Vec<&str> = line.split(',').collect();
            if let [_, d, m, _] = f[..] {
                deployer_of.insert(m.to_string(), d.to_string());
            }
        }
    }

    for (source, mode) in [(sniper_bot::utils::csv_logger::TRADE_LOG_PATH, "live"), (sniper_bot::utils::csv_logger::PAPER_TRADE_LOG_PATH, "paper")] {
        if skip(&sink, source) {
            continue;
        }
        let mut it = lines(source)?;
        let header: Vec<String> = store::split_csv(&it.next().unwrap_or_default());
        let mut book = Book::new(if mode == "live" { 1_000_000 } else { 2_000_000 });
        let mut n = 0;
        for line in it {
            if line.trim().is_empty() {
                continue;
            }
            let f = store::split_csv(&line);
            let get = |k: &str| header.iter().position(|h| h == k).and_then(|i| f.get(i)).cloned().unwrap_or_default();
            let at = chrono::DateTime::parse_from_rfc3339(&get("timestamp")).map(|t| t.timestamp_nanos_opt().unwrap_or(0)).unwrap_or(0);
            let mut vals = vec![Val::from(mode)];
            vals.extend(TRADE_HEADER.split(',').map(|k| field(&get(k))));
            sink.push(Row::Mirror { table: "trade_log", header: TRADE_STORE_HEADER, vals, rx_ns: at });
            let ctx = TradeContext { venue: "", deployer: deployer_of.get(&get("mint")).cloned(), ..Default::default() };
            for row in trades::rows(&mut book, &get, mode, at, &ctx, "import") {
                sink.push(row);
            }
            n += 1;
        }
        mark(&mut sink, source, n);
        println!("  {source}: {n} trades as orders and positions");
    }

    let source = sniper_bot::utils::deployer_record::RECORD_PATH;
    if !skip(&sink, source) {
        let mut n = 0;
        for line in lines(source)?.skip(1) {
            let f: Vec<&str> = line.split(',').collect();
            let [t, d, m, np] = f[..] else { continue };
            let Ok(t) = t.parse::<i64>() else { continue };
            sink.push(Row::Exec {
                sql: "INSERT INTO deployer_launches (unix_time, deployer, mint, no_pop) VALUES (?1, ?2, ?3, ?4)",
                params: vec![t.into(), d.into(), m.into(), (np == "1").into()],
            });
            n += 1;
        }
        mark(&mut sink, source, n);
        println!("  {source}: {n} rows");
    }

    let source = sniper_bot::config::rules::REFERENCE_TRADES_FILE;
    if !skip(&sink, source) {
        let mut n = 0;
        for line in lines(source)? {
            let Some((t, key, lamports)) = sniper_bot::utils::reference::parse_line(&line) else { continue };
            sink.push(Row::Exec {
                sql: "INSERT INTO reference_trades (unix_time, key, lamports_delta) VALUES (?1, ?2, ?3)",
                params: vec![t.into(), key.to_string().into(), lamports.into()],
            });
            n += 1;
        }
        mark(&mut sink, source, n);
        println!("  {source}: {n} lines");
    }

    let source = "logs/lookup_tables.txt";
    if !skip(&sink, source) {
        let mut n = 0;
        for (i, line) in lines(source)?.enumerate() {
            let a = line.trim();
            if a.is_empty() {
                continue;
            }
            sink.push(Row::Exec { sql: "INSERT OR IGNORE INTO lookup_tables (address, added_ns) VALUES (?1, ?2)", params: vec![a.into(), (i as i64).into()] });
            n += 1;
        }
        mark(&mut sink, source, n);
        println!("  {source}: {n} tables");
    }

    let source = "logs/cu_calibration.json";
    if !skip(&sink, source) {
        let text = std::fs::read_to_string(source)?;
        sink.push(Row::Exec {
            sql: "INSERT INTO kv (key, value, updated_ns) VALUES ('cu_calibration', ?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?1, updated_ns = ?2",
            params: vec![text.into(), store::now_ns().into()],
        });
        mark(&mut sink, source, 1);
        println!("  {source}: 1 value");
    }

    let source = "logs/validator_regions.json";
    if !skip(&sink, source) {
        let map: HashMap<String, serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(source)?)?;
        let s = |v: &serde_json::Value, k: &str| Val::from(v[k].as_str().map(str::to_string));
        let rows: Vec<Vec<Val>> = map.iter().map(|(ip, v)| vec![ip.as_str().into(), s(v, "continent"), s(v, "country"), s(v, "city")]).collect();
        let n = rows.len();
        sink.push(Row::Snapshot { table: "validator_regions", header: "ip,continent,country,city", rows, rx_ns: None });
        mark(&mut sink, source, n);
        println!("  {source}: {n} addresses");
    }

    let source = "token_names.json";
    if !skip(&sink, source) {
        let map: HashMap<String, serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(source)?)?;
        for (mint, v) in &map {
            sink.push(Row::Exec {
                sql: "INSERT INTO token_names (mint, symbol, name) VALUES (?1, ?2, ?3) ON CONFLICT(mint) DO UPDATE SET symbol = ?2, name = ?3",
                params: vec![mint.as_str().into(), Val::from(v["symbol"].as_str().map(str::to_string)), Val::from(v["name"].as_str().map(str::to_string))],
            });
        }
        mark(&mut sink, source, map.len());
        println!("  {source}: {} names", map.len());
    }

    for &(table, source) in MIRRORS {
        if skip(&sink, source) {
            continue;
        }
        let cols = columns(&sink.conn, table)?;
        let mut it = lines(source)?;
        let header = it.next().unwrap_or_default();
        if header.trim() != cols {
            println!("  {source}: skipped, its columns differ from table {table} (an older format)");
            continue;
        }
        let width = cols.split(',').count();
        let header: &'static str = Box::leak(cols.into_boxed_str());
        let snapshot = table == "leader_rtt";
        let mut rows = Vec::new();
        let (mut n, mut malformed) = (0, 0);
        for line in it {
            if line.trim().is_empty() {
                continue;
            }
            let vals: Vec<Val> = store::split_csv(&line).iter().map(|f| field(f)).collect();
            if vals.len() != width {
                malformed += 1;
                continue;
            }
            if snapshot {
                rows.push(vals);
            } else {
                sink.push(Row::Mirror { table, header, vals, rx_ns: 0 });
            }
            n += 1;
        }
        if snapshot {
            sink.push(Row::Snapshot { table, header, rows, rx_ns: Some(store::now_ns()) });
        }
        mark(&mut sink, source, n);
        println!("  {source}: {n} rows into {table}{}", if malformed > 0 { format!(" ({malformed} malformed lines skipped: wrong field count)") } else { String::new() });
    }

    if let Ok(entries) = std::fs::read_dir(sniper_bot::utils::tape_recorder::TAPES_DIR) {
        let mut files: Vec<String> = entries.filter_map(|e| e.ok()?.path().to_str().map(String::from)).filter(|p| p.ends_with(".jsonl")).collect();
        files.sort();
        for source in files {
            if skip(&sink, &source) {
                continue;
            }
            let day = Path::new(&source).file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            let mut n = 0;
            for line in lines(&source)? {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                sink.push(Row::Tape {
                    day: day.clone(),
                    mint: v["mint"].as_str().unwrap_or_default().to_string(),
                    bt: v["bt"].as_i64().unwrap_or(0),
                    slot: v["slot"].as_i64().unwrap_or(0),
                    json: line,
                    rx_ns: 0,
                });
                n += 1;
            }
            mark(&mut sink, &source, n);
            println!("  {source}: {n} launches into the {day} tapes file");
        }
    }
    sink.flush();
    println!(
        "done in {:.1} s: {} rows written, {} errors",
        t0.elapsed().as_secs_f64(),
        sink.stats.written.load(Relaxed),
        sink.stats.write_errors.load(Relaxed)
    );
    Ok(())
}

fn csv_field(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    let s = match v {
        ValueRef::Null => return String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned(),
    };
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn export(table: &str, mode: Option<&str>) -> Result<()> {
    let conn = migrate::open_reader(&store::path())?;
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::new(out.lock());
    if table == "reference_trades" {
        writeln!(out, "# the reference wallet's trades, one line per trade and key (deployer and coin creator): unix key lamports_delta")?;
        let mut st = conn.prepare("SELECT unix_time, key, lamports_delta FROM reference_trades ORDER BY rowid")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            writeln!(out, "{} {} {}", r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)?;
        }
        return Ok(());
    }
    let (header, filter): (String, String) = match table {
        "trade_log" => (TRADE_HEADER.to_string(), format!(" WHERE mode = '{}'", if mode == Some("live") { "live" } else { "paper" })),
        _ => {
            let cols = columns(&conn, table)?;
            if cols.is_empty() {
                return Err(anyhow!("no table {table}"));
            }
            (cols, String::new())
        }
    };
    let cols: Vec<String> = header.split(',').map(|c| format!("\"{c}\"")).collect();
    let order = if conn.prepare(&format!("SELECT id FROM {table} LIMIT 0")).is_ok() { "id" } else { "rowid" };
    let mut st = conn.prepare(&format!("SELECT {} FROM {table}{filter} ORDER BY {order}", cols.join(",")))?;
    writeln!(out, "{header}")?;
    let n = cols.len();
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let line: Vec<String> = (0..n).map(|i| r.get_ref(i).map(csv_field).unwrap_or_default()).collect();
        writeln!(out, "{}", line.join(","))?;
    }
    Ok(())
}

fn stats() -> Result<()> {
    let path = store::path();
    let conn = migrate::open_reader(&path)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) + std::fs::metadata(format!("{}-wal", path.display())).map(|m| m.len()).unwrap_or(0);
    writeln!(out, "{}: schema version {version} (this build {}), {:.1} MB with its WAL", path.display(), migrate::latest_version(), size as f64 / 1e6)?;
    let mut st = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let tables: Vec<String> = st.query_map([], |r| r.get(0))?.filter_map(|t| t.ok()).collect();
    for t in tables {
        let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0))?;
        writeln!(out, "  {t:22} {n:>10}")?;
    }
    if let Ok(entries) = std::fs::read_dir(store::tapes_dir()) {
        let mut days: Vec<_> = entries.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "sqlite")).collect();
        days.sort_by_key(|e| e.file_name());
        for e in days {
            let n: i64 = migrate::open_reader(&e.path()).and_then(|c| c.query_row("SELECT COUNT(*) FROM tapes", [], |r| r.get(0))).unwrap_or(-1);
            writeln!(out, "  tapes {:?}: {n} launches, {:.1} MB", e.file_name(), e.metadata().map(|m| m.len()).unwrap_or(0) as f64 / 1e6)?;
        }
    }
    Ok(())
}

/// Startup loads: the file parsed as the bot parses it, against the store's indexed query.
fn timings() -> Result<()> {
    let now = chrono::Utc::now().timestamp() as u64;
    let week = now.saturating_sub(7 * 86_400);
    let time = |label: &str, f: &mut dyn FnMut() -> usize| {
        let t = Instant::now();
        let n = f();
        println!("  {label:44} {n:>8} rows  {:>8.1} ms", t.elapsed().as_secs_f64() * 1e3);
    };
    println!("startup loads, file against store ({})", store::path().display());
    time("creator_history.csv (whole file, 7 days kept)", &mut || {
        std::fs::read_to_string("creator_history.csv")
            .map(|t| t.lines().skip(1).filter(|l| l.split(',').next().and_then(|x| x.parse::<u64>().ok()).is_some_and(|x| x >= week)).count())
            .unwrap_or(0)
    });
    time("store: creator_history, 7 days (indexed)", &mut || store::read::creator_history(week).len());
    time("logs/deployer_launches.csv (whole file)", &mut || std::fs::read_to_string("logs/deployer_launches.csv").map(|t| t.lines().count()).unwrap_or(0));
    time("store: deployer_launches, 7 days (indexed)", &mut || store::read::deployer_launches(week).len());
    time("reference_deployers.txt (whole file)", &mut || {
        std::fs::read_to_string("reference_deployers.txt").map(|t| t.lines().filter_map(sniper_bot::utils::reference::parse_line).count()).unwrap_or(0)
    });
    time("store: reference_trades (all)", &mut || store::read::reference_trades().len());
    time("logs/lookup_tables.txt", &mut || std::fs::read_to_string("logs/lookup_tables.txt").map(|t| t.lines().count()).unwrap_or(0));
    time("store: lookup_tables", &mut || store::read::lookup_tables().len());
    time("token_names.json (parsed)", &mut || {
        std::fs::read_to_string("token_names.json").ok().and_then(|t| serde_json::from_str::<HashMap<String, serde_json::Value>>(&t).ok()).map_or(0, |m| m.len())
    });
    time("store: token_names", &mut || store::read::token_names().len());
    time("logs/positions.csv (daily table input, bytes)", &mut || std::fs::read_to_string("logs/positions.csv").map(|t| t.len()).unwrap_or(0));
    time("store: positions_log as CSV text (bytes)", &mut || store::read::csv_text("positions_log", "date,kind,mint").len());
    Ok(())
}
