//! The trade log as positions and orders (the copy bot's tables). Every row `csv_logger::log_trade`
//! writes becomes one order; a buy opens a position (or records a failed one), each sell updates
//! it, and it closes when its tokens are gone. The launch's context (deployer, follow kind, fee
//! tier, the token account's rent, the venue) is noted at the decision (`note_context`).
//!
//! Status words in the trade log: a buy landed as "simulated" (paper) or "confirmed ..."; a sell
//! as "paper" or "confirmed ..."; anything else did not land.

use {
    super::{now_ns, run_id, write, Row, Val},
    std::{
        collections::HashMap,
        sync::{LazyLock, Mutex},
    },
};

/// What the decision knew about a launch, for its position row.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub venue: &'static str,
    pub deployer: Option<String>,
    pub follow_kind: Option<&'static str>,
    pub tier: Option<String>,
    /// Rent of the token account the buy creates (live; the full sell closes the account and gets
    /// it back, so a closed position has none locked).
    pub rent_lamports: u64,
}

struct Open {
    id: i64,
    sells: u32,
    tokens_held: i64,
}

/// Open positions and the next id: the live hook's (one, global) or the importer's.
pub struct Book {
    open: HashMap<(String, &'static str), Open>,
    next_id: i64,
}

impl Book {
    /// Ids start at `first_id` (the live book: unix microseconds at start, so ids of two runs never
    /// meet; the importer: its own range).
    pub fn new(first_id: i64) -> Self {
        Self { open: HashMap::new(), next_id: first_id }
    }

    fn id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }
}

static CONTEXT: LazyLock<Mutex<HashMap<String, Context>>> = LazyLock::new(Default::default);
static BOOK: LazyLock<Mutex<Book>> = LazyLock::new(|| Mutex::new(Book::new(now_ns() / 1_000)));

/// Token account rent: classic SPL (165 bytes) and Token-2022 with the immutable-owner extension
/// (170 bytes): 1,488,440 and 1,513,840 lamports on chain on 2026-10-13 (getMinimumBalanceForRent-
/// Exemption; the owner measured 1,513,840 on six real closes). Read again from the chain at start
/// (`set_rent`, from `snipe_test::load_rent`).
pub const RENT_SPL: u64 = 1_488_440;
pub const RENT_TOKEN_2022: u64 = 1_513_840;
static RENT_NOW: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((RENT_SPL, RENT_TOKEN_2022));

/// The chain's current rents (SPL, Token-2022).
pub fn set_rent(spl: u64, token_2022: u64) {
    *RENT_NOW.lock().unwrap_or_else(|e| e.into_inner()) = (spl, token_2022);
}

/// The rent of a token account of `token_2022` or classic SPL, as the chain has it now.
pub fn rent(token_2022: bool) -> u64 {
    let r = RENT_NOW.lock().unwrap_or_else(|e| e.into_inner());
    if token_2022 { r.1 } else { r.0 }
}

pub fn note_context(mint: &str, ctx: Context) {
    if !super::db_on() {
        return;
    }
    let mut map = CONTEXT.lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 50_000 {
        map.clear();
    }
    map.insert(mint.to_string(), ctx);
}

fn lamports(sol: &str) -> Option<i64> {
    sol.trim().parse::<f64>().ok().map(|v| (v * 1e9).round() as i64)
}

fn landed(action: &str, status: &str) -> bool {
    match action {
        "BUY" => status.starts_with("simulated") || status.starts_with("confirmed"),
        _ => status.starts_with("paper") || status.starts_with("confirmed"),
    }
}

/// A trade-log row (its CSV fields by name) as an order, and its position, written to the store.
/// `mode`: "paper"/"live".
pub fn on_trade(get: impl Fn(&str) -> String, mode: &'static str) {
    if !super::db_on() {
        return;
    }
    let mint = get("mint");
    let ctx = CONTEXT.lock().unwrap_or_else(|e| e.into_inner()).get(&mint).cloned().unwrap_or_default();
    let rows = {
        let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
        rows(&mut book, &get, mode, now_ns(), &ctx, run_id())
    };
    for row in rows {
        write(row);
    }
}

