//! `cargo xtask test --wasm` implementation.
//!
//! The wasm32 counterpart of [`super::qemu_tests`]. Where the bare-metal
//! verticals boot under QEMU, the wasm32 verticals boot in a real
//! (headless) browser: this module builds each wasm32 vertical `cdylib`
//! for `wasm32-unknown-unknown` and launches its puppeteer harness
//! against the compiled module. Each harness decides PASS/FAIL from the
//! kernel's console markers and propagates its exit status here. The
//! enrolled verticals are listed in [`VERTICALS`]; adding a wasm32
//! vertical is one row there (one driver, not a
//! per-vertical copy of the build/run glue).
//!
//! Kept opt-in behind `test --wasm` (mirroring `test --qemu`) because it
//! needs `node`, `puppeteer`, and a Chrome binary; a host lacking them
//! fails loudly rather than skipping (never silently
//! skip a test).

use std::path::PathBuf;

use crate::Context;

/// Rust target every wasm32 vertical is built for.
const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// One enrolled wasm32 browser vertical.
struct Vertical {
    /// Workspace package name.
    package: &'static str,
    /// File stem cargo emits for the `cdylib` (the crate name, `-` → `_`).
    artifact: &'static str,
    /// Harness runner, workspace-relative.
    harness: &'static str,
}

/// The wasm32 browser verticals `cargo xtask test --wasm` builds and runs,
/// in order. Each boots the compiled module in a headless browser and
/// scrapes its own console markers.
const VERTICALS: &[Vertical] = &[
    // Stage 3d + W8: boot, per-worker isolation, live scheduler ticks,
    // multi-worker SMP + cross-context IPI.
    Vertical {
        package: "tairix-test-kernel-arch-boot-wasm32",
        artifact: "tairix_test_kernel_arch_boot_wasm32.wasm",
        harness: "tests/integration/kernel_arch_boot_wasm32/web/harness.mjs",
    },
    // The `display`-row parity vertical: signed framebuffer `.rxe`
    // lifecycle presenting to a real canvas (`plans/WIRING.md`).
    Vertical {
        package: "tairix-test-framebuffer-display-wasm32",
        artifact: "tairix_test_framebuffer_display_wasm32.wasm",
        harness: "tests/integration/framebuffer_display_wasm32/web/harness.mjs",
    },
];

/// The wasm32 verticals and the triple they build for.
///
/// The lint gate (`super::target_clippy`) reads the same enrolment table the
/// build reads, so a newly enrolled vertical is linted without being listed
/// twice.
pub fn packages() -> (&'static str, Vec<&'static str>) {
    (WASM_TARGET, VERTICALS.iter().map(|v| v.package).collect())
}

/// Check the browser toolchain is present and build every wasm32
/// vertical once.
///
/// Call this before the (possibly repeated) [`run_once`] passes. A host
/// lacking `node` fails loudly here rather than skipping (never silently skip a test).
pub fn prepare(ctx: &Context) -> Result<(), String> {
    eprintln!("xtask: [test --wasm] building the wasm32 browser verticals");

    if !node_available() {
        return Err(
            "node is not on PATH; the wasm32 browser harness needs Node.js + puppeteer + Chrome"
                .to_string(),
        );
    }

    for vertical in VERTICALS {
        build(ctx, vertical)?;
    }
    Ok(())
}

/// Run every enrolled wasm32 harness once.
///
/// The caller ([`super::run_test`]) owns the repeat loop so a duration
/// budget covers the whole matrix as a unit; this runs exactly one pass.
pub fn run_once(ctx: &Context) -> Result<(), String> {
    for vertical in VERTICALS {
        run_harness(ctx, vertical)?;
    }
    Ok(())
}

fn build(ctx: &Context, vertical: &Vertical) -> Result<(), String> {
    let mut cmd = ctx.cargo();
    cmd.args([
        "build",
        "--locked",
        "-p",
        vertical.package,
        "--target",
        WASM_TARGET,
    ]);
    ctx.run(&format!("test --wasm (build {})", vertical.package), cmd)
}

fn run_harness(ctx: &Context, vertical: &Vertical) -> Result<(), String> {
    let wasm: PathBuf = ctx
        .target_dir()
        .join(WASM_TARGET)
        .join("debug")
        .join(vertical.artifact);
    let harness = ctx.workspace_root.join(vertical.harness);

    let mut cmd = std::process::Command::new("node");
    cmd.current_dir(&ctx.workspace_root)
        .arg(&harness)
        .arg("--wasm")
        .arg(&wasm);
    ctx.run(&format!("test --wasm (harness {})", vertical.package), cmd)
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
