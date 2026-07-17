//! The system bitmap font and the glyph blitter that draws it onto a
//! [`Surface`].
//!
//! [`BitmapFont`] couples the generated Inconsolata EX + M PLUS 1 Code +
//! `D2Coding` + Noto Sans Hebrew atlas with its metrics (cell size, pen advance,
//! line height) and the coverage-aware blitter.
//! [`BitmapFont::draw_text`] composites each glyph onto a `lib/raster`
//! [`Surface`] through that crate's single premultiplied-alpha
//! [`Pixel::over`] path: the text colour is premultiplied once, scaled per
//! coverage level into a 16-entry table, and blended per lit pixel — so
//! anti-aliased edges and translucent text both come out right with no
//! colour arithmetic duplicated here.

use rustos_raster::{Color, Pixel, Surface};
use rustos_vt::{char_width, truncate_to_width as truncate_to_columns};

use crate::atlas;
use crate::glyph::{lookup_or_fallback, Glyph};

/// The system monospace bitmap font: Inconsolata EX with its M PLUS 1 Code
/// Japanese, `D2Coding` Korean, and Noto Sans Hebrew companions, plus their shared
/// layout metrics.
///
/// The face's uniform advance already carries the inter-glyph side bearings
/// and its ascent + descent carry the line box, so the pen advances by
/// exactly the cell width and lines by exactly the cell height.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BitmapFont(());

impl BitmapFont {
    /// The built-in family, with Inconsolata EX primary, M PLUS 1 Code for
    /// Japanese, `D2Coding` for Korean, and Noto Sans Hebrew for Hebrew and
    /// Yiddish coverage.
    #[must_use]
    pub const fn inconsolata() -> Self {
        Self(())
    }

    /// The glyph cell width in pixels.
    #[must_use]
    pub const fn glyph_width(&self) -> u32 {
        atlas::CELL_WIDTH
    }

    /// The glyph cell height in pixels.
    #[must_use]
    pub const fn glyph_height(&self) -> u32 {
        atlas::CELL_HEIGHT
    }

    /// The horizontal pen advance per character in pixels.
    #[must_use]
    pub const fn advance(&self) -> u32 {
        atlas::CELL_WIDTH
    }

    /// The vertical distance between baselines in pixels.
    #[must_use]
    pub const fn line_height(&self) -> u32 {
        atlas::CELL_HEIGHT
    }

    /// The pixel width of `text` rendered on one line, including two-cell
    /// advances for wide Unicode scalars. An empty string is zero wide.
    ///
    /// Arithmetic saturates, so a pathologically long string reports
    /// [`u32::MAX`] rather than wrapping.
    #[must_use]
    pub fn text_width(&self, text: &str) -> u32 {
        text.chars().fold(0, |width, ch| {
            width.saturating_add(self.advance().saturating_mul(u32::from(char_width(ch))))
        })
    }

    /// The longest prefix of `text` whose rendered width fits within `width`
    /// pixels, truncated on a `char` boundary.
    ///
    /// This is the shared truncation every fixed-width text region uses to
    /// keep a label from spilling past its box (the taskbar's clock and task
    /// titles, the file browser's path bar and entry names), so the
    /// fit-to-width arithmetic lives in one place rather than being repeated
    /// per consumer. A `width` too small for even one glyph yields the empty
    /// string; a `text` that already fits is returned whole.
    #[must_use]
    pub fn truncate_to_width<'a>(&self, text: &'a str, width: u32) -> &'a str {
        truncate_to_columns(text, (width / self.advance()) as usize)
    }

    /// Draw `text` onto `surface` with its top-left corner at `(x, y)` in
    /// `color`, returning the pen x-coordinate after the last glyph.
    ///
    /// The pen advances by [`advance`](Self::advance) per terminal cell. Pixels
    /// that fall outside the surface (including at negative coordinates) are
    /// skipped, so off-screen text clips rather than panicking. Each covered
    /// glyph pixel is composited over the destination at its anti-aliased
    /// coverage, so translucent text and glyph edges blend correctly. A
    /// character the face does not cover draws the U+FFFD replacement glyph
    /// rather than being silently dropped.
    pub fn draw_text(
        &self,
        surface: &mut Surface,
        x: i32,
        y: i32,
        text: &str,
        color: Color,
    ) -> i32 {
        let sources = coverage_sources(color);
        let mut pen = x;
        for ch in text.chars() {
            let cells = u32::from(char_width(ch));
            let glyph = lookup_or_fallback(ch);
            draw_glyph(
                surface,
                pen,
                y,
                &glyph,
                self.advance().saturating_mul(cells),
                &sources,
            );
            pen = pen.saturating_add(advance_step(self.advance().saturating_mul(cells)));
        }
        pen
    }
}

/// The premultiplied source pixel for each of the 16 coverage levels:
/// `color` with its alpha scaled by `level / 15`, computed once per
/// [`BitmapFont::draw_text`] call so the per-pixel work is one table load
/// and one `over`.
fn coverage_sources(color: Color) -> [Pixel; 16] {
    let source = color.premultiply();
    // Nibble 15 must map to exactly 255 so full coverage keeps the caller's
    // alpha; 17 = 255 / 15.
    let mut sources = [source; 16];
    for (level, slot) in (0u8..).zip(sources.iter_mut()) {
        *slot = source.scale_alpha(level * 17);
    }
    sources
}

/// Blit one glyph at top-left `(x, y)`, blending each covered pixel.
fn draw_glyph(
    surface: &mut Surface,
    x: i32,
    y: i32,
    glyph: &Glyph,
    width: u32,
    sources: &[Pixel; 16],
) {
    for row in 0..atlas::CELL_HEIGHT {
        let py = y.saturating_add(step(row));
        let Ok(uy) = u32::try_from(py) else { continue };
        for col in 0..width.min(atlas::GLYPH_WIDTH) {
            let coverage = glyph.coverage(col, row);
            if coverage == 0 {
                continue;
            }
            let px = x.saturating_add(step(col));
            let Ok(ux) = u32::try_from(px) else { continue };
            if let Some(dst) = surface.get(ux, uy) {
                surface.set(ux, uy, sources[usize::from(coverage)].over(dst));
            }
        }
    }
}

/// The pen advance for one character as an `i32` step, saturating.
fn advance_step(advance: u32) -> i32 {
    i32::try_from(advance).unwrap_or(i32::MAX)
}

/// A glyph row/column offset as an `i32` step, saturating.
fn step(offset: u32) -> i32 {
    i32::try_from(offset).unwrap_or(i32::MAX)
}
