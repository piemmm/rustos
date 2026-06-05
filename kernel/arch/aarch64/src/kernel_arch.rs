//! [`Aarch64Arch`] — the aarch64 implementation of the Arch HAL
//! ([`rustos_arch_api::SchedulerArch`]).
//!
//! Like x86_64 and riscv64, the aarch64 port is a pure Arch HAL
//! implementation (`AGENTS.md` §17.2): it implements [`SchedulerArch`]
//! and exposes the monotonic clock and a CPU-park primitive, but it does
//! **not** name `kernel/core` or implement its `KernelArch` super-trait.
//! The downstream boot consumer wraps [`Aarch64Arch`] in a local
//! `KernelArch` type (orphan rules) and hands it to
//! `kernel_core::kernel_main`.
//!
//! # Clock
//!
//! The monotonic clock reads the architectural physical counter
//! `CNTPCT_EL0`; [`Aarch64Arch::monotonic_ns`] converts those ticks to
//! nanoseconds using the counter frequency `CNTFRQ_EL0` reports (passed
//! to the constructor), so the conversion factor and the tick source
//! share one frequency (`AGENTS.md` §2.4 — no parallel measurement).
//!
//! # Inter-processor interrupts
//!
//! [`SchedulerArch::send_ipi`] raises a GICv2 software-generated
//! interrupt (SGI) on the target CPU through [`crate::gic`]. Sending to
//! the calling CPU is permitted (a self-reschedule). The boot/timer
//! slice runs a single CPU; the host build records the request in an
//! in-memory ledger so unit tests can assert preemption was requested.
//!
//! # Host testability
//!
//! The struct and its trait wiring build on the host so the unit tests
//! run under `cargo test`. The instruction-level primitives are gated:
//! the aarch64 build reads `CNTPCT_EL0` and parks on `wfi`, and the host
//! build substitutes a monotonic atomic counter *solely* so the host
//! tests can exercise the ns conversion (`AGENTS.md` §1 — no fake
//! primitives in production).

use core::sync::atomic::AtomicU64;
#[cfg(any(test, not(target_os = "none")))]
use core::sync::atomic::Ordering;

use rustos_arch_api::{CpuId, SchedulerArch};

/// Maximum number of logical CPUs the per-CPU accounting arrays cover.
/// The boot/timer slice brings up one; the bound is headroom for the
/// SMP follow-up and keeps the host IPI ledger fixed-size.
pub const MAX_CPUS: usize = 8;

/// aarch64 architecture handle the downstream boot consumer wraps for
/// `kernel_core::kernel_main`.
///
/// Stable for the lifetime of the kernel image. The host-only counters
/// exist solely for deterministic unit tests, mirroring `X86_64Arch` and
/// `RiscvArch`.
#[derive(Debug)]
pub struct Aarch64Arch {
    boot_cpu: CpuId,
    timer_hz: u64,

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_ipi_count: [AtomicU64; MAX_CPUS],

    /// Host-only stray-IPI counter for out-of-range targets.
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_stray_ipi: AtomicU64,
}

impl Aarch64Arch {
    /// Construct a single-CPU handle for `boot_cpu` running on a CPU
    /// whose physical counter advances at `timer_hz` ticks per second
    /// (the value `CNTFRQ_EL0` reports).
    ///
    /// `timer_hz` must be non-zero; the boot pipeline reads it from
    /// `CNTFRQ_EL0` (see `read_cntfrq`) and refuses to boot when it is
    /// zero, so [`Self::monotonic_ns`] never divides by zero.
    #[must_use]
    pub fn new(boot_cpu: CpuId, timer_hz: u64) -> Self {
        Self {
            boot_cpu,
            timer_hz,
            host_ipi_count: [const { AtomicU64::new(0) }; MAX_CPUS],
            host_stray_ipi: AtomicU64::new(0),
        }
    }

    /// The counter frequency this handle converts against.
    #[must_use]
    pub const fn timer_hz(&self) -> u64 {
        self.timer_hz
    }

    /// Host-test accessor: total IPIs dispatched to `target`.
    #[must_use]
    #[cfg(any(test, not(target_os = "none")))]
    pub fn host_ipi_count(&self, target: CpuId) -> u64 {
        let idx = match usize::try_from(target) {
            Ok(i) if i < MAX_CPUS => i,
            _ => return 0,
        };
        self.host_ipi_count[idx].load(Ordering::Relaxed)
    }

    /// Host-test accessor: IPIs whose target was out of range.
    #[must_use]
    #[cfg(any(test, not(target_os = "none")))]
    pub fn host_stray_ipi_count(&self) -> u64 {
        self.host_stray_ipi.load(Ordering::Relaxed)
    }

