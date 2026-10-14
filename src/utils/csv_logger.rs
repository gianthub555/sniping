use {
    log::warn,
    std::{
        fs::OpenOptions,
        io::Write,
        sync::{LazyLock, Mutex},
    },
};

/// Live trades. Paper (DRY_RUN) trades go to their own file: the two histories never mix.
pub const TRADE_LOG_PATH: &str = "auto_sell_logs.csv";
pub const PAPER_TRADE_LOG_PATH: &str = "auto_sell_logs_paper.csv";

/// The trade log of the mode the bot runs in.
pub fn trade_log_path() -> &'static str {
    if crate::config::TRADE_SETTING.dry_run { PAPER_TRADE_LOG_PATH } else { TRADE_LOG_PATH }
}

/// "live" or "paper": which history a row belongs to.
pub fn mode_of_path(path: &str) -> &'static str {
    if path == PAPER_TRADE_LOG_PATH { "paper" } else { "live" }
}

const HEADER: &str = "timestamp,action,dex,mint,signature,status,sol_in,sol_out,token_amount,pnl_percent,reason,\
wallet_sol_change,launch_slot,ready_slot,landed_slot,detect_to_ready_ms,launch_to_landed_ms";
const COLUMNS: usize = 17;
/// The store's `trade_log` columns: the mode, then the file's.
const STORE_HEADER: &str = "mode,timestamp,action,dex,mint,signature,status,sol_in,sol_out,token_amount,pnl_percent,reason,\
wallet_sol_change,launch_slot,ready_slot,landed_slot,detect_to_ready_ms,launch_to_landed_ms";

static FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Clone, Default)]
pub struct TradeRecord {
    
    pub action: &'static str,
    pub dex: String,
    pub mint: String,
    pub signature: String,
    /// sent, simulated, confirmed, failed, timeout
    pub status: String,
    /// SOL put into the swap (buy amount, or the position's entry cost on a sell).
    pub sol_in: Option<f64>,
    
    pub sol_out: Option<f64>,
    pub token_amount: Option<u64>,
    pub pnl_percent: Option<f64>,
    
    pub reason: String,
    
    
    pub wallet_sol_change: Option<f64>,
    
    pub launch_slot: Option<u64>,
    
    pub ready_slot: Option<u64>,
    
    pub landed_slot: Option<u64>,
    
    pub detect_to_ready_ms: Option<f64>,
    
    
    pub launch_to_landed_ms: Option<u64>,
}



pub fn log_trade(record: &TradeRecord) {
    let fields: [(&str, String); COLUMNS] = [
        ("timestamp", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        ("action", record.action.to_string()),
        ("dex", record.dex.clone()),
        ("mint", record.mint.clone()),
        ("signature", record.signature.clone()),
        ("status", record.status.clone()),
        ("sol_in", opt(record.sol_in.map(|v| format!("{v:.9}")))),
        ("sol_out", opt(record.sol_out.map(|v| format!("{v:.9}")))),
        ("token_amount", opt(record.token_amount.map(|v| v.to_string()))),
        ("pnl_percent", opt(record.pnl_percent.map(|v| format!("{v:.2}")))),
        ("reason", record.reason.clone()),
        ("wallet_sol_change", opt(record.wallet_sol_change.map(|v| format!("{v:.9}")))),
        ("launch_slot", opt(record.launch_slot.map(|v| v.to_string()))),
        ("ready_slot", opt(record.ready_slot.map(|v| v.to_string()))),
        ("landed_slot", opt(record.landed_slot.map(|v| v.to_string()))),
        ("detect_to_ready_ms", opt(record.detect_to_ready_ms.map(|v| format!("{v:.2}")))),
        ("launch_to_landed_ms", opt(record.launch_to_landed_ms.map(|v| v.to_string()))),
    ];
    let line = fields.iter().map(|(_, value)| escape(value)).collect::<Vec<_>>().join(",");

    let path = trade_log_path();
    // The snipe test (SNIPE_TEST=1): its record of the snipe this row belongs to.
    crate::utils::snipe_test::on_trade(record, mode_of_path(path));
    // The whale rule (WHALE_MODE=1): its record of the trade this row belongs to.
    crate::processors::whale::on_trade(record, mode_of_path(path));
    // The store (STORE_DB=on): the row as it is in the file, and as an order of a position.
    if crate::store::db_on() {
        let mode = mode_of_path(path);
        let mut vals = vec![crate::store::Val::from(mode)];
        vals.extend(fields.iter().map(|(_, v)| crate::store::field(v)));
        crate::store::write(crate::store::Row::Mirror { table: "trade_log", header: STORE_HEADER, vals, rx_ns: crate::store::now_ns() });
        crate::store::trades::on_trade(|k| fields.iter().find(|(name, _)| *name == k).map(|(_, v)| v.clone()).unwrap_or_default(), mode);
    }
    if crate::store::files_on() {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            if file.metadata()?.len() == 0 {
                writeln!(file, "{HEADER}")?;
            }
            writeln!(file, "{line}")
        })();
        if let Err(e) = result {
            warn!("could not write {path}: {e}");
        }
    }
    let mut row: std::collections::BTreeMap<String, String> = fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    row.insert("mode".into(), mode_of_path(path).into());
    super::dashboard::record_trade_row(row);
}




pub fn upgrade_log_columns() {
    for path in [TRADE_LOG_PATH, PAPER_TRADE_LOG_PATH] {
        upgrade_columns(path);
    }
}

fn upgrade_columns(path: &str) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return;
    };
    if header == HEADER {
        return;
    }
    let old_columns = header.split(',').count();
    if !HEADER.starts_with(header) || old_columns >= COLUMNS {
        warn!("{path} has unexpected columns; new rows may not line up with its header");
        return;
    }
    let padding = ",".repeat(COLUMNS - old_columns);
    let mut upgraded = String::with_capacity(text.len() + 64);
    upgraded.push_str(HEADER);
    upgraded.push('\n');
    for line in lines.filter(|line| !line.trim().is_empty()) {
        upgraded.push_str(line);
        upgraded.push_str(&padding);
        upgraded.push('\n');
    }
    let backup = format!("{path}.bak");
    let result = std::fs::copy(path, &backup).and_then(|_| std::fs::write(path, upgraded));
    match result {
        Ok(()) => log::info!("{path}: added timing columns (previous file saved as {backup})"),
        Err(e) => warn!("could not add timing columns to {path}: {e}"),
    }
}

fn opt(value: Option<String>) -> String {
    value.unwrap_or_default()
}

fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}
