//! Stable audit-log event IDs and the writer used by `kernel/ipc`.
//!
//! Every security-relevant decision taken by this crate emits exactly
//! one structured record through [`tairix_log`]. The numeric
//! identifiers are part of the audit contract with external log
//! consumers and may not be re-used or re-numbered.
//! They live in the `kernel/ipc` range reserved by [`tairix_log::EventId`]
//! conventions: `3_000..4_000`. (`kernel/sec` owns `1_000..2_000`;
//! `kernel/mem` owns `2_000..3_000`.)
//!
//! # Event catalogue
//!
//! | ID   | Level | Name                          | When |
//! |-----:|-------|-------------------------------|------|
//! | 3000 | Info  | `PORT_CREATED`                | A capability-checked port was created. |
//! | 3001 | Error | `PORT_CREATE_DENIED`          | A port-creation request was refused (creator lacks bind authority). |
//! | 3002 | Info  | `PORT_DESTROYED`              | A port was destroyed (any subsequent send fails closed). |
//! | 3003 | Info  | `PORT_REGISTERED`             | A port was bound into the named-port registry under its `EndpointId`. |
//! | 3004 | Error | `PORT_REGISTER_DENIED`        | A registration was refused because the `EndpointId` was already bound. |
//! | 3005 | Info  | `PORT_UNREGISTERED`           | A port was removed from the registry and destroyed. |
//! | 3006 | Info  | `PORT_NAME_PUBLISHED`         | A well-known name was bound to an endpoint in the registry. |
//! | 3007 | Error | `PORT_NAME_PUBLISH_DENIED`    | A name binding was refused (name already bound, or its endpoint is not registered). |
//! | 3008 | Info  | `PORT_NAME_WITHDRAWN`         | A well-known name binding was removed (explicitly, or because its endpoint was unregistered). |
//! | 3010 | Debug | `MESSAGE_DELIVERED`           | A message was enqueued for delivery. Recorded at `Debug` for the same reason as `CALL_POSTED` (3043): routine high-throughput transport. |
//! | 3011 | Error | `MESSAGE_SEND_DENIED`         | A send was refused because the sender lacked the port's required capabilities. |
//! | 3012 | Error | `MESSAGE_TOO_LARGE`           | A send was refused because the payload exceeded the port's `max_payload`. |
//! | 3013 | Error | `MESSAGE_SEND_TO_CLOSED_PORT` | A send raced with destruction and lost. |
//! | 3014 | Debug | `MAILBOX_FULL`                | A send was refused because the receiver's mailbox was full. Recorded at `Debug`: a busy receiver is a routine resource condition on a normal high-rate path, not an authorisation decision, so recording one per refused send would flood the log; forensics recovers it by lowering the level. |
//! | 3020 | Info  | `SHMEM_CREATED`               | A shared-memory object was created. |
//! | 3021 | Info  | `SHMEM_MAPPED`                | A mapping into a recipient was established. |
//! | 3022 | Error | `SHMEM_MAP_DENIED`            | A mapping request was refused. |
//! | 3023 | Info  | `SHMEM_REVOKED`               | A shared-memory object was revoked; all live mappings invalidated. |
//! | 3030 | Info  | `NOTIFY_BOUND`                | A receiver bound to a notification channel. |
//! | 3031 | Info  | `NOTIFY_SIGNALLED`            | A notification was delivered. |
//! | 3032 | Error | `NOTIFY_SIGNAL_DENIED`        | A signal was refused (sender lacks the channel's signal capabilities). |
//! | 3040 | Info  | `CALL_ENDPOINT_CREATED`       | A capability-checked synchronous call endpoint was created. |
//! | 3041 | Error | `CALL_ENDPOINT_CREATE_DENIED` | A call-endpoint creation request was refused (creator lacks bind authority). |
//! | 3042 | Info  | `CALL_ENDPOINT_DESTROYED`     | A call endpoint was destroyed (in-flight callers fail closed). |
//! | 3043 | Debug | `CALL_POSTED`                 | A request was posted to a call endpoint, awaiting a reply. Recorded at `Debug`: the synchronous call path is the high-throughput RPC transport (e.g. the USB URB endpoint), so a successful post is routine and would otherwise flood the log two records per round-trip. Its authorisation/size denials (3044, 3045) stay at `Error`; the queue-full resource condition (3047) is `Debug` for the same reason as the post itself. |
//! | 3044 | Error | `CALL_POST_DENIED`            | A request was refused for lack of the endpoint's required capabilities. |
//! | 3045 | Error | `CALL_REQUEST_TOO_LARGE`      | A request was refused because its payload exceeded `max_request`. |
//! | 3046 | Error | `CALL_POST_TO_CLOSED_ENDPOINT`| A post raced with destruction and lost. |
//! | 3047 | Debug | `CALL_QUEUE_FULL`             | A post was refused because the endpoint's outstanding-call queue was full. `Debug` for the same reason as `MAILBOX_FULL` (3014): a busy server is a routine resource condition, not an authorisation decision. |
//! | 3048 | Debug | `CALL_REPLIED`                | A server delivered a reply to an in-flight call. Recorded at `Debug` for the same reason as `CALL_POSTED` (3043): routine high-throughput RPC completion. Its denial (3049) stays at `Error`. |
//! | 3049 | Error | `CALL_REPLY_DENIED`           | A reply was refused. The `reason` field discriminates: `oversize_reply` (the server exceeded `max_reply`) or `unknown_ticket` (no such in-flight call — it timed out, was cancelled, or the ticket is forged). |
//! | 3053 | Warn  | `CALL_TIMED_OUT`              | An in-flight call's deadline elapsed before the server replied; the ticket is retired and a late reply is refused. Recorded because the caller's own failure may be handled silently — for an in-kernel caller (the filesystem's block path) this is the only trace that a device missed its budget. |
//! | 3050 | Error | `CALL_ENDPOINT_REGISTER_DENIED` | A registry bind was refused because the `EndpointId` was already bound (the created endpoint is dropped; mirrors `PORT_REGISTER_DENIED`, 3004). |
//! | 3051 | Info  | `CALL_POSTER_VANISHED`        | A caller task exited with calls still in flight on this endpoint; the kernel cancelled them (queued requests dropped before service, in-service tickets retired so the server's reply fails closed, unclaimed replies discarded). |
//!
//! Adding a new event requires assigning the next free identifier in
//! this file and appending a row to the table in
//! `docs/src/architecture/ipc.md`.

