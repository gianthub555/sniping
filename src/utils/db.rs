//! The scorecard's data (2026-10-12), now in the store (`store`, `data/bigboss.sqlite`; first
//! kept in `DB_PATH`, default `bigboss.db`, whose rows the store copies over once).
//! Tables:
//! - `deployer_wallet`: each deployer wallet's first transaction (time, the SOL it received),
//!   looked up once over RPC (`utils::deployer_wallet`);
//! - `launch_result`: every followed launch's replayed result for a buy right behind the create's
//!   bundle (`utils::launch_results`), the scorecard's "deployer's earlier launches" facts;
//! - `score`: every followed launch the entry scored, with its facts and the verdict;
//! - `exit_decision`: every sell decision of pXGQ's exit (`SELL_RULE=pxgq`).
//!
//! Every write is a row for the store's writer thread (batched, never blocking the caller); the hot
//! path never touches SQLite. Reads happen at startup, to fill the in-memory caches.

use {
    crate::store::{self, Row, Val},
    rusqlite::Connection,
};

/// One row to write.
#[derive(Debug, Clone)]
pub enum Write {
    DeployerWallet { key: String, first_bt: Option<i64>, funded_sol: Option<f64>, funder: Option<String>, checked_at: i64 },
    LaunchResult { mint: String, deployer: String, creator: String, created: i64, pnl_sol: f64, down: bool, no_pop: bool },
    Score { mint: String, t: i64, deployer: String, creator: String, path: &'static str, score: i32, threshold: i32, passed: bool, verdict: String, facts: String },
    ExitDecision { mint: String, t: i64, paper: bool, reason: &'static str, share: f64, value_x: f64, peak_x: f64, held_secs: f64, net_behind_sol: f64 },
}

/// The database file in use (the store's).
pub fn path() -> String {
    store::path().display().to_string()
}

/// Opens the store (tests and tools: a writer connection at `path`, migrated).
pub fn open(path: &str) -> rusqlite::Result<Connection> {
    store::migrate::open_writer(std::path::Path::new(path)).map_err(|e| match e {
        store::migrate::OpenError::Sqlite(e) => e,
        other => rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(other.to_string()))),
    })
}

/// Opens the store (`store::init`). Call once at startup.
pub fn init() -> anyhow::Result<()> {
    store::init(crate::config::TRADE_SETTING.dry_run).map(|_| ())
}

/// Queues a row for the store's writer (dropped before `init`).
pub fn write(w: Write) {
    let (sql, params) = statement(&w);
    store::write(Row::Exec { sql, params });
}

/// The statement and parameters of a row.
fn statement(w: &Write) -> (&'static str, Vec<Val>) {
    match w {
        Write::DeployerWallet { key, first_bt, funded_sol, funder, checked_at } => (
            "INSERT INTO deployer_wallet (key, first_bt, funded_sol, funder, checked_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET first_bt = ?2, funded_sol = ?3, funder = ?4, checked_at = ?5",
            vec![key.clone().into(), (*first_bt).into(), (*funded_sol).into(), funder.clone().into(), (*checked_at).into()],
        ),
        Write::LaunchResult { mint, deployer, creator, created, pnl_sol, down, no_pop } => (
            "INSERT OR REPLACE INTO launch_result (mint, deployer, creator, created, pnl_sol, down, no_pop) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            vec![mint.clone().into(), deployer.clone().into(), creator.clone().into(), (*created).into(), (*pnl_sol).into(), (*down).into(), (*no_pop).into()],
        ),
        Write::Score { mint, t, deployer, creator, path, score, threshold, passed, verdict, facts } => (
            "INSERT INTO score (mint, t, deployer, creator, path, score, threshold, passed, verdict, facts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(mint) DO UPDATE SET verdict = CASE WHEN excluded.verdict = 'buy' THEN 'buy' ELSE score.verdict END",
            vec![
                mint.clone().into(),
                (*t).into(),
                deployer.clone().into(),
                creator.clone().into(),
                (*path).into(),
                (*score as i64).into(),
                (*threshold as i64).into(),
                (*passed).into(),
                verdict.clone().into(),
                facts.clone().into(),
            ],
        ),
        Write::ExitDecision { mint, t, paper, reason, share, value_x, peak_x, held_secs, net_behind_sol } => (
            "INSERT INTO exit_decision (mint, t, paper, reason, share, value_x, peak_x, held_secs, net_behind_sol) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            vec![
                mint.clone().into(),
                (*t).into(),
                (*paper).into(),
                (*reason).into(),
                (*share).into(),
                (*value_x).into(),
                (*peak_x).into(),
                (*held_secs).into(),
                (*net_behind_sol).into(),
            ],
        ),
    }
}

