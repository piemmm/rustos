//! Mouse tracking: enabling the report modes and the SGR-encoded reports.
//!
//! xterm reports the mouse as an escape sequence once tracking is enabled. A
//! program turns tracking on with a DEC private mode ([`MouseMode`], emitted as
//! `CSI ? <n> h`) and then reads reports in the SGR encoding
//! (`CSI < Cb ; Cx ; Cy M` for a press, `… m` for a release). Both directions
//! are named here once, in the shared vocabulary, so the
//! curses input decoder emits the enable sequences and parses the reports
//! through the one [`crate::Parser`] (`plans/CURSES.md` §C4).
//!
//! [`MouseReport::encode_button`] and [`MouseReport::decode`] are inverses over
//! the SGR `Cb` byte, so a report the emitter writes parses back unchanged.

/// Which mouse button (or wheel direction) a report names.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MouseButton {
    /// The left (primary) button — `Cb` low bits `0`.
    Left,
    /// The middle button — `Cb` low bits `1`.
    Middle,
    /// The right (secondary) button — `Cb` low bits `2`.
    Right,
    /// No button: a bare motion report or a release whose button is unknown —
    /// `Cb` low bits `3`.
    None,
    /// Wheel scrolled up — `Cb` `64`.
    WheelUp,
    /// Wheel scrolled down — `Cb` `65`.
    WheelDown,
}

/// The `Cb` bits that select the button (clearing the modifier and motion
/// bits): the low two bits plus the wheel bit.
const BUTTON_MASK: u16 = 0b1100_0011;
/// `Cb` bit set for a held Shift.
const SHIFT_BIT: u16 = 4;
/// `Cb` bit set for a held Meta/Alt.
const META_BIT: u16 = 8;
/// `Cb` bit set for a held Control.
const CTRL_BIT: u16 = 16;
/// `Cb` bit set for a motion (drag / move) report.
const MOTION_BIT: u16 = 32;

impl MouseButton {
    /// The `Cb` button bits for this button.
    #[must_use]
    const fn code(self) -> u16 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::None => 3,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
        }
    }

    /// The [`MouseButton`] for the masked button bits of a `Cb` byte, failing
    /// closed to [`MouseButton::None`] for an unrecognised pattern.
    #[must_use]
    const fn from_code(code: u16) -> MouseButton {
        match code {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            64 => MouseButton::WheelUp,
            65 => MouseButton::WheelDown,
            _ => MouseButton::None,
        }
    }
}

/// The mouse-tracking protocols a program can enable, as DEC private modes.
///
/// Each is emitted as `CSI ? <number> h` (enable) or `… l` (disable). The
/// richer protocols are supersets of [`MouseMode::Button`]; a program also
/// enables [`MouseMode::Sgr`] to receive the extended report encoding this
/// module parses.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MouseMode {
    /// Report button press and release only (mode `1000`).
    Button,
    /// Also report drags — motion with a button held (mode `1002`).
    Drag,
    /// Report every pointer motion (mode `1003`).
    AnyMotion,
    /// Use the SGR report encoding (mode `1006`), which this module parses.
    Sgr,
}

impl MouseMode {
    /// Every [`MouseMode`] in declaration order, for exhaustive iteration.
    pub const ALL: [MouseMode; 4] = [
        MouseMode::Button,
        MouseMode::Drag,
        MouseMode::AnyMotion,
        MouseMode::Sgr,
    ];

    /// The DEC private mode number this protocol is enabled with.
    #[must_use]
    pub const fn mode_number(self) -> u16 {
        match self {
            MouseMode::Button => crate::control::MODE_MOUSE_BUTTON,
            MouseMode::Drag => crate::control::MODE_MOUSE_DRAG,
            MouseMode::AnyMotion => crate::control::MODE_MOUSE_ANY,
            MouseMode::Sgr => crate::control::MODE_MOUSE_SGR,
        }
    }

    /// The [`MouseMode`] for a DEC private mode number, or `None` if the number
    /// is not a mouse-tracking mode.
    #[must_use]
    pub const fn from_mode_number(number: u16) -> Option<MouseMode> {
        match number {
            crate::control::MODE_MOUSE_BUTTON => Some(MouseMode::Button),
            crate::control::MODE_MOUSE_DRAG => Some(MouseMode::Drag),
            crate::control::MODE_MOUSE_ANY => Some(MouseMode::AnyMotion),
            crate::control::MODE_MOUSE_SGR => Some(MouseMode::Sgr),
            _ => None,
        }
    }
}

/// One SGR-encoded mouse report (`CSI < Cb ; Cx ; Cy M` / `m`).
///
/// `col` and `row` are 1-based cell coordinates, as on the wire. The modifier
/// flags and the motion flag are the `Cb` bits xterm sets; the [`pressed`]
/// flag is the final byte (`M` = press, `m` = release).
///
/// [`pressed`]: MouseReport::pressed
//
// The flags are independent `Cb` bits, not a state machine — any combination
// is legal (ctrl+drag, shift+release, …) — so a flat record models them more
// clearly than an enum, exactly as `Attributes` documents for its rendition
// flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MouseReport {
    /// The button (or wheel direction) the report names.
    pub button: MouseButton,
    /// 1-based column of the pointer.
    pub col: u16,
    /// 1-based row of the pointer.
    pub row: u16,
    /// `true` for a press report (`M`), `false` for a release (`m`).
    pub pressed: bool,
    /// Whether this is a motion (drag / move) report.
    pub motion: bool,
    /// Whether Shift was held.
    pub shift: bool,
    /// Whether Meta / Alt was held.
    pub meta: bool,
    /// Whether Control was held.
    pub ctrl: bool,
}

impl MouseReport {
    /// The `Cb` byte that encodes this report's button, modifiers, and motion.
    ///
    /// This is the inverse of the button/flag decoding in [`MouseReport::decode`]
    /// over the same bit layout, so a report round-trips through the emitter and
    /// parser unchanged.
    #[must_use]
    pub const fn encode_button(&self) -> u16 {
        let mut cb = self.button.code();
        if self.shift {
            cb |= SHIFT_BIT;
        }
        if self.meta {
            cb |= META_BIT;
        }
        if self.ctrl {
            cb |= CTRL_BIT;
        }
        if self.motion {
            cb |= MOTION_BIT;
        }
        cb
    }

    /// Build a [`MouseReport`] from the parsed `Cb`, column, row, and the
    /// press/release flag.
    #[must_use]
    pub const fn decode(cb: u16, col: u16, row: u16, pressed: bool) -> MouseReport {
        MouseReport {
            button: MouseButton::from_code(cb & BUTTON_MASK),
            col,
            row,
            pressed,
            motion: cb & MOTION_BIT != 0,
            shift: cb & SHIFT_BIT != 0,
            meta: cb & META_BIT != 0,
            ctrl: cb & CTRL_BIT != 0,
        }
    }
}
