//! Subcommand implementations for `cargo xtask`.
//!
//! Each variant of [`Command`] corresponds to a single, named developer
//! workflow. Adding a new pipeline step means adding a new variant here —
//! never appending hidden behaviour to `ci`.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::Context;

mod abi_check;
mod c_header;
mod cfg_check;
mod deps_check;
mod fssoak;
mod fuzz;
mod linkcheck;
mod model_check;
mod parallel;
mod proptest;
mod qemu_tests;
mod sbom;
mod seed;
mod spec_review;
mod supply_chain;
mod wasm_tests;

/// One sanctioned developer workflow.
#[derive(Copy, Clone, Debug)]
pub enum Command {
    Build,
    Test,
    Clippy,
    Fmt,
    DocsCheck,
    AbiCheck,
    CHeader,
    DepsCheck,
    CfgCheck,
    Coverage,
    Sbom,
    SupplyChain,
    Fuzz,
    Proptest,
    FsSoak,
    ModelCheck,
    SpecReview,
    Ci,
    Image,
}

impl Command {
    /// The full set of subcommands, in the order presented to users.
    pub const ALL: &'static [Command] = &[
        Command::Build,
        Command::Test,
        Command::Clippy,
        Command::Fmt,
        Command::DocsCheck,
        Command::AbiCheck,
        Command::CHeader,
        Command::DepsCheck,
        Command::CfgCheck,
        Command::Coverage,
        Command::Sbom,
        Command::SupplyChain,
        Command::Fuzz,
        Command::Proptest,
        Command::FsSoak,
        Command::ModelCheck,
        Command::SpecReview,
        Command::Ci,
        Command::Image,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "build" => Command::Build,
            "test" => Command::Test,
            "clippy" => Command::Clippy,
            "fmt" => Command::Fmt,
            "docs-check" => Command::DocsCheck,
            "abi-check" => Command::AbiCheck,
            "c-header" => Command::CHeader,
            "deps-check" => Command::DepsCheck,
            "cfg-check" => Command::CfgCheck,
            "coverage" => Command::Coverage,
            "sbom" => Command::Sbom,
            "supply-chain" => Command::SupplyChain,
            "fuzz" => Command::Fuzz,
            "proptest" => Command::Proptest,
            "fssoak" => Command::FsSoak,
            "model-check" => Command::ModelCheck,
            "spec-review" => Command::SpecReview,
            "ci" => Command::Ci,
            "image" => Command::Image,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Command::Build => "build",
            Command::Test => "test",
            Command::Clippy => "clippy",
            Command::Fmt => "fmt",
            Command::DocsCheck => "docs-check",
            Command::AbiCheck => "abi-check",
            Command::CHeader => "c-header",
            Command::DepsCheck => "deps-check",
            Command::CfgCheck => "cfg-check",
            Command::Coverage => "coverage",
            Command::Sbom => "sbom",
            Command::SupplyChain => "supply-chain",
            Command::Fuzz => "fuzz",
            Command::Proptest => "proptest",
            Command::FsSoak => "fssoak",
            Command::ModelCheck => "model-check",
            Command::SpecReview => "spec-review",
            Command::Ci => "ci",
            Command::Image => "image",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Command::Build => "Compile every workspace crate for the host target.",
            Command::Test => "Run host-side unit and integration tests.",
            Command::Clippy => "Run clippy across the workspace with warnings denied.",
            Command::Fmt => "Check formatting (`--fix` to apply).",
            Command::DocsCheck => "Build rustdoc and the mdBook with link checking.",
            Command::AbiCheck => "Verify generated ABI artefacts match their source of truth.",
            Command::CHeader => {
                "Generate/verify the C ABI development header (`--write` to regenerate)."
            }
            Command::DepsCheck => "Enforce the §17.4 modularity dependency graph.",
            Command::CfgCheck => "Reject target-conditional compilation outside the arch ports.",
            Command::Coverage => "Produce a host-side coverage report via cargo-llvm-cov.",
            Command::Sbom => "Emit a CycloneDX SBOM from Cargo.lock (§19.3).",
            Command::SupplyChain => {
                "Verify source-hash pins against Cargo.lock and the advisory SLA (§19.3)."
            }
            Command::Fuzz => "Drive the in-tree fuzz harnesses for a wall-clock budget (§19.6).",
            Command::Proptest => {
                "Drive the §19.7 stateful capability models for a wall-clock budget."
            }
            Command::FsSoak => {
                "Soak rustfs/ext4/fat32 on a ≥1 GiB RAM volume for a wall-clock budget."
            }
            Command::ModelCheck => {
                "Exhaustively model-check the §19.7 Silver capability + IPC state machine."
            }
            Command::SpecReview => "Reject unreviewed AI draft markers in source (§19.7).",
            Command::Ci => "Run the full pipeline a pull request must pass.",
            Command::Image => "Build platform images via tools/mkimage.",
        }
    }

    pub fn run(self, ctx: &Context, args: &[OsString]) -> Result<(), String> {
        match self {
            Command::Build => run_build(ctx, args),
            Command::Test => run_test(ctx, args),
            Command::Clippy => run_clippy(ctx, args),
            Command::Fmt => run_fmt(ctx, args),
            Command::DocsCheck => run_docs_check(ctx, args),
            Command::AbiCheck => run_abi_check(ctx, args),
            Command::CHeader => run_c_header(ctx, args),
            Command::DepsCheck => run_deps_check(ctx),
            Command::CfgCheck => run_cfg_check(ctx),
            Command::Coverage => run_coverage(ctx, args),
            Command::Sbom => run_sbom(ctx, args),
            Command::SupplyChain => run_supply_chain(ctx, args),
            Command::Fuzz => run_fuzz(ctx, args),
            Command::Proptest => run_proptest(ctx, args),
            Command::FsSoak => run_fssoak(ctx, args),
            Command::ModelCheck => run_model_check(args),
            Command::SpecReview => run_spec_review(ctx),
            Command::Ci => run_ci(ctx),
            Command::Image => run_image(ctx, args),
        }
    }
}

