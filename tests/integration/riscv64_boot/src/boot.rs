//! Thin downstream boot wrapper for the riscv64 QEMU verticals.
//!
//! The riscv64 boot pipeline itself now lives in the production
//! `rustos-kernel` crate (`rustos_kernel::riscv64::boot`), so there is
//! exactly one riscv64 boot orchestration in the workspace — the same one the production `rustos-kernel` binary runs. This
//! module re-exports the production [`RiscvBinArch`] / [`BootError`] /
//! [`try_boot`] and adds the single test-only affordance the verticals
//! need on top of it: publishing the firmware [`BootMemoryMap`] + DTB
//! pointer to the device-bring-up observers ([`crate::publish`]) before
//! delegating to the production [`boot`](rustos_kernel::riscv64::boot::boot),
//! which moves the map into the `kernel_core` hand-off.
//!
//! Keeping the publish affordance here (not in the production pipeline)
//! is the discipline: the production kernel never carries a
//! test-observer side channel. The boot-completed QEMU bins swap only
//! the audit sink, exactly as the x86_64 / aarch64 verticals do.

use rustos_log::{Level, Sink};

pub use rustos_kernel::riscv64::boot::{try_boot, BootError, RiscvBinArch};

/// Boot the riscv64 `virt`-board vertical and forward to the production
/// kernel pipeline.
///
/// Publishes the firmware [`BootMemoryMap`](rustos_kernel_mem::BootMemoryMap)
/// and the DTB pointer for the virtio-MMIO / framebuffer device-bring-up
/// observers ([`crate::publish`]) — which need them after the pipeline
/// has moved the map into the `kernel_core` hand-off — then delegates to
/// [`rustos_kernel::riscv64::boot::boot`].
///
/// `log_sink` / `audit_sink` are the `&'static` sinks the consuming
/// vertical installs; its audit sink flips the `SiFive` Test device on
/// `AuditEvent::BootCompleted`. `log_level` is the initial global log
/// filter `kernel_main` installs (`BootInfo::log_level`); a vertical
/// that must observe the `Debug`-level allow records (e.g.
/// `SyscallInvoked`, `EventId(5000)`) passes `Level::Debug`, the rest
/// pass `Level::Info`. Returns the bottom type.
///
/// # SAFETY-INVARIANT
///
/// `dtb` must be the verbatim `a1` device-tree pointer OpenSBI handed
/// the boot hart, forwarded unchanged by the arch port's `boot.s`.
pub fn boot(
    hartid: u64,
    dtb: u64,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
    log_level: Level,
) -> ! {
    // Publish before delegating, while the map can still be observed:
    // the production pipeline moves it into the hand-off. A build
    // failure here is non-fatal — the production boot recomputes the
    // same map and fails closed on the identical cause; the observers then simply see no published map.
    if let Ok(map) = rustos_kernel::riscv64::boot::build_boot_memory_map(dtb) {
        crate::publish::publish_memory_map(&map);
    }
    crate::publish::publish_dtb(dtb);
    rustos_kernel::riscv64::boot::boot(hartid, dtb, log_sink, audit_sink, log_level)
}
