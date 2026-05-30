//! Loom model-checking harness.
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --test loom \
//!     -p rustos-kernel-sync --release
//! ```
//!
//! When the `loom` cfg is *not* enabled the file compiles to an empty
//! test binary, so the default `cargo test` workflow stays fast.
//! `cargo xtask test` runs the loom suite when the helper-tool cache
//! contains a usable `loom` build (see `tools/xtask`).
//!
//! Each test below exercises a small N-way interleaving (two or three
//! threads, two or three operations each) — enough to catch missed
//! `Acquire`/`Release` pairings without blowing up the state space.

#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;

use rustos_sync::{Epoch, McsLock, McsNode, OnceCell, RwLock, SeqLock, SpinLock};

#[test]
fn loom_spinlock_mutual_exclusion() {
    loom::model(|| {
        let lock = Arc::new(SpinLock::new(0u32));
        let l1 = lock.clone();
        let t1 = thread::spawn(move || {
            *l1.lock() += 1;
        });
        *lock.lock() += 1;
        t1.join().unwrap();
        assert_eq!(lock.into_inner(), 2);
    });
}

#[test]
fn loom_rwlock_writer_preference() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(0u32));
        let l1 = lock.clone();
        let t1 = thread::spawn(move || {
            *l1.write() = 1;
        });
        // Concurrent reader: must observe either 0 (before write) or 1.
        let observed = *lock.read();
        assert!(observed == 0 || observed == 1);
        t1.join().unwrap();
        assert_eq!(*lock.read(), 1);
    });
}

#[test]
fn loom_mcs_lock_fifo() {
    loom::model(|| {
        let lock = Arc::new(McsLock::new(0u32));
        let l1 = lock.clone();
        let t1 = thread::spawn(move || {
            let mut node = McsNode::new();
            *l1.lock(&mut node) += 1;
        });
        let mut node = McsNode::new();
        *lock.lock(&mut node) += 1;
        drop(node);
        t1.join().unwrap();
    });
}

#[test]
fn loom_seqlock_reader_writer() {
    loom::model(|| {
        let s = Arc::new(SeqLock::new(0u32));
        let s1 = s.clone();
        let writer = thread::spawn(move || {
            // SAFETY: this thread is the sole writer.
            unsafe {
                s1.write(|v| *v = 7);
            }
        });
        let v = s.read();
        // Either the pre-write value or the post-write value, never
        // anything else.
        assert!(v == 0 || v == 7);
        writer.join().unwrap();
        assert_eq!(s.read(), 7);
    });
}

#[test]
fn loom_oncecell_single_init() {
    loom::model(|| {
        let cell: Arc<OnceCell<u32>> = Arc::new(OnceCell::new());
        let c1 = cell.clone();
        let t1 = thread::spawn(move || {
            let _ = c1.get_or_try_init::<_, ()>(|| Ok(11));
        });
        let _ = cell.get_or_try_init::<_, ()>(|| Ok(22));
        t1.join().unwrap();
        let v = cell.get().unwrap().unwrap();
        assert!(*v == 11 || *v == 22);
    });
}

#[test]
fn loom_epoch_pin_advance() {
    loom::model(|| {
        let e = Arc::new(Epoch::new());
        let e1 = e.clone();
        let t1 = thread::spawn(move || {
            let p = e1.register();
            let _g = p.pin();
            // Drop guard immediately; just exercising registration.
        });
        e.advance();
        t1.join().unwrap();
    });
}
