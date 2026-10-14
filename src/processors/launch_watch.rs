










use {
    super::common::cpi_event_payloads,
    crate::{
        instructions::{
            pumpfun_buy_ix::{CurveReserves, TRADE_EVENT_DISCRIMINATOR},
            pumpswap_buy_ix::PUMP_PROGRAM_ID,
        },
        utils::parse::{read_pubkey, read_u64},
    },
    carbon_core::transaction::TransactionMetadata,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicUsize, Ordering},
            LazyLock, Mutex,
        },
        time::{Duration, Instant},
    },
};


const WATCH_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct StreamView {
    
    pub launch: CurveReserves,
    
    pub latest: CurveReserves,
    pub other_buys: u32,
    pub other_buy_lamports: u64,
    
    pub other_buyers: Vec<(Pubkey, u64)>,
    pub sells: u32,
    
    pub creator: Option<Pubkey>,
    started: Instant,
    seen: HashSet<Signature>,
    launch_signature: Signature,
    /// Where the launch transaction landed (slot, block position) and the curve right after it,
    /// once the stream has shown it.
    pub launch_index: Option<(u64, u64)>,
    launch_after: Option<CurveReserves>,
    /// The wallet whose buy is in the create transaction (the dev), once the stream showed it.
    pub dev: Option<Pubkey>,
    /// The creator's move to a fee-sharing account: where it landed (slot, block position) and
    /// the new creator. A buy landing before that position still names the old creator.
    pub creator_migration: Option<(u64, u64, Pubkey)>,
    /// Every trade the stream showed, with its block position, for pricing a fill by position.
    pub trades: Vec<StreamTrade>,
}

#[derive(Debug, Clone, Copy)]
pub struct StreamTrade {
    pub slot: u64,
    pub index: u64,
    pub is_buy: bool,
    pub sol: u64,
    pub own: bool,
    pub user: Pubkey,
    /// The curve after this trade.
    pub after: CurveReserves,
}

impl StreamView {
    /// The curve as a buy landing at (`slot`, `index`) meets it: after every trade that landed
    /// before that position, and how many of them were other wallets' buys (count, lamports).
    pub fn state_before(&self, slot: u64, index: u64) -> (CurveReserves, u32, u64) {
        let mut ahead: Vec<&StreamTrade> = self.trades.iter().filter(|t| (t.slot, t.index) < (slot, index)).collect();
        ahead.sort_by_key(|t| (t.slot, t.index));
        let reserves = ahead.last().map(|t| t.after).or(self.launch_after).unwrap_or(self.launch);
        let buys: Vec<&&StreamTrade> = ahead.iter().filter(|t| t.is_buy && !t.own).collect();
        (reserves, buys.len() as u32, buys.iter().map(|t| t.sol).sum())
    }

    
    /// The dev and the wallets of the create's bundle: the buys at the block positions right
    /// after the create's, in its slot (as the replay defines them). Empty until the stream has
    /// shown the create.
    pub fn insiders(&self) -> Vec<Pubkey> {
        let Some((slot, create)) = self.launch_index else {
            return Vec::new();
        };
        let mut out: Vec<Pubkey> = self.dev.into_iter().collect();
        let mut next = create + 1;
        loop {
            let Some(t) = self.trades.iter().find(|t| t.slot == slot && t.index == next && t.is_buy && !t.own) else { break };
            out.push(t.user);
            next += 1;
        }
        out
    }

    pub fn price_move_percent(&self) -> f64 {
        let price = |r: &CurveReserves| {
            if r.virtual_token_reserves == 0 { 0.0 } else { r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64 }
        };
        let launch = price(&self.launch);
        if launch == 0.0 { 0.0 } else { (price(&self.latest) / launch - 1.0) * 100.0 }
    }
}

static WATCHED: LazyLock<Mutex<HashMap<Pubkey, StreamView>>> = LazyLock::new(Default::default);

static WATCH_COUNT: AtomicUsize = AtomicUsize::new(0);


pub fn watch(mint: Pubkey, launch: CurveReserves, launch_signature: Signature) {
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    watched.retain(|_, v| v.started.elapsed() < WATCH_TTL);
    watched.insert(
        mint,
        StreamView {
            launch,
            latest: launch,
            other_buys: 0,
            other_buy_lamports: 0,
            other_buyers: Vec::new(),
            sells: 0,
            creator: None,
            started: Instant::now(),
            seen: HashSet::from([launch_signature]),
            launch_signature,
            launch_index: None,
            launch_after: None,
            dev: None,
            creator_migration: None,
            trades: Vec::new(),
        },
    );
    WATCH_COUNT.store(watched.len(), Ordering::Relaxed);
}

pub fn unwatch(mint: &Pubkey) {
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    watched.remove(mint);
    WATCH_COUNT.store(watched.len(), Ordering::Relaxed);
}


pub struct WatchGuard(pub Pubkey);

impl Drop for WatchGuard {
    fn drop(&mut self) {
        unwatch(&self.0);
    }
}

pub fn view(mint: &Pubkey) -> Option<StreamView> {
    WATCHED.lock().unwrap_or_else(|e| e.into_inner()).get(mint).cloned()
}

pub fn is_watching() -> bool {
    WATCH_COUNT.load(Ordering::Relaxed) > 0
}



