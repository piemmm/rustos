//! The motherboard hardware-RNG seam and a hardware-first fast generator.
//!
//! Many platforms expose a hardware random source: an on-die DRBG/entropy
//! instruction (x86 `RDRAND`/`RDSEED`, ARMv8.5 `RNDR`/`RNDRRS`, RISC-V
//! `Zkr`), a TPM, or a virtio-rng device. [`HardwareRng`] is the seam through
//! which such a source is offered to `lib/rng`. The *concrete* driver is
//! architecture- or device-specific and therefore lives in
//! `kernel/arch/<target>` or a `drivers/*` crate (: no
//! target-conditional code in `lib/*`); this crate only consumes the trait,
//! so it stays architecture-neutral and host-testable.
//!
//! A hardware source plays two roles, exactly as the issue asks:
//!
//! * **Extra entropy.** Wrapped in [`HardwareEntropy`] it becomes an ordinary
//!   [`EntropySource`], so it can be XOR-mixed by
//!   [`crate::CombinedSource`] *alongside* the other platform sources and
//!   feed [`crate::CsRng`]. It is never trusted as the *sole* source — a
//!   vendor RNG could be weak or backdoored — only as one independent input.
//! * **A fast source.** [`PlatformFast`] uses the hardware source directly
//!   for fast, non-cryptographic `u64`s when present, and falls back to the
//!   software [`FastRng`] when it is absent or momentarily fails. There is no
//!   busy-retry-until-it-works loop: one failed draw
//!   simply falls through to the software generator.

use crate::entropy::{EntropyError, EntropySource};
use crate::fast::FastRng;
use crate::rand::RandU64;

/// A platform hardware random source.
///
/// Implementations are the architecture/device drivers (e.g. an `RDRAND`
/// wrapper in `kernel/arch/x86_64`). The method takes `&self` because such
/// sources are stateless from the caller's view; any internal retry that the
/// hardware guidance requires (Intel recommends retrying `RDRAND` up to ten
/// times) is the implementation's responsibility, so a transient under-supply
/// is hidden from callers and only a genuine, persistent failure surfaces as
/// [`EntropyError::Unavailable`].
pub trait HardwareRng {
    /// Fill `out` with hardware-generated random bytes.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] if the hardware cannot supply
    /// the bytes (instruction unsupported, or every internal retry was
    /// exhausted).
    fn try_fill(&self, out: &mut [u8]) -> Result<(), EntropyError>;
}

/// Adapts a [`HardwareRng`] into an [`EntropySource`] so it can seed and
/// reseed [`crate::CsRng`] or be one input to [`crate::CombinedSource`].
pub struct HardwareEntropy<'h, H: HardwareRng> {
    hardware: &'h H,
}

impl<'h, H: HardwareRng> HardwareEntropy<'h, H> {
    /// Wrap a hardware source as an entropy source.
    #[must_use]
    pub fn new(hardware: &'h H) -> Self {
        Self { hardware }
    }
}

impl<H: HardwareRng> EntropySource for HardwareEntropy<'_, H> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        self.hardware.try_fill(out)
    }
}

/// A fast `u64` generator that prefers a hardware source and falls back to
/// software.
///
/// Construct with [`PlatformFast::new`], passing the platform's hardware
/// source if one was detected. This is the issue's "hardware RNG as a fast
/// source, with fallback to a faster version of our own RNG if no hardware
/// present": when hardware is present it is used directly (and any transient
/// failure still falls through to the software generator, so [`RandU64`] is
/// infallible); when it is absent, the software [`FastRng`] is used outright.
///
/// As with [`FastRng`], this is **not** cryptographically secure and must not
/// produce keys or nonces; those go through [`crate::CsRng`].
pub enum PlatformFast<H: HardwareRng> {
    /// Hardware present: draw from it, with a software generator on standby.
    Hardware {
        /// The platform hardware source.
        hardware: H,
        /// Software generator used when a hardware draw fails.
        fallback: FastRng,
    },
    /// No hardware: the software generator is the fast source.
    Software(FastRng),
}

