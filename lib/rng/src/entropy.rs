//! Entropy sources and the seam that lets the platform supply them.
//!
//! [`EntropySource`] is the single seam through which raw, full-entropy bytes
//! enter the CSPRNG ([`crate::CsRng`]). It is deliberately minimal so that any
//! number of platform sources — a motherboard hardware RNG (RDRAND/RDSEED,
//! virtio-rng; see [`crate::hardware`]), boot-time timing jitter, an
//! interrupt-arrival pool — can implement it without naming a concrete
//! architecture, keeping `lib/rng` architecture-neutral (`AGENTS.md` §17.2:
//! target-conditional probing stays in `kernel/arch/<target>`).
//!
//! [`CombinedSource`] mixes several sources into one so the system never
//! trusts a single source: the issue's "in addition to any existing hardware
//! sources we can use for additional entropy". It XORs the sources' streams,
//! which is the standard robust combiner — if *any* input is independent and
//! full-entropy, the result is, so a backdoored or stuck source cannot lower
//! the quality of the pool.

use zeroize::Zeroize;

/// The reason an entropy draw could not be satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntropyError {
    /// No randomness could be produced (the device is absent, not yet
    /// initialised, or — for a hardware RNG — failed every retry). Callers
    /// fail closed: no key, nonce, or seed is derived from a failed draw
    /// (`AGENTS.md` §5.4).
    Unavailable,
}

/// A source of cryptographically usable raw entropy.
///
/// Implementations must fill `out` with bytes that, taken together, carry
/// close to `8 * out.len()` bits of entropy; the CSPRNG's HMAC conditioning
/// tolerates some shortfall, but a source that returns predictable bytes
/// violates the contract and must instead return [`EntropyError::Unavailable`].
pub trait EntropySource {
    /// Fill the whole of `out` with raw entropy.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] if randomness cannot be
    /// produced. On error the contents of `out` are unspecified and must not
    /// be used.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError>;
}

impl<T: EntropySource + ?Sized> EntropySource for &mut T {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        (**self).fill(out)
    }
}

/// Combines several [`EntropySource`]s into one by XOR-combining their
/// streams.
///
/// The combined fill succeeds as long as **at least one** wrapped source
/// fully satisfied the request; sources that fail are skipped (they
/// contribute nothing rather than corrupting the pool). Only if *every*
/// source fails does the combination report [`EntropyError::Unavailable`].
///
/// XOR is entropy-preserving for independent inputs, so adding a weak or
/// even adversarial source can never reduce the entropy contributed by a
/// good one — the rationale for mixing a motherboard hardware RNG with the
/// other platform sources rather than trusting it alone.
pub struct CombinedSource<'a, 'b> {
    sources: &'a mut [&'b mut dyn EntropySource],
}

impl<'a, 'b> CombinedSource<'a, 'b> {
    /// Wrap a slice of sources to be XOR-combined.
    #[must_use]
    pub fn new(sources: &'a mut [&'b mut dyn EntropySource]) -> Self {
        Self { sources }
    }
}

impl EntropySource for CombinedSource<'_, '_> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        for byte in out.iter_mut() {
            *byte = 0;
        }
        let mut any = false;
        for source in self.sources.iter_mut() {
            // Draw this source's contribution in fixed-size chunks (no
            // allocator on the entropy path) and XOR it into `out`.
            let mut chunk = [0u8; 64];
            let mut offset = 0;
            let mut complete = true;
            while offset < out.len() {
                let take = core::cmp::min(chunk.len(), out.len() - offset);
                if source.fill(&mut chunk[..take]).is_err() {
                    complete = false;
                    break;
                }
                for (dst, src) in out[offset..offset + take].iter_mut().zip(&chunk[..take]) {
                    *dst ^= *src;
                }
                offset += take;
            }
            chunk.zeroize();
            any |= complete;
        }
        if any {
            Ok(())
        } else {
            Err(EntropyError::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A counter-based deterministic source for tests: not entropy, but it
    /// lets us assert the plumbing (combination, XOR, failure handling).
    struct Counter {
        state: u8,
        step: u8,
    }

    impl EntropySource for Counter {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            for byte in out.iter_mut() {
                *byte = self.state;
                self.state = self.state.wrapping_add(self.step);
            }
            Ok(())
        }
    }

    /// A source that always fails.
    struct Dead;
    impl EntropySource for Dead {
        fn fill(&mut self, _out: &mut [u8]) -> Result<(), EntropyError> {
            Err(EntropyError::Unavailable)
        }
    }

    #[test]
    fn combined_is_the_xor_of_its_sources() {
        let mut a = Counter {
            state: 0x10,
            step: 1,
        };
        let mut b = Counter {
            state: 0xA0,
            step: 3,
        };
        // Expected XOR computed from independent copies of the same streams.
        let mut ea = Counter {
            state: 0x10,
            step: 1,
        };
        let mut eb = Counter {
            state: 0xA0,
            step: 3,
        };
        let (mut sa, mut sb) = ([0u8; 70], [0u8; 70]);
        ea.fill(&mut sa).unwrap();
        eb.fill(&mut sb).unwrap();

        let mut srcs: [&mut dyn EntropySource; 2] = [&mut a, &mut b];
        let mut combined = CombinedSource::new(&mut srcs);
        let mut out = [0u8; 70];
        combined.fill(&mut out).unwrap();

        for i in 0..70 {
            assert_eq!(out[i], sa[i] ^ sb[i]);
        }
    }

    #[test]
    fn combined_skips_failing_sources_but_uses_the_survivor() {
        let mut good = Counter {
            state: 0x55,
            step: 7,
        };
        let mut dead = Dead;
        let mut expected = Counter {
            state: 0x55,
            step: 7,
        };
        let mut exp = [0u8; 40];
        expected.fill(&mut exp).unwrap();

        let mut srcs: [&mut dyn EntropySource; 2] = [&mut dead, &mut good];
        let mut combined = CombinedSource::new(&mut srcs);
        let mut out = [0u8; 40];
        combined.fill(&mut out).unwrap();
        assert_eq!(out, exp, "a dead source must contribute zero (identity)");
    }

    #[test]
    fn combined_fails_only_when_every_source_fails() {
        let mut d1 = Dead;
        let mut d2 = Dead;
        let mut srcs: [&mut dyn EntropySource; 2] = [&mut d1, &mut d2];
        let mut combined = CombinedSource::new(&mut srcs);
        let mut out = [0u8; 16];
        assert_eq!(combined.fill(&mut out), Err(EntropyError::Unavailable));
    }
}
