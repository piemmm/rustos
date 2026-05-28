//! In-crate [`KernelArch`] wrapper around
//! [`rustos_arch_x86_64::kernel_arch::X86_64Arch`].
//!
//! # Why a wrapper
//!
//! `rustos_kernel_core::KernelArch` is a foreign trait and
//! `rustos_arch_x86_64::kernel_arch::X86_64Arch` is a foreign type, so
//! Rust's coherence rules forbid implementing the trait for the type
//! directly. The wrapper [`BinArch`] is the smallest possible local
//! type that owns an `X86_64Arch`, implements the
//! [`rustos_kernel_sched::SchedulerArch`] super-trait by delegation,
//! and implements [`rustos_kernel_core::KernelArch::halt`] by
//! forwarding to the free function
//! [`rustos_arch_x86_64::kernel_arch::halt`].
//!
//! The arch crate's
//! `kernel/arch/x86_64/Cargo.toml` comment explicitly documents this
//! split: pulling `rustos-kernel-core` into the arch crate would
//! transitively force a `#[global_allocator]` into the two pre-existing
//! freestanding Stage-2 QEMU test bins.

use rustos_arch_x86_64::apic_timer::{Calibration, Rdtsc, TscReader};
use rustos_arch_x86_64::kernel_arch::{halt as arch_halt, X86_64Arch};
use rustos_kernel_core::KernelArch;
use rustos_kernel_sched::{CpuId, SchedulerArch};

/// Local wrapper around [`X86_64Arch`] so the bin crate can implement
/// the foreign [`KernelArch`] trait on the foreign concrete type, and
/// carries the boot-time `Calibration` consumed by
/// [`KernelArch::monotonic_ns`] (Stage 2.7 follow-up (f3)).
///
/// The wrapper exists solely to satisfy Rust's orphan rules; the
/// `SchedulerArch` super-trait still delegates verbatim to
/// `X86_64Arch`. `KernelArch::monotonic_ns` reads RDTSC through the
/// arch crate's [`Rdtsc`] reader and converts the tick count into
/// nanoseconds via [`Calibration::tsc_ticks_to_ns`] — the same TSC
/// frequency the boot path measured against the PIT
/// (`AGENTS.md` §2.4 — no parallel measurement, no interface creep).
#[derive(Debug)]
pub struct BinArch {
    arch: X86_64Arch,
    calibration: Calibration,
}

impl BinArch {
    /// Construct a [`BinArch`] from an already-validated [`X86_64Arch`]
    /// and the boot-time `Calibration`.
    ///
    /// `calibration` is the value returned by
    /// `apic_timer::calibrate` in the bin crate's `boot::try_boot`; it
    /// carries the TSC frequency [`KernelArch::monotonic_ns`] needs.
    #[must_use]
    pub const fn new(arch: X86_64Arch, calibration: Calibration) -> Self {
        Self { arch, calibration }
    }

    /// Borrow the wrapped [`X86_64Arch`].
    #[must_use]
    pub const fn arch(&self) -> &X86_64Arch {
        &self.arch
    }

    /// Boot-time calibration captured during `kernel_main`.
    #[must_use]
    pub const fn calibration(&self) -> Calibration {
        self.calibration
    }
}

impl SchedulerArch for BinArch {
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

impl KernelArch for BinArch {
    fn halt(&self) -> ! {
        arch_halt()
    }

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        // Read RDTSC through the same arch-crate reader the calibration
        // path used so the tick source and the conversion factor share
        // the same time base. The `cpu` argument is currently unused on
        // x86_64 — the production target assumes the invariant-TSC
        // contract QEMU and modern Intel/AMD parts provide. A future
        // arch port that needs per-CPU offset compensation would feed
        // it into a `cpu_to_tsc_offset` table read here; nothing in the
        // current SMP bring-up populates such a table, so reading it
        // would be a stub (`AGENTS.md` §15.1) and is omitted.
        let mut rdtsc = Rdtsc;
        let ticks = rdtsc.read();
        self.calibration.tsc_ticks_to_ns(ticks)
    }
}

