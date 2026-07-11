//! QEMU runner for RustOS integration tests.
//!
//! This crate is the single, documented gateway between the host build and a
//! QEMU process used to execute a kernel-mode integration test. It exists
//! because the charter requires that all tests — including the QEMU-based
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
//! the charter forbids flaky tests and forbids retries. The runner
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
/// gateway deterministically (no flaky tests).
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
/// runner's stdin is `null` (no interactivity). To make
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
    /// How many times [`ready_marker`](Self::ready_marker) must appear
    /// before the key is sent (minimum 1). With a pointer sibling
    /// attached ([`Spec::with_virtio_mouse`]) the guest arms one driver
    /// instance per virtio-input node and prints the marker once per
    /// instance; injecting on the first sighting could race the
    /// keyboard's own arming (the first-armed instance may be the
    /// mouse's), losing the keypress against an un-ready device.
    pub ready_occurrences: u32,
}

/// A deterministic typed-text injection request for an interactive
/// vertical whose console input is the seat keyboard, not the serial line
/// — the multi-key sibling of [`KeyInjection`].
///
/// With a display attached the guest's primary console takes input only
/// from its own keyboard, so a dialogue (a passphrase, a login) cannot be
/// scripted over serial. The runner instead attaches a
/// `virtio-keyboard-device`, waits for [`ready_marker`](Self::ready_marker)
/// on the serial console (proving the keyboard driver is armed — typed
/// keys buffer as type-ahead until the guest's reader drains them), then
/// types [`text`](Self::text) one `sendkey` per character through the QEMU
/// monitor. Keys are **paced**: each is held briefly and the next is sent
/// only after the previous hold has elapsed, so repeated characters
/// ("tt") arrive as distinct press/release edges and are never coalesced
/// by the device (deterministic, not timing-lucky).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyTyping {
    /// Serial-console substring the runner waits for before typing.
    pub ready_marker: String,
    /// How many times [`ready_marker`](Self::ready_marker) must appear
    /// before typing starts (minimum 1) — the same per-driver-instance
    /// counting as [`KeyInjection::ready_occurrences`].
    pub ready_occurrences: u32,
    /// The text to type. Every character must be printable ASCII, `\n`
    /// (sent as `ret`), or `\t` (sent as `tab`); the runner fails the run
    /// on the first untypable character (fail closed, never skipped).
    pub text: String,
}

/// A deterministic pointer-motion injection request for an input vertical
/// — the mouse analogue of [`KeyInjection`].
///
/// The runner waits for [`ready_marker`](Self::ready_marker) on the serial
/// console, then sends one `mouse_move <dx> <dy>` through the QEMU monitor.
/// QEMU delivers the relative motion to the attached `virtio-mouse-device`
/// (`EV_REL` events), so the guest's pointer driver decodes and injects a
/// real device-originated motion. Requires [`Spec::with_virtio_mouse`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointerInjection {
    /// Serial-console substring the runner waits for before injecting —
    /// typically a marker proving the keyboard's own injection already
    /// landed, so the two injections are ordered and separately
    /// witnessed.
    pub ready_marker: String,
    /// Relative x motion in device counts (positive rightward).
    pub dx: i32,
    /// Relative y motion in device counts (positive downward).
    pub dy: i32,
}

/// One step of a deterministic serial-input script for an interactive
/// vertical.
///
/// The guest's `-serial stdio` console is fed from the runner's stdin,
/// but a `no_std`, non-interactive guest cannot type at itself and the
/// runner is non-interactive. To make a real UART
/// RX→`stream_read` exchange deterministic, the runner pipes QEMU's
/// stdin and replays the steps of [`Spec::serial_input`] strictly in
/// order: each step waits for the guest to print
/// [`ready_marker`](Self::ready_marker) on the serial console *after*
/// the previous step's match (proving the reader is blocked on input —
/// e.g. a login prompt), then writes [`line`](Self::line) to the pipe.
/// QEMU delivers the bytes to the guest's serial device RX exactly as a
/// human typing would — the serial-console analogue of [`KeyInjection`].
/// Because matching advances through the log, a repeated prompt (a
/// second `Username: ` after a refused login) anchors its own step. A
/// run that exits before every step was sent fails: an unreached marker
/// means the guest never made the expected exchange.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialInjection {
    /// Serial-console substring the runner waits for before writing.
    /// The guest prints it once it is blocked reading input.
    pub ready_marker: String,
    /// Bytes written verbatim to the guest's serial input (include the
    /// terminating `\n` for line-oriented readers).
    pub line: String,
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
    /// When `Some`, attach a `virtio-keyboard-device` and type the
    /// described text through paced monitor `sendkey`s once its readiness
    /// marker appears — the scripted-dialogue path for a guest whose
    /// primary console is the display, where [`Spec::serial_input`]
    /// cannot reach. Only the aarch64 argv honours it today.
    pub input_typing: Option<KeyTyping>,
    /// When `true`, attach a `virtio-mouse-device` after the keyboard —
    /// the same two-identical-virtio-input-nodes topology an interactive
    /// session presents — so a vertical can prove the keyboard is still
    /// driven when a pointer sibling is enumerated beside it. Only the
    /// aarch64 argv honours it today.
    pub input_mouse: bool,
    /// When `Some`, inject the described relative mouse motion through
    /// the QEMU monitor once its readiness marker appears on the serial
    /// console. Meaningful only with [`Spec::input_mouse`]; `None`
    /// injects no motion.
    pub input_pointer_move: Option<PointerInjection>,
    /// When non-empty, pipe QEMU's stdin and replay the steps in order:
    /// each waits for its readiness marker on the serial console (past
    /// the previous step's match) before writing its line. Empty leaves
    /// stdin closed (`null`). Used by the interactive-session verticals.
    pub serial_input: Vec<SerialInjection>,
    /// How the session presents itself to a human. Only the aarch64
    /// argv honours it today.
    pub session: SessionKind,
}

