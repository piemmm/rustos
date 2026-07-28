//! The live [`SupervisorSystem`] provider the pre-boot Supervisor console
//! reads its system state through (`plans/NEW-SUPERVISOR.md`).
//!
//! The Supervisor engine (`lib/supervisor`) is architecture-neutral and names
//! no kernel type; it drives its `version`, `mem`, `mem map`, `cpu`, `uptime`,
//! `date`, `memtest`, `reboot`, and `poweroff` commands through this
//! object-safe seam, which the kernel implements over state it already owns:
//! the leaked `KernelState` (the arch handle, the frame allocator, the
//! scheduler), the boot memory map, the wall clock, and the retained boot
//! audit-log ring. It computes nothing new — the one source of truth stays
//! where it lives (the charter forbids duplicating it).
//!
//! The provider is built once, at the same boot point the introspection source
//! is built, and published into a set-once cell the binding kernel's
//! Supervisor host reads through [`supervisor_system`]. Every method is
//! read-only except the explicitly audited `reboot`/`poweroff`, and none
//! panics on any input (`AGENTS.md` §2.9).

use tairix_abi::ABI_VERSION_CURRENT;
use tairix_arch_api::{MachineTakeover, TakeoverError};
use tairix_kernel_mem::{
    ram_test_owned_window, run_destructive, BootMemoryMap, DestructiveOutcome, Frame, RegionKind,
    PAGE_SIZE,
};
use tairix_supervisor::{Geometry, MemtestUi, Report, Screen, TestOutcome};
use tairix_sync::once::OnceCell;

use crate::boot_audit_ring::{BootAuditRing, TailRecord};
use crate::bootinfo::KernelArch;
use crate::init::KernelState;
use crate::wallclock::WallClockSource;

/// One binary mebibyte, the unit RAM figures are shown in.
const MIB: u64 = 1024 * 1024;

/// Upper bound on the free RAM one `memtest` pass will claim and test, in
/// bytes.
///
/// The Supervisor runs *after* the frame allocator is live, so a RAM test
/// must confine itself to memory it explicitly owns — free frames it
/// allocates and frees — never the live map (that would corrupt the running
/// kernel). This caps how much free RAM a single pass borrows so the test
/// never starves the rest of the system of memory (`AGENTS.md` §26.3); the
/// pass additionally never takes more than half of what is free, and stops
/// early if the allocator runs out. It is a safety bound on a borrowed
/// resource, not a scalable capacity.
const MEMTEST_MAX_BYTES: u64 = 64 * MIB;

/// A supervisor-only authorization witness for reading the machine-takeover
/// handle ([`KernelArch::machine_takeover`]).
///
/// The destructive whole-RAM takeover mechanism is irreversible: it stops
/// every CPU, overwrites all of RAM, and can only end in a reset. It must be
/// reachable **only** from the confirmed, audited `memtest full` path in this
/// module and nowhere else. Holding a `&dyn KernelArch` is deliberately *not*
/// enough to obtain the [`MachineTakeover`] handle: the accessor demands a
/// `&TakeoverGrant`, and this type carries a private field so it can be
/// constructed only inside this module through its module-private `mint`
/// constructor. No other kernel subsystem, driver, or userland
/// path can mint one, so the takeover mechanism cannot be invoked from outside
/// the Supervisor — the accessor is the single gate and this witness is its
/// only key.
pub struct TakeoverGrant {
    /// Private, so the witness cannot be constructed by a struct literal
    /// outside this module; [`TakeoverGrant::mint`] is the only constructor.
    _seal: (),
}

impl TakeoverGrant {
    /// Mint the witness. Module-private on purpose: only the takeover drive in
    /// this file calls it, so no code elsewhere can authorize a takeover.
    const fn mint() -> Self {
        Self { _seal: () }
    }
}

/// Whether the ordered two-step takeover handshake left the machine ready for
/// the destructive sweep, or refused fail-closed.
///
/// Extracted from [`KernelSupervisorSystem::memtest_takeover`] so the pure
/// decision — quiesce, then prepare, in order, both fail-closed — is
/// host-testable against a mock [`MachineTakeover`] without a real machine.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TakeoverReady {
    /// Both steps succeeded; the caller is now the only running CPU and paging
    /// is flattened, so the destructive sweep may run (and never return).
    Ready,
    /// A step refused; the machine is unchanged and recoverable, and the
    /// reason is carried for the operator report.
    Refused(TakeoverError),
}

