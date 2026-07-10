//! HID boot-protocol mouse report decode (USB HID 1.11 §B.2).
//!
//! A boot mouse delivers a report of at least 3 bytes:
//!
//! | byte | content                                              |
//! |------|------------------------------------------------------|
//! | 0    | button bitmap (bit 0 left, bit 1 right, bit 2 middle) |
//! | 1    | signed X displacement (two's complement)             |
//! | 2    | signed Y displacement (two's complement)             |
//! | 3    | signed wheel displacement (common extension, optional) |
//! | 4..  | device-specific (ignored)                            |
//!
//! Buttons carry *state* and are diffed against the previous report;
//! displacements are *edges* and surface directly as `Pointer` /
//! `Scroll` deltas on the shared axis encoding ([`crate::AXIS_X`] /
//! [`crate::AXIS_Y`]).

use rustos_abi::driver::input::{Input, InputEvent, InputEventKind};
use rustos_abi::DriverError;

use crate::{
    poll_source, PendingEvents, ReportDecode, ReportSource, AXIS_X, AXIS_Y,
    POINTER_BUTTON_CODE_BASE, POINTER_BUTTON_COUNT,
};

/// Minimum byte length of a boot mouse input report.
pub const BOOT_MOUSE_REPORT_MIN: usize = 3;

/// Buttons the boot protocol defines (bits 0..3 of byte 0) — exactly the
/// shared platform-neutral button set. Bits 3..8 are device-specific per
/// §B.2 and deliberately not interpreted.
#[allow(clippy::cast_possible_truncation)] // 3 fits a u8 by definition.
const BUTTON_COUNT: u8 = POINTER_BUTTON_COUNT as u8;

/// Mask selecting the boot-protocol button bits.
const BUTTON_MASK: u8 = (1 << BUTTON_COUNT) - 1;

/// Worst-case events one report can decode to: three button edges plus
/// X, Y, and wheel deltas.
const MAX_EVENTS: usize = BUTTON_COUNT as usize + 3;

/// Boot-protocol mouse state: the previously reported button bitmap.
///
/// Kept separate from [`BootMouse`] so the shared [`poll_source`] drain
/// can borrow the state and the report source disjointly.
pub(crate) struct MouseState {
    buttons: u8,
}

impl MouseState {
    const fn new() -> Self {
        Self { buttons: 0 }
    }
}

/// Append a motion event when `delta` is non-zero.
fn push_motion(
    pending: &mut PendingEvents<MAX_EVENTS>,
    kind: InputEventKind,
    axis: u16,
    delta: i8,
) -> Result<(), DriverError> {
    if delta == 0 {
        return Ok(());
    }
    pending.push(InputEvent {
        kind,
        reserved0: 0,
        code: axis,
        value: i32::from(delta),
    })
}

impl ReportDecode<MAX_EVENTS> for MouseState {
    /// Validate and decode one boot mouse report.
    ///
    /// Rejects any report shorter than [`BOOT_MOUSE_REPORT_MIN`] bytes
    /// ([`DriverError::LengthOutOfRange`]) without touching the button
    /// state; reports longer than [`crate::REPORT_BUF_LEN`] never reach
    /// the decoder (the source contract caps them). Byte 3, when
    /// present, is the de-facto wheel extension; trailing bytes beyond
    /// it are device-specific and not interpreted.
    fn decode(
        &mut self,
        report: &[u8],
        pending: &mut PendingEvents<MAX_EVENTS>,
    ) -> Result<(), DriverError> {
        if report.len() < BOOT_MOUSE_REPORT_MIN {
            return Err(DriverError::LengthOutOfRange);
        }
        let buttons = report[0] & BUTTON_MASK;
        let changed = self.buttons ^ buttons;
        for bit in 0..BUTTON_COUNT {
            if changed & (1 << bit) != 0 {
                let pressed = buttons & (1 << bit) != 0;
                pending.push(InputEvent {
                    kind: InputEventKind::Key,
                    reserved0: 0,
                    code: POINTER_BUTTON_CODE_BASE + u16::from(bit),
                    value: i32::from(pressed),
                })?;
            }
        }
        self.buttons = buttons;
        let delta = |byte: u8| i8::from_le_bytes([byte]);
        push_motion(pending, InputEventKind::Pointer, AXIS_X, delta(report[1]))?;
        push_motion(pending, InputEventKind::Pointer, AXIS_Y, delta(report[2]))?;
        if report.len() > BOOT_MOUSE_REPORT_MIN {
            push_motion(pending, InputEventKind::Scroll, AXIS_Y, delta(report[3]))?;
        }
        Ok(())
    }
}

/// A USB boot-protocol mouse, reached through a [`ReportSource`].
///
/// The driver holds the source for the whole load; dropping the
/// [`BootMouse`] is the quiesce step (the decoder issues the device
/// nothing, so a reload is constructing a fresh instance over the same
/// endpoint). The button bitmap and the undrained-event latch are the
/// only state carried between [`poll`](Input::poll) calls.
pub struct BootMouse<S: ReportSource> {
    source: S,
    state: MouseState,
    pending: PendingEvents<MAX_EVENTS>,
}

impl<S: ReportSource> BootMouse<S> {
    /// Bind the decoder to the report stream reachable through
    /// `source`. Performs no I/O; the first [`poll`](Input::poll) is
    /// the first access.
    #[must_use]
    pub fn new(source: S) -> Self {
        Self {
            source,
            state: MouseState::new(),
            pending: PendingEvents::new(),
        }
    }
}

impl<S: ReportSource> Input for BootMouse<S> {
    /// Drain pending mouse reports into `events`.
    ///
    /// Button edges are diffed against the previous report; X/Y/wheel
    /// deltas surface directly. Events that do not fit are latched for
    /// the next `poll`, and the per-call report budget
    /// ([`crate::REPORT_POLL_BUDGET`]) bounds the work a flooding
    /// device can force on one `poll`.
    fn poll(&mut self, events: &mut [InputEvent]) -> Result<usize, DriverError> {
        poll_source(&mut self.source, &mut self.state, &mut self.pending, events)
    }
}
