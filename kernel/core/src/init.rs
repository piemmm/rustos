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

use crate::sched::{CpuId, SchedError, Scheduler, SchedulerArch};
use rustos_abi::hwtree::HwResource;
use rustos_abi::{DescriptorTable, Errno};
use rustos_caps::CapabilitySet;
use rustos_kernel_ipc::PortRegistry;
use rustos_kernel_irq::{IrqController, IrqTable};
use rustos_kernel_mem::{AllocError, FrameAllocator, PhysMap, UserAddressSpace};
use rustos_kernel_sched_api::{Priority, StepOutcome};
use rustos_kernel_sec::{
    CapTable, IdentityTable, ProcName, TaskCapabilities, TaskId as SecTaskId, UserId,
};
use rustos_log::{set_max_level, Field, Level, Sink};
use rustos_sync::RwLock;
use rustos_util::fmt::format_hex_u64;

use crate::aspace::AddressSpaceRegistry;
use crate::audit::{emit, AuditEvent};
use crate::bootinfo::{BootInfo, BootInfoError, IrqRouting, KernelArch};
use crate::dispatch_slot::AlreadyInstalledError;
use crate::procwait::{KernelProcessWait, ProcessWait};
use crate::random::{BootReserve, RandomReserve};
use crate::spawn::{InitSpawnCtx, ProcessSpawn};
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
    // The arch port's PID-1 spawn seam (`plans/PI.md` P6c-3), captured
    // before `boot` is consumed by `run_phases`. `Option<&dyn _>` is
    // `Copy`, so this is a copy of the reference, not a move.
    let init_spawn = boot.init;

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
            value: rustos_log::FieldValue::Str("7"),
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
                        value: rustos_log::FieldValue::Str(phase.as_str()),
                    },
                    Field {
                        key: "cause",
                        value: rustos_log::FieldValue::Str(cause),
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
            value: rustos_log::FieldValue::Str("spawn_init"),
        }],
    );

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
            &state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            build_shared_mem_facility(state.arch.as_ref(), &state.frame_allocator),
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
    fn unpark(&self, id: rustos_kernel_sched_api::TaskId) {
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
        cpu: rustos_kernel_sched_api::CpuId,
    ) -> Option<rustos_kernel_sched_api::TaskId> {
        // The live scheduler's per-CPU current-task slot — the same slot the
        // dispatch hook reads to identify a syscall caller. A console-read backing parks the *current* task without
        // being handed its id, so it resolves it here.
        self.scheduler.current_task(cpu)
    }

    fn current_cpu(&self) -> Option<rustos_kernel_sched_api::CpuId> {
        // The arch port's per-CPU identity — the same value the scheduler
        // and timed-wake paths read. A blocking primitive reached without a
        // caller context (a `SleepLock` contended acquire) resolves the
        // current CPU here to then look up and park the current task.
        Some(SchedulerArch::current_cpu(self.arch))
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
}

