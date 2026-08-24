//! TAIRiX self-optimising CPU-dispatch framework (`lib/cpuops`).
//!
//! See the crate `README.md` and `plans/FIX-HARDWARE-FEATURES.md` for the full
//! design. In one line: this crate selects, per boot per distinct core type,
//! the fastest *correct*, feature-legal implementation of an accelerated
//! operation from a set of candidates, always falling closed to a portable
//! baseline. It reads capability from `tairix_abi::cpufeatures` (never
//! benchmarks it) and decides performance with an optional bounded benchmark.
//!
//! The crate is `no_std`, contains no `unsafe`, and names no architecture,
//! board, or `SoC`: concrete routines live in their owning crates and are gated
//! on the discovered feature bits this framework matches against. The selection
//! algorithm allocates nothing; only the optional [`OpsTables`] uses the heap,
//! behind the default-on `alloc` feature, so an allocator-free consumer depends
//! on this crate with `default-features = false`.

#![no_std]

// `alloc` backs only the optional [`OpsTables`]; the selection algorithm itself
// allocates nothing. Gating it behind the (default-on) `alloc` feature lets a
// consumer that only *selects* a routine (`lib/crc32c`, `lib/crypto`) depend on
// this crate with `default-features = false` and stay free of any global
// allocator, while a consumer that builds an `OpsTables` enables it.
#[cfg(feature = "alloc")]
extern crate alloc;

pub mod bench;

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

pub use bench::{BenchHarness, CycleCounter};
// The capability vocabulary is shared with the Arch HAL from the one ABI
// definition, so a candidate's required-feature gate is the exact set the ports
// produce.
pub use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};

/// The identity of a distinct core type — the key an [`OpsTables`] resolves one
/// resolved ops table per.
///
/// On asymmetric silicon (`big.LITTLE`, Intel hybrid) a feature set and a
/// benchmark measured on one cluster do not describe another, so selection is
/// keyed on this and resolved per core type as each CPU comes up, never
/// measured once on the boot CPU and imposed globally. It wraps the raw
/// hardware identity register the Arch HAL reads (`MIDR_EL1`, the `CPUID`
/// signature, `mvendorid:marchid:mimpid`); distinct microarchitectures carry
/// distinct raw ids, so equality separates clusters without a second pass.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CoreKey(pub u64);

/// A stable name identifying one op family (CRC32, the crypto-backend
/// availability decision, `memcpy`, …) across the log and the operator pin.
///
/// The framework is generic over *which* families exist — a consumer declares
/// its own [`Family`] with its own id — so this is a stable string label, not
/// a closed enum the framework would have to grow a variant of for every
/// consumer (which would be speculative surface the consumer, not the
/// framework, owns).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct FamilyId(pub &'static str);

/// One candidate implementation of an op family.
///
/// `impl_` is the op's implementation handle — typically an `extern "C" fn`
/// pointer consumed on the hot path, but any [`Copy`] handle the family's
/// `run`/reference adapters understand. `requires` is the exact set of
/// [`CpuFeature`] bits the implementation needs; the [`Selector`] filters a
/// candidate out unless *every* required bit is present, so an unsupported
/// instruction is never reached (it would trap).
#[derive(Copy, Clone, Debug)]
pub struct Candidate<T: Copy> {
    /// Stable, human-readable name — the log and pin key within a family.
    pub name: &'static str,
    /// The exact feature bits this implementation needs to be legal.
    pub requires: &'static [CpuFeature],
    /// The implementation handle consumed on the hot path.
    pub impl_: T,
}

/// How the [`Selector`] chooses among equally-correct, feature-legal survivors.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Selection {
    /// Choose the first verified survivor in declared order. The only correct
    /// policy for a crypto-backend *availability* decision: it never lets a
    /// benchmark near a secret.
    ByPriority,
    /// Choose the fastest verified survivor by a bounded benchmark. Permitted
    /// only for a family that is bit-identical in output and handles no secret
    /// and has no timing-security requirement.
    ByBenchmark,
}

