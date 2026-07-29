//! Deterministic fuzz harness for the capability-checked IPC port.
//!
//! [`Port::send`] is an IPC endpoint that accepts a payload and a sender's
//! capability set from a possibly hostile caller, so
//! ("every IPC endpoint ... has a fuzz target") it is driven by a fuzz
//! harness. This is the IPC-endpoint harness the burn-down (PLAN.md
//! item 5) calls for; it sits alongside the `lib/abi` decoder, the syscall
//! dispatcher, and the `userland/net` parser harnesses in the `cargo xtask
//! fuzz` target set.
//!
//! TAIRiX does not pull in an external fuzz runner: a
//! deterministic, per-run-seeded PRNG drives random `(sender capabilities,
//! payload)` pairs against a port and asserts the invariants the dispatch
//! path must uphold no matter what a caller crafts:
//!
//! 1. Sending never panics for any input.
//! 2. **Fail-closed**: a send succeeds *iff* the sender
//!    holds every required capability, the payload fits `max_payload`, and
//!    the mailbox has room — checked against an independent mirror that
//!    mirrors the dispatcher's caps → size → capacity precedence.
//! 3. A delivered message round-trips through `recv` byte-for-byte in FIFO
//!    order; the kernel copies the payload, so a sender cannot alter it
//!    after acceptance.
//! 4. The mailbox never exceeds its declared capacity.
//!
//! A separate test asserts the closed-port fast path fails closed with
//! [`Errno::NotFound`] regardless of how privileged the sender is.
//!
//! ## Wall-clock budget
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed. When
//! `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS`, the harness keeps
//! drawing from the *same continuing* PRNG stream until the budget elapses
//! — the "run each harness for its wall-clock budget" contract — while the logged
//! seed keeps any crash reproducible.

use std::collections::VecDeque;

use tairix_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN;
use tairix_abi::{CapabilityId, Errno};
use tairix_caps::CapabilitySet;
use tairix_kernel_ipc::{EndpointId, Port};
use tairix_kernel_sec::{TaskCapabilities, TaskId, UserId};
use tairix_log::{set_max_level, Event, Level, Sink};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// The port's payload bound; chosen well under the global ABI cap so the
/// port-local check is the binding one and oversize payloads are reached
/// with high probability.
const MAX_PAYLOAD: u32 = 64;

/// The port's mailbox depth.
const MAILBOX_CAPACITY: usize = 8;

/// The capability the port requires of every sender.
const REQUIRED_SEND_CAP: CapabilityId = CapabilityId::NET_RAW;

/// Small capability universe the fuzzer draws sender sets from. Includes
/// the required cap plus unrelated caps so both the satisfied and the
/// denied branches are exercised.
const CAP_UNIVERSE: &[CapabilityId] = &[
    CapabilityId::NET_RAW,
    CapabilityId::AUDIT_READ,
    CapabilityId::FS_MOUNT,
    CapabilityId::TIME_SET,
];

/// Silent sink — fuzz output must not pollute test stdout.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// xor-shift* PRNG. Deterministic, fast, zero-allocation.
struct Rng(u64);
impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
    let mut set = CapabilitySet::empty();
    for c in items {
        set.insert(*c);
    }
    set
}

/// Build a task whose effective set is exactly `caps` (the derive
/// intersection of a set with itself is that set).
fn task_with(task_id: u64, caps: &CapabilitySet, sink: &NullSink) -> TaskCapabilities {
    TaskCapabilities::derive(TaskId(task_id), UserId(1), *caps, *caps, sink)
}

/// Create the port under test, owned by an authorised creator.
fn authorised_port(sink: &NullSink) -> Port {
    let creator = task_with(
        1,
        &caps_of(&[CapabilityId::IPC_BIND_PRIVILEGED, REQUIRED_SEND_CAP]),
        sink,
    );
    Port::create(
        EndpointId(0x4242),
        &creator,
        caps_of(&[REQUIRED_SEND_CAP]),
        CapabilitySet::empty(),
        MAX_PAYLOAD,
        MAILBOX_CAPACITY,
        sink,
    )
    .expect("authorised creator may bind the port")
}

