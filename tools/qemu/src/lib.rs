//! QEMU runner for RustOS integration tests.
//!
//! This crate is the single, documented gateway between the host build and a
//! QEMU process used to execute a kernel-mode integration test. It exists
//! because `AGENTS.md` §7 requires that all tests — including the QEMU-based
//! ones from Stage 2 — run under one orchestrator (`cargo xtask test`) with
//! **no retries** and a **strict timeout**.
//!
//! # Contract
//!
//! A *QEMU integration test* is a `no_std`, `no_main` kernel binary built for
//! one of the Tier-1 bare-metal targets. The binary signals its result back
//! to the host through the QEMU `isa-debug-exit` device:
//!
//! * Writing the byte `SUCCESS_EXIT_CODE` (`0x10`) to the device's I/O port
//!   causes QEMU to exit with status `(0x10 << 1) | 1 == 0x21` (33). The
//!   runner treats this — and **only** this — as success.
//! * Writing any other byte causes QEMU to exit with a different status; the
//!   runner treats every such status as a test failure.
//!
//! The convention is the one popularised by the phil-opp `blog_os` series and
//! is the same one used by `bootimage`. It is encoded as
//! [`Outcome::from_qemu_status`] so the rule lives in exactly one place.
//!
//! # Timeouts and flakiness
//!
//! `AGENTS.md` §7 forbids flaky tests and forbids retries. The runner
//! therefore:
//!
//! * Enforces a hard wall-clock deadline supplied by the caller. A test that
//!   does not signal completion before the deadline is `Outcome::Timeout`,
//!   which the runner converts into a failure — never into a retry.
//! * Kills (`SIGKILL`) the QEMU child if the deadline is hit so a wedged VM
//!   cannot block subsequent tests.
//! * Inherits QEMU's stdout/stderr through capture so the failure report can
//!   include the full serial log without interleaving with later tests.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub mod iso;

/// Byte the kernel writes to the QEMU `isa-debug-exit` device to report
/// success. The corresponding QEMU process exit status is
/// `(SUCCESS_EXIT_CODE << 1) | 1`.
pub const SUCCESS_EXIT_CODE: u8 = 0x10;

/// Byte the kernel writes to the QEMU `isa-debug-exit` device to report
/// failure. The runner treats every non-success exit status as failure;
/// the kernel-side helper uses this value for clarity in logs.
pub const FAILURE_EXIT_CODE: u8 = 0x11;

/// I/O port the QEMU `isa-debug-exit` device listens on for x86_64 tests.
pub const ISA_DEBUG_EXIT_IOPORT: u16 = 0xf4;

/// I/O port size the QEMU `isa-debug-exit` device is configured with.
pub const ISA_DEBUG_EXIT_IOSIZE: u8 = 0x04;

/// Outcome of running a QEMU integration test.
#[derive(Debug)]
pub enum Outcome {
    /// The kernel signalled `SUCCESS_EXIT_CODE` and QEMU exited cleanly.
    Pass,
    /// The kernel signalled a non-success value, or QEMU exited with an
    /// unexpected status. `serial` holds the captured QEMU stdout for the
    /// failure report.
    Fail {
        /// QEMU exit status, as returned by the OS.
        status: i32,
        /// Captured QEMU stdout (serial-over-stdio), best-effort.
        serial: String,
    },
    /// The runner's deadline expired before QEMU exited; QEMU was killed.
    Timeout {
        /// Wall-clock budget the test was given.
        budget: Duration,
        /// Captured QEMU stdout up to the kill, best-effort.
        serial: String,
    },
}

impl Outcome {
    /// Decode a QEMU exit status under the `isa-debug-exit` convention.
    ///
    /// Returns `Outcome::Pass` iff `status == (SUCCESS_EXIT_CODE << 1) | 1`.
    /// Every other status is treated as `Outcome::Fail`. Callers attach a
    /// serial log to failures via [`Outcome::Fail`].
    #[must_use]
    pub fn from_qemu_status(status: i32, serial: String) -> Self {
        let success_status = i32::from((SUCCESS_EXIT_CODE << 1) | 1);
        if status == success_status {
            Outcome::Pass
        } else {
            Outcome::Fail { status, serial }
        }
    }

    /// Returns `true` only for `Outcome::Pass`. Convenience for runners that
    /// turn the outcome into a process exit code.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass)
    }
}

/// Per-architecture defaults the runner uses to construct a QEMU invocation.
///
/// Today only `x86_64` is supported; Stage 3b/3c/3d add their own variants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arch {
    /// `qemu-system-x86_64`, BIOS boot, `isa-debug-exit` on port `0xf4`.
    X86_64,
}