/// One accelerated operation: its candidates, its mandatory portable baseline,
/// and the portable reference plus vectors the self-verify runs against.
///
/// `T` is the implementation-handle type; `In` is one self-verify input; `Out`
/// is the operation's output, compared for bit-identity against the reference.
/// `run` invokes a candidate's `impl_` on an input, and `reference` is the
/// portable, always-correct computation every candidate must reproduce.
pub struct Family<'a, T: Copy, In, Out: PartialEq> {
    /// The family's stable id (log/pin key).
    pub id: FamilyId,
    /// Which selection policy applies.
    pub selection: Selection,
    /// The accelerated candidates, in descending declared priority.
    pub candidates: &'a [Candidate<T>],
    /// The portable, always-feature-legal last resort. Kept separate from
    /// `candidates` so it is *always* present and can never be filtered away.
    pub baseline: Candidate<T>,
    /// The portable reference the self-verify compares every survivor against.
    pub reference: fn(&In) -> Out,
    /// Invoke a candidate's implementation handle on an input.
    pub run: fn(T, &In) -> Out,
    /// The self-verify vectors (sizes, alignments, and edge cases). A family
    /// with no vectors cannot verify any candidate, so every accelerated
    /// candidate is rejected and the baseline wins (fail closed).
    pub vectors: &'a [In],
}

/// Why the [`Selector`] chose the implementation it did — the typed record for
/// the audit log.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DecisionReason {
    /// The first verified survivor in declared order (a `ByPriority` family, or
    /// a `ByBenchmark` family with a single survivor).
    Priority,
    /// The fastest survivor a benchmark measured.
    Benchmark,
    /// An operator pin named a candidate that passed the gate and verified.
    Pinned,
    /// An operator pin named a candidate that was absent, feature-illegal, or
    /// failed self-verify; the baseline was used instead.
    PinRejected,
    /// No accelerated candidate survived the gate and self-verify; the portable
    /// baseline was used.
    Baseline,
    /// The baseline itself failed self-verify — a programming error in the
    /// family. The baseline is still returned as the last resort (never a
    /// panic), and this reason flags the family for repair.
    BaselineUnverified,
}

/// The typed record of one selection, for the audit log. The crate performs no
/// I/O; the caller records this through a [`DecisionSink`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Decision {
    /// The family this decision is for.
    pub family: FamilyId,
    /// The core type it was made for.
    pub core: CoreKey,
    /// The feature set observed on that core.
    pub features: CpuFeatureSet,
    /// The name of the chosen implementation.
    pub chosen: &'static str,
    /// Why it was chosen.
    pub reason: DecisionReason,
}

/// The result of a selection: the chosen implementation handle plus the typed
/// [`Decision`] describing it.
#[derive(Copy, Clone, Debug)]
pub struct Selected<T: Copy> {
    /// The chosen implementation handle, consumed on the hot path.
    pub impl_: T,
    /// The typed record of the choice.
    pub decision: Decision,
}

/// A sink the caller supplies to record each [`Decision`] (typically
/// `lib/log`-backed in `kernel/core`). Keeping I/O out of the framework keeps
/// it `no_std` and capability-clean.
pub trait DecisionSink {
    /// Record one selection decision.
    fn record(&self, decision: &Decision);
}

/// The maximum number of accelerated candidates the [`Selector`] considers for
/// one family.
///
/// A family's candidate list is a tiny, compile-time-fixed `&'static` slice (a
/// handful of implementations of one op), so this is a validation bound on a
/// fixed set, never a machine-scaling capacity — and it lets selection run on a
/// bounded stack buffer with no heap allocation, so a consumer that only
/// selects (never builds an [`OpsTables`]) needs no global allocator.
const MAX_CANDIDATES: usize = 16;

/// The maximum number of operator pins a [`Selector`] holds.
///
/// Pins name at most one candidate per family and the set of pinnable families
/// is tiny and compile-time-fixed, so this is likewise a validation bound on a
/// fixed set, not a scaling capacity — and it lets a `Selector` hold its pins
/// on the stack, so the selection path allocates nothing.
const MAX_PINS: usize = 16;

/// The selection algorithm plus any operator pins.
///
/// Construct it once, optionally [`pin`](Self::pin) families for determinism,
/// and call [`select`](Self::select) per family per core type. The algorithm
/// is pure and host-testable; it holds no architecture, reads no register, and
/// allocates nothing — so a `no_std` consumer can resolve a routine without a
/// global allocator (the heap-backed [`OpsTables`] is a separate, opt-in type).
#[derive(Clone)]
pub struct Selector {
    pins: [Option<(FamilyId, &'static str)>; MAX_PINS],
    pin_count: usize,
}

impl Default for Selector {
    fn default() -> Self {
        Self::new()
    }
}

impl Selector {
    /// A selector with no pins.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pins: [None; MAX_PINS],
            pin_count: 0,
        }
    }

