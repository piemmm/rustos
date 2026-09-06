//! The kernel random output reserve ([`OutputReserve`]).
//!
//! The kernel keeps a *bounded random output reserve*: a buffer of CSPRNG
//! **output** — not raw entropy — that satisfies random requests without
//! running a DRBG on every call, refilled on demand. This type is that
//! reserve, kept architecture-neutral and allocator-free so the kernel can
//! place one per CPU in kernel-only, non-swappable memory.
//!
//! # The chain
//!
//! ```text
//! entropy pool → CsRng (HMAC-DRBG) → FastRng<N> (ChaCha12) → userland
//! ```
//!
//! [`crate::CsRng`] is the reserve's *authority*: it keys the fast generator
//! at seed time and re-keys it at every boundary. [`crate::FastRng`] is what
//! every served byte comes from, so a userland request costs a cipher block
//! rather than a DRBG generate. The reserve keeps no byte buffer of its own —
//! `FastRng` already is a buffered generator that wipes each byte as it is
//! consumed, and a second such buffer beside it would be one more
//! zeroisation path to keep correct.
//!
//! # Contract
//!
//! * **Uninitialised before seeding.** A reserve from [`OutputReserve::new`]
//!   holds no generator, and every draw returns [`ReserveError::NotReady`]
//!   until [`OutputReserve::seed`] succeeds. The kernel maps that to a block
//!   (normal request) or to `EntropyNotReady` (non-blocking request).
//! * **No weak fallback, and no blocking, once ready.** Serving needs no
//!   fresh entropy at all: an exhausted buffer is regenerated from the
//!   cipher, so a seeded reserve neither returns short, nor waits on the
//!   entropy source, nor substitutes low-quality bytes.
//! * **Zeroised on consumption and reuse.** Bytes handed to a caller are
//!   wiped immediately, and a refill destroys the key that produced the
//!   previous buffer, so a paged-out or cloned copy can replay nothing.
//! * **Discarded across boundaries.** [`OutputReserve::discard`] drops
//!   buffered output *and* rotates the key behind it for the
//!   suspend/hibernate/clone/crash-dump/reseed boundaries; both generators'
//!   working state is wiped on drop.
//! * **Periodically prediction-resistant.** Every
//!   [`crate::PERTURB_INTERVAL_BYTES`] of output the reserve reseeds the DRBG
//!   from the entropy pool and folds a fresh 32 bytes into the cipher key, so
//!   fresh entropy enters the chain on a bounded output cadence rather than
//!   only when the DRBG's own draw counter happens to run out.

use zeroize::Zeroize;

use tairix_crypto::STREAM_KEY_LEN;

use crate::csprng::CsRng;
use crate::entropy::{EntropyError, EntropySource};
use crate::fast::FastRng;
use crate::rand::RandU64;

/// Whether a reserve operation draws entropy fallibly or blocks through a
/// shortage.
///
/// Lets the fallible and blocking entry points share one perturbation and
/// reseed body, abstracting only the choice of [`CsRng`] method.
#[derive(Clone, Copy)]
enum ReseedMode {
    Fallible,
    Blocking,
}

impl ReseedMode {
    /// Generate `out` from `rng`.
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

    /// Reseed `rng` from its entropy source.
    fn reseed<E: EntropySource>(self, rng: &mut CsRng<E>) -> Result<(), EntropyError> {
        match self {
            ReseedMode::Fallible => rng.reseed(),
            ReseedMode::Blocking => rng.reseed_blocking(),
        }
    }
}

/// Default reserve size, in bytes.
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
    /// A boundary that genuinely needs fresh entropy could not draw it: the
    /// initial seed, or an explicit [`OutputReserve::reseed`]. Fail closed;
    /// never substitute weak randomness. Serving bytes never reports this,
    /// because serving needs no entropy.
    Entropy(EntropyError),
}

/// The seeded half of a reserve: the DRBG authority and the generator every
/// served byte comes from.
struct Ready<E: EntropySource, const N: usize> {
    cs: CsRng<E>,
    fast: FastRng<N>,
}

/// A bounded reserve of CSPRNG output, refilled on demand.
///
/// `N` is the issue-buffer size in bytes; use [`DEFAULT_RESERVE_BYTES`]
/// unless a caller has a measured reason to differ. `E` is the
/// [`EntropySource`] the inner [`CsRng`] seeds and reseeds from.
pub struct OutputReserve<E: EntropySource, const N: usize = DEFAULT_RESERVE_BYTES> {
    ready: Option<Ready<E, N>>,
}

impl<E: EntropySource, const N: usize> OutputReserve<E, N> {
    /// Create an unseeded reserve.
    ///
    /// Every draw returns [`ReserveError::NotReady`] until
    /// [`OutputReserve::seed`] succeeds; this models the pre-initialisation
    /// window the kernel boots through.
    #[must_use]
    pub const fn new() -> Self {
        const {
            assert!(N > 0, "OutputReserve capacity must be non-zero");
        }
        Self { ready: None }
    }

