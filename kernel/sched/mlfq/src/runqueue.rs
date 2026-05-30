//! Per-CPU bounded work-stealing queue.
//!
//! Each CPU owns one [`RunDeque`] per [`crate::Priority`] band. The
//! queue is **SPMC**: a single owner CPU pushes at the *bottom* end;
//! any CPU — including the owner itself — consumes via [`RunDeque::steal`]
//! from the *top* end. The hot push path is wait-free; the consume path
//! is lock-free (the loser of a CAS race returns [`Steal::Retry`]; the
//! scheduler retries against the same victim a bounded number of times,
//! then moves on).
//!
//! Routing the owner's local consumption through `steal` (rather than
//! the classical Chase–Lev LIFO `pop`) is a deliberate choice: MLFQ
//! fairness requires *FIFO order within a priority band*. Using the
//! same end for both stealers and the owner gives FIFO with minimal
//! extra cost (a single CAS on the consume path).
//!
//! ## Algorithm
//!
//! The implementation is the classical Chase–Lev work-stealing deque as
//! described in:
//!
//! > Chase, D. & Lev, Y. *Dynamic Circular Work-Stealing Deque*. SPAA '05.
//! > <https://www.dre.vanderbilt.edu/~schmidt/PDF/work-stealing-dequeue.pdf>
//!
//! with three RustOS-specific simplifications:
//!
//! 1. The buffer is **bounded** (its capacity is fixed at construction).
//!    An unbounded queue is a `DoS` vector against the kernel
//!    (`AGENTS.md` §5). Overflow is signalled to the caller via
//!    [`crate::SchedError::QueueFull`], which the scheduler handles by
//!    routing the affected task to another CPU.
//! 2. Slot payload is [`crate::TaskId`] (a `Copy` `u64`), not a
//!    typed pointer. Lost-CAS races therefore cannot leak or
//!    double-free a `Box`; the discarded read is simply ignored.
//! 3. Atomics are routed through an internal loom-compat shim so the
//!    same code can be model-checked under `loom`.
//!
//! ## Memory ordering
//!
//! Following the `PPoPP` '13 paper *Correct and Efficient Work-Stealing
//! for Weak Memory Models* the deque uses:
//!
//! * `Release` on `bottom` after a slot store (push) — pairs with stealer
//!   `Acquire` on `bottom`.
//! * `SeqCst` fence in `pop` between writing `bottom` and reading `top`,
//!   matching the `SeqCst` CAS that stealers do on `top`. This is the
//!   single sequencing point that makes the owner-stealer race
//!   linearisable on the last element.

use alloc::boxed::Box;

use crate::loom_compat::{fence, AtomicI64, AtomicU64, Ordering};

/// Outcome of a stealing attempt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Steal {
    /// Deque was empty at the time of the read.
    Empty,
    /// A concurrent operation invalidated this attempt. The caller should
    /// retry — typically against a different victim.
    Retry,
    /// Successfully removed `TaskId` from the top of the victim's deque.
    Stolen(u64),
}

/// A bounded SPMC work-stealing deque of [`crate::TaskId`] values.
///
/// **Producer:** exactly one CPU calls [`Self::push`].
///
/// **Consumer (stealer):** any number of CPUs — including the producer
/// itself — call [`Self::steal`].
pub struct RunDeque {
    /// Slot storage. Capacity is `mask + 1`, a power of two.
    buf: Box<[AtomicU64]>,
    /// `capacity - 1`; used to index slots without a `%` instruction.
    mask: usize,
    /// Stealer end. Monotonically non-decreasing.
    top: AtomicI64,
    /// Owner end. Monotonically non-decreasing.
    bottom: AtomicI64,
}

// SAFETY: every shared field is atomic; no `&mut` aliasing across threads.
unsafe impl Sync for RunDeque {}

impl RunDeque {
    /// Reserved task-id sentinel meaning "slot empty / never written".
    /// Mirrors the reservation documented on [`crate::TaskId`].
    pub const EMPTY: u64 = 0;

