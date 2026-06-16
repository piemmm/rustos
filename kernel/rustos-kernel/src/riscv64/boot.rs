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
//! implementation and names no concrete kernel subsystem (`AGENTS.md`
//! §17.2). This pipeline names `kernel/{core,mem,sec}` and
//! `kernel/sched/api`, so it lives here — exactly as x86_64 keeps its
//! boot pipeline and `BinArch` wrapper, and aarch64 its `boot_aarch64`,
//! in this crate. [`RiscvBinArch`] is the local `KernelArch` wrapper
//! around the arch port's [`RiscvArch`] (orphan rules).
//!
//! The riscv64 QEMU verticals (`tests/integration/riscv64_boot` and the
//! virtio-MMIO / framebuffer bins it backs) consume this very pipeline
//! through that downstream crate — they publish the firmware map for
//! their device-bring-up observers and then delegate here, so there is
//! exactly one riscv64 boot orchestration (`AGENTS.md` §2.2).
//!
//! # No `unwrap` / `expect` / `panic!`
//!
//! Every fallible step returns a [`BootError`]; [`boot`] logs the stable
//! cause string and parks the hart via the arch port's
//! [`rustos_arch_riscv64::halt_current_hart`] (`AGENTS.md` §2.9, §2 —
//! fail closed).

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

use crate::riscv64::dispatch::{production_dispatch, DISPATCH_SLOT};

/// Logical CPU id of the boot hart for the single-hart slice.
const BOOT_CPU: CpuId = 0;

/// Audit event id emitted on a boot-init failure. Shares the
/// `4000..5000` `kernel/core` range and the top-of-range slot the
/// x86_64 pipeline uses for the same "init failed before `kernel_main`"
/// signal, so external audit consumers decode one stable id across
/// arches (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_INIT_FAILED: EventId = EventId(4099);

/// Audit event: the riscv64 production kernel reached its RV-P2 paged
/// boot init point (Sv39 MMU enabled, trap vector + `ecall` dispatch
/// installed). Shares the `kernel/core`-owned `4000..5000` range and the
/// `4097` "reached" slot the aarch64 boot pipeline uses; only one arch's
/// boot module compiles per image, so the id never collides at runtime
/// (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_RISCV64_REACHED: EventId = EventId(4097);

/// Audit event: the boot path's kthread guard-arena decision
/// (`plans/PI.md` G3b-2) — carved+installed (Info) or software-canary
/// fallback (Warn), logged through the shared
/// [`crate::mem_map::log_guard_arena`] body. Shares the `kernel/core`-owned
/// `4000..5000` range; `4097`/`4099` are taken by the reached/init-failed
/// records above, and `4098` is free in this image (the x86_64 pipeline
/// uses it for its TSC-invariance record, but only one arch's boot module
/// compiles per image, so the id never collides at runtime, `AGENTS.md`
/// §5.4.4).
const KERNEL_BOOT_GUARD_ARENA: EventId = EventId(4098);

/// Exclusive upper bound for the kthread-stack guard arena: the spawn
/// seams' per-task identity window (`init_spawn_riscv64` /
/// `spawn_producer_riscv64` build `IDENTITY_GIB = 4` GiB Sv39 spaces). A
/// kthread stack above this would be unreachable — and its guard page
/// unfaultable — under the owning task's own root, so the carve refuses to
/// place the arena there (`AGENTS.md` §4).
const KTHREAD_ARENA_IDENTITY_LIMIT: u64 = 4 << 30;

/// Number of 1 GiB identity gigapages the boot address space maps.
///
/// 512 covers the whole Sv39 low VA range (`[0, 512 GiB)`) in a single
/// root table, so the kernel image, stack, the firmware DTB, the PLIC,
/// and the `virt`-board MMIO window all keep their physical addresses
/// once the MMU is on — whatever their addresses, with no `cfg(board)`
/// fork (`AGENTS.md` §17.2). Identity mapping makes physical == virtual,
/// so the device-bring-up verticals that read MMIO/DMA at physical
/// addresses keep working under the paged regime.
const IDENTITY_GIGABYTES: usize = 512;

/// Boot-time page-table frame source for the Sv39 identity map.
///
/// A single root table holds all 512 gigapage leaves, so the pool only
/// ever hands out one frame here. It lives in `.bss` for the lifetime of
/// the kernel image, so `satp` keeps pointing at a valid table after
/// [`enable_mmu_and_vectors`] returns even though the transient
/// [`AddressSpace`] handle is dropped (`AGENTS.md` §2.1 — the pool is
/// monotonic and never freed). The real per-process page tables are
/// built over the `kernel/mem` frame allocator at a later stage.
static BOOT_PAGE_TABLES: PageTablePool = PageTablePool::new();

/// Stable `"true"`/`"false"` audit-field value for a boolean condition.
/// Keeping the boot log to `&'static str` fields means the path takes no
/// allocation and cannot panic (`AGENTS.md` §2.9).
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
}

// SAFETY-INVARIANT: `RiscvBinArch::halt` returns the bottom type. The
// coercion fails to type-check if the impl ever loses `-> !`, pinning
// the contract at compile time (`AGENTS.md` §2.10).
const _RISCV_BIN_ARCH_HALT_RETURNS_NEVER: fn(&RiscvBinArch) -> ! =
    <RiscvBinArch as KernelArch>::halt;

