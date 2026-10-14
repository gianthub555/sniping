//! Durable nonce accounts for the fan-out (2026-10-12, HOT_NONCE_FANOUT / HOT_SELL_FANOUT): every
//! lane's copy of one buy or sell is signed on the same nonce account and value, each paying only
//! its own lane's tip, so at most one copy lands and only one tip is paid. Written after the copy
//! bot's pool (strategy-b, read only): the same rules, none of its code.
//!
//! **The rule:** an account is used again only once its value is seen to change after our last
//! use, i.e. a copy of ours landed or our cancel (an advance) landed. A durable-nonce transaction
//! never expires, so until that read a copy signed with the old value could still land: the
//! account stays out. The read must be at `confirmed` and taken at or after the slot of the
//! landing that changed it (a lagging read, or one before our cancel lands, must not free it: the
//! copy bot lost every later copy on four accounts that way).
//!
//! States: `Unknown` (not read) → `Free` → `Reserved` (handed to the decision thread's ring, not
//! sent yet) → `InUse` (copies out) → a landing makes it `Advanced` (free once a read at or after
//! the landing's slot shows a new value); no landing within `CANCEL_AFTER_NS` makes it
//! `Cancelling` (an advance out, resent every `CANCEL_RESEND_NS` until it settles). A live start
//! marks every account `Stale`: advanced once before its first use (copies signed before a restart
//! may still be out). A dry run never lands anything: its copies are released at once
//! (`release_unsent`, only for copies marked unsent).
//!
//! Pure state: the caller (`hot::nonces`) reads, sends the advances and reports back.

/// A copy that hasn't landed this long after it went out is cancelled.
pub const CANCEL_AFTER_NS: i64 = 2_000_000_000;
/// An unsettled cancel is asked for again this often (one whose blockhash expired unlanded).
pub const CANCEL_RESEND_NS: i64 = 2_000_000_000;
/// A landing whose new value hasn't shown this long after (forked away): cancel the account.
pub const ADVANCED_TIMEOUT_NS: i64 = 10_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Unknown,
    Stale,
    Free { value: [u8; 32] },
    /// Handed to the decision thread (unused on chain until it reports a copy sent).
    Reserved { value: [u8; 32] },
    /// Copies signed with `value` are out (`sent`), or only simulated (`!sent`, dry run).
    InUse { value: [u8; 32], copy: u64, sent: bool, at_ns: i64 },
    /// An advance is out (asked for at `at_ns`); `landed`: one of the copies landed meanwhile.
    Cancelling { value: [u8; 32], copy: u64, at_ns: i64, landed: bool },
    /// A copy landed in `slot` (seen at `at_ns`).
    Advanced { value: [u8; 32], copy: u64, slot: u64, at_ns: i64 },
}

/// What the pool asks its owner to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Todo {
    /// Send an advance for account `index` (copy 0 and a zero value: the live start's advance).
    Cancel { index: u8, copy: u64, value: [u8; 32] },
    /// Read account `index` (confirmed) from a node at `min_slot` or later.
    Read { index: u8, min_slot: u64 },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub acquired: u64,
    pub exhausted: u64,
    pub landed: u64,
    pub cancels_sent: u64,
    pub cancelled: u64,
    pub released_unsent: u64,
}

#[derive(Debug, Clone)]
pub struct NoncePool {
    slots: Vec<Slot>,
    next: usize,
    pub stats: Stats,
}

