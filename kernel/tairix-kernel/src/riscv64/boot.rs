//! Bare-metal boot pipeline for the riscv64 (QEMU `virt` / SiFive)
//! `tairix-kernel` binary — `plans/PI.md` RV-P1 / RV-P2.
//!
//! [`boot`] is the single entry point. The binary's
//! `extern "C" fn kernel_main(hartid, dtb)` (called from the arch
//! port's [`tairix_arch_riscv64::entry`] trampoline, `boot.s` →
//! `entry.rs`) forwards to it after `boot.s` has established the boot
//! stack and zeroed `.bss` on the boot hart. It performs the BSP
//! bring-up the paged boot slice needs and hands a validated
//! [`tairix_kernel_core::BootInfo`] to
//! [`tairix_kernel_core::kernel_main`]:
//!
//! 1. Enable the Sv39 identity MMU and install the S-mode trap vector
//!    ([`enable_mmu_and_vectors`]) so the `kernel_core` allocator /
//!    scheduler atomics run on Normal cacheable memory and a fault
//!    during bring-up is taken to a handler — the riscv64 analogue of
//!    the aarch64 P6c-2 step. Install the production `ecall` dispatch
//!    callback ([`crate::riscv64::dispatch::production_dispatch`]) before
//!    any user thread can run.
//! 2. Parse the flattened device tree (`a1`) for the first `/memory`
//!    node and the `/cpus` `timebase-frequency`.
//! 3. Build a [`tairix_kernel_mem::BootMemoryMap`] that reserves the
//!    firmware + kernel-image + boot-heap span `[ram_base,
//!    __kernel_end)` and marks `[__kernel_end, ram_end)` usable
//!    ([`build_boot_memory_map`]).
//! 4. Construct the [`RiscvBinArch`] handle (boot hart + timebase) and
//!    assemble the `BootInfo`.
//!
//! RV-P2 runs the production path **paged**: the boot identity-maps the
//! whole low Sv39 window with 1 GiB leaves, so every physical address
//! the board uses (the kernel image, the firmware DTB, the PLIC, the
//! `virt` MMIO window, and the carved DMA regions the device-bring-up
//! verticals read) keeps its address under translation. Enabling
//! asynchronous interrupts (the timer/PLIC) and dropping PID 1 into
//! user mode are staged follow-ups (`plans/PI.md` RV-P3), exactly as the
//! aarch64 port reached this point before P6c-3 wired user mode.
//!
//! # Why this lives in the bin crate, not the arch port
//!
//! The arch port (`kernel/arch/riscv64`) is a pure Arch HAL
//! implementation and names no concrete kernel subsystem. This pipeline names `kernel/{core,mem,sec}` and
//! `kernel/sched/api`, so it lives here — exactly as x86_64 keeps its
//! boot pipeline and `BinArch` wrapper, and aarch64 its `boot_aarch64`,
//! in this crate. [`RiscvBinArch`] is the local `KernelArch` wrapper
//! around the arch port's [`RiscvArch`] (orphan rules).
//!
//! The riscv64 QEMU verticals (`tests/integration/riscv64_boot` and the
//! virtio-MMIO / framebuffer bins it backs) consume this very pipeline
//! through that downstream crate — they publish the firmware map for
//! their device-bring-up observers and then delegate here, so there is
//! exactly one riscv64 boot orchestration.
//!
//! # No `unwrap` / `expect` / `panic!`
//!
//! Every fallible step returns a [`BootError`]; [`boot`] logs the stable
//! cause string and parks the hart via the arch port's
//! [`tairix_arch_riscv64::halt_current_hart`] (fail closed).

use alloc::sync::Arc;

use tairix_arch_api::{CpuId, SchedulerArch};
use tairix_arch_riscv64::context_hal::ContextSwitchHal;
use tairix_arch_riscv64::fdt::Fdt;
use tairix_arch_riscv64::irqmask::{SstatusIrqControl, SstatusState};
use tairix_arch_riscv64::paging::{AddressSpace, PageTablePool};
use tairix_arch_riscv64::{
    halt_current_hart, serial, syscall_entry, trap, RiscvArch, RiscvArchStorage, SERIAL_SINK,
};
use tairix_kernel_core::boot_audit_ring::{
    boot_audit_clock, BootAuditRing, BOOT_AUDIT_RING_CAPACITY,
};
use tairix_kernel_core::{kernel_main, BootInfo, ConsoleWrite, IrqRouting, KernelArch};
use tairix_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
use tairix_kernel_sched_api::SchedulerConfig;
use tairix_log::{log, Event, EventId, Field, Level, Sink, TeeSink};
use tairix_sync::InterruptControl;

use crate::riscv64::dispatch::{
    production_dispatch, production_user_fault, production_user_fault_terminate, DISPATCH_SLOT,
};

/// Logical CPU id of the boot hart for the single-hart slice.
const BOOT_CPU: CpuId = 0;

/// Audit event id emitted on a boot-init failure. Shares the
/// `4000..5000` `kernel/core` range and the top-of-range slot the
/// x86_64 pipeline uses for the same "init failed before `kernel_main`"
/// signal, so external audit consumers decode one stable id across
/// arches.
const KERNEL_BOOT_INIT_FAILED: EventId = EventId(4099);

/// Audit event: the riscv64 production kernel reached its RV-P2 paged
/// boot init point (Sv39 MMU enabled, trap vector + `ecall` dispatch
/// installed). Shares the `kernel/core`-owned `4000..5000` range and the
/// `4097` "reached" slot the aarch64 boot pipeline uses; only one arch's
/// boot module compiles per image, so the id never collides at runtime.
const KERNEL_BOOT_RISCV64_REACHED: EventId = EventId(4097);

/// Number of 1 GiB identity gigapages the boot address space maps.
///
/// This covers the Sv39 low VA range below the growable kernel heap's remap
/// window, so the kernel image, stack, the firmware DTB, the PLIC, and the
/// `virt`-board MMIO window all keep their physical addresses once the MMU
/// is on — whatever their addresses, with no `cfg(board)` fork. Identity
/// mapping makes physical == virtual, so the device-bring-up verticals that
/// read MMIO/DMA at physical addresses keep working under the paged regime.
/// The figure comes from the port, which owns both the window's placement
/// and the identity extent below it, so the two cannot drift into overlap.
///
/// `pub(crate)` so the root-unlock bring-up ([`crate::riscv64::root_unlock`])
/// sizes its device physical map ([`tairix_kernel_mem::DirectPhysMap`]) to the
/// same identity extent the live boot MMU maps, rather than repeating the
/// figure (one definition).
pub(crate) const IDENTITY_GIGABYTES: usize = tairix_arch_riscv64::paging::IDENTITY_GIGAPAGES;

/// Boot-time page-table frame source for the Sv39 identity map.
///
/// A single root table holds every gigapage leaf, so the pool only
/// ever hands out one frame here. It lives in `.bss` for the lifetime of
/// the kernel image, so `satp` keeps pointing at a valid table after
/// [`enable_mmu_and_vectors`] returns even though the transient
/// [`AddressSpace`] handle is dropped (the pool is
/// monotonic and never freed). The real per-process page tables are
/// built over the `kernel/mem` frame allocator at a later stage.
static BOOT_PAGE_TABLES: PageTablePool = PageTablePool::new();

