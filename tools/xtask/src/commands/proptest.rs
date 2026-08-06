//! `cargo xtask proptest` — drive the Bronze stateful models.
//!
//! the charter requires the capability-critical paths — `lib/caps`,
//! `kernel/sec`, and the IPC + syscall dispatch paths — to carry a
//! `proptest`-style stateful model that "runs under `cargo xtask proptest`
//! for ≥ 5 s per change". This orchestrator is the single place that runs
//! every such model for a wall-clock budget, mirroring the
//! [`fuzz`](crate::commands) orchestrator so a PR and a nightly soak share
//! one definition of the model set.
//!
//! Each [`Model`] names an existing `cargo test` integration harness
//! (`tests/proptest_model.rs`). The orchestrator exports
//! `TAIRIX_PROPTEST_BUDGET_SECS`, which the harness reads to keep running
//! batches from its proptest RNG until the budget elapses (a plain `cargo
//! test` leaves the variable unset and runs the fast fixed-case sweep
//! instead). It also exports a per-model RNG seed (`commands::seed`): by
//! default a *fresh* seed each run, so two soaks never replay the same
//! programs; with `--seed N`, a deterministic seed that reproduces a
//! logged counterexample. The chosen seed is logged with each job. A model
//! that finds a counterexample, hangs, or otherwise fails its invariant
//! fails the command fails closed.
//!
//! Adding a model means adding a [`Model`] here, never teaching `ci` about
//! it directly.

use std::ffi::OsString;
use std::time::Duration;

use crate::commands::parallel::{self, Job};
use crate::commands::seed;
use crate::Context;

/// One in-tree stateful proptest model the orchestrator knows how to run.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Model {
    /// Short, unique selector used by `--target`.
    pub name: &'static str,
    /// Workspace package that owns the model (`cargo test -p`).
    pub package: &'static str,
    /// Integration-test binary name (the `tests/<name>.rs` file stem).
    pub test: &'static str,
    /// One-line description shown by `cargo xtask proptest --list`.
    pub description: &'static str,
}

/// The closed set of Bronze models, in run order. Covers every
/// capability-critical path the charter names.
pub const MODELS: &[Model] = &[
    Model {
        name: "caps",
        package: "tairix-caps",
        test: "proptest_model",
        description: "lib/caps CapabilitySet + signed-token verification",
    },
    Model {
        name: "sec",
        package: "tairix-kernel-sec",
        test: "proptest_model",
        description: "kernel/sec CapTable + TaskCapabilities derive/delegate/revoke",
    },
    Model {
        name: "ipc",
        package: "tairix-kernel-ipc",
        test: "proptest_model",
        description: "kernel/ipc capability-checked port dispatch",
    },
    Model {
        name: "syscall",
        package: "tairix-kernel-syscall",
        test: "proptest_model",
        description: "kernel/syscall dispatch capability gate",
    },
];

/// How long to run each model.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// `--once`: a single smoke iteration per model, with a fresh, logged
    /// seed. This is the per-PR `ci` gate: `ci` runs each test once (on a
    /// developer machine and a CI runner alike); the wall-clock soak coverage
    /// is the time-limited GitHub soak, not `ci`.
    Once,
    /// `--quick`: a short wall-clock budget (≥ 5 s per model), used by the
    /// time-limited GitHub `ci` soak step (`tools/ci/soak.sh`).
    Quick,
    /// `--soak`: the nightly budget (≥ 24 h per model).
    Soak,
}

impl Mode {
    /// Per-model wall-clock budget, or `None` for [`Mode::Once`] (a single
    /// smoke iteration with no budget).
    #[must_use]
    pub fn budget(self) -> Option<Duration> {
        match self {
            // A single iteration: no wall-clock budget is exported.
            Mode::Once => None,
            // "runs under `cargo xtask proptest` for ≥ 5 s".
            Mode::Quick => Some(Duration::from_secs(5)),
            // Match the soak floor so the nightly story is uniform.
            Mode::Soak => Some(Duration::from_hours(24)),
        }
    }
}

/// Parsed `proptest` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// Selected budget.
    pub mode: Mode,
    /// Optional model filter (`--target <name>`); runs all when `None`.
    pub only: Option<String>,
    /// `--list`: print the registry and exit without running anything.
    pub list: bool,
    /// Override the per-model budget in seconds (`--secs <n>`); CI never
    /// passes it — it exists for the orchestrator's own tests and local runs.
    pub secs: Option<u64>,
    /// Reproduce an earlier run by fixing its proptest RNG seed (`--seed <n>`).
    ///
    /// Unset (the default, including in `ci`) draws a fresh per-model seed each
    /// run, so consecutive soaks explore new programs. Setting it
    /// replays the exact sequence the orchestrator logged for a counterexample.
    pub seed: Option<u64>,
}

