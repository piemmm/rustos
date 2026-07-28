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
use tairix_kernel_mem::{ram_test_owned_window, BootMemoryMap, Frame, RegionKind, PAGE_SIZE};
use tairix_supervisor::{Report, TestOutcome};
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