/// Drive the ordered two-step machine-takeover handshake up to the point the
/// destructive sweep may run: [`MachineTakeover::quiesce_secondaries`] then
/// [`MachineTakeover::prepare_takeover`], in that order, each failing closed.
///
/// On [`TakeoverReady::Refused`] no destructive step has been taken and the
/// machine is left running and recoverable (the trait contract).
fn prepare_machine_takeover(handle: &dyn MachineTakeover) -> TakeoverReady {
    // SAFETY: reached only from the confirmed, audited `memtest full` path
    // holding the supervisor `TakeoverGrant`; the operator has decided to tear
    // the machine down, so quiescing every other CPU and then flattening
    // paging is the intended, deliberate action. The two steps are driven in
    // the required order and both fail closed without a destructive step on
    // error.
    match unsafe { handle.quiesce_secondaries() } {
        Ok(()) => {}
        Err(err) => return TakeoverReady::Refused(err),
    }
    // SAFETY: `quiesce_secondaries` succeeded, so the caller is the only
    // running CPU — the precondition `prepare_takeover` requires — and the
    // same deliberate-tear-down justification holds.
    match unsafe { handle.prepare_takeover() } {
        Ok(()) => TakeoverReady::Ready,
        Err(err) => TakeoverReady::Refused(err),
    }
}

/// The live system state the pre-boot Supervisor presents and the two
/// machine-control actions it drives.
///
/// Object-safe so the binding kernel's Supervisor host holds it behind a
/// `&'static dyn SupervisorSystem`; `Sync` because the single published
/// instance is shared. Every rendering method is read-only and writes only to
/// the supplied [`Report`]; `reboot`/`poweroff` are the sole state-changing
/// methods and are audited by the engine before they run.
pub trait SupervisorSystem: Sync {
    /// Render the kernel version, target architecture, and ABI version.
    fn version(&self, out: &mut dyn Report);

    /// Render installed/usable RAM, the kernel-heap size, and free memory.
    fn memory(&self, out: &mut dyn Report);

    /// Render the boot memory map (usable / reserved regions).
    fn memory_map(&self, out: &mut dyn Report);

    /// Render the CPU / core count and the detected CPU model and features.
    fn cpu(&self, out: &mut dyn Report);

    /// Render the monotonic time since boot.
    fn uptime(&self, out: &mut dyn Report);

    /// Render the wall-clock date/time (or that it is not yet set).
    fn date(&self, out: &mut dyn Report);

    /// Run the thorough, non-destructive RAM test over free frames the
    /// Supervisor owns, for `passes` passes, rendering progress and any fault
    /// to `out`. `abort` is polled between frames; when it returns `true` the
    /// test stops early and reports [`TestOutcome::Aborted`].
    fn memtest(
        &self,
        passes: u32,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome;

    /// Reset the machine. Returns only if the platform cannot reboot, so the
    /// caller can report the failure rather than pretend it worked.
    fn reboot(&self);

    /// Power the machine off / halt. Returns only if the platform has no
    /// power-off primitive.
    fn poweroff(&self);

    /// Attempt the one-way, destructive `memtest full` whole-RAM takeover
    /// test.
    ///
    /// Called only from the confirmed, audited engine path. On a platform
    /// that wired the machine-takeover slice this never returns: it stops
    /// every CPU, tests all of RAM (overwriting it), and resets. It **returns**
    /// only when the takeover did not proceed — the platform has no takeover
    /// mechanism, or the quiesce/prepare handshake failed closed — having
    /// rendered the reason to `out` and left the machine unchanged, so the
    /// engine stays in the REPL (fail closed, never a panic, never a partial
    /// tear-down).
    fn memtest_takeover(&self, out: &mut dyn Report);
}

/// The production [`SupervisorSystem`] over the running kernel's own state.
///
/// Holds only `'static` borrows of state the kernel already owns and adds no
/// authority of its own; it is built once in [`crate::init`] (where the
/// leaked `KernelState` and the boot memory map are in scope) and published
/// through [`install_supervisor_system`].
pub struct KernelSupervisorSystem<A: KernelArch + 'static> {
    /// The leaked kernel state: the arch handle (reboot/poweroff, CPU
    /// features, direct physical map), the frame allocator (RAM figures and
    /// the owned frames `memtest` tests), and the scheduler (core count).
    state: &'static KernelState<A>,
    /// The boot memory map, retained `'static` so `mem map` can list its
    /// usable/reserved regions.
    memory_map: &'static BootMemoryMap,
    /// The kernel wall clock (`date`).
    wall_clock: &'static (dyn WallClockSource + 'static),
    /// Committed size of the kernel heap region, in bytes (`mem`).
    kernel_heap_bytes: u64,
}

