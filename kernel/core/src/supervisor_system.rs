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
//! is built, and published into a set-once cell the binding kernel's Supervisor
//! host reads through [`supervisor_system`]. Every method is read-only except
//! the explicitly audited `reboot`/`poweroff`, and none panics on any input.

use tairix_abi::ABI_VERSION_CURRENT;
use tairix_arch_api::TakeoverError;
use tairix_kalloc::FreeListAllocator;
use tairix_kernel_mem::{
    ram_snapshot_free_regions, ram_sweep_pattern, ram_takeover_test_bytes, BootMemoryMap,
    MemoryRegion, PhysAddr, RamFault, RamTestPattern, RegionKind, SweepObserver, PAGE_SIZE,
};
use tairix_supervisor::{Geometry, MemtestUi, Report, Screen};
use tairix_sync::once::OnceCell;

use crate::boot_audit_ring::{BootAuditRing, TailRecord};
use crate::bootinfo::KernelArch;
use crate::init::KernelState;
use crate::sched::CpuId;
use crate::wallclock::WallClockSource;

/// One binary mebibyte, the unit RAM figures are shown in.
const MIB: u64 = 1024 * 1024;

/// Capacity of the reserved-memory region snapshot the takeover sweep walks.
///
/// The sweep tests only the frame allocator's currently-free runs, copied into
/// a fixed-size array on the reserved takeover stack first (see
/// [`ram_snapshot_free_regions`]) so the sweep never reads a heap-backed
/// structure it is about to overwrite. A freshly-booted machine's free memory
/// is a handful of large runs and the framebuffer carve adds at most one
/// split, so this is generously above any real count; a free set too
/// fragmented to fit is refused fail-closed *before* the machine is quiesced.
/// At 24 bytes per [`MemoryRegion`] the array is ~6 KiB — negligible on the
/// reserved 64 KiB takeover stack.
const MAX_TAKEOVER_REGIONS: usize = 256;

