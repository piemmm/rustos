//! Architecture-neutral kernel entry and init sequencing.
//!
//! The contract documented in `docs/src/architecture/kernel.md` is:
//!
//! 1. [`kernel_main`] is the *only* public entry point the
//!    architecture port calls after it has built a [`BootInfo`].
//! 2. Subsystems initialise in a fixed order — `log → mem → sec →
//!    sched → ipc` — and that order is part of the kernel ABI:
//!    re-ordering would change which audit events external consumers
//!    observe (`AGENTS.md` §2.4, no interface creep).
//! 3. A failure in any phase is **fatal**: the failed-phase event is
//!    logged and the boot CPU enters [`KernelArch::halt`]. The kernel
//!    never silently resets (`AGENTS.md` §2 Stage 2 deliverables).
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

use crate::sched::{CpuId, SchedError, Scheduler, SchedulerArch};
use rustos_caps::CapabilitySet;
use rustos_kernel_ipc::PortRegistry;
use rustos_kernel_irq::{IrqController, IrqTable};
use rustos_kernel_mem::{AllocError, FrameAllocator, PhysMap, UserAddressSpace};
use rustos_kernel_sched_api::{Priority, StepOutcome};
use rustos_kernel_sec::{CapTable, IdentityTable, TaskCapabilities, TaskId as SecTaskId, UserId};
use rustos_log::{log, set_max_level, Event, Field, Level, Sink};
use rustos_sync::RwLock;

use crate::aspace::AddressSpaceRegistry;
use crate::audit::AuditEvent;
use crate::bootinfo::{BootInfo, BootInfoError, IrqRouting, KernelArch};
use crate::dispatch_slot::AlreadyInstalledError;
use crate::random::{BootReserve, RandomReserve};
use crate::spawn::InitSpawnCtx;
use crate::syscalls::KernelDispatchHook;

/// Ordered identifier of every subsystem init phase orchestrated by
/// [`kernel_main`].
///
/// The numeric ordering is meaningful: phase `N` must complete before
/// phase `N+1` begins, and the order is the audit-log contract with
/// external consumers (`AGENTS.md` §5.4).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Phase {
    /// Install the global log sink and level filter.
    Log,
    /// Construct the physical [`FrameAllocator`] from the boot memory
    /// map.
    Mem,
    /// Verify and freeze the bootstrap identity table.
    Sec,
    /// Build the SMP scheduler.
    Sched,
    /// Consult the architecture port's [`crate::KernelArch::irq_routing`]
    /// and construct the kernel-wide [`rustos_kernel_irq::IrqTable`].
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
    /// `kernel/sec` rejected the bootstrap identity table.
    Sec(rustos_abi::Errno),
    /// `kernel/sched` rejected the scheduler configuration.
    Sched(SchedError),
    /// The bin-crate [`crate::DispatchCallbackSlot`] already held a
    /// hook when the `Syscall` phase attempted to publish ours.
    ///
    /// The slot is set-once per boot; a second publish indicates a
    /// programmer error (e.g. a test harness pre-installed a hook,
    /// or `kernel_main` was re-entered). `AGENTS.md` §5.4.5 — fail
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
            InitError::Sched(_) => Phase::Sched,
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
            InitError::DispatcherAlreadyInstalled(_) => "syscall_dispatcher_already_installed",
        }
    }
}

