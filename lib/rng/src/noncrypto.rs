//! A statistically excellent but **predictable** generator (xoshiro256++).
//!
//! [`NonCryptoRng`] is named for the property that matters at a call site:
//! its output is *not* unpredictable. Four consecutive draws carry the whole
//! 256-bit state, and recovering it is arithmetic rather than cryptanalysis,
//! so an observer who sees a handful of outputs can reproduce every past and
//! future one. Statistical quality and unpredictability are unrelated
//! properties: xoshiro256++ (Blackman & Vigna, 2018) passes the full
//! `BigCrush`/`PractRand` batteries and is still trivially invertible.
//!
//! Reach for it where decorrelation or reproducibility is the goal and an
//! observer predicting the stream costs nothing — spreading per-CPU
//! work-stealing scans so idle CPUs do not convoy on one queue lock, and
//! seeded test fixtures. Anything that should be unpredictable takes
//! [`crate::FastRng`]; long-lived key material takes [`crate::CsRng`].
//!
//! A non-cryptographic PRNG is an ordinary algorithm rather than a security
//! primitive, so implementing it here is not hand-rolled cryptography and
//! avoids a dependency. The state is expanded from the seed with
//! `SplitMix64`, the canonical companion seeder, so a single `u64` fills all
//! 256 state bits without a zero-state pathology.

use crate::rand::RandU64;

/// `SplitMix64` (Steele, Lea & Flood) — the canonical seeder.
///
/// Used only to expand a `u64` seed into well-mixed words: xoshiro's four
/// state words here, the cipher key of [`crate::FastRng::seed_from_u64`]
/// there. Crate-internal and never a generator in its own right — the two
/// public types are the generators, and one definition of the expansion
/// keeps them from drifting.
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) const fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A fast, statistically excellent, **predictable** xoshiro256++ generator.
///
/// See the module docs for what that rules out.
pub struct NonCryptoRng {
    s: [u64; 4],
}

#[inline]
fn rotl(x: u64, k: u32) -> u64 {
    x.rotate_left(k)
}

impl NonCryptoRng {
    /// Seed a generator from a single `u64`, expanding it to the full 256-bit
    /// state with `SplitMix64`.
    ///
    /// Every seed — `0` included — yields a non-degenerate state: the four
    /// words are distinct `SplitMix64` outputs, and the all-zero state
    /// xoshiro cannot leave is not reachable from any seed. Adjacent seeds
    /// give unrelated streams, because `SplitMix64` avalanches, so a caller
    /// wanting one independent stream per CPU can simply seed with the CPU
    /// index.
    ///
    /// `const` so a consumer can seed a generator held in a `static`.
    #[must_use]
    pub const fn seed_from_u64(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        Self {
            s: [sm.next(), sm.next(), sm.next(), sm.next()],
        }
    }
}

impl RandU64 for NonCryptoRng {
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

impl core::fmt::Debug for NonCryptoRng {
    /// Elide the state: printing it hands the stream to anyone reading a log,
    /// and the point of this type is that the stream is easy to continue.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NonCryptoRng").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{NonCryptoRng, SplitMix64};
    use crate::rand::RandU64;

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
        let mut a = NonCryptoRng::seed_from_u64(0xDEAD_BEEF);
        let mut b = NonCryptoRng::seed_from_u64(0xDEAD_BEEF);
        let mut c = NonCryptoRng::seed_from_u64(0xDEAD_BEF0);
        for _ in 0..64 {
            let x = a.next_u64();
            assert_eq!(x, b.next_u64());
        }
        let mut differs = false;
        let mut a2 = NonCryptoRng::seed_from_u64(0xDEAD_BEEF);
        for _ in 0..64 {
            if a2.next_u64() != c.next_u64() {
                differs = true;
            }
        }
        assert!(differs, "different seeds must produce different streams");
    }

    /// Adjacent seeds are exactly what a per-CPU consumer hands in (the CPU
    /// index), so their streams must not run in lockstep or overlap.
    #[test]
    fn adjacent_seeds_give_unrelated_streams() {
        let mut streams = [[0u64; 64]; 8];
        for (seed, stream) in (0u64..).zip(streams.iter_mut()) {
            let mut g = NonCryptoRng::seed_from_u64(seed);
            for slot in stream.iter_mut() {
                *slot = g.next_u64();
            }
        }
        for (i, a) in streams.iter().enumerate() {
            for b in &streams[i + 1..] {
                assert_ne!(a, b, "two adjacent seeds produced the same stream");
                assert!(
                    a.iter().zip(b).filter(|(x, y)| x == y).count() < 4,
                    "two streams shared too many draws to be independent"
                );
            }
        }
    }

    /// The all-zero state is the one xoshiro cannot leave, and no seed may
    /// reach it.
    #[test]
    fn no_seed_yields_the_degenerate_all_zero_state() {
        for seed in [0u64, 1, u64::MAX, 0x9E37_79B9_7F4A_7C15] {
            let g = NonCryptoRng::seed_from_u64(seed);
            assert_ne!(g.s, [0; 4]);
        }
    }

    #[test]
    fn output_bytes_are_well_balanced() {
        // Deterministic seed => reproducible, never flaky. Over 1 MiB of
        // output the mean byte must sit near 127.5 and every bit position
        // must be set close to half the time.
        let mut g = NonCryptoRng::seed_from_u64(0x0123_4567_89AB_CDEF);
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

    #[test]
    fn debug_does_not_leak_state() {
        extern crate alloc;
        use alloc::format;
        let printed = format!("{:?}", NonCryptoRng::seed_from_u64(1));
        assert!(printed.contains("NonCryptoRng"));
        assert!(!printed.contains("s:"));
    }
}
