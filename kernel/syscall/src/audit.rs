//! Stable audit event identifiers used by `kernel/syscall`.
//!
//! The dispatcher emits exactly one structured log record per security
//! decision it takes. The numeric identifiers
//! assigned here live in the `kernel/syscall` reserved range
//! `5_000..6_000`; they form part of the audit contract with external
//! consumers and must not be re-used or re-numbered.
//!
//! # Event catalogue
//!
//! | ID   | Level | Name                          | When |
//! |-----:|-------|-------------------------------|------|
//! | 5000 | Debug | `SYSCALL_INVOKED`             | A syscall passed every dispatcher check and was forwarded to its handler. Emitted only for security-relevant syscalls (`SyscallSpec::audit`). Recorded at `Debug` so a busy workload's steady allow stream cannot flood the default `Info` console; lower the filter to capture it. |
//! | 5001 | Error | `SYSCALL_PERMISSION_DENIED`   | The caller's effective capability set does not contain the syscall's `required_capability`. |
//! | 5002 | Error | `SYSCALL_UNKNOWN`             | The supplied syscall number is outside the `abi-v1` table. |
//! | 5003 | Error | `SYSCALL_BAD_ARGUMENTS`       | One or more arguments failed type-specific validation. |
//! | 5004 | Error | `SYSCALL_HANDLER_REJECTED`    | The owning subsystem rejected the call after the dispatcher checks passed. |
//! | 5005 | Debug | `SYSCALL_HANDLER_WOULD_BLOCK` | The owning subsystem had nothing to return yet and the caller may retry (`Errno::WouldBlock`). Not a rejection: recorded at `Debug` so a routine poll-while-pending cannot flood the log. |
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
    ///
    /// An *allow* record for a security-relevant syscall. Recorded at
    /// [`Level::Debug`]: a routine workload invokes audited syscalls
    /// continuously, so at `Info` this record drowns every other line on
    /// the default console filter. The record remains available for
    /// forensics when the level is lowered; refusals stay at
    /// [`Level::Error`] and always surface.
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
    /// The owning subsystem had nothing to return yet and the caller may
    /// retry (the handler returned [`Errno::WouldBlock`]).
    ///
    /// This is **not** a rejection — the dispatcher's capability and
    /// argument checks all passed and no security decision was taken; the
    /// call simply made no progress (the `abi-v1` `EAGAIN`/`EWOULDBLOCK`
    /// retry signal, e.g. `users_db_read` while the encrypted root is still
    /// being unlocked, or a non-blocking `ipc_recv` on an empty mailbox).
    /// It is recorded at [`Level::Debug`] so a caller that legitimately
    /// polls-while-pending cannot flood the log with errors, while the
    /// record remains available for flood/DoS forensics when the level is
    /// lowered.
    ///
    /// [`Errno::WouldBlock`]: rustos_abi::Errno::WouldBlock
    SyscallHandlerWouldBlock,
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
            Self::SyscallHandlerWouldBlock => 5005,
        })
    }

    /// Severity at which this event is emitted.
    ///
    /// Refused or failed dispatches are recorded at [`Level::Error`] so
    /// they surface above a routine info filter. The high-rate benign
    /// outcomes — a successful dispatch and a would-block retry — are
    /// recorded at [`Level::Debug`] so they cannot flood the default
    /// `Info` console; lowering the filter recovers them for forensics.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            // The benign high-rate outcomes: the continuous allow stream
            // of a routine workload and the "nothing yet, retry" signal.
            // Neither is an error, and both are too frequent for the
            // default console filter.
            Self::SyscallInvoked | Self::SyscallHandlerWouldBlock => Level::Debug,
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
            Self::SyscallHandlerWouldBlock => "syscall pending; caller may retry",
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
    use rustos_log::{EventId, Level};

    #[test]
    fn ids_are_frozen_and_in_range() {
        for ev in [
            AuditEvent::SyscallInvoked,
            AuditEvent::SyscallPermissionDenied,
            AuditEvent::SyscallUnknown,
            AuditEvent::SyscallBadArguments,
            AuditEvent::SyscallHandlerRejected,
            AuditEvent::SyscallHandlerWouldBlock,
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
        assert_eq!(AuditEvent::SyscallHandlerWouldBlock.id(), EventId(5005));
    }

    #[test]
    fn a_would_block_outcome_is_recorded_below_error() {
        // The benign "nothing yet, retry" outcome must never surface at the
        // error level a genuine rejection does — otherwise a caller that
        // legitimately polls while pending (e.g. `login` reading
        // `users_db_read` while the encrypted root unlocks) floods the boot
        // log with errors. It is `Debug`, below
        // the default `Info` filter, so it is dropped unless the level is
        // lowered for forensics.
        assert_eq!(AuditEvent::SyscallHandlerWouldBlock.level(), Level::Debug);
        assert!(
            AuditEvent::SyscallHandlerWouldBlock.level()
                < AuditEvent::SyscallHandlerRejected.level()
        );
        assert!(AuditEvent::SyscallHandlerWouldBlock.level() < Level::Info);
    }

    #[test]
    fn a_successful_dispatch_is_recorded_below_the_default_filter() {
        // The steady allow stream of a busy workload must never flood the
        // default `Info` console: `SyscallInvoked` is `Debug`, dropped by
        // the default filter and recovered by lowering it for forensics.
        // Every refusal stays at `Error` and always surfaces.
        assert_eq!(AuditEvent::SyscallInvoked.level(), Level::Debug);
        assert!(AuditEvent::SyscallInvoked.level() < Level::Info);
        assert!(AuditEvent::SyscallInvoked.level() < AuditEvent::SyscallPermissionDenied.level());
    }
}
