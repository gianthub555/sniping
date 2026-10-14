//! A3: every pump.fun buy's bid, kept per leader client class for the fee engine (`fee`).
//!
//! A buy is recorded when an early feed first shows it (the slow path's copies of the preprocessed
//! frames and of the shred batches): its fee per compute unit, compute limit and tip. When the
//! regular stream shows it executed, the same record gets its block index, its distance from its
//! coin's create, whether it was part of the create's bundle (the run of at most four buys right
//! after the create in its slot, as `positions::create_bundle_end` defines it), and its order among the
//! coin's other buys (`rank`, 1 = the first buyer outside the bundle). A buy first seen on the regular stream is recorded then. Everything here
//! runs off the hot path; the rings are bounded.

use {
    super::{
        clients::{self, Class, CLASSES},
        decode::{self, Tx},
    },
    crate::{
        instructions::{pumpfun_buy_ix::TRADE_EVENT_DISCRIMINATOR, pumpswap_buy_ix::PUMP_PROGRAM_ID},
        processors::common::cpi_event_payloads,
        utils::parse::{read_pubkey, read_u64},
    },
    carbon_core::transaction::TransactionMetadata,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, VecDeque},
        sync::{LazyLock, Mutex},
    },
};

const COMPUTE_BUDGET: Pubkey = Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
/// Bids kept per class (pump.fun runs tens of buys a slot: this holds a few hundred slots).
const RING: usize = 16_384;
/// An early sighting waits this long for its execution.
const PENDING_MS: u64 = 60_000;
const MAX_PENDING: usize = 65_536;
/// Coins whose create the stream showed, for positions and bundles.
const MAX_COINS: usize = 8_192;
/// Where the preprocessed frame's transaction starts (`hot::engine::PP_HEADER`).
const PP_HEADER: usize = 1 + 8 + 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Preprocessed,
    Shreds,
    Regular,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bid {
    pub slot: u64,
    pub sig: [u8; 8],
    pub mint: Pubkey,
    /// Priority fee, micro-lamports per compute unit.
    pub fee_per_cu: u64,
    pub cu_limit: u32,
    pub tip: u64,
    pub tip_lane: &'static str,
    /// Block index, once executed.
    pub index: Option<u64>,
    /// Slots after the coin's create, and block positions after it when in its slot.
    pub slot_delta: Option<u64>,
    pub after_create: Option<u64>,
    pub bundled: bool,
    /// Order among the coin's buys outside the bundle (1 = first), once executed.
    pub rank: Option<u32>,
    /// Unix ms when first seen, and where.
    pub seen_ms: u64,
    pub source: Source,
}

struct Coin {
    slot: u64,
    index: u64,
    /// The bundle: the last position of the run of buys right after the create, while open.
    run_end: u64,
    run_open: bool,
    outside: u32,
}

/// The most buys a create's bundle can hold (a Jito bundle is at most five transactions).
const MAX_BUNDLE_BUYS: u64 = 4;

#[derive(Default)]
struct ClassRing {
    bids: VecDeque<Bid>,
    /// Sequence number of `bids[0]`.
    first_seq: u64,
}

impl ClassRing {
    fn push(&mut self, b: Bid) -> u64 {
        if self.bids.len() == RING {
            self.bids.pop_front();
            self.first_seq += 1;
        }
        self.bids.push_back(b);
        self.first_seq + self.bids.len() as u64 - 1
    }

    fn get_mut(&mut self, seq: u64) -> Option<&mut Bid> {
        let i = seq.checked_sub(self.first_seq)? as usize;
        self.bids.get_mut(i)
    }
}

#[derive(Default)]
struct State {
    rings: [ClassRing; CLASSES],
    pending: HashMap<[u8; 8], (usize, u64)>,
    pending_order: VecDeque<([u8; 8], u64)>,
    coins: HashMap<Pubkey, Coin>,
    coin_order: VecDeque<Pubkey>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The landing service an account is a tip account of.
pub fn lane_of_tip(key: &Pubkey) -> Option<&'static str> {
    static LISTS: LazyLock<Vec<(&'static str, &'static [Pubkey])>> = LazyLock::new(|| {
        (0..super::stamps::LANES)
            .map(|l| (super::lanes::LANE_NAMES[l], super::lanes::tip_accounts(l)))
            .filter(|(_, a)| !a.is_empty())
            .collect()
    });
    LISTS.iter().find(|(_, list)| list.contains(key)).map(|(name, _)| *name)
}

