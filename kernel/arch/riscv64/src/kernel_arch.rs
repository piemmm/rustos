//! [`RiscvArch`] — the riscv64 implementation of the Arch HAL
//! ([`rustos_arch_api::SchedulerArch`]).
//!
//! Like x86_64, the riscv64 port is a pure Arch HAL implementation
//! (`AGENTS.md` §17.2): it implements [`SchedulerArch`] and exposes the
//! monotonic clock and the hart-park primitive, but it does **not**
//! name `kernel/core` or implement its `KernelArch` super-trait. The
//! downstream boot consumer wraps [`RiscvArch`] in a local `KernelArch`
//! type (orphan rules), constructs it from the device-tree timebase,
//! and hands it to `kernel_core::kernel_main`.
//!
//! # Clock
//!
//! The monotonic clock reads the architectural `time` CSR via the
//! `rdtime` instruction (always available to S-mode on the QEMU `virt`
//! board). [`RiscvArch::monotonic_ns`] converts those ticks to
//! nanoseconds using the `timebase-frequency` the boot pipeline read
//! from the device tree (`fdt`), so the conversion and the tick source
//! share one frequency (`AGENTS.md` §2.4 — no parallel measurement).
//!
//! # Host testability
//!
//! The struct and its trait wiring build on the host so the unit tests
//! in `kernel_arch_tests.rs` run under `cargo test` without a riscv64
//! target. The instruction-level primitives are gated: the riscv64
//! build reads the `time` CSR via `rdtime` and parks on `wfi`, and the
//! host build substitutes a monotonic atomic counter *solely* so the
//! host tests can exercise the ns conversion. The hart park
//! (`halt_current_hart`) is freestanding-only; the production path is
//! the `target_arch = "riscv64"` cfg and the host shims are never
//! linked into a kernel image (`AGENTS.md` §1 — no fake primitives in
//! production).

use rustos_arch_api::{CpuId, SchedulerArch};

/// riscv64 architecture handle the downstream boot consumer wraps for
/// `kernel_core::kernel_main`.
#[derive(Debug)]
pub struct RiscvArch {
    boot_cpu: CpuId,
    timebase_hz: u64,
}

impl RiscvArch {
    /// Construct a handle for `boot_cpu` running on a hart whose `time`
    /// CSR advances at `timebase_hz` ticks per second.
    ///
    /// `timebase_hz` must be non-zero; the boot pipeline reads it from
    /// the device tree's `/cpus` `timebase-frequency` and refuses to
    /// boot when it is absent, so [`Self::monotonic_ns`] never
    /// divides by zero.
    #[must_use]
    pub const fn new(boot_cpu: CpuId, timebase_hz: u64) -> Self {
        Self {
            boot_cpu,
            timebase_hz,
        }
    }

    /// The `time` CSR frequency this handle converts against.
    #[must_use]
    pub const fn timebase_hz(&self) -> u64 {
        self.timebase_hz
    }

    /// Monotonic nanoseconds since the `time` CSR's epoch.
    ///
    /// Reads the architectural `time` CSR and converts ticks to
    /// nanoseconds against this handle's `timebase_hz`, so the tick
    /// source and the conversion factor share one frequency
    /// (`AGENTS.md` §2.4 — no parallel measurement). The downstream
    /// `KernelArch` wrapper forwards `monotonic_ns` here.
    #[must_use]
    pub fn monotonic_ns(&self) -> u64 {
        let ticks = u128::from(read_time());
        let hz = u128::from(self.timebase_hz.max(1));
        // `ticks * 1e9 / hz` in 128-bit space cannot overflow for any
        // realistic uptime, and the `max(1)` defends a malformed
        // frequency from a division trap (`AGENTS.md` §2.9).
        let ns = ticks.saturating_mul(1_000_000_000) / hz;
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

impl SchedulerArch for RiscvArch {
    fn current_cpu(&self) -> CpuId {
        self.boot_cpu
    }

    fn ticks_now(&self) -> u64 {
        read_time()
    }

    fn send_ipi(&self, _target: CpuId) {
        // The boot-to-`BootCompleted` slice is single-hart, so the only
        // valid `target` is the calling CPU, which the trait documents
        // as a permitted no-op (a self-reschedule). Multi-hart IPI
        // delivery via the SBI `IPI` extension lands with riscv64 SMP
        // bring-up; emitting one here would be a stub for an
        // unreachable path (`AGENTS.md` §15.1).
    }
}

/// Read the architectural `time` CSR (nanosecond-resolution monotonic
/// tick source on the `virt` board).
#[cfg(target_arch = "riscv64")]
pub(crate) fn read_time() -> u64 {
    let ticks: u64;
    // SAFETY: `rdtime` reads the unprivileged `time` CSR; it has no
    // side effects and is available to S-mode on every riscv64 platform
    // RustOS targets (QEMU `virt` delegates it).
    unsafe {
        core::arch::asm!("rdtime {}", out(reg) ticks, options(nomem, nostack, preserves_flags));
    }
    ticks
}

/// Host substitute for the `time` CSR: a strictly increasing counter so
/// the unit tests below observe a monotonic clock. Never linked into a
/// kernel image (the riscv64 build uses [`read_time`] above).
#[cfg(not(target_arch = "riscv64"))]
pub(crate) fn read_time() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

/// Park the calling hart forever on `wfi` with interrupts disabled.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn park() -> ! {
    // SAFETY: clearing `sstatus.SIE` masks S-mode interrupts; `wfi` is
    // a well-defined wait-for-interrupt hint. The loop defends against
    // a spurious wake. This is the riscv64 form of the `AGENTS.md` §2
    // "never silently reset" contract.
    unsafe {
        core::arch::asm!("csrci sstatus, 2", options(nomem, nostack));
    }
    loop {
        // SAFETY: `wfi` is a hint with no architectural side effects.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Park the calling hart forever (the panic bridge and the downstream
/// `KernelArch` wrapper's `halt` both forward here). Masks S-mode
/// interrupts and spins on `wfi`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn halt_current_hart() -> ! {
    park()
}

#[cfg(test)]
#[path = "kernel_arch_tests.rs"]
mod tests;
