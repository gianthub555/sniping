//! Versioned schema migrations tracked in `PRAGMA user_version` (as the copy bot's store). Append
//! only: never edit a shipped file. A database newer than this build is refused.

use rusqlite::{Connection, OpenFlags};
use std::path::Path;

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    ("0002_snipe_test", include_str!("migrations/0002_snipe_test.sql")),
    ("0003_whale", include_str!("migrations/0003_whale.sql")),
    ("0004_bundle10", include_str!("migrations/0004_bundle10.sql")),
];

/// The tapes' day files have their own one-table schema.
pub const TAPES_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tapes (
    id     INTEGER PRIMARY KEY,
    rx_ns  INTEGER NOT NULL,
    mint   TEXT NOT NULL,
    bt     INTEGER,
    slot   INTEGER,
    json   TEXT NOT NULL            -- the line logs/tapes/<day>.jsonl holds
);
CREATE INDEX IF NOT EXISTS tapes_mint ON tapes (mint);
";

pub fn latest_version() -> u32 {
    MIGRATIONS.len() as u32
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema version {found} is newer than this build's {latest}: refusing to open it")]
    Newer { found: u32, latest: u32 },
}

/// Applies every migration newer than the database's `user_version`, each in its own transaction.
pub fn migrate(conn: &mut Connection) -> Result<u32, OpenError> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > latest_version() {
        return Err(OpenError::Newer { found: current, latest: latest_version() });
    }
    for (i, (name, sql)) in MIGRATIONS.iter().enumerate() {
        let version = i as u32 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        log::info!("store: applied schema migration {name} (version {version})");
    }
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

fn pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(())
}

/// Opens (creating it and its directory if needed) the database with the writer's pragmas, and
/// migrates it.
pub fn open_writer(path: &Path) -> Result<Connection, OpenError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut conn = Connection::open(path)?;
    pragmas(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// A tapes day file (created with its table if needed).
pub fn open_tapes(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)?;
    pragmas(&conn)?;
    conn.execute_batch(TAPES_SCHEMA)?;
    Ok(conn)
}

/// A read-only connection (startup loads, reports, tests; the dashboard does the same in Node).
pub fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_database_gets_every_table_and_a_newer_one_is_refused() {
        let dir = std::env::temp_dir().join(format!("bigboss-migrate-{}", std::process::id()));
        let path = dir.join("t.sqlite");
        let conn = open_writer(&path).unwrap();
        let v: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, latest_version());
        let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0)).unwrap();
        assert!(tables >= 29, "{tables}");
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        // Opening again changes nothing.
        drop(conn);
        assert_eq!(open_writer(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0)).unwrap(), latest_version());
        // A database from a newer build is refused.
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", latest_version() + 1).unwrap();
        drop(c);
        assert!(matches!(open_writer(&path), Err(OpenError::Newer { .. })));
        let _ = std::fs::remove_dir_all(dir);
    }
}
