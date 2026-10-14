//! Recent blockhashes from shreds (A5's second source): the final tick of each slot whose last
//! batch was decoded. The "safe" hash, `SAFE_DEPTH` slots below the newest, goes into the
//! blockhash cell `utils::blockhash::SHRED_HASH`. Ported from pumpfeed (`hub/blockhash.rs`, with
//! its `BLOCKHASH_SAFE_DEPTH=2`).

use {
    crate::utils::blockhash::SHRED_HASH,
    solana_hash::Hash,
    std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicU64, Ordering},
            LazyLock, Mutex,
        },
    },
};

const RING: usize = 512;
pub const SAFE_DEPTH: u64 = 2;

/// (slot, hash, known at ns)
#[derive(Default)]
pub struct Book {
    ring: VecDeque<(u64, [u8; 32], u64)>,
    tip: u64,
}

impl Book {
    fn with_capacity() -> Self {
        Book { ring: VecDeque::with_capacity(RING + 1), tip: 0 }
    }

    pub fn find(&self, slot: u64) -> Option<(u64, [u8; 32], u64)> {
        self.ring.iter().rev().find(|e| e.0 == slot).copied()
    }

    /// True when the slot is new.
    pub fn insert(&mut self, e: (u64, [u8; 32], u64)) -> bool {
        if self.find(e.0).is_some() {
            return false;
        }
        self.tip = self.tip.max(e.0);
        self.ring.push_back(e);
        if self.ring.len() > RING {
            self.ring.pop_front();
        }
        true
    }

    /// The newest hash at least `depth` slots below the tip.
    pub fn safe(&self, depth: u64) -> Option<(u64, [u8; 32], u64)> {
        self.ring.iter().filter(|e| e.0 + depth <= self.tip).max_by_key(|e| e.0).copied()
    }
}

pub static BOOK: LazyLock<Mutex<Book>> = LazyLock::new(|| Mutex::new(Book::with_capacity()));

/// Slot hashes decoded, and how the pumpfeed comparison went (parity runs).
pub mod counts {
    use std::sync::atomic::AtomicU64;
    pub static SLOTS: AtomicU64 = AtomicU64::new(0);
    pub static COMPARED: AtomicU64 = AtomicU64::new(0);
    pub static AGREE: AtomicU64 = AtomicU64::new(0);
    pub static MISMATCH: AtomicU64 = AtomicU64::new(0);
    /// pumpfeed named a slot this book does not hold (yet).
    pub static MISSING: AtomicU64 = AtomicU64::new(0);
}

/// A slot's final entry hash, decoded at `now`: kept, and the safe one published.
pub fn on_slot_hash(slot: u64, hash: [u8; 32], now: u64) {
    let safe = {
        let mut b = BOOK.lock().unwrap_or_else(|e| e.into_inner());
        if !b.insert((slot, hash, now)) {
            return;
        }
        b.safe(SAFE_DEPTH)
    };
    counts::SLOTS.fetch_add(1, Ordering::Relaxed);
    if let Some((s, h, at)) = safe {
        SHRED_HASH.store(&Hash::new_from_array(h), at, s);
    }
}

/// pumpfeed's hash for `slot`, against ours (parity runs).
pub fn compare(slot: u64, theirs: &[u8; 32]) {
    let ours = BOOK.lock().unwrap_or_else(|e| e.into_inner()).find(slot);
    let c = |a: &AtomicU64| a.fetch_add(1, Ordering::Relaxed);
    c(&counts::COMPARED);
    match ours {
        Some((_, h, _)) if h == *theirs => c(&counts::AGREE),
        Some(_) => c(&counts::MISMATCH),
        None => c(&counts::MISSING),
    };
}

pub fn report() -> String {
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let tip = BOOK.lock().unwrap_or_else(|e| e.into_inner()).tip;
    format!(
        "blockhash from shreds: {} slots, tip {tip}; against pumpfeed: {} compared, {} agree, {} differ, {} not in ours",
        g(&counts::SLOTS),
        g(&counts::COMPARED),
        g(&counts::AGREE),
        g(&counts::MISMATCH),
        g(&counts::MISSING)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_safe_hash_is_two_below_the_tip() {
        let mut b = Book::with_capacity();
        for s in [10, 11, 13, 12] {
            assert!(b.insert((s, [s as u8; 32], s)));
        }
        assert!(!b.insert((12, [0; 32], 0)), "the first hash of a slot stays");
        assert_eq!(b.safe(2).map(|e| e.0), Some(11));
        assert_eq!(b.find(12).map(|e| e.1), Some([12; 32]));
        assert!(b.safe(10).is_none());
    }
}
