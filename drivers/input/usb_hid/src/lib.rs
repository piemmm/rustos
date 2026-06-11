//! RustOS USB-HID boot-protocol input driver (keyboard + mouse).
//!
//! Decodes the two **HID boot-protocol** report formats — the 8-byte
//! keyboard report and the 3-or-more-byte mouse report (USB HID 1.11
//! Appendix B) — into platform-neutral
//! [`rustos_abi::driver::input::InputEvent`]s. Boot protocol is the
//! fixed report shape every USB keyboard and mouse must speak without a
//! report-descriptor parse, which makes it the correct first bring-up
//! path for the Pi 4's USB ports (`plans/PI.md` P10): the decoder needs
//! no descriptor parsing and is proven host-side.
//!
//! # Layered seam
//!
//! The decoders ([`BootKeyboard`], [`BootMouse`]) are written against
//! the [`ReportSource`] seam, not a concrete USB transfer ring. The
//! seam is defined in `lib/abi` (`rustos_abi::driver::input`) because
//! its producer is a sibling driver — the xHCI driver
//! (`drivers/bus/usb`) servicing the device's interrupt-IN endpoint —
//! and drivers depend only on `lib/*` (`AGENTS.md` §17.4). Host tests
//! drive the decoders over a mock report queue. This mirrors the
//! `emmc2` `SdhciHost` and `rpi_hvs` mailbox seams (`AGENTS.md` §2.2):
//! the protocol layer is proven host-side, the transport below it on
//! metal.
//!
//! # Event encoding
//!
//! * Keyboard keys surface as [`InputEventKind::Key`] events whose
//!   `code` is the **HID usage ID** from usage page `0x07`
//!   (`0x04` = `A`, …); the eight boot modifiers surface as usages
//!   `0xE0..=0xE7` ([`keyboard::MODIFIER_USAGE_BASE`]). `value` is `1`
//!   for a press and `0` for a release. Keymap translation, repeat,
//!   and lock state are higher-layer concerns (as for `ps2`).
//! * Mouse buttons surface as `Key` events with codes
//!   [`mouse::BUTTON_CODE_BASE`]` + n` (`0x110`/`0x111`/`0x112` for
//!   left/right/middle — the same codes a virtio pointer device
//!   delivers, so the WM sees one button vocabulary, `AGENTS.md` §2.2).
//! * Motion surfaces as `Pointer` events on axes [`AXIS_X`]/[`AXIS_Y`]
//!   and wheel motion as `Scroll` on [`AXIS_Y`], matching the
//!   `lib/abi` axis encoding (`lib/abi/src/driver/input.rs`).
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`BootKeyboard`] and [`BootMouse`] are public *types* the driver
//! host instantiates over the report sources it wires up; the host
//! never reaches them beyond the [`Input`] trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; `poll` requires no
//! further per-method capability (the dispatcher routes decoded events
//! to the focused session — see `lib/abi/src/driver/input.rs`). The
//! driver runs in user space and does not request `CAP_DRV_KERNEL`
//! (`AGENTS.md` §4 / §8).
//!
//! [`Input`]: rustos_abi::driver::input::Input
//! [`InputEventKind::Key`]: rustos_abi::driver::input::InputEventKind::Key

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::input::{InputEvent, InputEventKind};
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

pub mod keyboard;
pub mod mouse;

#[cfg(test)]
mod tests;

