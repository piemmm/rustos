//! Bare-metal boot pipeline for the riscv64 QEMU `virt` board.
//!
//! [`boot`] is the single entry point. The binary's
//! `extern "C" fn kernel_main(hartid, dtb)` (called from `entry.rs`)
//! forwards to it. It performs the minimum BSP bring-up the
//! boot-to-`BootCompleted` slice needs and hands a validated
//! [`rustos_kernel_core::BootInfo`] to
//! [`rustos_kernel_core::kernel_main`]:
//!
//! 1. Parse the flattened device tree (`a1`) for the first `/memory`
//!    node and the `/cpus` `timebase-frequency`.
//! 2. Build a [`rustos_kernel_mem::BootMemoryMap`] that reserves the
//!    firmware + kernel-image + boot-heap span `[ram_base,
//!    __kernel_end)` and marks `[__kernel_end, ram_end)` usable.
//! 3. Construct the [`RiscvArch`] handle (boot hart + timebase) and
//!    assemble the `BootInfo`.
//!
//! No paging or trap setup is required to reach `BootCompleted`: the
//! `virt` board enters S-mode with `satp = 0` (bare addressing) and the
//! init pipeline never faults. Sv39 paging, the trap vector, the DTB
//! virtio-mmio walk, and SMP are the next riscv64 deliverables
//! (`PLAN.md` Stage 4.D Item 4).
//!
//! # No `unwrap` / `expect` / `panic!`
//!
//! Every fallible step returns a [`BootError`]; [`boot`] logs the
//! stable cause string and parks the hart via
//! [`crate::kernel_arch::halt_current_hart`] (`AGENTS.md` §2.9, §2 —
//! fail closed, never silently reset).

use alloc::sync::Arc;

use rustos_kernel_core::{kernel_main, BootInfo, DispatchCallbackSlot};
use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
use rustos_kernel_sched_api::{CpuId, SchedulerConfig};
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::fdt::Fdt;
use crate::kernel_arch::{halt_current_hart, RiscvArch};

/// Logical CPU id of the boot hart for the single-hart slice.
const BOOT_CPU: CpuId = 0;

/// Audit event id emitted on a boot-init failure. Shares the
/// `4000..5000` `kernel/core` range and the top-of-range slot the
/// x86_64 pipeline uses for the same "init failed before `kernel_main`"
/// signal, so external audit consumers decode one stable id across
/// arches (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_INIT_FAILED: EventId = EventId(4099);

/// Bin-crate-independent [`DispatchCallbackSlot`] handed to
/// [`BootInfo`]. Set-once via its internal `OnceCell`; the
/// `Phase::Syscall` step publishes the production dispatch hook into it
/// (the riscv64 slice does not enable a syscall trampoline, so nothing
/// reads it before `BootCompleted` — `AGENTS.md` §2.1).
static DISPATCH_SLOT: DispatchCallbackSlot = DispatchCallbackSlot::new();

extern "C" {
    /// One byte past the end of the kernel image (including the boot
    /// heap), defined by `link/riscv64-virt.ld`. The usable
    /// physical-memory region starts here.
    static __kernel_end: u8;
}

/// Address of the linker-provided `__kernel_end` symbol.
fn kernel_end_addr() -> u64 {
    // `addr_of!` reads the symbol's address without forming a reference
    // to the (zero-sized, never-dereferenced) marker.
    core::ptr::addr_of!(__kernel_end) as u64
}

/// Failure modes of [`boot`].
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

/// Boot the kernel on the boot hart and forward to
/// [`rustos_kernel_core::kernel_main`].
///
/// `log_sink` / `audit_sink` are the `&'static` sinks installed in the
/// [`BootInfo`]: the QEMU integration test substitutes an audit sink
/// that flips the `SiFive` Test device on `AuditEvent::BootCompleted`.
///
/// Returns the bottom type. On failure it logs one
/// `KERNEL_BOOT_INIT_FAILED` record and parks the hart forever.
///
/// # SAFETY-INVARIANT
///
/// `dtb` must be the verbatim `a1` value OpenSBI handed the boot hart —
/// a pointer to a valid flattened device tree readable for the life of
/// the kernel. `boot.s` forwards it unchanged.
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

fn try_boot(
    hartid: u64,
    dtb: u64,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
) -> Result<BootInfo<'static, RiscvArch>, BootError> {
    if hartid != u64::from(BOOT_CPU) {
        return Err(BootError::UnexpectedHart);
    }

    // 1. Parse the device tree for RAM extent and the timer frequency.
    //
    // SAFETY: `dtb` is the verbatim `a1` pointer from OpenSBI (see the
    // `boot` SAFETY-INVARIANT); it addresses a valid flattened device
    // tree that lives for the life of the guest.
    let fdt = unsafe { Fdt::from_ptr(dtb as *const u8) }.map_err(|_| BootError::Fdt)?;
    let (ram_base, ram_size) = fdt.first_memory_region().ok_or(BootError::NoMemoryMap)?;
    let timebase_hz = fdt.timebase_frequency().ok_or(BootError::NoTimebase)?;
    let ram_end = ram_base
        .checked_add(ram_size)
        .ok_or(BootError::NoMemoryMap)?;

    // 2. Build the physical-memory map: reserve everything from RAM
    //    base through the end of the kernel image + boot heap, then mark
    //    the remainder usable.
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

    // Publish the firmware map and the device-tree pointer for a
    // driver-bring-up observer (the virtio-MMIO QEMU verticals) before
    // the map is moved into the `kernel_core` hand-off. Both slots are
    // set-once (`AGENTS.md` §2.1); see `crate::publish`.
    crate::publish::publish_memory_map(&memory_map);
    crate::publish::publish_dtb(dtb);

    // 3. Assemble the hand-off and validate it before handing control
    //    to the architecture-neutral kernel core.
    let arch = Arc::new(RiscvArch::new(BOOT_CPU, timebase_hz));
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
