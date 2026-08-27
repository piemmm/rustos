//! Reclaimable-memory classification, budget, and accounting
//! (`plans/SMARTRAM.md`).
//!
//! A reclaimable cache holds *derived* state — data that can always be
//! rebuilt from its canonical source — so the memory it occupies is a
//! loan the VM can call in at any time. This module is the one
//! definition of how such a cache is classed, bounded, and accounted:
//! every consumer — the kernel's filesystem, block, transform and
//! launch caches, and the desktop session's rasterised-asset caches —
//! charges its entries here, and reclaim decisions read these numbers
//! rather than re-deriving their own.
//!
//! # Classification and admission
//!
//! Each entry belongs to exactly one [`ReclaimClass`]. Classes order
//! reclaim: under pressure the cheaper-to-rebuild class is evicted
//! first (`plans/SMARTRAM.md` section 7, matching
//! `plans/SWAPSWAPSWAP.md` section 6 — clean file cache is reclaimed
//! before anything more expensive).
//!
//! Before a cache admits anything it declares a [`CacheCandidate`] —
//! class, [`ReclaimOwner`], [`RebuildCost`], [`Sensitivity`],
//! [`InvalidationSource`], [`ReclaimRule`], and its worst-case
//! per-entry bookkeeping bytes — and passes the
//! [`classify`](CacheCandidate::classify) gate. The gate fails closed
//! with a typed [`AdmissionRefusal`]: an unknown class or owner,
//! unruled-out sensitive material (credentials, keys, capability
//! tokens), unbounded per-entry metadata, a missing reclaim rule, or a
//! missing invalidation source is refused, and the producer serves
//! uncached. No unowned, unclassifiable, or uninvalidatable memory
//! exists in the model.
//!
//! # Budgets and hysteresis
//!
//! A [`CacheBudget`] is derived from the size of the backing resource
//! (the kernel heap arena), never a free-standing magic number. Growth
//! and shrink use two watermarks so a cache does not oscillate on one
//! threshold: an insert that would push usage past the *hard* limit
//! forces eviction down to the *low* watermark.
//!
//! # Fail-closed accounting
//!
//! [`CacheAccounting`] refuses overflow and underflow with a typed
//! [`AccountingError`] instead of wrapping or saturating: a cache whose
//! books stop balancing is a defect, and its caller drops the entry
//! rather than corrupting the ledger.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The reclaim class of a cached entry (`plans/SMARTRAM.md` section 5).
///
/// This is the complete taxonomy the plan defines; consumers for the
/// classes beyond the filesystem cache arrive with the stages that
/// build them, but the classification model is one closed definition.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReclaimClass {
    /// Disposable UI state (rasterised assets, glyph atlases, window
    /// snapshots): cheapest to lose, first to go under pressure.
    DisposableUi,
    /// Speculative prefetch (listings, thumbnails, completion indexes):
    /// never needed for correctness, dropped with disposable UI.
    PredictivePrefetch,
    /// Idle-time validation work products (scan progress, candidate
    /// fingerprints): speculative work stops as pressure begins.
    BackgroundValidation,
    /// Semantic app-launch state (parsed manifests, validation
    /// summaries, command-resolution results): cheap entries shrink at
    /// mild pressure.
    SemanticAppCache,
    /// Runtime-owned derived state (loader preparation, resource maps):
    /// grouped with the semantic cache in the pressure order.
    RuntimeCache,
    /// Clean, rebuildable file *data* re-readable from the volume:
    /// one bounded device read rebuilds a chunk, reclaimed from mild
    /// pressure before anything is compressed into `ramzip`.
    CleanFileData,
    /// Expensive intermediate forms of authorised data (verified,
    /// decrypted, decompressed, parsed records): reclaimed at moderate
    /// pressure, after clean file data.
    TransformCache,
    /// Filesystem *metadata* — stat records, lookup results, directory
    /// entries, security records. Small, hot, and rebuilt by a
    /// multi-step tree walk, so it outlives file data under pressure.
    FsMetadata,
    /// Rebuildable recovery-assist state (verification windows, health
    /// summaries): never the source of truth, but justified by recovery
    /// latency, so it is preserved the longest.
    ReliabilityAssist,
}

impl ReclaimClass {
    /// Every class, in reclaim order (first reclaimed first).
    pub const ALL: [Self; 9] = [
        Self::DisposableUi,
        Self::PredictivePrefetch,
        Self::BackgroundValidation,
        Self::SemanticAppCache,
        Self::RuntimeCache,
        Self::CleanFileData,
        Self::TransformCache,
        Self::FsMetadata,
        Self::ReliabilityAssist,
    ];

    /// Eviction order under pressure: lower is reclaimed first,
    /// following the `plans/SMARTRAM.md` section 7 pressure policy
    /// (disposable and speculative classes at mild pressure, clean file
    /// data before transform cache, hot metadata and recovery assist
    /// preserved the longest).
    ///
    /// Deterministic for equal inputs: the ordering is a pure function
    /// of the class.
    #[must_use]
    pub const fn reclaim_priority(self) -> u8 {
        match self {
            Self::DisposableUi => 0,
            Self::PredictivePrefetch => 1,
            Self::BackgroundValidation => 2,
            Self::SemanticAppCache => 3,
            Self::RuntimeCache => 4,
            Self::CleanFileData => 5,
            Self::TransformCache => 6,
            Self::FsMetadata => 7,
            Self::ReliabilityAssist => 8,
        }
    }

    /// The class's slot in the per-class accounting array.
    #[must_use]
    pub const fn index(self) -> usize {
        self.reclaim_priority() as usize
    }
}