fn class_of(slot: u64) -> Class {
    clients::class_of_slot(slot).or_else(|| clients::lookup(slot)).unwrap_or(Class::Other)
}

/// A regular-stream transaction's bid: (micro-lamports per CU, compute limit, tip lamports, the
/// tipped service or "none"). Legacy and v0: their compute-budget instructions; v1: its config.
pub fn bid_of(tx: &TransactionMetadata) -> (u64, u32, u64, &'static str) {
    let mut keys = tx.message.static_account_keys().to_vec();
    keys.extend_from_slice(&tx.meta.loaded_addresses.writable);
    keys.extend_from_slice(&tx.meta.loaded_addresses.readonly);
    let (mut price, mut limit, mut tip, mut lane) = (0, 200_000, 0, "none");
    for ix in tx.message.instructions() {
        let Some(program) = keys.get(ix.program_id_index as usize) else { continue };
        if *program == COMPUTE_BUDGET {
            match ix.data.first() {
                Some(3) if ix.data.len() >= 9 => price = u64::from_le_bytes(ix.data[1..9].try_into().expect("8")),
                Some(2) if ix.data.len() >= 5 => limit = u32::from_le_bytes(ix.data[1..5].try_into().expect("4")).max(1),
                _ => {}
            }
        } else if *program == Pubkey::default() && ix.data.len() == 12 && ix.data[..4] == [2, 0, 0, 0] {
            let lamports = u64::from_le_bytes(ix.data[4..12].try_into().expect("8"));
            if let Some(name) = ix.accounts.get(1).and_then(|i| keys.get(*i as usize)).and_then(lane_of_tip) {
                if lamports > tip {
                    (tip, lane) = (lamports, name);
                }
            }
        }
    }
    // v1 transactions carry the priority fee (total lamports) and the limit in their config.
    if let solana_message::VersionedMessage::V1(m) = &tx.message {
        limit = m.config.compute_unit_limit.filter(|l| *l > 0).unwrap_or(200_000);
        price = (m.config.priority_fee.unwrap_or(0) as u128 * 1_000_000 / limit as u128) as u64;
    }
    (price, limit, tip, lane)
}

impl State {
    fn expire(&mut self, now: u64) {
        while let Some(&(sig, at)) = self.pending_order.front() {
            if now.saturating_sub(at) < PENDING_MS && self.pending_order.len() <= MAX_PENDING {
                break;
            }
            self.pending_order.pop_front();
            self.pending.remove(&sig);
        }
    }

    fn seen(&mut self, b: Bid) {
        if self.pending.contains_key(&b.sig) {
            return;
        }
        let class = class_of(b.slot) as usize;
        let seq = self.rings[class].push(b);
        self.pending.insert(b.sig, (class, seq));
        self.pending_order.push_back((b.sig, b.seen_ms));
    }
}

/// The buys of one early-feed transaction (`slot`: the slot the feed named).
pub fn note_tx(source: Source, slot: u64, tx: &Tx<'_>) {
    let mut buys: [Option<Pubkey>; 4] = [None; 4];
    let mut n = 0;
    let mut price = 0;
    decode::buys(tx, |b| {
        if n < 4 {
            buys[n] = Some(b.mint);
            n += 1;
        }
        price = b.cu_price;
    });
    if n == 0 {
        return;
    }
    let tip = tx.tip(|k| lane_of_tip(k).is_some());
    let sig: [u8; 8] = tx.signature()[..8].try_into().expect("8");
    let now = now_ms();
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.expire(now);
    for mint in buys.into_iter().flatten().take(1) {
        st.seen(Bid {
            slot,
            sig,
            mint,
            fee_per_cu: price,
            cu_limit: tx.cu_limit(),
            tip: tip.map_or(0, |t| t.0),
            tip_lane: tip.and_then(|t| lane_of_tip(&t.1)).unwrap_or("none"),
            index: None,
            slot_delta: None,
            after_create: None,
            bundled: false,
            rank: None,
            seen_ms: now,
            source,
        });
    }
}