/// How a QEMU session presents itself to a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Headless test run: `-display none`, no human input devices — the
    /// deterministic [`Runner::run`] shape every vertical uses.
    HeadlessTest,
    /// Interactive windowed session: QEMU's default display backend
    /// (cocoa/gtk/sdl) so the guest's `ramfb` scan-out is visible, plus
    /// human-driven `virtio-keyboard-device` and `virtio-mouse-device`
    /// input from the window — the [`Runner::run_interactive`] shape
    /// `cargo xtask run` launches.
    WindowedInteractive,
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
            input_typing: None,
            input_mouse: false,
            input_pointer_move: None,
            serial_input: Vec::new(),
            session: SessionKind::HeadlessTest,
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
            input_typing: None,
            input_mouse: false,
            input_pointer_move: None,
            serial_input: Vec::new(),
            session: SessionKind::HeadlessTest,
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
            input_typing: None,
            input_mouse: false,
            input_pointer_move: None,
            serial_input: Vec::new(),
            session: SessionKind::HeadlessTest,
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
    /// guest-side interactivity.
    #[must_use]
    pub fn with_virtio_keyboard(
        mut self,
        ready_marker: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        self.input_keyboard = Some(KeyInjection {
            ready_marker: ready_marker.into(),
            key: key.into(),
            ready_occurrences: 1,
        });
        self
    }

    /// Require the keyboard injection's readiness marker to appear `n`
    /// times (clamped at `>= 1`) before the key is sent. Used with
    /// [`Self::with_virtio_mouse`]: each virtio-input node arms its own
    /// driver instance and prints the marker once, so the injection
    /// waits until every instance — the keyboard's included — is armed.
    /// A no-op when no keyboard injection was requested.
    #[must_use]
    pub fn with_keyboard_ready_occurrences(mut self, n: u32) -> Self {
        if let Some(k) = &mut self.input_keyboard {
            k.ready_occurrences = n.max(1);
        }
        self
    }

    /// Attach a `virtio-keyboard-device` and type `text` through paced
    /// monitor `sendkey`s once `ready_marker` has appeared `occurrences`
    /// times (clamped at `>= 1`) on the serial console. Used by a vertical
    /// whose guest console is the display: the dialogue is typed at the
    /// seat keyboard, buffering as type-ahead until the guest reads it.
    #[must_use]
    pub fn with_typed_keys(
        mut self,
        ready_marker: impl Into<String>,
        occurrences: u32,
        text: impl Into<String>,
    ) -> Self {
        self.input_typing = Some(KeyTyping {
            ready_marker: ready_marker.into(),
            ready_occurrences: occurrences.max(1),
            text: text.into(),
        });
        self
    }

    /// Attach a `virtio-mouse-device` after the keyboard — the same
    /// two-identical-virtio-input-nodes topology an interactive session
    /// presents. Used by the autoload-input vertical to prove the
    /// keyboard driver instance is still loaded and delivering when a
    /// pointer sibling matches the same driver bundle.
    #[must_use]
    pub fn with_virtio_mouse(mut self) -> Self {
        self.input_mouse = true;
        self
    }

    /// Inject one relative mouse motion (`mouse_move dx dy`) through the
    /// QEMU monitor once `ready_marker` appears on the serial console.
    /// Also attaches the `virtio-mouse-device` the motion targets, so a
    /// spec cannot ask for motion with no device to deliver it to.
    #[must_use]
    pub fn with_pointer_move(mut self, ready_marker: impl Into<String>, dx: i32, dy: i32) -> Self {
        self.input_mouse = true;
        self.input_pointer_move = Some(PointerInjection {
            ready_marker: ready_marker.into(),
            dx,
            dy,
        });
        self
    }

    /// Append one step to the serial-input script: pipe QEMU's stdin and
    /// write `line` to the guest's serial input once the guest prints
    /// `ready_marker` on the serial console, past the previous step's
    /// match. Call repeatedly to script a whole exchange (prompt → reply
    /// → next prompt → …); the steps replay strictly in order, and a run
    /// that exits before every step was sent fails. Used by the
    /// interactive-session verticals to type at the blocked login
    /// deterministically without runner interactivity.
    #[must_use]
    pub fn with_serial_input(
        mut self,
        ready_marker: impl Into<String>,
        line: impl Into<String>,
    ) -> Self {
        self.serial_input.push(SerialInjection {
            ready_marker: ready_marker.into(),
            line: line.into(),
        });
        self
    }

    /// Present QEMU's default windowed display and attach human-driven
    /// virtio keyboard and mouse devices — the interactive session shape
    /// [`Runner::run_interactive`] launches for `cargo xtask run`.
    #[must_use]
    pub fn windowed_interactive(mut self) -> Self {
        self.session = SessionKind::WindowedInteractive;
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
        validate_boot_inputs(spec)?;

        // Every port boots the kernel ELF directly — x86_64 via QEMU's
        // PVH `-kernel` loader, riscv64 via OpenSBI (`-bios default` +
        // `-kernel`), aarch64 via QEMU's `-kernel` loader — so the
        // kernel ELF *is* the boot artifact and no boot media is built.
        let mut cmd = Command::new(spec.arch.qemu_binary());
        match spec.arch {
            Arch::X86_64 => x86_64::push_argv(&mut cmd, spec, &spec.kernel),
            Arch::Riscv64 => riscv64::push_argv(&mut cmd, spec, &spec.kernel),
            Arch::Aarch64 => aarch64::push_argv(&mut cmd, spec, &spec.kernel),
        }
        // Caller-supplied extras are appended *after* the per-arch defaults
        // so a developer can override them ad-hoc (e.g. `-d int,cpu_reset`).
        for a in &spec.extra_args {
            cmd.arg(a);
        }

        // When the spec asks for key or pointer injection, attach a QEMU
        // monitor on a private unix socket so the runner can drive
        // `sendkey` / `mouse_move` once the guest is ready. The socket is
        // server-side in QEMU (created at startup, well before the guest's
        // readiness marker) and the runner connects as a client.
        let monitor = (spec.input_keyboard.is_some()
            || spec.input_typing.is_some()
            || spec.input_pointer_move.is_some())
        .then(MonitorSocket::reserve);
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
        // Serial input rides the `-serial stdio` console: pipe stdin only
        // when a vertical asked to type at the guest, otherwise keep it
        // closed so no stray host input can reach the guest (deterministic, non-interactive runs).
        if spec.serial_input.is_empty() {
            cmd.stdin(Stdio::null());
        } else {
            cmd.stdin(Stdio::piped());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let child = cmd.spawn()?;
        supervise(child, spec, monitor.as_ref())
    }

    /// Launch QEMU as an **interactive** session and wait for it to end.
    ///
    /// The developer-facing sibling of [`Runner::run`] (`cargo xtask
    /// run`): stdio is inherited, so the guest's `-serial stdio` console
    /// is the caller's own terminal; no wall-clock deadline is applied
    /// and nothing is captured — the session ends when the user closes
    /// the QEMU window, the guest powers off, or the caller interrupts
    /// it. Pair with [`Spec::windowed_interactive`] for a visible
    /// display and human keyboard/mouse input.
    ///
    /// # Errors
    ///
    /// Returns `Err` if a boot input is missing or QEMU could not be
    /// spawned. The `Ok` value is QEMU's raw process exit status code
    /// (`-1` when terminated by a signal); an interactive session has no
    /// pass/fail protocol, so no [`Outcome`] is decoded.
    pub fn run_interactive(spec: &Spec) -> io::Result<i32> {
        validate_boot_inputs(spec)?;

        let mut cmd = Command::new(spec.arch.qemu_binary());
        match spec.arch {
            Arch::X86_64 => x86_64::push_argv(&mut cmd, spec, &spec.kernel),
            Arch::Riscv64 => riscv64::push_argv(&mut cmd, spec, &spec.kernel),
            Arch::Aarch64 => aarch64::push_argv(&mut cmd, spec, &spec.kernel),
        }
        for a in &spec.extra_args {
            cmd.arg(a);
        }
        if std::env::var_os("RUSTOS_QEMU_DEBUG").is_some() {
            eprintln!("rustos-qemu: {cmd:?}");
        }
        // The caller's terminal *is* the guest serial console: inherit
        // all three stdio streams and simply wait — in the foreground,
        // with no deadline — for the user to end the session.
        cmd.stdin(Stdio::inherit());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        let status = cmd.status()?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// Fail closed before spawning QEMU if the kernel ELF or a backing image
/// is missing: QEMU would otherwise abort mid-boot with an opaque error
/// the caller could only report as a generic failure.
fn validate_boot_inputs(spec: &Spec) -> io::Result<()> {
    if !spec.kernel.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("kernel ELF not found: {}", spec.kernel.display()),
        ));
    }
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
    Ok(())
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

    let SerialDrain {
        captured,
        marker_seen,
        typing_marker_seen,
        pointer_marker_seen,
        reader,
    } = spawn_serial_drain(&mut child, spec);

    // The piped stdin handle the serial injection writes through. Held
    // for the rest of the run: dropping it closes the guest's serial
    // input, and QEMU treats a closed stdio chardev as console EOF.
    let mut serial_stdin = child.stdin.take();

    // Drain stderr on its own thread too. QEMU writes its own startup and
    // runtime diagnostics there (not to the guest serial console), so a
    // failure to even reach the guest — a corrupted pflash store, a bad
    // device argument — would otherwise leave the serial log empty with
    // nothing explaining the non-zero exit. Reading it also stops QEMU
    // wedging on a full stderr pipe (no flaky tests).
    let captured_err = Arc::new(Mutex::new(String::new()));
    let err_reader = {
        let captured_err = Arc::clone(&captured_err);
        let stderr = child.stderr.take();
        std::thread::spawn(move || drain_stream(stderr, &captured_err, &[]))
    };

    // Poll for completion in short ticks so the deadline is precise to
    // the millisecond. We deliberately do *not* sleep until the deadline
    // and then check once: that pattern adds up to `timeout` of latency
    // for fast-failing tests, which would slow `cargo xtask ci`.
    let tick = Duration::from_millis(25);
    let mut injections = InjectionState::new(spec);
    // Serial-input script cursor: the next step to send, and the byte
    // offset in the captured serial log just past the previous step's
    // matched marker. Matching only ever advances, so each marker must
    // arrive in order and a repeated prompt (e.g. a second `Username: `
    // after a refused login) anchors its own step rather than re-firing
    // on the first occurrence.
    let mut serial_step = 0usize;
    let mut serial_search_from = 0usize;
    let done = 'run: loop {
        if let Some(status) = child.try_wait()? {
            break 'run exit_reason(
                spec,
                serial_step,
                injections.pointer_sent,
                injections.typing_done(spec),
                status,
            );
        }
        if let Err(e) = injections.drive(
            spec,
            monitor,
            &marker_seen,
            &typing_marker_seen,
            &pointer_marker_seen,
        ) {
            let _ = child.kill();
            let _ = child.wait();
            break 'run DoneReason::InjectionFailed(e);
        }
        let advanced = advance_serial_script(
            &spec.serial_input,
            &captured,
            &mut serial_stdin,
            &mut serial_step,
            &mut serial_search_from,
        );
        if let Err(e) = advanced {
            let _ = child.kill();
            let _ = child.wait();
            break 'run DoneReason::InjectionFailed(format!(
                "serial input injection failed at step {serial_step}: {e}"
            ));
        }
        if Instant::now() >= deadline {
            // Strict, no-retry kill. `wait` afterwards is best
            // effort so we don't leave a zombie behind.
            let _ = child.kill();
            let _ = child.wait();
            break 'run DoneReason::TimedOut;
        }
        std::thread::sleep(tick);
    };

    // The child has exited (or been killed); the reader thread sees
    // EOF on the closed pipe and finishes. Drop the monitor connections
    // and the guest's serial input pipe only now, once the run is
    // complete.
    drop(injections);
    drop(serial_stdin);
    let _ = reader.join();
    let _ = err_reader.join();
    let mut serial = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    append_stderr(&mut serial, &captured_err);
    match done {
        DoneReason::Exited(code) => Ok(spec.arch.outcome_from_status(code, serial)),
        DoneReason::TimedOut => Ok(Outcome::Timeout {
            budget: spec.timeout,
            serial,
        }),
        DoneReason::InjectionFailed(reason) => {
            // The failure message rides the serial log so the report
            // explains *why* the run was cut short, exactly as a guest
            // diagnostic would.
            if !serial.is_empty() && !serial.ends_with('\n') {
                serial.push('\n');
            }
            serial.push_str("rustos-qemu: ");
            serial.push_str(&reason);
            serial.push('\n');
            Ok(Outcome::Fail { status: -1, serial })
        }
    }
}

