//! riscv64 platform entropy source.
//!
//! Implements the Arch HAL [`PlatformEntropy`](rustos_arch_api::PlatformEntropy)
//! surface for riscv64. The RISC-V scalar-crypto `Zkr` extension exposes a
//! true entropy source through the `seed` CSR (`0x015`), but that CSR is an
//! **M-mode** resource: an S-mode read traps unless the M-mode firmware
//! (OpenSBI) has delegated access through `mseccfg.sseed`, and that
//! delegation is not yet wired in the RustOS boot path. Issuing the read
//! before the delegation lands would fault, so the port honestly declares the
//! source `EntropySupport::Pending` and every draw fails closed with
//! `EntropyError::Unavailable` rather than weakening to predictable bytes.
//!
//! When the M-mode `seed`-CSR delegation lands (tracked below), this port
//! gains the real `csrr`-based draw and the profile becomes
//! `EntropySupport::Supported`; until then the kernel CSPRNG reserve simply
//! stays unseeded on riscv64, exactly as it fails closed before any platform
//! source is available.

use rustos_arch_api::{EntropyProfile, EntropySupport, PlatformEntropy};
use rustos_rng::{EntropyError, HardwareRng};

/// Tracking note for the `Pending` declaration: the `Zkr` `seed` CSR read
/// needs the M-mode `mseccfg.sseed` delegation, not yet wired in boot.
const PENDING_NOTE: &str =
    "the Zkr `seed` CSR (0x015) is an M-mode resource; an S-mode read needs \
     the OpenSBI `mseccfg.sseed` delegation, which the RustOS riscv64 boot \
     path does not yet wire — failing closed until it lands";

/// riscv64 implementation of the Arch HAL platform-entropy surface.
///
/// Zero-sized: no usable per-instance state until the `seed`-CSR delegation
/// lands.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformRng;

impl PlatformRng {
    /// Construct the riscv64 platform-entropy handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for riscv64: the `Zkr` source exists in the
    /// ISA but cannot be used until the M-mode `seed`-CSR delegation lands,
    /// so it is a tracked `EntropySupport::Pending`.
    #[must_use]
    pub const fn declared_profile() -> EntropyProfile {
        EntropyProfile {
            hardware_rng: EntropySupport::Pending(PENDING_NOTE),
        }
    }
}

impl HardwareRng for PlatformRng {
    fn try_fill(&self, _out: &mut [u8]) -> Result<(), EntropyError> {
        // Fail closed: the `seed` CSR is not yet reachable from S-mode.
        Err(EntropyError::Unavailable)
    }
}

impl PlatformEntropy for PlatformRng {
    fn profile(&self) -> EntropyProfile {
        Self::declared_profile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_api::entropy::conformance;

    #[test]
    fn passes_entropy_conformance() {
        conformance::run_all(&PlatformRng::new());
    }

    #[test]
    fn declared_profile_is_pending_and_not_release_ready() {
        let profile = PlatformRng::new().profile();
        assert_eq!(profile.validate(), Ok(()));
        assert!(profile.hardware_rng.is_pending());
        assert!(!profile.is_release_ready());
        assert!(!profile.provides_hardware_entropy());
    }

    #[test]
    fn draw_fails_closed() {
        let mut out = [0u8; 16];
        assert_eq!(
            PlatformRng::new().try_fill(&mut out),
            Err(EntropyError::Unavailable)
        );
    }
}