pub use keyboard::BootKeyboard;
pub use mouse::BootMouse;
pub use rustos_abi::driver::input::ReportSource;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"UHID"` with a version nibble, matching the other
/// drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x5548_4944_0000_0001;

/// `code` value for the X axis in the platform-neutral
/// [`InputEventKind::Pointer`] / [`InputEventKind::Scroll`] encoding
/// (`lib/abi/src/driver/input.rs`).
pub const AXIS_X: u16 = 0;

/// `code` value for the Y axis in the platform-neutral pointer /
/// scroll encoding.
pub const AXIS_Y: u16 = 1;

/// Byte length of the report buffer a [`poll`](rustos_abi::driver::input::Input::poll)
/// hands to [`ReportSource::next_report`].
///
/// The boot keyboard report is exactly 8 bytes and the boot mouse
/// report is 3 bytes plus up to 5 device-specific trailing bytes (USB
/// HID 1.11 §B.1/§B.2), so 8 bytes holds every report either decoder
/// accepts. A source delivering a longer report is rejected fail-closed
/// by the decoders' length validation (`AGENTS.md` §5.4).
pub const REPORT_BUF_LEN: usize = 8;

/// Upper bound on reports consumed by a single `poll`.
///
/// A bound on a *defence* against a hostile or faulty device that
/// streams reports faster than the caller drains events — not a
/// scalable capacity (`AGENTS.md` §24.4). Undrained reports stay queued
/// at the source and are consumed by the next `poll`, so the bound
/// never loses input; it only stops a single `poll` from spinning
/// (`AGENTS.md` §2.1).
pub const REPORT_POLL_BUDGET: usize = 64;

/// Driver entry point (`AGENTS.md` §8).
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

/// The zeroed placeholder event slots of a [`PendingEvents`] hold.
const EVENT_ZERO: InputEvent = InputEvent {
    kind: InputEventKind::Key,
    reserved0: 0,
    code: 0,
    value: 0,
};

/// Fixed-capacity FIFO of decoded events not yet handed to a caller.
///
/// One boot report can decode to more events than the caller's buffer
/// has room for (a keyboard report releasing six keys while pressing
/// six others). The decoder always decodes a consumed report *whole*
/// into this latch — never half-applies it (`AGENTS.md` §5.4) — and
/// `poll` drains the latch across calls, so no event is ever dropped.
/// `N` is each decoder's worst-case events-per-report, a protocol
/// constant, not a capacity (`AGENTS.md` §24.4).
pub(crate) struct PendingEvents<const N: usize> {
    events: [InputEvent; N],
    len: usize,
    next: usize,
}

impl<const N: usize> PendingEvents<N> {
    pub(crate) const fn new() -> Self {
        Self {
            events: [EVENT_ZERO; N],
            len: 0,
            next: 0,
        }
    }

    /// Append `event`, failing closed if the latch is full.
    ///
    /// The decoders bound their per-report event count by `N`, so a
    /// full latch means a decoder-internal accounting bug; surfacing it
    /// as an error beats silently dropping input (`AGENTS.md` §2.9).
    pub(crate) fn push(&mut self, event: InputEvent) -> Result<(), DriverError> {
        if self.len == N {
            return Err(DriverError::DeviceFault);
        }
        self.events[self.len] = event;
        self.len += 1;
        Ok(())
    }

    pub(crate) fn pop(&mut self) -> Option<InputEvent> {
        if self.next == self.len {
            return None;
        }
        let event = self.events[self.next];
        self.next += 1;
        if self.next == self.len {
            self.next = 0;
            self.len = 0;
        }
        Some(event)
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.next == self.len
    }
}

/// Per-device decoder state: turn one validated report into events.
///
/// Implemented by [`keyboard::KeyboardState`] and [`mouse::MouseState`];
/// the shared [`poll_source`] drives either through this trait so the
/// drain loop exists exactly once (`AGENTS.md` §2.2).
pub(crate) trait ReportDecode<const N: usize> {
    /// Decode `report` whole into `pending`, updating the device state.
    ///
    /// Must validate every byte of `report` and reject the whole report
    /// on any failure without touching the device state (`AGENTS.md`
    /// §5.4).
    fn decode(&mut self, report: &[u8], pending: &mut PendingEvents<N>) -> Result<(), DriverError>;
}

/// Shared `poll` drain: latch first, then budgeted report consumption.
///
/// Drains previously latched events into `events`, then consumes up to
/// [`REPORT_POLL_BUDGET`] reports from `source` — decoding each whole
/// into the latch and moving what fits into `events` — stopping when no
/// report is pending, `events` is full, or the budget is spent.
pub(crate) fn poll_source<S: ReportSource, D: ReportDecode<N>, const N: usize>(
    source: &mut S,
    state: &mut D,
    pending: &mut PendingEvents<N>,
    events: &mut [InputEvent],
) -> Result<usize, DriverError> {
    if events.is_empty() {
        return Err(DriverError::BufferTooSmall);
    }
    let mut written = 0;
    while written < events.len() {
        if let Some(event) = pending.pop() {
            events[written] = event;
            written += 1;
        } else {
            break;
        }
    }
    let mut budget = REPORT_POLL_BUDGET;
    while written < events.len() && pending.is_empty() && budget > 0 {
        budget -= 1;
        let mut buf = [0u8; REPORT_BUF_LEN];
        let Some(len) = source.next_report(&mut buf)? else {
            break;
        };
        if len > buf.len() {
            // The source claims more bytes than it was given room for.
            return Err(DriverError::DeviceFault);
        }
        state.decode(&buf[..len], pending)?;
        while written < events.len() {
            if let Some(event) = pending.pop() {
                events[written] = event;
                written += 1;
            } else {
                break;
            }
        }
    }
    Ok(written)
}