/// Advance the ordered serial-input script as far as the captured serial
/// log currently allows: for each remaining step whose readiness marker
/// has arrived *past the previous step's match*, write its line to the
/// guest's serial input and move the cursor on. Matching only ever
/// advances through the log, so a repeated prompt anchors its own step.
///
/// `step` and `search_from` carry the script cursor between poll ticks;
/// the matched end of a marker is a UTF-8 boundary (it follows a complete
/// marker match), so the next slice start is always valid.
///
/// # Errors
///
/// Returns the write error when the guest's stdin pipe is missing or the
/// write/flush fails; the caller turns it into an injection failure.
fn advance_serial_script(
    steps: &[SerialInjection],
    captured: &Mutex<String>,
    serial_stdin: &mut Option<std::process::ChildStdin>,
    step: &mut usize,
    search_from: &mut usize,
) -> io::Result<()> {
    while *step < steps.len() {
        let s = &steps[*step];
        let found = {
            let log = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            log[*search_from..]
                .find(&s.ready_marker)
                .map(|at| *search_from + at + s.ready_marker.len())
        };
        let Some(matched_end) = found else { break };
        // Safe to use stdin: `run` piped it because the script is
        // non-empty.
        let stdin = serial_stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("qemu stdin pipe missing"))?;
        stdin.write_all(s.line.as_bytes())?;
        stdin.flush()?;
        *search_from = matched_end;
        *step += 1;
    }
    Ok(())
}

