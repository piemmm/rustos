//! VM pressure bands and cache reclaim ordering (`plans/SMARTRAM.md`
//! SMART2, `plans/SWAPSWAPSWAP.md` section 6).
//!
//! This module is the one definition of the system's memory-pressure
//! state and of the order reclaimable caches are shrunk in as pressure
//! rises. The band vocabulary (normal, mild, moderate, severe,
//! critical) is shared with `plans/SWAPSWAPSWAP.md`; there is no
//! parallel vocabulary and no second pressure model. The `ramzip`
//! handoff and VM escalation ordering, which need the kernel's own
//! anonymous-memory tier, live beside it in `kernel/mem::pressure` and
//! consume the band defined here.
//!
//! # Two gauges, one band
//!
//! [`PressureGauge`] is the interface every cache consults, and there
//! are exactly two implementations because there are exactly two
//! vantage points:
//!
//! - [`MemoryPressure`] — the kernel's *measuring* gauge. It samples a
//!   [`FreeMemorySource`] (in production the physical frame allocator,
//!   whose free-frame count is the authoritative "how much RAM is
//!   left" figure) and folds the reading into a banded state machine
//!   with hysteresis: each band is entered below one watermark and left
//!   above a strictly higher one, so a reading that hovers on a single
//!   threshold cannot oscillate the band. Sampling happens on the
//!   caller's own operations (a cache consults the gauge as it works);
//!   there is no background worker and no periodic tick.
//! - [`ReportedPressure`] — a *receiving* gauge for a process that
//!   cannot see physical memory at all. It holds the band the kernel
//!   last reported and answers from that, so a userland cache obeys
//!   exactly the same policy as a kernel one without inventing a second
//!   notion of pressure. Until the first report arrives it answers
//!   critical: an unknown band admits nothing.
//!
//! # Reclaim ordering
//!
//! [`shrink_target`] maps a band and a [`ReclaimClass`] to the byte
//! ceiling that class must shrink to, following `plans/SMARTRAM.md`
//! section 7: disposable and speculative classes drop at mild
//! pressure, clean file data begins reclaim at mild and finishes at
//! moderate together with transform cache, metadata and recovery
//! assist are preserved longest, and at severe or critical pressure
//! every class obeys a forced shrink to zero.
//!
//! # Reserves
//!
//! The thresholds carry a reserve floor derived from the backing size.
//! A reading at or below the reserve is critical pressure regardless
//! of band history, and the [`GrowthAllowance`] one reading yields
//! refuses any cache growth that would dip into the reserve — cache
//! expansion can never be the cause of reserve exhaustion, and a
//! caller admitting a whole run of entries draws each one down from
//! that single allowance rather than re-asking per entry. A backing
//! whose size is unknown (zero) reports critical pressure and admits
//! nothing: fail closed, never a guess.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::model::{CacheBudget, ReclaimClass};

