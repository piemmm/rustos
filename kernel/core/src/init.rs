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

use alloc::sync::Arc;

use rustos_kernel_mem::{AllocError, FrameAllocator};
use rustos_kernel_sched::{SchedError, Scheduler};
use rustos_kernel_sec::IdentityTable;
use rustos_log::{log, set_max_level, Event, Field, Level, Sink};

use crate::audit::AuditEvent;
use crate::bootinfo::{BootInfo, BootInfoError, KernelArch};

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
    /// Prepare the IPC subsystem (currently a no-op — `kernel/ipc`
    /// holds no global state; the phase event still fires so external
    /// log consumers can rely on a consistent boot timeline).
    Ipc,
}

impl Phase {
    /// Iteration order used by [`kernel_main`].
    pub const ORDER: [Phase; 5] = [Phase::Log, Phase::Mem, Phase::Sec, Phase::Sched, Phase::Ipc];

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
    let log_sink: &(dyn Sink + Sync) = boot.log_sink;
    let audit_sink: &(dyn Sink + Sync) = boot.audit_sink;
    let arch_for_halt = Arc::clone(&boot.arch);

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
            value: "5",
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
    let outcome = run_phases(boot, log_sink, audit_sink);

    if let Err(err) = outcome {
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

    emit(
        audit_sink,
        Level::Info,
        AuditEvent::BootCompleted,
        &[Field {
            key: "next",
            value: "stage_2_7_syscall_registration",
        }],
    );
    arch_for_halt.halt();
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
    audit_sink: &(dyn Sink + Sync),
) -> Result<KernelState<A>, InitError> {
    // Pre-flight: re-validate the handover before logging Phase::Log
    // started — a malformed BootInfo means we cannot even trust the
    // log_level we just installed.
    boot.validate().map_err(InitError::BadBootInfo)?;

    let BootInfo {
        memory_map,
        identity,
        scheduler_config,
        arch,
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

    // Phase 5 — Ipc. `kernel/ipc` holds no global state at this stage
    // (Stage 2.5 deliberately keeps ports per-process); the phase
    // event still fires so the boot timeline is uniform.
    phase_started(log_sink, Phase::Ipc);
    phase_ready(log_sink, Phase::Ipc);

    Ok(KernelState {
        frame_allocator,
        identity_table,
        scheduler,
        arch,
    })
}

/// In-memory record of the live kernel subsystems built by
/// [`run_phases`].
///
/// Stage 2.7 will pass this to the syscall registrar and then enter
/// the scheduler dispatch loop. Until then it is consumed inside
/// [`kernel_main`] and dropped at halt — that is intentional and
/// documented; no global mutable static escapes (`AGENTS.md` §2 final
/// rule).
struct KernelState<A: KernelArch> {
    #[allow(dead_code)] // Stage 2.7 will wire these to the syscall layer.
    frame_allocator: FrameAllocator,
    #[allow(dead_code)]
    identity_table: IdentityTable,
    #[allow(dead_code)]
    scheduler: Scheduler<A>,
    #[allow(dead_code)]
    arch: Arc<A>,
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
    use crate::test_arch::TestArch;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
    use rustos_kernel_sched::SchedulerConfig;
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

    fn bootinfo_with(
        log_sink: &'static TestSink,
        audit_sink: &'static TestSink,
        memory_map: BootMemoryMap,
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
}
