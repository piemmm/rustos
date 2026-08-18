//! Loom model-checking harness for the lock-free send fast path of
//! [`tairix_kernel_ipc::port::Port`].
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --test loom \
//!     -p tairix-kernel-ipc --release
//! ```
//!
//! When `loom` is *not* enabled, this file compiles to an empty test
//! binary so the default `cargo test` workflow stays fast. The
//! `cargo xtask test` driver runs the loom suite when the
//! helper-tool cache contains a usable `loom` build, mirroring
//! `kernel/sync/tests/loom.rs` and `kernel/sched/tests/loom.rs`.
//!
//! The invariant under test is:
//!
//! > A send racing with `destroy()` must observe one of two outcomes:
//! > either it observed `OPEN` and the message was enqueued before
//! > `destroy` saw it, *or* it observed `CLOSED` and returned
//! > [`Errno::NotFound`] without enqueueing. The kernel must never
//! > end up in a state where the message was enqueued *after*
//! > `destroy` drained the mailbox (which would constitute a leaked
//! > delivery on a dead port).
//!
//! The model is small enough to enumerate every interleaving in
//! seconds. The properties checked are the same ones the
//! deterministic stress test in `tests/integration.rs` asserts; loom
//! adds confidence by exhaustively exploring orderings the host
//! scheduler may not produce on its own.

#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;

use tairix_abi::{CapabilityId, Errno};
use tairix_caps::CapabilitySet;
use tairix_kernel_ipc::port::{EndpointId, Port};
use tairix_kernel_sec::captable::{ProcessId, TaskCapabilities};
use tairix_kernel_sec::identity::UserId;
use tairix_log::{Event, Sink};

/// A do-nothing sink: loom tests are not interested in audit
/// observability, only the lock-free state machine, and `RefCell` is
/// not loom-safe.
struct DiscardSink;

impl Sink for DiscardSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
    let mut s = CapabilitySet::empty();
    for c in items {
        s.insert(*c);
    }
    s
}

fn make_creator() -> TaskCapabilities {
    TaskCapabilities::derive(
        TaskId(1),
        UserId(1),
        caps_of(&[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW]),
        caps_of(&[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW]),
        &DiscardSink,
    )
}

fn make_sender() -> TaskCapabilities {
    TaskCapabilities::derive(
        TaskId(7),
        UserId(1),
        caps_of(&[CapabilityId::NET_RAW]),
        caps_of(&[CapabilityId::NET_RAW]),
        &DiscardSink,
    )
}

#[test]
fn loom_send_versus_destroy_never_leaks() {
    loom::model(|| {
        let port = Arc::new(
            Port::create(
                EndpointId(1),
                &make_creator(),
                caps_of(&[CapabilityId::NET_RAW]),
                CapabilitySet::empty(),
                8,
                4,
                &DiscardSink,
            )
            .expect("open"),
        );
        let sender_caps = make_sender();

        let sender = {
            let port = port.clone();
            thread::spawn(move || port.send(&sender_caps, b"x", &DiscardSink))
        };
        let destroyer = {
            let port = port.clone();
            thread::spawn(move || port.destroy(&DiscardSink))
        };

        let send_result = sender.join().expect("sender joined");
        destroyer.join().expect("destroyer joined");

        // Invariant: the port is closed at the end.
        assert!(port.is_closed());

        // Invariant: if the send returned Ok, the mailbox has been
        // drained by destroy() and contains nothing afterwards. If
        // the send was refused with NotFound, the mailbox is also
        // empty. There is no third valid outcome.
        match send_result {
            Ok(()) => {
                assert!(
                    port.recv().is_none(),
                    "destroy must drain the message the send delivered"
                );
            }
            Err(Errno::NotFound) => {
                assert!(port.recv().is_none(), "no message ever enqueued");
            }
            Err(other) => panic!("unexpected send result: {other:?}"),
        }
    });
}

#[test]
fn loom_send_fast_path_observes_closed_state() {
    // Two senders racing while destroy runs concurrently. None of
    // them may end up with a buffered message that survives the
    // destroy.
    loom::model(|| {
        let port = Arc::new(
            Port::create(
                EndpointId(2),
                &make_creator(),
                caps_of(&[CapabilityId::NET_RAW]),
                CapabilitySet::empty(),
                8,
                4,
                &DiscardSink,
            )
            .expect("open"),
        );

        let s1 = {
            let port = port.clone();
            let task = make_sender();
            thread::spawn(move || port.send(&task, b"a", &DiscardSink))
        };
        let s2 = {
            let port = port.clone();
            let task = make_sender();
            thread::spawn(move || port.send(&task, b"b", &DiscardSink))
        };
        port.destroy(&DiscardSink);

        let _ = s1.join().expect("s1 joined");
        let _ = s2.join().expect("s2 joined");

        // Mailbox must be empty no matter who won.
        assert!(port.recv().is_none());
        assert!(port.is_closed());
    });
}