impl<H: HardwareRng> PlatformFast<H> {
    /// Build a fast generator, preferring `hardware` when `Some`.
    ///
    /// `fallback_seed` seeds the software [`FastRng`] used either as the sole
    /// generator (no hardware) or as the standby for transient hardware
    /// failures. Seed it from [`crate::CsRng::try_next_u64`] for an
    /// unpredictable fallback.
    #[must_use]
    pub fn new(hardware: Option<H>, fallback_seed: u64) -> Self {
        match hardware {
            Some(hardware) => Self::Hardware {
                hardware,
                fallback: FastRng::seed_from_u64(fallback_seed),
            },
            None => Self::Software(FastRng::seed_from_u64(fallback_seed)),
        }
    }

    /// `true` if this generator is backed by a hardware source.
    #[must_use]
    pub fn is_hardware_backed(&self) -> bool {
        matches!(self, Self::Hardware { .. })
    }
}

impl<H: HardwareRng> RandU64 for PlatformFast<H> {
    fn next_u64(&mut self) -> u64 {
        match self {
            Self::Hardware { hardware, fallback } => {
                let mut bytes = [0u8; 8];
                if hardware.try_fill(&mut bytes).is_ok() {
                    u64::from_le_bytes(bytes)
                } else {
                    fallback.next_u64()
                }
            }
            Self::Software(fallback) => fallback.next_u64(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csprng::CsRng;

    /// A mock hardware RNG yielding a fixed byte, optionally failing.
    struct MockHw {
        byte: u8,
        fails: bool,
    }

    impl HardwareRng for MockHw {
        fn try_fill(&self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.fails {
                return Err(EntropyError::Unavailable);
            }
            for b in out.iter_mut() {
                *b = self.byte;
            }
            Ok(())
        }
    }

    #[test]
    fn hardware_entropy_can_seed_the_csprng() {
        let hw = MockHw {
            byte: 0x5A,
            fails: false,
        };
        let mut rng = CsRng::new(HardwareEntropy::new(&hw)).expect("seed from hardware");
        // It produces output (the DRBG conditions the fixed bytes); we only
        // assert the plumbing works end to end.
        let mut out = [0u8; 32];
        rng.try_fill_bytes(&mut out).expect("draw");
    }

    #[test]
    fn hardware_entropy_propagates_failure() {
        let hw = MockHw {
            byte: 0,
            fails: true,
        };
        assert_eq!(
            CsRng::new(HardwareEntropy::new(&hw)).err(),
            Some(EntropyError::Unavailable)
        );
    }

    #[test]
    fn platform_fast_uses_hardware_when_present() {
        let hw = MockHw {
            byte: 0xCD,
            fails: false,
        };
        let mut fast = PlatformFast::new(Some(hw), 0);
        assert!(fast.is_hardware_backed());
        // Every byte is 0xCD => the u64 is all 0xCD.
        assert_eq!(fast.next_u64(), 0xCDCD_CDCD_CDCD_CDCD);
    }

    #[test]
    fn platform_fast_falls_back_to_software_when_no_hardware() {
        let mut fast = PlatformFast::<MockHw>::new(None, 0xABCD_EF01);
        assert!(!fast.is_hardware_backed());
        // Matches a bare FastRng with the same seed.
        let mut reference = FastRng::seed_from_u64(0xABCD_EF01);
        assert_eq!(fast.next_u64(), reference.next_u64());
    }

    #[test]
    fn platform_fast_falls_back_on_transient_hardware_failure() {
        let hw = MockHw {
            byte: 0,
            fails: true,
        };
        let mut fast = PlatformFast::new(Some(hw), 0x1234);
        // Hardware fails => the value must come from the software fallback,
        // matching a FastRng seeded identically.
        let mut reference = FastRng::seed_from_u64(0x1234);
        assert_eq!(fast.next_u64(), reference.next_u64());
    }
}
