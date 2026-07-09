//! Bare-metal boot pipeline for the riscv64 (QEMU `virt` / SiFive)
//! `rustos-kernel` binary — `plans/PI.md` RV-P1 / RV-P2.
//!
//! [`boot`] is the single entry point. The binary's
//! `extern "C" fn kernel_main(hartid, dtb)` (called from the arch
//! port's [`rustos_arch_riscv64::entry`] trampoline, `boot.s` →
//! `entry.rs`) forwards to it after `boot.s` has established the boot
//! stack and zeroed `.bss` on the boot hart. It performs the BSP
//! bring-up the paged boot slice needs and hands a validated
//! [`rustos_kernel_core::BootInfo`] to
//! [`rustos_kernel_core::kernel_main`]:
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
//! 3. Build a [`rustos_kernel_mem::BootMemoryMap`] that reserves the
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
//! [`rustos_arch_riscv64::halt_current_hart`] (fail closed).

use alloc::sync::Arc;

use rustos_arch_api::{CpuId, SchedulerArch};
use rustos_arch_riscv64::context_hal::ContextSwitchHal;
use rustos_arch_riscv64::fdt::Fdt;
use rustos_arch_riscv64::paging::{AddressSpace, PageTablePool};
use rustos_arch_riscv64::{
    halt_current_hart, serial, syscall_entry, trap, RiscvArch, RiscvArchStorage,
};
use rustos_kernel_core::{kernel_main, BootInfo, ConsoleWrite, KernelArch};
use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
use rustos_kernel_sched_api::SchedulerConfig;
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::riscv64::dispatch::{production_dispatch, production_user_fault, DISPATCH_SLOT};

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

/// Audit event: the boot path's kthread guard-arena decision
/// (`plans/PI.md` G3b-2) — carved+installed (Info) or software-canary
/// fallback (Warn), logged through the shared
/// [`crate::mem_map::log_guard_arena`] body. Shares the `kernel/core`-owned
/// `4000..5000` range; `4097`/`4099` are taken by the reached/init-failed
/// records above, and `4098` is free in this image (the x86_64 pipeline
/// uses it for its TSC-invariance record, but only one arch's boot module
/// compiles per image, so the id never collides at runtime).
const KERNEL_BOOT_GUARD_ARENA: EventId = EventId(4098);

/// Exclusive upper bound for the kthread-stack guard arena: the spawn
/// seams' per-task identity window (`init_spawn_riscv64` /
/// `spawn_producer_riscv64` build `IDENTITY_GIB = 4` GiB Sv39 spaces). A
/// kthread stack above this would be unreachable — and its guard page
/// unfaultable — under the owning task's own root, so the carve refuses to
/// place the arena there.
const KTHREAD_ARENA_IDENTITY_LIMIT: u64 = 4 << 30;

/// Number of 1 GiB identity gigapages the boot address space maps.
///
/// 512 covers the whole Sv39 low VA range (`[0, 512 GiB)`) in a single
/// root table, so the kernel image, stack, the firmware DTB, the PLIC,
/// and the `virt`-board MMIO window all keep their physical addresses
/// once the MMU is on — whatever their addresses, with no `cfg(board)`
/// fork. Identity mapping makes physical == virtual,
/// so the device-bring-up verticals that read MMIO/DMA at physical
/// addresses keep working under the paged regime.
const IDENTITY_GIGABYTES: usize = 512;

/// Boot-time page-table frame source for the Sv39 identity map.
///
/// A single root table holds all 512 gigapage leaves, so the pool only
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
}