impl<A: KernelArch + 'static> KernelSupervisorSystem<A> {
    /// Build the provider over the leaked kernel state, boot memory map, and
    /// wall clock.
    ///
    /// Crate-internal because [`KernelState`] is a private hand-off type: only
    /// [`crate::init`] constructs the provider.
    #[must_use]
    pub(crate) const fn new(
        state: &'static KernelState<A>,
        memory_map: &'static BootMemoryMap,
        wall_clock: &'static (dyn WallClockSource + 'static),
        kernel_heap_bytes: u64,
    ) -> Self {
        Self {
            state,
            memory_map,
            wall_clock,
            kernel_heap_bytes,
        }
    }

    /// Read the monotonic clock on the issuing CPU (uptime / wall projection).
    fn monotonic_ns(&self) -> u64 {
        let cpu = crate::sched::SchedulerArch::current_cpu(&*self.state.arch);
        self.state.arch.monotonic_ns(cpu)
    }

    /// Write `bytes` as a MiB figure (`bytes / 1 MiB`).
    fn write_mib(out: &mut dyn Report, bytes: u64) {
        out.write_u64(bytes / MIB);
        out.write_str(" MiB");
    }

    /// Run the destructive whole-RAM sweep over the direct physical map, then
    /// reset the machine. Never returns.
    ///
    /// Reached only once the takeover handshake has succeeded
    /// ([`prepare_machine_takeover`] returned [`TakeoverReady::Ready`]): every
    /// other CPU is halted, interrupts and the watchdog are masked, and paging
    /// is flattened, so the machine will not resume and the only sequel is a
    /// reset. The sweep therefore overwrites all of RAM — including the frames
    /// the live kernel occupies — through the safe, range-checked
    /// [`run_destructive`] engine over the [`BootMemoryMap`], never raw pointer
    /// arithmetic. Progress and the outcome are rendered on the memtest86-style
    /// fullscreen display ([`MemtestUi`]) which drives the shared `lib/vt`
    /// terminal vocabulary through the console; the in-memory audit ring is
    /// already gone, so the console is the only record. There is no abort seam:
    /// once committed the machine is already destroyed, so the sweep's abort
    /// callback is a constant `false`.
    fn run_destructive_and_reset(&self, out: &mut dyn Report) -> ! {
        if let Some(physmap) = self.state.arch.direct_phys_map() {
            // The takeover owns the whole machine and the console now, so the
            // test presents the rich fullscreen display. A caller that knows
            // the console is a genuinely dumb line can build the screen with
            // `plain = true` for the line-oriented fallback; here we render
            // richly and the presenter clamps every position to the assumed
            // 80x24 geometry (the one-way console cannot be queried for size).
            let screen = Screen::new(out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            let outcome = run_destructive(
                self.memory_map,
                physmap,
                |tested, total| ui.progress(tested, total),
                || false,
            );
            match outcome {
                DestructiveOutcome::Passed { tested } => ui.passed(tested),
                DestructiveOutcome::Faulted(fault) => {
                    ui.faulted(fault.phys.as_u64(), fault.expected, fault.observed);
                }
                DestructiveOutcome::Aborted { tested } => ui.aborted(tested),
            }
        } else {
            out.line("memtest full: no direct physical map; RAM cannot be addressed. Resetting.");
        }
        // The machine cannot resume; the only sequel is a reset.
        self.state.arch.reboot();
        // A port whose reset returned (or none is wired) must not fall back
        // into a machine whose paging we have already flattened; halt
        // deterministically instead (never a busy task, just a dead CPU).
        loop {
            core::hint::spin_loop();
        }
    }
}

