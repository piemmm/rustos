//! `cargo xtask test --wasm` implementation.
//!
//! The wasm32 counterpart of [`super::qemu_tests`]. Where the bare-metal
//! verticals boot under QEMU, the wasm32 vertical boots in a real
//! (headless) browser: this module builds the
//! `rustos-test-kernel-arch-boot-wasm32` `cdylib` for
//! `wasm32-unknown-unknown` and launches the puppeteer harness
//! (`tests/integration/kernel_arch_boot_wasm32/web/harness.mjs`) against
//! the compiled module. The harness decides PASS/FAIL from the kernel's
//! console markers (boot, isolation, cooperative-scheduler ticks) and
//! propagates its exit status here.
//!
//! Kept opt-in behind `test --wasm` (mirroring `test --qemu`) because it
//! needs `node`, `puppeteer`, and a Chrome binary; a host lacking them
//! fails loudly rather than skipping (`AGENTS.md` §7 — never silently
//! skip a test).

use std::path::PathBuf;

use crate::Context;

/// Workspace package + Rust target of the wasm32 boot vertical.
const PACKAGE: &str = "rustos-test-kernel-arch-boot-wasm32";
const WASM_TARGET: &str = "wasm32-unknown-unknown";
/// File stem cargo emits for the `cdylib` (the crate name with `-` → `_`).
const WASM_ARTIFACT: &str = "rustos_test_kernel_arch_boot_wasm32.wasm";
/// Harness runner, workspace-relative.
const HARNESS: &str = "tests/integration/kernel_arch_boot_wasm32/web/harness.mjs";

/// Check the browser toolchain is present and build the wasm32 module once.
///
/// Call this before the (possibly repeated) [`run_once`] passes. A host
/// lacking `node` fails loudly here rather than skipping (`AGENTS.md` §7 —
/// never silently skip a test).
pub fn prepare(ctx: &Context) -> Result<(), String> {
    eprintln!("xtask: [test --wasm] building the wasm32 boot vertical");

    if !node_available() {
        return Err(
            "node is not on PATH; the wasm32 browser harness needs Node.js + puppeteer + Chrome"
                .to_string(),
        );
    }

    build(ctx)
}

/// Run the wasm32 boot harness once.
///
/// The caller ([`super::run_test`]) owns the repeat loop so a duration
/// budget covers the whole matrix as a unit; this runs exactly one pass.
pub fn run_once(ctx: &Context) -> Result<(), String> {
    run_harness(ctx)
}

fn build(ctx: &Context) -> Result<(), String> {
    let mut cmd = ctx.cargo();
    cmd.args(["build", "--locked", "-p", PACKAGE, "--target", WASM_TARGET]);
    ctx.run(&format!("test --wasm (build {PACKAGE})"), cmd)
}

fn run_harness(ctx: &Context) -> Result<(), String> {
    let wasm: PathBuf = ctx
        .workspace_root
        .join("target")
        .join(WASM_TARGET)
        .join("debug")
        .join(WASM_ARTIFACT);
    let harness = ctx.workspace_root.join(HARNESS);

    let mut cmd = std::process::Command::new("node");
    cmd.current_dir(&ctx.workspace_root)
        .arg(&harness)
        .arg("--wasm")
        .arg(&wasm);
    ctx.run("test --wasm (harness)", cmd)
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
