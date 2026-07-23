//! Boot-provided seed entropy source (firmware/bootloader `rng-seed`).
//!
//! Firmware and boot loaders hand the kernel a block of random seed material
//! at hand-off — the device tree's `/chosen/rng-seed` on an FDT platform, the
//! UEFI RNG protocol's output on a firmware platform. It is one of the
//! entropy sources the charter enumerates ("bootloader seed material"), and
//! on an emulated or virtualised machine it is frequently the *only* one: a
//! guest CPU exposes no on-die hardware RNG and its cycle counter advances
//! deterministically under a translating emulator, so both the hardware-RNG
//! ([`crate::HardwareEntropy`]) and the CPU-timing-jitter ([`crate::JitterSource`])
//! sources fail closed there. Feeding the boot seed into the mix lets the
//! kernel CSPRNG seed on exactly those machines — QEMU's `virt` board, for
//! one, always publishes `/chosen/rng-seed` — instead of leaving the reserve
//! forever unseeded.
//!
//! # It never lowers the bar, and is never trusted alone
//!
//! Like every other source, the boot seed is XOR-mixed with the hardware and
//! software sources through [`crate::MixedPair`] before it reaches
//! [`crate::CsRng`]; XOR is entropy-preserving for independent inputs, so a
//! weak or observed boot seed can never reduce the entropy a healthy hardware
//! RNG contributes on a machine that has one. The seed is *input material*,
//! not final output — the DRBG conditions it before any caller sees a byte.
//!
//! # Consume-once, then wiped
//!
//! The seed carries a fixed, finite amount of entropy, so it is used exactly
//! **once**: the first [`EntropySource::fill`] expands it (SHA-256 in counter
//! mode, over `lib/crypto`, never a hand-rolled mixer) to satisfy the initial
//! CSPRNG instantiation, then the retained copy is zeroised and every later
//! draw fails closed with [`EntropyError::Unavailable`]. Under the mix that
//! simply means the boot seed contributes the XOR identity (nothing) to every
//! subsequent reseed — reseeds draw their fresh entropy from the
//! interrupt-timing pool, exactly as a general-purpose kernel wipes the boot
//! `rng-seed` after folding it in once. The copy is also zeroised on drop.

use zeroize::Zeroize;

use tairix_crypto::{sha256, SHA256_OUTPUT_LEN};

use crate::entropy::{EntropyError, EntropySource};

/// Maximum boot-seed length retained, in bytes.
///
/// Sized for the common firmware seeds (QEMU `virt` publishes 32 bytes;
/// 64 bytes covers a full 512-bit seed) — well past the 256 bits the DRBG
/// needs, so a longer seed is truncated rather than growing the buffer.
pub const MAX_BOOT_SEED_LEN: usize = 64;

/// Domain-separation label folded into the expansion so this source's output
/// cannot collide with another SHA-256 use of the same bytes.
const DOMAIN: &[u8; 8] = b"ROSBSEED";

/// A boot-provided-seed [`EntropySource`].
///
/// Construct it from the firmware seed bytes with [`BootSeedSource::new`], or
/// [`BootSeedSource::empty`] when the platform provided none (it then always
/// fails closed and contributes nothing to the mix). See the [module
/// docs](self) for the consume-once discipline.
pub struct BootSeedSource {
    /// The retained seed bytes (`seed[..len]`), zeroised once spent.
    seed: [u8; MAX_BOOT_SEED_LEN],
    /// Valid seed length in `seed`.
    len: usize,
    /// Whether the one-shot seed has already been consumed.
    spent: bool,
}

impl BootSeedSource {
    /// Retain up to [`MAX_BOOT_SEED_LEN`] bytes of firmware seed material.
    ///
    /// A seed longer than the buffer is truncated (the prefix already carries
    /// far more than the DRBG needs); an empty slice yields a source that
    /// behaves exactly like [`BootSeedSource::empty`].
    #[must_use]
    pub fn new(seed: &[u8]) -> Self {
        let mut buf = [0u8; MAX_BOOT_SEED_LEN];
        let len = seed.len().min(MAX_BOOT_SEED_LEN);
        buf[..len].copy_from_slice(&seed[..len]);
        Self {
            seed: buf,
            len,
            spent: false,
        }
    }

    /// A source with no seed: every draw fails closed and it contributes the
    /// XOR identity (nothing) to the mix.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            seed: [0u8; MAX_BOOT_SEED_LEN],
            len: 0,
            spent: false,
        }
    }

    /// Whether a non-empty seed is present and not yet consumed.
    #[must_use]
    pub const fn has_seed(&self) -> bool {
        self.len != 0 && !self.spent
    }
}

impl EntropySource for BootSeedSource {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        if out.is_empty() {
            return Ok(());
        }
        if self.spent || self.len == 0 {
            // Consume-once: after the initial seed the boot material is gone,
            // so it contributes nothing to later reseeds (fail closed).
            return Err(EntropyError::Unavailable);
        }

