//! Build orchestration for the TAIRiX workspace.
//!
//! `cargo xtask <command>` is the only sanctioned way to build, test, lint,
//! document, and audit TAIRiX. Driving every check through one entry point
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
//!   `tairix-kernel` build script orphans: it compiles the embedded userland
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
//! - `clippy`       — `-D warnings` for the host
//!   (`--workspace --all-targets`) **and once per Tier-1 target**: the three
//!   freestanding triples over the cross-compiled product tree, each QEMU
//!   triple over its guests (which lints the payload programs they embed), and
//!   `wasm32` over the browser verticals. A host-only pass lints none of the
//!   `freestanding` bodies that actually ship — see `commands::target_clippy`
//! - `fmt`          — `cargo fmt --all -- --check` (pass `--fix` to apply)
//! - `docs-check`   — rustdoc (deny warnings) + mdBook build + link check
//! - `abi-check`    — verifies the generated kernel syscall table matches
//!   its source of truth in `lib/abi`
//! - `c-header`     — generates (`--write`) or verifies the C ABI
//!   development header in `include/` from the `lib/abi` source of truth
//! - `font-atlas`   — generates (`--write`) or verifies the system font
//!   glyph atlas in `lib/font/src/` from the committed faces in
//!   `lib/font/assets/`
//! - `devids`       — verifies the compact PCI/USB ID-database tables (each
//!   inside its consuming command bundle's `Resources/`) against the vetted
//!   snapshots in
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
//! - `charter-cite` — reject a comment or package description that cites a
//!   charter section number in place of the reason it states; a reference to
//!   another document (a plan, a `docs/` page, an RFC, a hardware manual)
//!   passes when that document is named beside the section number
//! - `miri`         — interpret the crates whose safety rests on a
//!   hand-written `unsafe` core (`lib/collections`, `lib/hash`) under Miri,
//!   the undefined-behaviour oracle a test suite cannot be: it checks that a
//!   raw pointer stayed in bounds, that a slot was initialised before it was
//!   read, and that two `&mut` never aliased
//! - `model-check` — exhaustively model-check the Silver capability +
//!   IPC state machine (an in-tree explicit-state checker; the TLA+
//!   equivalent), failing closed on any invariant counterexample
//! - `bench`        — host microbenchmarks for the per-pixel desktop paths:
//!   `Surface::blit`, rounded-rect fill, `box_blur`/`frost_region`,
//!   `resample`, the scan-out channel encode, and the window manager's
//!   whole-frame composite over a representative window stack. Reports ns/px
//!   and ns/frame for a `--iters N` / `--rounds N` budget, one family at a
//!   time with `--filter <substring>`. Wall-clock timings are load-dependent,
//!   so this is evidence for a completion report and is deliberately **not**
//!   a `ci` gate; CI may run it only as a smoke check that every family still
//!   produces a number
//! - `ci`           — the full pipeline a PR must pass, ordered cheapest-first
//!   so a failing PR fails fast: `fmt --check`, then the concurrent
//!   static-gate group (`deps-check`, `cfg-check`, `help-lint`,
//!   `spec-review`, `charter-cite`, `supply-chain`, `abi-check`, `c-header`,
//!   `font-atlas`, `devids`, `model-check` —
//!   all read-only, non-compiling checks run at once so their costs overlap),
//!   then `docs-check` (rustdoc/link failures are the common first trip and
//!   need only a doc build, so they run ahead of the compile-heavy stages),
//!   `clippy`, `test` (run 20× on a GitHub Actions runner to catch flaky
//!   tests; once locally so a pre-push `ci` is not punishingly slow),
//!   `cargo deny check`, `fuzz --quick`, `proptest --quick`, `miri`,
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
use std::process::{Child, Command, ExitCode};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