/// Emit a phase event through a `Sink`.
fn emit(sink: &(dyn Sink + Sync), level: Level, event: AuditEvent, fields: &[Field<'_>]) {
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
///   [`KernelArch::halt`]. *Never* silently resets — see `AGENTS.md`
///   §2 Stage 2 deliverables.
///
/// # SAFETY-INVARIANTs verified at entry
///
/// * `boot.validate()` is invoked before any other work — every field
///   invariant documented on [`BootInfo`] is checked.
/// * `boot.boot_cpu == boot.arch.current_cpu()` — re-asserted as a
///   `debug_assert_eq!` so production builds pay no extra cost while
///   debug builds catch arch porting bugs that would route IPIs to the
///   wrong CPU (`AGENTS.md` §1, §2.10).
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
    // The arch port's PID-1 spawn seam (`plans/PI.md` P6c-3), captured
    // before `boot` is consumed by `run_phases`. `Option<&dyn _>` is
    // `Copy`, so this is a copy of the reference, not a move.
    let init_spawn = boot.init;

    // `BootStarted` / `BootCompleted` / `PhaseFailed` are audit
    // lifecycle events (`AGENTS.md` §5.4.4 — security-relevant
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
            value: "7",
        }],
    );

    // SAFETY-INVARIANT: `boot_cpu` must equal `arch.current_cpu()` at
    // entry. We re-assert in debug builds *after* `boot.validate()`
    // has rejected obviously-malformed handovers — that way an
    // out-of-range `boot_cpu` is reported as a structured
    // `BootInfoError::BootCpuOutOfRange` audit record rather than as
    // a debug-mode assertion (`AGENTS.md` §2.10, §5.4 — fail closed
    // with a stable cause string).
    if boot.validate().is_ok() {
        debug_assert_eq!(
            boot.boot_cpu,
            boot.arch.current_cpu(),
            "BootInfo.boot_cpu disagrees with KernelArch::current_cpu()",
        );
    }

    // Per-phase orchestration. On failure we halt via the arch port.
    let state = match run_phases(boot, log_sink, audit_sink) {
        Ok(state) => state,
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
                        value: phase.as_str(),
                    },
                    Field {
                        key: "cause",
                        value: cause,
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
            value: "spawn_init",
        }],
    );

    // Spawn PID 1 (`init`) into user mode when the arch port installed a
    // spawn seam (`plans/PI.md` P6c-3). On success the seam diverges into
    // the spawned program and never returns; on failure (or when no seam
    // is installed) we fall through to the fail-closed halt below
    // (`AGENTS.md` §2.9 — never silently reset).
    if let Some(init) = init_spawn {
        // The core-side registration context the seam drives: it builds
        // the arch image (through the public `spawn_image` caller) and
        // hands it back through `admit_init`, which registers the task with
        // this kernel state's scheduler / capability table / address-space
        // registry and dispatches it. Every borrow targets the leaked
        // `KernelState`, which lives for the running kernel's lifetime.
        let ctx = KernelInitSpawner {
            frames: &state.frame_allocator,
            audit: audit_sink,
            scheduler: &state.scheduler,
            caps: &state.caps,
            aspaces: &state.aspaces,
            arch: state.arch.as_ref(),
        };
        init.spawn_init(&ctx);
    }

    arch_for_halt.halt();
}

/// The concrete [`InitSpawnCtx`] [`kernel_main`] hands the arch
/// [`crate::InitSpawn`] seam to spawn PID 1 (`plans/PI.md` P6c-3).
///
/// It borrows the live kernel registries from the leaked [`KernelState`]
/// so the seam can register the freshly built `init` task (scheduler,
/// capability table, address-space registry) and dispatch it without ever
/// naming the concrete scheduler or arch types itself (`AGENTS.md` §17.2 /
/// §17.4 — the generics stay on this side of the object-safe boundary).
struct KernelInitSpawner<'a, A: KernelArch> {
    frames: &'a FrameAllocator,
    audit: &'static (dyn Sink + Sync),
    scheduler: &'a Scheduler<A>,
    caps: &'a RwLock<CapTable>,
    aspaces: &'a RwLock<AddressSpaceRegistry>,
    arch: &'a A,
}

