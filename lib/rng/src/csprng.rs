//! The cryptographically secure RNG ([`CsRng`]).
//!
//! [`CsRng`] is the generator the rest of RustOS reaches for whenever the
//! randomness must be unpredictable: `RustFS` volume and per-record keys, the
//! encrypted-swap key (`AGENTS.md` §4), nonces, the KASLR/ASLR seed (§19.2),
//! capability-token material. It pairs an [`HmacDrbg`] with an
//! [`EntropySource`] and reseeds from that source on a fixed schedule, so a
//! one-time state compromise cannot predict output indefinitely (forward
//! secrecy / prediction resistance).
//!
//! # Why the API is fallible
//!
//! Every draw can trigger a reseed, and a reseed needs fresh entropy that may
//! be momentarily unavailable. Rather than block, spin, or panic (`AGENTS.md`
//! §2.1, §2.9), [`CsRng`] surfaces that as [`EntropyError`] and the caller
//! fails closed (§5.4). Reaching for randomness is therefore an explicit,
//! checked operation, never a silent one.

use zeroize::Zeroize;

use crate::drbg::{DrbgError, HmacDrbg};
use crate::entropy::{EntropyError, EntropySource};
use crate::fast::FastRng;

/// Bytes of fresh entropy drawn to instantiate the DRBG (256-bit security
/// strength).
const SEED_ENTROPY_LEN: usize = 32;

/// Bytes drawn for the instantiation nonce (half the security strength, per
/// NIST SP 800-90Ar1 §8.6.7).
const SEED_NONCE_LEN: usize = 16;

/// Bytes of fresh entropy drawn on each reseed.
const RESEED_ENTROPY_LEN: usize = 32;

/// Default number of [`CsRng::try_fill_bytes`] calls between automatic
/// reseeds. Far below the DRBG's hard [`crate::drbg::RESEED_INTERVAL`]; the
/// frequent reseed buys prediction resistance cheaply, since a reseed is a
/// single entropy draw plus two HMACs.
pub const DEFAULT_RESEED_INTERVAL: u64 = 1 << 16;

/// A cryptographically secure random number generator.
///
/// Owns its [`EntropySource`] so reseeding needs no extra plumbing at the
/// call site.
pub struct CsRng<E: EntropySource> {
    drbg: HmacDrbg,
    entropy: E,
    calls_since_reseed: u64,
    reseed_interval: u64,
}

