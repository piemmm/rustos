//! Select Graphic Rendition (SGR): the text attributes and their colours, plus
//! the rendered character-cell attribute set.
//!
//! [`Sgr`] is the typed vocabulary of individual SGR operations. The numeric
//! parameter encoding lives here, in [`Sgr::write_params`] and
//! [`decode_params`], so the emitter and the parser share **one** SGR table: every [`Sgr`] the emitter writes decodes back to the
//! identical [`Sgr`].
//!
//! [`Attributes`] is the folded result — the rendition state a cell is drawn
//! with. It is the shared attribute representation reused by both the consumer
//! (the terminal `Grid`) and the emitter (the curses renderer).

use alloc::vec::Vec;

use crate::color::{BasicColor, Color};

/// One Select Graphic Rendition operation.
///
/// Each variant has exactly one canonical parameter encoding (see
/// [`Sgr::write_params`]), so it survives an emit→parse round-trip unchanged.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Sgr {
    /// `0` — reset every attribute to the default.
    Reset,
    /// `1` — bold / increased intensity.
    Bold,
    /// `2` — dim / decreased intensity.
    Dim,
    /// `3` — italic.
    Italic,
    /// `4` — underline.
    Underline,
    /// `5` — blink.
    Blink,
    /// `7` — reverse video (swap foreground and background).
    Reverse,
    /// `9` — crossed-out / strikethrough.
    Strike,
    /// `22` — normal intensity (clears both [`Sgr::Bold`] and [`Sgr::Dim`]).
    ResetIntensity,
    /// `23` — clears [`Sgr::Italic`].
    ResetItalic,
    /// `24` — clears [`Sgr::Underline`].
    ResetUnderline,
    /// `25` — clears [`Sgr::Blink`].
    ResetBlink,
    /// `27` — clears [`Sgr::Reverse`].
    ResetReverse,
    /// `29` — clears [`Sgr::Strike`].
    ResetStrike,
    /// Set the foreground colour (`30..=37` / `90..=97`, `38;5;n`, `38;2;r;g;b`,
    /// or `39` for the default).
    Foreground(Color),
    /// Set the background colour (`40..=47` / `100..=107`, `48;5;n`,
    /// `48;2;r;g;b`, or `49` for the default).
    Background(Color),
}

/// Extended-colour selector for `38` (foreground) and `48` (background).
const EXTENDED_FOREGROUND: u16 = 38;
const EXTENDED_BACKGROUND: u16 = 48;
/// Sub-selector after `38`/`48`: `5` = 256-colour index, `2` = truecolour.
const EXTENDED_INDEXED: u16 = 5;
const EXTENDED_RGB: u16 = 2;

impl Sgr {
    /// Append this operation's canonical numeric parameters to `out`.
    ///
    /// The emitter joins these with `;` and wraps them in `CSI … m`. The
    /// encoding is the single SGR table shared with [`decode_params`].
    pub fn write_params(&self, out: &mut Vec<u16>) {
        match self {
            Sgr::Reset => out.push(0),
            Sgr::Bold => out.push(1),
            Sgr::Dim => out.push(2),
            Sgr::Italic => out.push(3),
            Sgr::Underline => out.push(4),
            Sgr::Blink => out.push(5),
            Sgr::Reverse => out.push(7),
            Sgr::Strike => out.push(9),
            Sgr::ResetIntensity => out.push(22),
            Sgr::ResetItalic => out.push(23),
            Sgr::ResetUnderline => out.push(24),
            Sgr::ResetBlink => out.push(25),
            Sgr::ResetReverse => out.push(27),
            Sgr::ResetStrike => out.push(29),
            Sgr::Foreground(color) => write_color_params(out, *color, false),
            Sgr::Background(color) => write_color_params(out, *color, true),
        }
    }
}

/// Append the parameters for a foreground (`background == false`) or background
/// (`background == true`) colour.
fn write_color_params(out: &mut Vec<u16>, color: Color, background: bool) {
    match color {
        Color::Default => out.push(if background { 49 } else { 39 }),
        Color::Basic(basic) => out.push(basic_code(basic, background)),
        Color::Indexed(index) => {
            out.push(if background {
                EXTENDED_BACKGROUND
            } else {
                EXTENDED_FOREGROUND
            });
            out.push(EXTENDED_INDEXED);
            out.push(u16::from(index));
        }
        Color::Rgb(r, g, b) => {
            out.push(if background {
                EXTENDED_BACKGROUND
            } else {
                EXTENDED_FOREGROUND
            });
            out.push(EXTENDED_RGB);
            out.push(u16::from(r));
            out.push(u16::from(g));
            out.push(u16::from(b));
        }
    }
}

/// The `30..=37` / `90..=97` (foreground) or `40..=47` / `100..=107`
/// (background) code for a [`BasicColor`].
fn basic_code(basic: BasicColor, background: bool) -> u16 {
    let index = basic.index();
    let (base, bright_base) = if background { (40, 100) } else { (30, 90) };
    if basic.is_bright() {
        u16::from(bright_base + (index - 8))
    } else {
        u16::from(base + index)
    }
}