mod commands;
mod floor;

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

    /// Runs `cmd` inheriting stdio, under [`DEFAULT_COMMAND_TIMEOUT`].
    ///
    /// This is the budget every step gets unless the phase is known ahead of
    /// time to legitimately outrun it (see [`run_with_timeout`]).
    ///
    /// Returns an error describing the failure if the child cannot be
    /// spawned, exits with a non-zero status, or is killed for exceeding its
    /// budget.
    ///
    /// [`run_with_timeout`]: Context::run_with_timeout
    pub fn run(&self, label: &str, cmd: Command) -> Result<(), String> {
        self.run_with_timeout(label, cmd, DEFAULT_COMMAND_TIMEOUT)
    }

    /// Runs `cmd` inheriting stdio, killing it if it is still running after
    /// `budget` (widened upward by `$TAIRIX_XTASK_TIMEOUT_SECS`, see
    /// [`effective_timeout`]).
    ///
    /// This is the one place `xtask` spawns and waits for an external
    /// command; every pipeline step reaches an external process through
    /// here (directly via [`run`](Context::run), or with a longer budget for
    /// a phase that is legitimately long — the QEMU integration-test matrix
    /// build, the image gate's cross-compiles). A hang in any of them is
    /// exactly the failure this exists to bound: without it, a single stuck
    /// `cargo test` invocation stalls `cargo xtask ci` forever, and an
    /// operator has to notice and kill it by hand.
    ///
    /// `std::process::Child` has no timed wait, so the child's blocking
    /// `wait()` runs on a dedicated thread while this thread races it
    /// against the deadline with `Receiver::recv_timeout` — no polling loop,
    /// no sleeping and re-checking `try_wait()`.
    ///
    /// A `cargo` invocation is rarely a single process: it forks `rustc`,
    /// the built test binary, or (for `--qemu`) a QEMU guest as
    /// grandchildren `Child::kill` cannot reach, so the child is spawned in
    /// its own process group and a timeout signals the whole group (see
    /// [`spawn_in_own_group`] and [`await_within`]).
    ///
    /// A timeout is always reported as an error naming the step, the budget,
    /// and that it was killed; it is never retried and never folds into a
    /// passing result.
    ///
    /// [`effective_timeout`]: effective_timeout
    /// [`spawn_in_own_group`]: spawn_in_own_group
    /// [`await_within`]: await_within
    pub fn run_with_timeout(
        &self,
        label: &str,
        mut cmd: Command,
        budget: Duration,
    ) -> Result<(), String> {
        let budget = effective_timeout(budget)?;
        eprintln!("xtask: [{label}] {cmd:?} (timeout {budget:?})");
        let mut child = spawn_in_own_group(&mut cmd)
            .map_err(|err| format!("{label} could not be spawned: {err}"))?;
        let pid = child.id();
        let started = Instant::now();
        let status = await_within(label, pid, budget, move || child.wait())?;
        // Every stage reports its wall clock in the same shape the concurrent
        // job runner uses, so one grep over a pipeline log profiles the whole
        // run. Without it there is no evidence for which phase to make faster.
        eprintln!(
            "xtask: [{label}] {} in {:?}",
            if status.success() { "done" } else { "FAILED" },
            started.elapsed()
        );
        if status.success() {
            Ok(())
        } else {
            Err(format!("{label} failed with {status}"))
        }
    }
}