/// A preprocessed frame as the hot path forwards it.
pub fn note_pp_frame(bytes: &[u8]) {
    let Some(slot) = bytes.get(1..9).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes) else { return };
    if let Some(tx) = bytes.get(PP_HEADER..).and_then(Tx::parse) {
        note_tx(Source::Preprocessed, slot, &tx);
    }
}

/// A shred entry batch (its pump.fun transactions) as the hot path forwards it.
pub fn note_entries(slot: u64, payload: &[u8]) {
    let mut ranges = [(0u32, 0u32); 512];
    let Some(n) = decode::entry_transactions(payload, &mut ranges) else { return };
    for &(at, len) in &ranges[..n] {
        if let Some(tx) = payload.get(at as usize..(at + len) as usize).and_then(Tx::parse) {
            note_tx(Source::Shreds, slot, &tx);
        }
    }
}

/// A pump.fun transaction that executed (regular stream).
pub fn note_executed(tx: &TransactionMetadata) {
    let creates = crate::processors::pumpfun::created_mints(tx);
    let trades = cpi_event_payloads(tx, &PUMP_PROGRAM_ID, &TRADE_EVENT_DISCRIMINATOR);
    if creates.is_empty() && trades.is_empty() {
        return;
    }
    let Some(index) = tx.index else { return };
    let now = now_ms();
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.expire(now);
    for (mint, token_program) in &creates {
        super::cu::note_launch(*mint, *token_program);
        if st.coins.len() >= MAX_COINS {
            if let Some(old) = st.coin_order.pop_front() {
                st.coins.remove(&old);
            }
        }
        st.coins.insert(*mint, Coin { slot: tx.slot, index, run_end: index, run_open: true, outside: 0 });
        st.coin_order.push_back(*mint);
    }
    let mut bid = None;
    let sig: [u8; 8] = tx.signature.as_ref()[..8].try_into().expect("8");
    for body in trades {
        let (Ok(mint), Ok(_sol)) = (read_pubkey(&body, 0), read_u64(&body, 32)) else { continue };
        // A create's own dev buy is not a bid on the coin; sells are not bids.
        if body.get(48) != Some(&1) || creates.iter().any(|(m, _)| *m == mint) {
            continue;
        }
        let (price, limit, tip, lane) = *bid.get_or_insert_with(|| bid_of(tx));
        let mut placed = (None, None, false, None);
        if let Some(c) = st.coins.get_mut(&mint) {
            if (tx.slot, index) > (c.slot, c.index) {
                let same = tx.slot == c.slot;
                let bundled = c.run_open && same && index == c.run_end + 1 && c.run_end - c.index < MAX_BUNDLE_BUYS;
                if bundled {
                    c.run_end = index;
                } else if same && index > c.run_end {
                    c.run_open = false;
                }
                if !bundled {
                    c.outside += 1;
                }
                placed = (Some(tx.slot - c.slot), same.then(|| index - c.index), bundled, (!bundled).then_some(c.outside));
            }
        }
        let (slot_delta, after_create, bundled, rank) = placed;
        match st.pending.remove(&sig) {
            Some((class, seq)) => {
                if let Some(b) = st.rings[class].get_mut(seq) {
                    b.index = Some(index);
                    b.slot = tx.slot;
                    (b.slot_delta, b.after_create, b.bundled, b.rank) = (slot_delta, after_create, bundled, rank);
                }
            }
            None => {
                let class = class_of(tx.slot) as usize;
                st.rings[class].push(Bid {
                    slot: tx.slot,
                    sig,
                    mint,
                    fee_per_cu: price,
                    cu_limit: limit,
                    tip,
                    tip_lane: lane,
                    index: Some(index),
                    slot_delta,
                    after_create,
                    bundled,
                    rank,
                    seen_ms: now,
                    source: Source::Regular,
                });
            }
        }
    }
}

