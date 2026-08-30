//! Freed-heap probe for IPC payloads: no request or reply may reach the
//! kernel free list still holding its plaintext.
//!
//! The kernel heap is shared across every principal and does not zero on
//! free, so a payload left un-wiped in a released block is readable by
//! whatever allocates it next. Passphrases (the session and elevation
//! exchanges), sealed app-data secrets, and delegated capability tokens all
//! cross these endpoints, so the wipe is a property of the primitive rather
//! than of the endpoints that happen to carry a secret today.
//!
//! Proving it needs to observe memory *after* the owner released it, which no
//! safe read can do — so the observation is made in the allocator itself,
//! where the block is still owned. [`WipeProbe`] scans each block on the way
//! out for a run of the armed sentinel; `probe_detects_an_unwiped_payload`
//! keeps the scan honest by leaking a payload on purpose and requiring the
//! probe to see it.

extern crate alloc;
extern crate std;

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

use tairix_abi::CapabilityId;
use tairix_caps::CapabilitySet;
use tairix_kernel_mem::sensitive::alloc_sensitive;
use tairix_kernel_sec::captable::{ProcessId, TaskCapabilities};
use tairix_kernel_sec::identity::UserId;

use crate::audit::RecordingSink;
use crate::call::{CallEndpoint, CallEndpointLimits, CallTicket, RecvCall, ReplyOutcome};
use crate::port::{EndpointId, Port};

/// Payload length every probe test uses, as the endpoint bounds want it.
/// Comfortably above [`RUN`] so a leaked block is unmistakable.
const PAYLOAD_CAP: u32 = 256;

/// The same length as a byte count.
const PAYLOAD_LEN: usize = PAYLOAD_CAP as usize;

/// Consecutive sentinel bytes that count as a leaked payload. Long enough
/// that unrelated test data cannot match by chance.
const RUN: usize = 32;

/// Sentinel for the IPC round trips.
const IPC_SENTINEL: u8 = 0xA5;

/// A distinct sentinel for the self-check, so a block it frees dirty and a
/// later test reuses cannot be mistaken for an IPC leak.
const SELFCHECK_SENTINEL: u8 = 0x5A;

/// Sentinel the probe is currently scanning for; `0` disarms it.
static PATTERN: AtomicU8 = AtomicU8::new(0);

/// Set when an armed scan saw its sentinel in a released block.
static LEAKED: AtomicBool = AtomicBool::new(false);

/// Serialises the armed window so two probe tests cannot share it.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// Pass-through allocator that inspects every block on release.
struct WipeProbe;

// SAFETY: every method forwards to `std::alloc::System` with the caller's
// unmodified pointer and layout, so the allocator contract is whatever
// `System` guarantees. The added read happens before the block is handed
// back, while this allocation still owns it, and touches exactly the
// `layout.size()` bytes the caller allocated.
unsafe impl GlobalAlloc for WipeProbe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let pattern = PATTERN.load(Ordering::Acquire);
        if pattern != 0 && layout.size() >= RUN {
            let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
            if bytes.windows(RUN).any(|w| w.iter().all(|b| *b == pattern)) {
                LEAKED.store(true, Ordering::Release);
            }
        }
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static PROBE: WipeProbe = WipeProbe;

/// Arms the probe for its lifetime and disarms it on drop, so a panicking
/// body cannot leave the scan armed for every later test.
struct Armed;

impl Armed {
    fn new(pattern: u8) -> Self {
        PATTERN.store(pattern, Ordering::Release);
        Self
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        PATTERN.store(0, Ordering::Release);
    }
}

/// Run `body` with the probe armed for `pattern`, reporting whether any
/// block was released still holding it.
fn with_probe(pattern: u8, body: impl FnOnce()) -> bool {
    let _lock = PROBE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    LEAKED.store(false, Ordering::Release);
    {
        let _armed = Armed::new(pattern);
        body();
    }
    LEAKED.load(Ordering::Acquire)
}

fn task_with(process: u64, caps: &[CapabilityId]) -> TaskCapabilities {
    let sink = RecordingSink::new();
    let mut set = CapabilitySet::empty();
    for cap in caps {
        set.insert(*cap);
    }
    TaskCapabilities::derive(ProcessId(process), UserId(1), set, set, &sink)
}

fn endpoint(id: u64, sink: &RecordingSink) -> CallEndpoint {
    let creator = task_with(1, &[]);
    CallEndpoint::create(
        EndpointId(id),
        &creator,
        CapabilitySet::empty(),
        CapabilitySet::empty(),
        CallEndpointLimits {
            max_request: PAYLOAD_CAP,
            max_reply: PAYLOAD_CAP,
            capacity: 8,
        },
        sink,
    )
    .expect("unrestricted endpoint")
}

/// A sentinel-filled payload in a wiping buffer, so the test's own source
/// copy can never be the leak the probe reports.
fn secret() -> tairix_kernel_mem::SensitiveBuffer {
    let mut buf = alloc_sensitive(PAYLOAD_LEN).expect("payload buffer");
    buf.as_bytes_mut().fill(IPC_SENTINEL);
    buf
}

#[test]
fn probe_detects_an_unwiped_payload() {
    // Keeps the round-trip assertions from passing vacuously: a plain `Vec`
    // copy is exactly what the endpoints used to leave in freed heap.
    let seen = with_probe(SELFCHECK_SENTINEL, || {
        drop(alloc::vec![SELFCHECK_SENTINEL; PAYLOAD_LEN]);
    });
    assert!(seen, "the probe must see a payload released un-wiped");
}

