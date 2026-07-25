//! TAIRiX CRC-32C (Castagnoli) checksum — the one first-party definition.
//!
//! CRC-32C is the fast, **non-cryptographic** block-integrity checksum TAIRiX
//! uses to catch media / transport corruption (bit rot, torn writes,
//! misdirected reads) cheaply, before the expensive cryptographic checks run.
//! It is *not* a security primitive: authenticity rests on the AEAD tag and
//! the cryptographic content hash, so a first-party implementation is
//! permitted (the charter's "never hand-roll crypto" bar does not apply to an
//! error-detecting checksum).
//!
//! CRC-32C (the Castagnoli polynomial, reflected `0x1EDC_6F41` →
//! `0x82F6_3B78`) is chosen over an ad-hoc hash because it has *guaranteed*
//! Hamming-distance error-detection properties a general-purpose hash lacks,
//! and because both Tier-1 native ISAs with a CRC instruction compute exactly
//! it in one general-purpose-register instruction: x86_64 SSE4.2 `crc32` and
//! the `ARMv8` `crc32c*` family. It is the same checksum ext4 metadata, btrfs,
//! `iSCSI`, and SCTP standardised on.
//!
//! # Two implementations, one selected at runtime
//!
//! The crate carries a portable table-driven [`crc32c_portable`] baseline that
//! is always correct on every target, plus per-architecture hardware
//! candidates (`x86_64`/`aarch64` modules, compiled behind a build-script-emitted
//! `crc32c_<arch>` cfg — the charter-legal `lib/abi-trap` precedent, so no
//! target-architecture conditional-compilation predicate appears in the source
//! the `cfg-check` guards). Which one
//! a process uses is decided **once**, by [`resolve`], from the
//! [`CpuFeatureSet`] the kernel delivers it (never by probing the instruction
//! blindly, which would trap). Selection goes through the generic `lib/cpuops`
//! framework, so the hardware candidate is:
//!
//! 1. **capability-gated** — chosen only when the required feature bit is set;
//! 2. **self-verified** — its output is checked bit-for-bit against the
//!    portable reference over a fixed vector of edge-case inputs before it can
//!    ever be selected, so a decode bug is structurally unpickable;
//! 3. **fail-closed** — if the bit is absent or verification fails, the
//!    portable baseline is used. [`checksum`] before [`resolve`] runs (or after
//!    a failed resolve) is the portable baseline too — never a trap, never a
//!    panic.
//!
//! [`checksum`] is the hot-path entry consumers call; it reads the resolved
//! implementation from a set-once cell (no code patching — a plain function
//! pointer, W^X-clean).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

// `CpuFeature` names the feature bits the per-arch hardware candidates gate on;
// on a target with no CRC instruction there are no candidates, so the import is
// legitimately unused there (`CpuFeatureSet` is always used).
#[cfg_attr(not(any(crc32c_x86_64, crc32c_aarch64)), allow(unused_imports))]
use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
use tairix_cpuops::{Candidate, CoreKey, Decision, Family, FamilyId, Selection, Selector};
use tairix_sync::OnceCell;

#[cfg(crc32c_aarch64)]
pub mod aarch64;
#[cfg(crc32c_x86_64)]
pub mod x86_64;

/// The implementation-handle type: a checksum over a byte slice.
pub type Crc32cFn = fn(&[u8]) -> u32;

/// The family's stable id — the `lib/cpuops` log/pin key.
pub const FAMILY_ID: FamilyId = FamilyId("crc32c");

/// Name of the portable baseline candidate.
pub const BASELINE_NAME: &str = "crc32c-portable";

/// Name of the hardware candidate on this build (empty-set on a target with no
/// CRC instruction). Stable log/pin key for the accelerated path.
#[cfg(crc32c_x86_64)]
pub const HW_NAME: &str = "crc32c-sse4.2";
/// Name of the hardware candidate on this build.
#[cfg(crc32c_aarch64)]
pub const HW_NAME: &str = "crc32c-armv8";

/// The Castagnoli CRC-32C generator polynomial in reflected form.
const CRC32C_REFLECTED_POLY: u32 = 0x82F6_3B78;

