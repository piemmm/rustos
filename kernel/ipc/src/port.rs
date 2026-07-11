//! Capability-checked typed message ports.
//!
//! A [`Port`] is a kernel-owned endpoint identified by a stable
//! [`EndpointId`] declared via `lib/abi`. Each port carries:
//!
//! * `required_send_caps` — the [`CapabilitySet`] a sender's task must
//!   hold for a send to succeed. The kernel enforces this on every
//!   call; the receiver does **not** re-check (final
//!   bullet).
//! * `required_recv_caps` — the set the creator (and any later binder
//!   in 2.7's syscall layer) must hold *at port creation*.
//! * `max_payload` — the maximum payload length, bounded above by
//!   [`rustos_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN`].
//! * a bounded mailbox; out-of-room sends fail with
//!   [`Errno::LengthOutOfRange`] and an audit record.
//!
//! Every refused operation emits exactly one audit event through
//! [`crate::audit`] before returning the [`Errno`] to the caller
//! ("fail-closed"). Ports use a lock-free atomic state word so the
//! send fast path can reject delivery to a closed port without
//! taking the mailbox lock; see [`tests/loom.rs`](../../tests/loom.rs)
//! for the model-checked interleavings.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use rustos_abi::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN;
use rustos_abi::{Errno, Origin};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_log::{Field, Sink};
use rustos_util::fmt::{format_hex_u64, format_usize};

use crate::audit::{record, AuditEvent};
use crate::loom_compat::{AtomicU32, Ordering};

/// Stable endpoint identifier carried in the IPC header.
///
/// Wraps the same `u64` declared by
/// [`rustos_abi::ipc::IpcMessageHeader::endpoint`]; the newtype keeps
/// port identifiers distinct from task identifiers, capability
/// identifiers, and other 64-bit kernel handles.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct EndpointId(pub u64);

/// Fixed (non-tunable) atomic states a [`Port`] can be in.
///
/// Encoded into a single `AtomicU32` so the send fast path observes
/// the port's liveness in one relaxed-acquire load and can reject
/// closed ports without acquiring the mailbox lock.
mod state {
    /// Open and accepting messages.
    pub(super) const OPEN: u32 = 0;
    /// `destroy()` has begun; senders must fail-closed.
    pub(super) const CLOSED: u32 = 1;
}

/// A message in flight or queued for delivery on a [`Port`].
///
/// Payload is owned by the kernel until `recv`; sender bytes are
/// copied into a kernel-side `Vec<u8>` at enqueue time so the sender
/// cannot mutate the buffer after the send has been accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    /// Sender task identifier, taken from the kernel-trusted capability
    /// record at enqueue time — never a caller-supplied value.
    pub sender: u64,
    /// The sender's kernel-attested [`Origin`], snapshotted from its own
    /// task state when the send was accepted, so a receiver can
    /// authenticate each message's principal without trusting anything
    /// the sender wrote into the payload.
    pub origin: Origin,
    /// Payload bytes; length is always `<= max_payload` of the port.
    pub payload: Vec<u8>,
}

/// One end of an IPC message channel.
///
/// Construct with [`Port::create`]; tear down with [`Port::destroy`].
/// Sends use [`Port::send`], which performs the capability check
/// described in the module docs; receives use [`Port::recv`].
pub struct Port {
    id: EndpointId,
    /// Task that bound this port. Only this task may receive from it (or
    /// observe it through a wait-set), and the exit path reclaims every
    /// port by this owner so a dead task's mailbox never lingers. Never
    /// caller-supplied: recorded from the kernel-trusted capability
    /// record at create time.
    owner: u64,
    required_send_caps: CapabilitySet,
    required_recv_caps: CapabilitySet,
    max_payload: u32,
    mailbox_capacity: usize,
    // State word read on every send before taking the lock.
    state: AtomicU32,
    // Mailbox under a spinlock. We use `kernel/sync`'s `SpinLock`
    // because IPC sends never block on I/O and contention is bounded
    // by the mailbox capacity.
    mailbox: rustos_sync::SpinLock<VecDeque<Message>>,
}

