//! The single SQLite writer (as the copy bot's `StoreWriter`). Callers queue rows on a bounded
//! channel and never wait: a full queue drops the row and counts it. One thread gathers rows for
//! `batch_ms` or up to `batch_rows` and writes them in one transaction. Tapes go to a separate file
//! per UTC day (`<tapes_dir>/YYYY-MM-DD.sqlite`), the newest `keep_days` kept.

use {
    super::migrate,
    rusqlite::{types::ToSqlOutput, Connection, ToSql},
    std::{
        collections::HashMap,
        path::PathBuf,
        sync::{
            atomic::{AtomicU64, Ordering::Relaxed},
            mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError},
            Arc,
        },
        time::{Duration, Instant},
    },
};

/// One value of a row.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    I(i64),
    F(f64),
    T(String),
}

impl ToSql for Val {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Val::Null => ToSqlOutput::from(rusqlite::types::Null),
            Val::I(v) => ToSqlOutput::from(*v),
            Val::F(v) => ToSqlOutput::from(*v),
            Val::T(v) => ToSqlOutput::from(v.as_str()),
        })
    }
}

impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Val::I(v)
    }
}
impl From<u64> for Val {
    fn from(v: u64) -> Self {
        Val::I(i64::try_from(v).unwrap_or(i64::MAX))
    }
}
impl From<u32> for Val {
    fn from(v: u32) -> Self {
        Val::I(v as i64)
    }
}
impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Val::I(v as i64)
    }
}
impl From<f64> for Val {
    fn from(v: f64) -> Self {
        if v.is_finite() {
            Val::F(v)
        } else {
            Val::Null
        }
    }
}
impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Val::T(v.to_string())
    }
}
impl From<String> for Val {
    fn from(v: String) -> Self {
        Val::T(v)
    }
}
impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(v: Option<T>) -> Self {
        v.map_or(Val::Null, Into::into)
    }
}

/// A CSV field as the file holds it: empty is NULL, the rest text (the column's affinity turns
/// numbers into numbers).
pub fn field(s: &str) -> Val {
    if s.is_empty() {
        Val::Null
    } else {
        Val::T(s.to_string())
    }
}

/// One thing to write.
#[derive(Debug, Clone)]
pub enum Row {
    /// A statement with fixed text (prepared once, cached).
    Exec { sql: &'static str, params: Vec<Val> },
    /// A row of a mirrored file: its table, the file's header (the columns, comma-separated) and
    /// the values in that order. `rx_ns` is added.
    Mirror { table: &'static str, header: &'static str, vals: Vec<Val>, rx_ns: i64 },
    /// A file rewritten whole: the table is emptied and refilled in the same transaction. `rx_ns`
    /// None for a table without that column.
    Snapshot { table: &'static str, header: &'static str, rows: Vec<Vec<Val>>, rx_ns: Option<i64> },
    /// One tape line, to the day file of `day` (YYYY-MM-DD).
    Tape { day: String, mint: String, bt: i64, slot: i64, json: String, rx_ns: i64 },
}

#[derive(Debug, Default)]
pub struct Stats {
    pub enqueued: AtomicU64,
    /// Rows dropped because the queue was full (the caller never waits).
    pub dropped: AtomicU64,
    pub written: AtomicU64,
    pub write_errors: AtomicU64,
    pub batches: AtomicU64,
    pub last_batch_us: AtomicU64,
    pub max_batch_us: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub tapes_dir: PathBuf,
    pub keep_days: u32,
    pub batch_ms: u64,
    pub batch_rows: usize,
    pub capacity: usize,
}

impl Config {
    pub fn new(path: PathBuf, tapes_dir: PathBuf) -> Self {
        Self { path, tapes_dir, keep_days: 7, batch_ms: 250, batch_rows: 500, capacity: 65_536 }
    }
}

#[derive(Clone)]
pub struct Handle {
    tx: SyncSender<Row>,
    pub stats: Arc<Stats>,
}

impl Handle {
    /// Never blocks. False (and a counted drop) when the queue is full or the writer is gone.
    pub fn write(&self, row: Row) -> bool {
        match self.tx.try_send(row) {
            Ok(()) => {
                self.stats.enqueued.fetch_add(1, Relaxed);
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Relaxed);
                false
            }
        }
    }
}

