//! `cargo xtask loom` — model-check the synchronisation primitives over every
//! thread interleaving.
//!
//! A test suite runs whichever interleaving the host scheduler happened to
//! pick; it cannot say that *no* interleaving loses a wake-up or drops an
//! `Acquire`/`Release` pairing. `loom` can: it substitutes its own atomics
//! and cell for `core`'s and replays the model under every ordering the
//! memory model permits, so a missed edge is a reported counterexample rather
//! than a flake somebody sees once a year.
//!
//! It is a separate stage because it cannot ride the ordinary test pass: the
//! substitution is a `--cfg loom` whole-crate rebuild, and building the
//! production crate that way would make every other consumer of it wrong.
//!
//! The stage exists at all because the harness it drives had stopped
//! compiling and nothing noticed — the models are only worth writing if
//! something runs them.

use std::ffi::OsString;
use std::process::Command;

use crate::commands::parallel::{self, Job};
use crate::Context;

/// One crate whose primitives carry loom models.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Workspace package (`cargo test -p`).
    pub package: &'static str,
    /// The `tests/` harness holding its models.
    pub harness: &'static str,
    /// Why this crate's correctness needs an interleaving oracle.
    pub description: &'static str,
}

/// The crates carrying loom models.
pub const TARGETS: &[Target] = &[Target {
    package: "tairix-sync",
    harness: "loom",
    description: "the spin/MCS/RW locks, the seqlock, and the set-once cell",
}];

/// The cfg that swaps `core`'s atomics for the model checker's. Applied
/// through `RUSTFLAGS`, so it forces a rebuild of the target crate and its
/// dependents into a scratch profile of their own.
const LOOM_CFG: &str = "--cfg loom";

/// Parsed `loom` arguments.
pub struct Options {
    /// Restrict the run to one package.
    package: Option<String>,
    /// List the targets and exit.
    list: bool,
}

/// Parse `--package <name>` and `--list`.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut opts = Options {
        package: None,
        list: false,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some("--list") => opts.list = true,
            Some("--package" | "-p") => {
                let name = rest
                    .next()
                    .ok_or_else(|| "loom: --package needs a value".to_string())?;
                opts.package = Some(name.to_string_lossy().into_owned());
            }
            _ => {
                return Err(format!(
                    "loom: unexpected argument {}; usage: cargo xtask loom \
                     [--package <name>] [--list]",
                    arg.display()
                ))
            }
        }
    }
    Ok(opts)
}

/// The targets a run covers, honouring `--package`.
fn selected(opts: &Options) -> Result<Vec<&'static Target>, String> {
    let Some(name) = opts.package.as_deref() else {
        return Ok(TARGETS.iter().collect());
    };
    match TARGETS.iter().find(|t| t.package == name) {
        Some(target) => Ok(vec![target]),
        None => Err(format!(
            "loom: unknown package `{name}`; known: {}",
            TARGETS
                .iter()
                .map(|t| t.package)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Model-check every selected crate, failing closed.
///
/// # Errors
/// Any harness that fails to build, or whose model reports a counterexample.
pub fn run(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    let opts = parse(args)?;
    if opts.list {
        for target in TARGETS {
            println!("{:<24} {}", target.package, target.description);
        }
        return Ok(());
    }
    let jobs: Vec<Job> = selected(&opts)?.iter().map(|t| job_for(ctx, t)).collect();
    let concurrency = parallel::default_concurrency(jobs.len());
    parallel::run(jobs, concurrency)
}

/// One package's model-checked harness.
fn job_for(ctx: &Context, target: &Target) -> Job {
    let mut cmd: Command = ctx.cargo();
    // Release: loom replays a model thousands of times, and a debug build
    // spends the stage's whole budget in the interpreter's own bookkeeping.
    cmd.args([
        "test",
        "-p",
        target.package,
        "--test",
        target.harness,
        "--release",
        "--locked",
    ]);
    cmd.env("RUSTFLAGS", LOOM_CFG);
    Job::new(format!("loom {}", target.package), cmd)
}

#[cfg(test)]
mod tests {
    use super::{parse, selected, TARGETS};
    use std::ffi::OsString;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_selects_every_target() {
        let opts = parse(&[]).expect("no arguments");
        assert_eq!(selected(&opts).expect("all").len(), TARGETS.len());
    }

    #[test]
    fn a_named_package_selects_only_that_one() {
        let opts = parse(&args(&["--package", "tairix-sync"])).expect("filter");
        let picked = selected(&opts).expect("known package");
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].package, "tairix-sync");
    }

    #[test]
    fn an_unknown_package_is_refused_rather_than_silently_running_nothing() {
        let opts = parse(&args(&["--package", "nope"])).expect("filter");
        assert!(selected(&opts).is_err());
    }

    #[test]
    fn malformed_arguments_are_refused() {
        assert!(parse(&args(&["--package"])).is_err());
        assert!(parse(&args(&["--what"])).is_err());
    }

    /// Every target must name a harness that exists, or the stage passes by
    /// building nothing — the exact silence this stage was added to end.
    #[test]
    fn every_target_names_a_harness_that_exists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root");
        for target in TARGETS {
            let crate_dir = target
                .package
                .strip_prefix("tairix-")
                .unwrap_or(target.package);
            let path = root
                .join("lib")
                .join(crate_dir)
                .join("tests")
                .join(format!("{}.rs", target.harness));
            assert!(path.exists(), "missing loom harness: {}", path.display());
        }
    }
}
