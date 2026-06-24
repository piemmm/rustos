//! The cryptographically secure RNG ([`CsRng`]).
//!
//! [`CsRng`] is the generator the rest of RustOS reaches for whenever the
//! randomness must be unpredictable: `RustFS` volume and per-record keys, the
//! encrypted-swap key, nonces, the KASLR/ASLR seed,
//! capability-token material. It pairs an [`HmacDrbg`] with an
//! [`EntropySource`] and reseeds from that source on a fixed schedule, so a
//! one-time state compromise cannot predict output indefinitely (forward
//! secrecy / prediction resistance).
//!
//! # Two draw styles: fallible and blocking
//!
//! Every draw can trigger a reseed, and a reseed needs fresh entropy that may
//! be momentarily unavailable. [`CsRng`] offers the caller both ways to cope,
//! and neither spins or panics:
//!
//! * **Fallible** ([`CsRng::try_fill_bytes`], [`CsRng::try_next_u64`], …):
//!   when a required reseed has no entropy *right now*, the draw returns the
//!   typed, transient [`EntropyError::Reseeding`] without disturbing the
//!   generator. The caller fails closed or retries. A *hard* failure
//!   (no source at all, e.g. at instantiation) is the distinct
//!   [`EntropyError::Unavailable`].
//! * **Blocking** ([`CsRng::fill_bytes_blocking`],
//!   [`CsRng::try_next_u64_blocking`], …): when a required reseed has no
//!   entropy, the draw **waits** for it through the entropy source's
//!   [`EntropySource::fill_blocking`] seam (the platform source parks the
//!   task; it never busy-spins) and then returns the bytes. It only fails
//!   with [`EntropyError::Unavailable`] when the source is genuinely dead.
//!
//! Reaching for randomness is therefore always an explicit, checked
//! operation, never a silent one.

use zeroize::Zeroize;

use crate::drbg::{DrbgError, HmacDrbg};
use crate::entropy::{EntropyError, EntropySource};
use crate::fast::FastRng;

