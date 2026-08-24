//! Architecture-neutral kernel entry and init sequencing.
//!
//! The contract documented in `docs/src/architecture/kernel.md` is:
//!
//! 1. [`kernel_main`] is the *only* public entry point the
//!    architecture port calls after it has built a [`BootInfo`].
//! 2. Subsystems initialise in a fixed order — `log → mem → sec →
//!    sched → ipc` — and that order is part of the kernel ABI:
//!    re-ordering would change which audit events external consumers
//!    observe (no interface creep).
//! 3. A failure in any phase is **fatal**: the failed-phase event is
//!    logged and the boot CPU enters [`KernelArch::halt`]. The kernel
//!    never silently resets (Stage 2 deliverables).
//!
//! Stage 2.7 will extend [`kernel_main`] with a syscall-registration
//! phase and replace the trailing `arch.halt()` with the dispatch into
//! the scheduler hot loop. Until then, halting after `BootCompleted`
//! is the documented contract.
//!
//! # Phase numbering
//!
//! The [`Phase`] enum is the single source of truth: the audit fields
//! emitted by every phase event carry [`Phase::as_str`], and tests
//! assert against those strings rather than line-counting log records.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sched::{CpuId, SchedError, Scheduler, SchedulerArch};
use tairix_abi::hwtree::HwResource;
use tairix_abi::{BootFacts, DescriptorTable, Errno};
use tairix_caps::CapabilitySet;
use tairix_kernel_ipc::PortRegistry;
use tairix_kernel_irq::{IrqController, IrqTable, MonotonicClock};
use tairix_kernel_mem::{AllocError, FrameAllocator, PhysMap, UserAddressSpace};
use tairix_kernel_sched_api::{ExitDisposition, Priority, StepOutcome};
use tairix_kernel_sec::{
    CapTable, ProcName, ProcessId as SecProcessId, TaskCapabilities, TaskId as SecTaskId, UserId,
};
use tairix_log::{set_max_level, Field, Level, Sink};
use tairix_sync::RwLock;
use tairix_util::fmt::format_hex_u64;

use crate::aspace::AddressSpaceRegistry;
use crate::audit::{emit, AuditEvent};
use crate::bootinfo::{BootInfo, BootInfoError, IrqRouting, KernelArch};
use crate::dispatch_slot::AlreadyInstalledError;
use crate::procwait::{KernelProcessWait, ProcessWait};
use crate::random::{BootReserve, RandomReserve};
use crate::rlimit::{default_pinned_limit_bytes, LimitSet};
use crate::spawn::InitSpawnCtx;
use crate::syscalls::{KernelDispatchHook, KernelSpawnCtx, SpawnCredential};

/// Ordered identifier of every subsystem init phase orchestrated by
/// [`kernel_main`].
///
/// The numeric ordering is meaningful: phase `N` must complete before
/// phase `N+1` begins, and the order is the audit-log contract with
/// external consumers.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Phase {
    /// Install the global log sink and level filter.
    Log,
    /// Construct the physical [`FrameAllocator`] from the boot memory
    /// map.
    Mem,
    /// Build, verify, and install the compiled-in system identity table.
    Sec,
    /// Build the SMP scheduler.
    Sched,
    /// Consult the architecture port's [`crate::KernelArch::irq_routing`]
    /// and construct the kernel-wide [`tairix_kernel_irq::IrqTable`].
    ///
    /// Phase 4.D Item 2-tail.2 — inserted between [`Phase::Sched`] and
    /// [`Phase::Syscall`] so the IRQ table is wired with a
    /// realistic `max_line` and the production controller is in
    /// place before any syscall can race the deferral path.
    Irq,
    /// Register the production syscall dispatcher.
    ///
    /// Stage 2.7 follow-up (f4). Between `Sched` and `Ipc`,
    /// [`kernel_main`] publishes a [`crate::DispatchHook`] (built
    /// from `KernelState`'s scheduler, capability table, arch port,
    /// and audit sink) into the bin-crate-owned
    /// [`crate::DispatchCallbackSlot`]. The arch-level
    /// `set_dispatch_callback` is still invoked before `syscall` is
    /// enabled — this phase is the *kernel-side* publication point,
    /// not the trampoline.
    Syscall,
    /// Prepare the IPC subsystem (currently a no-op — `kernel/ipc`
    /// holds no global state; the phase event still fires so external
    /// log consumers can rely on a consistent boot timeline).
    Ipc,
}

impl Phase {
    /// Iteration order used by [`kernel_main`].
    pub const ORDER: [Phase; 7] = [
        Phase::Log,
        Phase::Mem,
        Phase::Sec,
        Phase::Sched,
        Phase::Irq,
        Phase::Syscall,
        Phase::Ipc,
    ];

    /// Short, fixed name suitable for inclusion as the `phase` field of
    /// an [`AuditEvent::PhaseStarted`] / [`AuditEvent::PhaseReady`] /
    /// [`AuditEvent::PhaseFailed`] record.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Log => "log",
            Phase::Mem => "mem",
            Phase::Sec => "sec",
            Phase::Sched => "sched",
            Phase::Irq => "irq",
            Phase::Syscall => "syscall",
            Phase::Ipc => "ipc",
        }
    }
}

/// Reason a phase failed.
///
/// The variants are intentionally typed (not opaque error codes) so
/// the panic handler and the audit-log writer can emit a stable
/// machine-readable `cause` field — external dashboards key off this
/// to escalate.
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
    /// [`BootInfo::validate`] rejected the handover.
    BadBootInfo(BootInfoError),
    /// `kernel/mem` rejected the memory map or could not initialise.
    Mem(AllocError),
    /// The compiled-in system identity table was rejected by the
    /// `kernel/sec` verifier or could not be installed into the boot
    /// path's identity cell.
    Sec(tairix_abi::Errno),
    /// `kernel/sched` rejected the scheduler configuration.
    Sched(SchedError),
    /// The scheduler reported zero CPUs while installing per-CPU state.
    CpuStateZeroCpus,
    /// Per-CPU state allocation failed during scheduler initialization.
    CpuStateAllocationFailed,
    /// Per-CPU state was already installed during scheduler initialization.
    CpuStateAlreadyInstalled,
    /// The bin-crate [`crate::DispatchCallbackSlot`] already held a
    /// hook when the `Syscall` phase attempted to publish ours.
    ///
    /// The slot is set-once per boot; a second publish indicates a
    /// programmer error (e.g. a test harness pre-installed a hook,
    /// or `kernel_main` was re-entered). — fail
    /// closed: report and halt, no silent recovery.
    DispatcherAlreadyInstalled(AlreadyInstalledError),
}

impl InitError {
    /// Phase the error belongs to.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        match self {
            // BadBootInfo precedes phase entry — it is reported under
            // the first phase (`log`) because the validator runs as
            // the kernel attempts to install the log filter.
            InitError::BadBootInfo(_) => Phase::Log,
            InitError::Mem(_) => Phase::Mem,
            InitError::Sec(_) => Phase::Sec,
            InitError::Sched(_)
            | InitError::CpuStateZeroCpus
            | InitError::CpuStateAllocationFailed
            | InitError::CpuStateAlreadyInstalled => Phase::Sched,
            InitError::DispatcherAlreadyInstalled(_) => Phase::Syscall,
        }
    }

    /// Short, fixed name for the `cause` audit field.
    #[must_use]
    pub const fn cause(&self) -> &'static str {
        match self {
            InitError::BadBootInfo(e) => e.as_str(),
            InitError::Mem(AllocError::OutOfMemory) => "mem_out_of_memory",
            InitError::Mem(AllocError::InvariantViolation) => "mem_invariant",
            InitError::Mem(AllocError::SizeUnsupported) => "mem_size_unsupported",
            InitError::Mem(AllocError::ZeroSize) => "mem_zero_size",
            InitError::Mem(AllocError::OutOfRange) => "mem_out_of_range",
            InitError::Mem(AllocError::MetadataAllocFailed) => "mem_metadata_alloc_failed",
            InitError::Mem(_) => "mem_unknown",
            InitError::Sec(_) => "sec_identity_rejected",
            InitError::Sched(_) => "sched_construction_failed",
            InitError::CpuStateZeroCpus => "sched_cpu_state_zero_cpus",
            InitError::CpuStateAllocationFailed => "sched_cpu_state_allocation_failed",
            InitError::CpuStateAlreadyInstalled => "sched_cpu_state_already_installed",
            InitError::DispatcherAlreadyInstalled(_) => "syscall_dispatcher_already_installed",
        }
    }
}

/// Architecture-neutral kernel entry point.
///
/// Called by the Stage 3 arch crate immediately after it has built a
/// [`BootInfo`] from the platform's boot protocol. Drives the
/// documented init order, then either:
///
/// * **Succeeds**: emits [`AuditEvent::BootCompleted`] and parks the
///   boot CPU via [`KernelArch::halt`]. Stage 2.7 will replace the
///   trailing halt with the scheduler dispatch loop; until then,
///   halting is the contract.
/// * **Fails**: emits [`AuditEvent::PhaseFailed`] with the offending
///   `phase` and `cause`, then parks the boot CPU via
///   [`KernelArch::halt`]. *Never* silently resets — Stage 2 deliverables.
///
/// # SAFETY-INVARIANTs verified at entry
///
/// * `boot.validate()` is invoked before any other work — every field
///   invariant documented on [`BootInfo`] is checked.
/// * `boot.boot_cpu == boot.arch.current_cpu()` — re-asserted as a
///   `debug_assert_eq!` so production builds pay no extra cost while
///   debug builds catch arch porting bugs that would route IPIs to the
///   wrong CPU.
#[allow(clippy::needless_pass_by_value)] // BootInfo is consumed by design.
pub fn kernel_main<A: KernelArch>(boot: BootInfo<'_, A>) -> ! {
    // Phase 0 — install the log filter immediately. Until the filter
    // is in place, log records are routed at the default `Info` level.
    set_max_level(boot.log_level);

    // Capture the references we use later before destructuring `boot`
    // into the phases (each phase consumes its piece of the handover).
    let log_sink: &'static (dyn Sink + Sync) = boot.log_sink;
    let audit_sink: &'static (dyn Sink + Sync) = boot.audit_sink;
    let arch_for_halt = Arc::clone(&boot.arch);
    // The CPU topology the SMP bring-up below drives, captured before
    // `boot` is consumed by `run_phases` (both fields are `Copy`).
    let boot_cpu = boot.boot_cpu;
    let cpu_count = boot.cpu_count;
    // The arch port's PID-1 spawn seam (`plans/PI.md` P6c-3), captured
    // before `boot` is consumed by `run_phases`. `Option<&dyn _>` is
    // `Copy`, so this is a copy of the reference, not a move.
    let init_spawn = boot.init;

    // Fold the boot CPU's own detected CPU-feature set into the migration-safe
    // common set delivered to every spawned process, and declare how many
    // cores will contribute (each secondary folds its own set in
    // `run_secondary`, since a core can only read its own ID registers). A
    // port without the CPU-feature HAL slice contributes nothing, so the
    // delivered set stays empty and every process resolves its accelerated
    // routines to the portable baseline (fail closed, never a trap).
    if let Some(cpu_features) = boot.arch.cpu_features() {
        crate::cpuops::expect_contributions(cpu_count);
        crate::cpuops::contribute(cpu_features.detect(boot_cpu));
    }

    // `BootStarted` / `BootCompleted` / `PhaseFailed` are audit
    // lifecycle events (security-relevant
    // decisions). They route through `audit_sink`. `PhaseStarted` /
    // `PhaseReady` remain on `log_sink` as diagnostic timeline
    // markers. Production wires both sinks to the same backend; the
    // QEMU integration test bin intercepts `audit_sink` only.
    emit(
        audit_sink,
        Level::Info,
        AuditEvent::BootStarted,
        &[Field {
            key: "phase_count",
            value: tairix_log::FieldValue::Str("7"),
        }],
    );

    // SAFETY-INVARIANT: `boot_cpu` must equal `arch.current_cpu()` at
    // entry. We re-assert in debug builds *after* `boot.validate()`
    // has rejected obviously-malformed handovers — that way an
    // out-of-range `boot_cpu` is reported as a structured
    // `BootInfoError::BootCpuOutOfRange` audit record rather than as
    // a debug-mode assertion (fail closed
    // with a stable cause string).
    if boot.validate().is_ok() {
        debug_assert_eq!(
            boot.boot_cpu,
            boot.arch.current_cpu(),
            "BootInfo.boot_cpu disagrees with KernelArch::current_cpu()",
        );
    }

    // Per-phase orchestration. On failure we halt via the arch port.
    // `run_phases` also hands back the leaked-`'static` process-wait
    // producer so the PID-1 / driver spawn context can record a spawned
    // task's parent/child link against the same producer the `wait`
    // syscall drives (`plans/SPAWN.md` SP6).
    let (state, process_wait) = match run_phases(boot, log_sink, audit_sink) {
        Ok(booted) => booted,
        Err(err) => {
            let phase = err.phase();
            let cause = err.cause();
            emit(
                audit_sink,
                Level::Error,
                AuditEvent::PhaseFailed,
                &[
                    Field {
                        key: "phase",
                        value: tairix_log::FieldValue::Str(phase.as_str()),
                    },
                    Field {
                        key: "cause",
                        value: tairix_log::FieldValue::Str(cause),
                    },
                ],
            );
            arch_for_halt.halt();
        }
    };

    emit(
        audit_sink,
        Level::Info,
        AuditEvent::BootCompleted,
        &[Field {
            key: "next",
            value: tairix_log::FieldValue::Str("spawn_init"),
        }],
    );

    // Install the port's live-core-frequency source (the Arch HAL `coreclock`
    // slice) and enable it on the boot CPU; each secondary enables it on
    // itself as it comes up. The per-CPU estimator then samples the
    // core/reference counter pair at every preemption tick and reports the
    // live "cpu MHz" through the System Information API. A port without the
    // slice installs nothing, so the estimator reports no frequency and
    // readers fall back to the discovered nominal figure (fail closed). The
    // handle is read from the leaked-`'static` kernel state, so it outlives
    // the estimator's set-once install.
    if let Some(core_clock) = state.arch.core_clock() {
        crate::cpufreq::install(core_clock);
    }

    // Bring the remaining discovered CPUs online now that every init
    // phase has succeeded: the scheduler, IRQ dispatch, and syscall hook
    // are live, so a started core can immediately join the shared
    // dispatch loop. Each secondary performs its own per-CPU hardware
    // init in the arch port's entry and arrives through
    // `crate::run_secondary`; until PID 1 spawns work it parks on its
    // idle instruction, woken by the scheduler's placement IPI.
    start_secondaries(state, boot_cpu, cpu_count);

    // Every core has now folded its CPU-feature set into the migration-safe
    // common set (`crate::cpuops`), so it is finalised: select the
    // self-optimising accelerated-routine implementations against it (CRC-32C
    // for the in-kernel ARXFS physical-integrity checksum, and the crypto
    // SHA-256 backend-availability decision) and audit each choice. A core
    // without the feature HAL slice leaves the set empty, so the portable
    // baseline is chosen everywhere (fail closed). The crypto family's
    // self-verify is a boot-time known-answer self-test: if the audited
    // primitive fails it (broken cryptography), the kernel must not proceed —
    // it has already emitted the fatal audit record, so halt now.
    if !crate::cpuops::resolve_accelerated_ops(audit_sink) {
        arch_for_halt.halt();
    }

    // Spawn PID 1 (`init`) into user mode when the arch port installed a
    // spawn seam (`plans/PI.md` P6c-3). On success the seam diverges into
    // the spawned program and never returns; on failure (or when no seam
    // is installed) we fall through to the fail-closed halt below
    // (never silently reset).
    if let Some(init) = init_spawn {
        // The core-side registration context the seam drives: it builds
        // the arch image (through the public `spawn_image` caller) and
        // hands it back through `admit_init`, which registers the task with
        // this kernel state's scheduler / capability table / address-space
        // registry and dispatches it. Every borrow targets the leaked
        // `KernelState`, which lives for the running kernel's lifetime.
        //
        // The context is leaked to `'static` (a one-shot boot publish over
        // the already-leaked `KernelState`, never a mutable global ) and handed to the seam as a
        // `&'static (dyn InitSpawnCtx + Sync)`. That lets an in-kernel
        // service the seam admits *before* `admit_init` diverges into the
        // dispatch loop — the aarch64 root-unlock kthread, whose `'static +
        // Send` body outlives this frame — capture the context and later
        // drive `spawn_driver_process` to autoload user-space drivers off the
        // mounted root (`plans/PI.md` P11;). On the failure
        // path `spawn_init` returns and we halt below, so the leak is
        // immaterial; on success it diverges and the context lives for the
        // running kernel's lifetime, exactly like the state it borrows.
        let ctx: &'static (dyn InitSpawnCtx + Sync) = Box::leak(Box::new(KernelInitSpawner::new(
            state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            build_shared_mem_facility(state.arch.as_ref(), state.frame_allocator),
        )));
        init.spawn_init(ctx);
    }

    arch_for_halt.halt();
}

