//! Subcommand implementations for `cargo xtask`.
//!
//! Each variant of [`Command`] corresponds to a single, named developer
//! workflow. Adding a new pipeline step means adding a new variant here —
//! never appending hidden behaviour to `ci`.

use std::ffi::OsString;
use std::path::Path;

use crate::Context;

mod abi_check;
mod cfg_check;
mod deps_check;
mod fuzz;
mod linkcheck;
mod model_check;
mod proptest;
mod qemu_tests;
mod sbom;
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
    DepsCheck,
    CfgCheck,
    Coverage,
    Sbom,
    SupplyChain,
    Fuzz,
    Proptest,
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
        Command::DepsCheck,
        Command::CfgCheck,
        Command::Coverage,
        Command::Sbom,
        Command::SupplyChain,
        Command::Fuzz,
        Command::Proptest,
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
            "deps-check" => Command::DepsCheck,
            "cfg-check" => Command::CfgCheck,
            "coverage" => Command::Coverage,
            "sbom" => Command::Sbom,
            "supply-chain" => Command::SupplyChain,
            "fuzz" => Command::Fuzz,
            "proptest" => Command::Proptest,
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
            Command::DepsCheck => "deps-check",
            Command::CfgCheck => "cfg-check",
            Command::Coverage => "coverage",
            Command::Sbom => "sbom",
            Command::SupplyChain => "supply-chain",
            Command::Fuzz => "fuzz",
            Command::Proptest => "proptest",
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
            Command::DepsCheck => run_deps_check(ctx),
            Command::CfgCheck => run_cfg_check(ctx),
            Command::Coverage => run_coverage(ctx, args),
            Command::Sbom => run_sbom(ctx, args),
            Command::SupplyChain => run_supply_chain(ctx, args),
            Command::Fuzz => run_fuzz(ctx, args),
            Command::Proptest => run_proptest(ctx, args),
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
const GUI_CRATES: &[&str] = &["rustos-wm", "rustos-iconbar"];

fn run_test(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // `--qemu` opts in to the bare-metal QEMU integration tests in
    // `tests/integration/*`. Per AGENTS.md §7 they share the test
    // entry point (`cargo xtask test`) so a single command runs the
    // whole matrix; per the same section we never retry on failure
    // and every run has a strict, finite timeout.
    let mut forward = Vec::with_capacity(args.len());
    let mut run_qemu = false;
    let mut run_wasm = false;
    for a in args {
        if a == "--qemu" {
            run_qemu = true;
        } else if a == "--wasm" {
            run_wasm = true;
        } else {
            forward.push(a.clone());
        }
    }

    let mut cmd = ctx.cargo();
    cmd.args(["test", "--workspace", "--all-targets", "--locked"]);
    cmd.args(&forward);
    ctx.run("test", cmd)?;

    if run_qemu {
        qemu_tests::run_all(ctx)?;
    }
    // `--wasm` boots the wasm32 vertical in a headless browser. It is
    // opt-in (like `--qemu`) because it needs node + puppeteer + Chrome;
    // see `commands/wasm_tests.rs`.
    if run_wasm {
        wasm_tests::run_all(ctx)?;
    }
    Ok(())
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
    run_test(ctx, &[OsString::from("--qemu")])?;
    run_docs_check(ctx, &[])?;
    run_deny(ctx)?;
    // §19.3 supply-chain integrity: the source-hash allow-list and the
    // advisory SLA. Runs right after `cargo deny` (which blocks an
    // advisory immediately); this gate caps how long one may be accepted
    // and fails closed when a pin drifts from `Cargo.lock`.
    run_supply_chain(ctx, &[])?;
    // §19.6: the per-PR fuzz budget. Runs each in-tree harness for its
    // ≥ 60 s `--quick` budget; a crash, hang, or invariant failure fails
    // the gate (fail-closed). The nightly soak is `cargo xtask fuzz
    // --soak`, run outside `ci`.
    run_fuzz(ctx, &[OsString::from("--quick")])?;
    // §19.7 Bronze: the per-PR stateful-model budget. Runs each capability
    // model for its ≥ 5 s `--quick` budget; a counterexample, hang, or
    // invariant failure fails the gate (fail-closed). The nightly soak is
    // `cargo xtask proptest --soak`, run outside `ci`.
    run_proptest(ctx, &[OsString::from("--quick")])?;
    // §19.7 Silver: exhaustively model-check the capability + IPC state
    // machine on every PR. The check is exhaustive (not budgeted) and fast,
    // so it always runs; a reachable invariant violation fails closed.
    run_model_check(&[])?;
    // §19.7: reject any unreviewed AI-drafted artefact marker that reached
    // the tree. Static and cheap; fails closed.
    run_spec_review(ctx)?;
    run_abi_check(ctx, &[])?;
    Ok(())
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
    use super::cargo_subcommand_available;
    use crate::Context;

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
}
