//! x86_64 side-channel mitigations (`AGENTS.md` §19.1).
//!
//! Implements the Arch HAL
//! [`SideChannelMitigation`](rustos_arch_api::SideChannelMitigation)
//! surface for x86_64, whose silicon is vulnerable to the full microarchitectural
//! side-channel zoo (Meltdown, Spectre v1/v2, MDS, L1TF, MMIO stale
//! data). The barrier primitives the kernel calls on each privilege
//! transition are emitted here; the declarative
//! [`MitigationProfile`](rustos_arch_api::MitigationProfile) states
//! honestly which §19.1 mitigations are applied today and which are
//! tracked as [`Mitigation::Pending`](rustos_arch_api::Mitigation::Pending)
//! behind a not-yet-landed subsystem.
//!
//! # What is applied today
//!
//! * **Syscall entry/exit speculation barrier** — `lfence`. It
//!   serialises the instruction stream against speculative loads
//!   (the documented Spectre-v1 / speculative-store-bypass load fence)
//!   and is unconditionally available on every x86_64 CPU (SSE2 is
//!   mandatory), so it is always safe to emit.
//! * **Context-switch microarchitectural-buffer flush** — `verw`. On a
//!   CPU that enumerates `MD_CLEAR` this clears the store/fill/load
//!   buffers (the documented MDS / L1TF / MMIO-stale-data mitigation);
//!   on a CPU without it the instruction is a harmless segment check.
//!   `verw` predates the architecture, so it never faults.
//!
//! # What is `Pending` (tracked, not yet shippable)
//!
//! * **Kernel/user address-space isolation (KPTI)** — a separate `CR3`
//!   per privilege level requires the Stage 6 user/kernel boundary and
//!   the real arch page tables; there is no user mode to isolate from
//!   yet (`PLAN.md` §19 burn-down item 10 / Stage 6).
//! * **Context-switch indirect-branch-predictor barrier (IBPB)** —
//!   writing `IA32_PRED_CMD` (MSR `0x49`) `#GP`s on a CPU that does not
//!   enumerate the `IBPB` feature, so it must be gated on a `CPUID`
//!   feature probe. The `CPUID`/feature-MSR plumbing is not yet wired,
//!   so issuing it unconditionally would be a fault, not a mitigation.
//!
//! Both gaps are declared honestly so the conformance suite accepts the
//! port today while
//! [`MitigationProfile::is_release_ready`](rustos_arch_api::MitigationProfile::is_release_ready)
//! continues to report the port as not-yet-shippable until they land.

use rustos_arch_api::{Mitigation, MitigationProfile, SideChannelMitigation};

/// x86_64 implementation of the Arch HAL side-channel surface.
///
/// Zero-sized: the barrier primitives need no per-instance state today.
/// When `CPUID`-gated IBPB/IBRS lands the discovered feature bits will
/// live here.
#[derive(Debug, Default, Clone, Copy)]
pub struct SideChannel;

impl SideChannel {
    /// Construct the x86_64 side-channel handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest §19.1 declaration for x86_64 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> MitigationProfile {
        MitigationProfile {
            address_space_isolation: Mitigation::Pending(
                "KPTI / per-privilege CR3 needs the Stage 6 user/kernel boundary and real arch \
                 page tables (PLAN.md §19 burn-down item 10)",
            ),
            syscall_entry_barrier: Mitigation::Applied,
            syscall_exit_barrier: Mitigation::Applied,
            context_switch_buffer_flush: Mitigation::Applied,
            context_switch_indirect_branch_barrier: Mitigation::Pending(
                "IBPB via IA32_PRED_CMD #GPs without the CPUID IBPB feature bit; the \
                 CPUID/feature-MSR probe is not yet wired",
            ),
        }
    }
}

impl SideChannelMitigation for SideChannel {
    fn profile(&self) -> MitigationProfile {
        Self::declared_profile()
    }

    fn syscall_entry_barrier(&self) {
        speculation_fence();
    }

    fn syscall_exit_barrier(&self) {
        speculation_fence();
    }

    fn context_switch_barrier(&self) {
        clear_cpu_buffers();
        speculation_fence();
    }
}

/// `lfence` — serialise the instruction stream against speculative
/// loads. The Spectre-v1 / SSB load fence; unconditionally available.
#[inline]
fn speculation_fence() {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // SAFETY: `LFENCE` (Intel SDM Vol 2A) is unconditionally
        // available on every x86_64 CPU (SSE2 is mandatory). It is a
        // serialising load fence with no memory or flag side effects and
        // no operands, so it cannot fault and needs no clobbers.
        unsafe {
            core::arch::asm!("lfence", options(nomem, nostack, preserves_flags));
        }
    }
}

/// `verw` against an in-memory selector — clear the microarchitectural
/// store/fill/load buffers on a CPU that enumerates `MD_CLEAR`, the
/// documented MDS / L1TF / MMIO-stale-data mitigation. Harmless on a CPU
/// without `MD_CLEAR`.
#[inline]
fn clear_cpu_buffers() {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // The buffer-clearing side effect on an `MD_CLEAR` part happens
        // for any memory operand regardless of selector validity; a NULL
        // selector simply clears ZF without faulting.
        let selector: u16 = 0;
        // SAFETY: `VERW` (Intel SDM Vol 2B) never faults: it verifies a
        // segment selector read from the supplied memory operand and, on
        // an `MD_CLEAR`-enumerating CPU, overwrites the affected internal
        // buffers. `selector` is a live stack local, so the memory
        // operand is valid. `VERW` writes the zero flag, so the block
        // does not claim `preserves_flags`, and it reads (does not
        // write) memory, so `readonly` is honest.
        unsafe {
            core::arch::asm!(
                "verw word ptr [{sel}]",
                sel = in(reg) &selector,
                options(readonly, nostack),
            );
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
    fn declared_profile_is_honest_and_matches_silicon() {
        let profile = SideChannel::new().profile();
        // The honesty gate accepts it (every Pending carries a note).
        assert_eq!(profile.validate(), Ok(()));

        // x86_64 applies the speculation fence and the MDS buffer clear.
        assert_eq!(profile.syscall_entry_barrier, Mitigation::Applied);
        assert_eq!(profile.syscall_exit_barrier, Mitigation::Applied);
        assert_eq!(profile.context_switch_buffer_flush, Mitigation::Applied);

        // KPTI and IBPB are tracked Pending gaps, so the port is not yet
        // release-ready (AGENTS.md §19.1 — a target that does not pass
        // cannot ship; that gate fires when these land).
        assert!(profile.address_space_isolation.is_pending());
        assert!(profile.context_switch_indirect_branch_barrier.is_pending());
        assert!(!profile.is_release_ready());
    }

    #[test]
    fn barriers_are_callable_on_the_host() {
        // On the host the instruction emission is compiled out; this
        // pins the call sites so the trait wiring cannot rot.
        let sc = SideChannel::new();
        sc.syscall_entry_barrier();
        sc.syscall_exit_barrier();
        sc.context_switch_barrier();
    }
}
