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
//! | 4020 | Error | `SYSCALL_FEATURE_UNAVAILABLE` | audit  | The dispatcher reached a syscall handler whose backing subsystem is intentionally not yet wired in (see `KernelSyscallHandlers`). The `feature` field names which deferral was hit. |
//! | 4021 | Error | `SYSCALL_NO_CALLER_CONTEXT`   | audit  | A syscall fired on a CPU with no current task, or whose current task has no capability record. The `KernelDispatchHook` emits this then signals the bin-crate callback to halt the CPU (`AGENTS.md` §5.4.5). |
//! | 4030 | Info  | `PROCESS_SPAWNED`             | audit  | A process was spawned: its image was built and the CPU is about to enter it in user mode. The `entry` field carries the relocated entry-point VA. |
//! | 4031 | Error | `PROCESS_SPAWN_DENIED`        | audit  | A spawn was refused because the caller does not hold `CAP_PROC_SPAWN`; no address space was built (`AGENTS.md` §5.4 — fail closed). |
//! | 4032 | Error | `PROCESS_SPAWN_FAILED`        | audit  | A spawn was authorised but building the process image failed; the partially built address space is discarded. The `cause` field names the `SpawnError`. |
//! | 4040 | Info  | `USERS_DB_LOADED`             | audit  | `/System/Security/Users` was read off the mounted root volume and parsed; the `records` field carries the account count. |
//! | 4041 | Error | `USERS_DB_REJECTED`           | audit  | The users database could not be read or failed validation; no `UsersDb` is held and every login refuses (`AGENTS.md` §5.4 — fail closed). The `cause` field names the refusal. |
//! | 4042 | Info  | `DRIVER_STORE_SCANNED`        | audit  | The `/System/Drivers/` signed-driver store was enumerated for autoload candidates (`AGENTS.md` §18.3 / §18.6). The `drivers` field carries the count of bundle image paths found; `skipped` the count of entries refused fail-closed during the walk. |
//! | 4050 | Info  | `INPUT_DELIVERED`             | audit  | A keyboard driver delivered the **first** key edge to the input-focus arbiter via `key_inject` (`AGENTS.md` §18.3 / §20). Emitted exactly once over the kernel's lifetime, carries no key content or timing — it witnesses that an autoloaded input driver is live, never a per-keystroke record. |
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

