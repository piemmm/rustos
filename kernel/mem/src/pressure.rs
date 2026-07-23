//! VM pressure bands and reclaim ordering (`plans/SMARTRAM.md` SMART2,
//! `plans/SWAPSWAPSWAP.md` section 6).
//!
//! This module is the one definition of the system's memory-pressure
//! state and of the order reclaimable caches are shrunk in as pressure
//! rises. The band vocabulary (normal, mild, moderate, severe,
//! critical) is shared with `plans/SWAPSWAPSWAP.md`; there is no
//! parallel vocabulary and no second pressure model.
//!
//! # The gauge
//!
//! [`MemoryPressure`] samples a [`FreeMemorySource`] — in production
//! the physical [`FrameAllocator`], whose free-frame count is the
//! authoritative "how much RAM is left" figure — and folds the reading
//! into a banded state machine with hysteresis: each band is entered
//! below one watermark and left above a strictly higher one, so a
//! reading that hovers on a single threshold cannot oscillate the
//! band. Sampling happens on the caller's own operations (a cache
//! consults the gauge as it works); there is no background worker and
//! no periodic tick.
//!
//! # Reclaim ordering and the `ramzip` handoff
//!
//! [`shrink_target`] maps a band and a [`ReclaimClass`] to the byte
//! ceiling that class must shrink to, following `plans/SMARTRAM.md`
//! section 7: disposable and speculative classes drop at mild
//! pressure, clean file data begins reclaim at mild and finishes at
//! moderate together with transform cache, metadata and recovery
//! assist are preserved longest, and at severe or critical pressure
//! every class obeys a forced shrink to zero. [`ramzip_handoff`] fixes
//! the ordering `plans/SWAPSWAPSWAP.md` requires: cold anonymous pages
//! are compressed only from moderate pressure onward, and at moderate
//! only once clean and transform cache have been reclaimed —
//! reconstructable clean cache is always cheaper than encrypted
//! compressed anonymous storage. [`escalation`] is the deterministic
//! next step when reclaim cannot help.
//!
//! # Reserves
//!
//! The thresholds carry a reserve floor derived from the backing size.
//! A reading at or below the reserve is critical pressure regardless
//! of band history, and [`MemoryPressure::growth_permitted`] refuses
//! any cache growth that would dip into the reserve — cache expansion
//! can never be the cause of reserve exhaustion. A backing whose size
//! is unknown (zero) reports critical pressure and admits nothing:
//! fail closed, never a guess.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::frame::{FrameAllocator, PAGE_SIZE, RESERVE_DIVISOR};
use crate::reclaim::{CacheBudget, ReclaimClass};

/// The system memory-pressure band, shared with
/// `plans/SWAPSWAPSWAP.md` section 6. Ordered: a later variant is
/// deeper pressure.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum PressureBand {
    /// Reserves are protected and free memory is plentiful: bounded
    /// opportunistic cache growth is allowed.
    Normal,
    /// Free memory is tightening: speculative growth stops, disposable
    /// caches drop, and clean file cache begins reclaim.
    Mild,
    /// Clean file and transform cache finish reclaim; only then are
    /// cold anonymous pages handed to `ramzip` for compression.
    Moderate,
    /// Every cache class obeys forced shrink requests; `ramzip` grows
    /// toward its cap under its own reserve rules.
    Severe,
    /// No speculative work runs; escalation belongs to the VM
    /// pressure policy.
    Critical,
}

impl PressureBand {
    /// Every band, shallowest first.
    pub const ALL: [Self; 5] = [
        Self::Normal,
        Self::Mild,
        Self::Moderate,
        Self::Severe,
        Self::Critical,
    ];

    /// The band's depth: 0 is normal, 4 is critical.
    #[must_use]
    pub const fn depth(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Mild => 1,
            Self::Moderate => 2,
            Self::Severe => 3,
            Self::Critical => 4,
        }
    }

    /// The band at `depth`, clamped into range so a stored raw value
    /// can never decode to an out-of-model state.
    #[must_use]
    pub const fn from_depth(depth: u8) -> Self {
        match depth {
            0 => Self::Normal,
            1 => Self::Mild,
            2 => Self::Moderate,
            3 => Self::Severe,
            _ => Self::Critical,
        }
    }

    /// The next shallower band (normal relaxes to itself).
    const fn relaxed(self) -> Self {
        Self::from_depth(self.depth().saturating_sub(1))
    }
}

/// A live "how much memory is left" reading the gauge samples.
///
/// The production source is the physical [`FrameAllocator`]; tests
/// inject a controllable double. Both figures are byte counts of the
/// same backing resource, so `free <= total` for an honest source.
pub trait FreeMemorySource: Sync {
    /// Bytes of the backing resource currently free.
    fn free_bytes(&self) -> usize;
    /// Total bytes of the backing resource.
    fn total_bytes(&self) -> usize;
}

