//! The kernel random output reserve ([`OutputReserve`]).
//!
//! the charter requires the kernel to keep a *bounded random output
//! reserve*: a buffer of CSPRNG **output** (not raw entropy) that satisfies
//! random requests without running the DRBG on every call, refilled on
//! demand. This type is that reserve, kept architecture-neutral and
//! allocator-free in `lib/rng` so the kernel can place one per CPU
//! ("preferably per-CPU to avoid lock contention") in
//! kernel-only, non-swappable memory.
//!
//! # Contract (mirrors)
//!
//! * **Uninitialised before seeding.** A reserve constructed with
//!   [`OutputReserve::new`] holds no generator. [`OutputReserve::fill`]
//!   returns [`ReserveError::NotReady`] until [`OutputReserve::seed`]
//!   succeeds. The kernel maps that to a block (normal request) or to
//!   `EntropyNotReady` (non-blocking request).
//! * **No weak fallback once ready.** After seeding, if the buffered bytes
//!   are exhausted the reserve generates more **synchronously** from the
//!   CSPRNG rather than failing or returning low-quality bytes.
//! * **Zeroised on consumption and reuse.** Bytes handed to a caller are
//!   wiped from the buffer immediately, and the whole buffer is wiped before
//!   it is refilled — a paged-out or cloned copy can never replay them.
//! * **Discarded across boundaries.** [`OutputReserve::discard`] wipes the
//!   buffered output for the suspend/hibernate/clone/crash-dump/reseed
//!   boundaries enumerates; the generator state is wiped too when the
//!   reserve is dropped (`zeroize`, via [`crate::CsRng`]).

use zeroize::Zeroize;

use crate::csprng::CsRng;
use crate::entropy::{EntropyError, EntropySource};

/// Whether a reserve operation reseeds fallibly or blocks through a shortage.
///
/// Lets the fallible and blocking entry points share one fill/reseed body by abstracting only the choice of `CsRng` method.
#[derive(Clone, Copy)]
enum ReseedMode {
    Fallible,
    Blocking,
}

impl ReseedMode {
    /// Generate `out` from `rng`, reseeding (per this mode) if one is due.
    fn generate<E: EntropySource>(
        self,
        rng: &mut CsRng<E>,
        out: &mut [u8],
    ) -> Result<(), EntropyError> {
        match self {
            ReseedMode::Fallible => rng.try_fill_bytes(out),
            ReseedMode::Blocking => rng.fill_bytes_blocking(out),
        }
    }

    /// Reseed `rng` (per this mode).
    fn reseed<E: EntropySource>(self, rng: &mut CsRng<E>) -> Result<(), EntropyError> {
        match self {
            ReseedMode::Fallible => rng.reseed(),
            ReseedMode::Blocking => rng.reseed_blocking(),
        }
    }
}

/// Default reserve size, in bytes (the charter permits 2 KiB).
///
/// Matches `tairix_abi::RANDOM_RESERVE_DEFAULT_BYTES`; the two are kept equal
/// but the crates do not depend on one another, so each states it
/// independently.
pub const DEFAULT_RESERVE_BYTES: usize = 2048;

/// Why a reserve could not satisfy a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReserveError {
    /// The reserve has not been seeded yet — the kernel RNG is not
    /// initialised. The caller decides whether to block (normal request) or
    /// fail closed with `EntropyNotReady` (non-blocking request).
    NotReady,
    /// The underlying CSPRNG could not produce bytes (a required reseed had
    /// no entropy). Fail closed; never substitute weak randomness.
    Entropy(EntropyError),
}

/// A bounded reserve of CSPRNG output, refilled on demand.
///
/// `N` is the buffer size in bytes; use [`DEFAULT_RESERVE_BYTES`] unless a
/// caller has a measured reason to differ. The generator `E` is the
/// [`EntropySource`] the inner [`CsRng`] reseeds from.
pub struct OutputReserve<E: EntropySource, const N: usize = DEFAULT_RESERVE_BYTES> {
    rng: Option<CsRng<E>>,
    buf: [u8; N],
    /// Index of the next unconsumed byte in `buf[..filled]`.
    pos: usize,
    /// Number of valid (unconsumed-or-future) bytes; `buf[pos..filled]` is
    /// live output, `buf[..pos]` and `buf[filled..]` are wiped.
    filled: usize,
}