/// Fraction of a memory backing held back as a reserve: `total /
/// RESERVE_DIVISOR` bytes that no speculative consumer may draw into,
/// so the system always retains headroom to make progress (grow the
/// kernel heap, build page tables, service a fault) even when it is
/// otherwise starving. ~1.6% of the backing.
///
/// The one definition shared by the frame allocator's user-commit floor
/// and the pressure band's critical floor, so the two can never
/// diverge.
pub const RESERVE_DIVISOR: usize = 64;

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
/// The production source is the kernel's physical frame allocator,
/// which implements this trait over its free-frame count; tests inject
/// a controllable double. Both figures are byte counts of the same
/// backing resource, so `free <= total` for an honest source.
pub trait FreeMemorySource: Sync {
    /// Bytes of the backing resource currently free.
    fn free_bytes(&self) -> usize;
    /// Total bytes of the backing resource.
    fn total_bytes(&self) -> usize;
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
// defined once above and imported there.

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

/// A one-way notification that the published band changed.
///
/// The gauge is sampled from wherever memory is spent — a cache
/// operation, a demand fault, a direct-reclaim sweep — so this callback
/// can fire in any of those contexts. It must therefore be **lock-free
/// and allocation-free**: an observer that took a lock could be
/// re-entered by an allocation inside its own wake path and deadlock
/// against itself on one CPU. The kernel's observer flags a deferred
/// wake and returns; the real unpark happens later, at a safe
/// dispatcher-context point.
pub trait BandObserver: Sync {
    /// The published band just changed to `band`. Fires once per stored
    /// change, never on a sample that leaves the band alone.
    fn band_changed(&self, band: PressureBand);
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
    /// Notified once per stored band change, so a watcher can be woken
    /// on the transition instead of polling the gauge. `None` leaves
    /// the gauge purely passive (host tests, an early boot before the
    /// wake path exists).
    observer: Option<&'static (dyn BandObserver + 'static)>,
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
            observer: None,
        }
    }

    /// Notify `observer` on every stored band change.
    ///
    /// Set once, at construction, so the gauge needs no interior
    /// mutability for the hook and a sampler on any CPU sees the same
    /// observer. The callback runs inside [`sample`](Self::sample) and
    /// so inherits its context: it must be lock-free and
    /// allocation-free (see [`BandObserver`]).
    #[must_use]
    pub const fn observed_by(mut self, observer: &'static (dyn BandObserver + 'static)) -> Self {
        self.observer = Some(observer);
        self
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
        self.publish(self.source.free_bytes())
    }

    /// Fold `free` into the published band, counting and notifying a
    /// change. The one definition [`sample`](Self::sample) and
    /// [`growth_allowance`](Self::growth_allowance) share, so a single
    /// reading is folded exactly once however it was taken.
    fn publish(&self, free: usize) -> PressureBand {
        let next = self.thresholds.fold(self.band(), free);
        let previous = self.band.swap(next.depth(), Ordering::Relaxed);
        if previous != next.depth() {
            self.transitions[next.depth() as usize].fetch_add(1, Ordering::Relaxed);
            if let Some(observer) = self.observer {
                observer.band_changed(next);
            }
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

    /// One reading, folded once, carrying the headroom above the reserve
    /// floor it leaves: growth is permitted only at normal pressure, and
    /// never past the floor — cache expansion can never be the cause of
    /// reserve exhaustion.
    ///
    /// Taking the reading here rather than per admission is what lets a
    /// caller admitting a *run* of entries decide all of them from one
    /// reading, instead of re-reading the free-memory source (in the
    /// kernel: taking the global frame-allocator lock) once per entry.
    pub fn growth_allowance(&self) -> GrowthAllowance {
        let free = self.source.free_bytes();
        GrowthAllowance::new(
            self.publish(free),
            free.saturating_sub(self.thresholds.reserve),
        )
    }
}

/// One gauge reading, from which a caller admitting a **run** of entries
/// decides every one of them without touching the gauge's source again.
///
/// A cache that retains many entries per operation — a block cache
/// holding each device block of one coalesced read, a write journal
/// recording each block of one write — asked the gauge per *entry*
/// before this existed. In the kernel that reading is the physical
/// frame allocator, so a 64-block admission took the global
/// frame-allocator lock 128 times to answer a question whose answer
/// cannot change between two entries of the same operation.
///
/// The allowance is also the *stricter* answer. Admitted bytes come
/// from the kernel heap's existing slack, so the free reading does not
/// fall as entries are admitted: re-asking per entry returns the same
/// verdict every time and lets a run admit many multiples of the
/// headroom the first answer covered. Drawing each entry's cost down
/// from one headroom is what makes the reserve floor bound the *run*.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GrowthAllowance {
    band: PressureBand,
    remaining: usize,
}

impl GrowthAllowance {
    /// An allowance over `band` with `remaining` bytes admissible before
    /// the reading would reach the reserve floor.
    #[must_use]
    pub const fn new(band: PressureBand, remaining: usize) -> Self {
        Self { band, remaining }
    }

    /// An allowance that admits nothing: the caller already knows growth
    /// is refused (a poisoned or operator-disabled cache) and needs no
    /// reading to say so.
    #[must_use]
    pub const fn refused() -> Self {
        Self::new(PressureBand::Critical, 0)
    }

    /// An allowance over `band` with no reserve floor in view, so only
    /// the caller's own budget bounds the run.
    ///
    /// This is the honest answer for a gauge that cannot measure
    /// physical memory: the floor is kernel state a process must not
    /// read, and it is already folded into the band such a gauge was
    /// told.
    #[must_use]
    pub const fn unbounded(band: PressureBand) -> Self {
        Self::new(band, usize::MAX)
    }

    /// The band the reading folded to.
    #[must_use]
    pub const fn band(self) -> PressureBand {
        self.band
    }

