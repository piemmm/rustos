//! Stable audit-log event IDs and the writer used by `kernel/ipc`.
//!
//! Every security-relevant decision taken by this crate emits exactly
//! one structured record through [`rustos_log`]. The numeric
//! identifiers are part of the audit contract with external log
//! consumers (`AGENTS.md` §5.4.4) and may not be re-used or re-numbered.
//! They live in the `kernel/ipc` range reserved by [`rustos_log::EventId`]
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
//! | 3010 | Info  | `MESSAGE_DELIVERED`           | A message was enqueued for delivery. |
//! | 3011 | Error | `MESSAGE_SEND_DENIED`         | A send was refused because the sender lacked the port's required capabilities. |
//! | 3012 | Error | `MESSAGE_TOO_LARGE`           | A send was refused because the payload exceeded the port's `max_payload`. |
//! | 3013 | Error | `MESSAGE_SEND_TO_CLOSED_PORT` | A send raced with destruction and lost. |
//! | 3014 | Error | `MAILBOX_FULL`                | A send was refused because the receiver's mailbox was full. |
//! | 3020 | Info  | `SHMEM_CREATED`               | A shared-memory object was created. |
//! | 3021 | Info  | `SHMEM_MAPPED`                | A mapping into a recipient was established. |
//! | 3022 | Error | `SHMEM_MAP_DENIED`            | A mapping request was refused. |
//! | 3023 | Info  | `SHMEM_REVOKED`               | A shared-memory object was revoked; all live mappings invalidated. |
//! | 3030 | Info  | `NOTIFY_BOUND`                | A receiver bound to a notification channel. |
//! | 3031 | Info  | `NOTIFY_SIGNALLED`            | A notification was delivered. |
//! | 3032 | Error | `NOTIFY_SIGNAL_DENIED`        | A signal was refused (sender lacks the channel's signal capabilities). |
//!
//! Adding a new event requires assigning the next free identifier in
//! this file and appending a row to the table in
//! `docs/src/architecture/ipc.md`.

use rustos_log::{log, Event, EventId, Field, Level, Sink};

/// Audit log event identifiers used by `kernel/ipc`.
///
/// The associated numeric values are part of the ABI between RustOS
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
    /// A message was enqueued for delivery.
    MessageDelivered,
    /// A send was refused for lack of the port's required capabilities.
    MessageSendDenied,
    /// A send was refused because the payload exceeded `max_payload`.
    MessageTooLarge,
    /// A send raced with destruction and lost.
    MessageSendToClosedPort,
    /// A send was refused because the receiver's mailbox was full.
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
        })
    }

    /// Severity at which this event is emitted.
    ///
    /// Successful decisions are recorded at [`Level::Info`]; refused
    /// decisions are recorded at [`Level::Error`] so they surface above
    /// a routine info filter without further configuration.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::PortCreated
            | Self::PortDestroyed
            | Self::PortRegistered
            | Self::PortUnregistered
            | Self::MessageDelivered
            | Self::ShmemCreated
            | Self::ShmemMapped
            | Self::ShmemRevoked
            | Self::NotifyBound
            | Self::NotifySignalled => Level::Info,
            Self::PortCreateDenied
            | Self::PortRegisterDenied
            | Self::MessageSendDenied
            | Self::MessageTooLarge
            | Self::MessageSendToClosedPort
            | Self::MailboxFull
            | Self::ShmemMapDenied
            | Self::NotifySignalDenied => Level::Error,
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
        }
    }
}

/// Emit `event` to `sink` with the supplied structured fields.
///
/// Returns whatever [`rustos_log::log`] returns: `true` if the event
/// passed the global level filter, `false` if it was dropped. Callers
/// in this crate ignore the return value because the audit trail's
/// configuration — not the call site — decides whether the record
/// reaches a backing store; the *decision* itself is recorded by
/// virtue of the call.
pub(crate) fn record<S: Sink + ?Sized>(sink: &S, event: AuditEvent, fields: &[Field<'_>]) -> bool {
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
    use rustos_log::{set_max_level, Event, Level, Sink};
    use std::cell::RefCell;

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
    use rustos_log::{EventId, Field};

    #[test]
    fn ids_are_frozen() {
        // Pinning the numeric values pins the audit contract.
        assert_eq!(AuditEvent::PortCreated.id(), EventId(3000));
        assert_eq!(AuditEvent::PortCreateDenied.id(), EventId(3001));
        assert_eq!(AuditEvent::PortDestroyed.id(), EventId(3002));
        assert_eq!(AuditEvent::PortRegistered.id(), EventId(3003));
        assert_eq!(AuditEvent::PortRegisterDenied.id(), EventId(3004));
        assert_eq!(AuditEvent::PortUnregistered.id(), EventId(3005));
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
            value: "1",
        }];
        record(&sink, AuditEvent::MessageDelivered, &fields);
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn refused_events_log_at_error_level() {
        for ev in [
            AuditEvent::PortCreateDenied,
            AuditEvent::PortRegisterDenied,
            AuditEvent::MessageSendDenied,
            AuditEvent::MessageTooLarge,
            AuditEvent::MessageSendToClosedPort,
            AuditEvent::MailboxFull,
            AuditEvent::ShmemMapDenied,
            AuditEvent::NotifySignalDenied,
        ] {
            assert_eq!(ev.level(), rustos_log::Level::Error);
        }
    }
}
