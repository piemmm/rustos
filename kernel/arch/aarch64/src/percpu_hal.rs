//! aarch64 per-CPU storage (`AGENTS.md` §17.2 "per-CPU storage").
//!
//! Implements the Arch HAL [`PerCpu`](rustos_arch_api::PerCpu) surface
//! for aarch64 over the **`TPIDR_EL1`** system register — the
//! EL1-private thread-pointer the kernel uses as its per-CPU anchor
//! (its EL0 counterpart `TPIDR_EL0` belongs to user-space TLS and is
//! never touched here). This slice keeps the `TPIDR_EL1` read/write in
//! exactly one place so the architecture-neutral kernel reaches the
//! per-CPU word through the one HAL trait (`AGENTS.md` §2.2).
//!
//! The stored word is opaque to this surface: the kernel decides whether
//! it holds the address of a per-CPU control block or a dense CPU index
//! (see the [`PerCpu`](rustos_arch_api::PerCpu) trait docs). On the host
//! build there is no `TPIDR_EL1`, so the handle backs the word with an
//! in-handle cell solely for the unit tests; it is never linked into a
//! kernel image (`AGENTS.md` §1).

use core::sync::atomic::{AtomicUsize, Ordering};

use rustos_arch_api::PerCpu;

/// aarch64 implementation of the Arch HAL per-CPU storage surface.
///
/// On the bare-metal target the read/write hit `TPIDR_EL1`, so the
/// in-handle cell is unused there; on the host build it backs the word so
/// the round-trip and isolation conformance verticals run under
/// `cargo test`.
#[derive(Debug, Default)]
pub struct PerCpuStorage {
    /// Host-only backing for the per-CPU word. On the bare-metal target
    /// `TPIDR_EL1` is the source of truth and this field is never read;
    /// kept so the host and bare-metal builds share one struct shape
    /// (`AGENTS.md` §1).
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_base: AtomicUsize,
}

impl PerCpuStorage {
    /// Construct the aarch64 per-CPU storage handle.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            host_base: AtomicUsize::new(0),
        }
    }
}

impl PerCpu for PerCpuStorage {
    fn read_self_base(&self) -> usize {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            let tpidr: usize;
            // SAFETY: `mrs x, TPIDR_EL1` reads the EL1 thread-pointer
            // system register. It is side-effect-free, cannot fault at
            // EL1, and returns whatever per-CPU word the kernel installed.
            unsafe {
                core::arch::asm!("mrs {}, TPIDR_EL1", out(reg) tpidr, options(nomem, nostack, preserves_flags));
            }
            tpidr
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            self.host_base.load(Ordering::Relaxed)
        }
    }

    unsafe fn write_self_base(&self, base: usize) {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            // SAFETY: `msr TPIDR_EL1, x` sets the calling CPU's per-CPU
            // word. The trait's safety contract requires the caller to
            // run this on the CPU whose word is being set and to pass the
            // value the kernel's per-CPU resolution expects; the write
            // has no other effect and cannot fault at EL1.
            unsafe {
                core::arch::asm!("msr TPIDR_EL1, {}", in(reg) base, options(nomem, nostack, preserves_flags));
            }
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            self.host_base.store(base, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_api::percpu::conformance;

    #[test]
    fn passes_per_cpu_conformance() {
        conformance::run_all(&PerCpuStorage::new());
        let dynamic: &dyn PerCpu = &PerCpuStorage::new();
        conformance::run_all(dynamic);
    }

    #[test]
    fn per_cpu_word_is_isolated_across_cpus() {
        conformance::run_isolation(&PerCpuStorage::new(), &PerCpuStorage::new());
    }
}