use tairix_log::{log, Event, EventId, Field, Level, Sink};

/// Audit log event identifiers used by `kernel/ipc`.
///
/// The associated numeric values are part of the ABI between TAIRiX
/// and external log consumers and may not be re-used or re-numbered.
/// See the module-level table for the meaning of each ID.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AuditEvent {
    /// A capability-checked port was created.
    PortCreated,
    /// A port-creation request was refused.
    PortCreateDenied,
    /// A port was destroyed.
    PortDestroyed,
    /// A port was bound into the named-port registry.
    PortRegistered,
    /// A registration was refused because the `EndpointId` was already bound.
    PortRegisterDenied,
    /// A port was removed from the registry and destroyed.
    PortUnregistered,
    /// A well-known name was bound to an endpoint in the registry.
    PortNamePublished,
    /// A name binding was refused: the name was already bound, or the
    /// endpoint it would resolve to is not registered.
    PortNamePublishDenied,
    /// A well-known name binding was removed (explicitly, or because the
    /// endpoint it resolved to was unregistered).
    PortNameWithdrawn,
    /// A message was enqueued for delivery.
    ///
    /// Recorded at [`Level::Debug`] for the same reason as
    /// [`Self::CallPosted`]: a routine high-throughput transport, not an
    /// authorisation decision.
    MessageDelivered,
    /// A send was refused for lack of the port's required capabilities.
    MessageSendDenied,
    /// A send was refused because the payload exceeded `max_payload`.
    MessageTooLarge,
    /// A send raced with destruction and lost.
    MessageSendToClosedPort,
    /// A send was refused because the receiver's mailbox was full.
    ///
    /// This is a resource condition, not a denial: the receiver merely has
    /// not drained the mailbox yet, and the sender may retry
    /// ([`Errno::WouldBlock`](tairix_abi::Errno::WouldBlock)). See
    /// [`Self::level`] for why it is recorded at `Debug`.
    MailboxFull,
    /// A shared-memory object was created.
    ShmemCreated,
    /// A mapping into a recipient was established.
    ShmemMapped,
    /// A mapping request was refused.
    ShmemMapDenied,
    /// A shared-memory object was revoked; all mappings invalidated.
    ShmemRevoked,
    /// A receiver bound to a notification channel.
    NotifyBound,
    /// A notification was delivered.
    NotifySignalled,
    /// A signal was refused (sender lacks the channel's signal capabilities).
    NotifySignalDenied,
    /// A capability-checked synchronous call endpoint was created.
    CallEndpointCreated,
    /// A call-endpoint creation request was refused.
    CallEndpointCreateDenied,
    /// A call endpoint was destroyed (in-flight callers fail closed).
    CallEndpointDestroyed,
    /// A request was posted to a call endpoint, awaiting a reply.
    CallPosted,
    /// A request was refused for lack of the endpoint's required capabilities.
    CallPostDenied,
    /// A request was refused because its payload exceeded `max_request`.
    CallRequestTooLarge,
    /// A post raced with destruction and lost.
    CallPostToClosedEndpoint,
    /// A post was refused because the endpoint's outstanding-call queue was full.
    ///
    /// This is a resource condition, not a denial: the server merely has not
    /// drained the queue yet, and the caller may retry
    /// ([`Errno::WouldBlock`](tairix_abi::Errno::WouldBlock)). See
    /// [`Self::level`] for why it is recorded at `Debug`.
    CallQueueFull,
    /// A server delivered a reply to an in-flight call.
    CallReplied,
    /// A reply was refused (unknown ticket, or reply exceeded `max_reply`).
    CallReplyDenied,
    /// A registry bind was refused because the id was already bound.
    CallEndpointRegisterDenied,
    /// A caller exited with calls still in flight; the kernel cancelled them.
    CallPosterVanished,
    /// A payload copy could not be allocated, so the transfer was refused.
    ///
    /// Every kernel-owned payload is a wiped-on-drop buffer, so a port send,
    /// call post, or reply fails closed when the kernel heap cannot hold the
    /// copy. One event covers all three: the condition, its severity, and the
    /// operator's response are the same wherever it is hit, and the endpoint
    /// and length are in the record's fields. Unlike
    /// [`MailboxFull`](Self::MailboxFull) this is machine distress rather
    /// than ordinary back-pressure, so it stays at `Error`.
    PayloadAllocFailed,
    /// A destroyed endpoint's delegated per-endpoint grants were revoked.
    ///
    /// Endpoint ids are numeric and re-creatable, so a grant naming one must
    /// not outlive the endpoint *instance* it was issued against: destroying
    /// the endpoint withdraws every holder's authority over that id in the
    /// same step, and this records how much authority the teardown withdrew.
    CallEndpointGrantsRevoked,
    /// An in-flight call's deadline elapsed before the server replied.
    ///
    /// The ticket is retired, so the caller fails closed and a late reply is
    /// refused. Worth a record of its own: the caller may handle the timeout
    /// silently, and an in-kernel caller takes no path through the syscall
    /// dispatcher's audit at all, so without this a device that stopped
    /// answering leaves no trace but the puzzling refusal of its own late
    /// reply.
    CallTimedOut,
}