impl FreeMemorySource for FrameAllocator {
    fn free_bytes(&self) -> usize {
        self.free_frames().saturating_mul(PAGE_SIZE)
    }

    fn total_bytes(&self) -> usize {
        // Usable frames, not the bitmap's address-space extent: holes
        // and reserved regions are not memory pressure can spend.
        self.usable_frames().saturating_mul(PAGE_SIZE)
    }
}

/// Watermark denominators for each band's *enter* threshold, as a
/// fraction of the backing size. These are initial implementation
/// targets in the shape `plans/SWAPSWAPSWAP.md` section 6 sketches
/// (moderate — the compression band — starts below ~10% free and stops
/// above ~14%); they are not ABI, and tuning them against the section
/// 14 benchmarks only changes these constants.
const MILD_ENTER_DIVISOR: usize = 5; // enter below 20% free
const MODERATE_ENTER_DIVISOR: usize = 10; // enter below 10% free
const SEVERE_ENTER_DIVISOR: usize = 16; // enter below 6.25% free
const CRITICAL_ENTER_DIVISOR: usize = 32; // enter below 3.125% free

/// Watermark fractions for each band's *exit* threshold. Every exit
/// sits strictly above its band's enter watermark (the hysteresis gap)
/// and strictly below the next shallower band's enter watermark, so
/// relaxing one band always lands inside the shallower band's range.
const MILD_EXIT_DIVISOR: usize = 4; // leave above 25% free
const MODERATE_EXIT_NUMERATOR: usize = 7; // leave above 14% free
const MODERATE_EXIT_DIVISOR: usize = 50;
const SEVERE_EXIT_NUMERATOR: usize = 2; // leave above 8% free
const SEVERE_EXIT_DIVISOR: usize = 25;
const CRITICAL_EXIT_DIVISOR: usize = 20; // leave above 5% free

// The reserve floor as a fraction of the backing (below this the system
// is critical regardless of band history, and no cache growth may dip
// into it) is the same fraction the frame allocator holds back from user
// commits, so the two floors can never diverge: `RESERVE_DIVISOR` is
// defined once in `crate::frame` and imported above.

/// The per-band enter/exit watermarks and the reserve floor, in bytes,
/// derived from the size of the backing resource — never free-standing
/// magic numbers, so a small board and a large server both get a
/// proportionate policy from the same derivation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PressureThresholds {
    total: usize,
    reserve: usize,
    /// Enter watermarks for mild, moderate, severe, critical: the band
    /// is entered when the free reading drops below its watermark.
    enter: [usize; 4],
    /// Exit watermarks for mild, moderate, severe, critical: the band
    /// is left when the free reading rises above its watermark.
    exit: [usize; 4],
}

impl PressureThresholds {
    /// Derive the thresholds from the byte size of the backing
    /// resource. A zero backing yields thresholds that report critical
    /// pressure for every reading: an unknown backing admits nothing.
    #[must_use]
    pub const fn from_total(total: usize) -> Self {
        Self {
            total,
            reserve: total / RESERVE_DIVISOR,
            enter: [
                total / MILD_ENTER_DIVISOR,
                total / MODERATE_ENTER_DIVISOR,
                total / SEVERE_ENTER_DIVISOR,
                total / CRITICAL_ENTER_DIVISOR,
            ],
            exit: [
                total / MILD_EXIT_DIVISOR,
                total / MODERATE_EXIT_DIVISOR * MODERATE_EXIT_NUMERATOR,
                total / SEVERE_EXIT_DIVISOR * SEVERE_EXIT_NUMERATOR,
                total / CRITICAL_EXIT_DIVISOR,
            ],
        }
    }

    /// The reserve floor in bytes.
    #[must_use]
    pub const fn reserve(&self) -> usize {
        self.reserve
    }