fn run_build(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // `--headless` builds the first-class headless configuration required
    // by AGENTS.md §17.3 / §17.5: every `userland/gui/*` crate is excluded
    // from the image so the system must remain buildable without the
    // desktop. The flag is consumed here; everything else is forwarded.
    let mut headless = false;
    let mut forward = Vec::with_capacity(args.len());
    for a in args {
        if a == "--headless" {
            headless = true;
        } else {
            forward.push(a.clone());
        }
    }

    let mut cmd = ctx.cargo();
    cmd.args(["build", "--workspace", "--all-targets", "--locked"]);
    if headless {
        for gui in GUI_CRATES {
            cmd.arg("--exclude");
            cmd.arg(gui);
        }
    }
    cmd.args(&forward);
    ctx.run(
        if headless {
            "build --headless"
        } else {
            "build"
        },
        cmd,
    )
}

/// The `userland/gui/*` crates excluded from the headless image (§17.3).
const GUI_CRATES: &[&str] = &["rustos-wm", "rustos-taskbar"];

/// The environment variable GitHub Actions sets to `"true"` on every runner.
///
/// It is documented as always present (and equal to `"true"`) inside a
/// GitHub Actions job, and absent on a developer's machine, so it is the
/// canonical, forge-set signal that we are running in CI rather than
/// locally.
const GITHUB_ACTIONS_ENV: &str = "GITHUB_ACTIONS";

/// Whether we are executing inside a GitHub Actions job.
///
/// Reads the runner-set [`GITHUB_ACTIONS_ENV`] signal. The QEMU matrix uses
/// it only to scale per-test timeouts for the slower shared runners; the
/// test count itself is no longer CI-dependent (`ci` runs the matrix once,
/// §7).
fn in_github_actions() -> bool {
    std::env::var_os(GITHUB_ACTIONS_ENV).is_some_and(|v| v == "true")
}

/// Default wall-clock budget for `cargo xtask test --soak`: 24 h.
///
/// Matches the fuzz/proptest soak floor (§19.6/§19.7). The nightly `soak`
/// workflow repeats the whole test matrix for this long via
/// `tools/ci/soak.sh` so a flake too rare to surface in the per-PR
/// single-pass run still gets a full night of exposure. Flake-hunting
/// repetition lives in the time-limited GitHub soaks, not in `ci`: a
/// developer-machine and per-PR `ci` run executes the matrix exactly once.
pub const TEST_SOAK_SECS: u64 = 24 * 60 * 60;

/// How many times the test matrix repeats.
///
/// `ci` and `--count N` drive a fixed number of passes; the nightly soak
/// drives a wall-clock budget instead. Either way the *whole* matrix
/// (host, then opt-in QEMU and wasm) is one pass, so a duration budget is
/// not multiplied across stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunBudget {
    /// Run the matrix exactly this many times (always ≥ 1).
    Count(u32),
    /// Repeat the matrix until this wall-clock budget elapses.
    Duration(Duration),
}

