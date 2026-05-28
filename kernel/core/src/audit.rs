//! Stable audit-log event IDs for `kernel/core`.
//!
//! Every architecture-neutral init-time and panic-time decision taken
//! by the kernel entry crate emits exactly one structured log record
//! through [`rustos_log`]. The numeric identifiers are part of the
//! audit contract with external log consumers (`AGENTS.md` §5.4.4) and
//! may not be re-used or re-numbered.
//!
//! Per the range convention established in `lib/log` (subsystems pick
//! ranges of `1_000`), `kernel/core` owns `4_000..5_000`. Earlier
//! subsystems already published the lower ranges:
//!
//! * `1_000..2_000` — `kernel/sec`
//! * `3_000..4_000` — `kernel/ipc`
//!
//! # Event catalogue
//!
//! | ID   | Level | Name                          | Sink   | When |
//! |-----:|-------|-------------------------------|--------|------|
//! | 4000 | Info  | `KERNEL_BOOT_STARTED`         | audit  | `kernel_main` entered, before any subsystem init. |
//! | 4001 | Info  | `KERNEL_PHASE_STARTED`        | log    | An init phase began. The `phase` field names it. |
//! | 4002 | Info  | `KERNEL_PHASE_READY`          | log    | An init phase completed successfully. |
//! | 4003 | Error | `KERNEL_PHASE_FAILED`         | audit  | An init phase failed; the kernel will halt. |
//! | 4004 | Info  | `KERNEL_BOOT_COMPLETED`       | audit  | Every init phase finished; control passes to the scheduler. |
//! | 4010 | Error | `KERNEL_PANIC`                | audit  | The kernel panicked; the handler logged context and is about to halt. |
//!
//! "audit" events route through the `audit_sink` channel
//! (`AGENTS.md` §5.4.4 — security-relevant decisions); "log" events
//! route through the diagnostic `log_sink` channel. Production
//! kernels typically wire both sinks to the same COM1 backend, so
//! both channels are visible on the boot console; QEMU integration
//! tests intercept the audit channel only.
//!
//! Adding a new event requires assigning the next free identifier in
//! this file and updating the table in
//! `docs/src/architecture/kernel.md`.

use rustos_log::EventId;

/// Audit event identifiers emitted by `kernel/core`.
///
/// The numeric values are part of the stable ABI between RustOS and
/// external log consumers; see the module-level table for the meaning
/// of each ID.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum AuditEvent {
    /// `kernel_main` entered, before any subsystem init.
    BootStarted,
    /// An init phase began.
    PhaseStarted,
    /// An init phase completed successfully.
    PhaseReady,
    /// An init phase failed; the kernel will halt next.
    PhaseFailed,
    /// Every init phase finished; control passes to the scheduler.
    BootCompleted,
    /// The kernel panicked; the handler logged context and is halting.
    Panic,
}

impl AuditEvent {
    /// Stable numeric identifier carried by the emitted log record.
    #[must_use]
    pub const fn id(self) -> EventId {
        EventId(match self {
            Self::BootStarted => 4000,
            Self::PhaseStarted => 4001,
            Self::PhaseReady => 4002,
            Self::PhaseFailed => 4003,
            Self::BootCompleted => 4004,
            Self::Panic => 4010,
        })
    }

    /// Short, fixed name used as the `message` field of the emitted
    /// [`rustos_log::Event`]. Kept under the 120-character convention
    /// described in `lib/log` so a single record fits one terminal line.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::BootStarted => "kernel boot started",
            Self::PhaseStarted => "kernel init phase started",
            Self::PhaseReady => "kernel init phase ready",
            Self::PhaseFailed => "kernel init phase failed",
            Self::BootCompleted => "kernel boot completed",
            Self::Panic => "kernel panic",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AuditEvent;

    #[test]
    fn event_ids_are_in_kernel_core_range() {
        for ev in [
            AuditEvent::BootStarted,
            AuditEvent::PhaseStarted,
            AuditEvent::PhaseReady,
            AuditEvent::PhaseFailed,
            AuditEvent::BootCompleted,
            AuditEvent::Panic,
        ] {
            let id = ev.id().0;
            assert!(
                (4_000..5_000).contains(&id),
                "{ev:?} id {id} escapes kernel/core range"
            );
        }
    }

    #[test]
    fn event_ids_are_unique() {
        let ids = [
            AuditEvent::BootStarted.id().0,
            AuditEvent::PhaseStarted.id().0,
            AuditEvent::PhaseReady.id().0,
            AuditEvent::PhaseFailed.id().0,
            AuditEvent::BootCompleted.id().0,
            AuditEvent::Panic.id().0,
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "duplicate event id");
            }
        }
    }
}