impl Port {
    /// Create a new capability-checked port.
    ///
    /// `creator` must already hold every capability listed in
    /// `required_recv_caps`; this enforces the "bind-time check"
    /// half of (the sender check happens on every
    /// [`Self::send`]). The creator additionally must hold
    /// [`rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED`] when
    /// `required_send_caps` is non-empty — i.e. a port that restricts
    /// who may *send* into it is by definition a privileged endpoint —
    /// **or** when `id` is a reserved well-known service rendezvous
    /// ([`rustos_abi::ipc::is_reserved_endpoint`]): an open bind on a
    /// reserved id would let an unprivileged squatter claim traffic
    /// meant for the service, exactly the refusal
    /// [`crate::CallEndpoint::create`] makes. The creator becomes the
    /// port's [`owner`](Self::owner).
    ///
    /// # Errors
    ///
    /// * [`Errno::PermissionDenied`] if `creator` does not satisfy the
    ///   bind authority described above.
    /// * [`Errno::LengthOutOfRange`] if `max_payload >
    ///   IPC_MESSAGE_MAX_PAYLOAD_LEN` or `mailbox_capacity == 0`.
    ///
    /// On any failure exactly one
    /// [`AuditEvent::PortCreateDenied`] is emitted; on success exactly
    /// one [`AuditEvent::PortCreated`].
    pub fn create<S: Sink + ?Sized>(
        id: EndpointId,
        creator: &TaskCapabilities,
        required_send_caps: CapabilitySet,
        required_recv_caps: CapabilitySet,
        max_payload: u32,
        mailbox_capacity: usize,
        audit: &S,
    ) -> Result<Self, Errno> {
        let mut id_buf = [0u8; 16];
        let id_field = Field {
            key: "port",
            value: rustos_log::FieldValue::Str(format_hex_u64(id.0, &mut id_buf)),
        };

        if max_payload > IPC_MESSAGE_MAX_PAYLOAD_LEN || mailbox_capacity == 0 {
            record(audit, AuditEvent::PortCreateDenied, &[id_field]);
            return Err(Errno::LengthOutOfRange);
        }

        // The creator must already hold every required-recv capability:
        // a binder may not grant itself authority it does not already
        // have (no ambient authority).
        if !required_recv_caps.is_subset_of(creator.effective()) {
            record(audit, AuditEvent::PortCreateDenied, &[id_field]);
            return Err(Errno::PermissionDenied);
        }

        // A port that restricts who may send is privileged, and so is a
        // reserved well-known rendezvous id (a squatter must not claim a
        // service's traffic); binding either requires IPC_BIND_PRIVILEGED.
        if (!required_send_caps.is_empty() || rustos_abi::ipc::is_reserved_endpoint(id.0))
            && !creator.has(rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED)
        {
            record(audit, AuditEvent::PortCreateDenied, &[id_field]);
            return Err(Errno::PermissionDenied);
        }

        record(audit, AuditEvent::PortCreated, &[id_field]);

        Ok(Self {
            id,
            owner: creator.task().0,
            required_send_caps,
            required_recv_caps,
            max_payload,
            mailbox_capacity,
            state: AtomicU32::new(state::OPEN),
            mailbox: rustos_sync::SpinLock::new(VecDeque::new()),
        })
    }

    /// Endpoint identifier this port was created with.
    #[must_use]
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// The task that bound this port — the only task that may receive
    /// from it or observe it through a wait-set.
    #[must_use]
    pub fn owner(&self) -> u64 {
        self.owner
    }

