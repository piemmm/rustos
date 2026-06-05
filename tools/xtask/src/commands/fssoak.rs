//! `cargo xtask fssoak` — drive the in-RAM filesystem soak.
//!
//! `.junie/filesystems.md` requires a filesystem soak that formats a
//! ≥ 1 GiB RAM volume with each first-party formatter and exercises it
//! for integrity and the fail-closed extremes, for `rustfs`, `ext4`, and
//! `fat32` **in parallel**. This orchestrator is the single place that
//! runs each filesystem's soak for a wall-clock budget, mirroring the
//! §19.6 [`fuzz`](crate::commands) and §19.7
//! [`proptest`](crate::commands) orchestrators so a PR smoke and a
//! nightly soak share one definition of the target set.
//!
//! Each [`Target`] names a `#[test]` entry point in the
//! `rustos-test-fs-soak` integration binary (`tests/fs_soak.rs`). The
//! orchestrator exports `RUSTOS_FSSOAK_BUDGET_SECS` (loop until it
//! elapses) and `RUSTOS_FSSOAK_BYTES` (the ≥ 1 GiB device size); a plain
//! `cargo test` leaves both unset and runs a single smoke iteration on a
//! smaller device instead. A target that finds an inconsistency, hangs,
//! or otherwise fails its invariant fails the command — the soak fails
//! closed.
//!
//! The per-target fan-out into parallel jobs is `tools/ci/soak.sh`'s job
//! (like fuzz/proptest); this command runs one target at a time.
//!
//! Adding a filesystem means adding a [`Target`] here, never teaching
//! `ci` about it directly.

use std::ffi::OsString;
use std::time::Duration;

use crate::Context;

/// One filesystem the soak orchestrator knows how to run.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Short, unique selector used by `--target` and `soak.sh`.
    pub name: &'static str,
    /// `#[test]` entry point in the soak integration binary.
    pub test_fn: &'static str,
    /// One-line description shown by `cargo xtask fssoak --list`.
    pub description: &'static str,
}

/// Workspace package owning the soak harness (`cargo test -p`).
const PACKAGE: &str = "rustos-test-fs-soak";

/// Integration-test binary (the `tests/<name>.rs` file stem).
const TEST_BIN: &str = "fs_soak";

/// Device size, in bytes, the soak formats: the 1 GiB minimum from
/// `.junie/filesystems.md`. `--quick` keeps the full size and simply
/// runs fewer iterations (budget-bounded) rather than shrinking below
/// the spec'd minimum.
const SOAK_DEVICE_BYTES: u64 = 1024 * 1024 * 1024;

/// The closed set of soak filesystems, in run order. Mirrors the
/// `rustos_test_fs_soak::TARGETS` registry the harness owns.
pub const TARGETS: &[Target] = &[
    Target {
        name: "rustfs",
        test_fn: "soak_rustfs",
        description: "native rustfs: format + integrity + extremes",
    },
    Target {
        name: "ext4",
        test_fn: "soak_ext4",
        description: "ext4: multi-group format + integrity + extremes",
    },
    Target {
        name: "fat32",
        test_fn: "soak_fat32",
        description: "fat32: format + integrity + extremes",
    },
    Target {
        name: "rustfs-random",
        test_fn: "soak_rustfs_random",
        description: "rustfs: randomized, model-checked op mix (new path each run)",
    },
];

/// How long to run each filesystem's soak.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// `--quick`: the per-PR / smoke budget (≥ 5 s per filesystem).
    Quick,
    /// `--soak`: the nightly budget (≥ 24 h per filesystem).
    Soak,
}

impl Mode {
    /// Per-target wall-clock budget.
    #[must_use]
    pub fn budget(self) -> Duration {
        match self {
            // Mirror the §19.6/§19.7 quick floor.
            Mode::Quick => Duration::from_secs(5),
            Mode::Soak => Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Parsed `fssoak` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// Selected budget.
    pub mode: Mode,
    /// Optional target filter (`--target <name>`); runs all when `None`.
    pub only: Option<String>,
    /// `--list`: print the registry and exit without running anything.
    pub list: bool,
    /// Override the per-target budget in seconds (`--secs <n>`).
    pub secs: Option<u64>,
}

/// Parse `fssoak` arguments. `--quick` is the default when neither
/// budget flag is given.
///
/// # Errors
/// Returns a usage error for an unknown flag, a missing flag value, a
/// non-numeric `--secs`, or a `--quick`/`--soak` conflict.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut mode: Option<Mode> = None;
    let mut only: Option<String> = None;
    let mut list = false;
    let mut secs: Option<u64> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(flag) = arg.to_str() else {
            return Err(format!("fssoak: argument {arg:?} is not valid UTF-8"));
        };
        match flag {
            "--quick" => set_mode(&mut mode, Mode::Quick)?,
            "--soak" => set_mode(&mut mode, Mode::Soak)?,
            "--list" => list = true,
            "--target" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "fssoak: `--target` requires a filesystem name".to_string())?;
                let name = value
                    .to_str()
                    .ok_or_else(|| "fssoak: `--target` value is not valid UTF-8".to_string())?;
                only = Some(name.to_string());
            }
            "--secs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "fssoak: `--secs` requires a number".to_string())?;
                let parsed = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| format!("fssoak: `--secs` expects a u64, got {value:?}"))?;
                secs = Some(parsed);
            }
            other => {
                return Err(format!(
                    "fssoak: unexpected argument {other:?}; usage: \
                     cargo xtask fssoak [--quick | --soak] [--target NAME] [--secs N] [--list]"
                ));
            }
        }
    }

    Ok(Options {
        mode: mode.unwrap_or(Mode::Quick),
        only,
        list,
        secs,
    })
}