/// Seed the kernel CSPRNG output reserve from the arch port's platform
/// entropy source, replacing the unseeded `NullEntropy` boot reserve.
///
/// Fail-soft and audited (a security-relevant state change is logged): when
/// the port exposes a usable source and a draw produces bytes, a
/// [`crate::random::SeededReserve`] is installed and `random_get` begins
/// serving cryptographic output; otherwise the reserve is left unseeded so
/// every draw keeps failing closed with
/// [`rustos_abi::Errno::EntropyNotReady`] — never weakened to predictable
/// bytes. There is no panic and no busy-wait: a momentarily-underfull source
/// is the port's bounded-retry concern, and a hard failure simply leaves the
/// reserve unseeded.
fn seed_entropy_reserve<A: KernelArch + 'static>(state: &'static KernelState<A>) {
    use crate::random::{
        ArchEntropy, ArchTicks, IrqEntropyObserver, SeededReserve, IRQ_ENTROPY_POOL,
    };
    use rustos_rng::{EntropySource, InterruptPoolSource, JitterSource, MixedPair};

    let Some(source) = state.arch.platform_entropy() else {
        emit(
            state.audit_sink,
            Level::Info,
            AuditEvent::EntropyReserveUnseeded,
            &[Field {
                key: "cause",
                value: rustos_log::FieldValue::Str("no_source"),
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
                value: rustos_log::FieldValue::Str("source_pending"),
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
    let sources = if jitter_healthy {
        "hardware+jitter"
    } else {
        "hardware"
    };

    // Add the asynchronous interrupt-arrival-timing pool as a third,
    // independent source. It contributes nothing at boot (it fails closed
    // until interrupts have flowed) but folds fresh timing into every reseed
    // for forward secrecy; the interrupt observer that feeds it is installed
    // below, only once a seeded reserve exists to drain it.
    let interrupt = InterruptPoolSource::new(&IRQ_ENTROPY_POOL);
    let mixed = MixedPair::new(MixedPair::new(hardware, jitter), interrupt);
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
                    value: rustos_log::FieldValue::Str(sources),
                }],
            );
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
                    value: rustos_log::FieldValue::Str("draw_failed"),
                }],
            );
        }
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
    fn service_between_dispatches(&self) {
        let _ = crate::waitq::drain_pending_wakes();
        // Deliver any foreground `^C`/`^Z` the console line discipline
        // queued from interrupt context: like the deferred wakes, the actual
        // scheduler-driving delivery runs here, where taking the run-queue
        // locks is safe.
        let _ = crate::procsignal::drain_pending_foreground();
        self.arch.pump_console_tx();
    }
}

impl<A: KernelArch + 'static> InitSpawnCtx for KernelInitSpawner<'_, A> {
    fn frames(&self) -> &FrameAllocator {
        self.frames
    }

    fn audit(&self) -> &(dyn Sink + Sync) {
        self.audit
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn admit_init(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        stack: Box<dyn crate::kthread::KernelStack + Send>,
        pre_resume: Box<dyn FnMut(u64) + Send>,
        live: Option<Box<dyn rustos_kernel_mem::LiveUserSpace + Send>>,
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
        // When the seam retained a live, mutable address space, admit PID 1
        // with it so its `mem_map` / `mmio_map` syscalls mutate its own
        // space through the per-CPU live-space slot (`plans/PI.md`
        // 5d-0-ii (b′)); otherwise admit the plain form and those syscalls
        // fail closed.
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
            ),
            None => crate::kthread::spawn_user_kthread_with_stack(
                self.scheduler,
                cs,
                stack,
                cpu,
                Priority::Normal,
                pre_resume,
                work,
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
        let sec_id = SecTaskId(task_id);
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
            return;
        }

        // Establish PID 1's standard streams: the
        // standard descriptor table (`stdin` readable,
        // `stdout`/`stderr`/`stdinfo` writable), each backed by the
        // discovered console the boot path installed, so `init` writes
        // its banner through `stream_write(STDOUT, …)` over an inherited
        // stream rather than an ambient device.
        self.aspaces
            .write()
            .set_streams(sec_id, DescriptorTable::standard());

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
        let _ = task_id;
        // Run the dispatch loop with device IRQs **enabled** so RustOS is
        // fully preemptive: every in-kernel task and
        // kthread the loop dispatches executes with interrupts deliverable,
        // so a long in-kernel operation (a slow MMIO bring-up read, a busy
        // driver poll) can no longer mask interrupts for its whole span and
        // starve the preemption one-shot, the buffered-serial transmit
        // drain, or an interrupt-driven waiter — the cooperative
        // dispatch loop the charter forbids. A device IRQ taken mid-task services
        // its source and returns to the same task (the kernel stays
        // non-preemptible); its lock-free handler flags a deferred wake
        // that `drain_pending_wakes` performs here, in dispatcher context,
        // where taking the scheduler/run-queue locks is safe.
        self.arch.set_device_irqs(true);
        loop {
            match self.scheduler.step(cpu) {
                // A task ran. Keep dispatching while live tasks remain;
                // stop once every task has exited so `kernel_main` halts.
                Ok(StepOutcome::Ran(_)) => {
                    if self.scheduler.live_task_count() == 0 {
                        break;
                    }
                    // Service the per-dispatch background work (deferred
                    // wakes + buffered console transmit) now that the task
                    // has suspended and no kernel lock / scheduler critical
                    // section is in flight.
                    self.service_between_dispatches();
                }
                // No runnable task this step. If every live task has
                // exited, the system is finished — break so `kernel_main`
                // halts fail-closed. Otherwise the live
                // tasks are all **parked** (a perpetual service blocked in
                // a blocking-wait syscall, e.g. `devmgr` on `hw_tree_wait`):
                // park the CPU until the next interrupt, then re-step —
                // never busy-spin (tickless idle).
                Ok(StepOutcome::Idle) => {
                    if self.scheduler.live_task_count() == 0 {
                        break;
                    }
                    // Race-free park: mask device IRQs, then drain once more
                    // so a wake a handler flagged just before we commit to
                    // sleep is observed and re-dispatched rather than slept
                    // through (no lost wake-up). If
                    // nothing became runnable, top up the buffered console
                    // transmit one last time (so a port whose transmit FIFO
                    // is the `wfi` wake source has it armed against the
                    // remaining backlog) and `wait_for_interrupt` parks
                    // on a `wfi`-class instruction that still wakes on the
                    // pending-but-masked interrupt; re-enabling IRQs then
                    // *takes* it, its handler flags the wake, and the loop
                    // re-steps and drains it.
                    self.arch.set_device_irqs(false);
                    let woke = crate::waitq::drain_pending_wakes();
                    // A queued foreground signal is dispatchable work too: a
                    // `^C` typed while every task is parked must terminate
                    // the foreground job now, not after the next unrelated
                    // interrupt.
                    let delivered = crate::procsignal::drain_pending_foreground();
                    if !(woke || delivered) {
                        self.arch.pump_console_tx();
                        self.arch.wait_for_interrupt();
                    }
                    self.arch.set_device_irqs(true);
                }
                Err(_) => break,
            }
        }
        // Leave device IRQs masked before returning to `kernel_main`'s
        // fail-closed halt: there is no dispatcher left to service them.
        self.arch.set_device_irqs(false);
    }