impl AuditEvent {
    /// Stable numeric identifier carried by the emitted [`Event`].
    #[must_use]
    pub const fn id(self) -> EventId {
        EventId(match self {
            Self::PortCreated => 3000,
            Self::PortCreateDenied => 3001,
            Self::PortDestroyed => 3002,
            Self::PortRegistered => 3003,
            Self::PortRegisterDenied => 3004,
            Self::PortUnregistered => 3005,
            Self::PortNamePublished => 3006,
            Self::PortNamePublishDenied => 3007,
            Self::PortNameWithdrawn => 3008,
            Self::MessageDelivered => 3010,
            Self::MessageSendDenied => 3011,
            Self::MessageTooLarge => 3012,
            Self::MessageSendToClosedPort => 3013,
            Self::MailboxFull => 3014,
            Self::ShmemCreated => 3020,
            Self::ShmemMapped => 3021,
            Self::ShmemMapDenied => 3022,
            Self::ShmemRevoked => 3023,
            Self::NotifyBound => 3030,
            Self::NotifySignalled => 3031,
            Self::NotifySignalDenied => 3032,
            Self::CallEndpointCreated => 3040,
            Self::CallEndpointCreateDenied => 3041,
            Self::CallEndpointDestroyed => 3042,
            Self::CallPosted => 3043,
            Self::CallPostDenied => 3044,
            Self::CallRequestTooLarge => 3045,
            Self::CallPostToClosedEndpoint => 3046,
            Self::CallQueueFull => 3047,
            Self::CallReplied => 3048,
            Self::CallReplyDenied => 3049,
            Self::CallEndpointRegisterDenied => 3050,
            Self::CallPosterVanished => 3051,
            Self::CallEndpointGrantsRevoked => 3052,
            Self::CallTimedOut => 3053,
            Self::PayloadAllocFailed => 3060,
        })
    }

