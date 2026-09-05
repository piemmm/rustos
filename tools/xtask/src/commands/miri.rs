//! `cargo xtask miri` — run the workspace's `unsafe` cores under an
//! undefined-behaviour oracle.
//!
//! A test suite proves what a program computes; it cannot prove that a raw
//! pointer stayed in bounds, that a slot was initialised before it was read,
//! or that two `&mut` never aliased. Miri interprets the program and checks
//! exactly those, so it is the oracle the hand-written containers need and
//! the ordinary test matrix cannot be.
//!
//! The stage is deliberately narrow. Miri interprets every operation, so
//! pointing it at the whole workspace would cost hours and tell us nothing
//! about the crates that carry no `unsafe` at all. [`TARGETS`] therefore names
//! the crates whose safety rests on a hand-written `unsafe` core, and each of
//! those crates scales its own sweeps down under `cfg(miri)` — the wide input
//! search belongs to the ordinary and budgeted runs; this one is looking for
//! undefined behaviour, which one pass over each code path already exposes.
//!
//! Adding a crate here means adding a [`Target`], never teaching `ci` about it
//! directly.

use std::ffi::OsString;
use std::process::Command;

use crate::commands::parallel::{self, Job};
use crate::commands::seed;
use crate::Context;

/// One crate the oracle is pointed at.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Workspace package (`cargo miri test -p`).
    pub package: &'static str,
    /// Why this crate's safety needs an oracle.
    pub description: &'static str,
}

/// The crates whose soundness rests on a hand-written `unsafe` core.
pub const TARGETS: &[Target] = &[
    Target {
        package: "tairix-collections",
        description: "the open-addressed hash table's control array and iterators",
    },
    Target {
        package: "tairix-inline",
        description: "the allocation-free tier's inline slot arrays, and the volatile scrub a secret ring leaves behind",
    },
    Target {
        package: "tairix-hash",
        description: "the one-shot key-publication cell the containers are keyed through",
    },
];

/// Miri's own flags.
///
/// Stacked Borrows is the stricter aliasing model of the two Miri ships and is
/// the one an intrusive, pointer-based container is most likely to violate, so
/// the default stands. Isolation stays on — a container touches no clock, no
/// filesystem, and no network, and a stage that needed to would be telling us
/// something — with the one harness seed forwarded so a reported failure
/// replays exactly.
const MIRIFLAGS: &str = "-Zmiri-strict-provenance";

/// Parsed `miri` arguments.
pub struct Options {
    /// Restrict the run to one package.
    package: Option<String>,
    /// Base seed for the harnesses, so a reported failure replays.
    seed: Option<u64>,
    /// List the targets and exit.
    list: bool,
}

/// Parse `--package <name>`, `--seed <n>`, and `--list`.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut opts = Options {
        package: None,
        seed: None,
        list: false,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some("--list") => opts.list = true,
            Some("--package" | "-p") => {
                let value = rest
                    .next()
                    .and_then(|v| v.to_str().map(str::to_string))
                    .ok_or_else(|| "miri: --package needs a name".to_string())?;
                opts.package = Some(value);
            }
            Some("--seed") => {
                let value = rest
                    .next()
                    .and_then(|v| v.to_str())
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| "miri: --seed needs a u64".to_string())?;
                opts.seed = Some(value);
            }
            _ => {
                return Err(format!(
                    "miri: unexpected argument {}; usage: cargo xtask miri \
                     [--package <name>] [--seed <n>] [--list]",
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
            "miri: unknown package `{name}`; known: {}",
            TARGETS
                .iter()
                .map(|t| t.package)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Run the oracle over every selected crate, failing closed.
pub fn run(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    let opts = parse(args)?;
    if opts.list {
        for target in TARGETS {
            println!("{:<24} {}", target.package, target.description);
        }
        return Ok(());
    }
    if !crate::commands::cargo_subcommand_available(ctx, "miri") {
        return Err(
            "miri is not installed; run `rustup component add miri` (it is pinned in \
             rust-toolchain.toml, so `rustup toolchain install` also brings it)"
                .to_string(),
        );
    }

    let targets = selected(&opts)?;
    // Each package is an independent host process, so the set runs
    // concurrently under the shared bounded runner rather than paying the sum
    // of the interpreter's costs.
    let jobs: Vec<Job> = targets
        .iter()
        .enumerate()
        .map(|(index, target)| job_for(ctx, target, opts.seed, index))
        .collect();
    let concurrency = parallel::default_concurrency(jobs.len());
    parallel::run(jobs, concurrency)
}

/// One package's interpreted test run.
fn job_for(ctx: &Context, target: &Target, seed: Option<u64>, index: usize) -> Job {
    let mut cmd: Command = ctx.cargo();
    cmd.args(["miri", "test", "-p", target.package, "--locked"]);
    let job_seed = seed::job_seed(seed, index);
    cmd.env(seed::FUZZ_SEED_ENV, job_seed.to_string());
    // Miri hides the host environment from the interpreted program, so the
    // seed is forwarded explicitly; without it the harness falls back to the
    // wall clock, which isolation correctly refuses.
    cmd.env(
        "MIRIFLAGS",
        format!("{MIRIFLAGS} -Zmiri-env-forward={}", seed::FUZZ_SEED_ENV),
    );
    Job::new(format!("miri {} (seed {job_seed})", target.package), cmd)
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
    fn a_package_filter_selects_exactly_one_target() {
        let opts = parse(&args(&["--package", "tairix-collections"])).expect("filter");
        let chosen = selected(&opts).expect("one");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-collections");
    }

    #[test]
    fn an_unknown_package_is_refused_rather_than_silently_skipped() {
        let opts = parse(&args(&["--package", "nope"])).expect("filter");
        assert!(selected(&opts).is_err());
    }

    #[test]
    fn a_malformed_argument_is_refused() {
        assert!(parse(&args(&["--seed"])).is_err());
        assert!(parse(&args(&["--seed", "not-a-number"])).is_err());
        assert!(parse(&args(&["--what"])).is_err());
    }

    /// Every target must name a real workspace package, and none twice.
    #[test]
    fn the_registry_is_distinct() {
        for (index, target) in TARGETS.iter().enumerate() {
            assert!(target.package.starts_with("tairix-"), "{}", target.package);
            assert!(!target.description.is_empty());
            for other in &TARGETS[index + 1..] {
                assert_ne!(target.package, other.package);
            }
        }
    }
}