    /// Pin `family` to the candidate named `candidate` (a boot parameter
    /// override). A pinned candidate **still self-verifies**: pinning cannot
    /// defeat correctness, so a pinned buggy candidate falls to the baseline.
    /// A later pin for the same family replaces an earlier one.
    pub fn pin(&mut self, family: FamilyId, candidate: &'static str) {
        for (id, name) in self.pins[..self.pin_count].iter_mut().flatten() {
            if *id == family {
                *name = candidate;
                return;
            }
        }
        // A new pin is recorded while the bounded buffer has room. The set of
        // pinnable families is tiny and fixed, so this is never reached in
        // practice; an over-full buffer drops the extra pin (fail-safe: the
        // correct default selection still runs) rather than allocating.
        if self.pin_count < MAX_PINS {
            self.pins[self.pin_count] = Some((family, candidate));
            self.pin_count += 1;
        }
    }

    /// The pinned candidate name for `family`, if any.
    #[must_use]
    pub fn pinned(&self, family: FamilyId) -> Option<&'static str> {
        for (id, name) in self.pins[..self.pin_count].iter().flatten() {
            if *id == family {
                return Some(*name);
            }
        }
        None
    }

    /// Select the implementation of `family` to use on a core with the given
    /// `features`, keyed for the log by `core`.
    ///
    /// `bench` is the benchmark harness a [`Selection::ByBenchmark`] family
    /// needs; pass `None` for a [`Selection::ByPriority`] family (a
    /// `ByBenchmark` family with no harness deterministically falls to declared
    /// priority — correct, just not speed-optimal). The algorithm never panics
    /// and never busy-waits.
    #[must_use]
    pub fn select<T, In, Out>(
        &self,
        family: &Family<'_, T, In, Out>,
        features: CpuFeatureSet,
        core: CoreKey,
        bench: Option<&BenchHarness<'_>>,
    ) -> Selected<T>
    where
        T: Copy,
        Out: PartialEq,
    {
        // A pin, if present, wins when — and only when — the named candidate is
        // feature-legal and self-verifies. Otherwise the pin is rejected in
        // favour of the fail-closed baseline.
        if let Some(name) = self.pinned(family.id) {
            if let Some(candidate) = find_by_name(family, name) {
                let (impl_, chosen, reason) = if legal_and_verified(family, candidate, features) {
                    (candidate.impl_, candidate.name, DecisionReason::Pinned)
                } else {
                    (
                        family.baseline.impl_,
                        family.baseline.name,
                        DecisionReason::PinRejected,
                    )
                };
                return build(impl_, family.id, chosen, core, features, reason);
            }
            // A pin naming no known candidate is ignored; fall through to normal
            // selection rather than deny a correct default.
        }

        // Verified, feature-legal survivor indices in declared priority order,
        // gathered into a bounded stack buffer so selection allocates nothing.
        let mut survivors = [0usize; MAX_CANDIDATES];
        let mut count = 0usize;
        for (index, candidate) in family.candidates.iter().enumerate() {
            if count == MAX_CANDIDATES {
                // A family declaring more than `MAX_CANDIDATES` candidates has
                // the surplus ignored fail-safe (the highest-priority
                // survivors already are), never a panic.
                break;
            }
            if legal_and_verified(family, candidate, features) {
                survivors[count] = index;
                count += 1;
            }
        }

        if count == 0 {
            // Nothing above baseline survived. The baseline is the reference by
            // construction, but self-verify it too so a broken baseline is
            // flagged rather than silently trusted.
            let reason = if verifies(family, &family.baseline) {
                DecisionReason::Baseline
            } else {
                DecisionReason::BaselineUnverified
            };
            return build(
                family.baseline.impl_,
                family.id,
                family.baseline.name,
                core,
                features,
                reason,
            );
        }

        let survivors = &survivors[..count];
        let (position, reason) = choose(family, survivors, bench);
        let winner = &family.candidates[survivors[position]];
        build(winner.impl_, family.id, winner.name, core, features, reason)
    }
}

