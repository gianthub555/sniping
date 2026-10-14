//! bundle10: buy right behind a big create bundle (the owner's spec of 2026-10-13). The count, the
//! rule and the exit as pure, allocation-free logic; the engine's bundle window, the shadow log and
//! the live path call these. Switches and numbers: `config::bundle10`.
//!
//! The count, as the research defines it: the transactions of the create's slot right after the
//! create (block index create+1, create+2, ...), stopping at the first that is not a buy of this
//! coin, at a gap in the index, or after 4; the distinct buyers in them besides the dev and the
//! creator. A buyer that cannot be read does not count. Every buy of each transaction counts
//! (bundlers put several wallets in one transaction). A transaction is a buy of the coin when its
//! FIRST trade of the coin is a buy (the research's decoder): its later trades of the coin count
//! too, a sell among them included (`mixed`), and move the post-bundle curve.
//!
//! Exact where the feed gives the block index (the regular stream, after execution: its trade
//! events, at any CPI depth). The early feeds (preprocessed, entry batches) have no index and carry
//! only pump.fun transactions, so a gap cannot be seen there: the count takes the coin's trades in
//! arrival order and passes over transactions that do not touch the coin (they cannot be placed
//! without an index). Shadow logs both counts and how often they agree.

use {
    super::decode::Buy,
    crate::{config::bundle10::Settings, instructions::pumpfun_buy_ix::CurveReserves, processors::shred_feed},
    solana_pubkey::Pubkey,
    std::sync::atomic::{AtomicBool, AtomicU64},
};

/// The rule: at least this many distinct bundle buyers.
pub const MIN_COUNT: u32 = 10;
/// Shadow logs a launch when either count reaches this.
pub const SHADOW_MIN_COUNT: u32 = 8;
/// At most this many transactions after the create are counted.
pub const MAX_TXS: usize = 4;
/// The exit arms once the price reaches this many times the fill.
pub const ARM_X: f64 = 1.05;
/// The standard curve's virtual reserves at launch (the rule takes only these).
pub const INITIAL_VSOL: u64 = 30_000_000_000;
pub const INITIAL_VTOK: u64 = 1_073_000_000_000_000;
/// Distinct buyers told apart (beyond, each new one counts without the check: far past 10).
const MAX_WALLETS: usize = 32;
/// Trades of the coin kept for the quote (the research saw up to 56 bundle wallets); a bundle
/// with more is refused (`Miss::TooManyBuys`).
pub const MAX_BUYS: usize = 128;

/// The feed threads hand the decider each trade of a just-created coin (`engine::TradeEv`): set
/// at the engine's start when BUNDLE10_SHADOW is on. Off, a feed thread checks this flag once a
/// frame and does nothing else for bundle10.
pub static FEED: AtomicBool = AtomicBool::new(false);
/// Shadow records a full ring dropped (the engine never waits for the shadow log).
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

/// One pump.fun trade of some coin: a top-level instruction (early feeds) or a trade event (the
/// regular stream).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trade {
    pub mint: Pubkey,
    pub buy: bool,
    /// The buyer or seller (None: not readable).
    pub user: Option<Pubkey>,
    /// A buy: tokens exact (or 0) and SOL in (or its cap), as `decode::Buy`; a sell: tokens and
    /// the minimum SOL out. A trade event gives both exactly.
    pub tokens: u64,
    pub lamports: u64,
}

impl Trade {
    pub fn of_buy(b: &Buy) -> Self {
        Trade { mint: b.mint, buy: true, user: b.buyer, tokens: b.tokens, lamports: b.lamports }
    }
}

/// Why the count stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// A transaction whose first trade of the coin is a sell.
    NotABuy,
    /// The block index skipped one: a transaction with no trade of this coin (exact feeds only).
    Gap,
    /// Four transactions were counted.
    Four,
}

impl Stop {
    pub fn code(s: Option<Stop>) -> u8 {
        match s {
            None => 0,
            Some(Stop::NotABuy) => 1,
            Some(Stop::Gap) => 2,
            Some(Stop::Four) => 3,
        }
    }

    pub fn name(code: u8) -> &'static str {
        ["open", "not_a_buy", "gap", "four"].get(code as usize).copied().unwrap_or("?")
    }
}

/// One trade of the coin in a counted transaction, for the quote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Op {
    sell: bool,
    tokens: u64,
    lamports: u64,
}

/// bundle10's count of one create's bundle, fed one trade at a time.
#[derive(Clone, Copy, Debug)]
pub struct Count {
    mint: Pubkey,
    dev: Pubkey,
    creator: Pubkey,
    /// Exact feeds: the block index the next transaction must have to be counted.
    next_index: Option<u32>,
    stop: Option<Stop>,
    txs: usize,
    /// The counted transactions' first signature bytes (the shadow's check that they executed).
    sigs: [[u8; 8]; MAX_TXS],
    /// Each counted transaction's next trade (its place in the transaction): a frame seen twice,
    /// on two feeds, counts once.
    next_seq: [u16; MAX_TXS],
    wallets: [Pubkey; MAX_WALLETS],
    distinct: u32,
    unreadable: u32,
    mixed: bool,
    ops: [Op; MAX_BUYS],
    n_ops: usize,
    /// Trades past MAX_BUYS (left out of the quote).
    dropped: u32,
}

