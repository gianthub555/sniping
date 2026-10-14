//! Loads reference wallets' pump.fun trade history into the follow rule's history file
//! (`reference_deployers.txt`, as the stream records them: block time, the coin's creator and
//! deployer, SOL out or in). For a wallet newly added to REFERENCE_WALLETS, whose past the bot's
//! startup catch-up (3 days at most, from the file's last line) would not reach. Reads RPC only
//! (Helius getTransactionsForAddress, RPC_ENDPOINT from .env); sends nothing.
//!
//!   reference_backfill [--days N] <wallet>...   every trade of each wallet in the last N days
//!                                               (default 7), appended to the file in the working
//!                                               directory
//!
//! Run it while the bot is stopped, or accept that a trade the running bot also records live may
//! be written twice for the days that overlap (a deployer's profit is then counted twice).
use {anyhow::{anyhow, Result}, solana_pubkey::Pubkey, std::str::FromStr};

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    // Each line written before the tool exits (no writer thread to drain).
    std::env::set_var("ASYNC_FILE_WRITES", "false");
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut days = 7u64;
    if let Some(i) = args.iter().position(|a| a == "--days") {
        days = args.get(i + 1).and_then(|d| d.parse().ok()).ok_or_else(|| anyhow!("--days N"))?;
        args.drain(i..=i + 1);
    }
    let wallets: Vec<Pubkey> = args.iter().map(|a| Pubkey::from_str(a).map_err(|_| anyhow!("not a key: {a}"))).collect::<Result<_>>()?;
    if wallets.is_empty() {
        return Err(anyhow!("usage: reference_backfill [--days N] <wallet>..."));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let since = now.saturating_sub(days * 86_400);
    let known = sniper_bot::utils::reference::load();
    sniper_bot::utils::creator_history::load(now);
    println!("{} keys in {} before; loading {days} days for {} wallet(s)", known, sniper_bot::config::rules::REFERENCE_TRADES_FILE, wallets.len());
    let rt = tokio::runtime::Runtime::new()?;
    for w in wallets {
        let started = std::time::Instant::now();
        let (trades, coins) = rt.block_on(sniper_bot::utils::reference::backfill_wallet(w, since, now))?;
        println!("{w}: {trades} trades on {coins} coins in {:.0} s", started.elapsed().as_secs_f64());
    }
    println!("{} keys in the history now; followed right now: {}", sniper_bot::utils::reference::keys().len(), sniper_bot::utils::reference::followed_count(now));
    Ok(())
}