/// The owner a reclaimable cache is charged to (`plans/SMARTRAM.md`
/// section 8): every entry's memory must be attributable, so a cache
/// with no owner is refused at classification. Variants exist only for
/// the owners the kernel already has an identity for; session- and
/// service-owned caches arrive with the stages that introduce those
/// identities.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReclaimOwner {
    /// A kernel subsystem, named by its stable subsystem identifier.
    KernelSubsystem(&'static str),
    /// A mounted filesystem volume, identified by its stable per-boot
    /// mount handle (never a discovery-order device name).
    FilesystemVolume {
        /// The volume's stable per-boot mount handle.
        volume: u64,
    },
    /// A task / address space, identified by its task id.
    Task {
        /// The owning task's id.
        task: u64,
    },
    /// A graphical desktop session, identified by the seat it is bound
    /// to. Distinct from [`Self::Task`] because the memory is
    /// seat-scoped rather than merely process-scoped: revoking the
    /// seat invalidates every entry charged here, and a per-seat
    /// ledger keeps one session's rendered user data attributable to
    /// (and reclaimable with) exactly that seat.
    DesktopSession {
        /// The seat the session holds.
        seat: u64,
    },
    /// A userland process — an ordinary program or a long-running
    /// system service — named by the stable label its own cache
    /// installer supplies, for a cache that cannot resolve
    /// [`Self::Task`]'s numeric id.
    ///
    /// `abi-v1` gives a process no query to read its own task id back,
    /// so this is the honest fallback for two shapes of caller: a
    /// library embedded in many different consumer programs (which
    /// names itself, not the host program it happens to run inside),
    /// and a singleton system service reasoning about its own memory
    /// (which names itself directly, parallel to
    /// [`Self::KernelSubsystem`] but outside the kernel). Every entry
    /// stays inside the one process that charged it — the ledger is
    /// never merged across processes — so the label is all the
    /// attribution the audit trail needs.
    UserlandProcess(&'static str),
}

/// How expensive a cached entry is to rebuild from its canonical
/// source (`plans/SMARTRAM.md` section 5).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RebuildCost {
    /// One bounded read or trivial recomputation.
    Cheap,
    /// A multi-step walk or parse.
    Moderate,
    /// A verification, decryption, decompression, or render pipeline.
    Expensive,
    /// State that shortens recovery from a fault; still rebuildable.
    RecoveryCritical,
}

/// What a cached entry's bytes may reveal (`plans/SMARTRAM.md`
/// section 5). Credentials, keys, and capability tokens are not a
/// sensitivity *level* a cache may hold — [`CacheCandidate::classify`]
/// refuses them outright.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Sensitivity {
    /// Reveals nothing beyond public system state.
    Public,
    /// A user's data or names (including plaintext of encrypted
    /// storage): zeroed whenever the entry is released.
    UserData,
    /// System-owned data that is not per-user.
    SystemData,
    /// Derived from secret material without containing it.
    SecretDerived,
    /// Contains credentials, cryptographic keys, or capability tokens:
    /// never cacheable.
    CredentialOrKey,
}

/// The canonical event family that invalidates a cached entry
/// (`plans/SMARTRAM.md` section 10). A cache whose entries have no
/// declared invalidation source is refused: a stale entry is a
/// correctness and security defect, not a performance quirk.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InvalidationSource {
    /// Precise invalidation by the object's single writer (a file
    /// write, truncate, rename, delete, or metadata update seen by the
    /// one driver instance).
    SourceMutation,
    /// A generation token on the canonical identity (mount generation,
    /// removable-media generation, content hash, COW epoch).
    GenerationToken,
    /// A key-epoch change on the material the entry was derived under.
    KeyEpoch,
    /// A policy-epoch change (ACL, MAC, capability authority,
    /// manifest, or signature policy).
    PolicyEpoch,
    /// The owning task, session, or service is torn down.
    OwnerTeardown,
}

/// How a cache releases an entry when the VM asks (`plans/SMARTRAM.md`
/// section 5). A producer that cannot state a rule has declared the
/// entry non-reclaimable, and it is refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReclaimRule {
    /// Drop the entry outright; the canonical source rebuilds it.
    Drop,
    /// Shrink in place (trim a summary, halve an index).
    Shrink,
    /// Write pending derived state back, then drop.
    FlushThenDrop,
    /// Ask the owning service to release; the owner must comply.
    NotifyOwner,
    /// Justified by recovery latency: kept until severe pressure, but
    /// still released on a forced shrink.
    PreserveUntilSevere,
}

/// The per-entry bookkeeping ceiling, in bytes.
///
/// This is a validation bound, deliberately fixed: an entry's metadata
/// (map nodes, key copies, recency-index slots) must be small and
/// statically boundable, or the cache's footprint can no longer be
/// reasoned about from its payload ledger. The value covers the
/// largest in-tree entry key — a filesystem name component (255 bytes)
/// plus the fixed per-entry overhead — with headroom; a candidate
/// declaring more is refused as unbounded.
pub const MAX_ENTRY_METADATA: usize = 512;

/// Why [`CacheCandidate::classify`] refused a candidate
/// (`plans/SMARTRAM.md` SMART1). Unknown fails closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AdmissionRefusal {
    /// The producer could not classify the entries.
    UnknownClass,
    /// No owner to charge the memory to.
    UnknownOwner,
    /// The entries contain — or the producer cannot rule out —
    /// credentials, keys, or capability tokens.
    SensitiveMaterial,
    /// Per-entry bookkeeping exceeds [`MAX_ENTRY_METADATA`].
    UnboundedMetadata,
    /// No reclaim rule was declared: the entries could not be released
    /// on demand, so they are not cache.
    NonReclaimable,
    /// No invalidation source was declared: the entries could go stale
    /// undetected.
    MissingInvalidation,
}

impl AdmissionRefusal {
    /// The stable `cause` label carried by the refusal's audit record
    /// (see [`crate::audit`]).
    #[must_use]
    pub const fn cause(self) -> &'static str {
        match self {
            Self::UnknownClass => "unknown_class",
            Self::UnknownOwner => "unknown_owner",
            Self::SensitiveMaterial => "sensitive_material",
            Self::UnboundedMetadata => "unbounded_metadata",
            Self::NonReclaimable => "non_reclaimable",
            Self::MissingInvalidation => "missing_invalidation",
        }
    }
}

