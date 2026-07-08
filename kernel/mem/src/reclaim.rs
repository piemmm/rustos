//! Reclaimable-memory classification, budget, and accounting
//! (`plans/SMARTRAM.md`).
//!
//! A reclaimable cache holds *derived* state — data that can always be
//! rebuilt from its canonical source — so the memory it occupies is a
//! loan the VM can call in at any time. This module is the one
//! definition of how such a cache is classed, bounded, and accounted:
//! the consumer (today the filesystem cache in `kernel/core::fs`)
//! charges every entry here, and reclaim decisions read these numbers
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
    const fn index(self) -> usize {
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
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CacheBudget {
    hard: usize,
    low: usize,
}

/// The backing-resource fraction a filesystem cache may occupy.
///
/// One volume's cache is capped at 1/16 of the kernel heap arena: with
/// the fixed 64 MiB heap this is 4 MiB per volume, so the two boot
/// volumes together stay under 1/8 of the heap and cache growth can
/// never be the cause of kernel-heap exhaustion (`plans/SMARTRAM.md`
/// section 7 — reserves are preserved by construction).
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
        }
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
/// Bytes are kept per [`ReclaimClass`]; every mutation is
/// checked-arithmetic and fails closed with [`AccountingError`] rather
/// than wrapping. Event counters saturate: they are diagnostics, and a
/// saturated diagnostic is still truthful about "a very large number".
#[derive(Debug, Default)]
pub struct CacheAccounting {
    class_bytes: [usize; ReclaimClass::ALL.len()],
    hits: u64,
    misses: u64,
    insertions: u64,
    invalidations: u64,
    evictions: u64,
    refusals: u64,
}

