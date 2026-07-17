//! riscv64 per-CPU storage ("per-CPU storage").
//!
//! Implements the Arch HAL [`PerCpu`](tairix_arch_api::PerCpu) surface
//! for riscv64 over the **`tp`** (thread-pointer) register. `tp` is the
//! conventional RISC-V per-hart anchor: `boot.s` (boot hart) and `smp.s`
//! (secondary harts) seed it with the SBI-handed hart id before entering
//! Rust, and [`crate::smp::current_hartid`] already reads it as the
//! per-CPU identity. This slice generalises that read/write into the one
//! HAL trait the architecture-neutral kernel reaches through, so the
//! `tp` access lives in exactly one place.
//!
//! The stored word is opaque to this surface: the kernel decides whether
//! `tp` holds the hart id or the address of a per-hart control block (see
//! the [`PerCpu`](tairix_arch_api::PerCpu) trait docs). On the host build
//! there is no `tp`, so the handle backs the word with an in-handle cell
//! solely for the unit tests; it is never linked into a kernel image.

use core::sync::atomic::AtomicUsize;

use tairix_arch_api::PerCpu;

/// riscv64 implementation of the Arch HAL per-CPU storage surface.
///
/// On the bare-metal target the read/write hit the `tp` register, so the
/// in-handle cell is unused there; on the host build it backs the word so
/// the round-trip and isolation conformance verticals run under
/// `cargo test`.
#[derive(Debug, Default)]
pub struct PerCpuStorage {
    /// Host-only backing for the per-CPU word. On the bare-metal target
    /// `tp` is the source of truth and this field is never read; kept so
    /// the host and bare-metal builds share one struct shape.
    #[cfg_attr(all(target_arch = "riscv64", target_os = "none"), allow(dead_code))]
    host_base: AtomicUsize,
}

impl PerCpuStorage {
    /// Construct the riscv64 per-CPU storage handle.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            host_base: AtomicUsize::new(0),
        }
    }
}

impl PerCpu for PerCpuStorage {
    fn read_self_base(&self) -> usize {
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            let tp: usize;
            // SAFETY: reading `tp` has no side effects and cannot fault;
            // it returns whatever per-hart word the kernel installed
            // (the boot/secondary trampolines seed it with the hart id).
            unsafe {
                core::arch::asm!("mv {}, tp", out(reg) tp, options(nomem, nostack, preserves_flags));
            }
            tp
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            self.host_base.load(core::sync::atomic::Ordering::Relaxed)
        }
    }

    unsafe fn write_self_base(&self, base: usize) {
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // SAFETY: writing `tp` sets the calling hart's per-CPU word.
            // The trait's safety contract requires the caller to run this
            // on the hart whose word is being set and to pass the value
            // the kernel's per-hart resolution expects; `mv tp, _` has no
            // other effect and cannot fault.
            unsafe {
                core::arch::asm!("mv tp, {}", in(reg) base, options(nomem, nostack, preserves_flags));
            }
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            self.host_base
                .store(base, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::percpu::conformance;

    #[test]
    fn passes_per_cpu_conformance() {
        conformance::run_all(&PerCpuStorage::new());
        let dynamic: &dyn PerCpu = &PerCpuStorage::new();
        conformance::run_all(dynamic);
    }

    #[test]
    fn per_cpu_word_is_isolated_across_harts() {
        conformance::run_isolation(&PerCpuStorage::new(), &PerCpuStorage::new());
    }
}
