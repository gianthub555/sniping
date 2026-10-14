//! Address lookup tables for the early stream. A v0 transaction names some accounts by position
//! in a lookup table, and wire bytes (shreds, the preprocessed stream) carry only those positions,
//! so the launch parser saw them as unknown. On 26 Sep 2026 that sent 27% of the bot's snipes to
//! the regular stream, 4-5 ms later: their creates take wrapped SOL (the quote mint check) from one
//! of two lookup tables. A table is fetched over RPC in the background the first time a launch
//! uses it (other trades' tables are left alone), kept, and remembered in `KNOWN_FILE` so the
//! next start loads it before the first launch. A position past the cached end (the table was
//! extended) fetches it again.

use {
    crate::config::RPC_CLIENT,
    log::{debug, info},
    solana_commitment_config::CommitmentConfig,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::{Arc, LazyLock, Mutex},
        time::{Duration, Instant},
    },
};

/// Lookup table account data: 56 bytes of metadata, then the addresses.
const META_SIZE: usize = 56;
/// A table that failed to load, or was extended, is not asked for again sooner than this.
const REFETCH_AFTER: Duration = Duration::from_secs(10);
/// Tables launches have used, one per line, loaded at start.
const KNOWN_FILE: &str = "logs/lookup_tables.txt";

#[derive(Default)]
struct Table {
    addresses: Arc<Vec<Pubkey>>,
    requested: Option<Instant>,
}

static TABLES: LazyLock<Mutex<HashMap<Pubkey, Table>>> = LazyLock::new(Default::default);

/// The address at `index` of `table`, when cached. A cached table too short for `index` was
/// extended since: it is fetched again.
pub fn resolve(table: &Pubkey, index: u8) -> Option<Pubkey> {
    let (found, stale) = {
        let tables = TABLES.lock().unwrap_or_else(|e| e.into_inner());
        match tables.get(table) {
            Some(t) => (t.addresses.get(index as usize).copied(), !t.addresses.is_empty()),
            None => (None, false),
        }
    };
    if found.is_none() && stale {
        request(*table);
    }
    found
}

/// Fetches `table` in the background unless it is cached already (a launch uses it).
pub fn ensure(table: &Pubkey) {
    let cached = TABLES.lock().unwrap_or_else(|e| e.into_inner()).get(table).is_some_and(|t| !t.addresses.is_empty());
    if !cached {
        request(*table);
    }
}

/// Loads the tables earlier launches used (`KNOWN_FILE`).
pub fn warm() {
    let tables: Vec<Pubkey> = if crate::store::read_from_db() {
        crate::store::read::lookup_tables().iter().filter_map(|l| l.trim().parse().ok()).collect()
    } else {
        let known = std::fs::read_to_string(KNOWN_FILE).unwrap_or_default();
        known.lines().filter_map(|l| l.trim().parse().ok()).collect()
    };
    if !tables.is_empty() {
        info!("lookup tables: loading {} that launches used before", tables.len());
    }
    tables.into_iter().for_each(request);
}

fn remember(table: &Pubkey) {
    if crate::store::db_on() {
        crate::store::write(crate::store::Row::Exec {
            sql: "INSERT OR IGNORE INTO lookup_tables (address, added_ns) VALUES (?1, ?2)",
            params: vec![table.to_string().into(), crate::store::now_ns().into()],
        });
    }
    if !crate::store::files_on() {
        return;
    }
    let known = std::fs::read_to_string(KNOWN_FILE).unwrap_or_default();
    if known.lines().any(|l| l.trim() == table.to_string()) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(KNOWN_FILE) {
        let _ = std::io::Write::write_all(&mut f, format!("{table}\n").as_bytes());
    }
}

/// Fetches `table` in the background unless it was asked for recently. Needs a Tokio runtime
/// (without one, as in unit tests, nothing is fetched).
pub fn request(table: Pubkey) {
    {
        let mut tables = TABLES.lock().unwrap_or_else(|e| e.into_inner());
        let entry = tables.entry(table).or_default();
        if entry.requested.is_some_and(|at| at.elapsed() < REFETCH_AFTER) {
            return;
        }
        entry.requested = Some(Instant::now());
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    runtime.spawn(async move {
        match RPC_CLIENT.get_account_with_commitment(&table, CommitmentConfig::processed()).await {
            Ok(response) => match response.value.map(|a| addresses(&a.data)) {
                Some(Some(addresses)) => {
                    let n = addresses.len();
                    TABLES.lock().unwrap_or_else(|e| e.into_inner()).entry(table).or_default().addresses = Arc::new(addresses);
                    remember(&table);
                    info!("lookup table {table} cached: {n} addresses");
                }
                _ => debug!("lookup table {table}: not a lookup table"),
            },
            Err(e) => debug!("lookup table {table}: {e}"),
        }
    });
}

/// The addresses stored in lookup table account data.
fn addresses(data: &[u8]) -> Option<Vec<Pubkey>> {
    let body = data.get(META_SIZE..)?;
    if body.len() % 32 != 0 {
        return None;
    }
    body.chunks_exact(32).map(|c| Pubkey::try_from(c).ok()).collect()
}

/// Test hook: a table's addresses, as if fetched.
#[cfg(test)]
/// Drops a cached table (tests that need it uncached; other tests may have cached it).
#[cfg(test)]
pub fn forget(table: &Pubkey) {
    TABLES.lock().unwrap_or_else(|e| e.into_inner()).remove(table);
}

pub fn insert(table: Pubkey, addresses: Vec<Pubkey>) {
    TABLES.lock().unwrap_or_else(|e| e.into_inner()).insert(table, Table { addresses: Arc::new(addresses), requested: None });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_data_is_metadata_then_addresses() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let mut data = vec![0u8; META_SIZE];
        data.extend_from_slice(a.as_ref());
        data.extend_from_slice(b.as_ref());
        assert_eq!(addresses(&data), Some(vec![a, b]));
        assert_eq!(addresses(&data[..META_SIZE + 5]), None);
    }

    #[test]
    fn a_cached_table_resolves_and_a_missing_index_does_not() {
        let table = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        insert(table, vec![a]);
        assert_eq!(resolve(&table, 0), Some(a));
        assert_eq!(resolve(&table, 1), None);
        assert_eq!(resolve(&Pubkey::new_unique(), 0), None);
    }
}
