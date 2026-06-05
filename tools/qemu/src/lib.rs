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
//! to the host through an architecture-specific debug-exit device (today only
//! x86_64's `isa-debug-exit`; Stage 3b/3c/3d ports will add their own):
//!
//! * Writing the byte `SUCCESS_EXIT_CODE` (`0x10`) to the device causes QEMU
//!   to exit with status `(0x10 << 1) | 1 == 0x21` (33). The runner treats
//!   this — and **only** this — as success.
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
//!
//! # Per-architecture surface
//!
//! Architecture-specific defaults (RAM size, OVMF/UEFI flags, debug-exit
//! device) and argv assembly live in dedicated modules (`x86_64` today;
//! Stage 3b/3c/3d add `aarch64`, `riscv64`, `wasm32`). The generic types in
//! this file — [`Outcome`], [`Arch`], [`Spec`], [`Runner`] — are
//! architecture-neutral; [`Runner::run`] dispatches into the per-arch module
//! through a single `match` on [`Spec::arch`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod aarch64;
pub mod disk;
pub mod iso;
pub mod riscv64;
pub mod x86_64;

/// Byte the kernel writes to its architecture-specific debug-exit device to
/// report success. The corresponding QEMU process exit status is
/// `(SUCCESS_EXIT_CODE << 1) | 1`.
pub const SUCCESS_EXIT_CODE: u8 = 0x10;

/// Byte the kernel writes to its architecture-specific debug-exit device to
/// report failure. The runner treats every non-success exit status as
/// failure; the kernel-side helper uses this value for clarity in logs.
pub const FAILURE_EXIT_CODE: u8 = 0x11;

/// I/O port the QEMU `isa-debug-exit` device listens on for x86_64 tests.
///
/// Re-exported from [`x86_64::ISA_DEBUG_EXIT_IOPORT`] for callers that
/// already depend on the top-level module.
pub const ISA_DEBUG_EXIT_IOPORT: u16 = x86_64::ISA_DEBUG_EXIT_IOPORT;

/// I/O port size the QEMU `isa-debug-exit` device is configured with.
///
/// Re-exported from [`x86_64::ISA_DEBUG_EXIT_IOSIZE`] for callers that
/// already depend on the top-level module.
pub const ISA_DEBUG_EXIT_IOSIZE: u8 = x86_64::ISA_DEBUG_EXIT_IOSIZE;

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

/// A backing block device attached to the guest.
///
/// The attachment is a raw image surfaced to the guest as a modern
/// virtio block device: a `virtio-blk-pci` function on x86_64 (driven by
/// the Stage 4.D `PciTransport`) or a `virtio-blk-device` on the riscv64
/// `virt` board's virtio-mmio bus (driven by `MmioTransport`). The host
/// prepares the image with [`disk::plant_raw_disk`]; this type only
/// records where it lives so the per-arch argv builder can attach it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDevice {
    /// Path to the raw backing image on the host.
    pub image: PathBuf,
}

/// A virtio network interface attached to the guest.
///
/// The attachment is a modern virtio-net device backed by QEMU's
/// user-mode (SLIRP) network: a `virtio-net-pci` function on x86_64
/// (driven by the Stage 4.D `PciTransport`) or a `virtio-net-device` on
/// the riscv64 `virt` board's virtio-mmio bus (driven by
/// `MmioTransport`). The user-mode backend needs no host privileges and
/// gives the guest a fixed SLIRP topology (guest `10.0.2.15`, gateway
/// `10.0.2.2`), so a kernel-side test can ARP for and ICMP-echo the
/// gateway deterministically (`AGENTS.md` §7 — no flaky tests).
///
/// When [`NetDevice::pcap`] is set the runner attaches a
/// `filter-dump` that writes every frame on the interface to that host
/// path in `pcap` format, so the host harness can verify the on-wire
/// exchange after the run without linking a packet-capture library into
/// the guest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetDevice {
    /// Optional host path for a `pcap` capture of all traffic on this
    /// interface. `None` attaches no capture.
    pub pcap: Option<PathBuf>,
}

