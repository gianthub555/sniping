//! Ed25519 with its nonces made ahead of time (2026-10-11; the copy bot's `sign::presign`, ported):
//! the hot path's signer.
//!
//! An Ed25519 signature of message M by key (a, A) is (R, s) with R = r·B and
//! s = r + H(R‖A‖M)·a. Making R (a base-point multiplication) is most of the cost, and it doesn't
//! depend on M. A thread off the hot cores makes (r, R) pairs ahead of time ([`Refill`]); the hot
//! thread finishes each signature with one hash and one multiply-add ([`Presigner::sign`]): about
//! 3 µs for a 1 KB message against 25–46 µs for `Keypair::sign_message` here. Verifiers check only
//! R and s: any r gives a valid signature.
//!
//! The one rule: an r signs one message only (two messages signed with one r give the key away).
//! - The queue hands each pair out once and wipes its slot as it does.
//! - r = H(secret prefix ‖ 32 fresh random bytes ‖ counter): secret and unique even if the
//!   random source were weak (the prefix is RFC 8032's, the counter never repeats in a process).
//!
//! When the queue is empty (or the random source fails) the hot thread signs the RFC 8032 way, as
//! before. The signatures differ from the deterministic ones, so one message signed twice gets
//! two signatures. `HOT_PRESIGN=false` keeps `Keypair::sign_message` (today's path).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use ed25519_dalek::hazmat::ExpandedSecretKey;
use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha512};
use solana_keypair::Keypair;
use zeroize::Zeroize;

/// Pairs kept ready: launches come seconds apart, so this covers well over 1,000 buys between
/// refills (the refill thread looks every millisecond).
pub const CAPACITY: usize = 1024;
const REFILL_EVERY: Duration = Duration::from_millis(1);

/// The key as the hot path needs it: the expanded secret (scalar and nonce prefix) and the public
/// key, computed once. Never printed or logged.
pub struct HotKey {
    expanded: ExpandedSecretKey,
    verifying: VerifyingKey,
    pubkey_bytes: [u8; 32],
}

impl HotKey {
    pub fn of(keypair: &Keypair) -> Self {
        let mut secret: [u8; 32] = *keypair.secret_bytes();
        let expanded = ExpandedSecretKey::from(&secret);
        secret.zeroize();
        let pubkey_bytes = solana_signer::Signer::pubkey(keypair).to_bytes();
        let verifying = VerifyingKey::from_bytes(&pubkey_bytes).expect("a keypair's public key is a curve point");
        HotKey { expanded, verifying, pubkey_bytes }
    }

    pub fn pubkey_bytes(&self) -> [u8; 32] {
        self.pubkey_bytes
    }

    /// The RFC 8032 (deterministic) signature, byte for byte `Keypair::sign_message`'s.
    pub fn sign_rfc(&self, msg: &[u8]) -> [u8; 64] {
        ed25519_dalek::hazmat::raw_sign::<Sha512>(&self.expanded, msg, &self.verifying).to_bytes()
    }
}

#[derive(Clone, Copy, Default)]
struct Pair {
    r: Scalar,
    big_r: [u8; 32],
}

impl Pair {
    fn wipe(&mut self) {
        self.r.zeroize();
        self.big_r.zeroize();
    }
}

#[repr(align(128))]
struct Padded(AtomicUsize);

/// A single-producer, single-consumer queue of pairs (no locks, no allocation after `pair`).
struct Ring {
    slots: Box<[UnsafeCell<Pair>]>,
    mask: usize,
    /// The next slot to take (the hot thread's).
    head: Padded,
    /// The next slot to fill (the refill thread's).
    tail: Padded,
}

// SAFETY: a slot is written only by the producer while it's outside [head, tail), and read and
// wiped only by the consumer while inside; the Release/Acquire pairs on head and tail order the
// slot accesses. `Presigner` and `Refill` are the only handles, one of each, neither `Clone`.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Drop for Ring {
    fn drop(&mut self) {
        for s in self.slots.iter_mut() {
            s.get_mut().wipe();
        }
    }
}

/// The hot thread's end: signs with a ready pair, or the RFC 8032 way when none is left.
pub struct Presigner {
    ring: Arc<Ring>,
    key: Arc<HotKey>,
    /// Signatures made with a ready pair, and without one (the queue was empty).
    pub presigned: u64,
    pub fallbacks: u64,
}

/// The refill thread's end: keeps the queue full.
pub struct Refill {
    ring: Arc<Ring>,
    hash_prefix: [u8; 32],
    counter: u64,
}

impl Drop for Refill {
    fn drop(&mut self) {
        self.hash_prefix.zeroize();
    }
}

/// The two ends for `key`, with an empty queue of `capacity` (rounded up to a power of two).
pub fn pair(key: Arc<HotKey>, capacity: usize) -> (Presigner, Refill) {
    let cap = capacity.max(2).next_power_of_two();
    let ring = Arc::new(Ring {
        slots: (0..cap).map(|_| UnsafeCell::new(Pair::default())).collect(),
        mask: cap - 1,
        head: Padded(AtomicUsize::new(0)),
        tail: Padded(AtomicUsize::new(0)),
    });
    let refill = Refill { ring: ring.clone(), hash_prefix: key.expanded.hash_prefix, counter: 0 };
    (Presigner { ring, key, presigned: 0, fallbacks: 0 }, refill)
}

impl Presigner {
    pub fn pubkey_bytes(&self) -> [u8; 32] {
        self.key.pubkey_bytes
    }

    /// Pairs ready now.
    pub fn ready(&self) -> usize {
        let t = self.ring.tail.0.load(Ordering::Acquire);
        t - self.ring.head.0.load(Ordering::Relaxed)
    }