/// Stable `"true"`/`"false"` audit-field value for a boolean condition.
/// Keeping the boot log to `&'static str` fields means the path takes no
/// allocation and cannot panic.
const fn yes_no(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

extern "C" {
    /// One byte past the end of the kernel image (including the boot
    /// heap), defined by the binary's linker script
    /// (`kernel/arch/riscv64/link/riscv64-virt.ld`). The usable
    /// physical-memory region starts at the next page boundary.
    static __kernel_end: u8;
}

/// Address of the linker-provided `__kernel_end` symbol.
fn kernel_end_addr() -> u64 {
    // `addr_of!` reads the symbol's address without forming a reference
    // to the (zero-sized, never-dereferenced) marker.
    core::ptr::addr_of!(__kernel_end) as u64
}

/// Local [`KernelArch`] wrapper around the arch port's [`RiscvArch`].
///
/// `kernel_core::KernelArch` is a foreign trait and `RiscvArch` is a
/// foreign type, so Rust's coherence rules forbid implementing the
/// trait for the type directly. This is the smallest local type that
/// owns a `RiscvArch`, delegates the [`SchedulerArch`] super-trait, and
/// implements [`KernelArch::halt`] / [`KernelArch::monotonic_ns`] by
/// forwarding to the arch port — mirroring the x86_64 `BinArch` and the
/// aarch64 `Aarch64BinArch`.
#[derive(Debug)]
pub struct RiscvBinArch {
    arch: RiscvArch,
    /// The boot CPU's model name, discovered once at boot from the
    /// device tree's cpu `compatible` (S-mode cannot read the M-mode
    /// identity CSRs, so the tree is the only source); `None` when the
    /// tree names no part the port knows.
    cpu_name: Option<tairix_abi::CpuName>,
}

impl RiscvBinArch {
    /// Wrap `arch` (and the boot-discovered CPU name) so it can be
    /// handed to `kernel_core::kernel_main`.
    #[must_use]
    pub const fn new(arch: RiscvArch, cpu_name: Option<tairix_abi::CpuName>) -> Self {
        Self { arch, cpu_name }
    }
}

impl SchedulerArch for RiscvBinArch {
    fn current_cpu(&self) -> CpuId {
        self.arch.current_cpu()
    }

    fn ticks_now(&self) -> u64 {
        self.arch.ticks_now()
    }

    fn send_ipi(&self, target: CpuId) {
        self.arch.send_ipi(target);
    }

    fn set_preemption(&self, armed: bool) {
        // Tickless preemption: forward the scheduler's
        // arm/disarm decision to the arch port, which programs the
        // supervisor-timer one-shot. The default no-op would silently drop
        // preemption, so the delegation is required.
        self.arch.set_preemption(armed);
    }

    fn set_wakeup(&self, deadline_ns: Option<u64>) {
        // Forward the nearest blocking-wait deadline to the arch port,
        // which combines it with the quantum and arms the single
        // supervisor-timer one-shot to the earlier. The
        // default no-op would silently drop timed wakes, so the delegation
        // is required.
        self.arch.set_wakeup(deadline_ns);
    }
}

impl KernelArch for RiscvBinArch {
    type Cs = ContextSwitchHal;

    fn context_switch(&self) -> Self::Cs {
        ContextSwitchHal::new()
    }

    fn halt(&self) -> ! {
        halt_current_hart()
    }

    fn reboot(&self) {
        // Request a cold reboot through the SBI System-Reset extension; on
        // success the firmware resets the platform and this never returns. A
        // return means SRST is unimplemented or the firmware refused (fail
        // safe): the caller reports it and carries on. Off the freestanding
        // target there is no SBI, so power control is unsupported.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        {
            use tairix_arch_riscv64::sbi;
            let _ = sbi::system_reset(sbi::SBI_SRST_TYPE_COLD_REBOOT, sbi::SBI_SRST_REASON_NONE);
        }
    }

    fn poweroff(&self) {
        // Request an orderly shutdown through the SBI System-Reset
        // extension; on success the firmware powers the platform down and
        // this never returns. A return means SRST is unimplemented or
        // refused (fail safe). Off the freestanding target there is no SBI.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        {
            use tairix_arch_riscv64::sbi;
            let _ = sbi::system_reset(sbi::SBI_SRST_TYPE_SHUTDOWN, sbi::SBI_SRST_REASON_NONE);
        }
    }

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        self.arch.monotonic_ns()
    }

    fn arch_id(&self) -> Option<tairix_abi::Arch> {
        Some(tairix_abi::Arch::Riscv64)
    }

    fn cpu_features(&self) -> Option<&dyn tairix_arch_api::CpuFeatures> {
        // The riscv64 `misa` + device-tree `riscv,isa` detector; each hart
        // folds its own detected set into the migration-safe common set
        // delivered to every process (`kernel/core::cpuops`). The detector is a
        // stateless `const` value, so a `'static` reference to it is sound.
        const DETECT: tairix_arch_riscv64::cpufeatures::CpuFeatureDetect =
            tairix_arch_riscv64::cpufeatures::CpuFeatureDetect::new();
        Some(&DETECT)
    }

    fn core_clock(&self) -> Option<&dyn tairix_arch_api::CoreClock> {
        // The riscv64 `rdcycle` core-clock counter over the `rdtime` /
        // `timebase-frequency` reference: the kernel's per-CPU estimator
        // divides the two counters' deltas to report the live "cpu MHz".
        // The handle is a stateless `const` value, so a `'static` reference
        // to it is sound.
        const CLOCK: tairix_arch_riscv64::coreclock::CoreClockCounter =
            tairix_arch_riscv64::coreclock::CoreClockCounter::new();
        // There is no CSR reporting the reference rate, so publish the
        // discovered `timebase-frequency` to give the ratio its scale.
        tairix_arch_riscv64::coreclock::set_reference_hz(self.arch.timebase_hz());
        Some(&CLOCK)
    }

    fn cpu_name(&self) -> Option<tairix_abi::CpuName> {
        // Captured from the device tree at construction; an unlisted or
        // generic (`riscv`) compatible stays an honest `None` (the boot
        // facts record "unknown"), never a guessed name.
        self.cpu_name
    }

    fn ticks_to_ns(&self, ticks: u64) -> u64 {
        // `ticks_now` is the raw `time` CSR, so the identity default would
        // misreport CPU time; convert against the same discovered timebase
        // frequency `monotonic_ns` uses.
        self.arch.ticks_to_ns(ticks)
    }

    fn park_translation(&self) -> Option<fn()> {
        // Re-installs the boot space's `satp` root (published by the boot
        // `switch()`) so no user root stays active after its task suspends
        // — the invariant a dead task's page-table reclamation relies on.
        fn park() {
            // Fire-and-forget from the dispatcher: with no park root
            // published yet there is nothing to leave (fail closed), so
            // the `bool` outcome is deliberately discarded.
            let _ = tairix_arch_riscv64::paging::park_kernel_root();
        }
        Some(park)
    }

    fn wait_for_interrupt(&self) {
        // The tickless idle park. The dispatch loop
        // calls this with `sstatus.SIE` already cleared (it masked S-mode
        // interrupts to close the park/wake race and drained any
        // already-flagged wake), so `wfi` parks the hart until an interrupt
        // becomes pending — it wakes on a pending-but-untaken interrupt
        // even with `SIE == 0`, so no edge is lost. The loop
        // re-enables interrupts after we return, *taking* the pending one
        // then (its lock-free handler flags the deferred wake the next
        // `drain_pending_wakes` consumes). On a host build there is no
        // S-mode, so this is a benign no-op.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        {
            // SAFETY: the trap vector is installed (`enable_mmu_and_vectors`)
            // and the timer source armed by this point; `wfi` parks until a
            // pending interrupt and leaves `sstatus.SIE` unchanged (masked).
            unsafe {
                trap::wait_for_interrupt();
            }
        }
    }

    fn set_device_irqs(&self, enabled: bool) {
        // Toggle this hart's S-mode interrupt taking (`sstatus.SIE`) so the
        // dispatch loop runs in-kernel tasks/kthreads with interrupts
        // enabled (the fully preemptive kernel), masking
        // them only around the idle park and before halt. Enabling `SIE` in
        // S-mode is safe: a timer tick taken in S-mode runs lock-free
        // accounting but never reschedules the kernel (the trap handler
        // gates preemption on the saved `SPP`), and a PLIC external
        // interrupt forwards to the lock-free dispatcher. On a host build
        // there is no S-mode, so this is a benign no-op.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        {
            // SAFETY: toggles only `sstatus.SIE`; the trap vector is
            // installed by the time the dispatch loop runs.
            unsafe {
                trap::set_supervisor_interrupts(enabled);
            }
        }
        #[cfg(not(all(freestanding, kernel_isa = "riscv64")))]
        {
            let _ = enabled;
        }
    }

    fn irq_routing(&self) -> IrqRouting {
        // Size the core `IrqTable` to the discovered PLIC source ceiling and
        // hand it the shared PLIC controller (`Phase::Irq`), so a device
        // source — the root-unlock virtio-blk completion line, an autoloaded
        // driver's line — can be bound. Without this override the core falls
        // back to the unsupported routing (`max_line = 0`) and every device
        // `bind` fails closed as out-of-range, wedging every interrupt-driven
        // bring-up. A board with no PLIC (or a host build) returns the
        // unsupported routing and interrupt-driven bring-up fails closed.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        {
            crate::riscv64::irq::plic_routing()
        }
        #[cfg(not(all(freestanding, kernel_isa = "riscv64")))]
        {
            IrqRouting::unsupported()
        }
    }

    fn install_irq_dispatch(&self, table: &'static tairix_kernel_irq::IrqTable) {
        // Set up tickless supervisor-timer preemption now that the
        // scheduler is up (P-1b, `plans/PI.md` D2b-2b-A): register the
        // per-hart preempt storage, install the U-mode-preemption
        // callback, record the per-quantum interval derived from the
        // device-tree `timebase-frequency`, and enable `sie.STIE` — but
        // leave the timer disarmed. TAIRiX is tickless: the scheduler arms the one-shot to one quantum only when
        // it dispatches onto a contended hart (via
        // `RiscvArch::set_preemption`) and disarms otherwise, so a hart
        // running a sole task takes no timer ticks. The kernel keeps
        // `sstatus.SIE == 0`, so a tick is *taken* only while a U-mode task
        // runs (the privilege rule U < S).
        arm_preemption(self.arch.timebase_hz());
        // Wire the S-mode external-interrupt (PLIC) dispatch on top: publish
        // this table, build + publish the PLIC controller from the base +
        // source count discovered from the firmware tree at boot
        // (`seed_hardware_tree` → `irq::record_plic`), install the
        // claim → `IrqTable::fire` → complete dispatcher, and enable
        // `sie.SEIE` so the interrupt-driven bootstrap-floor bring-up (the
        // root-unlock virtio-blk completion line, an autoloaded driver's
        // device line) can be taken. Additive: every source stays masked at
        // the controller until a driver arms its own line, and `sstatus.SIE`
        // is toggled by the dispatch loop, so this changes no behaviour until
        // the first line is armed. A board with no PLIC leaves the dispatch
        // unwired and interrupt-driven bring-up fails closed.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        crate::riscv64::irq::install_dispatch(table);
        #[cfg(not(all(freestanding, kernel_isa = "riscv64")))]
        let _ = table;
    }

    fn machine_takeover(
        &self,
        _grant: &tairix_kernel_core::supervisor_system::TakeoverGrant,
    ) -> Option<&'static (dyn tairix_arch_api::MachineTakeover + Sync)> {
        // The riscv64 destructive whole-RAM takeover the Supervisor's
        // confirmed `memtest` drives (`plans/NEW-SUPERVISOR.md` §9). The
        // handle is minted only through the arch port's gated accessor, and
        // this override is itself reachable only with the supervisor-only
        // `TakeoverGrant`, so the mechanism stays confined to that one path.
        // Off the freestanding target this module does not compile; boot.rs
        // is freestanding-only, so the handle is always available here.
        Some(tairix_arch_riscv64::takeover::machine_takeover_handle())
    }

    fn install_kernel_remap(
        arch: &'static Self,
        frames: &'static tairix_kernel_mem::FrameAllocator,
        physmap: &'static (dyn tairix_kernel_mem::PhysMap + Sync),
    ) -> Option<&'static dyn tairix_kernel_mem::KernelVirtMap> {
        // Reserve the top of the Sv39 range as the growable kernel heap's
        // remap window and hand back the neutral map over it. The window's
        // shared intermediate tables — and every table a heap region needs —
        // come from the same allocator-backed page-table source the spawn
        // path uses, so no fixed `.bss` pool caps how much the heap can
        // grow. The port's own handle carries the SBI RFENCE cross-CPU
        // invalidation.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        {
            // The frame source carries the identity physical map itself;
            // `physmap` backs the *caller's* own bookkeeping.
            let _ = physmap;
            let tables = crate::riscv64::spawn_producer::page_table_source(frames).ok()?;
            let window = tairix_arch_riscv64::paging::reserve_kernel_window(tables)?;
            let space = tairix_arch_riscv64::paging::AddressSpace::new_kernel_window(tables)?;
            let remap =
                alloc::boxed::Box::leak(alloc::boxed::Box::new(tairix_kernel_mem::KernelRemap::<
                    _,
                    tairix_arch_riscv64::irqmask::PortIrqControl,
                >::new(
                    window, space, &arch.arch
                )));
            Some(remap)
        }
        #[cfg(not(all(freestanding, kernel_isa = "riscv64")))]
        {
            let _ = (arch, frames, physmap);
            None
        }
    }

    fn direct_phys_map(&self) -> Option<&'static (dyn tairix_kernel_mem::PhysMap + Sync)> {
        // The identity direct map (`virtual == physical` over the configured
        // `[0, IDENTITY_GIB GiB)` window, covering RAM) the shared-memory
        // facility scrubs region frames through. On a host build there is no
        // S-mode RAM to map, so none is offered and `shm_*` stays fail-closed.
        #[cfg(all(freestanding, kernel_isa = "riscv64"))]
        {
            Some(&crate::riscv64::spawn_producer::SPAWN_TABLE_PHYSMAP)
        }
        #[cfg(not(all(freestanding, kernel_isa = "riscv64")))]
        {
            None
        }
    }
}