/// A deterministic key-injection request for an input vertical.
///
/// A `no_std`, non-interactive QEMU guest cannot type at itself, and the
/// runner's stdin is `null` (`AGENTS.md` §7 — no interactivity). To make
/// a real device→driver input event deterministic, the runner attaches a
/// `virtio-keyboard-device` and a QEMU monitor over a private unix
/// socket, waits for the guest to print [`ready_marker`](Self::ready_marker)
/// on the serial console (proving the driver has the event queue armed),
/// then sends one `sendkey` through the monitor. QEMU emits a real
/// press+release event pair to the guest — the virtio-input analogue of
/// the PS/2 vertical's `0xD2` output-buffer injection, with the event
/// originating device-side rather than guest-side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyInjection {
    /// Serial-console substring the runner waits for before injecting.
    /// The guest prints it once its virtio-input event queue is armed.
    pub ready_marker: String,
    /// QEMU `QKeyCode` name to send (e.g. `"a"`). QEMU translates it to
    /// the guest-visible evdev keycode the driver decodes.
    pub key: String,
}

/// Tier-1 architecture this runner can target.
///
/// `X86_64` and `Riscv64` ship today; Stage 3b/3d add the remaining
/// Tier-1 targets behind their own per-arch modules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arch {
    /// `qemu-system-x86_64`, UEFI boot via OVMF, `isa-debug-exit` on port
    /// `0xf4`. See the [`x86_64`] module for the full argv contract.
    X86_64,
    /// `qemu-system-riscv64`, `OpenSBI` boot on the `virt` board, result
    /// reported through the `SiFive` Test device. See the [`riscv64`]
    /// module for the full argv contract.
    Riscv64,
    /// `qemu-system-aarch64`, `-kernel` boot on the `virt` board, result
    /// reported through ARM semihosting (`SYS_EXIT`). See the
    /// [`aarch64`] module for the full argv contract.
    Aarch64,
}

impl Arch {
    /// Name of the QEMU system binary for this architecture.
    #[must_use]
    pub fn qemu_binary(self) -> &'static str {
        match self {
            Arch::X86_64 => x86_64::QEMU_BINARY,
            Arch::Riscv64 => riscv64::QEMU_BINARY,
            Arch::Aarch64 => aarch64::QEMU_BINARY,
        }
    }

    /// Decode a QEMU host-process exit status into an [`Outcome`] under
    /// this architecture's result protocol.
    ///
    /// The convention is architecture-specific: x86_64 reports success
    /// through `isa-debug-exit` as a *non-zero* status
    /// ([`Outcome::from_qemu_status`]), whereas riscv64 reports success
    /// through the `SiFive` Test device as a *zero* status
    /// ([`riscv64::outcome_from_status`]). Keeping the rule beside the
    /// per-arch argv builder means the two halves cannot drift.
    #[must_use]
    pub fn outcome_from_status(self, status: i32, serial: String) -> Outcome {
        match self {
            Arch::X86_64 => Outcome::from_qemu_status(status, serial),
            Arch::Riscv64 => riscv64::outcome_from_status(status, serial),
            Arch::Aarch64 => aarch64::outcome_from_status(status, serial),
        }
    }
}