impl Count {
    /// The count of `mint`'s bundle; `create_index` is the create's block index where the feed
    /// gives one (then the count is exact), None on the early feeds.
    pub fn new(mint: Pubkey, dev: Pubkey, creator: Pubkey, create_index: Option<u32>) -> Self {
        Count {
            mint,
            dev,
            creator,
            next_index: create_index.map(|i| i + 1),
            stop: None,
            txs: 0,
            sigs: [[0; 8]; MAX_TXS],
            next_seq: [0; MAX_TXS],
            wallets: [Pubkey::default(); MAX_WALLETS],
            distinct: 0,
            unreadable: 0,
            mixed: false,
            ops: [Op::default(); MAX_BUYS],
            n_ops: 0,
            dropped: 0,
        }
    }

    /// One trade of the create's slot after the create: in block order with its transaction's
    /// `index` (exact feeds) or in arrival order with None (early feeds). `sig`: its transaction's
    /// first signature bytes; `seq`: its place among the transaction's trades. Trades of other
    /// coins are passed over. Whether it was counted.
    pub fn event(&mut self, index: Option<u32>, sig: [u8; 8], seq: u16, t: &Trade) -> bool {
        if t.mint != self.mint {
            return false;
        }
        // A later trade of a counted transaction (or the same one again, from a second feed).
        if let Some(i) = self.sigs[..self.txs].iter().position(|s| *s == sig) {
            if seq < self.next_seq[i] {
                return false;
            }
            self.next_seq[i] = seq + 1;
            self.apply(t);
            return true;
        }
        if self.stop.is_some() {
            return false;
        }
        if let (Some(want), Some(got)) = (self.next_index, index) {
            if got < want {
                return false;
            }
            if got > want {
                self.stop = Some(Stop::Gap);
                return false;
            }
            self.next_index = Some(want + 1);
        }
        // The transaction's first trade of the coin decides it.
        if !t.buy {
            self.stop = Some(Stop::NotABuy);
            return false;
        }
        self.sigs[self.txs] = sig;
        self.next_seq[self.txs] = seq + 1;
        self.txs += 1;
        self.apply(t);
        if self.txs == MAX_TXS {
            self.stop = Some(Stop::Four);
        }
        true
    }

    /// A whole transaction, its trades in order. Whether any of them was counted.
    pub fn tx(&mut self, index: Option<u32>, sig: [u8; 8], trades: &[Trade]) -> bool {
        let mut counted = false;
        for (k, t) in trades.iter().enumerate() {
            counted |= self.event(index, sig, k as u16, t);
        }
        counted
    }

    fn apply(&mut self, t: &Trade) {
        if self.n_ops < MAX_BUYS {
            self.ops[self.n_ops] = Op { sell: !t.buy, tokens: t.tokens, lamports: t.lamports };
            self.n_ops += 1;
        } else {
            self.dropped += 1;
        }
        if !t.buy {
            self.mixed = true;
            return;
        }
        match t.user {
            None => self.unreadable += 1,
            Some(w) if w == self.dev || w == self.creator => {}
            Some(w) => {
                let seen = self.distinct as usize;
                if !self.wallets[..seen.min(MAX_WALLETS)].contains(&w) {
                    if seen < MAX_WALLETS {
                        self.wallets[seen] = w;
                    }
                    self.distinct += 1;
                }
            }
        }
    }

    /// The count: distinct buyers besides the dev and the creator.
    pub fn wallets(&self) -> u32 {
        self.distinct
    }

    /// Transactions counted (0-4).
    pub fn txs(&self) -> usize {
        self.txs
    }

    pub fn stop(&self) -> Option<Stop> {
        self.stop
    }

    /// The counted transactions' first signature bytes.
    pub fn sigs(&self) -> &[[u8; 8]] {
        &self.sigs[..self.txs]
    }

    /// Buys in the counted transactions whose buyer could not be read.
    pub fn unreadable(&self) -> u32 {
        self.unreadable
    }

    /// A counted transaction sold the coin too (after buying it first).
    pub fn mixed(&self) -> bool {
        self.mixed
    }

    /// Trades of the coin in the counted transactions.
    pub fn trades(&self) -> u32 {
        self.n_ops as u32 + self.dropped
    }

    /// More trades than the quote keeps: the decision refuses it.
    pub fn overflow(&self) -> bool {
        self.dropped > 0
    }

    /// The curve after the whole bundle: every counted trade (the dev's, the creator's and
    /// unreadable buyers' too, and a mixed transaction's sells), applied in order to the curve
    /// after the dev buy.
    pub fn post_bundle(&self, after_dev: CurveReserves, fee_bps: u64) -> Post {
        let mut r = after_dev;
        let mut largest = 0;
        let mut buys = 0;
        for op in &self.ops[..self.n_ops] {
            if op.sell {
                r = apply_sell(r, op.tokens);
            } else {
                let before = r.real_sol_reserves;
                r = shred_feed::apply_buy(r, op.tokens, op.lamports, fee_bps);
                largest = largest.max(r.real_sol_reserves - before);
                buys += 1;
            }
        }
        Post { reserves: r, lamports: r.real_sol_reserves.saturating_sub(after_dev.real_sol_reserves), largest, buys }
    }
}

/// The curve after a sell of `tokens` (the fee leaves with the seller's SOL, not the curve's).
pub fn apply_sell(mut r: CurveReserves, tokens: u64) -> CurveReserves {
    let sol = (r.virtual_sol_reserves as u128 * tokens as u128 / (r.virtual_token_reserves as u128 + tokens as u128)) as u64;
    let sol = sol.min(r.real_sol_reserves);
    r.virtual_token_reserves += tokens;
    r.real_token_reserves += tokens;
    r.virtual_sol_reserves -= sol;
    r.real_sol_reserves -= sol;
    r
}

