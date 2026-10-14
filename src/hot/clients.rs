//! Leader client classes. How a block producer orders conflicting buys on one bonding curve
//! depends on its client: Agave and Jito-Agave sort by fee only within short receive bursts,
//! Firedancer and Frankendancer pull the highest fee per compute unit each time the account frees,
//! Harmonic runs short auctions on fee plus tip, BAM's ranking is undocumented. A JitoLabs leader
//! behind a Jito relayer (`leaders::behind_relayer`) is its own class: the relayer holds what
//! reaches its TPU, so only a Jito bundle gets to it in time. The fee engine
//! (`fee`) keeps one table row per class, and the logs name each slot's class.
//!
//! The class of each upcoming slot's producer sits in a lock-free ring (`class_of_slot`) the hot
//! path reads without allocating; a side task fills it from the leader schedule and gossip
//! (`utils::leaders`).

use {
    crate::utils::leaders,
    std::sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Class {
    Agave = 0,
    JitoAgave = 1,
    Firedancer = 2,
    Frankendancer = 3,
    Harmonic = 4,
    Bam = 5,
    Other = 6,
    /// JitoLabs behind a Jito relayer.
    JitoRelayer = 7,
}

pub const CLASSES: usize = 8;
pub const ALL: [Class; CLASSES] = [Class::Agave, Class::JitoAgave, Class::Firedancer, Class::Frankendancer, Class::Harmonic, Class::Bam, Class::Other, Class::JitoRelayer];
pub const NAMES: [&str; CLASSES] = ["Agave", "JitoAgave", "Firedancer", "Frankendancer", "Harmonic", "BAM", "other", "JitoRelayer"];

impl Class {
    pub fn name(self) -> &'static str {
        NAMES[self as usize]
    }

    /// The class a log names (`name`).
    pub fn from_name(name: &str) -> Option<Class> {
        ALL.iter().copied().find(|c| c.name() == name)
    }

    pub fn from_index(i: u8) -> Class {
        ALL.get(i as usize).copied().unwrap_or(Class::Other)
    }

    /// The class of a client as gossip names it (Agave, JitoLabs, AgaveBam, FireBAM, Firedancer,
    /// Frankendancer, HarmonicAgave, HarmonicFiredancer, Rakurai, ...).
    pub fn of_client(client: &str) -> Class {
        let c = client.to_ascii_lowercase();
        if c.starts_with("harmonic") {
            Class::Harmonic
        } else if c.contains("bam") {
            Class::Bam
        } else if c == "frankendancer" {
            Class::Frankendancer
        } else if c.contains("firedancer") {
            Class::Firedancer
        } else if c.starts_with("jito") {
            Class::JitoAgave
        } else if c == "agave" || c.contains("solana") {
            Class::Agave
        } else {
            Class::Other
        }
    }
}

/// Slots the ring covers ahead of the feed (16384 slots: about 1.8 hours).
const RING: usize = 16_384;
/// `slot << 8 | (class + 1)` per slot modulo `RING`; 0: not filled.
static BY_SLOT: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];

/// The class of `slot`'s producer, when the ring has it. Lock-free, no allocation.
#[inline]
pub fn class_of_slot(slot: u64) -> Option<Class> {
    let v = BY_SLOT[(slot as usize) % RING].load(Ordering::Relaxed);
    (v >> 8 == slot && v & 0xff != 0).then(|| Class::from_index((v & 0xff) as u8 - 1))
}

/// Sets `slot`'s class in the ring (the test kit and benchmarks: their fixture slots have no
/// schedule behind them).
pub fn set_slot_class(slot: u64, class: Class) {
    BY_SLOT[(slot as usize) % RING].store(slot << 8 | (class as u64 + 1), Ordering::Relaxed);
}

/// The class of `slot`'s producer from the schedule and gossip (takes locks: off the hot path).
pub fn lookup(slot: u64) -> Option<Class> {
    of_identity(&leaders::leader(slot)?)
}

/// A node's class from gossip: its client, and for JitoLabs whether a relayer fronts its TPU.
pub fn of_identity(id: &solana_pubkey::Pubkey) -> Option<Class> {
    match Class::of_client(&leaders::client(id)?) {
        Class::JitoAgave if leaders::behind_relayer(id) == Some(true) => Some(Class::JitoRelayer),
        c => Some(c),
    }
}

/// Fills the ring for `RING` slots from `from`. How many slots got a class.
pub fn fill(from: u64) -> usize {
    let mut n = 0;
    for slot in from..from + RING as u64 {
        if let Some(c) = lookup(slot) {
            BY_SLOT[(slot as usize) % RING].store(slot << 8 | (c as u64 + 1), Ordering::Relaxed);
            n += 1;
        }
    }
    n
}

/// Keeps the ring filled ahead of the feed (on the runtime): the leader schedule starts if it is
/// not running, then every 30 s the ring is refilled from the newest slot the feeds saw.
pub fn spawn() {
    leaders::ensure_spawned();
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            let mut slot = super::lanes::FEED_SLOT.load(Ordering::Relaxed);
            if slot == 0 {
                slot = crate::config::RPC_CLIENT.get_slot().await.unwrap_or(0);
            }
            if slot > 0 {
                let _ = tokio::task::spawn_blocking(move || fill(slot.saturating_sub(64))).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gossip_client_names_map_to_their_scheduler_class() {
        for (name, class) in [
            ("Agave", Class::Agave),
            ("JitoLabs", Class::JitoAgave),
            ("AgaveBam", Class::Bam),
            ("FireBAM", Class::Bam),
            ("Firedancer", Class::Firedancer),
            ("Frankendancer", Class::Frankendancer),
            ("HarmonicAgave", Class::Harmonic),
            ("HarmonicFiredancer", Class::Harmonic),
            ("Rakurai", Class::Other),
        ] {
            assert_eq!(Class::of_client(name), class, "{name}");
        }
    }

    #[test]
    fn the_slot_ring_answers_only_for_the_slot_it_holds() {
        let slot = 1_000_000_007u64;
        BY_SLOT[(slot as usize) % RING].store(slot << 8 | (Class::Harmonic as u64 + 1), Ordering::Relaxed);
        assert_eq!(class_of_slot(slot), Some(Class::Harmonic));
        assert_eq!(class_of_slot(slot + RING as u64), None, "another slot in the same cell");
    }
}