/// Spawns `cmd` as the leader of its own process group.
///
/// A `cargo` invocation is rarely a single process: it forks `rustc`, the
/// built test binary, or a QEMU guest as grandchildren that `Child::kill`
/// cannot reach. Making the child a group leader (`process_group(0)`, so the
/// group id equals its own pid) is what lets a timeout later signal every
/// descendant at once instead of only the direct child.
pub(crate) fn spawn_in_own_group(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// Races `wait` against `budget`, killing the process group led by `pid` when
/// the budget expires first.
///
/// `std::process::Child` has no timed wait, so the blocking wait runs on a
/// dedicated thread while this thread races it against the deadline with
/// `Receiver::recv_timeout` — no polling loop, no sleeping and re-checking
/// `try_wait()`.
///
/// Generic over what the wait yields so the one implementation serves both a
/// step that inherits the terminal (waiting for an exit status) and one whose
/// output is captured for orderly printing (waiting for the output too);
/// a second copy of this race would be a second thing to get wrong.
pub(crate) fn await_within<T: Send + 'static>(
    label: &str,
    pid: u32,
    budget: Duration,
    wait: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, String> {
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = thread::Builder::new().spawn(move || {
        // The receiver may already have moved on past a timeout; a send with
        // no one listening is simply discarded.
        let _ = done_tx.send(wait());
    });
    let waiter = match waiter {
        Ok(handle) => handle,
        Err(err) => {
            kill_process_tree(pid);
            return Err(format!(
                "{label}: could not start the wait thread ({err}); \
                 pid {pid} was killed"
            ));
        }
    };

    match done_rx.recv_timeout(budget) {
        Ok(Ok(value)) => {
            let _ = waiter.join();
            Ok(value)
        }
        Ok(Err(err)) => {
            let _ = waiter.join();
            Err(format!("{label} could not be awaited: {err}"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = waiter.join();
            Err(format!(
                "{label}: the wait thread ended without reporting a status"
            ))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_process_tree(pid);
            // A further bounded wait for the background thread to reap the
            // now-killed process, so the process table is clean by the time
            // this call returns. A process stuck in an uninterruptible kernel
            // sleep would block the underlying wait regardless of what we do
            // here, so this stays bounded rather than joining unconditionally.
            let _ = done_rx.recv_timeout(KILL_REAP_GRACE);
            Err(format!(
                "{label} exceeded its {budget:?} timeout and was killed \
                 (SIGTERM, then SIGKILL, to its process group)"
            ))
        }
    }
}

/// Wall-clock budget [`Context::run`] grants an ordinary step: a `cargo`
/// invocation that compiles and/or tests some slice of the workspace once,
/// incrementally, on the host.
///
/// A capacity like this must scale with the machine, not be a number picked
/// for whoever happened to write it, so it is layered rather than frozen:
/// this is the floor every ordinary step gets, `run_with_timeout` lets a
/// step that is known ahead of time to need more ask for it explicitly (see
/// [`LONG_BUILD_COMMAND_TIMEOUT`]), and `$TAIRIX_XTASK_TIMEOUT_SECS` lets an
/// operator on a slower machine raise *every* budget, including the longer
/// ones, without editing either constant. Forty-five minutes comfortably
/// covers an incremental `cargo test --workspace --all-targets` pass while
/// still catching a genuinely hung process well inside a CI job's own outer
/// deadline.
pub(crate) const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_mins(45);

/// Wall-clock budget for a step that legitimately outruns
/// [`DEFAULT_COMMAND_TIMEOUT`]: cross-compiling a bare-metal or PIE target
/// from a clean `target/` rebuilds `core`/`alloc`/`compiler_builtins` from
/// source (`-Z build-std`) or links many packages in one `cargo build`
/// (the QEMU integration-test matrix), either of which can legitimately run
/// longer than an incremental host compile/test pass. Used explicitly by the
/// image gate's kernel and driver/app cross-compiles and by the QEMU matrix
/// build, never by default.
pub(crate) const LONG_BUILD_COMMAND_TIMEOUT: Duration = Duration::from_hours(2);

/// Wall-clock deadline for a child that is *meant* to run for `budget`: a
/// soaking fuzz harness, property model, or filesystem soak.
///
/// Such a child is doing exactly what it was asked while its budget runs, so
/// its deadline is that budget plus an ordinary step's allowance — far more
/// than compiling and starting one test binary needs, and thus reached only by
/// a child that has genuinely stopped making progress. Handing one
/// [`DEFAULT_COMMAND_TIMEOUT`] instead kills every soak whose budget outlasts
/// it, so the one definition lives here and every soak orchestrator uses it.
pub(crate) fn soak_deadline(budget: Duration) -> Duration {
    budget.saturating_add(DEFAULT_COMMAND_TIMEOUT)
}

/// Environment variable that raises every [`Context::run`] /
/// [`Context::run_with_timeout`] budget for a slower machine, without
/// editing either constant above. Unset by default; see
/// [`effective_timeout`].
const TIMEOUT_ENV_VAR: &str = "TAIRIX_XTASK_TIMEOUT_SECS";

/// How long the process group is given to exit on `SIGTERM` before
/// [`kill_process_tree`] escalates to `SIGKILL`. Short, because a timed-out
/// command has already failed; this only gives a well-behaved child (e.g.
/// QEMU flushing a disk image) a moment to exit cleanly rather than being
/// hard-killed outright.
const SIGTERM_GRACE: Duration = Duration::from_secs(2);

/// How long [`Context::run_with_timeout`] additionally waits, after killing
/// a timed-out process group, for the background wait thread to reap it.
/// Bounded for the same reason the outer budget is: a process wedged in an
/// uninterruptible kernel sleep would block the underlying `wait()` for as
/// long as we let it, so this cannot be an unconditional join either.
const KILL_REAP_GRACE: Duration = Duration::from_secs(5);

/// Widens `requested` upward to `$TAIRIX_XTASK_TIMEOUT_SECS` when that
/// variable is set, and leaves it untouched otherwise.
///
/// The override only ever raises a budget, never lowers one: a step that
/// asked for [`LONG_BUILD_COMMAND_TIMEOUT`] already knows it needs more than
/// the default, and an operator's "my machine is slow" knob must not undo
/// that. A set-but-unparsable value fails closed — returning an error rather
/// than silently keeping the built-in budget — because that silence would
/// hide a typo behind the exact kind of unbounded hang this mechanism
/// exists to prevent.
fn effective_timeout(requested: Duration) -> Result<Duration, String> {
    match env::var(TIMEOUT_ENV_VAR) {
        Ok(raw) => Ok(requested.max(parse_timeout_env_override(&raw)?)),
        Err(_) => Ok(requested),
    }
}

/// Parses a `$TAIRIX_XTASK_TIMEOUT_SECS` value into a duration.
///
/// A pure function so the parsing (and its fail-closed rejection of a
/// malformed value) is unit-testable without touching real process
/// environment state.
fn parse_timeout_env_override(raw: &str) -> Result<Duration, String> {
    let secs: u64 = raw.trim().parse().map_err(|_| {
        format!("{TIMEOUT_ENV_VAR}={raw:?} is not a positive integer number of seconds")
    })?;
    if secs == 0 {
        return Err(format!("{TIMEOUT_ENV_VAR}=0 is not a valid timeout"));
    }
    Ok(Duration::from_secs(secs))
}

/// Kills the process group led by `pid`: `SIGTERM`, a short grace period,
/// then `SIGKILL`.
///
/// The polite signal comes first so a step that installs a handler can tidy
/// up (a QEMU guest releasing its display, a test removing a scratch file),
/// and the unconditional one follows so a step that ignores or blocks
/// `SIGTERM` still cannot outlive its budget. The group having already
/// exited on the first signal is the expected outcome, not a failure.
#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    send_signal_to_group(pid, libc::SIGTERM);
    thread::sleep(SIGTERM_GRACE);
    send_signal_to_group(pid, libc::SIGKILL);
}

/// `xtask`'s process-management code targets Unix hosts only (as does, for
/// example, the existing Unix-domain-socket link peer in
/// `commands/netpeer.rs`); there is no non-Unix process-group primitive to
/// reach a grandchild through here.
#[cfg(not(unix))]
fn kill_process_tree(_pid: u32) {}

/// Sends `signal` to the process group led by `leader`.
///
/// [`Context::run_with_timeout`] spawns the child with `process_group(0)`,
/// which makes the child its own group leader, so its pid is also its
/// process-group id and every descendant it spawns inherits that group.
/// Signalling the group is therefore what reaches the grandchildren — the
/// rustc, test-binary and QEMU processes a `cargo` step really consists of.
///
/// This goes through `killpg` rather than the `kill` command because the
/// command is not dependable for this: measured on a stock Linux host,
/// procps-ng's `kill -s TERM -<pgid>` ignores the negated-pid group form and
/// still exits zero, which would leave the runaway processes running while
/// the pipeline reported them killed. A silent failure in the one mechanism
/// meant to stop a runaway build is worse than no mechanism at all.
///
/// Best-effort by design: the group having already exited on an earlier
/// signal is the success case, not an error worth reporting.
#[cfg(unix)]
fn send_signal_to_group(leader: u32, signal: i32) {
    let Ok(group) = libc::pid_t::try_from(leader) else {
        return;
    };
    // SAFETY: `killpg` is a plain signal-delivery syscall with no memory
    // operands, so the only soundness obligation is passing well-formed
    // scalars. `group` is this process's own just-spawned child's pid,
    // converted without truncation, and that child was spawned as its own
    // group leader, so it names that group and no other; `signal` is one of
    // the two constants its only caller passes. A group that has already
    // exited answers `ESRCH`, which needs no handling here.
    unsafe {
        libc::killpg(group, signal);
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
        let abs = OsString::from("/var/lib/actions-runner/tairix-cache/target");
        assert_eq!(
            resolve_target_dir(root, Some(abs)),
            Path::new("/var/lib/actions-runner/tairix-cache/target")
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

    /// A `Context` whose fields the timeout machinery never reads: it only
    /// spawns and waits on the `Command` it is handed.
    fn test_context() -> Context {
        Context {
            workspace_root: PathBuf::from("."),
            cargo: OsString::from("cargo"),
        }
    }

    #[test]
    fn run_with_timeout_succeeds_within_budget() {
        let ctx = test_context();
        let cmd = Command::new("true");
        // Five seconds is generous relative to an instantly-exiting `true`,
        // so this cannot fail merely because the host happens to be busy.
        ctx.run_with_timeout("unit-test-ok", cmd, Duration::from_secs(5))
            .expect("a command finishing well inside its budget must succeed");
    }

    #[test]
    fn run_with_timeout_reports_failure_from_the_child_itself() {
        let ctx = test_context();
        let cmd = Command::new("false");
        let err = ctx
            .run_with_timeout("unit-test-nonzero-exit", cmd, Duration::from_secs(5))
            .expect_err("a non-zero exit must surface as an error");
        assert!(err.contains("failed"), "unexpected error: {err}");
    }

    #[test]
    fn run_with_timeout_kills_and_reports_a_hung_command() {
        let ctx = test_context();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5"]);
        let started = std::time::Instant::now();
        // A 200 ms budget against a 5 s sleep leaves an enormous margin, so a
        // busy host cannot turn this into a false pass.
        let err = ctx
            .run_with_timeout("unit-test-hang", cmd, Duration::from_millis(200))
            .expect_err("a command that outlives its budget must fail, never pass");
        // The report must say why it failed and that the command was not
        // left running, and must name the step so an operator reading a
        // pipeline log knows which one hung.
        assert!(err.contains("timeout"), "unexpected error: {err}");
        assert!(err.contains("killed"), "unexpected error: {err}");
        assert!(err.contains("unit-test-hang"), "unexpected error: {err}");
        // Bounded well under the 5 s sleep: the 200 ms budget plus the fixed
        // SIGTERM grace and the bounded post-kill reap wait, with headroom.
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "run_with_timeout should return promptly once the process group \
             is killed, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn run_with_timeout_kills_the_whole_process_group_not_just_the_direct_child() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let ctx = test_context();
        let marker = std::env::temp_dir().join(format!(
            "xtask-timeout-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        // The backgrounded `sleep` is a grandchild of the `sh` this spawns:
        // it is not the direct child `run_with_timeout` sees, so it can only
        // be dead afterwards if the whole process *group* was signalled.
        //
        // Its duration deliberately dwarfs this test's own runtime. A short
        // sleep would end of its own accord while the assertions ran, so the
        // test would pass just as happily against a kill that did nothing —
        // which is exactly the silent failure this test exists to catch.
        let script = format!("sleep 600 & echo $! > {} ; wait", marker.display());
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script]);
        let started = std::time::Instant::now();
        let result = ctx.run_with_timeout("unit-test-grandchild", cmd, Duration::from_millis(200));
        assert!(
            result.is_err(),
            "the hung command must be reported as a failure"
        );
        // Returning at all within a few seconds already proves the kill
        // landed: without it this call would wait out the full ten minutes.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "run_with_timeout must return once the group is killed, took {:?}",
            started.elapsed()
        );

        let grandchild_pid: u32 = std::fs::read_to_string(&marker)
            .expect("the backgrounded grandchild should have recorded its pid before the kill")
            .trim()
            .parse()
            .expect("the recorded pid should be a plain integer");
        let _ = std::fs::remove_file(&marker);

        // `kill -0` only probes for the process's existence; its diagnostic
        // on the expected "already dead" answer is discarded so a passing
        // run leaves no alarming text in the test log.
        let still_alive = Command::new("kill")
            .args(["-0", &grandchild_pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("invoking `kill -0` should not itself fail to spawn")
            .success();
        assert!(
            !still_alive,
            "grandchild pid {grandchild_pid} should have been killed along with the rest \
             of the process group, not left running"
        );
    }

    #[test]
    fn soak_deadline_outlasts_the_budget_it_covers() {
        // A soak child still running near the end of its budget is doing the
        // work it was asked to; a deadline at or below the budget kills it.
        for budget in [
            Duration::from_secs(5),
            Duration::from_hours(7),
            Duration::from_hours(24),
        ] {
            let deadline = soak_deadline(budget);
            assert!(
                deadline > budget,
                "deadline {deadline:?} must outlast the budget {budget:?}"
            );
            assert_eq!(deadline, budget + DEFAULT_COMMAND_TIMEOUT);
        }
    }

    #[test]
    fn soak_deadline_saturates_instead_of_overflowing() {
        // An operator-supplied budget must never turn a deadline into a panic.
        assert_eq!(soak_deadline(Duration::MAX), Duration::MAX);
    }

    #[test]
    fn timeout_env_override_parses_a_positive_integer() {
        assert_eq!(
            parse_timeout_env_override("120").expect("120 is a valid override"),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn timeout_env_override_rejects_zero() {
        assert!(
            parse_timeout_env_override("0").is_err(),
            "a zero-second override must fail closed, not disable the timeout"
        );
    }

    #[test]
    fn timeout_env_override_rejects_malformed_values() {
        for bad in ["", "soon", "-5", "5s", "1.5"] {
            assert!(
                parse_timeout_env_override(bad).is_err(),
                "{bad:?} should be rejected, not silently ignored"
            );
        }
    }

    #[test]
    fn effective_timeout_passes_through_when_the_override_is_unset() {
        // This reads real process environment state, so it only asserts
        // when the override is genuinely absent; an operator who happens to
        // have it set is exercising a different, already-covered path
        // (`timeout_env_override_parses_a_positive_integer`) rather than
        // making this test flaky.
        if std::env::var(TIMEOUT_ENV_VAR).is_ok() {
            return;
        }
        assert_eq!(
            effective_timeout(Duration::from_secs(30)).expect("no override set"),
            Duration::from_secs(30)
        );
    }
}
