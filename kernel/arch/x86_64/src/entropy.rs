//! x86_64 platform entropy source.
//!
//! Implements the Arch HAL [`PlatformEntropy`](rustos_arch_api::PlatformEntropy)
//! surface for x86_64 over the on-die digital random-number generator: the
//! `RDSEED` instruction (a true non-deterministic entropy source, the
//! conditioner's seed input) is preferred, falling back to `RDRAND` (the
//! DRBG output of that same source) when `RDSEED` is not enumerated. Both are
//! detected at runtime through `CPUID` before they are issued — the
//! instructions are *not* universal across x86_64 silicon — and a draw that
//! cannot be satisfied (the feature is absent, or every bounded retry of a
//! momentarily-underfull generator was exhausted) fails closed with
//! `EntropyError::Unavailable`. The kernel never weakens to predictable
//! bytes; `lib/rng` conditions whatever this yields through its DRBG before
//! any caller sees output (hardware output is input material, never final
//! output).
//!
//! The instructions are issued only on the bare-metal target
//! (`target_os = "none"`); on the host build (`cargo test`) the draw fails
//! closed, exactly as the [`crate::memtag`] store-tag instruction is
//! `cfg`-gated out — the real instruction path is exercised by the QEMU
//! verticals.

use rustos_arch_api::{EntropyProfile, EntropySupport, PlatformEntropy};
use rustos_rng::{EntropyError, HardwareRng};

/// x86_64 implementation of the Arch HAL platform-entropy surface.
///
/// Zero-sized: the generator is addressed by instruction, not by any
/// per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformRng;

impl PlatformRng {
    /// Construct the x86_64 platform-entropy handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for x86_64: a hardware entropy instruction is
    /// implemented. Whether `RDSEED`/`RDRAND` are enumerated on a given part
    /// is decided at runtime; an absent instruction makes a draw fail closed.
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

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod hw {
    use super::EntropyError;

    /// `CPUID` leaf 1, `ECX` bit 30: `RDRAND` is supported.
    const CPUID_LEAF_FEATURES: u32 = 1;
    const ECX_RDRAND_BIT: u32 = 1 << 30;

    /// `CPUID` leaf 7 sub-leaf 0, `EBX` bit 18: `RDSEED` is supported.
    const CPUID_LEAF_EXTENDED_FEATURES: u32 = 7;
    const EBX_RDSEED_BIT: u32 = 1 << 18;

    /// Bounded retry budget for a single 64-bit hardware draw.
    ///
    /// `RDRAND`/`RDSEED` report success (the intrinsic returns `1`) only when
    /// they delivered a fresh value; under heavy contention the generator can
    /// be momentarily underfull and return failure. Intel's guidance is to
    /// retry a small, *bounded* number of times — never an unbounded spin
    /// (the charter forbids a retry-until-it-works loop) — then fail closed.
    /// `RDSEED` reseeds more slowly than `RDRAND`, so it gets the larger
    /// budget.
    const RDRAND_RETRIES: u32 = 16;
    const RDSEED_RETRIES: u32 = 128;

    /// Fill `out` from the on-die RNG, preferring `RDSEED` over `RDRAND`.
    pub(super) fn fill(out: &mut [u8]) -> Result<(), EntropyError> {
        // CPUID is universal on x86_64 and side-effect-free.
        let leaf1_ecx = unsafe { core::arch::x86_64::__cpuid(CPUID_LEAF_FEATURES) }.ecx;
        let max_leaf = unsafe { core::arch::x86_64::__cpuid(0) }.eax;
        let leaf7_ebx = if max_leaf >= CPUID_LEAF_EXTENDED_FEATURES {
            unsafe { core::arch::x86_64::__cpuid_count(CPUID_LEAF_EXTENDED_FEATURES, 0) }.ebx
        } else {
            0
        };

        let use_rdseed = leaf7_ebx & EBX_RDSEED_BIT != 0;
        let use_rdrand = leaf1_ecx & ECX_RDRAND_BIT != 0;
        if !use_rdseed && !use_rdrand {
            return Err(EntropyError::Unavailable);
        }

        let mut filled = 0;
        while filled < out.len() {
            // SAFETY: each draw fn is only called after CPUID confirmed its
            // instruction is enumerated on this CPU; that is the entire
            // safety obligation of the `#[target_feature]` draw helpers.
            let word = if use_rdseed {
                unsafe { rdseed_u64() }
            } else {
                unsafe { rdrand_u64() }
            };
            let Some(word) = word else {
                // A bounded draw was exhausted: fail the whole request closed
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

    /// Draw one 64-bit word from `RDSEED`, retrying a bounded number of times.
    ///
    /// # Safety
    ///
    /// The caller must have confirmed via `CPUID` that `RDSEED` is enumerated
    /// on the executing CPU.
    #[target_feature(enable = "rdseed")]
    unsafe fn rdseed_u64() -> Option<u64> {
        let mut val = 0u64;
        for _ in 0..RDSEED_RETRIES {
            // SAFETY: RDSEED is enumerated (caller obligation); the intrinsic
            // writes `val` and returns 1 only when a fresh value was produced.
            if unsafe { core::arch::x86_64::_rdseed64_step(&mut val) } == 1 {
                return Some(val);
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Draw one 64-bit word from `RDRAND`, retrying a bounded number of times.
    ///
    /// # Safety
    ///
    /// The caller must have confirmed via `CPUID` that `RDRAND` is enumerated
    /// on the executing CPU.
    #[target_feature(enable = "rdrand")]
    unsafe fn rdrand_u64() -> Option<u64> {
        let mut val = 0u64;
        for _ in 0..RDRAND_RETRIES {
            // SAFETY: RDRAND is enumerated (caller obligation); the intrinsic
            // writes `val` and returns 1 only when a fresh value was produced.
            if unsafe { core::arch::x86_64::_rdrand64_step(&mut val) } == 1 {
                return Some(val);
            }
            core::hint::spin_loop();
        }
        None
    }
}

/// Fill `out` from the on-die RNG (bare metal), failing closed when no
/// instruction is enumerated or a bounded draw is exhausted.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn fill_from_hardware(out: &mut [u8]) -> Result<(), EntropyError> {
    hw::fill(out)
}

/// Host build: no bare-metal RNG instruction is issued (deterministic tests,
/// no dependency on the host CPU's feature set), so a draw fails closed.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
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
        // On the host build the instruction is not issued, so a draw fails
        // closed (the conformance suite tolerates this for a Supported port).
        let mut out = [0u8; 16];
        assert_eq!(
            PlatformRng::new().try_fill(&mut out),
            Err(EntropyError::Unavailable)
        );
    }
}
