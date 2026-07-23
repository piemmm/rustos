//! The kernel random output reserve, composed into `KernelState`.
//!
//! TAIRiX has exactly one kernel cryptographic random subsystem. The
//! `random_get` syscall (`abi-v1` syscall 8) serves bytes from a bounded
//! reserve of CSPRNG **output** — `tairix_rng::OutputReserve` — refilled on
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

use alloc::sync::Arc;

use tairix_abi::Errno;
use tairix_arch_api::SchedulerArch;
use tairix_kernel_irq::IrqDispatchObserver;
use tairix_rng::{
    BootSeedSource, EntropyError, EntropySource, InterruptEntropyPool, InterruptPoolSource,
    JitterSource, MixedPair, OutputReserve, ReserveError, TimeSource, MAX_BOOT_SEED_LEN,
};
use tairix_sync::SpinLock;
use zeroize::Zeroize;

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
/// handle (x86 `RDSEED`/`RDRAND`, ARMv8.5 `RNDR`, …). It is **not** trusted
/// alone: the boot path XOR-mixes it with an independent [`ArchTicks`]-driven
/// timing-jitter source through [`KernelEntropy`] before it ever seeds the
/// reserve (the charter forbids a single trusted source). A port whose
/// hardware source produces no bytes contributes the XOR identity and the mix
/// falls back to whatever the other source supplies; only if *both* are
/// unavailable does the seed fail closed and the reserve stay unseeded —
/// never weakened to predictable bytes.
pub struct ArchEntropy {
    source: &'static dyn tairix_arch_api::PlatformEntropy,
}

