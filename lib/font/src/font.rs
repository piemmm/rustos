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

use tairix_raster::{Color, Pixel, Surface};
use tairix_vt::{char_width, truncate_to_width as truncate_to_columns};

use crate::atlas;
use crate::cache;
use crate::glyph::{cell_index, lookup_or_fallback, Glyph};

/// The system monospace bitmap font: Inconsolata EX with its M PLUS 1 Code
/// Japanese, `D2Coding` Korean, and Noto Sans Hebrew companions, plus their shared
/// layout metrics.
///
/// The face's uniform advance already carries the inter-glyph side bearings
/// and its ascent + descent carry the line box, so the pen advances by
/// exactly the cell width and lines by exactly the cell height.
///
/// A font renders at a chosen **cell height in physical pixels**. The atlas is
/// authored at one native size ([`atlas::CELL_HEIGHT`]); [`inconsolata`] keeps
/// that size for the text console, while [`with_pixel_height`] asks for a
/// smaller cell — the desktop resolves a comfortable physical size from the
/// theme's logical font size and the DPI scale. A smaller cell resamples each
/// glyph from the native bitmap with an area-averaging filter (cached, see
/// [`crate::cache`]); at the native size the resampler is bypassed entirely, so
/// console rendering is byte-for-byte what it always was. Every derived metric
/// (advance, cell width, line height) scales with the cell height, keeping the
/// font monospaced and its width-to-height ratio constant.
///
/// [`inconsolata`]: Self::inconsolata
/// [`with_pixel_height`]: Self::with_pixel_height
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BitmapFont {
    /// The cell height this font renders at, in physical pixels, always in
    /// [`MIN_PIXEL_HEIGHT`](Self::MIN_PIXEL_HEIGHT)..=[`atlas::CELL_HEIGHT`].
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
    /// resamples smaller.
    pub const MIN_PIXEL_HEIGHT: u32 = 8;

    /// The built-in family at its **native** atlas size, with Inconsolata EX
    /// primary, M PLUS 1 Code for Japanese, `D2Coding` for Korean, and Noto
    /// Sans Hebrew for Hebrew and Yiddish coverage.
    ///
    /// This is the size the text console renders at; its glyphs come straight
    /// from the atlas with no resampling.
    #[must_use]
    pub const fn inconsolata() -> Self {
        Self {
            cell_height: atlas::CELL_HEIGHT,
        }
    }

    /// The built-in family rendered at a cell height of `pixels` physical
    /// pixels, clamped to
    /// [`MIN_PIXEL_HEIGHT`](Self::MIN_PIXEL_HEIGHT)..=[`atlas::CELL_HEIGHT`].
    ///
    /// The desktop uses this to render UI text at the theme's requested size:
    /// a height below the native atlas cell resamples each glyph down with an
    /// area-averaging filter, so smaller text stays smoothly anti-aliased
    /// rather than pixel-dropped. Asking for the native height (or larger, which
    /// clamps to it) yields exactly [`inconsolata`](Self::inconsolata); the
    /// atlas is never upscaled.
    #[must_use]
    pub const fn with_pixel_height(pixels: u32) -> Self {
        let cell_height = if pixels < Self::MIN_PIXEL_HEIGHT {
            Self::MIN_PIXEL_HEIGHT
        } else if pixels > atlas::CELL_HEIGHT {
            atlas::CELL_HEIGHT
        } else {
            pixels
        };
        Self { cell_height }
    }

    /// `true` when this font renders straight from the atlas with no
    /// resampling (its cell height is the native atlas height).
    #[must_use]
    const fn is_native(self) -> bool {
        self.cell_height == atlas::CELL_HEIGHT
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
    /// The pen advances by [`advance`](Self::advance) per terminal cell. Pixels
    /// that fall outside the surface (including at negative coordinates) are
    /// skipped, so off-screen text clips rather than panicking. Each covered
    /// glyph pixel is composited over the destination at its anti-aliased
    /// coverage, so translucent text and glyph edges blend correctly. A
    /// character the face does not cover draws the U+FFFD replacement glyph
    /// rather than being silently dropped.
    pub fn draw_text(self, surface: &mut Surface, x: i32, y: i32, text: &str, color: Color) -> i32 {
        if self.is_native() {
            return self.draw_text_native(surface, x, y, text, color);
        }
        self.draw_text_scaled(surface, x, y, text, color)
    }

    /// Draw `text` straight from the atlas at the native cell size — the exact
    /// path the text console has always used, with no resampling.
    fn draw_text_native(
        self,
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

    /// Draw `text` at a sub-native cell size, blitting each glyph's resampled
    /// coverage (fetched from the shared cache) instead of the atlas bitmap.
    fn draw_text_scaled(
        self,
        surface: &mut Surface,
        x: i32,
        y: i32,
        text: &str,
        color: Color,
    ) -> i32 {
        let sources = coverage_sources(color);
        let advance = self.advance();
        // A glyph may cover two cells, so its resampled bitmap is two cells
        // wide; a one-cell glyph is clipped to the left cell, exactly as the
        // native path clips a narrow glyph to `CELL_WIDTH` of `GLYPH_WIDTH`.
        let full_width = advance.saturating_mul(2);
        let height = self.cell_height;
        // Bounded by the native glyph, so the resampled bitmap always fits.
        let mut buffer = [0u8; (atlas::GLYPH_WIDTH * atlas::CELL_HEIGHT) as usize];
        let mut pen = x;
        for ch in text.chars() {
            let cells = u32::from(char_width(ch));
            let index = cell_index(ch);
            cache::scaled_coverage(index, full_width, height, &mut buffer);
            let visible = advance.saturating_mul(cells).min(full_width);
            draw_scaled_glyph(
                surface, pen, y, &buffer, full_width, height, visible, &sources,
            );
            pen = pen.saturating_add(advance_step(advance.saturating_mul(cells)));
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

/// Blit one resampled glyph at top-left `(x, y)` from its row-major coverage
/// `bitmap` (`bitmap_width` wide, `height` tall), blending each covered pixel
/// up to `visible` columns. Off-surface pixels clip rather than panic, exactly
/// like the native blitter.
#[allow(clippy::too_many_arguments)]
fn draw_scaled_glyph(
    surface: &mut Surface,
    x: i32,
    y: i32,
    bitmap: &[u8],
    bitmap_width: u32,
    height: u32,
    visible: u32,
    sources: &[Pixel; 16],
) {
    for row in 0..height {
        let py = y.saturating_add(step(row));
        let Ok(uy) = u32::try_from(py) else { continue };
        for col in 0..visible.min(bitmap_width) {
            let coverage = bitmap
                .get((row * bitmap_width + col) as usize)
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
