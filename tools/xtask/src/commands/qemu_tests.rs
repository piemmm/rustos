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

const TESTS: &[QemuTest] = &[
    QemuTest {
        package: "rustos-test-memory-isolation",
        binary: "rustos-test-memory-isolation",
        cpus: 1,
        timeout: Duration::from_secs(60),
    },
    // Stage 3a (b) deliverable: AP bring-up + scheduler stress on real
    // (emulated) cores. The host-side `rustos-test-scheduler-stress`
    // workspace test continues to satisfy the AGENTS.md §7 unit / cross-
    // crate contract; this enrolment is the QEMU-on-real-cores half of
    // the same Stage-2 deliverable mandated by `PLAN.md` lines 154-158.
    QemuTest {
        package: "rustos-test-scheduler-stress-qemu",
        binary: "rustos-test-scheduler-stress-qemu",
        cpus: 4,
        timeout: Duration::from_secs(120),
    },
    // Stage 3a (c7-bin) deliverable: boot the production
    // `rustos-kernel` boot pipeline (Multiboot2 → ACPI/MADT →
    // `X86_64Arch` → per-CPU init → `BootInfo` →
    // `kernel_core::kernel_main`) and assert
    // `AuditEvent::BootCompleted` (`EventId(4004)`) appears on the
    // audit sink. The test binary `rustos-test-kernel-arch-boot`
    // wraps the lib half of `rustos-kernel` with an audit-observer
    // Sink that flips `qemu_exit::exit_success` on observing
    // `BootCompleted` — see
    // `tests/integration/kernel_arch_boot/src/main.rs`. Single CPU
    // suffices: the (c7-bin) scope only brings up the BSP. The
    // 60-second budget matches `memory_isolation`'s — both are
    // strictly bring-up tests with no workload.
    QemuTest {
        package: "rustos-test-kernel-arch-boot",
        binary: "rustos-test-kernel-arch-boot",
        cpus: 1,
        timeout: Duration::from_secs(60),
    },
];

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