fn set_mode(slot: &mut Option<Mode>, mode: Mode) -> Result<(), String> {
    match slot {
        Some(existing) if *existing != mode => {
            Err("fssoak: `--quick` and `--soak` are mutually exclusive".to_string())
        }
        _ => {
            *slot = Some(mode);
            Ok(())
        }
    }
}

/// Resolve the targets an [`Options`] selects, preserving registry order.
///
/// # Errors
/// Returns an error if `--target` names a filesystem that is not
/// registered.
pub fn selected(opts: &Options) -> Result<Vec<Target>, String> {
    let Some(name) = opts.only.as_deref() else {
        return Ok(TARGETS.to_vec());
    };
    match TARGETS.iter().find(|t| t.name == name) {
        Some(t) => Ok(vec![*t]),
        None => Err(format!(
            "fssoak: unknown filesystem `{name}`; known: {}",
            TARGETS
                .iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Run the selected filesystems' soaks for their budget.
pub fn run(ctx: &Context, opts: &Options) -> Result<(), String> {
    if opts.list {
        for t in TARGETS {
            println!("{:<8} {}", t.name, t.description);
        }
        return Ok(());
    }

    let budget = match opts.secs {
        Some(s) => Duration::from_secs(s),
        None => opts.mode.budget(),
    };
    let targets = selected(opts)?;
    for t in &targets {
        let mut cmd = ctx.cargo();
        cmd.args([
            "test",
            "-p",
            PACKAGE,
            "--test",
            TEST_BIN,
            "--locked",
            "--",
            "--exact",
            t.test_fn,
            "--nocapture",
        ]);
        cmd.env("RUSTOS_FSSOAK_BUDGET_SECS", budget.as_secs().to_string());
        cmd.env("RUSTOS_FSSOAK_BYTES", SOAK_DEVICE_BYTES.to_string());
        let label = format!("fssoak {} ({} s)", t.name, budget.as_secs());
        ctx.run(&label, cmd)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse, selected, Mode, TARGETS};
    use std::ffi::OsString;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn defaults_to_quick_running_every_target() {
        let opts = parse(&argv(&[])).expect("empty args parse");
        assert_eq!(opts.mode, Mode::Quick);
        assert!(opts.only.is_none());
        assert!(!opts.list);
        assert_eq!(selected(&opts).expect("all targets").len(), TARGETS.len());
    }

    #[test]
    fn quick_budget_meets_the_five_second_floor() {
        assert!(Mode::Quick.budget().as_secs() >= 5);
    }

    #[test]
    fn soak_budget_meets_the_twenty_four_hour_floor() {
        assert!(Mode::Soak.budget().as_secs() >= 24 * 60 * 60);
    }

    #[test]
    fn soak_flag_selects_the_soak_budget() {
        let opts = parse(&argv(&["--soak"])).expect("soak parses");
        assert_eq!(opts.mode, Mode::Soak);
    }

    #[test]
    fn target_filter_selects_one_known_filesystem() {
        let opts = parse(&argv(&["--target", "ext4"])).expect("target parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].test_fn, "soak_ext4");
    }

    #[test]
    fn unknown_target_fails_closed() {
        let opts = parse(&argv(&["--target", "zfs"])).expect("flag parses");
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
    fn unknown_flag_is_rejected() {
        assert!(parse(&argv(&["--turbo"])).is_err());
    }

    #[test]
    fn list_flag_is_parsed() {
        let opts = parse(&argv(&["--list"])).expect("list parses");
        assert!(opts.list);
    }

    #[test]
    fn registry_covers_every_soak_target() {
        for required in ["rustfs", "ext4", "fat32", "rustfs-random"] {
            assert!(
                TARGETS.iter().any(|t| t.name == required),
                "missing required soak target {required}"
            );
        }
    }

    #[test]
    fn every_registered_target_has_a_unique_name() {
        for (i, a) in TARGETS.iter().enumerate() {
            for b in &TARGETS[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate soak target name");
            }
        }
    }
}
