//! TAIRiX page/region zeroing — the one first-party definition.
//!
//! Clearing memory to zero is one of the kernel's hottest and most
//! security-critical primitives: every freshly-allocated frame is zeroed
//! before it becomes user-visible (so no stale bytes leak across a process
//! boundary), and every frame that ever held a secret is scrubbed on free
//! (the zero-on-free guarantee). On a page-sized, page-aligned region the
//! portable byte fill leaves a great deal of the hardware on the table:
//! modern ISAs carry a dedicated block-zero primitive that clears a whole
//! cache line without a read-for-ownership.
//!
//! # Two axes, and why this is capability-gated (never benchmarked)
//!
//! The generic `lib/cpuops` framework separates *capability* ("does this core
//! have the instruction?") from *performance* ("which equally-correct routine
//! is fastest?"). Page-zero sits firmly on the capability axis: aarch64
//! `DC ZVA` and x86_64 ERMS `rep stosb` are *unconditionally* faster than a
//! scalar byte loop when present and bit-identical in result, so the choice is
//! a pure feature decision — declared priority (hardware first, portable
//! baseline last), never a microbenchmark. Racing a page-zero benchmark at
//! boot would be pointless churn (and Linux likewise selects `DC ZVA`/ERMS by
//! feature, not by timing). The framework's `ByBenchmark` axis is reserved for
//! ops where the fastest implementation genuinely varies by microarchitecture.
//!
//! # One resolved routine, kernel-wide
//!
//! A kernel page-zero routine may run on any core the scheduler migrates work
//! to, so it must be legal on *every* such core. It is therefore resolved
//! **once**, at boot, against the migration-safe *common* feature set (the
//! intersection over all cores, computed by `kernel/core`), into a single
//! set-once function pointer — no per-call, per-CPU table lookup on this hot
//! path. Because the set is an intersection, any bit it advertises is present
//! on every core, so a dispatched `DC ZVA`/`rep stosb` can never trap after a
//! migration. (Per-core-type keying, which `lib/cpuops` also offers, is for a
//! future consumer that is pinned to a core type; a migratable kernel routine
//! correctly uses the common set.)
//!
//! # Correct by construction
//!
//! Selection goes through `lib/cpuops`, so the hardware candidate is:
//!
//! 1. **capability-gated** — chosen only when its feature bit is set;
//! 2. **self-verified** — before it can ever be selected, its output is
//!    checked byte-for-byte against the portable baseline over a fixed vector
//!    of sizes and alignments (empty, sub-block, exact-block, block-crossing,
//!    a full page), including that it zeroes *exactly* the requested region
//!    and touches nothing past it;
//! 3. **fail-closed** — if the bit is absent or verification fails, the
//!    portable baseline is used. [`zero`] before [`resolve`] runs (or after a
//!    failed resolve) is the portable baseline too — never a trap, never a
//!    panic.
//!
//! [`zero`] is the hot-path entry consumers call; it reads the resolved
//! implementation from a set-once cell (no code patching — a plain function
//! pointer, W^X-clean).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

// `CpuFeature` names the feature bits the per-arch hardware candidates gate on;
// on a target with no hardware candidate there are none, so the import is
// legitimately unused there (`CpuFeatureSet` is always used).
#[cfg_attr(not(any(pagezero_x86_64, pagezero_aarch64)), allow(unused_imports))]
use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
use tairix_cpuops::{Candidate, CoreKey, Decision, Family, FamilyId, Selection, Selector};
use tairix_sync::OnceCell;

#[cfg(pagezero_aarch64)]
pub mod aarch64;
#[cfg(pagezero_x86_64)]
pub mod x86_64;

/// The implementation-handle type: zero every byte of a mutable region.
///
/// A `fn(&mut [u8])` is the natural shape of the op and what the kernel
/// consumer holds; it is `Copy + Send + Sync`, so the set-once resolved-routine
/// cell needs no `unsafe`.
pub type PageZeroFn = fn(&mut [u8]);

/// The family's stable id — the `lib/cpuops` log/pin key.
pub const FAMILY_ID: FamilyId = FamilyId("pagezero");

/// Name of the portable baseline candidate.
pub const BASELINE_NAME: &str = "pagezero-portable";

/// Name of the hardware candidate on this build. Stable log/pin key for the
/// accelerated path.
#[cfg(pagezero_x86_64)]
pub const HW_NAME: &str = "pagezero-erms";
/// Name of the hardware candidate on this build.
#[cfg(pagezero_aarch64)]
pub const HW_NAME: &str = "pagezero-dc-zva";

/// Zero every byte of `buf` with the portable byte fill.
///
/// This is the baseline every hardware candidate is verified against, the
/// implementation used on any target without a block-zero instruction, and the
/// fail-closed fallback [`zero`] uses before [`resolve`] runs. Correct on every
/// architecture; `[u8]::fill` lowers to the compiler's best portable memset.
pub fn zero_portable(buf: &mut [u8]) {
    buf.fill(0);
}