/// Append QEMU's captured stderr to the serial log under a labelled
/// banner, but only when it carried something.
///
/// The serial console (stdout) is the guest's own output; QEMU's stderr
/// is the host emulator's. Folding a non-empty stderr into the reported
/// log — rather than discarding it — is what makes a guest that never
/// reached its serial console (e.g. a status-1 exit on a corrupted
/// pflash store) diagnosable instead of an opaque empty log. An empty
/// stderr adds no banner so a clean run's log stays uncluttered.
fn append_stderr(serial: &mut String, captured_err: &Mutex<String>) {
    let stderr = captured_err
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if stderr.trim().is_empty() {
        return;
    }
    if !serial.is_empty() && !serial.ends_with('\n') {
        serial.push('\n');
    }
    serial.push_str("--- qemu stderr ---\n");
    serial.push_str(&stderr);
    if !serial.ends_with('\n') {
        serial.push('\n');
    }
}

/// Why the wait loop ended. Keeps serial assembly out of the loop body so
/// the captured log is read once, after the reader thread is joined.
enum DoneReason {
    /// The child exited with this status code.
    Exited(i32),
    /// The deadline elapsed; the child was killed.
    TimedOut,
    /// A requested key/serial injection could not be delivered; the
    /// child was killed. The message explains which injection and why.
    InjectionFailed(String),
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

/// The running background stdout drain of a spawned QEMU child: the
/// shared captured-serial buffer, the per-injection readiness flags, and
/// the drain thread's handle ([`spawn_serial_drain`]).
struct SerialDrain {
    /// Everything the guest has printed on serial so far.
    captured: Arc<Mutex<String>>,
    /// Set once the key injection's readiness marker has appeared the
    /// required number of times.
    marker_seen: Arc<AtomicBool>,
    /// Set once the typed-text injection's readiness marker has appeared
    /// the required number of times.
    typing_marker_seen: Arc<AtomicBool>,
    /// Set once the pointer injection's readiness marker has appeared.
    pointer_marker_seen: Arc<AtomicBool>,
    /// The drain thread, joined once the child has exited.
    reader: std::thread::JoinHandle<()>,
}

/// Start the background stdout drain for a spawned QEMU child.
///
/// Draining on a background thread serves two needs: a chatty guest must
/// not deadlock on a full stdout pipe while the caller polls, and the
/// key injector needs to watch the serial stream for its readiness
/// marker as it arrives rather than only after exit. The drain thread
/// flips the flag once the marker has appeared the required number of
/// times ([`KeyInjection::ready_occurrences`]). The serial-input script
/// instead matches against the captured log in the caller's poll loop,
/// because its markers are ordered and positional (each anchors past the
/// previous step's match).
fn spawn_serial_drain(child: &mut Child, spec: &Spec) -> SerialDrain {
    let captured = Arc::new(Mutex::new(String::new()));
    let marker_seen = Arc::new(AtomicBool::new(false));
    let typing_marker_seen = Arc::new(AtomicBool::new(false));
    let pointer_marker_seen = Arc::new(AtomicBool::new(false));
    let reader = {
        let captured = Arc::clone(&captured);
        let stdout = child.stdout.take();
        let mut markers: Vec<(String, u32, Arc<AtomicBool>)> = Vec::new();
        if let Some(k) = &spec.input_keyboard {
            markers.push((
                k.ready_marker.clone(),
                k.ready_occurrences.max(1),
                Arc::clone(&marker_seen),
            ));
        }
        if let Some(t) = &spec.input_typing {
            markers.push((
                t.ready_marker.clone(),
                t.ready_occurrences.max(1),
                Arc::clone(&typing_marker_seen),
            ));
        }
        if let Some(p) = &spec.input_pointer_move {
            markers.push((p.ready_marker.clone(), 1, Arc::clone(&pointer_marker_seen)));
        }
        std::thread::spawn(move || {
            drain_stream(stdout, &captured, &markers);
        })
    };
    SerialDrain {
        captured,
        marker_seen,
        typing_marker_seen,
        pointer_marker_seen,
        reader,
    }
}

/// Read one of QEMU's output pipes to EOF, appending every chunk to
/// `captured` and raising each marker's flag once its substring has
/// appeared the required number of times in the stream so far.
/// Best-effort: a read error simply ends the drain.
///
/// Used for both stdout (serial, with the key/serial-injection readiness
/// markers) and stderr (QEMU's own diagnostics, marker-free), so the two
/// pipes share one drain loop. Draining stderr is not
/// optional cosmetics: QEMU prints startup failures — e.g. a corrupted
/// pflash store: `pflash … has invalid size 0` — to stderr and exits
/// status 1, and a piped-but-unread stderr both loses that diagnostic and
/// can deadlock QEMU once the 64 KiB pipe buffer fills.
fn drain_stream(
    stream: Option<impl Read>,
    captured: &Mutex<String>,
    markers: &[(String, u32, Arc<AtomicBool>)],
) {
    let Some(mut r) = stream else { return };
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
                for (marker, needed, seen) in markers {
                    if !seen.load(Ordering::Acquire)
                        && guard.matches(marker.as_str()).count() >= *needed as usize
                    {
                        seen.store(true, Ordering::Release);
                    }
                }
            }
        }
    }
}