impl Arch {
    /// Name of the QEMU system binary for this architecture.
    #[must_use]
    pub fn qemu_binary(self) -> &'static str {
        match self {
            Arch::X86_64 => "qemu-system-x86_64",
        }
    }
}

/// Configuration for a single QEMU test invocation.
///
/// Built by the caller (typically `cargo xtask test --qemu`) and consumed by
/// [`Runner::run`]. Fields are public so callers can construct one inline
/// without a builder; defaults appropriate for x86_64 BIOS boot live on
/// [`Spec::for_x86_64_kernel`].
#[derive(Debug)]
pub struct Spec {
    /// Architecture to target.
    pub arch: Arch,
    /// Path to the kernel ELF that QEMU will load via `-kernel`.
    pub kernel: PathBuf,
    /// Number of emulated CPUs (`-smp`). Must be `>= 1`.
    pub cpus: u32,
    /// RAM size for the guest in mebibytes (`-m`).
    pub ram_mib: u32,
    /// Hard wall-clock deadline. The runner kills QEMU if this elapses.
    pub timeout: Duration,
    /// Extra arguments appended verbatim to the QEMU command line. Use
    /// sparingly — they bypass the runner's input validation.
    pub extra_args: Vec<OsString>,
}

impl Spec {
    /// Minimal x86_64 UEFI-boot spec suitable for a Stage-2 QEMU integration
    /// test. Defaults: single CPU, 256 MiB of RAM, 60 s timeout.
    ///
    /// 256 MiB is comfortable headroom for OVMF + GRUB + the test kernel;
    /// smaller values trip OVMF's own minimum-RAM checks on some
    /// distributions.
    #[must_use]
    pub fn for_x86_64_kernel(kernel: impl Into<PathBuf>) -> Self {
        Self {
            arch: Arch::X86_64,
            kernel: kernel.into(),
            cpus: 1,
            ram_mib: 256,
            timeout: Duration::from_secs(60),
            extra_args: Vec::new(),
        }
    }

    /// Override the CPU count.
    #[must_use]
    pub fn with_cpus(mut self, cpus: u32) -> Self {
        self.cpus = cpus.max(1);
        self
    }

    /// Override the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// QEMU runner.
///
/// `Runner` is intentionally tiny: a single [`Runner::run`] entry point that
/// translates a [`Spec`] into a child process, waits for it under a deadline,
/// and returns an [`Outcome`]. Anything more elaborate (parallel runs, `JUnit`
/// output, …) lives in `tools/xtask` so this crate stays trivially auditable.
pub struct Runner;

impl Runner {
    /// Execute a single QEMU integration test.
    ///
    /// # Errors
    ///
    /// Returns `Err` only if QEMU itself could not be spawned (missing
    /// binary, OS-level failure). Test failures are reported through
    /// [`Outcome`], not through `Err` — that distinction is what lets the
    /// caller print a clean failure report instead of an opaque error.
    pub fn run(spec: &Spec) -> io::Result<Outcome> {
        if !spec.kernel.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("kernel ELF not found: {}", spec.kernel.display()),
            ));
        }

        // On x86_64 we wrap the kernel ELF in a GRUB BIOS ISO so QEMU's
        // multiboot2 loader (via GRUB) can boot it. The ISO is built once
        // per `run` next to the kernel; rebuilds are cheap (a few MiB).
        let iso = match spec.arch {
            Arch::X86_64 => {
                let kernel_dir = spec
                    .kernel
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                let stem = spec.kernel.file_stem().unwrap_or_default().to_owned();
                let staging = kernel_dir.join(format!("{}.grub-staging", stem.to_string_lossy()));
                let iso_path = kernel_dir.join(format!("{}.iso", stem.to_string_lossy()));
                Some(iso::build_grub_iso(&spec.kernel, &staging, &iso_path)?)
            }
        };

        let iso_path = iso.as_deref().ok_or_else(|| {
            io::Error::other("internal invariant: x86_64 ISO was not built before QEMU spawn")
        })?;

        let mut cmd = Command::new(spec.arch.qemu_binary());
        push_args(&mut cmd, spec, iso_path)?;
        if std::env::var_os("RUSTOS_QEMU_DEBUG").is_some() {
            eprintln!("rustos-qemu: {cmd:?}");
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let started = Instant::now();
        let deadline = started + spec.timeout;

        // Poll for completion in short ticks so the deadline is precise to
        // the millisecond. We deliberately do *not* sleep until the deadline
        // and then check once: that pattern adds up to `timeout` of latency
        // for fast-failing tests, which would slow `cargo xtask ci`.
        let tick = Duration::from_millis(25);
        loop {
            if let Some(status) = child.try_wait()? {
                let serial = read_to_string(child.stdout.take());
                let code = status.code().unwrap_or(-1);
                return Ok(Outcome::from_qemu_status(code, serial));
            }
            if Instant::now() >= deadline {
                // Strict, no-retry kill. `wait` afterwards is best
                // effort so we don't leave a zombie behind.
                let _ = child.kill();
                let _ = child.wait();
                let serial = read_to_string(child.stdout.take());
                return Ok(Outcome::Timeout {
                    budget: spec.timeout,
                    serial,
                });
            }
            std::thread::sleep(tick);
        }
    }
}

