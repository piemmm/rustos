//! The kernel random output reserve, composed into `KernelState`.
//!
//! RustOS has exactly one kernel cryptographic random subsystem. The
//! `random_get` syscall (`abi-v1` syscall 8) serves bytes from a bounded
//! reserve of CSPRNG **output** — `rustos_rng::OutputReserve` — refilled on
//! demand and never weakened to a low-quality fallback.
//! This module is the thin seam that lets `KernelState` *hold* that reserve
//! without naming a concrete entropy source, and that maps a reserve failure
//! onto the stable random ABI errno.
//!
//! # Why a trait object
//!
//! [`OutputReserve<E, N>`] is generic over its entropy source `E` and buffer
//! size `N`, but the syscall handler must not be generic over them: it reads
//! the reserve through one borrow held in `KernelState`, and the concrete
//! entropy seam is supplied later (the platform-RNG `EntropySource`,
//! is still pending — see [`NullEntropy`]). [`RandomReserve`] is the
//! object-safe view the handler depends on; `KernelState` stores a
//! `Box<dyn RandomReserve + Send + Sync>` and swaps in a seeded reserve when
//! the entropy seam lands, without touching the `random_get` ABI signature
//! (no interface creep — security by default).
//!
//! # Fail closed
//!
//! Until the reserve is seeded it is **not ready**: a draw returns
//! [`ReserveError::NotReady`], which the handler maps to
//! [`Errno::EntropyNotReady`] (before the kernel RNG is
//! initialised a non-blocking request fails closed rather than returning
//! weak randomness). A draw never substitutes predictable bytes.

use rustos_abi::Errno;
use rustos_rng::{EntropyError, EntropySource, OutputReserve, ReserveError};

/// Object-safe view of the kernel's CSPRNG output reserve.
///
/// One method: draw `out.len()` cryptographically secure bytes, choosing the
/// fallible (`non_blocking`) or blocking reserve path. It exists so
/// `KernelState` can hold the reserve as a `Box<dyn RandomReserve + Send +
/// Sync>` while the concrete entropy source `E` stays an implementation
/// detail of whatever seeds it (the platform entropy
/// seam is the only architecture-aware part). The blanket impl below is the
/// single bridge to [`OutputReserve`]; there is no second drawing path.
pub trait RandomReserve {
    /// Fill `out` with cryptographically secure random bytes.
    ///
    /// When `non_blocking` is set, a required reseed that has no fresh
    /// entropy fails closed (the fallible [`OutputReserve::fill`]); otherwise
    /// the call waits through the reseed (the blocking
    /// [`OutputReserve::fill_blocking`]). Generation from a *seeded* reserve
    /// never blocks for entropy.
    ///
    /// # Errors
    ///
    /// * [`ReserveError::NotReady`] before the reserve is seeded.
    /// * [`ReserveError::Entropy`] when a required reseed has no entropy. On
    ///   error `out` is left zeroed rather than partially filled.
    fn draw(&mut self, out: &mut [u8], non_blocking: bool) -> Result<(), ReserveError>;
}

impl<E, const N: usize> RandomReserve for OutputReserve<E, N>
where
    E: EntropySource + Send + Sync,
{
    fn draw(&mut self, out: &mut [u8], non_blocking: bool) -> Result<(), ReserveError> {
        if non_blocking {
            self.fill(out)
        } else {
            self.fill_blocking(out)
        }
    }
}

/// The unseeded boot entropy source.
///
/// `KernelState` boots with an [`OutputReserve`] parameterised over this
/// source so a reserve always exists, but it is **never seeded from it**:
/// every draw reports [`EntropyError::Unavailable`], so the reserve stays
/// [`ReserveError::NotReady`] and `random_get` fails closed with
/// [`Errno::EntropyNotReady`] until the platform-RNG `EntropySource` (
/// the same seam the encrypted-swap key is drawn from) is installed and
/// the reserve re-seeded. A genuinely dead source is exactly what says a blocking draw must fail closed on rather than park forever.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullEntropy;

