//! TAIRiX synchronisation primitives.
//!
//! This `lib/` crate ships the foundational synchronisation primitives
//! consumed across the workspace. They carry no kernel dependency, so
//! they live in `lib/` where every layer —
//! kernel subsystems (`kernel/mem`, `kernel/ipc`, `kernel/irq`), the
//! scheduler implementations under `kernel/sched/`, and the
//! architecture ports — may consume the one deduplicated surface.
//!
//! # Primitive catalogue
//!
//! | Primitive | Use when |
//! | --- | --- |
//! | [`SpinLock<T>`] | Short critical section, low contention, no IRQ. |
//! | [`IrqSafeSpinLock<T, I>`] | Same, but the data may be touched from an interrupt handler. |
//! | [`RwLock<T>`] | Many readers, occasional writer; writer-preference. |
//! | [`McsLock<T>`] | High-contention critical sections; need fair queueing. |
//! | [`SeqLock<T>`] | Read-mostly data; readers must never block (e.g. `gettimeofday`). |
//! | [`OnceCell<T>`] / [`Once<T>`] | Set-once / lazy initialisation, no panic on poison. |
//!
//! Each module's documentation states:
//! - exact use cases,
//! - when **not** to use the primitive,
//! - ordering guarantees,
//! - the highest IRQ level at which it is safe.
//!
//! For a decision tree and the architecture-level overview see
//! `docs/src/architecture/sync.md`.
//!
//! # Design notes
//!
//! - Every primitive is usable from a `static` (`const fn new` is
//!   provided on all stable builds; the `loom` model-checking build
//!   relaxes this because `loom`'s atomic constructors are not `const`).
//! - The crate needs only `core`, never `alloc`. That is deliberate and
//!   load-bearing: a `no_std` binary whose crate graph includes `alloc` must
//!   supply a `#[global_allocator]`, so a single allocating primitive here
//!   would force a heap onto the freestanding boot binaries that deliberately
//!   have none — and push them into hand-rolling their own lock instead. A
//!   primitive that must allocate does not belong in this crate.
//! - Every `unsafe` block carries a `// SAFETY:` rationale per.

#![no_std]
#![cfg_attr(loom, allow(dead_code))]

mod loom_compat;

pub mod irq;
/// Debug-only lock-site observation for the lockup watchdog. Present only
/// with the `lock-diagnostics` feature (a `watchdog-diagnostics` kernel
/// build); a shippable build compiles it out entirely.
#[cfg(feature = "lock-diagnostics")]
pub mod lockwatch;
pub mod mcs;
pub mod once;
pub mod rwlock;
pub mod seqlock;
pub mod spinlock;

pub use irq::{InterruptControl, IrqState, NopInterruptControl, NopIrqState};
pub use mcs::{McsGuard, McsLock, McsNode};
pub use once::{AlreadySetError, InitError, Once, OnceCell, PoisonError};
pub use rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard};
pub use seqlock::SeqLock;
pub use spinlock::{IrqSafeSpinLock, IrqSafeSpinLockGuard, SpinLock, SpinLockGuard};

// ---------------------------------------------------------------------------
// Unit tests.
// ---------------------------------------------------------------------------
//
// Loom-gated concurrency tests live in `tests/loom.rs`; property tests
// for the RwLock fairness invariant live in `tests/rwlock_fairness.rs`.
// The tests below are deterministic single-threaded sanity checks plus a
// handful of multi-threaded smoke tests using `std::thread`.

