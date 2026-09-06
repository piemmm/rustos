//! A fast, non-cryptographic generator (xoshiro256++).
//!
//! [`FastRng`] is the system's *fast* random source: ~5 ns per `u64`, far
//! cheaper than the HMAC-DRBG [`crate::CsRng`]. It is xoshiro256++
//! (Blackman & Vigna, 2018), a well-studied generator with a 2^256 period
//! that passes the full `BigCrush`/`PractRand` batteries. It is **not**
//! cryptographically secure — its state is recoverable from output — so it
//! must never key a cipher, generate a nonce, or seed ASLR; those uses go
//! through [`crate::CsRng`]. It is exactly the right tool for the
//! non-security randomness the OS needs in bulk: scheduler victim selection,
//! hashed-collection seeds, backoff jitter, test fuzzing.
//!
//! Constructing one is not hand-rolled cryptography: a
//! non-cryptographic PRNG is an ordinary algorithm, not a security primitive,
//! and rolling it ourselves avoids an external dependency (default).
//! The state is expanded from the seed with `SplitMix64`, the canonical
//! companion seeder, so a single `u64` seed still fills all 256 state bits
//! without zero-state pathologies.

use crate::rand::RandU64;

/// `SplitMix64` (Steele, Lea & Flood) — the canonical seeder for xoshiro.
///
/// Used only to expand a seed into well-mixed state words; never exposed as a
/// general generator because xoshiro256++ is the better choice for that.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A fast, non-cryptographic xoshiro256++ generator.
#[derive(Clone)]
pub struct FastRng {
    s: [u64; 4],
}

#[inline]
fn rotl(x: u64, k: u32) -> u64 {
    x.rotate_left(k)
}

impl FastRng {
    /// Seed a generator from a single `u64`, expanding it to the full 256-bit
    /// state with `SplitMix64`. Every seed — including `0` — yields a valid,
    /// non-degenerate state.
    ///
    /// `const` so a consumer can seed a generator held in a `static`.
    #[must_use]
    pub const fn seed_from_u64(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        Self {
            s: [sm.next(), sm.next(), sm.next(), sm.next()],
        }
    }

    /// Seed from a full 256-bit state. If every word is zero — the one state
    /// xoshiro cannot leave — it is replaced by the `SplitMix64` expansion of
    /// `0`, so the returned generator is always non-degenerate.
    #[must_use]
    pub fn from_state(state: [u64; 4]) -> Self {
        if state == [0; 4] {
            return Self::seed_from_u64(0);
        }
        Self { s: state }
    }

    /// Seed from 32 raw bytes, read as four little-endian state words
    /// (repairing the all-zero state as [`FastRng::from_state`] does).
    #[must_use]
    pub fn from_seed_bytes(bytes: &[u8; 32]) -> Self {
        Self::from_state(state_from_bytes(bytes))
    }

    /// Seed from any [`crate::EntropySource`], drawing 32 bytes of state.
    ///
    /// This is the "fast generator seeded securely" path: the per-call speed
    /// is xoshiro's, but the starting point is unpredictable.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::EntropyError`] if the source cannot supply 32
    /// bytes; no generator is constructed in that case.
    pub fn try_from_entropy(
        source: &mut dyn crate::EntropySource,
    ) -> Result<Self, crate::EntropyError> {
        let mut bytes = [0u8; 32];
        source.fill(&mut bytes)?;
        Ok(Self::from_seed_bytes(&bytes))
    }
}

/// Read 32 bytes as four little-endian `u64` state words.
fn state_from_bytes(bytes: &[u8; 32]) -> [u64; 4] {
    let mut state = [0u64; 4];
    for (word, chunk) in state.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*chunk);
    }
    state
}

impl RandU64 for FastRng {
    fn next_u64(&mut self) -> u64 {
        // xoshiro256++: result = rotl(s0 + s3, 23) + s0, then advance state.
        let result = rotl(self.s[0].wrapping_add(self.s[3]), 23).wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = rotl(self.s[3], 45);
        result
    }
}

impl core::fmt::Debug for FastRng {
    /// Non-cryptographic, but still elide the state — printing it would
    /// expose the stream to anyone reading a log.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FastRng").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entropy::{EntropyError, EntropySource};

