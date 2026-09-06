//! Statistical soak for the kernel random subsystem.
//!
//! Drives `tairix_rng`'s two unpredictable generators — the buffered `ChaCha12`
//! `FastRng` and the HMAC-SHA256 `CsRng` — through a first-party NIST
//! SP 800-22-style battery, and holds every test in that battery against a
//! known-bad generator it must reject.
//!
//! The battery is what a *statistical* test can honestly claim: it cannot
//! distinguish a good PRNG from true randomness, so it never certifies one.
//! What it does is reject the structure a broken generator leaves behind —
//! a bias, a short-range correlation, linear dependence over GF(2),
//! compressibility. The load-bearing proofs of the construction itself (the
//! key-erasure split, backtracking resistance, zeroise-on-consume) are unit
//! tests where the generators live; this crate is the outer check that the
//! bytes actually coming out look like nothing at all.
//!
//! Statistical tests are ordinary numerical algorithms rather than
//! cryptographic primitives, so implementing them here is legitimate; they
//! live in host test code because their arithmetic is floating point, which
//! `lib/rng`'s `no_std` body has no business carrying.
//!
//! A plain `cargo test` runs one fixed-seed pass, so the per-PR gate is
//! deterministic and can never be flaky. `cargo xtask rngsoak` exports a
//! wall-clock budget and the harness keeps drawing fresh sequences until it
//! elapses, accumulating into one verdict — the band narrows as the count
//! grows, so a longer soak is strictly more sensitive rather than merely
//! longer.
//!
//! Test scaffolding only: it lives under `tests/` and is never part of a
//! TAIRiX build.

pub mod battery;
pub mod bits;
pub mod generators;
pub mod special;
pub mod statistics;

use std::time::Instant;

use tairix_fuzzseed::RNGSOAK_SEED_ENV;

pub use battery::{Accumulator, Verdict, MINIMUM_SEQUENCES};
pub use generators::{build, Stream, CONTROLS, TARGETS};
pub use statistics::{SEQUENCE_BITS, SEQUENCE_BYTES};

/// Seed a run uses when [`RNGSOAK_SEED_ENV`] is unset and there is no budget.
///
/// Fixed rather than drawn from host entropy, unlike the other soaks: a
/// statistical verdict is a probabilistic one, so a fresh-seed smoke run
/// would carry the battery's whole false-alarm probability into every PR
/// gate. With this the gate either passes forever or fails forever.
pub const SMOKE_SEED: u64 = 0x5241_4e44_4f4d_0001;

/// Bytes per generator one smoke pass tests: 256 sequences.
///
/// Comfortably above the two-level rule's own 64-sequence minimum, and
/// enough that a gross regression is rejected by many standard deviations,
/// while keeping the per-PR pass to a couple of seconds. Sensitivity to a
/// *marginal* defect is the soak's job, and it gets it by accumulating: the
/// band narrows as the sequence count grows.
pub const SMOKE_BYTES: u64 = 256 * SEQUENCE_BYTES as u64;

/// Draw `bytes` worth of sequences from `generator` into `into`.
///
/// # Errors
/// Propagates a generator failure.
fn accumulate(
    generator: &mut dyn Stream,
    bytes: u64,
    into: &mut Accumulator,
) -> Result<(), String> {
    let mut sequence = vec![0u8; SEQUENCE_BYTES];
    let sequences = bytes / SEQUENCE_BYTES as u64;
    for _ in 0..sequences {
        generator.fill(&mut sequence)?;
        into.record(&sequence);
    }
    Ok(())
}