    fn spawn_kernel_service(
        &self,
        mut body: crate::kthread::KernelServiceBody,
    ) -> Option<rustos_kernel_sched_api::TaskId> {
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
        spawn: &dyn ProcessSpawn,
        path: &str,
        rxe: &[u8],
        caps: CapabilitySet,
        grants: &[HwResource],
        args: &[&[u8]],
        node_id: Option<u32>,
    ) -> Result<u64, Errno> {
        // Build the production runtime-spawn context over the same live
        // subsystems PID-1 admission uses and drive the architecture's
        // `ProcessSpawn::spawn_with` (`plans/SPAWN.md` SP3). The bin-crate
        // caller never names `Scheduler<A>` / `KernelSpawnCtx` — that
        // assembly happens here, behind the object-safe `InitSpawnCtx`
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
            SecTaskId(0),
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
            // A boot-autoloaded driver is a kernel-trusted system principal:
            // admit it under the fixed system credential (uid 0 / gid 0), the
            // spawn-as-user counterpart of the `SecTaskId(0)` supervisor
            // identity above. uid 0 carries no ambient authority; the driver's
            // powers flow only from `caps`.
            SpawnCredential::system(),
        );
        // A boot-floor driver reads its configuration from its argument
        // vector alone; it inherits no environment (there is no principal
        // yet whose exported variables it could meaningfully receive).
        spawn.spawn_with(rxe, &ctx, caps, args, &[])
    }

    fn terminate_driver_process(&self, handle: u64) -> Result<(), Errno> {
        // The handle is the driver's PID, which is its scheduler task id and,
        // equally, the numeric its security id was minted under
        // (`admit_process` builds `SecTaskId(task_id)`). Reclaim every
        // kernel-held piece of the driver under that one id.
        let sched_id = handle;
        let sec_id = SecTaskId(handle);

        // Presence is keyed on the address-space registry entry every spawned
        // driver registers: if neither it nor a capability record exists, no
        // live driver bears this handle, so the unload is a benign idempotent
        // miss (the device manager may diff the same vanished node twice).
        let known =
            self.aspaces.read().contains(sec_id) || self.caps.read().caps_for(sec_id).is_some();
        if !known {
            return Err(Errno::NotFound);
        }

        // Reap the scheduler task: mark it Exited (never dispatched again) and
        // drop its body, reclaiming its kernel stack, live address space, and
        // page-table frames. A parked driver (the common case — a driver
        // blocked in `irq_wait` / a served-endpoint park) drops immediately;
        // a vanished id is a benign no-op. Idempotent, never a panic.
        let _ = self.scheduler.exit(sched_id);

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
        crate::sharedreg::reclaim_task(self.shared_mem_facility, sec_id);

        // Destroy every synchronous call endpoint the driver served before
        // dropping its capability record, mirroring the `exit` syscall: a
        // user-space service that is torn down must not leave callers blocked
        // in `ipc_call` forever — destroying its endpoints cancels their
        // in-flight calls, and waking `CALL_WAITQ` re-runs each parked caller's
        // poll so it abandons fail-closed.
        if crate::callreg::unregister_owned_by(handle, self.audit) > 0 {
            crate::waitq::call_wake();
        }

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
                value: rustos_log::FieldValue::Str(format_hex_u64(handle, &mut handle_buf)),
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
        memory_map,
        identity,
        scheduler_config,
        arch,
        dispatcher_callback_slot,
        consoles,
        programs,
        spawn_service,
        app_store,
        seat_registry,
        users_db,
        users_admin,
        hw_tree,
        filesystem,
        volumes,
        spawn_identity,
        kernel_heap_bytes,
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
        identity_table,
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
    // the per-task retained live address space (`plans/PI.md` 5d-0-ii (b′)/(c)):
    // each routes a syscall to the calling task's *own* live space via the
    // per-CPU slot, reading the current CPU from the same `'static` arch handle
    // the process-wait producer uses, so a task that retains a live space (the
    // aarch64 ports) gets a working producer and one that does not fails closed
    // with `NotImplemented` exactly as the `NULL_*` defaults did. All are
    // `Box::leak`'d for the same one-shot-publish reason as the hook, arch-
    // generic so this names no concrete port.
    let (mem_map, mmio_map_facility, dma_alloc_facility, shared_mem_facility) =
        live_producers(state.arch.as_ref(), &state.frame_allocator);

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
            &state.frame_allocator,
            // The same leaked-`'static` allocator, handed to the spawn
            // producer as a `'static` page-table frame source so a child's
            // page tables come from reclaimable RAM that scales with the
            // machine rather than a fixed `.bss` pool.
            &state.frame_allocator,
            programs,
            spawn_service,
            process_wait,
            seat_registry,
            mem_map,
            mmio_map_facility,
            dma_alloc_facility,
        )
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
        // Resolve a spawn-as-user switch against the authoritative identity
        // table the boot path installed (`PREREQUISITES.md` P-C) — the same
        // table the filesystem service resolves caller groups against; the
        // default `NULL_IDENTITY` keeps a switch fail-closed when no root was
        // unlocked, and the default `spawn` (inherit) never consults it.
        .with_identity(spawn_identity)
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

    Ok((state, process_wait))
}