pub fn observe_trades(tx: &TransactionMetadata, own_wallet: &Pubkey) {
    if !is_watching() {
        return;
    }
    let payloads = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    if payloads.is_empty() {
        return;
    }
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    let mut counted: HashSet<Pubkey> = HashSet::new();
    for body in payloads {
        let Ok(mint) = read_pubkey(&body, 0) else { continue };
        let Some(view) = watched.get_mut(&mint) else { continue };
        if tx.signature == view.launch_signature && view.launch_index.is_none() {
            if let (Ok(vq), Ok(vt), Ok(rq), Ok(rt)) = (read_u64(&body, 89), read_u64(&body, 97), read_u64(&body, 105), read_u64(&body, 113)) {
                view.launch_index = Some((tx.slot, tx.index.unwrap_or(0)));
                view.launch_after = Some(CurveReserves { virtual_token_reserves: vt, virtual_sol_reserves: vq, real_token_reserves: rt, real_sol_reserves: rq });
                view.dev = read_pubkey(&body, 49).ok();
            }
        }
        if view.seen.contains(&tx.signature) && !counted.contains(&mint) {
            continue;
        }
        let (Ok(sol), Ok(vq), Ok(vt), Ok(rq), Ok(rt), Ok(user), Ok(creator)) = (
            read_u64(&body, 32),
            read_u64(&body, 89),
            read_u64(&body, 97),
            read_u64(&body, 105),
            read_u64(&body, 113),
            read_pubkey(&body, 49),
            read_pubkey(&body, 169),
        ) else {
            continue;
        };
        let is_buy = body.get(48) == Some(&1);
        view.seen.insert(tx.signature);
        counted.insert(mint);
        view.latest = CurveReserves {
            virtual_token_reserves: vt,
            virtual_sol_reserves: vq,
            real_token_reserves: rt,
            real_sol_reserves: rq,
        };
        view.creator = Some(creator);
        view.trades.push(StreamTrade { slot: tx.slot, index: tx.index.unwrap_or(u64::MAX), is_buy, sol, own: user == *own_wallet, user, after: view.latest });
        if user == *own_wallet {
            continue;
        }
        if is_buy {
            view.other_buys += 1;
            view.other_buy_lamports += sol;
            view.other_buyers.push((user, sol));
        } else {
            view.sells += 1;
        }
    }
}


pub fn observe_creator_migration(mint: &Pubkey, sharing_config: &Pubkey, slot: u64, index: Option<u64>) {
    if !is_watching() {
        return;
    }
    if let Some(view) = WATCHED.lock().unwrap_or_else(|e| e.into_inner()).get_mut(mint) {
        view.creator = Some(*sharing_config);
        view.creator_migration.get_or_insert((slot, index.unwrap_or(u64::MAX), *sharing_config));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fill_by_position_sees_only_the_trades_that_landed_before_it() {
        let r = |sol: u64| CurveReserves { virtual_sol_reserves: sol, virtual_token_reserves: 1_000, ..Default::default() };
        let mut view = StreamView {
            launch: r(30), latest: r(36), other_buys: 0, other_buy_lamports: 0, other_buyers: Vec::new(), sells: 0, creator: None,
            started: Instant::now(), seen: HashSet::new(), launch_signature: Signature::default(), launch_index: Some((100, 10)),
            launch_after: Some(r(31)), dev: None, creator_migration: None, trades: Vec::new(),
        };
        let t = |slot, index, sol, after| StreamTrade { slot, index, is_buy: true, sol, own: false, user: Pubkey::default(), after: r(after) };
        // Recorded out of order: a next-slot buy first, then two launch-slot buys.
        view.trades = vec![t(101, 5, 3, 36), t(100, 11, 1, 32), t(100, 40, 1, 33)];
        assert_eq!(view.state_before(100, 20), (r(32), 1, 1), "only the buy at position 11 is ahead");
        assert_eq!(view.state_before(100, 11), (r(31), 0, 0), "nothing ahead: the launch's own state");
        assert_eq!(view.state_before(101, 6), (r(36), 3, 5), "everything in the launch slot and the next-slot buy");
    }

    #[test]
    fn the_insiders_are_the_dev_and_the_buys_right_behind_the_create() {
        let r = CurveReserves::default();
        let (dev, b1, b2, sniper, late) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let mut view = StreamView {
            launch: r, latest: r, other_buys: 0, other_buy_lamports: 0, other_buyers: Vec::new(), sells: 0, creator: None,
            started: Instant::now(), seen: HashSet::new(), launch_signature: Signature::default(), launch_index: None,
            launch_after: None, dev: None, creator_migration: None, trades: Vec::new(),
        };
        let t = |slot, index, user| StreamTrade { slot, index, is_buy: true, sol: 1, own: false, user, after: r };
        // Recorded out of order; a gap at 13 ends the bundle, the next slot's buys are not in it.
        view.trades = vec![t(100, 12, b2), t(100, 11, b1), t(100, 14, sniper), t(101, 11, late)];
        assert!(view.insiders().is_empty(), "the create not seen yet");
        view.launch_index = Some((100, 10));
        view.dev = Some(dev);
        assert_eq!(view.insiders(), vec![dev, b1, b2]);
    }

    fn reserves(sol: u64, tokens: u64) -> CurveReserves {
        CurveReserves { virtual_token_reserves: tokens, virtual_sol_reserves: sol, real_token_reserves: 0, real_sol_reserves: 0 }
    }

    #[test]
    fn price_move_compares_latest_with_launch() {
        let mut view = StreamView {
            launch: reserves(30_000_000_000, 1_000_000_000),
            latest: reserves(30_000_000_000, 1_000_000_000),
            other_buys: 0,
            other_buy_lamports: 0,
            other_buyers: Vec::new(),
            sells: 0,
            creator: None,
            started: Instant::now(),
            seen: HashSet::new(),
            launch_signature: Signature::default(),
            launch_index: None,
            launch_after: None,
            dev: None,
            creator_migration: None,
            trades: Vec::new(),
        };
        assert_eq!(view.price_move_percent(), 0.0);
        view.latest = reserves(33_000_000_000, 909_090_909);
        assert!((view.price_move_percent() - 21.0).abs() < 0.1);
    }
}