    /// Construct a deque of `capacity` slots.
    ///
    /// Returns `None` if `capacity` is not a power of two, is less than 2,
    /// or exceeds `i64::MAX as usize`. A typed `Option` is preferable to
    /// a panic so the caller — always the scheduler — can surface the
    /// configuration error through `SchedError` (`AGENTS.md` §2.9).
    #[must_use]
    pub fn try_new(capacity: usize) -> Option<Self> {
        if capacity < 2 || !capacity.is_power_of_two() {
            return None;
        }
        // i64::MAX is far in excess of any plausible kernel queue size,
        // but bound it explicitly so the `as i64` casts in this module
        // are always defined.
        // i64::MAX as usize is safe on any host with usize <= 64 bits; on
        // 32-bit hosts we can never exceed usize::MAX anyway.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let limit = i64::MAX as usize;
        if capacity > limit {
            return None;
        }
        let mut v = alloc::vec::Vec::with_capacity(capacity);
        for _ in 0..capacity {
            v.push(AtomicU64::new(Self::EMPTY));
        }
        Some(Self {
            buf: v.into_boxed_slice(),
            mask: capacity - 1,
            top: AtomicI64::new(0),
            bottom: AtomicI64::new(0),
        })
    }

    /// Returns the deque's fixed capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Number of items currently in the deque (approximate under concurrency).
    ///
    /// The result is exact in the absence of concurrent stealers and is
    /// in `[0, capacity]` otherwise. Used by the deque's own tests only —
    /// never to enforce safety — so it is compiled only under `cfg(test)`.
    #[cfg(test)]
    #[must_use]
    pub fn len_approx(&self) -> usize {
        let b = self.bottom.load(Ordering::Acquire);
        let t = self.top.load(Ordering::Acquire);
        let diff = b.wrapping_sub(t);
        if diff <= 0 {
            0
        } else {
            // Bounded by `capacity()` because `push` rejects when full.
            // `diff` is positive here.
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            // diff > 0 was checked above, and capacity <= isize::MAX.
            let d = diff as usize;
            d.min(self.capacity())
        }
    }

    /// Owner-side push at the bottom end.
    ///
    /// Returns `Err(task)` if the deque is full so the caller can hand
    /// the task off to another CPU.
    pub fn push(&self, task: u64) -> Result<(), u64> {
        debug_assert_ne!(task, Self::EMPTY, "TaskId 0 is reserved");
        let b = self.bottom.load(Ordering::Relaxed);
        let t = self.top.load(Ordering::Acquire);
        let size = b.wrapping_sub(t);
        // `size` may transiently be negative if a stealer just completed
        // and bumped `top` past `bottom`'s pre-pop value; in that case
        // there is space and we proceed. A *positive* size that has
        // reached `capacity` is the only refusal condition.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        // size >= 0 was checked above; capacity fits in usize.
        let full = size >= 0 && size as usize >= self.capacity();
        if full {
            return Err(task);
        }
        // `b` is non-negative for any reachable state of a deque whose
        // owner only ever calls push/pop in pairs starting from 0; the
        // truncation cannot lose information because the bits we keep
        // (those masked by `self.mask`) are by definition unaffected.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let idx = (b as usize) & self.mask;
        self.buf[idx].store(task, Ordering::Relaxed);
        // Publish the slot write before the bottom advance is observed.
        fence(Ordering::Release);
        self.bottom.store(b.wrapping_add(1), Ordering::Relaxed);
        Ok(())
    }