impl<A: KernelArch + 'static> SupervisorSystem for KernelSupervisorSystem<A> {
    fn version(&self, out: &mut dyn Report) {
        out.write_str("TAIRiX ");
        out.write_str(env!("CARGO_PKG_VERSION"));
        out.write_str("  arch ");
        out.write_str(
            self.state
                .arch
                .arch_id()
                .map_or("unknown", |arch| arch.name()),
        );
        out.write_str("  abi-v");
        out.write_u64(u64::from(ABI_VERSION_CURRENT));
        out.newline();
    }

    fn memory(&self, out: &mut dyn Report) {
        let page = PAGE_SIZE as u64;
        let usable = self.state.frame_allocator.usable_frames() as u64 * page;
        let free = self.state.frame_allocator.free_frames() as u64 * page;
        out.write_str("usable RAM:  ");
        Self::write_mib(out, usable);
        out.newline();
        out.write_str("free RAM:    ");
        Self::write_mib(out, free);
        out.newline();
        out.write_str("kernel heap: ");
        Self::write_mib(out, self.kernel_heap_bytes);
        out.newline();
    }

    fn memory_map(&self, out: &mut dyn Report) {
        out.line("boot memory map:");
        for region in self.memory_map.regions() {
            let kind = match region.kind {
                RegionKind::Usable => "usable  ",
                RegionKind::Reserved => "reserved",
            };
            out.write_str("  ");
            out.write_str(kind);
            out.write_str("  ");
            out.write_hex(region.start.as_u64());
            out.write_str(" + ");
            Self::write_mib(out, region.length);
            out.newline();
        }
    }

    fn cpu(&self, out: &mut dyn Report) {
        out.write_str("cores: ");
        out.write_u64(u64::from(self.state.scheduler.cpu_count()));
        out.newline();
        // The port's discovered CPU model name, when it derived one; a port
        // with none reports no model line rather than fabricating one.
        if let Some(name) = self.state.arch.cpu_name() {
            if let Some(model) = name.as_str() {
                out.write_str("model: ");
                out.write_str(model);
                out.newline();
            }
        }
        // The detected CPU-feature bits on the issuing core, when the port
        // exposes a feature slice; a port with none reports no line rather
        // than fabricating features.
        if let Some(features) = self.state.arch.cpu_features() {
            let cpu_id = crate::sched::SchedulerArch::current_cpu(&*self.state.arch);
            out.write_str("features: 0x");
            out.write_hex(features.detect(cpu_id).bits());
            out.newline();
        }
    }

    fn uptime(&self, out: &mut dyn Report) {
        // The one arch-neutral monotonic clock (the wait-queue clock); an
        // honest zero before it is installed.
        let ns = crate::waitq::wait_now_ns().unwrap_or(0);
        let secs = ns / 1_000_000_000;
        out.write_str("up ");
        out.write_u64(secs);
        out.write_str(" s");
        out.newline();
    }

    fn date(&self, out: &mut dyn Report) {
        let reading = self.wall_clock.read(self.monotonic_ns());
        if reading.state().is_set() {
            out.write_str("wall clock: ");
            let time = reading.time();
            let secs = time.secs();
            // No civil-time source exists at the bootstrap floor; report the
            // honest Unix-epoch seconds rather than fabricate a calendar date.
            if secs < 0 {
                out.write_str("-");
                out.write_u64(secs.unsigned_abs());
            } else {
                out.write_u64(secs.unsigned_abs());
            }
            out.line(" s since the Unix epoch");
        } else {
            out.line("wall clock: not set (no time source before the root is mounted)");
        }
    }

    fn memtest(
        &self,
        passes: u32,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome {
        let Some(physmap) = self.state.arch.direct_phys_map() else {
            out.line("memtest: no direct physical map on this platform; RAM cannot be tested.");
            return TestOutcome::Aborted;
        };
        let page = PAGE_SIZE as u64;
        for pass in 1..=passes {
            out.write_str("memtest: pass ");
            out.write_u64(u64::from(pass));
            out.write_str(" of ");
            out.write_u64(u64::from(passes));
            out.newline();
            // Borrow at most half of the currently-free RAM, capped, so the
            // test never starves the rest of the system.
            let free_bytes = self.state.frame_allocator.free_frames() as u64 * page;
            let budget = (free_bytes / 2).min(MEMTEST_MAX_BYTES);
            let frame_budget = (budget / page) as usize;
            let mut held: alloc::vec::Vec<Frame> = alloc::vec::Vec::new();
            let mut tested_bytes = 0u64;
            let mut fault = None;
            let mut aborted = false;
            while held.len() < frame_budget {
                if abort() {
                    aborted = true;
                    break;
                }
                // Take one more free frame to test; running out is the
                // natural bound, not an error.
                let Ok(frame) = self.state.frame_allocator.alloc() else {
                    break;
                };
                match ram_test_owned_window(physmap, frame.start(), PAGE_SIZE) {
                    Ok(true) => {
                        tested_bytes += page;
                        if tested_bytes % (8 * MIB) == 0 {
                            out.write_bytes(b"\r  tested ");
                            out.write_u64(tested_bytes / MIB);
                            out.write_str(" MiB");
                        }
                    }
                    // Unmappable frame: leave it untested rather than trust it.
                    Ok(false) => {}
                    Err(f) => {
                        fault = Some(f);
                    }
                }
                held.push(frame);
                if fault.is_some() {
                    break;
                }
            }
            // Return every borrowed frame before rendering the result: the
            // test owns them only for its duration and must leave the machine
            // exactly as it found it.
            for frame in held {
                let _ = self.state.frame_allocator.free(frame);
            }
            out.write_bytes(b"\r");
            if let Some(f) = fault {
                out.write_str("  RAM FAULT at physical 0x");
                out.write_hex(f.phys.as_u64());
                out.write_str(" (expected 0x");
                out.write_hex(f.expected);
                out.write_str(", read 0x");
                out.write_hex(f.observed);
                out.line(")");
                return TestOutcome::Failed;
            }
            if aborted {
                out.write_str("  aborted after ");
                out.write_u64(tested_bytes / MIB);
                out.line(" MiB");
                return TestOutcome::Aborted;
            }
            out.write_str("  ");
            out.write_u64(tested_bytes / MIB);
            out.line(" MiB of free RAM verified");
        }
        TestOutcome::Passed
    }

    fn reboot(&self) {
        self.state.arch.reboot();
    }

    fn poweroff(&self) {
        self.state.arch.poweroff();
    }

    fn memtest_takeover(&self, out: &mut dyn Report) {
        // Minting the supervisor-only witness here is the sole authorization
        // for reading the takeover handle: the accessor cannot be called
        // without a `&TakeoverGrant`, and nothing outside this module can mint
        // one, so the destructive mechanism is reachable only from here.
        let grant = TakeoverGrant::mint();
        let Some(handle) = self.state.arch.machine_takeover(&grant) else {
            out.line("memtest full: machine takeover is not supported on this platform.");
            return;
        };
        match prepare_machine_takeover(handle) {
            TakeoverReady::Refused(err) => {
                out.write_str("memtest full: takeover could not proceed (");
                out.write_str(err.as_str());
                out.line("); the machine is unchanged.");
            }
            // The handshake succeeded: the machine is ours and cannot resume.
            // Run the sweep and reset; this never returns.
            TakeoverReady::Ready => self.run_destructive_and_reset(out),
        }
    }
}

