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

use rustos_arch_x86_64::kernel_arch::{halt as arch_halt, X86_64Arch};
use rustos_kernel_core::KernelArch;
use rustos_kernel_sched::{CpuId, SchedulerArch};

/// Local newtype wrapping [`X86_64Arch`] so the bin crate can implement
/// the foreign [`KernelArch`] trait on the foreign concrete type.
///
/// The wrapper is `#[repr(transparent)]` so the layout matches the
/// underlying type — useful for inspection but not relied on by any
/// public API. It exists solely to satisfy Rust's orphan rules; every
/// method delegates verbatim.
#[repr(transparent)]
#[derive(Debug)]
pub struct BinArch(pub X86_64Arch);

impl BinArch {
    /// Construct a [`BinArch`] from an already-validated [`X86_64Arch`].
    #[must_use]
    pub const fn new(arch: X86_64Arch) -> Self {
        Self(arch)
    }

    /// Borrow the wrapped [`X86_64Arch`].
    #[must_use]
    pub const fn arch(&self) -> &X86_64Arch {
        &self.0
    }
}

impl SchedulerArch for BinArch {
    fn current_cpu(&self) -> CpuId {
        self.0.current_cpu()
    }

    fn ticks_now(&self) -> u64 {
        self.0.ticks_now()
    }

    fn send_ipi(&self, target: CpuId) {
        self.0.send_ipi(target);
    }
}

impl KernelArch for BinArch {
    fn halt(&self) -> ! {
        arch_halt()
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

    #[test]
    fn current_cpu_delegates_to_inner() {
        let arch = BinArch::new(arch_with_boot_cpu(2, 0xA2));
        assert_eq!(arch.current_cpu(), 2);
    }

    #[test]
    fn ticks_now_is_monotonic_on_host() {
        let arch = BinArch::new(arch_with_boot_cpu(0, 0xA0));
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
        let bin = BinArch::new(arch);
        bin.send_ipi(1);
        bin.send_ipi(1);
        bin.send_ipi(0);
        // The inner host-only counters were ticked through the wrapper.
        assert_eq!(bin.arch().host_ipi_count(1), 2);
        assert_eq!(bin.arch().host_ipi_count(0), 1);
        assert_eq!(bin.arch().host_stray_ipi_count(), 0);
    }
}