impl EntropySource for NullEntropy {
    fn fill(&mut self, _out: &mut [u8]) -> Result<(), EntropyError> {
        Err(EntropyError::Unavailable)
    }
}

/// The reserve type `KernelState` boots with: a default-capacity reserve over
/// the unseeded [`NullEntropy`] source.
pub type BootReserve = OutputReserve<NullEntropy>;

/// Adapts an Arch HAL platform-entropy handle into an [`EntropySource`] so it
/// can seed (and, for forward secrecy, reseed) the kernel CSPRNG output
/// reserve.
///
/// The wrapped `&'static dyn PlatformEntropy` is the per-port hardware-RNG
/// handle (x86 `RDSEED`/`RDRAND`, ARMv8.5 `RNDR`, …). The boot path installs
/// a [`SeededReserve`] built from it only when a draw actually produces bytes;
/// a port with no usable source fails the draw closed and the kernel reserve
/// stays unseeded, exactly as it boots — never weakened to predictable bytes.
///
/// This is the kernel's one entropy input today. The charter forbids trusting
/// a single source alone, so additional software inputs (boot-time timing
/// jitter, an interrupt-arrival pool) mixed via `rustos_rng::CombinedSource`
/// are a tracked follow-up; until then the hardware source feeds the DRBG,
/// whose output is what callers actually receive.
pub struct ArchEntropy {
    source: &'static dyn rustos_arch_api::PlatformEntropy,
}

impl ArchEntropy {
    /// Wrap a platform-entropy handle as an entropy source.
    #[must_use]
    pub fn new(source: &'static dyn rustos_arch_api::PlatformEntropy) -> Self {
        Self { source }
    }
}

impl EntropySource for ArchEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        // The handle's `try_fill` already retries a momentarily-underfull
        // hardware source a bounded number of times and fails closed; there
        // is no extra retry or fallback here (no weakening).
        self.source.try_fill(out)
    }
}

/// The reserve type the kernel installs once a platform entropy source is
/// available, type-matched (default capacity) to [`BootReserve`] so both
/// share the one `Box<dyn RandomReserve>` field type.
pub type SeededReserve = OutputReserve<ArchEntropy>;

/// Map a reserve failure onto the stable random ABI errno.
///
/// Both the pre-seed [`ReserveError::NotReady`] and a reseed-time
/// [`ReserveError::Entropy`] shortage collapse onto
/// [`Errno::EntropyNotReady`]: the caller learns "no cryptographically secure
/// bytes are available right now" and fails closed or retries, exactly as the
/// `random_get` contract promises (`lib/abi`'s `random` module). The handler
/// never distinguishes the two as a randomness oracle. [`ReserveError`] is
/// `#[non_exhaustive]`, so any future variant also fails closed to the same
/// errno rather than leaking through.
#[must_use]
pub fn reserve_errno(_err: ReserveError) -> Errno {
    // Every reserve failure — the pre-seed `NotReady`, a transient reseed
    // `Entropy` shortage, or any future `#[non_exhaustive]` variant — means
    // "no cryptographically secure bytes right now" and collapses onto one
    // errno, so the cause is never an oracle.
    Errno::EntropyNotReady
}

#[cfg(test)]
mod tests {
    use super::{reserve_errno, BootReserve, NullEntropy, RandomReserve};
    use rustos_abi::Errno;
    use rustos_rng::{EntropyError, EntropySource, OutputReserve, ReserveError};

    /// Deterministic stand-in for a seeded entropy source (not real entropy):
    /// a counter so the reserve's drawing behaviour is reproducible in tests.
    struct CountingSource(u64);

