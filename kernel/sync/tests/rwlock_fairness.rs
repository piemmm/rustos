//! Property test for [`RwLock`]'s writer-preference fairness invariant.
//!
//! The invariant under test, from `src/rwlock.rs`:
//!
//! > Once `pending_writers > 0`, no reader observes a successful
//! > `try_read` until the next writer has completed.
//!
//! We model the lock as a deterministic state machine driven by a
//! caller-supplied operation sequence (proptest generates the sequences).
//! For every prefix we check that the invariant holds against the
//! observable behaviour of the real [`RwLock`].

#![cfg(not(loom))]

use proptest::prelude::*;
use rustos_kernel_sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Debug, Clone, Copy)]
enum Op {
    TryRead,
    DropRead,
    TryWriteStart,
    DropWrite,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::TryRead),
        Just(Op::DropRead),
        Just(Op::TryWriteStart),
        Just(Op::DropWrite),
    ]
}

/// Execute the operation sequence against the real lock and check the
/// fairness invariant on every step.
fn run(ops: &[Op]) {
    let lock: RwLock<u32> = RwLock::new(0);
    let mut readers: Vec<RwLockReadGuard<'_, u32>> = Vec::new();
    let mut writer: Option<RwLockWriteGuard<'_, u32>> = None;

    for op in ops {
        match op {
            Op::TryRead => {
                // Snapshot the writer-pending state *before* the call.
                let was_pending = lock.is_write_pending();
                if let Some(g) = lock.try_read() {
                    // Invariant: try_read may only succeed if no writer
                    // was pending and no writer was held.
                    assert!(
                        !was_pending,
                        "try_read succeeded while a writer was pending or held"
                    );
                    readers.push(g);
                }
            }
            Op::DropRead => {
                if !readers.is_empty() {
                    readers.pop();
                }
            }
            Op::TryWriteStart => {
                if writer.is_none() {
                    if let Some(w) = lock.try_write() {
                        // Invariant: only one writer at a time, and no
                        // readers exist while it is held.
                        assert!(readers.is_empty(), "writer acquired with active readers");
                        writer = Some(w);
                    }
                }
            }
            Op::DropWrite => {
                writer = None;
            }
        }
        // Counter invariant: the lock's reported reader count tracks
        // our local view exactly while no writer is held.
        if writer.is_none() {
            assert_eq!(lock.reader_count(), readers.len());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn rwlock_fairness_invariant_holds(ops in proptest::collection::vec(op_strategy(), 0..32)) {
        run(&ops);
    }
}