/// A supervisor-only authorization witness for reading the machine-takeover
/// handle ([`KernelArch::machine_takeover`]).
///
/// The one-way whole-RAM takeover mechanism is irreversible: it stops
/// every CPU, overwrites all of RAM, and can only end in a reset. It must be
/// reachable **only** from the audited `memtest` path in this
/// module and nowhere else. Holding a `&dyn KernelArch` is deliberately *not*
/// enough to obtain the [`MachineTakeover`](tairix_arch_api::MachineTakeover)
/// handle: the accessor demands a
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

    /// Reset the machine. Returns only if the platform cannot reboot, so the
    /// caller can report the failure rather than pretend it worked.
    fn reboot(&self);

    /// Power the machine off / halt. Returns only if the platform has no
    /// power-off primitive.
    fn poweroff(&self);

    /// Attempt the one-way `memtest` whole-RAM takeover test.
    ///
    /// Called only from the audited engine path. On a platform
    /// that wired the machine-takeover slice this never returns: it stops
    /// every CPU and then tests all of RAM continuously (overwriting it),
    /// pattern after pattern, loop after loop, until the machine is reset. It
    /// **returns**
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
    /// The binary's kernel heap, read live for `mem` (it grows and shrinks
    /// by whole regions, so its size is a reading, not a boot constant).
    heap: &'static FreeListAllocator,
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
        heap: &'static FreeListAllocator,
    ) -> Self {
        Self {
            state,
            memory_map,
            wall_clock,
            heap,
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

    /// Mint the supervisor-only grant, stop every other CPU, read the port's
    /// machine-takeover handle, and drive the one-way whole-RAM `memtest`.
    ///
    /// The **quiesce is architecture-neutral and happens here**, not inside the
    /// per-port takeover: it uses the neutral directed IPI
    /// ([`crate::sched::SchedulerArch::send_ipi`]) and the boot-published
    /// liveness/ack tables
    /// through [`tairix_arch_api::quiesce_others`], so the shared handshake is
    /// written once rather than duplicated into every port. Only *parking* a
    /// stopped core is per-silicon (each port's interrupt path). The quiesce is
    /// the **last fallible step before the irreversible tear-down**: it runs
    /// only after the takeover mechanism and the physical map are confirmed
    /// available, so a machine whose peers were stopped is always one that will
    /// go on to reset — never left with its cores parked and no takeover to
    /// follow.
    ///
    /// On a port that wired the takeover slice, once quiesce succeeds
    /// [`take_over`](tairix_arch_api::MachineTakeover::take_over) **never
    /// returns**: it masks interrupts, stops the watchdog, switches onto a
    /// reserved stack, and runs the `sweep` below (how it reaches physical RAM
    /// directly is per-silicon — riscv64 bare mode, x86_64 a reserved
    /// identity table, aarch64 its live identity map). This method **returns**
    /// only when the takeover did not proceed — no mechanism is wired, no
    /// physical map, a peer CPU would not quiesce, the free set is too
    /// fragmented to snapshot, or the preparation failed — having left the
    /// machine unchanged, so the caller renders the reason and the REPL stays
    /// (fail closed, never a panic).
    ///
    /// The `sweep` renders the memtest86-style fullscreen display
    /// ([`MemtestUi`]) and tests RAM through the safe, range-checked
    /// [`ram_sweep_pattern`] engine, never raw pointer arithmetic. It first
    /// copies the frame allocator's currently-**free** runs into a
    /// **reserved-memory snapshot** on the takeover stack (carving out the live
    /// console framebuffer), so it depends on none of the RAM it is about to
    /// overwrite: every in-use frame — the heap the takeover renders through, a
    /// DMA buffer a device may still map non-cacheably, all driver and userland
    /// memory — is marked used by the allocator and never enters the snapshot,
    /// and the scan-out surface it displays through is carved out. It then
    /// **loops forever**, cycling every [`RamTestPattern`]
    /// over that snapshot and updating the display's elapsed timer,
    /// completed-loop count, current physical address, and scrolling fault log
    /// until the machine is reset; there is no abort seam, because the machine
    /// only leaves the test by a reset.
    fn drive_takeover(&self, out: &mut dyn Report) {
        // Minting the supervisor-only witness here is the sole authorization
        // for reading the takeover handle: the accessor cannot be called
        // without a `&TakeoverGrant`, and nothing outside this module can mint
        // one, so the takeover mechanism is reachable only from here.
        let grant = TakeoverGrant::mint();
        let Some(handle) = self.state.arch.machine_takeover(&grant) else {
            out.line("memtest: machine takeover is not supported on this platform.");
            return;
        };
        let Some(physmap) = self.state.arch.direct_phys_map() else {
            out.line("memtest: no direct physical map; RAM cannot be addressed.");
            return;
        };
        // The single authority on which RAM is safe to overwrite: the frame
        // allocator's currently-free runs. Every in-use frame — the kernel
        // image and page tables, the heap (this takeover's own console and
        // audit ring included), DMA buffers a device may still map
        // non-cacheably, and all driver and userland memory — is marked used
        // and is never swept. Writing such an in-use frame races its owner and
        // can wedge the machine; that is exactly what froze a real Raspberry
        // Pi 4, where the old boot-map sweep reached a live DMA buffer.
        let frames = self.state.frame_allocator;
        // The active console framebuffer is the one exclusion the *allocator*
        // may not know about: firmware carves it out of usable DRAM the
        // allocator can still consider free, so keep it out explicitly to
        // protect the live progress display. `None` on a serial-only boot.
        let framebuffer = self.state.arch.console_framebuffer();
        let exclude_count = usize::from(framebuffer.is_some());
        // The sweep walks a reserved-memory copy of those free runs, not any
        // heap-backed structure it is about to overwrite. Refuse fail-closed
        // *before* quiescing if the free set is so fragmented that, even after
        // the framebuffer carve splits at most one run, it could not fit the
        // reserved snapshot — leaving the machine running rather than
        // half-torn-down. (A fresh Supervisor has a handful of free runs, so
        // this never trips in practice.)
        let mut free_runs = 0usize;
        frames.for_each_free_region(|_, _| free_runs += 1);
        if free_runs.saturating_add(exclude_count) > MAX_TAKEOVER_REGIONS {
            out.line("memtest: memory too fragmented to snapshot; the machine is unchanged.");
            return;
        }
        // Stop every other CPU *before* the irreversible tear-down. This is the
        // last step that may fail closed: if a peer will not halt within the
        // bounded handshake the machine is left running and recoverable and the
        // REPL stays. A single-CPU (or not-yet-SMP) boot has no peers and this
        // succeeds immediately.
        let arch = self.state.arch.as_ref();
        let current = crate::sched::SchedulerArch::current_cpu(arch);
        if let Err(cpu) = tairix_arch_api::quiesce_others(current, |peer| {
            crate::sched::SchedulerArch::send_ipi(arch, peer);
        }) {
            render_takeover_refusal(out, TakeoverError::CpuQuiesceTimeout { cpu });
            return;
        }
        // Scope the sweep closure so its unique borrow of `out` ends before the
        // refusal path below reuses `out`. On a supported port `take_over`
        // never returns, so the refusal path is only reached when nothing was
        // torn down and the sweep was never run.
        let refused = {
            let mut sweep = || {
                // The takeover owns the whole machine and the console now, so
                // the test presents the rich fullscreen display; the presenter
                // clamps every position to the assumed 80x24 geometry (the
                // one-way console cannot be queried for size).
                let screen = Screen::new(out, Geometry::DEFAULT, false);
                // The one exclusion the free-run set cannot supply on its own:
                // a firmware-carved framebuffer sitting in usable DRAM the
                // allocator may still consider free. Keeping it out protects
                // the live progress display. Everything else the sweep must
                // avoid — every in-use frame — the allocator already excludes.
                let mut excludes = [(PhysAddr::new(0), 0u64); 1];
                let mut ex_n = 0usize;
                if let Some((base, len)) = framebuffer {
                    excludes[ex_n] = (base, len);
                    ex_n += 1;
                }
                // Copy the allocator's currently-free runs into a snapshot on
                // the reserved takeover stack, carving out the framebuffer,
                // *before* any write. From here the sweep reads only this
                // snapshot and never a heap-backed structure, an in-use frame,
                // or the scan-out surface it is about to destroy — the
                // dependencies that otherwise wedge the run the instant the
                // sweep reaches the frames holding them. A too-fragmented set
                // was refused before the quiesce; the snapshot only ever
                // truncates surplus *free* runs, and a `None` (a programming
                // error — more excludes than the cap) degrades to an empty
                // sweep rather than panicking.
                let mut snapshot = [MemoryRegion {
                    start: PhysAddr::new(0),
                    length: 0,
                    kind: RegionKind::Usable,
                }; MAX_TAKEOVER_REGIONS];
                let region_count =
                    ram_snapshot_free_regions(frames, &excludes[..ex_n], &mut snapshot)
                        .unwrap_or(0);
                let regions = &snapshot[..region_count];
                let total = ram_takeover_test_bytes(regions);
                let mut observer = TakeoverObserver {
                    ui: MemtestUi::new(screen),
                    arch,
                    cpu: current,
                    start_ns: arch.monotonic_ns(current),
                };
                observer.ui.begin();
                observer.ui.set_total(total);
                // Surface the reserved framebuffer extent, the number of free
                // runs the sweep walks, and how many ranges were kept out (the
                // framebuffer) as on-screen diagnostics, so a metal run shows
                // exactly what the sweep excluded and how free RAM was seen.
                observer.ui.set_environment(
                    framebuffer.map(|(base, len)| (base.as_u64(), len)),
                    region_count as u64,
                    ex_n as u64,
                );
                // Test all free RAM continuously: cycle every pattern over
                // every swept frame, over and over, until the operator resets
                // the machine. Each completed cycle bumps the loop counter; a
                // bad cell is logged and the sweep keeps going (it never stops
                // on a fault). This never returns — the machine only leaves the
                // test by a reset.
                loop {
                    for &pattern in RamTestPattern::ALL {
                        observer.ui.set_pattern(pattern.name());
                        ram_sweep_pattern(regions, physmap, pattern, total, &mut observer);
                    }
                    let elapsed = observer.elapsed_secs();
                    observer.ui.loop_complete(elapsed);
                }
            };
            // SAFETY: reached only from the confirmed, audited `memtest`
            // path holding the supervisor `TakeoverGrant`; the operator has
            // decided to tear the machine down, so quiescing every CPU and
            // overwriting all of RAM are the intended, deliberate actions. On
            // success `take_over` never returns — it runs `sweep` on a
            // reserved stack. The `sweep` closure's code lives in the reserved
            // kernel image, and it first copies the frame allocator's *free*
            // runs into reserved-stack memory and excludes the live
            // framebuffer, so it reads and writes-through only memory the sweep
            // never destroys — that reserved snapshot, the direct physical map,
            // the framebuffer it displays through, and every in-use frame the
            // allocator kept out (the kernel heap it renders and keeps time
            // through included) — satisfying the reserved-memory contract
            // `take_over` requires.
            unsafe { handle.take_over(&mut sweep) }
        };
        // `take_over` returned, so the takeover did not proceed and the machine
        // is unchanged. Report the reason fail-closed and stay in the REPL.
        render_takeover_refusal(out, refused);
    }
}

/// The live-display adapter the continuous `memtest` sweep reports through.
///
/// It owns the [`MemtestUi`] and turns each engine callback into a display
/// update: [`progress`](SweepObserver::progress) redraws the bar and the
/// elapsed clock, [`fault`](SweepObserver::fault) appends to the scrolling
/// error log, and [`window`](SweepObserver::window) records the physical
/// address of the frame currently under test (shown live, so a metal run
/// pins where the sweep was if a bad cell or a wedged access ever stalls it).
/// The elapsed time is read from the port's monotonic clock
/// ([`KernelArch::monotonic_ns`]) — a bare counter read (`CNTPCT_EL0` /
/// `RDTSC` / the `time` CSR) that needs no memory, so the timer keeps
/// advancing while the machine is owned by the test.
///
/// Generic over the concrete arch (`KernelArch` carries an associated
/// context-switch type, so it is not a `dyn` trait); the takeover only ever
/// constructs one, for the running port.
struct TakeoverObserver<'a, A: KernelArch> {
    /// The fullscreen presenter, borrowing the takeover console.
    ui: MemtestUi<'a>,
    /// The port's monotonic clock, read for the elapsed timer.
    arch: &'a A,
    /// The CPU the clock is read on (the sole surviving core post-quiesce).
    cpu: CpuId,
    /// The clock reading captured when the sweep began, so elapsed time is a
    /// difference rather than an absolute counter value.
    start_ns: u64,
}

