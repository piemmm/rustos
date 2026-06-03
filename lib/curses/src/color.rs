//! Colour-pair allocation and capability-driven colour downgrade.
//!
//! Two concerns live here, both about colour the *application* asked for versus
//! colour the *terminal* can show:
//!
//! * [`ColorPairs`] is the curses colour-pair table — a foreground/background
//!   pair addressed by a small id, with id `0` reserved for the terminal
//!   default. An application allocates a pair once and then draws with its id.
//! * [`downgrade`] maps any [`Color`] to the nearest colour a terminal of a
//!   given [`ColorDepth`] can render, so the [minimal-diff renderer] never
//!   emits an SGR colour the terminal would misinterpret (`plans/CURSES.md`
//!   §C4). Truecolour degrades to the 256-colour palette, then to the 16 ANSI
//!   colours, then to monochrome, by capability.
//!
//! [minimal-diff renderer]: mod@crate::render

use alloc::vec;
use alloc::vec::Vec;

use rustos_termcap::ColorDepth;
use rustos_vt::{BasicColor, Color};

use crate::error::{CursesError, Result};

/// The reserved colour-pair id for the terminal's default foreground on its
/// default background.
pub const DEFAULT_PAIR: u16 = 0;

/// The largest colour-pair id the table holds (ids `1..=MAX_COLOR_PAIRS` are
/// allocatable; `0` is the reserved default).
pub const MAX_COLOR_PAIRS: u16 = 256;

/// A foreground / background colour pair.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ColorPair {
    /// The foreground colour.
    pub fg: Color,
    /// The background colour.
    pub bg: Color,
}

impl ColorPair {
    /// The default pair: the terminal's configured default colours.
    pub const DEFAULT: ColorPair = ColorPair {
        fg: Color::Default,
        bg: Color::Default,
    };

    /// A pair of `fg` on `bg`.
    #[must_use]
    pub const fn new(fg: Color, bg: Color) -> ColorPair {
        ColorPair { fg, bg }
    }
}

/// The curses colour-pair table.
///
/// Pair `0` ([`DEFAULT_PAIR`]) is fixed to the terminal default and cannot be
/// redefined. Pairs `1..=MAX_COLOR_PAIRS` are defined explicitly with
/// [`ColorPairs::init_pair`], handed out automatically with
/// [`ColorPairs::alloc_pair`], and looked up with [`ColorPairs::get`]. A slot
/// that has never been defined is [`None`], which lets [`alloc_pair`] find
/// the next free id.
///
/// [`alloc_pair`]: ColorPairs::alloc_pair
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColorPairs {
    pairs: Vec<Option<ColorPair>>,
}

impl Default for ColorPairs {
    fn default() -> Self {
        Self::new()
    }
}

impl ColorPairs {
    /// A fresh table holding only the reserved default pair.
    #[must_use]
    pub fn new() -> ColorPairs {
        ColorPairs {
            pairs: vec![Some(ColorPair::DEFAULT)],
        }
    }

    /// Define pair `id` as `fg` on `bg`.
    ///
    /// # Errors
    ///
    /// Returns [`CursesError::BadColorPair`] if `id` is `0` (the reserved
    /// default) or greater than [`MAX_COLOR_PAIRS`].
    pub fn init_pair(&mut self, id: u16, fg: Color, bg: Color) -> Result<()> {
        if id == DEFAULT_PAIR || id > MAX_COLOR_PAIRS {
            return Err(CursesError::BadColorPair);
        }
        let index = usize::from(id);
        if index >= self.pairs.len() {
            self.pairs.resize(index + 1, None);
        }
        self.pairs[index] = Some(ColorPair::new(fg, bg));
        Ok(())
    }

    /// Define the next free pair as `fg` on `bg` and return its id (curses
    /// `alloc_pair`).
    ///
    /// The lowest id in `1..=MAX_COLOR_PAIRS` that has not been defined is
    /// chosen, so an application can request pairs without tracking ids
    /// itself.
    ///
    /// # Errors
    ///
    /// Returns [`CursesError::BadColorPair`] if every allocatable id is
    /// already defined.
    pub fn alloc_pair(&mut self, fg: Color, bg: Color) -> Result<u16> {
        for id in 1..=MAX_COLOR_PAIRS {
            let index = usize::from(id);
            if index >= self.pairs.len() || self.pairs[index].is_none() {
                self.init_pair(id, fg, bg)?;
                return Ok(id);
            }
        }
        Err(CursesError::BadColorPair)
    }

    /// The pair defined for `id`.
    ///
    /// An id that was never defined (or is out of range) resolves to the
    /// default pair, so a lookup is always total and never panics.
    #[must_use]
    pub fn get(&self, id: u16) -> ColorPair {
        self.pairs
            .get(usize::from(id))
            .copied()
            .flatten()
            .unwrap_or(ColorPair::DEFAULT)
    }
}

/// Map `color` to the nearest colour a terminal of `depth` can render.
///
/// A colour the depth already supports is returned unchanged; otherwise it is
/// reduced one step at a time — truecolour to a 256-palette index, a
/// 256-palette index to one of the 16 ANSI colours, and any colour to
/// [`Color::Default`] on a monochrome terminal.
#[must_use]
pub fn downgrade(color: Color, depth: ColorDepth) -> Color {
    if depth.supports(color) {
        return color;
    }
    match depth {
        ColorDepth::None => Color::Default,
        ColorDepth::Ansi16 => Color::Basic(to_ansi16(color)),
        ColorDepth::Indexed256 => Color::Indexed(to_indexed256(color)),
        // `TrueColor` supports every model, so the early return above already
        // handled it; reaching here is impossible but must not panic.
        ColorDepth::TrueColor => color,
    }
}