/// How a reseed draws its fresh entropy from the source.
#[derive(Clone, Copy)]
enum ReseedMode {
    /// Non-blocking: a momentary shortage surfaces as [`EntropyError::Reseeding`].
    Fallible,
    /// Blocking: wait through a momentary shortage via
    /// [`EntropySource::fill_blocking`].
    Blocking,
}

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
    /// This is the **fallible** reseed: a momentary entropy shortage is not
    /// waited out. Use [`CsRng::reseed_blocking`] to wait instead.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Reseeding`] if the source cannot supply the
    /// reseed entropy *right now*. The existing DRBG state is left intact, so
    /// the caller may retry, but no new output is produced from a failed
    /// reseed.
    pub fn reseed(&mut self) -> Result<(), EntropyError> {
        self.reseed_with(ReseedMode::Fallible)
    }

    /// Draw fresh entropy and reseed the DRBG, **blocking** through a
    /// momentary entropy shortage until the source can supply it.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] only if the source is genuinely
    /// dead (it can never supply entropy, so waiting would not help); the
    /// DRBG state is left intact.
    pub fn reseed_blocking(&mut self) -> Result<(), EntropyError> {
        self.reseed_with(ReseedMode::Blocking)
    }

    /// Draw fresh reseed entropy through `mode` and fold it into the DRBG.
    ///
    /// Shared by the fallible and blocking reseeds so the seed handling,
    /// zeroisation, and clock reset live in one place.
    fn reseed_with(&mut self, mode: ReseedMode) -> Result<(), EntropyError> {
        let mut fresh = [0u8; RESEED_ENTROPY_LEN];
        let drawn = match mode {
            ReseedMode::Fallible => self.entropy.fill(&mut fresh).map_err(|_| {
                // A reseed shortage is transient: the generator is intact, so
                // surface the typed retryable signal rather than a hard error.
                EntropyError::Reseeding
            }),
            ReseedMode::Blocking => self.entropy.fill_blocking(&mut fresh),
        };
        if let Err(e) = drawn {
            fresh.zeroize();
            return Err(e);
        }
        self.drbg.reseed(&fresh, &[]);
        fresh.zeroize();
        self.calls_since_reseed = 0;
        Ok(())
    }

    /// Fill `out` with cryptographically secure random bytes, reseeding first
    /// if the reseed interval has elapsed.
    ///
    /// This is the **fallible** draw: a required reseed that cannot draw fresh
    /// entropy fails with [`EntropyError::Reseeding`] rather than waiting. Use
    /// [`CsRng::fill_bytes_blocking`] to wait through the reseed instead.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Reseeding`] when a required reseed cannot draw
    /// fresh entropy right now. The buffer is not partially filled on error.
    pub fn try_fill_bytes(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        self.fill_bytes_with(out, ReseedMode::Fallible)
    }

    /// Fill `out` with cryptographically secure random bytes, **blocking**
    /// through a reseed if one is required and its entropy is momentarily
    /// unavailable.
    ///
    /// The wait happens in the entropy source's
    /// [`EntropySource::fill_blocking`] (the platform source parks the calling
    /// task; it never busy-spins). When no reseed is needed
    /// — the common case — this does exactly as much work as
    /// [`CsRng::try_fill_bytes`] and returns without ever blocking.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError::Unavailable`] only when a required reseed's
    /// source is genuinely dead. The buffer is not partially filled on error.
    pub fn fill_bytes_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        self.fill_bytes_with(out, ReseedMode::Blocking)
    }

    /// Reseed (per `mode`) when due, then generate. Shared by the fallible and
    /// blocking fills so the reseed-clock and DRBG-limit handling is written
    /// once.
    fn fill_bytes_with(&mut self, out: &mut [u8], mode: ReseedMode) -> Result<(), EntropyError> {
        if self.calls_since_reseed >= self.reseed_interval || self.drbg.needs_reseed() {
            self.reseed_with(mode)?;
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
                self.reseed_with(mode)?;
                match self.drbg.generate(out, &[]) {
                    Ok(()) => {
                        self.calls_since_reseed += 1;
                        Ok(())
                    }
                    // A reseed just succeeded, so a still-required reseed means
                    // the DRBG is in an impossible state, not a mere shortage.
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

    /// Return a cryptographically secure `u64`, blocking through a reseed if
    /// required.
    ///
    /// # Errors
    ///
    /// As [`CsRng::fill_bytes_blocking`].
    pub fn try_next_u64_blocking(&mut self) -> Result<u64, EntropyError> {
        let mut bytes = [0u8; 8];
        self.fill_bytes_blocking(&mut bytes)?;
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

    /// Return a cryptographically secure `u32`, blocking through a reseed if
    /// required.
    ///
    /// # Errors
    ///
    /// As [`CsRng::fill_bytes_blocking`].
    pub fn try_next_u32_blocking(&mut self) -> Result<u32, EntropyError> {
        let mut bytes = [0u8; 4];
        self.fill_bytes_blocking(&mut bytes)?;
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
    fn reseed_failure_is_surfaced_as_transient_reseeding() {
        // Budget 1: only the instantiation seed succeeds. With interval 1 the
        // first draw still succeeds (the clock check is `calls >= interval`,
        // and `calls` starts at 0), but the second draw triggers a reseed
        // that has no entropy left. That surfaces as the typed, transient
        // `Reseeding` (the generator is intact), never as a hard error and
        // never hidden behind weak output.
        let mut rng =
            CsRng::with_reseed_interval(CountingSource::with_budget(3, 1), 1, &[]).unwrap();
        let mut out = [0u8; 8];
        rng.try_fill_bytes(&mut out).expect("first draw succeeds");
        assert_eq!(rng.try_fill_bytes(&mut out), Err(EntropyError::Reseeding));
        // The DRBG is intact: an explicit fallible reseed reports the same
        // transient signal rather than corrupting state.
        assert_eq!(rng.reseed(), Err(EntropyError::Reseeding));
    }

    /// A source whose non-blocking `fill` is exhausted after `budget` draws,
    /// but whose blocking `fill_blocking` always delivers — a stand-in for a
    /// pool a parking platform source would wait on.
    struct ParkingSource {
        counter: u64,
        budget: u32,
    }

    impl ParkingSource {
        fn new(seed: u64, budget: u32) -> Self {
            Self {
                counter: seed,
                budget,
            }
        }

        fn produce(&mut self, out: &mut [u8]) {
            for byte in out.iter_mut() {
                self.counter = self
                    .counter
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *byte = self.counter.to_le_bytes()[4];
            }
        }
    }

    impl EntropySource for ParkingSource {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.budget == 0 {
                return Err(EntropyError::Unavailable);
            }
            self.budget -= 1;
            self.produce(out);
            Ok(())
        }

        fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.budget == 0 {
                // Model a wait that replenishes the pool, then deliver.
                self.budget = 1;
            }
            self.fill(out)
        }
    }

    #[test]
    fn blocking_draw_waits_through_a_reseed_shortage() {
        // Budget 1: only instantiation succeeds; the interval-1 reseed on the
        // second draw would fail the fallible path…
        let mut rng = CsRng::with_reseed_interval(ParkingSource::new(11, 1), 1, &[]).unwrap();
        let mut out = [0u8; 8];
        rng.fill_bytes_blocking(&mut out)
            .expect("first blocking draw");
        // …but the blocking draw waits for entropy and still succeeds.
        rng.fill_bytes_blocking(&mut out)
            .expect("blocking draw waits through the reseed");
        assert_ne!(out, [0u8; 8]);
    }

    #[test]
    fn blocking_and_fallible_share_the_no_reseed_fast_path() {
        // With a generous interval no reseed is due, so both styles produce
        // the same stream from the same seed and neither waits.
        let mut a = CsRng::new(ParkingSource::new(7, 4)).unwrap();
        let mut b = CsRng::new(ParkingSource::new(7, 4)).unwrap();
        let (mut oa, mut ob) = ([0u8; 32], [0u8; 32]);
        a.try_fill_bytes(&mut oa).unwrap();
        b.fill_bytes_blocking(&mut ob).unwrap();
        assert_eq!(oa, ob, "no reseed due ⇒ identical, neither blocks");
    }

    #[test]
    fn reseed_blocking_recovers_where_fallible_reseed_fails() {
        // Budget 1: instantiation consumes it, so a fallible reseed is a
        // transient miss while a blocking reseed waits and succeeds.
        let mut rng = CsRng::new(ParkingSource::new(21, 1)).unwrap();
        assert_eq!(rng.reseed(), Err(EntropyError::Reseeding));
        rng.reseed_blocking()
            .expect("blocking reseed waits, succeeds");
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
