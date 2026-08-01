//! Stable audit-log event IDs for `kernel/core`.
//!
//! Every architecture-neutral init-time and panic-time decision taken
//! by the kernel entry crate emits exactly one structured log record
//! through [`tairix_log`]. The numeric identifiers are part of the
//! audit contract with external log consumers and
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
//! | 4021 | Error | `SYSCALL_NO_CALLER_CONTEXT` | audit | A syscall fired on a CPU with no current task, or whose current task has no capability record. The `KernelDispatchHook` emits this then signals the bin-crate callback to halt the CPU. |
//! | 4030 | Info  | `PROCESS_SPAWNED`             | audit  | A process was spawned: its image was built and the CPU is about to enter it in user mode. The `entry` field carries the relocated entry-point VA. |
//! | 4031 | Error | `PROCESS_SPAWN_DENIED` | audit | A spawn was refused because the caller does not hold `CAP_PROC_SPAWN`; no address space was built (fail closed). |
//! | 4032 | Error | `PROCESS_SPAWN_FAILED`        | audit  | A spawn was authorised but building the process image failed; the partially built address space is discarded. The `cause` field names the `SpawnError`. |
//! | 4036 | Info/Warn | `PROCESS_SIGNAL_CROSS_PRINCIPAL` | audit | The `signal` syscall's cross-principal authority decision, reached only once the target is not the caller's own child: allowed (`Info`) by same-uid or `CAP_PROC_CONTROL`, denied (`Warn`) otherwise. The `caller`, `pid`, `target`, `signal`, and `rule` fields name the decision. |
//! | 4037 | Info/Warn | `PROCESS_PRIORITY_CHANGE` | audit | A `sched_set_priority` decision that needed authority beyond the caller's own child: a cross-principal target (same-uid or `CAP_PROC_CONTROL`) or a raise toward `High` (always `CAP_PROC_CONTROL`). Allowed is `Info`, denied is `Warn`; the `caller`, `pid`, `target`, `priority`, `rule`, and `raise` fields carry the decision. An own-child lowering is the caller's standing grant and is not recorded here. |
//! | 4040 | Info  | `USERS_DB_LOADED`             | audit  | `/System/Security/Users` was read off the mounted root volume and parsed; the `records` field carries the account count. |
//! | 4041 | Error | `USERS_DB_REJECTED` | audit | The users database could not be read or failed validation; no `UsersDb` is held and every login refuses (fail closed). The `cause` field names the refusal. |
//! | 4042 | Info | `DRIVER_STORE_SCANNED` | audit | The `/System/Drivers/` signed-driver store was enumerated for autoload candidates. The `drivers` field carries the count of bundle image paths found; `skipped` the count of entries refused fail-closed during the walk. |
//! | 4045 | Info | `USER_ADMIN_APPLIED` | audit | A `CAP_USER_ADMIN` account-administration operation was validated, persisted, and made live. The `op`, `target`, and `caller_uid` fields name the operation, the affected account/group, and the kernel-attested caller. |
//! | 4046 | Warn | `USER_ADMIN_REJECTED` | audit | A `CAP_USER_ADMIN` account-administration operation was refused; nothing changed (fail closed). The `op`, `target`, `caller_uid`, and `errno` fields name the operation, the affected account/group, the caller, and the refusal. |
//! | 4050 | Info | `INPUT_DELIVERED` | audit | An input driver delivered the **first** record of its kind to the input-focus arbiter — `kind=key` for the first `key_inject` edge, `kind=pointer` for the first `pointer_inject` record. Emitted at most once per input kind over the kernel's lifetime, carries no event content or timing — it witnesses that an autoloaded driver of that class is live, never a per-event record. |
//! | 4051 | Info | `SEAT_SWITCHED` | audit | A `CAP_SEAT_ADMIN` `seat_switch` retargeted a seat's foreground text console. The `seat` and `console` fields name the seat and the new foreground. |
//! | 4052 | Warn | `SEAT_LEASE_REVOKED` | audit | A `CAP_SEAT_ADMIN` `seat_revoke` forcibly evicted a seat's lease holder. The `seat` and `evicted` fields name the seat and the evicted owner's task id. |
//! | 4100 | Info | `FS_NODE_MUTATED` | audit | A capability- and permission-checked filesystem mutation succeeded (`fs_mkdir`/`fs_unlink`/`fs_rename`/`fs_set_mode`/`fs_set_owner`). The `op`, `uid`, and `path` fields name the operation, the caller's kernel-attested uid, and the target; `to` carries a rename's destination, `mode` a chmod's new mode (octal), and `owner`/`group` a chown's new ids. Paths are bounded to the log field limit. |
//! | 4101 | Warn | `FS_MUTATION_DENIED` | audit | A filesystem mutation was refused by the secured VFS; nothing changed (fail closed). Carries the same `op`/`uid`/`path`(/`to`/`mode`/`owner`/`group`) fields as `FS_NODE_MUTATED` plus the refusal's `errno`. |
//! | 4130 | Warn | `VOLUME_DEGRADED`   | audit | A served volume's backing block device reported itself unhealthy while still serving I/O. Emitted once on the edge into `Degraded`; the `dev` field names the block-service endpoint. |
//! | 4131 | Warn | `VOLUME_RECOVERING` | audit | A served volume's backing block device stalled/reset and entered its bounded recovery grace window. Emitted once on the edge into `Recovering`; `dev` names the block-service endpoint. |
//! | 4132 | Info | `VOLUME_RECOVERED`  | audit | A degraded/recovering volume returned to healthy service (the disk came back). Emitted once on the edge back to `Available`; `dev` names the block-service endpoint. |
//!
//! "audit" events route through the `audit_sink` channel
//! (security-relevant decisions); "log" events
//! route through the diagnostic `log_sink` channel. Production
//! kernels typically wire both sinks to the same COM1 backend, so
//! both channels are visible on the boot console; QEMU integration
//! tests intercept the audit channel only.
//!
//! Adding a new event requires assigning the next free identifier in
//! this file and updating the table in
//! `docs/src/architecture/kernel.md`.

