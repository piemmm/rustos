//! Cross-module integration tests for `kernel/ipc`.
//!
//! These tests exercise the contract from the *consumer* side (the
//! Stage 2.7 dispatcher's perspective): the public API only, never
//! the crate-private internals. They cover scenarios called out
//! explicitly in the Stage 2.5 brief:
//!
//! * "port destruction during in-flight send"
//! * "shared-memory revocation racing with mapper"
//!
//! Both scenarios use `std::thread` to drive real concurrent
//! interleavings; the loom model checker covers the lock-free send
//! fast path in `tests/loom.rs`.

#![cfg(not(loom))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use rustos_abi::{CapabilityId, Errno};
use rustos_caps::CapabilitySet;
use rustos_kernel_ipc::audit::AuditEvent;
use rustos_kernel_ipc::port::{EndpointId, Port};
use rustos_kernel_ipc::shmem::{SharedMemory, ShmemId};
use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
use rustos_kernel_sec::identity::UserId;
use rustos_log::{set_max_level, Event, Level, Sink};

/// Thread-safe recording sink: integration tests are multi-threaded
/// where the per-crate `RecordingSink` (`RefCell`) is not.
struct SyncSink {
    events: std::sync::Mutex<Vec<u32>>,
}

impl SyncSink {
    fn new() -> Self {
        set_max_level(Level::Trace);
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn ids(&self) -> Vec<u32> {
        self.events.lock().unwrap().clone()
    }
}

impl Sink for SyncSink {
    fn write_event(&self, event: &Event<'_>) {
        self.events.lock().unwrap().push(event.id.0);
    }
}

fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
    let mut s = CapabilitySet::empty();
    for c in items {
        s.insert(*c);
    }
    s
}

fn task_with(task_id: u64, caps: &[CapabilityId]) -> TaskCapabilities {
    let sink = SyncSink::new();
    let set = caps_of(caps);
    TaskCapabilities::derive(TaskId(task_id), UserId(1), set, set, &sink)
}

/// A send racing with `destroy()` must either succeed *or* fail
/// fail-closed with [`Errno::NotFound`]; it must never lose a message
/// silently and must never crash the kernel. Run the race many times
/// to make the interleaving observable in practice.
#[test]
fn destroy_during_in_flight_send_never_leaks_a_message() {
    for _ in 0..200 {
        let sink = Arc::new(SyncSink::new());
        let creator = task_with(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
        );
        let port = Arc::new(
            Port::create(
                EndpointId(0x00C0_FFEE),
                &creator,
                caps_of(&[CapabilityId::NET_RAW]),
                CapabilitySet::empty(),
                64,
                32,
                &*sink,
            )
            .expect("open"),
        );

        let sender_task = task_with(7, &[CapabilityId::NET_RAW]);
        let start = Arc::new(AtomicBool::new(false));

        let sender_handle = {
            let port = Arc::clone(&port);
            let sink = Arc::clone(&sink);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                port.send(&sender_task, b"race", &*sink)
            })
        };

        let destroyer_handle = {
            let port = Arc::clone(&port);
            let sink = Arc::clone(&sink);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                port.destroy(&*sink);
            })
        };

        start.store(true, Ordering::Release);

        let send_result = sender_handle.join().expect("sender joined");
        destroyer_handle.join().expect("destroyer joined");

        match send_result {
            Ok(()) => {
                // Send won the race; the message was delivered. After
                // destroy ran, the mailbox is empty (drained).
                assert!(port.is_closed());
                assert!(port.recv().is_none(), "destroy drains delivered messages");
            }
            Err(Errno::NotFound) => {
                // Destroy won the race; the kernel rejected fail-closed.
                assert!(port.is_closed());
                let ids = sink.ids();
                assert!(
                    ids.contains(&AuditEvent::MessageSendToClosedPort.id().0),
                    "rejection must be audited"
                );
            }
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }
}