/// Decode a complete SGR parameter list into the sequence of [`Sgr`]
/// operations it represents, invoking `sink` once per recognised operation in
/// order.
///
/// An empty list means `CSI m`, which is `CSI 0 m` — a reset. Unrecognised
/// codes and malformed extended-colour runs are skipped (fail closed) rather than producing a bogus operation. This is the
/// inverse of [`Sgr::write_params`] over the same table.
pub fn decode_params(params: &[u16], mut sink: impl FnMut(Sgr)) {
    if params.is_empty() {
        sink(Sgr::Reset);
        return;
    }
    let mut i = 0;
    while i < params.len() {
        let code = params[i];
        match code {
            0 => sink(Sgr::Reset),
            1 => sink(Sgr::Bold),
            2 => sink(Sgr::Dim),
            3 => sink(Sgr::Italic),
            4 => sink(Sgr::Underline),
            5 => sink(Sgr::Blink),
            7 => sink(Sgr::Reverse),
            9 => sink(Sgr::Strike),
            22 => sink(Sgr::ResetIntensity),
            23 => sink(Sgr::ResetItalic),
            24 => sink(Sgr::ResetUnderline),
            25 => sink(Sgr::ResetBlink),
            27 => sink(Sgr::ResetReverse),
            29 => sink(Sgr::ResetStrike),
            30..=37 => emit_basic(&mut sink, code - 30, false),
            40..=47 => emit_basic(&mut sink, code - 40, true),
            90..=97 => emit_basic(&mut sink, code - 90 + 8, false),
            100..=107 => emit_basic(&mut sink, code - 100 + 8, true),
            39 => sink(Sgr::Foreground(Color::Default)),
            49 => sink(Sgr::Background(Color::Default)),
            EXTENDED_FOREGROUND | EXTENDED_BACKGROUND => {
                let background = code == EXTENDED_BACKGROUND;
                if let Some((color, consumed)) = decode_extended_color(&params[i + 1..]) {
                    if background {
                        sink(Sgr::Background(color));
                    } else {
                        sink(Sgr::Foreground(color));
                    }
                    i += consumed;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Emit a [`BasicColor`] operation for palette `index`, if it is in range.
fn emit_basic(sink: &mut impl FnMut(Sgr), index: u16, background: bool) {
    let Ok(index) = u8::try_from(index) else {
        return;
    };
    if let Some(basic) = BasicColor::from_index(index) {
        let color = Color::Basic(basic);
        sink(if background {
            Sgr::Background(color)
        } else {
            Sgr::Foreground(color)
        });
    }
}

/// Decode the parameters following a `38`/`48` selector. Returns the colour and
/// the number of extra parameters consumed, or `None` if the run is malformed
/// or out of range (fail closed).
fn decode_extended_color(rest: &[u16]) -> Option<(Color, usize)> {
    match rest.first().copied() {
        Some(EXTENDED_INDEXED) => {
            let index = u8::try_from(*rest.get(1)?).ok()?;
            Some((Color::Indexed(index), 2))
        }
        Some(EXTENDED_RGB) => {
            let r = u8::try_from(*rest.get(1)?).ok()?;
            let g = u8::try_from(*rest.get(2)?).ok()?;
            let b = u8::try_from(*rest.get(3)?).ok()?;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

/// The rendition state a character cell is drawn with: the boolean attribute
/// flags plus the foreground and background colours.
///
/// This is the shared attribute representation — the terminal `Grid` stores it
/// per cell, and the curses renderer folds [`Sgr`] operations into it. Folding
/// is [`Attributes::apply`]; [`Attributes::default`] is the unstyled cell.
//
// The seven booleans are independent SGR rendition states, not a state
// machine: any combination is legal (bold+underline+reverse, none, …), so a
// flat record models them more clearly than an enum, exactly as
// `rustos_abi::input::Modifiers` models its independent modifier keys.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Attributes {
    /// Bold / increased intensity.
    pub bold: bool,
    /// Dim / decreased intensity.
    pub dim: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
    /// Blink.
    pub blink: bool,
    /// Reverse video.
    pub reverse: bool,
    /// Crossed-out / strikethrough.
    pub strike: bool,
    /// Foreground colour.
    pub foreground: Color,
    /// Background colour.
    pub background: Color,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            blink: false,
            reverse: false,
            strike: false,
            foreground: Color::Default,
            background: Color::Default,
        }
    }
}

impl Attributes {
    /// The unstyled attribute set: no flags, default colours.
    pub const PLAIN: Self = Self {
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        blink: false,
        reverse: false,
        strike: false,
        foreground: Color::Default,
        background: Color::Default,
    };

    /// Fold one [`Sgr`] operation into this attribute set.
    ///
    /// [`Sgr::Reset`] returns to [`Attributes::PLAIN`]; the flag setters and
    /// their `Reset*` counterparts toggle the matching field; the colour
    /// operations replace the matching colour.
    pub fn apply(&mut self, sgr: Sgr) {
        match sgr {
            Sgr::Reset => *self = Self::PLAIN,
            Sgr::Bold => self.bold = true,
            Sgr::Dim => self.dim = true,
            Sgr::Italic => self.italic = true,
            Sgr::Underline => self.underline = true,
            Sgr::Blink => self.blink = true,
            Sgr::Reverse => self.reverse = true,
            Sgr::Strike => self.strike = true,
            Sgr::ResetIntensity => {
                self.bold = false;
                self.dim = false;
            }
            Sgr::ResetItalic => self.italic = false,
            Sgr::ResetUnderline => self.underline = false,
            Sgr::ResetBlink => self.blink = false,
            Sgr::ResetReverse => self.reverse = false,
            Sgr::ResetStrike => self.strike = false,
            Sgr::Foreground(color) => self.foreground = color,
            Sgr::Background(color) => self.background = color,
        }
    }
}