/// The boot-leaked [`crate::WaitQueueArch`] adapter (Design D P-2).
///
/// Bridges the global blocking wait-queue (`crate::waitq`) to the live,
/// generic `Scheduler<A>` and arch port without the wait-queue naming
/// either concrete type: an explicit or timed
/// wake `unpark`s through the scheduler (whose wake-pending token closes
/// the lost-wakeup race), the timed sweep reads `monotonic_ns`, and
/// the nearest-deadline one-shot is armed through the arch port's
/// `set_wakeup`. Holds only `'static` borrows into the leaked
/// `KernelState`, so it is itself leaked and installed once at boot.
struct SchedWaitQueueArch<A: KernelArch + 'static> {
    scheduler: &'static Scheduler<A>,
    arch: &'static A,
}

impl<A: KernelArch + 'static> crate::waitq::WaitQueueArch for SchedWaitQueueArch<A> {
    fn unpark(&self, id: tairix_kernel_sched_api::TaskId) {
        // Cancellation-safe: `unpark` of a not-yet-parked task records a
        // wake-pending token rather than erroring, so a wake racing the
        // park is never lost. A vanished task is a
        // benign no-op for a wake.
        let _ = self.scheduler.unpark(id);
    }

    fn now_ns(&self) -> u64 {
        self.arch
            .monotonic_ns(SchedulerArch::current_cpu(self.arch))
    }

    fn set_wakeup(&self, deadline_ns: Option<u64>) {
        SchedulerArch::set_wakeup(self.arch, deadline_ns);
    }

    fn current_task(
        &self,
        cpu: tairix_kernel_sched_api::CpuId,
    ) -> Option<tairix_kernel_sched_api::TaskId> {
        // The live scheduler's per-CPU current-task slot — the same slot the
        // dispatch hook reads to identify a syscall caller. A console-read backing parks the *current* task without
        // being handed its id, so it resolves it here.
        self.scheduler.current_task(cpu)
    }

    fn current_cpu(&self) -> Option<tairix_kernel_sched_api::CpuId> {
        // The arch port's per-CPU identity — the same value the scheduler
        // and timed-wake paths read. A blocking primitive reached without a
        // caller context (a `SleepLock` contended acquire) resolves the
        // current CPU here to then look up and park the current task.
        Some(SchedulerArch::current_cpu(self.arch))
    }
}

impl<A: KernelArch + 'static> MonotonicClock for SchedWaitQueueArch<A> {
    fn now_ns(&self) -> u64 {
        // The runaway-interrupt safety net reads the same arch monotonic
        // clock the timed-wake sweep does, at the CPU currently running the
        // interrupt-context `IrqTable::fire`. Cross-CPU skew is immaterial
        // to a coarse rate budget of 100 000 fires/second.
        self.arch
            .monotonic_ns(SchedulerArch::current_cpu(self.arch))
    }
}

impl crate::watchdog::StuckOwnerResolver for IrqTable {
    fn owner_of_line(&self, line: u32) -> Option<u64> {
        // The inherent `IrqTable::owner_of_line` returns the owning
        // `TaskId`; expose only its raw id to the arch-neutral watchdog so
        // a hard-lockup report can attribute a stuck line to its driver.
        IrqTable::owner_of_line(self, line).map(|task| task.0)
    }
}

impl<A: KernelArch + 'static> crate::preempt::PreemptCompetitor for SchedWaitQueueArch<A> {
    fn has_runnable_competitor(&self, cpu: CpuId) -> bool {
        // A competitor is a task queued runnable on `cpu` other than the
        // one it is currently running (the run-queue excludes the current
        // task). The preempt path uses this so a fired quantum tick
        // reschedules only when a switch would change what runs — a lone
        // runnable task is left in place. An out-of-range CPU or a
        // transient query error is treated as "no competitor" (fail
        // closed: never a spurious preemption).
        matches!(self.scheduler.queue_depth(cpu), Ok(depth) if depth > 0)
    }

    fn keep_periodic_tick(&self, _cpu: CpuId) {
        // Delegate to the live policy: the non-tickless CFQ policy re-arms
        // this CPU's quantum so its fixed-HZ tick keeps firing for a lone
        // running task (and it re-checks its run queue each tick, picking
        // up work later enqueued here without an IPI); the tickless
        // siblings (EEVDF, MLFQ) no-op, so a quiet core keeps taking no
        // ticks. The re-arm targets the calling CPU — the one whose tick
        // just fired, which is where the preempt path runs this.
        self.scheduler.rearm_periodic_tick();
    }
}

/// Leak a [`SchedWaitQueueArch`] over the boot-leaked `KernelState` and
/// install it as the global wait-queue hook (Design D P-2), so the
/// explicit-wake (`crate::hw_tree_wake`) and timed-wake
/// (`crate::timed_wake_sweep`) paths reach the live scheduler + arch
/// without the wait-queue naming either concrete type. Set-once per boot; a stray re-install is a benign skip (this is
/// the only caller). Factored out of `run_phases` to
/// keep that function within its line budget.
fn publish_wait_queue_arch<A: KernelArch + 'static>(state: &'static KernelState<A>) {
    let wait_arch: &'static SchedWaitQueueArch<A> = Box::leak(Box::new(SchedWaitQueueArch {
        scheduler: &state.scheduler,
        arch: state.arch.as_ref(),
    }));
    let _ = crate::waitq::install_wait_arch(wait_arch);
    // The same leaked adapter is the runaway-interrupt safety net's
    // monotonic clock: installing it lets `IrqTable::fire` rate-account
    // each line and quarantine a runaway one (a never-quiesced or hostile
    // source) instead of letting it peg a CPU. Set-once; a stray
    // re-install is a benign skip.
    let _ = state.irq.set_clock(wait_arch);
    // Give the CPU-lockup watchdog the live IRQ table so a hard-lockup
    // report can attribute the stuck controller line to the driver that
    // bound it (`stuck_owner=<task>`), or say `unbound` for a spurious /
    // contained line no driver owns. The boot-leaked `KernelState` outlives
    // the kernel, so `&state.irq` is `'static`. Set-once; a stray re-install
    // is a benign skip.
    crate::watchdog::install_irq_owner(&state.irq);
    // The same leaked adapter answers the preempt path's "is there a
    // runnable competitor on this CPU?" query, so a fired quantum tick
    // reschedules only when a switch would change what runs (the tick
    // still fires for a lone task — TAIRiX stays non-tickless under CFQ —
    // but does not needlessly switch away from it, avoiding the
    // per-quantum address-space/TLB churn that path would otherwise incur).
    crate::preempt::install_competitor_gate(wait_arch);
    // Size the futex's key→queue table from the discovered CPU count, so
    // threads contending on *distinct* locks do not serialise on one bucket
    // lock (`plans/THREADS.md` decision 5). Here in boot publication, before
    // any thread exists to wait: the table is fixed by its first use, so a
    // sizing that arrived after a key had resolved would be refused. The count
    // itself is a contention choice — a build that skipped this still blocks
    // and wakes exactly as specified, on one bucket.
    crate::futex::init_buckets(state.scheduler.cpu_count() as usize);
}

/// Type-erased secondary-CPU hand-off over the boot-leaked
/// [`KernelState`]: what [`crate::run_secondary`] needs to join a
/// started core to the live scheduler without naming the concrete
/// `Scheduler<A>` / arch pair (see [`crate::smp::SecondaryDispatch`]).
struct KernelSecondaryDispatch<A: KernelArch + 'static> {
    state: &'static KernelState<A>,
    boot_cpu: CpuId,
    cpu_count: u32,
    /// One `false`-initialised flag per dense CPU id (slot `i` is dense
    /// id `i`; the boot CPU's slot is never set). A started secondary
    /// sets its own slot from [`crate::smp::run_secondary`] once it has
    /// fully brought up and is about to join the dispatch loop; the boot
    /// CPU's bring-up barrier ([`wait_secondary_online`]) waits on it.
    online: &'static [AtomicBool],
}

impl<A: KernelArch + 'static> KernelSecondaryDispatch<A> {
    /// Whether secondary `cpu` has acknowledged it is fully online. An
    /// out-of-range id reads `false` (fail closed).
    fn is_online(&self, cpu: CpuId) -> bool {
        self.online
            .get(cpu as usize)
            .is_some_and(|slot| slot.load(Ordering::Acquire))
    }
}

impl<A: KernelArch + 'static> crate::smp::SecondaryDispatch for KernelSecondaryDispatch<A> {
    fn boot_cpu(&self) -> CpuId {
        self.boot_cpu
    }

    fn cpu_count(&self) -> u32 {
        self.cpu_count
    }

    fn audit_sink(&self) -> &'static (dyn Sink + Sync) {
        self.state.audit_sink
    }

    fn contribute_cpu_features(&self, cpu: CpuId) {
        if let Some(cpu_features) = self.state.arch.cpu_features() {
            crate::cpuops::contribute(cpu_features.detect(cpu));
        }
        // Runs on the secondary itself as it comes up (a core can only read
        // and arm its own counters), so this is where a per-CPU core-clock
        // counter (aarch64 `PMCCNTR_EL0`) is enabled on this core; a no-op on
        // a port with no core-clock slice or an already-global counter.
        crate::cpufreq::enable_this_cpu();
    }

    fn mark_online(&self, cpu: CpuId) {
        if let Some(slot) = self.online.get(cpu as usize) {
            slot.store(true, Ordering::Release);
        }
    }

    fn run(&self, cpu: CpuId) {
        run_dispatch_loop(
            &self.state.scheduler,
            self.state.arch.as_ref(),
            cpu,
            DispatchRole::Secondary,
        );
    }
}

/// Allocate the per-CPU liveness + quiesce-ack tables for `cpu_count` CPUs,
/// publish them to the cross-CPU quiesce coordinator, and return the leaked
/// liveness table.
///
/// Both tables carry one `AtomicBool` per dense CPU id, sized to the
/// discovered core count (never a fixed ceiling) and leaked `&'static` for the
/// kernel's lifetime. The liveness table is the one a started secondary marks
/// as it comes online and the bring-up barrier reads; the ack table backs the
/// destructive-takeover stop handshake, published alongside liveness so that
/// handshake reads real per-CPU state.
///
/// # Errors
///
/// Returns a stable cause string if the coordinator refuses the publish (the
/// tables are set-once per boot), so the caller fails closed rather than
/// running a later takeover against stale liveness.
fn init_liveness_and_quiesce_tables(cpu_count: u32) -> Result<&'static [AtomicBool], &'static str> {
    let table = || -> &'static [AtomicBool] {
        Box::leak(
            (0..cpu_count)
                .map(|_| AtomicBool::new(false))
                .collect::<alloc::vec::Vec<_>>()
                .into_boxed_slice(),
        )
    };
    let online = table();
    let quiesce_ack = table();
    match tairix_arch_api::quiesce_publish_tables(online, quiesce_ack) {
        Ok(()) => Ok(online),
        Err(tairix_arch_api::QuiescePublishError::AlreadyPublished) => {
            Err("quiesce_tables_already_published")
        }
        Err(tairix_arch_api::QuiescePublishError::LengthMismatch) => {
            Err("quiesce_tables_length_mismatch")
        }
    }
}

/// Bring every discovered secondary CPU online: publish the dispatch
/// hand-off, then ask the arch port to start each dense id in
/// `0..cpu_count` other than the boot CPU, auditing every acceptance
/// and refusal.
///
/// A refusal is degraded-but-correct, never fatal: the failure is loud
/// on the audit log (`SecondaryCpuStartFailed` with the port's cause)
/// and the system continues on the cores that are online — the
/// scheduler's placement and work stealing simply never see the missing
/// core run. A single-CPU handover (or a port with no bring-up surface
/// on a `cpu_count == 1` boot) starts nothing and publishes nothing.
fn start_secondaries<A: KernelArch + 'static>(
    state: &'static KernelState<A>,
    boot_cpu: CpuId,
    cpu_count: u32,
) {
    if cpu_count <= 1 {
        return;
    }
    let audit_sink = state.audit_sink;
    let Some(bringup) = state.arch.secondary_bringup() else {
        // A multi-CPU handover from a port with no bring-up surface is a
        // wiring defect: report it loudly rather than silently running
        // single-CPU with a scheduler sized for more.
        emit(
            audit_sink,
            Level::Warn,
            AuditEvent::SecondaryCpuStartFailed,
            &[Field {
                key: "cause",
                value: tairix_log::FieldValue::Str("no_bringup_surface"),
            }],
        );
        return;
    };
    // Allocate the per-CPU liveness table (which the dispatch handle and the
    // bring-up barrier read) and the companion quiesce-ack table, and publish
    // both to the cross-CPU quiesce coordinator. Both are sized to the
    // discovered core count (never a fixed ceiling). A publish failure is
    // set-once/wiring corruption: fail closed, loud, rather than run a
    // destructive takeover against stale liveness later.
    let online = match init_liveness_and_quiesce_tables(cpu_count) {
        Ok(online) => online,
        Err(cause) => {
            emit(
                audit_sink,
                Level::Error,
                AuditEvent::SecondaryCpuStartFailed,
                &[Field {
                    key: "cause",
                    value: tairix_log::FieldValue::Str(cause),
                }],
            );
            return;
        }
    };
    let handle: &'static KernelSecondaryDispatch<A> =
        Box::leak(Box::new(KernelSecondaryDispatch {
            state,
            boot_cpu,
            cpu_count,
            online,
        }));
    if crate::smp::publish_secondary_dispatch(handle).is_err() {
        // The hand-off slot is set-once per boot; a second publish means
        // this path ran twice — refuse to start anything against a stale
        // handle (fail closed) and say so.
        emit(
            audit_sink,
            Level::Error,
            AuditEvent::SecondaryCpuStartFailed,
            &[Field {
                key: "cause",
                value: tairix_log::FieldValue::Str("dispatch_already_published"),
            }],
        );
        return;
    }
    // Bring the secondaries up **one at a time**, waiting for each to
    // acknowledge it is fully online before releasing the next and before
    // returning (the caller then spawns PID 1). This serialisation is a
    // correctness requirement, not a nicety: a secondary released last
    // must finish adopting the kernel translation regime and arming its
    // per-CPU interrupt state *before* the boot CPU proceeds to mutate
    // shared kernel state (spawning PID 1, allocating page tables), or
    // that core can fault mid-bring-up on real hardware — a cache/
    // coherency hazard a cacheless emulator never exhibits, observed as
    // the last dense id deterministically never coming online.
    // SAFETY: the `KernelArch::secondary_bringup` contract obliges a port
    // returning `Some` to have installed its secondary entry and stack pool
    // before handing over a multi-CPU `BootInfo`.
    unsafe { start_each_secondary(state, bringup, handle, boot_cpu, cpu_count) };
}

/// Start each secondary dense id in `0..cpu_count` (skipping the boot CPU)
/// one at a time, waiting bounded for each to come online, auditing every
/// start, missed online-ack, and refusal.
///
/// Degraded-but-correct: a refusal or a missed acknowledgement is loud on the
/// audit log and the boot proceeds on the cores that did come up.
///
/// # Safety
///
/// The caller must have confirmed `bringup` is the port's real secondary
/// bring-up surface (its entry and stack pool installed) and published the
/// dispatch `handle`; each `cpu` started is a dense id in `0..cpu_count`
/// other than `boot_cpu`.
unsafe fn start_each_secondary<A: KernelArch + 'static>(
    state: &'static KernelState<A>,
    bringup: &dyn tairix_arch_api::SecondaryBringup,
    handle: &'static KernelSecondaryDispatch<A>,
    boot_cpu: CpuId,
    cpu_count: u32,
) {
    let audit_sink = state.audit_sink;
    let arch = state.arch.as_ref();
    for cpu in 0..cpu_count {
        if cpu == boot_cpu {
            continue;
        }
        let mut cpu_buf = [0u8; 12];
        let cpu_str = tairix_util::fmt::format_usize(cpu as usize, &mut cpu_buf);
        // SAFETY: forwarded from this function's contract — `cpu` is a dense
        // id in `0..cpu_count` and is not the (already running) boot CPU, and
        // `bringup` is the port's installed secondary surface.
        match unsafe { bringup.start_secondary(cpu) } {
            Ok(()) => {
                emit(
                    audit_sink,
                    Level::Info,
                    AuditEvent::SecondaryCpuStarted,
                    &[Field {
                        key: "cpu",
                        value: tairix_log::FieldValue::Str(cpu_str),
                    }],
                );
                // The core signals arrival from `run_secondary`
                // (`SecondaryCpuOnline`, `mark_online`). Wait for it,
                // bounded; a core that never checks in is audited and the
                // boot proceeds on the cores that did (degraded, loud).
                if !wait_secondary_online(arch, handle, boot_cpu, cpu) {
                    emit(
                        audit_sink,
                        Level::Warn,
                        AuditEvent::SecondaryCpuStartFailed,
                        &[
                            Field {
                                key: "cpu",
                                value: tairix_log::FieldValue::Str(cpu_str),
                            },
                            Field {
                                key: "cause",
                                value: tairix_log::FieldValue::Str("no_online_ack"),
                            },
                        ],
                    );
                }
            }
            Err(err) => emit(
                audit_sink,
                Level::Warn,
                AuditEvent::SecondaryCpuStartFailed,
                &[
                    Field {
                        key: "cpu",
                        value: tairix_log::FieldValue::Str(cpu_str),
                    },
                    Field {
                        key: "cause",
                        value: tairix_log::FieldValue::Str(err.as_str()),
                    },
                ],
            ),
        }
    }
}