/// Writes a row here and now (tests and tools).
pub fn apply(conn: &Connection, w: &Write) -> rusqlite::Result<()> {
    let (sql, params) = statement(w);
    conn.execute(sql, rusqlite::params_from_iter(params.iter())).map(|_| ())
}

/// Every cached deployer wallet: (key, first transaction's unix time, SOL it received).
pub fn load_deployer_wallets(conn: &Connection) -> rusqlite::Result<Vec<(String, Option<i64>, Option<f64>)>> {
    let mut stmt = conn.prepare("SELECT key, first_bt, funded_sol FROM deployer_wallet")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

/// Every recorded launch result, oldest first: (creator, created, pnl_sol, down).
pub fn load_launch_results(conn: &Connection) -> rusqlite::Result<Vec<(String, i64, f64, bool)>> {
    let mut stmt = conn.prepare("SELECT creator, created, pnl_sol, down FROM launch_result ORDER BY created")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    rows.collect()
}

/// A read-only connection to the database in use (startup loads; the writer has its own).
pub fn read() -> rusqlite::Result<Connection> {
    store::migrate::open_reader(&store::path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip_and_a_score_keeps_its_buy() {
        let dir = std::env::temp_dir().join(format!("bigboss-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.db");
        let conn = open(file.to_str().unwrap()).unwrap();
        apply(&conn, &Write::DeployerWallet { key: "k".into(), first_bt: Some(100), funded_sol: Some(1.5), funder: Some("f".into()), checked_at: 5 }).unwrap();
        apply(&conn, &Write::DeployerWallet { key: "k".into(), first_bt: Some(90), funded_sol: Some(2.0), funder: None, checked_at: 6 }).unwrap();
        assert_eq!(load_deployer_wallets(&conn).unwrap(), vec![("k".to_string(), Some(90), Some(2.0))]);
        for (mint, created, pnl) in [("m2", 20, -0.1), ("m1", 10, 0.2)] {
            let w = Write::LaunchResult { mint: mint.into(), deployer: "d".into(), creator: "c".into(), created, pnl_sol: pnl, down: pnl < 0.0, no_pop: false };
            apply(&conn, &w).unwrap();
        }
        assert_eq!(load_launch_results(&conn).unwrap(), vec![("c".to_string(), 10, 0.2, false), ("c".to_string(), 20, -0.1, true)]);
        let score = |verdict: &str| Write::Score {
            mint: "m".into(), t: 1, deployer: "d".into(), creator: "c".into(), path: "hot", score: 70, threshold: 61, passed: true,
            verdict: verdict.into(), facts: "{}".into(),
        };
        apply(&conn, &score("buy")).unwrap();
        apply(&conn, &score("positions already open")).unwrap();
        let v: String = conn.query_row("SELECT verdict FROM score WHERE mint = 'm'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "buy");
        apply(&conn, &Write::ExitDecision { mint: "m".into(), t: 1, paper: true, reason: "SCALE", share: 0.5, value_x: 1.3, peak_x: 1.3, held_secs: 4.0, net_behind_sol: 2.0 }).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM exit_decision", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