/// Opens and migrates the database on the calling thread (errors surface at startup), then hands
/// the connection to the writer thread. Returns the handle and the thread.
pub fn spawn(cfg: Config) -> Result<(Handle, std::thread::JoinHandle<()>), migrate::OpenError> {
    let conn = migrate::open_writer(&cfg.path)?;
    let (tx, rx) = sync_channel::<Row>(cfg.capacity);
    let stats = Arc::new(Stats::default());
    let st = stats.clone();
    let thread = std::thread::Builder::new()
        .name("store-writer".into())
        .spawn(move || {
            crate::hot::affinity::keep_off_hot_cores();
            run(conn, rx, cfg, st)
        })
        .map_err(|e| migrate::OpenError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))?;
    Ok((Handle { tx, stats }, thread))
}

pub struct Tapes {
    dir: PathBuf,
    keep_days: u32,
    open: Option<(String, Connection)>,
}

impl Tapes {
    fn conn(&mut self, day: &str) -> rusqlite::Result<&Connection> {
        if self.open.as_ref().is_none_or(|(d, _)| d != day) {
            let conn = migrate::open_tapes(&self.dir.join(format!("{day}.sqlite")))?;
            self.open = Some((day.to_string(), conn));
            self.prune(day);
        }
        Ok(&self.open.as_ref().expect("opened").1)
    }

    /// Deletes day files older than the newest `keep_days` (by name: YYYY-MM-DD sorts as a date).
    fn prune(&self, today: &str) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let mut days: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.len() == "YYYY-MM-DD.sqlite".len() && n.ends_with(".sqlite") && n.as_str() <= &format!("{today}.sqlite")[..])
            .collect();
        days.sort();
        let excess = days.len().saturating_sub(self.keep_days as usize);
        for name in &days[..excess] {
            let base = self.dir.join(name);
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", base.display()));
            }
            log::info!("store: tapes day file {name} deleted (keeping {} days)", self.keep_days);
        }
    }
}

fn run(mut conn: Connection, rx: Receiver<Row>, cfg: Config, stats: Arc<Stats>) {
    let mut tapes = Tapes { dir: cfg.tapes_dir.clone(), keep_days: cfg.keep_days, open: None };
    let mut sql_cache: HashMap<(&'static str, &'static str), String> = HashMap::new();
    let mut batch: Vec<Row> = Vec::with_capacity(cfg.batch_rows);
    let window = Duration::from_millis(cfg.batch_ms);
    while let Ok(row) = rx.recv() {
        batch.push(row);
        let deadline = Instant::now() + window;
        let mut gone = false;
        while batch.len() < cfg.batch_rows {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(row) => batch.push(row),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    gone = true;
                    break;
                }
            }
        }
        flush(&mut conn, &mut tapes, &mut sql_cache, &mut batch, &stats);
        if gone {
            break;
        }
    }
    flush(&mut conn, &mut tapes, &mut sql_cache, &mut batch, &stats);
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
}

fn plain_insert_sql(table: &str, header: &str) -> String {
    let cols: Vec<String> = header.split(',').map(|c| format!("\"{c}\"")).collect();
    format!("INSERT INTO {table} ({}) VALUES ({})", cols.join(","), vec!["?"; cols.len()].join(","))
}

fn insert_sql(table: &str, header: &str) -> String {
    let cols: Vec<String> = header.split(',').map(|c| format!("\"{c}\"")).collect();
    let marks = vec!["?"; cols.len() + 1].join(",");
    format!("INSERT INTO {table} (rx_ns,{}) VALUES ({marks})", cols.join(","))
}