/// A non-destructive, read-only view of the retained boot audit-log ring the
/// Supervisor's `log` command tails.
///
/// Object-safe so the binding kernel's Supervisor host reads it behind a
/// `&'static dyn BootLogTail`, erasing the concrete
/// [`BootAuditRing`]'s capacity/interrupt-control generics. A viewer walks
/// [`seq_range`](Self::seq_range) and fetches each record by sequence with
/// [`record`](Self::record); an evicted sequence reads back [`None`], never a
/// different record, so a tail read stays consistent under a live writer.
pub trait BootLogTail: Sync {
    /// The number of records ever written (for a "last k of N" view).
    fn total(&self) -> u64;
    /// The `[oldest, newest]` sequence range currently retained, or [`None`]
    /// when the ring is empty.
    fn seq_range(&self) -> Option<(u64, u64)>;
    /// The record at `seq`, or [`None`] if it has aged out.
    fn record(&self, seq: u64) -> Option<TailRecord>;
}

impl<const N: usize, I> BootLogTail for BootAuditRing<N, I>
where
    I: tairix_sync::irq::InterruptControl + Sync,
{
    fn total(&self) -> u64 {
        BootAuditRing::total(self)
    }

    fn seq_range(&self) -> Option<(u64, u64)> {
        BootAuditRing::seq_range(self)
    }

    fn record(&self, seq: u64) -> Option<TailRecord> {
        BootAuditRing::record(self, seq)
    }
}

/// The set-once published live [`SupervisorSystem`] the binding kernel's
/// Supervisor host reads through. Set once per boot by [`crate::init`].
static SUPERVISOR_SYSTEM: OnceCell<&'static (dyn SupervisorSystem + 'static)> = OnceCell::new();

