//! `cargo xtask ci-long` — the long-runner flake hunt.
//!
//! `ci` runs every test exactly once, so a rare, timing-dependent flake can
//! slip through a green pull-request run. This command is the deliberate
//! counterpart for a dedicated, long-lived runner: it runs the *same* checks
//! `ci` does, but every **test-executing** stage is run [`REPS`] times
//! sequentially and then [`REPS`] times concurrently before the next test, so
//! a flake that only appears under repetition or contention is forced into the
//! open. The orchestration lives in [`super::run_ci_long`]; this module owns
//! the repeat-and-parallelise engine and the set of tests it drives.
//!
//! # What is repeated, and what is not
//!
//! Only stages that actually *execute test cases* are flake-hunted: the host
//! `cargo test` matrix, the release-profile crypto constant-time tests, the
//! QEMU integration tests, the fuzz harnesses, and the proptest models. The
//! deterministic gates `ci` also runs — `fmt`, `clippy`, `deps-check`,
//! `cfg-check`, `docs-check`, `cargo deny`, `supply-chain`, `model-check`
//! (exhaustive by construction), `spec-review`, `abi-check`, `c-header`, and
//! the image gate — are pass/fail checks whose result cannot change between
//! runs, so repeating them 2·[`REPS`] times would burn a long-runner's time
//! for no signal. They run once, exactly as in `ci`.
//!
//! # Sequential then concurrent, per test
//!
//! For each test the hunt first runs [`REPS`] passes strictly one at a time
//! (the timing profile of an isolated run), then submits [`REPS`] replicas to
//! the shared concurrency runner ([`super::parallel`]) at once (the timing
//! profile under contention). The two profiles catch different flakes: a
//! use-once race surfaces sequentially; a shared-resource or scheduling race
//! surfaces concurrently.
//!
//! The concurrency **budget** differs by test kind, deliberately:
//!
//! * host / crypto / fuzz / proptest replicas are deadline-free host
//!   processes — a slow run merely finishes later, it does not fail — so all
//!   [`REPS`] replicas run at once (budget = `reps`).
//! * QEMU replicas are weighted by their emulated-CPU count against a budget
//!   of the host's logical CPUs, exactly as [`super::qemu_tests::run_once`]
//!   does. A QEMU guest has a hard wall-clock deadline, and oversubscribing
//!   the host's cores with more concurrent vCPUs than it has would starve the
//!   guests of TCG time and turn the run into a *load-dependent* timeout — the
//!   very flakiness the charter forbids manufacturing. So the [`REPS`] QEMU
//!   replicas still all get submitted; they simply overlap as far as the host
//!   can honestly sustain rather than all at once.

use crate::commands::parallel::{self, Job};
use crate::commands::{fuzz, proptest, qemu_tests};
use crate::Context;

/// How many times each test is run in each phase.
///
/// A test is run this many times sequentially and then this many times
/// concurrently, so the hunt executes each test `2 * REPS` times in total.
pub(crate) const REPS: u32 = 20;

/// One test the flake hunt repeats.
///
/// A unit pairs a human label with a **factory** that builds a fresh [`Job`]
/// each call (a `Job` is single-use), plus the concurrency budget the runner
/// admits its replicas under during the concurrent phase.
pub(crate) struct FlakeUnit<'a> {
    /// Human-readable name shown in the phase banners.
    label: String,
    /// Maximum in-flight job weight admitted while running this unit's
    /// replicas concurrently (see the module docs for why it differs by kind).
    concurrency_budget: usize,
    /// Builds a fresh, single-use job for the given repetition index. The
    /// index only varies a fuzz/proptest PRNG seed; it is ignored otherwise.
    factory: Box<dyn Fn(usize) -> Job + 'a>,
}

impl<'a> FlakeUnit<'a> {
    fn new(
        label: impl Into<String>,
        concurrency_budget: usize,
        factory: impl Fn(usize) -> Job + 'a,
    ) -> Self {
        Self {
            label: label.into(),
            concurrency_budget,
            factory: Box::new(factory),
        }
    }
}