/// The QEMU `QKeyCode` (or `shift-` combination) that types `c`.
///
/// Covers the full printable-ASCII set on the default (US) keymap QEMU
/// translates `sendkey` names through, plus `\n` (`ret`) and `\t`
/// (`tab`). Every other character is refused with an error naming it —
/// the run then fails rather than silently typing a corrupted script
/// (fail closed, never a skipped or guessed key).
fn qkeycode_for(c: char) -> Result<String, String> {
    // The base (unshifted) names for the non-alphanumeric keys.
    let plain = |name: &str| Ok(name.to_string());
    let shifted = |name: &str| Ok(format!("shift-{name}"));
    match c {
        'a'..='z' | '0'..='9' => Ok(c.to_string()),
        'A'..='Z' => Ok(format!("shift-{}", c.to_ascii_lowercase())),
        '\n' => plain("ret"),
        '\t' => plain("tab"),
        ' ' => plain("spc"),
        '-' => plain("minus"),
        '=' => plain("equal"),
        '[' => plain("bracket_left"),
        ']' => plain("bracket_right"),
        ';' => plain("semicolon"),
        '\'' => plain("apostrophe"),
        '`' => plain("grave_accent"),
        '\\' => plain("backslash"),
        ',' => plain("comma"),
        '.' => plain("dot"),
        '/' => plain("slash"),
        '!' => shifted("1"),
        '@' => shifted("2"),
        '#' => shifted("3"),
        '$' => shifted("4"),
        '%' => shifted("5"),
        '^' => shifted("6"),
        '&' => shifted("7"),
        '*' => shifted("8"),
        '(' => shifted("9"),
        ')' => shifted("0"),
        '_' => shifted("minus"),
        '+' => shifted("equal"),
        '{' => shifted("bracket_left"),
        '}' => shifted("bracket_right"),
        ':' => shifted("semicolon"),
        '"' => shifted("apostrophe"),
        '~' => shifted("grave_accent"),
        '|' => shifted("backslash"),
        '<' => shifted("comma"),
        '>' => shifted("dot"),
        '?' => shifted("slash"),
        other => Err(format!("no QKeyCode mapping for character {other:?}")),
    }
}

