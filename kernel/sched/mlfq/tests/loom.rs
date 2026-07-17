//! Loom model-checking harness for the run-queue's lock-free fast path.
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --test loom \
//!     -p tairix-kernel-sched-mlfq --release
//! ```
//!
//! When the `loom` cfg is *not* enabled, the file compiles to an empty
//! test binary so the default `cargo test` workflow stays fast.
//! `cargo xtask test` runs the loom suite when the helper-tool cache
//! contains a usable `loom` build (mirroring `lib/sync/tests/loom.rs`).
//!
//! Coverage:
//!
//! * `loom_owner_push_then_pop_no_stealer`: a sanity case that the
//!   single-thread path is sound.
//! * `loom_owner_push_versus_stealer`: the *non-trivial* interleaving
//!   the spec is most concerned with — the owner racing a single
//!   stealer for the last element. The invariant is: the element is
//!   handed to *exactly one* of the two, never lost, never duplicated.

#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;

use tairix_kernel_sched_mlfq::runqueue::{RunDeque, Steal};

#[test]
fn loom_owner_push_then_pop_no_stealer() {
    loom::model(|| {
        let q = RunDeque::try_new(4).expect("cap 4");
        q.push(7).expect("push");
        assert_eq!(q.pop(), Some(7));
        assert_eq!(q.pop(), None);
    });
}

#[test]
fn loom_owner_push_versus_stealer() {
    loom::model(|| {
        let q = Arc::new(RunDeque::try_new(2).expect("cap 2"));
        // Owner pushes one element; concurrently, a stealer attempts to
        // claim it. The owner then pops. The element must end up with
        // exactly one of the two parties.
        q.push(42).expect("push");

        let q2 = q.clone();
        let stealer = thread::spawn(move || match q2.steal() {
            Steal::Stolen(v) => Some(v),
            Steal::Empty | Steal::Retry => None,
        });
        let owner_got = q.pop();
        let stealer_got = stealer.join().expect("join");

        let count = owner_got.iter().count() + stealer_got.iter().count();
        assert!(
            count <= 1,
            "element must not be observed by both owner and stealer"
        );
        if count == 0 {
            // Stealer's Retry race lost; element still in the deque.
            assert_eq!(q.pop(), Some(42));
        } else {
            let observed = owner_got.or(stealer_got).expect("one side won");
            assert_eq!(observed, 42);
            assert_eq!(q.pop(), None);
        }
    });
}