impl RunBudget {
    /// Run `body` once per matrix pass, passing the 1-based pass number.
    ///
    /// `Count(n)` runs exactly `n` passes (clamped to ≥ 1). `Duration`
    /// always runs at least one pass and keeps going until the budget
    /// elapses; the clock is checked *after* each pass, so a pass already
    /// in flight always finishes and the suite is never cut off mid-run.
    fn for_each<F>(self, mut body: F) -> Result<(), String>
    where
        F: FnMut(u64) -> Result<(), String>,
    {
        match self {
            RunBudget::Count(n) => {
                for pass in 1..=u64::from(n.max(1)) {
                    body(pass)?;
                }
                Ok(())
            }
            RunBudget::Duration(budget) => {
                let start = Instant::now();
                let mut pass = 0u64;
                loop {
                    pass += 1;
                    body(pass)?;
                    if start.elapsed() >= budget {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Whether more than one pass is expected (gates per-pass logging).
    fn is_repeated(self) -> bool {
        !matches!(self, RunBudget::Count(1))
    }

    /// Human-readable budget for the `[test]` banner.
    fn describe(self) -> String {
        match self {
            RunBudget::Count(n) => format!("{n} pass(es)"),
            RunBudget::Duration(d) => format!("soaking for {}s", d.as_secs()),
        }
    }
}

/// Parsed options for the `test` subcommand.
#[derive(Debug)]
struct TestOptions {
    /// How many times to repeat the whole matrix (host, QEMU, wasm).
    budget: RunBudget,
    /// Run the bare-metal QEMU integration matrix (`--qemu`).
    run_qemu: bool,
    /// Run the wasm32 browser-headless vertical (`--wasm`).
    run_wasm: bool,
    /// Remaining arguments forwarded verbatim to `cargo test`.
    forward: Vec<OsString>,
}

/// Parse the `test` subcommand arguments.
///
/// Recognises `--qemu`, `--wasm`, `--count N` (alias `--iterations N`),
/// `--soak`, and `--secs N`; everything else is forwarded verbatim to
/// `cargo test`. `--count` rejects a missing, non-numeric, or zero value
/// rather than silently defaulting, so a typo can never quietly collapse
/// the matrix to a single run. A fixed count and a wall-clock budget are
/// mutually exclusive: combining `--count` with `--soak`/`--secs` is an
/// error rather than a silent precedence rule.
fn parse_test_options(args: &[OsString]) -> Result<TestOptions, String> {
    let mut forward = Vec::with_capacity(args.len());
    let mut run_qemu = false;
    let mut run_wasm = false;
    let mut count: Option<u32> = None;
    let mut soak = false;
    let mut secs: Option<u64> = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--qemu" {
            run_qemu = true;
        } else if a == "--wasm" {
            run_wasm = true;
        } else if a == "--soak" {
            soak = true;
        } else if a == "--count" || a == "--iterations" {
            let value = iter.next().ok_or_else(|| {
                format!(
                    "test: `{}` requires a positive integer argument",
                    a.to_string_lossy()
                )
            })?;
            count = Some(parse_iteration_count(value)?);
        } else if a == "--secs" {
            let value = iter
                .next()
                .ok_or_else(|| "test: `--secs` requires an integer argument".to_string())?;
            secs = Some(parse_secs(value)?);
        } else {
            forward.push(a.clone());
        }
    }

    // A duration budget (`--soak`, optionally tuned by `--secs`) and a fixed
    // pass count are two ways of saying the same thing; allowing both would
    // need an arbitrary precedence rule, so fail closed instead.
    let duration = match (soak, secs) {
        (_, Some(s)) => Some(Duration::from_secs(s)),
        (true, None) => Some(Duration::from_secs(TEST_SOAK_SECS)),
        (false, None) => None,
    };
    if duration.is_some() && count.is_some() {
        return Err(
            "test: `--count`/`--iterations` cannot be combined with `--soak`/`--secs`; \
             choose a fixed pass count or a wall-clock budget"
                .to_string(),
        );
    }
    let budget = match duration {
        Some(d) => RunBudget::Duration(d),
        None => RunBudget::Count(count.unwrap_or(1)),
    };

    Ok(TestOptions {
        budget,
        run_qemu,
        run_wasm,
        forward,
    })
}

/// Parse a `--count`/`--iterations` value: a positive (non-zero) integer.
fn parse_iteration_count(value: &OsString) -> Result<u32, String> {
    let text = value
        .to_str()
        .ok_or_else(|| "test: iteration count is not valid UTF-8".to_string())?;
    let count: u32 = text.parse().map_err(|_| {
        format!("test: invalid iteration count {text:?}; expected a positive integer")
    })?;
    if count == 0 {
        return Err("test: iteration count must be at least 1".to_string());
    }
    Ok(count)
}

/// Parse a `--secs` value: a non-negative number of seconds.
fn parse_secs(value: &OsString) -> Result<u64, String> {
    let text = value
        .to_str()
        .ok_or_else(|| "test: `--secs` value is not valid UTF-8".to_string())?;
    text.parse::<u64>().map_err(|_| {
        format!("test: invalid `--secs` value {text:?}; expected a non-negative integer")
    })
}

fn run_test(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // `--qemu` opts in to the bare-metal QEMU integration tests in
    // `tests/integration/*`. Per AGENTS.md §7 they share the test
    // entry point (`cargo xtask test`) so a single command runs the
    // whole matrix; per the same section we never retry on failure
    // and every run has a strict, finite timeout.
    //
    // `--count N` (alias `--iterations N`) runs the whole matrix N times;
    // it defaults to one, and `ci` runs the matrix exactly once (§7). The
    // flake-hunting repetition lives in the time-limited GitHub soaks:
    // `--soak` (tuned by `--secs N`) repeats the matrix for a wall-clock
    // budget, which the nightly `soak` workflow uses to run the tests for
    // 24 h. `--count N` remains for the orchestrator's own tests and ad-hoc
    // local repeat runs.
    let opts = parse_test_options(args)?;

    // Build the opt-in matrices once, before any repeated passes, so a soak
    // re-runs the binaries rather than rebuilding them each pass. The host
    // `cargo test` invocation builds incrementally on its own.
    if opts.run_qemu {
        qemu_tests::build_all(ctx)?;
    }
    // `--wasm` boots the wasm32 vertical in a headless browser. It is
    // opt-in (like `--qemu`) because it needs node + puppeteer + Chrome;
    // see `commands/wasm_tests.rs`.
    if opts.run_wasm {
        wasm_tests::prepare(ctx)?;
    }

    if opts.budget.is_repeated() {
        eprintln!("xtask: [test] {}", opts.budget.describe());
    }
    // A pass is the *whole* matrix: host, then QEMU, then wasm. Looping here
    // (rather than inside each stage) means a duration budget covers the
    // matrix as a unit instead of being spent in full on each stage.
    opts.budget.for_each(|pass| {
        let mut cmd = ctx.cargo();
        cmd.args(["test", "--workspace", "--all-targets", "--locked"]);
        cmd.args(&opts.forward);
        let label = if opts.budget.is_repeated() {
            format!("test (pass {pass})")
        } else {
            "test".to_string()
        };
        ctx.run(&label, cmd)?;

        if opts.run_qemu {
            qemu_tests::run_once(ctx)?;
        }
        if opts.run_wasm {
            wasm_tests::run_once(ctx)?;
        }
        Ok(())
    })
}

fn run_clippy(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    let mut cmd = ctx.cargo();
    cmd.args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ]);
    cmd.args(args);
    ctx.run("clippy", cmd)
}

fn run_fmt(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    let apply = args.iter().any(|a| a == "--fix" || a == "--apply");
    let mut cmd = ctx.cargo();
    cmd.args(["fmt", "--all"]);
    if !apply {
        cmd.args(["--", "--check"]);
    }
    ctx.run(if apply { "fmt --fix" } else { "fmt --check" }, cmd)
}

fn run_docs_check(ctx: &Context, _args: &[OsString]) -> Result<(), String> {
    // rustdoc with warnings denied — broken intra-doc links fail the build.
    let mut doc = ctx.cargo();
    doc.args([
        "doc",
        "--workspace",
        "--no-deps",
        "--locked",
        "--document-private-items",
    ])
    .env("RUSTDOCFLAGS", "-D warnings");
    ctx.run("docs-check (rustdoc)", doc)?;

    // mdBook build. The book lives in `docs/`.
    if !mdbook_available() {
        return Err(
            "mdbook is not on PATH; install it with `cargo install --locked mdbook`".to_string(),
        );
    }
    let mut book = std::process::Command::new("mdbook");
    book.current_dir(ctx.workspace_root.join("docs"));
    book.args(["build"]);
    ctx.run("docs-check (mdbook)", book)?;

    // In-tree relative-link checker; see `commands/linkcheck.rs` for the
    // rationale for owning this rather than delegating to a preprocessor.
    let book_src = ctx.workspace_root.join("docs/src");
    eprintln!("xtask: [docs-check (linkcheck)] {}", book_src.display());
    linkcheck::run(&book_src)?;
    Ok(())
}

fn run_abi_check(ctx: &Context, _args: &[OsString]) -> Result<(), String> {
    // Stage 2.7: real syscall ABI cross-check. `abi_check::check_sync`
    // enforces both the pair-existence rule (`AGENTS.md` §9) and the
    // SHA-256 hash equality between the kernel-side table and the
    // `lib/abi` source of truth. Its unit tests exercise the desync
    // failure mode against a mutated fixture (see
    // `tools/xtask/src/commands/abi_check.rs`).
    let syscalls = ctx.workspace_root.join(abi_check::DEFAULT_SYSCALLS_PATH);
    let table = ctx.workspace_root.join(abi_check::DEFAULT_TABLE_PATH);
    eprintln!(
        "xtask: [abi-check] {} ↔ {}",
        relative(&ctx.workspace_root, &syscalls),
        relative(&ctx.workspace_root, &table),
    );
    abi_check::check_sync(&ctx.workspace_root, &syscalls, &table)
}

fn run_c_header(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // The C development header (`AGENTS.md` §9) is a generated view of the
    // `lib/abi` source of truth. With no arguments this verifies the
    // committed header is in sync (the `ci` drift guard); `--write`
    // regenerates it, reviewed by diff like the kernel syscall table.
    let mut write = false;
    for arg in args {
        if arg == "--write" {
            write = true;
        } else {
            return Err(format!(
                "c-header: unexpected argument {arg:?}; usage: cargo xtask c-header [--write]"
            ));
        }
    }
    let include_dir = ctx.workspace_root.join(c_header::DEFAULT_INCLUDE_DIR);
    if write {
        eprintln!(
            "xtask: [c-header --write] {}",
            relative(&ctx.workspace_root, &include_dir)
        );
        c_header::write(&ctx.workspace_root, &include_dir)
    } else {
        eprintln!(
            "xtask: [c-header] {}",
            relative(&ctx.workspace_root, &include_dir)
        );
        c_header::check_sync(&ctx.workspace_root, &include_dir)
    }
}

fn run_deps_check(ctx: &Context) -> Result<(), String> {
    // §17.4 / §17.5: walk the workspace dependency graph and reject any
    // layering violation, concrete-scheduler naming outside the sanctioned
    // crates, or a non-GUI crate reaching the optional desktop.
    eprintln!("xtask: [deps-check] {}", ctx.workspace_root.display());
    deps_check::run(&ctx.workspace_root)
}

fn run_cfg_check(ctx: &Context) -> Result<(), String> {
    // §17.2 / §17.5: reject target-conditional compilation outside the
    // architecture ports and the build glue.
    eprintln!("xtask: [cfg-check] {}", ctx.workspace_root.display());
    cfg_check::run(&ctx.workspace_root)
}

fn run_coverage(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // `cargo-llvm-cov` is a cargo subcommand: its binary rejects a bare
    // `--version` and is only reachable as `cargo llvm-cov`. Probe it the
    // same way it is invoked below so the availability check matches reality.
    if !cargo_subcommand_available(ctx, "llvm-cov") {
        return Err(
            "cargo-llvm-cov is not installed; run `cargo install cargo-llvm-cov --locked`"
                .to_string(),
        );
    }
    let mut cmd = ctx.cargo();
    cmd.args(["llvm-cov", "--workspace", "--locked", "--summary-only"]);
    cmd.args(args);
    ctx.run("coverage", cmd)
}

fn run_sbom(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // §19.3: emit a CycloneDX SBOM from the committed `Cargo.lock`. The
    // default is stdout (composes with redirection and signing); an
    // explicit `--output PATH` (or `-o PATH`) writes the document to disk,
    // creating any missing parent directories (e.g. the gitignored
    // `images/`). The generator itself lives in `commands/sbom.rs`.
    let mut output: Option<std::path::PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--output" || arg == "-o" {
            let path = iter
                .next()
                .ok_or_else(|| "sbom: `--output` requires a path argument".to_string())?;
            output = Some(std::path::PathBuf::from(path));
        } else {
            return Err(format!(
                "sbom: unexpected argument {arg:?}; usage: cargo xtask sbom [--output PATH]"
            ));
        }
    }
    sbom::run(&ctx.workspace_root, output.as_deref())
}

