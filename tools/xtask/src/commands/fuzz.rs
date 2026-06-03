//! `cargo xtask fuzz` — drive the in-tree fuzz harnesses (`AGENTS.md` §19.6).
//!
//! RustOS does not pull in an external fuzz runner (`AGENTS.md` §2.12): the
//! per-crate harnesses are deterministic, seeded, allocation-free Rust tests
//! that §19.6 explicitly sanctions as the "equivalent in-tree harness". This
//! orchestrator is the single place that runs every such harness for a
//! wall-clock budget, so a PR and a nightly soak share one definition of the
//! target set.
//!
//! Each [`Target`] names an existing `cargo test` integration harness. The
//! orchestrator exports `RUSTOS_FUZZ_BUDGET_SECS`, which the harness reads to
//! keep drawing fresh inputs from its continuing PRNG stream until the budget
//! elapses (a plain `cargo test` leaves the variable unset and runs the fast,
//! fixed-iteration smoke sweep instead). A harness that crashes, hangs, or
//! fails its invariant fails the command — §19.6 fails closed.
//!
//! Adding a harness means adding a [`Target`] here, never teaching `ci`
//! about it directly. The §19.6 burn-down now covers the wire decoders,
//! the syscall dispatcher, the `userland/net` protocol parsers, and the
//! capability-checked IPC port endpoint.

use std::ffi::OsString;
use std::time::Duration;

use crate::Context;

/// One in-tree fuzz harness the orchestrator knows how to run.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Workspace package that owns the harness (`cargo test -p`).
    pub package: &'static str,
    /// Integration-test binary name (the `tests/<name>.rs` file stem).
    pub test: &'static str,
    /// One-line description shown by `cargo xtask fuzz --list`.
    pub description: &'static str,
}

/// The closed set of fuzz harnesses, in run order.
///
/// Every entry is a deterministic `cargo test` harness; this registry is
/// what wires them into the §19.6 budgeted runs. The set covers the wire
/// decoders (`lib/abi`), the syscall dispatcher, the `userland/net`
/// protocol parsers, and the capability-checked IPC port endpoint.
pub const TARGETS: &[Target] = &[
    Target {
        package: "rustos-abi",
        test: "fuzz_decode",
        description: "lib/abi wire decoders (IPC + manifest headers)",
    },
    Target {
        package: "rustos-kernel-syscall",
        test: "fuzz_args",
        description: "syscall dispatcher argument validation",
    },
    Target {
        package: "rustos-net-icmp",
        test: "fuzz_parse",
        description: "userland/net ARP/IPv4/ICMP/Ethernet parsers",
    },
    Target {
        package: "rustos-kernel-ipc",
        test: "fuzz_port",
        description: "IPC port send dispatch (capability + size + capacity)",
    },
    Target {
        package: "rustos-kernel-mem",
        test: "fuzz_swap",
        description: "encrypted-swap restore path (untrusted swap-device bytes)",
    },
    Target {
        package: "rustos-drv-fs-rustfs",
        test: "fuzz_mount",
        description:
            "rustfs mount / metadata + directory decode (superblock ring, root, trees, dirents)",
    },
    Target {
        package: "rustos-compress",
        test: "fuzz_compress",
        description: "first-party LZ decode (untrusted compressed-record bytes)",
    },
    Target {
        package: "rustos-svg",
        test: "fuzz_svg",
        description: "SVG asset decode (untrusted /System/Graphics image bytes)",
    },
];

/// How long to run each harness.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// `--quick`: the per-PR budget wired into `ci` (≥ 5 s per harness).
    Quick,
    /// `--soak`: the nightly budget (≥ 24 h per harness).
    Soak,
}

impl Mode {
    /// Per-harness wall-clock budget in seconds.
    #[must_use]
    pub fn budget(self) -> Duration {
        match self {
            // §19.6: "runs each harness for ≥ 5 s on every PR".
            Mode::Quick => Duration::from_secs(5),
            // §19.6: "runs each harness for ≥ 24 h".
            Mode::Soak => Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Parsed `fuzz` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// Selected budget.
    pub mode: Mode,
    /// Optional harness filter (`--target <name>`); runs all when `None`.
    pub only: Option<String>,
    /// `--list`: print the registry and exit without running anything.
    pub list: bool,
    /// Override the per-harness budget in seconds (`--secs <n>`).
    ///
    /// Exists so the orchestrator's own unit tests and local smoke runs do
    /// not have to wait the full budget; CI never passes it.
    pub secs: Option<u64>,
}

/// Parse `fuzz` arguments. `--quick` is the default when neither budget flag
/// is given.
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
            return Err(format!("fuzz: argument {arg:?} is not valid UTF-8"));
        };
        match flag {
            "--quick" => set_mode(&mut mode, Mode::Quick)?,
            "--soak" => set_mode(&mut mode, Mode::Soak)?,
            "--list" => list = true,
            "--target" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "fuzz: `--target` requires a harness name".to_string())?;
                let name = value
                    .to_str()
                    .ok_or_else(|| "fuzz: `--target` value is not valid UTF-8".to_string())?;
                only = Some(name.to_string());
            }
            "--secs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "fuzz: `--secs` requires a number".to_string())?;
                let parsed = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| format!("fuzz: `--secs` expects a u64, got {value:?}"))?;
                secs = Some(parsed);
            }
            other => {
                return Err(format!(
                    "fuzz: unexpected argument {other:?}; usage: \
                     cargo xtask fuzz [--quick | --soak] [--target NAME] [--secs N] [--list]"
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
            Err("fuzz: `--quick` and `--soak` are mutually exclusive".to_string())
        }
        _ => {
            *slot = Some(mode);
            Ok(())
        }
    }
}