/// Assemble a [`Selected`] with its typed [`Decision`].
fn build<T: Copy>(
    impl_: T,
    family: FamilyId,
    chosen: &'static str,
    core: CoreKey,
    features: CpuFeatureSet,
    reason: DecisionReason,
) -> Selected<T> {
    Selected {
        impl_,
        decision: Decision {
            family,
            core,
            features,
            chosen,
            reason,
        },
    }
}

/// Choose the position (into `survivors`) and the reason to record, given the
/// family's policy and the optional benchmark harness. `survivors` is a
/// non-empty list of indices into `family.candidates`, in declared-priority
/// order.
fn choose<T, In, Out>(
    family: &Family<'_, T, In, Out>,
    survivors: &[usize],
    bench: Option<&BenchHarness<'_>>,
) -> (usize, DecisionReason)
where
    T: Copy,
    Out: PartialEq,
{
    match family.selection {
        Selection::ByPriority => (0, DecisionReason::Priority),
        Selection::ByBenchmark => {
            // A single survivor needs no measurement; a missing harness or an
            // (impossible) empty vector set falls closed to declared priority.
            match (survivors.len(), bench, family.vectors.last()) {
                (0..=1, _, _) | (_, None, _) | (_, _, None) => (0, DecisionReason::Priority),
                (_, Some(harness), Some(warm)) => {
                    // Gather the survivor implementation handles onto a bounded
                    // stack buffer (seeded with the first survivor's handle,
                    // then filled) so benchmarking allocates nothing.
                    let mut impls = [family.candidates[survivors[0]].impl_; MAX_CANDIDATES];
                    for (position, &candidate_index) in survivors.iter().enumerate() {
                        impls[position] = family.candidates[candidate_index].impl_;
                    }
                    (
                        harness.fastest(&impls[..survivors.len()], family.run, warm),
                        DecisionReason::Benchmark,
                    )
                }
            }
        }
    }
}

/// Find a candidate (or the baseline) by name within a family.
fn find_by_name<'f, T, In, Out>(
    family: &'f Family<'_, T, In, Out>,
    name: &str,
) -> Option<&'f Candidate<T>>
where
    T: Copy,
    Out: PartialEq,
{
    if family.baseline.name == name {
        return Some(&family.baseline);
    }
    family.candidates.iter().find(|c| c.name == name)
}

/// `true` if `candidate` is feature-legal on `features` and self-verifies.
fn legal_and_verified<T, In, Out>(
    family: &Family<'_, T, In, Out>,
    candidate: &Candidate<T>,
    features: CpuFeatureSet,
) -> bool
where
    T: Copy,
    Out: PartialEq,
{
    features.contains_all(candidate.requires) && verifies(family, candidate)
}