fn run_supply_chain(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // §19.3: verify the committed source-hash allow-list against
    // `Cargo.lock` and enforce the advisory SLA. `--write-pins`
    // regenerates the `[[source-pin]]` blocks from the lockfile
    // (reviewed by diff, like the lockfile itself); the default verifies.
    let mut write_pins = false;
    for arg in args {
        if arg == "--write-pins" {
            write_pins = true;
        } else {
            return Err(format!(
                "supply-chain: unexpected argument {arg:?}; usage: \
                 cargo xtask supply-chain [--write-pins]"
            ));
        }
    }
    eprintln!("xtask: [supply-chain] {}", ctx.workspace_root.display());
    supply_chain::run(&ctx.workspace_root, write_pins)
}

fn run_fuzz(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // §19.6: drive the in-tree fuzz harnesses for a wall-clock budget.
    // `--quick` (the default and the `ci` budget) runs each ≥ 60 s;
    // `--soak` runs each ≥ 24 h for the nightly job. The harness set and
    // the budget live in `commands/fuzz.rs`.
    let opts = fuzz::parse(args)?;
    eprintln!("xtask: [fuzz] {}", ctx.workspace_root.display());
    fuzz::run(ctx, &opts)
}

fn run_proptest(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // §19.7 Bronze: drive the stateful capability models for a wall-clock
    // budget. `--quick` (the default and the `ci` budget) runs each ≥ 5 s;
    // `--soak` runs each ≥ 24 h for the nightly job. The model set and the
    // budget live in `commands/proptest.rs`.
    let opts = proptest::parse(args)?;
    eprintln!("xtask: [proptest] {}", ctx.workspace_root.display());
    proptest::run(ctx, &opts)
}