    /// Byte size of the backing resource the thresholds were derived
    /// from.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.total
    }

    /// Enter watermarks (bytes free) for mild, moderate, severe,
    /// critical, in depth order: the band is entered when the free
    /// reading drops below its watermark. Reported values (the policy
    /// actually in force), never promises.
    #[must_use]
    pub const fn enter_watermarks(&self) -> [usize; 4] {
        self.enter
    }

    /// Exit watermarks (bytes free) for mild, moderate, severe,
    /// critical, in depth order: the band is left when the free reading
    /// rises above its watermark (the hysteresis gap).
    #[must_use]
    pub const fn exit_watermarks(&self) -> [usize; 4] {
        self.exit
    }

    /// The warm-up *start* watermark: opportunistic decompression
    /// (fault clustering, background warm-up) may begin only while the
    /// free reading is above this — the mild band's exit watermark, so
    /// "comfortably above" is the same figure that fully relaxes mild
    /// pressure (`plans/SWAPSWAPSWAP.md` section 6).
    #[must_use]
    pub const fn warmup_start(&self) -> usize {
        self.exit[0]
    }

    /// The warm-up *stop* watermark: a running warm-up step stops as
    /// soon as the free reading falls to this — the mild band's enter
    /// watermark, strictly below [`Self::warmup_start`], giving the
    /// warm-up path its own hysteresis gap distinct from the
    /// compression thresholds.
    #[must_use]
    pub const fn warmup_stop(&self) -> usize {
        self.enter[0]
    }

    /// The band a free reading maps to with no hysteresis history: the
    /// deepest band whose enter watermark the reading is below. A zero
    /// backing or a reading inside the reserve is critical.
    const fn raw_band(&self, free: usize) -> PressureBand {
        if self.total == 0 || free <= self.reserve {
            return PressureBand::Critical;
        }
        // Deepest first: the enter watermarks strictly decrease with
        // depth, so the first match is the deepest applicable band.
        if free < self.enter[3] {
            PressureBand::Critical
        } else if free < self.enter[2] {
            PressureBand::Severe
        } else if free < self.enter[1] {
            PressureBand::Moderate
        } else if free < self.enter[0] {
            PressureBand::Mild
        } else {
            PressureBand::Normal
        }
    }

    /// The exit watermark of `band` (undefined for normal, which is
    /// never relaxed out of; callers only ask for entered bands).
    const fn exit_of(&self, band: PressureBand) -> usize {
        match band {
            PressureBand::Normal => 0,
            PressureBand::Mild => self.exit[0],
            PressureBand::Moderate => self.exit[1],
            PressureBand::Severe => self.exit[2],
            PressureBand::Critical => self.exit[3],
        }
    }

    /// Fold one free reading into the current band with hysteresis:
    /// deepening applies immediately; relaxing steps one band at a
    /// time and only past the departing band's exit watermark.
    ///
    /// Deterministic for equal inputs: a pure function of
    /// `(current, free)`.
    const fn fold(&self, current: PressureBand, free: usize) -> PressureBand {
        let raw = self.raw_band(free);
        if raw.depth() >= current.depth() {
            return raw;
        }
        let mut band = current;
        while band.depth() > raw.depth() && free > self.exit_of(band) {
            band = band.relaxed();
        }
        band
    }
}

/// The banded memory-pressure gauge: one shared state machine over one
/// [`FreeMemorySource`], sampled by its consumers on their own
/// operations (no background worker, no tick).
///
/// The published band is a single atomic, so concurrent samplers never
/// block each other; two racing samples fold the same reading to the
/// same band, and the reserve floor is re-checked on every growth
/// decision, so a lost store can never admit growth into the reserve.
///
/// Each stored band change bumps the entered band's transition counter
/// (a saturation-free `u64` per band), so pressure-state transitions
/// stay observable through the internal diagnostics without any
/// background worker or public ABI (`plans/SMARTRAM.md` SMART9).
pub struct MemoryPressure {
    source: &'static (dyn FreeMemorySource + 'static),
    thresholds: PressureThresholds,
    band: AtomicU8,
    /// Entries into each band, indexed by [`PressureBand::depth`]. The
    /// starting band is not counted: the gauge begins there rather
    /// than transitioning into it.
    transitions: [AtomicU64; 5],
}