/// The one-shot marker-gated monitor injections [`supervise`] drives on
/// every poll tick: whether each requested injection has been sent, and
/// the **single** still-open monitor connection every injection shares.
/// One connection, deliberately: QEMU's monitor chardev socket serves
/// one client at a time, so a second connection while the first is held
/// open would sit unaccepted in the listen backlog and its command would
/// silently never be processed — and the readline monitor discards a
/// command if the peer disconnects before processing it, so the one
/// stream is opened on the first send and held for the rest of the run.
struct InjectionState {
    key_sent: bool,
    pointer_sent: bool,
    /// Characters of the typed-text script already sent (`text.len()`
    /// when done, or when no typing was requested).
    typed: usize,
    /// The earliest instant the next typed key may be sent — the pacing
    /// that keeps every press/release pair distinct on the device.
    next_typed_key_at: Instant,
    conn: Option<UnixStream>,
}

/// Milliseconds a typed key is held down (`sendkey <key> <hold>`).
const TYPED_KEY_HOLD_MS: u32 = 40;

/// Minimum interval between consecutive typed keys. Strictly longer than
/// [`TYPED_KEY_HOLD_MS`], so each key's release lands before the next
/// press — a repeated character ("tt") is two clean edges, never a
/// redundant press the device would coalesce.
const TYPED_KEY_INTERVAL: Duration = Duration::from_millis(80);

impl InjectionState {
    /// Each `*_sent` flag starts "done" when its injection was not
    /// requested, so [`Self::drive`] only ever acts on requested ones.
    fn new(spec: &Spec) -> Self {
        Self {
            key_sent: spec.input_keyboard.is_none(),
            pointer_sent: spec.input_pointer_move.is_none(),
            typed: 0,
            next_typed_key_at: Instant::now(),
            conn: None,
        }
    }

    /// `true` once every requested typed character has been sent.
    fn typing_done(&self, spec: &Spec) -> bool {
        // `Option::is_none_or` needs Rust 1.82; the workspace MSRV is older.
        spec.input_typing
            .as_ref()
            .map_or(true, |t| self.typed >= t.text.chars().count())
    }

