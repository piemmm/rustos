//! Per-generator statistical soak entry points, plus the negative controls
//! that keep the battery from being vacuous.
//!
//! A plain `cargo test` runs one fixed-seed smoke pass per generator; the
//! soak (`cargo xtask rngsoak`) exports the budget and byte-count seams to
//! keep drawing until the budget elapses, reading them through the same
//! `tairix-fuzzseed` names the orchestrator writes.

use std::env;

use tairix_fuzzseed::{budget_deadline, RNGSOAK_BUDGET_ENV, RNGSOAK_BYTES_ENV};
use tairix_test_rng_soak::{run_control, run_target, Verdict, CONTROLS, SMOKE_BYTES, TARGETS};

/// Bytes per generator per pass; the orchestrator overrides it.
fn bytes() -> u64 {
    env::var(RNGSOAK_BYTES_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SMOKE_BYTES)
}

fn run(name: &str) {
    if let Err(e) = run_target(name, bytes(), budget_deadline(RNGSOAK_BUDGET_ENV)) {
        panic!("rngsoak {name} failed: {e}");
    }
}

#[test]
fn soak_fast() {
    run("fast");
}

#[test]
fn soak_csprng() {
    run("csprng");
}

/// The assertion that makes the whole battery mean something: **every**
/// statistic in it must reject at least one known-bad generator.
///
/// A statistical test that rejects nothing is not a weak test, it is not a
/// test — it would pass a counter as readily as a cipher. This pins the
/// coverage per statistic rather than in aggregate, so a test that silently
/// stopped discriminating (a wrong constant, a tail collapsed to 1) fails
/// here instead of quietly certifying whatever it is handed.
#[test]
fn every_statistic_rejects_a_known_bad_generator() {
    let mut covered: Vec<&'static str> = Vec::new();
    for control in CONTROLS {
        let verdicts = run_control(control).unwrap_or_else(|e| panic!("control {control}: {e}"));
        // An inconclusive verdict is not a rejection. Crediting one would
        // make this assertion hold for every statistic on a control run that
        // was simply too short, which is the vacuity it exists to prevent.
        assert!(
            verdicts.iter().all(|(_, v)| *v != Verdict::TooFewSequences),
            "control {control} ran too few sequences to conclude anything"
        );
        covered.extend(
            verdicts
                .iter()
                .filter(|(_, v)| v.is_rejection())
                .map(|(name, _)| *name),
        );
    }
    let uncovered: Vec<&str> = tairix_test_rng_soak::statistics::ALL
        .iter()
        .map(|s| s.name)
        .filter(|name| !covered.contains(name))
        .collect();
    assert!(
        uncovered.is_empty(),
        "no negative control is rejected by: {}",
        uncovered.join(", ")
    );
}

/// The controls must be rejected for the *right* reason, so a coincidence
/// cannot stand in for detection power.
///
/// Linear dependence over GF(2) is what the matrix-rank test exists to find,
/// and the LFSR is the only control that exhibits it while passing the
/// bias-and-correlation tests. If the rank test ever stopped catching it, the
/// aggregate assertion above would still pass on the counter alone.
#[test]
fn the_matrix_rank_test_catches_the_lfsr_specifically() {
    let verdicts = run_control("lfsr").expect("the LFSR control runs");
    let rank = verdicts
        .iter()
        .find(|(name, _)| *name == "matrix-rank")
        .map(|(_, v)| *v)
        .expect("the rank test is in the battery");
    assert!(
        rank.is_rejection(),
        "the rank test did not reject an LFSR; it said {rank:?}"
    );
    // And the LFSR must still *pass* the bias-and-correlation tests, or it
    // would be an ordinary bad generator rather than the specific
    // linear-but-statistically-excellent control this asserts about.
    for name in ["frequency", "cusum-forward"] {
        let v = verdicts
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
            .expect("the statistic is in the battery");
        assert_eq!(v, Verdict::Accepted, "{name} rejected the LFSR: {v:?}");
    }
}

#[test]
fn registry_lists_every_soak_target() {
    assert_eq!(TARGETS, &["fast", "csprng"]);
    assert_eq!(CONTROLS, &["lfsr", "counter"]);
}