impl<A: KernelArch> InitSpawnCtx for KernelInitSpawner<'_, A> {
    fn frames(&self) -> &FrameAllocator {
        self.frames
    }

    fn audit(&self) -> &(dyn Sink + Sync) {
        self.audit
    }

    unsafe fn admit_init(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        pre_resume: Box<dyn FnMut() + Send>,
        mut enter: Box<dyn FnMut() + Send>,
    ) {
        let cpu: CpuId = SchedulerArch::current_cpu(self.arch);

        // Admit PID 1 as a resumable **user kthread** (`plans/SPAWN.md`
        // SP2): the work body performs the user-mode transition on the
        // task's own kernel stack, and the `pre_resume` hook reactivates
        // PID 1's address-space root before every switch into it so it
        // `eret`s back into EL0 under the correct translation regime.
        // `enter` diverges into EL0, so the work never returns through the
        // trampoline's terminal `Exit` — PID 1 leaves EL0 only through a
        // rescheduling syscall (`yield`/`exit`), whose trap path suspends
        // it back to the scheduler. The unit `()` the work yields satisfies
        // the `FnMut(&mut Yielder<_>)` body signature for the (impossible)
        // case the transition ever returned.
        let work = move |_yielder: &mut crate::kthread::Yielder<A::Cs>| {
            enter();
        };
        let cs = self.arch.context_switch();
        let Ok(task_id) = crate::kthread::spawn_user_kthread(
            self.scheduler,
            cs,
            cpu,
            Priority::Normal,
            pre_resume,
            work,
        ) else {
            // The home queue could not admit the task — fail closed: return
            // so the seam (and then `kernel_main`) halts the CPU.
            return;
        };

        // Register the task's caps under the *same* numeric id the
        // dispatcher recovers (`SecTaskId(current_task)`), so PID 1's first
        // syscall resolves a caller context (`AGENTS.md` §5.4.1). `init`'s
        // effective set is the intersection of its user grant and manifest
        // request; the boot path passes the system grant, so use it for both
        // bounds (uid 0 — the system user, `AGENTS.md` §5.1).
        let sec_id = SecTaskId(task_id);
        let record = TaskCapabilities::derive(sec_id, UserId(0), caps, caps, self.audit);
        self.caps.write().insert(record);

        // Register PID 1's frozen address space + direct map under the same
        // id, so a first syscall that copies from user memory (e.g.
        // `console_write` reading `init`'s banner) resolves the caller's
        // mappings instead of failing closed with `BadAddress`
        // (`plans/PI.md` P6c-3 follow-up). A fresh task id is never already
        // present; should registration nonetheless be refused, fail closed
        // by returning so the seam (and `kernel_main`) halts the CPU
        // (`AGENTS.md` §2.9) rather than entering a program whose user
        // memory the kernel cannot reach.
        if self
            .aspaces
            .write()
            .register(sec_id, space, physmap)
            .is_err()
        {
            return;
        }

        // Drive PID 1 (and anything it spawns) to completion: each `step`
        // dispatches the next runnable task on this CPU. The first step
        // sets the per-CPU current task to PID 1, runs its `pre_resume`
        // hook, and switches into it; control returns here when the task
        // suspends through a rescheduling syscall (`yield`/`exit`) or its
        // kernel stack could not seed a frame (fail-closed `Exit`). The
        // loop stops once no task is live or the CPU idles, then returns so
        // `kernel_main` halts fail-closed (`AGENTS.md` §2.9). A real
        // session frontend that never exits is `plans/SPAWN.md` SP4.
        //
        // SAFETY: the seam built PID 1's image into and switched to the
        // active address space before calling here, and the EL1/trap vector
        // is installed, so the new program's first syscall is handled (this
        // method's contract); the `pre_resume` hook keeps the correct root
        // active across every later switch into a user kthread.
        let _ = task_id;
        loop {
            match self.scheduler.step(cpu) {
                Ok(StepOutcome::Ran(_)) if self.scheduler.live_task_count() > 0 => {}
                Ok(_) | Err(_) => break,
            }
        }
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
/// The function is intentionally non-public — external callers go
/// through [`kernel_main`]. Splitting it out lets the unit tests in
/// this module assert phase-by-phase behaviour without the trailing
/// `arch.halt()` swallowing the test thread.
fn run_phases<A: KernelArch>(
    boot: BootInfo<'_, A>,
    log_sink: &(dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
) -> Result<&'static KernelState<A>, InitError> {
    // Pre-flight: re-validate the handover before logging Phase::Log
    // started — a malformed BootInfo means we cannot even trust the
    // log_level we just installed.
    boot.validate().map_err(InitError::BadBootInfo)?;

    let BootInfo {
        memory_map,
        identity,
        scheduler_config,
        arch,
        dispatcher_callback_slot,
        console,
        ..
    } = boot;

    // Phase 1 — Log. The filter was already installed before we
    // arrived; the explicit phase event marks the transition for
    // external consumers tracking the boot timeline.
    phase_started(log_sink, Phase::Log);
    phase_ready(log_sink, Phase::Log);

    // Phase 2 — Mem.
    phase_started(log_sink, Phase::Mem);
    let frame_allocator = FrameAllocator::new(&memory_map).map_err(InitError::Mem)?;
    phase_ready(log_sink, Phase::Mem);

    // Phase 3 — Sec. The identity-table verifier emits its own audit
    // record on the `audit_sink`; we still emit our phase markers on
    // the diagnostic `log_sink` so the two streams stay aligned.
    phase_started(log_sink, Phase::Sec);
    let identity_table = identity.verify(audit_sink).map_err(InitError::Sec)?;
    phase_ready(log_sink, Phase::Sec);

    // Phase 4 — Sched.
    phase_started(log_sink, Phase::Sched);
    let scheduler =
        Scheduler::new(scheduler_config, Arc::clone(&arch)).map_err(InitError::Sched)?;
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
    // `irq_bind` / `irq_wait` (`AGENTS.md` §5.4.5 — fail closed). The
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
    // `'static` slot, not a global *mutable* static (`AGENTS.md`
    // §2.1 — the per-CPU bootstrap area is the only sanctioned
    // mutable static; this allocation is immutable after creation
    // because every interior field carries its own synchronisation
    // primitive (`Scheduler`'s internal locks, `RwLock<CapTable>`)).
    let state: &'static KernelState<A> = Box::leak(Box::new(KernelState {
        frame_allocator,
        identity_table,
        scheduler,
        caps: RwLock::new(CapTable::new()),
        ipc: RwLock::new(PortRegistry::new()),
        aspaces: RwLock::new(AddressSpaceRegistry::new()),
        // The kernel random output reserve boots **unseeded** over the
        // `NullEntropy` source (`AGENTS.md` §22): a reserve always
        // exists, but `random_get` fails closed with `EntropyNotReady`
        // until the platform-RNG entropy seam (§17.2) re-seeds it — the
        // same seam the encrypted-swap key is drawn from (§4), still
        // pending. Boxed as a `dyn RandomReserve` so the boot reserve and
        // a later seeded one share one field type.
        rng: RwLock::new(Box::new(BootReserve::new()) as Box<dyn RandomReserve + Send + Sync>),
        arch,
        audit_sink,
        irq: irq_table,
        irq_controller,
    }));

    // Hand the arch port a `'static` reference to the freshly
    // constructed IrqTable so its external-IRQ trap dispatcher can
    // translate a vector-level hit to `IrqTable::fire`. The default
    // [`KernelArch::install_irq_dispatch`] is a no-op; real arch
    // ports (x86_64) override it to publish the reference into the
    // arch crate's dispatcher slot (set-once per boot — `AGENTS.md`
    // §2.1).
    state.arch.install_irq_dispatch(&state.irq);

    // Phase 6 — Syscall. Publish the production `DispatchHook` into
    // the bin-crate-owned slot. The hook itself is `Box::leak`'d for
    // the same reason as `KernelState`: its borrows reference
    // `KernelState` fields and must therefore be `'static`.
    phase_started(log_sink, Phase::Syscall);
    let hook: &'static KernelDispatchHook<'static, A> =
        Box::leak(Box::new(KernelDispatchHook::new(
            &state.scheduler,
            &state.caps,
            state.arch.as_ref(),
            audit_sink,
            &state.irq,
            state.irq_controller,
            &state.ipc,
            &state.aspaces,
            &state.rng,
            console,
        )));
    dispatcher_callback_slot
        .install_dispatcher(hook)
        .map_err(InitError::DispatcherAlreadyInstalled)?;
    phase_ready(log_sink, Phase::Syscall);

    // Phase 6 — Ipc. The named-port registry is composed into
    // `KernelState` above (`ipc: RwLock<PortRegistry>`) and borrowed by
    // the `KernelDispatchHook` so the `ipc_send` / `ipc_recv` handlers
    // resolve an endpoint against a live, kernel-owned map. It boots
    // empty — every endpoint is published at runtime by the binder that
    // holds the bind authority (`AGENTS.md` §5.2); the phase event fires
    // so the boot timeline is uniform.
    phase_started(log_sink, Phase::Ipc);
    phase_ready(log_sink, Phase::Ipc);

    Ok(state)
}

