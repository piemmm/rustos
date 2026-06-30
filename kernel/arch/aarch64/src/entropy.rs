//! aarch64 platform entropy source.
//!
//! Implements the Arch HAL [`PlatformEntropy`](rustos_arch_api::PlatformEntropy)
//! surface for aarch64 over the ARMv8.5 `FEAT_RNG` random-number system
//! register `RNDR` (the reseeded DRBG output of an on-die entropy source).
//! `FEAT_RNG` is detected at runtime from `ID_AA64ISAR0_EL1.RNDR` before the
//! register is read — it is *not* present on every ARMv8 part — and a read
//! that the hardware cannot satisfy (it sets `PSTATE.NZCV` to indicate "no
//! value", and every bounded retry was exhausted) fails closed with
//! `EntropyError::Unavailable`. The kernel never weakens to predictable
//! bytes; `lib/rng` conditions whatever this yields through its DRBG before
//! any caller sees output (hardware output is input material, never final
//! output).
//!
//! The system-register reads are issued only on the bare-metal target
//! (`target_os = "none"`); on the host build (`cargo test`) the draw fails
//! closed, exactly as the [`crate::memtag`] `stg` store is `cfg`-gated out —
//! the real path is exercised by the QEMU verticals on a CPU model with
//! `FEAT_RNG`.

use rustos_arch_api::{EntropyProfile, EntropySupport, PlatformEntropy};
use rustos_rng::{EntropyError, HardwareRng};

/// aarch64 implementation of the Arch HAL platform-entropy surface.
///
/// Zero-sized: the generator is addressed by system register, not by any
/// per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformRng;

impl PlatformRng {
    /// Construct the aarch64 platform-entropy handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for aarch64: the `FEAT_RNG` `RNDR` path is
    /// implemented. Whether `FEAT_RNG` is present is decided at runtime from
    /// `ID_AA64ISAR0_EL1`; its absence makes a draw fail closed.
    #[must_use]
    pub const fn declared_profile() -> EntropyProfile {
        EntropyProfile {
            hardware_rng: EntropySupport::Supported,
        }
    }
}

impl HardwareRng for PlatformRng {
    fn try_fill(&self, out: &mut [u8]) -> Result<(), EntropyError> {
        fill_from_hardware(out)
    }
}

impl PlatformEntropy for PlatformRng {
    fn profile(&self) -> EntropyProfile {
        Self::declared_profile()
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod hw {
    use super::EntropyError;

    /// Bounded retry budget for a single `RNDR` read.
    ///
    /// `RNDR` reports "no value available" through `PSTATE.NZCV` when the
    /// conditioned output is momentarily exhausted; the architecture expects
    /// a small, *bounded* retry, never an unbounded spin (the charter forbids
    /// a retry-until-it-works loop), after which the draw fails closed.
    const RNDR_RETRIES: u32 = 64;

    /// `true` if `ID_AA64ISAR0_EL1.RNDR` (bits [63:60]) reports `FEAT_RNG`.
    fn feat_rng() -> bool {
        let isar0: u64;
        // SAFETY: `ID_AA64ISAR0_EL1` is an EL1-readable, side-effect-free
        // identification register.
        unsafe {
            core::arch::asm!(
                "mrs {v}, ID_AA64ISAR0_EL1",
                v = out(reg) isar0,
                options(nomem, nostack, preserves_flags),
            );
        }
        ((isar0 >> 60) & 0xF) >= 1
    }

    /// Read one 64-bit word from `RNDR`, retrying a bounded number of times.
    fn rndr() -> Option<u64> {
        for _ in 0..RNDR_RETRIES {
            let val: u64;
            let ok: u64;
            // SAFETY: only reached after `feat_rng()` confirmed `FEAT_RNG`.
            // `RNDR` (encoded `S3_3_C2_C4_0`) returns a random value in `val`
            // and sets `PSTATE.NZCV`: success leaves the flags clear, a "no
            // value" failure sets Z. `cset ne` therefore yields 1 on success.
            // The block must not declare `preserves_flags` (the read sets
            // them) and `cset` reads the flags the `mrs` just wrote.
            unsafe {
                core::arch::asm!(
                    "mrs {v}, S3_3_C2_C4_0",
                    "cset {ok:w}, ne",
                    v = out(reg) val,
                    ok = out(reg) ok,
                    options(nomem, nostack),
                );
            }
            if ok != 0 {
                return Some(val);
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Fill `out` from `RNDR`, failing closed when `FEAT_RNG` is absent or a
    /// bounded read is exhausted.
    pub(super) fn fill(out: &mut [u8]) -> Result<(), EntropyError> {
        if !feat_rng() {
            return Err(EntropyError::Unavailable);
        }
        let mut filled = 0;
        while filled < out.len() {
            let Some(word) = rndr() else {
                // A bounded read was exhausted: fail the whole request closed
                // rather than partially fill.
                return Err(EntropyError::Unavailable);
            };
            let bytes = word.to_le_bytes();
            let take = (out.len() - filled).min(bytes.len());
            out[filled..filled + take].copy_from_slice(&bytes[..take]);
            filled += take;
        }
        Ok(())
    }
}

/// Fill `out` from `RNDR` (bare metal), failing closed when `FEAT_RNG` is
/// absent or a bounded read is exhausted.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn fill_from_hardware(out: &mut [u8]) -> Result<(), EntropyError> {
    hw::fill(out)
}

/// Host build: the `RNDR` system register does not exist off the bare-metal
/// aarch64 target, so a draw fails closed.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn fill_from_hardware(_out: &mut [u8]) -> Result<(), EntropyError> {
    Err(EntropyError::Unavailable)
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
    fn declared_profile_is_supported_and_release_ready() {
        let profile = PlatformRng::new().profile();
        assert_eq!(profile.validate(), Ok(()));
        assert!(matches!(profile.hardware_rng, EntropySupport::Supported));
        assert!(profile.is_release_ready());
        assert!(profile.provides_hardware_entropy());
    }

    #[test]
    fn host_draw_fails_closed() {
        let mut out = [0u8; 16];
        assert_eq!(
            PlatformRng::new().try_fill(&mut out),
            Err(EntropyError::Unavailable)
        );
    }
}