impl MemoryPressure {
    /// Build the gauge over the backing's live source, deriving the
    /// thresholds from its total size and starting from the band the
    /// first reading maps to.
    #[must_use]
    pub fn over(source: &'static (dyn FreeMemorySource + 'static)) -> Self {
        let thresholds = PressureThresholds::from_total(source.total_bytes());
        let band = thresholds.raw_band(source.free_bytes());
        Self {
            source,
            thresholds,
            band: AtomicU8::new(band.depth()),
            transitions: [const { AtomicU64::new(0) }; 5],
        }
    }

    /// The thresholds the gauge folds readings against.
    #[must_use]
    pub const fn thresholds(&self) -> PressureThresholds {
        self.thresholds
    }

    /// The band of the most recent sample, without taking a reading.
    #[must_use]
    pub fn band(&self) -> PressureBand {
        PressureBand::from_depth(self.band.load(Ordering::Relaxed))
    }

    /// Take a fresh reading and fold it into the band with hysteresis,
    /// returning the resulting band. A sample that changes the stored
    /// band counts one entry into the new band; a sample that holds the
    /// band counts nothing.
    pub fn sample(&self) -> PressureBand {
        let free = self.source.free_bytes();
        let next = self.thresholds.fold(self.band(), free);
        let previous = self.band.swap(next.depth(), Ordering::Relaxed);
        if previous != next.depth() {
            self.transitions[next.depth() as usize].fetch_add(1, Ordering::Relaxed);
        }
        next
    }

    /// How many sampled transitions have entered `band` since the gauge
    /// was built. The band the gauge started in is not counted.
    #[must_use]
    pub fn band_entries(&self, band: PressureBand) -> u64 {
        self.transitions[band.depth() as usize].load(Ordering::Relaxed)
    }

    /// The backing's live free-byte reading.
    #[must_use]
    pub fn free_bytes(&self) -> usize {
        self.source.free_bytes()
    }

    /// Byte size of the backing resource the gauge watches.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.source.total_bytes()
    }

    /// Whether a cache may grow by `cost_bytes` right now: growth is
    /// permitted only at normal pressure, and never when it would take
    /// the free reading to (or below) the reserve floor — cache
    /// expansion can never be the cause of reserve exhaustion.
    pub fn growth_permitted(&self, cost_bytes: usize) -> bool {
        if !matches!(self.sample(), PressureBand::Normal) {
            return false;
        }
        self.source.free_bytes().saturating_sub(cost_bytes) > self.thresholds.reserve
    }
}

/// The byte ceiling `class` must shrink to at `band`, against the
/// cache's own [`CacheBudget`] (`plans/SMARTRAM.md` section 7).
///
/// The ordering is the plan's, deterministic and pure:
///
/// - **normal** — no forced shrink; growth runs to the hard limit.
/// - **mild** — disposable and speculative classes (`DisposableUi`,
///   `PredictivePrefetch`, `BackgroundValidation`) drop outright;
///   semantic, runtime, and clean-file classes shrink to the low
///   watermark; transform cache, metadata, and recovery assist are
///   preserved.
/// - **moderate** — clean file and transform cache (and everything
///   cheaper) finish reclaim; only hot metadata and recovery assist
///   are preserved, and only to the low watermark.
/// - **severe / critical** — every class obeys a forced shrink to
///   zero.
#[must_use]
pub const fn shrink_target(band: PressureBand, class: ReclaimClass, budget: CacheBudget) -> usize {
    match band {
        PressureBand::Normal => budget.hard(),
        PressureBand::Mild => match class {
            ReclaimClass::DisposableUi
            | ReclaimClass::PredictivePrefetch
            | ReclaimClass::BackgroundValidation => 0,
            ReclaimClass::SemanticAppCache
            | ReclaimClass::RuntimeCache
            | ReclaimClass::CleanFileData => budget.low(),
            ReclaimClass::TransformCache
            | ReclaimClass::FsMetadata
            | ReclaimClass::ReliabilityAssist => budget.hard(),
        },
        PressureBand::Moderate => match class {
            ReclaimClass::FsMetadata | ReclaimClass::ReliabilityAssist => budget.low(),
            _ => 0,
        },
        PressureBand::Severe | PressureBand::Critical => 0,
    }
}

/// Whether `ramzip` may compress cold anonymous pages at `band`, given
/// the clean-file plus transform cache bytes still resident.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RamzipHandoff {
    /// Compression is not the next step: either pressure is too
    /// shallow for `ramzip` activity, or cheaper clean/transform cache
    /// must be reclaimed first.
    HoldCompression,
    /// Cold eligible anonymous pages may be compressed, under the
    /// reserve, cap, and fail-closed rules `plans/SWAPSWAPSWAP.md`
    /// owns.
    CompressColdAnonymous,
}

/// The handoff ordering to `ramzip` (`plans/SWAPSWAPSWAP.md` section
/// 6): no compression at normal or mild pressure; at moderate pressure
/// compression starts only once clean file and transform cache have
/// been reclaimed (reconstructable clean cache is cheaper than
/// encrypted compressed anonymous storage); at severe pressure
/// `ramzip` owns cold-anonymous policy regardless of cache residue; at
/// critical pressure speculative work stops and [`escalation`] owns
/// the next step.
///
/// This is the integration seam `plans/SWAPSWAPSWAP.md` SWAP3 binds
/// to; until the `ramzip` store lands, the ordering is enforced
/// against the caches alone (their shrink targets already reach zero
/// before this gate opens).
#[must_use]
pub const fn ramzip_handoff(band: PressureBand, clean_and_transform_bytes: usize) -> RamzipHandoff {
    match band {
        PressureBand::Normal | PressureBand::Mild | PressureBand::Critical => {
            RamzipHandoff::HoldCompression
        }
        PressureBand::Moderate => {
            if clean_and_transform_bytes == 0 {
                RamzipHandoff::CompressColdAnonymous
            } else {
                RamzipHandoff::HoldCompression
            }
        }
        PressureBand::Severe => RamzipHandoff::CompressColdAnonymous,
    }
}