/// Architecture-neutral configuration for a single QEMU test invocation.
///
/// Built by the caller (typically `cargo xtask test --qemu`) and consumed by
/// [`Runner::run`]. Fields that are *only* meaningful on one architecture
/// (RAM size, OVMF flags, debug-exit device, ISO build) live in the
/// per-arch modules (`x86_64`, etc.) rather than on `Spec` itself, so this
/// type stays honest as Stage 3b/3c/3d add their own ports.
///
/// Defaults appropriate for x86_64 UEFI boot live on
/// [`Spec::for_x86_64_kernel`].
#[derive(Debug)]
pub struct Spec {
    /// Architecture to target.
    pub arch: Arch,
    /// Path to the kernel ELF that QEMU will load.
    pub kernel: PathBuf,
    /// Number of emulated CPUs (`-smp`). Must be `>= 1`.
    pub cpus: u32,
    /// Hard wall-clock deadline. The runner kills QEMU if this elapses.
    pub timeout: Duration,
    /// Backing block devices attached as `virtio-blk-pci` functions, in
    /// declaration order. Empty for tests that need no storage.
    pub block_devices: Vec<BlockDevice>,
    /// Virtio network interfaces attached over QEMU user-mode networking,
    /// in declaration order. Empty for tests that need no network.
    pub net_devices: Vec<NetDevice>,
    /// When `true`, attach a QEMU `ramfb` display device. `ramfb` is a
    /// firmware-programmed linear framebuffer whose scan-out surface
    /// lives in guest RAM; the guest programs its geometry over the
    /// `fw_cfg` interface. The riscv64 `virt` board carries the
    /// `fw_cfg` device `ramfb` rides on, so this is the display-class
    /// analogue of [`Spec::with_virtio_blk`] for the framebuffer
    /// vertical. x86_64 ignores it today.
    pub display_ramfb: bool,
    /// Extra arguments appended verbatim to the QEMU command line after the
    /// per-arch defaults. Use sparingly — they bypass the runner's input
    /// validation.
    pub extra_args: Vec<OsString>,
    /// When `Some`, attach a `virtio-keyboard-device` and inject the
    /// described key once the guest prints the readiness marker on the
    /// serial console. `None` attaches no input device. Used by the
    /// aarch64 virtio-input vertical; other arches ignore it today.
    pub input_keyboard: Option<KeyInjection>,
}

impl Spec {
    /// Minimal x86_64 UEFI-boot spec suitable for a Stage-2 QEMU integration
    /// test. Defaults: single CPU, 60 s timeout. The default guest RAM and
    /// firmware come from the [`x86_64`] module.
    #[must_use]
    pub fn for_x86_64_kernel(kernel: impl Into<PathBuf>) -> Self {
        Self {
            arch: Arch::X86_64,
            kernel: kernel.into(),
            cpus: 1,
            timeout: Duration::from_secs(60),
            block_devices: Vec::new(),
            net_devices: Vec::new(),
            display_ramfb: false,
            extra_args: Vec::new(),
            input_keyboard: None,
        }
    }

    /// Override the CPU count. Clamped at `>= 1` because `-smp 0` is
    /// rejected by every QEMU we target.
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

    /// Minimal riscv64 `virt`-board spec suitable for a QEMU integration
    /// test. Defaults: single CPU, 60 s timeout. The default guest RAM
    /// and firmware come from the [`riscv64`] module.
    #[must_use]
    pub fn for_riscv64_kernel(kernel: impl Into<PathBuf>) -> Self {
        Self {
            arch: Arch::Riscv64,
            kernel: kernel.into(),
            cpus: 1,
            timeout: Duration::from_secs(60),
            block_devices: Vec::new(),
            net_devices: Vec::new(),
            display_ramfb: false,
            extra_args: Vec::new(),
            input_keyboard: None,
        }
    }

    /// Minimal aarch64 `virt`-board spec suitable for a QEMU integration
    /// test. Defaults: single CPU, 60 s timeout. The default guest RAM,
    /// CPU model, and result protocol come from the [`aarch64`] module.
    #[must_use]
    pub fn for_aarch64_kernel(kernel: impl Into<PathBuf>) -> Self {
        Self {
            arch: Arch::Aarch64,
            kernel: kernel.into(),
            cpus: 1,
            timeout: Duration::from_secs(60),
            block_devices: Vec::new(),
            net_devices: Vec::new(),
            display_ramfb: false,
            extra_args: Vec::new(),
            input_keyboard: None,
        }
    }

    /// Attach a raw backing image as an additional virtio block device.
    /// On x86_64 it surfaces as a `virtio-blk-pci` function; on riscv64
    /// as a `virtio-blk-device` on the `virt` board's virtio-mmio bus.
    /// Prepare the image's contents with [`disk::plant_raw_disk`] before
    /// [`Runner::run`].
    #[must_use]
    pub fn with_virtio_blk(mut self, image: impl Into<PathBuf>) -> Self {
        self.block_devices.push(BlockDevice {
            image: image.into(),
        });
        self
    }

