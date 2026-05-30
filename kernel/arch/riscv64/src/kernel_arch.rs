//! [`RiscvArch`] — the riscv64 implementation of
//! [`rustos_kernel_core::KernelArch`].
//!
//! Unlike x86_64 (whose `KernelArch` wrapper lives in `rustos-kernel`
//! to keep `kernel/core` out of the arch crate), the riscv64 port owns
//! its `KernelArch` impl directly — see this crate's `Cargo.toml` for
//! the rationale. `RiscvArch` is deliberately tiny: the boot pipeline
//! (`boot`) constructs one, stores it in an `Arc`, and hands it to
//! `kernel_core::kernel_main` inside the [`rustos_kernel_core::BootInfo`].
//!
//! # Clock
//!
//! The monotonic clock reads the architectural `time` CSR via the
//! `rdtime` instruction (always available to S-mode on the QEMU `virt`
//! board). [`KernelArch::monotonic_ns`] converts those ticks to
//! nanoseconds using the `timebase-frequency` the boot pipeline read
//! from the device tree (`fdt`), so the conversion and the tick source
//! share one frequency (`AGENTS.md` §2.4 — no parallel measurement).
//!
//! # Host testability
//!
//! The struct and its trait wiring build on the host so the unit tests
//! below run under `cargo test` without a riscv64 target. The two
//! instruction-level primitives are gated: the riscv64 build uses
//! `rdtime` / `wfi`, and the host build substitutes a monotonic atomic
//! counter and a spin park *solely* so the host tests can exercise the
//! delegation and the ns conversion. The production path is the
//! `target_arch = "riscv64"` cfg; the host shims are never linked into
//! a kernel image (`AGENTS.md` §1 — no fake primitives in production).

use rustos_kernel_core::KernelArch;
use rustos_kernel_sched_api::{CpuId, SchedulerArch};

/// riscv64 architecture handle handed to `kernel_core::kernel_main`.
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
    /// boot when it is absent, so [`KernelArch::monotonic_ns`] never
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

impl KernelArch for RiscvArch {
    fn halt(&self) -> ! {
        park()
    }

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        let ticks = u128::from(read_time());
        let hz = u128::from(self.timebase_hz.max(1));
        // `ticks * 1e9 / hz` in 128-bit space cannot overflow for any
        // realistic uptime, and the `max(1)` defends a malformed
        // frequency from a division trap (`AGENTS.md` §2.9).
        let ns = ticks.saturating_mul(1_000_000_000) / hz;
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

/// Read the architectural `time` CSR (nanosecond-resolution monotonic
/// tick source on the `virt` board).
#[cfg(target_arch = "riscv64")]
fn read_time() -> u64 {
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
fn read_time() -> u64 {
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

/// Park the calling hart forever, for callers outside the
/// [`KernelArch`] trait (the panic bridge). Forwards to the same `wfi`
/// park as [`KernelArch::halt`].
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn halt_current_hart() -> ! {
    park()
}

/// Host substitute for [`park`]; present only so [`KernelArch::halt`]
/// type-checks on the host. The host unit tests never call it.
#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
fn park() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

// SAFETY-INVARIANT: `RiscvArch::halt` returns the bottom type. The
// coercion fails to type-check if the impl ever loses `-> !`, pinning
// the contract at compile time (`AGENTS.md` §2.10).
const _RISCV_ARCH_HALT_RETURNS_NEVER: fn(&RiscvArch) -> ! = <RiscvArch as KernelArch>::halt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_cpu_returns_boot_cpu() {
        let arch = RiscvArch::new(0, 10_000_000);
        assert_eq!(arch.current_cpu(), 0);
    }

    #[test]
    fn ticks_now_is_monotonic_on_host() {
        let arch = RiscvArch::new(0, 10_000_000);
        let a = arch.ticks_now();
        let b = arch.ticks_now();
        assert!(b > a);
    }

    #[test]
    fn monotonic_ns_is_non_decreasing_on_host() {
        // The host clock counts ticks 1, 2, 3, …; at 1 GHz a tick is
        // one nanosecond, so the readings are strictly increasing.
        let arch = RiscvArch::new(0, 1_000_000_000);
        let a = arch.monotonic_ns(0);
        let b = arch.monotonic_ns(0);
        let c = arch.monotonic_ns(0);
        assert!(b >= a, "expected b >= a, got a={a} b={b}");
        assert!(c >= b, "expected c >= b, got b={b} c={c}");
    }

    #[test]
    fn timebase_is_round_tripped() {
        let arch = RiscvArch::new(3, 24_000_000);
        assert_eq!(arch.timebase_hz(), 24_000_000);
    }

    #[test]
    fn zero_timebase_does_not_divide_by_zero() {
        // A malformed (zero) frequency must not trap; `monotonic_ns`
        // clamps the divisor to 1 (`AGENTS.md` §2.9 — fail safe).
        let arch = RiscvArch::new(0, 0);
        let _ = arch.monotonic_ns(0);
    }
}