/// The compress-out reclaim batch, in pages, for one direct-reclaim
/// sweep at `band` (`plans/SWAPSWAPSWAP.md` section 6, section 11 —
/// bounded, benchmarked, never ABI).
///
/// This bounds the work one triggering event (a demand fault under
/// pressure) may spend scanning and compressing the faulting task's own
/// cold anonymous pages: a small, fixed batch keeps direct reclaim off
/// the critical path (the sweep is amortised across many faults, never a
/// single unbounded pass) while still relieving pressure in step with how
/// deep it is.
///
/// The batch is zero except where compression is actually the answer,
/// mirroring [`ramzip_handoff`]:
///
/// - **normal / mild** — compression never runs here (cheaper cache
///   reclaim and speculative-growth stops handle these bands), so the
///   batch is zero and the caller does no reclaim work at all.
/// - **moderate** — a modest batch: cold anonymous pages begin
///   compressing once the cheaper caches have drained.
/// - **severe** — a larger batch: reserves are close, so a triggering
///   fault reclaims harder, still bounded.
/// - **critical** — zero: speculative work stops and [`escalation`]
///   hands the next step to the VM policy (freeze / kill / clean OOM),
///   not to more compression.
///
/// Pure and deterministic for equal inputs.
#[must_use]
pub const fn ramzip_reclaim_batch(band: PressureBand) -> usize {
    match band {
        PressureBand::Normal | PressureBand::Mild | PressureBand::Critical => 0,
        PressureBand::Moderate => MODERATE_RECLAIM_BATCH_PAGES,
        PressureBand::Severe => SEVERE_RECLAIM_BATCH_PAGES,
    }
}

/// Direct-reclaim batch at moderate pressure: 32 pages (128 KiB with a
/// 4 KiB page). Small enough that the scan+compress cost is a negligible
/// addition to a fault that was already going to allocate, large enough
/// that a task steadily faulting under moderate pressure makes real
/// headroom over successive faults. Not ABI; a tuning constant validated
/// against the section 19 benchmarks.
const MODERATE_RECLAIM_BATCH_PAGES: usize = 32;

/// Direct-reclaim batch at severe pressure: 128 pages (512 KiB). Reserves
/// are close, so a triggering fault reclaims four times as hard as at
/// moderate — still a bounded pass, never an unbounded sweep. Not ABI.
const SEVERE_RECLAIM_BATCH_PAGES: usize = 128;

/// The deterministic next step when the VM must free memory.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EscalationStep {
    /// No action is needed at this band.
    Hold,
    /// Shrink reclaimable caches to their band targets first.
    ReclaimCaches,
    /// Caches are drained: hand cold anonymous pages to `ramzip`
    /// under its own reserve and cap rules.
    HandOffToRamzip,
    /// Reclaim cannot help and compression is no longer the answer:
    /// escalate through the VM pressure policy (lower-tier swap where
    /// approved, freeze or kill selected tasks, or a clean OOM) —
    /// deterministically, never a panic or a retry loop.
    VmPolicy,
}