    impl EntropySource for CountingSource {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            for byte in out.iter_mut() {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *byte = self.0.to_le_bytes()[4];
            }
            Ok(())
        }
    }

    #[test]
    fn boot_reserve_is_unseeded_and_draws_not_ready() {
        let mut reserve = BootReserve::new();
        assert!(!reserve.is_ready());
        let mut out = [0u8; 8];
        assert_eq!(reserve.draw(&mut out, true), Err(ReserveError::NotReady));
        // The fail-closed draw leaves the buffer untouched.
        assert_eq!(out, [0u8; 8]);
    }

    #[test]
    fn null_entropy_is_always_unavailable() {
        let mut out = [0u8; 4];
        assert_eq!(NullEntropy.fill(&mut out), Err(EntropyError::Unavailable));
    }

    #[test]
    fn seeded_reserve_draws_through_the_object_safe_seam() {
        let mut reserve = OutputReserve::<CountingSource, 64>::new();
        reserve
            .seed(CountingSource(7))
            .expect("the deterministic source seeds");
        let mut out = [0u8; 16];
        RandomReserve::draw(&mut reserve, &mut out, true).expect("a seeded reserve serves");
        assert_ne!(out, [0u8; 16], "must produce non-zero output");
    }

    #[test]
    fn reserve_errors_map_to_entropy_not_ready() {
        assert_eq!(
            reserve_errno(ReserveError::NotReady),
            Errno::EntropyNotReady
        );
        assert_eq!(
            reserve_errno(ReserveError::Entropy(EntropyError::Reseeding)),
            Errno::EntropyNotReady
        );
        assert_eq!(
            reserve_errno(ReserveError::Entropy(EntropyError::Unavailable)),
            Errno::EntropyNotReady
        );
    }

    /// A stub Arch HAL platform-entropy handle for the `ArchEntropy` tests:
    /// either fills with a deterministic non-zero pattern or fails closed.
    struct StubPort {
        fills: bool,
    }

    impl rustos_arch_api::PlatformEntropy for StubPort {
        fn profile(&self) -> rustos_arch_api::EntropyProfile {
            rustos_arch_api::EntropyProfile {
                hardware_rng: rustos_arch_api::EntropySupport::Supported,
            }
        }
    }

    impl rustos_rng::HardwareRng for StubPort {
        fn try_fill(&self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.fills {
                // Deterministic non-zero pattern via a wrapping `u8` counter
                // (no `usize`-to-`u8` cast); not real entropy, just enough for
                // the DRBG to seed and produce distinct output in the test.
                let mut acc: u8 = 7;
                for b in out.iter_mut() {
                    *b = acc;
                    acc = acc.wrapping_mul(31).wrapping_add(13);
                }
                Ok(())
            } else {
                Err(EntropyError::Unavailable)
            }
        }
    }

    static FILLING_PORT: StubPort = StubPort { fills: true };
    static DEAD_PORT: StubPort = StubPort { fills: false };

    #[test]
    fn arch_entropy_forwards_to_the_platform_handle() {
        use super::ArchEntropy;
        let mut src = ArchEntropy::new(&FILLING_PORT);
        let mut out = [0u8; 32];
        src.fill(&mut out).expect("the filling stub supplies bytes");
        assert_ne!(out, [0u8; 32]);

        let mut dead = ArchEntropy::new(&DEAD_PORT);
        assert_eq!(dead.fill(&mut out), Err(EntropyError::Unavailable));
    }

    #[test]
    fn seeded_reserve_over_arch_entropy_serves_then_fails_closed_when_dead() {
        use super::{ArchEntropy, SeededReserve};
        // A working source seeds the reserve and it serves cryptographic bytes.
        let mut reserve = SeededReserve::new();
        reserve
            .seed(ArchEntropy::new(&FILLING_PORT))
            .expect("a working platform source seeds the reserve");
        let mut out = [0u8; 16];
        RandomReserve::draw(&mut reserve, &mut out, true).expect("a seeded reserve serves");
        assert_ne!(out, [0u8; 16]);

        // A dead source cannot seed: the reserve stays unseeded (fail closed).
        let mut unseeded = SeededReserve::new();
        assert!(unseeded.seed(ArchEntropy::new(&DEAD_PORT)).is_err());
        assert!(!unseeded.is_ready());
    }
}
