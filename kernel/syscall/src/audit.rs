//! Stable audit event identifiers used by `kernel/syscall`.
//!
//! The dispatcher emits exactly one structured log record per security
//! decision it takes (`AGENTS.md` §5.4.4). The numeric identifiers
//! assigned here live in the `kernel/syscall` reserved range
//! `5_000..6_000`; they form part of the audit contract with external
//! consumers and must not be re-used or re-numbered.
//!
//! # Event catalogue
//!
//! | ID   | Level | Name                          | When |
//! |-----:|-------|-------------------------------|------|
//! | 5000 | Info  | `SYSCALL_INVOKED`             | A syscall passed every dispatcher check and was forwarded to its handler. Emitted only for security-relevant syscalls (`SyscallSpec::audit`). |
//! | 5001 | Error | `SYSCALL_PERMISSION_DENIED`   | The caller's effective capability set does not contain the syscall's `required_capability`. |
//! | 5002 | Error | `SYSCALL_UNKNOWN`             | The supplied syscall number is outside the `abi-v1` table. |
//! | 5003 | Error | `SYSCALL_BAD_ARGUMENTS`       | One or more arguments failed type-specific validation. |
//! | 5004 | Error | `SYSCALL_HANDLER_REJECTED`    | The owning subsystem rejected the call after the dispatcher checks passed. |
//!
//! Adding a new event takes the next free identifier in this file and a
//! row in `docs/src/architecture/syscalls.md`.

use rustos_log::{log, Event, EventId, Field, Level, Sink};

/// Audit event identifiers used by `kernel/syscall`.
///
/// The associated numeric values are part of the audit contract; see the
/// module-level catalogue.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AuditEvent {
    /// A syscall was dispatched to its handler.
    SyscallInvoked,
    /// A syscall was refused because the caller lacked the required
    /// capability.
    SyscallPermissionDenied,
    /// The supplied syscall number was outside the `abi-v1` table.
    SyscallUnknown,
    /// One or more arguments failed type-specific validation.
    SyscallBadArguments,
    /// The owning subsystem rejected the call.
    SyscallHandlerRejected,
}

impl AuditEvent {
    /// Stable numeric identifier carried by the emitted [`Event`].
    #[must_use]
    pub const fn id(self) -> EventId {
        EventId(match self {
            Self::SyscallInvoked => 5000,
            Self::SyscallPermissionDenied => 5001,
            Self::SyscallUnknown => 5002,
            Self::SyscallBadArguments => 5003,
            Self::SyscallHandlerRejected => 5004,
        })
    }

    /// Severity at which this event is emitted.
    ///
    /// Successful dispatches are recorded at [`Level::Info`]; refused or
    /// failed dispatches at [`Level::Error`] so they surface above a
    /// routine info filter.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::SyscallInvoked => Level::Info,
            Self::SyscallPermissionDenied
            | Self::SyscallUnknown
            | Self::SyscallBadArguments
            | Self::SyscallHandlerRejected => Level::Error,
        }
    }

    /// Short, stable human-readable message embedded in the [`Event`].
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::SyscallInvoked => "syscall dispatched",
            Self::SyscallPermissionDenied => "syscall denied: missing capability",
            Self::SyscallUnknown => "syscall denied: unknown number",
            Self::SyscallBadArguments => "syscall denied: invalid arguments",
            Self::SyscallHandlerRejected => "syscall rejected by handler",
        }
    }
}

/// Emit `event` to `sink` with the supplied structured fields.
///
/// Returns whatever [`rustos_log::log`] returns: `true` if the event made
/// it past the global level filter, `false` if it was dropped. The
/// dispatcher ignores the return value because the audit trail's
/// configuration — not the call site — decides whether the record
/// reaches a backing store; the decision is recorded by virtue of the
/// call (mirrors `kernel/sec::audit::record`).
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

#[cfg(test)]
mod tests {
    use super::AuditEvent;
    use rustos_log::EventId;

    #[test]
    fn ids_are_frozen_and_in_range() {
        for ev in [
            AuditEvent::SyscallInvoked,
            AuditEvent::SyscallPermissionDenied,
            AuditEvent::SyscallUnknown,
            AuditEvent::SyscallBadArguments,
            AuditEvent::SyscallHandlerRejected,
        ] {
            let EventId(raw) = ev.id();
            assert!(
                (5_000..6_000).contains(&raw),
                "audit id {raw} outside kernel/syscall reserved range"
            );
        }
        assert_eq!(AuditEvent::SyscallInvoked.id(), EventId(5000));
        assert_eq!(AuditEvent::SyscallPermissionDenied.id(), EventId(5001));
        assert_eq!(AuditEvent::SyscallUnknown.id(), EventId(5002));
        assert_eq!(AuditEvent::SyscallBadArguments.id(), EventId(5003));
        assert_eq!(AuditEvent::SyscallHandlerRejected.id(), EventId(5004));
    }
}
