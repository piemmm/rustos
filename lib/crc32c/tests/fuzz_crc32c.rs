//! Deterministic fuzz harness for the CRC-32C implementations.
//!
//! CRC-32C guards data read off untrusted media, so the *correctness* of the
//! hardware-accelerated path is a safety property: a decode bug in the
//! SSE4.2 / `crc32c*` folding that disagreed with the reference would let bit
//! rot slip past the fast check (or reject good data). The charter requires
//! every accelerated routine be fuzzed against its portable reference.
//!
//! The single invariant, over arbitrary byte buffers of arbitrary length and
//! alignment:
//!
//! * the *selected* implementation ([`tairix_crc32c::checksum`], resolved to
//!   the hardware candidate when this build has one and the feature bit is
//!   presented) produces **exactly** the portable reference
//!   ([`tairix_crc32c::crc32c_portable`]). A divergence — or a panic / OOB —
//!   is the failure; the run aborting *is* the failure.
//!
//! On a build with a hardware candidate (an x86_64 or aarch64 host, or the
//! native targets) this fuzzes the real `crc32` / `crc32c*` folding against the
//! table reference. On a target with no CRC instruction both sides are the
//! portable routine and the harness still confirms it is stable.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` draws
//! buffers. A plain `cargo test` runs the fixed [`SMOKE_ITERATIONS`] sweep;
//! `cargo xtask fuzz` extends the loop to a wall-clock budget.

use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
use tairix_crc32c::{checksum, crc32c_portable, resolve};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

/// Largest buffer the harness draws (a few 8-byte-word chunks plus a tail,
/// enough to exercise the word loop and every tail length).
const MAX_LEN: usize = 300;

/// The maximal feature set: presents *both* the x86_64 and aarch64 CRC bits,
/// so whichever hardware candidate this build compiled in is selected. A build
/// with none falls closed to the portable baseline (still correct).
fn all_crc_features() -> CpuFeatureSet {
    CpuFeatureSet::new()
        .with(CpuFeature::Sse42)
        .with(CpuFeature::Crc32)
}

#[test]
fn selected_impl_matches_the_reference_on_any_input() {
    // Resolve once to the hardware candidate (when present); `checksum` then
    // dispatches through the selected implementation for the rest of the run.
    let _ = resolve(all_crc_features());

    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "selected_impl_matches_the_reference_on_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut buf = [0u8; MAX_LEN];
    let mut iteration: u64 = 0;
    loop {
        let len = rng.at_most(MAX_LEN);
        for byte in buf.iter_mut().take(len) {
            *byte = rng.next_u8();
        }
        let input = &buf[..len];

        // The selected (possibly hardware) implementation must agree with the
        // portable reference bit-for-bit.
        assert_eq!(
            checksum(input),
            crc32c_portable(input),
            "selected CRC-32C diverged from the reference on {len} bytes"
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
