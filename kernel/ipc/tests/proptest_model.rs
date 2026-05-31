//! Stateful property model for the `kernel/ipc` port (`AGENTS.md` §19.7
//! Bronze).
//!
//! [`Port`] is the capability-checked IPC dispatch endpoint. §19.7 requires it
//! to carry a `proptest`-style stateful model alongside its unit tests and the
//! §19.6 fuzz harness (`tests/fuzz_port.rs`). Where the fuzz harness hammers
//! raw `(caps, payload)` bytes for crashes, this model generates a *structured*
//! sequence of `send` / `recv` / `destroy` commands and replays it against an
//! independent reference model, letting proptest **shrink** any counterexample
//! to a minimal failing program. The invariants checked after every command:
//!
//! * `send` is **fail-closed** in the exact `Port::send` precedence — closed
//!   port, then capabilities, then size, then capacity — checked against a
//!   mirror that never consults the live port (`AGENTS.md` §5.4).
//! * a delivered message round-trips through `recv` byte-for-byte in FIFO
//!   order, so a sender cannot mutate an accepted payload.
//! * occupancy equals the model and never exceeds the declared capacity.
//! * once destroyed, every send fails closed regardless of authority.
//!
//! ## Wall-clock budget (`AGENTS.md` §19.7)
//!
//! A plain `cargo test` runs [`SMOKE_CASES`] sequences; `cargo xtask proptest`
//! exports `RUSTOS_PROPTEST_BUDGET_SECS` and [`drive`] repeats batches from the
//! same deterministic RNG until the budget elapses.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};
use rustos_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN;
use rustos_abi::{CapabilityId, Errno};
use rustos_caps::CapabilitySet;
use rustos_kernel_ipc::{EndpointId, Port};
use rustos_kernel_sec::{TaskCapabilities, TaskId, UserId};
use rustos_log::{set_max_level, Event, Level, Sink};

/// Sequences run by a plain `cargo test` (no budget set).
const SMOKE_CASES: u32 = 256;
/// Sequences per batch under a wall-clock budget.
const BUDGET_BATCH_CASES: u32 = 256;
/// Port payload bound; well under the global ABI cap so the port-local
/// check is the binding one and oversize payloads are reached often.
const MAX_PAYLOAD: u32 = 16;
/// Mailbox depth.
const MAILBOX_CAPACITY: usize = 4;
/// Capability every sender must hold.
const REQUIRED_SEND_CAP: CapabilityId = CapabilityId::NET_RAW;

/// Capabilities a sender may be drawn with; includes the required cap plus
/// unrelated ones so both the satisfied and denied branches are exercised.
const CAP_UNIVERSE: &[CapabilityId] = &[
    CapabilityId::NET_RAW,
    CapabilityId::AUDIT_READ,
    CapabilityId::FS_MOUNT,
];

struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

fn budget_deadline() -> Option<Instant> {
    let secs: u64 = std::env::var("RUSTOS_PROPTEST_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(Instant::now() + Duration::from_secs(secs))
}

fn drive<S: Strategy>(strategy: S, check: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let deadline = budget_deadline();
    let cases = if deadline.is_some() {
        BUDGET_BATCH_CASES
    } else {
        SMOKE_CASES
    };
    let config = Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner =
        TestRunner::new_with_rng(config, TestRng::deterministic_rng(RngAlgorithm::ChaCha));
    loop {
        if let Err(err) = runner.run(&strategy, &check) {
            panic!("proptest stateful model found a counterexample: {err}");
        }
        if !matches!(deadline, Some(end) if Instant::now() < end) {
            break;
        }
    }
}

fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
    let mut set = CapabilitySet::empty();
    for c in items {
        set.insert(*c);
    }
    set
}

fn task_with(task_id: u64, caps: &CapabilitySet) -> TaskCapabilities {
    TaskCapabilities::derive(TaskId(task_id), UserId(1), *caps, *caps, &NullSink)
}

