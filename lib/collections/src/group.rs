//! The control-group scan: one metadata byte per slot, examined a group at a
//! time.
//!
//! The hash table stores one control byte beside each slot — [`EMPTY`],
//! [`DELETED`], or the key's seven-bit tag with the top bit clear — and probes
//! by loading [`GROUP_LEN`] of them at once and asking three questions of the
//! whole group: which lanes carry this tag, which are empty (the probe chain
//! ends there), and which are free to take an insertion. [`GroupMatch`]
//! answers all three from one load, so a probe costs one scan per group
//! whether it is looking up or inserting.
//!
//! Two candidates answer it in a handful of vector instructions where the
//! silicon has them; [`scan_portable`] answers it with word-at-a-time bit
//! tricks everywhere else. Which one runs is decided once, by [`resolve`],
//! through the `lib/cpuops` capability gate and its mandatory self-verify — so
//! a vector instruction is never reached on a core that lacks it, and a
//! candidate that disagrees with the portable reference on any vector is
//! structurally unpickable. A build with no candidate at all (every target
//! whose vector unit is off) calls the baseline directly, paying neither the
//! resolved-cell load nor the indirect call.

use tairix_cpuops::{Candidate, CoreKey, Decision, Family, FamilyId, Selection, Selector};
use tairix_sync::OnceCell;

#[cfg(any(swiss_neon, swiss_sse2))]
use tairix_abi::cpufeatures::CpuFeature;
use tairix_abi::cpufeatures::CpuFeatureSet;

#[cfg(swiss_neon)]
mod aarch64;
#[cfg(swiss_sse2)]
mod x86_64;

/// Control bytes examined per scan.
///
/// Sixteen is the width of one SSE2 / NEON vector register, and gives the
/// portable baseline two 64-bit words per scan. It is a fixed structural
/// constant of the layout, not a capacity.
pub const GROUP_LEN: usize = 16;

/// One group of control bytes.
pub type Group = [u8; GROUP_LEN];

/// Control byte for a slot that has never been used. A probe stops here:
/// nothing beyond it on this chain can hold the key.
pub const EMPTY: u8 = 0b1111_1111;

/// Control byte for a slot whose entry was removed while its group had no
/// empty lane. It may take a new entry, but does not end a probe chain.
pub const DELETED: u8 = 0b1000_0000;

/// Mask selecting the seven-bit tag a full slot's control byte carries.
pub const TAG_MASK: u8 = 0b0111_1111;

/// `true` if `ctrl` marks a live entry (top bit clear).
#[must_use]
pub const fn is_full(ctrl: u8) -> bool {
    ctrl & 0b1000_0000 == 0
}

/// What one scan of a control group found.
///
/// Each field is a bitmask over the group's lanes, lane *n* in bit *n*.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct GroupMatch {
    /// Full lanes whose tag equals the probe tag — the candidates whose keys
    /// the caller must compare.
    pub tag: u16,
    /// Lanes that are [`EMPTY`]. A non-zero value ends the probe chain.
    pub empty: u16,
    /// Lanes that are [`EMPTY`] or [`DELETED`] — where an insertion may land.
    pub free: u16,
}

/// A candidate's implementation handle.
pub type GroupScanFn = fn(&Group, u8) -> GroupMatch;

/// The family's stable id — the `lib/cpuops` log and pin key.
pub const FAMILY_ID: FamilyId = FamilyId("hash-group-scan");

/// Name of the portable baseline candidate.
pub const BASELINE_NAME: &str = "group-scan-portable";

/// Name of the vector candidate on this build.
#[cfg(swiss_sse2)]
pub const VECTOR_NAME: &str = "group-scan-sse2";
/// Name of the vector candidate on this build.
#[cfg(swiss_neon)]
pub const VECTOR_NAME: &str = "group-scan-neon";

/// Every byte's high bit.
const HIGH: u64 = 0x8080_8080_8080_8080;
/// Every byte's low seven bits.
const SEVEN: u64 = 0x7f7f_7f7f_7f7f_7f7f;
/// Every byte's low bit.
const LOW: u64 = 0x0101_0101_0101_0101;
/// Gathers the eight high bits of a word into its top byte when multiplied
/// in: bit `8n+7` lands at bit `56+n`.
const GATHER: u64 = 0x0002_0408_1020_4081;