fn run_fssoak(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // `.junie/filesystems.md`: drive the in-RAM filesystem soak for a
    // wall-clock budget. `--quick` runs each filesystem ≥ 5 s; `--soak`
    // runs each ≥ 24 h for the nightly job. The target set and the budget
    // live in `commands/fssoak.rs`; the parallel per-filesystem fan-out is
    // `tools/ci/soak.sh`'s job, not `ci`'s.
    let opts = fssoak::parse(args)?;
    eprintln!("xtask: [fssoak] {}", ctx.workspace_root.display());
    fssoak::run(ctx, &opts)
}

fn run_model_check(args: &[OsString]) -> Result<(), String> {
    // §19.7 Silver: exhaustively model-check the capability + IPC state
    // machine. The model and the explicit-state checker live in
    // `commands/model_check.rs`; the formal narrative is in
    // `docs/src/security/model/capability_ipc.md`. Fails closed on any
    // reachable state or transition that violates an invariant.
    let opts = model_check::parse(args)?;
    eprintln!("xtask: [model-check] exhaustive capability + IPC state machine");
    model_check::run(&opts)
}

fn run_spec_review(ctx: &Context) -> Result<(), String> {
    // §19.7: fail closed if any unreviewed AI-drafted artefact marker
    // reaches the tree. The scanner lives in `commands/spec_review.rs`.
    eprintln!("xtask: [spec-review] {}", ctx.workspace_root.display());
    spec_review::run(&ctx.workspace_root)
}

