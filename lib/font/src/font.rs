//! The system bitmap font and the glyph blitter that draws it onto a
//! [`Surface`].
//!
//! [`BitmapFont`] is a thin, cached front end to the sandboxed OS font
//! service (`fontd`): it holds only the monospace layout metrics (cell size,
//! pen advance, baseline, line height) derived from the console-atlas
//! geometry constants, and fetches each glyph's coverage bitmap from the
//! service over [`crate::client`]. No font outline or face lives in this
//! process.
//! [`BitmapFont::draw_text`] composites each fetched glyph onto a `lib/raster`
//! [`Surface`] through that crate's single premultiplied-alpha
//! [`Pixel::over`] path: the text colour is premultiplied once, scaled per
//! 8-bit coverage level into a 256-entry table, and blended per lit pixel —
//! so anti-aliased edges and translucent text both come out right with no
//! colour arithmetic duplicated here.

use tairix_raster::{Color, Pixel, Surface};
use tairix_vt::{char_width, truncate_to_width as truncate_to_columns};

use crate::atlas;
use crate::client;

/// The system monospace bitmap font: the layout metrics a client needs to
/// draw text, backed by the sandboxed font service for glyph coverage.
///
/// The face's uniform advance already carries the inter-glyph side bearings
/// and its ascent + descent carry the line box, so the pen advances by
/// exactly the cell width and lines by exactly the cell height.
///
/// A font renders at a chosen **cell height in physical pixels**. The metrics
/// derive from one native size ([`atlas::CELL_HEIGHT`], the console-atlas
/// geometry); [`inconsolata`] keeps that size, while [`with_pixel_height`]
/// asks for any other cell — the desktop resolves a comfortable physical size
/// from the theme's logical font size and the DPI scale. Every glyph is
/// rasterised by the font service **directly from the TrueType outline** at
/// the requested size, so text is crisp whether tiny or very large — never a
/// stretched bitmap. Every derived metric (advance, cell width, baseline, line
/// height) scales with the cell height, keeping the font monospaced and its
/// width-to-height ratio constant.
///
/// [`inconsolata`]: Self::inconsolata
/// [`with_pixel_height`]: Self::with_pixel_height
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BitmapFont {
    /// The cell height this font renders at, in physical pixels, always in
    /// [`MIN_PIXEL_HEIGHT`](Self::MIN_PIXEL_HEIGHT)..=[`MAX_PIXEL_HEIGHT`](Self::MAX_PIXEL_HEIGHT).
    cell_height: u32,
}

impl Default for BitmapFont {
    /// The native-size built-in family ([`inconsolata`](Self::inconsolata)).
    fn default() -> Self {
        Self::inconsolata()
    }
}

impl BitmapFont {
    /// The smallest cell height a font may render at, in physical pixels.
    ///
    /// Below this a monospace glyph loses the distinguishing strokes that keep
    /// text legible, so [`with_pixel_height`](Self::with_pixel_height) never
    /// renders smaller.
    pub const MIN_PIXEL_HEIGHT: u32 = 8;

    /// The largest cell height a font may render at, in physical pixels.
    ///
    /// The outline rasteriser produces a crisp glyph at any size, but a cell
    /// this tall is already a large heading; the bound caps the size of a
    /// single cached bitmap so a pathological request cannot demand an
    /// unbounded rasterisation.
    pub const MAX_PIXEL_HEIGHT: u32 = 512;

    /// The built-in family at its **native** cell size (the console-atlas
    /// geometry). This is the size the text console renders at.
    #[must_use]
    pub const fn inconsolata() -> Self {
        Self {
            cell_height: atlas::CELL_HEIGHT,
        }
    }

    /// The built-in family rendered at a cell height of `pixels` physical
    /// pixels, clamped to
    /// [`MIN_PIXEL_HEIGHT`](Self::MIN_PIXEL_HEIGHT)..=[`MAX_PIXEL_HEIGHT`](Self::MAX_PIXEL_HEIGHT).
    ///
    /// The desktop uses this to render UI text at the theme's requested size.
    /// Every height rasterises each glyph from the outline (in the font
    /// service) at that exact size, so both smaller and larger text stay
    /// crisply anti-aliased rather than stretched from a fixed bitmap. Asking
    /// for exactly the native height yields [`inconsolata`](Self::inconsolata).
    #[must_use]
    pub const fn with_pixel_height(pixels: u32) -> Self {
        let cell_height = if pixels < Self::MIN_PIXEL_HEIGHT {
            Self::MIN_PIXEL_HEIGHT
        } else if pixels > Self::MAX_PIXEL_HEIGHT {
            Self::MAX_PIXEL_HEIGHT
        } else {
            pixels
        };
        Self { cell_height }
    }