    /// Send whichever requested injections have just become ready.
    ///
    /// # Errors
    ///
    /// Returns the failing injection's message; the caller kills the
    /// child and fails the run with it.
    fn drive(
        &mut self,
        spec: &Spec,
        monitor: Option<&MonitorSocket>,
        key_seen: &AtomicBool,
        typing_seen: &AtomicBool,
        pointer_seen: &AtomicBool,
    ) -> Result<(), String> {
        // Safe to unwrap inside the closures: each `*_sent` flag is only
        // `false` when its injection request and `monitor` are both `Some`.
        if !self.key_sent && key_seen.load(Ordering::Acquire) {
            let key = &spec.input_keyboard.as_ref().expect("key present").key;
            self.send(monitor, "key", &format!("sendkey {key}"))?;
            self.key_sent = true;
        }
        if let Some(typing) = &spec.input_typing {
            // Paced: at most one key per call, and only once the previous
            // key's hold has fully elapsed, so repeated characters arrive
            // as distinct press/release edges.
            if self.typed < typing.text.chars().count()
                && typing_seen.load(Ordering::Acquire)
                && Instant::now() >= self.next_typed_key_at
            {
                let c = typing
                    .text
                    .chars()
                    .nth(self.typed)
                    .expect("index bounded by the count above");
                let key =
                    qkeycode_for(c).map_err(|e| format!("typed-text injection failed: {e}"))?;
                self.send(
                    monitor,
                    "typed-text",
                    &format!("sendkey {key} {TYPED_KEY_HOLD_MS}"),
                )?;
                self.typed += 1;
                self.next_typed_key_at = Instant::now() + TYPED_KEY_INTERVAL;
            }
        }
        if !self.pointer_sent && pointer_seen.load(Ordering::Acquire) {
            let mv = spec.input_pointer_move.as_ref().expect("motion present");
            self.send(
                monitor,
                "pointer",
                &format!("mouse_move {} {}", mv.dx, mv.dy),
            )?;
            self.pointer_sent = true;
        }
        Ok(())
    }

    /// Write one newline-terminated command over the shared monitor
    /// connection, opening it on the first send. The HMP monitor accepts
    /// commands immediately, so the banner need not be read first.
    ///
    /// # Errors
    ///
    /// Returns the failing injection's message (`what` names it).
    fn send(
        &mut self,
        monitor: Option<&MonitorSocket>,
        what: &str,
        command: &str,
    ) -> Result<(), String> {
        let fail = |e: io::Error| format!("{what} injection failed: {e}");
        if self.conn.is_none() {
            let mon = monitor.expect("monitor present for injection");
            self.conn = Some(UnixStream::connect(mon.path()).map_err(fail)?);
        }
        let Some(stream) = self.conn.as_mut() else {
            // Unreachable after the ensure above; refuse rather than panic.
            return Err(fail(io::Error::other("monitor connection vanished")));
        };
        stream
            .write_all(format!("{command}\n").as_bytes())
            .and_then(|()| stream.flush())
            .map_err(fail)
    }
}