/// Writes a batch: the main file's rows in one transaction, each day's tapes in one transaction of
/// that day's file. Pure of the channel, for tests.
pub fn flush(conn: &mut Connection, tapes: &mut impl TapeSink, cache: &mut HashMap<(&'static str, &'static str), String>, batch: &mut Vec<Row>, stats: &Stats) {
    if batch.is_empty() {
        return;
    }
    let t0 = Instant::now();
    let main: Vec<&Row> = batch.iter().filter(|r| !matches!(r, Row::Tape { .. })).collect();
    if !main.is_empty() {
        let result = (|| -> rusqlite::Result<(u64, u64)> {
            let tx = conn.transaction()?;
            let (mut ok, mut bad) = (0u64, 0u64);
            for row in &main {
                let r = match row {
                    Row::Exec { sql, params } => tx.prepare_cached(sql).and_then(|mut st| st.execute(rusqlite::params_from_iter(params.iter()))).map(|_| ()),
                    Row::Mirror { table, header, vals, rx_ns } => {
                        let sql = cache.entry((table, header)).or_insert_with(|| insert_sql(table, header));
                        tx.prepare_cached(sql).and_then(|mut st| {
                            st.execute(rusqlite::params_from_iter(std::iter::once(&Val::I(*rx_ns)).chain(vals.iter()))).map(|_| ())
                        })
                    }
                    Row::Snapshot { table, header, rows, rx_ns } => (|| {
                        tx.execute(&format!("DELETE FROM {table}"), [])?;
                        let sql = match rx_ns {
                            Some(_) => cache.entry((table, header)).or_insert_with(|| insert_sql(table, header)).clone(),
                            None => plain_insert_sql(table, header),
                        };
                        let mut st = tx.prepare_cached(&sql)?;
                        for vals in rows {
                            match rx_ns {
                                Some(ns) => st.execute(rusqlite::params_from_iter(std::iter::once(&Val::I(*ns)).chain(vals.iter())))?,
                                None => st.execute(rusqlite::params_from_iter(vals.iter()))?,
                            };
                        }
                        Ok(())
                    })(),
                    Row::Tape { .. } => Ok(()),
                };
                match r {
                    Ok(()) => ok += 1,
                    Err(e) => {
                        bad += 1;
                        log::warn!("store: a row was not written ({e}): {}", describe(row));
                    }
                }
            }
            tx.commit()?;
            Ok((ok, bad))
        })();
        match result {
            Ok((ok, bad)) => {
                stats.written.fetch_add(ok, Relaxed);
                stats.write_errors.fetch_add(bad, Relaxed);
            }
            Err(e) => {
                stats.write_errors.fetch_add(main.len() as u64, Relaxed);
                log::warn!("store: a batch of {} rows failed to commit: {e}", main.len());
            }
        }
    }
    let mut by_day: HashMap<&str, Vec<&Row>> = HashMap::new();
    for row in batch.iter() {
        if let Row::Tape { day, .. } = row {
            by_day.entry(day.as_str()).or_default().push(row);
        }
    }
    for (day, rows) in by_day {
        match tapes.write_day(day, &rows) {
            Ok(n) => {
                stats.written.fetch_add(n, Relaxed);
            }
            Err(e) => {
                stats.write_errors.fetch_add(rows.len() as u64, Relaxed);
                log::warn!("store: {} tape rows of {day} failed: {e}", rows.len());
            }
        }
    }
    let us = t0.elapsed().as_micros() as u64;
    stats.batches.fetch_add(1, Relaxed);
    stats.last_batch_us.store(us, Relaxed);
    stats.max_batch_us.fetch_max(us, Relaxed);
    batch.clear();
}

fn describe(row: &Row) -> String {
    match row {
        Row::Exec { sql, .. } => sql.split_whitespace().take(4).collect::<Vec<_>>().join(" "),
        Row::Mirror { table, .. } | Row::Snapshot { table, .. } => format!("table {table}"),
        Row::Tape { day, .. } => format!("tape {day}"),
    }
}

/// Where tape rows go (the day files; a test stub).
pub trait TapeSink {
    fn write_day(&mut self, day: &str, rows: &[&Row]) -> rusqlite::Result<u64>;
}

impl TapeSink for Tapes {
    fn write_day(&mut self, day: &str, rows: &[&Row]) -> rusqlite::Result<u64> {
        let conn = self.conn(day)?;
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare_cached("INSERT INTO tapes (rx_ns, mint, bt, slot, json) VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for row in rows {
                if let Row::Tape { mint, bt, slot, json, rx_ns, .. } = row {
                    st.execute(rusqlite::params![rx_ns, mint, bt, slot, json])?;
                    n += 1;
                }
            }
        }
        tx.commit()?;
        Ok(n)
    }
}