/// Wait — **bounded** — for secondary `cpu` to acknowledge it has fully
/// brought up and is about to join the dispatch loop (the `mark_online`
/// edge `crate::smp::run_secondary` publishes after the arch port's
/// secondary entry has adopted the kernel translation regime and armed
/// this core's per-CPU interrupt state).
///
/// Returns `true` once the core checks in, or `false` if it does not
/// within the budget. This is the one-shot secondary-bring-up handshake:
/// a bounded, fail-loud wait (the narrow spin the charter permits for a
/// hardware handshake, never a task's steady state), not a busy-poll —
/// the boot CPU has nothing else it may safely do until the core it just
/// released is live. The budget is generous: a healthy core checks in
/// within microseconds, so it only ever elapses for a genuinely dead
/// core, which the caller then audits rather than wedging the boot.
fn wait_secondary_online<A: KernelArch + 'static>(
    arch: &A,
    handle: &KernelSecondaryDispatch<A>,
    boot_cpu: CpuId,
    cpu: CpuId,
) -> bool {
    // 500 ms — orders of magnitude above a real core's microsecond
    // bring-up, small enough that a dead core does not stall the boot
    // perceptibly.
    const BRINGUP_BUDGET_NS: u64 = 500_000_000;
    let start = arch.monotonic_ns(boot_cpu);
    loop {
        if handle.is_online(cpu) {
            return true;
        }
        if arch.monotonic_ns(boot_cpu).saturating_sub(start) >= BRINGUP_BUDGET_NS {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Seed the kernel CSPRNG output reserve from the arch port's platform
/// entropy source, replacing the unseeded `NullEntropy` boot reserve.
///
/// Fail-soft and audited (a security-relevant state change is logged): when
/// the port exposes a usable source and a draw produces bytes, a
/// [`crate::random::SeededReserve`] is installed and `random_get` begins
/// serving cryptographic output; otherwise the reserve is left unseeded so
/// every draw keeps failing closed with
/// [`tairix_abi::Errno::EntropyNotReady`] — never weakened to predictable
/// bytes. There is no panic and no busy-wait: a momentarily-underfull source
/// is the port's bounded-retry concern, and a hard failure simply leaves the
/// reserve unseeded.
fn seed_entropy_reserve<A: KernelArch + 'static>(state: &'static KernelState<A>) {
    use crate::random::{
        take_boot_seed_source, ArchEntropy, ArchTicks, IrqEntropyObserver, SeededReserve,
        IRQ_ENTROPY_POOL,
    };
    use tairix_rng::{EntropySource, InterruptPoolSource, JitterSource, MixedPair};

    let Some(source) = state.arch.platform_entropy() else {
        emit(
            state.audit_sink,
            Level::Info,
            AuditEvent::EntropyReserveUnseeded,
            &[Field {
                key: "cause",
                value: tairix_log::FieldValue::Str("no_source"),
            }],
        );
        return;
    };
    if !source.profile().provides_hardware_entropy() {
        // The port declares a tracked `Pending` / `Unsupported` source; do
        // not attempt a draw that will fail, just record the fail-closed
        // state.
        emit(
            state.audit_sink,
            Level::Info,
            AuditEvent::EntropyReserveUnseeded,
            &[Field {
                key: "cause",
                value: tairix_log::FieldValue::Str("source_pending"),
            }],
        );
        return;
    }

    // Never trust the hardware RNG alone: XOR-mix it with an independent
    // CPU-timing-jitter source before it seeds (and reseeds) the reserve. A
    // stuck, backdoored, or observable hardware source cannot lower the seed's
    // quality below the jitter source's contribution, and vice versa.
    let hardware = ArchEntropy::new(source);
    let mut jitter = JitterSource::new(ArchTicks::new(state.arch.clone()));
    // Probe the jitter source once so the audit records honestly whether the
    // second, independent source is contributing on this platform (a
    // deterministic/emulated counter fails its health tests and yields, in
    // which case the mix falls back to the hardware source alone).
    let jitter_healthy = {
        let mut probe = [0u8; 8];
        jitter.fill(&mut probe).is_ok()
    };

    // Add the asynchronous interrupt-arrival-timing pool as a third,
    // independent source. It contributes nothing at boot (it fails closed
    // until interrupts have flowed) but folds fresh timing into every reseed
    // for forward secrecy; the interrupt observer that feeds it is installed
    // below, only once a seeded reserve exists to drain it.
    let interrupt = InterruptPoolSource::new(&IRQ_ENTROPY_POOL);

    // Fold in the firmware-provided boot seed (the FDT `/chosen/rng-seed`) as
    // a fourth, independent source. It is the source of last resort: on an
    // emulated or virtualised machine the CPU exposes no hardware RNG and its
    // cycle counter is deterministic, so both `hardware` and `jitter` above
    // fail closed, and without the boot seed the reserve would never seed at
    // all — leaving `random_get`, the per-boot machine id, and the ramzip
    // sealing key all unavailable. It is a one-shot contribution consumed
    // here and wiped; later reseeds draw fresh entropy from the interrupt
    // pool. XOR-mixed like every source, so it can never lower the quality a
    // real hardware RNG contributes on a machine that has one.
    let boot_seed = take_boot_seed_source();
    let boot_seed_present = boot_seed.has_seed();
    let sources = match (jitter_healthy, boot_seed_present) {
        (true, true) => "hardware+jitter+bootseed",
        (true, false) => "hardware+jitter",
        (false, true) => "hardware+bootseed",
        (false, false) => "hardware",
    };

    let mixed = MixedPair::new(
        MixedPair::new(MixedPair::new(hardware, jitter), interrupt),
        boot_seed,
    );
    let mut reserve: SeededReserve<A> = SeededReserve::new();
    match reserve.seed(mixed) {
        Ok(()) => {
            // Swap the seeded reserve in for the unseeded boot reserve.
            *state.rng.write() = Box::new(reserve);
            // Now that a seeded, reseeding reserve exists, start feeding
            // interrupt-arrival timing into the pool it reseeds from. The
            // observer is set-once and lives for the kernel's lifetime.
            let observer: &'static IrqEntropyObserver<A> = Box::leak(Box::new(
                IrqEntropyObserver::new(state.arch.clone(), &IRQ_ENTROPY_POOL),
            ));
            let _ = state.irq.set_observer(observer);
            emit(
                state.audit_sink,
                Level::Info,
                AuditEvent::EntropyReserveSeeded,
                &[Field {
                    key: "sources",
                    value: tairix_log::FieldValue::Str(sources),
                }],
            );
            // A seeded CSPRNG now exists, so bring the process-global
            // compressed-memory tier (`ramzip`) online: this is the one
            // point a tier is installed. A failed key draw leaves none
            // installed and the compressed path stays inert (fail closed).
            install_ramzip_tier(state);
        }
        Err(_) => {
            // The source is enumerated but could not produce bytes (every
            // bounded draw was exhausted). Leave the reserve unseeded.
            emit(
                state.audit_sink,
                Level::Info,
                AuditEvent::EntropyReserveUnseeded,
                &[Field {
                    key: "cause",
                    value: tairix_log::FieldValue::Str("draw_failed"),
                }],
            );
        }
    }
}

/// Bring the process-global compressed-memory tier (`ramzip`) online
/// once the kernel CSPRNG reserve is seeded.
///
/// The tier's per-boot key and nonce salt are drawn from the seeded
/// reserve through a thin adapter onto the sealing layer's entropy
/// seam, and its capacity policy is derived from the discovered
/// physical RAM. This is the single install point; installation is
/// fail-closed — a failed draw (or an already-installed tier) leaves
/// the global tier absent, so the compressed fault-in and reclaim
/// paths stay inert rather than running with a weak or absent key.
fn install_ramzip_tier<A: KernelArch + 'static>(state: &'static KernelState<A>) {
    use tairix_kernel_mem::ramzip::{install, Ramzip, RamzipCaps};
    use tairix_kernel_mem::{EntropySource, SealError, PAGE_SIZE};

    /// Adapt the kernel CSPRNG output reserve to the sealing layer's
    /// entropy seam: a non-blocking draw (a seeded reserve never blocks
    /// for entropy) that fails the key derivation closed rather than
    /// yielding predictable bytes.
    struct ReserveEntropy<'a>(&'a mut (dyn RandomReserve + Send + Sync));
    impl EntropySource for ReserveEntropy<'_> {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
            self.0.draw(out, true).map_err(|_| SealError::Entropy)
        }
    }

    let ram = state
        .frame_allocator
        .usable_frames()
        .saturating_mul(PAGE_SIZE);
    let caps = RamzipCaps::from_physical(ram);
    let mut guard = state.rng.write();
    let built = {
        let mut entropy = ReserveEntropy(&mut **guard);
        Ramzip::new(caps, &mut entropy)
    };
    drop(guard);
    if let Ok(tier) = built {
        // First install wins; the boot path calls this exactly once, so a
        // `false` return would itself be a boot-path defect — but even
        // then the already-installed tier is authoritative, never two.
        let _installed = install(tier);
    }
}

/// The production [`InitSpawnCtx`] [`kernel_main`] hands the arch
/// [`crate::InitSpawn`] seam to spawn PID 1 (`plans/PI.md` P6c-3) and the
/// bin crate's driver autoloader drives to spawn user-space drivers
/// ([`spawn_driver_process`](InitSpawnCtx::spawn_driver_process),
/// `plans/PI.md` P10/P11).
///
/// It borrows the live kernel registries from the leaked `KernelState`
/// so the seam can register a freshly built task (scheduler, capability
/// table, address-space registry) and dispatch it without ever naming the
/// concrete scheduler or arch types itself (the generics stay on this side of the object-safe boundary).
///
/// Public and constructible through [`new`](Self::new) for the same reason
/// [`KernelSpawnCtx`] is: a QEMU integration vertical
/// drives the *production* spawn path through it rather than re-implementing
/// the `KernelSpawnCtx` assembly. The fields stay private so the borrow set
/// can only be supplied through [`new`](Self::new).
pub struct KernelInitSpawner<'a, A: KernelArch> {
    // `'static` because `kernel_main` builds this over the leaked
    // `KernelState`, and a kernel service spawned through
    // `spawn_kernel_service` must hold a DMA region for the running
    // kernel's whole lifetime (`InitSpawnCtx::static_frames`).
    frames: &'static FrameAllocator,
    audit: &'static (dyn Sink + Sync),
    scheduler: &'a Scheduler<A>,
    caps: &'a RwLock<CapTable>,
    aspaces: &'a RwLock<AddressSpaceRegistry>,
    arch: &'a A,
    /// The scheduler-side process-wait producer a driver spawned through
    /// [`spawn_driver_process`](InitSpawnCtx::spawn_driver_process) is
    /// recorded with, so the supervising task can later reap it
    /// (`plans/SPAWN.md` SP6). `'static` because it is `Box::leak`'d over
    /// the leaked `KernelState` like every other boot-installed producer.
    /// PID-1 admission (`admit_init`) does not consult it; it exists for the
    /// driver-spawn path. A boot path that wired no real producer passes the
    /// fail-closed [`crate::NULL_PROCESS_WAIT`].
    process_wait: &'static (dyn ProcessWait + 'static),
    /// The kernel IRQ table, so
    /// [`terminate_driver_process`](InitSpawnCtx::terminate_driver_process)
    /// can release every line a torn-down driver bound — the same
    /// [`IrqTable::release_for`] the `exit` syscall runs, here driven for a
    /// driver the device manager unloads. PID-1 admission and the
    /// driver-spawn path do not consult it.
    irq: &'a IrqTable,
    /// The shared-memory facility
    /// [`terminate_driver_process`](InitSpawnCtx::terminate_driver_process)
    /// frees a torn-down driver's shared-memory regions through (the same
    /// producer the `shm_*` syscalls drive). The driver-store unload runs in
    /// the service's own context, not the driver's, so the facility scrubs +
    /// frees a region's frames through the kernel direct map. A build with no
    /// facility wired passes the fail-closed
    /// [`crate::devres::NULL_SHARED_MEM_FACILITY`]; PID-1 admission and the
    /// driver-spawn path do not consult it.
    shared_mem_facility: &'static (dyn crate::devres::SharedMemFacility + 'static),
}

impl<'a, A: KernelArch> KernelInitSpawner<'a, A> {
    /// Bind a spawn context to the live kernel subsystems.
    ///
    /// `frames` is the leaked-`'static` physical-frame allocator (it doubles
    /// as the `'static` page-table frame source for a spawned child);
    /// `audit` is the boot audit sink; `scheduler` / `caps` / `aspaces` /
    /// `arch` are the live registries a freshly built task is registered
    /// with; `process_wait` is the producer a spawned driver's parent/child
    /// wait link is recorded with (the fail-closed
    /// [`crate::NULL_PROCESS_WAIT`] when none is wired).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        frames: &'static FrameAllocator,
        audit: &'static (dyn Sink + Sync),
        scheduler: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        aspaces: &'a RwLock<AddressSpaceRegistry>,
        arch: &'a A,
        process_wait: &'static (dyn ProcessWait + 'static),
        irq: &'a IrqTable,
        shared_mem_facility: &'static (dyn crate::devres::SharedMemFacility + 'static),
    ) -> Self {
        Self {
            frames,
            audit,
            scheduler,
            caps,
            aspaces,
            arch,
            process_wait,
            irq,
            shared_mem_facility,
        }
    }
}

/// Which CPU is driving [`run_dispatch_loop`], deciding who may end it.
///
/// The boot CPU owns system termination: when every live task has exited
/// it breaks out so `kernel_main` halts fail-closed. A secondary CPU
/// never terminates the system — before PID 1 is admitted (and again
/// whenever the machine is momentarily drained) its queue is simply
/// empty, so it parks on the idle path and waits for the scheduler's
/// placement IPI or a device interrupt to bring it work.
#[derive(Clone, Copy)]
enum DispatchRole {
    /// The boot CPU: breaks out once no live task remains.
    Boot,
    /// A started secondary CPU: parks on idle, never terminates.
    Secondary,
}

/// Service the work the dispatch loop owes between two task dispatches,
/// once the just-run task has suspended and no kernel lock or scheduler
/// critical section is in flight.
///
/// Two things, in order:
///
/// 1. Perform any wake a device-IRQ / timer handler deferred while the
///    task ran. The handler is lock-free and only flagged the wake; the
///    real `unpark` runs here, where taking the scheduler/run-queue locks
///    is safe.
/// 2. Top up the buffered console transmit
///    ([`KernelArch::pump_console_tx`]). The loop calls this on **every**
///    successful dispatch, not only when it idles, so a port whose
///    transmit is buffered keeps draining even while a perpetually
///    runnable in-kernel kthread (e.g. the polled USB-keyboard report
///    pump) keeps the loop from ever reaching its idle park — the output
///    then flows at the loop's dispatch rate, independent of the
///    transmit-FIFO interrupt the silicon may not self-sustain. A no-op on ports with synchronous
///    console output.
fn service_between_dispatches<A: KernelArch>(arch: &A) {
    let _ = crate::waitq::drain_pending_wakes();
    // Deliver any foreground `^C`/`^Z` the console line discipline
    // queued from interrupt context: like the deferred wakes, the actual
    // scheduler-driving delivery runs here, where taking the run-queue
    // locks is safe.
    let _ = crate::procsignal::drain_pending_foreground();
    arch.pump_console_tx();
}