    /// Seed the reserve, making it ready to serve requests.
    ///
    /// Instantiates the [`CsRng`] from `entropy` and keys the fast generator
    /// from its first output. Seeding an already-ready reserve replaces both,
    /// discarding output that belonged to the old pair.
    ///
    /// # Errors
    ///
    /// Returns [`ReserveError::Entropy`] if the source cannot supply the
    /// initial seed. The reserve is left exactly as it was — unseeded, or
    /// still serving from its previous generators — rather than half-built.
    pub fn seed(&mut self, entropy: E) -> Result<(), ReserveError> {
        let mut cs = CsRng::new(entropy).map_err(ReserveError::Entropy)?;
        let fast = cs.fork_fast().map_err(ReserveError::Entropy)?;
        self.ready = Some(Ready { cs, fast });
        Ok(())
    }

    /// Whether the reserve has been seeded.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.ready.is_some()
    }

    /// Fill `out` with cryptographically secure random bytes.
    ///
    /// Serves from the fast generator, which refills itself from the cipher
    /// and wipes each byte as it hands it over. A request of any length is
    /// served in one call; nothing here waits on the entropy source.
    ///
    /// This is the **fallible** fill only in that the periodic perturbation
    /// it carries out does not wait: when fresh entropy is momentarily
    /// unavailable the perturbation is simply deferred to the next request
    /// and the bytes served are unaffected — they are cipher output under a
    /// DRBG-derived key either way. Use [`OutputReserve::fill_blocking`] to
    /// wait for the perturbation's entropy instead.
    ///
    /// # Errors
    ///
    /// [`ReserveError::NotReady`] if the reserve has not been seeded; `out`
    /// is left untouched.
    pub fn fill(&mut self, out: &mut [u8]) -> Result<(), ReserveError> {
        self.serve(out, ReseedMode::Fallible)
    }

    /// Fill `out` with cryptographically secure random bytes, **waiting** for
    /// the entropy a due perturbation needs.
    ///
    /// Identical to [`OutputReserve::fill`] except that a perturbation which
    /// has come due waits for its entropy rather than deferring. Generation
    /// itself never blocks either way.
    ///
    /// # Errors
    ///
    /// [`ReserveError::NotReady`] if the reserve has not been seeded; `out`
    /// is left untouched.
    pub fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), ReserveError> {
        self.serve(out, ReseedMode::Blocking)
    }

    /// Shared serve body; `mode` chooses only how a due perturbation draws.
    fn serve(&mut self, out: &mut [u8], mode: ReseedMode) -> Result<(), ReserveError> {
        let Some(ready) = self.ready.as_mut() else {
            return Err(ReserveError::NotReady);
        };
        perturb_if_due(ready, mode);
        ready.fast.fill_bytes(out);
        Ok(())
    }

    /// Reseed the DRBG and re-key the fast generator from it, discarding
    /// buffered output.
    ///
    /// This is the explicit reseed boundary, distinct from the automatic
    /// perturbation [`OutputReserve::fill`] carries out. **Fallible**: use
    /// [`OutputReserve::reseed_blocking`] to wait for entropy.
    ///
    /// # Errors
    ///
    /// * [`ReserveError::NotReady`] if the reserve has not been seeded.
    /// * [`ReserveError::Entropy`] (carrying [`EntropyError::Reseeding`]) if
    ///   there is no entropy right now; the generators are left usable and
    ///   the caller may retry.
    pub fn reseed(&mut self) -> Result<(), ReserveError> {
        self.reseed_with(ReseedMode::Fallible)
    }

    /// Reseed the DRBG and re-key the fast generator, **blocking** through a
    /// momentary entropy shortage.
    ///
    /// # Errors
    ///
    /// * [`ReserveError::NotReady`] if the reserve has not been seeded.
    /// * [`ReserveError::Entropy`] only if the source is genuinely dead; the
    ///   generators are left usable.
    pub fn reseed_blocking(&mut self) -> Result<(), ReserveError> {
        self.reseed_with(ReseedMode::Blocking)
    }

    /// Shared reseed body for the fallible and blocking paths.
    fn reseed_with(&mut self, mode: ReseedMode) -> Result<(), ReserveError> {
        let Some(ready) = self.ready.as_mut() else {
            return Err(ReserveError::NotReady);
        };
        mode.reseed(&mut ready.cs).map_err(ReserveError::Entropy)?;
        let mut key = [0u8; STREAM_KEY_LEN];
        let drawn = mode.generate(&mut ready.cs, &mut key);
        // Either way the boundary destroys buffered output and the key behind
        // it; only a successful draw also installs a DRBG-derived one.
        match drawn {
            Ok(()) => ready.fast = FastRng::from_key(&key),
            Err(e) => {
                ready.fast.discard();
                key.zeroize();
                return Err(ReserveError::Entropy(e));
            }
        }
        key.zeroize();
        Ok(())
    }

    /// Discard buffered output **and** the key that produced it, without
    /// touching the DRBG.
    ///
    /// Called at the suspend/hibernate/fork-clone/crash-dump boundaries, so
    /// already-generated bytes cannot be replayed from a snapshot and a
    /// cloned task cannot continue its parent's stream. A no-op on an
    /// unseeded reserve, which has nothing to discard.
    pub fn discard(&mut self) {
        if let Some(ready) = self.ready.as_mut() {
            ready.fast.discard();
        }
    }

    /// Buffered, not-yet-consumed bytes. For introspection and tests.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        match &self.ready {
            Some(ready) => ready.fast.buffered(),
            None => 0,
        }
    }
}