    /// Consume a task from the top end. Safe to call from any CPU,
    /// including the owner.
    pub fn steal(&self) -> Steal {
        let t = self.top.load(Ordering::Acquire);
        fence(Ordering::SeqCst);
        let b = self.bottom.load(Ordering::Acquire);
        let size = b.wrapping_sub(t);
        if size <= 0 {
            return Steal::Empty;
        }
        // size > 0 implies t >= 0; mask preserves the low bits we need.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let idx = (t as usize) & self.mask;
        let task = self.buf[idx].load(Ordering::Relaxed);
        match self
            .top
            .compare_exchange(t, t.wrapping_add(1), Ordering::SeqCst, Ordering::Relaxed)
        {
            Ok(_) if task != Self::EMPTY => Steal::Stolen(task),
            // Either the CAS lost (a peer stealer raced us) or the slot
            // was a sentinel from a torn write. Both look the same to
            // the caller: retry.
            Ok(_) | Err(_) => Steal::Retry,
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn rejects_non_pow2_capacity() {
        assert!(RunDeque::try_new(3).is_none());
        assert!(RunDeque::try_new(0).is_none());
        assert!(RunDeque::try_new(1).is_none());
        assert!(RunDeque::try_new(2).is_some());
        assert!(RunDeque::try_new(1024).is_some());
    }

    #[test]
    fn steal_is_fifo_relative_to_push() {
        let q = RunDeque::try_new(8).expect("cap 8");
        for i in 1..=4u64 {
            q.push(i).expect("space");
        }
        let mut stolen = Vec::new();
        loop {
            match q.steal() {
                Steal::Stolen(v) => stolen.push(v),
                Steal::Empty => break,
                Steal::Retry => {}
            }
        }
        assert_eq!(stolen, alloc::vec![1, 2, 3, 4]);
    }

    #[test]
    fn push_returns_err_when_full() {
        let q = RunDeque::try_new(2).expect("cap 2");
        q.push(1).expect("first");
        q.push(2).expect("second");
        assert_eq!(q.push(3), Err(3));
    }

    #[test]
    fn steal_on_empty_returns_empty_repeatedly() {
        let q = RunDeque::try_new(4).expect("cap 4");
        assert_eq!(q.steal(), Steal::Empty);
        assert_eq!(q.steal(), Steal::Empty);
        q.push(7).expect("space");
        assert_eq!(q.steal(), Steal::Stolen(7));
        assert_eq!(q.steal(), Steal::Empty);
    }

    #[test]
    fn steal_then_push_reuses_slots() {
        let q = RunDeque::try_new(4).expect("cap 4");
        for i in 1..=4u64 {
            q.push(i).expect("space");
        }
        // Drain via steal then refill.
        for _ in 0..4 {
            match q.steal() {
                Steal::Stolen(_) | Steal::Retry => {}
                Steal::Empty => unreachable!("not empty yet"),
            }
        }
        // Now empty; we must be able to push capacity items again.
        for i in 10..14u64 {
            q.push(i).expect("space");
        }
        assert_eq!(q.len_approx(), 4);
    }

    #[test]
    fn concurrent_steal_against_owner() {
        // Drive the owner from this thread, run multiple stealers in others.
        // This is *not* a model check (that's in `tests/loom.rs`); it's a
        // soak test that catches obvious data races under TSAN. Stealers
        // terminate when the producer signals it is done *and* a final
        // steal-pass returns `Empty`, so the test cannot hang.
        use core::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::thread;

        let q = Arc::new(RunDeque::try_new(1024).expect("cap"));
        let total = 5_000u64;
        let stealers = 3;
        let producer_done = Arc::new(AtomicBool::new(false));

        let observed = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let mut handles = Vec::new();
        for _ in 0..stealers {
            let q = q.clone();
            let observed = observed.clone();
            let done = producer_done.clone();
            handles.push(thread::spawn(move || {
                let mut local: Vec<u64> = Vec::new();
                loop {
                    match q.steal() {
                        Steal::Stolen(v) => local.push(v),
                        Steal::Empty => {
                            if done.load(Ordering::Acquire) && matches!(q.steal(), Steal::Empty) {
                                break;
                            }
                            std::thread::yield_now();
                        }
                        Steal::Retry => {}
                    }
                }
                observed.lock().unwrap().extend(local);
            }));
        }

        // Owner side: push only. Consumers (stealers) drain.
        let mut pushed = 0u64;
        while pushed < total {
            if q.push(pushed + 1).is_ok() {
                pushed += 1;
            } else {
                std::thread::yield_now();
            }
        }
        producer_done.store(true, Ordering::Release);
        for h in handles {
            h.join().expect("stealer joined");
        }
        // Final post-join sweep in case a stealer left items behind by
        // exiting on a stale Empty observation.
        let mut leftover: Vec<u64> = Vec::new();
        loop {
            match q.steal() {
                Steal::Stolen(v) => leftover.push(v),
                Steal::Empty => break,
                Steal::Retry => {}
            }
        }
        let mut all: Vec<u64> = leftover;
        all.extend(observed.lock().unwrap().iter().copied());
        all.sort_unstable();
        let expected: Vec<u64> = (1..=total).collect();
        assert_eq!(all, expected, "no task lost or duplicated");
    }
}