/// The `q` quantile of fee per CU over the newest `slots` distinct slots led by `class`, and how
/// many bids it covers. None without bids.
pub fn fee_quantile(class: Class, slots: usize, q: f64) -> Option<(u64, usize)> {
    let st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let ring = &st.rings[class as usize].bids;
    let mut fees = Vec::new();
    let mut seen_slots = 0;
    let mut last = u64::MAX;
    for b in ring.iter().rev() {
        if b.slot != last {
            if seen_slots == slots {
                break;
            }
            seen_slots += 1;
            last = b.slot;
        }
        fees.push(b.fee_per_cu);
    }
    if fees.is_empty() {
        return None;
    }
    fees.sort_unstable();
    let i = ((fees.len() - 1) as f64 * q).round() as usize;
    Some((fees[i.min(fees.len() - 1)], fees.len()))
}

/// Fee per CU of the newest `max` buys at `rank` (1 = the coin's first buyer outside its bundle)
/// in their coin's create slot, under `class`; newest first.
pub fn rank_fees(class: Class, rank: u32, max: usize) -> Vec<u64> {
    let st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.rings[class as usize].bids.iter().rev().filter(|b| b.rank == Some(rank) && b.slot_delta == Some(0)).take(max).map(|b| b.fee_per_cu).collect()
}

/// The bids of one coin, in block order (executed ones).
pub fn of_mint(mint: &Pubkey) -> Vec<Bid> {
    let st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<Bid> = st.rings.iter().flat_map(|r| r.bids.iter()).filter(|b| b.mint == *mint && b.index.is_some()).copied().collect();
    v.sort_by_key(|b| (b.slot, b.index));
    v
}

/// One line for the stats log: per class, bids held and fee per CU over its last 50 slots.
pub fn report() -> String {
    clients::ALL
        .iter()
        .map(|c| match (fee_quantile(*c, 50, 0.5), fee_quantile(*c, 50, 0.9)) {
            (Some((p50, n)), Some((p90, _))) => format!("{} p50 {:.1} p90 {:.1} lamports/CU ({n} bids)", c.name(), p50 as f64 / 1e6, p90 as f64 / 1e6),
            _ => format!("{} -", c.name()),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
pub(crate) fn clear() {
    *STATE.lock().unwrap() = State::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bid(slot: u64, fee: u64) -> Bid {
        Bid {
            slot,
            sig: [slot as u8; 8],
            mint: Pubkey::new_unique(),
            fee_per_cu: fee,
            cu_limit: 100_000,
            tip: 0,
            tip_lane: "none",
            index: None,
            slot_delta: None,
            after_create: None,
            bundled: false,
            rank: None,
            seen_ms: 0,
            source: Source::Shreds,
        }
    }

    #[test]
    fn the_quantile_covers_the_newest_slots_only() {
        let mut ring = ClassRing::default();
        for s in 0..100u64 {
            for k in 0..3 {
                ring.push(bid(s, s * 10 + k));
            }
        }
        let mut st = State::default();
        st.rings[Class::Firedancer as usize] = ring;
        *STATE.lock().unwrap() = st;
        let (p100, n) = fee_quantile(Class::Firedancer, 50, 1.0).unwrap();
        assert_eq!(n, 150, "50 slots of 3 bids");
        assert_eq!(p100, 992);
        let (p0, _) = fee_quantile(Class::Firedancer, 50, 0.0).unwrap();
        assert_eq!(p0, 500, "slot 50 is the oldest of the 50 newest");
        assert_eq!(fee_quantile(Class::Agave, 50, 0.5), None);
        clear();
    }

    #[test]
    fn a_full_ring_drops_the_oldest_and_still_finds_by_sequence() {
        let mut ring = ClassRing::default();
        let first = ring.push(bid(1, 1));
        let mut last = first;
        for s in 0..RING as u64 {
            last = ring.push(bid(2 + s, 2));
        }
        assert!(ring.get_mut(first).is_none(), "dropped");
        assert_eq!(ring.get_mut(last).unwrap().slot, 1 + RING as u64);
        assert_eq!(ring.bids.len(), RING);
    }
}
