//! aarch64 side-channel mitigations.
//!
//! Implements the Arch HAL
//! [`SideChannelMitigation`](rustos_arch_api::SideChannelMitigation)
//! surface for
//! aarch64. The `ARMv8` cores RustOS targets (Cortex-A in the Raspberry
//! Pi 3/4/5 and generic `ARMv8` servers) are exposed to the speculative
//! side-channel classes (Spectre v1/v2) but **not** to the Intel
//! microarchitectural-data-sampling family (MDS, L1TF, MMIO stale
//! data), which are properties of Intel store/fill/load-buffer
//! microarchitecture.
//!
//! # What is applied today
//!
//! * **Syscall entry/exit speculation barrier** — `csdb` (Consume
//!   Speculative Data Barrier), the documented `ARMv8` Spectre-v1 barrier
//!   (names "`CSDB`/`SB`"). It is encoded in the hint
//!   space, so it decodes on every `ARMv8` core and is a NOP on a core
//!   that does not speculate — always safe to emit.
//!
//! # What is `NotVulnerable` (justified no-op)
//!
//! * **Context-switch microarchitectural-buffer flush** — the MDS /
//!   L1TF / MMIO-stale-data buffer-sampling classes are Intel
//!   microarchitectural flaws; the `ARMv8` cores RustOS targets do not
//!   expose the affected store/fill/load buffers, so there is nothing
//!   to flush (a no-op is permitted where the
//!   silicon is provably not vulnerable).
//!
//! # What is `Pending` (tracked, not yet shippable)
//!
//! * **Kernel/user address-space isolation** — `ARMv8` splits user
//!   (`TTBR0_EL1`) from kernel (`TTBR1_EL1`) natively, but unmapping the
//!   kernel from the user-reachable translation regime needs the Stage 6
//!   user/kernel boundary; there is no user mode to isolate from yet
//!   (`PLAN.md` §19 burn-down item 10 / Stage 6).
//! * **Context-switch indirect-branch-predictor barrier** — the `ARMv8`
//!   Spectre-v2 (BTB/BHB) mitigation is a per-core sequence (a BHB
//!   clearing loop or the `SMCCC_ARCH_WORKAROUND` firmware call) selected
//!   from the discovered CPU MIDR; the MIDR/firmware-feature probe is not
//!   yet wired, so emitting one core's sequence blindly would be wrong on
//!   another.

use rustos_arch_api::{Mitigation, MitigationProfile, SideChannelMitigation};

/// aarch64 implementation of the Arch HAL side-channel surface.
///
/// Zero-sized: the barrier primitive needs no per-instance state today.
/// When the MIDR-gated Spectre-v2 sequence lands the discovered CPU
/// identity will live here.
#[derive(Debug, Default, Clone, Copy)]
pub struct SideChannel;

impl SideChannel {
    /// Construct the aarch64 side-channel handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for aarch64 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> MitigationProfile {
        MitigationProfile {
            address_space_isolation: Mitigation::Pending(
                "unmapping the kernel from the user TTBR0_EL1 regime needs the Stage 6 \
                 user/kernel boundary (PLAN.md §19 burn-down item 10)",
            ),
            syscall_entry_barrier: Mitigation::Applied,
            syscall_exit_barrier: Mitigation::Applied,
            context_switch_buffer_flush: Mitigation::NotVulnerable(
                "MDS / L1TF / MMIO-stale-data are Intel store/fill/load-buffer sampling flaws; \
                 the ARMv8 cores RustOS targets do not expose those buffers",
            ),
            context_switch_indirect_branch_barrier: Mitigation::Pending(
                "the ARMv8 Spectre-v2 BHB/BTB sequence is MIDR-specific (BHB-clear loop or \
                 SMCCC firmware workaround); the MIDR/firmware-feature probe is not yet wired",
            ),
        }
    }
}

impl SideChannelMitigation for SideChannel {
    fn profile(&self) -> MitigationProfile {
        Self::declared_profile()
    }

    fn syscall_entry_barrier(&self) {
        speculation_barrier();
    }

    fn syscall_exit_barrier(&self) {
        speculation_barrier();
    }

    fn context_switch_barrier(&self) {
        // The MDS-class buffer flush is a justified no-op on ARMv8 (see
        // the module docs); the Spectre-v2 IBP barrier is Pending. The
        // speculation barrier is still emitted so a switched-to task does
        // not consume speculative state across the boundary.
        speculation_barrier();
    }
}

/// `csdb` — the `ARMv8` Consume Speculative Data Barrier (the documented
/// Spectre-v1 barrier). Encoded in the hint space, so it decodes on
/// every `ARMv8` core and is a NOP where the core does not speculate.
#[inline]
fn speculation_barrier() {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        // SAFETY: `CSDB` (Arm ARM, the Spectre-v1 barrier) is a hint-space
        // instruction: it decodes on every ARMv8 implementation, has no
        // register, memory, or flag side effects, and cannot fault.
        unsafe {
            core::arch::asm!("csdb", options(nomem, nostack, preserves_flags));
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
        assert_eq!(profile.validate(), Ok(()));

        // aarch64 applies the speculation barrier on each boundary.
        assert_eq!(profile.syscall_entry_barrier, Mitigation::Applied);
        assert_eq!(profile.syscall_exit_barrier, Mitigation::Applied);

        // The MDS-class buffer flush is a justified no-op (Intel-only).
        assert!(matches!(
            profile.context_switch_buffer_flush,
            Mitigation::NotVulnerable(_)
        ));

        // KPTI and the Spectre-v2 IBP barrier are tracked Pending gaps.
        assert!(profile.address_space_isolation.is_pending());
        assert!(profile.context_switch_indirect_branch_barrier.is_pending());
        assert!(!profile.is_release_ready());
    }

    #[test]
    fn barriers_are_callable_on_the_host() {
        let sc = SideChannel::new();
        sc.syscall_entry_barrier();
        sc.syscall_exit_barrier();
        sc.context_switch_barrier();
    }
}