impl ArchEntropy {
    /// Wrap a platform-entropy handle as an entropy source.
    #[must_use]
    pub fn new(source: &'static dyn tairix_arch_api::PlatformEntropy) -> Self {
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

/// A [`TimeSource`] over the Arch HAL's monotonic high-resolution counter
/// ([`SchedulerArch::ticks_now`] — x86 `RDTSC`, aarch64 `CNTPCT_EL0`, riscv64
/// `time`), driving the kernel's CPU-timing-jitter entropy source
/// ([`tairix_rng::JitterSource`]).
///
/// Holds an [`Arc`] of the arch handle (which lives for the whole kernel) so
/// the jitter source — owned by the reseeding reserve — can read the counter
/// on every seed and reseed. Architecture-neutral: the target-specific counter
/// read stays behind the HAL.
pub struct ArchTicks<A: SchedulerArch> {
    arch: Arc<A>,
}

impl<A: SchedulerArch> ArchTicks<A> {
    /// Build a time source over the shared arch handle.
    #[must_use]
    pub fn new(arch: Arc<A>) -> Self {
        Self { arch }
    }
}

impl<A: SchedulerArch> TimeSource for ArchTicks<A> {
    fn now(&mut self) -> u64 {
        self.arch.ticks_now()
    }
}

/// The one shared interrupt-arrival-timing entropy pool.
///
/// Fed a high-resolution timestamp on every interrupt dispatch by
/// [`IrqEntropyObserver`] (installed on the kernel `IrqTable`), and drained on
/// each CSPRNG reseed by the [`InterruptPoolSource`] half of
/// [`KernelEntropy`]. A `static` so the interrupt-context observer and the
/// reseeding reserve reference the exact same pool without a lock; all its
/// state is atomic (see [`InterruptEntropyPool`]).
pub static IRQ_ENTROPY_POOL: InterruptEntropyPool = InterruptEntropyPool::new();

/// The [`IrqDispatchObserver`] that feeds interrupt-arrival timing into
/// [`IRQ_ENTROPY_POOL`].
///
/// On each interrupt dispatch it reads the Arch HAL monotonic high-resolution
/// counter ([`SchedulerArch::ticks_now`]) and records it. The
/// physically-unpredictable low bits of successive interrupts' arrival timing
/// are an independent entropy input, mixed with the hardware RNG and the CPU
/// jitter source (never trusted alone). Recording is wait-free (one counter
/// read plus one atomic store), so the interrupt hot path pays only that.
pub struct IrqEntropyObserver<A: SchedulerArch> {
    arch: Arc<A>,
    pool: &'static InterruptEntropyPool,
}

impl<A: SchedulerArch> IrqEntropyObserver<A> {
    /// Build the observer over the shared arch handle and the pool it feeds
    /// (the kernel passes [`IRQ_ENTROPY_POOL`], the same pool the reseeding
    /// reserve drains).
    #[must_use]
    pub fn new(arch: Arc<A>, pool: &'static InterruptEntropyPool) -> Self {
        Self { arch, pool }
    }
}

impl<A: SchedulerArch + Send + Sync> IrqDispatchObserver for IrqEntropyObserver<A> {
    fn on_irq(&self, _line: u32) {
        // Only the arrival *timing* is sampled (not the line), so the pool's
        // repetition-count health test genuinely measures the timing source's
        // variance. Wait-free: a counter read and one atomic store.
        self.pool.record(self.arch.ticks_now());
    }
}

/// The kernel's entropy: the platform hardware RNG XOR-mixed with an
/// independent CPU-timing-jitter source, the asynchronous
/// interrupt-arrival-timing pool ([`IRQ_ENTROPY_POOL`]), *and* the
/// firmware-provided boot seed ([`BootSeedSource`]), so no source is trusted
/// alone. XOR is entropy-preserving for independent inputs, so a backdoored,
/// stuck, or observable source cannot lower the seed's quality below what the
/// others contribute, and vice versa. The interrupt pool contributes nothing
/// at boot (it fails closed until interrupts have flowed) and folds in fresh
/// timing on every reseed for forward secrecy; the boot seed is a one-shot
/// contribution consumed at the initial seed and wiped, so it is the source
/// of last resort that lets the reserve seed on an emulated/virtualised
/// machine whose CPU offers neither a hardware RNG nor usable timing jitter.
pub type KernelEntropy<A> = MixedPair<
    MixedPair<MixedPair<ArchEntropy, JitterSource<ArchTicks<A>>>, InterruptPoolSource<'static>>,
    BootSeedSource,
>;

/// Write-once latch holding the firmware/bootloader boot seed until the
/// CSPRNG is seeded from it.
///
/// The architecture boot path captures the seed it discovered — the device
/// tree's `/chosen/rng-seed` on an FDT platform — into this latch through
/// [`capture_boot_entropy_seed`], and [`take_boot_seed_source`] moves it into
/// the one-shot [`BootSeedSource`] the reserve seeds from, wiping the latch.
/// The latch keeps `lib/rng` and the seeding logic architecture-neutral: the
/// only per-arch part is *reading* the seed from the platform's boot data.
struct BootSeedLatch {
    seed: [u8; MAX_BOOT_SEED_LEN],
    /// Valid seed length in `seed`; `0` means no seed captured.
    len: usize,
    /// Set once the seed has been moved into the reserve, so a late or
    /// duplicate capture cannot resurrect consumed material.
    taken: bool,
}

/// The one boot-seed latch, written once at boot and consumed once at seed
/// time. Guarded by a spinlock because the capture (early boot, one CPU) and
/// the take (the seeding step) are distinct call sites; the critical section
/// is a short byte copy, never a steady-state spin.
static BOOT_SEED: SpinLock<BootSeedLatch> = SpinLock::new(BootSeedLatch {
    seed: [0u8; MAX_BOOT_SEED_LEN],
    len: 0,
    taken: false,
});

/// Capture up to [`MAX_BOOT_SEED_LEN`] bytes of firmware-provided boot-seed
/// material for the kernel CSPRNG seed.
///
/// Called by the architecture boot path with the bytes it read from the
/// platform's boot data (e.g. the FDT `/chosen/rng-seed`). Write-once: an
/// empty slice, a second capture, or a capture after the seed was consumed is
/// ignored, so no path can overwrite or resurrect the seed. The bytes are
/// *input material* only — [`seed_entropy_reserve`](crate::init) XOR-mixes
/// them with every other source and the DRBG conditions the result before any
/// caller sees output; they are never trusted alone.
pub fn capture_boot_entropy_seed(bytes: &[u8]) {
    let mut latch = BOOT_SEED.lock();
    if latch.taken || latch.len != 0 || bytes.is_empty() {
        return;
    }
    let n = bytes.len().min(MAX_BOOT_SEED_LEN);
    latch.seed[..n].copy_from_slice(&bytes[..n]);
    latch.len = n;
}

/// Move the captured boot seed into a one-shot [`BootSeedSource`], wiping the
/// latch. Returns [`BootSeedSource::empty`] when no seed was captured (the
/// common case on a machine with a working hardware RNG), so the mix simply
/// gains a source that contributes nothing.
pub(crate) fn take_boot_seed_source() -> BootSeedSource {
    let mut latch = BOOT_SEED.lock();
    if latch.taken || latch.len == 0 {
        return BootSeedSource::empty();
    }
    let source = BootSeedSource::new(&latch.seed[..latch.len]);
    latch.seed.zeroize();
    latch.len = 0;
    latch.taken = true;
    source
}

/// The reserve type the kernel installs once seeded, over the mixed
/// [`KernelEntropy`] source, default-capacity like [`BootReserve`] so both
/// share the one `Box<dyn RandomReserve>` field type.
pub type SeededReserve<A> = OutputReserve<KernelEntropy<A>>;

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
    use tairix_abi::Errno;
    use tairix_rng::{EntropyError, EntropySource, OutputReserve, ReserveError};

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

    impl tairix_arch_api::PlatformEntropy for StubPort {
        fn profile(&self) -> tairix_arch_api::EntropyProfile {
            tairix_arch_api::EntropyProfile {
                hardware_rng: tairix_arch_api::EntropySupport::Supported,
            }
        }
    }

    impl tairix_rng::HardwareRng for StubPort {
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

    /// A varying host clock (an LCG) standing in for a healthy
    /// high-resolution counter, so the jitter half of the mix is usable in a
    /// host test. Returned as a boxed `FnMut` so each test owns its own.
    fn varying_clock(seed: u64) -> impl FnMut() -> u64 {
        let mut lcg = seed;
        let mut now: u64 = 0;
        move || {
            lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            now = now.wrapping_add((lcg >> 40) | 1);
            now
        }
    }

    /// A lockstep host clock (constant delta): its jitter fails the health
    /// tests, modelling a platform with no usable timing jitter.
    fn lockstep_clock() -> impl FnMut() -> u64 {
        let mut now: u64 = 0;
        move || {
            now = now.wrapping_add(1);
            now
        }
    }

    #[test]
    fn mixed_reserve_seeds_from_hardware_when_jitter_is_unavailable() {
        use super::ArchEntropy;
        use tairix_rng::{JitterSource, MixedPair, OutputReserve};

        // Hardware works, jitter is dead (lockstep clock): the mix must still
        // seed from the hardware source alone — the fail-fallback direction.
        let jitter = JitterSource::new(lockstep_clock());
        let mixed = MixedPair::new(ArchEntropy::new(&FILLING_PORT), jitter);
        let mut reserve = OutputReserve::<_>::new();
        reserve.seed(mixed).expect("hardware alone seeds the mix");
        let mut out = [0u8; 16];
        RandomReserve::draw(&mut reserve, &mut out, true).expect("a seeded reserve serves");
        assert_ne!(out, [0u8; 16]);
    }

    #[test]
    fn mixed_reserve_seeds_from_jitter_when_hardware_is_dead() {
        use super::ArchEntropy;
        use tairix_rng::{JitterSource, MixedPair, OutputReserve};

        // Hardware is dead, jitter is healthy (varying clock): the mix must
        // still seed from the independent jitter source alone — this is the
        // defense-in-depth the "never trust one source" rule buys.
        let jitter = JitterSource::new(varying_clock(0xC0FF_EE00));
        let mixed = MixedPair::new(ArchEntropy::new(&DEAD_PORT), jitter);
        let mut reserve = OutputReserve::<_>::new();
        reserve.seed(mixed).expect("jitter alone seeds the mix");
        let mut out = [0u8; 16];
        RandomReserve::draw(&mut reserve, &mut out, true).expect("a seeded reserve serves");
        assert_ne!(out, [0u8; 16]);
    }

    #[test]
    fn mixed_reserve_fails_closed_when_both_sources_are_dead() {
        use super::ArchEntropy;
        use tairix_rng::{JitterSource, MixedPair, OutputReserve};

        // Neither source can supply bytes: the seed fails closed and the
        // reserve stays unseeded (never weakened to predictable bytes).
        let jitter = JitterSource::new(lockstep_clock());
        let mixed = MixedPair::new(ArchEntropy::new(&DEAD_PORT), jitter);
        let mut unseeded = OutputReserve::<_>::new();
        assert!(unseeded.seed(mixed).is_err());
        assert!(!unseeded.is_ready());
    }

    #[test]
    fn three_way_mix_seeds_from_the_interrupt_pool_alone() {
        // The full `KernelEntropy` shape, `MixedPair<MixedPair<hw, jitter>,
        // interrupt>`, with hardware dead and jitter dead: once enough fresh
        // interrupt-timing samples have arrived, the interrupt pool alone
        // seeds the reserve — the third independent source pulling its weight
        // in the "never trust one source" mix.
        use super::ArchEntropy;
        use tairix_rng::{
            InterruptEntropyPool, InterruptPoolSource, JitterSource, MixedPair, OutputReserve,
        };

        let pool = InterruptEntropyPool::new();
        // Build the source first (captures a zero baseline), then feed a full
        // fresh ring of varying samples so the freshness gate opens.
        let interrupt = InterruptPoolSource::new(&pool);
        let mut lcg = 0x1357_9BDFu64;
        for _ in 0..128 {
            lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            pool.record(lcg);
        }
        let hw_jitter = MixedPair::new(
            ArchEntropy::new(&DEAD_PORT),
            JitterSource::new(lockstep_clock()),
        );
        let mixed = MixedPair::new(hw_jitter, interrupt);
        let mut reserve = OutputReserve::<_>::new();
        reserve
            .seed(mixed)
            .expect("interrupt pool alone seeds the three-way mix");
        let mut out = [0u8; 16];
        RandomReserve::draw(&mut reserve, &mut out, true).expect("a seeded reserve serves");
        assert_ne!(out, [0u8; 16]);
    }

    #[test]
    fn three_way_mix_fails_closed_when_all_three_sources_are_dead() {
        // Hardware dead, jitter dead, and the interrupt pool empty (no fresh
        // samples): the seed must fail closed and the reserve stay unseeded.
        use super::ArchEntropy;
        use tairix_rng::{
            InterruptEntropyPool, InterruptPoolSource, JitterSource, MixedPair, OutputReserve,
        };

        let pool = InterruptEntropyPool::new();
        let interrupt = InterruptPoolSource::new(&pool);
        let hw_jitter = MixedPair::new(
            ArchEntropy::new(&DEAD_PORT),
            JitterSource::new(lockstep_clock()),
        );
        let mixed = MixedPair::new(hw_jitter, interrupt);
        let mut unseeded = OutputReserve::<_>::new();
        assert!(unseeded.seed(mixed).is_err());
        assert!(!unseeded.is_ready());
    }

    #[test]
    fn four_way_mix_seeds_from_the_boot_seed_alone() {
        // The exact production `KernelEntropy` shape with the firmware boot
        // seed as the fourth source: hardware dead (no `FEAT_RNG`), jitter
        // dead (deterministic emulated counter), interrupt pool empty (no
        // IRQs yet) — precisely the QEMU/emulated-machine boot. The boot seed
        // alone must seed the reserve, so `random_get`, the per-boot machine
        // id, and the ramzip sealing key all become available instead of the
        // reserve staying forever unseeded.
        use super::ArchEntropy;
        use tairix_rng::{
            BootSeedSource, InterruptEntropyPool, InterruptPoolSource, JitterSource, MixedPair,
            OutputReserve,
        };

        let pool = InterruptEntropyPool::new();
        let interrupt = InterruptPoolSource::new(&pool);
        let hw_jitter = MixedPair::new(
            ArchEntropy::new(&DEAD_PORT),
            JitterSource::new(lockstep_clock()),
        );
        let boot_seed = BootSeedSource::new(&[0x5Au8; 32]);
        let mixed = MixedPair::new(MixedPair::new(hw_jitter, interrupt), boot_seed);
        let mut reserve = OutputReserve::<_>::new();
        reserve
            .seed(mixed)
            .expect("the boot seed alone seeds the four-way mix");
        let mut out = [0u8; 16];
        RandomReserve::draw(&mut reserve, &mut out, true).expect("a seeded reserve serves");
        assert_ne!(out, [0u8; 16]);
    }

    #[test]
    fn four_way_mix_fails_closed_when_every_source_is_dead() {
        // Hardware dead, jitter dead, interrupt pool empty, and *no* boot seed
        // (an empty source): the seed must still fail closed and the reserve
        // stay unseeded — the boot seed rescues only a machine that actually
        // provided one, never weakens the fail-closed guarantee.
        use super::ArchEntropy;
        use tairix_rng::{
            BootSeedSource, InterruptEntropyPool, InterruptPoolSource, JitterSource, MixedPair,
            OutputReserve,
        };

        let pool = InterruptEntropyPool::new();
        let interrupt = InterruptPoolSource::new(&pool);
        let hw_jitter = MixedPair::new(
            ArchEntropy::new(&DEAD_PORT),
            JitterSource::new(lockstep_clock()),
        );
        let mixed = MixedPair::new(
            MixedPair::new(hw_jitter, interrupt),
            BootSeedSource::empty(),
        );
        let mut unseeded = OutputReserve::<_>::new();
        assert!(unseeded.seed(mixed).is_err());
        assert!(!unseeded.is_ready());
    }

    #[test]
    fn boot_seed_latch_captures_once_then_yields_a_source_and_wipes() {
        // The write-once latch: a capture makes `take` yield a seeded source,
        // and a second `take` yields an empty one (the seed is consumed and
        // wiped, never resurrected). This test is the sole toucher of the
        // process-global `BOOT_SEED` latch.
        use super::{capture_boot_entropy_seed, take_boot_seed_source};

        capture_boot_entropy_seed(&[0x11u8; 32]);
        // A second capture is ignored (write-once).
        capture_boot_entropy_seed(&[0x22u8; 32]);
        let first = take_boot_seed_source();
        assert!(
            first.has_seed(),
            "the captured seed is handed to the source"
        );
        let second = take_boot_seed_source();
        assert!(
            !second.has_seed(),
            "a consumed latch yields an empty source, never a replay"
        );
    }
}
