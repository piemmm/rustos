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
//! - `build`        — `cargo build --workspace --all-targets`;
//!   `--target aarch64-rpi` instead builds the flashable platform image
//!   (the `image` pipeline below). Runs `prune` first so the superseded
//!   build-script output an earlier build orphaned does not accumulate
//! - `clean`        — `cargo clean` to reclaim `target/` disk space (a full
//!   multi-arch `-Z build-std` tree runs to tens of GB per target); cargo
//!   selectors are forwarded (`--release`, `--doc`, `--target <triple>`,
//!   `-p <crate>`) and the reclaimed size is reported
//! - `prune`        — reclaim only the *superseded* build-script output the
//!   `rustos-kernel` build script orphans: it compiles the embedded userland
//!   programs (each a ~1 GB `-Z build-std` tree) into an `OUT_DIR` cargo keys
//!   by build-script fingerprint, so every `build.rs` change strands the
//!   previous tree forever. Keeps the newest `build/<pkg>-<hash>` per package
//!   and removes the older siblings; run automatically before every `build`
//!   and `image`, and on demand. Unlike `clean` it never touches the live
//!   build, so the next compile is still incremental
//! - `test`         — `cargo test --workspace --all-targets`; `--count N`
//!   (alias `--iterations N`) repeats the whole matrix N times to surface
//!   flaky tests, defaulting to one run; `--soak` (tuned by `--secs N`)
//!   instead repeats it for a wall-clock budget (24 h by default), which the
//!   nightly `soak` workflow uses to run the tests repeatedly for 24 h
//! - `clippy`       — `cargo clippy --workspace --all-targets -- -D warnings`
//! - `fmt`          — `cargo fmt --all -- --check` (pass `--fix` to apply)
//! - `docs-check`   — rustdoc (deny warnings) + mdBook build + link check
//! - `abi-check`    — verifies the generated kernel syscall table matches
//!   its source of truth in `lib/abi`
//! - `c-header`     — generates (`--write`) or verifies the C ABI
//!   development header in `include/` from the `lib/abi` source of truth
//! - `font-atlas`   — generates (`--write`) or verifies the Inconsolata EX
//!   glyph atlas in `lib/font/src/` from the committed face in
//!   `lib/font/assets/`
//! - `devids`       — verifies the compact PCI/USB ID-database tables in
//!   `lib/devids/tables/` against the vetted snapshots in
//!   `lib/devids/assets/` (`--write` to regenerate; `--fetch` — developer-run
//!   only, never CI — to import and vet the upstream databases)
//! - `deps-check` — enforces the modularity dependency graph
//!   (layering, concrete-scheduler naming, optional-desktop boundary)
//! - `cfg-check` — rejects target-conditional compilation
//!   outside the architecture ports and build glue
//! - `help-lint` — lints every command app's discovered `Help/` tree
//!   (plans/APPS.md §8.1): canonical `en-US/` presence, required-locale completeness,
//!   structural bounds, cross-locale `OPTIONS` switch-key drift, the
//!   content-policy screen, and per-command coverage
//! - `coverage`     — `cargo llvm-cov` report for the host-testable subset
//! - `sbom` — emit a `CycloneDX` SBOM from `Cargo.lock`:
//!   every workspace and external crate with its version, source URL,
//!   and pinned source checksum
//! - `supply-chain` — verify the committed source-hash allow-list
//!   against `Cargo.lock` and enforce the RUSTSEC advisory SLA;
//!   `--write-pins` regenerates the pins from the lockfile
//! - `fuzz`         — drive the in-tree fuzz harnesses for a wall-clock
//!   budget: `--quick` (≥ 60 s each, the `ci` budget) or
//!   `--soak` (≥ 24 h each, nightly)
//! - `model-check` — exhaustively model-check the Silver capability +
//!   IPC state machine (an in-tree explicit-state checker; the TLA+
//!   equivalent), failing closed on any invariant counterexample
//! - `ci`           — the full pipeline a PR must pass, ordered cheapest-first
//!   so a failing PR fails fast: `fmt --check`, then the concurrent
//!   static-gate group (`deps-check`, `cfg-check`, `help-lint`,
//!   `spec-review`, `supply-chain`, `abi-check`, `c-header`, `font-atlas`,
//!   `devids`, `model-check` —
//!   all read-only, non-compiling checks run at once so their costs overlap),
//!   then `docs-check` (rustdoc/link failures are the common first trip and
//!   need only a doc build, so they run ahead of the compile-heavy stages),
//!   `clippy`, `test` (run 20× on a GitHub Actions runner to catch flaky
//!   tests; once locally so a pre-push `ci` is not punishingly slow),
//!   `cargo deny check`, `fuzz --quick`, `proptest --quick`,
//!   the release crypto constant-time tests, and the image gate
//! - `ci-long`      — the same checks as `ci`, but for a dedicated long-lived
//!   runner: every test-executing stage (the host test matrix, the release
//!   crypto constant-time tests, the QEMU integration tests, the fuzz
//!   harnesses, and the proptest models) is run 20× sequentially and then 20×
//!   concurrently, per test, to force out timing- and contention-dependent
//!   flakes; the deterministic gates run once. `--dry-run` prints the plan
//!   without running anything
//! - `image`        — build platform images via `tools/mkimage`
//!   (`--target aarch64-rpi`: the Raspberry Pi 4 SD image; the pinned
//!   firmware blobs are staged build inputs, see
//!   `tools/mkimage/firmware.lock` and `docs/src/install/raspberry_pi.md`)
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
///
/// `Clone` is derived so a concurrent pipeline stage can hand each worker its
/// own owned copy (both fields are cheap-to-clone owned values), letting the
/// worker closures be `'static` for the shared concurrency runner.
#[derive(Clone)]
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

    /// Absolute path to the directory cargo writes build artifacts into.
    ///
    /// This is **not** unconditionally `workspace_root/target`: cargo honours
    /// `$CARGO_TARGET_DIR` (CI points it at a runner-local cache outside the
    /// checkout so a multi-GB `target/` survives `actions/checkout` wiping the
    /// workspace). Resolving artifact paths against the same directory cargo
    /// actually built into is the only way a post-build step (`test --qemu`,
    /// `test --wasm`) can find the binary it just asked cargo to produce.
    #[must_use]
    pub fn target_dir(&self) -> PathBuf {
        resolve_target_dir(&self.workspace_root, env::var_os("CARGO_TARGET_DIR"))
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

/// Resolve cargo's target directory from the workspace root and the value of
/// `$CARGO_TARGET_DIR` (passed in so this stays a pure, testable function).
///
/// Mirrors cargo's own precedence for this workspace: an absolute
/// `CARGO_TARGET_DIR` is used verbatim; a relative one is resolved against the
/// workspace root (where every `cargo xtask` subprocess sets its current
/// directory); when it is unset, the default is `workspace_root/target`. The
/// workspace pins no `build.target-dir` in `.cargo/config.toml`, so the
/// environment variable is the only override in play.
fn resolve_target_dir(workspace_root: &Path, cargo_target_dir: Option<OsString>) -> PathBuf {
    match cargo_target_dir {
        Some(dir) if !dir.is_empty() => workspace_root.join(dir),
        _ => workspace_root.join("target"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dir_defaults_to_workspace_target_when_unset() {
        let root = Path::new("/ws");
        assert_eq!(resolve_target_dir(root, None), root.join("target"));
    }

    #[test]
    fn target_dir_defaults_when_env_is_empty() {
        let root = Path::new("/ws");
        assert_eq!(
            resolve_target_dir(root, Some(OsString::from(""))),
            root.join("target")
        );
    }

    #[test]
    fn target_dir_honours_absolute_override() {
        let root = Path::new("/ws");
        let abs = OsString::from("/var/lib/actions-runner/rustos-cache/target");
        assert_eq!(
            resolve_target_dir(root, Some(abs)),
            Path::new("/var/lib/actions-runner/rustos-cache/target")
        );
    }

    #[test]
    fn target_dir_resolves_relative_override_against_workspace_root() {
        let root = Path::new("/ws");
        assert_eq!(
            resolve_target_dir(root, Some(OsString::from("build/out"))),
            root.join("build/out")
        );
    }
}
