//! [`Capabilities`] — what one terminal can do, expressed in `lib/vt` terms.
//!
//! A capability record names, for one [`TermType`], the depth
//! of colour it renders, the screen-control operations it accepts, the keys it
//! sends, and the optional features (mouse reporting, bracketed paste) it
//! supports. Every escape sequence it references is a [`Op`] from the one
//! shared vocabulary — this module defines no second
//! escape-sequence table. [`Capabilities::referenced_ops`] returns that exact
//! set so the database can be checked against `lib/vt`.

use alloc::vec;
use alloc::vec::Vec;

use rustos_vt::{BasicColor, Color, EraseMode, Op, Sgr, Title};

use crate::term_type::TermType;

/// How much colour a terminal renders.
///
/// The depth bounds which [`Color`] models the terminal can display:
/// [`ColorDepth::supports`] is the single check, so a renderer degrades a
/// colour the terminal cannot show rather than emitting a sequence it would
/// misinterpret.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ColorDepth {
    /// Monochrome — only [`Color::Default`] is meaningful.
    None,
    /// The 16 ANSI colours ([`Color::Basic`]).
    Ansi16,
    /// The 256-colour palette ([`Color::Indexed`]) and the 16 ANSI colours.
    Indexed256,
    /// 24-bit truecolour ([`Color::Rgb`]) and every shallower model.
    TrueColor,
}

impl ColorDepth {
    /// Whether a terminal at this depth can render `color`.
    ///
    /// [`Color::Default`] is always renderable; the explicit models require a
    /// depth that reaches them. A renderer uses this to degrade an
    /// out-of-range colour (`plans/CURSES.md` §C4) instead of emitting a
    /// sequence the terminal cannot honour.
    #[must_use]
    pub const fn supports(self, color: Color) -> bool {
        match color {
            Color::Default => true,
            Color::Basic(_) => !matches!(self, ColorDepth::None),
            Color::Indexed(_) => matches!(self, ColorDepth::Indexed256 | ColorDepth::TrueColor),
            Color::Rgb(_, _, _) => matches!(self, ColorDepth::TrueColor),
        }
    }

    /// One representative [`Color`] of each model this depth can render.
    ///
    /// Used by [`Capabilities::referenced_ops`] so the database's colour claims
    /// are checked against `lib/vt`'s SGR encoding.
    #[must_use]
    fn sample_colors(self) -> Vec<Color> {
        if matches!(self, ColorDepth::None) {
            // A monochrome terminal renders no SGR colour at all, not even an
            // explicit "default" — it would not process the sequence.
            return Vec::new();
        }
        let mut colors = vec![Color::Default];
        for basic in BasicColor::ALL {
            colors.push(Color::Basic(basic));
        }
        if matches!(self, ColorDepth::Indexed256 | ColorDepth::TrueColor) {
            colors.push(Color::Indexed(0));
            colors.push(Color::Indexed(128));
            colors.push(Color::Indexed(255));
        }
        if matches!(self, ColorDepth::TrueColor) {
            colors.push(Color::Rgb(0x30, 0x70, 0xf0));
        }
        colors
    }
}

/// The mouse-tracking protocol a terminal reports with.
///
/// This is a capability *fact*, not an escape sequence: the bytes that enable
/// a tracking mode and the bytes a click report arrives in are added to
/// `lib/vt`'s vocabulary when the curses input decoder needs to emit and parse
/// them (`plans/CURSES.md` §C4). The record states only what the terminal can
/// do, so it never duplicates a sequence definition.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MouseReporting {
    /// The terminal does not report the mouse.
    None,
    /// Button press, release, and drag are reported (xterm `1000`/`1002`
    /// class).
    ButtonEvent,
    /// Every pointer movement is reported, not just drags (xterm `1003`
    /// class).
    AnyEvent,
}

/// A terminal's mouse-reporting support.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MouseSupport {
    /// The richest tracking protocol the terminal reports with.
    pub reporting: MouseReporting,
    /// Whether the terminal encodes reports in the SGR extended form (xterm
    /// `1006`), which lifts the 223-column limit of the legacy encoding.
    pub sgr_extended: bool,
}

impl MouseSupport {
    /// No mouse reporting.
    pub const NONE: Self = Self {
        reporting: MouseReporting::None,
        sgr_extended: false,
    };

    /// Button/drag tracking with the SGR extended encoding.
    pub const BUTTON: Self = Self {
        reporting: MouseReporting::ButtonEvent,
        sgr_extended: true,
    };

    /// Full motion tracking with the SGR extended encoding.
    pub const ANY: Self = Self {
        reporting: MouseReporting::AnyEvent,
        sgr_extended: true,
    };

