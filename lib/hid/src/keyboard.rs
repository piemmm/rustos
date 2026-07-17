//! HID boot-protocol keyboard report decode (USB HID 1.11 §B.1).
//!
//! A boot keyboard delivers a fixed 8-byte input report:
//!
//! | byte | content                                            |
//! |------|----------------------------------------------------|
//! | 0    | modifier bitmap (usages `0xE0..=0xE7`, bit `n` = `0xE0 + n`) |
//! | 1    | reserved / OEM (not interpreted)                   |
//! | 2..8 | up to six concurrently held key usage IDs (page 7) |
//!
//! The report carries *state*, not edges: every held key appears in
//! every report. The decoder state therefore diffs each accepted report
//! against the previous one and emits one [`InputEvent`] per key edge —
//! release events first, then presses, then modifier changes — exactly
//! once per edge.

use tairix_abi::driver::input::{Input, InputEvent, InputEventKind};
use tairix_abi::DriverError;

use crate::{poll_source, PendingEvents, ReportDecode, ReportSource};

/// Byte length of a boot keyboard input report.
pub const BOOT_KEYBOARD_REPORT_LEN: usize = 8;

/// Key-array slots in a boot keyboard report (bytes 2..8).
const KEY_SLOTS: usize = 6;

/// HID usage ID of the first modifier (`LeftControl`); modifier bit `n`
/// of report byte 0 surfaces as the `Key` code `MODIFIER_USAGE_BASE + n`
/// (`0xE0..=0xE7`, HID Usage Tables).
pub const MODIFIER_USAGE_BASE: u16 = 0xE0;

/// Largest key-array error usage (`0x01` `ErrorRollOver`, `0x02`
/// `POSTFail`, `0x03` `ErrorUndefined`). An array slot in `0x01..=0x03`
/// marks the whole array as invalid for that report.
const ERROR_USAGE_MAX: u8 = 0x03;

/// Key-array value meaning "no key in this slot".
const USAGE_NONE: u8 = 0x00;

/// Worst-case events one report can decode to: six releases plus six
/// presses (the key array fully replaced) plus eight modifier edges.
const MAX_EVENTS: usize = 2 * KEY_SLOTS + 8;

/// Boot-protocol keyboard state: the previously reported hold set.
///
/// Kept separate from [`BootKeyboard`] so the shared
/// [`poll_source`] drain can borrow the state and the report source
/// disjointly.
pub(crate) struct KeyboardState {
    modifiers: u8,
    keys: [u8; KEY_SLOTS],
}

impl KeyboardState {
    const fn new() -> Self {
        Self {
            modifiers: 0,
            keys: [USAGE_NONE; KEY_SLOTS],
        }
    }

    fn push_key(
        pending: &mut PendingEvents<MAX_EVENTS>,
        code: u16,
        pressed: bool,
    ) -> Result<(), DriverError> {
        pending.push(InputEvent {
            kind: InputEventKind::Key,
            reserved0: 0,
            code,
            value: i32::from(pressed),
        })
    }
}

impl ReportDecode<MAX_EVENTS> for KeyboardState {
    /// Validate and diff one boot keyboard report.
    ///
    /// Rejects any report that is not exactly
    /// [`BOOT_KEYBOARD_REPORT_LEN`] bytes
    /// ([`DriverError::LengthOutOfRange`]) without touching the held
    /// state. Byte 1 is reserved/OEM by the spec and deliberately not
    /// interpreted — real keyboards put vendor data there, so enforcing
    /// zero would refuse conforming hardware.
    ///
    /// If any key-array slot carries an error usage (`0x01..=0x03` —
    /// rollover or POST failure), the array is unknown for this report:
    /// the modifier bitmap (still valid per §B.1) is diffed, the held
    /// key set is left untouched, and no key edges are fabricated
    /// (never guess).
    fn decode(
        &mut self,
        report: &[u8],
        pending: &mut PendingEvents<MAX_EVENTS>,
    ) -> Result<(), DriverError> {
        if report.len() != BOOT_KEYBOARD_REPORT_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let modifiers = report[0];
        let mut keys = [USAGE_NONE; KEY_SLOTS];
        keys.copy_from_slice(&report[2..2 + KEY_SLOTS]);
        let array_valid = !keys
            .iter()
            .any(|&k| k != USAGE_NONE && k <= ERROR_USAGE_MAX);

        if array_valid {
            // Releases first: keys held before but absent now.
            for &old in &self.keys {
                if old != USAGE_NONE && !keys.contains(&old) {
                    Self::push_key(pending, u16::from(old), false)?;
                }
            }
            // Presses: keys present now but not before. The duplicate
            // guard means a hostile report repeating one usage in
            // several slots still produces a single press.
            for (slot, &new) in keys.iter().enumerate() {
                if new != USAGE_NONE && !self.keys.contains(&new) && !keys[..slot].contains(&new) {
                    Self::push_key(pending, u16::from(new), true)?;
                }
            }
            self.keys = keys;
        }

        let changed = self.modifiers ^ modifiers;
        for bit in 0u8..8 {
            if changed & (1 << bit) != 0 {
                let pressed = modifiers & (1 << bit) != 0;
                Self::push_key(pending, MODIFIER_USAGE_BASE + u16::from(bit), pressed)?;
            }
        }
        self.modifiers = modifiers;
        Ok(())
    }
}

/// A USB boot-protocol keyboard, reached through a [`ReportSource`].
///
/// The driver holds the source for the whole load; dropping the
/// [`BootKeyboard`] is the quiesce step (the decoder issues the device
/// nothing, so a reload is constructing a fresh instance over the same
/// endpoint). The held-key set and the undrained-event latch are the
/// only state carried between [`poll`](Input::poll) calls.
pub struct BootKeyboard<S: ReportSource> {
    source: S,
    state: KeyboardState,
    pending: PendingEvents<MAX_EVENTS>,
}

impl<S: ReportSource> BootKeyboard<S> {
    /// Bind the decoder to the report stream reachable through
    /// `source`. Performs no I/O; the first [`poll`](Input::poll) is
    /// the first access.
    #[must_use]
    pub fn new(source: S) -> Self {
        Self {
            source,
            state: KeyboardState::new(),
            pending: PendingEvents::new(),
        }
    }

    /// Mutable access to the underlying report source.
    ///
    /// A driver that owns the concrete source (e.g. the USB boot-keyboard
    /// driver holding an xHCI [`ReportSource`]) reaches it through here to
    /// drive source-specific controls the generic decoder has no business
    /// knowing about — enabling the controller's completion interrupt and
    /// acknowledging it around an `irq_wait`, so the keyboard is serviced
    /// on its interrupt rather than busy-polled. The decode state is
    /// untouched, so interleaving these calls with [`Input::poll`] is safe.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }
}

impl<S: ReportSource> Input for BootKeyboard<S> {
    /// Drain pending keyboard reports into `events`.
    ///
    /// Each consumed report is diffed against the previous one and its
    /// key edges appended; events that do not fit are latched for the
    /// next `poll`, so a too-small buffer loses nothing. The per-call
    /// report budget ([`crate::REPORT_POLL_BUDGET`]) bounds the work a
    /// flooding device can force on one `poll`.
    fn poll(&mut self, events: &mut [InputEvent]) -> Result<usize, DriverError> {
        poll_source(&mut self.source, &mut self.state, &mut self.pending, events)
    }
}