    /// `true` when at least one delivered message is waiting to be
    /// drained — the non-consuming readiness peek the wait-set scan
    /// uses; the woken owner's `ipc_recv` performs the actual dequeue.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.mailbox.lock().is_empty()
    }

    /// Maximum payload (bytes) this port will accept.
    #[must_use]
    pub fn max_payload(&self) -> u32 {
        self.max_payload
    }

    /// Capability set required of every sender.
    #[must_use]
    pub fn required_send_caps(&self) -> &CapabilitySet {
        &self.required_send_caps
    }

    /// Capability set required of any binder/receiver at create time.
    #[must_use]
    pub fn required_recv_caps(&self) -> &CapabilitySet {
        &self.required_recv_caps
    }

    /// `true` once [`Self::destroy`] has run.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) == state::CLOSED
    }

    /// Mark the port closed; subsequent sends fail with
    /// [`Errno::NotFound`].
    ///
    /// Idempotent: a second call is a no-op (and still records one
    /// [`AuditEvent::PortDestroyed`], since destruction *attempts* are
    /// the security event). In-flight messages already enqueued are
    /// drained and discarded — receivers learn that the port closed
    /// through their dispatcher in Stage 2.7.
    pub fn destroy<S: Sink + ?Sized>(&self, audit: &S) {
        // Transition OPEN -> CLOSED. Use Release so the prior content
        // of the mailbox is visible to any thread that subsequently
        // sees `CLOSED`, and Acquire on the reverse to pair with the
        // load on the send fast path.
        self.state.store(state::CLOSED, Ordering::Release);
        let drained = {
            let mut q = self.mailbox.lock();
            let n = q.len();
            q.clear();
            n
        };
        let mut id_buf = [0u8; 16];
        let mut drained_buf = [0u8; 12];
        record(
            audit,
            AuditEvent::PortDestroyed,
            &[
                Field {
                    key: "port",
                    value: rustos_log::FieldValue::Str(format_hex_u64(self.id.0, &mut id_buf)),
                },
                Field {
                    key: "drained",
                    value: rustos_log::FieldValue::Str(format_usize(drained, &mut drained_buf)),
                },
            ],
        );
    }

    /// Enqueue `payload` for delivery on this port.
    ///
    /// The kernel enforces every check (final bullet):
    ///
    /// 1. **Lock-free fast path.** If the port has been destroyed,
    ///    [`Errno::NotFound`] is returned without taking the mailbox
    ///    lock and one [`AuditEvent::MessageSendToClosedPort`] is
    ///    emitted.
    /// 2. **Capability check.** Every capability in
    ///    `required_send_caps` must be in `sender.effective()`;
    ///    otherwise [`Errno::PermissionDenied`] +
    ///    [`AuditEvent::MessageSendDenied`].
    /// 3. **Size check.** Payload bytes must be `<= max_payload`,
    ///    bounded again by [`IPC_MESSAGE_MAX_PAYLOAD_LEN`]; otherwise
    ///    [`Errno::MessageTooLarge`] + [`AuditEvent::MessageTooLarge`].
    /// 4. **Capacity check.** If the mailbox is at capacity,
    ///    [`Errno::LengthOutOfRange`] + [`AuditEvent::MailboxFull`].
    ///
    /// On success the payload is copied into a kernel-owned buffer
    /// and one [`AuditEvent::MessageDelivered`] is emitted.
    pub fn send<S: Sink + ?Sized>(
        &self,
        sender: &TaskCapabilities,
        payload: &[u8],
        audit: &S,
    ) -> Result<(), Errno> {
        // Stable field rendering for every audit branch.
        let mut id_buf = [0u8; 16];
        let mut sender_buf = [0u8; 16];
        let mut len_buf = [0u8; 12];
        let port_field = Field {
            key: "port",
            value: rustos_log::FieldValue::Str(format_hex_u64(self.id.0, &mut id_buf)),
        };
        let sender_field = Field {
            key: "sender",
            value: rustos_log::FieldValue::Str(format_hex_u64(sender.task().0, &mut sender_buf)),
        };
        let len_field = Field {
            key: "len",
            value: rustos_log::FieldValue::Str(format_usize(payload.len(), &mut len_buf)),
        };

        // 1. Fast path: reject sends to closed ports without locking.
        if self.state.load(Ordering::Acquire) == state::CLOSED {
            record(
                audit,
                AuditEvent::MessageSendToClosedPort,
                &[port_field, sender_field],
            );
            return Err(Errno::NotFound);
        }

        // 2. Capability check.
        if !self.required_send_caps.is_subset_of(sender.effective()) {
            record(
                audit,
                AuditEvent::MessageSendDenied,
                &[port_field, sender_field],
            );
            return Err(Errno::PermissionDenied);
        }

        // 3. Size check (port-local plus global ABI cap). Compute the
        //    effective limit in `u64` so a port whose `max_payload`
        //    saturates `usize` on a 32-bit target (wasm32) still
        //    rejects oversize payloads correctly.
        let effective_max = u64::from(self.max_payload).min(u64::from(IPC_MESSAGE_MAX_PAYLOAD_LEN));
        if payload.len() as u64 > effective_max {
            record(
                audit,
                AuditEvent::MessageTooLarge,
                &[port_field, sender_field, len_field],
            );
            return Err(Errno::MessageTooLarge);
        }

        // 4. Enqueue under the mailbox lock; re-check destruction
        //    after acquiring, because `destroy()` may have raced
        //    between step 1 and here.
        let mut q = self.mailbox.lock();
        if self.state.load(Ordering::Acquire) == state::CLOSED {
            // Release the lock implicitly by dropping `q` after the
            // record call below — but emit the audit event first so
            // the trail records the *attempt*.
            drop(q);
            record(
                audit,
                AuditEvent::MessageSendToClosedPort,
                &[port_field, sender_field],
            );
            return Err(Errno::NotFound);
        }
        if q.len() >= self.mailbox_capacity {
            drop(q);
            record(audit, AuditEvent::MailboxFull, &[port_field, sender_field]);
            return Err(Errno::LengthOutOfRange);
        }
        q.push_back(Message {
            sender: sender.task().0,
            origin: sender.attest_origin(),
            payload: payload.to_vec(),
        });
        drop(q);
        record(
            audit,
            AuditEvent::MessageDelivered,
            &[port_field, sender_field, len_field],
        );
        Ok(())
    }

    /// Dequeue the oldest delivered message, if any.
    ///
    /// Does *not* perform a capability check: the
    /// kernel decides whether a receiver may bind to a port at
    /// creation time, and the receiver "does not re-check" on every
    /// read. The Stage 2.7 dispatcher is responsible for routing the
    /// returned message to the bound receiver task.
    pub fn recv(&self) -> Option<Message> {
        self.mailbox.lock().pop_front()
    }

    /// Deliver the oldest message to `f`, dequeuing it only if `f`
    /// succeeds (peek-then-commit).
    ///
    /// The message stays at the head of the mailbox while `f` runs and
    /// is removed only when `f` returns `Ok`. If `f` returns `Err` —
    /// for example the receiver's `copy_to_user` faulted, or the
    /// destination buffer was too small — the message is left queued so
    /// a later [`Self::recv`] / `recv_with` re-delivers it rather than
    /// dropping it on the floor (fail closed). The
    /// mailbox lock is held for the duration of `f`, so the peek and the
    /// commit are atomic against a concurrent `recv`: two receivers can
    /// never observe the same head message.
    ///
    /// Returns `None` when the mailbox is empty (and `f` is not called),
    /// otherwise `Some` of whatever `f` returned. Like [`Self::recv`] it
    /// performs no capability check — the receiver's authority is fixed
    /// at bind time.
    pub fn recv_with<R, E>(
        &self,
        f: impl FnOnce(&Message) -> Result<R, E>,
    ) -> Option<Result<R, E>> {
        let mut q = self.mailbox.lock();
        let outcome = f(q.front()?);
        if outcome.is_ok() {
            q.pop_front();
        }
        Some(outcome)
    }

    /// Number of messages currently buffered in the mailbox.
    ///
    /// Snapshot only — the value may change immediately under
    /// concurrent senders. Useful for assertions and for the test
    /// suite; production paths should not branch on this.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mailbox.lock().len()
    }

    /// `true` if the mailbox currently has no buffered messages.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mailbox.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use rustos_abi::CapabilityId;
    use rustos_kernel_sec::captable::TaskId;
    use rustos_kernel_sec::identity::UserId;

    fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    fn task_with(task_id: u64, caps: &[CapabilityId]) -> TaskCapabilities {
        let sink = RecordingSink::new();
        let set = caps_of(caps);
        TaskCapabilities::derive(TaskId(task_id), UserId(1), set, set, &sink)
    }

    #[test]
    fn create_rejects_oversize_max_payload() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let err = Port::create(
            EndpointId(1),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            IPC_MESSAGE_MAX_PAYLOAD_LEN + 1,
            8,
            &sink,
        )
        .err()
        .expect("oversize is refused");
        assert_eq!(err, Errno::LengthOutOfRange);
        assert!(sink.ids().contains(&AuditEvent::PortCreateDenied.id().0));
    }

    #[test]
    fn create_rejects_zero_mailbox_capacity() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let err = Port::create(
            EndpointId(2),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            128,
            0,
            &sink,
        )
        .err()
        .expect("zero mailbox is refused");
        assert_eq!(err, Errno::LengthOutOfRange);
    }

    #[test]
    fn create_rejects_recv_caps_not_held_by_creator() {
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]); // holds nothing
        let required_recv = caps_of(&[CapabilityId::AUDIT_READ]);
        let err = Port::create(
            EndpointId(3),
            &creator,
            CapabilitySet::empty(),
            required_recv,
            128,
            8,
            &sink,
        )
        .err()
        .expect("must not grant unheld authority");
        assert_eq!(err, Errno::PermissionDenied);
        assert!(sink.ids().contains(&AuditEvent::PortCreateDenied.id().0));
    }

    #[test]
    fn create_requires_ipc_bind_privileged_for_restricted_sender() {
        let sink = RecordingSink::new();
        // Creator holds the send-caps but lacks IPC_BIND_PRIVILEGED;
        // therefore may not bind a port that restricts its senders.
        let creator = task_with(1, &[CapabilityId::NET_RAW]);
        let required_send = caps_of(&[CapabilityId::NET_RAW]);
        let err = Port::create(
            EndpointId(4),
            &creator,
            required_send,
            CapabilitySet::empty(),
            128,
            8,
            &sink,
        )
        .err()
        .expect("privileged bind requires IPC_BIND_PRIVILEGED");
        assert_eq!(err, Errno::PermissionDenied);
    }

    #[test]
    fn create_succeeds_for_authorised_creator() {
        let sink = RecordingSink::new();
        let creator = task_with(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
        );
        let p = Port::create(
            EndpointId(5),
            &creator,
            caps_of(&[CapabilityId::NET_RAW]),
            CapabilitySet::empty(),
            128,
            8,
            &sink,
        )
        .expect("authorised");
        assert_eq!(p.id(), EndpointId(5));
        assert_eq!(p.max_payload(), 128);
        assert!(p.is_empty());
        assert!(sink.ids().contains(&AuditEvent::PortCreated.id().0));
    }

    fn open_port() -> (RecordingSink, Port) {
        let sink = RecordingSink::new();
        let creator = task_with(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
        );
        let p = Port::create(
            EndpointId(0xA),
            &creator,
            caps_of(&[CapabilityId::NET_RAW]),
            CapabilitySet::empty(),
            32,
            4,
            &sink,
        )
        .expect("open port");
        (sink, p)
    }

    #[test]
    fn send_with_required_cap_succeeds_and_recv_returns_payload() {
        let (sink, port) = open_port();
        let sender = task_with(7, &[CapabilityId::NET_RAW]);
        assert_eq!(port.send(&sender, b"hello", &sink), Ok(()));
        let msg = port.recv().expect("delivered");
        assert_eq!(msg.sender, 7);
        assert_eq!(msg.payload, b"hello");
        assert!(sink.ids().contains(&AuditEvent::MessageDelivered.id().0));
    }

    #[test]
    fn send_without_required_cap_is_eperm_and_audited() {
        let (sink, port) = open_port();
        let sender = task_with(7, &[]); // missing NET_RAW
        assert_eq!(
            port.send(&sender, b"x", &sink),
            Err(Errno::PermissionDenied)
        );
        assert!(sink.ids().contains(&AuditEvent::MessageSendDenied.id().0));
        // Payload was *not* enqueued.
        assert!(port.is_empty());
    }

    #[test]
    fn oversize_payload_is_emsgsize_and_audited() {
        let (sink, port) = open_port();
        let sender = task_with(7, &[CapabilityId::NET_RAW]);
        let big = alloc::vec![0u8; port.max_payload() as usize + 1];
        assert_eq!(port.send(&sender, &big, &sink), Err(Errno::MessageTooLarge));
        assert!(sink.ids().contains(&AuditEvent::MessageTooLarge.id().0));
    }

    #[test]
    fn mailbox_full_is_audited_and_does_not_drop_existing_messages() {
        let (sink, port) = open_port();
        let sender = task_with(7, &[CapabilityId::NET_RAW]);
        for _ in 0..4 {
            port.send(&sender, b"x", &sink).expect("fits");
        }
        assert_eq!(
            port.send(&sender, b"x", &sink),
            Err(Errno::LengthOutOfRange)
        );
        assert!(sink.ids().contains(&AuditEvent::MailboxFull.id().0));
        assert_eq!(port.len(), 4);
    }

    #[test]
    fn send_to_destroyed_port_fast_path_returns_not_found() {
        let (sink, port) = open_port();
        port.destroy(&sink);
        assert!(port.is_closed());
        let sender = task_with(7, &[CapabilityId::NET_RAW]);
        assert_eq!(port.send(&sender, b"x", &sink), Err(Errno::NotFound));
        assert!(sink
            .ids()
            .contains(&AuditEvent::MessageSendToClosedPort.id().0));
    }

    #[test]
    fn destroy_drains_in_flight_messages() {
        let (sink, port) = open_port();
        let sender = task_with(7, &[CapabilityId::NET_RAW]);
        port.send(&sender, b"a", &sink).unwrap();
        port.send(&sender, b"b", &sink).unwrap();
        port.destroy(&sink);
        assert!(port.recv().is_none());
        // Subsequent sends are refused fail-closed.
        assert_eq!(port.send(&sender, b"c", &sink), Err(Errno::NotFound));
    }

    #[test]
    fn destroy_is_idempotent_and_records_each_attempt() {
        let (sink, port) = open_port();
        port.destroy(&sink);
        let n1 = sink.len();
        port.destroy(&sink);
        let n2 = sink.len();
        assert!(n2 > n1, "each destroy attempt is audited");
    }

    #[test]
    fn unrestricted_port_accepts_any_sender_and_recv_is_uncapped() {
        // A port that declares no required send caps may be used by
        // any task — but the kernel still enforces the size check.
        let sink = RecordingSink::new();
        let creator = task_with(1, &[]);
        let port = Port::create(
            EndpointId(0xB),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            16,
            2,
            &sink,
        )
        .expect("open");
        let sender = task_with(99, &[]); // no caps at all
        port.send(&sender, b"ok", &sink).expect("anyone can send");
        let _ = port.recv().expect("delivered");
    }

    #[test]
    fn recv_with_on_empty_mailbox_does_not_call_the_closure() {
        let (_sink, port) = open_port();
        let mut called = false;
        let outcome = port.recv_with(|_msg| -> Result<(), ()> {
            called = true;
            Ok(())
        });
        assert!(outcome.is_none(), "empty mailbox yields None");
        assert!(!called, "the closure runs only when a message is present");
    }

    #[test]
    fn recv_with_commits_the_message_when_the_closure_succeeds() {
        let (sink, port) = open_port();
        let sender = task_with(7, &[CapabilityId::NET_RAW]);
        port.send(&sender, b"payload", &sink).expect("delivered");

        let seen = port
            .recv_with(|msg| -> Result<Vec<u8>, ()> { Ok(msg.payload.clone()) })
            .expect("a message was present")
            .expect("the closure succeeded");
        assert_eq!(seen, b"payload");
        // A successful closure commits the dequeue.
        assert!(port.is_empty());
    }

    #[test]
    fn recv_with_retains_the_message_when_the_closure_fails() {
        let (sink, port) = open_port();
        let sender = task_with(7, &[CapabilityId::NET_RAW]);
        port.send(&sender, b"first", &sink).expect("delivered");
        port.send(&sender, b"second", &sink).expect("delivered");

        // A failing closure (e.g. a faulting `copy_to_user`) must leave
        // the head message queued so it is not dropped on the floor.
        let outcome = port.recv_with(|_msg| -> Result<(), Errno> { Err(Errno::BadAddress) });
        assert_eq!(outcome, Some(Err(Errno::BadAddress)));
        assert_eq!(port.len(), 2, "a failed receive drops nothing");

        // The very next receive still sees the original head message.
        let head = port.recv().expect("head still present");
        assert_eq!(head.payload, b"first");
    }
}