/// The kernel dispatch loop — the one definition every CPU runs, boot
/// and secondary alike (`role` decides only who may end it).
///
/// Runs with device IRQs **enabled** so TAIRiX is fully preemptive:
/// every in-kernel task and kthread the loop dispatches executes with
/// interrupts deliverable, so a long in-kernel operation (a slow MMIO
/// bring-up read, a busy driver poll) can no longer mask interrupts for
/// its whole span and starve the preemption one-shot, the
/// buffered-serial transmit drain, or an interrupt-driven waiter — the
/// cooperative dispatch loop the charter forbids. A device IRQ taken
/// mid-task services its source and returns to the same task (the
/// kernel stays non-preemptible); its lock-free handler flags a
/// deferred wake that [`service_between_dispatches`] performs here, in
/// dispatcher context, where taking the scheduler/run-queue locks is
/// safe.
///
/// The idle path parks race-free: it masks device IRQs, drains once more,
/// and rechecks scheduler readiness (the local queue plus global overflow)
/// before committing to sleep. The recheck covers a placement IPI that arrived
/// and was consumed after `step` reported idle but before IRQ masking; any
/// later IPI remains pending while masked and wakes
/// [`KernelArch::wait_for_interrupt`]. Device IRQs are
/// left masked when the loop returns: whichever way it ended, there is no
/// dispatcher left on this CPU to service them.
fn run_dispatch_loop<A: KernelArch>(
    scheduler: &Scheduler<A>,
    arch: &A,
    cpu: CpuId,
    role: DispatchRole,
) {
    arch.set_device_irqs(true);
    loop {
        // Stamp both watchdog heartbeats: one loop iteration means the
        // scheduler regained this CPU and is making a fresh decision (a CPU
        // that stops reaching here is a soft lockup the timer-tick
        // `check_stall` will report), and reaching here at all is proof the
        // CPU is alive and taking interrupts — it either just woke from
        // `wfi` by taking one or is running continuously (a CPU that stops
        // reaching here while still Active is a hard lockup). Two relaxed
        // monotonic-counter reads' worth of stores per dispatch, off any
        // lock.
        let now_ns = arch.monotonic_ns(cpu);
        crate::watchdog::note_progress(cpu, now_ns);
        // Refresh the liveness heartbeat too: the non-maskable sample is
        // only taken while this CPU runs, not while it is parked in `wfi`,
        // so a CPU returning to work after a long idle park would otherwise
        // carry a stale liveness heartbeat inherited from before the park.
        crate::watchdog::note_alive(cpu, now_ns);
        // Publish this CPU as running work only *after* stamping fresh
        // progress *and* liveness heartbeats, so a cross-CPU watchdog scan
        // can never catch it Active with a stale heartbeat inherited from a
        // long idle park (which would be a false soft- or hard-lockup
        // report).
        crate::watchdog::set_activity(cpu, crate::watchdog::WatchdogActivity::Active);
        // Retire any reschedule obligation left by the task that just
        // suspended before the policy makes its next decision. CFQ arms
        // the incoming task's one-shot inside `step`; clearing later in
        // the task shim would erase a timer that expired between that arm
        // and the switch to user mode, leaving a CPU-bound task with no
        // armed timer and no pending reschedule.
        crate::preempt::clear_preempt_pending(cpu);
        // Kernel-activity breadcrumb: this CPU is entering the scheduler
        // step (task pick + context switch). A CPU that wedges here reports
        // `k_site=dispatch`, distinguishing a scheduler/run-queue stall from
        // one inside a syscall or fault resolver (`crate::watchdog`).
        crate::watchdog::note_kernel_breadcrumb(
            cpu,
            crate::watchdog::KernelBreadcrumb::Dispatch,
            0,
        );
        let outcome = scheduler.step(cpu);
        // A user task can suspend back to this dispatcher directly from a
        // timer exception. Native exception entry masks device interrupts,
        // and that per-CPU mask is not part of the task context, so the
        // switch hands the masked state to us. Restore delivery only after
        // `step` has returned — no task or scheduler critical section is in
        // flight here — before draining deferred work or dispatching again.
        // Without this restoration each CPU eventually remains masked under
        // preemptive load and the whole system loses timer/device progress.
        arch.set_device_irqs(true);
        match outcome {
            // A task ran. On the boot CPU, stop once every task has
            // exited so `kernel_main` halts; keep dispatching otherwise.
            Ok(StepOutcome::Ran(id)) => {
                // If this dispatch just retired a task that was terminated
                // while it executed (a signal kill or a driver unload of a
                // still-running process), land the deferred teardown now:
                // the task returned to this loop and executes nowhere, so
                // reclaiming its resources can no longer race its own
                // accesses into a wild fault. A task that was not killed
                // while running makes this a single relaxed atomic read.
                crate::procsignal::land_running_kill(id);
                if matches!(role, DispatchRole::Boot) && scheduler.live_task_count() == 0 {
                    break;
                }
                // Service the per-dispatch background work (deferred
                // wakes + buffered console transmit) now that the task
                // has suspended and no kernel lock / scheduler critical
                // section is in flight.
                service_between_dispatches(arch);
            }
            // No runnable task this step. The boot CPU treats a fully
            // drained system as finished; otherwise the live tasks are
            // all parked (or homed elsewhere), so park this CPU until
            // the next interrupt — the scheduler's placement IPI when
            // work lands here, or a device IRQ — then re-step. Never
            // busy-spin (tickless idle).
            Ok(StepOutcome::Idle) => {
                if matches!(role, DispatchRole::Boot) && scheduler.live_task_count() == 0 {
                    break;
                }
                arch.set_device_irqs(false);
                let woke = crate::waitq::drain_pending_wakes();
                // A queued foreground signal is dispatchable work too: a
                // `^C` typed while every task is parked must terminate
                // the foreground job now, not after the next unrelated
                // interrupt.
                let delivered = crate::procsignal::drain_pending_foreground();
                let Ok(local_ready) = scheduler.has_ready_work(cpu) else {
                    break;
                };
                if !(woke || delivered || local_ready) {
                    arch.pump_console_tx();
                    // Parked with nothing to run: a legitimately quiet CPU,
                    // never a lockup. Publish Idle so the cross-CPU scan
                    // does not judge it; the loop re-stamps progress and
                    // republishes Active at its top on the next wake.
                    crate::watchdog::set_activity(cpu, crate::watchdog::WatchdogActivity::Idle);
                    arch.wait_for_interrupt();
                }
                arch.set_device_irqs(true);
            }
            Err(_) => break,
        }
    }
    arch.set_device_irqs(false);
    // No dispatcher runs on this CPU any more, so it owes no progress:
    // publish Offline so the watchdog never mistakes a retired CPU for a
    // lockup.
    crate::watchdog::set_activity(cpu, crate::watchdog::WatchdogActivity::Offline);
}

#[cfg(test)]
mod dispatch_loop_tests {
    use super::{run_dispatch_loop, DispatchRole};
    use crate::sched::{Scheduler, SchedulerConfig};
    use crate::test_arch::TestArch;
    use crate::KernelArch;
    use alloc::sync::Arc;
    use std::thread;
    use tairix_kernel_sched_api::{Priority, StepOutcome, TaskAction};

    /// A task may suspend the dispatcher from an exception context whose
    /// architecture mask still blocks device interrupts. Once the scheduler
    /// step returns, the dispatcher must restore delivery before it handles
    /// deferred wakes or chooses more work; otherwise every CPU can
    /// eventually become permanently interrupt-masked under preemptive load.
    #[test]
    fn dispatcher_restores_device_irqs_after_a_task_step() {
        let arch = Arc::new(TestArch::with_cpus(1));
        let scheduler = Scheduler::new(
            SchedulerConfig {
                cpus: 1,
                queue_capacity_per_band: 8,
                yields_before_demotion: 1,
                boost_interval_ticks: 10,
            },
            arch.clone(),
        )
        .expect("scheduler builds");
        let task_arch = arch.clone();
        scheduler
            .spawn(0, Priority::Normal, move |_| {
                task_arch.set_device_irqs(false);
                TaskAction::Exit
            })
            .expect("task spawns");

        run_dispatch_loop(&scheduler, arch.as_ref(), 0, DispatchRole::Boot);

        assert_eq!(
            arch.irq_enable_count(),
            2,
            "initial enable plus post-step restoration must both occur"
        );
    }

    #[test]
    fn idle_commit_rechecks_work_published_after_the_idle_step() {
        let arch = Arc::new(TestArch::with_cpus(1));
        let scheduler = Arc::new(
            Scheduler::new(
                SchedulerConfig {
                    cpus: 1,
                    queue_capacity_per_band: 8,
                    yields_before_demotion: 1,
                    boost_interval_ticks: 10,
                },
                arch.clone(),
            )
            .expect("scheduler builds"),
        );
        let sentinel = scheduler
            .spawn_parked(0, Priority::Normal, |_| TaskAction::Park)
            .expect("parked sentinel spawns");
        arch.arm_idle_mask_gate();

        let publisher_arch = arch.clone();
        let publisher_scheduler = scheduler.clone();
        let publisher = thread::spawn(move || {
            while !publisher_arch.idle_mask_gate_entered() {
                thread::yield_now();
            }
            let retire_scheduler = publisher_scheduler.clone();
            publisher_scheduler
                .spawn(0, Priority::Normal, move |_| {
                    retire_scheduler
                        .exit(sentinel)
                        .expect("sentinel remains live until the injected task runs");
                    TaskAction::Exit
                })
                .expect("work publishes in the masked idle-commit window");
            publisher_arch.release_idle_mask_gate();
        });

        run_dispatch_loop(scheduler.as_ref(), arch.as_ref(), 0, DispatchRole::Boot);
        publisher.join().expect("publisher thread completes");

        assert_eq!(
            arch.interrupt_wait_count(),
            0,
            "the dispatcher must not sleep after work was published in its idle-commit window"
        );
        assert_eq!(scheduler.live_task_count(), 0);
        assert_eq!(scheduler.step(0), Ok(StepOutcome::Idle));
    }
}

impl<A: KernelArch + 'static> InitSpawnCtx for KernelInitSpawner<'_, A> {
    fn frames(&self) -> &FrameAllocator {
        self.frames
    }

    fn audit(&self) -> &(dyn Sink + Sync) {
        self.audit
    }

    // Every argument is a distinct piece of the first process's admission
    // state; bundling them into a struct would only move the same list one
    // level out.
    #[allow(clippy::too_many_arguments)]
    unsafe fn admit_init(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        stack_span: crate::aspace::StackSpan,
        stack: Box<dyn crate::kthread::KernelStack + Send>,
        pre_resume: crate::spawn::ProcessResume,
        live: Option<alloc::sync::Arc<crate::procspace::ProcessSpace>>,
        entry: crate::spawn::UserThreadEntry,
    ) {
        let cpu: CpuId = SchedulerArch::current_cpu(self.arch);

        // Admit PID 1 as a resumable **user kthread** (`plans/SPAWN.md`
        // SP2): the work body performs the user-mode transition on the
        // task's own kernel stack, and the switch-in hook reactivates
        // PID 1's address-space root before every switch into it so it
        // `eret`s back into EL0 under the correct translation regime.
        // The transition diverges into EL0, so the work never returns through
        // the trampoline's terminal `Exit` — PID 1 leaves EL0 only through a
        // rescheduling syscall (`yield`/`exit`), whose trap path suspends
        // it back to the scheduler.
        let work = move |_yielder: &mut crate::kthread::Yielder<A::Cs>| {
            // SAFETY: this runs on PID 1's own first dispatch, so its
            // switch-in hook has already activated its address space, and
            // this method's contract has the trap path installed.
            unsafe { entry.enter() }
        };
        // PID 1's first thread carries the process hook bound to its own
        // thread pointer (`0` — thread-local storage is the layer above this
        // one).
        let pre_resume = crate::spawn::thread_pre_resume(&pre_resume, entry.regs.tls_base);
        let cs = self.arch.context_switch();
        // When the seam retained a live, mutable address space, admit PID 1
        // with it so its `mem_map` / `mmio_map` syscalls mutate its own
        // space through the per-CPU live-space slot (`plans/PI.md`
        // 5d-0-ii (b′)); otherwise admit the plain form and those syscalls
        // fail closed.
        //
        // PID 1 is admitted **parked** (the trailing `true`): the secondary
        // CPUs are already online by the time this runs, so a Ready
        // admission could be work-stolen onto another core and take its
        // first syscall before the caps/aspace/streams below exist. It is
        // made runnable only by the `unpark` just before the dispatch loop,
        // once that state is installed.
        let admitted = match live {
            Some(live) => crate::kthread::spawn_user_kthread_with_stack_live(
                self.scheduler,
                cs,
                stack,
                cpu,
                Priority::Normal,
                pre_resume,
                live,
                work,
                true,
            ),
            None => crate::kthread::spawn_user_kthread_with_stack(
                self.scheduler,
                cs,
                stack,
                cpu,
                Priority::Normal,
                pre_resume,
                work,
                true,
            ),
        };
        let Ok(task_id) = admitted else {
            // The home queue could not admit the task — fail closed: return
            // so the seam (and then `kernel_main`) halts the CPU.
            return;
        };

        // Register the task's caps under the *same* numeric id the
        // dispatcher recovers (`SecTaskId(current_task)`), so PID 1's first
        // syscall resolves a caller context. PID 1 runs as the system
        // principal (uid 0 — the system user), which has no users-db row:
        // its registered manifest *is* its ceiling, so it stands as both
        // derive bounds (`plans/CAPABILITY_USE.md` §4.1, the one legitimate
        // manifest-as-ceiling shape; a user session's ceiling is its
        // account grant, threaded through `SpawnCredential`).
        let sec_id = SecProcessId::leader(SecTaskId(task_id));
        // Attach PID 1's process-instance identity (a kernel-trusted bootstrap
        // principal minted from the shared per-boot counter), so its syscalls
        // are attributed to this instance distinctly from any task that later
        // reuses the numeric id.
        let record = TaskCapabilities::derive(sec_id, UserId(0), caps, caps, self.audit)
            // Marked so PID 1's inherit-spawned children (the boot
            // services) are each bounded by their own registered manifest,
            // not by PID 1's.
            .as_system_principal()
            .with_proc_id(crate::proc_id::mint_proc_id_bootstrap())
            // PID 1 is the init process; its name is kernel-known, not
            // resolved from any caller.
            .with_name(ProcName::from_bytes_truncating(b"init"));
        self.caps.write().insert(record);

        // Register PID 1's frozen address space + direct map under the same
        // id, so a first syscall that copies from user memory (e.g.
        // `stream_write` reading `init`'s banner) resolves the caller's
        // mappings instead of failing closed with `BadAddress`
        // (`plans/PI.md` P6c-3 follow-up). A fresh task id is never already
        // present; should registration nonetheless be refused, fail closed
        // by returning so the seam (and `kernel_main`) halts the CPU rather than entering a program whose user
        // memory the kernel cannot reach.
        if self
            .aspaces
            .write()
            .register(sec_id, space, physmap)
            .is_err()
        {
            // PID 1 is still parked; retire it so it is not left an
            // unrunnable orphan before we halt.
            let _ = self.scheduler.exit(task_id);
            return;
        }

        // Establish PID 1's standard streams: the
        // standard descriptor table (`stdin` readable,
        // `stdout`/`stderr`/`stdinfo` writable), each backed by the
        // discovered console the boot path installed, so `init` writes
        // its banner through `stream_write(STDOUT, …)` over an inherited
        // stream rather than an ambient device.
        {
            let mut aspaces = self.aspaces.write();
            aspaces.set_streams(sec_id, DescriptorTable::standard());
            // Record PID 1's reserved user-stack span beside its streams,
            // under the same write lock, so the stack-growth fault path can
            // back pages inside it on demand — bounded by its `StackBytes`
            // limit. The span is seam-derived from the validated spawn
            // layout, never a caller-supplied value.
            aspaces.set_stack_span(sec_id, SecTaskId(task_id), stack_span);
        }

        // Drive PID 1 (and anything it spawns) to completion: each `step`
        // dispatches the next runnable task on this CPU. The first step
        // sets the per-CPU current task to PID 1, runs its `pre_resume`
        // hook, and switches into it; control returns here when the task
        // suspends through a rescheduling syscall (`yield`/`exit`) or its
        // kernel stack could not seed a frame (fail-closed `Exit`). The
        // loop stops once no task is live or the CPU idles, then returns so
        // `kernel_main` halts fail-closed. A real
        // session frontend that never exits is `plans/SPAWN.md` SP4.
        //
        // SAFETY: the seam built PID 1's image into and switched to the
        // active address space before calling here, and the EL1/trap vector
        // is installed, so the new program's first syscall is handled (this
        // method's contract); the `pre_resume` hook keeps the correct root
        // active across every later switch into a user kthread.
        // PID 1's caps, address space, and streams are now installed, so it
        // is safe to make it runnable. Unpark is the single point that
        // enqueues it and must run **before** the dispatch loop — a parked
        // PID 1 would leave the loop idling forever with nothing to wake it.
        // A refused wake on a freshly parked task is a kernel invariant
        // violation: retire it and fail closed to the halt.
        if self.scheduler.unpark(task_id).is_err() {
            let _ = self.scheduler.exit(task_id);
            return;
        }
        // Drive the shared kernel dispatch loop as the boot CPU: it runs
        // until every task has exited (or the scheduler errors), then
        // returns with device IRQs masked so `kernel_main` halts
        // fail-closed with no dispatcher left to service them.
        run_dispatch_loop(self.scheduler, self.arch, cpu, DispatchRole::Boot);
    }

    fn spawn_kernel_service(
        &self,
        mut body: crate::kthread::KernelServiceBody,
    ) -> Option<tairix_kernel_sched_api::TaskId> {
        // Admit the service as a kernel-only resumable kthread on the boot
        // CPU's run queue (`plans/SPAWN.md` SP1). It must be admitted
        // **before** `admit_init` drives the dispatch loop, so the loop
        // dispatches it alongside PID 1. The work shim wraps the
        // dispatcher-side concrete `Yielder<A::Cs>` in the object-safe
        // `YielderHandle`, so the arch seam's `body` never names the port's
        // context-switch type. The admitted
        // scheduler [`TaskId`] is returned so the caller can wake the
        // service by id (the driver-store server registers it on
        // `SERVE_WAITQ`); a failed admission yields `None`.
        let cpu: CpuId = SchedulerArch::current_cpu(self.arch);
        let cs = self.arch.context_switch();
        let work = move |yielder: &mut crate::kthread::Yielder<A::Cs>| {
            let mut handle = crate::kthread::YielderHandle::new(yielder);
            body(&mut handle);
        };
        crate::kthread::spawn_kthread(self.scheduler, cs, cpu, Priority::Normal, work).ok()
    }

