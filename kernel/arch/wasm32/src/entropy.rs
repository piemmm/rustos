//! wasm32 platform entropy source.
//!
//! Implements the Arch HAL [`PlatformEntropy`](rustos_arch_api::PlatformEntropy)
//! surface for the wasm32 host environment. A browser/host sandbox *does*
//! expose a CSPRNG (`crypto.getRandomValues`), but a freestanding
//! `wasm32-unknown-unknown` module reaches it only through an imported host
//! function, and that import is not yet bound in the RustOS wasm32 runtime.
//! Until the import is wired, the port honestly declares the source
//! `EntropySupport::Pending` and every draw fails closed with
//! `EntropyError::Unavailable` rather than fabricating bytes.
//!
//! When the host entropy import lands (tracked below), this port forwards to
//! it and the profile becomes `EntropySupport::Supported`; until then the
//! kernel CSPRNG reserve stays unseeded on wasm32, failing closed exactly as
//! before any platform source is available.

use rustos_arch_api::{EntropyProfile, EntropySupport, PlatformEntropy};
use rustos_rng::{EntropyError, HardwareRng};

/// Tracking note for the `Pending` declaration: the host CSPRNG exists but
/// the freestanding module's import is not yet bound.
const PENDING_NOTE: &str = "the wasm32 host CSPRNG (`crypto.getRandomValues`) is reachable only \
     through an imported host function, which the RustOS wasm32 runtime does \
     not yet bind — failing closed until that import is wired";

/// wasm32 implementation of the Arch HAL platform-entropy surface.
///
/// Zero-sized: no usable per-instance state until the host entropy import is
/// bound.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformRng;

impl PlatformRng {
    /// Construct the wasm32 platform-entropy handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for wasm32: the host CSPRNG exists but its
    /// import is not yet bound, so the source is a tracked
    /// `EntropySupport::Pending`.
    #[must_use]
    pub const fn declared_profile() -> EntropyProfile {
        EntropyProfile {
            hardware_rng: EntropySupport::Pending(PENDING_NOTE),
        }
    }
}

impl HardwareRng for PlatformRng {
    fn try_fill(&self, _out: &mut [u8]) -> Result<(), EntropyError> {
        // Fail closed: the host entropy import is not yet bound.
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