    /// Bytes still admissible before the reading would reach the reserve
    /// floor. [`usize::MAX`] where the gauge has no floor in view.
    #[must_use]
    pub const fn remaining_bytes(self) -> usize {
        self.remaining
    }

    /// Whether a draw of `cost_bytes` would be admitted, without taking
    /// it — the question a caller asks before doing *speculative* work
    /// whose only value is that the result can be retained.
    #[must_use]
    pub const fn permits(self, cost_bytes: usize) -> bool {
        if !matches!(self.band, PressureBand::Normal) {
            return false;
        }
        // Strictly above the floor: a draw that lands exactly on the
        // reserve is refused, so cache growth can never be what exhausts
        // it.
        match self.remaining.checked_sub(cost_bytes) {
            Some(left) => left > 0,
            None => false,
        }
    }

    /// Draw `cost_bytes` from the allowance, reporting whether it was
    /// admitted. A refusal leaves the allowance untouched, so a caller
    /// may keep offering smaller entries.
    pub fn take(&mut self, cost_bytes: usize) -> bool {
        if !self.permits(cost_bytes) {
            return false;
        }
        // `permits` proved the headroom strictly exceeds the cost, so the
        // saturating form is exact and there is no underflow to guard.
        self.remaining = self.remaining.saturating_sub(cost_bytes);
        true
    }
}

/// What a reclaimable cache needs from the pressure model, wherever it
/// runs.
///
/// Two implementations, one policy: [`MemoryPressure`] measures free
/// memory directly (the kernel), [`ReportedPressure`] holds the band
/// the kernel reported (a userland process). A cache written against
/// this trait obeys the same [`shrink_target`] ordering on either side
/// of the syscall boundary, so there is exactly one notion of memory
/// pressure in the system.
pub trait PressureGauge: Sync {
    /// The band to act on now, taking a fresh reading where the gauge
    /// has one to take.
    fn sample(&self) -> PressureBand;

    /// One reading, from which a run of admissions is decided.
    fn growth_allowance(&self) -> GrowthAllowance;

    /// Whether a cache may grow by `cost_bytes` right now — a run of
    /// one, so it is the allowance and never a second policy.
    fn growth_permitted(&self, cost_bytes: usize) -> bool {
        self.growth_allowance().take(cost_bytes)
    }
}

impl PressureGauge for MemoryPressure {
    fn sample(&self) -> PressureBand {
        Self::sample(self)
    }

    fn growth_allowance(&self) -> GrowthAllowance {
        Self::growth_allowance(self)
    }
}

/// The band a process was told about, for a cache that cannot measure
/// memory itself.
///
/// A userland process has no view of physical memory: free frames,
/// watermarks, and the reserve floor are all kernel state it cannot and
/// must not read. It is instead *told* the band — woken on a change and
/// reading the coarse published value — and stores it here, so its
/// caches consult the same [`PressureGauge`] interface, obey the same
/// [`shrink_target`] ordering, and need no second pressure model.
///
/// A fresh instance answers [`PressureBand::Critical`] until the first
/// [`report`](Self::report): an unknown band admits nothing and forces
/// every class to zero, so a process that never learns the band renders
/// uncached rather than growing blind.
pub struct ReportedPressure {
    band: AtomicU8,
}

impl ReportedPressure {
    /// A gauge that has not been told anything yet: critical until the
    /// first [`report`](Self::report).
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            band: AtomicU8::new(PressureBand::Critical.depth()),
        }
    }

    /// Publish the band the kernel reported, returning whether it
    /// differed from the band already held — the caller shrinks its
    /// caches on a change and does nothing on a repeat.
    pub fn report(&self, band: PressureBand) -> bool {
        self.band.swap(band.depth(), Ordering::Relaxed) != band.depth()
    }

    /// The band currently held.
    #[must_use]
    pub fn band(&self) -> PressureBand {
        PressureBand::from_depth(self.band.load(Ordering::Relaxed))
    }
}

impl PressureGauge for ReportedPressure {
    fn sample(&self) -> PressureBand {
        self.band()
    }

