//! The pre-boot **Supervisor** console engine (`lib/supervisor`).
//!
//! The Supervisor is a small, built-in command monitor an operator can drop
//! into from the boot screen *before* the encrypted root is mounted — a
//! "quick busybox, but different" for inspecting and controlling the machine
//! while it is still at the bootstrap floor. The boot path draws a brief
//! `[Press ESC for supervisor]` window; pressing `ESC` there (or at the
//! passphrase prompt) enters this REPL, whose prompt is `* `.
//!
//! # A presenter, not a source of truth
//!
//! Every datum the Supervisor shows — the kernel version, the memory map,
//! the hardware tree, the partition table, the boot audit log — is already
//! computed by an existing kernel subsystem. This crate computes none of it:
//! it is a **presenter + control surface** over the [`SupervisorHost`] seam,
//! which the kernel implements over those subsystems. That keeps the one
//! source of truth where it already lives (the charter forbids duplicating
//! it) and keeps this crate tiny and arch-neutral.
//!
//! # Arch-neutral and seam-only
//!
//! The engine names no architecture, board, device, or kernel type. It talks
//! to the outside world only through object-safe seams: [`Report`] for
//! output, [`SupInput`] for keyboard bytes, and [`SupervisorHost`] for the
//! data and the control actions (`reboot`, `poweroff`, `mount`). The kernel
//! wires those to the real console, the interrupt-driven reader, the reset
//! primitive, and the real unlock path; a host unit test wires them to
//! in-memory mocks. Nothing here allocates — the bootstrap floor cannot
//! assume a heap — and nothing here panics on any input (`AGENTS.md` §2.9).
//!
//! # Security
//!
//! The Supervisor runs at full kernel authority at the physical console
//! before any user is authenticated, so its threat model is
//! **physical-console access only** — the physical-attacker class the charter
//! already places out of scope. That is a reason to audit loudly and fail
//! closed, never to weaken a defence: `mount` runs the *real* passphrase
//! unlock (no oracle, no fail-open), no command reveals key material, every
//! command is read-only unless it explicitly performs an audited control
//! action, and entering the console plus every state-changing command is
//! reported through [`SupervisorHost::audit`] as a stable event the kernel
//! maps onto the hash-chained audit log.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

// The engine is allocation-free: lines land in a fixed on-stack buffer,
// tokens are borrowed slices, and numbers format through a fixed inline
// buffer. `alloc` is pulled in only by the unit tests, which build `Vec`s of
// captured output to assert against.
#[cfg(test)]
extern crate alloc;

mod dispatch;
mod repl;

pub mod commands;
pub mod screen;

pub use dispatch::{Command, Flow, Session, COMMANDS};
pub use repl::{run_supervisor, PROMPT};
pub use screen::{Geometry, Screen, Style};

/// The longest command line, in bytes, the Supervisor accepts.
///
/// A pre-boot operator types short commands, so a generous fixed ceiling is
/// correct; it is a validation bound on operator input, not a scalable
/// capacity. A line longer than this is refused (reported and dropped) rather
/// than truncated, so a half-line is never dispatched.
pub const MAX_LINE_LEN: usize = 512;

/// The most command-line tokens the Supervisor splits a line into.
///
/// A fixed bound on untrusted input: extra whitespace-separated words beyond
/// this are folded into the last token rather than growing an unbounded
/// table. No built-in command needs more than a handful of arguments.
pub const MAX_TOKENS: usize = 16;

/// How the Supervisor REPL ended, telling the boot path what to do next.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SupervisorExit {
    /// Resume the normal boot: redraw the passphrase prompt and carry on as
    /// though the Supervisor had never been entered.
    ContinueBoot,
    /// The operator mounted the root from inside the Supervisor (a real,
    /// passphrase-checked unlock). The boot path continues **without** a
    /// second passphrase prompt.
    Mounted,
}

/// The result of a memory test or a disk surface scan.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TestOutcome {
    /// The test ran to completion with no fault.
    Passed,
    /// The test found a fault (a bad RAM cell, an unreadable block). The
    /// details were already written to the [`Report`], secret-free.
    Failed,
    /// The operator pressed `ESC` to abort before completion.
    Aborted,
}