/// A cache's declaration of what it intends to hold, submitted to
/// [`classify`](Self::classify) before any entry is admitted.
///
/// Fields the producer cannot vouch for are declared `None` and the
/// candidate is refused — unknown never defaults to admissible.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CacheCandidate {
    /// The reclaim class of every entry, or `None` if unclassifiable.
    pub class: Option<ReclaimClass>,
    /// The owner charged for the memory, or `None` if unattributable.
    pub owner: Option<ReclaimOwner>,
    /// The declared rebuild cost.
    pub rebuild_cost: RebuildCost,
    /// The declared sensitivity, or `None` if the producer cannot rule
    /// out sensitive content.
    pub sensitivity: Option<Sensitivity>,
    /// The declared invalidation source, or `None` if entries could go
    /// stale undetected.
    pub invalidation: Option<InvalidationSource>,
    /// The declared reclaim rule, or `None` if entries could not be
    /// released on demand.
    pub rule: Option<ReclaimRule>,
    /// Worst-case per-entry bookkeeping bytes (keys, map nodes, index
    /// slots) on top of the payload.
    pub entry_metadata_bytes: usize,
}

impl CacheCandidate {
    /// Classify the candidate, fail closed.
    ///
    /// A pure function: equal candidates always classify identically,
    /// so admission decisions are deterministic under equal inputs.
    ///
    /// # Errors
    ///
    /// A typed [`AdmissionRefusal`] naming the first missing or
    /// forbidden dimension; the caller then serves uncached.
    pub fn classify(self) -> Result<CachePolicy, AdmissionRefusal> {
        let class = self.class.ok_or(AdmissionRefusal::UnknownClass)?;
        let owner = self.owner.ok_or(AdmissionRefusal::UnknownOwner)?;
        // Unknown sensitivity is treated as the most sensitive.
        let sensitivity = self
            .sensitivity
            .ok_or(AdmissionRefusal::SensitiveMaterial)?;
        if sensitivity == Sensitivity::CredentialOrKey {
            return Err(AdmissionRefusal::SensitiveMaterial);
        }
        let invalidation = self
            .invalidation
            .ok_or(AdmissionRefusal::MissingInvalidation)?;
        let rule = self.rule.ok_or(AdmissionRefusal::NonReclaimable)?;
        if self.entry_metadata_bytes > MAX_ENTRY_METADATA {
            return Err(AdmissionRefusal::UnboundedMetadata);
        }
        Ok(CachePolicy {
            class,
            owner,
            rebuild_cost: self.rebuild_cost,
            sensitivity,
            invalidation,
            rule,
        })
    }
}

/// A fully classified cache policy: every dimension known, the owner
/// chargeable, sensitive material excluded, and the per-entry metadata
/// bounded. Only [`CacheCandidate::classify`] constructs one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CachePolicy {
    class: ReclaimClass,
    owner: ReclaimOwner,
    rebuild_cost: RebuildCost,
    sensitivity: Sensitivity,
    invalidation: InvalidationSource,
    rule: ReclaimRule,
}

impl CachePolicy {
    /// The reclaim class every entry is charged under.
    #[must_use]
    pub const fn class(self) -> ReclaimClass {
        self.class
    }

    /// The owner the memory is charged to.
    #[must_use]
    pub const fn owner(self) -> ReclaimOwner {
        self.owner
    }

    /// The declared rebuild cost.
    #[must_use]
    pub const fn rebuild_cost(self) -> RebuildCost {
        self.rebuild_cost
    }

    /// The declared sensitivity.
    #[must_use]
    pub const fn sensitivity(self) -> Sensitivity {
        self.sensitivity
    }

    /// The declared invalidation source.
    #[must_use]
    pub const fn invalidation(self) -> InvalidationSource {
        self.invalidation
    }

    /// The declared reclaim rule.
    #[must_use]
    pub const fn rule(self) -> ReclaimRule {
        self.rule
    }
}

/// Why a cache refused to admit or account an entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AccountingError {
    /// Charging the entry would overflow the ledger.
    Overflow,
    /// Discharging the entry would underflow the ledger — the books no
    /// longer balance, which is a caller defect surfaced loudly.
    Underflow,
}

/// The grow/shrink bounds of one bounded cache, in bytes.
///
/// `hard` is the ceiling an insert may never push usage past; `low` is
/// the watermark a forced shrink evicts down to. Keeping them apart is
/// the hysteresis `plans/SMARTRAM.md` section 7 requires: growth up to
/// `hard`, shrink down to `low`, never both on one threshold.
///
/// `floor` is the working-set share of `hard` that pressure short of
/// severe does not take (see [`with_working_set_floor`](Self::with_working_set_floor));
/// it is zero for a cache that is speculation all the way down.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CacheBudget {
    hard: usize,
    low: usize,
    floor: usize,
}

/// The backing-resource fraction one bounded cache may occupy.
///
/// Each cache is capped at 1/16 of the kernel heap arena: with the
/// fixed 64 MiB heap this is 4 MiB per cache. A boot volume carries two
/// (the clean filesystem cache and the transform cache), so the two
/// boot volumes' four caches together stay at or under 1/4 of the heap
/// and cache growth can never be the cause of kernel-heap exhaustion
/// (`plans/SMARTRAM.md` section 7 — reserves are preserved by
/// construction, and the pressure gauge stops growth long before the
/// ceiling matters).
const BACKING_DIVISOR: usize = 16;

/// The shrink watermark as a fraction of the hard limit: a forced
/// shrink evicts down to 3/4 of `hard`, so post-shrink inserts have
/// real headroom before the next eviction pass.
const LOW_NUMERATOR: usize = 3;
const LOW_DIVISOR: usize = 4;

impl CacheBudget {
    /// Derive the budget for one cache from the byte size of the
    /// resource backing it (the kernel heap arena), per the documented
    /// policy fractions. A tiny backing yields a tiny budget; zero
    /// yields zero, which admits nothing (fail closed, never a panic).
    #[must_use]
    pub const fn from_backing(backing_bytes: usize) -> Self {
        let hard = backing_bytes / BACKING_DIVISOR;
        Self {
            hard,
            low: hard / LOW_DIVISOR * LOW_NUMERATOR,
            floor: 0,
        }
    }

    /// Derive the budget from a ceiling the consumer already knows
    /// outright, rather than as a fraction of a larger resource it only
    /// borrows part of.
    ///
    /// [`from_backing`](Self::from_backing) fits a cache taking a small
    /// share of something it does not own. Some caches instead have a
    /// ceiling that *is* the derived figure: retained window furniture
    /// can never usefully exceed one screen's worth of pixels, because
    /// no more chrome than fills the screen can be visible at once, so
    /// everything above that belongs to off-screen or stacked-under
    /// windows and is exactly what reclaim should take first.
    ///
    /// `hard_bytes` must itself come from discovered hardware — a
    /// display mode, a device geometry — never a hand-picked constant.
    /// Zero yields zero, which admits nothing (fail closed).
    #[must_use]
    pub const fn from_ceiling(hard_bytes: usize) -> Self {
        Self {
            hard: hard_bytes,
            low: hard_bytes / LOW_DIVISOR * LOW_NUMERATOR,
            floor: 0,
        }
    }