/// Run every unit `reps` times sequentially, then `reps` times concurrently,
/// before moving on to the next unit. Fails closed: the first sequential pass
/// or concurrent batch that fails aborts the hunt and returns the failure
/// (there is no retry — a failure is a flake found, which is the point).
pub(crate) fn flake_hunt(units: Vec<FlakeUnit<'_>>, reps: u32) -> Result<(), String> {
    let reps = reps.max(1);
    for unit in units {
        eprintln!("xtask: [ci-long] {} — {reps}x sequential", unit.label);
        for pass in 1..=reps {
            let job = (unit.factory)((pass - 1) as usize);
            // Budget one: the single job streams its output live and runs
            // strictly alone, giving the isolated-run timing profile.
            parallel::run(vec![job], 1).map_err(|e| {
                format!(
                    "ci-long: {} failed on sequential pass {pass}/{reps}: {e}",
                    unit.label
                )
            })?;
        }
        eprintln!("xtask: [ci-long] {} — {reps}x concurrent", unit.label);
        let jobs: Vec<Job> = (0..reps).map(|i| (unit.factory)(i as usize)).collect();
        parallel::run(jobs, unit.concurrency_budget).map_err(|e| {
            format!(
                "ci-long: {} failed running {reps}x concurrently: {e}",
                unit.label
            )
        })?;
    }
    Ok(())
}

/// The ordered set of tests the flake hunt drives: the host test matrix, the
/// QEMU integration tests, the fuzz harnesses, and the proptest models. The
/// QEMU binaries must already be built ([`super::qemu_tests::build_all`]).
pub(crate) fn all_units(ctx: &Context, reps: u32) -> Vec<FlakeUnit<'_>> {
    let mut units = host_units(ctx, reps);
    units.extend(qemu_units(ctx));
    units.extend(fuzz_units(ctx, reps));
    units.extend(proptest_units(ctx, reps));
    units
}

/// The host `cargo test` matrix and the release-profile crypto constant-time
/// tests — two distinct build profiles, so two units. Deadline-free, so every
/// replica runs at once.
fn host_units(ctx: &Context, reps: u32) -> Vec<FlakeUnit<'_>> {
    let budget = reps as usize;
    vec![
        FlakeUnit::new("host cargo test", budget, move |_| {
            let mut cmd = ctx.cargo();
            cmd.args(["test", "--workspace", "--all-targets", "--locked"]);
            Job::new("host cargo test", cmd)
        }),
        // The constant-time comparison guarantee can be broken by the
        // optimiser, so this runs the secret-handling tests at `-C
        // opt-level=3`, mirroring `ci`'s crypto-constant-time gate.
        FlakeUnit::new("crypto constant-time (release)", budget, move |_| {
            let mut cmd = ctx.cargo();
            cmd.args(["test", "--release", "--locked", "-p", "rustos-crypto"]);
            Job::new("crypto constant-time (release)", cmd)
        }),
    ]
}

/// One unit per QEMU integration test, weighted by emulated CPUs against a
/// host-CPU budget so concurrent replicas never oversubscribe the host and
/// manufacture a load-dependent timeout (see the module docs).
fn qemu_units(ctx: &Context) -> Vec<FlakeUnit<'_>> {
    let budget = parallel::host_parallelism();
    qemu_tests::enrolments()
        .into_iter()
        .map(|enrol| {
            let target_dir = ctx.target_dir();
            let label = format!("qemu {}", enrol.package);
            let job_label = label.clone();
            let weight = usize::try_from(enrol.cpus).unwrap_or(1);
            FlakeUnit::new(label, budget, move |_| {
                let target_dir = target_dir.clone();
                let job_label = job_label.clone();
                Job::closure(job_label, weight, move || enrol.run(&target_dir))
            })
        })
        .collect()
}

/// One unit per fuzz harness, each running its single smoke iteration (as
/// `fuzz --once` does) with a fresh seed per repetition. Deadline-free, so
/// every replica runs at once.
fn fuzz_units(ctx: &Context, reps: u32) -> Vec<FlakeUnit<'_>> {
    let budget = reps as usize;
    fuzz::TARGETS
        .iter()
        .map(|target| {
            FlakeUnit::new(format!("fuzz {}", target.test), budget, move |index| {
                fuzz::job_for(ctx, target, None, None, index)
            })
        })
        .collect()
}

/// One unit per proptest model, each running its single smoke iteration (as
/// `proptest --once` does) with a fresh seed per repetition. Deadline-free, so
/// every replica runs at once.
fn proptest_units(ctx: &Context, reps: u32) -> Vec<FlakeUnit<'_>> {
    let budget = reps as usize;
    proptest::MODELS
        .iter()
        .map(|model| {
            FlakeUnit::new(format!("proptest {}", model.name), budget, move |index| {
                proptest::job_for(ctx, model, None, None, index)
            })
        })
        .collect()
}