    fn static_frames(&self) -> Option<&'static FrameAllocator> {
        Some(self.frames)
    }

    fn static_audit(&self) -> Option<&'static (dyn Sink + Sync)> {
        // The boot audit sink is `'static` (leaked at boot), so a service
        // kthread can route its security decisions onto the audit
        // channel for the life of the kernel.
        Some(self.audit)
    }

    fn spawn_driver_process(
        &self,
        path: &str,
        rxe: &[u8],
        caps: CapabilitySet,
        grants: &[HwResource],
        args: &[&[u8]],
        node_id: Option<u32>,
    ) -> Result<u64, Errno> {
        // Build the production runtime-spawn context over the same live
        // subsystems PID-1 admission uses and drive the deferred-load admit
        // (`plans/FIX-DESKTOP.md` §2.6.5): the driver image the signed
        // `drvhost` load gate already verified is a *prebuilt* plan, so the
        // driver builds its own isolated address space on its own first slice
        // — the autoloading boot service is never blocked on the build. The
        // bin-crate caller never names `Scheduler<A>` / `KernelSpawnCtx` —
        // that assembly happens here, behind the object-safe `InitSpawnCtx`
        // boundary.
        //
        // `frames` is `'static`, so it doubles as the `'static` page-table
        // frame source the producer builds the child's page tables from
        // (reclaimable RAM that scales with the machine). The child
        // is minted one owner-checked grant per requested resource;
        // the `grants` originate kernel-side (the discovered hardware tree),
        // never from an untrusted caller.
        //
        // The driver is recorded against the kernel boot supervisor identity
        // (`SecTaskId(0)` — task/uid 0, the system context): a
        // boot-autoloaded driver has no userland parent that `wait`s on it.
        // It is established with the fail-closed all-closed descriptor table
        // (`DescriptorTable::closed`): a driver is not a text session, so it
        // inherits no console and reaches no stream backing rather than being
        // handed an ambient device; a driver's
        // diagnostics flow through `lib/log`, never `stdout`.
        let ctx = KernelSpawnCtx::new(
            self.frames,
            Some(self.frames),
            self.audit,
            self.scheduler,
            self.caps,
            self.aspaces,
            self.arch,
            // A driver spawn has no user-space parent process: the kernel
            // itself is the spawner, which the sentinel names.
            SecProcessId(0),
            self.process_wait,
            DescriptorTable::closed(),
            // A driver spawn wires no standard-stream open entries: its
            // all-closed table above is the whole stream story.
            alloc::vec::Vec::new(),
            grants,
            node_id,
            // A boot-floor driver is a kernel-trusted bootstrap principal
            // admitted before any untrusted code runs; mint its
            // process-instance identity from the shared per-boot counter
            // (the entropy reserve is not seeded this early).
            crate::proc_id::mint_proc_id_bootstrap(),
            // Attest the driver's name from the kernel-resolved store path
            // the signed load gate verified the image from, through the one
            // shared naming rule — a bundle's generic `Run` entry point
            // names its owning driver directory, any other path its final
            // component — so a process listing and the audit origin always
            // name the driver, never from the spawner's argv.
            ProcName::from_path(path.as_bytes()),
            // Record the kernel-resolved driver-store path the signed load
            // gate verified, so even a driver process could name its own
            // program to a self-spawn without trusting argv.
            path.as_bytes().to_vec(),
            // A boot-autoloaded driver is a kernel-trusted system principal:
            // admit it under the fixed system credential (uid 0 / gid 0), the
            // spawn-as-user counterpart of the `SecTaskId(0)` supervisor
            // identity above. uid 0 carries no ambient authority; the driver's
            // powers flow only from `caps`.
            SpawnCredential::system(),
            // A driver is a trusted, manifest-bounded principal, never a
            // parser sandbox.
            false,
        );
        // A boot-floor driver reads its configuration from its argument
        // vector alone; it inherits no environment (there is no principal
        // yet whose exported variables it could meaningfully receive). The
        // verified image bytes and argument vector are copied into the
        // prebuilt plan because the driver materialises on its own task,
        // after this call returns, and cannot borrow the caller's buffers.
        let plan = crate::syscalls::LoadPlan::Prebuilt {
            rxe: alloc::borrow::Cow::Owned(rxe.to_vec()),
            requested: caps,
        };
        let owned_args = args.iter().map(|a| a.to_vec()).collect();
        ctx.admit_loading(plan, owned_args, alloc::vec::Vec::new())
            .map_err(crate::spawn::admit_errno)
    }

    fn terminate_driver_process(&self, handle: u64) -> Result<(), Errno> {
        // The handle is the driver's PID, which is its scheduler task id and,
        // equally, the numeric its security id was minted under
        // (`admit_process` builds `SecTaskId(task_id)`). Reclaim every
        // kernel-held piece of the driver under that one id.
        let sched_id = handle;
        let sec_id = SecProcessId(handle);

        // Presence is keyed on the address-space registry entry every spawned
        // driver registers: if neither it nor a capability record exists, no
        // live driver bears this handle, so the unload is a benign idempotent
        // miss (the device manager may diff the same vanished node twice).
        let known = self.aspaces.read().contains(sec_id)
            || self.caps.read().caps_of_process(sec_id).is_some();
        if !known {
            return Err(Errno::NotFound);
        }

        // Reap the driver's scheduler tasks: mark each Exited (never dispatched
        // again) and drop its body, reclaiming its kernel stack and, with the
        // last of them, the process's live address space and page-table frames.
        // A parked driver (the common case — one blocked in `irq_wait` / a
        // served-endpoint park) drops immediately; a vanished id is a benign
        // no-op. Idempotent, never a panic.
        //
        // The unit is the driver's whole **thread group**: a user-space driver
        // may have created threads of its own (`plans/THREADS.md`), and one left
        // running would keep executing against the state this teardown withdraws
        // with no path left to stop it. A build that registered no capability
        // record still has the leader task to stop.
        //
        // A thread that is *still executing* on another CPU cannot be reclaimed
        // here: withdrawing the address space while its own code still runs
        // would turn a legitimate access into a wild fault (the same defect the
        // signal-terminate path fixes). The scheduler reports such a thread
        // `Deferred`; defer the whole teardown to the dispatch loop, which
        // reclaims the process through the one shared landing rule once the last
        // of them retires (the scheduler already IPI'd that CPU). The unload is
        // committed either way — audit it now.
        let mut threads: alloc::vec::Vec<u64> =
            self.caps.read().threads_of(sec_id).map(|t| t.0).collect();
        if threads.is_empty() {
            threads.push(sched_id);
        }
        let mut deferred = false;
        for thread in threads {
            if let Ok(ExitDisposition::Deferred) = self.scheduler.exit(thread) {
                crate::procsignal::defer_plain_reclaim(thread, sec_id);
                deferred = true;
            } else {
                // Down now: retire its per-thread state so the group's count can
                // reach zero and a deferred sibling's landing knows it was last.
                let _ = crate::threads::retire(self.caps, self.aspaces, SecTaskId(thread));
            }
        }
        if deferred {
            let mut handle_buf = [0u8; 16];
            emit(
                self.audit,
                Level::Info,
                AuditEvent::DriverUnloaded,
                &[Field {
                    key: "handle",
                    value: tairix_log::FieldValue::Str(format_hex_u64(handle, &mut handle_buf)),
                }],
            );
            return Ok(());
        }

        // Withdraw the address-space-registry entry: reclaims the driver's
        // device-resource grants, standard streams, resource limits, and
        // matched-node record together, so no stale grant or mapping survives
        // the driver (the same withdrawal the `exit` syscall path will drive).
        self.aspaces.write().withdraw(sec_id);

        // Release every shared-memory mapping the driver held, dropping each
        // reference and zeroing + freeing any region whose last reference this
        // releases. Mirrors the `exit` syscall's reclaim; the region frames
        // are scrubbed through the kernel direct map, so freeing works even
        // though this teardown runs in the driver-store service's context, not
        // the driver's own (a driver may be the region owner whose last
        // grantee already vanished).
        crate::sharedreg::reclaim_process(self.shared_mem_facility, sec_id);

        // Destroy every synchronous call endpoint the driver served before
        // dropping its capability record, mirroring the `exit` syscall: a
        // user-space service that is torn down must not leave callers blocked
        // in `ipc_call` forever — destroying its endpoints cancels their
        // in-flight calls, waking `CALL_WAITQ` re-runs each parked caller's
        // poll so it abandons fail-closed, and the vanish observer lets the
        // volume layer react to an unplugged disk's dead block service.
        crate::callreg::teardown_owned_by(handle, self.aspaces, self.audit);

        // Drop every wait-set the driver owned, mirroring the `exit` syscall.
        // A wait-set holds no resource of its own (its members only *name* the
        // endpoints and IRQ lines reclaimed around here), so dropping the sets
        // is the whole reclamation; idempotent.
        crate::waitset::release_owned_by(handle);

        // Release every IRQ line the driver bound (`docs/src/security/irq.md`):
        // the kernel unmasks no lines on teardown; a later driver that wants
        // the same line re-issues `irq_bind`.
        let _ = self.irq.release_for(sec_id);

        // Drop the capability record last, so a concurrent `cap_query` racing
        // this teardown never observes a task whose caps vanished while the
        // scheduler still believed it lived — the same ordering the `exit`
        // syscall keeps.
        let _ = self.caps.write().remove(sec_id);

        let mut handle_buf = [0u8; 16];
        emit(
            self.audit,
            Level::Info,
            AuditEvent::DriverUnloaded,
            &[Field {
                key: "handle",
                value: tairix_log::FieldValue::Str(format_hex_u64(handle, &mut handle_buf)),
            }],
        );
        Ok(())
    }
}

/// Drive every init phase in [`Phase::ORDER`].
///
/// Returns `Ok(())` if every phase completed successfully, or the
/// first [`InitError`] encountered. Each phase that begins emits one
/// [`AuditEvent::PhaseStarted`] record and, on success, one
/// [`AuditEvent::PhaseReady`] record. A failing phase emits neither a
/// `Ready` nor a duplicate `Started` for downstream phases.
///
/// On success it hands back both the leaked-`'static` [`KernelState`] and
/// the leaked-`'static` process-wait producer the [`Phase::Syscall`] step
/// built, so [`kernel_main`] can give the PID-1 / driver spawn context the
/// *same* producer the `wait` syscall drives (`plans/SPAWN.md` SP6) rather
/// than building a second, divergent one.
///
/// The function is intentionally non-public — external callers go
/// through [`kernel_main`]. Splitting it out lets the unit tests in
/// this module assert phase-by-phase behaviour without the trailing
/// `arch.halt()` swallowing the test thread.
// `run_phases` is the single linear boot sequence whose phase order the
// `docs/src/architecture/kernel.md` init-order section documents step by
// step; keeping every phase (mem, sec, sched, irq, state assembly, the live
// producers, and the dispatcher install) in one place is what makes that
// order auditable in one read. Splitting it to satisfy the line lint would
// scatter the documented order across helpers for no clarity gain, so the
// length is allowed deliberately (the producer and dispatcher-facility
// construction are already factored into `live_producers` /
// `build_shared_mem_facility`).
#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_lines)]
fn run_phases<A: KernelArch>(
    boot: BootInfo<'_, A>,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
) -> Result<
    (
        &'static KernelState<A>,
        &'static (dyn ProcessWait + 'static),
    ),
    InitError,