    /// This budget, declaring `floor_bytes` of it the owner's live
    /// **working set**: bytes pressure short of severe leaves alone.
    ///
    /// A reclaimable cache is normally pure speculation — the shallowest
    /// pressure may take all of it, because rebuilding an entry is local
    /// work the owner can repeat at will. Some derived state is not like
    /// that: rebuilding it needs the filesystem, or a round trip to
    /// another process, and those are exactly what a machine short of
    /// memory has least of. Dropping such a cache at the first tightening
    /// does not free memory the system can use — the desktop's whole icon
    /// set is a fraction of one screen — while the owner, still drawing
    /// the same screen, immediately reads and decodes every entry again.
    /// The measured result is a machine that spends its scarcest resources
    /// re-deriving what it just discarded.
    ///
    /// So a cache may declare the part of its budget that is not
    /// speculation. The floor is honoured up to moderate pressure and
    /// yields entirely at severe and critical, where every class shrinks
    /// to zero and a coarser fallback is the honest answer. It binds
    /// growth and forced shrink alike, because both read the one
    /// [`shrink_target`](crate::shrink_target) policy.
    ///
    /// `floor_bytes` is clamped to `hard`: a floor above the ceiling
    /// would describe bytes the cache could never hold. It must itself be
    /// derived from discovered hardware — the display the icons are drawn
    /// on, the machine's RAM — never a hand-picked constant.
    #[must_use]
    pub const fn with_working_set_floor(mut self, floor_bytes: usize) -> Self {
        self.floor = if floor_bytes > self.hard {
            self.hard
        } else {
            floor_bytes
        };
        self
    }

    /// The working-set bytes a forced shrink short of severe pressure
    /// leaves in place. Zero unless
    /// [`with_working_set_floor`](Self::with_working_set_floor) declared
    /// otherwise.
    #[must_use]
    pub const fn floor(self) -> usize {
        self.floor
    }

    /// The ceiling an insert may never push usage past.
    #[must_use]
    pub const fn hard(self) -> usize {
        self.hard
    }

    /// The watermark a forced shrink evicts down to.
    #[must_use]
    pub const fn low(self) -> usize {
        self.low
    }
}

/// The running byte ledger and event counters of one bounded cache.
///
/// Bytes and live entries are kept per [`ReclaimClass`], split into the
/// entry payloads and the per-entry bookkeeping metadata charged on top
/// of them, so the payload and metadata contributions stay separately
/// observable; every mutation is checked-arithmetic and fails closed
/// with [`AccountingError`] rather than wrapping. Event counters
/// saturate: they are diagnostics, and a saturated diagnostic is still
/// truthful about "a very large number".
///
/// The ledger is interior-atomic so one instance can be shared
/// (`alloc::sync::Arc`) with the read-only System Information export
/// while the owning cache keeps mutating it. **Mutation is
/// single-writer**: the owning cache serialises every charge, discharge,
/// and record under its own lock (the checked read-modify-write pairs
/// rely on that), while readers take lock-free per-field snapshots — a
/// multi-field view may straddle an in-flight mutation, but each field
/// itself is never torn.
#[derive(Debug, Default)]
pub struct CacheAccounting {
    payload_bytes: [AtomicUsize; ReclaimClass::ALL.len()],
    metadata_bytes: [AtomicUsize; ReclaimClass::ALL.len()],
    /// Live entries per class: one increment per successful charge, one
    /// decrement per successful discharge, zeroed with the byte ledger.
    entries: [AtomicU64; ReclaimClass::ALL.len()],
    hits: [AtomicU64; ReclaimClass::ALL.len()],
    misses: [AtomicU64; ReclaimClass::ALL.len()],
    insertions: AtomicU64,
    invalidations: AtomicU64,
    evictions: AtomicU64,
    refusals: [AtomicU64; ReclaimClass::ALL.len()],
    pressure_shrinks: [AtomicU64; ReclaimClass::ALL.len()],
    teardowns: [AtomicU64; ReclaimClass::ALL.len()],
    failures: [AtomicU64; ReclaimClass::ALL.len()],
}

/// One reclaim class's exported ledger figures: the live byte/entry
/// gauges plus the monotonic per-class event counters, all as `u64`
/// (`AGENTS.md` 64-bit-native rule for exported sizes).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ReclaimClassStats {
    /// Cached payload bytes currently held for the class.
    pub payload_bytes: u64,
    /// Per-entry bookkeeping metadata bytes currently held.
    pub metadata_bytes: u64,
    /// Entries currently held.
    pub entries: u64,
    /// Admissions refused.
    pub refusals: u64,
    /// Pressure-forced shrink passes that hit the class.
    pub pressure_shrinks: u64,
    /// Whole-cache teardown drains that hit the class.
    pub teardowns: u64,
    /// Detected internal failures attributed to the class.
    pub failures: u64,
    /// Lookups of the class served from cache (the cache avoided the
    /// canonical source): the numerator of the class's hit ratio.
    pub hits: u64,
    /// Lookups of the class that fell through to the canonical source:
    /// the miss half of the hit ratio.
    pub misses: u64,
}

/// Saturating sum of a per-class counter array (a whole-cache view of a
/// class-attributed diagnostic).
fn class_sum(counters: &[AtomicU64; ReclaimClass::ALL.len()]) -> u64 {
    counters.iter().fold(0u64, |total, counter| {
        total.saturating_add(counter.load(Ordering::Relaxed))
    })
}

/// Saturating increment of one saturating diagnostic counter
/// (single-writer: the owning cache serialises mutations).
fn bump(counter: &AtomicU64) {
    let value = counter.load(Ordering::Relaxed);
    counter.store(value.saturating_add(1), Ordering::Relaxed);
}

