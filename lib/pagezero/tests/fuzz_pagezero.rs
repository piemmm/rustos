//! Deterministic fuzz harness for the page-zero implementations.
//!
//! Zeroing memory is a security property: the kernel relies on `zero` to erase
//! every byte of a freshly-allocated or freed frame. A hardware candidate that
//! under-zeroed (left a byte set) would leak a stale byte across a process
//! boundary or defeat the zero-on-free scrub; one that over-zeroed (wrote past
//! the region) would corrupt neighbouring memory. The charter requires every
//! accelerated routine be fuzzed against its portable reference.
//!
//! Over arbitrary lengths and start alignments the harness asserts two
//! invariants of the *selected* implementation
//! ([`tairix_pagezero::zero`], resolved to the hardware candidate when this
//! build has one and the feature bit is presented):
//!
//! * it leaves the target sub-slice byte-for-byte identical to the portable
//!   reference ([`tairix_pagezero::zero_portable`]) applied to an identical
//!   pre-fill — i.e. the target is all zero;
//! * it touches nothing outside the target sub-slice (the surrounding bytes
//!   keep their pre-fill).
//!
//! On an x86_64 or aarch64 host this fuzzes the real `rep stosb` / `DC ZVA`
//! path against the byte-fill reference; on a target with no hardware candidate
//! both sides are the portable routine and the harness confirms it is stable.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG draws lengths,
//! alignments, and fill patterns. A plain `cargo test` runs the fixed
//! [`SMOKE_ITERATIONS`] sweep; `cargo xtask fuzz` extends the loop to a
//! wall-clock budget.

use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
use tairix_pagezero::{resolve, zero, zero_portable};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

/// The scratch buffer size — larger than a page so the draw exercises the
/// aligned-interior `DC ZVA` / ERMS path plus unaligned head and tail, and
/// arbitrary start offsets within it.
const BUF_LEN: usize = 5000;

/// The maximal feature set: presents *both* the x86_64 ERMS and aarch64
/// `DC ZVA` bits, so whichever hardware candidate this build compiled in is
/// selected. A build with none falls closed to the portable baseline (still
/// correct).
fn all_pagezero_features() -> CpuFeatureSet {
    CpuFeatureSet::new()
        .with(CpuFeature::Erms)
        .with(CpuFeature::DcZva)
}

fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

#[test]
fn selected_impl_zeroes_exactly_the_region_on_any_input() {
    // Resolve once to the hardware candidate (when present); `zero` then
    // dispatches through the selected implementation for the rest of the run.
    let _ = resolve(all_pagezero_features());

    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "selected_impl_zeroes_exactly_the_region_on_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    // Two identically pre-filled buffers: the reference byte-fill is applied to
    // one, the selected implementation to the other; they must end equal.
    let mut got = [0u8; BUF_LEN];
    let mut expected = [0u8; BUF_LEN];
    let mut iteration: u64 = 0;
    loop {
        // A random start offset and length within the buffer.
        let offset = bounded(next(), BUF_LEN);
        let len = bounded(next(), BUF_LEN - offset);
        // Fill both buffers with the same non-zero-ish random pattern.
        for i in 0..BUF_LEN {
            let byte = next().to_le_bytes()[0];
            got[i] = byte;
            expected[i] = byte;
        }

        zero(&mut got[offset..offset + len]);
        zero_portable(&mut expected[offset..offset + len]);

        assert_eq!(
            got, expected,
            "selected page-zero diverged from the reference: offset {offset}, len {len}"
        );
        // Explicitly: the target region is all zero, and the bytes outside it
        // are untouched (the reference guarantees this, and equality above
        // transfers it to the selected implementation).
        assert!(
            got[offset..offset + len].iter().all(|&b| b == 0),
            "target region not fully zeroed: offset {offset}, len {len}"
        );

        iteration += 1;
        match deadline {
            Some(deadline) => {
                if !tairix_fuzzseed::within_budget(Some(deadline)) {
                    break;
                }
            }
            None => {
                if iteration >= SMOKE_ITERATIONS {
                    break;
                }
            }
        }
    }
}