/// The 256-palette index nearest to `color` (used when degrading truecolour to
/// an [`ColorDepth::Indexed256`] terminal).
fn to_indexed256(color: Color) -> u8 {
    match color {
        Color::Rgb(r, g, b) => rgb_to_indexed256(r, g, b),
        // The shallower models are already representable on a 256-colour
        // terminal, so `downgrade` never routes them here; map conservatively.
        Color::Indexed(index) => index,
        Color::Basic(basic) => basic.index(),
        Color::Default => 0,
    }
}

/// The [`BasicColor`] nearest to `color` (used when degrading to an
/// [`ColorDepth::Ansi16`] terminal).
fn to_ansi16(color: Color) -> BasicColor {
    match color {
        Color::Basic(basic) => basic,
        Color::Rgb(r, g, b) => rgb_to_ansi16(r, g, b),
        Color::Indexed(index) => {
            let (r, g, b) = indexed256_to_rgb(index);
            rgb_to_ansi16(r, g, b)
        }
        // `Ansi16` supports `Default`, so this is unreachable through
        // `downgrade`; black is the safe neutral.
        Color::Default => BasicColor::Black,
    }
}

/// The six xterm colour-cube channel levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Quantise a channel to the 6-level cube coordinate xterm uses (`0..=5`).
fn channel_to_cube(value: u8) -> u8 {
    let mut best = 0u8;
    let mut best_dist = u16::MAX;
    for (level, &reference) in CUBE_LEVELS.iter().enumerate() {
        let dist = u16::from(value).abs_diff(u16::from(reference));
        if dist < best_dist {
            best_dist = dist;
            best = u8::try_from(level).unwrap_or(5);
        }
    }
    best
}

/// The 256-palette index nearest to an RGB triple: either a colour-cube entry
/// or, when the triple is close to neutral grey, a greyscale-ramp entry.
fn rgb_to_indexed256(r: u8, g: u8, b: u8) -> u8 {
    let cr = channel_to_cube(r);
    let cg = channel_to_cube(g);
    let cb = channel_to_cube(b);
    let cube_index = 16 + 36 * cr + 6 * cg + cb;
    let (cube_r, cube_g, cube_b) = indexed256_to_rgb(cube_index);
    let cube_dist = rgb_distance(r, g, b, cube_r, cube_g, cube_b);

    let grey_level = grey_ramp_level(r, g, b);
    let grey_index = 232 + grey_level;
    let (grey_r, grey_g, grey_b) = indexed256_to_rgb(grey_index);
    let grey_dist = rgb_distance(r, g, b, grey_r, grey_g, grey_b);

    if grey_dist < cube_dist {
        grey_index
    } else {
        cube_index
    }
}

/// The greyscale-ramp step (`0..=23`) nearest to an RGB triple's average.
fn grey_ramp_level(r: u8, g: u8, b: u8) -> u8 {
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    // The ramp runs 8, 18, …, 238 (step 10); choose the nearest step.
    let step = (average.saturating_sub(8) + 5) / 10;
    u8::try_from(step.min(23)).unwrap_or(23)
}

/// The RGB triple a 256-palette index displays as (the standard xterm palette:
/// 16 system colours, a 6×6×6 cube, and a 24-step greyscale ramp).
fn indexed256_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => {
            let (r, g, b) = system_color_rgb(index);
            (r, g, b)
        }
        16..=231 => {
            let i = index - 16;
            let levels = [0u8, 95, 135, 175, 215, 255];
            let r = levels[usize::from(i / 36)];
            let g = levels[usize::from((i / 6) % 6)];
            let b = levels[usize::from(i % 6)];
            (r, g, b)
        }
        232..=255 => {
            let level = 8u16 + u16::from(index - 232) * 10;
            let v = u8::try_from(level.min(255)).unwrap_or(255);
            (v, v, v)
        }
    }
}

/// The approximate RGB of the 16 ANSI system colours (`0..=15`).
fn system_color_rgb(index: u8) -> (u8, u8, u8) {
    const PALETTE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (170, 0, 0),
        (0, 170, 0),
        (170, 85, 0),
        (0, 0, 170),
        (170, 0, 170),
        (0, 170, 170),
        (170, 170, 170),
        (85, 85, 85),
        (255, 85, 85),
        (85, 255, 85),
        (255, 255, 85),
        (85, 85, 255),
        (255, 85, 255),
        (85, 255, 255),
        (255, 255, 255),
    ];
    PALETTE[usize::from(index & 0x0f)]
}

/// The [`BasicColor`] whose system-palette RGB is nearest to a triple.
fn rgb_to_ansi16(r: u8, g: u8, b: u8) -> BasicColor {
    let mut best = BasicColor::Black;
    let mut best_dist = u32::MAX;
    for basic in BasicColor::ALL {
        let (pr, pg, pb) = system_color_rgb(basic.index());
        let dist = rgb_distance(r, g, b, pr, pg, pb);
        if dist < best_dist {
            best_dist = dist;
            best = basic;
        }
    }
    best
}

/// Squared Euclidean distance between two RGB triples.
fn rgb_distance(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let dr = u32::from(r1.abs_diff(r2));
    let dg = u32::from(g1.abs_diff(g2));
    let db = u32::from(b1.abs_diff(b2));
    dr * dr + dg * dg + db * db
}
