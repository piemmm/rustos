//! Build orchestration for the RustOS workspace.
//!
//! `cargo xtask <command>` is the only sanctioned way to build, test, lint,
//! document, and audit RustOS. Driving every check through one entry point
//! keeps local developer flows and CI identical: when CI is green, a
//! contributor running the same command locally on a clean clone gets the
//! same result.
//!
//! ## Commands
//!
//! - `build`        — `cargo build --workspace --all-targets`
//! - `test`         — `cargo test --workspace --all-targets`
//! - `clippy`       — `cargo clippy --workspace --all-targets -- -D warnings`
//! - `fmt`          — `cargo fmt --all -- --check` (pass `--fix` to apply)
//! - `docs-check`   — rustdoc (deny warnings) + mdBook build + link check
//! - `abi-check`    — verifies the generated kernel syscall table matches
//!   its source of truth in `lib/abi`
//! - `deps-check`   — enforces the §17.4 modularity dependency graph
//!   (layering, concrete-scheduler naming, optional-desktop boundary)
//! - `cfg-check`    — rejects target-conditional compilation (§17.2)
//!   outside the architecture ports and build glue
//! - `coverage`     — `cargo llvm-cov` report for the host-testable subset
//! - `sbom`         — emit a `CycloneDX` SBOM from `Cargo.lock` (§19.3):
//!   every workspace and external crate with its version, source URL,
//!   and pinned source checksum
//! - `supply-chain` — verify the committed source-hash allow-list
//!   against `Cargo.lock` and enforce the RUSTSEC advisory SLA (§19.3);
//!   `--write-pins` regenerates the pins from the lockfile
//! - `fuzz`         — drive the in-tree fuzz harnesses for a wall-clock
//!   budget (§19.6): `--quick` (≥ 60 s each, the `ci` budget) or
//!   `--soak` (≥ 24 h each, nightly)
//! - `model-check`  — exhaustively model-check the §19.7 Silver capability +
//!   IPC state machine (an in-tree explicit-state checker; the TLA+
//!   equivalent), failing closed on any invariant counterexample
//! - `ci`           — the full pipeline a PR must pass: `fmt --check`,
//!   `clippy`, `deps-check`, `cfg-check`, `test`, `docs-check`,
//!   `cargo deny check`, `supply-chain`, `fuzz --quick`, `proptest --quick`,
//!   `model-check`, `spec-review`, `abi-check`
//! - `image`        — build platform images via `tools/mkimage`
//!
//! The set above is closed: every subsystem documented in `AGENTS.md` and
//! `PLAN.md` is reachable through exactly one of these subcommands. New
//! pipeline steps belong in a *named* subcommand, never inlined into `ci`.

use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod commands;

use commands::Command as Subcommand;

fn main() -> ExitCode {
    let argv: Vec<OsString> = env::args_os().skip(1).collect();
    let Some((name, rest)) = argv.split_first() else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };

    let Some(name_str) = name.to_str() else {
        eprintln!("xtask: command name is not valid UTF-8");
        return ExitCode::from(2);
    };

    if matches!(name_str, "-h" | "--help" | "help") {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }

    let Some(command) = Subcommand::parse(name_str) else {
        eprintln!("xtask: unknown command `{name_str}`\n\n{}", usage());
        return ExitCode::from(2);
    };

    let ctx = match Context::discover() {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!("xtask: {err}");
            return ExitCode::FAILURE;
        }
    };

    match command.run(&ctx, rest) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> String {
    let mut out = String::from("usage: cargo xtask <command> [args]\n\ncommands:\n");
    for cmd in Subcommand::ALL {
        // `write!` into a `String` is infallible; the result is discarded.
        let _ = writeln!(out, "  {:<12} {}", cmd.name(), cmd.summary());
    }
    out
}

/// Repository-wide paths discovered once per invocation.
pub struct Context {
    /// Absolute path to the workspace root (the directory containing the
    /// top-level `Cargo.toml`).
    pub workspace_root: PathBuf,
    /// Path to the `cargo` executable the parent shell used to invoke us.
    /// Honouring `$CARGO` keeps custom toolchains (rustup overrides, CI
    /// matrix shims) consistent across nested calls.
    pub cargo: OsString,
}

impl Context {
    fn discover() -> Result<Self, String> {
        let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let workspace_root = workspace_root_from_manifest_dir()?;
        Ok(Self {
            workspace_root,
            cargo,
        })
    }

    /// Builds a `cargo` command rooted at the workspace.
    #[must_use]
    pub fn cargo(&self) -> Command {
        let mut cmd = Command::new(&self.cargo);
        cmd.current_dir(&self.workspace_root);
        cmd
    }

    /// Runs `cmd` inheriting stdio. Returns an error describing the failure
    /// if the child exits with a non-zero status.
    pub fn run(&self, label: &str, mut cmd: Command) -> Result<(), String> {
        let printable = format!("{cmd:?}");
        eprintln!("xtask: [{label}] {printable}");
        match cmd.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("{label} failed with {status}")),
            Err(err) => Err(format!("{label} could not be spawned: {err}")),
        }
    }
}

/// Resolve the workspace root from the manifest directory of the xtask crate.
///
/// We deliberately avoid shelling out to `cargo locate-project` so this works
/// even when the cargo cache is cold or when the xtask binary is invoked
/// from a packaged release tarball.
fn workspace_root_from_manifest_dir() -> Result<PathBuf, String> {
    // `CARGO_MANIFEST_DIR` is set by cargo when xtask is invoked through the
    // `cargo xtask` alias. When the binary is run directly we walk up from
    // the executable looking for `Cargo.toml` containing `[workspace]`.
    if let Some(dir) = env::var_os("CARGO_MANIFEST_DIR") {
        let path = PathBuf::from(dir);
        // `tools/xtask` → `tools` → workspace root.
        let root = path
            .ancestors()
            .find(|p| is_workspace_root(p))
            .ok_or_else(|| {
                format!(
                    "could not locate the workspace root above {}",
                    path.display()
                )
            })?;
        return Ok(root.to_path_buf());
    }

    let mut dir = env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    loop {
        if is_workspace_root(&dir) {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("no workspace Cargo.toml found in any parent directory".to_string());
        }
    }
}

fn is_workspace_root(dir: &Path) -> bool {
    let manifest = dir.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return false;
    };
    text.contains("[workspace]")
}