> {
    // Pre-flight: re-validate the handover before logging Phase::Log
    // started — a malformed BootInfo means we cannot even trust the
    // log_level we just installed.
    boot.validate().map_err(InitError::BadBootInfo)?;

    let BootInfo {
        cpu_count,
        memory_map,
        scheduler_config,
        arch,
        dispatcher_callback_slot,
        consoles,
        programs,
        image_builder,
        app_store,
        seat_registry,
        users_db,
        users_admin,
        hw_tree,
        filesystem,
        volumes,
        volume_service,
        spawn_identity,
        kernel_heap_bytes,
        installed_memory_bytes,
        ..
    } = boot;

    // Phase 1 — Log. The filter was already installed before we
    // arrived; the explicit phase event marks the transition for
    // external consumers tracking the boot timeline.
    phase_started(log_sink, Phase::Log);
    phase_ready(log_sink, Phase::Log);

    // Phase 2 — Mem.
    phase_started(log_sink, Phase::Mem);
    // Prove the installed RAM before trusting it with a single frame: a
    // quick but effective self-test of every usable region (walking-pattern
    // stuck-bit and power-of-two address-line checks) drawn on the boot
    // console as a verified-MiB counter that climbs to the installed total.
    // It runs here, ahead of the allocator, precisely so it may write freely
    // to the free RAM it tests; it leaves every tested region zeroed, and a
    // detected fault halts the boot rather than run on memory the kernel
    // could not trust (fail closed).
    // Retain the boot memory map `'static`: the frame allocator is built from
    // it here, and the pre-boot Supervisor's `mem map` command lists its
    // usable/reserved regions later (`plans/NEW-SUPERVISOR.md`) — one owner,
    // no second copy.
    let memory_map: &'static tairix_kernel_mem::BootMemoryMap = Box::leak(Box::new(memory_map));
    crate::memtest::run(arch.as_ref(), memory_map, installed_memory_bytes, consoles);
    let frame_allocator: &'static FrameAllocator = Box::leak(Box::new(
        FrameAllocator::new(memory_map).map_err(InitError::Mem)?,
    ));
    // Activate growable kernel-heap backing at the first point both required
    // components exist. Scheduler/runtime initialization allocates from the
    // heap, so deferring this until after those allocations can exhaust the
    // fixed bootstrap region on a valid discovered-sized topology.
    if let Some(physmap) = arch.direct_phys_map() {
        crate::kheap::install_frame_heap_source(frame_allocator, physmap);
    }
    phase_ready(log_sink, Phase::Mem);

    // Phase 3 — Sec. Build the compiled-in system identity (the OS-owned
    // accounts and groups — kernel policy, tamper-proof as the kernel
    // text) and publish it into the boot path's identity cell, so
    // spawn-as-user and filesystem group resolution for the system
    // accounts work from first boot, before any volume is mounted. The
    // encrypted-root unlock later replaces the cell's table with the
    // merged system∪human table. The verifier emits its own audit record
    // on the `audit_sink`; we still emit our phase markers on the
    // diagnostic `log_sink` so the two streams stay aligned. A boot path
    // with no identity cell (a host harness) skips the install and every
    // credential resolution stays fail-closed.
    phase_started(log_sink, Phase::Sec);
    if let Some(cell) = spawn_identity {
        let table = crate::groups::system_identity_table(audit_sink).map_err(InitError::Sec)?;
        // The cell is set-once: a refused install means the boot path
        // published a table before the sec phase ran — a re-entry logic
        // error surfaced fail-closed rather than silently kept.
        cell.install(table)
            .map_err(|_| InitError::Sec(tairix_abi::Errno::AlreadyExists))?;
    }
    phase_ready(log_sink, Phase::Sec);

    // Phase 4 — Sched.
    phase_started(log_sink, Phase::Sched);
    let scheduler =
        Scheduler::new(scheduler_config, Arc::clone(&arch)).map_err(InitError::Sched)?;
    crate::initialize_cpu_state(scheduler_config.cpus).map_err(|error| match error {
        crate::CpuStateInitError::ZeroCpus => InitError::CpuStateZeroCpus,
        crate::CpuStateInitError::AllocationFailed => InitError::CpuStateAllocationFailed,
        crate::CpuStateInitError::AlreadyInstalled => InitError::CpuStateAlreadyInstalled,
    })?;
    // Install the port's park-translation hook before any user task can
    // be dispatched: every user-task suspend then re-parks the CPU on the
    // permanent boot root, the invariant a dead task's page-table
    // reclamation (the live-space drop at reap) relies on. A port with no
    // hardware user address spaces returns `None` and the dispatcher
    // skips the park.
    if let Some(park) = arch.park_translation() {
        crate::kthread::install_park_translation(park);
    }
    phase_ready(log_sink, Phase::Sched);

    // Phase 5 — Irq. Consult the arch port's
    // [`KernelArch::irq_routing`] hook and construct the kernel-wide
    // [`IrqTable`] with the returned `max_line`. The controller
    // returned here is the `'static` seam every subsequent
    // [`IrqTable::fire`] call goes through; it is sourced from the
    // arch port (`x86_64` returns an `IoApicController` programmed
    // against the MADT-discovered IO-APIC; the
    // [`KernelArch::irq_routing`] default returns the conservative
    // [`IrqRouting::unsupported`] which keeps `max_line = 0` and
    // makes every `mask` call surface `Errno::NotImplemented`).
    //
    // The phase is placed strictly between `Sched` and `Syscall` so
    // the IRQ table is in place before any caller can dispatch
    // `irq_bind` / `irq_wait` (fail closed). The
    // audit log fields the phase emits are `phase = "irq"`.
    phase_started(log_sink, Phase::Irq);
    let routing: IrqRouting = arch.irq_routing();
    let irq_table = IrqTable::new(routing.max_line);
    let irq_controller: &'static (dyn IrqController + Send + Sync) = routing.controller;
    phase_ready(log_sink, Phase::Irq);

    // Assemble `KernelState` and lift it to `'static` so the
    // `Phase::Syscall` step can publish a `&'static dyn DispatchHook`
    // referencing its fields. The `Box::leak` is intentional: the
    // kernel never returns, so the leak is a one-shot publish into a
    // `'static` slot, not a global *mutable* static (the per-CPU bootstrap area is the only sanctioned
    // mutable static; this allocation is immutable after creation
    // because every interior field carries its own synchronisation
    // primitive (`Scheduler`'s internal locks, `RwLock<CapTable>`)).
    let state: &'static KernelState<A> = Box::leak(Box::new(KernelState {
        frame_allocator,
        scheduler,
        caps: RwLock::new(CapTable::new()),
        ipc: RwLock::new(PortRegistry::new()),
        aspaces: RwLock::new(AddressSpaceRegistry::new()),
        // The kernel random output reserve boots **unseeded** over the
        // `NullEntropy` source: a reserve always
        // exists, but `random_get` fails closed with `EntropyNotReady`
        // until the platform-RNG entropy seam re-seeds it — the
        // same seam the encrypted-swap key is drawn from, still
        // pending. Boxed as a `dyn RandomReserve` so the boot reserve and
        // a later seeded one share one field type.
        rng: RwLock::new(Box::new(BootReserve::new()) as Box<dyn RandomReserve + Send + Sync>),
        arch,
        audit_sink,
        irq: irq_table,
        irq_controller,
    }));

    // Derive the per-boot resource-limit default from the discovered
    // installed-memory total: the pinned-memory budget (`mem_pin`,
    // `plans/STRESSTEST.md` ST2) scales with the machine instead of a
    // hard-wired ceiling, and every task — the boot floor and all its
    // descendants — inherits it through the registry's one default. A
    // boot that never learned the total keeps the compile-time floor
    // rather than fabricating a zero bound that would refuse every pin.
    if installed_memory_bytes != 0 {
        state
            .aspaces
            .write()
            .set_default_limits(LimitSet::with_pinned_default(default_pinned_limit_bytes(
                installed_memory_bytes,
            )));
    }

    // Hand the arch port a `'static` reference to the freshly
    // constructed IrqTable so its external-IRQ trap dispatcher can
    // translate a vector-level hit to `IrqTable::fire`. The default
    // [`KernelArch::install_irq_dispatch`] is a no-op; real arch
    // ports (x86_64) override it to publish the reference into the
    // arch crate's dispatcher slot (set-once per boot).
    state.arch.install_irq_dispatch(&state.irq);

    // Seed the kernel CSPRNG output reserve from the platform entropy source
    // now that the arch handle is live. Until this point the reserve is the
    // unseeded `NullEntropy` boot reserve and every draw fails closed; after a
    // successful seed `random_get` serves cryptographic output and minted
    // `ProcId`s gain their unpredictable half. Fail-soft: a port with no
    // usable source leaves the reserve unseeded (still fail-closed), never
    // weakened to predictable bytes.
    seed_entropy_reserve(state);

    // Mint the per-boot identifier now the reserve is (best-effort) seeded
    // (`PREREQUISITES.md` P-E). The draw is non-blocking and fail-closed: a
    // port whose entropy source could not seed the reserve yields
    // `BootId::UNSET`, and `boot_id_get` then reports `EntropyNotReady` rather
    // than the all-zero sentinel — never a predictable id. Audited as a
    // security-relevant state change (the record carries neither entropy nor
    // the id itself).
    let boot_id = crate::boot_id::mint_boot_id(&state.rng);
    emit(
        audit_sink,
        Level::Info,
        if boot_id.is_unset() {
            AuditEvent::BootIdUnavailable
        } else {
            AuditEvent::BootIdMinted
        },
        &[],
    );

    // Build the scheduler-side process-wait producer the `wait` syscall
    // drives (`plans/SPAWN.md` SP6b). It owns the parent/child + exit-status
    // bookkeeping and parks a waiting parent back on the scheduler until a
    // child is reapable; it needs only the `'static` arch handle (to read the
    // current CPU when parking), so it is built here over the leaked
    // `KernelState` and `Box::leak`'d for the same one-shot-publish reason as
    // the hook below. Until this stage the handler held the
    // fail-closed `NULL_PROCESS_WAIT`.
    // Keep the concrete `&'static KernelProcessWait` binding: the signal
    // producer below composes over it (it owns the shared parent/child +
    // exit-status bookkeeping both syscalls read), while the handler consumes
    // it through the `dyn ProcessWait` coercion.
    let process_wait_concrete: &'static KernelProcessWait<A> =
        Box::leak(Box::new(KernelProcessWait::new(state.arch.as_ref())));
    let process_wait: &'static (dyn crate::procwait::ProcessWait + 'static) = process_wait_concrete;

    // Build the scheduler-side process-signal producer the `signal` syscall
    // drives (`plans/SPAWN.md` SP7b). It authorises the target and records a
    // signalled termination through the same `KernelProcessWait` above (one
    // source of truth for the parent/child relationship, never a second copy)
    // and delivers the signal by driving the live scheduler (unpark for
    // continue, exit for terminate/kill). `Box::leak`'d for the same
    // one-shot-publish reason as the hook. Until this stage the handler held
    // the fail-closed `NULL_PROCESS_SIGNAL`.
    let process_signal_concrete = Box::leak(Box::new(crate::procsignal::KernelProcessSignal::new(
        process_wait_concrete,
        &state.scheduler,
        // The thread-group table, so a signal to a PID reaches every thread of
        // that process rather than its leader alone (`plans/THREADS.md`
        // decision 10).
        &state.caps,
    )));
    let process_signal: &'static (dyn crate::procsignal::ProcessSignal + 'static) =
        process_signal_concrete;
    // The same producer is the foreground `^C`/`^Z` delivery target the
    // console line discipline queues to (`plans/SPAWN.md` SP9): one delivery
    // engine, two entry points (the parent-authorised syscall and the
    // console's standing foreground instruction).
    let _ = crate::procsignal::install_foreground_signal(process_signal_concrete);

    // Publish the wait-queue arch hook (Design D P-2) so the explicit /
    // timed wake paths reach the live scheduler + arch (factored out to
    // keep this function within the line budget).
    publish_wait_queue_arch(state);

    // Give the stall watchdog its report channel: a CPU that stops making
    // scheduler progress is reported on the same serial/console sink the
    // panic path uses, so a soft lockup is loud rather than a silent seize.
    // Until this point the watchdog records heartbeats but stays quiet
    // (fail-safe); the wait-queue hook installed just above supplies the
    // monotonic clock its tick-driven check reads.
    crate::watchdog::install_report_sink(audit_sink);

    // In a debug-diagnostics build, give the watchdog its separate
    // diagnostic channel: the address-bearing lockup detail
    // (image-relative pc/backtrace, the kernel-activity breadcrumb) goes to
    // the `log_sink` (the diagnostic/UART stream), never the persistent
    // hash-chained audit trail above — so no kernel address ever lands on
    // the tamper-evident log. A shippable image compiles this out (there is
    // no such detail to route) and pays nothing.
    #[cfg(feature = "watchdog-diagnostics")]
    crate::watchdog::install_diagnostic_sink(log_sink);

    // Give the watchdog its best-effort recovery channel, when the port has
    // one: a detected lockup is met with a cross-CPU reschedule (soft) or a
    // directed attention interrupt (hard). A port without one leaves
    // recovery inert — the lockup is still reported loudly, and the attempt
    // is honestly recorded as `unsupported` (fail closed).
    if let Some(recovery) = state.arch.watchdog_recovery() {
        crate::watchdog::install_recovery(recovery);
    }

    // Give the watchdog the port's kernel-internal line-name resolver, when
    // it has one: a hard-lockup report then attributes a stuck line the
    // kernel services itself (the platform MSI multiplexer, the console UART)
    // to a stable category name (`stuck_owner=<name>`) instead of a bare
    // `unbound`, so a reader can tell the USB/PCIe MSI line a wedged CPU
    // could not service from a genuinely spurious line. A port without one
    // leaves such a line rendering `unbound` exactly as before.
    if let Some(names) = state.arch.watchdog_line_names() {
        crate::watchdog::install_kernel_line_names(names);
    }

    // Wrap every boot-installed console's input half in the blocking
    // adapter (the stream backing owns blocking, never
    // the program): a `stream_read` finding its device empty parks the
    // caller back on the scheduler until input arrives, instead of
    // reporting a zero-length read user space cannot distinguish from
    // end of input. Each adapter needs the same `'static` arch handle
    // (to read the current CPU when parking) as the process-wait
    // producer above, so the rebuilt list is `Box::leak`'d the same way
    // — a one-shot publish, not a global mutable static. A console whose read half fails closed (a write-only
    // serial port's `NULL_CONSOLE_READ`) keeps failing closed: the
    // inner error propagates straight through without parking.
    let consoles: &'static [crate::console::ConsoleDevice] = {
        let mut wrapped = alloc::vec::Vec::with_capacity(consoles.len());
        for device in consoles {
            // Each console gets its own secret-entry feedback over its own
            // output: `stream_input_mode(Secret)` (a password read) arms it, the
            // blocking reader feeds and animates it, so every text-console
            // password prompt shows the shared `[input active...]` marker.
            let secret: &'static crate::console::SecretFeedback =
                Box::leak(Box::new(crate::console::SecretFeedback::new(device.write)));
            let blocking: &'static (dyn crate::console::ConsoleRead + Sync + 'static) =
                Box::leak(Box::new(crate::console::BlockingConsoleRead::new(
                    state.arch.as_ref(),
                    device.read,
                    Some(secret),
                )));
            // Preserve the console's injected-input half across the
            // wrap: a keyboard-backed console keeps the same
            // `ConsoleInputQueue` the blocking `read` adapter now drains,
            // so an input-focus arbiter push still reaches the parked
            // reader (`plans/PI.md` P11).
            wrapped.push(
                crate::console::ConsoleDevice::with_input(device.write, blocking, device.input)
                    .with_secret(secret),
            );
        }
        Box::leak(wrapped.into_boxed_slice())
    };

    // Build the production `mem_map` / `mmio_map` / `dma_alloc` producers over
    // the calling process's live address space (`plans/PI.md` 5d-0-ii (b′)/(c)):
    // each routes a syscall to the calling task's *own* live space via the
    // per-CPU slot, reading the current CPU from the same `'static` arch handle
    // the process-wait producer uses, so a task that retains a live space (the
    // aarch64 ports) gets a working producer and one that does not fails closed
    // with `NotImplemented` exactly as the `NULL_*` defaults did. All are
    // `Box::leak`'d for the same one-shot-publish reason as the hook, arch-
    // generic so this names no concrete port.
    let (mem_map, file_map, mmio_map_facility, dma_alloc_facility, shared_mem_facility) =
        live_producers(state.arch.as_ref(), state.frame_allocator);

    // Publish the discovered physical-RAM size so every reclaimable cache
    // sizes its budget against the RAM the machine actually has, not the
    // (now merely bootstrap) heap size. Set before the mount/unlock path
    // builds any cache.
    crate::memstats::set_cache_backing_bytes(
        state
            .frame_allocator
            .usable_frames()
            .saturating_mul(tairix_kernel_mem::PAGE_SIZE),
    );

    // Register the production `ramzip` stats feed so the System
    // Information `RAMZIP_STATS` query reports the live global tier's
    // counters. Safe before (and whether or not) a tier installs: it
    // reads the global tier, which reports an idle all-zero snapshot
    // until `install_ramzip_tier` brings one online.
    crate::memstats::install_global_ramzip_stats();

    // The production wall clock, named so it backs *both* the
    // `wall_time_get`/`wall_time_set` syscalls and the introspection
    // uptime domain's boot-instant projection — one clock, no second copy.
    // `Box::leak`'d for the same one-shot-publish reason as the hook.
    let wall_clock: &'static crate::wallclock::KernelWallClock =
        Box::leak(Box::new(crate::wallclock::KernelWallClock::new()));

    // The live introspection source the `sysinfo_introspect` syscall serves
    // (`PREREQUISITES.md` P-C): built over the leaked `KernelState` (its
    // `CapTable` / scheduler / frame allocator / per-task limits / arch),
    // the mounted filesystem service (mount table), the wall clock (uptime),
    // and the binding kernel's committed heap size. Leaked for the same
    // one-shot-publish reason as the hook. A boot path that wires no
    // filesystem still answers every domain truthfully (an empty mount list,
    // the unprovisioned identity sentinel), so the broker that holds
    // `CAP_SYSINFO_INTROSPECT` can serve every query.
    let introspect: &'static (dyn crate::introspect::IntrospectSource + 'static) = Box::leak(
        Box::new(crate::introspect_source::KernelIntrospectSource::new(
            state,
            filesystem,
            wall_clock,
            // The account directory (uid + username, no credential
            // material) is derived from the same kernel-held database
            // `users_db_read` serves; with none installed the directory is
            // truthfully empty.
            users_db,
            kernel_heap_bytes,
        )),
    );

    // The pre-boot Supervisor's live system-state provider
    // (`plans/NEW-SUPERVISOR.md`): built over the same leaked `KernelState`
    // (arch handle, frame allocator, scheduler), the retained boot memory
    // map, and the wall clock, then published set-once for the binding
    // kernel's Supervisor host to read through
    // `crate::supervisor_system::supervisor_system`. Leaked for the same
    // one-shot-publish reason as the introspection source. A second install
    // fails closed rather than re-pointing the live provider; the boot path
    // installs it exactly once, so the result is discarded.
    let supervisor_system: &'static (dyn crate::supervisor_system::SupervisorSystem + 'static) =
        Box::leak(Box::new(
            crate::supervisor_system::KernelSupervisorSystem::new(
                state,
                memory_map,
                wall_clock,
                kernel_heap_bytes,
            ),
        ));
    let _ = crate::supervisor_system::install_supervisor_system(supervisor_system);

    // Phase 6 — Syscall. Publish the production `DispatchHook` into
    // the bin-crate-owned slot. The hook itself is `Box::leak`'d for
    // the same reason as `KernelState`: its borrows reference
    // `KernelState` fields and must therefore be `'static`.
    phase_started(log_sink, Phase::Syscall);
    let hook = KernelDispatchHook::new(
        &state.scheduler,
            &state.caps,
            state.arch.as_ref(),
            audit_sink,
            &state.irq,
            state.irq_controller,
            &state.ipc,
            &state.aspaces,
            &state.rng,
            consoles,
            state.frame_allocator,
            // The same leaked-`'static` allocator, handed to the spawn
            // producer as a `'static` page-table frame source so a child's
            // page tables come from reclaimable RAM that scales with the
            // machine rather than a fixed `.bss` pool.
            state.frame_allocator,
            programs,
            process_wait,
            seat_registry,
            mem_map,
            mmio_map_facility,
            dma_alloc_facility,
        )
        // Serve `file_map` / `file_unmap` and the user-fault resolver
        // through the demand-paged file-mapping producer; the default
        // `NULL_FILE_MAP` keeps both syscalls (and every fault) fail-closed
        // until installed.
        .with_file_map(file_map)
        // Serve the users database the boot path loaded off the mounted
        // root volume (`plans/PI.md` P11); the default `NULL_USERS_DB`
        // keeps `users_db_read` fail-closed when no root volume was
        // mounted.
        .with_users_db(users_db)
        // Serve the account-administration engine the unlock path installs
        // (`plans/CAPABILITY_USE.md` CU4); the default `NULL_USERS_ADMIN`
        // keeps `users_admin` fail-closed when no root volume was mounted.
        .with_users_admin(users_admin)
        // Serve the discovered hardware tree the boot path seeded
        // (Design D); the default `NULL_HW_TREE` keeps `hw_tree_read` /
        // `hw_tree_wait` fail-closed when no inventory was seeded.
        .with_hw_tree(hw_tree)
        // Serve the `fs_*` syscalls through the disk-backed filesystem
        // service the boot path installed (`PREREQUISITES.md` P-A); the
        // default `NULL_FILESYSTEM` keeps every `fs_*` syscall fail-closed
        // when no volume was mounted.
        .with_filesystem(filesystem)
        // Resolve `id::<volume-id>/…` paths against the volume forest the
        // boot path publishes mounted volumes into (`plans/DEVICES.md`
        // D3a); the default `NULL_VOLUME_FOREST` keeps every `id::`
        // resolution fail-closed when no volume was published.
        .with_volumes(volumes)
        // Delegate runtime volume attach/detach to the service the boot
        // path installed (`plans/DEVICES.md` D3b); the default
        // `NULL_VOLUME_SERVICE` keeps both syscalls fail-closed when no
        // boot path can host runtime volumes.
        .with_volume_service(volume_service)
        // Resolve a spawn-as-user switch against the authoritative identity
        // table the sec phase installed into the boot path's cell — the same
        // table the filesystem service resolves caller groups against; a
        // boot path with no cell falls to the inert `NULL_IDENTITY`, which
        // keeps a switch fail-closed, and the default `spawn` (inherit)
        // never consults it.
        .with_identity(spawn_identity.unwrap_or(&crate::syscalls::NULL_IDENTITY))
        // Serve `wall_time_get` / `wall_time_set` through the production
        // wall clock (`PREREQUISITES.md` P-D). It boots `Unset`; a trusted
        // time source drives it via `wall_time_set` under `CAP_TIME_SET`.
        // `Box::leak`'d for the same one-shot-publish reason as the hook.
        .with_wall_clock(wall_clock)
        // Serve `sysinfo_introspect` through the live introspection source
        // built above (`PREREQUISITES.md` P-C); the default `NULL_INTROSPECT`
        // keeps the syscall fail-closed `NotImplemented` until wired.
        .with_introspect(introspect)
        // Serve `boot_id_get` with the per-boot id minted above
        // (`PREREQUISITES.md` P-E). When the reserve could not be seeded this
        // is `BootId::UNSET` and `boot_id_get` fails closed `EntropyNotReady`.
        .with_boot_id(boot_id)
        // Serve `log_emit` through the kernel diagnostic sink; the audit sink
        // stays kernel-only.
        .with_log_sink(log_sink)
        // Serve `msi_alloc` through the arch MSI controller (`None` is fail-closed).
        .with_msi_alloc_facility(state.arch.as_ref().msi_alloc_facility())
        // Serve `shm_*` through the `kernel/mem`-backed shared-memory producer.
        .with_shared_mem_facility(shared_mem_facility)
        // Serve `signal` through the scheduler-side producer built above
        // (`plans/SPAWN.md` SP7b); the default `NULL_PROCESS_SIGNAL` keeps
        // `signal` fail-closed `NotImplemented` until this is installed.
        .with_process_signal(process_signal);
    // Install the on-disk application store when the boot path provided one
    // (`plans/APPS.md` deliverable 8): the `spawn` syscall then verifies and
    // launches `…/<Name>.app/Run` bundles from the mounted volume. With none
    // installed a store-bundle spawn fails closed, parking nothing.
    let hook = match app_store {
        Some(store) => hook.with_app_store(store),
        None => hook,
    };
    // Mint the boot-static machine summary the ungated `boot_facts_get`
    // syscall reports — the arch port's stated identity, the validated CPU
    // count, and the boot path's pre-carve installed-memory total. A port
    // with no Tier-1 identity (the host test arch) or a boot path that
    // never learned the installed total leaves the facts uninstalled and
    // the syscall failing closed rather than fabricating a machine shape.
    let hook = match state.arch.arch_id() {
        Some(arch_id) if installed_memory_bytes != 0 => hook.with_boot_facts(BootFacts {
            arch: arch_id,
            // A port that discovered no CPU model installs the honest
            // `UNKNOWN`; readers render their own fallback for it.
            cpu_name: state
                .arch
                .cpu_name()
                .unwrap_or(tairix_abi::CpuName::UNKNOWN),
            cpu_count,
            memory_bytes: installed_memory_bytes,
        }),
        _ => hook,
    };
    let hook = Box::leak(Box::new(hook));
    dispatcher_callback_slot
        .install_dispatcher(hook)
        .map_err(InitError::DispatcherAlreadyInstalled)?;
    // Publish the same leaked hook as the signal producer's task-reclaim
    // seam, so the signal-terminate path drives the exact teardown the
    // `exit` handler runs (a killed task must release its capability
    // record, IRQ bindings, endpoints, and open files — a leaked pipe end
    // would park its peer forever). Set-once; a stray re-install is a
    // benign skip (this is the only caller).
    let _ = process_signal_concrete.install_task_reclaim(hook);
    // Publish the same leaked signal producer as the dispatch loop's
    // deferred-kill lander: when a task is terminated while it is still
    // executing on another CPU, the terminate path defers the reap+reclaim
    // and the dispatch loop lands it through this seam once the owning
    // dispatch has retired the task (it executes nowhere by then, so the
    // reclaim can no longer race its accesses into a wild fault). Set-once;
    // a stray re-install is a benign skip (this is the only caller).
    let _ = crate::procsignal::install_deferred_kill_lander(process_signal_concrete);
    phase_ready(log_sink, Phase::Syscall);

    // Phase 6 — Ipc. The named-port registry is composed into
    // `KernelState` above (`ipc: RwLock<PortRegistry>`) and borrowed by
    // the `KernelDispatchHook` so the `ipc_send` / `ipc_recv` handlers
    // resolve an endpoint against a live, kernel-owned map. It boots
    // empty — every endpoint is published at runtime by the binder that
    // holds the bind authority; the phase event fires
    // so the boot timeline is uniform.
    phase_started(log_sink, Phase::Ipc);
    phase_ready(log_sink, Phase::Ipc);

    // Publish the launch-services bundle the asynchronous process-launch
    // path captures (`plans/FIX-DESKTOP.md` §2.6.5): a `spawn` admits its
    // child at once and the child materialises its own image on its first
    // slice, off the spawning caller's task, through the `'static` handles
    // bundled here. Every handle is the *same* leaked `KernelState` /
    // boot-handover object the syscall dispatcher resolves a caller against,
    // so the loading child's re-derived capability record and registered
    // address space land in exactly the registries the dispatcher reads.
    // `install_over` is the one shared build+publish both this boot path and
    // a manually-assembled boot (a QEMU integration kernel) use, so the
    // launch bundle is wired identically everywhere; it is set-once and
    // idempotent, so a host binary that drives `run_phases` twice in one
    // process leaves the first live bundle in place rather than overwriting
    // it.
    let _ = crate::spawn_services::install_over(
        state.arch.as_ref(),
        state.frame_allocator,
        audit_sink,
        filesystem,
        app_store,
        &state.aspaces,
        &state.caps,
        process_wait,
        image_builder,
    );

    Ok((state, process_wait))
}