    /// The signature of `msg`: with a ready pair (each used once), else the RFC 8032 way.
    /// Allocation-free.
    #[inline]
    pub fn sign(&mut self, msg: &[u8]) -> [u8; 64] {
        let ring = &*self.ring;
        let h = ring.head.0.load(Ordering::Relaxed);
        if ring.tail.0.load(Ordering::Acquire) == h {
            self.fallbacks += 1;
            return self.key.sign_rfc(msg);
        }
        // SAFETY: h < tail, so the producer is done with this slot and won't touch it until
        // head moves past it (below, after the wipe).
        let slot = unsafe { &mut *ring.slots[h & ring.mask].get() };
        let mut p = *slot;
        slot.wipe();
        ring.head.0.store(h + 1, Ordering::Release);
        // H(R ‖ A ‖ M): the one hash on the hot thread (ring's SHA-512 is ~40% faster than sha2's).
        let mut d = ring::digest::Context::new(&ring::digest::SHA512);
        d.update(&p.big_r);
        d.update(self.key.verifying.as_bytes());
        d.update(msg);
        let mut wide = [0u8; 64];
        wide.copy_from_slice(d.finish().as_ref());
        let k = Scalar::from_bytes_mod_order_wide(&wide);
        let mut s = k * self.key.expanded.scalar + p.r;
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&p.big_r);
        out[32..].copy_from_slice(s.as_bytes());
        p.wipe();
        s.zeroize();
        self.presigned += 1;
        out
    }
}

impl Refill {
    fn make(&mut self) -> Option<Pair> {
        let mut fresh = [0u8; 32];
        getrandom::getrandom(&mut fresh).ok()?;
        self.counter += 1;
        let mut d = Sha512::new();
        d.update(self.hash_prefix);
        d.update(fresh);
        d.update(self.counter.to_le_bytes());
        fresh.zeroize();
        let mut wide: [u8; 64] = d.finalize().into();
        let r = Scalar::from_bytes_mod_order_wide(&wide);
        wide.zeroize();
        Some(Pair { r, big_r: EdwardsPoint::mul_base(&r).compress().to_bytes() })
    }

    /// Fills the queue; how many pairs it added.
    pub fn fill(&mut self) -> usize {
        let ring = self.ring.clone();
        let cap = ring.mask + 1;
        let mut n = 0;
        loop {
            let t = ring.tail.0.load(Ordering::Relaxed);
            if t - ring.head.0.load(Ordering::Acquire) >= cap {
                return n;
            }
            let Some(mut p) = self.make() else {
                return n;
            };
            // SAFETY: t - head < cap, so the consumer has wiped this slot and moved past it.
            unsafe { *ring.slots[t & ring.mask].get() = p };
            p.wipe();
            ring.tail.0.store(t + 1, Ordering::Release);
            n += 1;
        }
    }

    /// Keeps the queue full until `stop` (the refill thread's body).
    pub fn run(mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            self.fill();
            std::thread::sleep(REFILL_EVERY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn strict_ok(k: &HotKey, msg: &[u8], sig: &[u8; 64]) -> bool {
        k.verifying.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(sig)).is_ok()
    }

    /// Presigned signatures verify (strictly, as validators do, and through the Solana crates), and
    /// each pair is used once: no two signatures share an R. One message signed twice gets two
    /// different valid signatures.
    #[test]
    fn presigned_signatures_verify_and_never_reuse_a_nonce() {
        let keypair = Keypair::new();
        let k = Arc::new(HotKey::of(&keypair));
        let (mut pre, mut refill) = pair(k.clone(), 64);
        let mut rs = HashSet::new();
        for round in 0..20 {
            let before = pre.ready();
            assert_eq!(refill.fill(), 64 - before);
            for len in [0usize, 1, 100, 700, 1100] {
                let msg: Vec<u8> = (0..len).map(|i| (i * 7 + len + round) as u8).collect();
                let sig = pre.sign(&msg);
                assert!(strict_ok(&k, &msg, &sig));
                assert!(solana_signature::Signature::from(sig).verify(&k.pubkey_bytes(), &msg));
                assert!(rs.insert(sig[..32].to_vec()), "an R came out twice");
                assert_ne!(sig, k.sign_rfc(&msg), "made with a pair, not RFC 8032");
            }
        }
        assert_eq!((pre.presigned, pre.fallbacks), (100, 0));
        let a = pre.sign(b"same");
        let b = pre.sign(b"same");
        assert_ne!(a, b);
        assert!(strict_ok(&k, b"same", &a) && strict_ok(&k, b"same", &b));
    }

    /// An empty queue signs the RFC 8032 way: byte for byte the keypair's own signature.
    #[test]
    fn an_empty_queue_falls_back_to_the_keypairs_signature() {
        let keypair = Keypair::new();
        let k = Arc::new(HotKey::of(&keypair));
        let (mut pre, _refill) = pair(k, 8);
        let msg = b"a buy";
        let sig = pre.sign(msg);
        let mut own = [0u8; 64];
        own.copy_from_slice(solana_signer::Signer::sign_message(&keypair, msg).as_ref());
        assert_eq!(sig, own);
        assert_eq!((pre.presigned, pre.fallbacks), (0, 1));
    }

    /// The hot thread's signature allocates nothing.
    #[test]
    fn a_presigned_signature_allocates_nothing() {
        let keypair = Keypair::new();
        let k = Arc::new(HotKey::of(&keypair));
        let (mut pre, mut refill) = pair(k, 8);
        refill.fill();
        let msg = [7u8; 1100];
        let _ = pre.sign(&msg);
        let (_, n) = super::super::alloc_audit::allocations(|| pre.sign(&msg));
        assert_eq!(n, 0);
    }
}