// SAFETY-INVARIANT: `BinArch::halt` returns the bottom type. The
// compile-time function-pointer coercion below fails to type-check if
// the impl ever loses `-> !` (e.g. a `Result<!, !>` return or a
// `unreachable!()`-followed return type). This is the pattern called
// out by the arch crate's `_HALT_RETURNS_NEVER` const assertion;
// repeating it here pins the impl on this side of the wrapper too —
// `AGENTS.md` §2.10 (encode the invariant in the type system).
const _BIN_ARCH_HALT_RETURNS_NEVER: fn(&BinArch) -> ! = <BinArch as KernelArch>::halt;

// SAFETY-INVARIANT: `BinArch` implements `SchedulerArch`. A regression
// that broke the super-trait impl (e.g. a missing `current_cpu`)
// would surface at this `const _` coercion before the kernel binary
// linked. `AGENTS.md` §2.4 — no interface creep — applies in both
// directions: shrinking the surface is a defect too.
const _BIN_ARCH_IS_SCHED_ARCH: fn(&BinArch) -> u32 = <BinArch as SchedulerArch>::current_cpu;

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_x86_64::percpu::MAX_CPUS;

    fn arch_with_boot_cpu(boot_cpu: u32, lapic: u8) -> X86_64Arch {
        let mut map = [None; MAX_CPUS];
        map[boot_cpu as usize] = Some(lapic);
        X86_64Arch::new(boot_cpu, lapic, map).expect("valid X86_64Arch")
    }

    /// Synthesise a `Calibration` for tests. The exact values are
    /// irrelevant to the delegating super-trait methods; `monotonic_ns`
    /// uses a 1 GHz TSC rate so a one-tick reading converts to 1 ns.
    fn test_calibration() -> Calibration {
        Calibration {
            ticks_per_second: 100_000,
            initial_count: 100,
            period_micros: 1_000,
            tsc_per_second: 1_000_000_000,
        }
    }

    #[test]
    fn current_cpu_delegates_to_inner() {
        let arch = BinArch::new(arch_with_boot_cpu(2, 0xA2), test_calibration());
        assert_eq!(arch.current_cpu(), 2);
    }

    #[test]
    fn ticks_now_is_monotonic_on_host() {
        let arch = BinArch::new(arch_with_boot_cpu(0, 0xA0), test_calibration());
        let a = arch.ticks_now();
        let b = arch.ticks_now();
        let c = arch.ticks_now();
        assert!(b > a);
        assert!(c > b);
    }

    #[test]
    fn send_ipi_delegates_to_inner_host_counter() {
        let arch = X86_64Arch::new(0, 0xA0, {
            let mut m = [None; MAX_CPUS];
            m[0] = Some(0xA0);
            m[1] = Some(0xA1);
            m
        })
        .unwrap();
        let bin = BinArch::new(arch, test_calibration());
        bin.send_ipi(1);
        bin.send_ipi(1);
        bin.send_ipi(0);
        // The inner host-only counters were ticked through the wrapper.
        assert_eq!(bin.arch().host_ipi_count(1), 2);
        assert_eq!(bin.arch().host_ipi_count(0), 1);
        assert_eq!(bin.arch().host_stray_ipi_count(), 0);
    }

    #[test]
    fn monotonic_ns_is_non_decreasing_on_host() {
        // On the host build path, `BinArch::monotonic_ns` reads RDTSC
        // (via `Rdtsc`) and converts through `Calibration::tsc_ticks_to_ns`.
        // RDTSC is monotonically non-decreasing on every x86_64 CPU
        // RustOS is built on, including the CI host, so two
        // consecutive reads must satisfy `a <= b` (`AGENTS.md` §7 —
        // no flaky tests; we assert a non-strict ordering because
        // the conversion can compress two close ticks onto the same
        // ns value).
        let arch = BinArch::new(arch_with_boot_cpu(0, 0xA0), test_calibration());
        let a = arch.monotonic_ns(0);
        let b = arch.monotonic_ns(0);
        let c = arch.monotonic_ns(0);
        assert!(b >= a, "expected b >= a, got a={a} b={b}");
        assert!(c >= b, "expected c >= b, got b={b} c={c}");
    }

    #[test]
    fn calibration_is_round_tripped_through_constructor() {
        let cal = test_calibration();
        let arch = BinArch::new(arch_with_boot_cpu(0, 0xA0), cal);
        assert_eq!(arch.calibration(), cal);
    }
}