// SAFETY-INVARIANT: `RiscvBinArch::halt` returns the bottom type. The
// coercion fails to type-check if the impl ever loses `-> !`, pinning
// the contract at compile time.
const _RISCV_BIN_ARCH_HALT_RETURNS_NEVER: fn(&RiscvBinArch) -> ! =
    <RiscvBinArch as KernelArch>::halt;

/// Preemption-quantum rate, in slices per second. The shared
/// [`DEFAULT_PREEMPT_QUANTUM_HZ`](tairix_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ)
/// the aarch64 port also uses (defined once so the two cannot diverge): a ~10 ms slice bounds a runaway user task's hold on
/// a contended hart while costing negligible trap overhead. This is
/// **not** a periodic tick — the timer is armed one-shot to one quantum
/// only when a hart is contended (tickless). The
/// interval in `time`-CSR ticks is derived from the discovered
/// `timebase-frequency`, never a board constant.
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
const PREEMPT_TICK_HZ: u64 = tairix_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ;

/// Caller-owned per-hart preemption backing for the production boot hart.
///
/// The production riscv64 image is single-hart (`BootInfo::new(BOOT_CPU,
/// 1, …)`), so a `PreemptStorage<1>` covers it; secondary-hart preemption
/// is sized from the discovered hart count when SMP bring-up lands
/// (the per-hart timer bookkeeping is the discovered
/// hart count, never a baked-in ceiling). Published once by
/// [`arm_preemption`].
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
static PREEMPT_STORAGE: tairix_arch_riscv64::preempt::PreemptStorage<1> =
    tairix_arch_riscv64::preempt::PreemptStorage::new();

