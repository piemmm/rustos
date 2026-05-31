//! riscv64 side-channel mitigations (`AGENTS.md` §19.1).
//!
//! Implements the Arch HAL
//! [`SideChannelMitigation`](rustos_arch_api::SideChannelMitigation)
//! surface for
//! riscv64. The RISC-V cores RustOS targets — the QEMU `virt` board and
//! the SiFive U-series (U54 / U74) — execute strictly **in order**: they
//! do not continue past a faulting load or a mispredicted branch, so the
//! transient-execution side channels that drive Meltdown, Spectre, MDS,
//! and L1TF have no exploitable window on this silicon.
//!
//! # What is applied today
//!
//! * **Syscall entry/exit speculation/ordering barrier** — `fence`
//!   (full `iorw,iorw` memory fence). It is unconditionally available in
//!   the base ISA and orders memory accesses across the privilege
//!   boundary. It is emitted as a conservative, always-safe barrier so
//!   the boundary is correctly serialised (and so an out-of-order RISC-V
//!   core, were one to be added as a target, still gets a barrier here).
//!
//! # What is `NotVulnerable` (justified no-op)
//!
//! * **Kernel/user address-space isolation (KPTI)** — Meltdown requires
//!   speculation past a faulting privileged load; the in-order cores
//!   RustOS targets never do, so there is no Meltdown-class leak.
//!   Kernel/user separation is still enforced by ordinary page-table
//!   permissions (the `U` bit), which is not the §19.1 KPTI control.
//! * **Context-switch microarchitectural-buffer flush** — MDS / L1TF /
//!   MMIO-stale-data are Intel store/fill/load-buffer sampling flaws; the
//!   RISC-V cores RustOS targets do not expose those buffers.
//! * **Context-switch indirect-branch-predictor barrier** — exploiting a
//!   poisoned branch predictor needs transient execution past the
//!   mispredict; the in-order cores RustOS targets do not provide that
//!   window.
//!
//! Were RustOS to add an out-of-order RISC-V core (a future Tier-2
//! target), this profile must be revisited per that core's errata
//! (`AGENTS.md` §19.1 — a no-op is permitted only where the silicon is
//! provably not vulnerable).

use rustos_arch_api::{Mitigation, MitigationProfile, SideChannelMitigation};

/// riscv64 implementation of the Arch HAL side-channel surface.
///
/// Zero-sized: the barrier primitive needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct SideChannel;

impl SideChannel {
    /// Construct the riscv64 side-channel handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest §19.1 declaration for riscv64 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> MitigationProfile {
        MitigationProfile {
            address_space_isolation: Mitigation::NotVulnerable(
                "Meltdown needs speculation past a faulting load; the in-order RISC-V cores \
                 RustOS targets (QEMU virt, SiFive U54/U74) never do, and page-permission \
                 isolation still applies",
            ),
            syscall_entry_barrier: Mitigation::Applied,
            syscall_exit_barrier: Mitigation::Applied,
            context_switch_buffer_flush: Mitigation::NotVulnerable(
                "MDS / L1TF / MMIO-stale-data are Intel store/fill/load-buffer sampling flaws; \
                 the RISC-V cores RustOS targets do not expose those buffers",
            ),
            context_switch_indirect_branch_barrier: Mitigation::NotVulnerable(
                "exploiting a poisoned branch predictor needs transient execution past the \
                 mispredict; the in-order RISC-V cores RustOS targets provide no such window",
            ),
        }
    }
}

impl SideChannelMitigation for SideChannel {
    fn profile(&self) -> MitigationProfile {
        Self::declared_profile()
    }

    fn syscall_entry_barrier(&self) {
        ordering_fence();
    }

    fn syscall_exit_barrier(&self) {
        ordering_fence();
    }

    fn context_switch_barrier(&self) {
        // The buffer flush and the indirect-branch barrier are justified
        // no-ops on the in-order RISC-V cores RustOS targets (see the
        // module docs); the ordering fence is still emitted so the switch
        // boundary is serialised.
        ordering_fence();
    }
}

/// `fence` — a full `iorw,iorw` memory-ordering barrier. Unconditionally
/// available in the base RISC-V ISA; conservative and always safe.
#[inline]
fn ordering_fence() {
    #[cfg(all(target_arch = "riscv64", target_os = "none"))]
    {
        // SAFETY: the unqualified `fence` (RISC-V unprivileged ISA) is a
        // full `iorw,iorw` memory-ordering barrier available on every
        // RISC-V hart. It has no register operands and cannot fault; the
        // absence of `nomem` keeps the compiler from reordering memory
        // accesses across the barrier, which is the point of emitting it.
        unsafe {
            core::arch::asm!("fence", options(nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_api::sidechannel::conformance;

    #[test]
    fn passes_side_channel_conformance() {
        conformance::run_all(&SideChannel::new());
    }

    #[test]
    fn declared_profile_is_honest_and_release_ready() {
        let profile = SideChannel::new().profile();
        assert_eq!(profile.validate(), Ok(()));

        // The ordering fence is emitted on each boundary.
        assert_eq!(profile.syscall_entry_barrier, Mitigation::Applied);
        assert_eq!(profile.syscall_exit_barrier, Mitigation::Applied);

        // Every other slot is a justified no-op on the in-order cores
        // RustOS targets — so the riscv64 port has no outstanding
        // side-channel gap and is release-ready.
        assert!(matches!(
            profile.address_space_isolation,
            Mitigation::NotVulnerable(_)
        ));
        assert!(matches!(
            profile.context_switch_buffer_flush,
            Mitigation::NotVulnerable(_)
        ));
        assert!(matches!(
            profile.context_switch_indirect_branch_barrier,
            Mitigation::NotVulnerable(_)
        ));
        assert!(profile.is_release_ready());
    }

    #[test]
    fn barriers_are_callable_on_the_host() {
        let sc = SideChannel::new();
        sc.syscall_entry_barrier();
        sc.syscall_exit_barrier();
        sc.context_switch_barrier();
    }
}