    /// Attach a virtio network interface backed by QEMU user-mode
    /// networking, with no host-side capture. On x86_64 it surfaces as a
    /// `virtio-net-pci` function; on riscv64 as a `virtio-net-device` on
    /// the `virt` board's virtio-mmio bus.
    #[must_use]
    pub fn with_virtio_net(mut self) -> Self {
        self.net_devices.push(NetDevice::default());
        self
    }

    /// Attach a virtio network interface backed by QEMU user-mode
    /// networking and capture every frame on it to `pcap` (in `pcap`
    /// format) so the host harness can verify the on-wire exchange after
    /// [`Runner::run`].
    #[must_use]
    pub fn with_virtio_net_pcap(mut self, pcap: impl Into<PathBuf>) -> Self {
        self.net_devices.push(NetDevice {
            pcap: Some(pcap.into()),
        });
        self
    }

    /// Attach a QEMU `ramfb` display device so the guest can program a
    /// linear framebuffer over `fw_cfg`. Used by the framebuffer-display
    /// vertical on the riscv64 `virt` board.
    #[must_use]
    pub fn with_ramfb(mut self) -> Self {
        self.display_ramfb = true;
        self
    }

    /// Attach a `virtio-keyboard-device` and inject `key` (a QEMU
    /// `QKeyCode` name, e.g. `"a"`) once the guest prints `ready_marker`
    /// on the serial console. Used by the aarch64 virtio-input vertical
    /// to make a real device→driver input event deterministic without
    /// guest-side interactivity (`AGENTS.md` §7).
    #[must_use]
    pub fn with_virtio_keyboard(
        mut self,
        ready_marker: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        self.input_keyboard = Some(KeyInjection {
            ready_marker: ready_marker.into(),
            key: key.into(),
        });
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
        // Fail closed before spawning QEMU if a backing image is missing:
        // QEMU would otherwise abort mid-boot with an opaque error that the
        // runner could only report as a generic failure.
        for dev in &spec.block_devices {
            if !dev.image.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "virtio-blk backing image not found: {}",
                        dev.image.display()
                    ),
                ));
            }
        }

        // Resolve the bootable artifact through the per-arch backend.
        // x86_64 wraps the kernel ELF in a GRUB BIOS ISO so QEMU's
        // multiboot2 loader (via GRUB) can boot it (built once per `run`
        // next to the kernel; rebuilds are cheap). The riscv64 `virt`
        // board boots the ELF directly through OpenSBI (`-bios default` +
        // `-kernel`), so the kernel ELF *is* the artifact.
        let boot_artifact = match spec.arch {
            Arch::X86_64 => x86_64::build_boot_artifact(spec)?,
            // The `virt` board boots the ELF directly (riscv64 via
            // OpenSBI, aarch64 via QEMU's `-kernel` loader), so the
            // kernel ELF *is* the artifact.
            Arch::Riscv64 | Arch::Aarch64 => spec.kernel.clone(),
        };

        let mut cmd = Command::new(spec.arch.qemu_binary());
        match spec.arch {
            Arch::X86_64 => x86_64::push_argv(&mut cmd, spec, &boot_artifact)?,
            Arch::Riscv64 => riscv64::push_argv(&mut cmd, spec, &boot_artifact),
            Arch::Aarch64 => aarch64::push_argv(&mut cmd, spec, &boot_artifact),
        }
        // Caller-supplied extras are appended *after* the per-arch defaults
        // so a developer can override them ad-hoc (e.g. `-d int,cpu_reset`).
        for a in &spec.extra_args {
            cmd.arg(a);
        }

        // When the spec asks for key injection, attach a QEMU monitor on
        // a private unix socket so the runner can drive `sendkey` once the
        // guest is ready. The socket is server-side in QEMU (created at
        // startup, well before the guest's readiness marker) and the
        // runner connects as a client.
        let monitor = spec
            .input_keyboard
            .as_ref()
            .map(|_| MonitorSocket::reserve());
        if let Some(mon) = &monitor {
            cmd.arg("-chardev");
            let mut chardev = OsString::from("socket,id=rustos-mon,server=on,wait=off,path=");
            chardev.push(mon.path());
            cmd.arg(chardev);
            cmd.arg("-mon");
            cmd.arg("chardev=rustos-mon,mode=readline");
        }

        if std::env::var_os("RUSTOS_QEMU_DEBUG").is_some() {
            eprintln!("rustos-qemu: {cmd:?}");
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let child = cmd.spawn()?;
        supervise(child, spec, monitor.as_ref())
    }
}