/// Set up tickless supervisor-timer preemption on the boot hart: register
/// the per-hart preempt storage, install the trap callbacks
/// ([`crate::riscv64_preempt_wiring::install_callbacks`]), record the
/// per-quantum interval from [`PREEMPT_TICK_HZ`], and enable the timer and
/// reschedule-IPI sources (`sie.STIE` / `sie.SSIE`) — but leave the timer
/// disarmed. The scheduler arms the
/// one-shot to one quantum only when it dispatches onto a contended hart
/// (`RiscvArch::set_preemption`), and disarms otherwise (tickless / `NO_HZ`).
///
/// Called once per boot from [`RiscvBinArch::install_irq_dispatch`], in
/// the kernel-core `Irq` phase — after the scheduler is built and before
/// `init` drops to U-mode. The kernel runs with `sstatus.SIE == 0`, so no
/// tick is *taken* until a U-mode task runs (the privilege rule U < S),
/// so this is **additive and non-regressing**: a tick taken in U-mode
/// drives [`crate::riscv64_preempt_wiring::preempt_dispatch`] immediately, and a one-shot
/// that fires in S-mode disarms without context-switching (the kernel is
/// non-preemptible) but is latched by [`crate::riscv64_preempt_wiring::tick_dispatch`] and
/// honoured when the interrupted syscall completes — an expired quantum
/// is never silently lost.
///
/// No *scheduler-fairness* tick callback is installed: EEVDF is tickless
/// (fairness is advanced inside `Scheduler::step`, not by a periodic
/// count). The per-tick callback that *is* installed
/// ([`crate::riscv64_preempt_wiring::tick_dispatch`]) latches the pending preemption and runs
/// the blocking-wait timed-wake sweep (Design D P-2): it releases any
/// elapsed `hw_tree_wait`-style waiter and re-arms the one-shot to the
/// next deadline, so the SBI timer is armed only for a real pending
/// event — a preemption quantum and/or the nearest wakeup — never a
/// fixed periodic tick.
///
/// A zero `timebase_hz` (a board that does not report the timer rate)
/// leaves the kernel cooperative rather than arming a nonsense interval —
/// fail-safe. The boot pipeline already refuses a
/// zero/absent `timebase-frequency` (`BootError::NoTimebase`), so this is
/// defence-in-depth.
fn arm_preemption(timebase_hz: u64) {
    #[cfg(all(freestanding, kernel_isa = "riscv64"))]
    {
        use tairix_arch_riscv64::preempt;

        if timebase_hz == 0 {
            return;
        }

        // Set-once per boot; a stray re-call fails closed by halting rather
        // than re-pointing the live per-hart slices.
        if PREEMPT_STORAGE.register().is_err() {
            halt_current_hart();
        }

        // Install every trap callback before any source is unmasked, so a
        // delivered trap already has a handler.
        crate::riscv64_preempt_wiring::install_callbacks();

        let interval = preempt::interval_for_hz(timebase_hz, PREEMPT_TICK_HZ);

        // SAFETY: this is the boot hart (id 0); the callbacks are installed
        // (above), the per-hart storage is registered (above), and the trap
        // vector is installed (`enable_mmu_and_vectors`, before
        // `kernel_main`). Neither call sets `sstatus.SIE`, so no trap is
        // taken until a U-mode task runs.
        //
        // `enable_ipi` is not optional: `wfi` resumes only for *locally*
        // enabled sources, so without `sie.SSIE` a `send_ipi` neither wakes
        // the idle park nor ever traps to be acknowledged.
        unsafe {
            preempt::enable_ipi();
            preempt::init_local_preempt(0, interval);
        }
    }
    #[cfg(not(all(freestanding, kernel_isa = "riscv64")))]
    {
        let _ = timebase_hz;
    }
}

/// The system console device the riscv64 boot path installs on
/// [`tairix_kernel_core::BootInfo`].
///
/// A zero-sized [`ConsoleWrite`] adapter over the SBI console: every
/// `stream_write` byte is forwarded verbatim through the arch port's
/// [`tairix_arch_riscv64::serial::write_console_bytes`] (no `\n`
/// translation — the bytes reach the device exactly as the program
/// wrote them). It is the riscv64 analogue of the
/// aarch64 `UartConsole`'s output half: the "first discovered console"
/// stream **backing** the spawner attaches to fd 1,
/// not a program-facing interface.
///
/// No [`tairix_kernel_core::ConsoleRead`] half is installed: the SBI
/// legacy console exposes no non-blocking input drain, so fd 0 reads
/// fail closed until a real input backing lands — PID
/// 1 `init` and the embedded `Shell` `Run` program only *write* (a
/// banner) and `spawn`, so this slice needs no console input.
#[derive(Debug, Default, Copy, Clone)]
pub struct RiscvUartConsole;

impl ConsoleWrite for RiscvUartConsole {
    fn write(&self, bytes: &[u8]) -> Result<usize, tairix_abi::Errno> {
        // The busy-wait SBI transmit accepts every byte, so the write is
        // total and never short, and performs no `\n` translation.
        Ok(serial::write_console_bytes(bytes))
    }
}

/// The single `'static` [`RiscvUartConsole`] the boot path lists in the
/// [`tairix_kernel_core::BootInfo::with_consoles`] console list.
/// Zero-sized, so it has no `.bss`/`.data` footprint — mirroring
/// [`tairix_arch_riscv64::SERIAL_SINK`].
pub static RISCV_UART_CONSOLE: RiscvUartConsole = RiscvUartConsole;

/// The riscv64 boot console list: the SBI console is the only console.
/// Its read half is the fail-closed [`tairix_kernel_core::NULL_CONSOLE_READ`]
/// (the SBI legacy console exposes no non-blocking input drain), so fd 0
/// reads keep failing closed exactly as before.
pub static RISCV_UART_CONSOLES: [tairix_kernel_core::ConsoleDevice; 1] =
    [tairix_kernel_core::ConsoleDevice::new(
        &RISCV_UART_CONSOLE,
        &tairix_kernel_core::NULL_CONSOLE_READ,
    )];

/// Failure modes of [`boot`] and [`build_boot_memory_map`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BootError {
    /// The boot hart was not hart 0; the single-hart slice only brings
    /// up logical CPU 0 on the boot hart.
    UnexpectedHart,
    /// The device tree at `a1` could not be parsed.
    Fdt,
    /// The device tree advertised no `/memory` node.
    NoMemoryMap,
    /// The device tree advertised no `/cpus` `timebase-frequency`.
    NoTimebase,
    /// The kernel image plus heap left no usable RAM above it.
    UsableRegionEmpty,
    /// The boot page-table pool could not satisfy the Sv39 identity
    /// map, so the MMU could not be enabled (`plans/PI.md` RV-P2).
    MmuEnableFailed,
    /// `BootInfo::validate` rejected the assembled hand-off.
    BootInfoInvalid,
}

impl BootError {
    /// Stable cause string for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnexpectedHart => "unexpected_boot_hart",
            Self::Fdt => "fdt_parse_failed",
            Self::NoMemoryMap => "no_memory_map",
            Self::NoTimebase => "no_timebase_frequency",
            Self::UsableRegionEmpty => "usable_region_empty",
            Self::MmuEnableFailed => "mmu_enable_failed",
            Self::BootInfoInvalid => "bootinfo_invalid",
        }
    }
}

