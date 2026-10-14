//! Startup reads from the store (`STORE_FILES=off`): indexed, time-bounded queries instead of
//! parsing whole files. Each returns its rows and logs how long it took.

use {
    super::migrate,
    log::info,
    rusqlite::Connection,
    std::time::Instant,
};

fn conn() -> rusqlite::Result<Connection> {
    migrate::open_reader(&super::path())
}

fn timed<T>(what: &str, f: impl FnOnce(&Connection) -> rusqlite::Result<Vec<T>>) -> Vec<T> {
    let t = Instant::now();
    match conn().and_then(|c| f(&c)) {
        Ok(rows) => {
            info!("store: {what}: {} rows in {:.1} ms", rows.len(), t.elapsed().as_secs_f64() * 1e3);
            rows
        }
        Err(e) => {
            log::warn!("store: {what} not read: {e}");
            Vec::new()
        }
    }
}

/// Creator history since `since` (unix): (time, deployer, mint), oldest first.
pub fn creator_history(since: u64) -> Vec<(u64, String, String)> {
    timed("creator history", |c| {
        let mut st = c.prepare("SELECT unix_time, deployer, mint FROM creator_history WHERE unix_time >= ?1 ORDER BY unix_time")?;
        let rows = st.query_map([since as i64], |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    })
}

/// The no-buyers record since `since`: (time, deployer, mint, no_pop), oldest first.
pub fn deployer_launches(since: u64) -> Vec<(u64, String, String, bool)> {
    timed("deployer launches", |c| {
        let mut st = c.prepare("SELECT unix_time, deployer, mint, no_pop FROM deployer_launches WHERE unix_time >= ?1 ORDER BY unix_time")?;
        let rows = st.query_map([since as i64], |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0)))?;
        rows.collect()
    })
}

/// Every reference-wallet trade line: (time, key, lamports). All of them: the follow rule sums a
/// key's whole history.
pub fn reference_trades() -> Vec<(u64, String, i64)> {
    timed("reference trades", |c| {
        let mut st = c.prepare("SELECT unix_time, key, lamports_delta FROM reference_trades ORDER BY unix_time")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    })
}

pub fn lookup_tables() -> Vec<String> {
    timed("lookup tables", |c| {
        let mut st = c.prepare("SELECT address FROM lookup_tables ORDER BY added_ns")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    })
}

pub fn kv(key: &str) -> Option<String> {
    timed("kv", |c| {
        let mut st = c.prepare("SELECT value FROM kv WHERE key = ?1")?;
        let rows = st.query_map([key], |r| r.get(0))?;
        rows.collect()
    })
    .into_iter()
    .next()
}

/// (ip, continent, country, city)
pub fn validator_regions() -> Vec<(String, Option<String>, Option<String>, Option<String>)> {
    timed("validator regions", |c| {
        let mut st = c.prepare("SELECT ip, continent, country, city FROM validator_regions")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    })
}

/// (mint, symbol, name)
pub fn token_names() -> Vec<(String, Option<String>, Option<String>)> {
    timed("token names", |c| {
        let mut st = c.prepare("SELECT mint, symbol, name FROM token_names")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    })
}

/// A mirrored table back as its file's CSV text (header and rows, in insertion order), for code
/// that reads the file (the positions daily table).
pub fn csv_text(table: &str, header: &str) -> String {
    let cols: Vec<String> = header.split(',').map(|c| format!("\"{c}\"")).collect();
    let sql = format!("SELECT {} FROM {table} ORDER BY id", cols.join(","));
    let mut out = String::from(header);
    out.push('\n');
    let result = conn().and_then(|c| {
        let mut st = c.prepare(&sql)?;
        let n = cols.len();
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            for i in 0..n {
                if i > 0 {
                    out.push(',');
                }
                use rusqlite::types::ValueRef;
                match r.get_ref(i)? {
                    ValueRef::Null => {}
                    ValueRef::Integer(v) => out.push_str(&v.to_string()),
                    ValueRef::Real(v) => out.push_str(&v.to_string()),
                    ValueRef::Text(t) | ValueRef::Blob(t) => out.push_str(&String::from_utf8_lossy(t)),
                }
            }
            out.push('\n');
        }
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("store: {table} not read: {e}");
    }
    out
}