/// The store rows of one trade-log row: an order, and its position's insert or update, and the
/// launch row's verdict for a buy. Pure but for the book (the importer runs it on old files).
pub fn rows(book: &mut Book, get: &impl Fn(&str) -> String, mode: &'static str, now: i64, ctx: &Context, run: &str) -> Vec<Row> {
    let mut out = Vec::with_capacity(3);
    let mint = get("mint");
    let action = get("action");
    let status = get("status");
    let ok = landed(&action, &status);
    let venue = if ctx.venue.is_empty() { if get("dex") == "pumpswap" { "pumpswap" } else { "pumpfun" } } else { ctx.venue };
    let tokens = get("token_amount").parse::<i64>().ok();
    let wallet = lamports(&get("wallet_sol_change"));
    let sig = Val::from(Some(get("signature")).filter(|s| !s.is_empty()));
    let landed_slot = Val::from(get("landed_slot").parse::<i64>().ok());
    let launch_slot = Val::from(get("launch_slot").parse::<i64>().ok());
    let order = |book: &mut Book, position_id: i64, side: &str, reason: String, requested: Option<i64>, quote: Option<i64>, step: Option<u32>| Row::Exec {
        sql: "INSERT INTO orders (client_id, position_id, mint, side, mode, reason, requested, status, sig, t_signal, t_landed,
                signal_slot, landed_slot, quote_net, tokens, wallet_delta_lamports, error, venue, run_id, sell_step,
                detect_to_ready_ms, launch_to_landed_ms, pnl_percent, decision_slot)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
        params: vec![
            book.id().into(),
            position_id.into(),
            mint.clone().into(),
            side.into(),
            mode.into(),
            reason.into(),
            requested.unwrap_or(0).into(),
            (if ok { "landed" } else if status.starts_with("timeout") { "timed_out" } else { "failed" }).into(),
            sig.clone(),
            now.into(),
            Val::from(ok.then_some(now)),
            launch_slot.clone(),
            landed_slot.clone(),
            quote.into(),
            tokens.into(),
            wallet.into(),
            Val::from((!ok).then(|| status.clone())),
            venue.into(),
            run.into(),
            step.into(),
            get("detect_to_ready_ms").parse::<f64>().ok().into(),
            get("launch_to_landed_ms").parse::<i64>().ok().into(),
            get("pnl_percent").parse::<f64>().ok().into(),
            Val::from(get("ready_slot").parse::<i64>().ok()),
        ],
    };
    if action == "BUY" {
        let id = book.id();
        let sol_in = lamports(&get("sol_in"));
        let held = if ok { tokens.unwrap_or(0) } else { 0 };
        out.push(Row::Exec {
            sql: "INSERT INTO positions (id, run_id, mint, mode, state, opened_ns, closed_ns, tokens_held, fill_quote_net, fill_tokens,
                    sol_in, wallet_delta_lamports, bought_ns, venue, dex, deployer, follow_kind, tier, rent_locked_lamports)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params: vec![
                id.into(),
                run.into(),
                mint.clone().into(),
                mode.into(),
                (if ok { "OPEN" } else { "BUY_FAILED" }).into(),
                now.into(),
                Val::from((!ok).then_some(now)),
                held.into(),
                (if ok { sol_in } else { None }).unwrap_or(0).into(),
                held.into(),
                (if ok { sol_in } else { None }).unwrap_or(0).into(),
                wallet.unwrap_or(0).into(),
                Val::from(ok.then_some(now)),
                venue.into(),
                get("dex").into(),
                ctx.deployer.clone().into(),
                ctx.follow_kind.into(),
                ctx.tier.clone().into(),
                // Rent locked in the token account: live only (a paper buy's modeled cost has none).
                (if ok && mode == "live" { ctx.rent_lamports } else { 0 }).into(),
            ],
        });
        let reason = get("reason");
        out.push(order(book, id, "buy", if reason.is_empty() { "ENTRY".into() } else { reason }, sol_in, sol_in.filter(|_| ok), None));
        out.push(Row::Exec {
            sql: "UPDATE launches SET verdict = ?1, skip_reason = COALESCE(?2, skip_reason), updated_ns = ?3 WHERE mint = ?4 AND venue = ?5",
            params: vec![(if ok { "bought" } else { "buy_failed" }).into(), Val::from((!ok).then(|| status.clone())), now.into(), mint.clone().into(), venue.into()],
        });
        if ok {
            book.open.insert((mint, mode), Open { id, sells: 0, tokens_held: held });
        }
        return out;
    }
    // A sell: its position (opened in this book), the step, and the position's totals.
    let key = (mint.clone(), mode);
    let (id, step, closed) = match book.open.get_mut(&key) {
        None => (0, None, false),
        Some(p) => {
            p.sells += 1;
            if ok {
                p.tokens_held -= tokens.unwrap_or(0);
            }
            (p.id, Some(p.sells), ok && p.tokens_held <= 0)
        }
    };
    if closed {
        book.open.remove(&key);
    }
    let sol_out = lamports(&get("sol_out"));
    out.push(order(book, id, "sell", get("reason"), tokens, sol_out.filter(|_| ok), step));
    if id != 0 && ok {
        out.push(Row::Exec {
            sql: "UPDATE positions SET sol_out = sol_out + ?1, wallet_delta_lamports = wallet_delta_lamports + ?2,
                    tokens_held = MAX(tokens_held - ?3, 0), exit_reason = ?4,
                    state = CASE WHEN ?5 THEN 'CLOSED' ELSE state END,
                    closed_ns = CASE WHEN ?5 THEN ?6 ELSE closed_ns END,
                    rent_locked_lamports = CASE WHEN ?5 THEN 0 ELSE rent_locked_lamports END
                  WHERE id = ?7",
            params: vec![
                sol_out.unwrap_or(0).into(),
                wallet.unwrap_or(0).into(),
                tokens.unwrap_or(0).into(),
                get("reason").into(),
                closed.into(),
                now.into(),
                id.into(),
            ],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_that_landed() {
        assert!(landed("BUY", "simulated") && landed("BUY", "confirmed (slot 4)"));
        assert!(!landed("BUY", "lost race: price moved") && !landed("BUY", "failed") && !landed("BUY", "simulation failed: x"));
        assert!(landed("SELL", "paper") && landed("SELL", "confirmed") && !landed("SELL", "failed"));
        assert_eq!(lamports("0.000065000"), Some(65_000));
        assert_eq!(lamports("-0.002070000"), Some(-2_070_000));
    }
}