/// Build the physical-memory map from the device tree at `dtb`:
/// reserve everything from RAM base through the end of the kernel image
/// + boot heap (`__kernel_end`), then mark the remainder usable.
///
/// This is the single riscv64 boot memory-map builder: [`try_boot`] uses it to assemble the `kernel_core` hand-off,
/// and the downstream `tests/integration/riscv64_boot` consumer uses it
/// to publish the same map to its device-bring-up observers before
/// delegating here.
///
/// # SAFETY-INVARIANT
///
/// `dtb` must be the verbatim `a1` value OpenSBI handed the boot hart —
/// a pointer to a valid flattened device tree readable for the life of
/// the kernel. The arch port's `boot.s` forwards it unchanged.
pub fn build_boot_memory_map(dtb: u64) -> Result<BootMemoryMap, BootError> {
    // SAFETY: `dtb` is the verbatim `a1` pointer from OpenSBI (see the
    // SAFETY-INVARIANT above); it addresses a valid flattened device
    // tree that lives for the life of the guest. `Fdt::from_ptr`
    // validates the magic and bounds the blob before any further read.
    let fdt = unsafe { Fdt::from_ptr(dtb as *const u8) }.map_err(|_| BootError::Fdt)?;
    memory_map_from_fdt(&fdt, dtb)
}

/// Build the [`BootMemoryMap`] from an already-parsed `fdt` whose blob lives
/// at physical `dtb_base`.
///
/// The kernel image + boot heap `[ram_base, __kernel_end]` is reserved, the
/// remainder is usable — and the device-tree blob itself is reserved out of
/// that usable span. OpenSBI places the DTB high in RAM (well above the
/// kernel image), so without this the blob sits in usable memory the frame
/// allocator would eventually hand out and the early-boot RAM self-test
/// (`tairix_kernel_mem::ramtest`) would zero, destroying the tree every later
/// consumer (device discovery, the QEMU scenarios) still reads. The blob is
/// live for the life of the kernel, so it is reserved like the kernel image.
fn memory_map_from_fdt(fdt: &Fdt<'_>, dtb_base: u64) -> Result<BootMemoryMap, BootError> {
    let (ram_base, ram_size) = fdt.first_memory_region().ok_or(BootError::NoMemoryMap)?;
    let ram_end = ram_base
        .checked_add(ram_size)
        .ok_or(BootError::NoMemoryMap)?;

    let usable_start = align_up_u64(kernel_end_addr(), PAGE_SIZE as u64);
    if usable_start < ram_base || usable_start >= ram_end {
        return Err(BootError::UsableRegionEmpty);
    }
    let mut memory_map = BootMemoryMap::new();
    memory_map.push(MemoryRegion {
        kind: RegionKind::Reserved,
        start: PhysAddr::new(ram_base),
        length: usable_start - ram_base,
    });
    memory_map.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(usable_start),
        length: ram_end - usable_start,
    });
    // Reserve the whole frames the DTB blob occupies out of the usable
    // window, so the tree survives both the allocator and the RAM self-test
    // — the one shared reservation both DTB-bearing ports call.
    crate::mem_map::reserve_blob_frames(&mut memory_map, dtb_base, fdt.total_size() as u64);
    Ok(memory_map)
}

/// Enable the Sv39 identity MMU and install the S-mode trap vector on
/// the boot hart, returning `true` when the MMU is live.
///
/// Returns `false` (leaving `satp == 0`) when the boot page-table pool
/// cannot satisfy the identity map — a fail-closed signal the caller
/// logs and parks on rather than running `kernel_main` unpaged. The trap vector is installed only on success, so
/// `stvec` is never left pointing at a handler the paged regime did not
/// reach.
///
/// This is the riscv64 analogue of the aarch64 `enable_mmu_and_vectors`
/// (`plans/PI.md` P6c-2 → RV-P2): it makes RAM Normal/cacheable so the
/// `kernel_core` allocator and scheduler atomics run on well-defined
/// memory, and points `stvec` at the handler so a fault during the
/// remaining bring-up is taken rather than silently looping.
///
/// The transient [`AddressSpace`] handle is dropped on return; `satp`
/// keeps pointing at [`BOOT_PAGE_TABLES`]' root table, which lives for
/// the kernel's lifetime.
fn enable_mmu_and_vectors() -> bool {
    let Some(space) = AddressSpace::new_identity_gigapages(&BOOT_PAGE_TABLES, IDENTITY_GIGABYTES)
    else {
        return false;
    };
    // SAFETY: `new_identity_gigapages` identity-maps
    // `[0, IDENTITY_GIGABYTES GiB)` — everything below the kernel remap
    // window — so
    // the executing `pc`, the boot stack, the firmware DTB, the PLIC,
    // and the `virt` MMIO window all keep their physical addresses —
    // enabling the MMU does not move the ground under the running code,
    // exactly as `AddressSpace::switch`'s contract requires.
    // `install_trap_vector` then points `stvec` at the handler (without
    // enabling any interrupt source) so a synchronous fault during the
    // remaining bring-up is taken to a handler. Both run once, here, on
    // the boot hart.
    unsafe {
        space.switch();
        trap::install_trap_vector();
    }
    true
}

