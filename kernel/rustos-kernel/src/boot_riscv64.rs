//! Bare-metal boot pipeline for the riscv64 (QEMU `virt` / SiFive)
//! `rustos-kernel` binary — `plans/PI.md` RV-P1.
//!
//! [`boot`] is the single entry point. The binary's
//! `extern "C" fn kernel_main(hartid, dtb)` (called from the arch
//! port's [`rustos_arch_riscv64::entry`] trampoline, `boot.s` →
//! `entry.rs`) forwards to it after `boot.s` has established the boot
//! stack and zeroed `.bss` on the boot hart. It performs the minimum
//! BSP bring-up the boot-to-`BootCompleted` slice needs and hands a
//! validated [`rustos_kernel_core::BootInfo`] to
//! [`rustos_kernel_core::kernel_main`]:
//!
//! 1. Parse the flattened device tree (`a1`) for the first `/memory`
//!    node and the `/cpus` `timebase-frequency`.
//! 2. Build a [`rustos_kernel_mem::BootMemoryMap`] that reserves the
//!    firmware + kernel-image + boot-heap span `[ram_base,
//!    __kernel_end)` and marks `[__kernel_end, ram_end)` usable
//!    ([`build_boot_memory_map`]).
//! 3. Construct the [`RiscvBinArch`] handle (boot hart + timebase) and
//!    assemble the `BootInfo`.
//!
//! No paging or trap setup is required to reach `BootCompleted`: the
//! `virt` board enters S-mode with `satp = 0` (bare addressing), RISC-V
//! atomics are well-defined on it, and the init pipeline never faults.
//! Enabling the Sv39 MMU and dropping PID 1 into user mode are staged
//! follow-ups (`plans/PI.md` RV-P-series), exactly as the aarch64 port
//! reached its boot-init point before P4–P6 wired user mode.
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
use rustos_arch_riscv64::{halt_current_hart, RiscvArch, RiscvArchStorage};
use rustos_kernel_core::{kernel_main, BootInfo, DispatchCallbackSlot, KernelArch};
use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
use rustos_kernel_sched_api::SchedulerConfig;
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

/// Logical CPU id of the boot hart for the single-hart slice.
const BOOT_CPU: CpuId = 0;

/// Audit event id emitted on a boot-init failure. Shares the
/// `4000..5000` `kernel/core` range and the top-of-range slot the
/// x86_64 pipeline uses for the same "init failed before `kernel_main`"
/// signal, so external audit consumers decode one stable id across
/// arches (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_INIT_FAILED: EventId = EventId(4099);

/// Bin-crate [`DispatchCallbackSlot`] handed to [`BootInfo`]. Set-once
/// via its internal `OnceCell`; the riscv64 boot-to-`BootCompleted`
/// slice does not enable a syscall trampoline, so nothing reads it
/// before `BootCompleted` (`AGENTS.md` §2.1).
static DISPATCH_SLOT: DispatchCallbackSlot = DispatchCallbackSlot::new();

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

/// Boot the kernel on the boot hart and forward to
/// [`rustos_kernel_core::kernel_main`].
///
/// `log_sink` / `audit_sink` are the `&'static` sinks installed in the
/// [`BootInfo`]: in production both are the port's SBI-backed
/// [`rustos_arch_riscv64::SERIAL_SINK`]; a QEMU integration test
/// substitutes an audit sink that flips the `SiFive` Test device on
/// `AuditEvent::BootCompleted`.
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
    match try_boot(hartid, dtb, log_sink, audit_sink) {
        Ok(boot_info) => kernel_main(boot_info),
        Err(err) => {
            log_init_failure(log_sink, err);
            halt_current_hart()
        }
    }
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
    let memory_map = memory_map_from_fdt(&fdt)?;

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