/// Build and `Box::leak` the production `mem_map` / `mmio_map` / `dma_alloc`
/// / `shm_*` producers over the calling process's live address space
/// (`plans/PI.md` 5d-0-ii (b′)/(c); `plans/USB.md` for shared memory).
///
/// Each is arch-generic (it reads the current CPU from the `'static` `arch`
/// handle and routes to the calling task's own live space) and `Box::leak`'d for the one-shot-publish reason `KernelState`
/// is. The shared-memory producer additionally draws the region backing
/// from `frames` (the kernel allocator). Factored out of [`run_phases`] so
/// the four long-typed bindings live in one place.
fn live_producers<A: KernelArch>(
    arch: &'static A,
    frames: &'static FrameAllocator,
) -> (
    &'static (dyn crate::memmap::MemMap + 'static),
    &'static (dyn crate::filemap::FileMap + 'static),
    &'static (dyn crate::devres::MmioMapFacility + 'static),
    &'static (dyn crate::devres::DmaAllocFacility + 'static),
    &'static (dyn crate::devres::SharedMemFacility + 'static),
) {
    // `LiveMemMap` implements both the anonymous-memory and the
    // demand-paged file-mapping producer over the same per-CPU live-space
    // routing; the two facilities are distinct leaked instances of the one
    // type (each holds only the shared `'static` arch borrow).
    (
        Box::leak(Box::new(crate::live_producer::LiveMemMap::new(arch))),
        Box::leak(Box::new(crate::live_producer::LiveMemMap::new(arch))),
        Box::leak(Box::new(crate::live_producer::LiveMmioMap::new(arch))),
        Box::leak(Box::new(crate::live_producer::LiveDmaAlloc::new(arch))),
        build_shared_mem_facility(arch, frames),
    )
}

/// Build (and `Box::leak`) the production shared-memory facility over the
/// arch direct physical map and the kernel frame allocator, or the
/// fail-closed [`crate::devres::NULL_SHARED_MEM_FACILITY`] when the port
/// wires no direct map (then `shm_*` return `NotImplemented`).
///
/// The one definition both the syscall handler (via [`live_producers`]) and
/// the [`KernelInitSpawner`] driver-unload path build their facility from, so
/// the construction logic is not duplicated. The two call sites get distinct
/// leaked instances, which is correct: [`crate::live_producer::LiveSharedMem`]
/// holds only `'static` borrows of the shared frame allocator, direct map,
/// and arch, so a region created through one instance frees correctly through
/// the other (both drive the same allocator and direct map).
fn build_shared_mem_facility<A: KernelArch>(
    arch: &'static A,
    frames: &'static FrameAllocator,
) -> &'static (dyn crate::devres::SharedMemFacility + 'static) {
    match arch.direct_phys_map() {
        Some(physmap) => {
            let facility: &'static (dyn crate::devres::SharedMemFacility + 'static) =
                Box::leak(Box::new(crate::live_producer::LiveSharedMem::new(
                    arch, frames, physmap,
                )));
            // Record the production facility (first-wins) so kernel-side
            // shared-region consumers outside the boot handover — the
            // runtime volume attach path's kernel hold — reach the same
            // mechanism. The inert null is never recorded, so a port
            // without a direct map stays fail-closed.
            crate::devres::install_shared_mem_facility(facility);
            facility
        }
        None => &crate::devres::NULL_SHARED_MEM_FACILITY,
    }
}

/// In-memory record of the live kernel subsystems built by
/// [`run_phases`].
///
/// Lives for the lifetime of the running kernel: `kernel_main`
/// `Box::leak`s the value so the `Phase::Syscall` step can publish a
/// `'static dyn DispatchHook` referencing its fields. The kernel
/// never returns from `kernel_main`'s halt, so the leak is a
/// one-shot publish, not a global mutable static.
///
/// The scheduler, IRQ dispatch, and syscall hook all read it for the rest of
/// the boot, so `kernel_main` holds the `'static` reference until it halts.
pub(crate) struct KernelState<A: KernelArch> {
    pub(crate) frame_allocator: &'static FrameAllocator,
    pub(crate) scheduler: Scheduler<A>,
    /// Per-task capability registry, read by the `KernelDispatchHook` on every
    /// syscall and written by the `cap_delegate` / `cap_revoke` handlers.
    /// Reader-preferring `RwLock` so the syscall hot path takes only a shared
    /// lock (mirrors `Scheduler::tasks`'s composition strategy).
    pub(crate) caps: RwLock<CapTable>,
    /// Named-port registry. The `KernelDispatchHook` reads this on
    /// every `ipc_send` / `ipc_recv` to resolve the endpoint carried
    /// in the syscall against the live, kernel-owned [`PortRegistry`];
    /// the binder that holds the bind authority publishes endpoints
    /// into it at runtime. Wrapped in the same
    /// reader-preferring `RwLock` as `caps` so the syscall hot path
    /// takes only a shared lock and the kernel composes both
    /// registries under one lock-ordering policy (the registry itself owns no lock, mirroring `CapTable`).
    pub(crate) ipc: RwLock<PortRegistry>,
    /// Per-task address-space registry backing the kernel's
    /// `copy_from_user` / `copy_to_user` boundary.
    /// Maps a task's [`tairix_kernel_sec::TaskId`] to its user
    /// [`tairix_kernel_mem::AddressSpace`] and the [`PhysMap`] that
    /// backs it, so a syscall handler can resolve the caller's task id
    /// to the pair [`tairix_kernel_mem::uaccess`] walks. Wrapped in the
    /// same reader-preferring `RwLock` as `caps` / `ipc` so the syscall
    /// hot path takes only a shared lock and the kernel composes every
    /// registry under one lock-ordering policy (the
    /// registry owns no lock of its own).
    ///
    /// [`PhysMap`]: tairix_kernel_mem::PhysMap
    //
    // Increment C (`PLAN.md` Stage 7) threads this registry into the
    // `KernelDispatchHook` / `KernelSyscallHandlers` below, where
    // `with_caller_aspace` reads it to resolve the caller's task id to
    // its address space. Increment D wires the deferred syscalls' copy
    // path through that accessor. The registry boots empty: entries are
    // populated by the spawner and withdrawn on `exit` once those call
    // sites reach it.
    pub(crate) aspaces: RwLock<AddressSpaceRegistry>,
    /// The kernel's single cryptographic random output reserve. The `KernelDispatchHook` borrows it so
    /// `random_get` draws CSPRNG output from it before copying the
    /// bytes into the caller's buffer. It boots **unseeded** over the
    /// [`NullEntropy`](crate::random::NullEntropy) source, so a draw fails closed with
    /// [`tairix_abi::Errno::EntropyNotReady`] until the platform-RNG
    /// entropy seam re-seeds the boxed reserve in
    /// place. Held type-erased behind a `Box<dyn RandomReserve>` and
    /// wrapped in the same reader-preferring `RwLock` as `caps` / `ipc`
    /// / `aspaces` (the draw takes the write guard because the reserve
    /// mutates its buffer as it serves).
    pub(crate) rng: RwLock<Box<dyn RandomReserve + Send + Sync>>,
    pub(crate) arch: Arc<A>,
    /// Audit sink the dispatch hook emits security-relevant records
    /// through. Held here so the hook borrows it for the lifetime of
    /// `KernelState` rather than re-discovering it at syscall time.
    pub(crate) audit_sink: &'static (dyn Sink + Sync),
    /// Kernel IRQ table backing the `irq_bind` / `irq_wait`
    /// syscalls. The `irq_bind` handler binds against the calling
    /// task's [`tairix_kernel_sec::TaskId`]; the `irq_wait` handler
    /// runs a yield-cycle on [`IrqTable::try_wait_step`]; the
    /// `exit` handler calls [`IrqTable::release_for`] to evict
    /// every binding the exiting task held.
    pub(crate) irq: IrqTable,
    /// Controller-mask seam consumed by [`IrqTable::fire`] from the
    /// arch port's trap path.
    ///
    /// Sourced from the architecture port's
    /// [`crate::KernelArch::irq_routing`] hook during [`Phase::Irq`].
    /// The default is the kernel/irq
    /// [`tairix_kernel_irq::UNSUPPORTED_CONTROLLER`] (every `mask`
    /// returns [`tairix_kernel_irq::MaskError::Unsupported`]); ports
    /// with a programmable controller (x86_64's `IoApicController`)
    /// override the trait method and return a real instance.
    ///
    /// Stored as a `&'static` trait object so [`KernelDispatchHook`]'s
    /// `&'a (dyn IrqController + Sync)` borrow can be taken without
    /// indirection through `Box`. The reference's stability for the
    /// lifetime of the running kernel is the arch port's contract.
    pub(crate) irq_controller: &'static (dyn IrqController + Send + Sync),
}

fn phase_started(sink: &(dyn Sink + Sync), phase: Phase) {
    emit(
        sink,
        Level::Info,
        AuditEvent::PhaseStarted,
        &[Field {
            key: "phase",
            value: tairix_log::FieldValue::Str(phase.as_str()),
        }],
    );
}

