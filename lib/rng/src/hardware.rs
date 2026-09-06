//! The motherboard hardware-RNG seam.
//!
//! Many platforms expose a hardware random source: an on-die DRBG/entropy
//! instruction (x86 `RDRAND`/`RDSEED`, ARMv8.5 `RNDR`/`RNDRRS`, RISC-V
//! `Zkr`), a TPM, or a virtio-rng device. [`HardwareRng`] is the seam through
//! which such a source is offered to `lib/rng`. The *concrete* driver is
//! architecture- or device-specific and therefore lives in
//! `kernel/arch/<target>` or a `drivers/*` crate (no target-conditional code
//! in a shared crate); this crate only consumes the trait, so it stays
//! architecture-neutral and host-testable.
//!
//! **A hardware source is entropy *input*, never final output.** Wrapped in
//! [`HardwareEntropy`] it becomes an ordinary [`EntropySource`], so
//! [`crate::CombinedSource`] can XOR-mix it *alongside* the other platform
//! sources before it feeds [`crate::CsRng`]. It is never trusted as the sole
//! source — a vendor RNG could be weak or backdoored — and its bytes never
//! reach a caller unconditioned. A generator wanting speed takes
//! [`crate::FastRng`], whose output is conditioned by a cipher and costs less
//! than a hardware instruction anyway.

use crate::entropy::{EntropyError, EntropySource};

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

#[cfg(test)]
mod tests {
    use super::{EntropyError, HardwareEntropy, HardwareRng};
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
}