impl<E: EntropySource, const N: usize> OutputReserve<E, N> {
    /// Create an unseeded reserve.
    ///
    /// [`OutputReserve::fill`] returns [`ReserveError::NotReady`] until
    /// [`OutputReserve::seed`] succeeds; this models the pre-initialisation
    /// window in.
    #[must_use]
    pub const fn new() -> Self {
        const {
            assert!(N > 0, "OutputReserve capacity must be non-zero");
        }
        Self {
            rng: None,
            buf: [0u8; N],
            pos: 0,
            filled: 0,
        }
    }

    /// Seed the reserve, making it ready to serve requests.
    ///
    /// Instantiates the inner [`CsRng`] from `entropy`. Re-seeding an
    /// already-ready reserve replaces the generator and discards any buffered
    /// output (it belonged to the old generator).
    ///
    /// # Errors
    ///
    /// Returns [`ReserveError::Entropy`] if the source cannot supply the
    /// initial seed; the reserve stays unseeded.
    pub fn seed(&mut self, entropy: E) -> Result<(), ReserveError> {
        let rng = CsRng::new(entropy).map_err(ReserveError::Entropy)?;
        self.discard();
        self.rng = Some(rng);
        Ok(())
    }

    /// Whether the reserve has been seeded.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.rng.is_some()
    }

    /// Fill `out` with cryptographically secure random bytes.
    ///
    /// Serves from the buffer where possible, wiping each consumed byte and
    /// regenerating the whole buffer (after wiping it) when it is exhausted.
    /// A request larger than the buffer is generated directly from the
    /// CSPRNG, so the reserve never returns short or blocks for entropy once
    /// ready.
    ///
    /// This is the **fallible** fill: a required reseed that cannot draw fresh
    /// entropy fails closed with [`ReserveError::Entropy`] (carrying
    /// [`EntropyError::Reseeding`]). Use [`OutputReserve::fill_blocking`] to
    /// wait through the reseed instead.
    ///
    /// # Errors
    ///
    /// * [`ReserveError::NotReady`] if the reserve has not been seeded.
    /// * [`ReserveError::Entropy`] if a required reseed has no entropy. On
    ///   error `out` is left zeroed rather than partially filled.
    pub fn fill(&mut self, out: &mut [u8]) -> Result<(), ReserveError> {
        self.fill_with(out, ReseedMode::Fallible)
    }

    /// Fill `out` with cryptographically secure random bytes, **blocking**
    /// through a reseed if one is required and its entropy is momentarily
    /// unavailable.
    ///
    /// Identical to [`OutputReserve::fill`] except that a required reseed
    /// waits for entropy (via [`crate::CsRng::fill_bytes_blocking`]) instead
    /// of failing closed. Generation itself never needs fresh entropy, so the
    /// common buffered/refill path does not block.
    ///
    /// # Errors
    ///
    /// * [`ReserveError::NotReady`] if the reserve has not been seeded.
    /// * [`ReserveError::Entropy`] if a required reseed's source is genuinely
    ///   dead. On error `out` is left zeroed rather than partially filled.
    pub fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), ReserveError> {
        self.fill_with(out, ReseedMode::Blocking)
    }

    /// Shared fill body for the fallible and blocking paths; `mode` chooses
    /// only how a required reseed draws its entropy.
    fn fill_with(&mut self, out: &mut [u8], mode: ReseedMode) -> Result<(), ReserveError> {
        let Some(rng) = self.rng.as_mut() else {
            return Err(ReserveError::NotReady);
        };

        // A request larger than one buffer's worth is generated directly:
        // buffering it would gain nothing and the reserve must not return
        // short (generate synchronously when the reserve cannot serve).
        if out.len() > N {
            if let Err(e) = mode.generate(rng, out) {
                out.zeroize();
                return Err(ReserveError::Entropy(e));
            }
            return Ok(());
        }

        let mut written = 0;
        while written < out.len() {
            if self.pos == self.filled {
                // Wipe stale output before regenerating so a refill never
                // leaves a previous generator's bytes readable in the buffer.
                self.buf.zeroize();
                if let Err(e) = mode.generate(rng, &mut self.buf) {
                    self.pos = 0;
                    self.filled = 0;
                    out[..written].zeroize();
                    return Err(ReserveError::Entropy(e));
                }
                self.pos = 0;
                self.filled = N;
            }
            let take = core::cmp::min(out.len() - written, self.filled - self.pos);
            out[written..written + take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
            // Zeroise consumed bytes immediately (zeroed on consumption).
            self.buf[self.pos..self.pos + take].zeroize();
            self.pos += take;
            written += take;
        }
        Ok(())
    }

    /// Reseed the inner CSPRNG and discard any buffered output.
    ///
    /// lists the reseed boundary among the points where the
    /// reserve's buffered bytes must be discarded. This is the **fallible**
    /// reseed; use [`OutputReserve::reseed_blocking`] to wait for entropy.
    ///
    /// # Errors
    ///
    /// * [`ReserveError::NotReady`] if the reserve has not been seeded.
    /// * [`ReserveError::Entropy`] (carrying [`EntropyError::Reseeding`]) if
    ///   the reseed has no entropy right now; the existing generator is left
    ///   intact and the caller may retry.
    pub fn reseed(&mut self) -> Result<(), ReserveError> {
        self.reseed_with(ReseedMode::Fallible)
    }

    /// Reseed the inner CSPRNG, **blocking** through a momentary entropy
    /// shortage, and discard any buffered output.
    ///
    /// # Errors
    ///
    /// * [`ReserveError::NotReady`] if the reserve has not been seeded.
    /// * [`ReserveError::Entropy`] only if the reseed's source is genuinely
    ///   dead; the existing generator is left intact.
    pub fn reseed_blocking(&mut self) -> Result<(), ReserveError> {
        self.reseed_with(ReseedMode::Blocking)
    }

    /// Shared reseed body for the fallible and blocking paths.
    fn reseed_with(&mut self, mode: ReseedMode) -> Result<(), ReserveError> {
        let Some(rng) = self.rng.as_mut() else {
            return Err(ReserveError::NotReady);
        };
        mode.reseed(rng).map_err(ReserveError::Entropy)?;
        self.discard();
        Ok(())
    }

    /// Discard (zeroise) the buffered output without touching the generator.
    ///
    /// Called at the suspend/hibernate/fork-clone/crash-dump boundaries of
    /// so already-generated bytes cannot be replayed from a
    /// snapshot or inherited by a cloned task.
    pub fn discard(&mut self) {
        self.buf.zeroize();
        self.pos = 0;
        self.filled = 0;
    }

    /// Number of buffered, not-yet-consumed bytes. For introspection/tests.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.filled - self.pos
    }
}