/// Supervise a spawned QEMU child to completion: drain its serial output
/// on a background thread, inject a key once the guest signals readiness
/// (if requested), enforce the deadline, and assemble the [`Outcome`].
///
/// Split out of [`Runner::run`] so the spawn-and-validate path and the
/// wait loop each stay within one screen.
fn supervise(
    mut child: Child,
    spec: &Spec,
    monitor: Option<&MonitorSocket>,
) -> io::Result<Outcome> {
    let deadline = Instant::now() + spec.timeout;

    // Drain stdout on a background thread. Two reasons: a chatty guest
    // must not deadlock on a full stdout pipe while we poll, and the
    // key-injector needs to watch the serial stream for its readiness
    // marker as it arrives rather than only after exit.
    let captured = Arc::new(Mutex::new(String::new()));
    let marker_seen = Arc::new(AtomicBool::new(false));
    let reader = {
        let captured = Arc::clone(&captured);
        let marker_seen = Arc::clone(&marker_seen);
        let marker = spec.input_keyboard.as_ref().map(|k| k.ready_marker.clone());
        let stdout = child.stdout.take();
        std::thread::spawn(move || drain_stdout(stdout, &captured, marker.as_deref(), &marker_seen))
    };

    // Poll for completion in short ticks so the deadline is precise to
    // the millisecond. We deliberately do *not* sleep until the deadline
    // and then check once: that pattern adds up to `timeout` of latency
    // for fast-failing tests, which would slow `cargo xtask ci`.
    let tick = Duration::from_millis(25);
    // `injected` starts "done" when no injection was requested.
    let mut injected = spec.input_keyboard.is_none();
    // Hold the monitor connection open for the rest of the run: a
    // readline monitor discards a command if the peer disconnects
    // before it is processed, so the stream must outlive the send.
    let mut monitor_conn: Option<UnixStream> = None;
    let done = loop {
        if let Some(status) = child.try_wait()? {
            break DoneReason::Exited(status.code().unwrap_or(-1));
        }
        if !injected && marker_seen.load(Ordering::Acquire) {
            // Safe to unwrap: `injected` is only `false` when both
            // `input_keyboard` and `monitor` are `Some`.
            let mon = monitor.expect("monitor present for injection");
            let key = &spec.input_keyboard.as_ref().expect("key present").key;
            match inject_key(mon.path(), key) {
                Ok(stream) => monitor_conn = Some(stream),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    let mut serial = captured
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    serial.push_str("\nrustos-qemu: key injection failed: ");
                    serial.push_str(&e.to_string());
                    serial.push('\n');
                    return Ok(Outcome::Fail { status: -1, serial });
                }
            }
            injected = true;
        }
        if Instant::now() >= deadline {
            // Strict, no-retry kill. `wait` afterwards is best
            // effort so we don't leave a zombie behind.
            let _ = child.kill();
            let _ = child.wait();
            break DoneReason::TimedOut;
        }
        std::thread::sleep(tick);
    };

    // The child has exited (or been killed); the reader thread sees
    // EOF on the closed pipe and finishes. Drop the monitor connection
    // only now, once the run is complete.
    drop(monitor_conn);
    let _ = reader.join();
    let serial = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    match done {
        DoneReason::Exited(code) => Ok(spec.arch.outcome_from_status(code, serial)),
        DoneReason::TimedOut => Ok(Outcome::Timeout {
            budget: spec.timeout,
            serial,
        }),
    }
}

/// Why the wait loop ended. Keeps serial assembly out of the loop body so
/// the captured log is read once, after the reader thread is joined.
enum DoneReason {
    /// The child exited with this status code.
    Exited(i32),
    /// The deadline elapsed; the child was killed.
    TimedOut,
}

