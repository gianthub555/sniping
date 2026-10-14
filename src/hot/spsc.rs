//! A bounded single-producer single-consumer ring of `Copy` values: one feed thread pushes, the
//! decision thread pops. Allocated once; push and pop never allocate, lock or make a syscall.

use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct Ring<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    /// Next slot the producer writes (only the producer stores it).
    head: CachePadded<AtomicUsize>,
    /// Next slot the consumer reads (only the consumer stores it).
    tail: CachePadded<AtomicUsize>,
}

/// Keeps the two indices on separate cache lines, so the threads do not fight over one line.
#[repr(align(64))]
struct CachePadded<T>(T);

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

// SAFETY: a slot is written only by the one producer before `head` publishes it (Release) and read
// only by the one consumer after it sees that `head` (Acquire); `Producer` and `Consumer` are not
// `Clone`, so there is exactly one of each.
unsafe impl<T: Send> Sync for Ring<T> {}

pub struct Producer<T> {
    ring: Arc<Ring<T>>,
}

pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
}

unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Send for Consumer<T> {}

/// A ring holding up to `capacity` values (rounded up to a power of two).
pub fn channel<T: Copy>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let size = capacity.max(2).next_power_of_two();
    let slots = (0..size).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect::<Vec<_>>().into_boxed_slice();
    let ring = Arc::new(Ring { slots, mask: size - 1, head: CachePadded(AtomicUsize::new(0)), tail: CachePadded(AtomicUsize::new(0)) });
    (Producer { ring: ring.clone() }, Consumer { ring })
}

impl<T: Copy> Producer<T> {
    /// Pushes `value`; false (and the value is dropped) when the ring is full.
    #[inline]
    pub fn push(&mut self, value: T) -> bool {
        let r = &*self.ring;
        let head = r.head.load(Ordering::Relaxed);
        if head.wrapping_sub(r.tail.load(Ordering::Acquire)) > r.mask {
            return false;
        }
        // SAFETY: the slot at `head` is not visible to the consumer until `head` moves past it.
        unsafe { (*r.slots[head & r.mask].get()).write(value) };
        r.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }
}

impl<T: Copy> Consumer<T> {
    /// The oldest value, if any.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        let r = &*self.ring;
        let tail = r.tail.load(Ordering::Relaxed);
        if tail == r.head.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the producer published this slot (Acquire above) and will not reuse it until
        // `tail` moves past it.
        let value = unsafe { (*r.slots[tail & r.mask].get()).assume_init_read() };
        r.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_come_out_in_order_and_a_full_ring_refuses() {
        let (mut tx, mut rx) = channel::<u64>(4);
        for i in 0..4 {
            assert!(tx.push(i));
        }
        assert!(!tx.push(99), "full");
        assert_eq!(rx.pop(), Some(0));
        assert!(tx.push(4));
        assert_eq!((1..=4).map(|_| rx.pop().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(rx.pop(), None);
    }

    #[test]
    fn a_million_values_cross_threads_intact() {
        let (mut tx, mut rx) = channel::<u64>(1024);
        let producer = std::thread::spawn(move || {
            for i in 0..1_000_000u64 {
                while !tx.push(i) {
                    std::hint::spin_loop();
                }
            }
        });
        let mut next = 0;
        while next < 1_000_000 {
            if let Some(v) = rx.pop() {
                assert_eq!(v, next);
                next += 1;
            }
        }
        producer.join().unwrap();
    }
}