/// Print the planned hunt without running anything (`ci-long --dry-run`): the
/// test set and the per-test repetition counts, so an operator can see the
/// shape and cost of a run before committing a long-runner to it.
pub(crate) fn print_plan(ctx: &Context) {
    let units = all_units(ctx, REPS);
    eprintln!(
        "ci-long plan: {} test(s), each run {REPS}x sequential then {REPS}x concurrent \
         (deterministic gates run once):",
        units.len()
    );
    for unit in &units {
        eprintln!("  - {}", unit.label);
    }
}

#[cfg(test)]
mod tests {
    use super::{all_units, flake_hunt, FlakeUnit, REPS};
    use crate::commands::parallel::Job;
    use crate::Context;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn test_ctx() -> Context {
        Context {
            workspace_root: PathBuf::from("/nonexistent-workspace"),
            cargo: OsString::from("cargo"),
        }
    }

    /// A counting unit whose factory records how many jobs it built and, via
    /// each job's own closure, how many actually ran.
    fn counting_unit(
        built: &AtomicUsize,
        ran: Arc<AtomicUsize>,
        fail_on_run: Option<usize>,
    ) -> FlakeUnit<'_> {
        FlakeUnit::new("counter", 8, move |_| {
            built.fetch_add(1, Ordering::SeqCst);
            let ran = Arc::clone(&ran);
            Job::closure("counter", 1, move || {
                let n = ran.fetch_add(1, Ordering::SeqCst);
                match fail_on_run {
                    Some(fail_at) if n == fail_at => Err("boom".to_string()),
                    _ => Ok(()),
                }
            })
        })
    }

    #[test]
    fn reps_is_the_documented_twenty() {
        assert_eq!(REPS, 20);
    }

    #[test]
    fn flake_hunt_runs_each_unit_reps_times_sequentially_then_concurrently() {
        let built = AtomicUsize::new(0);
        let ran = Arc::new(AtomicUsize::new(0));
        let unit = counting_unit(&built, Arc::clone(&ran), None);
        flake_hunt(vec![unit], 3).expect("all passes succeed");
        // 3 sequential + 3 concurrent = 6 jobs built and run.
        assert_eq!(built.load(Ordering::SeqCst), 6);
        assert_eq!(ran.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn flake_hunt_fails_closed_and_stops_on_a_sequential_failure() {
        let built = AtomicUsize::new(0);
        let ran = Arc::new(AtomicUsize::new(0));
        // Fail the second sequential run (0-based index 1).
        let unit = counting_unit(&built, Arc::clone(&ran), Some(1));
        let err = flake_hunt(vec![unit], 4).expect_err("a failing pass must fail the hunt");
        assert!(
            err.contains("sequential pass 2/4"),
            "unexpected error: {err}"
        );
        // Stopped after the failing pass: the third/fourth sequential passes
        // and the whole concurrent phase never ran.
        assert_eq!(ran.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn flake_hunt_treats_zero_reps_as_one() {
        let built = AtomicUsize::new(0);
        let ran = Arc::new(AtomicUsize::new(0));
        let unit = counting_unit(&built, Arc::clone(&ran), None);
        flake_hunt(vec![unit], 0).expect("zero clamps to one");
        // 1 sequential + 1 concurrent.
        assert_eq!(ran.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn all_units_covers_every_test_source() {
        let ctx = test_ctx();
        let units = all_units(&ctx, 2);
        let labels: Vec<&str> = units.iter().map(|u| u.label.as_str()).collect();
        assert!(labels.contains(&"host cargo test"));
        assert!(labels.contains(&"crypto constant-time (release)"));
        // Every fuzz harness and proptest model is its own unit.
        assert_eq!(
            labels.iter().filter(|l| l.starts_with("fuzz ")).count(),
            crate::commands::fuzz::TARGETS.len()
        );
        assert_eq!(
            labels.iter().filter(|l| l.starts_with("proptest ")).count(),
            crate::commands::proptest::MODELS.len()
        );
        // At least one QEMU enrolment is present.
        assert!(labels.iter().any(|l| l.starts_with("qemu ")));
    }
}