/// Boot the kernel on the boot hart and forward to
/// [`tairix_kernel_core::kernel_main`].
///
/// `log_sink` / `audit_sink` are the `&'static` sinks installed in the
/// [`BootInfo`]: in production both are the port's SBI-backed
/// [`tairix_arch_riscv64::SERIAL_SINK`]; a QEMU integration test
/// substitutes an audit sink that flips the `SiFive` Test device on
/// `AuditEvent::BootCompleted`.
///
/// `log_level` is the initial global log filter `kernel_main` installs
/// (`BootInfo::log_level`). Production passes [`Level::Info`]; an audit
/// observer vertical that must see the `Debug`-level allow records (e.g.
/// `SyscallInvoked`, `EventId(5000)`) passes [`Level::Debug`].
///
/// RV-P2: enables the Sv39 identity MMU and installs the trap vector +
/// the production `ecall` dispatch callback before handing off, so the
/// production path runs paged with syscall dispatch wired
/// (`plans/PI.md` RV-P2). No user space exists yet, so nothing `ecall`s
/// before user mode is wired (RV-P3); installing the callback here keeps
/// the ordering identical to the aarch64 / x86_64 boot paths. A failure to enable the MMU is fatal — the boot
/// path fails closed rather than running `kernel_main` unpaged.
///
/// Returns the bottom type. On failure it logs one
/// `KERNEL_BOOT_INIT_FAILED` record and parks the hart forever
/// (fail closed).
///
/// # SAFETY-INVARIANT
///
/// Called exactly once, on the boot hart, from the arch trampoline
/// after `boot.s`'s invariants hold (S-mode, stack established, `.bss`
/// zeroed, valid `a0`/`a1`).
pub fn boot(
    hartid: u64,
    dtb: u64,
    heap: &'static tairix_kalloc::FreeListAllocator,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
    log_level: Level,
) -> ! {
    // Make every kernel heap's lock interrupt-safe before anything can be
    // interrupted while holding one: install this port's per-hart
    // `sstatus.SIE` mask/restore. Done at boot entry — before interrupts are
    // ever enabled and before any secondary hart is started — so an interrupt
    // can never fire on a hart mid-allocation and reenter the allocator,
    // spinning forever on the lock its own interrupted mainline holds (a
    // single-CPU self-deadlock). One install covers every hart and every heap
    // the binary holds: the hooks mask the *current* hart's interrupts and
    // are read by the allocator itself, so a heap the boot handover never
    // names is covered too.
    tairix_kalloc::install_irq_control(kalloc_irq_disable, kalloc_irq_restore);

    // RV-P2: enable the Sv39 identity MMU + S-mode trap vector before
    // any allocator/scheduler work, then install the production `ecall`
    // dispatch callback. The arch port's `ecall` trap path fails closed
    // if it fires before a callback is installed, so pin it before any
    // user thread can run.
    let mmu_on = enable_mmu_and_vectors();

    // Route a fatal S-mode (kernel-mode) trap into the one fatal-report
    // path, now that `stvec` above can deliver one and before any further
    // work can fault. With the slot empty the trap handler parks the hart
    // with interrupts masked and prints nothing, so a kernel fault becomes
    // a mute machine — and, if it lands inside a lock's critical section, a
    // system-wide deadlock behind the guard it never dropped.
    //
    // The slot is set-once and this call never overrides an occupant: a
    // QEMU vertical that observes its own deliberate faults publishes its
    // handler ahead of `boot` and keeps it, asserting its own install
    // succeeded. Either way the machine has exactly one fatal policy.
    let _ = crate::riscv64::panic_ctx::install_kernel_fault_handler();

    syscall_entry::set_dispatch_callback(production_dispatch);
    // Demand-paged file mappings resolve their U-mode data page faults
    // through the same resident hook; install the resolver beside the
    // dispatch callback so both are in place before user space exists.
    // This single-entry boot path installs exactly once; a second publish
    // would be a programmer error, so it parks fail-closed rather than
    // running with an unpredictable fault path.
    if tairix_arch_riscv64::fault::set_user_fault_resolver(production_user_fault).is_err() {
        halt_current_hart()
    }
    // Beside the resolver, install the terminator the trap handler uses for a
    // U-mode exception it cannot resolve (a wild jump's instruction page
    // fault, an illegal instruction, a misaligned access): it kills the
    // offending task and keeps the hart alive, so one task's bad instruction
    // can never park a core. Installed once, before user space; a second
    // publish is a programmer error that parks fail-closed rather than
    // running with an unpredictable fault path.
    if tairix_arch_riscv64::fault::set_user_fault_terminator(production_user_fault_terminate)
        .is_err()
    {
        halt_current_hart()
    }

    log_reached(log_sink, hartid, dtb, mmu_on);

    if !mmu_on {
        // The boot page-table pool could not satisfy the identity map;
        // refuse to run `kernel_main` on un-paged memory (fail closed).
        log_init_failure(log_sink, BootError::MmuEnableFailed);
        halt_current_hart()
    }

    match try_boot(hartid, dtb, heap, log_sink, audit_sink, log_level) {
        Ok(boot_info) => kernel_main(boot_info),
        Err(err) => {
            log_init_failure(log_sink, err);
            halt_current_hart()
        }
    }
}

/// Mask this hart's supervisor interrupts for a kernel-heap-allocator
/// critical section, returning the prior `sstatus.SIE` as an opaque token.
///
/// The `fn`-pointer adapter the boot path installs into the global heap so
/// the allocator's lock is interrupt-safe: an interrupt taken on a hart
/// already holding the lock can no longer reenter `alloc`/`dealloc` and spin
/// forever on the lock its own interrupted mainline holds. It masks through
/// the port's one masking primitive, so the discipline is defined once; the
/// token exists only because a `fn` pointer cannot carry the state type.
fn kalloc_irq_disable() -> usize {
    <SstatusIrqControl as InterruptControl>::disable().as_token()
}

/// Restore this hart's supervisor interrupt state from a token
/// [`kalloc_irq_disable`] returned, closing the allocator critical section.
fn kalloc_irq_restore(token: usize) {
    // SAFETY: `token` is the `sstatus.SIE` state a paired `kalloc_irq_disable`
    // captured on this hart; restoring it re-enables interrupts only if they
    // were enabled before.
    unsafe {
        <SstatusIrqControl as InterruptControl>::restore(SstatusState::from_token(token));
    }
}

/// The retained, tail-able in-memory boot audit ring for this port.
///
/// Composed into the boot audit channel through [`AUDIT_SINK`], so every
/// audit record the kernel emits from the earliest boot onward is teed into
/// it and can be read back non-destructively — the store the pre-boot
/// Supervisor's `log` command tails (`plans/NEW-SUPERVISOR.md`). It is
/// guarded by the port's one interrupt-masking primitive, so a record copy
/// masks this hart's interrupts for its short, allocation-free duration and
/// the ring is safe to write from an interrupt handler that logs. It stamps
/// each record with the kernel's monotonic since-boot clock
/// ([`boot_audit_clock`]).
pub static BOOT_AUDIT_RING: BootAuditRing<BOOT_AUDIT_RING_CAPACITY, SstatusIrqControl> =
    BootAuditRing::new(boot_audit_clock);

/// The production boot **audit** channel: a fan-out delivering each record to
/// both the SBI serial console ([`SERIAL_SINK`]) and the retained
/// [`BOOT_AUDIT_RING`].
///
/// `main.rs` passes this as [`BootInfo`]'s audit sink for the production
/// binary; the QEMU boot verticals substitute their own audit sink through
/// [`boot`] directly, so retaining the trail is a production-only wiring and
/// never disturbs a test's audit interception.
pub static AUDIT_SINK: TeeSink<'static, 2> = TeeSink::new([&SERIAL_SINK, &BOOT_AUDIT_RING]);

/// Log the RV-P2 paged-boot init line (MMU + dispatch reached).
fn log_reached(sink: &(dyn Sink + Sync), hartid: u64, dtb: u64, mmu_on: bool) {
    let level = if mmu_on { Level::Info } else { Level::Warn };
    log(
        sink,
        &Event {
            level,
            id: KERNEL_BOOT_RISCV64_REACHED,
            message:
                "tairix-kernel riscv64 (qemu virt / sifive): reached rv-p2 paged boot init point",
            fields: &[
                Field {
                    key: "boot_hart_ok",
                    value: tairix_log::FieldValue::Str(yes_no(hartid == u64::from(BOOT_CPU))),
                },
                Field {
                    key: "dtb_present",
                    value: tairix_log::FieldValue::Str(yes_no(dtb != 0)),
                },
                Field {
                    key: "mmu_enabled",
                    value: tairix_log::FieldValue::Str(yes_no(mmu_on)),
                },
                Field {
                    key: "dispatch_installed",
                    value: tairix_log::FieldValue::Str(yes_no(
                        syscall_entry::dispatch_callback().is_some(),
                    )),
                },
                Field {
                    key: "next_stage",
                    value: tairix_log::FieldValue::Str("rv_p3_spawn_init_u_mode"),
                },
            ],
        },
    );
}

fn log_init_failure(sink: &(dyn Sink + Sync), err: BootError) {
    log(
        sink,
        &Event {
            level: Level::Error,
            id: KERNEL_BOOT_INIT_FAILED,
            message: "kernel boot init failed",
            fields: &[Field {
                key: "cause",
                value: tairix_log::FieldValue::Str(err.as_str()),
            }],
        },
    );
}