    /// Severity at which this event is emitted.
    ///
    /// Refused decisions are recorded at [`Level::Error`] so they surface
    /// above a routine info filter without further configuration.
    /// Successful decisions are recorded at [`Level::Info`], **except** the
    /// high-throughput transport records [`MessageDelivered`](Self::MessageDelivered),
    /// [`CallPosted`](Self::CallPosted), and [`CallReplied`](Self::CallReplied):
    /// these paths are the high-throughput transport (e.g. window events,
    /// input, notifications, and the USB URB RPC endpoint), so a
    /// successful delivery, post, or reply is routine throughput that would
    /// flood the log one or two records per transaction. They are recorded at
    /// [`Level::Debug`], below the default `Info` filter, and remain available
    /// for forensics when the level is lowered; their *denials* stay at `Error`.
    ///
    /// [`MailboxFull`](Self::MailboxFull) and
    /// [`CallQueueFull`](Self::CallQueueFull) are `Debug` for a related but
    /// distinct reason: a full mailbox or call queue is a *resource*
    /// condition on those same high-rate paths, not an authorisation decision
    /// — the sender did nothing wrong, the receiver merely has not drained
    /// yet — so recording one per refused send would let ordinary traffic to
    /// a busy receiver flood the log exactly as the transport records themselves
    /// would. They stay recoverable by lowering the level, while every
    /// genuine denial ([`MessageSendDenied`](Self::MessageSendDenied),
    /// [`CallPostDenied`](Self::CallPostDenied), …) stays `Error`.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::PortCreated
            | Self::PortDestroyed
            | Self::PortRegistered
            | Self::PortUnregistered
            | Self::PortNamePublished
            | Self::PortNameWithdrawn
            | Self::ShmemCreated
            | Self::ShmemMapped
            | Self::ShmemRevoked
            | Self::NotifyBound
            | Self::NotifySignalled
            | Self::CallEndpointCreated
            | Self::CallEndpointDestroyed
            | Self::CallPosterVanished
            | Self::CallEndpointGrantsRevoked => Level::Info,
            // A missed deadline is an anomaly the operator must see above the
            // default filter, but it is handled: the caller fails closed.
            Self::CallTimedOut => Level::Warn,
            Self::MessageDelivered
            | Self::CallPosted
            | Self::CallReplied
            | Self::MailboxFull
            | Self::CallQueueFull => Level::Debug,
            Self::PortCreateDenied
            | Self::PortRegisterDenied
            | Self::PortNamePublishDenied
            | Self::MessageSendDenied
            | Self::MessageTooLarge
            | Self::MessageSendToClosedPort
            | Self::ShmemMapDenied
            | Self::NotifySignalDenied
            | Self::CallEndpointCreateDenied
            | Self::CallPostDenied
            | Self::CallRequestTooLarge
            | Self::CallPostToClosedEndpoint
            | Self::CallReplyDenied
            | Self::CallEndpointRegisterDenied
            | Self::PayloadAllocFailed => Level::Error,
        }
    }

    /// Short, stable human-readable message embedded in the [`Event`].
    ///
    /// The text is part of the contract with structured log readers
    /// and must not change once shipped.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::PortCreated => "ipc port created",
            Self::PortCreateDenied => "ipc port creation denied",
            Self::PortDestroyed => "ipc port destroyed",
            Self::PortRegistered => "ipc port registered",
            Self::PortRegisterDenied => "ipc port registration denied",
            Self::PortUnregistered => "ipc port unregistered",
            Self::PortNamePublished => "ipc port name published",
            Self::PortNamePublishDenied => "ipc port name publish denied",
            Self::PortNameWithdrawn => "ipc port name withdrawn",
            Self::MessageDelivered => "ipc message delivered",
            Self::MessageSendDenied => "ipc message send denied",
            Self::MessageTooLarge => "ipc message too large",
            Self::MessageSendToClosedPort => "ipc message send to closed port",
            Self::MailboxFull => "ipc mailbox full",
            Self::ShmemCreated => "ipc shmem created",
            Self::ShmemMapped => "ipc shmem mapped",
            Self::ShmemMapDenied => "ipc shmem map denied",
            Self::ShmemRevoked => "ipc shmem revoked",
            Self::NotifyBound => "ipc notify bound",
            Self::NotifySignalled => "ipc notify signalled",
            Self::NotifySignalDenied => "ipc notify signal denied",
            Self::CallEndpointCreated => "ipc call endpoint created",
            Self::CallEndpointCreateDenied => "ipc call endpoint creation denied",
            Self::CallEndpointDestroyed => "ipc call endpoint destroyed",
            Self::CallPosted => "ipc call posted",
            Self::CallPostDenied => "ipc call post denied",
            Self::CallRequestTooLarge => "ipc call request too large",
            Self::CallPostToClosedEndpoint => "ipc call post to closed endpoint",
            Self::CallQueueFull => "ipc call queue full",
            Self::CallReplied => "ipc call replied",
            Self::CallReplyDenied => "ipc call reply denied",
            Self::CallEndpointRegisterDenied => "ipc call endpoint registration denied",
            Self::CallPosterVanished => "ipc calls cancelled, poster exited",
            Self::CallEndpointGrantsRevoked => "ipc call endpoint grants revoked",
            Self::CallTimedOut => "ipc call timed out",
            Self::PayloadAllocFailed => "ipc payload allocation failed",
        }
    }
}