/// The set-once resolved implementation for the whole kernel image.
///
/// Holds the winning function pointer once [`resolve`] runs. Before it is set
/// (or if resolution ever failed and left it unset) [`zero`] falls closed to
/// the portable baseline.
static RESOLVED: OnceCell<PageZeroFn> = OnceCell::new();

/// The accelerated candidates available on this build, in descending declared
/// priority. A target with no block-zero instruction has none and the baseline
/// is the only implementation.
#[cfg(pagezero_x86_64)]
const CANDIDATES: &[Candidate<PageZeroFn>] = &[Candidate {
    name: HW_NAME,
    requires: &[CpuFeature::Erms],
    impl_: x86_64::zero_erms,
}];
#[cfg(pagezero_aarch64)]
const CANDIDATES: &[Candidate<PageZeroFn>] = &[Candidate {
    name: HW_NAME,
    requires: &[CpuFeature::DcZva],
    impl_: aarch64::zero_dc_zva,
}];
#[cfg(not(any(pagezero_x86_64, pagezero_aarch64)))]
const CANDIDATES: &[Candidate<PageZeroFn>] = &[];

/// The scratch-buffer capacity the self-verify runs over — a full page, so the
/// vectors exercise the head/aligned-middle/tail structure of a block-zero
/// routine on the same size the kernel actually clears.
///
/// A validation bound on a fixed self-verify set, not a machine-scaling
/// capacity, and small enough to live on the boot stack.
const SCRATCH_CAP: usize = 4096;

/// One self-verify case: a length to zero within the shared scratch buffer and
/// the non-zero pattern to pre-fill it with.
///
/// The candidate and the reference each pre-fill the *whole* scratch with the
/// pattern, zero the first `len` bytes, and are compared over the whole
/// capacity — so a candidate that under-zeroes (leaves part of `[0, len)`
/// non-zero) or over-zeroes (clears past `len`) is rejected.
#[derive(Clone, Copy)]
struct ZeroCase<'a> {
    scratch: &'a core::cell::RefCell<[u8; SCRATCH_CAP]>,
    len: usize,
    seed: u8,
}

/// Fill `buf` with a position-dependent, always-non-zero pattern, so a byte
/// that should be zeroed but is not is always detectable (a zero in the
/// pattern could hide an under-zero).
fn prefill(buf: &mut [u8], seed: u8) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = seed.wrapping_add(i.to_le_bytes()[0]) | 1;
    }
}