/// Assemble the validated [`BootInfo`] hand-off for the boot hart.
///
/// Split out from [`boot`] so the failure path is a plain `Result` with
/// no `unwrap`/`panic`.
///
/// # SAFETY-INVARIANT
///
/// As [`boot`]: `dtb` is the verbatim `a1` device-tree pointer OpenSBI
/// handed the boot hart.
pub fn try_boot(
    hartid: u64,
    dtb: u64,
    heap: &'static tairix_kalloc::FreeListAllocator,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
    log_level: Level,
) -> Result<BootInfo<'static, RiscvBinArch>, BootError> {
    // Single-hart boot slice: one per-CPU slot, owned by an
    // allocator-free `static` backing.
    static STORAGE: RiscvArchStorage<1> = RiscvArchStorage::new();

    if hartid != u64::from(BOOT_CPU) {
        return Err(BootError::UnexpectedHart);
    }

    // 1. Parse the device tree once for RAM extent and the timer
    //    frequency.
    //
    // SAFETY: `dtb` is the verbatim `a1` pointer from OpenSBI (see the
    // `boot` SAFETY-INVARIANT); it addresses a valid flattened device
    // tree that lives for the life of the guest.
    let fdt = unsafe { Fdt::from_ptr(dtb as *const u8) }.map_err(|_| BootError::Fdt)?;
    let timebase_hz = fdt.timebase_frequency().ok_or(BootError::NoTimebase)?;

    // Capture the firmware-provided boot seed (`/chosen/rng-seed`) before the
    // parsed tree is consumed by hardware discovery, so `kernel_core::
    // kernel_main` can fold it into the CSPRNG seed. Under QEMU (or any OpenSBI
    // that lacks a usable `Zkr` seed CSR) with a deterministic cycle counter
    // this is the only usable entropy source, so without it the reserve — and
    // ramzip's sealing key with it — would never seed. Input material only,
    // XOR-mixed and DRBG-conditioned kernel-side; never trusted alone.
    if let Some(seed) = fdt.chosen_rng_seed() {
        tairix_kernel_core::random::capture_boot_entropy_seed(seed);
    }

    // 2. Build the physical-memory map from the same parsed tree. The
    //    installed-RAM total is the device tree's whole `/memory` window —
    //    the figure the ungated `boot_facts_get` syscall reports.
    let installed_memory_bytes = fdt.first_memory_region().map_or(0, |(_base, size)| size);
    let memory_map = memory_map_from_fdt(&fdt, dtb)?;

    // 3. Assemble the hand-off and validate it before handing control
    //    to the architecture-neutral kernel core.
    let cpu_name = fdt
        .boot_cpu_compatible()
        .and_then(tairix_arch_riscv64::cpuname::name_for_compatible)
        .and_then(tairix_abi::CpuName::new);

    // Discover the platform hardware tree from the firmware device tree and
    // publish it to the authoritative `HW_TREE` the `hw_tree_read` /
    // `hw_tree_wait` syscalls read, so user space observes the same
    // inventory the kernel discovered (Design D). It also runs the
    // bootstrap-floor virtio-MMIO `DeviceID` probe, so the served tree
    // carries the autoloadable per-device Block/Input/Network nodes. It
    // consumes the parsed `fdt`, which no later step needs. Returns the
    // leaked `'static` tree so the root-storage bind resolution below can
    // borrow the same nodes.
    let tree = seed_hardware_tree(fdt, dtb, log_sink);

    // Resolve + audit which discovered node carries the bootstrap root block
    // device, and which floor block driver binds it, through the same shared
    // `lib/devmatch` policy the user-space `devmgr` uses, then stash the
    // binding (with the firmware DTB pointer and the tree) for the init seam
    // where the in-kernel root-unlock kthread reads it once
    // (`plans/NETWORK.md` N4e-riscv64, the aarch64 buffered-tree analogue). A
    // `None` binding (no/ambiguous disk) leaves the unlock a no-op and `login`
    // fails closed; the tree is still stashed so an input driver can still
    // autoload once the store is reachable.
    let binding = crate::root_storage::resolve_root_block_driver(tree, log_sink);
    crate::unlock_service::record_boot(binding, dtb, tree);

    let arch = Arc::new(RiscvBinArch::new(
        RiscvArch::new(&STORAGE, BOOT_CPU, timebase_hz),
        cpu_name,
    ));
    // Publish the arch handle for the panic-handler bridge before it is
    // moved into `BootInfo` (a panic after this point carries registers +
    // a backtrace; before it, the pre-init SBI path runs).
    // SAFETY: `arch` is moved into `BootInfo` immediately below (which
    // `kernel_main` consumes and stores); `Arc::as_ptr` is stable for the
    // lifetime of any clone of the `Arc`.
    unsafe {
        crate::riscv64::panic_ctx::publish_arch(Arc::as_ptr(&arch));
    }
    let boot_info = BootInfo::new(
        BOOT_CPU,
        1,
        "",
        memory_map,
        SchedulerConfig::defaults_for(1),
        arch,
        log_sink,
        audit_sink,
        log_level,
        &DISPATCH_SLOT,
        heap,
    )
    // Install the SBI console as the only console-list entry so PID 1
    // `init` and its session can write their startup banners. Its read half is the fail-closed
    // `NULL_CONSOLE_READ`: the SBI legacy console exposes no
    // non-blocking input drain, so fd 0 fails closed this slice.
    .with_consoles(&RISCV_UART_CONSOLES)
    // Record the device-tree-discovered installed-RAM total so the core
    // mints the `boot_facts_get` machine summary from it.
    .with_installed_memory(installed_memory_bytes)
    // Hand the shared identity cell to the core: the sec phase
    // publishes the compiled-in system identity into it, so the
    // system/service accounts resolve (spawn-as-user, filesystem
    // groups) from first boot; a later encrypted-root unlock replaces
    // the held table with the merged system∪human table.
    .with_spawn_identity(&crate::root_mount::LATE_IDENTITY)
    // Install the PID 1 spawn seam (`plans/PI.md` RV-P3): once every init
    // phase has succeeded and `kernel_main` emits `BootCompleted`, the core
    // invokes it to build `init`'s U-mode image and drop into user mode.
    .with_init(&crate::riscv64::init_spawn::RISCV_INIT_SPAWN)
    // Install the runtime `spawn` producer + embedded-program registry
    // (`plans/PI.md` RV-P3 / `plans/SPAWN.md` SP3b): the `spawn` syscall
    // resolves a path against the registry and drives the producer to build
    // a fresh, hardware-isolated child Sv39 space, so PID 1 `init` can
    // launch the user's session.
    .with_spawn(
        &crate::spawn_layout::PROGRAM_REGISTRY,
        &crate::riscv64::spawn_producer::RISCV_PROCESS_SPAWN,
    )
    // Serve the discovered hardware tree: the
    // `hw_tree_read` / `hw_tree_wait` syscalls read the one authoritative
    // `HW_TREE`, so the user-space device manager observes the same
    // inventory the kernel discovered (Design D).
    .with_hw_tree(&crate::hwtree_store::HW_TREE_SOURCE)
    // Install the on-disk application store (`plans/APPS.md` deliverable 8):
    // this port embeds no program rows, so every command app and service is
    // spawned from its verified `/System` store bundle. The storage bring-up
    // resolves the store's readiness latch on every outcome (mount installed
    // or given up), so a spawn racing the mount parks and always wakes.
    .with_app_store(&crate::app_store::APP_STORE)
    // Hand the syscall dispatch hook the shared set-once credential cell: the
    // in-kernel root-unlock kthread publishes the mounted root volume's
    // database into it once the operator's passphrase unlocks the encrypted
    // root. Until that install the cell fails every `users_db_read` closed, so
    // login refuses every attempt until a root is mounted.
    .with_users_db(&crate::root_mount::LATE_USERS_DB)
    .with_users_admin(&crate::root_mount::LATE_USERS_ADMIN)
    // Serve the `fs_*` syscalls through the production filesystem service: it
    // routes each operation through the secured VFS against the late-installed
    // read-only `/System` mount. The cell fails closed until the disk-owning
    // task publishes the `/System` window (`system_mount::install_system_mount`),
    // so wiring the hook here changes no boot behaviour until that install
    // lands.
    .with_filesystem(&crate::system_mount::FS_SERVICE)
    // Resolve `id::<volume-id>/…` paths against the volume forest the
    // mount/unlock tasks publish each mounted volume's stable identity into
    // (`plans/DEVICES.md` D3a). Fails closed `NotFound` until a volume is
    // published, so wiring it here changes no boot behaviour.
    .with_volumes(&crate::system_mount::VOLUME_FOREST)
    // Delegate runtime volume attach/detach (`plans/DEVICES.md` D3b) to the
    // production service; it fails closed `NotImplemented` until the mount
    // task wires its audit sink and pressure gauge.
    .with_volume_service(&crate::volume_service::VOLUME_SERVICE);
    boot_info
        .validate()
        .map_err(|_| BootError::BootInfoInvalid)?;
    Ok(boot_info)
}