fn phase_ready(sink: &(dyn Sink + Sync), phase: Phase) {
    emit(
        sink,
        Level::Info,
        AuditEvent::PhaseReady,
        &[Field {
            key: "phase",
            value: tairix_log::FieldValue::Str(phase.as_str()),
        }],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::SchedulerConfig;
    use crate::test_arch::TestArch;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use tairix_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
    use tairix_log::Level;

    fn make_memory_map() -> BootMemoryMap {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: (PAGE_SIZE as u64) * 64,
            kind: RegionKind::Usable,
        });
        map
    }

    fn leak_dispatch_slot() -> &'static crate::DispatchCallbackSlot {
        // Mirrors the bin-crate `static DISPATCH_SLOT` convention but
        // with `Box::leak` (permitted in tests).
        Box::leak(Box::new(crate::DispatchCallbackSlot::new()))
    }

    fn bootinfo_with(
        log_sink: &'static TestSink,
        audit_sink: &'static TestSink,
        memory_map: BootMemoryMap,
    ) -> BootInfo<'static, TestArch> {
        bootinfo_with_slot(log_sink, audit_sink, memory_map, leak_dispatch_slot())
    }

    fn bootinfo_with_slot(
        log_sink: &'static TestSink,
        audit_sink: &'static TestSink,
        memory_map: BootMemoryMap,
        slot: &'static crate::DispatchCallbackSlot,
    ) -> BootInfo<'static, TestArch> {
        let arch = Arc::new(TestArch::with_cpus(1));
        BootInfo::new(
            0,
            1,
            "",
            memory_map,
            SchedulerConfig::defaults_for(1),
            arch,
            log_sink,
            audit_sink,
            Level::Info,
            slot,
        )
    }

    #[test]
    fn the_sec_phase_installs_the_compiled_identity_into_a_wired_cell() {
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let cell: &'static crate::fs::LateIdentity =
            Box::leak(Box::new(crate::fs::LateIdentity::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map()).with_spawn_identity(cell);

        run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        // The compiled-in system identity is live before any volume
        // exists: a spawn-as-user switch to a service account resolves its
        // primary group and ceiling from the boot-installed table, while a
        // human uid (no on-disk half yet) fails closed.
        assert!(cell.is_installed());
        let (gid, sups, ceiling) = cell
            .resolve_credential(tairix_users::DEVMGR_UID.0)
            .expect("the devmgr service account resolves");
        assert_eq!(gid.0, tairix_users::SERVICES_GID.0);
        assert!(sups.is_empty());
        assert!(ceiling.contains(tairix_abi::CapabilityId::DRV_LOAD));
        assert!(matches!(
            cell.resolve_credential(1000),
            Err(tairix_abi::Errno::PermissionDenied)
        ));
    }

    #[test]
    fn run_phases_emits_each_phase_in_documented_order() {
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());

        match run_phases(boot, log_sink, audit_sink) {
            Ok(_state) => {}
            Err(err) => panic!("phases must succeed, got {err:?}"),
        }

        // Build the expected event sequence: for each phase a
        // `PhaseStarted` followed by a `PhaseReady`.
        let expected: alloc::vec::Vec<u32> = Phase::ORDER
            .iter()
            .flat_map(|_| {
                [
                    AuditEvent::PhaseStarted.id().0,
                    AuditEvent::PhaseReady.id().0,
                ]
            })
            .collect();
        assert_eq!(log_sink.event_ids(), expected);

        // The phase field values must follow the documented order.
        let phases: alloc::vec::Vec<alloc::string::String> = log_sink
            .snapshot()
            .into_iter()
            .filter(|e| e.id == AuditEvent::PhaseStarted.id())
            .map(|e| e.fields[0].1.clone())
            .collect();
        let expected_phases: alloc::vec::Vec<&str> =
            Phase::ORDER.iter().map(|p| p.as_str()).collect();
        assert_eq!(phases, expected_phases);
    }

    #[test]
    fn run_phases_fails_under_mem_on_empty_memory_map() {
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, BootMemoryMap::new());

        match run_phases(boot, log_sink, audit_sink) {
            Ok(_) => panic!("empty memory map must fail mem phase"),
            Err(err) => assert_eq!(err.phase(), Phase::Mem),
        }
    }

    #[test]
    fn spawn_kernel_service_admits_a_kthread_on_the_boot_cpu() {
        // The aarch64 keyboard service rides this seam (`plans/PI.md`
        // P10/P11): a kernel-only kthread admitted alongside PID 1 so the
        // dispatch loop runs it. Building a live `KernelState` through
        // `run_phases` and a `KernelInitSpawner` over it lets us assert the
        // service is admitted onto the boot CPU's scheduler.
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        let (state, process_wait) = run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let ctx = KernelInitSpawner::new(
            state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            &crate::devres::NULL_SHARED_MEM_FACILITY,
        );

        let before = state.scheduler.live_task_count();
        // A trivial body; admission registers the kthread on the run queue
        // without running it (the work runs on the next `step`). The seam
        // returns the admitted task's scheduler id so a caller can wake it
        // (the driver-store server registers it on `SERVE_WAITQ`).
        let first = ctx.spawn_kernel_service(Box::new(|_yielder| {}));
        assert!(first.is_some());
        assert_eq!(state.scheduler.live_task_count(), before + 1);
        // A second service is admitted independently, with a distinct id.
        let second = ctx.spawn_kernel_service(Box::new(|_yielder| {}));
        assert!(second.is_some());
        assert_ne!(first, second);
        assert_eq!(state.scheduler.live_task_count(), before + 2);
    }

    #[test]
    fn service_between_dispatches_tops_up_the_console_transmit() {
        // Regression for the Pi 4 metal serial stall: the dispatch loop's
        // Ran arm must top up the buffered console transmit on **every**
        // dispatch, not only when it reaches the idle park — otherwise a
        // perpetually-runnable in-kernel kthread (the polled USB-keyboard
        // report pump) keeps the loop from ever idling and the log freezes
        // on real silicon (the transmit-FIFO interrupt does not self-sustain
        // the drain). Before the fix the Ran arm only drained deferred wakes
        // and never pumped, so this count stayed `0`.
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        let (state, process_wait) = run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let _ = process_wait;
        assert_eq!(state.arch.pump_console_tx_count(), 0);
        // Each per-dispatch servicing pumps the console transmit exactly
        // once (on top of draining any deferred wake).
        service_between_dispatches(state.arch.as_ref());
        assert_eq!(state.arch.pump_console_tx_count(), 1);
        service_between_dispatches(state.arch.as_ref());
        assert_eq!(state.arch.pump_console_tx_count(), 2);
    }

    #[test]
    fn dispatch_loop_clears_a_stale_tick_before_policy_dispatch() {
        // A CPU index no other test latches, so parallel test threads
        // never observe each other through the process-wide slots.
        const CPU: CpuId = 60;
        let arch = Arc::new(TestArch::with_cpus(CPU + 1));
        arch.set_current_cpu(CPU);
        let scheduler = Scheduler::new(SchedulerConfig::defaults_for(CPU + 1), Arc::clone(&arch))
            .expect("scheduler builds");
        scheduler
            .spawn(CPU, Priority::Normal, |_| {
                tairix_kernel_sched_api::TaskAction::Exit
            })
            .expect("task admitted");

        crate::preempt::note_preempt_tick(CPU);
        run_dispatch_loop(&scheduler, arch.as_ref(), CPU, DispatchRole::Boot);

        assert!(!crate::preempt::take_preempt_pending(CPU));
    }

    #[test]
    fn spawn_driver_process_admits_a_loading_child() {
        // The production `KernelInitSpawner` admits a driver spawn as a
        // *loading* child (`plans/FIX-DESKTOP.md` §2.6.5) and returns its PID
        // at once; the child builds its own image on its own first slice (this
        // host suite never dispatches it). Synchronously observable: a
        // placeholder capability record is installed under the returned id, so
        // the child's first slice resolves a caller context.
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        let (state, process_wait) = run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let ctx = KernelInitSpawner::new(
            state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            &crate::devres::NULL_SHARED_MEM_FACILITY,
        );

        let mut caps = CapabilitySet::empty();
        caps.insert(tairix_abi::CapabilityId::DRV_LOAD);
        let args: [&[u8]; 1] = [b"reply-endpoint"];

        let pid = ctx
            .spawn_driver_process(
                "/System/Drivers/storage/virtio_blk",
                b"driver-image-bytes",
                caps,
                &[],
                &args,
                Some(7),
            )
            .expect("the deferred-load admit returns the driver's PID");
        assert_ne!(pid, 0, "a real scheduler task id is minted");

        // The placeholder record exists and carries an empty effective set:
        // the child derives its `ceiling ∩ manifest` set only when it loads.
        let table = state.caps.read();
        let record = table
            .caps_for(SecTaskId(pid))
            .expect("the admitted loading driver has a placeholder capability record");
        assert!(
            !record.has(tairix_abi::CapabilityId::DRV_LOAD),
            "the placeholder set is empty until the child derives its effective set on load"
        );
    }

    #[test]
    fn spawn_driver_process_attests_the_drivers_name_from_its_store_path() {
        // Regression, twice over: every boot-autoloaded driver used to be
        // admitted with an empty attested name (a blank `ps`/`top` COMMAND),
        // and a store bundle spawned from its generic `Run` entry point was
        // then named `Run`. The seam derives the child's name from the
        // kernel-resolved driver-store path through the one shared rule, so
        // both shapes name the driver itself.
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        let (state, process_wait) = run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let ctx = KernelInitSpawner::new(
            state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            &crate::devres::NULL_SHARED_MEM_FACILITY,
        );

        let mut caps = CapabilitySet::empty();
        caps.insert(tairix_abi::CapabilityId::DRV_LOAD);
        let pid = ctx
            .spawn_driver_process(
                "/System/Drivers/storage/virtio_blk",
                b"driver-image-bytes",
                caps,
                &[],
                &[],
                None,
            )
            .expect("the admitting producer admits the driver");

        let table = state.caps.read();
        let record = table
            .caps_for(SecTaskId(pid))
            .expect("the admitted driver has a capability record");
        assert_eq!(
            record.name(),
            "virtio_blk",
            "a plain store path names the driver by its final component"
        );
        drop(table);

        // The discovered-tier shape: a signed store bundle whose entry
        // point is the generic `Run` leaf. The owning driver directory
        // names the process, never `Run`.
        let mut caps = CapabilitySet::empty();
        caps.insert(tairix_abi::CapabilityId::DRV_LOAD);
        let pid = ctx
            .spawn_driver_process(
                "/System/Drivers/input/usb_kbd/Run",
                b"driver-image-bytes",
                caps,
                &[],
                &[],
                None,
            )
            .expect("the admitting producer admits the bundled driver");

        let table = state.caps.read();
        let record = table
            .caps_for(SecTaskId(pid))
            .expect("the admitted bundled driver has a capability record");
        assert_eq!(
            record.name(),
            "usb_kbd",
            "a bundle's `Run` entry point names its owning driver directory"
        );
    }

    #[test]
    fn terminate_driver_process_reclaims_a_known_driver_and_is_idempotent() {
        // The hot-removal mechanism: when the device manager unloads a
        // driver whose hardware-tree node vanished, the kernel reclaims its
        // capability record (and, on the full path, its scheduler task,
        // grants, served endpoints, and IRQ bindings) and audits the unload.
        // Tearing the same handle down twice is a benign idempotent miss.
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        let (state, process_wait) = run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let ctx = KernelInitSpawner::new(
            state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            &crate::devres::NULL_SHARED_MEM_FACILITY,
        );

        // An unknown handle names no live driver: fail closed, reclaim
        // nothing, never a panic.
        assert_eq!(ctx.terminate_driver_process(0x9999), Err(Errno::NotFound));

        // Make a handle "known" exactly as `admit_process` does for a spawned
        // driver: a capability record minted under its `SecTaskId`.
        let handle = 0x4242u64;
        let sec = SecProcessId(handle);
        let mut caps = CapabilitySet::empty();
        caps.insert(tairix_abi::CapabilityId::DRV_LOAD);
        let record = TaskCapabilities::derive(sec, UserId(0), caps, caps, audit_sink);
        state.caps.write().insert(record);
        assert!(state.caps.read().caps_of_process(sec).is_some());

        audit_sink.clear();
        // Teardown reclaims the capability record and audits the unload.
        assert_eq!(ctx.terminate_driver_process(handle), Ok(()));
        assert!(
            state.caps.read().caps_of_process(sec).is_none(),
            "the driver's capability record is reclaimed"
        );
        assert!(
            audit_sink
                .event_ids()
                .contains(&AuditEvent::DriverUnloaded.id().0),
            "a successful unload is audited"
        );

        // A second teardown of the now-gone handle is a benign idempotent
        // miss — never a panic, never a double-reclaim.
        assert_eq!(ctx.terminate_driver_process(handle), Err(Errno::NotFound));
    }

    /// Unloading a driver stops its whole thread group, not just its leader.
    ///
    /// A user-space driver may create threads of its own, and this teardown
    /// withdraws the state they run against. A sibling left live would keep
    /// executing with its capability record gone and no path left to stop it —
    /// an unkillable runaway holding a core.
    #[test]
    fn terminate_driver_process_stops_every_thread_of_the_group() {
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        let (state, process_wait) = run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let ctx = KernelInitSpawner::new(
            state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            &crate::devres::NULL_SHARED_MEM_FACILITY,
        );

        // A two-thread driver: its leader plus one thread aliased onto the same
        // capability record, exactly as `thread_create` registers one.
        let leader = state
            .scheduler
            .spawn(0, Priority::Normal, |_| {
                tairix_kernel_sched_api::TaskAction::Exit
            })
            .expect("the driver's leader is admitted");
        let sibling = state
            .scheduler
            .spawn(0, Priority::Normal, |_| {
                tairix_kernel_sched_api::TaskAction::Exit
            })
            .expect("the thread it created is admitted");
        let sec = SecProcessId(leader);
        let mut caps = CapabilitySet::empty();
        caps.insert(tairix_abi::CapabilityId::DRV_LOAD);
        state.caps.write().insert(TaskCapabilities::derive(
            sec,
            UserId(0),
            caps,
            caps,
            audit_sink,
        ));
        state
            .caps
            .write()
            .register_thread(SecTaskId(sibling), sec)
            .expect("the sibling aliases the live record");
        let live_before = state.scheduler.live_task_count();

        assert_eq!(ctx.terminate_driver_process(leader), Ok(()));

        assert_eq!(
            state.scheduler.live_task_count(),
            live_before - 2,
            "both threads of the group are retired, not only the leader"
        );
        assert!(
            state.caps.read().caps_of_process(sec).is_none(),
            "the process's capability record goes with its last thread"
        );
        assert_eq!(
            state.caps.read().thread_count(sec),
            0,
            "no thread of the group is left aliased onto a reclaimed record"
        );
    }

    #[test]
    fn run_phases_fails_validation_when_bootinfo_is_bad() {
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let mut boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        boot.boot_cpu = 99; // out of range vs cpu_count = 1.

        match run_phases(boot, log_sink, audit_sink) {
            Ok(_) => panic!("bad bootinfo must fail validation"),
            Err(err) => {
                assert_eq!(err.phase(), Phase::Log);
                assert_eq!(err.cause(), "boot_cpu_out_of_range");
            }
        }
    }

    /// (f4) — the `Syscall` init phase publishes a hook into the
    /// supplied `DispatchCallbackSlot` strictly between the
    /// `PhaseReady{phase=sched}` and the `PhaseStarted{phase=ipc}`
    /// records on the diagnostic log.
    ///
    /// This is the "registration-ordering" invariant the prompt for
    /// (f4) demands: `BootCompleted` cannot fire without
    /// `install_dispatcher` having been called.
    #[test]
    fn syscall_phase_installs_dispatcher_between_sched_and_ipc() {
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let slot = leak_dispatch_slot();
        let boot = bootinfo_with_slot(log_sink, audit_sink, make_memory_map(), slot);

        // Slot is empty up to entry.
        assert!(!slot.is_installed());
        run_phases(boot, log_sink, audit_sink).expect("phases succeed");
        // Hook published.
        assert!(
            slot.is_installed(),
            "Syscall phase must install the dispatch hook"
        );

        // Phase ordering: `sched` ready precedes `syscall` started
        // precedes `ipc` started on the diagnostic log.
        let phase_started_id = AuditEvent::PhaseStarted.id();
        let phase_ready_id = AuditEvent::PhaseReady.id();
        let events = log_sink.snapshot();
        let sched_ready_pos = events
            .iter()
            .position(|e| e.id == phase_ready_id && e.fields[0].1 == "sched")
            .expect("sched ready present");
        let syscall_started_pos = events
            .iter()
            .position(|e| e.id == phase_started_id && e.fields[0].1 == "syscall")
            .expect("syscall started present");
        let syscall_ready_pos = events
            .iter()
            .position(|e| e.id == phase_ready_id && e.fields[0].1 == "syscall")
            .expect("syscall ready present");
        let ipc_started_pos = events
            .iter()
            .position(|e| e.id == phase_started_id && e.fields[0].1 == "ipc")
            .expect("ipc started present");
        assert!(sched_ready_pos < syscall_started_pos);
        assert!(syscall_started_pos < syscall_ready_pos);
        assert!(syscall_ready_pos < ipc_started_pos);
    }

    /// Stage 4.D Item 2-tail.2 — the `Irq` init phase lands strictly
    /// between `Sched` and `Syscall` on the diagnostic log, carrying
    /// the documented `phase = "irq"` field. The `TestArch`
    /// inherits [`KernelArch::irq_routing`]'s
    /// [`IrqRouting::unsupported`] default, so the regression bound
    /// here is *ordering*; the controller-installation contract is
    /// covered by the kernel binary's host tests against the real
    /// IO-APIC controller.
    #[test]
    fn irq_phase_lands_between_sched_and_syscall() {
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let started_id = AuditEvent::PhaseStarted.id();
        let ready_id = AuditEvent::PhaseReady.id();
        let events = log_sink.snapshot();
        let pos = |id: tairix_log::EventId, name: &str| -> usize {
            events
                .iter()
                .position(|e| e.id == id && e.fields[0].1 == name)
                .unwrap_or_else(|| panic!("event {name} missing"))
        };
        let sched_ready = pos(ready_id, "sched");
        let irq_started = pos(started_id, "irq");
        let irq_ready = pos(ready_id, "irq");
        let syscall_started = pos(started_id, "syscall");
        assert!(
            sched_ready < irq_started,
            "irq must follow sched ({sched_ready} < {irq_started})"
        );
        assert!(
            irq_started < irq_ready,
            "irq started precedes irq ready ({irq_started} < {irq_ready})"
        );
        assert!(
            irq_ready < syscall_started,
            "syscall must follow irq ({irq_ready} < {syscall_started})"
        );
    }

    /// (f4) — installing into a slot that already holds a hook
    /// surfaces [`InitError::DispatcherAlreadyInstalled`] under
    /// `Phase::Syscall`, **not** silently overwriting (fail closed).
    #[test]
    fn run_phases_fails_under_syscall_when_slot_already_installed() {
        use crate::dispatch_slot::{DispatchHook, DispatchOutcome};
        use tairix_kernel_syscall::RawArgs;

        struct Dummy;
        impl DispatchHook for Dummy {
            fn dispatch(&self, _raw_number: u64, _args: RawArgs) -> DispatchOutcome {
                DispatchOutcome::NoCallerContext
            }
        }

        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let slot = leak_dispatch_slot();
        // Pre-install a hook so `run_phases` collides.
        let pre: &'static Dummy = Box::leak(Box::new(Dummy));
        slot.install_dispatcher(pre as &'static dyn DispatchHook)
            .expect("pre-install succeeds");

        let boot = bootinfo_with_slot(log_sink, audit_sink, make_memory_map(), slot);
        match run_phases(boot, log_sink, audit_sink) {
            Ok(_) => panic!("must fail when slot is pre-installed"),
            Err(err) => {
                assert_eq!(err.phase(), Phase::Syscall);
                assert_eq!(err.cause(), "syscall_dispatcher_already_installed");
            }
        }

        // The `Syscall` phase emitted a `PhaseStarted` (to mark the
        // attempt) but **not** a `PhaseReady`, and `Ipc` was never
        // reached.
        let events = log_sink.snapshot();
        let syscall_started = events
            .iter()
            .filter(|e| e.id == AuditEvent::PhaseStarted.id() && e.fields[0].1 == "syscall")
            .count();
        let syscall_ready = events
            .iter()
            .filter(|e| e.id == AuditEvent::PhaseReady.id() && e.fields[0].1 == "syscall")
            .count();
        let ipc_started = events
            .iter()
            .filter(|e| e.id == AuditEvent::PhaseStarted.id() && e.fields[0].1 == "ipc")
            .count();
        assert_eq!(syscall_started, 1);
        assert_eq!(syscall_ready, 0);
        assert_eq!(ipc_started, 0);
    }
}
