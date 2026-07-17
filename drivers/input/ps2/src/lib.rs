//! TAIRiX i8042 PS/2 keyboard input driver (x86_64).
//!
//! Implements [`tairix_abi::driver::input::Input`] for a keyboard attached
//! to the Intel 8042 keyboard controller — the legacy "PS/2" controller every
//! x86 PC and QEMU's default `q35`/`i440fx` machines expose. The controller is
//! a two-port byte-addressed register file: the status/command register at
//! I/O port `0x64` and the data register at `0x60`. The driver polls the
//! status register's output-buffer-full bit and, when a byte is waiting,
//! reads and decodes a scancode-set-1 byte stream into platform-neutral
//! [`InputEvent`]s (`lib/abi/src/driver/input.rs`).
//!
//! Higher-level concerns — key repeat, modifier/lock state, keymap
//! translation to characters, and routing to the focused session — live above
//! this driver in `userland/gui/wm` and the session layer. The driver itself
//! only owns the controller drain and the scancode→event decode.
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`].
//! [`Ps2Keyboard`] is a public *type* re-exported so the driver host can
//! instantiate it through [`Ps2Keyboard::new`]; the host reaches it only
//! through the [`Input`] trait afterwards.
//!
//! # Port access
//!
//! The driver never issues an `inb`/`outb` instruction itself. It reaches the
//! two controller ports exclusively through the host-supplied
//! [`PortIo8`] seam (`lib/abi`), which the x86_64
//! architecture port implements. The driver
//! therefore carries no architecture-conditional `cfg` and no ambient
//! authority over the I/O port space: it can only touch the
//! ports the supplied backend lets it.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`].
//! Per-method access is gated by possession of the
//! [`DriverHandle`] the host issues on a successful
//! [`register`]; the [`Input`] trait declares no additional per-method
//! capability (`lib/abi/src/driver/input.rs`). The driver runs in user space;
//! it does not request `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::input::{Input, InputEvent, InputEventKind};
use tairix_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, PortIo8};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// Mirrors the convention the bus, storage, and display drivers use: the host
/// re-issues a host-local handle when binding the driver into its load table;
/// this constant is the on-the-wire signal that every load-time gate cleared.
/// The bytes spell `"PS2K"`.
const REGISTER_HANDLE_MARKER: u64 = 0x5053_324B_0000_0001;

/// I/O port of the 8042 data register (read scancodes, write device bytes).
const DATA_PORT: u16 = 0x60;

/// I/O port of the 8042 status (read) / command (write) register.
const STATUS_PORT: u16 = 0x64;

/// Status-register bit set when the output buffer holds a byte for the host.
const STATUS_OUTPUT_FULL: u8 = 1 << 0;

/// Status-register bit set when the pending byte came from the auxiliary
/// (mouse) port rather than the keyboard port.
const STATUS_AUX_DATA: u8 = 1 << 5;

/// Scancode-set-1 prefix byte introducing an extended (`E0`) keycode.
const EXTENDED_PREFIX: u8 = 0xE0;

/// Scancode-set-1 bit that distinguishes a key-release (break) code from a
/// key-press (make) code.
const RELEASE_BIT: u8 = 0x80;

/// Offset added to an extended (`E0`-prefixed) make code to keep its
/// platform-neutral keycode disjoint from the base set.
const EXTENDED_KEYCODE_BASE: u16 = 0xE000;

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// A PS/2 keyboard attached to the i8042 controller, reached through a
/// [`PortIo8`] backend.
///
/// The driver holds the backend for the whole load; dropping the
/// [`Ps2Keyboard`] is the quiesce step (the controller is left untouched —
/// the driver issues it no commands — so a reload is simply constructing a
/// fresh instance over the same ports). The only mutable state is the
/// one-byte extended-prefix latch carried between [`poll`](Input::poll)
/// calls, so an `E0` prefix that arrives at the tail of one drain is paired
/// with its code on the next.
pub struct Ps2Keyboard<P: PortIo8> {
    port: P,
    extended: bool,
}

impl<P: PortIo8> Ps2Keyboard<P> {
    /// Bind the driver to the controller reachable through `port`.
    ///
    /// Performs **no** I/O: the controller is left in whatever state the
    /// firmware or a prior owner programmed it (this driver relies only on
    /// the controller's power-on default of scancode-set translation, which
    /// QEMU and PC firmware enable). Construction before the controller is
    /// known-good is therefore sound; the first [`poll`](Input::poll) is the
    /// first access.
    #[must_use]
    pub fn new(port: P) -> Self {
        Self {
            port,
            extended: false,
        }
    }

    /// Decode one scancode-set-1 byte, advancing the extended-prefix latch.
    ///
    /// Returns `None` for a byte that does not complete a key event — an `E0`
    /// prefix (latched for the next byte) or a code whose 7-bit make value is
    /// zero (the controller's detection-error / buffer-overrun markers, which
    /// are not keys).
    fn decode(&mut self, raw: u8) -> Option<InputEvent> {
        if raw == EXTENDED_PREFIX {
            self.extended = true;
            return None;
        }
        let released = raw & RELEASE_BIT != 0;
        let make = raw & !RELEASE_BIT;
        let extended = core::mem::replace(&mut self.extended, false);
        if make == 0 {
            return None;
        }
        let code = if extended {
            EXTENDED_KEYCODE_BASE | u16::from(make)
        } else {
            u16::from(make)
        };
        Some(InputEvent {
            kind: InputEventKind::Key,
            reserved0: 0,
            code,
            value: i32::from(!released),
        })
    }
}

impl<P: PortIo8> Input for Ps2Keyboard<P> {
    /// Drain pending keyboard scancodes into `events`.
    ///
    /// Reads the status register and, while the output buffer holds a
    /// keyboard byte, consumes and decodes it. The drain stops when the
    /// output buffer is empty, when the next byte belongs to the auxiliary
    /// (mouse) port — it is **not** consumed, so a future pointer driver
    /// still sees it — when `events` is full, or when a per-call read budget
    /// is exhausted. The budget bounds the work a stuck controller can force
    /// on a single poll, so the driver can never spin; any
    /// undrained bytes are read on the next [`poll`](Input::poll).
    ///
    /// Decoded events use platform-neutral keycodes: a base scancode-set-1
    /// make code (`1..=0x7F`) for unprefixed keys and `0xE000 | make` for
    /// `E0`-extended keys, with `value == 1` for a press and `0` for a
    /// release (`lib/abi/src/driver/input.rs`).
    fn poll(&mut self, events: &mut [InputEvent]) -> Result<usize, DriverError> {
        if events.is_empty() {
            return Err(DriverError::BufferTooSmall);
        }
        let mut written = 0;
        let mut budget = events.len().saturating_mul(2).saturating_add(2);
        while written < events.len() && budget > 0 {
            let status = self.port.read8(STATUS_PORT);
            if status & STATUS_OUTPUT_FULL == 0 {
                break;
            }
            if status & STATUS_AUX_DATA != 0 {
                break;
            }
            budget -= 1;
            let raw = self.port.read8(DATA_PORT);
            if let Some(event) = self.decode(raw) {
                events[written] = event;
                written += 1;
            }
        }
        Ok(written)
    }
}
