//! One producer, one consumer, one buffer that is allocated once.
//!
//! The producer is an audio callback: it may not allocate, lock or make a
//! system call, so it writes into a fixed slab and moves an atomic. The
//! consumer is the dictation worker. When the worker falls behind far enough
//! to fill the ring the producer drops what it cannot fit and counts it,
//! rather than overwriting samples the worker is still reading.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

struct Inner {
    buf: Vec<UnsafeCell<i16>>,
    mask: usize,
    /// Samples written, ever.
    head: AtomicUsize,
    /// Samples read, ever.
    tail: AtomicUsize,
    dropped: AtomicU64,
}

use std::cell::UnsafeCell;

// Safety: `head` is only ever written by the producer and `tail` only by the
// consumer. A slot is written before `head` is released and read only after
// `head` is acquired, so no slot is touched by both halves at once.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

pub struct Producer(Arc<Inner>);
pub struct Consumer(Arc<Inner>);

/// A ring holding at least `samples`, rounded up to a power of two.
pub fn ring(samples: usize) -> (Producer, Consumer) {
    let cap = samples.max(2).next_power_of_two();
    let mut buf = Vec::with_capacity(cap);
    buf.resize_with(cap, || UnsafeCell::new(0));
    let inner = Arc::new(Inner {
        buf,
        mask: cap - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        dropped: AtomicU64::new(0),
    });
    (Producer(inner.clone()), Consumer(inner))
}

impl Producer {
    /// Writes what fits and counts the rest as dropped.
    pub fn write(&self, pcm: &[i16]) {
        let inner = &*self.0;
        let cap = inner.mask + 1;
        let head = inner.head.load(Ordering::Relaxed);
        let tail = inner.tail.load(Ordering::Acquire);
        let free = cap - (head - tail);
        let n = pcm.len().min(free);
        if n < pcm.len() {
            inner
                .dropped
                .fetch_add((pcm.len() - n) as u64, Ordering::Relaxed);
        }
        for (i, s) in pcm[..n].iter().enumerate() {
            // Safety: this slot is between `head` and `tail + cap`, so the
            // consumer cannot be reading it.
            unsafe { *inner.buf[(head + i) & inner.mask].get() = *s };
        }
        inner.head.store(head + n, Ordering::Release);
    }
}

impl Consumer {
    pub fn len(&self) -> usize {
        let inner = &*self.0;
        inner.head.load(Ordering::Acquire) - inner.tail.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Move everything available onto the end of `out`.
    pub fn drain(&self, out: &mut Vec<i16>) -> usize {
        let inner = &*self.0;
        let tail = inner.tail.load(Ordering::Relaxed);
        let head = inner.head.load(Ordering::Acquire);
        let n = head - tail;
        out.reserve(n);
        for i in 0..n {
            // Safety: below `head`, so the producer has finished with it.
            out.push(unsafe { *inner.buf[(tail + i) & inner.mask].get() });
        }
        inner.tail.store(head, Ordering::Release);
        n
    }

    /// Throw away everything but the last `keep` samples. This is what the
    /// worker does while the microphone is open but nobody is talking: the
    /// ring holds the run-up to the next phrase and nothing older.
    pub fn keep_last(&self, keep: usize) {
        let inner = &*self.0;
        let tail = inner.tail.load(Ordering::Relaxed);
        let head = inner.head.load(Ordering::Acquire);
        let want = head.saturating_sub(keep);
        if want > tail {
            inner.tail.store(want, Ordering::Release);
        }
    }

    pub fn dropped(&self) -> u64 {
        self.0.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_come_back_in_order() {
        let (p, c) = ring(8);
        p.write(&[1, 2, 3]);
        p.write(&[4, 5]);
        let mut out = Vec::new();
        assert_eq!(c.drain(&mut out), 5);
        assert_eq!(out, vec![1, 2, 3, 4, 5]);
        assert!(c.is_empty());
    }

    #[test]
    fn wraps_around_the_end() {
        let (p, c) = ring(4);
        let mut out = Vec::new();
        for round in 0..10i16 {
            p.write(&[round * 2, round * 2 + 1]);
            c.drain(&mut out);
        }
        assert_eq!(out.len(), 20);
        assert_eq!(out[19], 19);
    }

    #[test]
    fn a_full_ring_drops_the_newest_and_counts_it() {
        let (p, c) = ring(4);
        p.write(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(c.dropped(), 2);
        let mut out = Vec::new();
        c.drain(&mut out);
        assert_eq!(out, vec![1, 2, 3, 4]);
    }

    #[test]
    fn keeping_the_last_samples_discards_the_rest() {
        let (p, c) = ring(16);
        p.write(&[1, 2, 3, 4, 5, 6, 7, 8]);
        c.keep_last(3);
        let mut out = Vec::new();
        c.drain(&mut out);
        assert_eq!(out, vec![6, 7, 8]);
    }

    #[test]
    fn keeping_more_than_there_is_keeps_everything() {
        let (p, c) = ring(16);
        p.write(&[1, 2, 3]);
        c.keep_last(99);
        let mut out = Vec::new();
        c.drain(&mut out);
        assert_eq!(out, vec![1, 2, 3]);
    }
}