#[cfg(all(test, not(loom)))]
extern crate std;

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::string::String;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::vec::Vec;

    // -- SpinLock ----------------------------------------------------------

    #[test]
    fn spinlock_single_threaded_basic() {
        let l = SpinLock::new(0u32);
        {
            let mut g = l.lock();
            *g = 42;
        }
        assert_eq!(*l.lock(), 42);
        assert!(!l.is_locked());
    }

    #[test]
    fn spinlock_try_lock_yields_none_when_held() {
        let l = SpinLock::new(0u32);
        let g = l.lock();
        assert!(l.try_lock().is_none());
        drop(g);
        assert!(l.try_lock().is_some());
    }

    #[test]
    fn spinlock_into_inner_returns_value() {
        let l = SpinLock::new(String::from("alpha"));
        assert_eq!(l.into_inner(), "alpha");
    }

    #[test]
    fn spinlock_multithreaded_increment() {
        let l = Arc::new(SpinLock::new(0u64));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let l = l.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    *l.lock() += 1;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*l.lock(), 4000);
    }

    #[test]
    fn spinlock_get_mut_bypasses_atomic() {
        let mut l = SpinLock::new(7u32);
        *l.get_mut() = 8;
        assert_eq!(*l.lock(), 8);
    }

    // -- IrqSafeSpinLock ---------------------------------------------------

    #[test]
    fn irq_safe_spinlock_acts_as_spinlock_on_host() {
        let l: IrqSafeSpinLock<u32> = IrqSafeSpinLock::new(0);
        *l.lock() = 11;
        assert_eq!(*l.lock(), 11);
    }

    #[test]
    fn irq_safe_spinlock_try_lock() {
        let l: IrqSafeSpinLock<u32> = IrqSafeSpinLock::new(0);
        let g = l.lock();
        assert!(l.try_lock().is_none());
        drop(g);
        assert!(l.try_lock().is_some());
    }

    // -- RwLock ------------------------------------------------------------

    #[test]
    fn rwlock_many_readers() {
        let l = RwLock::new(99u32);
        let a = l.read();
        let b = l.read();
        assert_eq!(*a + *b, 198);
        assert_eq!(l.reader_count(), 2);
        drop(a);
        drop(b);
        assert_eq!(l.reader_count(), 0);
    }

    #[test]
    fn rwlock_writer_excludes_readers() {
        let l = RwLock::new(0u32);
        let w = l.write();
        assert!(l.try_read().is_none());
        drop(w);
        assert!(l.try_read().is_some());
    }

    #[test]
    fn rwlock_pending_writer_blocks_new_readers() {
        // Acquire a reader, register a writer intent via try_write; new
        // readers must now back off. We can't actually queue a writer
        // without spinning forever, so just exercise the bit logic by
        // calling try_write while a reader is held.
        let l = RwLock::new(0u32);
        let r = l.read();
        // try_write fails because there is a reader, but importantly
        // it leaves pending_writers back at 0.
        assert!(l.try_write().is_none());
        assert!(!l.is_write_pending());
        drop(r);
        // Now try_write succeeds.
        let w = l.try_write().expect("write should succeed");
        drop(w);
    }

    #[test]
    fn rwlock_multithreaded_writers() {
        let l = Arc::new(RwLock::new(0u64));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let l = l.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..500 {
                    *l.write() += 1;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*l.read(), 2000);
    }

    // -- McsLock -----------------------------------------------------------

    #[test]
    fn mcs_lock_single_thread() {
        let l = McsLock::new(0u32);
        let mut node = McsNode::new();
        {
            let mut g = l.lock(&mut node);
            *g = 5;
        }
        let mut node2 = McsNode::new();
        assert_eq!(*l.lock(&mut node2), 5);
    }

    #[test]
    fn mcs_lock_multithreaded_increment() {
        let l = Arc::new(McsLock::new(0u64));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let l = l.clone();
            handles.push(thread::spawn(move || {
                let mut node = McsNode::new();
                for _ in 0..1000 {
                    let mut n = McsNode::new();
                    *l.lock(&mut n) += 1;
                    // Touch the outer node so it isn't optimised away.
                    let _ = &mut node;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut node = McsNode::new();
        assert_eq!(*l.lock(&mut node), 4000);
    }

    // -- SeqLock -----------------------------------------------------------

    #[test]
    fn seqlock_basic_read_write() {
        let s = SeqLock::new((0u32, 0u32));
        assert_eq!(s.read(), (0, 0));
        // SAFETY: single writer in single-threaded test.
        unsafe {
            s.write(|v| *v = (3, 4));
        }
        assert_eq!(s.read(), (3, 4));
    }

    #[test]
    fn seqlock_readers_concurrent_with_writer() {
        let s = Arc::new(SeqLock::new(0u64));
        let writer = {
            let s = s.clone();
            thread::spawn(move || {
                for i in 0u64..5_000 {
                    // SAFETY: this test is the sole writer.
                    unsafe {
                        s.write(|v| *v = i);
                    }
                }
            })
        };
        let reader = {
            let s = s.clone();
            thread::spawn(move || {
                for _ in 0..5_000 {
                    let v = s.read();
                    // Whatever we observed must be a value the writer
                    // actually published.
                    assert!(v < 5_000);
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
    }

    // -- Once / OnceCell ---------------------------------------------------

    #[test]
    fn oncecell_set_then_get() {
        let c: OnceCell<u32> = OnceCell::new();
        assert!(c.get().unwrap().is_none());
        c.set(7).unwrap();
        assert_eq!(c.get().unwrap(), Some(&7));
        let again = c.set(8);
        assert!(matches!(again, Err(AlreadySetError(8))));
    }

    #[test]
    fn oncecell_get_or_try_init_success() {
        let c: OnceCell<u32> = OnceCell::new();
        let r = c.get_or_try_init::<_, ()>(|| Ok(42));
        assert_eq!(r, Ok(&42));
        let r2 = c.get_or_try_init::<_, ()>(|| panic!("must not run"));
        assert_eq!(r2, Ok(&42));
    }

    #[test]
    fn oncecell_get_or_try_init_poison() {
        let c: OnceCell<u32> = OnceCell::new();
        let r = c.get_or_try_init::<_, &'static str>(|| Err("boom"));
        assert_eq!(r, Err(InitError::Init("boom")));
        assert!(c.is_poisoned());
        let r2 = c.get_or_try_init::<_, &'static str>(|| Ok(99));
        assert_eq!(r2, Err(InitError::Poisoned));
        assert_eq!(c.get(), Err(PoisonError));
    }

    #[test]
    fn oncecell_take_resets() {
        let mut c: OnceCell<String> = OnceCell::new();
        c.set(String::from("hi")).unwrap();
        assert_eq!(c.take().unwrap(), Some(String::from("hi")));
        assert!(c.get().unwrap().is_none());
    }

    #[test]
    fn once_call_once_idempotent() {
        let o: Once<u32> = Once::new();
        let r1 = o.call_once::<_, ()>(|| Ok(1));
        assert_eq!(r1, Ok(&1));
        let r2 = o.call_once::<_, ()>(|| Ok(2));
        assert_eq!(r2, Ok(&1));
    }

    #[test]
    fn once_infallible_helper() {
        let o: Once<u32> = Once::new();
        let r = o.call_once_infallible(|| 11).unwrap();
        assert_eq!(r, &11);
    }

    #[test]
    fn oncecell_multithreaded_single_init() {
        use std::sync::atomic::{AtomicU32, Ordering as AOrd};
        let c: Arc<OnceCell<u32>> = Arc::new(OnceCell::new());
        let calls = Arc::new(AtomicU32::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            let calls = calls.clone();
            handles.push(thread::spawn(move || {
                let v = c
                    .get_or_try_init::<_, ()>(|| {
                        calls.fetch_add(1, AOrd::Relaxed);
                        Ok(123)
                    })
                    .unwrap();
                assert_eq!(*v, 123);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(calls.load(AOrd::Relaxed), 1);
    }

    // -- ZST + ?Sized smoke checks ----------------------------------------

    #[test]
    fn spinlock_around_zst() {
        let l = SpinLock::new(());
        let _ = l.lock();
    }

    // -- Extra coverage tests ---------------------------------------------

    #[test]
    fn spinlock_default_and_debug() {
        let l: SpinLock<u32> = SpinLock::default();
        let _ = std::format!("{l:?}");
        let _g = l.lock();
        let _ = std::format!("{l:?}");
    }

    #[test]
    fn irq_safe_spinlock_get_mut_and_into_inner() {
        let mut l: IrqSafeSpinLock<u32> = IrqSafeSpinLock::new(3);
        *l.get_mut() = 9;
        assert_eq!(l.into_inner(), 9);
    }

    #[test]
    fn rwlock_into_inner_and_get_mut() {
        let mut l = RwLock::new(7u32);
        *l.get_mut() = 8;
        assert_eq!(l.into_inner(), 8);
    }

    #[test]
    fn rwlock_default_and_debug() {
        let l: RwLock<u32> = RwLock::default();
        let _ = std::format!("{l:?}");
        let _g = l.write();
        let _ = std::format!("{l:?}");
    }

    #[test]
    fn rwlock_write_waits_for_readers() {
        // A blocking writer should be granted after readers release.
        let l = Arc::new(RwLock::new(0u32));
        let r = l.read();
        let l2 = l.clone();
        let h = thread::spawn(move || {
            *l2.write() = 42;
        });
        // Hold the reader briefly so the writer parks in `pending`.
        std::thread::sleep(std::time::Duration::from_millis(10));
        drop(r);
        h.join().unwrap();
        assert_eq!(*l.read(), 42);
    }

    #[test]
    fn rwlock_pending_writer_blocks_concurrent_readers() {
        // While a writer holds the lock, no `try_read` may succeed until it
        // is released. The handshake is deterministic: the writer signals
        // once it holds the lock and waits for permission to release, so the
        // assertions never depend on thread timing (no
        // flaky tests).
        let l = Arc::new(RwLock::new(0u32));
        let l2 = l.clone();
        let (acquired_tx, acquired_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let w = thread::spawn(move || {
            let _g = l2.write();
            // Announce that the write lock is held, then hold it until told.
            acquired_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            // _g drops here, releasing the writer.
        });
        // Wait until the writer definitely holds the lock before asserting.
        acquired_rx.recv().unwrap();
        assert!(l.try_read().is_none());
        // Let the writer release, then a reader must be able to acquire.
        release_tx.send(()).unwrap();
        w.join().unwrap();
        assert!(l.try_read().is_some());
    }

    #[test]
    fn mcs_lock_into_inner_and_get_mut() {
        let mut l = McsLock::new(0u32);
        *l.get_mut() = 11;
        assert_eq!(l.into_inner(), 11);
    }

    #[test]
    fn mcs_default_is_default_of_t() {
        let l: McsLock<u32> = McsLock::default();
        let mut n = McsNode::default();
        assert_eq!(*l.lock(&mut n), 0);
    }

    #[test]
    fn seqlock_sequence_advances_on_write() {
        let s = SeqLock::new(0u32);
        assert_eq!(s.sequence(), 0);
        // SAFETY: single writer in single-threaded test.
        unsafe { s.write(|v| *v = 1) };
        assert_eq!(s.sequence(), 2);
    }

    #[test]
    fn oncecell_take_on_empty_and_poisoned() {
        let mut empty: OnceCell<u32> = OnceCell::new();
        assert_eq!(empty.take(), Ok(None));

        let mut poisoned: OnceCell<u32> = OnceCell::new();
        let _ = poisoned.get_or_try_init::<_, ()>(|| Err(()));
        assert_eq!(poisoned.take(), Err(PoisonError));
    }

    #[test]
    fn oncecell_set_on_poisoned_returns_value() {
        let c: OnceCell<u32> = OnceCell::new();
        let _ = c.get_or_try_init::<_, ()>(|| Err(()));
        match c.set(5) {
            Err(AlreadySetError(v)) => assert_eq!(v, 5),
            Ok(()) => panic!("set must fail on poisoned cell"),
        }
    }

    #[test]
    fn oncecell_set_succeeds_then_subsequent_fails() {
        let c: OnceCell<u32> = OnceCell::new();
        assert!(!c.is_initialised());
        c.set(1).unwrap();
        assert!(c.is_initialised());
        assert!(!c.is_poisoned());
        let again = c.set(2);
        assert!(matches!(again, Err(AlreadySetError(2))));
    }

    #[test]
    fn oncecell_default_and_debug() {
        let c: OnceCell<u32> = OnceCell::default();
        let _ = std::format!("{c:?}");
        c.set(1).unwrap();
        let _ = std::format!("{c:?}");
        let p: OnceCell<u32> = OnceCell::new();
        let _ = p.get_or_try_init::<_, ()>(|| Err(()));
        let _ = std::format!("{p:?}");
    }

    #[test]
    fn once_default_get_is_poisoned_handling() {
        let o: Once<u32> = Once::default();
        assert!(!o.is_poisoned());
        assert!(o.get().unwrap().is_none());
        let _ = o.call_once::<_, &'static str>(|| Err("nope"));
        assert!(o.is_poisoned());
        assert_eq!(o.get(), Err(PoisonError));
        let _ = std::format!("{o:?}");
    }

    #[test]
    fn poison_error_display() {
        let p = PoisonError;
        let s = std::format!("{p}");
        assert!(s.contains("poisoned"));
    }

    #[test]
    fn nop_irq_state_traits() {
        let s = NopIrqState::default();
        let t = s;
        assert_eq!(s, t);
    }
}