/// Run the battery over the named generator until `deadline`, then decide.
///
/// `None` — a plain `cargo test` — runs exactly one pass of `bytes`. A
/// deadline keeps drawing fresh sequences from the same continuing stream and
/// reaches the verdict once, over everything accumulated.
///
/// # Errors
/// Returns a descriptive error naming the seed when the generator is unknown,
/// cannot produce bytes, or is rejected by any statistic.
pub fn run_target(name: &str, bytes: u64, deadline: Option<Instant>) -> Result<(), String> {
    if !TARGETS.contains(&name) {
        return Err(format!(
            "rngsoak: unknown target `{name}`; known: {}",
            TARGETS.join(", ")
        ));
    }
    let seed = resolve_seed(name, deadline.is_some());
    let mut generator = build(name, seed)?;
    let mut accumulator = Accumulator::new();

    loop {
        let began = Instant::now();
        accumulate(generator.as_mut(), bytes, &mut accumulator)?;
        // A pass draws and tests a whole batch, so starting one that cannot
        // finish would overrun the budget by a full pass every run.
        let room = matches!(deadline, Some(end) if end.saturating_duration_since(Instant::now()) > began.elapsed());
        if !room {
            break;
        }
    }

    let rejected = accumulator.rejected();
    println!(
        "[rngsoak] {name}: {} sequences of {SEQUENCE_BITS} bits\n{}",
        accumulator.sequences(),
        accumulator.report()
    );
    if rejected.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "rngsoak {name} (seed {seed}) rejected by {}:\n{}",
            rejected.join(", "),
            accumulator.report()
        ))
    }
}

/// Sequences a control run draws.
///
/// Exactly the two-level rule's own minimum. A control fails by hundreds of
/// standard deviations, so there is nothing marginal for a longer run to
/// resolve — but it must not be *shorter*, because below the minimum every
/// verdict would be `TooFewSequences` and a coverage assertion over them
/// would hold for reasons that have nothing to do with the battery.
const CONTROL_SEQUENCES: u64 = MINIMUM_SEQUENCES;

/// Run the battery over a known-bad generator and report each statistic's
/// verdict.
///
/// Returns verdicts rather than a list of names so a caller can insist on a
/// *statistical* rejection and not merely a non-acceptance.
///
/// # Errors
/// Propagates a build or generation failure.
pub fn run_control(name: &str) -> Result<Vec<(&'static str, Verdict)>, String> {
    if !CONTROLS.contains(&name) {
        return Err(format!(
            "rngsoak: `{name}` is not a control; known: {}",
            CONTROLS.join(", ")
        ));
    }
    let mut generator = build(name, SMOKE_SEED)?;
    let mut accumulator = Accumulator::new();
    accumulate(
        generator.as_mut(),
        CONTROL_SEQUENCES * SEQUENCE_BYTES as u64,
        &mut accumulator,
    )?;
    println!(
        "[rngsoak] control {name}: {} sequences\n{}",
        accumulator.sequences(),
        accumulator.report()
    );
    Ok(accumulator.verdicts())
}

/// The seed for a run: the environment override, else fresh entropy for a
/// budgeted soak, else [`SMOKE_SEED`].
///
/// Announced either way, so even a fresh-seed soak failure replays exactly.
fn resolve_seed(name: &str, budgeted: bool) -> u64 {
    let seed = std::env::var(RNGSOAK_SEED_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            if budgeted {
                tairix_fuzzseed::entropy_seed()
            } else {
                SMOKE_SEED
            }
        });
    tairix_fuzzseed::announce(name, RNGSOAK_SEED_ENV, seed);
    seed
}

#[cfg(test)]
mod tests {
    use super::{run_control, run_target, CONTROLS, SEQUENCE_BYTES, SMOKE_SEED, TARGETS};

    #[test]
    fn an_unknown_target_fails_closed() {
        let err = run_target("mt19937", 0, None).expect_err("unknown targets must not run");
        for known in TARGETS {
            assert!(err.contains(known), "the error should list {known}: {err}");
        }
    }

    #[test]
    fn a_target_is_not_accepted_as_a_control_or_the_reverse() {
        for target in TARGETS {
            assert!(run_control(target).is_err(), "{target} is not a control");
        }
        for control in CONTROLS {
            assert!(
                run_target(control, 0, None).is_err(),
                "{control} is not a target"
            );
        }
    }

    /// A budget too small for the two-level rule's minimum must fail rather
    /// than report an empty verdict as a pass.
    #[test]
    fn a_budget_below_the_decision_minimum_is_rejected() {
        let err = run_target("fast", 8 * SEQUENCE_BYTES as u64, None)
            .expect_err("too few sequences cannot yield a verdict");
        assert!(err.contains("fast"), "{err}");
    }

    #[test]
    fn the_smoke_seed_is_fixed_so_the_gate_cannot_be_flaky() {
        assert_eq!(SMOKE_SEED, 0x5241_4e44_4f4d_0001);
    }
}