impl CacheAccounting {
    /// An empty ledger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            class_bytes: [0; ReclaimClass::ALL.len()],
            hits: 0,
            misses: 0,
            insertions: 0,
            invalidations: 0,
            evictions: 0,
            refusals: 0,
        }
    }

    /// Total bytes currently charged across all classes.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        // Per-class charges are individually checked, and each fits the
        // budget ceiling, so their sum cannot overflow in practice;
        // saturating keeps the diagnostic truthful even if it could.
        let mut total = 0usize;
        let mut i = 0;
        while i < self.class_bytes.len() {
            total = total.saturating_add(self.class_bytes[i]);
            i += 1;
        }
        total
    }

    /// Bytes currently charged to `class`.
    #[must_use]
    pub const fn class_bytes(&self, class: ReclaimClass) -> usize {
        self.class_bytes[class.index()]
    }

    /// Charge `bytes` to `class` for an admitted entry.
    ///
    /// # Errors
    ///
    /// [`AccountingError::Overflow`] if the ledger cannot represent the
    /// new total; nothing is charged.
    pub fn charge(&mut self, class: ReclaimClass, bytes: usize) -> Result<(), AccountingError> {
        let slot = &mut self.class_bytes[class.index()];
        *slot = slot.checked_add(bytes).ok_or(AccountingError::Overflow)?;
        self.insertions = self.insertions.saturating_add(1);
        Ok(())
    }

    /// Discharge `bytes` from `class` for a removed entry.
    ///
    /// # Errors
    ///
    /// [`AccountingError::Underflow`] if more is discharged than was
    /// ever charged — the books no longer balance; nothing is changed.
    pub fn discharge(&mut self, class: ReclaimClass, bytes: usize) -> Result<(), AccountingError> {
        let slot = &mut self.class_bytes[class.index()];
        *slot = slot.checked_sub(bytes).ok_or(AccountingError::Underflow)?;
        Ok(())
    }

    /// Reset the byte ledger to empty, keeping the event counters.
    ///
    /// This is the fail-closed companion of a whole-cache purge: after
    /// every entry has been dropped the ledger is empty by definition,
    /// and on the poison path (a detected charge/discharge imbalance)
    /// it is the only truthful value left. Never a substitute for
    /// per-entry [`discharge`](Self::discharge) in normal operation.
    pub fn zero_ledger(&mut self) {
        self.class_bytes = [0; ReclaimClass::ALL.len()];
    }

    /// Record a lookup served from the cache.
    pub fn record_hit(&mut self) {
        self.hits = self.hits.saturating_add(1);
    }

    /// Record a lookup that fell through to the canonical source.
    pub fn record_miss(&mut self) {
        self.misses = self.misses.saturating_add(1);
    }

    /// Record an entry dropped because its source changed.
    pub fn record_invalidation(&mut self) {
        self.invalidations = self.invalidations.saturating_add(1);
    }

    /// Record an entry evicted for space.
    pub fn record_eviction(&mut self) {
        self.evictions = self.evictions.saturating_add(1);
    }

    /// Record an entry refused admission (over-bound, unaccountable, or
    /// allocation failure).
    pub fn record_refusal(&mut self) {
        self.refusals = self.refusals.saturating_add(1);
    }

    /// Lookups served from the cache.
    #[must_use]
    pub const fn hits(&self) -> u64 {
        self.hits
    }

    /// Lookups that fell through to the canonical source.
    #[must_use]
    pub const fn misses(&self) -> u64 {
        self.misses
    }

    /// Entries admitted.
    #[must_use]
    pub const fn insertions(&self) -> u64 {
        self.insertions
    }

    /// Entries dropped because their source changed.
    #[must_use]
    pub const fn invalidations(&self) -> u64 {
        self.invalidations
    }

    /// Entries evicted for space.
    #[must_use]
    pub const fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Entries refused admission.
    #[must_use]
    pub const fn refusals(&self) -> u64 {
        self.refusals
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
        let mut acct = CacheAccounting::new();
        for (i, class) in ReclaimClass::ALL.into_iter().enumerate() {
            acct.charge(class, i + 1).expect("charges");
        }
        for (i, class) in ReclaimClass::ALL.into_iter().enumerate() {
            assert_eq!(acct.class_bytes(class), i + 1);
        }
        assert_eq!(acct.total_bytes(), (1..=ReclaimClass::ALL.len()).sum());
        for (i, class) in ReclaimClass::ALL.into_iter().enumerate() {
            acct.discharge(class, i + 1).expect("discharges");
        }
        assert_eq!(acct.total_bytes(), 0);
    }

    #[test]
    fn budget_is_derived_from_the_backing_with_hysteresis() {
        let budget = CacheBudget::from_backing(64 * 1024 * 1024);
        assert_eq!(budget.hard(), 4 * 1024 * 1024);
        assert_eq!(budget.low(), 3 * 1024 * 1024);
        assert!(budget.low() < budget.hard());
    }

    #[test]
    fn zero_backing_admits_nothing() {
        let budget = CacheBudget::from_backing(0);
        assert_eq!(budget.hard(), 0);
        assert_eq!(budget.low(), 0);
    }

    #[test]
    fn charge_and_discharge_balance_per_class() {
        let mut acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 4096)
            .expect("charges");
        acct.charge(ReclaimClass::FsMetadata, 128).expect("charges");
        assert_eq!(acct.class_bytes(ReclaimClass::CleanFileData), 4096);
        assert_eq!(acct.class_bytes(ReclaimClass::FsMetadata), 128);
        assert_eq!(acct.total_bytes(), 4224);
        acct.discharge(ReclaimClass::CleanFileData, 4096)
            .expect("discharges");
        assert_eq!(acct.total_bytes(), 128);
    }

    #[test]
    fn overflow_is_refused_and_charges_nothing() {
        let mut acct = CacheAccounting::new();
        acct.charge(ReclaimClass::FsMetadata, usize::MAX)
            .expect("charges");
        assert_eq!(
            acct.charge(ReclaimClass::FsMetadata, 1),
            Err(AccountingError::Overflow)
        );
        assert_eq!(acct.class_bytes(ReclaimClass::FsMetadata), usize::MAX);
    }

    #[test]
    fn underflow_is_refused_and_discharges_nothing() {
        let mut acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 10)
            .expect("charges");
        assert_eq!(
            acct.discharge(ReclaimClass::CleanFileData, 11),
            Err(AccountingError::Underflow)
        );
        assert_eq!(acct.class_bytes(ReclaimClass::CleanFileData), 10);
    }

    #[test]
    fn event_counters_track_each_path() {
        let mut acct = CacheAccounting::new();
        acct.record_hit();
        acct.record_miss();
        acct.record_invalidation();
        acct.record_eviction();
        acct.record_refusal();
        assert_eq!(acct.hits(), 1);
        assert_eq!(acct.misses(), 1);
        assert_eq!(acct.invalidations(), 1);
        assert_eq!(acct.evictions(), 1);
        assert_eq!(acct.refusals(), 1);
        assert_eq!(acct.insertions(), 0);
    }
}