/// The 256-entry lookup table for the reflected, table-driven baseline,
/// generated at compile time from [`CRC32C_REFLECTED_POLY`] so no table is
/// hand-transcribed (a transcription error would be caught by the known-answer
/// test, but generating it removes the possibility).
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0u32;
    while n < 256 {
        let mut crc = n;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_REFLECTED_POLY
            } else {
                crc >> 1
            };
            k += 1;
        }
        table[n as usize] = crc;
        n += 1;
    }
    table
}

/// The portable, always-correct CRC-32C of `data` (reflected, table-driven,
/// init `0xFFFF_FFFF`, final XOR `0xFFFF_FFFF`).
///
/// This is the baseline every hardware candidate is verified against and the
/// implementation used on any target without a CRC instruction, and before
/// [`resolve`] runs. Correct on every architecture and endianness.
#[must_use]
pub fn crc32c_portable(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[index];
    }
    !crc
}

/// The set-once resolved implementation for the whole process/kernel image.
///
/// Holds the winning function pointer once [`resolve`] runs. A `Crc32cFn` is
/// `Copy + Send + Sync`, so the cell needs no `unsafe`; before it is set (or if
/// resolution ever failed and left it unset) [`checksum`] falls closed to the
/// portable baseline.
static RESOLVED: OnceCell<Crc32cFn> = OnceCell::new();

/// The accelerated candidates available on this build, in descending declared
/// priority. Assembled per architecture; a target with no CRC instruction has
/// none and the baseline is the only implementation.
#[cfg(crc32c_x86_64)]
const CANDIDATES: &[Candidate<Crc32cFn>] = &[Candidate {
    name: HW_NAME,
    requires: &[CpuFeature::Sse42],
    impl_: x86_64::crc32c_sse42,
}];
#[cfg(crc32c_aarch64)]
const CANDIDATES: &[Candidate<Crc32cFn>] = &[Candidate {
    name: HW_NAME,
    requires: &[CpuFeature::Crc32],
    impl_: aarch64::crc32c_hw,
}];
#[cfg(not(any(crc32c_x86_64, crc32c_aarch64)))]
const CANDIDATES: &[Candidate<Crc32cFn>] = &[];

/// A self-verify input: one byte buffer the candidate and reference are both
/// run over.
type Vector = &'static [u8];

/// The fixed self-verify vectors: empty, sub-word, exact 8-byte-word boundary
/// crossings, a byte tail, larger buffers, and the canonical CRC-32C
/// known-answer input (`b"123456789"` → `0xE306_9283`). A candidate that
/// disagrees with the portable reference on any of these is rejected.
const VECTORS: &[Vector] = &[
    b"",
    b"a",
    b"1234567",
    b"12345678",
    b"123456789",
    b"the quick brown fox jumps over the lazy dog",
    &[0u8; 64],
    &[0xFFu8; 65],
    &[0x5Au8; 127],
];

/// Invoke a candidate over one vector (the `lib/cpuops` `run` adapter).
fn run(impl_: Crc32cFn, input: &Vector) -> u32 {
    impl_(input)
}

/// The portable reference (the `lib/cpuops` `reference` adapter).
fn reference(input: &Vector) -> u32 {
    crc32c_portable(input)
}

/// Build the `lib/cpuops` family describing the CRC-32C op: its candidates, its
/// mandatory portable baseline, and the reference plus vectors the self-verify
/// runs.
fn family() -> Family<'static, Crc32cFn, Vector, u32> {
    Family {
        id: FAMILY_ID,
        // CRC-32C hardware is unconditionally faster than the table baseline
        // and bit-identical, so the choice is a pure capability decision:
        // priority order (hardware first, baseline last) with no benchmark.
        selection: Selection::ByPriority,
        candidates: CANDIDATES,
        baseline: Candidate {
            name: BASELINE_NAME,
            requires: &[],
            impl_: crc32c_portable,
        },
        reference,
        run,
        vectors: VECTORS,
    }
}

/// Resolve the CRC-32C implementation for this image from the delivered
/// `features`, installing the winner for [`checksum`] and returning the typed
/// [`Decision`] (for the caller to record through the audit log).
///
/// Idempotent: the winner is installed once; a later call re-selects and
/// returns a fresh [`Decision`] but does not disturb the installed
/// implementation. Never panics; falls closed to the portable baseline.
#[must_use = "record the Decision through the audit log, or bind it to `_`"]
pub fn resolve(features: CpuFeatureSet) -> Decision {
    select_and_install(features, None)
}

