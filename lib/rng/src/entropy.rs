//! Entropy sources and the seam that lets the platform supply them.
//!
//! [`EntropySource`] is the single seam through which raw, full-entropy bytes
//! enter the CSPRNG ([`crate::CsRng`]). It is deliberately minimal so that any
//! number of platform sources — a motherboard hardware RNG (RDRAND/RDSEED,
//! virtio-rng; see [`crate::hardware`]), boot-time timing jitter, an
//! interrupt-arrival pool — can implement it without naming a concrete
//! architecture, keeping `lib/rng` architecture-neutral (:
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
    /// fail closed: no key, nonce, or seed is derived from a failed draw.
    Unavailable,
    /// A reseed was required to complete the draw but fresh entropy was only
    /// *momentarily* unavailable. This is the **transient** signal a
    /// non-blocking cryptographic draw returns instead of
    /// [`EntropyError::Unavailable`]: the existing generator state is intact,
    /// so the right response is to retry — or to reach for the blocking draw
    /// ([`crate::CsRng::fill_bytes_blocking`]), which waits through the
    /// reseed rather than failing. It is never produced by instantiation
    /// (a generator that cannot be seeded at all reports `Unavailable`).
    Reseeding,
}

/// A source of cryptographically usable raw entropy.
///
/// Implementations must fill `out` with bytes that, taken together, carry
/// close to `8 * out.len()` bits of entropy; the CSPRNG's HMAC conditioning
/// tolerates some shortfall, but a source that returns predictable bytes
/// violates the contract and must instead return [`EntropyError::Unavailable`].
pub trait EntropySource {
    /// Fill the whole of `out` with raw entropy, **without blocking**.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] if randomness cannot be
    /// produced right now. On error the contents of `out` are unspecified and
    /// must not be used.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError>;

    /// Fill the whole of `out` with raw entropy, **blocking** until enough is
    /// available.
    ///
    /// This is the seam through which a *blocking* cryptographic draw
    /// ([`crate::CsRng::fill_bytes_blocking`]) waits through a reseed. The
    /// default implementation simply delegates to [`EntropySource::fill`],
    /// which is correct for an always-ready source (e.g. a deterministic test
    /// source). A platform source whose entropy can be momentarily exhausted
    /// overrides this to **park the calling task** until its pool refills —
    /// it must wait, never busy-spin or retry-until-it-works.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] only for a *hard* failure — a
    /// source that is genuinely absent or broken, and so can never satisfy
    /// the request no matter how long the caller waits. A merely transient
    /// shortage is waited out, not reported.
    fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        self.fill(out)
    }
}

impl<T: EntropySource + ?Sized> EntropySource for &mut T {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        (**self).fill(out)
    }

    fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        (**self).fill_blocking(out)
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