#[test]
fn fuzz_send_is_fail_closed_and_recv_is_faithful() {
    set_max_level(Level::Error);
    let sink = NullSink;
    let port = authorised_port(&sink);

    // Independent model of the mailbox: the payloads we expect `recv` to
    // hand back, in order, and therefore the live occupancy.
    let mut expected: VecDeque<Vec<u8>> = VecDeque::new();

    let effective_max =
        usize::try_from(u64::from(MAX_PAYLOAD).min(u64::from(IPC_MESSAGE_MAX_PAYLOAD_LEN)))
            .expect("the port's payload bound fits usize on every supported target");

    let mut rng = Rng::new(tairix_fuzzseed::start(
        "fuzz_send_is_fail_closed_and_recv_is_faithful",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut accepted = 0u64;
    let mut denied_caps = 0u64;
    let mut denied_size = 0u64;
    let mut denied_full = 0u64;
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for iter in 0..SMOKE_ITERATIONS {
            // Draw a random sender capability set from the universe.
            let mask = rng.next_u64();
            let mut sender_caps = CapabilitySet::empty();
            for (bit, cap) in CAP_UNIVERSE.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    sender_caps.insert(*cap);
                }
            }
            let sender = task_with(0x100 + (iter & 0xFFFF), &sender_caps, &sink);

            // Random payload length in [0, 128] so oversize is reached.
            let len = (rng.next_u64() % 129) as usize;
            let mut payload = vec![0u8; len];
            for byte in &mut payload {
                *byte = (rng.next_u64() & 0xFF) as u8;
            }

            // Mirror of the dispatcher's decision, in its exact precedence:
            // capabilities, then size, then capacity.
            let caps_ok = caps_of(&[REQUIRED_SEND_CAP]).is_subset_of(sender.effective());
            let size_ok = payload.len() <= effective_max;
            let capacity_ok = expected.len() < MAILBOX_CAPACITY;

            let result = port.send(&sender, &payload, &sink);
            if !caps_ok {
                assert_eq!(
                    result,
                    Err(Errno::PermissionDenied),
                    "missing caps must deny"
                );
                denied_caps += 1;
            } else if !size_ok {
                assert_eq!(
                    result,
                    Err(Errno::MessageTooLarge),
                    "oversize must be refused"
                );
                denied_size += 1;
            } else if !capacity_ok {
                assert_eq!(
                    result,
                    Err(Errno::LengthOutOfRange),
                    "full mailbox must be refused"
                );
                denied_full += 1;
            } else {
                assert_eq!(result, Ok(()), "a satisfied send must be accepted");
                expected.push_back(payload);
                accepted += 1;
            }

            // The live mailbox depth must always equal the model and never
            // exceed the declared capacity.
            assert_eq!(port.len(), expected.len());
            assert!(port.len() <= MAILBOX_CAPACITY);

            // Drain occasionally, asserting FIFO byte-for-byte fidelity.
            if rng.next_u64().is_multiple_of(3) {
                match port.recv() {
                    Some(msg) => {
                        let want = expected.pop_front().expect("model had a queued message");
                        assert_eq!(
                            msg.payload, want,
                            "recv must return the bytes that were sent"
                        );
                    }
                    None => assert!(expected.is_empty(), "recv was empty but model was not"),
                }
            }
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }

    // The seed and ranges are chosen so every branch is exercised; a zero
    // here would mean the harness stopped testing something.
    assert!(accepted > 0, "fuzz produced no accepted sends");
    assert!(denied_caps > 0, "fuzz never exercised the capability check");
    assert!(denied_size > 0, "fuzz never exercised the size check");
    assert!(denied_full > 0, "fuzz never exercised the capacity check");
}

#[test]
fn fuzz_closed_port_fails_closed_for_any_sender() {
    set_max_level(Level::Error);
    let sink = NullSink;
    let port = authorised_port(&sink);
    port.destroy(&sink);

    let mut rng = Rng::new(tairix_fuzzseed::start(
        "fuzz_closed_port_fails_closed_for_any_sender",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    for _ in 0..10_000 {
        // Even a sender holding every capability cannot send to a closed
        // port — destruction wins over authority (fail
        // closed).
        let sender = task_with(7, &caps_of(CAP_UNIVERSE), &sink);
        let len = (rng.next_u64() % 129) as usize;
        let payload = vec![0u8; len];
        assert_eq!(port.send(&sender, &payload, &sink), Err(Errno::NotFound));
        assert!(port.is_empty());
    }
}
