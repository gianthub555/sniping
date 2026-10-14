






use {
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicBool, Ordering},
            LazyLock, Mutex,
        },
    },
    tokio::sync::Notify,
};

static TRADING: AtomicBool = AtomicBool::new(false);
/// A stop for good (`stop_buying`): buying stays off whatever the dashboard asks, until restart.
static STOPPED: AtomicBool = AtomicBool::new(false);
static STOP_REASON: Mutex<Option<String>> = Mutex::new(None);


pub fn trading_enabled() -> bool {
    TRADING.load(Ordering::Relaxed) && !STOPPED.load(Ordering::Relaxed)
}

/// Stops buying until the process restarts (a live test's limit or kill switch). Open positions are
/// still sold. Returns false when it was already stopped (the first reason is kept).
pub fn stop_buying(reason: &str) -> bool {
    if STOPPED.swap(true, Ordering::SeqCst) {
        return false;
    }
    *STOP_REASON.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.to_string());
    true
}

/// Why buying was stopped for good, if it was.
pub fn stop_reason() -> Option<String> {
    STOP_REASON.lock().unwrap_or_else(|e| e.into_inner()).clone()
}


pub fn set_trading(enabled: bool) -> bool {
    TRADING.swap(enabled, Ordering::Relaxed)
}


static SELL_REQUESTS: LazyLock<Mutex<HashSet<Pubkey>>> = LazyLock::new(Default::default);
static SELL_REQUESTED: LazyLock<Notify> = LazyLock::new(Notify::new);


pub fn request_sell(mint: Pubkey) {
    SELL_REQUESTS.lock().unwrap_or_else(|e| e.into_inner()).insert(mint);
    SELL_REQUESTED.notify_waiters();
}


pub fn take_sell_request(mint: &Pubkey) -> bool {
    SELL_REQUESTS.lock().unwrap_or_else(|e| e.into_inner()).remove(mint)
}


pub fn forget_sell_request(mint: &Pubkey) {
    SELL_REQUESTS.lock().unwrap_or_else(|e| e.into_inner()).remove(mint);
}


pub async fn sell_requested() {
    SELL_REQUESTED.notified().await;
}