/// Like [`resolve`], but honour an operator pin naming a specific candidate
/// (a boot parameter, for determinism / reproducible-build validation).
///
/// A pinned candidate **still self-verifies**: a pin cannot select an absent,
/// feature-illegal, or buggy implementation — it falls closed to the baseline.
#[must_use = "record the Decision through the audit log, or bind it to `_`"]
pub fn resolve_pinned(features: CpuFeatureSet, pin: &'static str) -> Decision {
    select_and_install(features, Some(pin))
}

fn select_and_install(features: CpuFeatureSet, pin: Option<&'static str>) -> Decision {
    let family = family();
    let mut selector = Selector::new();
    if let Some(name) = pin {
        selector.pin(FAMILY_ID, name);
    }
    let selected = selector.select(&family, features, CoreKey(features.bits()), None);
    // Set-once: the first resolve wins the installed implementation; a later
    // call's `AlreadySet` is expected and ignored (the decision is still
    // returned for logging).
    let _ = RESOLVED.set(selected.impl_);
    selected.decision
}

/// The CRC-32C of `data` using the resolved implementation.
///
/// The hot-path entry every consumer calls. Reads the set-once resolved
/// function pointer; before [`resolve`] runs, or if resolution failed, it is
/// the portable baseline (fail closed — never a trap, never a panic).
#[must_use]
pub fn checksum(data: &[u8]) -> u32 {
    match RESOLVED.get() {
        Ok(Some(implementation)) => implementation(data),
        _ => crc32c_portable(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_cpuops::DecisionReason;

    /// The canonical CRC-32C check value pins the polynomial, reflection, and
    /// XOR constants: any transcription slip in the generated table or the
    /// folding order changes this.
    #[test]
    fn portable_matches_the_known_answer_vector() {
        assert_eq!(crc32c_portable(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_portable(b""), 0x0000_0000);
    }

    /// Every hardware candidate compiled on this build must reproduce the
    /// portable reference on every self-verify vector — the same check the
    /// selector enforces, asserted directly so a broken candidate fails the
    /// crate's own tests, not only selection.
    #[test]
    fn candidates_match_the_reference_on_every_vector() {
        let family = family();
        for candidate in family.candidates {
            for vector in family.vectors {
                assert_eq!(
                    (family.run)(candidate.impl_, vector),
                    (family.reference)(vector),
                    "candidate {} disagrees with the reference",
                    candidate.name
                );
            }
        }
    }

    /// With no feature bits the baseline is selected and `checksum` returns the
    /// portable answer.
    #[test]
    fn resolve_falls_closed_to_baseline_without_features() {
        let decision = resolve(CpuFeatureSet::EMPTY);
        assert_eq!(decision.chosen, BASELINE_NAME);
        assert_eq!(decision.reason, DecisionReason::Baseline);
        assert_eq!(checksum(b"123456789"), 0xE306_9283);
    }

    /// On a build with a hardware candidate, presenting its feature bit selects
    /// it (priority over the baseline) and it still produces the correct
    /// answer. On a build without one, the baseline stands. Either way the
    /// result is correct — the point of the self-verify gate.
    #[test]
    fn hardware_candidate_selected_when_its_feature_is_present() {
        let features = hw_feature_set();
        let decision = resolve(features);
        assert_eq!(checksum(b"123456789"), 0xE306_9283);
        if CANDIDATES.is_empty() {
            assert_eq!(decision.chosen, BASELINE_NAME);
        } else {
            assert_eq!(decision.chosen, HW_NAME);
            assert_eq!(decision.reason, DecisionReason::Priority);
        }
    }

    /// The feature set that admits this build's hardware candidate (empty on a
    /// target without one).
    fn hw_feature_set() -> CpuFeatureSet {
        #[cfg(crc32c_x86_64)]
        {
            CpuFeatureSet::new().with(CpuFeature::Sse42)
        }
        #[cfg(crc32c_aarch64)]
        {
            CpuFeatureSet::new().with(CpuFeature::Crc32)
        }
        #[cfg(not(any(crc32c_x86_64, crc32c_aarch64)))]
        {
            CpuFeatureSet::EMPTY
        }
    }
}