impl RiscvBinArch {
    /// Wrap `arch` so it can be handed to `kernel_core::kernel_main`.
    #[must_use]
    pub const fn new(arch: RiscvArch) -> Self {
        Self { arch }
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

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        self.arch.monotonic_ns()
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
            let _ = rustos_arch_riscv64::paging::park_kernel_root();
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

    fn install_irq_dispatch(&self, _table: &'static rustos_kernel_irq::IrqTable) {
        // Set up tickless supervisor-timer preemption now that the
        // scheduler is up (P-1b, `plans/PI.md` D2b-2b-A): register the
        // per-hart preempt storage, install the U-mode-preemption
        // callback, record the per-quantum interval derived from the
        // device-tree `timebase-frequency`, and enable `sie.STIE` — but
        // leave the timer disarmed. RustOS is tickless: the scheduler arms the one-shot to one quantum only when
        // it dispatches onto a contended hart (via
        // `RiscvArch::set_preemption`) and disarms otherwise, so a hart
        // running a sole task takes no timer ticks. The kernel keeps
        // `sstatus.SIE == 0`, so a tick is *taken* only while a U-mode task
        // runs (the privilege rule U < S). `_table` is unused here — the
        // riscv64 production boot wires no PLIC external-IRQ dispatch in
        // this slice (the default no-op).
        arm_preemption(self.arch.timebase_hz());
    }

    fn direct_phys_map(&self) -> Option<&'static (dyn rustos_kernel_mem::PhysMap + Sync)> {
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
/// [`DEFAULT_PREEMPT_QUANTUM_HZ`](rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ)
/// the aarch64 port also uses (defined once so the two cannot diverge): a ~10 ms slice bounds a runaway user task's hold on
/// a contended hart while costing negligible trap overhead. This is
/// **not** a periodic tick — the timer is armed one-shot to one quantum
/// only when a hart is contended (tickless). The
/// interval in `time`-CSR ticks is derived from the discovered
/// `timebase-frequency`, never a board constant.
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
const PREEMPT_TICK_HZ: u64 = rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ;

/// Caller-owned per-hart preemption backing for the production boot hart.
///
/// The production riscv64 image is single-hart (`BootInfo::new(BOOT_CPU,
/// 1, …)`), so a `PreemptStorage<1>` covers it; secondary-hart preemption
/// is sized from the discovered hart count when SMP bring-up lands
/// (the per-hart timer bookkeeping is the discovered
/// hart count, never a baked-in ceiling). Published once by
/// [`arm_preemption`].
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
static PREEMPT_STORAGE: rustos_arch_riscv64::preempt::PreemptStorage<1> =
    rustos_arch_riscv64::preempt::PreemptStorage::new();

/// The U-mode-preemption callback the timer trap path invokes for a tick
/// taken from U-mode (installed via
/// [`rustos_arch_riscv64::preempt::set_preempt_callback`]).
///
/// It suspends the user task currently running on `cpu` back to the
/// scheduler with [`rustos_kernel_core::RescheduleAction::Yield`] — the
/// *involuntary* analogue of a `yield` syscall: the task is re-enqueued at
/// its priority and the scheduler picks the next runnable task, giving
/// EEVDF-ordered time-slicing. [`rustos_kernel_core::reschedule_current`]
/// returns `false` when no resumable user kthread is published on `cpu`
/// (unreachable from U-mode with none switched in, but the fail-closed
/// return means a stray invocation is a harmless no-op rather than an
/// unsound switch). The call only ever runs after
/// [`on_timer_interrupt`](rustos_arch_riscv64::preempt) disarmed the SBI
/// timer (`set_timer(u64::MAX)`), so `sip.STIP` is already cleared across
/// the context switch; the scheduler re-arms the next one-shot on its
/// following dispatch (tickless).
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
extern "C" fn production_preempt_dispatch(cpu: rustos_arch_api::CpuId) {
    let _ =
        rustos_kernel_core::reschedule_current(cpu, rustos_kernel_core::RescheduleAction::Yield);
}

/// The per-tick callback the timer trap path invokes on **every** tick
/// (U-mode *or* idle S-mode), installed via
/// [`rustos_arch_riscv64::preempt::set_timer_callback`].
///
/// It latches the fired tick as this hart's pending preemption
/// ([`rustos_kernel_core::note_preempt_tick`]) and runs the blocking-wait
/// timed-wake sweep (Design D P-2): any waiter whose finite deadline has
/// elapsed is unparked and the one-shot is re-armed to the next pending
/// deadline ([`rustos_kernel_core::timed_wake_sweep`]), so a finite
/// `hw_tree_wait` timeout fires even when the hart is otherwise idle
/// (every task parked) and takes no preemption tick. Both halves are pure
/// accounting (they never context-switch), so they are safe on a tick
/// taken in S-mode; the *immediate* preemption of a U-mode task is the
/// separate [`production_preempt_dispatch`] U-mode-only callback, while a
/// tick taken in S-mode is honoured through the latch at the interrupted
/// syscall's completion — the running task's quantum is never silently
/// lost to a tick the non-preemptible kernel could not act on.
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
extern "C" fn production_tick_dispatch(cpu: rustos_arch_api::CpuId) {
    rustos_kernel_core::note_preempt_tick(cpu);
    rustos_kernel_core::timed_wake_sweep();
}

/// Set up tickless supervisor-timer preemption on the boot hart: register
/// the per-hart preempt storage, install the U-mode-preemption callback,
/// record the per-quantum interval from [`PREEMPT_TICK_HZ`], and enable
/// `sie.STIE` — but leave the timer disarmed. The scheduler arms the
/// one-shot to one quantum only when it dispatches onto a contended hart
/// (`RiscvArch::set_preemption`), and disarms otherwise (tickless / `NO_HZ`).
///
/// Called once per boot from [`RiscvBinArch::install_irq_dispatch`], in
/// the kernel-core `Irq` phase — after the scheduler is built and before
/// `init` drops to U-mode. The kernel runs with `sstatus.SIE == 0`, so no
/// tick is *taken* until a U-mode task runs (the privilege rule U < S),
/// so this is **additive and non-regressing**: a tick taken in U-mode
/// drives [`production_preempt_dispatch`] immediately, and a one-shot
/// that fires in S-mode disarms without context-switching (the kernel is
/// non-preemptible) but is latched by [`production_tick_dispatch`] and
/// honoured when the interrupted syscall completes — an expired quantum
/// is never silently lost.
///
/// No *scheduler-fairness* tick callback is installed: EEVDF is tickless
/// (fairness is advanced inside `Scheduler::step`, not by a periodic
/// count). The per-tick callback that *is* installed
/// ([`production_tick_dispatch`]) latches the pending preemption and runs
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
        use rustos_arch_riscv64::preempt;

        if timebase_hz == 0 {
            return;
        }

        // Set-once per boot; a stray re-call fails closed by halting rather
        // than re-pointing the live per-hart slices.
        if PREEMPT_STORAGE.register().is_err() {
            halt_current_hart();
        }

        // Install the U-mode-preemption callback *before* arming the timer,
        // so the first tick taken from U-mode already has a handler.
        preempt::set_preempt_callback(production_preempt_dispatch);

        // Install the per-tick timed-wake sweep callback (Design D P-2), so
        // every tick — including one taken on an idle S-mode hart armed
        // solely for a blocking-wait deadline — releases any elapsed waiter
        // and re-arms the one-shot to the next deadline.
        preempt::set_timer_callback(production_tick_dispatch);

        let interval = preempt::interval_for_hz(timebase_hz, PREEMPT_TICK_HZ);

        // SAFETY: this is the boot hart (id 0); the preempt callback is
        // installed (above), the per-hart storage is registered (above),
        // and the trap vector is installed (`enable_mmu_and_vectors`, run
        // before `kernel_main`). It records the quantum, enables
        // `sie.STIE`, and leaves the timer disarmed; it does not set
        // `sstatus.SIE`, so no tick is taken until a U-mode task runs, and
        // the scheduler arms the first one-shot on its next dispatch.
        unsafe {
            preempt::init_local_preempt(0, interval);
        }
    }
    #[cfg(not(all(freestanding, kernel_isa = "riscv64")))]
    {
        let _ = timebase_hz;
    }
}