    /// The band alone bounds growth here: the reserve floor is
    /// physical-memory state only the kernel can see, and it is already
    /// folded into the band this gauge was told (a reading inside the
    /// reserve is reported critical). What bounds an individual
    /// admission on this side is the cache's own budget, not a reserve
    /// the process cannot read.
    fn growth_allowance(&self) -> GrowthAllowance {
        GrowthAllowance::unbounded(self.band())
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

#[cfg(test)]
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
    fn one_allowance_bounds_a_whole_run_of_admissions() {
        // The block cache admits every device block of one coalesced
        // read from one reading. Asking the gauge per block returned the
        // same verdict every time (admitted bytes come from heap slack,
        // so the free reading does not move), which let a run admit many
        // multiples of the headroom the first answer covered. Drawing
        // each block down from one allowance is what makes the reserve
        // floor bound the run.
        let t = PressureThresholds::from_total(TOTAL);
        let free = t.enter[0] + 4096;
        let (_, pressure) = gauge(free);
        let headroom = free - t.reserve;
        let cost = headroom / 4;

        let mut allowance = pressure.growth_allowance();
        assert_eq!(allowance.band(), PressureBand::Normal);
        assert_eq!(allowance.remaining_bytes(), headroom);
        // Three quarters of the headroom is admissible; the fourth would
        // land exactly on the floor and is refused, and a refusal leaves
        // the allowance intact for a smaller entry.
        assert!(allowance.take(cost));
        assert!(allowance.take(cost));
        assert!(allowance.take(cost));
        assert!(!allowance.take(cost));
        assert_eq!(allowance.remaining_bytes(), headroom - 3 * cost);
        assert!(allowance.take(cost - 1));

        // Re-asking per entry would have admitted the fourth: proof the
        // per-entry question was the weaker one.
        assert!(pressure.growth_permitted(cost));
    }

    #[test]
    fn permits_answers_the_draw_without_taking_it() {
        // Speculative work whose only value is that the result can be
        // retained asks before doing it, so the query must agree with the
        // draw and must not consume headroom.
        let t = PressureThresholds::from_total(TOTAL);
        let free = t.enter[0] + 4096;
        let (_, pressure) = gauge(free);
        let mut allowance = pressure.growth_allowance();
        let before = allowance.remaining_bytes();
        assert!(allowance.permits(4096));
        assert_eq!(allowance.remaining_bytes(), before, "a query draws nothing");
        assert!(
            !allowance.permits(before),
            "landing on the floor is refused"
        );
        assert!(allowance.take(4096));
        assert_eq!(allowance.remaining_bytes(), before - 4096);

        // Outside normal pressure nothing is permitted, whatever the cost.
        let mild = GrowthAllowance::new(PressureBand::Mild, usize::MAX);
        assert!(!mild.permits(0));
    }

    #[test]
    fn a_refused_allowance_admits_nothing_without_a_reading() {
        // The caller already knows growth is refused (a poisoned or
        // operator-disabled cache), so it needs no reading to say so.
        let mut refused = GrowthAllowance::refused();
        assert_eq!(refused.band(), PressureBand::Critical);
        assert_eq!(refused.remaining_bytes(), 0);
        assert!(!refused.take(0));
        assert!(!refused.take(1));
    }

    #[test]
    fn an_allowance_matches_the_single_admission_verdict_in_every_band() {
        // One definition, two spellings: `growth_permitted` is the
        // allowance drawn once, so the two can never diverge.
        let t = PressureThresholds::from_total(TOTAL);
        for free in [
            TOTAL / 2,
            t.enter[0] + 4096,
            t.enter[0] - 4096,
            t.enter[1] - 4096,
            t.enter[2] - 4096,
            t.enter[3] - 4096,
            t.reserve,
            0,
        ] {
            for cost in [0usize, 4096, TOTAL] {
                let (_, pressure) = gauge(free);
                let mut allowance = pressure.growth_allowance();
                let via_allowance = allowance.take(cost);
                let (_, fresh) = gauge(free);
                assert_eq!(
                    via_allowance,
                    fresh.growth_permitted(cost),
                    "free={free} cost={cost}"
                );
            }
        }
    }

    #[test]
    fn a_reported_gauge_bounds_a_run_by_its_band_alone() {
        // A process cannot see the reserve floor, so its allowance is
        // bounded only by the band it was told and by its own budget.
        let reported = ReportedPressure::unknown();
        assert!(!reported.growth_allowance().take(0));
        assert!(reported.report(PressureBand::Normal));
        let mut allowance = reported.growth_allowance();
        assert_eq!(allowance.remaining_bytes(), usize::MAX);
        for _ in 0..8 {
            assert!(allowance.take(1 << 20));
        }
        assert!(reported.report(PressureBand::Mild));
        assert!(!reported.growth_allowance().take(1));
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
}
