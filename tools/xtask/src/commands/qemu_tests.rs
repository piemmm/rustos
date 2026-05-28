//! QEMU integration-test driver invoked by `cargo xtask test --qemu`.
//!
//! AGENTS.md §7 mandates that the QEMU tests share the same orchestrator
//! as host-side tests and that each QEMU run has a *strict* per-test
//! timeout with **no retries**. This module enforces both: it builds the
//! enrolled kernels for `x86_64-unknown-none`, then drives each one
//! through [`rustos_qemu::Runner::run`] in series, failing the whole
//! `xtask test` invocation on the first failure or timeout.

use std::path::PathBuf;
use std::time::Duration;

use rustos_qemu::{Outcome, Runner, Spec};

use crate::Context;

/// One enrolled QEMU integration test.
struct QemuTest {
    /// Cargo package name (matches `[package].name`).
    package: &'static str,
    /// Binary name produced by the package (`[[bin]].name`).
    binary: &'static str,
    /// Number of emulated CPUs.
    cpus: u32,
    /// Hard wall-clock budget.
    timeout: Duration,
}

const TESTS: &[QemuTest] = &[QemuTest {
    package: "rustos-test-memory-isolation",
    binary: "rustos-test-memory-isolation",
    cpus: 1,
    timeout: Duration::from_secs(60),
}];

const TARGET: &str = "x86_64-unknown-none";

/// Build and execute every enrolled QEMU test. Returns the first failure.
pub fn run_all(ctx: &Context) -> Result<(), String> {
    eprintln!("xtask: [test --qemu] {} test(s) enrolled", TESTS.len());

    for t in TESTS {
        build_one(ctx, t)?;
        run_one(ctx, t)?;
    }
    Ok(())
}

fn build_one(ctx: &Context, t: &QemuTest) -> Result<(), String> {
    let mut cmd = ctx.cargo();
    cmd.args(["build", "--locked", "-p", t.package, "--target", TARGET]);
    ctx.run(&format!("test --qemu (build {})", t.package), cmd)
}

fn run_one(ctx: &Context, t: &QemuTest) -> Result<(), String> {
    let kernel: PathBuf = ctx
        .workspace_root
        .join("target")
        .join(TARGET)
        .join("debug")
        .join(t.binary);
    let spec = Spec::for_x86_64_kernel(&kernel)
        .with_cpus(t.cpus)
        .with_timeout(t.timeout);

    eprintln!(
        "xtask: [test --qemu (run {})] kernel={} cpus={} timeout={:?}",
        t.package,
        kernel.display(),
        t.cpus,
        t.timeout
    );

    match Runner::run(&spec).map_err(|e| format!("test --qemu ({}): {e}", t.package))? {
        Outcome::Pass => Ok(()),
        Outcome::Fail { status, serial } => Err(format!(
            "test --qemu ({}) FAILED (qemu status {status})\n--- serial ---\n{serial}\n--- end ---",
            t.package
        )),
        Outcome::Timeout { budget, serial } => Err(format!(
            "test --qemu ({}) TIMEOUT after {budget:?} (no retries per AGENTS.md §7)\n--- serial ---\n{serial}\n--- end ---",
            t.package
        )),
    }
}