fn authorised_port() -> Port {
    let creator = task_with(
        1,
        &caps_of(&[CapabilityId::IPC_BIND_PRIVILEGED, REQUIRED_SEND_CAP]),
    );
    Port::create(
        EndpointId(0x4242),
        &creator,
        caps_of(&[REQUIRED_SEND_CAP]),
        CapabilitySet::empty(),
        MAX_PAYLOAD,
        MAILBOX_CAPACITY,
        &NullSink,
    )
    .expect("authorised creator binds the port")
}

/// One operation on the port under test.
#[derive(Clone, Debug)]
enum Cmd {
    /// Send a payload; `cap_mask` selects the sender's capabilities and
    /// `len` is the payload length (deterministically filled).
    Send {
        cap_mask: u8,
        len: usize,
    },
    Recv,
    Destroy,
}

fn command() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        // Weight sends heavily so the mailbox actually fills.
        6 => (0u8..8u8, 0usize..=24).prop_map(|(cap_mask, len)| Cmd::Send { cap_mask, len }),
        3 => Just(Cmd::Recv),
        1 => Just(Cmd::Destroy),
    ]
}

fn program() -> impl Strategy<Value = Vec<Cmd>> {
    prop::collection::vec(command(), 0..=48)
}

#[test]
fn port_lifecycle_tracks_reference_model() {
    set_max_level(Level::Error);
    let effective_max =
        usize::try_from(u64::from(MAX_PAYLOAD).min(u64::from(IPC_MESSAGE_MAX_PAYLOAD_LEN)))
            .expect("payload bound fits usize");

    drive(program(), move |cmds| {
        let sink = NullSink;
        let port = authorised_port();
        // Reference model: queued payloads (FIFO) and the closed flag.
        let mut expected: VecDeque<Vec<u8>> = VecDeque::new();
        let mut closed = false;

        for c in &cmds {
            match c {
                Cmd::Send { cap_mask, len } => {
                    let mut sender_caps = CapabilitySet::empty();
                    for (bit, cap) in CAP_UNIVERSE.iter().enumerate() {
                        if cap_mask & (1 << bit) != 0 {
                            sender_caps.insert(*cap);
                        }
                    }
                    let sender = task_with(0x100, &sender_caps);
                    let payload: Vec<u8> = (0..*len)
                        .map(|i| u8::try_from(i % 251).unwrap_or(0))
                        .collect();

                    // Mirror of `Port::send`'s precedence, never touching the
                    // live port: closed → caps → size → capacity.
                    let caps_ok = caps_of(&[REQUIRED_SEND_CAP]).is_subset_of(sender.effective());
                    let size_ok = payload.len() <= effective_max;
                    let capacity_ok = expected.len() < MAILBOX_CAPACITY;
                    let want = if closed {
                        Err(Errno::NotFound)
                    } else if !caps_ok {
                        Err(Errno::PermissionDenied)
                    } else if !size_ok {
                        Err(Errno::MessageTooLarge)
                    } else if !capacity_ok {
                        Err(Errno::LengthOutOfRange)
                    } else {
                        Ok(())
                    };

                    let got = port.send(&sender, &payload, &sink);
                    prop_assert_eq!(got, want);
                    if want.is_ok() {
                        expected.push_back(payload);
                    }
                }
                Cmd::Recv => match port.recv() {
                    Some(msg) => {
                        let want = expected.pop_front().ok_or_else(|| {
                            TestCaseError::fail("recv returned an unmodelled message")
                        })?;
                        prop_assert_eq!(msg.payload, want);
                    }
                    None => prop_assert!(expected.is_empty(), "recv empty but model was not"),
                },
                Cmd::Destroy => {
                    port.destroy(&sink);
                    closed = true;
                    // Destruction drains in-flight messages.
                    expected.clear();
                }
            }

            prop_assert_eq!(port.len(), expected.len());
            prop_assert!(port.len() <= MAILBOX_CAPACITY);
            prop_assert_eq!(port.is_closed(), closed);
        }
        Ok(())
    });
}