impl NoncePool {
    pub fn new(n: usize) -> Self {
        NoncePool { slots: vec![Slot::Unknown; n], next: 0, stats: Stats::default() }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn slot(&self, index: u8) -> Slot {
        self.slots[index as usize]
    }

    /// Live start: every account not in use is advanced once before its first use.
    pub fn advance_first(&mut self) {
        for s in &mut self.slots {
            if matches!(s, Slot::Unknown | Slot::Free { .. }) {
                *s = Slot::Stale;
            }
        }
    }

    fn take_free(&mut self) -> Option<(usize, [u8; 32])> {
        let n = self.slots.len();
        for k in 0..n {
            let i = (self.next + k) % n;
            if let Slot::Free { value } = self.slots[i] {
                self.next = (i + 1) % n;
                return Some((i, value));
            }
        }
        self.stats.exhausted += 1;
        None
    }

    /// A free account for copy `copy` now (the sells): `(index, value)`.
    pub fn acquire(&mut self, copy: u64, sent: bool, now_ns: i64) -> Option<(u8, [u8; 32])> {
        let (i, value) = self.take_free()?;
        self.slots[i] = Slot::InUse { value, copy, sent, at_ns: now_ns };
        self.stats.acquired += 1;
        Some((i as u8, value))
    }

    /// A free account handed ahead of time to the decision thread's ring.
    pub fn reserve(&mut self) -> Option<(u8, [u8; 32])> {
        let (i, value) = self.take_free()?;
        self.slots[i] = Slot::Reserved { value };
        Some((i as u8, value))
    }

    /// A reserved account given back unused.
    pub fn unreserve(&mut self, index: u8) {
        if let Slot::Reserved { value } = self.slots[index as usize] {
            self.slots[index as usize] = Slot::Free { value };
        }
    }

    /// The decision thread signed copy `copy` on reserved account `index` (`sent`: they went out).
    pub fn used(&mut self, index: u8, copy: u64, sent: bool, now_ns: i64) -> bool {
        if let Slot::Reserved { value } = self.slots[index as usize] {
            self.slots[index as usize] = Slot::InUse { value, copy, sent, at_ns: now_ns };
            self.stats.acquired += 1;
            return true;
        }
        false
    }

    /// Copy `copy`'s transactions went out: from now on it is cancelled if it doesn't land.
    pub fn mark_sent(&mut self, index: u8, copy: u64) {
        if let Slot::InUse { value, copy: c, sent: false, at_ns } = self.slots[index as usize] {
            if c == copy {
                self.slots[index as usize] = Slot::InUse { value, copy, sent: true, at_ns };
            }
        }
    }

    /// Dry run: copy `copy` was never sent; the value is still unused on chain. Refused for a copy
    /// that was sent.
    pub fn release_unsent(&mut self, index: u8, copy: u64) -> bool {
        match self.slots[index as usize] {
            Slot::InUse { value, copy: c, sent: false, .. } if c == copy => {
                self.slots[index as usize] = Slot::Free { value };
                self.stats.released_unsent += 1;
                true
            }
            _ => false,
        }
    }

    /// One of copy `copy`'s transactions landed in `slot` (seen at `now_ns`).
    pub fn landed(&mut self, index: u8, copy: u64, slot: u64, now_ns: i64) {
        let s = &mut self.slots[index as usize];
        match *s {
            Slot::InUse { value, copy: c, .. } if c == copy => {
                *s = Slot::Advanced { value, copy, slot, at_ns: now_ns };
                self.stats.landed += 1;
            }
            Slot::Cancelling { value, copy: c, at_ns, landed: false } if c == copy => {
                *s = Slot::Cancelling { value, copy, at_ns, landed: true };
                self.stats.landed += 1;
            }
            _ => {}
        }
    }

    /// A read of account `index` (confirmed) from a node at `read_slot`. It frees an account only
    /// when it can show the advance: an unread account, or a landed copy's account read at or after
    /// the landing's slot with a new value. Any other read is ignored.
    pub fn on_value(&mut self, index: u8, read: [u8; 32], read_slot: u64) {
        let s = &mut self.slots[index as usize];
        match *s {
            Slot::Unknown => *s = Slot::Free { value: read },
            Slot::Advanced { value, slot, .. } if read_slot >= slot && read != value => *s = Slot::Free { value: read },
            _ => {}
        }
    }

    /// The advance for copy `copy` landed and the account read at or after its slot shows `read`.
    /// A new value frees it (true); the same value (the advance failed on chain) leaves it
    /// cancelling, due again at once.
    pub fn cancel_settled(&mut self, index: u8, copy: u64, read: [u8; 32]) -> bool {
        let Slot::Cancelling { value, copy: c, landed, .. } = self.slots[index as usize] else { return false };
        if c != copy {
            return false;
        }
        if read == value {
            self.slots[index as usize] = Slot::Cancelling { value, copy, at_ns: i64::MIN / 2, landed };
            return false;
        }
        self.slots[index as usize] = Slot::Free { value: read };
        if !landed {
            self.stats.cancelled += 1;
        }
        true
    }

    /// The live start's advance for `index` landed and the account now shows `read`.
    pub fn stale_settled(&mut self, index: u8, read: [u8; 32]) {
        if matches!(self.slots[index as usize], Slot::Stale | Slot::Cancelling { copy: 0, .. }) {
            self.slots[index as usize] = Slot::Free { value: read };
        }
    }

    /// The advance for `index` went out at `now_ns` (copy 0: the live start's).
    pub fn cancel_sent(&mut self, index: u8, copy: u64, now_ns: i64) {
        let s = &mut self.slots[index as usize];
        match *s {
            Slot::InUse { value, copy: c, .. } | Slot::Advanced { value, copy: c, .. } if c == copy => {
                *s = Slot::Cancelling { value, copy, at_ns: now_ns, landed: matches!(*s, Slot::Advanced { .. }) };
                self.stats.cancels_sent += 1;
            }
            Slot::Cancelling { value, copy: c, landed, .. } if c == copy => {
                *s = Slot::Cancelling { value, copy, at_ns: now_ns, landed };
                self.stats.cancels_sent += 1;
            }
            Slot::Stale if copy == 0 => {
                *s = Slot::Cancelling { value: [0; 32], copy: 0, at_ns: now_ns, landed: false };
                self.stats.cancels_sent += 1;
            }
            _ => {}
        }
    }

    /// A sell's copy on `index` signed again with a higher fee at `now_ns` (same value): its
    /// cancel timer starts again (only while it is still in use).
    pub fn resent(&mut self, index: u8, copy: u64, now_ns: i64) {
        if let Slot::InUse { value, copy: c, sent: true, .. } = self.slots[index as usize] {
            if c == copy {
                self.slots[index as usize] = Slot::InUse { value, copy, sent: true, at_ns: now_ns };
            }
        }
    }

    /// Cancels and reads due at `now_ns`.
    pub fn due(&self, now_ns: i64, out: &mut Vec<Todo>) {
        for (i, s) in self.slots.iter().enumerate() {
            let index = i as u8;
            match *s {
                Slot::Unknown => out.push(Todo::Read { index, min_slot: 0 }),
                Slot::Stale => out.push(Todo::Cancel { index, copy: 0, value: [0; 32] }),
                Slot::Advanced { value, copy, slot, at_ns } => {
                    out.push(Todo::Read { index, min_slot: slot });
                    if now_ns - at_ns >= ADVANCED_TIMEOUT_NS {
                        out.push(Todo::Cancel { index, copy, value });
                    }
                }
                Slot::InUse { value, copy, sent: true, at_ns } if now_ns - at_ns >= CANCEL_AFTER_NS => out.push(Todo::Cancel { index, copy, value }),
                Slot::Cancelling { value, copy, at_ns, .. } if now_ns - at_ns >= CANCEL_RESEND_NS => out.push(Todo::Cancel { index, copy, value }),
                _ => {}
            }
        }
    }

    /// Accounts free or reserved now (ready for a copy).
    pub fn ready(&self) -> usize {
        self.slots.iter().filter(|s| matches!(s, Slot::Free { .. } | Slot::Reserved { .. })).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(b: u8) -> [u8; 32] {
        [b; 32]
    }

    fn free_pool(n: usize) -> NoncePool {
        let mut p = NoncePool::new(n);
        for i in 0..n {
            p.on_value(i as u8, v(i as u8 + 1), 0);
        }
        p
    }

    /// An account is used again only after a read at or after its landing's slot shows a new
    /// value; a lagging read or the old value keeps it out.
    #[test]
    fn a_landed_account_is_freed_only_by_a_later_read_of_a_new_value() {
        let mut p = free_pool(1);
        let (i, value) = p.acquire(1, true, 0).unwrap();
        assert!(p.acquire(2, true, 0).is_none(), "in use");
        p.landed(i, 1, 500, 10);
        p.on_value(i, v(9), 499);
        assert!(matches!(p.slot(i), Slot::Advanced { .. }), "a read before the landing's slot");
        p.on_value(i, value, 600);
        assert!(matches!(p.slot(i), Slot::Advanced { .. }), "the old value");
        p.on_value(i, v(9), 500);
        assert_eq!(p.slot(i), Slot::Free { value: v(9) });
    }

    /// The copy bot's bug: a copy landed unseen, the cancel went out, and a read showing the landed
    /// copy's value freed the account before the cancel landed; the cancel then used that value up.
    /// Here a cancelling account ignores every read: only the cancel's own settled read frees it.
    #[test]
    fn a_cancelling_account_ignores_reads_until_its_cancel_settles() {
        let mut p = free_pool(1);
        let (i, value) = p.acquire(1, true, 0).unwrap();
        let mut todo = Vec::new();
        p.due(CANCEL_AFTER_NS, &mut todo);
        assert_eq!(todo, vec![Todo::Cancel { index: i, copy: 1, value }]);
        p.cancel_sent(i, 1, CANCEL_AFTER_NS);
        p.on_value(i, v(7), 10_000);
        assert!(matches!(p.slot(i), Slot::Cancelling { .. }), "a read does not free it");
        assert!(p.acquire(2, true, 0).is_none());
        assert!(!p.cancel_settled(i, 1, value), "the advance failed: still cancelling, due again");
        let mut todo = Vec::new();
        p.due(CANCEL_AFTER_NS + 1, &mut todo);
        assert!(matches!(todo[0], Todo::Cancel { .. }));
        assert!(p.cancel_settled(i, 1, v(8)));
        assert_eq!(p.slot(i), Slot::Free { value: v(8) });
        assert_eq!(p.stats.cancelled, 1);
    }

    /// A live start advances every account once before its first use.
    #[test]
    fn a_live_start_advances_each_account_before_use() {
        let mut p = free_pool(2);
        p.advance_first();
        assert!(p.acquire(1, true, 0).is_none() && p.reserve().is_none());
        let mut todo = Vec::new();
        p.due(0, &mut todo);
        assert_eq!(todo.len(), 2);
        p.cancel_sent(0, 0, 0);
        p.stale_settled(0, v(5));
        assert_eq!(p.acquire(1, true, 0), Some((0, v(5))));
    }

    /// The decision thread's grants: reserved accounts are out of the pool until used; used ones
    /// follow the copy rules; a dry run's unsent copies come straight back, a sent one never does.
    #[test]
    fn reserved_grants_and_dry_run_copies() {
        let mut p = free_pool(2);
        let (a, va) = p.reserve().unwrap();
        let (b, _) = p.reserve().unwrap();
        assert!(p.reserve().is_none() && p.acquire(9, true, 0).is_none());
        assert!(p.used(a, 1, false, 0));
        assert!(!p.used(a, 2, false, 0), "used once");
        assert!(p.release_unsent(a, 1));
        assert_eq!(p.slot(a), Slot::Free { value: va });
        assert!(p.used(b, 3, true, 0));
        assert!(!p.release_unsent(b, 3), "a sent copy is never released unchanged");
    }

    /// A sell re-bid restarts the cancel timer; a landing whose value never shows is cancelled.
    #[test]
    fn rebids_restart_the_timer_and_a_lost_landing_is_cancelled() {
        let mut p = free_pool(1);
        let (i, _) = p.acquire(1, true, 0).unwrap();
        p.resent(i, 1, CANCEL_AFTER_NS - 1);
        let mut todo = Vec::new();
        p.due(CANCEL_AFTER_NS, &mut todo);
        assert!(todo.is_empty(), "re-bid at {} restarted the timer", CANCEL_AFTER_NS - 1);
        p.landed(i, 1, 100, 0);
        let mut todo = Vec::new();
        p.due(ADVANCED_TIMEOUT_NS, &mut todo);
        assert!(todo.iter().any(|t| matches!(t, Todo::Cancel { .. })));
        assert!(todo.iter().any(|t| matches!(t, Todo::Read { min_slot: 100, .. })));
    }
}