/// Shared XOR-combine loop, parameterised by how each source is drawn so the
/// non-blocking [`EntropySource::fill`] and blocking
/// [`EntropySource::fill_blocking`] paths reuse one implementation (no
/// duplicated mixing algebra), and so both the borrowing [`CombinedSource`]
/// and the owning [`MixedPair`] share the single definition.
///
/// `draw_one` returns `true` if a source fully satisfied its chunked draw; a
/// source that fails is skipped (it contributes the XOR identity rather than
/// corrupting the pool). The result is [`EntropyError::Unavailable`] only when
/// *every* source failed.
fn combine_sources(
    out: &mut [u8],
    sources: &mut [&mut dyn EntropySource],
    mut draw_one: impl FnMut(&mut dyn EntropySource, &mut [u8]) -> bool,
) -> Result<(), EntropyError> {
    for byte in out.iter_mut() {
        *byte = 0;
    }
    let mut any = false;
    for source in sources.iter_mut() {
        // Draw this source's contribution in fixed-size chunks (no
        // allocator on the entropy path) and XOR it into `out`.
        let mut chunk = [0u8; 64];
        let mut offset = 0;
        let mut complete = true;
        while offset < out.len() {
            let take = core::cmp::min(chunk.len(), out.len() - offset);
            if !draw_one(&mut **source, &mut chunk[..take]) {
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

impl EntropySource for CombinedSource<'_, '_> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        combine_sources(out, self.sources, |source, chunk| {
            source.fill(chunk).is_ok()
        })
    }

    /// Blocks until at least one source can contribute.
    ///
    /// Each source is drawn through its own [`EntropySource::fill_blocking`],
    /// so a source whose pool is momentarily exhausted is waited out (parked)
    /// rather than skipped, while a genuinely dead source still returns
    /// `Unavailable` and is skipped. The combination only reports
    /// [`EntropyError::Unavailable`] when *every* source is hard-dead.
    fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        combine_sources(out, self.sources, |source, chunk| {
            source.fill_blocking(chunk).is_ok()
        })
    }
}

/// Two [`EntropySource`]s **owned** together and XOR-combined into one.
///
/// [`CombinedSource`] mixes sources it only *borrows*, which suits a one-shot
/// pool assembled on a stack frame. A long-lived generator that reseeds for
/// forward secrecy — the kernel's [`crate::OutputReserve`] — must instead
/// *own* its entropy source for the generator's whole life, and a borrowing
/// combiner cannot be that owned source. `MixedPair` fills that gap: it owns
/// both sources and applies the identical XOR-combine loop
/// (`combine_sources`), so a reseeding [`crate::CsRng`] can be seeded — and
/// re-seeded — from *both* a hardware RNG and an independent software source
/// without trusting either alone.
///
/// The mix succeeds as long as **either** source satisfied the draw; a source
/// that fails contributes the XOR identity (nothing) rather than corrupting
/// the pool, and only a draw where *both* sources fail reports
/// [`EntropyError::Unavailable`]. XOR is entropy-preserving for independent
/// inputs, so a stuck, degraded, or even adversarial `secondary` can never
/// lower the entropy a healthy `primary` contributes, and vice versa — the
/// charter's "no single source is trusted alone".
pub struct MixedPair<A: EntropySource, B: EntropySource> {
    primary: A,
    secondary: B,
}

impl<A: EntropySource, B: EntropySource> MixedPair<A, B> {
    /// Combine two owned entropy sources into one XOR-mixed source.
    pub fn new(primary: A, secondary: B) -> Self {
        Self { primary, secondary }
    }
}

impl<A: EntropySource, B: EntropySource> EntropySource for MixedPair<A, B> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        let mut sources: [&mut dyn EntropySource; 2] = [&mut self.primary, &mut self.secondary];
        combine_sources(out, &mut sources, |source, chunk| {
            source.fill(chunk).is_ok()
        })
    }

    fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        let mut sources: [&mut dyn EntropySource; 2] = [&mut self.primary, &mut self.secondary];
        combine_sources(out, &mut sources, |source, chunk| {
            source.fill_blocking(chunk).is_ok()
        })
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

    /// A source whose non-blocking `fill` is exhausted for the first
    /// `blocked_draws` calls but whose `fill_blocking` always succeeds — a
    /// stand-in for a pool that a parking source would wait on. It records how
    /// many times the blocking path actually had to wait.
    struct Parking {
        blocked_draws: u32,
        waits: u32,
    }

    impl EntropySource for Parking {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.blocked_draws > 0 {
                self.blocked_draws -= 1;
                return Err(EntropyError::Unavailable);
            }
            for byte in out.iter_mut() {
                *byte = 0x3C;
            }
            Ok(())
        }

        fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.blocked_draws > 0 {
                self.waits += 1;
                self.blocked_draws = 0;
            }
            for byte in out.iter_mut() {
                *byte = 0x3C;
            }
            Ok(())
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

    #[test]
    fn default_fill_blocking_delegates_to_fill() {
        // `Counter` does not override `fill_blocking`, so the default must
        // produce exactly what `fill` produces.
        let mut got = Counter {
            state: 0x10,
            step: 5,
        };
        let mut want = Counter {
            state: 0x10,
            step: 5,
        };
        let (mut a, mut b) = ([0u8; 24], [0u8; 24]);
        got.fill_blocking(&mut a).unwrap();
        want.fill(&mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn blocking_waits_through_a_transient_shortage() {
        // `fill` is exhausted once, so the non-blocking path fails closed…
        let mut p = Parking {
            blocked_draws: 1,
            waits: 0,
        };
        let mut out = [0u8; 16];
        assert_eq!(p.fill(&mut out), Err(EntropyError::Unavailable));
        // …but `fill_blocking` waits the shortage out and then succeeds.
        let mut p = Parking {
            blocked_draws: 1,
            waits: 0,
        };
        p.fill_blocking(&mut out)
            .expect("blocking draw waits, succeeds");
        assert_eq!(p.waits, 1, "the blocking path had to wait exactly once");
        assert_eq!(out, [0x3C; 16]);
    }

    #[test]
    fn combined_blocking_waits_out_a_transient_source() {
        // One transient source (exhausted once) plus a dead one: the
        // non-blocking combine fails, the blocking combine waits and succeeds.
        let mut transient = Parking {
            blocked_draws: 1,
            waits: 0,
        };
        let mut dead = Dead;
        let mut srcs: [&mut dyn EntropySource; 2] = [&mut transient, &mut dead];
        let mut combined = CombinedSource::new(&mut srcs);
        let mut out = [0u8; 16];
        assert_eq!(combined.fill(&mut out), Err(EntropyError::Unavailable));

        let mut transient = Parking {
            blocked_draws: 1,
            waits: 0,
        };
        let mut dead = Dead;
        let mut srcs: [&mut dyn EntropySource; 2] = [&mut transient, &mut dead];
        let mut combined = CombinedSource::new(&mut srcs);
        combined
            .fill_blocking(&mut out)
            .expect("blocking combine waits out the transient source");
        assert_eq!(out, [0x3C; 16], "only the transient source contributed");
    }

    #[test]
    fn combined_blocking_fails_only_when_every_source_is_hard_dead() {
        let mut d1 = Dead;
        let mut d2 = Dead;
        let mut srcs: [&mut dyn EntropySource; 2] = [&mut d1, &mut d2];
        let mut combined = CombinedSource::new(&mut srcs);
        let mut out = [0u8; 16];
        assert_eq!(
            combined.fill_blocking(&mut out),
            Err(EntropyError::Unavailable)
        );
    }

    #[test]
    fn mixed_pair_is_the_xor_of_its_owned_sources() {
        let a = Counter {
            state: 0x10,
            step: 1,
        };
        let b = Counter {
            state: 0xA0,
            step: 3,
        };
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

        let mut mixed = MixedPair::new(a, b);
        let mut out = [0u8; 70];
        mixed.fill(&mut out).unwrap();
        for i in 0..70 {
            assert_eq!(out[i], sa[i] ^ sb[i]);
        }
    }

    #[test]
    fn mixed_pair_survives_a_dead_secondary() {
        // A healthy hardware-like primary plus a dead secondary still yields
        // the primary's full contribution — the "no single source trusted
        // alone" mix must not fail when one side is absent.
        let mut expected = Counter {
            state: 0x55,
            step: 7,
        };
        let mut exp = [0u8; 40];
        expected.fill(&mut exp).unwrap();

        let mut mixed = MixedPair::new(
            Counter {
                state: 0x55,
                step: 7,
            },
            Dead,
        );
        let mut out = [0u8; 40];
        mixed.fill(&mut out).unwrap();
        assert_eq!(out, exp, "a dead secondary contributes the XOR identity");
    }

    #[test]
    fn mixed_pair_fails_closed_only_when_both_sources_die() {
        let mut mixed = MixedPair::new(Dead, Dead);
        let mut out = [0u8; 16];
        assert_eq!(mixed.fill(&mut out), Err(EntropyError::Unavailable));
    }

    #[test]
    fn mixed_pair_blocking_waits_out_a_transient_source() {
        // Primary momentarily exhausted, secondary dead: the non-blocking mix
        // fails closed, the blocking mix waits the primary out and succeeds.
        let mut mixed = MixedPair::new(
            Parking {
                blocked_draws: 1,
                waits: 0,
            },
            Dead,
        );
        let mut out = [0u8; 16];
        assert_eq!(mixed.fill(&mut out), Err(EntropyError::Unavailable));

        let mut mixed = MixedPair::new(
            Parking {
                blocked_draws: 1,
                waits: 0,
            },
            Dead,
        );
        mixed
            .fill_blocking(&mut out)
            .expect("blocking mix waits out the transient primary");
        assert_eq!(out, [0x3C; 16], "only the transient primary contributed");
    }
}