/// The system console device the riscv64 boot path installs on
/// [`rustos_kernel_core::BootInfo`].
///
/// A zero-sized [`ConsoleWrite`] adapter over the SBI console: every
/// `stream_write` byte is forwarded verbatim through the arch port's
/// [`rustos_arch_riscv64::serial::write_console_bytes`] (no `\n`
/// translation — the bytes reach the device exactly as the program
/// wrote them). It is the riscv64 analogue of the
/// aarch64 `UartConsole`'s output half: the "first discovered console"
/// stream **backing** the spawner attaches to fd 1,
/// not a program-facing interface.
///
/// No [`rustos_kernel_core::ConsoleRead`] half is installed: the SBI
/// legacy console exposes no non-blocking input drain, so fd 0 reads
/// fail closed until a real input backing lands — PID
/// 1 `init` and the embedded `Shell` `Run` program only *write* (a
/// banner) and `spawn`, so this slice needs no console input.
#[derive(Debug, Default, Copy, Clone)]
pub struct RiscvUartConsole;

impl ConsoleWrite for RiscvUartConsole {
    fn write(&self, bytes: &[u8]) -> Result<usize, rustos_abi::Errno> {
        // The busy-wait SBI transmit accepts every byte, so the write is
        // total and never short, and performs no `\n` translation.
        Ok(serial::write_console_bytes(bytes))
    }
}