/// The curve after the bundle and what the bundle put in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Post {
    pub reserves: CurveReserves,
    /// SOL the bundle put into the curve (fees out, a mixed transaction's sells taken off).
    pub lamports: u64,
    /// The largest single buy of the bundle.
    pub largest: u64,
    pub buys: u32,
}

// ---- Prices.

/// The curve's spot price, lamports per token unit.
#[inline]
pub fn spot(r: &CurveReserves) -> f64 {
    if r.virtual_token_reserves == 0 { 0.0 } else { r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64 }
}

/// The standard curve's price at launch.
#[inline]
pub fn launch_price() -> f64 {
    INITIAL_VSOL as f64 / INITIAL_VTOK as f64
}

/// Our buy of `lamports_in` (the fee included) on `r`: the tokens out and the curve after it. The
/// fill is `spot` of that curve: (vsol + net in) / (vtok - tokens out), not SOL paid / tokens.
#[inline]
pub fn our_buy(r: CurveReserves, lamports_in: u64, fee_bps: u64) -> (u64, CurveReserves) {
    let after = shred_feed::apply_buy(r, 0, lamports_in, fee_bps);
    (r.virtual_token_reserves - after.virtual_token_reserves, after)
}

/// SOL out for selling `tokens` into `r`: the curve formula, less the fee (rounded up).
#[inline]
pub fn sell_proceeds(r: &CurveReserves, tokens: u64, fee_bps: u64) -> u64 {
    let gross = (r.virtual_sol_reserves as u128 * tokens as u128 / (r.virtual_token_reserves as u128 + tokens as u128)) as u64;
    gross - (gross as u128 * fee_bps as u128).div_ceil(10_000) as u64
}

/// The bundle is over the 15 SOL cap every other path keeps (`shred_feed::MAX_EARLY_BUNDLE_LAMPORTS`).
#[inline]
pub fn blocked_by_15sol_cap(bundle_lamports: u64) -> bool {
    bundle_lamports > shred_feed::MAX_EARLY_BUNDLE_LAMPORTS
}

// ---- The rule.

/// What the rule reads of a launch. The checks that need the bot's state (balance, the day's loss,
/// open positions, whether we can land, the price guard) are the decision's.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    /// The create's arguments in a layout the decoder knows (the curve's creator is certain).
    pub layout_known: bool,
    pub sol_quote: bool,
    /// The curve's virtual reserves at launch (SOL, tokens).
    pub initial: (u64, u64),
    pub mayhem: bool,
    pub cashback: bool,
    pub dev_buy_lamports: u64,
    /// bundle10's count.
    pub count: u32,
    /// The bundle had more buys than the quote keeps.
    pub overflow: bool,
    pub bundle_lamports: u64,
    /// The post-bundle price over the launch price.
    pub post_bundle_mult: f64,
}

/// The first part of the rule a launch misses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Miss {
    Layout,
    NotSol,
    NotStandard,
    Mayhem,
    Cashback,
    NoDevBuy,
    Count,
    TooManyBuys,
    BundleCap,
    Ceiling,
    /// pump.fun's config is not loaded (no curve to quote): decided by the engine.
    Config,
}

impl Miss {
    pub fn code(self) -> u8 {
        match self {
            Miss::NotSol => 60,
            Miss::NotStandard => 61,
            Miss::Mayhem => 62,
            Miss::Cashback => 63,
            Miss::NoDevBuy => 64,
            Miss::Count => 65,
            Miss::TooManyBuys => 66,
            Miss::BundleCap => 67,
            Miss::Ceiling => 68,
            Miss::Layout => 69,
            Miss::Config => 70,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Miss::NotSol => "not_sol",
            Miss::NotStandard => "not_standard",
            Miss::Mayhem => "mayhem",
            Miss::Cashback => "cashback",
            Miss::NoDevBuy => "no_dev_buy",
            Miss::Count => "count",
            Miss::TooManyBuys => "too_many_buys",
            Miss::BundleCap => "bundle_cap",
            Miss::Ceiling => "ceiling",
            Miss::Layout => "layout",
            Miss::Config => "config",
        }
    }

    /// The name of a code from `code` (0: none missed).
    pub fn name_of(code: u8) -> &'static str {
        const ALL: [Miss; 11] =
            [Miss::Layout, Miss::NotSol, Miss::NotStandard, Miss::Mayhem, Miss::Cashback, Miss::NoDevBuy, Miss::Count, Miss::TooManyBuys, Miss::BundleCap, Miss::Ceiling, Miss::Config];
        ALL.iter().find(|m| m.code() == code).map_or("", |m| m.name())
    }
}

/// The spec's entry rule (a-d), its layout check, and the owner's exclusions (mayhem, cashback)
/// and ceiling.
pub fn rule(f: &Facts, s: &Settings) -> Result<(), Miss> {
    if !f.layout_known {
        return Err(Miss::Layout);
    }
    if !f.sol_quote {
        return Err(Miss::NotSol);
    }
    if f.initial != (INITIAL_VSOL, INITIAL_VTOK) {
        return Err(Miss::NotStandard);
    }
    if f.mayhem {
        return Err(Miss::Mayhem);
    }
    if f.cashback {
        return Err(Miss::Cashback);
    }
    if f.dev_buy_lamports == 0 {
        return Err(Miss::NoDevBuy);
    }
    if f.count < MIN_COUNT {
        return Err(Miss::Count);
    }
    if f.overflow {
        return Err(Miss::TooManyBuys);
    }
    if f.bundle_lamports > s.max_bundle_lamports {
        return Err(Miss::BundleCap);
    }
    if !(f.post_bundle_mult <= s.max_post_bundle_mult) {
        return Err(Miss::Ceiling);
    }
    Ok(())
}