fn push_args(cmd: &mut Command, spec: &Spec, iso: &Path) -> io::Result<()> {
    match spec.arch {
        Arch::X86_64 => {
            // OVMF/UEFI boot. Distros increasingly ship *only* `grub-efi`
            // (not `grub-pc-bin`), so the BIOS path is unreliable. UEFI
            // boot via OVMF works everywhere the firmware is installed,
            // including this CI host. The required pair of pflash
            // images is discovered through [`crate::iso::find_ovmf`].
            let ovmf = iso::find_ovmf()?;

            // Note: `-nographic` is *not* used because it implicitly
            // attaches the monitor and serial 0 to stdio, which collides
            // with our explicit `-serial stdio`. `-display none` gives the
            // headless behaviour we want without that implicit muxing.
            cmd.arg("-no-reboot")
                .arg("-display")
                .arg("none")
                .arg("-serial")
                .arg("stdio")
                .arg("-m")
                .arg(format!("{}M", spec.ram_mib))
                .arg("-smp")
                .arg(spec.cpus.to_string())
                .arg("-device")
                .arg(format!(
                    "isa-debug-exit,iobase=0x{ISA_DEBUG_EXIT_IOPORT:x},\
                     iosize=0x{ISA_DEBUG_EXIT_IOSIZE:x}"
                ))
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,readonly=on,file={}",
                    ovmf.code.display()
                ))
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,file={}",
                    ovmf.vars_copy.display()
                ))
                .arg("-cdrom")
                .arg(iso);
        }
    }
    for a in &spec.extra_args {
        cmd.arg(a);
    }
    Ok(())
}

fn read_to_string(mut s: Option<impl Read>) -> String {
    let mut out = String::new();
    if let Some(r) = s.as_mut() {
        let _ = r.read_to_string(&mut out);
    }
    out
}

/// Helper exported for downstream consumers (e.g. `cargo xtask`) that need to
/// resolve a path relative to the workspace root irrespective of cwd. Kept in
/// this crate because it is used by both the binary entry-point in
/// `src/bin/run.rs` and the xtask driver.
#[must_use]
pub fn workspace_relative(root: &Path, rel: impl AsRef<OsStr>) -> PathBuf {
    let mut p = root.to_path_buf();
    p.push(Path::new(rel.as_ref()));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_status_is_pass() {
        let s = i32::from((SUCCESS_EXIT_CODE << 1) | 1);
        assert!(matches!(
            Outcome::from_qemu_status(s, String::new()),
            Outcome::Pass
        ));
    }

    #[test]
    fn other_status_is_fail() {
        let s = i32::from((FAILURE_EXIT_CODE << 1) | 1);
        match Outcome::from_qemu_status(s, "log".into()) {
            Outcome::Fail { status, serial } => {
                assert_eq!(status, s);
                assert_eq!(serial, "log");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn spec_for_x86_64_defaults() {
        let s = Spec::for_x86_64_kernel("/tmp/k");
        assert_eq!(s.arch, Arch::X86_64);
        assert_eq!(s.cpus, 1);
        assert_eq!(s.ram_mib, 256);
        assert_eq!(s.timeout, Duration::from_secs(60));
    }

    #[test]
    fn spec_with_cpus_clamps_to_at_least_one() {
        let s = Spec::for_x86_64_kernel("/tmp/k").with_cpus(0);
        assert_eq!(s.cpus, 1);
    }

    #[test]
    fn missing_kernel_returns_not_found() {
        let s = Spec::for_x86_64_kernel("/definitely/not/a/real/path");
        let err = Runner::run(&s).expect_err("missing kernel should fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn qemu_binary_name_is_arch_specific() {
        assert_eq!(Arch::X86_64.qemu_binary(), "qemu-system-x86_64");
    }
}