use rustos_log::{log, Event, EventId, Field, Level, Sink};

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
    /// A syscall handler's backing subsystem is intentionally not yet
    /// wired in.
    ///
    /// Emitted by `KernelSyscallHandlers` (Stage 2.7 follow-up (f3))
    /// when a stable-ABI syscall reaches a handler whose dependency
    /// (named IPC port registry; user-memory copy-in) has not landed.
    /// The audit record carries a `feature` field naming the missing
    /// piece so external consumers can correlate user-visible
    /// `Errno::NotFound` / `Errno::NotImplemented` returns with the
    /// kernel-side deferral. See `AGENTS.md` §15.1 — the spec is
    /// stable, the impl is announced as inert.
    SyscallFeatureUnavailable,
    /// A syscall fired on a CPU with no identifiable caller.
    ///
    /// Emitted by `KernelDispatchHook` (Stage 2.7 follow-up (f4))
    /// when either `Scheduler::current_task` returns `None` for the
    /// issuing CPU or the per-task capability registry has no record
    /// for the running task. Both conditions are "should be impossible
    /// once the scheduler is live", but `AGENTS.md` §5.4.5 mandates
    /// fail-closed behaviour anyway: the audit record names the
    /// failing case (`cause` field) and the bin-crate dispatch
    /// callback halts the CPU exactly as the pre-(f5)
    /// `fail_closed_dispatch` did.
    SyscallNoCallerContext,
    /// A process was spawned: its image was built and the calling CPU is
    /// about to enter it in user mode.
    ///
    /// Emitted by the capability-checked spawn caller
    /// ([`crate::spawn::spawn_and_enter`]) after the user address space
    /// has been materialised and immediately before the Arch HAL
    /// `enter_user` transition (which never returns). The record carries
    /// the relocated entry-point virtual address (`AGENTS.md` §5.4.4 —
    /// security-relevant decisions are audited).
    ProcessSpawned,
    /// A spawn was refused because the caller lacks `CAP_PROC_SPAWN`.
    ///
    /// Emitted by [`crate::spawn::spawn_and_enter`] before any state is
    /// touched: the capability check fails closed and no address space is
    /// built (`AGENTS.md` §4 — no ambient authority; §5.4 — capability
    /// checks before state touches).
    ProcessSpawnDenied,
    /// A spawn was authorised but building the process image failed.
    ///
    /// Emitted by [`crate::spawn::spawn_and_enter`] when
    /// [`rustos_kernel_mem::build_process_image`] returns an error (a
    /// malformed image, an out-of-range segment, or frame exhaustion).
    /// The partially built address space is discarded by the caller
    /// (`AGENTS.md` §2.9 — fail closed).
    ProcessSpawnFailed,
    /// The `/System/Security/Users` database was read off the mounted
    /// root volume and parsed (`crate::users`, `plans/PI.md` P11).
    UsersDbLoaded,
    /// The `/System/Security/Users` database could not be read, or
    /// failed its bounded fail-closed validation; no database is held
    /// and every login refuses (`AGENTS.md` §5.4).
    UsersDbRejected,
    /// The `/System/Drivers/` signed-driver store was enumerated for
    /// autoload candidates (`crate::driver_store`, `AGENTS.md` §18.3 /
    /// §18.6).
    ///
    /// Emitted by [`crate::driver_store::enumerate_driver_store`] once
    /// per scan with the count of bundle image paths found (`drivers`)
    /// and the count of entries refused fail-closed during the bounded
    /// walk (`skipped`). A missing store is not an error — it simply
    /// yields zero drivers (`AGENTS.md` §18.4).
    DriverStoreScanned,
    /// A keyboard driver delivered the **first** key edge to the
    /// input-focus arbiter (`crate::input_focus`, `plans/PI.md` P11 —
    /// the autoload-by-discovery witness).
    ///
    /// Emitted by the `key_inject` syscall handler the first time
    /// [`crate::input_focus::InputFocus::inject`] succeeds, gated by a
    /// one-shot latch ([`crate::input_focus::InputFocus::note_first_delivery`]),
    /// so it fires exactly once over the kernel's lifetime. It witnesses
    /// that an (autoloaded) input driver has come up and is delivering
    /// input; it carries **no** key content, count, or timing — a
    /// per-keystroke record would leak typed secrets and is forbidden
    /// (`AGENTS.md` §20 — no input-content/timing noise on the log; §23.1
    /// — secret hygiene).
    InputDelivered,
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
            Self::SyscallFeatureUnavailable => 4020,
            Self::SyscallNoCallerContext => 4021,
            Self::ProcessSpawned => 4030,
            Self::ProcessSpawnDenied => 4031,
            Self::ProcessSpawnFailed => 4032,
            Self::UsersDbLoaded => 4040,
            Self::UsersDbRejected => 4041,
            Self::DriverStoreScanned => 4042,
            Self::InputDelivered => 4050,
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
            Self::SyscallFeatureUnavailable => "syscall feature unavailable",
            Self::SyscallNoCallerContext => "syscall has no caller context",
            Self::ProcessSpawned => "process spawned",
            Self::ProcessSpawnDenied => "process spawn denied",
            Self::ProcessSpawnFailed => "process spawn failed",
            Self::UsersDbLoaded => "users database loaded",
            Self::UsersDbRejected => "users database rejected",
            Self::DriverStoreScanned => "driver store scanned",
            Self::InputDelivered => "first input delivered to focus arbiter",
        }
    }
}

/// Emit one audit record through a `Sink`.
pub(crate) fn emit(sink: &dyn Sink, level: Level, event: AuditEvent, fields: &[Field<'_>]) {
    log(
        sink,
        &Event {
            level,
            id: event.id(),
            message: event.message(),
            fields,
        },
    );
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
            AuditEvent::SyscallFeatureUnavailable,
            AuditEvent::SyscallNoCallerContext,
            AuditEvent::ProcessSpawned,
            AuditEvent::ProcessSpawnDenied,
            AuditEvent::ProcessSpawnFailed,
            AuditEvent::UsersDbLoaded,
            AuditEvent::UsersDbRejected,
            AuditEvent::DriverStoreScanned,
            AuditEvent::InputDelivered,
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
            AuditEvent::SyscallFeatureUnavailable.id().0,
            AuditEvent::SyscallNoCallerContext.id().0,
            AuditEvent::ProcessSpawned.id().0,
            AuditEvent::ProcessSpawnDenied.id().0,
            AuditEvent::ProcessSpawnFailed.id().0,
            AuditEvent::UsersDbLoaded.id().0,
            AuditEvent::UsersDbRejected.id().0,
            AuditEvent::DriverStoreScanned.id().0,
            AuditEvent::InputDelivered.id().0,
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "duplicate event id");
            }
        }
    }
}