/// Build and `Box::leak` the production `mem_map` / `mmio_map` / `dma_alloc`
/// / `shm_*` producers over the per-task retained live address space
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
    &'static (dyn crate::devres::MmioMapFacility + 'static),
    &'static (dyn crate::devres::DmaAllocFacility + 'static),
    &'static (dyn crate::devres::SharedMemFacility + 'static),
) {
    (
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
        Some(physmap) => Box::leak(Box::new(crate::live_producer::LiveSharedMem::new(
            arch, frames, physmap,
        ))),
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
    /// into it at runtime. Wrapped in the same
    /// reader-preferring `RwLock` as `caps` so the syscall hot path
    /// takes only a shared lock and the kernel composes both
    /// registries under one lock-ordering policy (the registry itself owns no lock, mirroring `CapTable`).
    pub(crate) ipc: RwLock<PortRegistry>,
    /// Per-task address-space registry backing the kernel's
    /// `copy_from_user` / `copy_to_user` boundary.
    /// Maps a task's [`rustos_kernel_sec::TaskId`] to its user
    /// [`rustos_kernel_mem::AddressSpace`] and the [`PhysMap`] that
    /// backs it, so a syscall handler can resolve the caller's task id
    /// to the pair [`rustos_kernel_mem::uaccess`] walks. Wrapped in the
    /// same reader-preferring `RwLock` as `caps` / `ipc` so the syscall
    /// hot path takes only a shared lock and the kernel composes every
    /// registry under one lock-ordering policy (the
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
    /// The kernel's single cryptographic random output reserve. The `KernelDispatchHook` borrows it so
    /// `random_get` draws CSPRNG output from it before copying the
    /// bytes into the caller's buffer. It boots **unseeded** over the
    /// [`NullEntropy`](crate::random::NullEntropy) source, so a draw fails closed with
    /// [`rustos_abi::Errno::EntropyNotReady`] until the platform-RNG
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
            value: rustos_log::FieldValue::Str(phase.as_str()),
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
            value: rustos_log::FieldValue::Str(phase.as_str()),
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
            &state.frame_allocator,
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

        let ctx = KernelInitSpawner::new(
            &state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            &crate::devres::NULL_SHARED_MEM_FACILITY,
        );

        assert_eq!(state.arch.pump_console_tx_count(), 0);
        // Each per-dispatch servicing pumps the console transmit exactly
        // once (on top of draining any deferred wake).
        ctx.service_between_dispatches();
        assert_eq!(state.arch.pump_console_tx_count(), 1);
        ctx.service_between_dispatches();
        assert_eq!(state.arch.pump_console_tx_count(), 2);
    }

    /// A [`ProcessSpawn`] recording the `rxe`, capability set, and argument
    /// vector its `spawn_with` is handed, returning a fixed PID without
    /// building anything (it never consults the `SpawnCtx`). Lets the host
    /// suite assert [`KernelInitSpawner::spawn_driver_process`] forwards its
    /// inputs to the architecture producer's `spawn_with`; the matched
    /// node's grant threading is proven end-to-end by the `-M virt`
    /// `driver_spawn_qemu_aarch64` vertical (the grants live inside the
    /// opaque `KernelSpawnCtx`).
    struct RecordingSpawn {
        recorded: RwLock<Option<(alloc::vec::Vec<u8>, bool, usize)>>,
        pid: u64,
    }

    impl ProcessSpawn for RecordingSpawn {
        fn spawn_with(
            &self,
            rxe: &[u8],
            _ctx: &dyn crate::spawn::SpawnCtx,
            caps: CapabilitySet,
            args: &[&[u8]],
            _env: &[&[u8]],
        ) -> Result<u64, Errno> {
            *self.recorded.write() = Some((
                rxe.to_vec(),
                caps.contains(rustos_abi::CapabilityId::DRV_LOAD),
                args.len(),
            ));
            Ok(self.pid)
        }
    }

    #[test]
    fn spawn_driver_process_delegates_to_the_arch_producer() {
        // The production `KernelInitSpawner` must forward a driver spawn to
        // the architecture's `ProcessSpawn::spawn_with` with the gate-derived
        // capability set and argument vector intact, returning the producer's
        // PID — the path the bin crate's driver autoloader drives.
        let log_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let audit_sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let boot = bootinfo_with(log_sink, audit_sink, make_memory_map());
        let (state, process_wait) = run_phases(boot, log_sink, audit_sink).expect("phases succeed");

        let ctx = KernelInitSpawner::new(
            &state.frame_allocator,
            audit_sink,
            &state.scheduler,
            &state.caps,
            &state.aspaces,
            state.arch.as_ref(),
            process_wait,
            &state.irq,
            &crate::devres::NULL_SHARED_MEM_FACILITY,
        );

        let producer = RecordingSpawn {
            recorded: RwLock::new(None),
            pid: 0x4242,
        };
        let mut caps = CapabilitySet::empty();
        caps.insert(rustos_abi::CapabilityId::DRV_LOAD);
        let rxe = b"driver-image-bytes";
        let args: [&[u8]; 1] = [b"reply-endpoint"];

        let pid = ctx
            .spawn_driver_process(
                &producer,
                "/System/Drivers/storage/virtio_blk",
                rxe,
                caps,
                &[],
                &args,
                Some(7),
            )
            .expect("the recording producer admits the driver");
        assert_eq!(pid, 0x4242);

        let recorded = producer.recorded.read().clone();
        let (rxe_seen, had_drv_load, arg_count) =
            recorded.expect("spawn_with was invoked exactly once");
        assert_eq!(rxe_seen.as_slice(), rxe);
        assert!(had_drv_load, "the gate-derived capability set is forwarded");
        assert_eq!(arg_count, 1);
    }

    /// A [`ProcessSpawn`] that admits a host-built one-page address space
    /// through `ctx.admit_process` (mirroring the syscall suite's admitting
    /// double), so this suite can observe what the production
    /// `KernelInitSpawner` context attests onto the child's capability
    /// record.
    struct AdmittingSpawn;

    impl ProcessSpawn for AdmittingSpawn {
        fn spawn_with(
            &self,
            _rxe: &[u8],
            ctx: &dyn crate::spawn::SpawnCtx,
            caps: CapabilitySet,
            _args: &[&[u8]],
            _env: &[&[u8]],
        ) -> Result<u64, Errno> {
            use rustos_kernel_mem::{
                AddressSpace, Frame, HostPageTable, MapFlags, Page, SimPhysMap, VirtAddr,
            };
            let mut space = AddressSpace::new(HostPageTable::new());
            space
                .map(
                    Page::from_addr(VirtAddr::new(0x1000)).expect("aligned"),
                    Frame(9),
                    MapFlags::READ | MapFlags::USER,
                )
                .expect("host map");
            let frozen: Box<dyn UserAddressSpace + Send + Sync> = Box::new(space.freeze());
            let physmap: Box<dyn PhysMap + Send + Sync> =
                Box::new(SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE));
            let pre_resume: Box<dyn FnMut(u64) + Send> = Box::new(|_stack_top| {});
            let enter: Box<dyn FnMut() + Send> = Box::new(|| {});
            let stack: Box<dyn crate::kthread::KernelStack + Send> =
                Box::new(crate::kthread::BoxStack::new());
            // SAFETY: the host test never dispatches the admitted task, so
            // the inert `enter`/`pre_resume` closures never run and the
            // frozen host space need only answer `translate`; it faithfully
            // describes the one page mapped above.
            unsafe { ctx.admit_process(caps, frozen, physmap, stack, pre_resume, None, enter) }
                .map_err(|_| Errno::NoSpace)
        }
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
            &state.frame_allocator,
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
        caps.insert(rustos_abi::CapabilityId::DRV_LOAD);
        let pid = ctx
            .spawn_driver_process(
                &AdmittingSpawn,
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
        caps.insert(rustos_abi::CapabilityId::DRV_LOAD);
        let pid = ctx
            .spawn_driver_process(
                &AdmittingSpawn,
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
            &state.frame_allocator,
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
        let sec = SecTaskId(handle);
        let mut caps = CapabilitySet::empty();
        caps.insert(rustos_abi::CapabilityId::DRV_LOAD);
        let record = TaskCapabilities::derive(sec, UserId(0), caps, caps, audit_sink);
        state.caps.write().insert(record);
        assert!(state.caps.read().caps_for(sec).is_some());

        audit_sink.clear();
        // Teardown reclaims the capability record and audits the unload.
        assert_eq!(ctx.terminate_driver_process(handle), Ok(()));
        assert!(
            state.caps.read().caps_for(sec).is_none(),
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
    /// `Phase::Syscall`, **not** silently overwriting (fail closed).
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