/// Discover the platform hardware tree from the firmware `fdt` and publish
/// it to the authoritative [`crate::hwtree_store::HW_TREE`] the
/// `hw_tree_read` / `hw_tree_wait` syscalls read, so user space observes the
/// same inventory the kernel discovered (Design D).
///
/// Two phases feed the one buffered tree:
///
/// 1. Device-tree normalisation through the port's
///    [`tairix_arch_riscv64::platform::FdtDiscovery`] (root, memory, timer)
///    — pure, no MMIO register access.
/// 2. The bootstrap-floor virtio-MMIO `DeviceID` probe
///    (`plans/NETWORK.md` N4e-riscv64): the raw `virtio,mmio` firmware nodes
///    carry only their `compatible` string, which binds no driver — the
///    virtio bind key is the device id read from the transport. The probe
///    builds the MMIO bus from the same device tree
///    ([`tairix_drv_bus_mmio::virtio_mmio_bus_from_dtb`]) and reads each
///    slot's `DeviceID` through the frozen bus seam, emitting the probed
///    Block / Input / Network child nodes (`crate::hwdiscovery`) into the
///    same sink. The interrupt-driven input/network nodes carry their
///    discovered PLIC line, resolved by the arch port's
///    [`tairix_arch_riscv64::fdt::plic_device_source`] — a discovered value,
///    never a board constant.
///
/// The probe is safe here: [`boot`] enabled the Sv39 identity MMU before
/// `try_boot`, so the `virt`-board virtio-MMIO aperture the device tree
/// describes keeps its physical address and the side-effect-free `DeviceID`
/// reads land on mapped Device memory. A board whose tree describes no
/// `virtio,mmio` node (a bare SiFive part) makes `virtio_mmio_bus_from_dtb`
/// return `NotFound`, so the probe is a no-op there — additive and
/// metal-neutral.
///
/// Fail closed throughout: a malformed tree, a bus that cannot be built, or
/// an over-full/erroring bus leaves whatever was already collected and seeds
/// that, so the syscalls report the devices that *were* discovered rather
/// than failing the boot. The buffered tree is leaked to `'static` (a
/// one-shot boot publish, never a mutable global) so the inventory readers
/// can borrow it for the kernel's lifetime.
///
/// # SAFETY-INVARIANT
///
/// `dtb` is the verbatim `a1` device-tree pointer OpenSBI handed the boot
/// hart (as [`boot`]); it addresses the identity-mapped firmware blob that
/// lives for the kernel's life, so the per-slot re-parse and the bus builder
/// read valid, immutable bytes.
fn seed_hardware_tree(
    fdt: Fdt<'_>,
    dtb: u64,
    log_sink: &'static (dyn Sink + Sync),
) -> &'static [tairix_abi::HwNode] {
    use tairix_arch_api::PlatformDiscovery;
    // The validated blob's own length, captured before the discovery walk
    // consumes the `fdt` reader, so the virtio-MMIO probe below can reborrow
    // the same firmware bytes the bus builder needs.
    let total = fdt.total_size();
    // Record the PLIC register base + `riscv,ndev` source count for the
    // external-interrupt dispatch install (`irq::install_dispatch`, run later
    // in the kernel-core `Irq` phase, which has no device tree). Read before
    // the discovery walk consumes the `fdt` reader; a board with no PLIC
    // leaves both `None`, so nothing is recorded and the dispatch install
    // wires no external IRQ (interrupt-driven bring-up then fails closed).
    if let (Some(base), Some(ndev)) = (
        tairix_arch_riscv64::fdt::plic_base(&fdt),
        tairix_arch_riscv64::fdt::plic_ndev(&fdt),
    ) {
        crate::riscv64::irq::record_plic(base, ndev);
    }
    let mut sink = crate::boot_hwtree::CollectingHwNodeSink::new();
    // A discovery error leaves the sink empty; seed whatever was collected.
    let _ = tairix_arch_riscv64::platform::FdtDiscovery::new(fdt).discover(&mut sink);

    // Bootstrap-floor virtio-MMIO probe. Build the bus from the discovered
    // device tree and enumerate each populated slot's probed identity.
    //
    // SAFETY: `dtb`/`total` bound the firmware blob validated by the
    // `try_boot` `Fdt::from_ptr`; it is identity-mapped and immutable for
    // the kernel's life, and the virtio-MMIO aperture it describes is
    // identity-mapped Device memory the probe alone reads (side-effect-free
    // `DeviceID` registers, MMU on — `boot` enabled it before `try_boot`).
    let dtb_bytes = unsafe { core::slice::from_raw_parts(dtb as *const u8, total) };
    if let Ok(bus) = unsafe { tairix_drv_bus_mmio::virtio_mmio_bus_from_dtb(dtb_bytes) } {
        // Resolve each virtio slot's PLIC source from the firmware tree
        // (`plic_device_source` reads the node's single `interrupts` cell —
        // a discovered value, never a board constant) so an emitted
        // input/network node carries the interrupt line its interrupt-driven
        // user-space driver parks on. The first `fdt` was consumed by the
        // discovery walk above, so re-parse the validated blob per slot;
        // there are only a handful of virtio slots, so the re-read is
        // negligible boot cost.
        //
        // SAFETY: as above — `dtb` addresses the identity-mapped, immutable
        // firmware blob and the MMU is on.
        let slot_irq = |slot_base: u64| -> Option<u32> {
            let fdt = unsafe { Fdt::from_ptr(dtb as *const u8) }.ok()?;
            tairix_arch_riscv64::fdt::plic_device_source(&fdt, slot_base)
        };
        // Each probe reads `bus` sequentially, so the immutable borrows do
        // not overlap. An enumeration error leaves the affected nodes
        // undiscovered rather than aborting the boot (fail closed).
        let _ = crate::hwdiscovery::observe_virtio_mmio_block_devices(&bus, &mut sink);
        let _ = crate::hwdiscovery::observe_virtio_mmio_input_devices(
            &bus, &slot_irq, &mut sink, log_sink,
        );
        let _ = crate::hwdiscovery::observe_virtio_mmio_network_devices(
            &bus, &slot_irq, &mut sink, log_sink,
        );
    }

    // Leak the buffered tree to `'static` (a one-shot boot publish, never a
    // mutable global) so the `hw_tree_read` / `hw_tree_wait` syscalls and the
    // root-storage bind resolution can borrow it for the kernel's lifetime.
    let tree: &'static [tairix_abi::HwNode] = sink.leak();
    crate::hwtree_store::HW_TREE.seed(tree);
    tree
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up_u64(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}