/// An FNV-1a fingerprint of `buf` — cheap, order-sensitive, and detects any
/// single-byte difference, so comparing a candidate's post-state fingerprint to
/// the reference's is equivalent to comparing the buffers without copying them.
fn fingerprint(buf: &[u8]) -> u64 {
    let mut acc = 0xcbf2_9ce4_8422_2325u64;
    for &byte in buf {
        acc = (acc ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    acc
}

/// Run a candidate over one self-verify case and fingerprint the result (the
/// `lib/cpuops` `run` adapter).
fn run(impl_: PageZeroFn, case: &ZeroCase<'_>) -> u64 {
    let mut buf = case.scratch.borrow_mut();
    prefill(&mut buf[..], case.seed);
    impl_(&mut buf[..case.len]);
    fingerprint(&buf[..])
}

/// The portable reference over one self-verify case (the `lib/cpuops`
/// `reference` adapter). Uses the same shared scratch, run at a different time
/// than [`run`], so the two `borrow_mut`s never overlap.
fn reference(case: &ZeroCase<'_>) -> u64 {
    let mut buf = case.scratch.borrow_mut();
    prefill(&mut buf[..], case.seed);
    zero_portable(&mut buf[..case.len]);
    fingerprint(&buf[..])
}

/// The fixed self-verify lengths: empty, sub-word, word boundaries, a typical
/// 64-byte cache line and either side of it, larger spans, and a full page —
/// the sizes and alignments a block-zero routine's head/middle/tail split must
/// all get right.
const VERIFY_LENS: &[usize] = &[0, 1, 7, 8, 63, 64, 65, 127, 128, 256, 1024, 4095, 4096];

/// Resolve the page-zero implementation for this image from the delivered
/// `features`, installing the winner for [`zero`] and returning the typed
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
    // One shared scratch buffer on the stack; every case pre-fills and zeroes
    // it in turn (the selector runs `run` then `reference` sequentially per
    // case, so the borrows never overlap). A single 4 KiB buffer keeps the
    // boot-stack cost to one page and the crate allocation-free.
    let scratch = core::cell::RefCell::new([0u8; SCRATCH_CAP]);
    let mut cases = [ZeroCase {
        scratch: &scratch,
        len: 0,
        seed: 0,
    }; VERIFY_LENS.len()];
    for (case, (i, &len)) in cases.iter_mut().zip(VERIFY_LENS.iter().enumerate()) {
        case.len = len;
        // A distinct pattern per case so a routine cannot pass by luck of a
        // repeated pattern across cases.
        case.seed = 0x11u8.wrapping_add(i.to_le_bytes()[0].wrapping_mul(0x1d));
    }

    let family = Family {
        id: FAMILY_ID,
        // Hardware block-zero is unconditionally faster and bit-identical, so
        // the choice is a pure capability decision: priority order (hardware
        // first, baseline last) with no benchmark.
        selection: Selection::ByPriority,
        candidates: CANDIDATES,
        baseline: Candidate {
            name: BASELINE_NAME,
            requires: &[],
            impl_: zero_portable as PageZeroFn,
        },
        reference,
        run,
        vectors: &cases,
    };

    let mut selector = Selector::new();
    if let Some(name) = pin {
        selector.pin(FAMILY_ID, name);
    }
    // Keyed by the feature bits for the log; a kernel-wide routine is resolved
    // against the common set, so one key is correct.
    let selected = selector.select(&family, features, CoreKey(features.bits()), None);
    // Set-once: the first resolve wins the installed implementation; a later
    // call's `AlreadySet` is expected and ignored (the decision is still
    // returned for logging).
    let _ = RESOLVED.set(selected.impl_);
    selected.decision
}

/// Zero every byte of `buf` using the resolved implementation.
///
/// The hot-path entry every consumer calls. Reads the set-once resolved
/// function pointer; before [`resolve`] runs, or if resolution failed, it is
/// the portable baseline (fail closed — never a trap, never a panic).
pub fn zero(buf: &mut [u8]) {
    match RESOLVED.get() {
        Ok(Some(implementation)) => implementation(buf),
        _ => zero_portable(buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_cpuops::DecisionReason;

    #[test]
    fn portable_zeroes_exactly_the_slice() {
        let mut buf = [0xAAu8; 16];
        zero_portable(&mut buf[4..12]);
        assert_eq!(&buf[..4], &[0xAA; 4], "head untouched");
        assert_eq!(&buf[4..12], &[0; 8], "middle zeroed");
        assert_eq!(&buf[12..], &[0xAA; 4], "tail untouched");
        // Empty slice is a no-op.
        let mut empty: [u8; 0] = [];
        zero_portable(&mut empty[..]);
    }

    /// Every hardware candidate compiled on this build must reproduce the
    /// portable reference on every self-verify case — the same check the
    /// selector enforces, asserted directly so a broken candidate fails the
    /// crate's own tests, not only selection.
    #[test]
    fn candidates_match_the_reference_on_every_case() {
        let scratch = core::cell::RefCell::new([0u8; SCRATCH_CAP]);
        for (i, &len) in VERIFY_LENS.iter().enumerate() {
            let case = ZeroCase {
                scratch: &scratch,
                len,
                seed: 0x33u8.wrapping_add(i.to_le_bytes()[0]),
            };
            for candidate in CANDIDATES {
                assert_eq!(
                    run(candidate.impl_, &case),
                    reference(&case),
                    "candidate {} disagrees with the reference at len {len}",
                    candidate.name
                );
            }
        }
    }

    /// With no feature bits the baseline is selected and `zero` clears a buffer.
    #[test]
    fn resolve_falls_closed_to_baseline_without_features() {
        let decision = resolve(CpuFeatureSet::EMPTY);
        assert_eq!(decision.chosen, BASELINE_NAME);
        assert_eq!(decision.reason, DecisionReason::Baseline);
        let mut buf = [0x5Au8; 200];
        zero(&mut buf);
        assert!(buf.iter().all(|&b| b == 0));
    }

    /// On a build with a hardware candidate, presenting its feature bit selects
    /// it (priority over the baseline) and it still zeroes correctly. On a
    /// build without one, the baseline stands. Either way the result is correct
    /// — the point of the self-verify gate.
    #[test]
    fn hardware_candidate_selected_when_its_feature_is_present() {
        let decision = resolve(hw_feature_set());
        let mut buf = [0xFFu8; 300];
        zero(&mut buf);
        assert!(buf.iter().all(|&b| b == 0), "zeroed correctly either way");
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
        #[cfg(pagezero_x86_64)]
        {
            CpuFeatureSet::new().with(CpuFeature::Erms)
        }
        #[cfg(pagezero_aarch64)]
        {
            CpuFeatureSet::new().with(CpuFeature::DcZva)
        }
        #[cfg(not(any(pagezero_x86_64, pagezero_aarch64)))]
        {
            CpuFeatureSet::EMPTY
        }
    }
}