/// Emit `event` to `sink` with the supplied structured fields.
///
/// Returns whatever [`tairix_log::log`] returns: `true` if the event
/// passed the global level filter, `false` if it was dropped. Callers
/// ignore the return value because the audit trail's
/// configuration — not the call site — decides whether the record
/// reaches a backing store; the *decision* itself is recorded by
/// virtue of the call. Public because the kernel call-endpoint registry
/// (`kernel/core`'s `callreg`) audits its registration refusals with
/// this crate's vocabulary ([`AuditEvent::CallEndpointRegisterDenied`]),
/// exactly as [`crate::registry::PortRegistry`] audits port binds.
pub fn record<S: Sink + ?Sized>(sink: &S, event: AuditEvent, fields: &[Field<'_>]) -> bool {
    log(
        sink,
        &Event {
            level: event.level(),
            id: event.id(),
            message: event.message(),
            fields,
        },
    )
}

/// Shared test-only recording sink used by every module in this crate.
///
/// Lives outside `#[cfg(test)] mod tests` so unit tests in any module
/// can import it via `crate::audit::RecordingSink`. Mirrors the
/// pattern established by `kernel/sec`.
#[cfg(test)]
pub(crate) mod test_support {
    extern crate alloc;
    extern crate std;

    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use std::cell::RefCell;
    use tairix_log::{set_max_level, Event, Level, Sink};

    /// Single-threaded recording sink used by `kernel/ipc` tests.
    pub(crate) struct RecordingSink {
        events: RefCell<Vec<(Level, u32, String)>>,
    }

    impl RecordingSink {
        pub(crate) fn new() -> Self {
            // Lower the global filter so `Info` events are not dropped
            // by the default `Info` threshold under any later test
            // configuration.
            set_max_level(Level::Trace);
            Self {
                events: RefCell::new(Vec::new()),
            }
        }

        pub(crate) fn ids(&self) -> Vec<u32> {
            self.events.borrow().iter().map(|e| e.1).collect()
        }

        pub(crate) fn len(&self) -> usize {
            self.events.borrow().len()
        }
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events
                .borrow_mut()
                .push((event.level, event.id.0, event.message.to_string()));
        }
    }
}

#[cfg(test)]
pub(crate) use test_support::RecordingSink;

#[cfg(test)]
mod tests {
    use super::{record, AuditEvent, RecordingSink};
    use tairix_log::{EventId, Field};