/// Pack a word whose selected byte lanes have their high bit set into an
/// eight-lane bitmask.
#[allow(clippy::cast_possible_truncation)] // the shift leaves exactly eight bits
const fn movemask(word: u64) -> u16 {
    ((word & HIGH).wrapping_mul(GATHER) >> 56) as u16
}

/// Lanes of `word` whose byte equals `byte`, as high bits.
///
/// Adding `0x7f` to a byte's low seven bits carries into bit 7 unless those
/// bits are zero, and can never carry *out* of the byte — so unlike the
/// shorter `(x - 1) & !x` zero-byte test this one cannot let one lane's borrow
/// forge a match in the next. Exactness is required, not merely tidy: the
/// vector candidates compare exactly, and a baseline that reported a spurious
/// lane would fail the self-verify against them.
const fn eq_bytes(word: u64, byte: u8) -> u64 {
    let x = word ^ (LOW.wrapping_mul(byte as u64));
    let nonzero = ((x & SEVEN).wrapping_add(SEVEN) | x) & HIGH;
    !nonzero & HIGH
}

/// Scan one word of control bytes.
///
/// The empty test compares the whole byte rather than the two top bits it
/// would be enough to look at given the control encoding, so the scan is a
/// total function of its input: the vector candidates compare exactly, and a
/// baseline that agreed with them only for well-formed control bytes would
/// make the self-verify's bit-identity conditional.
const fn scan_word(word: u64, tag: u8) -> (u16, u16, u16) {
    (
        movemask(eq_bytes(word, tag)),
        movemask(eq_bytes(word, EMPTY)),
        movemask(word),
    )
}

/// The portable, always-correct group scan.
///
/// This is the baseline every vector candidate is verified against, the
/// implementation on any target whose vector unit is off, and what runs
/// before [`resolve`].
#[must_use]
pub fn scan_portable(group: &Group, tag: u8) -> GroupMatch {
    let (lo, hi) = group.split_at(GROUP_LEN / 2);
    // `split_at` at a constant half of a fixed-size array yields two halves of
    // exactly eight bytes; the fallible conversion is discharged here so the
    // scan needs no unchecked read.
    let (Ok(lo), Ok(hi)) = (<[u8; 8]>::try_from(lo), <[u8; 8]>::try_from(hi)) else {
        return GroupMatch::default();
    };
    // Little-endian so lane `n` of the group is bit `n` of the mask on every
    // port, matching what the vector candidates produce.
    let (tag_lo, empty_lo, free_lo) = scan_word(u64::from_le_bytes(lo), tag);
    let (tag_hi, empty_hi, free_hi) = scan_word(u64::from_le_bytes(hi), tag);
    GroupMatch {
        tag: tag_lo | (tag_hi << 8),
        empty: empty_lo | (empty_hi << 8),
        free: free_lo | (free_hi << 8),
    }
}

/// The resolved scan, once [`resolve`] has run on a build that has a
/// candidate to choose.
static RESOLVED: OnceCell<GroupScanFn> = OnceCell::new();

/// The accelerated candidates available on this build, in descending declared
/// priority. A target whose vector unit is off has none.
#[cfg(swiss_sse2)]
const CANDIDATES: &[Candidate<GroupScanFn>] = &[Candidate {
    name: VECTOR_NAME,
    requires: &[CpuFeature::Sse2],
    impl_: x86_64::scan_sse2,
}];
#[cfg(swiss_neon)]
const CANDIDATES: &[Candidate<GroupScanFn>] = &[Candidate {
    name: VECTOR_NAME,
    requires: &[CpuFeature::Asimd],
    impl_: aarch64::scan_neon,
}];
#[cfg(not(any(swiss_sse2, swiss_neon)))]
const CANDIDATES: &[Candidate<GroupScanFn>] = &[];

/// One self-verify input.
#[derive(Copy, Clone, Debug)]
pub struct ScanVector {
    /// The control group to scan.
    pub group: Group,
    /// The tag to look for.
    pub tag: u8,
}