/// Parse `proptest` arguments. `--quick` is the default when neither budget
/// flag is given.
///
/// # Errors
/// Returns a usage error for an unknown flag, a missing flag value, a
/// non-numeric `--secs`, or a `--quick`/`--soak` conflict.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut mode: Option<Mode> = None;
    let mut only: Option<String> = None;
    let mut list = false;
    let mut secs: Option<u64> = None;
    let mut seed: Option<u64> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(flag) = arg.to_str() else {
            return Err(format!(
                "proptest: argument {} is not valid UTF-8",
                arg.display()
            ));
        };
        match flag {
            "--once" => set_mode(&mut mode, Mode::Once)?,
            "--quick" => set_mode(&mut mode, Mode::Quick)?,
            "--soak" => set_mode(&mut mode, Mode::Soak)?,
            "--list" => list = true,
            "--target" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "proptest: `--target` requires a model name".to_string())?;
                let name = value
                    .to_str()
                    .ok_or_else(|| "proptest: `--target` value is not valid UTF-8".to_string())?;
                only = Some(name.to_string());
            }
            "--secs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "proptest: `--secs` requires a number".to_string())?;
                let parsed = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        format!("proptest: `--secs` expects a u64, got {}", value.display())
                    })?;
                secs = Some(parsed);
            }
            "--seed" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "proptest: `--seed` requires a number".to_string())?;
                let parsed = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        format!("proptest: `--seed` expects a u64, got {}", value.display())
                    })?;
                seed = Some(parsed);
            }
            other => {
                return Err(format!(
                    "proptest: unexpected argument {other:?}; usage: \
                     cargo xtask proptest [--once | --quick | --soak] [--target NAME] [--secs N] \
                     [--seed N] [--list]"
                ));
            }
        }
    }

    Ok(Options {
        mode: mode.unwrap_or(Mode::Quick),
        only,
        list,
        secs,
        seed,
    })
}

fn set_mode(slot: &mut Option<Mode>, mode: Mode) -> Result<(), String> {
    match slot {
        Some(existing) if *existing != mode => {
            Err("proptest: `--once`, `--quick`, and `--soak` are mutually exclusive".to_string())
        }
        _ => {
            *slot = Some(mode);
            Ok(())
        }
    }
}