#[test]
fn probe_ignores_a_wiped_payload() {
    let seen = with_probe(IPC_SENTINEL, || {
        drop(secret());
    });
    assert!(!seen, "a wiped buffer must leave nothing behind");
}

#[test]
fn call_round_trip_leaves_no_plaintext_in_freed_heap() {
    let sink = RecordingSink::new();
    let caller = task_with(7, &[]);
    let ep = endpoint(0xC1, &sink);

    let seen = with_probe(IPC_SENTINEL, || {
        let ticket = ep
            .post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
            .expect("posted");
        let RecvCall::Received(call) = ep.recv_call(usize::MAX) else {
            panic!("the request must be received");
        };
        assert_eq!(call.request.len(), PAYLOAD_LEN);
        ep.reply(call.ticket, secret().as_bytes(), &sink)
            .expect("replied");
        // Releases the kernel-owned copy of the request the server was handed.
        drop(call);
        let outcome = ep.take_reply(7, ticket, 0, &sink);
        assert!(matches!(outcome, ReplyOutcome::Ready(_)));
        // Releases the kernel-owned copy of the reply.
        drop(outcome);
    });

    assert!(!seen, "a served call must leave no plaintext in freed heap");
}

#[test]
fn abandoned_calls_leave_no_plaintext_in_freed_heap() {
    let sink = RecordingSink::new();
    let caller = task_with(7, &[]);

    let seen = with_probe(IPC_SENTINEL, || {
        // A request withdrawn by its poster, before service.
        let ep = endpoint(0xC2, &sink);
        let ticket = ep
            .post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
            .expect("posted");
        assert!(ep.cancel_one(7, ticket));

        // A request dropped because its poster exited.
        let ticket = ep
            .post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
            .expect("posted");
        assert_eq!(ep.cancel_posted_by(7, &sink), 1);
        assert!(matches!(
            ep.take_reply(7, ticket, 0, &sink),
            ReplyOutcome::Unknown
        ));

        // An unclaimed reply discarded with its poster.
        let ticket = ep
            .post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
            .expect("posted");
        let RecvCall::Received(call) = ep.recv_call(usize::MAX) else {
            panic!("the request must be received");
        };
        ep.reply(call.ticket, secret().as_bytes(), &sink)
            .expect("replied");
        drop(call);
        assert_eq!(ep.cancel_posted_by(7, &sink), 1);
        assert!(matches!(
            ep.take_reply(7, ticket, 0, &sink),
            ReplyOutcome::Unknown
        ));

        // A request and a reply cancelled by endpoint teardown.
        ep.post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
            .expect("posted");
        let ticket = ep
            .post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
            .expect("posted");
        let RecvCall::Received(call) = ep.recv_call(usize::MAX) else {
            panic!("the request must be received");
        };
        drop(call);
        ep.reply(CallTicket(ticket.0), secret().as_bytes(), &sink)
            .expect_err("that ticket is no longer in service");
        ep.destroy(&sink);
    });

    assert!(
        !seen,
        "an abandoned call must leave no plaintext in freed heap"
    );
}

#[test]
fn refused_calls_leave_no_plaintext_in_freed_heap() {
    let sink = RecordingSink::new();
    let caller = task_with(7, &[]);
    let ep = endpoint(0xC3, &sink);

    let seen = with_probe(IPC_SENTINEL, || {
        // Fill the endpoint, then post into the full queue: the refused
        // request's copy is dropped inside `post`.
        for _ in 0..8 {
            ep.post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
                .expect("posted");
        }
        ep.post(&caller, 0x51, secret().as_bytes(), u64::MAX, &sink)
            .expect_err("the queue is full");

        // A reply for an unknown ticket: its copy is dropped inside `reply`.
        ep.reply(CallTicket(0xDEAD), secret().as_bytes(), &sink)
            .expect_err("unknown ticket");
        ep.destroy(&sink);
    });

    assert!(
        !seen,
        "a refused call must leave no plaintext in freed heap"
    );
}

#[test]
fn port_message_leaves_no_plaintext_in_freed_heap() {
    let sink = RecordingSink::new();
    let sender = task_with(7, &[]);
    let owner = task_with(1, &[]);
    let port = Port::create(
        EndpointId(0xB1),
        &owner,
        CapabilitySet::empty(),
        CapabilitySet::empty(),
        PAYLOAD_CAP,
        4,
        &sink,
    )
    .expect("unrestricted port");

    let seen = with_probe(IPC_SENTINEL, || {
        // A delivered-and-drained message.
        port.send(&sender, secret().as_bytes(), &sink)
            .expect("delivered");
        let msg = port.recv().expect("drained");
        assert_eq!(msg.payload.len(), PAYLOAD_LEN);
        drop(msg);

        // A message refused because the mailbox is full.
        for _ in 0..4 {
            port.send(&sender, secret().as_bytes(), &sink)
                .expect("delivered");
        }
        port.send(&sender, secret().as_bytes(), &sink)
            .expect_err("the mailbox is full");

        // Messages dropped by teardown.
        port.destroy(&sink);
    });

    assert!(
        !seen,
        "a port message must leave no plaintext in freed heap"
    );
}
