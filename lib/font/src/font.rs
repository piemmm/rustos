//! A bitmap font and the glyph blitter that draws it onto a [`Surface`].
//!
//! A [`BitmapFont`] couples a glyph atlas with its metrics (cell size, pen
//! advance, line height) and a fallback glyph for characters the atlas does
//! not cover. [`BitmapFont::draw_text`] composites each glyph onto a
//! `lib/raster` [`Surface`] through that crate's single premultiplied-alpha
//! [`Pixel::over`] path, so text blends over whatever is already painted with
//! no colour arithmetic duplicated here (`AGENTS.md` §2.2).

use rustos_raster::{Color, Pixel, Surface};

use crate::glyphs::{Glyph, FIRST_CHAR, GLYPHS, GLYPH_HEIGHT, GLYPH_WIDTH, LAST_CHAR};

/// The fallback glyph for a character outside the atlas: a hollow box (the
/// conventional "missing glyph" tofu) so an unsupported character is visibly
/// wrong rather than silently dropped (`AGENTS.md` §2.9).
const FALLBACK: Glyph = [
    0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
];

/// A monospace bitmap font: a glyph atlas plus its layout metrics.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BitmapFont {
    glyphs: &'static [Glyph],
    first: char,
    last: char,
    glyph_width: u32,
    glyph_height: u32,
    advance: u32,
    line_height: u32,
}

impl BitmapFont {
    /// The built-in 5×7 monospace face covering printable ASCII.
    ///
    /// Glyphs advance by `glyph_width + 1` pixels and lines by
    /// `glyph_height + 1`, leaving one pixel of inter-glyph and inter-line
    /// gap so adjacent characters do not touch.
    #[must_use]
    pub const fn mono5x7() -> Self {
        Self {
            glyphs: &GLYPHS,
            first: FIRST_CHAR,
            last: LAST_CHAR,
            glyph_width: GLYPH_WIDTH,
            glyph_height: GLYPH_HEIGHT,
            advance: GLYPH_WIDTH + 1,
            line_height: GLYPH_HEIGHT + 1,
        }
    }

    /// The glyph cell width in pixels.
    #[must_use]
    pub const fn glyph_width(&self) -> u32 {
        self.glyph_width
    }

    /// The glyph cell height in pixels.
    #[must_use]
    pub const fn glyph_height(&self) -> u32 {
        self.glyph_height
    }

    /// The horizontal pen advance per character in pixels.
    #[must_use]
    pub const fn advance(&self) -> u32 {
        self.advance
    }

    /// The vertical distance between baselines in pixels.
    #[must_use]
    pub const fn line_height(&self) -> u32 {
        self.line_height
    }

    /// The pixel width of `text` rendered on one line: the tight bounding
    /// width with no trailing inter-glyph gap. An empty string is zero wide.
    ///
    /// Arithmetic saturates, so a pathologically long string reports
    /// [`u32::MAX`] rather than wrapping (`AGENTS.md` §2.9).
    #[must_use]
    pub fn text_width(&self, text: &str) -> u32 {
        let count = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
        match count {
            0 => 0,
            n => self
                .advance
                .saturating_mul(n - 1)
                .saturating_add(self.glyph_width),
        }
    }

    /// The longest prefix of `text` whose rendered width fits within `width`
    /// pixels, truncated on a `char` boundary.
    ///
    /// This is the shared truncation every fixed-width text region uses to
    /// keep a label from spilling past its box (the taskbar's clock and task
    /// titles, the file browser's path bar and entry names), so the
    /// fit-to-width arithmetic lives in one place rather than being repeated
    /// per consumer (`AGENTS.md` §2.2). A `width` too small for even one glyph
    /// yields the empty string; a `text` that already fits is returned whole.
    /// Arithmetic saturates, so a pathological width never wraps (`AGENTS.md`
    /// §2.9).
    #[must_use]
    pub fn truncate_to_width<'a>(&self, text: &'a str, width: u32) -> &'a str {
        if width < self.glyph_width {
            return "";
        }
        let extra = width - self.glyph_width;
        let advance = self.advance.max(1);
        let capacity = (1 + extra / advance) as usize;
        match text.char_indices().nth(capacity) {
            Some((byte, _)) => &text[..byte],
            None => text,
        }
    }

    /// Draw `text` onto `surface` with its top-left corner at `(x, y)` in
    /// `color`, returning the pen x-coordinate after the last glyph.
    ///
    /// The pen advances by [`advance`](Self::advance) per character. Pixels
    /// that fall outside the surface (including at negative coordinates) are
    /// skipped, so off-screen text clips rather than panicking (`AGENTS.md`
    /// §2.9). Each lit glyph pixel is composited over the destination, so
    /// translucent text blends correctly (`AGENTS.md` §10).
    pub fn draw_text(
        &self,
        surface: &mut Surface,
        x: i32,
        y: i32,
        text: &str,
        color: Color,
    ) -> i32 {
        let source = color.premultiply();
        let mut pen = x;
        for ch in text.chars() {
            self.draw_glyph(surface, pen, y, *self.glyph(ch), source);
            pen = pen.saturating_add(advance_step(self.advance));
        }
        pen
    }

    /// The atlas glyph for `ch`, or the [`FALLBACK`] box for an
    /// out-of-range character.
    fn glyph(&self, ch: char) -> &Glyph {
        if ch < self.first || ch > self.last {
            return &FALLBACK;
        }
        let index = (ch as usize) - (self.first as usize);
        self.glyphs.get(index).unwrap_or(&FALLBACK)
    }

    /// Blit one premultiplied `source` glyph at top-left `(x, y)`.
    fn draw_glyph(&self, surface: &mut Surface, x: i32, y: i32, glyph: Glyph, source: Pixel) {
        for (row_index, row) in glyph.iter().enumerate() {
            if u32::try_from(row_index).unwrap_or(u32::MAX) >= self.glyph_height {
                break;
            }
            let py = y.saturating_add(row_step(row_index));
            for col in 0..self.glyph_width {
                if !pixel_set(*row, self.glyph_width, col) {
                    continue;
                }
                let px = x.saturating_add(col_step(col));
                if let (Ok(ux), Ok(uy)) = (u32::try_from(px), u32::try_from(py)) {
                    if let Some(dst) = surface.get(ux, uy) {
                        surface.set(ux, uy, source.over(dst));
                    }
                }
            }
        }
    }
}

/// Whether column `col` (counting from the left) is lit in `row`, where the
/// leftmost column is the high used bit `1 << (width - 1)`.
fn pixel_set(row: u8, width: u32, col: u32) -> bool {
    let shift = width.saturating_sub(1).saturating_sub(col);
    (u32::from(row) >> shift) & 1 == 1
}

/// The pen advance for one character as an `i32` step, saturating.
fn advance_step(advance: u32) -> i32 {
    i32::try_from(advance).unwrap_or(i32::MAX)
}

/// A glyph row offset as an `i32` step, saturating.
fn row_step(row_index: usize) -> i32 {
    i32::try_from(row_index).unwrap_or(i32::MAX)
}

/// A glyph column offset as an `i32` step, saturating.
fn col_step(col: u32) -> i32 {
    i32::try_from(col).unwrap_or(i32::MAX)
}