/// Resolve the models an [`Options`] selects, preserving registry order.
///
/// # Errors
/// Returns an error if `--target` names a model that is not registered.
pub fn selected(opts: &Options) -> Result<Vec<Model>, String> {
    let Some(name) = opts.only.as_deref() else {
        return Ok(MODELS.to_vec());
    };
    match MODELS.iter().find(|m| m.name == name) {
        Some(m) => Ok(vec![*m]),
        None => Err(format!(
            "proptest: unknown model `{name}`; known models: {}",
            MODELS.iter().map(|m| m.name).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Run the selected models for their budget.
pub fn run(ctx: &Context, opts: &Options) -> Result<(), String> {
    if opts.list {
        for m in MODELS {
            println!("{:<10} {}  [{}]", m.name, m.description, m.package);
        }
        return Ok(());
    }

    // An explicit `--secs N` always wins; otherwise the mode supplies the
    // budget (`--once` is `None`: a single smoke iteration, no budget env).
    let budget: Option<Duration> = match opts.secs {
        Some(s) if s > 0 => Some(Duration::from_secs(s)),
        Some(_) => None,
        None => opts.mode.budget(),
    };
    let models = selected(opts)?;
    // Each model is an independent, budget-bounded host process, so the
    // registry runs concurrently rather than paying the sum of every model's
    // budget. The shared runner caps concurrency at the host's parallelism
    // and fails closed (`commands::parallel`).
    let jobs: Vec<Job> = models
        .iter()
        .enumerate()
        .map(|(i, m)| job_for(ctx, m, budget, opts.seed, i))
        .collect();
    let concurrency = parallel::default_concurrency(jobs.len());
    parallel::run(jobs, concurrency)
}

/// Build the `cargo test` [`Job`] that runs one stateful model.
///
/// `budget` is the wall-clock soak budget exported to the model (`None` runs
/// the single fixed-case smoke sweep, as `--once` does). `seed` and `index`
/// pick the per-job proptest RNG seed (`None` draws a fresh entropy seed each
/// call, so repeated runs draw new programs; `Some(base)` reproduces a logged
/// counterexample). The chosen seed is baked into the label so it reaches the
/// log.
///
/// Shared by [`run`] and the long-CI flake hunt ([`super::ci_long`]) so both
/// build a model job the one way.
pub(crate) fn job_for(
    ctx: &Context,
    model: &Model,
    budget: Option<Duration>,
    seed: Option<u64>,
    index: usize,
) -> Job {
    let mut cmd = ctx.cargo();
    cmd.args([
        "test",
        "-p",
        model.package,
        "--test",
        model.test,
        "--locked",
        "--",
        "--nocapture",
    ]);
    // A budget turns the model into a soak loop; without one the model runs
    // its single smoke iteration.
    if let Some(budget) = budget {
        cmd.env(
            tairix_fuzzseed::PROPTEST_BUDGET_ENV,
            budget.as_secs().to_string(),
        );
    }
    // Each model reseeds its proptest RNG from this value instead of a fixed
    // seed, so a fresh seed (the default) makes every run draw new programs
    // while an explicit seed reproduces a logged counterexample.
    let job_seed = seed::job_seed(seed, index);
    cmd.env(seed::PROPTEST_SEED_ENV, job_seed.to_string());
    let budget_desc = match budget {
        Some(b) => format!("{} s", b.as_secs()),
        None => "1 iteration".to_string(),
    };
    let label = format!("proptest {} ({budget_desc}, seed {job_seed})", model.name);
    Job::new(label, cmd).with_soak_budget(budget)
}

#[cfg(test)]
mod tests {
    use super::{parse, selected, Mode, MODELS};
    use std::ffi::OsString;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn defaults_to_quick_running_every_model() {
        let opts = parse(&argv(&[])).expect("empty args parse");
        assert_eq!(opts.mode, Mode::Quick);
        assert!(opts.only.is_none());
        assert!(!opts.list);
        assert_eq!(selected(&opts).expect("all models").len(), MODELS.len());
    }

    #[test]
    fn once_flag_selects_a_single_iteration_with_no_budget() {
        let opts = parse(&argv(&["--once"])).expect("once parses");
        assert_eq!(opts.mode, Mode::Once);
        // No wall-clock budget: the model runs its single smoke iteration.
        assert!(Mode::Once.budget().is_none());
    }

    #[test]
    fn quick_budget_meets_the_five_second_floor() {
        // the charter mandates ≥ 5 s per model for the budgeted soak step.
        assert!(Mode::Quick.budget().expect("quick is budgeted").as_secs() >= 5);
    }

    #[test]
    fn soak_budget_meets_the_twenty_four_hour_floor() {
        assert!(Mode::Soak.budget().expect("soak is budgeted").as_secs() >= 24 * 60 * 60);
    }

    #[test]
    fn soak_flag_selects_the_soak_budget() {
        let opts = parse(&argv(&["--soak"])).expect("soak parses");
        assert_eq!(opts.mode, Mode::Soak);
    }

    #[test]
    fn target_filter_selects_one_known_model() {
        let opts = parse(&argv(&["--target", "sec"])).expect("target parses");
        let chosen = selected(&opts).expect("known model");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-kernel-sec");
    }

    #[test]
    fn unknown_target_fails_closed() {
        let opts = parse(&argv(&["--target", "no_such_model"])).expect("flag parses");
        assert!(selected(&opts).is_err());
    }

    #[test]
    fn conflicting_budget_flags_are_rejected() {
        assert!(parse(&argv(&["--quick", "--soak"])).is_err());
    }

    #[test]
    fn secs_override_parses() {
        let opts = parse(&argv(&["--secs", "3"])).expect("secs parses");
        assert_eq!(opts.secs, Some(3));
    }

    #[test]
    fn secs_requires_a_number() {
        assert!(parse(&argv(&["--secs", "soon"])).is_err());
        assert!(parse(&argv(&["--secs"])).is_err());
    }

    #[test]
    fn seed_defaults_to_none_so_each_run_is_fresh() {
        // No `--seed` means a fresh seed per run, so consecutive soaks draw
        // new programs.
        let opts = parse(&argv(&[])).expect("empty args parse");
        assert_eq!(opts.seed, None);
    }

    #[test]
    fn seed_override_parses() {
        let opts = parse(&argv(&["--seed", "98765"])).expect("seed parses");
        assert_eq!(opts.seed, Some(98765));
    }

    #[test]
    fn seed_requires_a_number() {
        assert!(parse(&argv(&["--seed", "later"])).is_err());
        assert!(parse(&argv(&["--seed"])).is_err());
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(parse(&argv(&["--turbo"])).is_err());
    }

    #[test]
    fn list_flag_is_parsed() {
        let opts = parse(&argv(&["--list"])).expect("list parses");
        assert!(opts.list);
    }

    #[test]
    fn every_registered_model_has_a_unique_name() {
        for (i, a) in MODELS.iter().enumerate() {
            for b in &MODELS[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate proptest model name");
            }
        }
    }

    #[test]
    fn registry_covers_every_capability_critical_path() {
        // lib/caps, kernel/sec, IPC dispatch, syscall dispatch.
        for required in ["caps", "sec", "ipc", "syscall"] {
            assert!(
                MODELS.iter().any(|m| m.name == required),
                "missing required proptest model {required}"
            );
        }
    }
}
