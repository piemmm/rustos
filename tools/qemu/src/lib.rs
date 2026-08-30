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
//! * Enforces an *inactivity* deadline supplied by the caller: a guest is
//!   declared hung once it produces no new serial output for the whole
//!   budget. Every line the guest prints resets it, so a guest that is merely
//!   slow (heavily co-scheduled, or on a slow host core) is never killed while
//!   it keeps making progress. That immunity to host load is what lets the
//!   matrix run many guests at once without a slow one degrading into a flaky
//!   timeout. A guest that genuinely falls silent for the budget is
//!   `Outcome::Timeout`, which the runner converts into a failure — never
//!   into a retry.
//! * Enforces an absolute wall-clock ceiling too ([`Spec::runtime_ceiling`]),
//!   because the heartbeat alone cannot bound a guest that keeps *talking*
//!   while making no progress — a service retrying a failed request on a
//!   timer resets the heartbeat forever, and one such guest would otherwise
//!   stall the whole matrix behind it indefinitely. Exceeding it is
//!   `Outcome::RuntimeCeilingExceeded`, reported apart from `Timeout` because
//!   the two describe different faults. A run whose *success* requires long
//!   continuous work — a whole-RAM memtest sweep — declares its own ceiling
//!   ([`Spec::with_runtime_ceiling`]) rather than inheriting the multiple of a
//!   silence budget that describes no part of its work.
//! * Kills (`SIGKILL`) the QEMU child if either deadline is hit so a wedged VM
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

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tairix_binfmt::elf::{ElfView, SHT_SYMTAB};

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
    ///
    /// A pass carries its transcript like every other outcome: the guest
    /// exiting successfully is not the end of a run's verification — a
    /// screendump the caller asserts afterwards can still fail, and
    /// discarding the transcript on the way there leaves that failure with
    /// no evidence to explain it.
    Pass {
        /// Captured QEMU stdout for the whole run.
        serial: String,
    },
    /// The kernel signalled a non-success value, or QEMU exited with an
    /// unexpected status. `serial` holds the captured QEMU stdout for the
    /// failure report.
    Fail {
        /// QEMU exit status, as returned by the OS.
        status: i32,
        /// Captured QEMU stdout (serial-over-stdio), best-effort.
        serial: String,
    },
    /// The guest produced no new serial output for the whole inactivity
    /// budget before exiting; it was treated as hung and QEMU was killed.
    Timeout {
        /// No-progress (inactivity) budget the test was given: the longest
        /// the guest may fall silent before it is declared hung.
        budget: Duration,
        /// Captured QEMU stdout up to the kill, best-effort.
        serial: String,
        /// Every vCPU's register file, read off the QEMU monitor at the
        /// moment the guest was declared hung, with the kernel-text
        /// addresses it names resolved against the kernel ELF.
        cpu_state: String,
    },
    /// The run reached its absolute wall-clock ceiling
    /// ([`Spec::runtime_ceiling`]) while the guest was still alive and had
    /// not been silent long enough to be declared hung.
    ///
    /// This is the bound that catches a guest which keeps *talking* but never
    /// finishes — a service retrying a failed request on a timer, a
    /// choreography waiting on a witness that will never arrive, or a gated
    /// run whose out-of-guest observer never confirmed the round trip. The
    /// inactivity heartbeat can never fire for such a guest, so without this
    /// ceiling the run would never end.
    ///
    /// Reported separately from [`Self::Timeout`] because the two say
    /// different things about the guest, and conflating them costs a
    /// diagnosis: a hung guest stopped talking, whereas this guest was
    /// running and simply never completed. `silent_for` is how long it had
    /// produced no serial output when it was killed, which is the number that
    /// tells the two apart on sight — a value near zero means a live guest
    /// that kept working and never completed, while a value approaching
    /// `ceiling` means it went quiet early and stalled at a fixed point.
    RuntimeCeilingExceeded {
        /// Absolute wall-clock ceiling the run was given.
        ceiling: Duration,
        /// How long the guest had produced no serial output at the kill.
        silent_for: Duration,
        /// Captured QEMU stdout up to the kill, best-effort.
        serial: String,
        /// Every vCPU's register file, read off the QEMU monitor at the
        /// moment the run was killed, with the kernel-text addresses it
        /// names resolved against the kernel ELF.
        cpu_state: String,
    },
}

impl Outcome {
    /// Decode a QEMU exit status under the `isa-debug-exit` convention.
    ///
    /// Returns `Outcome::Pass` iff `status == (SUCCESS_EXIT_CODE << 1) | 1`.
    /// Every other status is treated as `Outcome::Fail`. Both carry the
    /// captured serial log.
    #[must_use]
    pub fn from_qemu_status(status: i32, serial: String) -> Self {
        let success_status = i32::from((SUCCESS_EXIT_CODE << 1) | 1);
        if status == success_status {
            Outcome::Pass { serial }
        } else {
            Outcome::Fail { status, serial }
        }
    }