impl CacheAccounting {
    /// An empty ledger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            payload_bytes: [const { AtomicUsize::new(0) }; ReclaimClass::ALL.len()],
            metadata_bytes: [const { AtomicUsize::new(0) }; ReclaimClass::ALL.len()],
            entries: [const { AtomicU64::new(0) }; ReclaimClass::ALL.len()],
            hits: [const { AtomicU64::new(0) }; ReclaimClass::ALL.len()],
            misses: [const { AtomicU64::new(0) }; ReclaimClass::ALL.len()],
            insertions: AtomicU64::new(0),
            invalidations: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            refusals: [const { AtomicU64::new(0) }; ReclaimClass::ALL.len()],
            pressure_shrinks: [const { AtomicU64::new(0) }; ReclaimClass::ALL.len()],
            teardowns: [const { AtomicU64::new(0) }; ReclaimClass::ALL.len()],
            failures: [const { AtomicU64::new(0) }; ReclaimClass::ALL.len()],
        }
    }

    /// Total bytes currently charged across all classes, payload and
    /// per-entry metadata together.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        // Per-class charges are individually checked, and each fits the
        // budget ceiling, so their sum cannot overflow in practice;
        // saturating keeps the diagnostic truthful even if it could.
        let mut total = 0usize;
        for i in 0..ReclaimClass::ALL.len() {
            total = total.saturating_add(self.payload_bytes[i].load(Ordering::Relaxed));
            total = total.saturating_add(self.metadata_bytes[i].load(Ordering::Relaxed));
        }
        total
    }

    /// Bytes currently charged to `class`, payload and per-entry
    /// metadata together — the footprint the budget and shrink targets
    /// bound.
    #[must_use]
    pub fn class_bytes(&self, class: ReclaimClass) -> usize {
        self.class_payload_bytes(class)
            .saturating_add(self.class_metadata_bytes(class))
    }

    /// Payload bytes currently charged to `class`.
    #[must_use]
    pub fn class_payload_bytes(&self, class: ReclaimClass) -> usize {
        self.payload_bytes[class.index()].load(Ordering::Relaxed)
    }

    /// Per-entry bookkeeping metadata bytes currently charged to
    /// `class`.
    #[must_use]
    pub fn class_metadata_bytes(&self, class: ReclaimClass) -> usize {
        self.metadata_bytes[class.index()].load(Ordering::Relaxed)
    }

    /// Charge an admitted entry to `class`: `payload` bytes of cached
    /// content plus `metadata` bytes of per-entry bookkeeping.
    ///
    /// # Errors
    ///
    /// [`AccountingError::Overflow`] if the ledger cannot represent
    /// either new component total; nothing is charged.
    pub fn charge(
        &self,
        class: ReclaimClass,
        payload: usize,
        metadata: usize,
    ) -> Result<(), AccountingError> {
        let i = class.index();
        let new_payload = self.payload_bytes[i]
            .load(Ordering::Relaxed)
            .checked_add(payload)
            .ok_or(AccountingError::Overflow)?;
        let new_metadata = self.metadata_bytes[i]
            .load(Ordering::Relaxed)
            .checked_add(metadata)
            .ok_or(AccountingError::Overflow)?;
        self.payload_bytes[i].store(new_payload, Ordering::Relaxed);
        self.metadata_bytes[i].store(new_metadata, Ordering::Relaxed);
        bump(&self.entries[i]);
        bump(&self.insertions);
        Ok(())
    }

    /// Discharge a removed entry from `class`: `payload` bytes of
    /// cached content plus `metadata` bytes of per-entry bookkeeping.
    ///
    /// # Errors
    ///
    /// [`AccountingError::Underflow`] if more is discharged than was
    /// ever charged in either component, or no live entry remains — the
    /// books no longer balance; nothing is changed.
    pub fn discharge(
        &self,
        class: ReclaimClass,
        payload: usize,
        metadata: usize,
    ) -> Result<(), AccountingError> {
        let i = class.index();
        let new_payload = self.payload_bytes[i]
            .load(Ordering::Relaxed)
            .checked_sub(payload)
            .ok_or(AccountingError::Underflow)?;
        let new_metadata = self.metadata_bytes[i]
            .load(Ordering::Relaxed)
            .checked_sub(metadata)
            .ok_or(AccountingError::Underflow)?;
        let new_entries = self.entries[i]
            .load(Ordering::Relaxed)
            .checked_sub(1)
            .ok_or(AccountingError::Underflow)?;
        self.payload_bytes[i].store(new_payload, Ordering::Relaxed);
        self.metadata_bytes[i].store(new_metadata, Ordering::Relaxed);
        self.entries[i].store(new_entries, Ordering::Relaxed);
        Ok(())
    }

    /// Reset the byte and entry ledger to empty, keeping the event
    /// counters.
    ///
    /// This is the fail-closed companion of a whole-cache purge: after
    /// every entry has been dropped the ledger is empty by definition,
    /// and on the poison path (a detected charge/discharge imbalance)
    /// it is the only truthful value left. Never a substitute for
    /// per-entry [`discharge`](Self::discharge) in normal operation.
    pub fn zero_ledger(&self) {
        for i in 0..ReclaimClass::ALL.len() {
            self.payload_bytes[i].store(0, Ordering::Relaxed);
            self.metadata_bytes[i].store(0, Ordering::Relaxed);
            self.entries[i].store(0, Ordering::Relaxed);
        }
    }

    /// Record a lookup of `class` served from the cache.
    pub fn record_hit(&self, class: ReclaimClass) {
        bump(&self.hits[class.index()]);
    }

    /// Record a lookup of `class` that fell through to the canonical
    /// source.
    pub fn record_miss(&self, class: ReclaimClass) {
        bump(&self.misses[class.index()]);
    }

    /// Record an entry dropped because its source changed.
    pub fn record_invalidation(&self) {
        bump(&self.invalidations);
    }

    /// Record an entry evicted for space.
    pub fn record_eviction(&self) {
        bump(&self.evictions);
    }

    /// Record an entry of `class` refused admission (over-bound,
    /// unaccountable, or allocation failure).
    pub fn record_refusal(&self, class: ReclaimClass) {
        bump(&self.refusals[class.index()]);
    }

    /// Record one pressure-forced shrink pass that actually reclaimed
    /// entries of `class` (the band's target was below the class's
    /// resident footprint).
    pub fn record_pressure_shrink(&self, class: ReclaimClass) {
        bump(&self.pressure_shrinks[class.index()]);
    }

    /// Record one whole-cache drain on an owner-teardown path (volume
    /// unmount, transaction rollback purge, cache drop) as it hits
    /// `class`; a cache holding several classes records the drain once
    /// per class it declares.
    pub fn record_teardown(&self, class: ReclaimClass) {
        bump(&self.teardowns[class.index()]);
    }

    /// Record one detected internal failure attributed to `class`: a
    /// ledger or index defect that poisoned the cache (fail closed).
    pub fn record_failure(&self, class: ReclaimClass) {
        bump(&self.failures[class.index()]);
    }

    /// Lookups served from the cache, summed across every class.
    #[must_use]
    pub fn hits(&self) -> u64 {
        class_sum(&self.hits)
    }

    /// Lookups that fell through to the canonical source, summed across
    /// every class.
    #[must_use]
    pub fn misses(&self) -> u64 {
        class_sum(&self.misses)
    }

    /// Lookups of `class` served from the cache.
    #[must_use]
    pub fn class_hits(&self, class: ReclaimClass) -> u64 {
        self.hits[class.index()].load(Ordering::Relaxed)
    }

    /// Lookups of `class` that fell through to the canonical source.
    #[must_use]
    pub fn class_misses(&self, class: ReclaimClass) -> u64 {
        self.misses[class.index()].load(Ordering::Relaxed)
    }

    /// Entries admitted.
    #[must_use]
    pub fn insertions(&self) -> u64 {
        self.insertions.load(Ordering::Relaxed)
    }

    /// Entries dropped because their source changed.
    #[must_use]
    pub fn invalidations(&self) -> u64 {
        self.invalidations.load(Ordering::Relaxed)
    }

    /// Entries evicted for space.
    #[must_use]
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    /// Entries refused admission, summed across every class.
    #[must_use]
    pub fn refusals(&self) -> u64 {
        class_sum(&self.refusals)
    }

    /// Pressure-forced shrink passes that reclaimed entries, summed
    /// across every class.
    #[must_use]
    pub fn pressure_shrinks(&self) -> u64 {
        class_sum(&self.pressure_shrinks)
    }

    /// Whole-cache drains on owner-teardown paths, summed across every
    /// class.
    #[must_use]
    pub fn teardowns(&self) -> u64 {
        class_sum(&self.teardowns)
    }

    /// Detected internal failures that poisoned the cache, summed
    /// across every class.
    #[must_use]
    pub fn failures(&self) -> u64 {
        class_sum(&self.failures)
    }

    /// Entries currently held for `class` (live gauge: charged minus
    /// discharged, zeroed with the byte ledger).
    #[must_use]
    pub fn class_entries(&self, class: ReclaimClass) -> u64 {
        self.entries[class.index()].load(Ordering::Relaxed)
    }

    /// Entries of `class` refused admission.
    #[must_use]
    pub fn class_refusals(&self, class: ReclaimClass) -> u64 {
        self.refusals[class.index()].load(Ordering::Relaxed)
    }

    /// Pressure-forced shrink passes that reclaimed entries of `class`.
    #[must_use]
    pub fn class_pressure_shrinks(&self, class: ReclaimClass) -> u64 {
        self.pressure_shrinks[class.index()].load(Ordering::Relaxed)
    }

    /// Whole-cache teardown drains that hit `class`.
    #[must_use]
    pub fn class_teardowns(&self, class: ReclaimClass) -> u64 {
        self.teardowns[class.index()].load(Ordering::Relaxed)
    }

    /// Detected internal failures attributed to `class`.
    #[must_use]
    pub fn class_failures(&self, class: ReclaimClass) -> u64 {
        self.failures[class.index()].load(Ordering::Relaxed)
    }

    /// Snapshot every exported per-class figure of `class` at once.
    ///
    /// A lock-free read: the figures are loaded field by field, so the
    /// snapshot may straddle an in-flight mutation (each field itself is
    /// never torn) — the sampling semantics every live gauge has.
    #[must_use]
    pub fn class_stats(&self, class: ReclaimClass) -> ReclaimClassStats {
        ReclaimClassStats {
            payload_bytes: self.class_payload_bytes(class) as u64,
            metadata_bytes: self.class_metadata_bytes(class) as u64,
            entries: self.class_entries(class),
            refusals: self.class_refusals(class),
            pressure_shrinks: self.class_pressure_shrinks(class),
            teardowns: self.class_teardowns(class),
            failures: self.class_failures(class),
            hits: self.class_hits(class),
            misses: self.class_misses(class),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully declared, admissible candidate for `class`.
    fn candidate(class: ReclaimClass) -> CacheCandidate {
        CacheCandidate {
            class: Some(class),
            owner: Some(ReclaimOwner::FilesystemVolume { volume: 7 }),
            rebuild_cost: RebuildCost::Cheap,
            sensitivity: Some(Sensitivity::UserData),
            invalidation: Some(InvalidationSource::SourceMutation),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: 96,
        }
    }

    #[test]
    fn every_class_maps_to_a_distinct_reclaim_priority() {
        let mut previous: Option<u8> = None;
        for class in ReclaimClass::ALL {
            let priority = class.reclaim_priority();
            if let Some(previous) = previous {
                assert!(previous < priority, "{class:?} breaks the order");
            }
            previous = Some(priority);
        }
        assert!(
            ReclaimClass::CleanFileData.reclaim_priority()
                < ReclaimClass::TransformCache.reclaim_priority()
        );
        assert!(
            ReclaimClass::DisposableUi.reclaim_priority()
                < ReclaimClass::CleanFileData.reclaim_priority()
        );
    }

    #[test]
    fn every_known_class_classifies_with_full_declarations() {
        for class in ReclaimClass::ALL {
            let policy = candidate(class).classify().expect("admissible");
            assert_eq!(policy.class(), class);
            assert_eq!(policy.owner(), ReclaimOwner::FilesystemVolume { volume: 7 });
            assert_eq!(policy.rebuild_cost(), RebuildCost::Cheap);
            assert_eq!(policy.sensitivity(), Sensitivity::UserData);
            assert_eq!(policy.invalidation(), InvalidationSource::SourceMutation);
            assert_eq!(policy.rule(), ReclaimRule::Drop);
        }
    }

    #[test]
    fn unknown_class_is_refused() {
        let mut c = candidate(ReclaimClass::CleanFileData);
        c.class = None;
        assert_eq!(c.classify(), Err(AdmissionRefusal::UnknownClass));
    }

    #[test]
    fn unknown_owner_is_refused() {
        let mut c = candidate(ReclaimClass::CleanFileData);
        c.owner = None;
        assert_eq!(c.classify(), Err(AdmissionRefusal::UnknownOwner));
    }

    #[test]
    fn credential_or_key_material_is_refused() {
        let mut c = candidate(ReclaimClass::CleanFileData);
        c.sensitivity = Some(Sensitivity::CredentialOrKey);
        assert_eq!(c.classify(), Err(AdmissionRefusal::SensitiveMaterial));
    }

    #[test]
    fn unknown_sensitivity_is_refused_as_sensitive() {
        let mut c = candidate(ReclaimClass::CleanFileData);
        c.sensitivity = None;
        assert_eq!(c.classify(), Err(AdmissionRefusal::SensitiveMaterial));
    }

    #[test]
    fn unbounded_entry_metadata_is_refused() {
        let mut c = candidate(ReclaimClass::CleanFileData);
        c.entry_metadata_bytes = MAX_ENTRY_METADATA + 1;
        assert_eq!(c.classify(), Err(AdmissionRefusal::UnboundedMetadata));
        c.entry_metadata_bytes = MAX_ENTRY_METADATA;
        assert!(c.classify().is_ok());
    }

    #[test]
    fn missing_invalidation_source_is_refused() {
        let mut c = candidate(ReclaimClass::CleanFileData);
        c.invalidation = None;
        assert_eq!(c.classify(), Err(AdmissionRefusal::MissingInvalidation));
    }

    #[test]
    fn missing_reclaim_rule_is_refused_as_non_reclaimable() {
        let mut c = candidate(ReclaimClass::CleanFileData);
        c.rule = None;
        assert_eq!(c.classify(), Err(AdmissionRefusal::NonReclaimable));
    }

    #[test]
    fn classification_is_deterministic_under_equal_inputs() {
        for class in ReclaimClass::ALL {
            assert_eq!(candidate(class).classify(), candidate(class).classify());
        }
        let mut refused = candidate(ReclaimClass::FsMetadata);
        refused.owner = None;
        assert_eq!(refused.classify(), refused.classify());
    }

    #[test]
    fn accounting_charges_every_class_independently() {
        let acct = CacheAccounting::new();
        for (i, class) in ReclaimClass::ALL.into_iter().enumerate() {
            acct.charge(class, i + 1, 0).expect("charges");
        }
        for (i, class) in ReclaimClass::ALL.into_iter().enumerate() {
            assert_eq!(acct.class_bytes(class), i + 1);
        }
        assert_eq!(acct.total_bytes(), (1..=ReclaimClass::ALL.len()).sum());
        for (i, class) in ReclaimClass::ALL.into_iter().enumerate() {
            acct.discharge(class, i + 1, 0).expect("discharges");
        }
        assert_eq!(acct.total_bytes(), 0);
    }

    #[test]
    fn metadata_bytes_are_accounted_separately_per_class() {
        let acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 4096, 96)
            .expect("charges");
        acct.charge(ReclaimClass::FsMetadata, 64, 160)
            .expect("charges");
        assert_eq!(acct.class_payload_bytes(ReclaimClass::CleanFileData), 4096);
        assert_eq!(acct.class_metadata_bytes(ReclaimClass::CleanFileData), 96);
        assert_eq!(acct.class_bytes(ReclaimClass::CleanFileData), 4192);
        assert_eq!(acct.class_payload_bytes(ReclaimClass::FsMetadata), 64);
        assert_eq!(acct.class_metadata_bytes(ReclaimClass::FsMetadata), 160);
        assert_eq!(acct.total_bytes(), 4096 + 96 + 64 + 160);
        acct.discharge(ReclaimClass::CleanFileData, 4096, 96)
            .expect("discharges");
        assert_eq!(acct.class_payload_bytes(ReclaimClass::CleanFileData), 0);
        assert_eq!(acct.class_metadata_bytes(ReclaimClass::CleanFileData), 0);
        assert_eq!(acct.total_bytes(), 64 + 160);
    }

    #[test]
    fn budget_is_derived_from_the_backing_with_hysteresis() {
        let budget = CacheBudget::from_backing(64 * 1024 * 1024);
        assert_eq!(budget.hard(), 4 * 1024 * 1024);
        assert_eq!(budget.low(), 3 * 1024 * 1024);
        assert!(budget.low() < budget.hard());
    }

    #[test]
    fn a_known_ceiling_is_taken_whole_with_the_same_hysteresis() {
        let budget = CacheBudget::from_ceiling(64 * 1024 * 1024);
        assert_eq!(budget.hard(), 64 * 1024 * 1024);
        assert_eq!(budget.low(), 48 * 1024 * 1024);
        assert_eq!(CacheBudget::from_ceiling(0), CacheBudget::from_backing(0));
    }

    #[test]
    fn zero_backing_admits_nothing() {
        let budget = CacheBudget::from_backing(0);
        assert_eq!(budget.hard(), 0);
        assert_eq!(budget.low(), 0);
    }

    #[test]
    fn charge_and_discharge_balance_per_class() {
        let acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 4096, 0)
            .expect("charges");
        acct.charge(ReclaimClass::FsMetadata, 128, 0)
            .expect("charges");
        assert_eq!(acct.class_bytes(ReclaimClass::CleanFileData), 4096);
        assert_eq!(acct.class_bytes(ReclaimClass::FsMetadata), 128);
        assert_eq!(acct.total_bytes(), 4224);
        acct.discharge(ReclaimClass::CleanFileData, 4096, 0)
            .expect("discharges");
        assert_eq!(acct.total_bytes(), 128);
    }

    #[test]
    fn overflow_is_refused_and_charges_nothing() {
        let acct = CacheAccounting::new();
        acct.charge(ReclaimClass::FsMetadata, usize::MAX, 0)
            .expect("charges");
        assert_eq!(
            acct.charge(ReclaimClass::FsMetadata, 1, 0),
            Err(AccountingError::Overflow)
        );
        assert_eq!(acct.class_bytes(ReclaimClass::FsMetadata), usize::MAX);
    }

    #[test]
    fn metadata_overflow_is_refused_and_charges_neither_component() {
        let acct = CacheAccounting::new();
        acct.charge(ReclaimClass::FsMetadata, 0, usize::MAX)
            .expect("charges");
        assert_eq!(
            acct.charge(ReclaimClass::FsMetadata, 1, 1),
            Err(AccountingError::Overflow)
        );
        assert_eq!(acct.class_payload_bytes(ReclaimClass::FsMetadata), 0);
        assert_eq!(
            acct.class_metadata_bytes(ReclaimClass::FsMetadata),
            usize::MAX
        );
    }

    #[test]
    fn underflow_is_refused_and_discharges_nothing() {
        let acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 10, 0)
            .expect("charges");
        assert_eq!(
            acct.discharge(ReclaimClass::CleanFileData, 11, 0),
            Err(AccountingError::Underflow)
        );
        assert_eq!(acct.class_bytes(ReclaimClass::CleanFileData), 10);
    }

    #[test]
    fn metadata_underflow_is_refused_and_discharges_neither_component() {
        let acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 10, 4)
            .expect("charges");
        assert_eq!(
            acct.discharge(ReclaimClass::CleanFileData, 10, 5),
            Err(AccountingError::Underflow)
        );
        assert_eq!(acct.class_payload_bytes(ReclaimClass::CleanFileData), 10);
        assert_eq!(acct.class_metadata_bytes(ReclaimClass::CleanFileData), 4);
    }

    #[test]
    fn event_counters_track_each_path() {
        let acct = CacheAccounting::new();
        acct.record_hit(ReclaimClass::CleanFileData);
        acct.record_miss(ReclaimClass::CleanFileData);
        acct.record_invalidation();
        acct.record_eviction();
        acct.record_refusal(ReclaimClass::CleanFileData);
        acct.record_pressure_shrink(ReclaimClass::CleanFileData);
        acct.record_teardown(ReclaimClass::CleanFileData);
        acct.record_failure(ReclaimClass::CleanFileData);
        assert_eq!(acct.hits(), 1);
        assert_eq!(acct.misses(), 1);
        assert_eq!(acct.invalidations(), 1);
        assert_eq!(acct.evictions(), 1);
        assert_eq!(acct.refusals(), 1);
        assert_eq!(acct.pressure_shrinks(), 1);
        assert_eq!(acct.teardowns(), 1);
        assert_eq!(acct.failures(), 1);
        assert_eq!(acct.insertions(), 0);
    }

    #[test]
    fn class_counters_attribute_events_to_their_class() {
        let acct = CacheAccounting::new();
        acct.record_refusal(ReclaimClass::CleanFileData);
        acct.record_refusal(ReclaimClass::CleanFileData);
        acct.record_refusal(ReclaimClass::FsMetadata);
        acct.record_pressure_shrink(ReclaimClass::TransformCache);
        acct.record_teardown(ReclaimClass::SemanticAppCache);
        acct.record_failure(ReclaimClass::RuntimeCache);
        acct.record_hit(ReclaimClass::CleanFileData);
        acct.record_hit(ReclaimClass::FsMetadata);
        acct.record_miss(ReclaimClass::FsMetadata);
        assert_eq!(acct.class_refusals(ReclaimClass::CleanFileData), 2);
        assert_eq!(acct.class_refusals(ReclaimClass::FsMetadata), 1);
        assert_eq!(acct.class_refusals(ReclaimClass::TransformCache), 0);
        assert_eq!(acct.refusals(), 3);
        assert_eq!(acct.class_pressure_shrinks(ReclaimClass::TransformCache), 1);
        assert_eq!(acct.class_teardowns(ReclaimClass::SemanticAppCache), 1);
        assert_eq!(acct.class_failures(ReclaimClass::RuntimeCache), 1);
        // Hits and misses attribute to their own class, and the summed
        // accessors fold every class together.
        assert_eq!(acct.class_hits(ReclaimClass::CleanFileData), 1);
        assert_eq!(acct.class_hits(ReclaimClass::FsMetadata), 1);
        assert_eq!(acct.class_misses(ReclaimClass::FsMetadata), 1);
        assert_eq!(acct.class_misses(ReclaimClass::CleanFileData), 0);
        assert_eq!(acct.hits(), 2);
        assert_eq!(acct.misses(), 1);
    }

    #[test]
    fn entries_gauge_tracks_charge_and_discharge() {
        let acct = CacheAccounting::new();
        assert_eq!(acct.class_entries(ReclaimClass::CleanFileData), 0);
        acct.charge(ReclaimClass::CleanFileData, 4096, 32)
            .expect("charges");
        acct.charge(ReclaimClass::CleanFileData, 4096, 32)
            .expect("charges");
        assert_eq!(acct.class_entries(ReclaimClass::CleanFileData), 2);
        acct.discharge(ReclaimClass::CleanFileData, 4096, 32)
            .expect("discharges");
        assert_eq!(acct.class_entries(ReclaimClass::CleanFileData), 1);
        // A discharge with no live entry no longer balances: fail closed.
        acct.discharge(ReclaimClass::CleanFileData, 4096, 32)
            .expect("discharges");
        assert_eq!(
            acct.discharge(ReclaimClass::CleanFileData, 0, 0),
            Err(AccountingError::Underflow)
        );
        assert_eq!(acct.class_entries(ReclaimClass::CleanFileData), 0);
    }

    #[test]
    fn zero_ledger_clears_both_components_and_keeps_counters() {
        let acct = CacheAccounting::new();
        acct.charge(ReclaimClass::TransformCache, 512, 96)
            .expect("charges");
        acct.record_teardown(ReclaimClass::TransformCache);
        acct.zero_ledger();
        assert_eq!(acct.total_bytes(), 0);
        assert_eq!(acct.class_payload_bytes(ReclaimClass::TransformCache), 0);
        assert_eq!(acct.class_metadata_bytes(ReclaimClass::TransformCache), 0);
        assert_eq!(acct.class_entries(ReclaimClass::TransformCache), 0);
        assert_eq!(acct.teardowns(), 1);
        assert_eq!(acct.insertions(), 1);
    }
}