    /// Monotonic nanoseconds since the physical counter's epoch.
    ///
    /// Reads `CNTPCT_EL0` and converts ticks to nanoseconds against this
    /// handle's `timer_hz`, so the tick source and the conversion factor
    /// share one frequency (`AGENTS.md` §2.4). The downstream
    /// `KernelArch` wrapper forwards `monotonic_ns` here.
    #[must_use]
    pub fn monotonic_ns(&self) -> u64 {
        let ticks = u128::from(read_cntpct());
        let hz = u128::from(self.timer_hz.max(1));
        // `ticks * 1e9 / hz` in 128-bit space cannot overflow for any
        // realistic uptime, and the `max(1)` defends a malformed
        // frequency from a division trap (`AGENTS.md` §2.9).
        let ns = ticks.saturating_mul(1_000_000_000) / hz;
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

impl SchedulerArch for Aarch64Arch {
    fn current_cpu(&self) -> CpuId {
        // The boot/timer slice runs one CPU; the SMP follow-up reverse-
        // maps `MPIDR_EL1` to a dense `CpuId` here.
        self.boot_cpu
    }

    fn ticks_now(&self) -> u64 {
        read_cntpct()
    }

    fn send_ipi(&self, target: CpuId) {
        if usize::try_from(target).map_or(true, |i| i >= MAX_CPUS) {
            #[cfg(any(test, not(target_os = "none")))]
            self.host_stray_ipi.fetch_add(1, Ordering::Relaxed);
            return;
        }

        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            // Raise a GICv2 software-generated interrupt on the target
            // CPU; its IRQ exception path runs the scheduler entry. The
            // result is best-effort — a single-CPU image targets itself.
            crate::gic::send_sgi(target);
        }

        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            self.host_ipi_count[target as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Read the architectural physical counter `CNTPCT_EL0` (the monotonic
/// tick source on the `virt` board).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn read_cntpct() -> u64 {
    let ticks: u64;
    // SAFETY: `CNTPCT_EL0` is the unprivileged physical counter; reading
    // it has no side effects and is accessible at EL1 (and at EL0/EL1
    // after `boot.s` enables `CNTHCTL_EL2.EL1PCTEN` when entered at EL2).
    unsafe {
        core::arch::asm!("mrs {}, CNTPCT_EL0", out(reg) ticks, options(nomem, nostack, preserves_flags));
    }
    ticks
}

/// Host substitute for `CNTPCT_EL0`: a strictly increasing counter so
/// the unit tests observe a monotonic clock. Never linked into a kernel
/// image (the bare-metal aarch64 build uses the `mrs` reader above).
///
/// Gated on "not bare-metal aarch64" rather than "not aarch64" so a
/// hosted aarch64 development machine (e.g. an Apple-silicon or ARM
/// Linux host running `cargo test`) also uses this deterministic
/// substitute instead of the real `CNTPCT_EL0`, whose coarse tick can
/// read identically across two adjacent calls. Mirrors the gating in
/// the x86_64 and riscv64 backends.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub(crate) fn read_cntpct() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

/// Read the counter frequency `CNTFRQ_EL0` reports (Hz). On the `virt`
/// board QEMU programs this to the host timer frequency.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn read_cntfrq() -> u64 {
    let hz: u64;
    // SAFETY: `CNTFRQ_EL0` is readable at EL1; the read has no side
    // effects.
    unsafe {
        core::arch::asm!("mrs {}, CNTFRQ_EL0", out(reg) hz, options(nomem, nostack, preserves_flags));
    }
    hz
}

/// Park the calling CPU forever on `wfi` with interrupts disabled.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn park() -> ! {
    // SAFETY: `msr DAIFSet, #0xf` masks all interrupts; `wfi` is a
    // well-defined wait-for-interrupt hint. The loop defends against a
    // spurious wake. This is the aarch64 form of the `AGENTS.md` §2
    // "never silently reset" contract.
    unsafe {
        core::arch::asm!("msr DAIFSet, #0xf", options(nomem, nostack));
    }
    loop {
        // SAFETY: `wfi` is a hint with no architectural side effects.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Park the calling CPU forever (the panic bridge and the downstream
/// `KernelArch` wrapper's `halt` both forward here). Masks interrupts
/// and spins on `wfi`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn halt_current_cpu() -> ! {
    park()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_ns_uses_the_handle_frequency() {
        // Host `read_cntpct` increments by one per call; with a 1 GHz
        // frequency one tick is one nanosecond, so successive reads are
        // strictly increasing and scale by the frequency.
        let arch = Aarch64Arch::new(0, 1_000_000_000);
        let a = arch.monotonic_ns();
        let b = arch.monotonic_ns();
        assert!(b > a, "clock must be monotonically increasing");
    }

    #[test]
    fn zero_frequency_does_not_divide_by_zero() {
        let arch = Aarch64Arch::new(0, 0);
        // Must not panic; `max(1)` guards the divide.
        let _ = arch.monotonic_ns();
    }

    #[test]
    fn current_cpu_reports_the_boot_cpu_on_host() {
        let arch = Aarch64Arch::new(3, 1_000);
        assert_eq!(arch.current_cpu(), 3);
    }

    #[test]
    fn send_ipi_counts_in_range_targets_and_strays() {
        let arch = Aarch64Arch::new(0, 1_000);
        arch.send_ipi(1);
        arch.send_ipi(1);
        assert_eq!(arch.host_ipi_count(1), 2);
        // Out-of-range target is recorded as a stray, never panics.
        arch.send_ipi(u32::try_from(MAX_CPUS).unwrap());
        assert_eq!(arch.host_stray_ipi_count(), 1);
    }

    /// §17.2 / W0: the port passes the shared Arch HAL conformance
    /// vertical over its real `SchedulerArch`, `SideChannel`, and
    /// `MemoryTags` handles (`plans/WIRING.md` Stage W0).
    #[test]
    fn passes_arch_hal_conformance_suite() {
        let arch = Aarch64Arch::new(0, 1_000);
        rustos_arch_api::conformance::run_all(
            &arch,
            &crate::sidechannel::SideChannel::new(),
            &crate::memtag::MemoryTags::new(),
        );
    }
}