/// A `map`/`with_bytes` reader racing with `revoke()` must observe
/// either the live buffer (read succeeds) or the revoked state
/// (read returns `None`). The kernel must never tear the buffer out
/// from under an in-progress access (which would, in a real kernel,
/// be a use-after-free).
#[test]
fn shmem_revocation_races_with_mapper() {
    for _ in 0..200 {
        let sink = Arc::new(SyncSink::new());
        let creator = task_with(1, &[]);
        let shm = Arc::new(
            SharedMemory::create(
                ShmemId(0x5EE5),
                &creator,
                CapabilitySet::empty(),
                64,
                &*sink,
            )
            .expect("create"),
        );

        // Pre-establish a mapping so the racing reader doesn't need
        // to also race with `map`.
        let recipient = task_with(2, &[]);
        let mapping = shm.map(&recipient, &*sink).expect("map");

        let start = Arc::new(AtomicBool::new(false));

        let reader = {
            let mapping = mapping;
            let start = Arc::clone(&start);
            thread::spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                // Read; succeed or fail closed.
                mapping.with_bytes(|b| {
                    // The buffer is either fully present (length 64,
                    // initialised to zero) or absent (revoked).
                    assert_eq!(b.len(), 64);
                    assert!(b.iter().all(|x| *x == 0));
                })
            })
        };

        let revoker = {
            let shm = Arc::clone(&shm);
            let sink = Arc::clone(&sink);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                shm.revoke(&*sink);
            })
        };

        start.store(true, Ordering::Release);
        let read = reader.join().expect("reader joined");
        revoker.join().expect("revoker joined");

        // Final state: revoked.
        assert!(shm.is_revoked());
        // The read either observed a fully-initialised buffer
        // (`Some`) or noticed the revocation (`None`). Both outcomes
        // are valid; the assertion is that nothing crashed.
        match read {
            Some(()) | None => {}
        }
    }
}

/// End-to-end happy path covering all three primitives in one flow.
#[test]
fn end_to_end_port_shmem_and_notification() {
    let sink = SyncSink::new();
    let owner = task_with(
        1,
        &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
    );
    let port = Port::create(
        EndpointId(1),
        &owner,
        caps_of(&[CapabilityId::NET_RAW]),
        CapabilitySet::empty(),
        16,
        4,
        &sink,
    )
    .unwrap();
    let shm = SharedMemory::create(ShmemId(1), &owner, CapabilitySet::empty(), 16, &sink).unwrap();
    let channel = rustos_kernel_ipc::notify::NotificationChannel::create(
        1,
        &owner,
        CapabilitySet::empty(),
        CapabilitySet::empty(),
        &sink,
    )
    .unwrap();

    let client = task_with(2, &[CapabilityId::NET_RAW]);
    port.send(&client, b"ping", &sink).unwrap();
    let mapping = shm.map(&client, &sink).unwrap();
    mapping
        .with_bytes_mut(|b| b.copy_from_slice(b"0123456789abcdef"))
        .unwrap();
    channel
        .signal(
            &client,
            rustos_kernel_ipc::notify::NotificationFlags(0b1),
            &sink,
        )
        .unwrap();

    assert_eq!(port.recv().unwrap().payload, b"ping");
    assert_eq!(mapping.as_bytes().unwrap(), b"0123456789abcdef".to_vec());
    assert_eq!(
        channel.take_pending(),
        rustos_kernel_ipc::notify::NotificationFlags(0b1)
    );

    // Tear-down: destroy + revoke; subsequent operations fail closed.
    port.destroy(&sink);
    shm.revoke(&sink);
    assert_eq!(port.send(&client, b"x", &sink), Err(Errno::NotFound));
    assert!(mapping.as_bytes().is_none());

    let ids = sink.ids();
    for must_have in [
        AuditEvent::PortCreated.id().0,
        AuditEvent::ShmemCreated.id().0,
        AuditEvent::MessageDelivered.id().0,
        AuditEvent::ShmemMapped.id().0,
        AuditEvent::NotifySignalled.id().0,
        AuditEvent::PortDestroyed.id().0,
        AuditEvent::ShmemRevoked.id().0,
        AuditEvent::MessageSendToClosedPort.id().0,
    ] {
        assert!(ids.contains(&must_have), "missing audit id {must_have}");
    }
}
