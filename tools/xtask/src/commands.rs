//! Subcommand implementations for `cargo xtask`.
//!
//! Each variant of [`Command`] corresponds to a single, named developer
//! workflow. Adding a new pipeline step means adding a new variant here —
//! never appending hidden behaviour to `ci`.

use std::ffi::OsString;
use std::path::Path;

use crate::Context;

mod abi_check;
mod linkcheck;
mod qemu_tests;

/// One sanctioned developer workflow.
#[derive(Copy, Clone, Debug)]
pub enum Command {
    Build,
    Test,
    Clippy,
    Fmt,
    DocsCheck,
    AbiCheck,
    Coverage,
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
        Command::Coverage,
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
            "coverage" => Command::Coverage,
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
            Command::Coverage => "coverage",
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
            Command::Coverage => "Produce a host-side coverage report via cargo-llvm-cov.",
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
            Command::Coverage => run_coverage(ctx, args),
            Command::Ci => run_ci(ctx),
            Command::Image => run_image(ctx, args),
        }
    }
}

fn run_build(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    let mut cmd = ctx.cargo();
    cmd.args(["build", "--workspace", "--all-targets", "--locked"]);
    cmd.args(args);
    ctx.run("build", cmd)
}

fn run_test(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    // `--qemu` opts in to the bare-metal QEMU integration tests in
    // `tests/integration/*`. Per AGENTS.md §7 they share the test
    // entry point (`cargo xtask test`) so a single command runs the
    // whole matrix; per the same section we never retry on failure
    // and every run has a strict, finite timeout.
    let mut forward = Vec::with_capacity(args.len());
    let mut run_qemu = false;
    for a in args {
        if a == "--qemu" {
            run_qemu = true;
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

fn run_ci(ctx: &Context) -> Result<(), String> {
    // The pipeline order is deliberate: cheap and deterministic checks run
    // first so a failing PR fails fast. The test phase opts in to `--qemu`
    // so the Stage-2 QEMU integration tests run as part of every PR per
    // `AGENTS.md` §7; CI hosts therefore need QEMU, `grub-mkrescue`,
    // `xorriso`, and OVMF, all documented under
    // `docs/src/platform/x86_64.md`.
    run_fmt(ctx, &[])?;
    run_clippy(ctx, &[])?;
    run_test(ctx, &[OsString::from("--qemu")])?;
    run_docs_check(ctx, &[])?;
    run_deny(ctx)?;
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