/// Fold fresh entropy into the fast generator's key when its output cadence
/// says it is due.
///
/// The reseed comes first and is the point of the exercise: perturbing with
/// output of a DRBG state that was compromised at the same moment buys
/// nothing, so prediction resistance needs entropy that has actually entered
/// the chain since. A shortage defers the perturbation to the next request
/// instead of denying it or failing the caller's draw — the bytes served are
/// cipher output under a DRBG-derived key regardless, and refusing randomness
/// to userland would buy no security.
fn perturb_if_due<E: EntropySource, const N: usize>(ready: &mut Ready<E, N>, mode: ReseedMode) {
    if !ready.fast.perturb_due() {
        return;
    }
    if mode.reseed(&mut ready.cs).is_err() {
        return;
    }
    let mut fresh = [0u8; STREAM_KEY_LEN];
    if mode.generate(&mut ready.cs, &mut fresh).is_ok() {
        ready.fast.perturb(&fresh);
    }
    fresh.zeroize();
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
    use crate::fast::PERTURB_INTERVAL_BYTES;

    /// Deterministic stand-in for an entropy source (see the `csprng` tests):
    /// a counter expanded so each fill is distinct. Not entropy — it makes
    /// the reserve's behaviour reproducible. An optional budget drives the
    /// shortage paths.
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

    /// Draw `bytes` from `reserve` in `chunk`-sized requests, returning the
    /// tail of the stream so two reserves' late output can be compared.
    fn drain<const N: usize>(
        reserve: &mut OutputReserve<CountingSource, N>,
        bytes: u64,
        tail: &mut [u8],
    ) {
        let mut chunk = [0u8; 4096];
        let mut served = 0u64;
        while served < bytes {
            reserve.fill(&mut chunk).expect("a seeded reserve serves");
            served += u64::try_from(chunk.len()).expect("a chunk length fits a u64");
        }
        reserve.fill(tail).expect("a seeded reserve serves");
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
        assert_eq!(r.fill_blocking(&mut out), Err(ReserveError::NotReady));
        // Output untouched on the early-boot failure.
        assert_eq!(out, [0u8; 8]);
        assert_eq!(r.buffered(), 0);
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
    fn the_reserve_serves_from_the_fast_generator_the_drbg_keyed() {
        // Pins the chain: the bytes userland gets are cipher output under a
        // key the DRBG produced, not DRBG output directly.
        use crate::csprng::CsRng;
        use crate::fast::FastRng;
        use crate::rand::RandU64;
        let mut reference: FastRng<64> = CsRng::new(CountingSource::new(3))
            .expect("seed")
            .fork_fast()
            .expect("fork");
        let mut expected = [0u8; 100];
        reference.fill_bytes(&mut expected);

        let mut r = ready_reserve::<64>(3);
        let mut served = [0u8; 100];
        r.fill(&mut served).expect("serve");
        assert_eq!(served, expected);
    }

    #[test]
    fn a_request_larger_than_the_buffer_is_served_in_one_call() {
        let mut r = ready_reserve::<16>(99);
        let mut out = [0u8; 100];
        r.fill(&mut out).expect("served across refills");
        assert_ne!(out, [0u8; 100]);
        // No 16-byte window repeats: the refills continued the stream.
        for (i, a) in out.chunks(16).enumerate() {
            for b in out.chunks(16).skip(i + 1) {
                assert_ne!(a, b, "a refill repeated its predecessor's output");
            }
        }
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
    fn consumed_bytes_leave_only_the_unserved_remainder_buffered() {
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
    fn discard_wipes_buffered_output_and_rotates_the_key() {
        let mut r = ready_reserve::<64>(42);
        let mut out = [0u8; 8];
        r.fill(&mut out).unwrap();
        assert!(r.buffered() > 0);
        r.discard();
        assert_eq!(r.buffered(), 0, "suspend/clone boundary wipes the reserve");
        // Still ready after a discard — only the fast generator moved on.
        assert!(r.is_ready());
        r.fill(&mut out).expect("serves again after discard");
    }

    /// A discard on an unseeded reserve must not panic or make it look ready.
    #[test]
    fn discard_on_an_unseeded_reserve_is_a_no_op() {
        let mut r = OutputReserve::<CountingSource, 64>::new();
        r.discard();
        assert!(!r.is_ready());
    }

    #[test]
    fn a_cloned_reserve_cannot_continue_its_parents_stream() {
        // Model a fork: parent and child start identical, the child's
        // reserve is discarded, and from then on their streams differ.
        let mut parent = ready_reserve::<64>(7);
        let mut child = ready_reserve::<64>(7);
        let (mut p, mut c) = ([0u8; 8], [0u8; 8]);
        parent.fill(&mut p).unwrap();
        child.fill(&mut c).unwrap();
        assert_eq!(p, c, "the fork starts from one state");
        child.discard();
        let (mut pa, mut ca) = ([0u8; 128], [0u8; 128]);
        parent.fill(&mut pa).unwrap();
        child.fill(&mut ca).unwrap();
        assert_ne!(pa, ca, "a discarded child must not replay the parent");
    }

    #[test]
    fn reseed_succeeds_and_discards_buffer() {
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
        assert_eq!(r.reseed_blocking(), Err(ReserveError::NotReady));
    }

    #[test]
    fn reseed_failure_is_surfaced_not_hidden() {
        // Budget 1: only the instantiation seed succeeds. A subsequent
        // explicit reseed has no entropy left, and that must surface rather
        // than be hidden or replaced with weak randomness.
        let mut r = OutputReserve::<CountingSource, 16>::new();
        r.seed(CountingSource::with_budget(9, 1)).unwrap();
        // Serving still works — generation needs no fresh entropy at all…
        let mut out = [0u8; 8];
        r.fill(&mut out).expect("generation needs no fresh entropy");
        // …but the reseed boundary does, and surfaces the typed, transient
        // `Reseeding` with the generators left intact.
        assert_eq!(
            r.reseed(),
            Err(ReserveError::Entropy(EntropyError::Reseeding))
        );
        r.fill(&mut out)
            .expect("still serves after a failed reseed");
    }

    /// The perturbation must actually happen: once the cadence elapses, the
    /// reserve's stream must diverge from an unperturbed generator's.
    #[test]
    fn the_reserve_perturbs_its_generator_on_the_output_cadence() {
        let mut perturbing = ready_reserve::<2048>(11);
        // Budget 1 covers instantiation only, so this reserve's perturbation
        // reseed finds nothing and it keeps its original key.
        let mut starved = OutputReserve::<CountingSource, 2048>::new();
        starved.seed(CountingSource::with_budget(11, 1)).unwrap();

        // Before the cadence elapses both are the same generator.
        let (mut early_a, mut early_b) = ([0u8; 64], [0u8; 64]);
        perturbing.fill(&mut early_a).unwrap();
        starved.fill(&mut early_b).unwrap();
        assert_eq!(early_a, early_b, "no perturbation is due yet");

        let (mut late_a, mut late_b) = ([0u8; 64], [0u8; 64]);
        drain(&mut perturbing, PERTURB_INTERVAL_BYTES, &mut late_a);
        drain(&mut starved, PERTURB_INTERVAL_BYTES, &mut late_b);
        assert_ne!(
            late_a, late_b,
            "the reserve did not fold fresh entropy in on its cadence"
        );
    }

    /// A perturbation with no entropy available must defer, never deny the
    /// caller's bytes.
    #[test]
    fn a_perturbation_shortage_never_denies_a_draw() {
        let mut starved = OutputReserve::<CountingSource, 2048>::new();
        starved.seed(CountingSource::with_budget(5, 1)).unwrap();
        let mut tail = [0u8; 64];
        drain(&mut starved, PERTURB_INTERVAL_BYTES, &mut tail);
        assert_ne!(tail, [0u8; 64], "output must keep flowing");
        // And it keeps serving past the missed perturbation.
        starved
            .fill_blocking(&mut tail)
            .expect("a blocking fill serves too");
    }

    /// A source whose non-blocking `fill` is exhausted after `budget` draws
    /// but whose `fill_blocking` always delivers — a stand-in for a parking
    /// platform source, exercising the reserve's blocking paths.
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
    fn debug_does_not_leak_state() {
        extern crate alloc;
        use alloc::format;
        let r = ready_reserve::<64>(5);
        assert_eq!(
            format!("{r:?}"),
            "OutputReserve { ready: true, capacity: 64, buffered: 0, .. }"
        );
    }
}