    /// Whether the terminal reports the mouse at all.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        !matches!(self.reporting, MouseReporting::None)
    }
}

/// The [`Op`] each arrow key sends, in the terminal's normal (non-application)
/// cursor mode.
///
/// In normal cursor mode an arrow key sends the same `CSI` sequence the
/// emitter writes for the matching cursor movement, so the input is expressed
/// as the [`Op`] the bytes parse back to through [`rustos_vt::Parser`] — one
/// vocabulary for output and input alike.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArrowKeys {
    /// Up arrow (`CSI A` → [`Op::CursorUp`]).
    pub up: Op,
    /// Down arrow (`CSI B` → [`Op::CursorDown`]).
    pub down: Op,
    /// Right arrow (`CSI C` → [`Op::CursorForward`]).
    pub right: Op,
    /// Left arrow (`CSI D` → [`Op::CursorBack`]).
    pub left: Op,
}

impl ArrowKeys {
    /// The standard ANSI arrow-key sequences.
    #[must_use]
    pub const fn ansi() -> Self {
        Self {
            up: Op::CursorUp(1),
            down: Op::CursorDown(1),
            right: Op::CursorForward(1),
            left: Op::CursorBack(1),
        }
    }
}

/// The keys a terminal sends.
///
/// Arrow-key *sequences* are expressed as `lib/vt` [`Op`]s ([`ArrowKeys`]). The
/// function, editing (Home/End/Insert/Delete/PgUp/PgDn), and keypad keys are
/// recorded as capability facts: their `SS3` / `CSI … ~` byte sequences are not
/// yet in `lib/vt`'s vocabulary and are added there — not invented here — when
/// the curses input decoder needs them (`plans/CURSES.md` §C4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyInput {
    /// The arrow keys, if the terminal sends them.
    pub arrows: Option<ArrowKeys>,
    /// Whether the terminal sends function keys (`F1`…).
    pub function_keys: bool,
    /// Whether the terminal sends the editing keys (Home/End/Insert/Delete/
    /// PageUp/PageDown).
    pub editing_keys: bool,
    /// Whether the terminal has an application keypad.
    pub keypad: bool,
}

/// What one terminal can do, the compiled-in capability record.
///
/// Build it from a [`TermType`] with [`Capabilities::for_term`] (or
/// [`TermType::capabilities`](crate::TermType::capabilities)). Every escape
/// sequence it claims is a `lib/vt` [`Op`]; [`Capabilities::referenced_ops`]
/// enumerates them for checking against the shared vocabulary.
//
// The booleans are independent capability facts, not a state machine: a
// terminal supports any combination of them, so a flat record models them more
// clearly than an enum (the same rationale `rustos_vt::Attributes` documents
// for its independent rendition flags).
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// The terminal this record describes.
    pub term_type: TermType,
    /// The colour depth the terminal renders.
    pub color: ColorDepth,
    /// Whether the terminal can address the cursor (relative moves and
    /// absolute positioning).
    pub cursor_addressing: bool,
    /// Whether the terminal supports erase-in-line and erase-in-display.
    pub erase: bool,
    /// Whether the terminal supports a settable scroll region and scrolling.
    pub scroll_region: bool,
    /// Whether the terminal has an alternate screen buffer.
    pub alt_screen: bool,
    /// Whether the terminal can show and hide the cursor.
    pub cursor_visibility: bool,
    /// Whether the terminal can set its window title.
    pub set_title: bool,
    /// The terminal's mouse-reporting support.
    pub mouse: MouseSupport,
    /// Whether the terminal supports bracketed paste.
    pub bracketed_paste: bool,
    /// The keys the terminal sends.
    pub keys: KeyInput,
}

impl Capabilities {
    /// The compiled-in capability record for `term`.
    #[must_use]
    pub const fn for_term(term: TermType) -> Capabilities {
        match term {
            TermType::Dumb => DUMB,
            TermType::Vt100 => VT100,
            TermType::Vt220 => VT220,
            TermType::Xterm | TermType::XtermColor | TermType::Xterm16Color => {
                xterm_family(term, ColorDepth::Ansi16, MouseSupport::NONE, false)
            }
            TermType::Xterm256Color => {
                xterm_family(term, ColorDepth::Indexed256, MouseSupport::BUTTON, true)
            }
            TermType::Alacritty | TermType::XtermKitty => {
                xterm_family(term, ColorDepth::TrueColor, MouseSupport::ANY, true)
            }
        }
    }

