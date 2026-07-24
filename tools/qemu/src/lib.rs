//! QEMU runner for TAIRiX integration tests.
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
//! * Retries interrupted pipe reads, fails immediately on a drain error or
//!   panic, and distinguishes a prematurely closed serial channel from a guest
//!   timeout if QEMU remains running; marker-gated input can therefore never
//!   fail with a misleading diagnosis because the host stopped observing
//!   output.
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
pub mod display;
pub mod riscv64;
pub mod screendump;
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
/// `dgram` netdev over a pair of unix datagram sockets: a
/// `virtio-net-pci` function on x86_64 (driven by the Stage 4.D
/// `PciTransport`) or a `virtio-net-device` on the aarch64/riscv64
/// `virt` boards' virtio-mmio bus (driven by `MmioTransport`). QEMU
/// binds [`qemu_sock`](Self::qemu_sock) and sends every guest frame as
/// one raw Ethernet datagram to [`peer_sock`](Self::peer_sock); the
/// harness binds `peer_sock` and answers as the guest's link peer.
/// Datagram sockets need no host privileges and give each concurrent
/// run its own private wire (no flaky tests, no port collisions).
///
/// When [`NetDevice::pcap`] is set the runner attaches a
/// `filter-dump` that writes every frame on the interface to that host
/// path in `pcap` format, so the host harness can verify the on-wire
/// exchange after the run without linking a packet-capture library into
/// the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetDevice {
    /// Unix datagram socket path QEMU binds (the guest end of the wire).
    pub qemu_sock: PathBuf,
    /// Unix datagram socket path the harness binds (the peer end); QEMU
    /// sends every guest frame here.
    pub peer_sock: PathBuf,
    /// Optional host path for a `pcap` capture of all traffic on this
    /// interface. `None` attaches no capture.
    pub pcap: Option<PathBuf>,
    /// Optional fixed MAC address for the device (`"aa:bb:cc:dd:ee:ff"`).
    /// `None` lets QEMU assign its default. A vertical whose guest derives
    /// its IPv6 link-local address from the device MAC (EUI-64) pins this
    /// so the host peer can address the guest deterministically.
    pub mac: Option<String>,
}

/// Render the `-netdev dgram,…` argument for net device `i` — the one
/// definition every per-arch argv builder shares, so the socket wiring
/// can never drift between transports.
pub(crate) fn netdev_dgram_arg(i: usize, dev: &NetDevice) -> OsString {
    let mut arg = OsString::from(format!("dgram,id=net{i},local.type=unix,local.path="));
    arg.push(dev.qemu_sock.as_os_str());
    arg.push(",remote.type=unix,remote.path=");
    arg.push(dev.peer_sock.as_os_str());
    arg
}

/// Render a `-device <driver>,netdev=net{i}[,mac=…][,<extra>]` argument —
/// the one definition every per-arch argv builder shares, so the device
/// MAC (from which a guest may derive its EUI-64 link-local address) is
/// pinned identically across transports (`virtio-net-device` on the mmio
/// boards, `virtio-net-pci` on x86_64).
pub(crate) fn net_device_arg(driver: &str, i: usize, dev: &NetDevice, extra: &str) -> OsString {
    let mut arg = format!("{driver},netdev=net{i}");
    if let Some(mac) = &dev.mac {
        arg.push_str(",mac=");
        arg.push_str(mac);
    }
    arg.push_str(extra);
    OsString::from(arg)
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

/// One step of a deterministic, ordered pointer-injection script — the
/// mouse analogue of [`KeyTyping`].
///
/// Steps run strictly in order, at most one per poll tick: a step fires
/// only after every earlier step has been sent, its own
/// [`ready_marker`](Self::ready_marker) has appeared the required number
/// of times on the serial console, and no earlier-requested screendump is
/// pending unverified (so a dump of the frame *before* a click can never
/// race the click). QEMU delivers each action to the attached
/// `virtio-mouse-device` in send order (`EV_REL` motions and `EV_KEY`
/// button edges share the device's one event queue), so the guest decodes
/// a real device-originated stream in the scripted order. Requires
/// [`Spec::with_virtio_mouse`] (implied by the builder).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointerStep {
    /// Serial-console substring the runner waits for before this step —
    /// a guest-emitted witness proving the state the step targets (a
    /// presented frame, an opened menu) is really established.
    pub ready_marker: String,
    /// How many times [`ready_marker`](Self::ready_marker) must appear
    /// before the step fires (minimum 1).
    pub ready_occurrences: u32,
    /// The pointer action this step injects.
    pub action: PointerAction,
}

/// The pointer action one [`PointerStep`] injects through the QEMU
/// monitor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerAction {
    /// One relative motion: `mouse_move <dx> <dy>` (device counts,
    /// positive rightward/downward).
    Move {
        /// Relative x motion in device counts (positive rightward).
        dx: i32,
        /// Relative y motion in device counts (positive downward).
        dy: i32,
    },
    /// Press the button (its bit joins the tracked button-state mask the
    /// monitor `mouse_button` command sets).
    Press(MouseButton),
    /// Release the button (its bit leaves the tracked mask).
    Release(MouseButton),
}

/// A pointer button a [`PointerAction`] presses or releases, named by
/// role exactly as the guest's closed pointer-button set is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseButton {
    /// The primary (left) button.
    Primary,
    /// The middle button.
    Middle,
    /// The secondary (right) button.
    Secondary,
}

impl MouseButton {
    /// The button's bit in the HMP `mouse_button` state mask, **as QEMU's
    /// `hmp_mouse_button` actually decodes it**: bit `0x1` = left,
    /// `0x2` = right, `0x4` = middle.
    ///
    /// The `qemu-system-*` `mouse_button` help string ("1=L, 2=M, 4=R") is
    /// wrong: `hmp_mouse_button` (`ui/ui-hmp-cmds.c`) feeds the state mask to
    /// `qemu_input_update_buttons` through a `bmap` whose entries are the
    /// legacy `MOUSE_EVENT_*` bits (`include/ui/console.h`) —
    /// `MOUSE_EVENT_LBUTTON = 0x1`, `MOUSE_EVENT_RBUTTON = 0x2`,
    /// `MOUSE_EVENT_MBUTTON = 0x4`. So state bit `0x2` raises
    /// `INPUT_BUTTON_RIGHT` and bit `0x4` raises `INPUT_BUTTON_MIDDLE`,
    /// the opposite of what the help string claims. Following the help
    /// string sent the secondary press as bit `0x4`, which QEMU delivered
    /// to the guest as a *middle*-button event — so a scripted right-click
    /// never reached the emulated virtio-mouse as a right-click at all.
    const fn mask_bit(self) -> u32 {
        match self {
            MouseButton::Primary => 0x1,
            MouseButton::Secondary => 0x2,
            MouseButton::Middle => 0x4,
        }
    }
}