    /// The glyph cell width in pixels: the native cell width scaled to this
    /// font's cell height (rounded to a whole pixel, never below one).
    #[must_use]
    pub const fn glyph_width(self) -> u32 {
        let scaled =
            (atlas::CELL_WIDTH * self.cell_height + atlas::CELL_HEIGHT / 2) / atlas::CELL_HEIGHT;
        if scaled == 0 {
            1
        } else {
            scaled
        }
    }

    /// The glyph cell height in pixels.
    #[must_use]
    pub const fn glyph_height(self) -> u32 {
        self.cell_height
    }

    /// The baseline row within the cell (pixel rows above the baseline): the
    /// native atlas baseline scaled to this font's cell height, so a resized
    /// glyph sits on the baseline exactly as the native cell does.
    #[must_use]
    pub const fn baseline(self) -> u32 {
        (atlas::BASELINE * self.cell_height + atlas::CELL_HEIGHT / 2) / atlas::CELL_HEIGHT
    }

    /// The horizontal pen advance per character in pixels (the cell width).
    #[must_use]
    pub const fn advance(self) -> u32 {
        self.glyph_width()
    }

    /// The vertical distance between baselines in pixels (the cell height).
    #[must_use]
    pub const fn line_height(self) -> u32 {
        self.cell_height
    }

    /// The pixel width of `text` rendered on one line, including two-cell
    /// advances for wide Unicode scalars. An empty string is zero wide.
    ///
    /// Arithmetic saturates, so a pathologically long string reports
    /// [`u32::MAX`] rather than wrapping.
    #[must_use]
    pub fn text_width(self, text: &str) -> u32 {
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
    pub fn truncate_to_width(self, text: &str, width: u32) -> &str {
        truncate_to_columns(text, (width / self.advance()) as usize)
    }

    /// Draw `text` onto `surface` with its top-left corner at `(x, y)` in
    /// `color`, returning the pen x-coordinate after the last glyph.
    ///
    /// The pen advances by [`advance`](Self::advance) per terminal cell. Each
    /// glyph's coverage is fetched from the font service (cached client-side)
    /// at this font's cell height and composited over the destination at its
    /// anti-aliased coverage, so translucent text and glyph edges blend
    /// correctly. Pixels that fall outside the surface (including at negative
    /// coordinates) are skipped, so off-screen text clips rather than
    /// panicking. A scalar the faces do not cover draws the U+FFFD replacement
    /// glyph (the service's fallback) rather than being silently dropped; if
    /// the service is unreachable the glyph composites nothing (fail closed)
    /// rather than reaching for any local font data.
    pub fn draw_text(self, surface: &mut Surface, x: i32, y: i32, text: &str, color: Color) -> i32 {
        let sources = coverage_sources(color);
        let advance = self.advance();
        let mut pen = x;
        for ch in text.chars() {
            let cells = u32::from(char_width(ch));
            let step_advance = advance.saturating_mul(cells);
            // A glyph may span two cells; the service returns a bitmap two
            // cells wide, so a narrow glyph is clipped to its own advance.
            client::with_glyph(ch, self.cell_height, |glyph| {
                let visible = step_advance.min(glyph.width);
                draw_coverage_glyph(surface, pen, y, glyph, visible, &sources);
            });
            pen = pen.saturating_add(advance_step(step_advance));
        }
        pen
    }
}

/// The premultiplied source pixel for each of the 256 8-bit coverage levels:
/// `color` with its alpha scaled by `level / 255`, computed once per
/// [`BitmapFont::draw_text`] call so the per-pixel work is one table load
/// and one `over`. Level 255 keeps the caller's exact alpha.
fn coverage_sources(color: Color) -> [Pixel; 256] {
    let source = color.premultiply();
    let mut sources = [source; 256];
    for (level, slot) in (0u8..=u8::MAX).zip(sources.iter_mut()) {
        *slot = source.scale_alpha(level);
    }
    sources
}

/// Blit one service-returned glyph at top-left `(x, y)` from its row-major
/// `width * height` 8-bit coverage, blending each covered pixel up to
/// `visible` columns. Off-surface pixels clip rather than panic.
fn draw_coverage_glyph(
    surface: &mut Surface,
    x: i32,
    y: i32,
    glyph: &client::CachedGlyph,
    visible: u32,
    sources: &[Pixel; 256],
) {
    let width = glyph.width;
    for row in 0..glyph.height {
        let py = y.saturating_add(step(row));
        let Ok(uy) = u32::try_from(py) else { continue };
        for col in 0..visible.min(width) {
            let coverage = glyph
                .data
                .get((row * width + col) as usize)
                .copied()
                .unwrap_or(0);
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