    /// Every `lib/vt` [`Op`] this record references.
    ///
    /// The set is the escape sequences the terminal accepts (the output
    /// capabilities, one representative `Op` per claimed feature, including a
    /// colour per renderable model) plus the arrow-key input sequences. Because
    /// each is a `lib/vt` `Op`, encoding and re-parsing every one of them
    /// reproduces it unchanged — the database emits nothing `lib/vt` does not
    /// define (the `no_record_emits_a_sequence_absent_from_vt`
    /// test asserts exactly this).
    #[must_use]
    pub fn referenced_ops(&self) -> Vec<Op> {
        let mut ops = Vec::new();
        if self.cursor_addressing {
            ops.push(Op::CursorPosition { row: 1, col: 1 });
            ops.push(Op::CursorUp(1));
            ops.push(Op::CursorDown(1));
            ops.push(Op::CursorForward(1));
            ops.push(Op::CursorBack(1));
            ops.push(Op::CursorColumn(1));
            ops.push(Op::CursorNextLine(1));
            ops.push(Op::CursorPrevLine(1));
        }
        if self.erase {
            for mode in [EraseMode::ToEnd, EraseMode::ToStart, EraseMode::All] {
                ops.push(Op::EraseInDisplay(mode));
                ops.push(Op::EraseInLine(mode));
            }
        }
        if self.scroll_region {
            ops.push(Op::SetScrollRegion { top: 1, bottom: 24 });
            ops.push(Op::ResetScrollRegion);
            ops.push(Op::ScrollUp(1));
            ops.push(Op::ScrollDown(1));
        }
        if self.alt_screen {
            ops.push(Op::EnterAltScreen);
            ops.push(Op::LeaveAltScreen);
        }
        if self.cursor_visibility {
            ops.push(Op::ShowCursor);
            ops.push(Op::HideCursor);
        }
        if self.set_title {
            ops.push(Op::SetTitle(Title::from_text("rustos")));
        }
        for color in self.color.sample_colors() {
            ops.push(Op::Sgr(Sgr::Foreground(color)));
            ops.push(Op::Sgr(Sgr::Background(color)));
        }
        if let Some(arrows) = &self.keys.arrows {
            ops.push(arrows.up.clone());
            ops.push(arrows.down.clone());
            ops.push(arrows.right.clone());
            ops.push(arrows.left.clone());
        }
        ops
    }
}

/// The `dumb` terminal: no addressing, no colour, no keys — the safe baseline.
const DUMB: Capabilities = Capabilities {
    term_type: TermType::Dumb,
    color: ColorDepth::None,
    cursor_addressing: false,
    erase: false,
    scroll_region: false,
    alt_screen: false,
    cursor_visibility: false,
    set_title: false,
    mouse: MouseSupport::NONE,
    bracketed_paste: false,
    keys: KeyInput {
        arrows: None,
        function_keys: false,
        editing_keys: false,
        keypad: false,
    },
};

/// The DEC VT100: cursor-addressable and scrollable, monochrome, with arrow
/// keys and a keypad but no alternate screen, mouse, or title.
const VT100: Capabilities = Capabilities {
    term_type: TermType::Vt100,
    color: ColorDepth::None,
    cursor_addressing: true,
    erase: true,
    scroll_region: true,
    alt_screen: false,
    cursor_visibility: false,
    set_title: false,
    mouse: MouseSupport::NONE,
    bracketed_paste: false,
    keys: KeyInput {
        arrows: Some(ArrowKeys::ansi()),
        function_keys: false,
        editing_keys: false,
        keypad: true,
    },
};

/// The DEC VT220: the VT100 baseline plus editing and function keys.
const VT220: Capabilities = Capabilities {
    term_type: TermType::Vt220,
    color: ColorDepth::None,
    cursor_addressing: true,
    erase: true,
    scroll_region: true,
    alt_screen: false,
    cursor_visibility: true,
    set_title: false,
    mouse: MouseSupport::NONE,
    bracketed_paste: false,
    keys: KeyInput {
        arrows: Some(ArrowKeys::ansi()),
        function_keys: true,
        editing_keys: true,
        keypad: true,
    },
};

/// The shared profile of the xterm family, parameterised by the features that
/// differ across the recognised xterm `TERM` values.
const fn xterm_family(
    term: TermType,
    color: ColorDepth,
    mouse: MouseSupport,
    bracketed_paste: bool,
) -> Capabilities {
    Capabilities {
        term_type: term,
        color,
        cursor_addressing: true,
        erase: true,
        scroll_region: true,
        alt_screen: true,
        cursor_visibility: true,
        set_title: true,
        mouse,
        bracketed_paste,
        keys: KeyInput {
            arrows: Some(ArrowKeys::ansi()),
            function_keys: true,
            editing_keys: true,
            keypad: true,
        },
    }
}