/// The system console device the riscv64 boot path installs on
/// [`rustos_kernel_core::BootInfo`].
///
/// A zero-sized [`ConsoleWrite`] adapter over the SBI console: every
/// `stream_write` byte is forwarded verbatim through the arch port's
/// [`rustos_arch_riscv64::serial::write_console_bytes`] (no `\n`
/// translation — the bytes reach the device exactly as the program
/// wrote them, `AGENTS.md` §16.4). It is the riscv64 analogue of the
/// aarch64 `UartConsole`'s output half: the "first discovered console"
/// stream **backing** the spawner attaches to fd 1 (`AGENTS.md` §20),
/// not a program-facing interface.
///
/// No [`rustos_kernel_core::ConsoleRead`] half is installed: the SBI
/// legacy console exposes no non-blocking input drain, so fd 0 reads
/// fail closed (`AGENTS.md` §5.4) until a real input backing lands — PID
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
/// reads keep failing closed exactly as before (`AGENTS.md` §5.4).
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
    /// Stable cause string for audit records (`AGENTS.md` §5.4.4).
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
/// This is the single riscv64 boot memory-map builder (`AGENTS.md`
/// §2.2): [`try_boot`] uses it to assemble the `kernel_core` hand-off,
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
/// logs and parks on rather than running `kernel_main` unpaged
/// (`AGENTS.md` §2.9). The trap vector is installed only on success, so
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
/// the kernel's lifetime (`AGENTS.md` §2.1).
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
/// RV-P2: enables the Sv39 identity MMU and installs the trap vector +
/// the production `ecall` dispatch callback before handing off, so the
/// production path runs paged with syscall dispatch wired
/// (`plans/PI.md` RV-P2). No user space exists yet, so nothing `ecall`s
/// before user mode is wired (RV-P3); installing the callback here keeps
/// the ordering identical to the aarch64 / x86_64 boot paths
/// (`AGENTS.md` §5.4.5). A failure to enable the MMU is fatal — the boot
/// path fails closed rather than running `kernel_main` unpaged.
///
/// Returns the bottom type. On failure it logs one
/// `KERNEL_BOOT_INIT_FAILED` record and parks the hart forever
/// (`AGENTS.md` §2.9 — fail closed).
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
) -> ! {
    // RV-P2: enable the Sv39 identity MMU + S-mode trap vector before
    // any allocator/scheduler work, then install the production `ecall`
    // dispatch callback. The arch port's `ecall` trap path fails closed
    // if it fires before a callback is installed, so pin it before any
    // user thread can run (`AGENTS.md` §5.4.5).
    let mmu_on = enable_mmu_and_vectors();
    syscall_entry::set_dispatch_callback(production_dispatch);

    log_reached(log_sink, hartid, dtb, mmu_on);

    if !mmu_on {
        // The boot page-table pool could not satisfy the identity map;
        // refuse to run `kernel_main` on un-paged memory (fail closed).
        log_init_failure(log_sink, BootError::MmuEnableFailed);
        halt_current_hart()
    }

    match try_boot(hartid, dtb, log_sink, audit_sink) {
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
                    value: yes_no(hartid == u64::from(BOOT_CPU)),
                },
                Field {
                    key: "dtb_present",
                    value: yes_no(dtb != 0),
                },
                Field {
                    key: "mmu_enabled",
                    value: yes_no(mmu_on),
                },
                Field {
                    key: "dispatch_installed",
                    value: yes_no(syscall_entry::dispatch_callback().is_some()),
                },
                Field {
                    key: "next_stage",
                    value: "rv_p3_spawn_init_u_mode",
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
                value: err.as_str(),
            }],
        },
    );
}

/// Assemble the validated [`BootInfo`] hand-off for the boot hart.
///
/// Split out from [`boot`] so the failure path is a plain `Result` with
/// no `unwrap`/`panic` (`AGENTS.md` §2.9).
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
    // discovered usable RAM (§24.2 policy, the sum of `Usable` region
    // lengths after the kernel-image reservation) and bounded to the seams'
    // 4 GiB identity window so the stack is reachable under the task's own
    // root. When no usable region fits a whole arena the carve returns
    // `None`, the install is skipped, and the seams fall back to a
    // software-canary `BoxStack` (fail closed, never fatal to boot,
    // `AGENTS.md` §2.9 / §2.17).
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
    // allocator-free `static` backing (`AGENTS.md` §24.1).
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
        Level::Info,
        &DISPATCH_SLOT,
    )
    // Install the SBI console as the only console-list entry so PID 1
    // `init` and its session can write their startup banners
    // (`AGENTS.md` §20). Its read half is the fail-closed
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
        &crate::riscv64::spawn_producer::RISCV_PROGRAM_REGISTRY,
        &crate::riscv64::spawn_producer::RISCV_PROCESS_SPAWN,
    );
    boot_info
        .validate()
        .map_err(|_| BootError::BootInfoInvalid)?;
    Ok(boot_info)
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up_u64(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}