fn run_ci(ctx: &Context) -> Result<(), String> {
    // The pipeline order is deliberate: cheap and deterministic checks run
    // first so a failing PR fails fast. The test phase opts in to `--qemu`
    // so the Stage-2 QEMU integration tests run as part of every PR per
    // `AGENTS.md` §7; CI hosts therefore need QEMU, `grub-mkrescue`,
    // `xorriso`, and OVMF, all documented under
    // `docs/src/platform/x86_64.md`.
    run_fmt(ctx, &[])?;
    run_clippy(ctx, &[])?;
    // Modularity gates (§17.5) are static, deterministic, and cheap, so
    // they run before the test matrix to fail a non-conforming PR fast.
    run_deps_check(ctx)?;
    run_cfg_check(ctx)?;
    // §7: run the whole test matrix exactly once. `ci` runs each test a
    // single time, on a developer machine and on a CI runner alike; the
    // flake-hunting repetition lives in the time-limited GitHub soaks
    // (`tools/ci/soak.sh`, `cargo xtask test --soak`), never in `ci`. The
    // fuzz and proptest gates below likewise run a single iteration here.
    run_test(ctx, &[OsString::from("--qemu")])?;
    run_docs_check(ctx, &[])?;
    run_deny(ctx)?;
    // §19.3 supply-chain integrity: the source-hash allow-list and the
    // advisory SLA. Runs right after `cargo deny` (which blocks an
    // advisory immediately); this gate caps how long one may be accepted
    // and fails closed when a pin drifts from `Cargo.lock`.
    run_supply_chain(ctx, &[])?;
    // §19.6: the per-PR fuzz gate. Runs each in-tree harness for a single
    // iteration with a fresh, logged seed (a crash, hang, or invariant
    // failure fails the gate, fail-closed). `ci` does not budget the
    // harnesses — the wall-clock soak coverage is the time-limited GitHub
    // soak (`cargo xtask fuzz --soak`, run outside `ci`).
    run_fuzz(ctx, &[OsString::from("--once")])?;
    // §19.7 Bronze: the per-PR stateful-model gate. Runs each capability
    // model for a single iteration with a fresh, logged seed; a
    // counterexample, hang, or invariant failure fails the gate
    // (fail-closed). The wall-clock soak is `cargo xtask proptest --soak`,
    // run outside `ci`.
    run_proptest(ctx, &[OsString::from("--once")])?;
    // §19.7 Silver: exhaustively model-check the capability + IPC state
    // machine on every PR. The check is exhaustive (not budgeted) and fast,
    // so it always runs; a reachable invariant violation fails closed.
    run_model_check(&[])?;
    // §19.7: reject any unreviewed AI-drafted artefact marker that reached
    // the tree. Static and cheap; fails closed.
    run_spec_review(ctx)?;
    // §19.1: re-run `lib/crypto`'s unit tests under release optimisation
    // (`[profile.release]` is `opt-level = 3`). The constant-time
    // comparison guarantee can be broken by the optimiser, so the charter
    // requires the secret-handling tests to pass under `-C opt-level=3`,
    // not only the debug profile the main test phase uses.
    run_crypto_constant_time(ctx)?;
    run_abi_check(ctx, &[])?;
    // §9: the C ABI development header is a generated view of `lib/abi`.
    // Verify the committed copy is in sync so a `lib/abi` change cannot land
    // without regenerating the header non-Rust programs link against.
    run_c_header(ctx, &[])?;
    Ok(())
}