/// A deterministic, marker-gated screendump request — the host-side
/// scan-out readback of a display vertical. Multiple requests run
/// strictly in declaration order.
///
/// A guest cannot observe its own scan-out after a present (the frame
/// left its address space), so the *host* takes the evidence: once
/// [`ready_marker`](Self::ready_marker) has appeared the runner sends one
/// `screendump <path>` through the QEMU monitor, which writes the current
/// display surface as a binary PPM ([`screendump::parse_ppm`]). The dump
/// is then read back and fully parsed before the next dump — or any
/// still-unsent pointer step — is allowed to fire, so a PASS chain that
/// ends on a pointer witness cannot outrun the dump and the file can
/// never be truncated by the guest exiting first. The caller asserts the
/// decoded pixels after the run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Screendump {
    /// Serial-console substring the runner waits for before dumping —
    /// typically the guest's own witness that the frame of interest
    /// reached the scan-out surface.
    pub ready_marker: String,
    /// How many times [`ready_marker`](Self::ready_marker) must appear
    /// before the dump is taken (minimum 1).
    pub ready_occurrences: u32,
    /// Host path the PPM image is written to (by QEMU) and read back
    /// from (by the runner and the asserting caller).
    pub path: PathBuf,
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
/// e.g. a login prompt), waits [`delay_after_marker`](Self::delay_after_marker),
/// then writes [`line`](Self::line) to the pipe.
/// QEMU delivers the bytes to the guest's serial device RX exactly as a
/// human typing would — one paced byte per supervision tick, the
/// serial-console analogue of [`KeyInjection`].
/// Because matching advances through the log, a repeated prompt (a
/// second `Username: ` after a refused login) anchors its own step. A
/// run that exits before every step was sent fails: an unreached marker
/// means the guest never made the expected exchange.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialInjection {
    /// Serial-console substring the runner waits for before writing.
    /// The guest prints it once it is blocked reading input.
    pub ready_marker: String,
    /// Delay between observing the readiness marker and writing the line.
    /// Zero sends on the same supervision tick.
    pub delay_after_marker: Duration,
    /// Bytes typed in order to the guest's serial input (include the
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
    /// When non-empty, attach a `virtio-keyboard-device` and type each
    /// step's text through paced monitor `sendkey`s once that step's
    /// readiness marker has appeared — the scripted-dialogue path for a
    /// guest whose primary console is the display, where
    /// [`Spec::serial_input`] cannot reach. Steps run strictly in order:
    /// a step types only after the previous step finished *and* its own
    /// marker was seen. Only the aarch64 argv honours it today.
    pub input_typing: Vec<KeyTyping>,
    /// When `true`, attach a `virtio-mouse-device` after the keyboard —
    /// the same two-identical-virtio-input-nodes topology an interactive
    /// session presents — so a vertical can prove the keyboard is still
    /// driven when a pointer sibling is enumerated beside it. Only the
    /// aarch64 argv honours it today.
    pub input_mouse: bool,
    /// When non-empty, inject the described pointer actions through the
    /// QEMU monitor strictly in order, each once its readiness marker has
    /// appeared on the serial console and every earlier-requested
    /// screendump has verified. Meaningful only with
    /// [`Spec::input_mouse`]; empty injects nothing.
    pub pointer_script: Vec<PointerStep>,
    /// When non-empty, pipe QEMU's stdin and replay the steps in order:
    /// each waits for its readiness marker on the serial console (past
    /// the previous step's match) before writing its line. Empty leaves
    /// stdin closed (`null`). Used by the interactive-session verticals.
    pub serial_input: Vec<SerialInjection>,
    /// When non-empty, take the QEMU monitor `screendump`s of the guest's
    /// display strictly in order, each once its readiness marker appears,
    /// holding later dumps and any still-unsent pointer steps back until
    /// the current dumped image has been read back and fully parsed — so
    /// a witness chain "present → dump → injection → guest PASS" is
    /// strictly ordered and no dump can be truncated by the guest
    /// exiting. Empty takes no dump.
    pub screendumps: Vec<Screendump>,
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
            input_typing: Vec::new(),
            input_mouse: false,
            pointer_script: Vec::new(),
            serial_input: Vec::new(),
            screendumps: Vec::new(),
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
            input_typing: Vec::new(),
            input_mouse: false,
            pointer_script: Vec::new(),
            serial_input: Vec::new(),
            screendumps: Vec::new(),
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
            input_typing: Vec::new(),
            input_mouse: false,
            pointer_script: Vec::new(),
            serial_input: Vec::new(),
            screendumps: Vec::new(),
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

    /// Attach a virtio network interface backed by a QEMU `dgram` netdev
    /// over the given unix datagram socket pair (QEMU binds `qemu_sock`
    /// and sends guest frames to `peer_sock`, which the harness binds),
    /// capturing every frame on it to `pcap` (in `pcap` format) so the
    /// host harness can verify the on-wire exchange after
    /// [`Runner::run`]. Bind `peer_sock` *before* the run so no early
    /// guest frame is dropped.
    #[must_use]
    pub fn with_virtio_net_dgram(
        mut self,
        qemu_sock: impl Into<PathBuf>,
        peer_sock: impl Into<PathBuf>,
        pcap: impl Into<PathBuf>,
    ) -> Self {
        self.net_devices.push(NetDevice {
            qemu_sock: qemu_sock.into(),
            peer_sock: peer_sock.into(),
            pcap: Some(pcap.into()),
            mac: None,
        });
        self
    }

    /// Like [`Self::with_virtio_net_dgram`] but pins the device's MAC
    /// address, so a guest that forms its IPv6 link-local address from the
    /// device MAC (EUI-64) is reachable at a MAC the host peer knows ahead
    /// of the run. `mac` is a QEMU MAC string (`"aa:bb:cc:dd:ee:ff"`).
    #[must_use]
    pub fn with_virtio_net_dgram_mac(
        mut self,
        qemu_sock: impl Into<PathBuf>,
        peer_sock: impl Into<PathBuf>,
        pcap: impl Into<PathBuf>,
        mac: impl Into<String>,
    ) -> Self {
        self.net_devices.push(NetDevice {
            qemu_sock: qemu_sock.into(),
            peer_sock: peer_sock.into(),
            pcap: Some(pcap.into()),
            mac: Some(mac.into()),
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

    /// Append one typed-text step: attach a `virtio-keyboard-device` and
    /// type `text` through paced monitor `sendkey`s once `ready_marker`
    /// has appeared `occurrences` times (clamped at `>= 1`) on the serial
    /// console. Call repeatedly to script a whole seat-keyboard dialogue
    /// (passphrase → login → choice → …); the steps run strictly in
    /// order, each gated on its own marker after the previous step
    /// finished. Used by a vertical whose guest console is the display:
    /// each step's characters buffer as type-ahead until the guest reads
    /// them.
    #[must_use]
    pub fn with_typed_keys(
        mut self,
        ready_marker: impl Into<String>,
        occurrences: u32,
        text: impl Into<String>,
    ) -> Self {
        self.input_typing.push(KeyTyping {
            ready_marker: ready_marker.into(),
            ready_occurrences: occurrences.max(1),
            text: text.into(),
        });
        self
    }

    /// Append one monitor `screendump` of the guest display into `path`,
    /// taken once `ready_marker` has appeared `occurrences` times
    /// (clamped at `>= 1`) on the serial console. Dumps run strictly in
    /// declaration order; later dumps and still-unsent pointer steps are
    /// held back until the current dumped image parses completely, so a
    /// PASS chain ending on a pointer witness cannot outrun any dump.
    #[must_use]
    pub fn with_screendump(
        mut self,
        ready_marker: impl Into<String>,
        occurrences: u32,
        path: impl Into<PathBuf>,
    ) -> Self {
        self.screendumps.push(Screendump {
            ready_marker: ready_marker.into(),
            ready_occurrences: occurrences.max(1),
            path: path.into(),
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

    /// Append one step to the ordered pointer-injection script, fired
    /// once `ready_marker` has appeared `occurrences` times (clamped at
    /// `>= 1`) on the serial console, after every earlier step. Also
    /// attaches the `virtio-mouse-device` the actions target, so a spec
    /// cannot ask for pointer input with no device to deliver it to.
    #[must_use]
    pub fn with_pointer_step(
        mut self,
        ready_marker: impl Into<String>,
        occurrences: u32,
        action: PointerAction,
    ) -> Self {
        self.input_mouse = true;
        self.pointer_script.push(PointerStep {
            ready_marker: ready_marker.into(),
            ready_occurrences: occurrences.max(1),
            action,
        });
        self
    }

    /// Append one step to the serial-input script: pipe QEMU's stdin and
    /// write `line` to the guest's serial input once the guest prints
    /// `ready_marker` on the serial console, past the previous step's match,
    /// and `delay_after_marker` has elapsed. Call repeatedly to script a whole
    /// exchange (prompt → reply → next prompt → …); the steps replay strictly
    /// in order, and a run that exits before every step was sent fails. Used
    /// by the interactive-session verticals to type at the blocked login and
    /// to reproduce deliberate human pauses deterministically.
    #[must_use]
    pub fn with_serial_input(
        mut self,
        ready_marker: impl Into<String>,
        delay_after_marker: Duration,
        line: impl Into<String>,
    ) -> Self {
        self.serial_input.push(SerialInjection {
            ready_marker: ready_marker.into(),
            delay_after_marker,
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
            || !spec.input_typing.is_empty()
            || !spec.pointer_script.is_empty()
            || !spec.screendumps.is_empty())
        .then(MonitorSocket::reserve);
        if let Some(mon) = &monitor {
            cmd.arg("-chardev");
            let mut chardev = OsString::from("socket,id=tairix-mon,server=on,wait=off,path=");
            chardev.push(mon.path());
            cmd.arg(chardev);
            cmd.arg("-mon");
            cmd.arg("chardev=tairix-mon,mode=readline");
        }

        if std::env::var_os("TAIRIX_QEMU_DEBUG").is_some() {
            eprintln!("tairix-qemu: {cmd:?}");
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

        // A windowed session must open a real host window. QEMU's *implicit*
        // default display is not portable — a build without GTK/SDL silently
        // falls back to a headless VNC server and no window ever appears — so
        // select a binary and windowing backend explicitly and fail loud if
        // none can present a window.
        let interactive = (spec.session == SessionKind::WindowedInteractive)
            .then(|| display::select_interactive(spec.arch.qemu_binary()))
            .transpose()
            .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;

        let program: PathBuf = match &interactive {
            Some(sel) => sel.binary.clone(),
            None => PathBuf::from(spec.arch.qemu_binary()),
        };
        let mut cmd = Command::new(&program);
        match spec.arch {
            Arch::X86_64 => x86_64::push_argv(&mut cmd, spec, &spec.kernel),
            Arch::Riscv64 => riscv64::push_argv(&mut cmd, spec, &spec.kernel),
            Arch::Aarch64 => aarch64::push_argv(&mut cmd, spec, &spec.kernel),
        }
        // The per-arch builders omit `-display` for a windowed session; the
        // chosen windowing backend is appended here, in one place, so the
        // selection logic is never duplicated across ports.
        if let Some(sel) = &interactive {
            cmd.arg("-display");
            cmd.arg(sel.backend.qemu_name());
            eprintln!(
                "tairix-qemu: [run] interactive display via {} -display {}",
                program.display(),
                sel.backend.qemu_name(),
            );
        }
        for a in &spec.extra_args {
            cmd.arg(a);
        }
        if std::env::var_os("TAIRIX_QEMU_DEBUG").is_some() {
            eprintln!("tairix-qemu: {cmd:?}");
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
        typing_markers_seen,
        pointer_markers_seen,
        screendump_markers_seen,
        reader,
    } = spawn_serial_drain(&mut child, spec);
    let mut reader = Some(reader);

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
    let mut err_reader = Some(err_reader);

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
    let mut serial_script = SerialScriptState::default();
    let mut serial_closed = false;
    let done = 'run: loop {
        if let Some(status) = child.try_wait()? {
            break 'run exit_reason(
                spec,
                serial_script.step,
                injections.pointer_done(spec),
                injections.typing_done(spec),
                injections.screendumps_verified(spec),
                status,
            );
        }
        if let Some(result) = completed_drain_result(&mut reader, "serial output") {
            serial_closed = result.is_ok();
            if let Err(reason) = result {
                let _ = child.kill();
                let _ = child.wait();
                break 'run DoneReason::DrainFailed(reason);
            }
        }
        if let Some(Err(reason)) = completed_drain_result(&mut err_reader, "qemu stderr") {
            let _ = child.kill();
            let _ = child.wait();
            break 'run DoneReason::DrainFailed(reason);
        }
        if let Err(e) = injections.drive(
            spec,
            monitor,
            &marker_seen,
            &typing_markers_seen,
            &pointer_markers_seen,
            &screendump_markers_seen,
        ) {
            let _ = child.kill();
            let _ = child.wait();
            break 'run DoneReason::InjectionFailed(e);
        }
        let advanced = advance_serial_script(
            &spec.serial_input,
            &captured,
            &mut serial_stdin,
            &mut serial_script,
            Instant::now(),
        );
        if let Err(e) = advanced {
            let _ = child.kill();
            let _ = child.wait();
            break 'run DoneReason::InjectionFailed(format!(
                "serial input injection failed at step {}: {e}",
                serial_script.step
            ));
        }
        if Instant::now() >= deadline {
            // Strict, no-retry kill. `wait` afterwards is best
            // effort so we don't leave a zombie behind.
            let _ = child.kill();
            let _ = child.wait();
            break 'run if serial_closed {
                DoneReason::DrainFailed(String::from("serial output closed before QEMU exited"))
            } else {
                DoneReason::TimedOut
            };
        }
        std::thread::sleep(tick);
    };

    // The child has exited (or been killed); the reader thread sees
    // EOF on the closed pipe and finishes. Drop the monitor connections
    // and the guest's serial input pipe only now, once the run is
    // complete.
    drop(injections);
    drop(serial_stdin);
    let drain_failure = finish_drain(reader.take(), "serial output")
        .or_else(|| finish_drain(err_reader.take(), "qemu stderr"));
    let done = match drain_failure {
        Some(reason) => DoneReason::DrainFailed(reason),
        None => done,
    };
    let mut serial = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    append_stderr(&mut serial, &captured_err);
    Ok(outcome_from_done(done, spec, serial))
}

/// Convert the completed supervision reason and captured output into the
/// architecture-specific public outcome.
fn outcome_from_done(done: DoneReason, spec: &Spec, mut serial: String) -> Outcome {
    match done {
        DoneReason::Exited(code) => spec.arch.outcome_from_status(code, serial),
        DoneReason::TimedOut => Outcome::Timeout {
            budget: spec.timeout,
            serial,
        },
        DoneReason::InjectionFailed(reason) | DoneReason::DrainFailed(reason) => {
            // The failure message rides the serial log so the report
            // explains *why* the run was cut short, exactly as a guest
            // diagnostic would.
            if !serial.is_empty() && !serial.ends_with('\n') {
                serial.push('\n');
            }
            serial.push_str("tairix-qemu: ");
            serial.push_str(&reason);
            serial.push('\n');
            Outcome::Fail { status: -1, serial }
        }
    }
}

/// Advance the ordered serial-input script as far as the captured serial
/// log currently allows: for each remaining step whose readiness marker
/// has arrived *past the previous step's match*, write its line to the
/// guest's serial input and move the cursor on. Matching only ever
/// advances through the log, so a repeated prompt anchors its own step.
///
/// `state` carries the script cursor, pending delay, and byte position between
/// poll ticks;
/// the matched end of a marker is a UTF-8 boundary (it follows a complete
/// marker match), so the next slice start is always valid.
///
/// # Errors
///
/// Returns the write error when the guest's stdin pipe is missing or the
/// write/flush fails; the caller turns it into an injection failure.
#[derive(Default)]
struct SerialScriptState {
    step: usize,
    search_from: usize,
    matched: Option<PendingSerialStep>,
}

struct PendingSerialStep {
    matched_end: usize,
    send_at: Instant,
    byte: usize,
}

fn advance_serial_script<W: Write>(
    steps: &[SerialInjection],
    captured: &Mutex<String>,
    serial_stdin: &mut Option<W>,
    state: &mut SerialScriptState,
    now: Instant,
) -> io::Result<()> {
    while state.step < steps.len() {
        let serial_step = &steps[state.step];
        if state.matched.is_none() {
            let found = {
                let log = captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                log[state.search_from..]
                    .find(&serial_step.ready_marker)
                    .map(|at| state.search_from + at + serial_step.ready_marker.len())
            };
            let Some(matched_end) = found else { break };
            state.matched = Some(PendingSerialStep {
                matched_end,
                send_at: now + serial_step.delay_after_marker,
                byte: 0,
            });
        }
        let Some(pending) = state.matched.as_mut() else {
            break;
        };
        if now < pending.send_at {
            break;
        }
        if let Some(byte) = serial_step.line.as_bytes().get(pending.byte) {
            // Safe to use stdin: `run` piped it because the script is
            // non-empty. One byte per supervision tick models human typing
            // and cannot burst an emulated UART FIFO with a whole line.
            let stdin = serial_stdin
                .as_mut()
                .ok_or_else(|| io::Error::other("qemu stdin pipe missing"))?;
            stdin.write_all(core::slice::from_ref(byte))?;
            stdin.flush()?;
            pending.byte += 1;
            if pending.byte < serial_step.line.len() {
                break;
            }
        }
        state.search_from = pending.matched_end;
        state.step += 1;
        state.matched = None;
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
    /// A QEMU output drain failed, panicked, or closed before the child;
    /// the child was killed. The message identifies the failed channel.
    DrainFailed(String),
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
            std::env::temp_dir().join(format!("tairix-qemu-mon-{}-{}.sock", std::process::id(), n));
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
    /// One flag per typed-text step, set once that step's readiness
    /// marker has appeared the required number of times. Index-aligned
    /// with [`Spec::input_typing`].
    typing_markers_seen: Vec<Arc<AtomicBool>>,
    /// One flag per pointer-script step, set once that step's readiness
    /// marker has appeared the required number of times. Index-aligned
    /// with [`Spec::pointer_script`].
    pointer_markers_seen: Vec<Arc<AtomicBool>>,
    /// One flag per screendump, set once that dump's readiness marker
    /// has appeared the required number of times. Index-aligned with
    /// [`Spec::screendumps`].
    screendump_markers_seen: Vec<Arc<AtomicBool>>,
    /// The drain thread, joined once the child has exited.
    reader: std::thread::JoinHandle<io::Result<()>>,
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
    let typing_markers_seen: Vec<Arc<AtomicBool>> = spec
        .input_typing
        .iter()
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();
    let pointer_markers_seen: Vec<Arc<AtomicBool>> = spec
        .pointer_script
        .iter()
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();
    let screendump_markers_seen: Vec<Arc<AtomicBool>> = spec
        .screendumps
        .iter()
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();
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
        for (t, seen) in spec.input_typing.iter().zip(&typing_markers_seen) {
            markers.push((
                t.ready_marker.clone(),
                t.ready_occurrences.max(1),
                Arc::clone(seen),
            ));
        }
        for (p, seen) in spec.pointer_script.iter().zip(&pointer_markers_seen) {
            markers.push((
                p.ready_marker.clone(),
                p.ready_occurrences.max(1),
                Arc::clone(seen),
            ));
        }
        for (d, seen) in spec.screendumps.iter().zip(&screendump_markers_seen) {
            markers.push((
                d.ready_marker.clone(),
                d.ready_occurrences.max(1),
                Arc::clone(seen),
            ));
        }
        std::thread::spawn(move || drain_stream(stdout, &captured, &markers))
    };
    SerialDrain {
        captured,
        marker_seen,
        typing_markers_seen,
        pointer_markers_seen,
        screendump_markers_seen,
        reader,
    }
}

/// Read one of QEMU's output pipes to EOF, appending every chunk to
/// `captured` and raising each marker's flag once its substring has
/// appeared the required number of times in the stream so far.
/// An interrupted read is retried. Any other read error is returned to
/// [`supervise`], which fails the run rather than silently losing the serial
/// channel and waiting for an unrelated guest timeout.
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
) -> io::Result<()> {
    let Some(mut r) = stream else { return Ok(()) };
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
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

/// Join a finished output-drain thread, distinguishing clean EOF from a
/// fail-loud read or panic diagnostic.
fn completed_drain_result(
    reader: &mut Option<std::thread::JoinHandle<io::Result<()>>>,
    channel: &str,
) -> Option<Result<(), String>> {
    if !reader
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
    {
        return None;
    }
    let handle = reader.take()?;
    Some(drain_join_result(handle.join(), channel))
}

/// Join an output drain after QEMU has exited, preserving read and panic
/// failures as harness diagnostics.
fn finish_drain(
    reader: Option<std::thread::JoinHandle<io::Result<()>>>,
    channel: &str,
) -> Option<String> {
    let handle = reader?;
    drain_join_result(handle.join(), channel).err()
}

/// Decode one joined output-drain result without conflating clean EOF, read
/// failure, and thread panic.
fn drain_join_result(
    joined: std::thread::Result<io::Result<()>>,
    channel: &str,
) -> Result<(), String> {
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{channel} drain failed: {e}")),
        Err(_) => Err(format!("{channel} drain thread panicked")),
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
    /// The pointer-script step to send next (`pointer_script.len()` once
    /// every step was sent, or when no script was requested).
    pointer_step: usize,
    /// The pointer buttons currently held down, as the HMP `mouse_button`
    /// state mask — tracked so a press adds its bit and a release removes
    /// only its own, exactly as a physical mouse would report.
    pointer_button_mask: u32,
    /// The typed-text step currently being typed (`input_typing.len()`
    /// once every step finished, or when no typing was requested).
    typed_step: usize,
    /// Characters of the current step already sent.
    typed_in_step: usize,
    /// The earliest instant the next typed key may be sent — the pacing
    /// that keeps every press/release pair distinct on the device.
    next_typed_key_at: Instant,
    /// The screendump to take next (`screendumps.len()` once every dump
    /// verified, or when none was requested).
    dump_step: usize,
    /// Progress of the current ([`Self::dump_step`]) screendump.
    dump_state: DumpState,
    conn: Option<UnixStream>,
}

/// Progress of the current screendump, in order: the command has not been
/// sent, it has been sent but the file has not yet parsed completely, and
/// the dumped image has been read back and fully parsed — at which point
/// the cursor advances to the next requested dump. A pending (sent but
/// unverified) dump holds every still-unsent pointer step back.
#[derive(Clone, Copy, Eq, PartialEq)]
enum DumpState {
    NotSent,
    Sent,
}

/// Milliseconds a typed key is held down (`sendkey <key> <hold>`).
const TYPED_KEY_HOLD_MS: u32 = 40;

/// Minimum interval between consecutive typed keys. Strictly longer than
/// [`TYPED_KEY_HOLD_MS`], so each key's release lands before the next
/// press — a repeated character ("tt") is two clean edges, never a
/// redundant press the device would coalesce.
const TYPED_KEY_INTERVAL: Duration = Duration::from_millis(80);

impl InjectionState {
    /// Each cursor starts "done" only through its list being empty, so
    /// [`Self::drive`] only ever acts on requested injections.
    fn new(spec: &Spec) -> Self {
        Self {
            key_sent: spec.input_keyboard.is_none(),
            pointer_step: 0,
            pointer_button_mask: 0,
            typed_step: 0,
            typed_in_step: 0,
            next_typed_key_at: Instant::now(),
            dump_step: 0,
            dump_state: DumpState::NotSent,
            conn: None,
        }
    }

    /// `true` once every requested screendump has been read back and
    /// fully parsed — the fact [`exit_reason`] requires of a spec that
    /// asked for any.
    fn screendumps_verified(&self, spec: &Spec) -> bool {
        self.dump_step >= spec.screendumps.len()
    }

    /// `true` once every requested pointer step has been sent.
    fn pointer_done(&self, spec: &Spec) -> bool {
        self.pointer_step >= spec.pointer_script.len()
    }

    /// `true` once every requested typed-text step has been fully sent.
    fn typing_done(&self, spec: &Spec) -> bool {
        self.typed_step >= spec.input_typing.len()
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
        typing_seen: &[Arc<AtomicBool>],
        pointer_seen: &[Arc<AtomicBool>],
        screendump_seen: &[Arc<AtomicBool>],
    ) -> Result<(), String> {
        // Safe to unwrap inside the closures: each `*_sent` flag is only
        // `false` when its injection request and `monitor` are both `Some`.
        if !self.key_sent && key_seen.load(Ordering::Acquire) {
            let key = &spec.input_keyboard.as_ref().expect("key present").key;
            self.send(monitor, "key", &format!("sendkey {key}"))?;
            self.key_sent = true;
        }
        if let Some(typing) = spec.input_typing.get(self.typed_step) {
            // Steps run strictly in order, each gated on its own marker;
            // within a step the keys are paced — at most one per call,
            // and only once the previous key's hold has fully elapsed —
            // so repeated characters arrive as distinct press/release
            // edges.
            let step_seen = typing_seen
                .get(self.typed_step)
                .is_some_and(|seen| seen.load(Ordering::Acquire));
            if step_seen && Instant::now() >= self.next_typed_key_at {
                if let Some(c) = typing.text.chars().nth(self.typed_in_step) {
                    let key =
                        qkeycode_for(c).map_err(|e| format!("typed-text injection failed: {e}"))?;
                    self.send(
                        monitor,
                        "typed-text",
                        &format!("sendkey {key} {TYPED_KEY_HOLD_MS}"),
                    )?;
                    self.typed_in_step += 1;
                    self.next_typed_key_at = Instant::now() + TYPED_KEY_INTERVAL;
                    // A fully-typed step advances immediately so an
                    // already-seen next marker starts the next step on
                    // the following tick.
                    if self.typed_in_step >= typing.text.chars().count() {
                        self.typed_step += 1;
                        self.typed_in_step = 0;
                    }
                } else {
                    // An empty step is complete the moment its marker is
                    // seen (nothing to type).
                    self.typed_step += 1;
                    self.typed_in_step = 0;
                }
            }
        }
        if let Some(dump) = spec.screendumps.get(self.dump_step) {
            let dump_seen = screendump_seen
                .get(self.dump_step)
                .is_some_and(|seen| seen.load(Ordering::Acquire));
            if self.dump_state == DumpState::NotSent && dump_seen {
                self.send(
                    monitor,
                    "screendump",
                    &format!("screendump {}", dump.path.display()),
                )?;
                self.dump_state = DumpState::Sent;
            }
            // QEMU writes the PPM inside the monitor command, but the
            // write races this poll loop: the dump is trusted only once
            // the file on disk parses as a complete image. Until then any
            // still-unsent pointer step — and every later dump — stays
            // held back, so a dump can never capture a frame its script
            // position has already moved past.
            if self.dump_state == DumpState::Sent {
                if let Ok(bytes) = std::fs::read(&dump.path) {
                    if screendump::parse_ppm(&bytes).is_ok() {
                        self.dump_step += 1;
                        self.dump_state = DumpState::NotSent;
                    }
                }
            }
        }
        // A pointer step fires strictly in script order, once its own
        // marker was seen and no earlier-requested dump is pending: a dump
        // whose marker has appeared must hold the pixels it was keyed on
        // before any further injection can change the screen. At most one
        // step per poll tick, so consecutive actions arrive as distinct
        // device events.
        let dump_pending = spec.screendumps.get(self.dump_step).is_some_and(|_| {
            self.dump_state == DumpState::Sent
                || screendump_seen
                    .get(self.dump_step)
                    .is_some_and(|seen| seen.load(Ordering::Acquire))
        });
        if let Some(step) = spec.pointer_script.get(self.pointer_step) {
            let step_seen = pointer_seen
                .get(self.pointer_step)
                .is_some_and(|seen| seen.load(Ordering::Acquire));
            if step_seen && !dump_pending {
                let command = match step.action {
                    PointerAction::Move { dx, dy } => format!("mouse_move {dx} {dy}"),
                    PointerAction::Press(button) => {
                        self.pointer_button_mask |= button.mask_bit();
                        format!("mouse_button {}", self.pointer_button_mask)
                    }
                    PointerAction::Release(button) => {
                        self.pointer_button_mask &= !button.mask_bit();
                        format!("mouse_button {}", self.pointer_button_mask)
                    }
                };
                self.send(monitor, "pointer", &command)?;
                self.pointer_step += 1;
            }
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
/// serial-input script completed, before every requested pointer step was
/// sent, before a requested typed-text script finished, or before every
/// requested screendump was taken and verified, means the exchange the
/// vertical was meant to prove never happened — the run fails even when
/// the guest itself reported success. Otherwise the guest's own exit
/// status decides the outcome.
fn exit_reason(
    spec: &Spec,
    serial_step: usize,
    pointer_done: bool,
    typing_done: bool,
    screendumps_verified: bool,
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
    if !pointer_done {
        return DoneReason::InjectionFailed(
            "pointer script incomplete: a step's readiness marker was not seen (or a dump it \
             waited on never verified) before exit"
                .into(),
        );
    }
    if !typing_done {
        return DoneReason::InjectionFailed(
            "typed-text script incomplete: its readiness marker was not seen (or the guest \
             exited mid-script)"
                .into(),
        );
    }
    if !spec.screendumps.is_empty() && !screendumps_verified {
        return DoneReason::InjectionFailed(
            "screendumps incomplete: a dump's readiness marker was not seen, or the guest \
             exited before its dumped image parsed completely"
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
            .with_serial_input("Username: ", Duration::ZERO, "root\n")
            .with_serial_input("Password: ", Duration::from_secs(1), "wrong\n");
        assert_eq!(s.serial_input.len(), 2);
        assert_eq!(s.serial_input[0].ready_marker, "Username: ");
        assert_eq!(s.serial_input[0].delay_after_marker, Duration::ZERO);
        assert_eq!(s.serial_input[0].line, "root\n");
        assert_eq!(s.serial_input[1].ready_marker, "Password: ");
        assert_eq!(s.serial_input[1].delay_after_marker, Duration::from_secs(1));
        assert_eq!(s.serial_input[1].line, "wrong\n");
    }

    #[test]
    fn serial_script_waits_for_each_steps_declared_delay() {
        let start = Instant::now();
        let steps = [SerialInjection {
            ready_marker: String::from("root@tairix ~% "),
            delay_after_marker: Duration::from_secs(1),
            line: String::from("s"),
        }];
        let captured = Mutex::new(String::from("root@tairix ~% "));
        let mut output = Some(Vec::new());
        let mut state = SerialScriptState::default();

        advance_serial_script(&steps, &captured, &mut output, &mut state, start)
            .expect("observe marker");
        assert_eq!(state.step, 0, "step must remain pending during delay");
        assert!(output.as_ref().expect("writer").is_empty());

        advance_serial_script(
            &steps,
            &captured,
            &mut output,
            &mut state,
            start + Duration::from_millis(999),
        )
        .expect("remain delayed");
        assert_eq!(state.step, 0);
        assert!(output.as_ref().expect("writer").is_empty());

        advance_serial_script(
            &steps,
            &captured,
            &mut output,
            &mut state,
            start + Duration::from_secs(1),
        )
        .expect("send after delay");
        assert_eq!(state.step, 1);
        assert_eq!(output.expect("writer"), b"s");
    }

    #[test]
    fn serial_script_types_at_most_one_byte_per_supervision_tick() {
        let start = Instant::now();
        let steps = [SerialInjection {
            ready_marker: String::from("prompt"),
            delay_after_marker: Duration::ZERO,
            line: String::from("ab"),
        }];
        let captured = Mutex::new(String::from("prompt"));
        let mut output = Some(Vec::new());
        let mut state = SerialScriptState::default();

        advance_serial_script(&steps, &captured, &mut output, &mut state, start)
            .expect("type first byte");
        assert_eq!(state.step, 0, "the second byte remains pending");
        assert_eq!(output.as_ref().expect("writer"), b"a");

        advance_serial_script(&steps, &captured, &mut output, &mut state, start)
            .expect("type second byte on the next tick");
        assert_eq!(state.step, 1);
        assert_eq!(output.expect("writer"), b"ab");
    }

    #[test]
    fn without_serial_input_records_no_injection() {
        let s = Spec::for_aarch64_kernel("/tmp/k");
        assert!(s.serial_input.is_empty());
    }

    struct InterruptedOnceReader {
        step: u8,
    }

    impl Read for InterruptedOnceReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let bytes = match self.step {
                0 => b"boot\n".as_slice(),
                1 => {
                    self.step += 1;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                2 => b"ready marker\n".as_slice(),
                _ => return Ok(0),
            };
            self.step += 1;
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    #[test]
    fn drain_stream_retries_an_interrupted_read() {
        let captured = Mutex::new(String::new());
        let seen = Arc::new(AtomicBool::new(false));
        let markers = [(String::from("ready marker"), 1, Arc::clone(&seen))];
        let reader = InterruptedOnceReader { step: 0 };

        drain_stream(Some(reader), &captured, &markers).expect("drain after interruption");

        assert!(seen.load(Ordering::Acquire));
        assert_eq!(
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_str(),
            "boot\nready marker\n"
        );
    }

    struct FailedReader;

    impl Read for FailedReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure"))
        }
    }

    #[test]
    fn drain_stream_propagates_a_hard_read_error() {
        let captured = Mutex::new(String::new());
        let err = drain_stream(Some(FailedReader), &captured, &[]).expect_err("hard read error");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert!(err.to_string().contains("fixture failure"));
    }

    #[test]
    fn drain_join_distinguishes_clean_eof_from_failure() {
        assert_eq!(drain_join_result(Ok(Ok(())), "serial output"), Ok(()));

        let read_error = io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure");
        let err = drain_join_result(Ok(Err(read_error)), "serial output")
            .expect_err("hard drain failure");
        assert!(err.contains("serial output drain failed"));

        let panic: std::thread::Result<io::Result<()>> = Err(Box::new("fixture panic"));
        let err = drain_join_result(panic, "qemu stderr").expect_err("drain panic");
        assert_eq!(err, "qemu stderr drain thread panicked");
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
            (String::from("tairix$ "), 1, Arc::clone(&second)),
        ];
        let feed: &[u8] = b"boot ok\nqueue armed\nbanner\ntairix$ ";
        drain_stream(Some(feed), &captured, &markers).expect("drain markers");
        assert!(first.load(Ordering::Acquire));
        assert!(second.load(Ordering::Acquire));
        assert_eq!(
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_str(),
            "boot ok\nqueue armed\nbanner\ntairix$ "
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
        drain_stream(Some(first_only), &captured, &markers).expect("drain first marker");
        assert!(
            !seen.load(Ordering::Acquire),
            "one occurrence must not satisfy a two-occurrence marker"
        );
        let second: &[u8] = b"more\nsc=irq_bind task=8\n";
        drain_stream(Some(second), &captured, &markers).expect("drain second marker");
        assert!(seen.load(Ordering::Acquire));
    }

    #[test]
    fn with_typed_keys_records_the_script_and_clamps_occurrences() {
        let s = Spec::for_aarch64_kernel("/tmp/k")
            .with_typed_keys("armed", 0, "root\n")
            .with_typed_keys("db loaded", 1, "g\n");
        assert_eq!(s.input_typing.len(), 2, "steps append in order");
        let t = &s.input_typing[0];
        assert_eq!(t.ready_marker, "armed");
        assert_eq!(t.ready_occurrences, 1, "occurrences clamp at >= 1");
        assert_eq!(t.text, "root\n");
        let t = &s.input_typing[1];
        assert_eq!(t.ready_marker, "db loaded");
        assert_eq!(t.ready_occurrences, 1);
        assert_eq!(t.text, "g\n");
    }

    #[test]
    fn with_screendump_records_ordered_requests_and_clamps_occurrences() {
        let s = Spec::for_aarch64_kernel("/tmp/k")
            .with_screendump("presented", 0, "/tmp/d1.ppm")
            .with_screendump("window served", 2, "/tmp/d2.ppm");
        assert_eq!(s.screendumps.len(), 2, "dumps append in order");
        let d = &s.screendumps[0];
        assert_eq!(d.ready_marker, "presented");
        assert_eq!(d.ready_occurrences, 1, "occurrences clamp at >= 1");
        assert_eq!(d.path, PathBuf::from("/tmp/d1.ppm"));
        let d = &s.screendumps[1];
        assert_eq!(d.ready_marker, "window served");
        assert_eq!(d.ready_occurrences, 2);
        assert_eq!(d.path, PathBuf::from("/tmp/d2.ppm"));
    }

    #[test]
    fn with_pointer_step_records_the_ordered_script_and_attaches_the_mouse() {
        let s = Spec::for_aarch64_kernel("/tmp/k")
            .with_pointer_step(
                "presented",
                0,
                PointerAction::Move {
                    dx: -9999,
                    dy: -9999,
                },
            )
            .with_pointer_step("presented", 1, PointerAction::Press(MouseButton::Primary))
            .with_pointer_step("menu open", 1, PointerAction::Release(MouseButton::Primary));
        assert!(s.input_mouse, "a pointer script implies the mouse device");
        assert_eq!(s.pointer_script.len(), 3, "steps append in order");
        let p = &s.pointer_script[0];
        assert_eq!(p.ready_marker, "presented");
        assert_eq!(p.ready_occurrences, 1, "occurrences clamp at >= 1");
        assert_eq!(
            p.action,
            PointerAction::Move {
                dx: -9999,
                dy: -9999
            }
        );
        assert_eq!(
            s.pointer_script[1].action,
            PointerAction::Press(MouseButton::Primary)
        );
        assert_eq!(
            s.pointer_script[2].action,
            PointerAction::Release(MouseButton::Primary)
        );
    }

    #[test]
    fn mouse_button_mask_bits_match_qemus_actual_button_decode() {
        // The bits QEMU's `hmp_mouse_button` actually decodes (via the
        // legacy `MOUSE_EVENT_*` `bmap`), *not* the wrong help string
        // ("1=L, 2=M, 4=R"): state bit 0x2 raises the right button and
        // 0x4 the middle. A press ORs its bit in, a release clears only
        // its own, so overlapping holds report faithfully.
        assert_eq!(MouseButton::Primary.mask_bit(), 0x1);
        assert_eq!(MouseButton::Secondary.mask_bit(), 0x2);
        assert_eq!(MouseButton::Middle.mask_bit(), 0x4);
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
    fn with_virtio_net_dgram_records_the_socket_pair_and_capture_path() {
        let s = Spec::for_riscv64_kernel("/tmp/k").with_virtio_net_dgram(
            "/tmp/net.qemu.sock",
            "/tmp/net.peer.sock",
            "/tmp/cap.pcap",
        );
        assert_eq!(s.net_devices.len(), 1);
        assert_eq!(
            s.net_devices[0].qemu_sock,
            PathBuf::from("/tmp/net.qemu.sock")
        );
        assert_eq!(
            s.net_devices[0].peer_sock,
            PathBuf::from("/tmp/net.peer.sock")
        );
        assert_eq!(s.net_devices[0].pcap, Some(PathBuf::from("/tmp/cap.pcap")));
    }

    #[test]
    fn net_interfaces_accumulate_in_declaration_order() {
        let s = Spec::for_x86_64_kernel("/tmp/k")
            .with_virtio_net_dgram("/tmp/a.qemu.sock", "/tmp/a.peer.sock", "/tmp/a.pcap")
            .with_virtio_net_dgram("/tmp/b.qemu.sock", "/tmp/b.peer.sock", "/tmp/b.pcap");
        assert_eq!(s.net_devices.len(), 2);
        assert_eq!(
            s.net_devices[0].qemu_sock,
            PathBuf::from("/tmp/a.qemu.sock")
        );
        assert_eq!(
            s.net_devices[1].qemu_sock,
            PathBuf::from("/tmp/b.qemu.sock")
        );
    }

    #[test]
    fn netdev_dgram_arg_renders_both_socket_paths() {
        let dev = NetDevice {
            qemu_sock: PathBuf::from("/tmp/net7.qemu.sock"),
            peer_sock: PathBuf::from("/tmp/net7.peer.sock"),
            pcap: None,
            mac: None,
        };
        assert_eq!(
            netdev_dgram_arg(7, &dev),
            OsString::from(
                "dgram,id=net7,local.type=unix,local.path=/tmp/net7.qemu.sock,\
                 remote.type=unix,remote.path=/tmp/net7.peer.sock"
            )
        );
    }

    #[test]
    fn net_device_arg_renders_driver_mac_and_extra() {
        let base = NetDevice {
            qemu_sock: PathBuf::from("/tmp/n.qemu.sock"),
            peer_sock: PathBuf::from("/tmp/n.peer.sock"),
            pcap: None,
            mac: None,
        };
        // No MAC: just the driver and netdev id (plus any extra suffix).
        assert_eq!(
            net_device_arg("virtio-net-pci", 0, &base, ",disable-legacy=on"),
            OsString::from("virtio-net-pci,netdev=net0,disable-legacy=on")
        );
        // A pinned MAC is threaded verbatim, before the extra suffix.
        let mac = NetDevice {
            mac: Some("52:54:00:00:00:15".into()),
            ..base
        };
        assert_eq!(
            net_device_arg("virtio-net-device", 1, &mac, ""),
            OsString::from("virtio-net-device,netdev=net1,mac=52:54:00:00:00:15")
        );
    }

    #[test]
    fn with_virtio_net_dgram_mac_pins_the_device_mac() {
        let s = Spec::for_riscv64_kernel("/tmp/k").with_virtio_net_dgram_mac(
            "/tmp/a.qemu.sock",
            "/tmp/a.peer.sock",
            "/tmp/a.pcap",
            "52:54:00:00:00:15",
        );
        assert_eq!(s.net_devices.len(), 1);
        assert_eq!(s.net_devices[0].mac.as_deref(), Some("52:54:00:00:00:15"));
    }

    #[test]
    fn missing_backing_image_returns_not_found_before_spawning_qemu() {
        // Plant a real (empty) kernel file so the failure is attributable
        // to the missing backing image, not to a missing kernel.
        let kernel =
            std::env::temp_dir().join(format!("tairix-qemu-kernel-{}.elf", std::process::id()));
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
            input_typing: Vec::new(),
            input_mouse: false,
            pointer_script: Vec::new(),
            serial_input: Vec::new(),
            screendumps: Vec::new(),
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