/// A reserved path for QEMU's monitor unix socket.
///
/// The path is unique per run (process id + a monotonic counter) so
/// parallel runs in one process (the `cargo xtask` soak) never collide.
/// QEMU creates the socket (`server=on`); dropping this removes the
/// socket file so a run leaves no stray socket behind.
struct MonitorSocket {
    path: PathBuf,
}

impl MonitorSocket {
    fn reserve() -> Self {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rustos-qemu-mon-{}-{}.sock", std::process::id(), n));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MonitorSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read QEMU stdout to EOF, appending every chunk to `captured` and
/// raising `marker_seen` once `marker` (if any) has appeared in the
/// stream so far. Best-effort: a read error simply ends the drain.
fn drain_stdout(
    stdout: Option<impl Read>,
    captured: &Mutex<String>,
    marker: Option<&str>,
    marker_seen: &AtomicBool,
) {
    let Some(mut r) = stdout else { return };
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                let mut guard = captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.push_str(&chunk);
                if let Some(m) = marker {
                    if !marker_seen.load(Ordering::Acquire) && guard.contains(m) {
                        marker_seen.store(true, Ordering::Release);
                    }
                }
            }
        }
    }
}

/// Send a single `sendkey <key>` command to QEMU's HMP monitor on the
/// unix socket at `path`, returning the still-open connection. QEMU
/// emits a real key press+release pair to the guest's input device. The
/// HMP monitor accepts newline-terminated commands immediately, so the
/// banner need not be read first — but the caller must keep the returned
/// stream alive until the run ends, because a readline monitor discards
/// the command if the peer disconnects before it is processed.
fn inject_key(path: &Path, key: &str) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(format!("sendkey {key}\n").as_bytes())?;
    stream.flush()?;
    Ok(stream)
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
    fn spec_for_x86_64_defaults_are_architecture_neutral() {
        // The generic Spec carries only architecture-neutral fields; the
        // x86_64-specific defaults (RAM size, OVMF flags) are owned by
        // the per-arch module and asserted by its own unit tests.
        let s = Spec::for_x86_64_kernel("/tmp/k");
        assert_eq!(s.arch, Arch::X86_64);
        assert_eq!(s.cpus, 1);
        assert_eq!(s.timeout, Duration::from_secs(60));
        assert!(s.extra_args.is_empty());
    }

    #[test]
    fn spec_with_cpus_clamps_to_at_least_one() {
        let s = Spec::for_x86_64_kernel("/tmp/k").with_cpus(0);
        assert_eq!(s.cpus, 1);
    }

    #[test]
    fn spec_with_timeout_overrides_the_default() {
        let s = Spec::for_x86_64_kernel("/tmp/k").with_timeout(Duration::from_secs(7));
        assert_eq!(s.timeout, Duration::from_secs(7));
    }

    #[test]
    fn missing_kernel_returns_not_found() {
        let s = Spec::for_x86_64_kernel("/definitely/not/a/real/path");
        let err = Runner::run(&s).expect_err("missing kernel should fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn with_virtio_blk_records_the_backing_image() {
        let s = Spec::for_x86_64_kernel("/tmp/k").with_virtio_blk("/tmp/disk.img");
        assert_eq!(s.block_devices.len(), 1);
        assert_eq!(s.block_devices[0].image, PathBuf::from("/tmp/disk.img"));
    }

    #[test]
    fn with_virtio_keyboard_records_the_injection_request() {
        let s = Spec::for_aarch64_kernel("/tmp/k").with_virtio_keyboard("ready-marker", "a");
        let k = s.input_keyboard.expect("keyboard injection recorded");
        assert_eq!(k.ready_marker, "ready-marker");
        assert_eq!(k.key, "a");
    }

    #[test]
    fn without_virtio_keyboard_records_no_injection() {
        let s = Spec::for_aarch64_kernel("/tmp/k");
        assert_eq!(s.input_keyboard, None);
    }

    #[test]
    fn with_virtio_net_records_a_capture_free_interface() {
        let s = Spec::for_x86_64_kernel("/tmp/k").with_virtio_net();
        assert_eq!(s.net_devices.len(), 1);
        assert_eq!(s.net_devices[0].pcap, None);
    }

    #[test]
    fn with_virtio_net_pcap_records_the_capture_path() {
        let s = Spec::for_riscv64_kernel("/tmp/k").with_virtio_net_pcap("/tmp/cap.pcap");
        assert_eq!(s.net_devices.len(), 1);
        assert_eq!(s.net_devices[0].pcap, Some(PathBuf::from("/tmp/cap.pcap")));
    }

    #[test]
    fn net_interfaces_accumulate_in_declaration_order() {
        let s = Spec::for_x86_64_kernel("/tmp/k")
            .with_virtio_net()
            .with_virtio_net_pcap("/tmp/cap.pcap");
        assert_eq!(s.net_devices.len(), 2);
        assert_eq!(s.net_devices[0].pcap, None);
        assert_eq!(s.net_devices[1].pcap, Some(PathBuf::from("/tmp/cap.pcap")));
    }

    #[test]
    fn missing_backing_image_returns_not_found_before_spawning_qemu() {
        // Plant a real (empty) kernel file so the failure is attributable
        // to the missing backing image, not to a missing kernel.
        let kernel =
            std::env::temp_dir().join(format!("rustos-qemu-kernel-{}.elf", std::process::id()));
        std::fs::write(&kernel, b"\x7fELF").expect("write placeholder kernel");
        let s = Spec {
            arch: Arch::X86_64,
            kernel,
            cpus: 1,
            timeout: Duration::from_secs(60),
            block_devices: vec![BlockDevice {
                image: PathBuf::from("/definitely/not/a/real/disk.img"),
            }],
            net_devices: Vec::new(),
            display_ramfb: false,
            extra_args: Vec::new(),
            input_keyboard: None,
        };
        let err = Runner::run(&s).expect_err("missing backing image should fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn qemu_binary_name_is_arch_specific() {
        assert_eq!(Arch::X86_64.qemu_binary(), "qemu-system-x86_64");
        assert_eq!(Arch::Riscv64.qemu_binary(), "qemu-system-riscv64");
    }

    #[test]
    fn spec_for_riscv64_defaults_are_architecture_neutral() {
        let s = Spec::for_riscv64_kernel("/tmp/k");
        assert_eq!(s.arch, Arch::Riscv64);
        assert_eq!(s.cpus, 1);
        assert_eq!(s.timeout, Duration::from_secs(60));
        assert!(s.block_devices.is_empty());
        assert!(s.extra_args.is_empty());
    }

    #[test]
    fn outcome_decode_is_per_arch() {
        // x86_64: success is the non-zero isa-debug-exit status.
        let x86_pass = i32::from((SUCCESS_EXIT_CODE << 1) | 1);
        assert!(Arch::X86_64
            .outcome_from_status(x86_pass, String::new())
            .is_pass());
        assert!(!Arch::X86_64.outcome_from_status(0, String::new()).is_pass());
        // riscv64: success is a zero SiFive-test status — the inverse.
        assert!(Arch::Riscv64
            .outcome_from_status(0, String::new())
            .is_pass());
        assert!(!Arch::Riscv64
            .outcome_from_status(x86_pass, String::new())
            .is_pass());
    }

    #[test]
    fn missing_riscv64_kernel_returns_not_found() {
        let s = Spec::for_riscv64_kernel("/definitely/not/a/real/path");
        let err = Runner::run(&s).expect_err("missing kernel should fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn isa_debug_exit_constants_match_x86_64_module() {
        // `crate::ISA_DEBUG_EXIT_IOPORT` is a re-export of the canonical
        // value in the per-arch module; the kernel side reaches for the
        // top-level path. A drift between the two halves would silently
        // break the test-result protocol.
        assert_eq!(ISA_DEBUG_EXIT_IOPORT, x86_64::ISA_DEBUG_EXIT_IOPORT);
        assert_eq!(ISA_DEBUG_EXIT_IOSIZE, x86_64::ISA_DEBUG_EXIT_IOSIZE);
    }
}