/// In-memory record of the live kernel subsystems built by
/// [`run_phases`].
///
/// Lives for the lifetime of the running kernel: `kernel_main`
/// `Box::leak`s the value so the `Phase::Syscall` step can publish a
/// `'static dyn DispatchHook` referencing its fields. The kernel
/// never returns from `kernel_main`'s halt, so the leak is a
/// one-shot publish, not a global mutable static (`AGENTS.md` §2.1).
///
/// `kernel_main` keeps the `'static` reference around for the
/// duration of the audit/halt sequence; future stages will hand it to
/// the scheduler dispatch loop.
pub(crate) struct KernelState<A: KernelArch> {
    #[allow(dead_code)] // Stage 4 will wire the allocator into the driver host.
    pub(crate) frame_allocator: FrameAllocator,
    #[allow(dead_code)] // Stage 5 will wire the table into the VFS.
    pub(crate) identity_table: IdentityTable,
    pub(crate) scheduler: Scheduler<A>,
    /// Per-task capability registry. The `KernelDispatchHook` reads
    /// this on every syscall; future `cap_delegate` / `cap_revoke`
    /// handlers write to it. Wrapped in a reader-preferring
    /// `RwLock` so the syscall hot path takes only a shared lock
    /// (mirrors `Scheduler::tasks`'s composition strategy).
    pub(crate) caps: RwLock<CapTable>,
    /// Named-port registry. The `KernelDispatchHook` reads this on
    /// every `ipc_send` / `ipc_recv` to resolve the endpoint carried
    /// in the syscall against the live, kernel-owned [`PortRegistry`];
    /// the binder that holds the bind authority publishes endpoints
    /// into it at runtime (`AGENTS.md` §5.2). Wrapped in the same
    /// reader-preferring `RwLock` as `caps` so the syscall hot path
    /// takes only a shared lock and the kernel composes both
    /// registries under one lock-ordering policy (`AGENTS.md` §2.1 —
    /// the registry itself owns no lock, mirroring `CapTable`).
    pub(crate) ipc: RwLock<PortRegistry>,
    /// Per-task address-space registry backing the kernel's
    /// `copy_from_user` / `copy_to_user` boundary (`AGENTS.md` §5.4).
    /// Maps a task's [`rustos_kernel_sec::TaskId`] to its user
    /// [`rustos_kernel_mem::AddressSpace`] and the [`PhysMap`] that
    /// backs it, so a syscall handler can resolve the caller's task id
    /// to the pair [`rustos_kernel_mem::uaccess`] walks. Wrapped in the
    /// same reader-preferring `RwLock` as `caps` / `ipc` so the syscall
    /// hot path takes only a shared lock and the kernel composes every
    /// registry under one lock-ordering policy (`AGENTS.md` §2.1 — the
    /// registry owns no lock of its own).
    ///
    /// [`PhysMap`]: rustos_kernel_mem::PhysMap
    //
    // Increment C (`PLAN.md` Stage 7) threads this registry into the
    // `KernelDispatchHook` / `KernelSyscallHandlers` below, where
    // `with_caller_aspace` reads it to resolve the caller's task id to
    // its address space. Increment D wires the deferred syscalls' copy
    // path through that accessor. The registry boots empty: entries are
    // populated by the spawner and withdrawn on `exit` once those call
    // sites reach it.
    pub(crate) aspaces: RwLock<AddressSpaceRegistry>,
    /// The kernel's single cryptographic random output reserve
    /// (`AGENTS.md` §22). The `KernelDispatchHook` borrows it so
    /// `random_get` draws CSPRNG output from it before copying the
    /// bytes into the caller's buffer. It boots **unseeded** over the
    /// [`NullEntropy`](crate::random::NullEntropy) source, so a draw fails closed with
    /// [`rustos_abi::Errno::EntropyNotReady`] until the platform-RNG
    /// entropy seam (`AGENTS.md` §17.2) re-seeds the boxed reserve in
    /// place. Held type-erased behind a `Box<dyn RandomReserve>` and
    /// wrapped in the same reader-preferring `RwLock` as `caps` / `ipc`
    /// / `aspaces` (the draw takes the write guard because the reserve
    /// mutates its buffer as it serves, `AGENTS.md` §2.1).
    pub(crate) rng: RwLock<Box<dyn RandomReserve + Send + Sync>>,
    pub(crate) arch: Arc<A>,
    /// Audit sink the dispatch hook emits security-relevant records
    /// through. Held here so the hook borrows it for the lifetime of
    /// `KernelState` rather than re-discovering it at syscall time.
    #[allow(dead_code)]
    // Borrowed by the leaked `KernelDispatchHook`; tests assert through observers.
    pub(crate) audit_sink: &'static (dyn Sink + Sync),
    /// Kernel IRQ table backing the `irq_bind` / `irq_wait`
    /// syscalls. The `irq_bind` handler binds against the calling
    /// task's [`rustos_kernel_sec::TaskId`]; the `irq_wait` handler
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
    /// [`rustos_kernel_irq::UNSUPPORTED_CONTROLLER`] (every `mask`
    /// returns [`rustos_kernel_irq::MaskError::Unsupported`]); ports
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
            value: phase.as_str(),
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
            value: phase.as_str(),
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
    use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
    use rustos_kernel_sec::IdentityTableBuilder;
    use rustos_log::Level;

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
        // with `Box::leak` (`AGENTS.md` §2.9 — permitted in tests).
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
            IdentityTableBuilder::new(),
            SchedulerConfig::defaults_for(1),
            arch,
            log_sink,
            audit_sink,
            Level::Info,
            slot,
        )
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
        let pos = |id: rustos_log::EventId, name: &str| -> usize {
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
    /// `Phase::Syscall`, **not** silently overwriting (`AGENTS.md`
    /// §5.4.5 — fail closed).
    #[test]
    fn run_phases_fails_under_syscall_when_slot_already_installed() {
        use crate::dispatch_slot::{DispatchHook, DispatchOutcome};
        use rustos_kernel_syscall::RawArgs;

        struct Dummy;
        impl DispatchHook for Dummy {
            fn dispatch(&self, _raw_number: u16, _args: RawArgs) -> DispatchOutcome {
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