// ---- The exit.

/// Why a bundle10 position sells everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sell {
    /// Armed, and the price fell to (1 - trail) x the peak.
    Trail,
    /// The create is BUNDLE10_MAX_HOLD_S old.
    Deadline,
    /// The curve completed: sold on PumpSwap.
    Graduated,
}

/// One bundle10 position's exit: the fill, the running peak, armed, the deadline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exit {
    pub fill: f64,
    pub peak: f64,
    pub armed: bool,
    /// The create's receive time + BUNDLE10_MAX_HOLD_S, unix ms.
    pub deadline_ms: u64,
    trail: f64,
}

impl Exit {
    /// `fill`: the curve's spot price right after our buy (`our_buy`); the peak starts there.
    /// `create_ms`: when the create was received (the deadline counts from it, not the fill).
    pub fn new(fill: f64, create_ms: u64, s: &Settings) -> Self {
        Exit { fill, peak: fill, armed: false, deadline_ms: create_ms + s.max_hold_ms, trail: s.trail }
    }

    /// The spot price after a trade of the coin (our buy in the reserves), at `now_ms`.
    pub fn on_trade(&mut self, price: f64, now_ms: u64) -> Option<Sell> {
        if now_ms >= self.deadline_ms {
            return Some(Sell::Deadline);
        }
        if price > self.peak {
            self.peak = price;
        }
        if price >= ARM_X * self.fill {
            self.armed = true;
        }
        (self.armed && price <= (1.0 - self.trail) * self.peak).then_some(Sell::Trail)
    }

    /// The clock between trades: the deadline.
    pub fn on_clock(&self, now_ms: u64) -> Option<Sell> {
        (now_ms >= self.deadline_ms).then_some(Sell::Deadline)
    }

    /// The curve completed during the hold: everything is sold on PumpSwap (the research valued
    /// the exit at the last curve state before completion).
    pub fn on_graduated(&self) -> Sell {
        Sell::Graduated
    }
}


// ---- The shadow's record.

