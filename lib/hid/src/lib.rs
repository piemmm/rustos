//! RustOS HID boot-protocol decode, console producer, and xHCI boot
//! orchestration (`lib/hid`).
//!
//! This is the arch-neutral, transport-agnostic HID logic the USB-HID
//! keyboard/mouse driver is built from. It lives in `lib/*` — not in the
//! driver crate — so **both** the in-kernel keyboard scaffold (transitional,
//! `plans/PI.md` P10) and the user-space keyboard driver process
//! (`drivers/input/usb_kbd`, the autoloaded steady state) compose it without a
//! `drivers/*`→`drivers/*` dependency, exactly as the
//! bus-agnostic xHCI protocol lives in [`rustos_usb`] rather than the xHCI
//! driver. The thin `drivers/input/usb_hid` driver keeps
//! only the `register` entry and the bind table.
//!
//! # What it decodes
//!
//! The two **HID boot-protocol** report formats — the fixed 8-byte keyboard
//! report and the 3-or-more-byte mouse report (USB HID 1.11 Appendix B) —
//! into platform-neutral [`rustos_abi::driver::input::InputEvent`]s. Boot
//! protocol is the fixed report shape every USB keyboard and mouse must speak
//! without a report-descriptor parse, which makes it the correct first
//! bring-up path for the Pi 4's USB ports (`plans/PI.md` P10): the decoder
//! needs no descriptor parsing and is proven host-side.
//!
//! # Layered seam
//!
//! The decoders ([`BootKeyboard`], [`BootMouse`]) are written against the
//! [`ReportSource`] seam, defined in `lib/abi` (`rustos_abi::driver::input`)
//! because its producer is the xHCI driver (`drivers/bus/usb`) servicing the
//! device's interrupt-IN endpoint, and a `lib/*` crate depends only on other
//! `lib/*` crates. Host tests drive the decoders over a
//! mock report queue — the `emmc2`/`rpi_hvs` seam shape:
//! the protocol layer is proven host-side, the transport below it on metal.
//!
//! # Event encoding
//!
//! * Keyboard keys surface as [`InputEventKind::Key`] events whose `code` is
//!   the **HID usage ID** from usage page `0x07` (`0x04` = `A`, …); the eight
//!   boot modifiers surface as usages `0xE0..=0xE7`
//!   ([`keyboard::MODIFIER_USAGE_BASE`]). `value` is `1` for a press and `0`
//!   for a release.
//! * For a directly attached keyboard the [`console`] producer resolves those
//!   usage edges into the [`Key`](rustos_input::Key) a US layout produces —
//!   applying the held modifiers and caps/num lock — and emits the decoded
//!   [`KeyInput`](rustos_abi::input::KeyInput) record through the shared
//!   `lib/keymap` map; a driver loop injects each record through the
//!   `key_inject` syscall ([`pump_once`], `plans/PI.md` P11), leaving the
//!   encoding and routing to the kernel input-focus arbiter. Key repeat remains a higher-layer concern.
//! * Mouse buttons surface as `Key` events with codes
//!   [`mouse::BUTTON_CODE_BASE`]` + n` (`0x110`/`0x111`/`0x112` for
//!   left/right/middle — the same codes a virtio pointer device delivers, so
//!   the WM sees one button vocabulary).
//! * Motion surfaces as `Pointer` events on axes [`AXIS_X`]/[`AXIS_Y`] and
//!   wheel motion as `Scroll` on [`AXIS_Y`], matching the `lib/abi` axis
//!   encoding (`lib/abi/src/driver/input.rs`).
//!
//! # Boot-keyboard orchestration
//!
//! [`service::bring_up_boot_keyboard`] is the composition a user-space USB
//! boot-keyboard driver runs at start-up: over its
//! [`DriverHost`](rustos_abi::DriverHost) it carves the device-shared DMA
//! region, maps the granted xHCI register BAR, brings the controller up over
//! [`rustos_usb`], and enumerates the boot keyboard.
//! [`service::derive_keyboard_resources`] turns the kernel-issued
//! device-resource grants into the BAR + DMA-aperture bounds that orchestration
//! needs. Both are arch-neutral and name no board.
//!
//! [`Input`]: rustos_abi::driver::input::Input
//! [`InputEventKind::Key`]: rustos_abi::driver::input::InputEventKind::Key
//! [`Key`]: rustos_input::Key

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::input::{InputEvent, InputEventKind};
use rustos_abi::{DriverBindKey, DriverError, HwMatchKey};
use rustos_usb::XHCI_COMPATIBLE;

pub mod console;
pub mod keyboard;
pub mod mouse;
pub mod service;

#[cfg(test)]
mod tests;

pub use console::{pump_once, ConsoleSink, KeyboardConsole};
pub use keyboard::BootKeyboard;
pub use mouse::BootMouse;
pub use rustos_abi::driver::input::ReportSource;
pub use service::{
    bring_up_boot_keyboard, bring_up_boot_keyboard_diagnostic, derive_keyboard_resources,
    BringupPhase, KeyboardBringupError, KeyboardResources, KeyboardSource,
};

/// The bind priority [`KEYBOARD_BIND_KEYS`] carries.
///
/// An exact `compatible`-string match for the controller node, mirroring the
/// other `compatible`-keyed drivers (`drivers/bus/pcie_brcm`,
/// `drivers/storage/emmc2`, priority 10).
const KEYBOARD_BIND_PRIORITY: u16 = 10;

/// The user-space USB boot-keyboard driver's hardware bind table: the xHCI USB host controller, matched by the
/// [`XHCI_COMPATIBLE`] `compatible` string the
/// bus driver publishes the controller node under (`drivers/bus/usb/vl805`'s
/// `node B`). The single source of truth the `drivers/input/usb_kbd` signed
/// manifest's bind table is authored from and `devmgr` resolves the
/// controller node against.
///
/// The keyboard driver brings the whole xHCI controller up itself — the
/// `Xhci` controller object cannot cross a process boundary, so it binds the
/// controller node directly rather than a separately-emitted HID-interface
/// node (`plans/PI.md` P10 D5).
pub const KEYBOARD_BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    KEYBOARD_BIND_PRIORITY,
    match HwMatchKey::compatible(XHCI_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: `XHCI_COMPATIBLE` is well within `HW_COMPATIBLE_MAX`.
        // A too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("XHCI_COMPATIBLE fits HW_COMPATIBLE_MAX"),
    },
)];

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