impl<E: EntropySource> CsRng<E> {
    /// Instantiate from `entropy`, drawing a fresh seed and nonce, using the
    /// [`DEFAULT_RESEED_INTERVAL`].
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] if the source cannot supply the
    /// initial seed; no generator is constructed in that case.
    pub fn new(entropy: E) -> Result<Self, EntropyError> {
        Self::with_reseed_interval(entropy, DEFAULT_RESEED_INTERVAL, &[])
    }

    /// Instantiate with a caller-chosen `personalization` string, which binds
    /// this instance's output to a domain (e.g. `b"rustfs/volume-key"`) so two
    /// generators seeded from the same source still diverge.
    ///
    /// # Errors
    ///
    /// As [`CsRng::new`].
    pub fn with_personalization(entropy: E, personalization: &[u8]) -> Result<Self, EntropyError> {
        Self::with_reseed_interval(entropy, DEFAULT_RESEED_INTERVAL, personalization)
    }

    /// Instantiate with an explicit reseed interval and personalization.
    ///
    /// A `reseed_interval` of `0` is treated as `1` (reseed before every
    /// draw — maximal prediction resistance).
    ///
    /// # Errors
    ///
    /// As [`CsRng::new`].
    pub fn with_reseed_interval(
        mut entropy: E,
        reseed_interval: u64,
        personalization: &[u8],
    ) -> Result<Self, EntropyError> {
        let mut seed = [0u8; SEED_ENTROPY_LEN + SEED_NONCE_LEN];
        entropy.fill(&mut seed)?;
        let drbg = HmacDrbg::new(
            &seed[..SEED_ENTROPY_LEN],
            &seed[SEED_ENTROPY_LEN..],
            personalization,
        );
        seed.zeroize();
        Ok(Self {
            drbg,
            entropy,
            calls_since_reseed: 0,
            reseed_interval: reseed_interval.max(1),
        })
    }

    /// Draw fresh entropy and reseed the DRBG, restoring prediction
    /// resistance and resetting the reseed clock.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] if the source cannot supply the
    /// reseed entropy. The existing DRBG state is left intact, so the caller
    /// may retry, but no new output is produced from a failed reseed.
    pub fn reseed(&mut self) -> Result<(), EntropyError> {
        let mut fresh = [0u8; RESEED_ENTROPY_LEN];
        self.entropy.fill(&mut fresh)?;
        self.drbg.reseed(&fresh, &[]);
        fresh.zeroize();
        self.calls_since_reseed = 0;
        Ok(())
    }

    /// Fill `out` with cryptographically secure random bytes, reseeding first
    /// if the reseed interval has elapsed.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] only when a required reseed
    /// cannot draw fresh entropy. The buffer is not partially filled on
    /// error.
    pub fn try_fill_bytes(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        if self.calls_since_reseed >= self.reseed_interval || self.drbg.needs_reseed() {
            self.reseed()?;
        }
        match self.drbg.generate(out, &[]) {
            Ok(()) => {
                self.calls_since_reseed += 1;
                Ok(())
            }
            Err(DrbgError::ReseedRequired) => {
                // The interval/needs_reseed check above already reseeds before
                // the hard limit, so this branch is only reachable if the
                // reseed clock and hard limit disagree; reseed and retry once.
                self.reseed()?;
                match self.drbg.generate(out, &[]) {
                    Ok(()) => {
                        self.calls_since_reseed += 1;
                        Ok(())
                    }
                    Err(DrbgError::ReseedRequired) => Err(EntropyError::Unavailable),
                }
            }
        }
    }

    /// Return a cryptographically secure `u64`.
    ///
    /// # Errors
    ///
    /// As [`CsRng::try_fill_bytes`].
    pub fn try_next_u64(&mut self) -> Result<u64, EntropyError> {
        let mut bytes = [0u8; 8];
        self.try_fill_bytes(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Return a cryptographically secure `u32`.
    ///
    /// # Errors
    ///
    /// As [`CsRng::try_fill_bytes`].
    pub fn try_next_u32(&mut self) -> Result<u32, EntropyError> {
        let mut bytes = [0u8; 4];
        self.try_fill_bytes(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Spawn a [`FastRng`] seeded from this CSPRNG.
    ///
    /// The returned fast generator is unpredictable at birth (its 256-bit
    /// state came from secure output) but cheap to run thereafter. Use it for
    /// bulk *non-security* randomness that still benefits from an
    /// unpredictable starting point; never for keys or nonces (those stay on
    /// [`CsRng`]).
    ///
    /// # Errors
    ///
    /// As [`CsRng::try_fill_bytes`].
    pub fn fork_fast(&mut self) -> Result<FastRng, EntropyError> {
        let mut state = [0u8; 32];
        self.try_fill_bytes(&mut state)?;
        let mut words = [0u64; 4];
        for (word, chunk) in words.iter_mut().zip(state.chunks_exact(8)) {
            *word = u64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
        }
        state.zeroize();
        Ok(FastRng::from_state(words))
    }

    /// Number of draws since the last (re)seed; for introspection and tests.
    #[must_use]
    pub fn calls_since_reseed(&self) -> u64 {
        self.calls_since_reseed
    }
}

impl<E: EntropySource> core::fmt::Debug for CsRng<E> {
    /// Never reveals DRBG state.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CsRng")
            .field("reseed_interval", &self.reseed_interval)
            .field("calls_since_reseed", &self.calls_since_reseed)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic stand-in for an entropy source: a counter expanded so
    /// each fill is distinct. Lets the tests assert determinism without
    /// needing real entropy; it is NOT used in production.
    struct CountingSource {
        counter: u64,
        budget: Option<u32>,
    }

    impl CountingSource {
        fn new(seed: u64) -> Self {
            Self {
                counter: seed,
                budget: None,
            }
        }

        /// A source that succeeds `n` times, then fails forever — to drive
        /// the reseed-failure path.
        fn with_budget(seed: u64, n: u32) -> Self {
            Self {
                counter: seed,
                budget: Some(n),
            }
        }
    }

    impl EntropySource for CountingSource {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            if let Some(b) = self.budget.as_mut() {
                if *b == 0 {
                    return Err(EntropyError::Unavailable);
                }
                *b -= 1;
            }
            for byte in out.iter_mut() {
                self.counter = self
                    .counter
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *byte = self.counter.to_le_bytes()[4];
            }
            Ok(())
        }
    }

    #[test]
    fn identical_sources_give_identical_streams() {
        let mut a = CsRng::new(CountingSource::new(1)).unwrap();
        let mut b = CsRng::new(CountingSource::new(1)).unwrap();
        let (mut oa, mut ob) = ([0u8; 100], [0u8; 100]);
        a.try_fill_bytes(&mut oa).unwrap();
        b.try_fill_bytes(&mut ob).unwrap();
        assert_eq!(oa, ob);
    }

    #[test]
    fn different_sources_diverge() {
        let mut a = CsRng::new(CountingSource::new(1)).unwrap();
        let mut b = CsRng::new(CountingSource::new(2)).unwrap();
        assert_ne!(a.try_next_u64().unwrap(), b.try_next_u64().unwrap());
    }

    #[test]
    fn personalization_diverges_same_source() {
        let mut a = CsRng::with_personalization(CountingSource::new(7), b"domain-a").unwrap();
        let mut b = CsRng::with_personalization(CountingSource::new(7), b"domain-b").unwrap();
        assert_ne!(a.try_next_u64().unwrap(), b.try_next_u64().unwrap());
    }

    #[test]
    fn construction_fails_when_entropy_is_unavailable() {
        let src = CountingSource::with_budget(1, 0);
        assert_eq!(CsRng::new(src).err(), Some(EntropyError::Unavailable));
    }

    #[test]
    fn reseed_clock_triggers_a_reseed() {
        // Interval 2 => reseed before the 3rd draw. Budget: 1 (instantiate)
        // + 1 (the triggered reseed) = 2 successful fills, then plenty more.
        let mut rng = CsRng::with_reseed_interval(CountingSource::new(9), 2, &[]).unwrap();
        let mut out = [0u8; 8];
        rng.try_fill_bytes(&mut out).unwrap();
        assert_eq!(rng.calls_since_reseed(), 1);
        rng.try_fill_bytes(&mut out).unwrap();
        assert_eq!(rng.calls_since_reseed(), 2);
        // This draw hits the interval and reseeds first, resetting the clock.
        rng.try_fill_bytes(&mut out).unwrap();
        assert_eq!(rng.calls_since_reseed(), 1);
    }

    #[test]
    fn reseed_failure_is_surfaced_not_hidden() {
        // Budget 1: only the instantiation seed succeeds. With interval 1 the
        // first draw still succeeds (the clock check is `calls >= interval`,
        // and `calls` starts at 0), but the second draw triggers a reseed
        // that has no entropy left — and that must surface, never be hidden.
        let mut rng =
            CsRng::with_reseed_interval(CountingSource::with_budget(3, 1), 1, &[]).unwrap();
        let mut out = [0u8; 8];
        rng.try_fill_bytes(&mut out).expect("first draw succeeds");
        assert_eq!(rng.try_fill_bytes(&mut out), Err(EntropyError::Unavailable));
    }

    #[test]
    fn fork_fast_is_unpredictable_and_each_fork_differs() {
        use crate::rand::RandU64;
        let mut rng = CsRng::new(CountingSource::new(42)).unwrap();
        let mut f1 = rng.fork_fast().unwrap();
        let mut f2 = rng.fork_fast().unwrap();
        assert_ne!(f1.next_u64(), f2.next_u64(), "two forks must differ");
    }

    #[test]
    fn output_is_well_balanced() {
        // Deterministic source => reproducible, never flaky.
        let mut rng = CsRng::new(CountingSource::new(0xABCD)).unwrap();
        let mut sum: u64 = 0;
        let mut buf = [0u8; 4096];
        let mut produced = 0usize;
        let n = 1usize << 20;
        while produced < n {
            rng.try_fill_bytes(&mut buf).unwrap();
            for &b in &buf {
                sum += u64::from(b);
            }
            produced += buf.len();
        }
        // Integer-only: mean*10 within 1.0 of 127.5, i.e. in [1265, 1285].
        let mean_times_10 = sum * 10 / produced as u64;
        assert!(
            (1265..=1285).contains(&mean_times_10),
            "mean*10 {mean_times_10} skewed"
        );
    }

    #[test]
    fn debug_does_not_leak_state() {
        extern crate alloc;
        use alloc::format;
        let rng = CsRng::new(CountingSource::new(5)).unwrap();
        let s = format!("{rng:?}");
        assert!(s.contains("CsRng"));
    }
}