/// bundle10's view of one create at its window's end, as the engine hands it to the shadow log
/// (`processors::bundle10_shadow`) across a ring. `Copy`, fixed size. Nothing in it is sent.
#[derive(Clone, Copy, Debug)]
pub struct Rec {
    pub mint: Pubkey,
    pub deployer: Pubkey,
    /// The creator the create names.
    pub creator: Pubkey,
    pub slot: u64,
    /// `stamps::Feed` as a number.
    pub feed: u8,
    pub recv_unix_ms: u64,
    /// From the create seen to its window's end, and bundle10's view computed after that.
    pub window_ns: u64,
    pub compute_ns: u64,
    pub mayhem: bool,
    pub token_2022: bool,
    /// pump.fun's initial curve and fee (bps), when its config is loaded.
    pub initial: (u64, u64),
    pub fee_bps: u64,
    pub dev_buy_lamports: u64,
    pub count: u32,
    pub txs: u8,
    /// `Stop::code`.
    pub stop: u8,
    pub unreadable: u32,
    pub trades: u32,
    pub overflow: bool,
    pub mixed: bool,
    pub sigs: [[u8; 8]; MAX_TXS],
    pub bundle_lamports: u64,
    pub largest: u64,
    pub post: CurveReserves,
    /// The post-bundle price over the launch price.
    pub post_mult: f64,
    /// `Miss::code` of the first part of the rule missed, 0 when it passes (cashback is checked
    /// by the shadow log, from the create's event).
    pub miss: u8,
    /// The existing hot path's count (`decide::bundle`: one buy per transaction; None with an
    /// unreadable buyer), the SOL it saw and its buys.
    pub hot_count: Option<u32>,
    pub hot_bundle_lamports: u64,
    pub hot_buys: u32,
    /// The existing decision: its reject code and whether it bought.
    pub existing_reject: u8,
    pub existing_bought: bool,
    /// The landing and selection checks, as they stood at the decision.
    pub fee_route_skip: bool,
    pub far_producer: bool,
    pub distance_go: bool,
    /// The buy's bid as the fee engine's table stood (micro-lamports per CU), its compute limit
    /// and the 0slot lane's tip.
    pub buy_cu_price: u64,
    pub buy_cu_limit: u32,
    pub buy_tip: u64,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            hot::{decide, decode::Launch},
            instructions::pumpfun_buy_ix::{bonding_curve_address, pumpfun_config, set_test_config},
            utils::utils::{TOKEN_2022_PROGRAM_ID, WSOL_MINT},
        },
    };

    struct Coin {
        mint: Pubkey,
        dev: Pubkey,
        creator: Pubkey,
    }

    fn coin() -> Coin {
        Coin { mint: Pubkey::new_unique(), dev: Pubkey::new_unique(), creator: Pubkey::new_unique() }
    }

    fn buy(mint: Pubkey, user: Option<Pubkey>, lamports: u64) -> Trade {
        Trade { mint, buy: true, user, tokens: 0, lamports }
    }

    fn sell(mint: Pubkey, tokens: u64) -> Trade {
        Trade { mint, buy: false, user: Some(Pubkey::new_unique()), tokens, lamports: 0 }
    }

    /// A transaction of `n` buys of `mint` by new wallets, 0.5 SOL each.
    fn wallets_tx(mint: Pubkey, n: usize) -> Vec<Trade> {
        (0..n).map(|_| buy(mint, Some(Pubkey::new_unique()), 500_000_000)).collect()
    }

    fn sig(i: u8) -> [u8; 8] {
        [i; 8]
    }

    fn initial() -> CurveReserves {
        CurveReserves { virtual_sol_reserves: INITIAL_VSOL, virtual_token_reserves: INITIAL_VTOK, real_token_reserves: 793_100_000_000_000, real_sol_reserves: 0 }
    }

    #[test]
    fn four_contiguous_bundle_transactions_count_every_buyer_in_each() {
        let c = coin();
        let mut n = Count::new(c.mint, c.dev, c.creator, Some(5));
        for (k, i) in (6..10).enumerate() {
            assert!(n.tx(Some(i), sig(k as u8), &wallets_tx(c.mint, 3)));
        }
        assert_eq!((n.wallets(), n.txs(), n.stop()), (12, 4, Some(Stop::Four)));
        assert_eq!(n.sigs(), &[sig(0), sig(1), sig(2), sig(3)]);
    }

    #[test]
    fn a_gap_in_the_block_index_stops_the_count() {
        let c = coin();
        let mut n = Count::new(c.mint, c.dev, c.creator, Some(5));
        assert!(n.tx(Some(6), sig(1), &wallets_tx(c.mint, 3)));
        assert!(n.tx(Some(7), sig(2), &wallets_tx(c.mint, 3)));
        // Index 8 has no trade of this coin (another program's, another coin's, or a failed one).
        assert!(!n.tx(Some(9), sig(3), &wallets_tx(c.mint, 3)));
        assert!(!n.tx(Some(10), sig(4), &wallets_tx(c.mint, 3)));
        assert_eq!((n.wallets(), n.txs(), n.stop()), (6, 2, Some(Stop::Gap)));
        // A gap right after the create: nothing counts.
        let mut first = Count::new(c.mint, c.dev, c.creator, Some(5));
        assert!(!first.tx(Some(7), sig(1), &wallets_tx(c.mint, 3)));
        assert_eq!((first.wallets(), first.stop()), (0, Some(Stop::Gap)));
    }

    #[test]
    fn a_transaction_whose_first_trade_of_the_coin_is_a_sell_stops_it_on_either_feed() {
        let c = coin();
        for exact in [true, false] {
            let at = |i: u32| exact.then_some(i);
            let mut n = Count::new(c.mint, c.dev, c.creator, at(5));
            assert!(n.tx(at(6), sig(1), &wallets_tx(c.mint, 4)));
            // A sell of this coin first (buys after it in the same transaction do not save it).
            let mut tx = vec![sell(c.mint, 1_000_000)];
            tx.extend(wallets_tx(c.mint, 2));
            assert!(!n.tx(at(7), sig(2), &tx));
            assert!(!n.tx(at(8), sig(3), &wallets_tx(c.mint, 4)));
            assert_eq!((n.wallets(), n.txs(), n.stop(), n.mixed()), (4, 1, Some(Stop::NotABuy), false), "exact {exact}");
        }
    }

    #[test]
    fn a_mixed_transaction_that_buys_first_counts_whole_and_its_sell_moves_the_curve() {
        let c = coin();
        for exact in [true, false] {
            let at = |i: u32| exact.then_some(i);
            let mut n = Count::new(c.mint, c.dev, c.creator, at(5));
            assert!(n.tx(at(6), sig(1), &wallets_tx(c.mint, 3)));
            // Another coin's trade first, then this coin's buy: the first trade OF THIS COIN decides.
            let mut tx = vec![sell(Pubkey::new_unique(), 5), buy(c.mint, Some(Pubkey::new_unique()), 400_000_000)];
            tx.push(sell(c.mint, 2_000_000_000_000));
            tx.push(buy(c.mint, Some(Pubkey::new_unique()), 300_000_000));
            assert!(n.tx(at(7), sig(2), &tx));
            assert!(n.tx(at(8), sig(3), &wallets_tx(c.mint, 2)));
            assert_eq!((n.wallets(), n.txs(), n.stop(), n.mixed(), n.trades()), (7, 3, None, true, 8), "exact {exact}");
        }
        // The sell is applied in its place: the post-bundle curve is what the trades leave.
        let mut n = Count::new(c.mint, c.dev, c.creator, Some(5));
        let tx = [buy(c.mint, Some(Pubkey::new_unique()), 2_000_000_000), sell(c.mint, 10_000_000_000_000)];
        n.tx(Some(6), sig(1), &tx);
        let post = n.post_bundle(initial(), 125);
        let want = apply_sell(shred_feed::apply_buy(initial(), 0, 2_000_000_000, 125), 10_000_000_000_000);
        assert_eq!(post.reserves, want);
        assert_eq!((post.buys, post.lamports), (1, want.real_sol_reserves));
    }

    #[test]
    fn a_fifth_transaction_is_not_counted() {
        let c = coin();
        for exact in [true, false] {
            let at = |i: u32| exact.then_some(i);
            let mut n = Count::new(c.mint, c.dev, c.creator, at(0));
            for i in 1..=4 {
                assert!(n.tx(at(i), sig(i as u8), &wallets_tx(c.mint, 2)));
            }
            assert!(!n.tx(at(5), sig(5), &wallets_tx(c.mint, 5)));
            assert_eq!((n.wallets(), n.txs(), n.stop()), (8, 4, Some(Stop::Four)), "exact {exact}");
        }
    }

    #[test]
    fn the_dev_and_the_creator_do_not_count_but_their_sol_is_in_the_quote() {
        set_test_config();
        let c = coin();
        let mut n = Count::new(c.mint, c.dev, c.creator, Some(5));
        let mut tx = wallets_tx(c.mint, 2);
        tx.push(buy(c.mint, Some(c.dev), 2_000_000_000));
        tx.push(buy(c.mint, Some(c.creator), 1_000_000_000));
        assert!(n.tx(Some(6), sig(1), &tx));
        assert_eq!(n.wallets(), 2);
        let initial = pumpfun_config().unwrap().initial_reserves;
        let post = n.post_bundle(initial, 125);
        let mut want = initial;
        for b in &tx {
            want = shred_feed::apply_buy(want, b.tokens, b.lamports, 125);
        }
        assert_eq!(post.reserves, want);
        assert_eq!(post.buys, 4);
        assert_eq!(post.lamports, want.real_sol_reserves - initial.real_sol_reserves);
        assert_eq!(post.largest, (2_000_000_000u128 * 10_000 / 10_125) as u64, "the dev's 2 SOL, fee out");
    }

    #[test]
    fn an_unreadable_buyer_does_not_count_and_the_count_goes_on() {
        let c = coin();
        let mut n = Count::new(c.mint, c.dev, c.creator, Some(5));
        let mut tx = wallets_tx(c.mint, 3);
        tx.push(buy(c.mint, None, 1_000_000_000));
        assert!(n.tx(Some(6), sig(1), &tx));
        assert!(n.tx(Some(7), sig(2), &wallets_tx(c.mint, 2)));
        assert_eq!((n.wallets(), n.unreadable(), n.txs()), (5, 1, 2));
        assert_eq!(n.post_bundle(CurveReserves::default(), 125).buys, 6, "its SOL is in the quote");
    }

    #[test]
    fn several_buys_by_one_wallet_count_once_and_several_wallets_in_one_transaction_all_count() {
        let c = coin();
        let mut n = Count::new(c.mint, c.dev, c.creator, Some(5));
        let w = Pubkey::new_unique();
        let tx1 = [buy(c.mint, Some(w), 1), buy(c.mint, Some(w), 1), buy(c.mint, Some(Pubkey::new_unique()), 1)];
        let tx2 = [buy(c.mint, Some(w), 1), buy(c.mint, Some(Pubkey::new_unique()), 1), buy(c.mint, Some(Pubkey::new_unique()), 1)];
        assert!(n.tx(Some(6), sig(1), &tx1));
        assert!(n.tx(Some(7), sig(2), &tx2));
        assert_eq!(n.wallets(), 4, "w once, and the three others");
    }

    #[test]
    fn another_coins_transaction_is_a_gap_on_an_exact_feed_and_passed_over_on_early_feeds() {
        let c = coin();
        let other = wallets_tx(Pubkey::new_unique(), 3);
        let mut exact = Count::new(c.mint, c.dev, c.creator, Some(5));
        assert!(exact.tx(Some(6), sig(1), &wallets_tx(c.mint, 3)));
        assert!(!exact.tx(Some(7), sig(2), &other));
        assert!(!exact.tx(Some(8), sig(3), &wallets_tx(c.mint, 3)));
        assert_eq!((exact.wallets(), exact.stop()), (3, Some(Stop::Gap)));
        // No index: it cannot be placed between the bundle's transactions, so it is passed over.
        let mut early = Count::new(c.mint, c.dev, c.creator, None);
        assert!(early.tx(None, sig(1), &wallets_tx(c.mint, 3)));
        assert!(!early.tx(None, sig(2), &other));
        assert!(early.tx(None, sig(3), &wallets_tx(c.mint, 3)));
        assert_eq!((early.wallets(), early.txs(), early.stop()), (6, 2, None));
        // A transaction buying this coin and another one is a buy of this coin.
        let mut both = wallets_tx(c.mint, 2);
        both.extend(wallets_tx(Pubkey::new_unique(), 2));
        assert!(early.tx(None, sig(4), &both));
        assert_eq!(early.wallets(), 8);
    }

    #[test]
    fn a_frame_seen_twice_counts_once_even_interleaved_with_another() {
        let c = coin();
        let mut n = Count::new(c.mint, c.dev, c.creator, None);
        let tx = wallets_tx(c.mint, 3);
        assert!(n.tx(None, sig(1), &tx));
        assert!(!n.tx(None, sig(1), &tx));
        assert_eq!((n.wallets(), n.txs(), n.trades()), (3, 1, 3));
        // Two feeds deliver transaction 2 trade by trade, interleaved with each other and with
        // transaction 3: each trade counts once, each transaction once.
        let (a, b) = (wallets_tx(c.mint, 3), wallets_tx(c.mint, 2));
        n.event(None, sig(2), 0, &a[0]);
        n.event(None, sig(2), 0, &a[0]);
        n.event(None, sig(3), 0, &b[0]);
        n.event(None, sig(2), 1, &a[1]);
        n.event(None, sig(3), 1, &b[1]);
        n.event(None, sig(2), 1, &a[1]);
        n.event(None, sig(2), 2, &a[2]);
        assert_eq!((n.wallets(), n.txs(), n.trades()), (8, 3, 8));
        // The fourth transaction fills the count; a counted one's late trades still apply.
        assert!(n.tx(None, sig(4), &wallets_tx(c.mint, 1)));
        assert_eq!(n.stop(), Some(Stop::Four));
        assert!(n.event(None, sig(3), 2, &buy(c.mint, Some(Pubkey::new_unique()), 1)));
        assert!(!n.tx(None, sig(5), &wallets_tx(c.mint, 1)));
        assert_eq!((n.wallets(), n.txs()), (10, 4));
    }

    #[test]
    fn more_than_128_trades_are_flagged_for_refusal() {
        let c = coin();
        let mut full = Count::new(c.mint, c.dev, c.creator, Some(0));
        for i in 1..=4 {
            full.tx(Some(i), sig(i as u8), &wallets_tx(c.mint, 32));
        }
        assert!(!full.overflow(), "128 fit");
        assert_eq!((full.wallets(), full.trades()), (128, 128));
        let mut over = Count::new(c.mint, c.dev, c.creator, Some(0));
        for i in 1..=4 {
            over.tx(Some(i), sig(i as u8), &wallets_tx(c.mint, 33));
        }
        assert!(over.overflow());
        assert_eq!((over.wallets(), over.trades()), (132, 132));
    }

    fn passing() -> Facts {
        Facts {
            layout_known: true,
            sol_quote: true,
            initial: (INITIAL_VSOL, INITIAL_VTOK),
            mayhem: false,
            cashback: false,
            dev_buy_lamports: 2_000_000_000,
            count: 10,
            overflow: false,
            bundle_lamports: 20_000_000_000,
            post_bundle_mult: 2.84,
        }
    }

    #[test]
    fn the_rule_takes_ten_or_more_on_a_standard_sol_curve_with_a_dev_buy() {
        let s = Settings::DEFAULT;
        assert_eq!(rule(&passing(), &s), Ok(()));
        let miss = |f: Facts| rule(&f, &s).unwrap_err();
        assert_eq!(miss(Facts { layout_known: false, ..passing() }), Miss::Layout);
        assert_eq!(miss(Facts { sol_quote: false, ..passing() }), Miss::NotSol);
        assert_eq!(miss(Facts { initial: (INITIAL_VSOL, INITIAL_VTOK + 1), ..passing() }), Miss::NotStandard);
        assert_eq!(miss(Facts { mayhem: true, ..passing() }), Miss::Mayhem);
        assert_eq!(miss(Facts { cashback: true, ..passing() }), Miss::Cashback);
        assert_eq!(miss(Facts { dev_buy_lamports: 0, ..passing() }), Miss::NoDevBuy);
        assert_eq!(miss(Facts { count: 9, ..passing() }), Miss::Count);
        assert_eq!(miss(Facts { overflow: true, ..passing() }), Miss::TooManyBuys);
        assert_eq!(rule(&Facts { count: 31, ..passing() }, &s), Ok(()));
        assert_eq!((Miss::name_of(Miss::Ceiling.code()), Miss::name_of(0)), ("ceiling", ""));
    }

    #[test]
    fn bundle10_uses_its_own_100_sol_cap_and_every_other_path_keeps_15() {
        let s = Settings::DEFAULT;
        assert_eq!(shred_feed::MAX_EARLY_BUNDLE_LAMPORTS, 15_000_000_000);
        assert_eq!(rule(&Facts { bundle_lamports: 100_000_000_000, ..passing() }, &s), Ok(()));
        assert_eq!(rule(&Facts { bundle_lamports: 100_000_000_001, ..passing() }, &s), Err(Miss::BundleCap));
        assert!(!blocked_by_15sol_cap(15_000_000_000));
        assert!(blocked_by_15sol_cap(20_000_000_000));
        // A configured cap is bundle10's alone.
        let tight = Settings { max_bundle_lamports: 30_000_000_000, ..Settings::DEFAULT };
        assert_eq!(rule(&Facts { bundle_lamports: 31_000_000_000, ..passing() }, &tight), Err(Miss::BundleCap));
        assert_eq!(shred_feed::MAX_EARLY_BUNDLE_LAMPORTS, 15_000_000_000);

        // The hot path's existing decision still refuses a 16 SOL bundle.
        set_test_config();
        let mint = Pubkey::new_unique();
        let l = Launch {
            mint,
            deployer: Pubkey::new_unique(),
            named_creator: Pubkey::new_unique(),
            holder_reward: false,
            token_program: TOKEN_2022_PROGRAM_ID,
            bonding_curve: Some(bonding_curve_address(&mint)),
            associated_bonding_curve: None,
            mayhem: false,
            creator_set_onchain: false,
            dev_buy_tokens: 0,
            dev_buy_lamports: 2_025_000_000,
            risky_host: Some(false),
            fast_rug_host: false,
            name_all_caps: false,
            fee_payer: Some(WSOL_MINT),
            signature: [0; 64],
        };
        let d = decide::derive(&l, &Pubkey::new_unique());
        let big = [Buy { mint, tokens: 0, lamports: 16_500_000_000, buyer: Some(Pubkey::new_unique()), cu_price: 0 }];
        assert_eq!(decide::decide(&l, &d, 1, &big, 2_000_000_000).reject_code(), decide::Reject::BundleTooBig.code());
    }

    #[test]
    fn the_ceiling_skips_a_post_bundle_price_over_six_times_the_launch() {
        let s = Settings::DEFAULT;
        assert_eq!(rule(&Facts { post_bundle_mult: 6.0, ..passing() }, &s), Ok(()));
        assert_eq!(rule(&Facts { post_bundle_mult: 6.01, ..passing() }, &s), Err(Miss::Ceiling));
        assert_eq!(rule(&Facts { post_bundle_mult: f64::NAN, ..passing() }, &s), Err(Miss::Ceiling));
        // A 20 SOL bundle after a 2 SOL dev buy: about 3x.
        let r = CurveReserves { virtual_sol_reserves: 52_000_000_000, virtual_token_reserves: 30 * INITIAL_VTOK / 52, ..Default::default() };
        let x = spot(&r) / launch_price();
        assert!((x - 3.004).abs() < 0.01, "{x}");
    }

    #[test]
    fn the_fill_is_the_spot_after_our_buy_and_the_sale_is_the_curve_less_the_fee() {
        let r = CurveReserves {
            virtual_sol_reserves: 52_000_000_000,
            virtual_token_reserves: 619_000_000_000_000,
            real_sol_reserves: 22_000_000_000,
            real_token_reserves: 400_000_000_000_000,
        };
        let (tokens, after) = our_buy(r, 100_000_000, 125);
        let net = (100_000_000u128 * 10_000 / 10_125) as u64;
        assert_eq!(after.virtual_sol_reserves, r.virtual_sol_reserves + net);
        assert_eq!(after.virtual_token_reserves, r.virtual_token_reserves - tokens);
        let fill = spot(&after);
        assert_eq!(fill, (r.virtual_sol_reserves + net) as f64 / (r.virtual_token_reserves - tokens) as f64);
        assert!(fill > spot(&r));
        // Sold straight back: the two fees and the rounding.
        let back = sell_proceeds(&after, tokens, 125);
        assert!(back < 100_000_000 && back > 97_400_000, "{back}");
    }

    fn exit(fill: f64) -> Exit {
        Exit::new(fill, 1_000_000, &Settings::DEFAULT)
    }

    #[test]
    fn the_exit_arms_at_1_05_times_the_fill() {
        let mut e = exit(1.0);
        assert_eq!(e.on_trade(1.04, 1_001_000), None);
        assert!(!e.armed);
        // Unarmed there is no stop: a fall to half does not sell.
        assert_eq!(e.on_trade(0.5, 1_002_000), None);
        assert_eq!(e.on_trade(1.05, 1_003_000), None);
        assert!(e.armed);
        assert_eq!(e.peak, 1.05);
    }

    #[test]
    fn armed_it_sells_all_fifteen_percent_under_the_peak() {
        let mut e = exit(1.0);
        assert_eq!(e.on_trade(1.2, 1_001_000), None);
        assert_eq!(e.on_trade(1.5, 1_002_000), None);
        assert_eq!(e.on_trade(1.2751, 1_003_000), None);
        assert_eq!(e.peak, 1.5);
        assert_eq!(e.on_trade(1.2749, 1_004_000), Some(Sell::Trail));
        // From the arming price itself.
        let mut e = exit(1.0);
        e.on_trade(1.05, 1_001_000);
        assert_eq!(e.on_trade(0.8924, 1_002_000), Some(Sell::Trail));
        // The trail is the setting's.
        let mut e = Exit::new(1.0, 1_000_000, &Settings { trail: 0.3, ..Settings::DEFAULT });
        e.on_trade(1.5, 1_001_000);
        assert_eq!(e.on_trade(1.1, 1_002_000), None);
        assert_eq!(e.on_trade(1.049, 1_003_000), Some(Sell::Trail));
    }

    #[test]
    fn the_deadline_is_sixty_seconds_after_the_create_whatever_the_price() {
        // Filled 2 s after the create: the deadline is still the create's + 60 s.
        let mut e = exit(1.0);
        assert_eq!(e.deadline_ms, 1_060_000);
        assert_eq!(e.on_clock(1_059_999), None);
        assert_eq!(e.on_trade(1.01, 1_059_999), None);
        assert_eq!(e.on_clock(1_060_000), Some(Sell::Deadline));
        assert_eq!(e.on_trade(3.0, 1_060_001), Some(Sell::Deadline));
        let long = Exit::new(1.0, 1_000_000, &Settings { max_hold_ms: 90_000, ..Settings::DEFAULT });
        assert_eq!(long.on_clock(1_060_000), None);
    }

    #[test]
    fn a_completed_curve_sells_on_pumpswap() {
        let mut e = exit(1.0);
        e.on_trade(1.3, 1_001_000);
        assert_eq!(e.on_graduated(), Sell::Graduated);
    }

    #[test]
    fn the_count_and_the_exit_allocate_nothing() {
        use super::super::alloc_audit::allocations;
        let c = coin();
        let txs: Vec<Vec<Trade>> = (0..5).map(|_| wallets_tx(c.mint, 4)).collect();
        let r = initial();
        let ((wallets, post), n) = allocations(|| {
            let mut count = Count::new(c.mint, c.dev, c.creator, None);
            for (i, tx) in txs.iter().enumerate() {
                for (k, t) in tx.iter().enumerate() {
                    count.event(None, sig(i as u8), k as u16, t);
                }
            }
            (count.wallets(), count.post_bundle(r, 125))
        });
        assert_eq!((wallets, n), (16, 0));
        let s = Settings::DEFAULT;
        let facts = Facts { count: wallets, bundle_lamports: post.lamports, post_bundle_mult: spot(&post.reserves) / launch_price(), ..passing() };
        let (sold, n) = allocations(|| {
            let ok = rule(&facts, &s).is_ok();
            let (_, after) = our_buy(post.reserves, s.size_lamports, 125);
            let mut e = Exit::new(spot(&after), 0, &s);
            (ok, e.on_trade(spot(&after) * 1.1, 1), e.on_trade(spot(&after) * 0.9, 2))
        });
        assert_eq!((sold, n), ((true, None, Some(Sell::Trail)), 0));
    }
}