/// A tapes sink for a directory (tests and the importer).
pub fn tapes_sink(dir: PathBuf, keep_days: u32) -> Tapes {
    Tapes { dir, keep_days, open: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bigboss-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A full queue drops at once and counts; it never waits for the writer.
    #[test]
    fn a_full_queue_drops_and_never_blocks() {
        let (tx, _rx) = sync_channel::<Row>(2);
        let h = Handle { tx, stats: Arc::new(Stats::default()) };
        let row = || Row::Exec { sql: "SELECT 1", params: vec![] };
        assert!(h.write(row()) && h.write(row()));
        let t = Instant::now();
        assert!(!h.write(row()));
        assert!(t.elapsed() < Duration::from_millis(20));
        assert_eq!((h.stats.enqueued.load(Relaxed), h.stats.dropped.load(Relaxed)), (2, 1));
    }

    /// Rows of every kind land; a bad row is counted and does not lose the rest of its batch.
    #[test]
    fn a_batch_writes_every_kind_and_survives_a_bad_row() {
        let d = tmp("batch");
        let mut conn = migrate::open_writer(&d.join("db.sqlite")).unwrap();
        let mut tapes = tapes_sink(d.join("tapes"), 7);
        let stats = Stats::default();
        let mut cache = HashMap::new();
        let mut batch = vec![
            Row::Exec { sql: "INSERT INTO kv (key, value, updated_ns) VALUES (?1, ?2, ?3)", params: vec!["a".into(), "1".into(), 5i64.into()] },
            Row::Mirror { table: "route_skips", header: "time,mint,followed,slot,reason,leader,client,class,city,rtt_ms,next_rtt_ms,offset_ms", vals: "t,M,true,9,far,L,c,k,x,1.5,,3".split(',').map(field).collect(), rx_ns: 7 },
            Row::Exec { sql: "INSERT INTO no_such_table VALUES (1)", params: vec![] },
            Row::Snapshot { table: "leader_rtt", header: "leader,slots_this_epoch,slots_next_epoch,client,class,city,country,rtt_ms", rows: vec![vec!["L1".into(), 4i64.into(), 0i64.into(), "j".into(), "J".into(), "F".into(), "DE".into(), 1.2.into()]], rx_ns: Some(8) },
            Row::Snapshot { table: "validator_regions", header: "ip,continent,country,city", rows: vec![vec!["1.2.3.4".into(), "EU".into(), "DE".into(), "F".into()]], rx_ns: None },
            Row::Tape { day: "2026-10-12".into(), mint: "M".into(), bt: 1, slot: 2, json: "{}".into(), rx_ns: 9 },
        ];
        flush(&mut conn, &mut tapes, &mut cache, &mut batch, &stats);
        assert_eq!((stats.written.load(Relaxed), stats.write_errors.load(Relaxed)), (5, 1));
        let (slot, next, offset): (i64, Option<f64>, f64) = conn.query_row("SELECT slot, next_rtt_ms, offset_ms FROM route_skips", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((slot, next, offset), (9, None, 3.0), "numbers stored as numbers, empty as NULL");
        // A snapshot replaces the table.
        let mut again = vec![Row::Snapshot { table: "leader_rtt", header: "leader,slots_this_epoch,slots_next_epoch,client,class,city,country,rtt_ms", rows: vec![], rx_ns: Some(10) }];
        flush(&mut conn, &mut tapes, &mut cache, &mut again, &stats);
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM leader_rtt", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        let t = migrate::open_reader(&d.join("tapes/2026-10-12.sqlite")).unwrap();
        assert_eq!(t.query_row("SELECT mint FROM tapes", [], |r| r.get::<_, String>(0)).unwrap(), "M");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn tapes_keep_the_newest_days() {
        let d = tmp("tapes");
        let mut tapes = tapes_sink(d.clone(), 3);
        for day in ["2026-10-01", "2026-10-02", "2026-10-03", "2026-10-04", "2026-10-05"] {
            let row = Row::Tape { day: day.into(), mint: "M".into(), bt: 0, slot: 0, json: "{}".into(), rx_ns: 0 };
            tapes.write_day(day, &[&row]).unwrap();
        }
        let mut left: Vec<String> = std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.ends_with(".sqlite")).collect();
        left.sort();
        assert_eq!(left, ["2026-10-03.sqlite", "2026-10-04.sqlite", "2026-10-05.sqlite"]);
        let _ = std::fs::remove_dir_all(d);
    }

    /// The writer thread end to end: rows queued from another thread are written within the batch
    /// window.
    #[test]
    fn the_thread_writes_queued_rows_in_batches() {
        let d = tmp("thread");
        let mut cfg = Config::new(d.join("db.sqlite"), d.join("tapes"));
        cfg.batch_ms = 20;
        let (h, thread) = spawn(cfg).unwrap();
        for i in 0..1_200i64 {
            assert!(h.write(Row::Exec { sql: "INSERT INTO kv (key, value, updated_ns) VALUES (?1, 'v', 0)", params: vec![format!("k{i}").into()] }));
        }
        let stats = h.stats.clone();
        drop(h);
        thread.join().unwrap();
        assert_eq!(stats.written.load(Relaxed), 1_200);
        assert!(stats.batches.load(Relaxed) >= 3, "500 rows a batch at most");
        let r = migrate::open_reader(&d.join("db.sqlite")).unwrap();
        assert_eq!(r.query_row("SELECT COUNT(*) FROM kv", [], |r| r.get::<_, i64>(0)).unwrap(), 1_200);
        let _ = std::fs::remove_dir_all(d);
    }
}