/// Resolve the harnesses an [`Options`] selects, preserving registry order.
///
/// # Errors
/// Returns an error if `--target` names a harness that is not registered.
pub fn selected(opts: &Options) -> Result<Vec<Target>, String> {
    let Some(name) = opts.only.as_deref() else {
        return Ok(TARGETS.to_vec());
    };
    match TARGETS.iter().find(|t| t.test == name) {
        Some(t) => Ok(vec![*t]),
        None => Err(format!(
            "fuzz: unknown target `{name}`; known targets: {}",
            TARGETS
                .iter()
                .map(|t| t.test)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Run the selected harnesses for their budget.
pub fn run(ctx: &Context, opts: &Options) -> Result<(), String> {
    if opts.list {
        for t in TARGETS {
            println!("{:<24} {}  [{}]", t.test, t.description, t.package);
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
        // `--test <name>` runs exactly that integration harness; `--exact`
        // is unnecessary because the test binary contains only fuzz fns.
        cmd.args([
            "test",
            "-p",
            t.package,
            "--test",
            t.test,
            "--locked",
            "--",
            "--nocapture",
        ]);
        cmd.env("RUSTOS_FUZZ_BUDGET_SECS", budget.as_secs().to_string());
        let label = format!("fuzz {} ({} s)", t.test, budget.as_secs());
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
        // §19.6 mandates ≥ 5 s per harness on every PR.
        assert!(Mode::Quick.budget().as_secs() >= 5);
    }

    #[test]
    fn soak_budget_meets_the_twenty_four_hour_floor() {
        // §19.6 mandates ≥ 24 h per harness for the nightly soak.
        assert!(Mode::Soak.budget().as_secs() >= 24 * 60 * 60);
    }

    #[test]
    fn soak_flag_selects_the_soak_budget() {
        let opts = parse(&argv(&["--soak"])).expect("soak parses");
        assert_eq!(opts.mode, Mode::Soak);
    }

    #[test]
    fn target_filter_selects_one_known_harness() {
        let opts = parse(&argv(&["--target", "fuzz_decode"])).expect("target parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].test, "fuzz_decode");
    }

    #[test]
    fn unknown_target_fails_closed() {
        let opts = parse(&argv(&["--target", "no_such_harness"])).expect("flag parses");
        assert!(selected(&opts).is_err());
    }

    #[test]
    fn conflicting_budget_flags_are_rejected() {
        assert!(parse(&argv(&["--quick", "--soak"])).is_err());
    }

    #[test]
    fn repeating_the_same_budget_flag_is_accepted() {
        let opts = parse(&argv(&["--quick", "--quick"])).expect("idempotent flag");
        assert_eq!(opts.mode, Mode::Quick);
    }

    #[test]
    fn secs_override_parses_and_wins() {
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
    fn every_registered_target_has_a_unique_test_name() {
        for (i, a) in TARGETS.iter().enumerate() {
            for b in &TARGETS[i + 1..] {
                assert_ne!(a.test, b.test, "duplicate fuzz target name");
            }
        }
    }

    #[test]
    fn net_parser_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_parse"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "rustos-net-icmp");
    }

    #[test]
    fn rustfs_mount_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_mount"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "rustos-drv-fs-rustfs");
    }

    #[test]
    fn compress_decode_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_compress"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "rustos-compress");
    }

    #[test]
    fn svg_decode_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_svg"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "rustos-svg");
    }

    #[test]
    fn ipc_port_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_port"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "rustos-kernel-ipc");
    }

    #[test]
    fn registry_covers_the_burn_down_endpoints() {
        // §19.6: wire decoders, dispatcher, userland/net parsers, IPC port.
        for required in ["fuzz_decode", "fuzz_args", "fuzz_parse", "fuzz_port"] {
            assert!(
                TARGETS.iter().any(|t| t.test == required),
                "missing required fuzz harness {required}"
            );
        }
    }
}
