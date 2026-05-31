//! wasm32 side-channel mitigations (`AGENTS.md` §19.1).
//!
//! Implements the Arch HAL
//! [`SideChannelMitigation`](rustos_arch_api::SideChannelMitigation)
//! surface for the
//! `wasm32-unknown-unknown` browser sandbox. On this target RustOS is a
//! guest of the JavaScript host (a Chrome-class engine), and the §19.1
//! microarchitectural defences are owned by that host, not by the
//! kernel:
//!
//! * **Memory isolation** is the WebAssembly model itself — one linear
//!   memory per Web Worker, with no shared page tables and no privileged
//!   instruction the guest can use to read another worker's memory
//!   (`kernel/arch/wasm32::isolation`). There is no kernel/user page
//!   table to unmap, so KPTI has no analogue here.
//! * **There is no speculation-barrier instruction** in the WebAssembly
//!   ISA, and no syscall trap that crosses a hardware privilege boundary:
//!   a "syscall" is a host import call mediated by the engine.
//! * **Microarchitectural side channels** (Spectre via high-resolution
//!   timers, MDS, L1TF) are mitigated by the host — site isolation,
//!   `performance.now()` clamping, cross-origin isolation (COOP/COEP) —
//!   which the guest cannot and must not try to override.
//!
//! Every §19.1 mitigation is therefore a justified
//! [`Mitigation::NotVulnerable`](rustos_arch_api::Mitigation::NotVulnerable)
//! no-op (`AGENTS.md` §19.1 — a no-op is
//! permitted where the silicon, here the sandbox, is provably not
//! vulnerable from the guest's vantage point). The barrier primitives
//! are empty: there is no instruction to emit and nothing the guest
//! could do that the host has not already done.

use rustos_arch_api::{Mitigation, MitigationProfile, SideChannelMitigation};

/// The justification shared by every wasm32 mitigation slot: the
/// browser host owns the microarchitectural defences and the
/// WebAssembly model owns memory isolation.
const HOST_OWNED: &str = "the wasm32 browser sandbox delegates microarchitectural defences to the \
                          JavaScript host (site isolation, timer clamping, COOP/COEP) and isolates \
                          memory via per-worker linear memory; the guest has no barrier instruction \
                          and no hardware privilege boundary to defend";

/// wasm32 implementation of the Arch HAL side-channel surface.
///
/// Zero-sized: every mitigation is host-owned, so the handle carries no
/// state and the barrier primitives are empty.
#[derive(Debug, Default, Clone, Copy)]
pub struct SideChannel;

impl SideChannel {
    /// Construct the wasm32 side-channel handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest §19.1 declaration for wasm32 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> MitigationProfile {
        MitigationProfile {
            address_space_isolation: Mitigation::NotVulnerable(HOST_OWNED),
            syscall_entry_barrier: Mitigation::NotVulnerable(HOST_OWNED),
            syscall_exit_barrier: Mitigation::NotVulnerable(HOST_OWNED),
            context_switch_buffer_flush: Mitigation::NotVulnerable(HOST_OWNED),
            context_switch_indirect_branch_barrier: Mitigation::NotVulnerable(HOST_OWNED),
        }
    }
}

impl SideChannelMitigation for SideChannel {
    fn profile(&self) -> MitigationProfile {
        Self::declared_profile()
    }

    // The WebAssembly ISA has no speculation-barrier instruction and the
    // host mediates every privilege transition, so the barrier primitives
    // are empty by design (not stubs): there is nothing for the guest to
    // emit (`AGENTS.md` §19.1).
    fn syscall_entry_barrier(&self) {}
    fn syscall_exit_barrier(&self) {}
    fn context_switch_barrier(&self) {}
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
    fn every_mitigation_is_host_owned_and_release_ready() {
        let profile = SideChannel::new().profile();
        assert_eq!(profile.validate(), Ok(()));
        // Every slot is a justified host-owned no-op, so the port has no
        // outstanding side-channel gap and is release-ready.
        for entry in profile.entries() {
            assert!(
                matches!(entry.mitigation, Mitigation::NotVulnerable(_)),
                "slot {} should be a host-owned no-op",
                entry.name
            );
        }
        assert!(profile.is_release_ready());
    }

    #[test]
    fn barriers_are_callable() {
        let sc = SideChannel::new();
        sc.syscall_entry_barrier();
        sc.syscall_exit_barrier();
        sc.context_switch_barrier();
    }
}