    #[test]
    fn ids_are_frozen() {
        // Pinning the numeric values pins the audit contract.
        assert_eq!(AuditEvent::PortCreated.id(), EventId(3000));
        assert_eq!(AuditEvent::PortCreateDenied.id(), EventId(3001));
        assert_eq!(AuditEvent::PortDestroyed.id(), EventId(3002));
        assert_eq!(AuditEvent::PortRegistered.id(), EventId(3003));
        assert_eq!(AuditEvent::PortRegisterDenied.id(), EventId(3004));
        assert_eq!(AuditEvent::PortUnregistered.id(), EventId(3005));
        assert_eq!(AuditEvent::PortNamePublished.id(), EventId(3006));
        assert_eq!(AuditEvent::PortNamePublishDenied.id(), EventId(3007));
        assert_eq!(AuditEvent::PortNameWithdrawn.id(), EventId(3008));
        assert_eq!(AuditEvent::MessageDelivered.id(), EventId(3010));
        assert_eq!(AuditEvent::MessageSendDenied.id(), EventId(3011));
        assert_eq!(AuditEvent::MessageTooLarge.id(), EventId(3012));
        assert_eq!(AuditEvent::MessageSendToClosedPort.id(), EventId(3013));
        assert_eq!(AuditEvent::MailboxFull.id(), EventId(3014));
        assert_eq!(AuditEvent::ShmemCreated.id(), EventId(3020));
        assert_eq!(AuditEvent::ShmemMapped.id(), EventId(3021));
        assert_eq!(AuditEvent::ShmemMapDenied.id(), EventId(3022));
        assert_eq!(AuditEvent::ShmemRevoked.id(), EventId(3023));
        assert_eq!(AuditEvent::NotifyBound.id(), EventId(3030));
        assert_eq!(AuditEvent::NotifySignalled.id(), EventId(3031));
        assert_eq!(AuditEvent::NotifySignalDenied.id(), EventId(3032));
        assert_eq!(AuditEvent::CallEndpointCreated.id(), EventId(3040));
        assert_eq!(AuditEvent::CallEndpointCreateDenied.id(), EventId(3041));
        assert_eq!(AuditEvent::CallEndpointDestroyed.id(), EventId(3042));
        assert_eq!(AuditEvent::PayloadAllocFailed.id(), EventId(3060));
        assert_eq!(AuditEvent::CallPosted.id(), EventId(3043));
        assert_eq!(AuditEvent::CallPostDenied.id(), EventId(3044));
        assert_eq!(AuditEvent::CallRequestTooLarge.id(), EventId(3045));
        assert_eq!(AuditEvent::CallPostToClosedEndpoint.id(), EventId(3046));
        assert_eq!(AuditEvent::CallQueueFull.id(), EventId(3047));
        assert_eq!(AuditEvent::CallReplied.id(), EventId(3048));
        assert_eq!(AuditEvent::CallReplyDenied.id(), EventId(3049));
        assert_eq!(AuditEvent::CallEndpointRegisterDenied.id(), EventId(3050));
        assert_eq!(AuditEvent::CallPosterVanished.id(), EventId(3051));
        assert_eq!(AuditEvent::CallEndpointGrantsRevoked.id(), EventId(3052));
    }

    #[test]
    fn ids_fall_within_kernel_ipc_reserved_range() {
        for ev in [
            AuditEvent::PortCreated,
            AuditEvent::PortCreateDenied,
            AuditEvent::PortDestroyed,
            AuditEvent::PortRegistered,
            AuditEvent::PortRegisterDenied,
            AuditEvent::PortUnregistered,
            AuditEvent::PortNamePublished,
            AuditEvent::PortNamePublishDenied,
            AuditEvent::PortNameWithdrawn,
            AuditEvent::MessageDelivered,
            AuditEvent::MessageSendDenied,
            AuditEvent::MessageTooLarge,
            AuditEvent::MessageSendToClosedPort,
            AuditEvent::MailboxFull,
            AuditEvent::ShmemCreated,
            AuditEvent::ShmemMapped,
            AuditEvent::ShmemMapDenied,
            AuditEvent::ShmemRevoked,
            AuditEvent::NotifyBound,
            AuditEvent::NotifySignalled,
            AuditEvent::NotifySignalDenied,
            AuditEvent::CallEndpointCreated,
            AuditEvent::CallEndpointCreateDenied,
            AuditEvent::CallEndpointDestroyed,
            AuditEvent::CallPosted,
            AuditEvent::CallPostDenied,
            AuditEvent::CallRequestTooLarge,
            AuditEvent::CallPostToClosedEndpoint,
            AuditEvent::CallQueueFull,
            AuditEvent::CallReplied,
            AuditEvent::CallReplyDenied,
            AuditEvent::CallEndpointRegisterDenied,
            AuditEvent::CallPosterVanished,
            AuditEvent::CallEndpointGrantsRevoked,
        ] {
            let id = ev.id().0;
            assert!(
                (3_000..4_000).contains(&id),
                "audit id {id} outside kernel/ipc range"
            );
        }
    }