/// The fixed self-verify vectors: an untouched group, a saturated one, tags at
/// both ends of the seven-bit range, a match in the first and last lane only,
/// and mixtures of live, deleted, and empty lanes so every mask is exercised
/// at every lane position. A candidate that disagrees with the portable
/// reference on any of these is rejected.
const VECTORS: &[ScanVector] = &{
    let mut vectors = [ScanVector {
        group: [EMPTY; GROUP_LEN],
        tag: 0,
    }; 8];
    vectors[1] = ScanVector {
        group: [DELETED; GROUP_LEN],
        tag: 0x7f,
    };
    vectors[2] = ScanVector {
        group: [0x2a; GROUP_LEN],
        tag: 0x2a,
    };
    vectors[3] = ScanVector {
        group: [0x2a; GROUP_LEN],
        tag: 0x2b,
    };
    let mut first = [EMPTY; GROUP_LEN];
    first[0] = 0x00;
    vectors[4] = ScanVector {
        group: first,
        tag: 0x00,
    };
    let mut last = [DELETED; GROUP_LEN];
    last[GROUP_LEN - 1] = 0x7f;
    vectors[5] = ScanVector {
        group: last,
        tag: 0x7f,
    };
    let mut mixed = [0u8; GROUP_LEN];
    let mut i = 0;
    while i < GROUP_LEN {
        mixed[i] = match i % 4 {
            0 => EMPTY,
            1 => DELETED,
            2 => 0x11,
            _ => 0x7f,
        };
        i += 1;
    }
    vectors[6] = ScanVector {
        group: mixed,
        tag: 0x11,
    };
    vectors[7] = ScanVector {
        group: mixed,
        tag: 0x7f,
    };
    vectors
};

/// Invoke a candidate over one vector (the `lib/cpuops` `run` adapter).
fn run(impl_: GroupScanFn, input: &ScanVector) -> GroupMatch {
    impl_(&input.group, input.tag)
}

/// The portable reference (the `lib/cpuops` `reference` adapter).
fn reference(input: &ScanVector) -> GroupMatch {
    scan_portable(&input.group, input.tag)
}

/// The `lib/cpuops` family describing the group scan.
fn family() -> Family<'static, GroupScanFn, ScanVector, GroupMatch> {
    Family {
        id: FAMILY_ID,
        // A vector scan answers all three questions in a handful of
        // instructions where the word-at-a-time baseline needs tens of them,
        // and the two are bit-identical, so the choice is a pure capability
        // decision. It is never benchmarked: the input is a control group
        // whose tags derive from the per-boot hash key, and a benchmark is a
        // timing measurement over exactly that.
        selection: Selection::ByPriority,
        candidates: CANDIDATES,
        baseline: Candidate {
            name: BASELINE_NAME,
            requires: &[],
            impl_: scan_portable,
        },
        reference,
        run,
        vectors: VECTORS,
    }
}

/// Resolve the group scan for this image from the delivered `features`,
/// installing the winner for every table in the process and returning the
/// typed [`Decision`] for the caller to record on the audit log.
///
/// Idempotent: the winner is installed once; a later call re-selects and
/// returns a fresh [`Decision`] without disturbing the installed
/// implementation. Never panics and falls closed to the portable baseline, so
/// a table that hashes before this runs is correct — only slower.
#[must_use = "record the Decision through the audit log, or bind it to `_`"]
pub fn resolve(features: CpuFeatureSet) -> Decision {
    let family = family();
    let selected = Selector::new().select(&family, features, CoreKey(features.bits()), None);
    let _ = RESOLVED.set(selected.impl_);
    selected.decision
}

/// Scan one control group — the hot-path entry the table probes through.
#[inline]
#[must_use]
pub fn scan(group: &Group, tag: u8) -> GroupMatch {
    // On a build with no candidate the resolved cell could only ever hold the
    // baseline, so this folds to a direct, inlinable call and the table pays
    // neither the cell load nor the indirect call.
    if CANDIDATES.is_empty() {
        return scan_portable(group, tag);
    }
    match RESOLVED.get() {
        Ok(Some(resolved)) => resolved(group, tag),
        _ => scan_portable(group, tag),
    }
}

#[cfg(test)]
mod tests;