/// §19.1: run `lib/crypto`'s unit tests under the release profile so the
/// constant-time comparison tests are exercised at `-C opt-level=3`. A
/// data-dependent branch introduced by the optimiser would surface here, in
/// the `constant_time` module's full-traversal assertions, rather than at
/// the debug optimisation level the main test phase uses.
fn run_crypto_constant_time(ctx: &Context) -> Result<(), String> {
    let mut cmd = ctx.cargo();
    cmd.args(["test", "--release", "--locked", "-p", "rustos-crypto"]);
    ctx.run("crypto-constant-time (release)", cmd)
}

fn run_deny(ctx: &Context) -> Result<(), String> {
    if !cargo_subcommand_available(ctx, "deny") {
        return Err(
            "cargo-deny is not installed; run `cargo install cargo-deny --locked`".to_string(),
        );
    }
    let mut cmd = ctx.cargo();
    cmd.args(["deny", "--all-features", "check"]);
    ctx.run("deny", cmd)
}

fn run_image(_ctx: &Context, _args: &[OsString]) -> Result<(), String> {
    // Image builders live under `tools/mkimage` and are introduced by
    // Stage 8 of `PLAN.md`. Refusing to silently succeed prevents Stage 0
    // from shipping a no-op that masks the work still to come.
    Err(
        "image: `tools/mkimage` is delivered by Stage 8; no images can be \
         built yet. See PLAN.md."
            .to_string(),
    )
}

fn mdbook_available() -> bool {
    tool_available("mdbook")
}