/// The set-once published boot audit-log tail the Supervisor's `log` command
/// reads. Installed by the per-arch boot path that owns the retained ring.
static BOOT_LOG_TAIL: OnceCell<&'static (dyn BootLogTail + 'static)> = OnceCell::new();

/// Error returned when a Supervisor cell is installed more than once.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct AlreadyInstalled;

/// Publish the production [`SupervisorSystem`]. Set-once per boot: a second
/// call fails closed rather than re-pointing the live provider.
///
/// # Errors
/// [`AlreadyInstalled`] if a provider was already installed.
pub fn install_supervisor_system(
    system: &'static (dyn SupervisorSystem + 'static),
) -> Result<(), AlreadyInstalled> {
    SUPERVISOR_SYSTEM.set(system).map_err(|_| AlreadyInstalled)
}

/// The installed [`SupervisorSystem`], or [`None`] before one is published.
#[must_use]
pub fn supervisor_system() -> Option<&'static (dyn SupervisorSystem + 'static)> {
    SUPERVISOR_SYSTEM.get().ok().flatten().copied()
}

/// Publish the retained boot audit-log tail. Set-once per boot.
///
/// # Errors
/// [`AlreadyInstalled`] if a tail was already installed.
pub fn install_boot_log_tail(
    tail: &'static (dyn BootLogTail + 'static),
) -> Result<(), AlreadyInstalled> {
    BOOT_LOG_TAIL.set(tail).map_err(|_| AlreadyInstalled)
}

/// The installed boot audit-log tail, or [`None`] before one is published.
#[must_use]
pub fn boot_log_tail() -> Option<&'static (dyn BootLogTail + 'static)> {
    BOOT_LOG_TAIL.get().ok().flatten().copied()
}

#[cfg(test)]
mod tests {
    use super::{prepare_machine_takeover, TakeoverReady};
    use core::cell::Cell;
    use tairix_arch_api::{MachineTakeover, TakeoverError};

    /// A takeover handle whose two steps return scripted outcomes without
    /// touching any hardware, recording whether `prepare` was reached so the
    /// ordered, fail-closed handshake is fully host-testable.
    struct MockTakeover {
        quiesce: Result<(), TakeoverError>,
        prepare: Result<(), TakeoverError>,
        prepare_called: Cell<bool>,
    }

    impl MockTakeover {
        fn new(quiesce: Result<(), TakeoverError>, prepare: Result<(), TakeoverError>) -> Self {
            Self {
                quiesce,
                prepare,
                prepare_called: Cell::new(false),
            }
        }
    }

    impl MachineTakeover for MockTakeover {
        unsafe fn quiesce_secondaries(&self) -> Result<(), TakeoverError> {
            self.quiesce
        }
        unsafe fn prepare_takeover(&self) -> Result<(), TakeoverError> {
            self.prepare_called.set(true);
            self.prepare
        }
    }

    #[test]
    fn both_steps_succeeding_reports_ready() {
        let handle = MockTakeover::new(Ok(()), Ok(()));
        assert_eq!(prepare_machine_takeover(&handle), TakeoverReady::Ready);
        assert!(handle.prepare_called.get());
    }

    #[test]
    fn a_quiesce_timeout_refuses_before_prepare() {
        let handle = MockTakeover::new(Err(TakeoverError::CpuQuiesceTimeout { cpu: 2 }), Ok(()));
        assert_eq!(
            prepare_machine_takeover(&handle),
            TakeoverReady::Refused(TakeoverError::CpuQuiesceTimeout { cpu: 2 }),
        );
        // The destructive `prepare` step must never run once quiesce failed.
        assert!(!handle.prepare_called.get());
    }

    #[test]
    fn an_unsupported_quiesce_refuses_before_prepare() {
        let handle = MockTakeover::new(Err(TakeoverError::NotSupported), Ok(()));
        assert_eq!(
            prepare_machine_takeover(&handle),
            TakeoverReady::Refused(TakeoverError::NotSupported),
        );
        assert!(!handle.prepare_called.get());
    }

    #[test]
    fn a_prepare_failure_refuses_fail_closed() {
        let handle = MockTakeover::new(Ok(()), Err(TakeoverError::PrepareFailed(-5)));
        assert_eq!(
            prepare_machine_takeover(&handle),
            TakeoverReady::Refused(TakeoverError::PrepareFailed(-5)),
        );
        assert!(handle.prepare_called.get());
    }
}