/// The escalation order when reclaim cannot help
/// (`plans/SMARTRAM.md` section 7, `plans/SWAPSWAPSWAP.md` section 6):
/// reclaimable cache is always the first answer while any remains;
/// with the caches drained, moderate and severe pressure hand off to
/// `ramzip`, and critical pressure escalates to the VM policy.
///
/// Pure and deterministic for equal inputs — two callers observing the
/// same band and residue compute the same step.
#[must_use]
pub const fn escalation(band: PressureBand, reclaimable_bytes: usize) -> EscalationStep {
    match band {
        PressureBand::Normal => EscalationStep::Hold,
        PressureBand::Mild => {
            if reclaimable_bytes > 0 {
                EscalationStep::ReclaimCaches
            } else {
                EscalationStep::Hold
            }
        }
        PressureBand::Moderate | PressureBand::Severe => {
            if reclaimable_bytes > 0 {
                EscalationStep::ReclaimCaches
            } else {
                EscalationStep::HandOffToRamzip
            }
        }
        PressureBand::Critical => {
            if reclaimable_bytes > 0 {
                EscalationStep::ReclaimCaches
            } else {
                EscalationStep::VmPolicy
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    extern crate std;
    use core::sync::atomic::AtomicUsize;
    use std::boxed::Box;

    /// A controllable memory reading: total fixed, free adjustable.
    struct FakeSource {
        total: usize,
        free: AtomicUsize,
    }

    impl FreeMemorySource for FakeSource {
        fn free_bytes(&self) -> usize {
            self.free.load(Ordering::Relaxed)
        }

        fn total_bytes(&self) -> usize {
            self.total
        }
    }

    /// One GiB backing, so the fractional watermarks land on readable
    /// byte counts.
    const TOTAL: usize = 1 << 30;

    fn gauge(free: usize) -> (&'static FakeSource, MemoryPressure) {
        let source: &'static FakeSource = Box::leak(Box::new(FakeSource {
            total: TOTAL,
            free: AtomicUsize::new(free),
        }));
        (source, MemoryPressure::over(source))
    }

    fn set_free(source: &FakeSource, free: usize) {
        source.free.store(free, Ordering::Relaxed);
    }

    fn budget() -> CacheBudget {
        CacheBudget::from_backing(64 * 1024 * 1024)
    }

    #[test]
    fn watermarks_are_ordered_with_a_hysteresis_gap() {
        let t = PressureThresholds::from_total(TOTAL);
        // Enter watermarks strictly decrease with depth; every exit
        // sits strictly above its enter and strictly below the next
        // shallower band's enter, so relaxing lands inside that band.
        for i in 0..3 {
            assert!(t.enter[i] > t.enter[i + 1]);
            assert!(t.exit[i] > t.enter[i]);
            assert!(t.exit[i + 1] < t.enter[i]);
        }
        assert!(t.exit[3] > t.enter[3]);
        assert!(t.reserve < t.enter[3]);
        assert!(t.reserve > 0);
    }

    #[test]
    fn normal_state_permits_bounded_growth() {
        let (_, pressure) = gauge(TOTAL / 2);
        assert_eq!(pressure.sample(), PressureBand::Normal);
        assert!(pressure.growth_permitted(4096));
    }

    #[test]
    fn growth_never_dips_into_the_reserve() {
        let t = PressureThresholds::from_total(TOTAL);
        // Just above the mild enter watermark the band is normal, but a
        // growth that would cross the reserve floor is still refused.
        let free = t.enter[0] + 4096;
        let (_, pressure) = gauge(free);
        assert_eq!(pressure.sample(), PressureBand::Normal);
        assert!(!pressure.growth_permitted(free - t.reserve));
        assert!(pressure.growth_permitted(4096));
    }

    #[test]
    fn mild_pressure_stops_speculative_growth() {
        let t = PressureThresholds::from_total(TOTAL);
        let (_, pressure) = gauge(t.enter[0] - 4096);
        assert_eq!(pressure.sample(), PressureBand::Mild);
        assert!(!pressure.growth_permitted(0));
    }

    #[test]
    fn deeper_bands_refuse_growth_outright() {
        for band in [
            PressureBand::Moderate,
            PressureBand::Severe,
            PressureBand::Critical,
        ] {
            let t = PressureThresholds::from_total(TOTAL);
            let free = match band {
                PressureBand::Moderate => t.enter[1] - 4096,
                PressureBand::Severe => t.enter[2] - 4096,
                _ => t.enter[3] - 4096,
            };
            let (_, pressure) = gauge(free);
            assert_eq!(pressure.sample(), band);
            assert!(!pressure.growth_permitted(0));
        }
    }

    #[test]
    fn hysteresis_holds_the_band_between_enter_and_exit() {
        let t = PressureThresholds::from_total(TOTAL);
        let (source, pressure) = gauge(t.enter[1] - 4096);
        assert_eq!(pressure.sample(), PressureBand::Moderate);
        // Rising back above the enter watermark but not past the exit
        // watermark holds the band: no oscillation on one threshold.
        set_free(source, t.enter[1] + 4096);
        assert_eq!(pressure.sample(), PressureBand::Moderate);
        // Only past the exit watermark does the band relax — one step.
        set_free(source, t.exit[1] + 4096);
        assert_eq!(pressure.sample(), PressureBand::Mild);
        set_free(source, t.exit[0] + 4096);
        assert_eq!(pressure.sample(), PressureBand::Normal);
    }

    #[test]
    fn band_transitions_are_counted_once_per_stored_change() {
        let t = PressureThresholds::from_total(TOTAL);
        let (source, pressure) = gauge(TOTAL / 2);
        // The starting band is not a transition.
        for band in PressureBand::ALL {
            assert_eq!(pressure.band_entries(band), 0);
        }
        // A held band counts nothing, however often it is sampled.
        assert_eq!(pressure.sample(), PressureBand::Normal);
        assert_eq!(pressure.sample(), PressureBand::Normal);
        assert_eq!(pressure.band_entries(PressureBand::Normal), 0);
        // Deepening to moderate counts one entry into moderate.
        set_free(source, t.enter[1] - 4096);
        assert_eq!(pressure.sample(), PressureBand::Moderate);
        assert_eq!(pressure.band_entries(PressureBand::Moderate), 1);
        // The hysteresis hold between enter and exit is not a change.
        set_free(source, t.enter[1] + 4096);
        assert_eq!(pressure.sample(), PressureBand::Moderate);
        assert_eq!(pressure.band_entries(PressureBand::Moderate), 1);
        // Relaxing one band counts one entry into the relaxed band.
        set_free(source, t.exit[1] + 4096);
        assert_eq!(pressure.sample(), PressureBand::Mild);
        assert_eq!(pressure.band_entries(PressureBand::Mild), 1);
        set_free(source, t.exit[0] + 4096);
        assert_eq!(pressure.sample(), PressureBand::Normal);
        assert_eq!(pressure.band_entries(PressureBand::Normal), 1);
        assert_eq!(pressure.band_entries(PressureBand::Severe), 0);
        assert_eq!(pressure.band_entries(PressureBand::Critical), 0);
    }

    #[test]
    fn repeated_band_swings_accumulate_entries() {
        let t = PressureThresholds::from_total(TOTAL);
        let (source, pressure) = gauge(TOTAL / 2);
        for _ in 0..3 {
            set_free(source, t.enter[3] - 4096);
            assert_eq!(pressure.sample(), PressureBand::Critical);
            set_free(source, TOTAL / 2);
            while pressure.sample() != PressureBand::Normal {}
        }
        assert_eq!(pressure.band_entries(PressureBand::Critical), 3);
        assert_eq!(pressure.band_entries(PressureBand::Normal), 3);
    }

    #[test]
    fn deepening_pressure_applies_immediately() {
        let (source, pressure) = gauge(TOTAL / 2);
        assert_eq!(pressure.sample(), PressureBand::Normal);
        let t = PressureThresholds::from_total(TOTAL);
        set_free(source, t.enter[3] - 4096);
        assert_eq!(pressure.sample(), PressureBand::Critical);
    }

    #[test]
    fn a_reading_inside_the_reserve_is_critical() {
        let t = PressureThresholds::from_total(TOTAL);
        let (_, pressure) = gauge(t.reserve);
        assert_eq!(pressure.sample(), PressureBand::Critical);
    }

    #[test]
    fn a_zero_backing_fails_closed_to_critical() {
        let source: &'static FakeSource = Box::leak(Box::new(FakeSource {
            total: 0,
            free: AtomicUsize::new(usize::MAX),
        }));
        let pressure = MemoryPressure::over(source);
        assert_eq!(pressure.sample(), PressureBand::Critical);
        assert!(!pressure.growth_permitted(0));
    }

    #[test]
    fn band_folding_is_deterministic_under_equal_inputs() {
        let t = PressureThresholds::from_total(TOTAL);
        for band in PressureBand::ALL {
            for free in [0, t.reserve, t.enter[2], t.exit[1], TOTAL] {
                assert_eq!(t.fold(band, free), t.fold(band, free));
            }
        }
    }

    #[test]
    fn disposable_cache_drops_before_clean_cache() {
        let b = budget();
        for class in [
            ReclaimClass::DisposableUi,
            ReclaimClass::PredictivePrefetch,
            ReclaimClass::BackgroundValidation,
        ] {
            assert_eq!(shrink_target(PressureBand::Mild, class, b), 0);
        }
        // Clean file cache begins reclaim at mild (low watermark) but
        // is not yet dropped; metadata and transform survive intact.
        assert!(shrink_target(PressureBand::Mild, ReclaimClass::CleanFileData, b) > 0);
        assert_eq!(
            shrink_target(PressureBand::Mild, ReclaimClass::CleanFileData, b),
            b.low()
        );
        assert_eq!(
            shrink_target(PressureBand::Mild, ReclaimClass::FsMetadata, b),
            b.hard()
        );
    }

    #[test]
    fn clean_and_transform_cache_drain_at_moderate_pressure() {
        let b = budget();
        assert_eq!(
            shrink_target(PressureBand::Moderate, ReclaimClass::CleanFileData, b),
            0
        );
        assert_eq!(
            shrink_target(PressureBand::Moderate, ReclaimClass::TransformCache, b),
            0
        );
        // Hot metadata and recovery assist are preserved, shrunk to
        // the low watermark.
        assert_eq!(
            shrink_target(PressureBand::Moderate, ReclaimClass::FsMetadata, b),
            b.low()
        );
        assert_eq!(
            shrink_target(PressureBand::Moderate, ReclaimClass::ReliabilityAssist, b),
            b.low()
        );
    }

    #[test]
    fn severe_and_critical_force_every_class_to_zero() {
        let b = budget();
        for band in [PressureBand::Severe, PressureBand::Critical] {
            for class in ReclaimClass::ALL {
                assert_eq!(shrink_target(band, class, b), 0);
            }
        }
    }

    #[test]
    fn shrink_targets_never_rise_as_pressure_deepens() {
        let b = budget();
        for class in ReclaimClass::ALL {
            let mut previous = usize::MAX;
            for band in PressureBand::ALL {
                let target = shrink_target(band, class, b);
                assert!(target <= previous);
                previous = target;
            }
        }
    }

    #[test]
    fn ramzip_compression_waits_for_clean_and_transform_reclaim() {
        // No compression at all while pressure is shallow.
        assert_eq!(
            ramzip_handoff(PressureBand::Normal, 0),
            RamzipHandoff::HoldCompression
        );
        assert_eq!(
            ramzip_handoff(PressureBand::Mild, 0),
            RamzipHandoff::HoldCompression
        );
        // At moderate pressure the cheaper caches must drain first.
        assert_eq!(
            ramzip_handoff(PressureBand::Moderate, 4096),
            RamzipHandoff::HoldCompression
        );
        assert_eq!(
            ramzip_handoff(PressureBand::Moderate, 0),
            RamzipHandoff::CompressColdAnonymous
        );
        // At severe pressure ramzip owns cold-anonymous policy.
        assert_eq!(
            ramzip_handoff(PressureBand::Severe, 4096),
            RamzipHandoff::CompressColdAnonymous
        );
        // At critical pressure speculative work stops; escalation owns
        // the next step.
        assert_eq!(
            ramzip_handoff(PressureBand::Critical, 0),
            RamzipHandoff::HoldCompression
        );
    }

    #[test]
    fn the_reclaim_batch_is_bounded_and_matches_the_handoff_bands() {
        // Compression never runs at normal/mild/critical, so the batch is
        // zero there — the caller does no reclaim work at all.
        assert_eq!(ramzip_reclaim_batch(PressureBand::Normal), 0);
        assert_eq!(ramzip_reclaim_batch(PressureBand::Mild), 0);
        assert_eq!(ramzip_reclaim_batch(PressureBand::Critical), 0);
        // Compression runs at moderate and severe; severe reclaims harder.
        let moderate = ramzip_reclaim_batch(PressureBand::Moderate);
        let severe = ramzip_reclaim_batch(PressureBand::Severe);
        assert!(moderate > 0);
        assert!(severe > moderate);
        // A batch is only ever non-zero where the handoff opens compression.
        for band in PressureBand::ALL {
            let opens = matches!(band, PressureBand::Moderate | PressureBand::Severe);
            assert_eq!(ramzip_reclaim_batch(band) > 0, opens, "{band:?}");
        }
    }

    #[test]
    fn escalation_is_deterministic_and_prefers_cache_reclaim() {
        assert_eq!(escalation(PressureBand::Normal, 0), EscalationStep::Hold);
        assert_eq!(
            escalation(PressureBand::Mild, 4096),
            EscalationStep::ReclaimCaches
        );
        assert_eq!(escalation(PressureBand::Mild, 0), EscalationStep::Hold);
        assert_eq!(
            escalation(PressureBand::Moderate, 4096),
            EscalationStep::ReclaimCaches
        );
        assert_eq!(
            escalation(PressureBand::Moderate, 0),
            EscalationStep::HandOffToRamzip
        );
        assert_eq!(
            escalation(PressureBand::Severe, 0),
            EscalationStep::HandOffToRamzip
        );
        assert_eq!(
            escalation(PressureBand::Critical, 4096),
            EscalationStep::ReclaimCaches
        );
        assert_eq!(
            escalation(PressureBand::Critical, 0),
            EscalationStep::VmPolicy
        );
    }

    #[test]
    fn the_frame_allocator_is_an_honest_source() {
        use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
        use crate::frame::PhysAddr;

        // Based at frame 1: the zero page is permanently reserved, so a
        // base-0 region would enroll one frame fewer than it spans.
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(PAGE_SIZE as u64),
            length: (64 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let allocator = FrameAllocator::new(&map).expect("allocator over a usable map");
        let total = FreeMemorySource::total_bytes(&allocator);
        let before = FreeMemorySource::free_bytes(&allocator);
        assert_eq!(total, 64 * PAGE_SIZE);
        assert!(before <= total);
        let frame = allocator.alloc().expect("one frame");
        let after = FreeMemorySource::free_bytes(&allocator);
        assert_eq!(after + PAGE_SIZE, before);
        allocator.free(frame).expect("free the frame");
        assert_eq!(FreeMemorySource::free_bytes(&allocator), before);
    }
}