/// The result of a Supervisor `mount` attempt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MountOutcome {
    /// The typed passphrase unlocked the root and it is now mounted.
    Mounted,
    /// The passphrase was wrong (the master key never unwrapped). No oracle:
    /// exactly like the normal unlock's wrong-passphrase path.
    WrongPassphrase,
    /// A structural failure the disk itself cannot satisfy (no table, no
    /// partition, an unreadable/invalid descriptor). Retrying cannot help.
    Failed,
}

/// A security-relevant Supervisor decision the kernel records on the
/// hash-chained audit log through [`SupervisorHost::audit`].
///
/// The engine emits a semantic event; the kernel owns the stable `lib/log`
/// event ids and the level, so this crate carries no logging dependency and
/// the audit vocabulary stays where the rest of the boot audit lives. No
/// event ever carries a secret, key byte, or passphrase.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SupervisorEvent {
    /// The operator entered the Supervisor console from the boot screen.
    Entered,
    /// The operator asked to resume the normal boot (`continue` / `boot`).
    ContinueBoot,
    /// The operator requested a reboot.
    Reboot,
    /// The operator requested a power-off / halt.
    Poweroff,
    /// The operator began an in-Supervisor root `mount` attempt.
    MountAttempt,
    /// An in-Supervisor `mount` unlocked and mounted the root.
    MountOk,
    /// An in-Supervisor `mount` failed (wrong passphrase or structural).
    MountFailed,
    /// The operator confirmed the one-way, destructive `memtest full`
    /// whole-RAM takeover test. Recorded immediately before the takeover is
    /// attempted, because a successful takeover destroys the in-memory audit
    /// ring and never returns — this is the last record the ring can hold.
    MemtestTakeover,
}

/// A byte sink the Supervisor renders its output into.
///
/// The kernel backs it with the console (applying the terminal newline
/// discipline); a host test backs it with an in-memory buffer. Only
/// [`Report::write_bytes`] is required; the formatting helpers have default
/// implementations so command code stays terse and allocation-free.
pub trait Report {
    /// Write `bytes` verbatim to the sink (used for text and for terminal
    /// control sequences alike).
    fn write_bytes(&mut self, bytes: &[u8]);

    /// Write a UTF-8 string.
    fn write_str(&mut self, text: &str) {
        self.write_bytes(text.as_bytes());
    }

    /// Write a string followed by a CR-LF line terminator.
    fn line(&mut self, text: &str) {
        self.write_str(text);
        self.write_bytes(b"\r\n");
    }

    /// End the current line with CR-LF.
    fn newline(&mut self) {
        self.write_bytes(b"\r\n");
    }

    /// Write an unsigned decimal integer, allocation-free.
    fn write_u64(&mut self, mut value: u64) {
        let mut buf = [0u8; 20];
        let mut idx = buf.len();
        loop {
            idx -= 1;
            buf[idx] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.write_bytes(&buf[idx..]);
    }

    /// Write an unsigned value as `0x`-prefixed lower-case hexadecimal,
    /// allocation-free.
    fn write_hex(&mut self, value: u64) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [0u8; 16];
        let mut idx = buf.len();
        let mut v = value;
        loop {
            idx -= 1;
            buf[idx] = DIGITS[(v & 0xf) as usize];
            v >>= 4;
            if v == 0 {
                break;
            }
        }
        self.write_bytes(b"0x");
        self.write_bytes(&buf[idx..]);
    }
}

/// A source of keyboard bytes for the Supervisor REPL.
///
/// The kernel backs it with the interrupt-driven console reader that parks
/// the task until a key arrives (never a busy-spin); a host test backs it
/// with a scripted byte sequence. [`SupInput::read_byte`] blocks (parks);
/// [`SupInput::poll_byte`] never blocks and is used to check for an `ESC`
/// abort during a long-running command.
pub trait SupInput {
    /// Read the next byte, parking until one arrives. Returns [`None`] only
    /// when the input has genuinely ended (the console closed).
    fn read_byte(&mut self) -> Option<u8>;

    /// Return an already-available byte without blocking, or [`None`] if
    /// none is buffered. Used to poll for an `ESC` abort during `memtest` /
    /// `test disk`. The default never yields a byte, so a backing that
    /// cannot poll simply makes those commands non-abortable rather than
    /// spinning.
    fn poll_byte(&mut self) -> Option<u8> {
        None
    }
}