impl<A: KernelArch> TakeoverObserver<'_, A> {
    /// Whole seconds elapsed since the sweep began, saturating so a counter
    /// that appears to go backwards never underflows.
    fn elapsed_secs(&self) -> u64 {
        self.arch
            .monotonic_ns(self.cpu)
            .saturating_sub(self.start_ns)
            / 1_000_000_000
    }
}

impl<A: KernelArch> SweepObserver for TakeoverObserver<'_, A> {
    fn window(&mut self, phys: u64) {
        // Drawn *before* the window is tested, so the address on screen names
        // the frame the sweep is on right now — if a bad cell or a wedged
        // access ever stalls the run, the last value shown pins it.
        self.ui.set_current(phys);
    }

    fn progress(&mut self, tested: u64, total: u64) {
        let elapsed = self.elapsed_secs();
        self.ui.progress(tested, total, elapsed);
    }

    fn fault(&mut self, fault: RamFault) {
        self.ui
            .record_fault(fault.phys.as_u64(), fault.expected, fault.observed);
    }
}

/// Render the fail-closed `memtest` refusal line for `err`.
///
/// Reached only when
/// [`take_over`](tairix_arch_api::MachineTakeover::take_over) returned rather than
/// resetting — no mechanism wired, a quiesce timeout, or a preparation
/// failure — so the machine is unchanged. The stable cause string comes from
/// [`TakeoverError::as_str`]; no payload value (a CPU id, a raw port status)
/// is rendered to the operator.
fn render_takeover_refusal(out: &mut dyn Report, err: TakeoverError) {
    out.write_str("memtest: takeover could not proceed (");
    out.write_str(err.as_str());
    out.line("); the machine is unchanged.");
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
        Self::write_mib(out, self.heap.capacity() as u64);
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

    fn reboot(&self) {
        self.state.arch.reboot();
    }

    fn poweroff(&self) {
        self.state.arch.poweroff();
    }

    fn memtest_takeover(&self, out: &mut dyn Report) {
        self.drive_takeover(out);
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
    use super::render_takeover_refusal;
    use tairix_arch_api::TakeoverError;
    use tairix_supervisor::Report;

    /// A host `Report` sink that captures the rendered bytes as UTF-8.
    #[derive(Default)]
    struct VecReport {
        bytes: alloc::vec::Vec<u8>,
    }

    impl Report for VecReport {
        fn write_bytes(&mut self, bytes: &[u8]) {
            self.bytes.extend_from_slice(bytes);
        }
    }

    impl VecReport {
        fn text(&self) -> alloc::string::String {
            alloc::string::String::from_utf8_lossy(&self.bytes).into_owned()
        }
    }

    #[test]
    fn refusal_renders_the_stable_cause_and_is_fail_closed() {
        for err in [
            TakeoverError::NotSupported,
            TakeoverError::CpuQuiesceTimeout { cpu: 3 },
            TakeoverError::PrepareFailed(-5),
        ] {
            let mut out = VecReport::default();
            render_takeover_refusal(&mut out, err);
            let text = out.text();
            assert!(
                text.contains(err.as_str()),
                "refusal must carry the stable cause string: {text:?}",
            );
            assert!(
                text.contains("the machine is unchanged"),
                "refusal must state the machine is unchanged (fail closed): {text:?}",
            );
            assert!(text.ends_with("\r\n"), "refusal must end the line");
        }
    }

    #[test]
    fn refusal_never_renders_a_payload_value() {
        // The raw port status / stuck CPU id must not leak to the operator.
        let mut out = VecReport::default();
        render_takeover_refusal(&mut out, TakeoverError::PrepareFailed(-1234));
        assert!(
            !out.text().contains("1234"),
            "payload value must not be rendered"
        );

        let mut out = VecReport::default();
        render_takeover_refusal(&mut out, TakeoverError::CpuQuiesceTimeout { cpu: 7 });
        assert!(
            !out.text().contains('7'),
            "stuck CPU id must not be rendered"
        );
    }
}