use tairix_log::{log, Event, EventId, Field, Level, Sink};

/// Audit event identifiers emitted by `kernel/core`.
///
/// The numeric values are part of the stable ABI between TAIRiX and
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
    /// kernel-side deferral. See — the spec is
    /// stable, the impl is announced as inert.
    SyscallFeatureUnavailable,
    /// A syscall fired on a CPU with no identifiable caller.
    ///
    /// Emitted by `KernelDispatchHook` (Stage 2.7 follow-up (f4))
    /// when either `Scheduler::current_task` returns `None` for the
    /// issuing CPU or the per-task capability registry has no record
    /// for the running task. Both conditions are "should be impossible
    /// once the scheduler is live", but the charter mandates
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
    /// the relocated entry-point virtual address (security-relevant decisions are audited).
    ProcessSpawned,
    /// A spawn was refused because the caller lacks `CAP_PROC_SPAWN`.
    ///
    /// Emitted by [`crate::spawn::spawn_and_enter`] before any state is
    /// touched: the capability check fails closed and no address space is
    /// built (no ambient authority; — capability
    /// checks before state touches).
    ProcessSpawnDenied,
    /// A spawn was authorised but building the process image failed.
    ///
    /// Emitted by [`crate::spawn::spawn_and_enter`] when
    /// [`tairix_kernel_mem::build_process_image`] returns an error (a
    /// malformed image, an out-of-range segment, or frame exhaustion).
    /// The partially built address space is discarded by the caller
    /// (fail closed).
    ProcessSpawnFailed,
    /// A loaded driver process was torn down: its scheduler task was
    /// reaped and its grants, served endpoints, IRQ bindings, capability
    /// record, and address space reclaimed.
    ///
    /// Emitted by [`crate::spawn::InitSpawnCtx::terminate_driver_process`]
    /// when the device manager unloads a driver whose hardware-tree node
    /// vanished. The record carries the torn-down driver `handle`. Tearing
    /// an already-gone handle down is a benign
    /// [`tairix_abi::Errno::NotFound`] and emits no record (idempotent).
    DriverUnloaded,
    /// A task was killed by an unresolvable user-mode fault: the fault
    /// resolver could not back the access, so the task's crash exit
    /// (`128 + SIGSEGV`) was recorded and its resources reclaimed — the
    /// task dies, never the machine.
    ///
    /// Emitted by the fault-kill path
    /// (`KernelSyscallHandlers::record_fault_exit`, a crate-private
    /// method) so a crashing program is visible on the system log, not
    /// only via its `wait` status. The record carries a debuggable
    /// post-mortem while never leaking address-space layout:
    ///
    /// * `task` — the reusable scheduler id.
    /// * `name` / `proc_id` — the faulting task's kernel-attested
    ///   executable basename and CSPRNG-minted process-instance identity
    ///   (never caller-supplied), so a crash names *which* program and
    ///   stays correlatable after the task id is recycled.
    /// * `write` — whether the fatal access was a store (`true`) or a load
    ///   (`false`).
    /// * `fault_class` — a coarse class of *why* the resolver refused the
    ///   access (`stack_limit` — stack growth refused because the task's
    ///   `StackBytes` soft bound is exhausted; `stack` — growth room the
    ///   resolver could not back, e.g. frame exhaustion; `file_region` — a
    ///   miss inside a live file mapping the resolver refused, e.g. past
    ///   end-of-file; `anon` — a miss inside a reserved anonymous region;
    ///   `wild` — an address outside every mapping).
    /// * `fault_offset` — a coarse, non-leaking locality bucket
    ///   (`null_page`, `below_stack_guard`, `region`, or `wild`); when it
    ///   carries a distance, `region_offset` holds that value — a
    ///   *distance* from a fixed anchor (virtual address 0, the stack
    ///   guard, a region end), **never** an absolute virtual address.
    ///
    /// The raw faulting address is deliberately **never** on the record
    /// (diagnostics policy: no address-space layout leakage onto the
    /// shared, hash-chained log).
    TaskFaultKilled,
    /// A task ended itself with a **nonzero** exit status.
    ///
    /// Emitted by the `exit` syscall handler so an abnormal termination is
    /// visible on the system log even when no parent reaps the task — a
    /// failing autoloaded service would otherwise vanish silently (fail
    /// loud). The record carries the task id and the exit code; both are
    /// program state, never secrets. A clean (`0`) exit stays quiet.
    TaskExitedNonzero,
    /// The `signal` syscall's cross-principal authority decision.
    ///
    /// `signal` first tries the caller's own live children
    /// (`crate::procsignal::ProcessSignal::resolve_child`), which needs no
    /// capability. Only once that lookup reports the target is not a child
    /// does the handler decide whether the target belongs to the caller's
    /// own principal (allowed) or requires `CAP_PROC_CONTROL` (allowed if
    /// held, refused otherwise). Emitted exactly once per such decision,
    /// `Info` on either allow and `Warn` on a refusal, carrying the
    /// caller's task id, the target's pid and task id, the requested
    /// signal, and which rule decided it — never a capability token.
    ProcessSignalCrossPrincipal,
    /// A `sched_set_priority` authority decision that reached beyond the
    /// caller's own child: a cross-principal target or a raise toward
    /// `High` (`plans/NEW-TASKBAR.md` T12).
    ///
    /// Emitted once per such decision, allowed (`Info`) or denied
    /// (`Warn`). The record carries the kernel-attested caller's task id,
    /// the target's pid and task id, the requested level's wire
    /// discriminant, which rule decided the target, and whether the
    /// change was a raise — never a capability token. An own-child
    /// lowering is the parent's standing grant and stays unrecorded,
    /// exactly as own-child signal delivery does.
    ProcessPriorityChange,
    /// The `/System/Security/Users` database was read off the mounted
    /// root volume and parsed (`crate::users`, `plans/PI.md` P11).
    UsersDbLoaded,
    /// The `/System/Security/Users` database could not be read, or
    /// failed its bounded fail-closed validation; no database is held
    /// and every login refuses.
    UsersDbRejected,
    /// The `/System/Security/Groups` group registry was read off the
    /// mounted root volume and parsed (`crate::groups`).
    GroupsDbLoaded,
    /// The `/System/Security/Groups` registry could not be read, failed its
    /// bounded fail-closed validation, or did not resolve every group a
    /// user references; no identity table is installed and the filesystem
    /// path resolves no caller groups (fail closed).
    GroupsDbRejected,
    /// The `/System/Drivers/` signed-driver store was enumerated for
    /// autoload candidates (`crate::driver_store`).
    ///
    /// Emitted by [`crate::driver_store::enumerate_driver_store`] once
    /// per scan with the count of bundle image paths found (`drivers`)
    /// and the count of entries refused fail-closed during the bounded
    /// walk (`skipped`). A missing store is not an error — it simply
    /// yields zero drivers.
    DriverStoreScanned,
    /// A `CAP_USER_ADMIN` account-administration operation was applied:
    /// validated, persisted to the root volume, and made live for the
    /// next spawn/login (`crate::useradmin`, `plans/CAPABILITY_USE.md`
    /// CU4). Carries the operation, target, and attested caller uid —
    /// never any password material.
    UserAdminApplied,
    /// A `CAP_USER_ADMIN` account-administration operation was refused
    /// and nothing changed (fail closed). Carries the operation, target,
    /// attested caller uid, and the refusing errno.
    UserAdminRejected,
    /// An input driver delivered the **first** record of its kind to the
    /// seat registry (`crate::seat`, `plans/PI.md` P11 —
    /// the autoload-by-discovery witness).
    ///
    /// Emitted by the `key_inject` / `pointer_inject` syscall handlers the
    /// first time [`crate::seat::SeatRegistry::inject`] /
    /// [`crate::seat::SeatRegistry::inject_pointer`] succeeds for that
    /// input kind, gated by a per-kind one-shot latch
    /// ([`crate::seat::SeatRegistry::note_first_delivery`]), so it fires
    /// at most once per kind — twice over the kernel's lifetime — with a
    /// `kind` field (`key` / `pointer`) attributing which input class
    /// proved itself live. It carries **no** event content, count, or
    /// timing — a per-event record would leak typed secrets and is
    /// forbidden (no input-content/timing noise on the log;
    /// — secret hygiene).
    InputDelivered,
    /// A `CAP_SEAT_ADMIN` `seat_switch` retargeted a seat's foreground
    /// text console (`plans/DISPLAY.md` D3).
    ///
    /// Emitted by the `seat_switch` syscall handler after the validated
    /// retarget takes effect. Carries the seat id and the new foreground
    /// console index — the redirect of every subsequent keystroke of an
    /// unowned seat is a security-relevant ownership change.
    SeatSwitched,
    /// A `CAP_SEAT_ADMIN` `seat_revoke` forcibly evicted a seat's lease
    /// holder (`plans/DISPLAY.md` D3).
    ///
    /// Emitted by the `seat_revoke` syscall handler after the eviction.
    /// Carries the seat id and the evicted owner's task id, so every
    /// eviction is attributable; the evicted owner's next owner-gated call
    /// fails closed with the distinct `SeatRevoked` refusal.
    SeatLeaseRevoked,
    /// A display-class hardware-tree node was published into the live tree
    /// and the kernel minted an independent seat for it
    /// (`plans/DISPLAY.md` D6 — multi-seat / hotplug).
    ///
    /// Emitted by the `hw_emit_node` syscall handler after the validated
    /// publish. Carries the new seat id and the display node id — a new
    /// input-routing destination coming into existence is a
    /// security-relevant topology change.
    SeatCreated,
    /// A display-class hardware-tree node left the live tree and the
    /// kernel destroyed its seat (`plans/DISPLAY.md` D6).
    ///
    /// Emitted by the `hw_remove_node` syscall handler after the validated
    /// removal. Carries the destroyed seat id and the vanished node id;
    /// every lease and handle on the dead seat fails closed from this
    /// point on.
    SeatDestroyed,
    /// The kernel CSPRNG output reserve was seeded from the platform
    /// entropy source ([`tairix_arch_api::PlatformEntropy`]).
    ///
    /// Emitted once at boot by [`crate::init`] when the arch port's
    /// hardware entropy source produced enough bytes to seed the reserve.
    /// After this, `random_get` serves cryptographic output. The record
    /// carries no entropy — only that the decision was taken (a
    /// security-relevant state change).
    EntropyReserveSeeded,
    /// The kernel CSPRNG output reserve could **not** be seeded at boot
    /// and stays unseeded (`random_get` keeps failing closed with
    /// `EntropyNotReady`).
    ///
    /// Emitted once at boot by [`crate::init`] when the arch port exposes
    /// no usable entropy source, or its source could not produce bytes
    /// (the feature is absent, or every bounded draw was exhausted). The
    /// kernel never weakens to predictable output; it fails closed. The
    /// record carries a `cause` field naming why.
    EntropyReserveUnseeded,
    /// The kernel minted the per-boot identifier ([`tairix_abi::BootId`])
    /// from the seeded CSPRNG output reserve (`PREREQUISITES.md` P-E).
    ///
    /// Emitted once at boot by [`crate::init`] after the reserve is seeded.
    /// The record carries **neither** the entropy nor the id itself — only
    /// that the per-boot identity became available (a security-relevant state
    /// change; the id is a public nonce read through `boot_id_get`).
    BootIdMinted,
    /// The kernel could **not** mint the per-boot identifier: the CSPRNG
    /// output reserve was not seeded in time, so the boot id stays
    /// [`tairix_abi::BootId::UNSET`] and `boot_id_get` fails closed with
    /// `EntropyNotReady` (`PREREQUISITES.md` P-E).
    ///
    /// Emitted once at boot by [`crate::init`] on a port whose entropy source
    /// could not seed the reserve. The kernel never substitutes a predictable
    /// id; it fails closed.
    BootIdUnavailable,
    /// A secondary CPU was asked to start (the arch port accepted the
    /// bring-up request for it).
    ///
    /// Emitted by `kernel_main`'s SMP bring-up once per secondary after
    /// the arch port's `SecondaryBringup::start_secondary` returns `Ok`.
    /// Acceptance is not liveness: the started core attests its own
    /// arrival with [`Self::SecondaryCpuOnline`]. The record carries the
    /// dense `cpu` id.
    SecondaryCpuStarted,
    /// A secondary CPU could **not** be started; the system continues on
    /// the cores that are online (the scheduler's work stealing drains
    /// the missing core's queue), degraded but correct.
    ///
    /// Emitted by `kernel_main`'s SMP bring-up with the dense `cpu` id
    /// and a `cause` field naming the arch port's refusal. A refused
    /// core is never retried blindly (no retry-until-it-works); the
    /// failure is loud and attributable here.
    SecondaryCpuStartFailed,
    /// A started secondary CPU reached the kernel dispatch loop and is
    /// scheduling tasks — the liveness witness for the bring-up.
    ///
    /// Emitted once by the secondary CPU itself from
    /// [`crate::smp::run_secondary`], after its per-CPU hardware init and
    /// before its first dispatch, with its dense `cpu` id.
    SecondaryCpuOnline,
    /// A CPU stopped making scheduler progress for longer than the stall
    /// threshold — a soft lockup (`crate::watchdog`).
    ///
    /// Emitted from the port's timer-tick path the first time
    /// [`crate::watchdog::check_stall`] observes a CPU whose last dispatch
    /// heartbeat is older than
    /// [`crate::watchdog::DEFAULT_SOFT_LOCKUP_THRESHOLD_NS`], reported once
    /// per stall episode (never once per tick). The `cpu`
    /// field names the stalled CPU and `stalled_ms` how long it has gone
    /// without progress. Both are diagnostic state, never secrets.
    CpuStallDetected,
    /// A CPU that was reported stalled resumed making scheduler progress
    /// (`crate::watchdog`).
    ///
    /// Emitted from the dispatch loop by [`crate::watchdog::note_progress`]
    /// the first time a previously-reported stalled CPU dispatches again,
    /// so a cleared stall closes its own record rather than leaving a
    /// dangling "stuck" line. The `cpu` field names the recovered CPU and
    /// `stalled_ms` how long the episode lasted.
    CpuStallCleared,
    /// A CPU stopped taking even the non-maskable watchdog sample while it
    /// was running work — a **hard** lockup (`crate::watchdog`): wedged
    /// with interrupts masked, an interrupt storm, or a dead core.
    ///
    /// Emitted by another CPU's cross-CPU watchdog scan, once per episode.
    /// Carries the full diagnosis a post-mortem needs: the locked `cpu`,
    /// the `observer` CPU that caught it, how long it has been silent
    /// (`stalled_ms`), the last-known interrupted program counter (`pc`)
    /// and processor state (`pstate`), and the `task` that was running —
    /// the "what" and "why", never secrets.
    CpuHardLockupDetected,
    /// A CPU that was reported hard-locked resumed taking its non-maskable
    /// watchdog sample (`crate::watchdog`) — it recovered.
    ///
    /// Emitted by the recovered CPU's own watchdog sample the first time it
    /// runs again after a reported hard lockup, so the episode closes its
    /// own record. Carries `cpu` and the `stalled_ms` the episode lasted.
    CpuHardLockupCleared,
    /// The watchdog asked the architecture port to break a locked-up CPU
    /// out of its lockup (`crate::watchdog`), best-effort.
    ///
    /// Emitted alongside a detection, carrying the target `cpu`, the
    /// lockup `kind` (`soft`/`hard`), and the `outcome` the port reported
    /// (`rescheduled`/`attention`/`unrecoverable`/`unsupported`), so the
    /// recovery attempt and its result are on the audit trail.
    CpuLockupRecovery,
    /// The address-bearing developer detail of a lockup report — the
    /// sampled program counter (`pc`) and processor state (`pstate`), the
    /// self-published kernel-activity breadcrumb (`k_site`/`k_detail`/
    /// `k_seq`), and the pre-silence call-stack backtrace (`k_bt`).
    ///
    /// Emitted **only** by a `watchdog-diagnostics` (non-shippable `debug`
    /// image) build, alongside the always-on lockup summary
    /// ([`CpuHardLockupDetected`](Self::CpuHardLockupDetected) /
    /// [`CpuStallDetected`](Self::CpuStallDetected)) but through the
    /// **diagnostic** (log/UART) sink, never the persistent hash-chained
    /// audit trail — so the audit log carries no kernel address. Every
    /// kernel-address field is rendered image-base-relative (`pc=+0x…`),
    /// never the absolute runtime address, so it resolves against the
    /// debug kernel ELF without disclosing the runtime load base. A
    /// shippable image never emits it and compiles the whole facility out;
    /// the id is reserved so the catalogue stays stable
    /// (`plans/WATCHDOG.md`).
    CpuLockupDiagnostic,
    /// A bound IRQ line fired past its rate budget and was **quarantined**
    /// by the runaway-interrupt safety net (`kernel/irq`): the kernel kept
    /// it masked and stopped delivering it, so a never-quiesced or hostile
    /// source can no longer peg a CPU through the mask/wake/re-arm cycle.
    ///
    /// Emitted once, at the syscall boundary (task context) the instant a
    /// waiter observes the quarantine — `irq_wait` directly or
    /// `waitset_wait` for an IRQ member — rather than from the
    /// interrupt-context `fire` path. Carries the `line` (never a secret)
    /// and the owning `task`; the waiter's syscall then fails closed with
    /// `Errno::Io`. A fresh `irq_bind` clears the quarantine.
    IrqLineQuarantined,
    /// A capability- and permission-checked filesystem mutation succeeded.
    ///
    /// Emitted by the `fs_mkdir` / `fs_unlink` / `fs_rename` / `fs_set_mode`
    /// / `fs_set_owner` syscall handlers after the secured VFS applied the
    /// change under the caller's kernel-attested identity. The record names
    /// the operation (`op`), the caller's uid (`uid`), and the target path
    /// (`path`, plus `to` for a rename) — never a capability token or
    /// secret. Every mutation of on-disk state is a security-relevant
    /// decision, so it is auditable; the paths are bounded to the log
    /// field limit on a character boundary so the record can never fail to
    /// encode and drop, which would let a mutation escape the trail.
    FsNodeMutated,
    /// A filesystem mutation was refused; nothing changed (fail closed).
    ///
    /// The refusing counterpart of [`FsNodeMutated`](Self::FsNodeMutated),
    /// emitted by the same handlers when the secured VFS returns an error
    /// (a permission, capability, mount-flag, or validation refusal). The
    /// record carries the same `op`/`uid`/`path`(/`to`) fields plus the
    /// refusal's `errno`, so an attempt to mutate state the caller may not
    /// touch is as visible as a successful one.
    FsMutationDenied,
    /// The boot-time system configuration store
    /// (`/System/Settings/Configuration/system.conf`) was read off the
    /// unlocked root and applied (`crate::syscfg`).
    ///
    /// Emitted once at unlock after the operator's cache switches are
    /// applied to the live cache-admission control. The record names
    /// whether the store was present (`source`: `store` / `default`) and
    /// the effective mode of each cache class (`cache.filesystem`,
    /// `cache.block`, `cache.transform`, `cache.semantic`) — a
    /// security-relevant boot policy, never any secret.
    SystemConfigApplied,
    /// The boot-time system configuration store could not be read or
    /// parsed; the kernel fell back to the all-caches-enabled defaults
    /// (fail-safe: the caches are accelerators, never the source of
    /// truth) and applied them (`crate::syscfg`).
    ///
    /// Emitted once at unlock when the store is present but malformed
    /// (unreadable, not UTF-8, or outside the closed grammar). A simply
    /// *absent* store is not a rejection — it is the normal default case
    /// and emits [`SystemConfigApplied`](Self::SystemConfigApplied) with
    /// `source: default`. Carries a `cause` naming the refusal.
    SystemConfigRejected,
    /// A self-optimising accelerated-routine family selected its
    /// implementation for this boot (`crate::cpuops` / `lib/cpuops`).
    ///
    /// Emitted once per family after the common CPU-feature set is
    /// finalised: it records which implementation won (`chosen`) and why
    /// (`reason` — `priority`/`benchmark`/`baseline`/`pinned`), so the
    /// routine selection is observable and pinnable for reproducible-build
    /// validation. Operational, not a security decision, but recorded with a
    /// stable event id like every other boot-time choice.
    CpuOpsRoutineSelected,
    /// The boot-time cryptographic power-on self-test failed: the live
    /// SHA-256 path did not reproduce its published known-answer vectors
    /// (`tairix_crypto::backend`), so even the audited software baseline is
    /// computing wrong answers.
    ///
    /// Emitted once, immediately before the kernel halts. Running with a
    /// crypto core that fails its known-answer test is not a recoverable
    /// condition — every capability signature, manifest hash, and
    /// encrypted-swap tag would rest on broken cryptography — so this is a
    /// fatal, security-critical boot fault (the FIPS power-on-self-test
    /// discipline: a failed POST renders the module inoperable). Carries the
    /// `family` whose self-test failed.
    CryptoSelfTestFailed,
    /// A served volume's backing block device began reporting itself
    /// unhealthy while still serving I/O (a recovered-error threshold, a
    /// pending sector reallocation). The `dev` field names the block-service
    /// endpoint. Emitted once on the healthy/recovering -> degraded edge, not
    /// per request.
    VolumeDegraded,
    /// A served volume's backing block device stalled or reset and entered
    /// its bounded recovery grace window; its I/O is being ridden out
    /// reissuably while it is given a bounded chance to come back. The `dev`
    /// field names the block-service endpoint. Emitted once on entry to the
    /// recovering edge.
    VolumeRecovering,
    /// A degraded or recovering volume returned to healthy service — the
    /// disk came back. The `dev` field names the block-service endpoint.
    /// Logged as a recovery, not a fault, and emitted once on the return
    /// edge so a disk that comes back to life is noted in the health trail.
    VolumeRecovered,
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
            Self::DriverUnloaded => 4033,
            Self::TaskFaultKilled => 4034,
            Self::TaskExitedNonzero => 4035,
            Self::ProcessSignalCrossPrincipal => 4036,
            Self::ProcessPriorityChange => 4037,
            Self::UsersDbLoaded => 4040,
            Self::UsersDbRejected => 4041,
            Self::GroupsDbLoaded => 4043,
            Self::GroupsDbRejected => 4044,
            Self::DriverStoreScanned => 4042,
            Self::UserAdminApplied => 4045,
            Self::UserAdminRejected => 4046,
            Self::InputDelivered => 4050,
            Self::SeatSwitched => 4051,
            Self::SeatLeaseRevoked => 4052,
            Self::SeatCreated => 4053,
            Self::SeatDestroyed => 4054,
            Self::EntropyReserveSeeded => 4060,
            Self::EntropyReserveUnseeded => 4061,
            Self::BootIdMinted => 4062,
            Self::BootIdUnavailable => 4063,
            Self::SecondaryCpuStarted => 4070,
            Self::SecondaryCpuStartFailed => 4071,
            Self::SecondaryCpuOnline => 4072,
            Self::CpuStallDetected => 4080,
            Self::CpuStallCleared => 4081,
            Self::CpuHardLockupDetected => 4082,
            Self::CpuHardLockupCleared => 4083,
            Self::CpuLockupRecovery => 4084,
            Self::CpuLockupDiagnostic => 4085,
            Self::IrqLineQuarantined => 4090,
            Self::FsNodeMutated => 4100,
            Self::FsMutationDenied => 4101,
            Self::SystemConfigApplied => 4110,
            Self::SystemConfigRejected => 4111,
            Self::CpuOpsRoutineSelected => 4120,
            Self::CryptoSelfTestFailed => 4121,
            Self::VolumeDegraded => 4130,
            Self::VolumeRecovering => 4131,
            Self::VolumeRecovered => 4132,
        })
    }

    /// Short, fixed name used as the `message` field of the emitted
    /// [`tairix_log::Event`]. Kept under the 120-character convention
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
            Self::DriverUnloaded => "driver unloaded",
            Self::TaskFaultKilled => "task killed by unresolvable user fault",
            Self::TaskExitedNonzero => "task exited with nonzero status",
            Self::ProcessSignalCrossPrincipal => "process signal cross-principal decision",
            Self::ProcessPriorityChange => "process scheduling-priority change decision",
            Self::UsersDbLoaded => "users database loaded",
            Self::UsersDbRejected => "users database rejected",
            Self::GroupsDbLoaded => "groups database loaded",
            Self::GroupsDbRejected => "groups database rejected",
            Self::DriverStoreScanned => "driver store scanned",
            Self::UserAdminApplied => "user administration applied",
            Self::UserAdminRejected => "user administration rejected",
            Self::InputDelivered => "first input delivered to focus arbiter",
            Self::SeatSwitched => "seat foreground switched",
            Self::SeatLeaseRevoked => "seat lease revoked",
            Self::SeatCreated => "seat created for display node",
            Self::SeatDestroyed => "seat destroyed with display node",
            Self::EntropyReserveSeeded => "entropy reserve seeded",
            Self::EntropyReserveUnseeded => "entropy reserve unseeded",
            Self::BootIdMinted => "per-boot id minted",
            Self::BootIdUnavailable => "per-boot id unavailable",
            Self::SecondaryCpuStarted => "secondary cpu start requested",
            Self::SecondaryCpuStartFailed => "secondary cpu start failed",
            Self::SecondaryCpuOnline => "secondary cpu online",
            Self::CpuStallDetected => "cpu stall detected",
            Self::CpuStallCleared => "cpu stall cleared",
            Self::CpuHardLockupDetected => "cpu hard lockup detected",
            Self::CpuHardLockupCleared => "cpu hard lockup cleared",
            Self::CpuLockupRecovery => "cpu lockup recovery requested",
            Self::CpuLockupDiagnostic => "cpu lockup diagnostic detail",
            Self::IrqLineQuarantined => "irq line quarantined (runaway interrupt)",
            Self::FsNodeMutated => "filesystem node mutated",
            Self::FsMutationDenied => "filesystem mutation denied",
            Self::SystemConfigApplied => "system configuration applied",
            Self::SystemConfigRejected => "system configuration rejected",
            Self::CpuOpsRoutineSelected => "cpu accelerated routine selected",
            Self::CryptoSelfTestFailed => "crypto power-on self-test failed",
            Self::VolumeDegraded => "volume backing device degraded",
            Self::VolumeRecovering => "volume backing device recovering",
            Self::VolumeRecovered => "volume backing device recovered",
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
            AuditEvent::DriverUnloaded,
            AuditEvent::TaskFaultKilled,
            AuditEvent::TaskExitedNonzero,
            AuditEvent::ProcessSignalCrossPrincipal,
            AuditEvent::ProcessPriorityChange,
            AuditEvent::UsersDbLoaded,
            AuditEvent::UsersDbRejected,
            AuditEvent::GroupsDbLoaded,
            AuditEvent::GroupsDbRejected,
            AuditEvent::DriverStoreScanned,
            AuditEvent::UserAdminApplied,
            AuditEvent::UserAdminRejected,
            AuditEvent::InputDelivered,
            AuditEvent::SeatSwitched,
            AuditEvent::SeatLeaseRevoked,
            AuditEvent::SeatCreated,
            AuditEvent::SeatDestroyed,
            AuditEvent::EntropyReserveSeeded,
            AuditEvent::EntropyReserveUnseeded,
            AuditEvent::BootIdMinted,
            AuditEvent::BootIdUnavailable,
            AuditEvent::SecondaryCpuStarted,
            AuditEvent::SecondaryCpuStartFailed,
            AuditEvent::SecondaryCpuOnline,
            AuditEvent::CpuStallDetected,
            AuditEvent::CpuStallCleared,
            AuditEvent::CpuHardLockupDetected,
            AuditEvent::CpuHardLockupCleared,
            AuditEvent::CpuLockupRecovery,
            AuditEvent::CpuLockupDiagnostic,
            AuditEvent::IrqLineQuarantined,
            AuditEvent::FsNodeMutated,
            AuditEvent::FsMutationDenied,
            AuditEvent::SystemConfigApplied,
            AuditEvent::SystemConfigRejected,
            AuditEvent::CpuOpsRoutineSelected,
            AuditEvent::CryptoSelfTestFailed,
            AuditEvent::VolumeDegraded,
            AuditEvent::VolumeRecovering,
            AuditEvent::VolumeRecovered,
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
            AuditEvent::DriverUnloaded.id().0,
            AuditEvent::TaskFaultKilled.id().0,
            AuditEvent::TaskExitedNonzero.id().0,
            AuditEvent::ProcessSignalCrossPrincipal.id().0,
            AuditEvent::ProcessPriorityChange.id().0,
            AuditEvent::UsersDbLoaded.id().0,
            AuditEvent::UsersDbRejected.id().0,
            AuditEvent::GroupsDbLoaded.id().0,
            AuditEvent::GroupsDbRejected.id().0,
            AuditEvent::DriverStoreScanned.id().0,
            AuditEvent::UserAdminApplied.id().0,
            AuditEvent::UserAdminRejected.id().0,
            AuditEvent::InputDelivered.id().0,
            AuditEvent::SeatSwitched.id().0,
            AuditEvent::SeatLeaseRevoked.id().0,
            AuditEvent::SeatCreated.id().0,
            AuditEvent::SeatDestroyed.id().0,
            AuditEvent::EntropyReserveSeeded.id().0,
            AuditEvent::EntropyReserveUnseeded.id().0,
            AuditEvent::BootIdMinted.id().0,
            AuditEvent::BootIdUnavailable.id().0,
            AuditEvent::SecondaryCpuStarted.id().0,
            AuditEvent::SecondaryCpuStartFailed.id().0,
            AuditEvent::SecondaryCpuOnline.id().0,
            AuditEvent::CpuStallDetected.id().0,
            AuditEvent::CpuStallCleared.id().0,
            AuditEvent::CpuHardLockupDetected.id().0,
            AuditEvent::CpuHardLockupCleared.id().0,
            AuditEvent::CpuLockupRecovery.id().0,
            AuditEvent::CpuLockupDiagnostic.id().0,
            AuditEvent::IrqLineQuarantined.id().0,
            AuditEvent::FsNodeMutated.id().0,
            AuditEvent::FsMutationDenied.id().0,
            AuditEvent::SystemConfigApplied.id().0,
            AuditEvent::SystemConfigRejected.id().0,
            AuditEvent::CpuOpsRoutineSelected.id().0,
            AuditEvent::CryptoSelfTestFailed.id().0,
            AuditEvent::VolumeDegraded.id().0,
            AuditEvent::VolumeRecovering.id().0,
            AuditEvent::VolumeRecovered.id().0,
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "duplicate event id");
            }
        }
    }
}