fn tool_available(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Probe for a cargo subcommand (`cargo <sub>`). Unlike a plain binary, a
/// cargo-subcommand executable expects its subcommand name as the first
/// argument, so it must be reached through `cargo` rather than invoked
/// directly with `--version`.
fn cargo_subcommand_available(ctx: &Context, sub: &str) -> bool {
    ctx.cargo()
        .args([sub, "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{cargo_subcommand_available, parse_test_options, RunBudget, TEST_SOAK_SECS};
    use crate::Context;
    use std::ffi::OsString;
    use std::time::Duration;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// The availability probe must fail closed: an unknown cargo subcommand
    /// is reported absent rather than mistakenly present. This guards the
    /// regression that motivated the probe — checking a cargo-subcommand
    /// binary with a bare `--version` (e.g. `cargo-llvm-cov --version`)
    /// errors out, so the probe routes through `cargo <sub>` instead.
    #[test]
    fn cargo_subcommand_probe_fails_closed_for_unknown_subcommand() {
        let ctx = Context::discover().expect("workspace context");
        assert!(!cargo_subcommand_available(
            &ctx,
            "definitely-not-a-real-cargo-subcommand"
        ));
    }

    #[test]
    fn test_options_default_to_a_single_run() {
        let opts = parse_test_options(&[]).expect("empty args parse");
        assert_eq!(opts.budget, RunBudget::Count(1));
        assert!(!opts.run_qemu);
        assert!(!opts.run_wasm);
        assert!(opts.forward.is_empty());
    }

    #[test]
    fn count_flag_sets_the_iteration_total() {
        let opts = parse_test_options(&argv(&["--qemu", "--count", "100"])).expect("parse");
        assert_eq!(opts.budget, RunBudget::Count(100));
        assert!(opts.run_qemu);
    }

    #[test]
    fn iterations_alias_matches_count() {
        let opts = parse_test_options(&argv(&["--iterations", "7"])).expect("parse");
        assert_eq!(opts.budget, RunBudget::Count(7));
    }

    #[test]
    fn unrecognised_arguments_are_forwarded_to_cargo_test() {
        let opts =
            parse_test_options(&argv(&["--count", "3", "--", "--nocapture"])).expect("parse");
        assert_eq!(opts.budget, RunBudget::Count(3));
        assert_eq!(opts.forward, argv(&["--", "--nocapture"]));
    }

    /// `--soak` with no override selects the 24 h budget the nightly soak
    /// workflow relies on to run the tests repeatedly for a full night.
    #[test]
    fn soak_flag_selects_the_twenty_four_hour_budget() {
        let opts = parse_test_options(&argv(&["--qemu", "--soak"])).expect("parse");
        assert_eq!(
            opts.budget,
            RunBudget::Duration(Duration::from_secs(TEST_SOAK_SECS))
        );
        assert!(opts.run_qemu);
    }

    /// `--secs` tunes the soak budget down for smoke runs.
    #[test]
    fn secs_overrides_the_soak_budget() {
        let opts = parse_test_options(&argv(&["--soak", "--secs", "120"])).expect("parse");
        assert_eq!(opts.budget, RunBudget::Duration(Duration::from_secs(120)));
    }

    /// `--secs` alone is enough to select a duration budget.
    #[test]
    fn secs_without_soak_sets_a_duration_budget() {
        let opts = parse_test_options(&argv(&["--secs", "30"])).expect("parse");
        assert_eq!(opts.budget, RunBudget::Duration(Duration::from_secs(30)));
    }

    /// A fixed count and a wall-clock budget are mutually exclusive; rather
    /// than pick a silent winner, combining them is an error.
    #[test]
    fn count_and_soak_conflict_is_rejected() {
        let err =
            parse_test_options(&argv(&["--count", "5", "--soak"])).expect_err("conflict rejected");
        assert!(
            err.contains("cannot be combined"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn non_numeric_secs_is_rejected() {
        let err = parse_test_options(&argv(&["--secs", "soon"])).expect_err("non-numeric rejected");
        assert!(err.contains("invalid `--secs`"), "unexpected error: {err}");
    }

    #[test]
    fn secs_without_a_value_is_rejected() {
        let err = parse_test_options(&argv(&["--secs"])).expect_err("missing value rejected");
        assert!(
            err.contains("requires an integer"),
            "unexpected error: {err}"
        );
    }

    /// `Count` runs exactly the requested number of passes, in order.
    #[test]
    fn run_budget_count_runs_exactly_n_passes() {
        let mut passes = Vec::new();
        RunBudget::Count(3)
            .for_each(|pass| {
                passes.push(pass);
                Ok(())
            })
            .expect("count budget runs");
        assert_eq!(passes, vec![1, 2, 3]);
    }

    /// A zero-second duration budget still runs one full pass: the clock is
    /// checked after the body, so the matrix is never cut off before a run.
    #[test]
    fn run_budget_duration_runs_at_least_one_pass() {
        let mut passes = 0u64;
        RunBudget::Duration(Duration::from_secs(0))
            .for_each(|_| {
                passes += 1;
                Ok(())
            })
            .expect("duration budget runs");
        assert_eq!(passes, 1);
    }

    /// A failing pass aborts the loop immediately and propagates the error
    /// (no retry, §7).
    #[test]
    fn run_budget_stops_on_first_failure() {
        let mut passes = 0u64;
        let err = RunBudget::Count(5)
            .for_each(|pass| {
                passes += 1;
                if pass == 2 {
                    Err("boom".to_string())
                } else {
                    Ok(())
                }
            })
            .expect_err("failure propagates");
        assert_eq!(err, "boom");
        assert_eq!(passes, 2);
    }

    /// A zero count must fail closed rather than silently collapse the
    /// matrix to no runs — the whole point of the flag is to *repeat*.
    #[test]
    fn zero_count_is_rejected() {
        let err = parse_test_options(&argv(&["--count", "0"])).expect_err("zero rejected");
        assert!(err.contains("at least 1"), "unexpected error: {err}");
    }

    #[test]
    fn non_numeric_count_is_rejected() {
        let err =
            parse_test_options(&argv(&["--count", "lots"])).expect_err("non-numeric rejected");
        assert!(
            err.contains("invalid iteration count"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn count_without_a_value_is_rejected() {
        let err = parse_test_options(&argv(&["--count"])).expect_err("missing value rejected");
        assert!(
            err.contains("requires a positive integer"),
            "unexpected error: {err}"
        );
    }
}