        // Expand the seed to the requested length with SHA-256 in counter
        // mode: block_i = sha256(DOMAIN || seed || i_le). The full seed is
        // never emitted directly; only its conditioned hash reaches `out`.
        let mut buf = [0u8; DOMAIN.len() + MAX_BOOT_SEED_LEN + 8];
        buf[..DOMAIN.len()].copy_from_slice(DOMAIN);
        buf[DOMAIN.len()..DOMAIN.len() + self.len].copy_from_slice(&self.seed[..self.len]);
        let counter_at = DOMAIN.len() + self.len;

        let mut produced = 0usize;
        let mut counter: u64 = 0;
        while produced < out.len() {
            buf[counter_at..counter_at + 8].copy_from_slice(&counter.to_le_bytes());
            let block = sha256(&buf[..counter_at + 8]);
            let take = core::cmp::min(SHA256_OUTPUT_LEN, out.len() - produced);
            out[produced..produced + take].copy_from_slice(&block[..take]);
            produced += take;
            counter += 1;
        }

        buf.zeroize();
        // The seed is spent: wipe the retained copy so it can neither be
        // reused nor recovered from kernel memory.
        self.seed.zeroize();
        self.len = 0;
        self.spent = true;
        Ok(())
    }
    // `fill_blocking` uses the default (delegates to `fill`): the boot seed is
    // an in-memory value with nothing to wait on — it is either present (and
    // consumed) now or it is not.
}

impl Drop for BootSeedSource {
    fn drop(&mut self) {
        self.seed.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_fails_closed_and_has_no_seed() {
        let mut src = BootSeedSource::empty();
        assert!(!src.has_seed());
        let mut out = [0u8; 16];
        assert_eq!(src.fill(&mut out), Err(EntropyError::Unavailable));
        assert_eq!(out, [0u8; 16], "output untouched on a failed draw");
    }

    #[test]
    fn a_seed_fills_and_is_not_the_raw_bytes() {
        let seed = [0x11u8; 32];
        let mut src = BootSeedSource::new(&seed);
        assert!(src.has_seed());
        let mut out = [0u8; 48];
        src.fill(&mut out).expect("a present seed fills");
        assert_ne!(
            &out[..32],
            &seed[..],
            "output is conditioned, not the raw seed"
        );
        assert_ne!(out, [0u8; 48], "conditioned output is non-zero");
    }

    #[test]
    fn output_is_deterministic_for_a_given_seed() {
        let seed = [0xABu8; 24];
        let mut a = BootSeedSource::new(&seed);
        let mut b = BootSeedSource::new(&seed);
        let (mut oa, mut ob) = ([0u8; 40], [0u8; 40]);
        a.fill(&mut oa).unwrap();
        b.fill(&mut ob).unwrap();
        assert_eq!(oa, ob, "same seed ⇒ same expansion");
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = BootSeedSource::new(&[1u8; 32]);
        let mut b = BootSeedSource::new(&[2u8; 32]);
        let (mut oa, mut ob) = ([0u8; 32], [0u8; 32]);
        a.fill(&mut oa).unwrap();
        b.fill(&mut ob).unwrap();
        assert_ne!(oa, ob, "distinct seeds must produce distinct output");
    }

    #[test]
    fn output_spans_multiple_sha256_blocks() {
        // 70 bytes crosses three 32-byte counter blocks; prove the whole
        // buffer is filled and the blocks differ (a fixed block would repeat).
        let mut src = BootSeedSource::new(&[0x5Au8; 16]);
        let mut out = [0u8; 70];
        src.fill(&mut out).unwrap();
        assert_ne!(&out[0..32], &out[32..64], "counter blocks must differ");
        assert_ne!(&out[64..70], &[0u8; 6], "tail block filled");
    }

    #[test]
    fn is_consumed_once_then_fails_closed() {
        let mut src = BootSeedSource::new(&[0x33u8; 32]);
        let mut out = [0u8; 32];
        src.fill(&mut out).expect("first draw succeeds");
        assert!(!src.has_seed(), "seed is spent after one draw");
        let mut again = [0u8; 32];
        assert_eq!(
            src.fill(&mut again),
            Err(EntropyError::Unavailable),
            "a spent boot seed contributes nothing to later reseeds"
        );
        assert_eq!(again, [0u8; 32]);
    }

    #[test]
    fn a_long_seed_is_truncated_to_the_buffer() {
        // A seed longer than the buffer must not panic; the prefix is used.
        let long = [0x77u8; MAX_BOOT_SEED_LEN + 40];
        let mut src = BootSeedSource::new(&long);
        let mut out = [0u8; 32];
        src.fill(&mut out).expect("an over-long seed still fills");
        assert_ne!(out, [0u8; 32]);
    }

    #[test]
    fn empty_request_is_ok_and_does_not_spend_the_seed() {
        let mut src = BootSeedSource::new(&[9u8; 32]);
        let mut out = [0u8; 0];
        assert_eq!(src.fill(&mut out), Ok(()));
        assert!(src.has_seed(), "an empty request must not consume the seed");
    }
}