/// The data and control seam the Supervisor presents and drives.
///
/// Every method is implemented by the kernel over an existing subsystem
/// (introspection, the memory-map RAM test, the partition reader, the
/// hardware tree, the audit-log ring, the real unlock path, the port reset
/// primitive). The engine calls them; it never reaches past this seam, so it
/// depends on no `kernel/*` crate and names no device (`AGENTS.md` §17.4).
///
/// All rendering methods take a [`Report`] and are **read-only**: they never
/// write to storage and never expose an arbitrary physical-address read.
/// The control methods are audited by the engine before they run.
pub trait SupervisorHost {
    /// Render the kernel version, build identity, target, and ABI version.
    fn version(&mut self, out: &mut dyn Report);

    /// Render installed/usable RAM, kernel-heap size, and memory-pressure.
    fn memory(&mut self, out: &mut dyn Report);

    /// Render the boot memory map (usable / reserved regions).
    fn memory_map(&mut self, out: &mut dyn Report);

    /// Render the CPU / core count and detected features.
    fn cpu(&mut self, out: &mut dyn Report);

    /// Render the discovered hardware tree (nodes and bind keys).
    fn hardware(&mut self, out: &mut dyn Report);

    /// Render the attached block devices and their geometry.
    fn disks(&mut self, out: &mut dyn Report);

    /// Render the partition table of `device` (MBR/GPT), or an error line
    /// if it cannot be read.
    fn partitions(&mut self, device: &str, out: &mut dyn Report);

    /// Render the root volume's descriptor / label / identity and whether it
    /// is present, is ARXFS, and is unlocked — **without** unlocking it.
    fn arxfs_status(&mut self, out: &mut dyn Report);

    /// List a directory. Pre-mount the only readable volume is the
    /// always-readable `/System`; `path` is `None` for its default listing.
    fn list(&mut self, path: Option<&str>, out: &mut dyn Report);

    /// Render the last `count` in-memory boot audit-log entries (all of them
    /// when `count` is `None`).
    fn log_tail(&mut self, count: Option<usize>, out: &mut dyn Report);

    /// Render a previous boot's recorded panic / lockup diagnostic, if one
    /// exists, or a line saying there is none.
    fn panic_log(&mut self, out: &mut dyn Report);

    /// Render the monotonic time since boot.
    fn uptime(&mut self, out: &mut dyn Report);

    /// Render the wall-clock date/time.
    fn date(&mut self, out: &mut dyn Report);

    /// Run the thorough RAM test for `passes` passes, rendering progress and
    /// any fault to `out`. `abort` is polled between units; when it returns
    /// `true` the test stops early and reports [`TestOutcome::Aborted`]. The
    /// test uses the safe memory-map RAM engine, never raw pointer
    /// arithmetic.
    fn memtest(
        &mut self,
        passes: u32,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome;

    /// Run a bounded, read-only surface scan of `device`, reporting read
    /// errors/timeouts. Never writes. `abort` is polled as for
    /// [`memtest`](SupervisorHost::memtest).
    fn scan_disk(
        &mut self,
        device: &str,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome;

    /// Attempt the **real** root unlock under `passphrase` (no oracle, no
    /// fail-open), rendering the outcome. The engine reads the passphrase
    /// into a zeroized buffer and passes it here; the buffer is wiped by the
    /// caller after this returns.
    fn mount(&mut self, passphrase: &[u8], out: &mut dyn Report) -> MountOutcome;

    /// Reset the machine. On success this never returns; if it returns, the
    /// platform could not reboot and the engine reports the failure.
    fn reboot(&mut self);

    /// Power the machine off / halt. On success this never returns; if it
    /// returns, the platform has no power-off and the engine reports it.
    fn poweroff(&mut self);

    /// Attempt the one-way, destructive `memtest full` whole-RAM takeover
    /// test. The engine calls this **only** after an explicit typed
    /// confirmation and after auditing [`SupervisorEvent::MemtestTakeover`].
    ///
    /// On a platform that can take the machine over this never returns: it
    /// stops every CPU, tests all of RAM (overwriting it), and resets. If it
    /// returns, the takeover did not proceed — the platform has no takeover
    /// mechanism, or a step failed closed — and the machine is unchanged; the
    /// reason has already been rendered to `out` and the engine stays in the
    /// REPL. It never partially tears the machine down.
    fn takeover_memtest(&mut self, out: &mut dyn Report);

    /// Record a security-relevant Supervisor decision on the audit log.
    fn audit(&mut self, event: SupervisorEvent);
}