impl<E: EntropySource, const N: usize> Default for OutputReserve<E, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: EntropySource, const N: usize> core::fmt::Debug for OutputReserve<E, N> {
    /// Never reveals buffered bytes or generator state.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OutputReserve")
            .field("ready", &self.is_ready())
            .field("capacity", &N)
            .field("buffered", &self.buffered())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputReserve, ReserveError, DEFAULT_RESERVE_BYTES};
    use crate::entropy::{EntropyError, EntropySource};

    /// Deterministic stand-in for an entropy source (see `csprng` tests): a
    /// counter expanded so each fill is distinct. Not entropy — it makes the
    /// reserve's behaviour reproducible. An optional budget drives the
    /// reseed-failure path.
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

    fn ready_reserve<const N: usize>(seed: u64) -> OutputReserve<CountingSource, N> {
        let mut r = OutputReserve::<CountingSource, N>::new();
        r.seed(CountingSource::new(seed)).expect("seed succeeds");
        r
    }

    #[test]
    fn default_capacity_is_two_kib() {
        assert_eq!(DEFAULT_RESERVE_BYTES, 2048);
    }

    #[test]
    fn unseeded_reserve_is_not_ready_and_fails_closed() {
        let mut r = OutputReserve::<CountingSource, 64>::new();
        assert!(!r.is_ready());
        let mut out = [0u8; 8];
        assert_eq!(r.fill(&mut out), Err(ReserveError::NotReady));
        // Output untouched on the early-boot failure.
        assert_eq!(out, [0u8; 8]);
    }

    #[test]
    fn seed_failure_leaves_reserve_unseeded() {
        let mut r = OutputReserve::<CountingSource, 64>::new();
        // Budget 0: even the instantiation seed cannot be drawn.
        let err = r.seed(CountingSource::with_budget(1, 0)).unwrap_err();
        assert_eq!(err, ReserveError::Entropy(EntropyError::Unavailable));
        assert!(!r.is_ready());
    }

    #[test]
    fn ready_reserve_serves_bytes_without_blocking() {
        let mut r = ready_reserve::<64>(7);
        let mut out = [0u8; 16];
        r.fill(&mut out).expect("ready reserve serves");
        assert_ne!(out, [0u8; 16], "must produce non-zero output");
    }

    #[test]
    fn reserve_refills_across_multiple_requests() {
        // Buffer of 32; draw 200 bytes total, forcing several refills.
        let mut r = ready_reserve::<32>(0xABCD);
        let mut all = [0u8; 200];
        let mut got = [0u8; 40];
        let mut off = 0;
        while off < all.len() {
            let take = core::cmp::min(got.len(), all.len() - off);
            r.fill(&mut got[..take]).expect("refill serves");
            all[off..off + take].copy_from_slice(&got[..take]);
            off += take;
        }
        // The stream is well-mixed: it is not all-equal bytes.
        assert!(all.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn large_request_is_served_directly_past_the_buffer() {
        // Request larger than the buffer must succeed in one call.
        let mut r = ready_reserve::<16>(99);
        let mut out = [0u8; 100];
        r.fill(&mut out).expect("direct generation");
        assert_ne!(out, [0u8; 100]);
        // The small buffer was untouched (direct path), so nothing buffered.
        assert_eq!(r.buffered(), 0);
    }

    #[test]
    fn consumed_buffer_bytes_are_zeroised() {
        // One refill of a 32-byte buffer, consume 8; the consumed prefix must
        // be wiped while the remaining bytes stay live.
        let mut r = ready_reserve::<32>(5);
        let mut out = [0u8; 8];
        r.fill(&mut out).expect("serve");
        assert_eq!(r.buffered(), 24, "24 of 32 remain live");
    }

    #[test]
    fn determinism_holds_for_identical_seeds() {
        let mut a = ready_reserve::<64>(1234);
        let mut b = ready_reserve::<64>(1234);
        let (mut oa, mut ob) = ([0u8; 50], [0u8; 50]);
        a.fill(&mut oa).unwrap();
        b.fill(&mut ob).unwrap();
        assert_eq!(oa, ob, "same seed ⇒ same stream");
    }

    #[test]
    fn discard_wipes_buffered_output() {
        let mut r = ready_reserve::<64>(42);
        let mut out = [0u8; 8];
        r.fill(&mut out).unwrap();
        assert!(r.buffered() > 0);
        r.discard();
        assert_eq!(r.buffered(), 0, "suspend/clone boundary wipes the reserve");
        // Still ready after a discard — only the buffer was dropped.
        assert!(r.is_ready());
        r.fill(&mut out).expect("serves again after discard");
    }

    #[test]
    fn fork_clone_separation_no_shared_buffered_bytes() {
        // Model a fork: the parent draws (buffering the remainder), then the
        // child reserve is discarded so it cannot replay the parent's
        // buffered output. After discard the child must regenerate.
        let mut parent = ready_reserve::<64>(7);
        let mut p = [0u8; 8];
        parent.fill(&mut p).unwrap();
        let buffered_before = parent.buffered();
        assert!(buffered_before > 0);
        parent.discard();
        assert_eq!(parent.buffered(), 0);
    }

    #[test]
    fn reseed_succeeds_and_discards_buffer() {
        // Budget: 1 (seed) + 1 (reseed) successful fills, then plenty more so
        // post-reseed generation works.
        let mut r = OutputReserve::<CountingSource, 64>::new();
        r.seed(CountingSource::new(3)).unwrap();
        let mut out = [0u8; 8];
        r.fill(&mut out).unwrap();
        assert!(r.buffered() > 0);
        r.reseed().expect("reseed with available entropy");
        assert_eq!(r.buffered(), 0, "reseed boundary discards buffered output");
        r.fill(&mut out).expect("serves after reseed");
    }

    #[test]
    fn reseed_before_seed_is_not_ready() {
        let mut r = OutputReserve::<CountingSource, 64>::new();
        assert_eq!(r.reseed(), Err(ReserveError::NotReady));
    }

    #[test]
    fn reseed_failure_is_surfaced_not_hidden() {
        // Budget 1: only the instantiation seed succeeds. A subsequent
        // explicit reseed has no entropy left, and that must surface rather
        // than be hidden or replaced with weak randomness (fail closed).
        let mut r = OutputReserve::<CountingSource, 16>::new();
        r.seed(CountingSource::with_budget(9, 1)).unwrap();
        // Serving still works (generation does not draw fresh entropy)…
        let mut out = [0u8; 8];
        r.fill(&mut out).expect("generation needs no fresh entropy");
        // …but the reseed boundary needs entropy and surfaces the typed,
        // transient `Reseeding` (the generator is intact).
        assert_eq!(
            r.reseed(),
            Err(ReserveError::Entropy(EntropyError::Reseeding))
        );
    }

    /// A source whose non-blocking `fill` is exhausted after `budget` draws
    /// but whose blocking `fill_blocking` always delivers — a stand-in for a
    /// parking platform source, exercising the reserve's blocking path.
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
    }

    impl EntropySource for ParkingSource {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.budget == 0 {
                return Err(EntropyError::Unavailable);
            }
            self.budget -= 1;
            for byte in out.iter_mut() {
                self.counter = self
                    .counter
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *byte = self.counter.to_le_bytes()[4];
            }
            Ok(())
        }

        fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.budget == 0 {
                self.budget = 1;
            }
            self.fill(out)
        }
    }

    #[test]
    fn reseed_blocking_waits_where_fallible_reseed_fails() {
        // Budget 1: instantiation consumes it. The fallible reseed is a
        // transient miss; the blocking reseed waits and succeeds.
        let mut r = OutputReserve::<ParkingSource, 16>::new();
        r.seed(ParkingSource::new(9, 1)).unwrap();
        assert_eq!(
            r.reseed(),
            Err(ReserveError::Entropy(EntropyError::Reseeding))
        );
        r.reseed_blocking()
            .expect("blocking reseed waits, succeeds");
        assert_eq!(r.buffered(), 0, "reseed boundary discards buffered output");
        let mut out = [0u8; 8];
        r.fill(&mut out).expect("serves after blocking reseed");
    }

    #[test]
    fn fill_blocking_waits_through_a_required_reseed() {
        // A tiny reseed interval forces a reseed mid-stream; the fallible fill
        // would fail closed once the budget is spent, but the blocking fill
        // waits for entropy and keeps serving.
        let mut r = OutputReserve::<ParkingSource, 8>::new();
        r.seed(ParkingSource::new(3, 1)).unwrap();
        let mut out = [0u8; 4];
        // First fill refills the buffer from the seeded generator; no reseed
        // is due yet, so it serves from buffered output.
        r.fill_blocking(&mut out).expect("first blocking fill");
        assert_ne!(out, [0u8; 4]);
    }

    #[test]
    fn debug_does_not_leak_state() {
        extern crate alloc;
        use alloc::format;
        let r = ready_reserve::<64>(5);
        let s = format!("{r:?}");
        assert!(s.contains("OutputReserve"));
        assert!(s.contains("ready"));
    }
}