/// The single `'static` [`RiscvUartConsole`] the boot path lists in the
/// [`rustos_kernel_core::BootInfo::with_consoles`] console list.
/// Zero-sized, so it has no `.bss`/`.data` footprint — mirroring
/// [`rustos_arch_riscv64::SERIAL_SINK`].
pub static RISCV_UART_CONSOLE: RiscvUartConsole = RiscvUartConsole;

/// The riscv64 boot console list: the SBI console is the only console.
/// Its read half is the fail-closed [`rustos_kernel_core::NULL_CONSOLE_READ`]
/// (the SBI legacy console exposes no non-blocking input drain), so fd 0
/// reads keep failing closed exactly as before.
pub static RISCV_UART_CONSOLES: [rustos_kernel_core::ConsoleDevice; 1] =
    [rustos_kernel_core::ConsoleDevice::new(
        &RISCV_UART_CONSOLE,
        &rustos_kernel_core::NULL_CONSOLE_READ,
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
    memory_map_from_fdt(&fdt)
}

/// Build the two-region [`BootMemoryMap`] from an already-parsed `fdt`.
fn memory_map_from_fdt(fdt: &Fdt<'_>) -> Result<BootMemoryMap, BootError> {
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
    // SAFETY: `new_identity_gigapages` identity-maps `[0, 512 GiB)`, so
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
/// [`rustos_kernel_core::kernel_main`].
///
/// `log_sink` / `audit_sink` are the `&'static` sinks installed in the
/// [`BootInfo`]: in production both are the port's SBI-backed
/// [`rustos_arch_riscv64::SERIAL_SINK`]; a QEMU integration test
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
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
    log_level: Level,
) -> ! {
    // RV-P2: enable the Sv39 identity MMU + S-mode trap vector before
    // any allocator/scheduler work, then install the production `ecall`
    // dispatch callback. The arch port's `ecall` trap path fails closed
    // if it fires before a callback is installed, so pin it before any
    // user thread can run.
    let mmu_on = enable_mmu_and_vectors();
    syscall_entry::set_dispatch_callback(production_dispatch);
    // Demand-paged file mappings resolve their U-mode data page faults
    // through the same resident hook; install the resolver beside the
    // dispatch callback so both are in place before user space exists.
    // This single-entry boot path installs exactly once; a second publish
    // would be a programmer error, so it parks fail-closed rather than
    // running with an unpredictable fault path.
    if rustos_arch_riscv64::fault::set_user_fault_resolver(production_user_fault).is_err() {
        halt_current_hart()
    }

    log_reached(log_sink, hartid, dtb, mmu_on);

    if !mmu_on {
        // The boot page-table pool could not satisfy the identity map;
        // refuse to run `kernel_main` on un-paged memory (fail closed).
        log_init_failure(log_sink, BootError::MmuEnableFailed);
        halt_current_hart()
    }

    match try_boot(hartid, dtb, log_sink, audit_sink, log_level) {
        Ok(boot_info) => kernel_main(boot_info),
        Err(err) => {
            log_init_failure(log_sink, err);
            halt_current_hart()
        }
    }
}

/// Log the RV-P2 paged-boot init line (MMU + dispatch reached).
fn log_reached(sink: &(dyn Sink + Sync), hartid: u64, dtb: u64, mmu_on: bool) {
    let level = if mmu_on { Level::Info } else { Level::Warn };
    log(
        sink,
        &Event {
            level,
            id: KERNEL_BOOT_RISCV64_REACHED,
            message:
                "rustos-kernel riscv64 (qemu virt / sifive): reached rv-p2 paged boot init point",
            fields: &[
                Field {
                    key: "boot_hart_ok",
                    value: rustos_log::FieldValue::Str(yes_no(hartid == u64::from(BOOT_CPU))),
                },
                Field {
                    key: "dtb_present",
                    value: rustos_log::FieldValue::Str(yes_no(dtb != 0)),
                },
                Field {
                    key: "mmu_enabled",
                    value: rustos_log::FieldValue::Str(yes_no(mmu_on)),
                },
                Field {
                    key: "dispatch_installed",
                    value: rustos_log::FieldValue::Str(yes_no(
                        syscall_entry::dispatch_callback().is_some(),
                    )),
                },
                Field {
                    key: "next_stage",
                    value: rustos_log::FieldValue::Str("rv_p3_spawn_init_u_mode"),
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
                value: rustos_log::FieldValue::Str(err.as_str()),
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
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
    log_level: Level,
) -> Result<BootInfo<'static, RiscvBinArch>, BootError> {
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

    // 2. Build the physical-memory map from the same parsed tree.
    let mut memory_map = memory_map_from_fdt(&fdt)?;

    // Carve a 2 MiB-aligned kthread-stack guard arena out of the map and
    // install it so the spawn seams (`init_spawn_riscv64`,
    // `spawn_producer_riscv64`) can draw kthread kernel stacks from it and
    // unmap each stack's guard page in the owning task's own Sv39 root —
    // turning a stack overrun into a synchronous store page fault rather
    // than a poison-canary detection (`plans/PI.md` G3b-2, the cross-port
    // sibling of the aarch64/x86_64 wiring). The arena is sized from the
    // discovered usable RAM (policy, the sum of `Usable` region
    // lengths after the kernel-image reservation) and bounded to the seams'
    // 4 GiB identity window so the stack is reachable under the task's own
    // root. When no usable region fits a whole arena the carve returns
    // `None`, the install is skipped, and the seams fall back to a
    // software-canary `BoxStack` (fail closed, never fatal to boot).
    let ram_bytes: u64 = memory_map
        .regions()
        .iter()
        .filter(|region| region.kind == RegionKind::Usable)
        .fold(0u64, |acc, region| acc.saturating_add(region.length));
    let guard_arena = crate::mem_map::carve_guard_arena_from_map(
        &mut memory_map,
        ram_bytes,
        KTHREAD_ARENA_IDENTITY_LIMIT,
    );
    if let Some(arena) = guard_arena {
        crate::stack_arena::KTHREAD_STACK_ARENA.install(
            arena.base,
            arena.len,
            &crate::stack_arena::IdentityBlockStore,
        );
    }
    crate::mem_map::log_guard_arena(
        log_sink,
        KERNEL_BOOT_GUARD_ARENA,
        guard_arena.map(|a| (a.base, a.len)),
    );

    // 3. Assemble the hand-off and validate it before handing control
    //    to the architecture-neutral kernel core.
    // Single-hart boot slice: one per-CPU slot, owned by an
    // allocator-free `static` backing.
    static STORAGE: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = Arc::new(RiscvBinArch::new(RiscvArch::new(
        &STORAGE,
        BOOT_CPU,
        timebase_hz,
    )));
    let boot_info = BootInfo::new(
        BOOT_CPU,
        1,
        "",
        memory_map,
        IdentityTableBuilder::new(),
        SchedulerConfig::defaults_for(1),
        arch,
        log_sink,
        audit_sink,
        log_level,
        &DISPATCH_SLOT,
    )
    // Install the SBI console as the only console-list entry so PID 1
    // `init` and its session can write their startup banners. Its read half is the fail-closed
    // `NULL_CONSOLE_READ`: the SBI legacy console exposes no
    // non-blocking input drain, so fd 0 fails closed this slice.
    .with_consoles(&RISCV_UART_CONSOLES)
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
    .with_hw_tree(&crate::hwtree_store::HW_TREE_SOURCE);
    boot_info
        .validate()
        .map_err(|_| BootError::BootInfoInvalid)?;
    Ok(boot_info)
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up_u64(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}