/// Classify a child exit observed by [`supervise`]: an exit before the
/// serial-input script completed, before a requested pointer motion was
/// ever sent, or before a requested typed-text script finished, means the
/// exchange the vertical was meant to prove never happened — the run
/// fails even when the guest itself reported success. Otherwise the
/// guest's own exit status decides the outcome.
fn exit_reason(
    spec: &Spec,
    serial_step: usize,
    pointer_injected: bool,
    typing_done: bool,
    status: std::process::ExitStatus,
) -> DoneReason {
    if serial_step < spec.serial_input.len() {
        return DoneReason::InjectionFailed(format!(
            "serial input script incomplete: {serial_step} of {} steps sent \
             before exit (next marker {:?} never seen)",
            spec.serial_input.len(),
            spec.serial_input[serial_step].ready_marker,
        ));
    }
    if !pointer_injected {
        return DoneReason::InjectionFailed(
            "pointer injection never sent: its readiness marker was not seen before exit".into(),
        );
    }
    if !typing_done {
        return DoneReason::InjectionFailed(
            "typed-text script incomplete: its readiness marker was not seen (or the guest \
             exited mid-script)"
                .into(),
        );
    }
    DoneReason::Exited(status.code().unwrap_or(-1))
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
    fn append_stderr_surfaces_qemu_diagnostics_under_a_banner() {
        // Regression: a guest that fails before reaching its serial
        // console (e.g. QEMU aborting on a corrupted pflash store) left
        // the reported log empty, because the diagnostic landed on QEMU's
        // stderr, which the supervisor discarded. The failure log must
        // now carry that stderr.
        let mut serial = String::from("partial guest serial");
        let err = Mutex::new(String::from(
            "qemu-system-x86_64: system firmware block device pflash1 has invalid size 0\n",
        ));
        append_stderr(&mut serial, &err);
        assert!(serial.contains("--- qemu stderr ---"));
        assert!(serial.contains("invalid size 0"));
        assert!(serial.starts_with("partial guest serial\n"));
    }

    #[test]
    fn append_stderr_is_a_noop_when_stderr_is_empty() {
        let mut serial = String::from("clean serial log\n");
        append_stderr(&mut serial, &Mutex::new(String::new()));
        assert_eq!(serial, "clean serial log\n");
        // Whitespace-only stderr is treated as empty too.
        let mut serial = String::from("clean serial log\n");
        append_stderr(&mut serial, &Mutex::new(String::from("   \n")));
        assert_eq!(serial, "clean serial log\n");
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
    fn with_serial_input_records_the_script_steps_in_order() {
        let s = Spec::for_aarch64_kernel("/tmp/k")
            .with_serial_input("Username: ", "root\n")
            .with_serial_input("Password: ", "wrong\n");
        assert_eq!(s.serial_input.len(), 2);
        assert_eq!(s.serial_input[0].ready_marker, "Username: ");
        assert_eq!(s.serial_input[0].line, "root\n");
        assert_eq!(s.serial_input[1].ready_marker, "Password: ");
        assert_eq!(s.serial_input[1].line, "wrong\n");
    }

    #[test]
    fn without_serial_input_records_no_injection() {
        let s = Spec::for_aarch64_kernel("/tmp/k");
        assert!(s.serial_input.is_empty());
    }

    #[test]
    fn drain_stream_raises_each_marker_flag_independently() {
        // Two markers watched on one stream: each flag flips exactly when
        // its own substring has arrived, so the key and serial injections
        // never trip on each other's readiness.
        let captured = Mutex::new(String::new());
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        let markers = [
            (String::from("queue armed"), 1, Arc::clone(&first)),
            (String::from("rustos$ "), 1, Arc::clone(&second)),
        ];
        let feed: &[u8] = b"boot ok\nqueue armed\nbanner\nrustos$ ";
        drain_stream(Some(feed), &captured, &markers);
        assert!(first.load(Ordering::Acquire));
        assert!(second.load(Ordering::Acquire));
        assert_eq!(
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_str(),
            "boot ok\nqueue armed\nbanner\nrustos$ "
        );
    }

    #[test]
    fn drain_stream_waits_for_the_required_marker_occurrence_count() {
        // Two driver instances each print the same armed marker; a
        // marker requiring two occurrences must not fire on the first
        // (the mouse instance arming before the keyboard's), only once
        // both have appeared.
        let captured = Mutex::new(String::new());
        let seen = Arc::new(AtomicBool::new(false));
        let markers = [(String::from("sc=irq_bind"), 2, Arc::clone(&seen))];
        let first_only: &[u8] = b"boot\nsc=irq_bind task=5\n";
        drain_stream(Some(first_only), &captured, &markers);
        assert!(
            !seen.load(Ordering::Acquire),
            "one occurrence must not satisfy a two-occurrence marker"
        );
        let second: &[u8] = b"more\nsc=irq_bind task=8\n";
        drain_stream(Some(second), &captured, &markers);
        assert!(seen.load(Ordering::Acquire));
    }

    #[test]
    fn with_typed_keys_records_the_script_and_clamps_occurrences() {
        let s = Spec::for_aarch64_kernel("/tmp/k").with_typed_keys("armed", 0, "root\n");
        let t = s.input_typing.expect("typing recorded");
        assert_eq!(t.ready_marker, "armed");
        assert_eq!(t.ready_occurrences, 1, "occurrences clamp at >= 1");
        assert_eq!(t.text, "root\n");
    }

    #[test]
    fn qkeycode_map_covers_the_typed_dialogue_characters() {
        // The exact character classes the interactive verticals type:
        // lowercase words, digits, space, hyphen, and the line terminator.
        assert_eq!(qkeycode_for('a').as_deref(), Ok("a"));
        assert_eq!(qkeycode_for('z').as_deref(), Ok("z"));
        assert_eq!(qkeycode_for('7').as_deref(), Ok("7"));
        assert_eq!(qkeycode_for(' ').as_deref(), Ok("spc"));
        assert_eq!(qkeycode_for('-').as_deref(), Ok("minus"));
        assert_eq!(qkeycode_for('\n').as_deref(), Ok("ret"));
        assert_eq!(qkeycode_for('\t').as_deref(), Ok("tab"));
    }

    #[test]
    fn qkeycode_map_shifts_uppercase_and_shifted_symbols() {
        assert_eq!(qkeycode_for('A').as_deref(), Ok("shift-a"));
        assert_eq!(qkeycode_for('!').as_deref(), Ok("shift-1"));
        assert_eq!(qkeycode_for('_').as_deref(), Ok("shift-minus"));
        assert_eq!(qkeycode_for('?').as_deref(), Ok("shift-slash"));
        assert_eq!(qkeycode_for('"').as_deref(), Ok("shift-apostrophe"));
    }

    #[test]
    fn qkeycode_map_refuses_untypable_characters() {
        // Non-ASCII and control characters have no deterministic key
        // sequence; the run fails rather than typing a corrupted script.
        assert!(qkeycode_for('é').is_err());
        assert!(qkeycode_for('\u{1b}').is_err());
    }

    #[test]
    fn every_printable_ascii_character_is_typable() {
        for b in 0x20u8..=0x7e {
            let c = b as char;
            assert!(
                qkeycode_for(c).is_ok(),
                "printable ASCII {c:?} must have a QKeyCode mapping"
            );
        }
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
            input_typing: None,
            input_mouse: false,
            input_pointer_move: None,
            serial_input: Vec::new(),
            session: SessionKind::HeadlessTest,
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
