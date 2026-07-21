//! TAIRiX HID boot-protocol decode + console producer (`lib/hid`).
//!
//! This is the arch-neutral, transport-agnostic HID boot-protocol *decode*
//! logic the USB-HID keyboard/mouse **class** drivers are built from. It lives
//! in `lib/*` — not in a driver crate — so a class driver
//! (`drivers/input/usb_kbd`, …) composes it without a `drivers/*`→`drivers/*`
//! dependency, exactly as the bus-agnostic xHCI protocol lives in the
//! `tairix_usb` crate rather than the xHCI driver. Controller bring-up and
//! enumeration are **not** here: they belong to the host-controller driver
//! (`drivers/bus/usb/xhci`), which serves a class driver's transfers over the
//! URB transport (`plans/USB.md`).
//!
//! # What it decodes
//!
//! The two **HID boot-protocol** report formats — the fixed 8-byte keyboard
//! report and the 3-or-more-byte mouse report (USB HID 1.11 Appendix B) —
//! into platform-neutral [`tairix_abi::driver::input::InputEvent`]s. Boot
//! protocol is the fixed report shape every USB keyboard and mouse must speak
//! without a report-descriptor parse, which makes it the correct first
//! bring-up path for the Pi 4's USB ports (`plans/PI.md` P10): the decoder
//! needs no descriptor parsing and is proven host-side.
//!
//! # Layered seam
//!
//! The decoders ([`BootKeyboard`], [`BootMouse`]) are written against the
//! [`ReportSource`] seam, defined in `lib/abi` (`tairix_abi::driver::input`)
//! because its producer is the class driver's URB transport (which submits
//! interrupt-IN URBs to the host-controller driver servicing the device's
//! interrupt-IN endpoint), and a `lib/*` crate depends only on other `lib/*`
//! crates. Host tests drive the decoders over a mock report queue: the
//! protocol layer is proven host-side, the transport below it on metal.
//!
//! # Event encoding
//!
//! * Keyboard keys surface as [`InputEventKind::Key`] events whose `code` is
//!   the **HID usage ID** from usage page `0x07` (`0x04` = `A`, …); the eight
//!   boot modifiers surface as usages `0xE0..=0xE7`
//!   ([`keyboard::MODIFIER_USAGE_BASE`]). `value` is `1` for a press and `0`
//!   for a release.
//! * For a directly attached keyboard the [`console`] producer resolves those
//!   usage edges into the [`Key`](tairix_input::Key) a US layout produces —
//!   applying the held modifiers and caps/num lock — and emits the decoded
//!   [`KeyInput`](tairix_abi::input::KeyInput) record through the shared
//!   `lib/keymap` map; a driver loop injects each record through the
//!   `key_inject` syscall ([`pump_once`], `plans/PI.md` P11), leaving the
//!   encoding and routing to the kernel input-focus arbiter. Key repeat remains a higher-layer concern.
//! * Mouse buttons surface as `Key` events with codes
//!   [`POINTER_BUTTON_CODE_BASE`]` + n` (`0x110`/`0x111`/`0x112` for
//!   left/right/middle — the same codes a virtio pointer device delivers, so
//!   the WM sees one button vocabulary).
//! * Motion surfaces as `Pointer` events on axes [`AXIS_X`]/[`AXIS_Y`] and
//!   wheel motion as `Scroll` on [`AXIS_Y`], matching the `lib/abi` axis
//!   encoding (`lib/abi/src/driver/input.rs`).
//!
//! [`Input`]: tairix_abi::driver::input::Input
//! [`InputEventKind::Key`]: tairix_abi::driver::input::InputEventKind::Key
//! [`Key`]: tairix_input::Key

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::input::{InputEvent, InputEventKind};
use tairix_abi::DriverError;

pub mod console;
pub mod keyboard;
pub mod mouse;
pub mod report;

#[cfg(test)]
mod tests;

pub use console::{pump_once, ConsoleSink, KeyboardConsole};
pub use keyboard::BootKeyboard;
pub use mouse::BootMouse;
pub use report::{
    parse as parse_report_descriptor, HidReportMap, ReportMapSummary, BOOT_KEYBOARD_NORM_LEN,
    BOOT_MOUSE_NORM_LEN, MAX_REPORT_DESCRIPTOR,
};
// The axis and pointer-button codes are the platform-neutral `lib/abi`
// vocabulary; one definition, imported rather than re-derived here.
pub use tairix_abi::driver::input::{
    ReportSource, AXIS_X, AXIS_Y, POINTER_BUTTON_CODE_BASE, POINTER_BUTTON_COUNT,
};

/// Byte length of the report buffer a [`poll`](tairix_abi::driver::input::Input::poll)
/// hands to [`ReportSource::next_report`].
///
/// The boot keyboard report is exactly 8 bytes and the boot mouse
/// report is 3 bytes plus up to 5 device-specific trailing bytes (USB
/// HID 1.11 §B.1/§B.2), so 8 bytes holds every report either decoder
/// accepts. A source delivering a longer report is rejected fail-closed
/// by the decoders' length validation.
pub const REPORT_BUF_LEN: usize = 8;

/// Upper bound on reports consumed by a single `poll`.
///
/// A bound on a *defence* against a hostile or faulty device that
/// streams reports faster than the caller drains events — not a
/// scalable capacity. Undrained reports stay queued
/// at the source and are consumed by the next `poll`, so the bound
/// never loses input; it only stops a single `poll` from spinning.
pub const REPORT_POLL_BUDGET: usize = 64;

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
/// into this latch — never half-applies it — and
/// `poll` drains the latch across calls, so no event is ever dropped.
/// `N` is each decoder's worst-case events-per-report, a protocol
/// constant, not a capacity.
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
    /// as an error beats silently dropping input.
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
/// drain loop exists exactly once.
pub(crate) trait ReportDecode<const N: usize> {
    /// Decode `report` whole into `pending`, updating the device state.
    ///
    /// Must validate every byte of `report` and reject the whole report
    /// on any failure without touching the device state.
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