    #[test]
    fn record_forwards_one_event_per_call() {
        let sink = RecordingSink::new();
        let kept = record(&sink, AuditEvent::PortCreated, &[]);
        assert!(kept);
        assert_eq!(sink.ids(), [AuditEvent::PortCreated.id().0]);
    }

    #[test]
    fn record_passes_fields_through() {
        let sink = RecordingSink::new();
        let fields = [Field {
            key: "port",
            value: tairix_log::FieldValue::Str("1"),
        }];
        record(&sink, AuditEvent::MessageDelivered, &fields);
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn routine_transport_logs_below_info() {
        // Routine high-throughput transport records (one-way delivery,
        // synchronous post/reply pair) are demoted below the default `Info`
        // filter to keep a busy IPC path from flooding the log; the records
        // remain available when the level is lowered for forensics. Their
        // denials stay at `Error`.
        use tairix_log::Level;
        assert_eq!(AuditEvent::MessageDelivered.level(), Level::Debug);
        assert_eq!(AuditEvent::CallPosted.level(), Level::Debug);
        assert_eq!(AuditEvent::CallReplied.level(), Level::Debug);
        assert!(AuditEvent::MessageDelivered.level() < Level::Info);
        assert!(AuditEvent::CallPosted.level() < Level::Info);
        assert!(AuditEvent::CallReplied.level() < Level::Info);

        // A lifecycle record such as `PortCreated` stays at `Info`.
        assert_eq!(AuditEvent::PortCreated.level(), Level::Info);
    }

    #[test]
    fn back_pressure_events_log_below_error_and_denials_stay_error() {
        // A full mailbox or call queue is the receiver/server merely being
        // slow, not an authorisation decision, so it must not cost the same
        // log severity as a genuine denial — otherwise an ordinary sender
        // hitting a busy receiver at syscall rate floods the audit log
        // exactly as the routine transport records would if they were `Error`.
        use tairix_log::Level;
        assert_eq!(AuditEvent::MailboxFull.level(), Level::Debug);
        assert_eq!(AuditEvent::CallQueueFull.level(), Level::Debug);
        assert!(AuditEvent::MailboxFull.level() < Level::Error);
        assert!(AuditEvent::CallQueueFull.level() < Level::Error);
        // A genuine denial on the very same paths stays at `Error`.
        assert_eq!(AuditEvent::MessageSendDenied.level(), Level::Error);
        assert_eq!(AuditEvent::CallPostDenied.level(), Level::Error);
        assert!(AuditEvent::MailboxFull.level() < AuditEvent::MessageSendDenied.level());
        assert!(AuditEvent::CallQueueFull.level() < AuditEvent::CallPostDenied.level());
        // And below Info.
        assert!(AuditEvent::MailboxFull.level() < Level::Info);
        assert!(AuditEvent::CallQueueFull.level() < Level::Info);
    }

    #[test]
    fn refused_events_log_at_error_level() {
        for ev in [
            AuditEvent::PortCreateDenied,
            AuditEvent::PortRegisterDenied,
            AuditEvent::PortNamePublishDenied,
            AuditEvent::MessageSendDenied,
            AuditEvent::MessageTooLarge,
            AuditEvent::MessageSendToClosedPort,
            AuditEvent::ShmemMapDenied,
            AuditEvent::NotifySignalDenied,
            AuditEvent::CallEndpointCreateDenied,
            AuditEvent::CallPostDenied,
            AuditEvent::CallRequestTooLarge,
            AuditEvent::CallPostToClosedEndpoint,
            AuditEvent::CallReplyDenied,
            AuditEvent::CallEndpointRegisterDenied,
        ] {
            assert_eq!(ev.level(), tairix_log::Level::Error);
        }
    }
}