    /// Returns `true` only for `Outcome::Pass`. Convenience for runners that
    /// turn the outcome into a process exit code.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass { .. })
    }

    /// The run's captured transcript, whichever outcome it reached.
    ///
    /// Every variant carries one, so a caller that persists the transcript
    /// as evidence does it once for the run rather than once per verdict —
    /// including for a pass, which is the outcome whose evidence a reader is
    /// most likely to want and least likely to have.
    #[must_use]
    pub fn serial(&self) -> &str {
        match self {
            Outcome::Pass { serial }
            | Outcome::Fail { serial, .. }
            | Outcome::Timeout { serial, .. }
            | Outcome::RuntimeCeilingExceeded { serial, .. } => serial,
        }
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
    /// Press *and* release the button, as one step.
    ///
    /// A click is one gesture, and this is how a script says so. Scripting it
    /// as a [`Press`](Self::Press) step followed by a
    /// [`Release`](Self::Release) step splits it across two poll ticks, which
    /// opens a window a guest can exit inside: a desktop acts on the press, so
    /// a guest whose PASS witness is that press's own effect races the release
    /// it has not been sent yet — and a witness that lands first fails the run
    /// for an incomplete script. Sending both mask changes in one tick closes
    /// that window without coalescing anything: the device still sees two
    /// distinct `mouse_button` events, in order.
    ///
    /// [`Press`](Self::Press) and [`Release`](Self::Release) remain for a
    /// gesture whose *duration* is the point — a tap-or-hold, a drag — where
    /// the steps between them are exactly what the script is expressing.
    Click(MouseButton),
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

/// A deterministic, marker-gated raw QEMU-monitor command.
///
/// The generic sibling of [`Screendump`] and [`KeyInjection`]: once
/// [`ready_marker`](Self::ready_marker) has appeared the required number of
/// times on the serial console, the runner sends [`command`](Self::command)
/// verbatim over the QEMU human monitor exactly once. It exists for
/// deterministic device-state changes a guest cannot make itself — the bond
/// failover vertical uses `set_link net0 off` to drop the active member's
/// carrier mid-flow (QEMU raises the guest's virtio config-change interrupt,
/// the driver reports the link down, the bond fails over). Commands run
/// strictly in declaration order, each once its own marker is seen; a run
/// that exits before every command was sent fails the run, so an unreached
/// marker is a test failure, never a silent skip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorCommand {
    /// Serial-console substring the runner waits for before sending.
    pub ready_marker: String,
    /// How many times [`ready_marker`](Self::ready_marker) must appear
    /// before the command is sent (minimum 1).
    pub ready_occurrences: u32,
    /// The human-monitor command line to send (no trailing newline; the
    /// runner appends it).
    pub command: String,
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

/// Multiple of the inactivity budget that bounds a run's total wall clock
/// when the run does not declare a ceiling of its own.
///
/// The inactivity budget is already sized as the longest a *healthy* guest
/// may legitimately fall silent, which is an upper bound on any single phase
/// of its run; such a guest completes well inside one such budget. Two
/// whole budgets of wall clock therefore leave ample margin for host
/// co-scheduling — the matrix admits guests only up to a third of the host's
/// logical CPUs, so contention stretches a run by far less than this — while
/// still bounding a guest that has genuinely wedged.
///
/// The derivation holds only while total runtime really is a small multiple of
/// one phase. A guest whose success *is* one long continuous sweep breaks that
/// premise — its runtime scales with the work and with host load, not with how
/// long it may go quiet — so it declares an explicit ceiling instead
/// ([`Spec::with_runtime_ceiling`]). See [`Spec::runtime_ceiling`].
const RUNTIME_CEILING_BUDGETS: u32 = 2;

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
    /// Inactivity (no-progress) budget: the longest the guest may produce no
    /// new serial output before the runner treats it as hung and kills QEMU.
    /// It is *not* a total-runtime deadline — every line the guest prints
    /// resets it — so a guest that is merely slow (co-scheduled, or on a slow
    /// host core) is never killed while it keeps making progress.
    ///
    /// It is also not the run's only bound: [`Spec::runtime_ceiling`] caps
    /// total wall clock, because a guest that keeps printing while never
    /// completing resets this heartbeat forever.
    pub timeout: Duration,
    /// Absolute wall-clock ceiling this run declared for itself, for a guest
    /// whose success requires long continuous work. `None` derives one from
    /// [`Spec::timeout`]; set it through [`Spec::with_runtime_ceiling`] and
    /// read it through [`Spec::runtime_ceiling`], which is the only bound the
    /// runner enforces.
    declared_runtime_ceiling: Option<Duration>,
    /// Guest RAM in mebibytes. `None` takes the per-arch default; set it
    /// through [`Spec::with_ram_mib`] and read it through
    /// [`Spec::ram_mib`], which is what the argv builders emit.
    declared_ram_mib: Option<u32>,
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
    /// When non-empty, send each raw QEMU-monitor command in order, each
    /// once its readiness marker has appeared the required number of times
    /// on the serial console. Used for deterministic device-state changes a
    /// guest cannot make itself (e.g. `set_link net0 off` to fail a bond
    /// member over mid-flow). A run that exits before every command was
    /// sent fails.
    pub monitor_commands: Vec<MonitorCommand>,
    /// How the session presents itself to a human. Only the aarch64
    /// argv honours it today.
    pub session: SessionKind,
    /// When `Some(marker)`, a guest-initiated machine **reset** — QEMU
    /// exiting with process status `0` under `-no-reboot`, rather than an
    /// architecture-specific debug-exit success — is the intended success
    /// signal, accepted **only** when the captured serial also contains
    /// `marker`. This is how the pre-boot Supervisor's one-way destructive
    /// `memtest` takeover vertical passes on x86_64, where success is
    /// normally the `isa-debug-exit` `0x21` status (a takeover cannot write
    /// it — it resets the real hardware). The marker gate keeps a crash that
    /// merely triple-faults into a reset (also status `0`) failing loud: it
    /// never printed the marker. `None` (the default) leaves the per-arch
    /// [`Arch::outcome_from_status`] convention untouched.
    pub reset_success_marker: Option<String>,
    /// When `Some`, a **harness-driven** success signal: the run completes
    /// as `Outcome::Pass` the moment this flag reads `true`, at which point
    /// the runner kills QEMU. It exists for two-process verticals whose
    /// success is proven by an out-of-guest observer (the harness-side
    /// `netpeer` link peer) rather than by the guest itself: the guest must
    /// *not* self-terminate on an intermediate witness, because the observer's
    /// confirming event is the **last** link in the causal chain (e.g. the
    /// peer receiving the guest's echo reply) and a guest self-exit races —
    /// and loses to — that reply leaving the machine. With this gate the guest
    /// stays alive and serving until the observer has its proof, so teardown
    /// can never precede it. A guest that instead reaches its own debug-exit
    /// (or falls silent for the inactivity budget) still ends the run through
    /// the normal paths, so a genuine failure remains fail-loud; the
    /// [`Spec::timeout`] additionally bounds the wait so a gate that never
    /// trips cannot hang the run. `None` (the default) leaves the run driven
    /// solely by the guest.
    pub completion_gate: Option<Arc<AtomicBool>>,
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
    /// test. Defaults: single CPU, 60 s inactivity budget. The default guest
    /// RAM and firmware come from the [`x86_64`] module.
    #[must_use]
    pub fn for_x86_64_kernel(kernel: impl Into<PathBuf>) -> Self {
        Self {
            arch: Arch::X86_64,
            kernel: kernel.into(),
            cpus: 1,
            timeout: Duration::from_secs(60),
            declared_runtime_ceiling: None,
            declared_ram_mib: None,
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
            monitor_commands: Vec::new(),
            session: SessionKind::HeadlessTest,
            reset_success_marker: None,
            completion_gate: None,
        }
    }

    /// Override the CPU count. Clamped at `>= 1` because `-smp 0` is
    /// rejected by every QEMU we target.
    #[must_use]
    pub fn with_cpus(mut self, cpus: u32) -> Self {
        self.cpus = cpus.max(1);
        self
    }

    /// Override the guest RAM size, in mebibytes.
    ///
    /// The per-arch default is comfortable headroom for a test kernel, not a
    /// figure any vertical is meant to depend on. A vertical overrides it
    /// when the *amount* of RAM is the thing under test — the x86_64
    /// direct-map vertical needs more RAM than the boot trampoline's own
    /// identity window to prove the boot path widens it. Clamped at `>= 1`
    /// because `-m 0` is rejected by every QEMU we target.
    #[must_use]
    pub fn with_ram_mib(mut self, ram_mib: u32) -> Self {
        self.declared_ram_mib = Some(ram_mib.max(1));
        self
    }

    /// Guest RAM in mebibytes: this run's declared size, or the per-arch
    /// default. The one value every argv builder emits.
    #[must_use]
    pub fn ram_mib(&self) -> u32 {
        self.declared_ram_mib.unwrap_or(match self.arch {
            Arch::X86_64 => x86_64::DEFAULT_RAM_MIB,
            Arch::Aarch64 => aarch64::DEFAULT_RAM_MIB,
            Arch::Riscv64 => riscv64::DEFAULT_RAM_MIB,
        })
    }

    /// Override the inactivity (no-progress) budget.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Absolute wall-clock ceiling for the whole run.
    ///
    /// The inactivity heartbeat ([`Spec::timeout`]) only catches a guest that
    /// stops talking. A guest that keeps printing while making no progress —
    /// a service retrying a failed request on a timer, a choreography waiting
    /// on a witness that never arrives — resets that heartbeat forever, so
    /// without this ceiling its run never ends and the whole matrix stalls
    /// behind it. Exceeding it is [`Outcome::RuntimeCeilingExceeded`].
    ///
    /// Derived from the inactivity budget unless the run declared a ceiling of
    /// its own ([`Spec::with_runtime_ceiling`]), so an ordinary test carries
    /// one number and its two bounds cannot drift apart.
    ///
    /// A declared ceiling below the inactivity budget would fire before a
    /// silent guest could ever be declared hung, so the budget is the floor.
    #[must_use]
    pub fn runtime_ceiling(&self) -> Duration {
        self.declared_runtime_ceiling
            .unwrap_or(self.timeout * RUNTIME_CEILING_BUDGETS)
            .max(self.timeout)
    }

    /// Declare this run's absolute wall-clock ceiling explicitly, for a guest
    /// whose success genuinely requires long continuous work.
    ///
    /// The derived default (twice the inactivity budget) assumes total runtime
    /// is a small multiple of one phase of a run. That is false for a guest
    /// whose whole job is one long sweep — the pre-boot Supervisor's
    /// whole-RAM memtest takeover, which must complete a full pass over guest
    /// RAM before the harness resets it: its runtime scales with the RAM it
    /// sweeps and with host contention, so a multiple of "how long it may go
    /// quiet" bounds nothing about it and kills it mid-sweep under load.
    /// Declaring the ceiling keeps the silence budget sharp for what it does
    /// measure while bounding total runtime by the work actually asked for.
    #[must_use]
    pub fn with_runtime_ceiling(mut self, ceiling: Duration) -> Self {
        self.declared_runtime_ceiling = Some(ceiling);
        self
    }

    /// Accept a guest-initiated machine **reset** (QEMU exit status `0` under
    /// `-no-reboot`) as success, but **only** when the captured serial also
    /// contains `marker`. See [`Spec::reset_success_marker`]: this is the
    /// success signal for a run whose guest deliberately resets the machine
    /// rather than writing an architecture-specific debug-exit code (the
    /// pre-boot Supervisor's one-way destructive `memtest` takeover on
    /// x86_64), with the marker gate keeping a crash-into-reset failing loud.
    #[must_use]
    pub fn with_reset_success_marker(mut self, marker: impl Into<String>) -> Self {
        self.reset_success_marker = Some(marker.into());
        self
    }

    /// Complete the run as `Outcome::Pass` as soon as `gate` reads `true`,
    /// killing QEMU at that instant. See [`Spec::completion_gate`]: this is
    /// the harness-driven success signal for a two-process vertical whose
    /// proof is held by the out-of-guest `netpeer` observer, so the guest
    /// stays alive and serving until the observer's confirming (last-in-chain)
    /// event has occurred rather than self-terminating on an earlier witness
    /// and racing it.
    #[must_use]
    pub fn with_completion_gate(mut self, gate: Arc<AtomicBool>) -> Self {
        self.completion_gate = Some(gate);
        self
    }

    /// Minimal riscv64 `virt`-board spec suitable for a QEMU integration
    /// test. Defaults: single CPU, 60 s inactivity budget. The default guest
    /// RAM and firmware come from the [`riscv64`] module.
    #[must_use]
    pub fn for_riscv64_kernel(kernel: impl Into<PathBuf>) -> Self {
        Self {
            arch: Arch::Riscv64,
            kernel: kernel.into(),
            cpus: 1,
            timeout: Duration::from_secs(60),
            declared_runtime_ceiling: None,
            declared_ram_mib: None,
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
            monitor_commands: Vec::new(),
            session: SessionKind::HeadlessTest,
            reset_success_marker: None,
            completion_gate: None,
        }
    }

    /// Minimal aarch64 `virt`-board spec suitable for a QEMU integration
    /// test. Defaults: single CPU, 60 s inactivity budget. The default guest
    /// RAM, CPU model, and result protocol come from the [`aarch64`] module.
    #[must_use]
    pub fn for_aarch64_kernel(kernel: impl Into<PathBuf>) -> Self {
        Self {
            arch: Arch::Aarch64,
            kernel: kernel.into(),
            cpus: 1,
            timeout: Duration::from_secs(60),
            declared_runtime_ceiling: None,
            declared_ram_mib: None,
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
            monitor_commands: Vec::new(),
            session: SessionKind::HeadlessTest,
            reset_success_marker: None,
            completion_gate: None,
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

    /// Append one raw QEMU-monitor command, sent once `ready_marker` has
    /// appeared `occurrences` times (clamped at `>= 1`) on the serial
    /// console. Commands run strictly in declaration order; a run that
    /// exits before every command was sent fails. Used for deterministic
    /// device-state changes a guest cannot make itself (e.g.
    /// `set_link net0 off` to fail a bond member over mid-flow).
    #[must_use]
    pub fn with_monitor_command(
        mut self,
        ready_marker: impl Into<String>,
        occurrences: u32,
        command: impl Into<String>,
    ) -> Self {
        self.monitor_commands.push(MonitorCommand {
            ready_marker: ready_marker.into(),
            ready_occurrences: occurrences.max(1),
            command: command.into(),
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

        // Attach a QEMU monitor on a private unix socket for *every* run.
        // Verticals that inject keys or take screendumps drive it during the
        // run; the rest need it only once, at the end, and only if things go
        // wrong — a guest that has to be killed as hung is interrogated over
        // it first, and a hang whose report
        // cannot say what the CPUs were doing can only be diagnosed by
        // re-running it, which is not a diagnosis. The socket is server-side
        // in QEMU (created at startup, well before any guest output) and the
        // runner connects as a client.
        let monitor = ReservedSocket::reserve("mon")
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
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
/// (if requested), enforce the inactivity deadline, and assemble the
/// [`Outcome`].
///
/// Split out of [`Runner::run`] so the spawn-and-validate path and the
/// wait loop each stay within one screen.
fn supervise(
    mut child: Child,
    spec: &Spec,
    monitor: Option<&ReservedSocket>,
) -> io::Result<Outcome> {
    // The inactivity heartbeat: the guest is declared hung once `spec.timeout`
    // elapses with no new serial output. A guest that is merely running slowly
    // — co-scheduled with the rest of the matrix, or landed on a slow host core
    // — keeps emitting boot and progress output, so its heartbeat keeps
    // resetting and it is never killed for being slow. That is what makes this
    // bound immune to host load, and in turn lets the matrix run guests
    // concurrently without a slow guest degrading into a flaky timeout.
    let mut heartbeat = ProgressClock::new(Instant::now());
    // The absolute ceiling, which every run carries because the heartbeat
    // cannot bound a guest that keeps talking while making no progress: a
    // service retrying a failed request on a timer, or a choreography waiting
    // on a witness that never arrives, resets the heartbeat forever. It also
    // bounds a gated run whose out-of-guest observer never confirms the round
    // trip — there the guest deliberately does not self-exit at all.
    //
    // The two bounds diagnose different faults: the heartbeat catches a guest
    // that stopped talking, the ceiling one that never finished. Which fired,
    // and how long the guest had been silent when it did, is reported rather
    // than collapsed into one "timeout" — that distinction is the difference
    // between hunting a guest-side stall and hunting a live-but-stuck guest,
    // and its absence once cost a long hunt.
    let run_start = Instant::now();

    let SerialDrain {
        captured,
        marker_seen,
        typing_markers_seen,
        pointer_markers_seen,
        screendump_markers_seen,
        monitor_markers_seen,
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

    let mut injections = InjectionState::new(spec);
    // Serial-input script cursor: the next step to send, and the byte
    // offset in the captured serial log just past the previous step's
    // matched marker. Matching only ever advances, so each marker must
    // arrive in order and a repeated prompt (e.g. a second `Username: `
    // after a refused login) anchors its own step rather than re-firing
    // on the first occurrence.
    let mut serial_script = SerialScriptState::default();
    let done = run_wait_loop(WaitLoop {
        child: &mut child,
        spec,
        monitor,
        reader: &mut reader,
        err_reader: &mut err_reader,
        serial_stdin: &mut serial_stdin,
        captured: &captured,
        markers: InjectionMarkers {
            key: &marker_seen,
            typing: &typing_markers_seen,
            pointer: &pointer_markers_seen,
            screendump: &screendump_markers_seen,
            monitor: &monitor_markers_seen,
        },
        injections: &mut injections,
        serial_script: &mut serial_script,
        heartbeat: &mut heartbeat,
        run_start,
    })?;

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

/// The running state the QEMU supervision wait-loop drives while a guest is
/// alive. Bundled into one borrow so [`run_wait_loop`] takes a single argument
/// rather than a dozen; every field is borrowed from [`supervise`], which
/// keeps ownership for the post-loop drain and outcome assembly.
struct WaitLoop<'a> {
    /// The spawned QEMU child being supervised.
    child: &'a mut Child,
    /// The test spec: deadline, injections, and optional completion gate.
    spec: &'a Spec,
    /// The QMP monitor connection, when the spec drives one.
    monitor: Option<&'a ReservedSocket>,
    /// The serial-drain thread handle, polled for an early drain failure.
    reader: &'a mut Option<std::thread::JoinHandle<io::Result<()>>>,
    /// The stderr-drain thread handle, polled for an early drain failure.
    err_reader: &'a mut Option<std::thread::JoinHandle<io::Result<()>>>,
    /// The guest's serial input pipe the serial-input script writes through.
    serial_stdin: &'a mut Option<ChildStdin>,
    /// Everything the guest has printed on serial so far.
    captured: &'a Arc<Mutex<String>>,
    /// The per-injection readiness flags the injector consults each tick.
    markers: InjectionMarkers<'a>,
    /// The ordered key/pointer/dump/monitor injection cursor.
    injections: &'a mut InjectionState,
    /// The serial-input script cursor.
    serial_script: &'a mut SerialScriptState,
    /// The inactivity heartbeat.
    heartbeat: &'a mut ProgressClock,
    /// Absolute run start, against which the runtime ceiling is measured.
    run_start: Instant,
}

/// Poll a running QEMU guest to completion and report why it finished.
///
/// Split out of [`supervise`] so the spawn/validate path and the wait loop
/// each stay within one screen. The deadline model is documented on
/// [`supervise`] and [`ProgressClock`]: an inactivity heartbeat that catches a
/// guest which stopped talking, plus an absolute runtime ceiling that catches
/// one which never finishes.
fn run_wait_loop(cx: WaitLoop<'_>) -> io::Result<DoneReason> {
    let WaitLoop {
        child,
        spec,
        monitor,
        reader,
        err_reader,
        serial_stdin,
        captured,
        markers,
        injections,
        serial_script,
        heartbeat,
        run_start,
    } = cx;
    // Poll for completion in short ticks so the deadline is precise to
    // the millisecond. We deliberately do *not* sleep until the deadline
    // and then check once: that pattern adds up to `timeout` of latency
    // for fast-failing tests, which would slow `cargo xtask ci`.
    let tick = Duration::from_millis(25);
    let mut serial_closed = false;
    loop {
        // Harness-driven success: the out-of-guest observer (the `netpeer` link
        // peer) has confirmed the round-trip. End the run as `Pass` at once,
        // before waiting on the guest — in this mode the guest never
        // self-exits, so the gate is the sole success path.
        if let Some(gate) = &spec.completion_gate {
            if gate.load(Ordering::Acquire) {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(DoneReason::CompletedByGate);
            }
        }
        if let Some(status) = child.try_wait()? {
            return Ok(exit_reason(spec, serial_script.step, &*injections, status));
        }
        if let Some(result) = completed_drain_result(reader, "serial output") {
            serial_closed = result.is_ok();
            if let Err(reason) = result {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(DoneReason::DrainFailed(reason));
            }
        }
        if let Some(Err(reason)) = completed_drain_result(err_reader, "qemu stderr") {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(DoneReason::DrainFailed(reason));
        }
        if let Err(e) = injections.drive(spec, monitor, &markers) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(DoneReason::InjectionFailed(e));
        }
        let advanced = advance_serial_script(
            &spec.serial_input,
            captured,
            serial_stdin,
            serial_script,
            Instant::now(),
        );
        if let Err(e) = advanced {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(DoneReason::InjectionFailed(format!(
                "serial input injection failed at step {}: {e}",
                serial_script.step
            )));
        }
        // Any new serial output is forward progress: reset the heartbeat so a
        // guest that is alive but slow is never mistaken for a hung one. The
        // captured log only ever grows, so its length is a cheap, monotonic
        // progress signal.
        let serial_len = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        heartbeat.observe(serial_len, Instant::now());
        // A run that never completes must fail loud instead of hanging,
        // whatever the guest is doing — including a guest that keeps talking,
        // which the inactivity heartbeat would never declare hung. The silence
        // at the kill rides along in the report, because it is what
        // distinguishes a live guest that never completed from one that
        // stalled and went quiet.
        if run_start.elapsed() >= spec.runtime_ceiling() {
            let silent_for = heartbeat.idle_for(Instant::now());
            // Interrogate the guest *before* killing it: once QEMU is gone
            // the only evidence left is the transcript, which is exactly the
            // evidence that has already run out.
            let cpu_state = injections.hang_report(monitor, &spec.kernel);
            let _ = child.kill();
            let _ = child.wait();
            return Ok(DoneReason::CeilingExceeded {
                silent_for,
                cpu_state,
            });
        }
        if heartbeat.idle_for(Instant::now()) >= spec.timeout {
            // Strict, no-retry kill, with the guest's own CPU state read off
            // the monitor first so one occurrence is diagnosable.
            let cpu_state = injections.hang_report(monitor, &spec.kernel);
            let _ = child.kill();
            let _ = child.wait();
            return Ok(if serial_closed {
                DoneReason::DrainFailed(String::from("serial output closed before QEMU exited"))
            } else {
                DoneReason::TimedOut { cpu_state }
            });
        }
        std::thread::sleep(tick);
    }
}

/// Inactivity heartbeat for the supervision loop.
///
/// Tracks the high-water mark of the guest's captured serial length and the
/// instant it last grew. Because the guest's serial log only ever grows, its
/// length is a cheap monotonic proxy for "the guest is doing something": every
/// increase resets the heartbeat. [`idle_for`](Self::idle_for) then reports how
/// long the guest has produced nothing, which the loop compares against
/// `spec.timeout`. This makes the deadline a *no-progress* ceiling rather than
/// a total-runtime one, so a guest that is merely slow (co-scheduled, or on a
/// slow host core) is never killed while it keeps emitting output.
struct ProgressClock {
    /// Largest captured serial length observed so far.
    seen_len: usize,
    /// Instant the captured length last increased (the last sign of life).
    last_progress: Instant,
}

impl ProgressClock {
    /// Start the heartbeat at `now` with no output yet observed.
    fn new(now: Instant) -> Self {
        Self {
            seen_len: 0,
            last_progress: now,
        }
    }

    /// Record the current captured serial length. Returns `true` and resets
    /// the heartbeat to `now` when the length grew (the guest made progress);
    /// returns `false` and leaves the heartbeat untouched otherwise.
    fn observe(&mut self, len: usize, now: Instant) -> bool {
        if len > self.seen_len {
            self.seen_len = len;
            self.last_progress = now;
            true
        } else {
            false
        }
    }

    /// How long the guest has produced no new serial output as of `now`.
    fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_progress)
    }
}

/// Convert the completed supervision reason and captured output into the
/// architecture-specific public outcome.
fn outcome_from_done(done: DoneReason, spec: &Spec, mut serial: String) -> Outcome {
    match done {
        DoneReason::Exited(code) => {
            // A guest that deliberately resets the machine (its only exit is a
            // reset/power-off) signals success through the reset itself, not an
            // architecture-specific debug-exit code — QEMU exits status `0`
            // under `-no-reboot`. Accept that as `Pass` only when the caller
            // opted in *and* the required marker was printed, so a crash that
            // merely triple-faults into a reset (also status `0`) still fails
            // loud (it never reached the marker). Otherwise fall through to the
            // per-arch debug-exit convention.
            if let Some(marker) = &spec.reset_success_marker {
                if code == 0 && serial.contains(marker.as_str()) {
                    return Outcome::Pass { serial };
                }
                return Outcome::Fail {
                    status: code,
                    serial,
                };
            }
            spec.arch.outcome_from_status(code, serial)
        }
        DoneReason::CompletedByGate => Outcome::Pass { serial },
        DoneReason::TimedOut { cpu_state } => Outcome::Timeout {
            budget: spec.timeout,
            serial,
            cpu_state,
        },
        DoneReason::CeilingExceeded {
            silent_for,
            cpu_state,
        } => Outcome::RuntimeCeilingExceeded {
            ceiling: spec.runtime_ceiling(),
            silent_for,
            serial,
            cpu_state,
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
    /// The guest fell silent for the whole inactivity budget; the child was
    /// killed. Carries the per-vCPU state read off the monitor before the
    /// kill.
    TimedOut {
        /// Every vCPU's register file at the kill, with kernel-text
        /// addresses resolved.
        cpu_state: String,
    },
    /// The run hit its absolute wall-clock ceiling while still alive; the
    /// child was killed. Carries how long the guest had been silent, which
    /// separates a live-but-never-completing guest from one stalled at a
    /// fixed point.
    CeilingExceeded {
        /// How long the guest had produced no serial output at the kill.
        silent_for: Duration,
        /// Every vCPU's register file at the kill, with kernel-text
        /// addresses resolved.
        cpu_state: String,
    },
    /// The [`Spec::completion_gate`] tripped: the out-of-guest observer
    /// (the `netpeer` link peer) confirmed success, so the child was
    /// killed and the run scored `Pass`.
    CompletedByGate,
    /// A requested key/serial injection could not be delivered; the
    /// child was killed. The message explains which injection and why.
    InjectionFailed(String),
    /// A QEMU output drain failed, panicked, or closed before the child;
    /// the child was killed. The message identifies the failed channel.
    DrainFailed(String),
}

/// Longest a single monitor read may block while the hang report is being
/// collected. A reply that has stopped arriving for this long has ended: the
/// human monitor sends nothing further until it is asked again.
const MONITOR_READ_QUIET: Duration = Duration::from_millis(250);

/// Total wall clock the hang report may spend reading the monitor.
///
/// The report is taken at the one moment the run has already concluded
/// something is wrong, so it must never itself become a second thing that
/// hangs. A whole register file for a handful of vCPUs arrives in
/// milliseconds; this is orders of magnitude more than that.
const MONITOR_READ_BUDGET: Duration = Duration::from_secs(3);

/// Bytes of monitor reply the hang report will hold. A register dump is a
/// few kilobytes per vCPU; this bounds a monitor that will not stop talking.
const MONITOR_READ_MAX_BYTES: usize = 256 * 1024;

/// Most kernel-text addresses one hang report names. A register dump for a
/// handful of vCPUs mentions a few dozen distinct ones; this bounds a report
/// that somehow mentions far more.
const LEGEND_MAX_ENTRIES: usize = 256;

/// `st_info` symbol type for a function (`STT_FUNC`).
const STT_FUNC: u8 = 2;

/// Resolve the kernel-text addresses a monitor register dump mentions, so a
/// hang report names code instead of numbers.
///
/// Deliberately arch-neutral: rather than parse each target's register
/// format, it takes every address-width hexadecimal word in `report` and
/// keeps the ones landing inside a function the kernel ELF defines. That
/// resolves the program counter, but also the link register and any return
/// address the dump happens to show — usually what says *how* a core reached
/// where it stopped.
///
/// Best effort: an unreadable or symbol-less ELF yields no legend rather than
/// an error. The verdict belongs to the hang, never to this.
fn symbol_legend(kernel: &Path, report: &str) -> String {
    let Ok(bytes) = std::fs::read(kernel) else {
        return String::new();
    };
    let functions = kernel_functions(&bytes);
    if functions.is_empty() {
        return String::new();
    }
    let mut resolved: BTreeMap<u64, String> = BTreeMap::new();
    for addr in hex_words(report) {
        if resolved.len() >= LEGEND_MAX_ENTRIES {
            break;
        }
        if resolved.contains_key(&addr) {
            continue;
        }
        if let Some(named) = resolve_addr(&functions, addr) {
            resolved.insert(addr, named);
        }
    }
    if resolved.is_empty() {
        return String::new();
    }
    let mut out = String::from("--- kernel text addresses named above ---\n");
    for (addr, name) in resolved {
        let _ = writeln!(out, "0x{addr:016x}  {name}");
    }
    out
}

/// Name `addr` as an offset into the function containing it, or [`None`]
/// when it lands in no function's extent.
///
/// `functions` must be sorted by start address ([`kernel_functions`]).
fn resolve_addr(functions: &[(u64, u64, String)], addr: u64) -> Option<String> {
    let index = match functions.binary_search_by_key(&addr, |(start, _, _)| *start) {
        Ok(exact) => exact,
        Err(0) => return None,
        Err(after) => after - 1,
    };
    let (start, size, name) = functions.get(index)?;
    (addr < start.saturating_add(*size)).then(|| format!("{name}+0x{:x}", addr - start))
}

/// Every sized function the kernel ELF defines, sorted by address.
///
/// Sized, because a nearest-preceding-symbol guess would put a name on an
/// address past the last function; a report that invents a call site is worse
/// than one that leaves the number alone.
fn kernel_functions(bytes: &[u8]) -> Vec<(u64, u64, String)> {
    let Ok(view) = ElfView::parse(bytes) else {
        return Vec::new();
    };
    let mut out: Vec<(u64, u64, String)> = Vec::new();
    for index in 0..view.header().shnum {
        let Ok(section) = view.section(index) else {
            continue;
        };
        if section.sh_type != SHT_SYMTAB {
            continue;
        }
        let Ok(table) = view.symbol_table(index) else {
            continue;
        };
        for i in 0..table.len() {
            let Ok(symbol) = table.symbol(i) else {
                continue;
            };
            if symbol.info & 0x0f != STT_FUNC || symbol.value == 0 || symbol.size == 0 {
                continue;
            }
            let Ok(name) = table.name(&symbol) else {
                continue;
            };
            out.push((symbol.value, symbol.size, name.to_string()));
        }
    }
    out.sort_unstable();
    out
}

/// Every address-width hexadecimal word in `text`, as a value.
///
/// A QEMU register dump writes them bare (`PC=0000000040444cd4`), so this
/// takes runs of 8 to 16 hex digits delimited by anything else — wide enough
/// to skip a field index or a small immediate, narrow enough to catch every
/// 32- and 64-bit register value.
fn hex_words(text: &str) -> Vec<u64> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, b) in bytes.iter().enumerate() {
        if b.is_ascii_hexdigit() {
            let _ = run_start.get_or_insert(i);
        } else if let Some(start) = run_start.take() {
            push_hex_word(&mut out, &bytes[start..i]);
        }
    }
    if let Some(start) = run_start {
        push_hex_word(&mut out, &bytes[start..]);
    }
    out
}

/// Decode one delimited run of hex digits, keeping only address-width ones.
fn push_hex_word(out: &mut Vec<u64>, word: &[u8]) {
    if !(8..=16).contains(&word.len()) {
        return;
    }
    if let Ok(text) = core::str::from_utf8(word) {
        if let Ok(value) = u64::from_str_radix(text, 16) {
            out.push(value);
        }
    }
}

/// The longest unix-socket path this host can bind, including its
/// terminating NUL.
///
/// A unix socket's address is a fixed `sun_path` array, not a heap string:
/// 108 bytes on Linux, 104 on macOS/BSD. Taking the smaller of the two makes
/// a path that passes here bindable on every host we build on, so a naming
/// mistake fails the same way everywhere instead of only on the tighter
/// platform. This is a bound the OS ABI dictates, not a capacity to grow.
const SOCKET_PATH_MAX: usize = 104;

/// A reserved path for a unix socket one QEMU run uses, removed when this is
/// dropped so a run leaves no stray socket behind.
///
/// Both of the run's socket kinds hold one: QEMU's monitor socket (which QEMU
/// itself creates, `server=on`) and each netstack wire's datagram pair. One
/// definition names them all and enforces the length bound in one place.
pub struct ReservedSocket {
    path: PathBuf,
}

impl ReservedSocket {
    /// Reserve a short, unique temp-directory path for one socket of this run,
    /// named `tairix-qemu-<role>-<pid>-<n>.sock`.
    ///
    /// `role` is a short fixed word naming the wire's end (`mon`, `net0q`,
    /// `net0p`, …). The process id and a monotonic counter make the path
    /// unique across concurrent runs both between processes and within one
    /// (the `cargo xtask` soak runs several guests at once).
    ///
    /// # The name must not carry the caller's identity
    ///
    /// A unix socket's address is a fixed `sun_path` array — 108 bytes on
    /// Linux, 104 on macOS — and the temp directory alone can consume half of
    /// it, since macOS hands out a 49-byte per-user directory. A name built
    /// from a test's package or binary name therefore overflows the bound on a
    /// perfectly ordinary host, and does so *deterministically for the
    /// longest-named tests only*, which reads like a mysterious per-test
    /// failure rather than the naming bug it is. The reserved name is a
    /// bounded constant shape instead, and a run's identity lives where it is
    /// actually useful for debugging: the `.pcap` capture and the serial log
    /// beside the kernel image.
    ///
    /// # Errors
    ///
    /// Fails closed with the measured length when even this shape cannot fit
    /// the bound, rather than deferring an opaque `bind` error to the caller.
    pub fn reserve(role: &str) -> Result<Self, String> {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tairix-qemu-{role}-{}-{n}.sock",
            std::process::id()
        ));
        let len = path.as_os_str().len();
        if len >= SOCKET_PATH_MAX {
            return Err(format!(
                "socket path {} is {len} bytes, over the {SOCKET_PATH_MAX}-byte unix-socket limit \
                 (set TMPDIR to a shorter directory)",
                path.display()
            ));
        }
        Ok(Self { path })
    }

    /// The reserved path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ReservedSocket {
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
    /// One flag per monitor command, set once that command's readiness
    /// marker has appeared the required number of times. Index-aligned
    /// with [`Spec::monitor_commands`].
    monitor_markers_seen: Vec<Arc<AtomicBool>>,
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
    let monitor_markers_seen: Vec<Arc<AtomicBool>> = spec
        .monitor_commands
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
        for (m, seen) in spec.monitor_commands.iter().zip(&monitor_markers_seen) {
            markers.push((
                m.ready_marker.clone(),
                m.ready_occurrences.max(1),
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
        monitor_markers_seen,
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
/// translates `sendkey` names through, plus `\n` (`ret`), `\t` (`tab`),
/// and the `\u{3}` ETX byte as the `ctrl-c` chord (so a script can type a
/// job-control interrupt at the seat keyboard; the guest terminal encodes
/// the resulting Ctrl-C key event back to the `0x03` byte). Every other
/// character is refused with an error naming it — the run then fails
/// rather than silently typing a corrupted script (fail closed, never a
/// skipped or guessed key).
fn qkeycode_for(c: char) -> Result<String, String> {
    // The base (unshifted) names for the non-alphanumeric keys.
    let plain = |name: &str| Ok(name.to_string());
    let shifted = |name: &str| Ok(format!("shift-{name}"));
    match c {
        'a'..='z' | '0'..='9' => Ok(c.to_string()),
        'A'..='Z' => Ok(format!("shift-{}", c.to_ascii_lowercase())),
        '\n' => plain("ret"),
        '\t' => plain("tab"),
        // The ETX control byte types the Ctrl-C chord: QEMU emits the
        // ctrl-down, c-down, c-up, ctrl-up scancodes, so the guest sees
        // `c` pressed with Ctrl held (a modifier-only edge produces no key
        // record — the chord is one delivered key press/release pair).
        '\u{3}' => plain("ctrl-c"),
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
    /// The raw monitor command to send next (`monitor_commands.len()`
    /// once every command was sent, or when none was requested).
    monitor_step: usize,
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

/// The per-injection readiness flags [`InjectionState::drive`] consults on
/// each poll tick, one slice per injection kind. Bundled into one borrow so
/// `drive` takes the markers as a single argument rather than five, keeping
/// its signature honest as injection kinds are added.
struct InjectionMarkers<'a> {
    /// The key-injection readiness flag ([`Spec::input_keyboard`]).
    key: &'a AtomicBool,
    /// Per typed-text-step flags ([`Spec::input_typing`]).
    typing: &'a [Arc<AtomicBool>],
    /// Per pointer-step flags ([`Spec::pointer_script`]).
    pointer: &'a [Arc<AtomicBool>],
    /// Per screendump flags ([`Spec::screendumps`]).
    screendump: &'a [Arc<AtomicBool>],
    /// Per monitor-command flags ([`Spec::monitor_commands`]).
    monitor: &'a [Arc<AtomicBool>],
}

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
            monitor_step: 0,
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

    /// `true` once every requested raw monitor command has been sent.
    fn monitor_done(&self, spec: &Spec) -> bool {
        self.monitor_step >= spec.monitor_commands.len()
    }

    /// The message for the first ordered injection script that had not
    /// completed when the guest exited, or `None` when every requested
    /// injection finished. [`exit_reason`] fails the run on a `Some`, so a
    /// vertical whose device-driven exchange never happened cannot pass on
    /// the guest's own exit status alone.
    fn incomplete_reason(&self, spec: &Spec) -> Option<&'static str> {
        if !self.pointer_done(spec) {
            return Some(
                "pointer script incomplete: a step's readiness marker was not seen (or a dump \
                 it waited on never verified) before exit",
            );
        }
        if !self.typing_done(spec) {
            return Some(
                "typed-text script incomplete: its readiness marker was not seen (or the guest \
                 exited mid-script)",
            );
        }
        if !spec.screendumps.is_empty() && !self.screendumps_verified(spec) {
            return Some(
                "screendumps incomplete: a dump's readiness marker was not seen, or the guest \
                 exited before its dumped image parsed completely",
            );
        }
        if !self.monitor_done(spec) {
            return Some(
                "monitor command script incomplete: a command's readiness marker was not seen \
                 before the guest exited",
            );
        }
        None
    }

    /// Send the next raw monitor command if its readiness marker has
    /// appeared: strictly in order, at most one per tick so a burst arrives
    /// as distinct monitor lines.
    ///
    /// # Errors
    ///
    /// The failing command's message; the caller kills the child.
    fn drive_monitor_commands(
        &mut self,
        spec: &Spec,
        monitor: Option<&ReservedSocket>,
        monitor_seen: &[Arc<AtomicBool>],
    ) -> Result<(), String> {
        if let Some(command) = spec.monitor_commands.get(self.monitor_step) {
            let step_seen = monitor_seen
                .get(self.monitor_step)
                .is_some_and(|seen| seen.load(Ordering::Acquire));
            if step_seen {
                self.send(monitor, "monitor-command", &command.command)?;
                self.monitor_step += 1;
            }
        }
        Ok(())
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
        monitor: Option<&ReservedSocket>,
        markers: &InjectionMarkers<'_>,
    ) -> Result<(), String> {
        // Safe to unwrap inside the closures: each `*_sent` flag is only
        // `false` when its injection request and `monitor` are both `Some`.
        if !self.key_sent && markers.key.load(Ordering::Acquire) {
            let key = &spec.input_keyboard.as_ref().expect("key present").key;
            self.send(monitor, "key", &format!("sendkey {key}"))?;
            self.key_sent = true;
        }
        self.drive_monitor_commands(spec, monitor, markers.monitor)?;
        if let Some(typing) = spec.input_typing.get(self.typed_step) {
            // Steps run strictly in order, each gated on its own marker;
            // within a step the keys are paced — at most one per call,
            // and only once the previous key's hold has fully elapsed —
            // so repeated characters arrive as distinct press/release
            // edges.
            let step_seen = markers
                .typing
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
            let dump_seen = markers
                .screendump
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
        // *step* per poll tick, so a motion is processed before whatever
        // follows it lands at the new position; a `Click` step sends both of
        // its mask changes in that one tick, because a click is one gesture.
        let dump_pending = spec.screendumps.get(self.dump_step).is_some_and(|_| {
            self.dump_state == DumpState::Sent
                || markers
                    .screendump
                    .get(self.dump_step)
                    .is_some_and(|seen| seen.load(Ordering::Acquire))
        });
        if let Some(step) = spec.pointer_script.get(self.pointer_step) {
            let step_seen = markers
                .pointer
                .get(self.pointer_step)
                .is_some_and(|seen| seen.load(Ordering::Acquire));
            if step_seen && !dump_pending {
                match step.action {
                    PointerAction::Move { dx, dy } => {
                        self.send(monitor, "pointer", &format!("mouse_move {dx} {dy}"))?;
                    }
                    PointerAction::Press(button) => {
                        self.pointer_button_mask |= button.mask_bit();
                        self.send_button_mask(monitor)?;
                    }
                    PointerAction::Release(button) => {
                        self.pointer_button_mask &= !button.mask_bit();
                        self.send_button_mask(monitor)?;
                    }
                    PointerAction::Click(button) => {
                        self.pointer_button_mask |= button.mask_bit();
                        self.send_button_mask(monitor)?;
                        self.pointer_button_mask &= !button.mask_bit();
                        self.send_button_mask(monitor)?;
                    }
                }
                self.pointer_step += 1;
            }
        }
        Ok(())
    }

    /// Send the tracked button-state mask as a `mouse_button` command.
    ///
    /// # Errors
    ///
    /// The failing injection's message, as [`send`](Self::send) reports it.
    fn send_button_mask(&mut self, monitor: Option<&ReservedSocket>) -> Result<(), String> {
        let command = format!("mouse_button {}", self.pointer_button_mask);
        self.send(monitor, "pointer", &command)
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
        monitor: Option<&ReservedSocket>,
        what: &str,
        command: &str,
    ) -> Result<(), String> {
        let fail = |e: io::Error| format!("{what} injection failed: {e}");
        if self.conn.is_none() {
            // Every run attaches a monitor, so this is unreachable in a real
            // run; refuse rather than panic if one ever is not.
            let mon = monitor.ok_or_else(|| fail(io::Error::other("no monitor attached")))?;
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

    /// Read every vCPU's register file off the QEMU monitor, for a run that
    /// is about to be killed as hung.
    ///
    /// A hang report that cannot say what the guest's CPUs were doing forces
    /// a re-run to learn anything, and a re-run is precisely how a hang must
    /// *not* be diagnosed. `info registers -a` gives each core's program
    /// counter, link register and interrupt mask, which
    /// [`symbol_legend`] then names: every core sitting in the idle
    /// wait-for-interrupt means nothing was runnable (a lost wake-up),
    /// whereas a core inside a loop with interrupts masked is a spin.
    /// `info cpus` enumerates the vCPUs the dump then walks.
    ///
    /// Best effort by construction: it reuses this run's single monitor
    /// connection (a socket chardev serves one client), and a monitor that
    /// cannot be reached, or a QEMU already gone, yields the reason *as* the
    /// report. The verdict is the hang's, never this call's.
    fn hang_report(&mut self, monitor: Option<&ReservedSocket>, kernel: &Path) -> String {
        let mut report = self.read_monitor_state(monitor);
        report.push_str(&symbol_legend(kernel, &report));
        report
    }

    /// The raw monitor answer [`Self::hang_report`] names addresses from.
    fn read_monitor_state(&mut self, monitor: Option<&ReservedSocket>) -> String {
        if let Err(e) = self.send(monitor, "hang diagnosis", "info cpus") {
            return format!("unavailable: {e}\n");
        }
        if let Err(e) = self.send(monitor, "hang diagnosis", "info registers -a") {
            return format!("unavailable: {e}\n");
        }
        let Some(stream) = self.conn.as_mut() else {
            return String::from("unavailable: monitor connection vanished\n");
        };
        // Bounded twice over — per-read and in total — because the monitor is
        // being read at the one moment the run has already decided something
        // is wrong, so it must never become a second thing that hangs.
        if let Err(e) = stream.set_read_timeout(Some(MONITOR_READ_QUIET)) {
            return format!("unavailable: {e}\n");
        }
        let deadline = Instant::now() + MONITOR_READ_BUDGET;
        let mut raw: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        while Instant::now() < deadline && raw.len() < MONITOR_READ_MAX_BYTES {
            // A closed socket and a read that times out both mean the reply
            // has ended: the monitor sends nothing further until it is asked
            // again, so neither is a failure.
            match stream.read(&mut chunk) {
                Ok(n) if n > 0 => raw.extend_from_slice(&chunk[..n]),
                _ => break,
            }
        }
        if raw.is_empty() {
            return String::from("unavailable: the monitor answered nothing\n");
        }
        String::from_utf8_lossy(&raw).into_owned()
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
    injections: &InjectionState,
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
    if let Some(reason) = injections.incomplete_reason(spec) {
        return DoneReason::InjectionFailed(reason.into());
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
            Outcome::Pass { .. }
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
    fn a_hang_report_names_only_addresses_inside_a_function() {
        // The `size` guard is what stops the legend inventing a call site:
        // an address past the last function's extent (a stack value, a small
        // constant) resolves to nothing rather than to whatever symbol
        // happens to precede it.
        let functions = vec![
            (0x1000_u64, 0x10_u64, String::from("first")),
            (0x2000, 0x20, String::from("second")),
        ];
        assert_eq!(
            resolve_addr(&functions, 0x1000).as_deref(),
            Some("first+0x0")
        );
        assert_eq!(
            resolve_addr(&functions, 0x100c).as_deref(),
            Some("first+0xc")
        );
        // In the gap between the two functions, and past the last one.
        assert_eq!(resolve_addr(&functions, 0x1010), None);
        assert_eq!(resolve_addr(&functions, 0x2100), None);
        // Below the first function.
        assert_eq!(resolve_addr(&functions, 0x0fff), None);
        assert_eq!(
            resolve_addr(&functions, 0x2004).as_deref(),
            Some("second+0x4")
        );
    }

    #[test]
    fn the_hang_report_scan_takes_register_values_and_nothing_else() {
        // A QEMU register dump writes addresses bare and everything else
        // around them: the scan must pick up the 32- and 64-bit register
        // values and skip the register indices, small decimal fields, and
        // the PSTATE mnemonics that are themselves hex letters.
        let dump = "CPU#0\n PC=0000000040444cd4 X00=0000000000000002\n                    PSTATE=600003c5 -ZC- EL1h\n* CPU #0: thread_id=2669729\n";
        assert_eq!(
            hex_words(dump),
            vec![0x0000_0000_4044_4cd4, 0x0000_0000_0000_0002, 0x6000_03c5]
        );
    }

    #[test]
    fn a_hang_report_without_a_readable_kernel_yields_no_legend() {
        // Best effort: the verdict belongs to the hang, so an ELF that
        // cannot be read costs the legend, never the report.
        assert!(
            symbol_legend(Path::new("/nonexistent/kernel.elf"), "PC=0000000040444cd4").is_empty()
        );
    }

    #[test]
    fn every_outcome_surfaces_its_transcript() {
        // The runner persists the transcript once per run, off this accessor,
        // so a variant that hid its own log would silently leave that run's
        // evidence on the floor — a pass most of all.
        for outcome in [
            Outcome::Pass {
                serial: "pass log".into(),
            },
            Outcome::Fail {
                status: 1,
                serial: "pass log".into(),
            },
            Outcome::Timeout {
                budget: Duration::from_secs(1),
                serial: "pass log".into(),
                cpu_state: String::new(),
            },
            Outcome::RuntimeCeilingExceeded {
                ceiling: Duration::from_secs(2),
                silent_for: Duration::ZERO,
                serial: "pass log".into(),
                cpu_state: String::new(),
            },
        ] {
            assert_eq!(outcome.serial(), "pass log");
        }
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
    fn reset_success_marker_accepts_a_marked_reset_exit() {
        // A guest that resets (status 0 under -no-reboot) having printed the
        // required marker is the takeover vertical's success: Pass.
        let spec = Spec::for_x86_64_kernel("/tmp/k").with_reset_success_marker("memtest: PASSED");
        let serial = String::from("... memtest: PASSED \u{2014} 181 MiB tested. Resetting.");
        assert!(matches!(
            outcome_from_done(DoneReason::Exited(0), &spec, serial),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn reset_success_marker_rejects_an_unmarked_reset_exit() {
        // A crash that merely triple-faults into a reset (status 0) without
        // ever printing the marker must still fail loud, not pass by accident.
        let spec = Spec::for_x86_64_kernel("/tmp/k").with_reset_success_marker("memtest: PASSED");
        match outcome_from_done(DoneReason::Exited(0), &spec, "partial boot".into()) {
            Outcome::Fail { status, serial } => {
                assert_eq!(status, 0);
                assert_eq!(serial, "partial boot");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn reset_success_marker_rejects_a_nonzero_exit_even_with_the_marker() {
        // Only a clean reset (status 0) is the success signal; a non-zero exit
        // is a failure regardless of what the guest printed.
        let spec = Spec::for_x86_64_kernel("/tmp/k").with_reset_success_marker("memtest: PASSED");
        match outcome_from_done(DoneReason::Exited(1), &spec, "memtest: PASSED".into()) {
            Outcome::Fail { status, .. } => assert_eq!(status, 1),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn without_a_reset_marker_the_per_arch_convention_still_applies() {
        // The opt-in changes nothing by default: on x86_64 a status-0 exit is
        // still the per-arch failure (success there is the isa-debug-exit
        // 0x21 status), so an unrelated test cannot pass by exiting 0.
        let spec = Spec::for_x86_64_kernel("/tmp/k");
        assert!(matches!(
            outcome_from_done(DoneReason::Exited(0), &spec, String::new()),
            Outcome::Fail { .. }
        ));
        let ok = i32::from((SUCCESS_EXIT_CODE << 1) | 1);
        assert!(matches!(
            outcome_from_done(DoneReason::Exited(ok), &spec, String::new()),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn completion_gate_builder_records_the_flag() {
        // By default there is no gate: the run is driven solely by the guest.
        let plain = Spec::for_riscv64_kernel("/tmp/k");
        assert!(plain.completion_gate.is_none());
        // `with_completion_gate` records the shared flag the harness observer
        // trips; the same `Arc` is what the runner reads each poll tick.
        let gate = Arc::new(AtomicBool::new(false));
        let spec = Spec::for_riscv64_kernel("/tmp/k").with_completion_gate(Arc::clone(&gate));
        let recorded = spec.completion_gate.expect("gate recorded");
        assert!(!recorded.load(Ordering::Acquire));
        gate.store(true, Ordering::Release);
        assert!(
            recorded.load(Ordering::Acquire),
            "the recorded gate is the same flag the harness trips"
        );
    }

    #[test]
    fn completed_by_gate_scores_pass_regardless_of_arch_convention() {
        // The out-of-guest observer's confirmation is success on its own: the
        // guest never wrote a debug-exit code (it did not self-exit), so the
        // outcome must be Pass without consulting the per-arch status rule.
        let spec = Spec::for_riscv64_kernel("/tmp/k")
            .with_completion_gate(Arc::new(AtomicBool::new(true)));
        assert!(matches!(
            outcome_from_done(DoneReason::CompletedByGate, &spec, "campaign log".into()),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn an_unfinished_run_reports_the_ceiling_and_the_silence_that_diagnoses_it() {
        // A run that reaches its ceiling is *not* the same failure as a guest
        // that fell silent, and the report must not collapse them: the silence
        // at the kill is what tells a reader which hunt to start. A guest that
        // was still talking (silence far below the ceiling) never finished
        // while alive; one that went quiet early stalled, with the
        // transcript's last line as the stall point.
        let budget = Duration::from_secs(360);
        let spec = Spec::for_riscv64_kernel("/tmp/k").with_timeout(budget);
        let live_but_unfinished = outcome_from_done(
            DoneReason::CeilingExceeded {
                silent_for: Duration::from_millis(20),
                cpu_state: String::new(),
            },
            &spec,
            "campaign log".into(),
        );
        match live_but_unfinished {
            Outcome::RuntimeCeilingExceeded {
                ceiling,
                silent_for,
                serial,
                ..
            } => {
                assert_eq!(ceiling, spec.runtime_ceiling());
                assert_eq!(silent_for, Duration::from_millis(20));
                assert_eq!(serial, "campaign log");
            }
            other => panic!("an unfinished run must not be reported as {other:?}"),
        }

        // The stalled shape carries its own silence, so the two are
        // distinguishable from the report alone.
        let stalled = outcome_from_done(
            DoneReason::CeilingExceeded {
                silent_for: Duration::from_secs(355),
                cpu_state: String::new(),
            },
            &spec,
            "boot log".into(),
        );
        let Outcome::RuntimeCeilingExceeded { silent_for, .. } = stalled else {
            panic!("a run stopped at its ceiling must report the ceiling outcome");
        };
        assert_eq!(silent_for, Duration::from_secs(355));
    }

    #[test]
    fn a_silent_guest_is_still_reported_as_a_plain_timeout() {
        // The inactivity budget keeps its own outcome, so a guest that stops
        // talking reads as a timeout rather than as an unfinished run.
        let spec = Spec::for_riscv64_kernel("/tmp/k").with_timeout(Duration::from_secs(60));
        assert!(matches!(
            outcome_from_done(
                DoneReason::TimedOut {
                    cpu_state: String::new()
                },
                &spec,
                "boot log".into()
            ),
            Outcome::Timeout { budget, .. } if budget == Duration::from_secs(60)
        ));
    }

    #[test]
    fn a_guest_that_keeps_talking_without_finishing_is_killed_at_the_ceiling() {
        // The regression this ceiling exists for: a guest that never completes
        // but keeps printing resets the inactivity heartbeat forever, so the
        // heartbeat alone can never end its run — one such guest wedged the
        // whole test matrix indefinitely. Stood up with an ordinary chattering
        // child rather than a real guest, because the fault is in the
        // supervision loop, not in any architecture's boot path.
        //
        // The child prints a hundred times more often than the silence budget
        // allows, so the heartbeat cannot fire however loaded the host is: the
        // ceiling is provably the bound under test, and the assertion is not a
        // timing race.
        let mut command = Command::new("sh");
        command
            .args(["-c", "while :; do echo tick; sleep 0.02; done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().expect("spawn the chattering child");

        let spec = Spec::for_riscv64_kernel("/tmp/k").with_timeout(Duration::from_secs(2));
        let started = Instant::now();
        let outcome = supervise(child, &spec, None).expect("supervision must not error");
        let elapsed = started.elapsed();

        match outcome {
            Outcome::RuntimeCeilingExceeded {
                ceiling,
                silent_for,
                ..
            } => {
                assert_eq!(ceiling, spec.runtime_ceiling());
                assert!(
                    silent_for < spec.timeout,
                    "the child was still talking at the kill, so its reported silence \
                     must be below the inactivity budget, got {silent_for:?}"
                );
            }
            other => panic!("a live, never-finishing child must hit the ceiling, got {other:?}"),
        }
        assert!(
            elapsed >= spec.runtime_ceiling(),
            "the run must last its whole ceiling, took {elapsed:?}"
        );
        assert!(
            elapsed < spec.runtime_ceiling() * 4,
            "the run must end promptly at its ceiling, took {elapsed:?}"
        );
    }

    #[test]
    fn a_pass_carries_the_transcript_a_later_assertion_needs() {
        // A guest exiting successfully does not finish the verification: the
        // caller still asserts its screendumps and its link peer's verdict.
        // Dropping the transcript on the pass path left those failures with
        // nothing to diagnose from, which is why the pass carries it too.
        let spec = Spec::for_riscv64_kernel("/tmp/k");
        let Outcome::Pass { serial } =
            outcome_from_done(DoneReason::CompletedByGate, &spec, "boot log".into())
        else {
            panic!("a gated completion is a pass");
        };
        assert_eq!(serial, "boot log");

        let Outcome::Pass { serial } =
            Outcome::from_qemu_status(i32::from((SUCCESS_EXIT_CODE << 1) | 1), "boot log".into())
        else {
            panic!("the success status is a pass");
        };
        assert_eq!(serial, "boot log");
    }

    #[test]
    fn the_runtime_ceiling_outlasts_the_inactivity_budget_it_is_derived_from() {
        // One declared budget yields both bounds, so they cannot drift apart,
        // and the ceiling is the looser of the two: a guest that merely falls
        // silent must be diagnosed as a timeout, never mislabelled as an
        // unfinished run.
        let spec = Spec::for_riscv64_kernel("/tmp/k").with_timeout(Duration::from_secs(60));
        assert!(spec.runtime_ceiling() > spec.timeout);
        assert_eq!(spec.runtime_ceiling(), Duration::from_secs(120));
    }

    #[test]
    fn a_declared_runtime_ceiling_replaces_the_derived_one_and_keeps_the_budget_sharp() {
        // A guest whose success is one long sweep needs a ceiling sized to that
        // work, not to a multiple of how long it may go quiet — while the
        // silence budget stays exactly as tight as it was.
        let spec = Spec::for_aarch64_kernel("/tmp/k")
            .with_timeout(Duration::from_secs(60))
            .with_runtime_ceiling(Duration::from_mins(15));
        assert_eq!(spec.runtime_ceiling(), Duration::from_mins(15));
        assert_eq!(spec.timeout, Duration::from_secs(60));
    }

    #[test]
    fn a_declared_ceiling_below_the_inactivity_budget_is_floored_at_it() {
        // A ceiling inside the silence budget would end every run before a
        // silent guest could ever be diagnosed as hung, collapsing the two
        // distinct faults into one; the budget is therefore the floor.
        let spec = Spec::for_x86_64_kernel("/tmp/k")
            .with_timeout(Duration::from_secs(60))
            .with_runtime_ceiling(Duration::from_secs(5));
        assert_eq!(spec.runtime_ceiling(), Duration::from_secs(60));
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
    fn progress_clock_resets_the_heartbeat_only_when_output_grows() {
        let start = Instant::now();
        let mut clock = ProgressClock::new(start);
        // Growth is progress: the heartbeat moves to the observation instant.
        assert!(clock.observe(10, start + Duration::from_secs(1)));
        assert_eq!(
            clock.idle_for(start + Duration::from_secs(1)),
            Duration::ZERO
        );
        // No growth (equal or shorter) is not progress and never moves it.
        assert!(!clock.observe(10, start + Duration::from_secs(2)));
        assert!(!clock.observe(3, start + Duration::from_secs(3)));
        // Idle is measured from the last growth, not the last observation.
        assert_eq!(
            clock.idle_for(start + Duration::from_secs(4)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn progress_clock_idle_only_reaches_the_budget_after_real_silence() {
        let start = Instant::now();
        let budget = Duration::from_secs(60);
        let mut clock = ProgressClock::new(start);
        // A guest that keeps emitting output, however slowly, never lets the
        // idle time reach the budget — this is what makes a slow guest safe
        // to co-schedule without a flaky timeout.
        let mut now = start;
        for _ in 0..100 {
            now += Duration::from_secs(30);
            assert!(clock.observe(clock.seen_len + 1, now));
            assert!(clock.idle_for(now) < budget);
        }
        // Once the output truly stops, the idle time crosses the budget.
        assert!(clock.idle_for(now + budget) >= budget);
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
        // The ETX byte types the Ctrl-C job-control chord (`plans/PTY.md`).
        assert_eq!(qkeycode_for('\u{3}').as_deref(), Ok("ctrl-c"));
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
    fn reserved_socket_paths_fit_the_unix_socket_bound() {
        // The bound is what makes a wire bindable on every host: a name built
        // from a long test-binary name overflowed `sun_path` under macOS's
        // 49-byte temp directory, and did so only for the longest-named
        // tests, which read like a per-test mystery rather than a naming bug.
        for role in ["mon", "net0q", "net0p", "net1q", "net1p"] {
            let sock = ReservedSocket::reserve(role).expect("reserve");
            let len = sock.path().as_os_str().len();
            assert!(
                len < SOCKET_PATH_MAX,
                "{} is {len} bytes, over the {SOCKET_PATH_MAX}-byte bound",
                sock.path().display()
            );
        }
    }

    #[test]
    fn reserved_socket_paths_are_unique_within_a_process() {
        // Concurrent runs in one process (the soak) must never share a wire.
        let a = ReservedSocket::reserve("net0p").expect("reserve");
        let b = ReservedSocket::reserve("net0p").expect("reserve");
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn dropping_a_reserved_socket_removes_its_file() {
        let sock = ReservedSocket::reserve("droptest").expect("reserve");
        let path = sock.path().to_path_buf();
        std::fs::write(&path, b"").expect("create socket stand-in");
        assert!(path.exists());
        drop(sock);
        assert!(!path.exists(), "a run leaves no stray socket behind");
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
            declared_runtime_ceiling: None,
            declared_ram_mib: None,
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
            monitor_commands: Vec::new(),
            session: SessionKind::HeadlessTest,
            reset_success_marker: None,
            completion_gate: None,
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