/// Run `candidate` over every self-verify vector and compare to the reference.
/// Returns `false` if the family has no vectors (unverifiable → fail closed) or
/// any output differs (a buggy accelerated path is structurally unpickable).
fn verifies<T, In, Out>(family: &Family<'_, T, In, Out>, candidate: &Candidate<T>) -> bool
where
    T: Copy,
    Out: PartialEq,
{
    if family.vectors.is_empty() {
        return false;
    }
    family
        .vectors
        .iter()
        .all(|v| (family.run)(candidate.impl_, v) == (family.reference)(v))
}

/// A growable, per-core-type table of resolved ops.
///
/// `Ops` is the consumer-defined struct of resolved implementation handles (a
/// struct of `extern "C" fn` pointers) consumed on the hot path. The map grows
/// on demand as each distinct [`CoreKey`] comes up (`big.LITTLE`, Intel
/// hybrid); the number of core types is tiny, so a linear scan is the right
/// structure, and it is a *growable capacity*, never a fixed ceiling.
///
/// Behind the default-on `alloc` feature: it is the crate's only heap user, so
/// an allocator-free consumer that never builds one (`lib/crc32c`,
/// `lib/crypto`) can turn the feature off.
#[cfg(feature = "alloc")]
pub struct OpsTables<Ops> {
    entries: Vec<(CoreKey, Ops)>,
}

#[cfg(feature = "alloc")]
impl<Ops> OpsTables<Ops> {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// The resolved ops for `core`, or `None` if that core type has not been
    /// resolved yet.
    #[must_use]
    pub fn get(&self, core: CoreKey) -> Option<&Ops> {
        self.entries
            .iter()
            .find(|(key, _)| *key == core)
            .map(|(_, ops)| ops)
    }

    /// The resolved ops for `core`, building and caching them with `build` on
    /// first sight of that core type.
    pub fn resolve(&mut self, core: CoreKey, build: impl FnOnce() -> Ops) -> &Ops {
        let index = if let Some(index) = self.entries.iter().position(|(key, _)| *key == core) {
            index
        } else {
            self.entries.push((core, build()));
            self.entries.len() - 1
        };
        &self.entries[index].1
    }

    /// The number of distinct core types resolved so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no core type has been resolved yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(feature = "alloc")]
impl<Ops> Default for OpsTables<Ops> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    // ---- A simple correctness family: "double a u64". ------------------
    //
    // The implementation handle is a `fn(u64) -> u64`. Candidates:
    //   * `crc-accel` — correct, but requires the CRC32 feature bit.
    //   * `wide`      — correct, no feature requirement.
    //   * `buggy`     — no requirement, but computes the wrong answer.
    // The portable baseline `portable` is correct and always legal.
    //
    // The self-verify input `Val` is a (non-`Copy`) newtype rather than a bare
    // `u64`, mirroring how a real consumer's input is a buffer, not a scalar
    // passed by value.
    type DblFn = fn(u64) -> u64;

    struct Val(u64);

    fn dbl(x: u64) -> u64 {
        x.wrapping_mul(2)
    }
    fn dbl_off_by_one(x: u64) -> u64 {
        x.wrapping_mul(2).wrapping_add(1)
    }
    fn dbl_run(f: DblFn, x: &Val) -> u64 {
        f(x.0)
    }
    fn dbl_ref(x: &Val) -> u64 {
        x.0.wrapping_mul(2)
    }

    const DBL_VECTORS: &[Val] = &[
        Val(0),
        Val(1),
        Val(2),
        Val(255),
        Val(u64::MAX / 2),
        Val(u64::MAX),
    ];

    fn dbl_family(
        selection: Selection,
        candidates: &'static [Candidate<DblFn>],
    ) -> Family<'static, DblFn, Val, u64> {
        Family {
            id: FamilyId("double"),
            selection,
            candidates,
            baseline: Candidate {
                name: "portable",
                requires: &[],
                impl_: dbl as DblFn,
            },
            reference: dbl_ref,
            run: dbl_run,
            vectors: DBL_VECTORS,
        }
    }

    const CRC_ACCEL: Candidate<DblFn> = Candidate {
        name: "crc-accel",
        requires: &[CpuFeature::Crc32],
        impl_: dbl as DblFn,
    };
    const WIDE: Candidate<DblFn> = Candidate {
        name: "wide",
        requires: &[],
        impl_: dbl as DblFn,
    };
    const BUGGY: Candidate<DblFn> = Candidate {
        name: "buggy",
        requires: &[],
        impl_: dbl_off_by_one as DblFn,
    };

    const CORE: CoreKey = CoreKey(0xA72);

    #[test]
    fn filters_on_missing_feature_and_keeps_the_legal_survivor() {
        // Declared order puts the CRC candidate first, the featureless one
        // second; the buggy one never survives verify.
        static CANDS: &[Candidate<DblFn>] = &[CRC_ACCEL, WIDE, BUGGY];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let sel = Selector::new();

        // No CRC32 bit: the accelerated candidate is gated out, `wide` wins.
        let got = sel.select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "wide");
        assert_eq!(got.decision.reason, DecisionReason::Priority);
        assert_eq!(got.decision.family, FamilyId("double"));
        assert_eq!(got.decision.core, CORE);
        assert_eq!(dbl_run(got.impl_, &Val(21)), 42);

        // CRC32 present: the highest-priority legal survivor wins.
        let with_crc = CpuFeatureSet::new().with(CpuFeature::Crc32);
        let got = sel.select(&family, with_crc, CORE, None);
        assert_eq!(got.decision.chosen, "crc-accel");
    }

    #[test]
    fn a_buggy_candidate_is_rejected_by_self_verify() {
        // The only candidate is buggy: it is filtered by verify, so the
        // baseline wins even though the candidate is feature-legal.
        static CANDS: &[Candidate<DblFn>] = &[BUGGY];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let got = Selector::new().select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "portable");
        assert_eq!(got.decision.reason, DecisionReason::Baseline);
    }

    #[test]
    fn falls_closed_to_baseline_when_all_survivors_gated_out() {
        static CANDS: &[Candidate<DblFn>] = &[CRC_ACCEL];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let got = Selector::new().select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "portable");
        assert_eq!(got.decision.reason, DecisionReason::Baseline);
    }

    #[test]
    fn a_family_with_no_vectors_cannot_verify_and_uses_baseline() {
        static CANDS: &[Candidate<DblFn>] = &[WIDE];
        let mut family = dbl_family(Selection::ByPriority, CANDS);
        family.vectors = &[];
        let got = Selector::new().select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "portable");
        // Baseline is itself unverifiable with no vectors — honestly flagged.
        assert_eq!(got.decision.reason, DecisionReason::BaselineUnverified);
    }

    #[test]
    fn a_pin_selects_a_named_legal_candidate() {
        static CANDS: &[Candidate<DblFn>] = &[CRC_ACCEL, WIDE];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let mut sel = Selector::new();
        sel.pin(FamilyId("double"), "wide");
        assert_eq!(sel.pinned(FamilyId("double")), Some("wide"));

        // Even with CRC32 present (where `crc-accel` would normally win),
        // the pin forces `wide`.
        let with_crc = CpuFeatureSet::new().with(CpuFeature::Crc32);
        let got = sel.select(&family, with_crc, CORE, None);
        assert_eq!(got.decision.chosen, "wide");
        assert_eq!(got.decision.reason, DecisionReason::Pinned);
    }

    #[test]
    fn a_pinned_illegal_candidate_falls_to_baseline() {
        static CANDS: &[Candidate<DblFn>] = &[CRC_ACCEL, WIDE];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let mut sel = Selector::new();
        sel.pin(FamilyId("double"), "crc-accel");
        // No CRC32 bit: the pinned candidate is feature-illegal, so the pin is
        // rejected and the baseline is used — pinning cannot reach an absent
        // instruction.
        let got = sel.select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "portable");
        assert_eq!(got.decision.reason, DecisionReason::PinRejected);
    }

    #[test]
    fn a_pinned_buggy_candidate_falls_to_baseline() {
        static CANDS: &[Candidate<DblFn>] = &[BUGGY, WIDE];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let mut sel = Selector::new();
        sel.pin(FamilyId("double"), "buggy");
        let got = sel.select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "portable");
        assert_eq!(got.decision.reason, DecisionReason::PinRejected);
    }

    #[test]
    fn a_pin_naming_no_candidate_is_ignored() {
        static CANDS: &[Candidate<DblFn>] = &[WIDE];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let mut sel = Selector::new();
        sel.pin(FamilyId("double"), "does-not-exist");
        let got = sel.select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "wide");
        assert_eq!(got.decision.reason, DecisionReason::Priority);
    }

    #[test]
    fn pinning_the_same_family_twice_replaces() {
        let mut sel = Selector::new();
        sel.pin(FamilyId("double"), "a");
        sel.pin(FamilyId("double"), "b");
        assert_eq!(sel.pinned(FamilyId("double")), Some("b"));
    }

    // ---- The ByBenchmark axis, over a deterministic fake counter. -------
    //
    // The op charges a shared cycle counter by a per-candidate cost, so a
    // cheaper candidate genuinely measures fewer cycles. Both candidates
    // compute the same (correct) answer, so both verify.
    struct FakeCycles {
        now: Cell<u64>,
    }
    impl CycleCounter for FakeCycles {
        fn cycles(&self) -> u64 {
            self.now.get()
        }
        fn cycles_monotonic_hint(&self) -> bool {
            true
        }
    }

    struct BenchIn<'a> {
        ctr: &'a FakeCycles,
        payload: u64,
    }
    fn bench_run(cost: u64, input: &BenchIn<'_>) -> u64 {
        input.ctr.now.set(input.ctr.now.get().wrapping_add(cost));
        input.payload.wrapping_mul(2)
    }
    fn bench_ref(input: &BenchIn<'_>) -> u64 {
        input.payload.wrapping_mul(2)
    }

    #[test]
    fn by_benchmark_picks_the_fastest_verified_candidate() {
        let ctr = FakeCycles { now: Cell::new(0) };
        let vectors = [BenchIn {
            ctr: &ctr,
            payload: 21,
        }];
        // `slow` (cost 10) declared first, `fast` (cost 3) second: benchmark
        // must override declared priority and pick `fast`.
        let candidates = [
            Candidate {
                name: "slow",
                requires: &[],
                impl_: 10u64,
            },
            Candidate {
                name: "fast",
                requires: &[],
                impl_: 3u64,
            },
        ];
        let family: Family<'_, u64, BenchIn<'_>, u64> = Family {
            id: FamilyId("bench"),
            selection: Selection::ByBenchmark,
            candidates: &candidates,
            baseline: Candidate {
                name: "portable",
                requires: &[],
                impl_: 100u64,
            },
            reference: bench_ref,
            run: bench_run,
            vectors: &vectors,
        };
        let harness = BenchHarness::with_budget(&ctr, 4, 5);
        let got = Selector::new().select(&family, CpuFeatureSet::EMPTY, CORE, Some(&harness));
        assert_eq!(got.decision.chosen, "fast");
        assert_eq!(got.decision.reason, DecisionReason::Benchmark);

        // Without a harness a ByBenchmark family falls to declared priority.
        let got = Selector::new().select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "slow");
        assert_eq!(got.decision.reason, DecisionReason::Priority);
    }

    #[test]
    fn by_benchmark_with_one_survivor_needs_no_harness() {
        let ctr = FakeCycles { now: Cell::new(0) };
        let vectors = [BenchIn {
            ctr: &ctr,
            payload: 1,
        }];
        let candidates = [Candidate {
            name: "only",
            requires: &[],
            impl_: 5u64,
        }];
        let family: Family<'_, u64, BenchIn<'_>, u64> = Family {
            id: FamilyId("bench-one"),
            selection: Selection::ByBenchmark,
            candidates: &candidates,
            baseline: Candidate {
                name: "portable",
                requires: &[],
                impl_: 9u64,
            },
            reference: bench_ref,
            run: bench_run,
            vectors: &vectors,
        };
        let got = Selector::new().select(&family, CpuFeatureSet::EMPTY, CORE, None);
        assert_eq!(got.decision.chosen, "only");
        assert_eq!(got.decision.reason, DecisionReason::Priority);
    }

    // ---- OpsTables: per-core-type resolution and growth. ----------------
    #[cfg(feature = "alloc")]
    #[test]
    fn ops_tables_resolve_once_per_core_type_and_grow() {
        let mut tables: OpsTables<&'static str> = OpsTables::new();
        assert!(tables.is_empty());

        let built = Cell::new(0u32);
        let a = *tables.resolve(CoreKey(1), || {
            built.set(built.get() + 1);
            "big"
        });
        assert_eq!(a, "big");
        // Second resolve of the same core type does not rebuild.
        let a2 = *tables.resolve(CoreKey(1), || {
            built.set(built.get() + 1);
            "SHOULD-NOT-RUN"
        });
        assert_eq!(a2, "big");
        assert_eq!(built.get(), 1);

        // A different core type grows the table on demand.
        let b = *tables.resolve(CoreKey(2), || "little");
        assert_eq!(b, "little");
        assert_eq!(tables.len(), 2);
        assert_eq!(tables.get(CoreKey(1)), Some(&"big"));
        assert_eq!(tables.get(CoreKey(9)), None);
    }

    // ---- DecisionSink: the injected log seam. ---------------------------
    struct RecordingSink {
        last: Cell<Option<Decision>>,
    }
    impl DecisionSink for RecordingSink {
        fn record(&self, decision: &Decision) {
            self.last.set(Some(*decision));
        }
    }

    #[test]
    fn decision_sink_records_the_typed_decision() {
        static CANDS: &[Candidate<DblFn>] = &[WIDE];
        let family = dbl_family(Selection::ByPriority, CANDS);
        let sink = RecordingSink {
            last: Cell::new(None),
        };
        let got = Selector::new().select(&family, CpuFeatureSet::EMPTY, CORE, None);
        sink.record(&got.decision);
        let recorded = sink.last.get().expect("a decision was recorded");
        assert_eq!(recorded.chosen, "wide");
        assert_eq!(recorded.features, CpuFeatureSet::EMPTY);
    }
}