    #[test]
    fn splitmix64_matches_reference_vector_for_seed_zero() {
        // Reference outputs of SplitMix64 seeded with 0 (Vigna's published
        // companion seeder); pins the seeder exactly.
        let mut sm = SplitMix64::new(0);
        assert_eq!(sm.next(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(sm.next(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(sm.next(), 0x06C4_5D18_8009_454F);
        assert_eq!(sm.next(), 0xF88B_B8A8_724C_81EC);
        assert_eq!(sm.next(), 0x1B39_896A_51A8_749B);
    }

    #[test]
    fn deterministic_for_a_given_seed_and_diverges_for_others() {
        let mut a = FastRng::seed_from_u64(0xDEAD_BEEF);
        let mut b = FastRng::seed_from_u64(0xDEAD_BEEF);
        let mut c = FastRng::seed_from_u64(0xDEAD_BEF0);
        for _ in 0..64 {
            let x = a.next_u64();
            assert_eq!(x, b.next_u64());
        }
        let mut differs = false;
        let mut a2 = FastRng::seed_from_u64(0xDEAD_BEEF);
        for _ in 0..64 {
            if a2.next_u64() != c.next_u64() {
                differs = true;
            }
        }
        assert!(differs, "different seeds must produce different streams");
    }

    #[test]
    fn all_zero_state_is_repaired() {
        // The forbidden all-zero state must not stick (it would emit only 0).
        let mut g = FastRng::from_state([0; 4]);
        let mut saw_nonzero = false;
        for _ in 0..8 {
            if g.next_u64() != 0 {
                saw_nonzero = true;
            }
        }
        assert!(saw_nonzero);
    }

    #[test]
    fn output_bytes_are_well_balanced() {
        // Deterministic seed => reproducible, never flaky. Over 1 MiB of
        // output the mean byte must sit near 127.5 and every bit position
        // must be set close to half the time.
        let mut g = FastRng::seed_from_u64(0x0123_4567_89AB_CDEF);
        let mut sum: u64 = 0;
        let mut bit_counts = [0u32; 8];
        let n = 1usize << 20;
        let mut buf = [0u8; 4096];
        let mut produced = 0;
        while produced < n {
            g.fill_bytes(&mut buf);
            for &byte in &buf {
                sum += u64::from(byte);
                for (bit, count) in bit_counts.iter_mut().enumerate() {
                    *count += u32::from((byte >> bit) & 1);
                }
            }
            produced += buf.len();
        }
        // Integer-only checks (no float casts): mean*10 must land within 1.0
        // of 127.5, i.e. in [1265, 1285].
        let produced = produced as u64;
        let mean_times_10 = sum * 10 / produced;
        assert!(
            (1265..=1285).contains(&mean_times_10),
            "mean*10 {mean_times_10} skewed"
        );
        let half = produced / 2;
        for count in bit_counts {
            // |count - half| / half < 0.01  <=>  |count - half| * 100 < half.
            let diff = u64::from(count).abs_diff(half);
            assert!(diff * 100 < half, "bit imbalance {diff} too high");
        }
    }

    struct FixedBytes(u8);
    impl EntropySource for FixedBytes {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            for b in out.iter_mut() {
                *b = self.0;
            }
            Ok(())
        }
    }

    #[test]
    fn seeding_from_entropy_uses_the_supplied_bytes() {
        let mut src = FixedBytes(0xAB);
        let g = FastRng::try_from_entropy(&mut src).expect("seed");
        // 0xABABABABABABABAB in every state word.
        assert_eq!(g.s, [0xABAB_ABAB_ABAB_ABAB; 4]);
    }

    #[test]
    fn seeding_from_a_failing_source_propagates_the_error() {
        struct Dead;
        impl EntropySource for Dead {
            fn fill(&mut self, _out: &mut [u8]) -> Result<(), EntropyError> {
                Err(EntropyError::Unavailable)
            }
        }
        assert_eq!(
            FastRng::try_from_entropy(&mut Dead).err(),
            Some(EntropyError::Unavailable)
        );
    }
}
