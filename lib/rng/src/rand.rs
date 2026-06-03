//! The common interface for an infallible `u64` generator.
//!
//! [`RandU64`] is implemented by the *infallible* generators in this crate —
//! the fast non-cryptographic [`crate::FastRng`] and the
//! hardware-with-fallback [`crate::hardware::PlatformFast`] — and provides the
//! shared, generator-independent sampling logic (byte filling and unbiased
//! bounded integers) once, so no consumer re-derives it (`AGENTS.md` §2.2).
//!
//! The cryptographic [`crate::CsRng`] deliberately does **not** implement this
//! trait: its draws can fail (a reseed may need entropy that is momentarily
//! unavailable) and must surface that as a `Result`, never paper over it
//! (`AGENTS.md` §2.9). It therefore exposes its own fallible API.

/// An infallible source of uniformly distributed `u64` values.
///
/// Implementors supply [`RandU64::next_u64`]; the rest is derived. None of
/// the provided methods allocate or panic.
pub trait RandU64 {
    /// Return the next uniformly distributed `u64`.
    fn next_u64(&mut self) -> u64;

    /// Return the next uniformly distributed `u32` (the high 32 bits of a
    /// fresh `u64`, which are the best-mixed for every generator here).
    fn next_u32(&mut self) -> u32 {
        // `>> 32` leaves a value in `0..2^32`; the `as u32` is exact, never
        // a truncation (the pedantic lint cannot prove the range).
        #[allow(clippy::cast_possible_truncation)]
        {
            (self.next_u64() >> 32) as u32
        }
    }

    /// Fill `out` with uniformly distributed bytes.
    fn fill_bytes(&mut self, out: &mut [u8]) {
        let mut chunks = out.chunks_exact_mut(8);
        for chunk in &mut chunks {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        let tail = chunks.into_remainder();
        if !tail.is_empty() {
            let bytes = self.next_u64().to_le_bytes();
            tail.copy_from_slice(&bytes[..tail.len()]);
        }
    }

    /// Return a uniformly distributed value in `0..bound`, free of modulo
    /// bias, using Lemire's nearly-divisionless rejection method. Returns `0`
    /// when `bound == 0` (an empty range has no element; the documented,
    /// panic-free convention, `AGENTS.md` §2.9).
    fn next_below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        // `m as u64` deliberately keeps the low 64 bits of the 128-bit
        // product (the rejection test); `(m >> 64) as u64` keeps the high 64
        // bits, which is exact because `m < 2^64 * bound <= 2^128`. Both
        // truncations are the algorithm, not a bug (Lemire).
        #[allow(clippy::cast_possible_truncation)]
        {
            let mut x = self.next_u64();
            let mut m = u128::from(x) * u128::from(bound);
            let mut low = m as u64;
            if low < bound {
                let threshold = bound.wrapping_neg() % bound;
                while low < threshold {
                    x = self.next_u64();
                    m = u128::from(x) * u128::from(bound);
                    low = m as u64;
                }
            }
            (m >> 64) as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RandU64;

    /// A controllable generator: yields a fixed script of `u64`s, then a
    /// trivial counter. Lets the provided-method tests pin exact behaviour.
    struct Scripted<'a> {
        script: &'a [u64],
        pos: usize,
        counter: u64,
    }

    impl RandU64 for Scripted<'_> {
        fn next_u64(&mut self) -> u64 {
            if self.pos < self.script.len() {
                let v = self.script[self.pos];
                self.pos += 1;
                v
            } else {
                self.counter = self.counter.wrapping_add(0x9E37_79B9_7F4A_7C15);
                self.counter
            }
        }
    }

    fn scripted(script: &[u64]) -> Scripted<'_> {
        Scripted {
            script,
            pos: 0,
            counter: 0,
        }
    }

    #[test]
    fn next_u32_is_the_high_half() {
        let mut g = scripted(&[0x0123_4567_89AB_CDEF]);
        assert_eq!(g.next_u32(), 0x0123_4567);
    }

    #[test]
    fn fill_bytes_handles_partial_tail() {
        let mut g = scripted(&[0x0807_0605_0403_0201, 0x100F_0E0D_0C0B_0A09]);
        let mut out = [0u8; 12];
        g.fill_bytes(&mut out);
        assert_eq!(
            out,
            [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C]
        );
    }

    #[test]
    fn next_below_zero_bound_is_zero() {
        let mut g = scripted(&[]);
        assert_eq!(g.next_below(0), 0);
    }

    #[test]
    fn next_below_stays_in_range() {
        let mut g = scripted(&[]);
        for bound in [1u64, 2, 3, 7, 10, 1000, u64::MAX] {
            for _ in 0..256 {
                assert!(g.next_below(bound) < bound);
            }
        }
    }

    #[test]
    fn next_below_rejects_the_biased_zone() {
        // With bound = 3, threshold = (2^64 mod 3) = 1, so a draw whose low
        // 64 bits of the widened product are 0 is rejected and the next draw
        // used. Script a rejected value (low == 0) followed by an accepted
        // one, and check the accepted draw's high-word result is returned.
        let bound = 3u64;
        let rejected = 0u64; // 0 * 3 = 0 -> low word 0 (< threshold 1)
        let accepted = u64::MAX; // (2^64-1)*3 -> high word 2
        let script = [rejected, accepted];
        let mut g = scripted(&script);
        assert_eq!(g.next_below(bound), 2);
    }

    #[test]
    fn next_below_is_roughly_uniform() {
        // Deterministic generator (counter) feeding the sampler: count how
        // often each of 6 buckets is hit over many draws; every bucket must
        // be populated and none wildly off. Deterministic => never flaky.
        let mut g = scripted(&[]);
        let mut counts = [0u32; 6];
        for _ in 0..60_000 {
            counts[usize::try_from(g.next_below(6)).unwrap()] += 1;
        }
        for c in counts {
            assert!(c > 8_000 && c < 12_000, "bucket count {c} out of band");
        }
    }
}
