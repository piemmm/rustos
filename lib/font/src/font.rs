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

use core::ops::Range;

use tairix_abi::font_ipc::FontWeight;
use tairix_geometry::Scale;
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Fonts, TextRole};
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
    /// The weight glyphs are requested in. A heavier weight thickens the
    /// coverage the service returns and leaves every metric alone, so it is
    /// not part of any layout arithmetic below.
    weight: FontWeight,
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
            weight: FontWeight::Regular,
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
        Self {
            cell_height,
            weight: FontWeight::Regular,
        }
    }

    /// The font a theme's `role` resolves to at `scale`: the role's authored
    /// logical size converted to a physical cell height through the one shared
    /// DPI scale, set in the weight the theme names.
    ///
    /// This is the only place a themed text role becomes a drawable font, so
    /// every surface — window furniture, the taskbar, a control label, an app's
    /// own text — sizes and weights a role identically and none of them repeats
    /// the logical-to-physical conversion.
    #[must_use]
    pub fn for_role(fonts: &Fonts, role: TextRole, scale: Scale) -> Self {
        let spec = fonts.spec(role);
        Self::with_pixel_height(scale.scale_length(u32::from(spec.size_px)))
            .with_weight(spec.weight)
    }

    /// The same font set in `weight`.
    ///
    /// The desktop draws a text role in the weight its theme names
    /// (`tairix_theme::FontSpec::weight`); a heavier weight is a different
    /// raster of the same outline at the same advance, so switching weight
    /// never moves a glyph or reflows a label.
    #[must_use]
    pub const fn with_weight(self, weight: FontWeight) -> Self {
        Self { weight, ..self }
    }

    /// The weight glyphs are requested in.
    #[must_use]
    pub const fn weight(self) -> FontWeight {
        self.weight
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
            client::with_glyph(ch, self.cell_height, self.weight, |glyph| {
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
///
/// Both axes are clipped against the surface once, before any pixel is
/// touched, so the loop below walks only pixels that land on it: each row
/// blends the glyph's coverage bytes against the destination row slice in
/// step, paying one bounds check and one row-address computation per row
/// rather than per pixel. The destination span comes from the surface's own
/// row accessor, so the glyph is confined by any clip window in force — a label
/// that reaches its view's edge stops there instead of running past it —
/// without this blitter knowing where that edge is.
fn draw_coverage_glyph(
    surface: &mut Surface,
    x: i32,
    y: i32,
    glyph: &crate::glyph_cache::CachedGlyph,
    visible: u32,
    sources: &[Pixel; 256],
) {
    let Some(columns) = visible_span(x, visible.min(glyph.width), surface.width()) else {
        return;
    };
    let Some(rows) = visible_span(y, glyph.height, surface.height()) else {
        return;
    };
    let Ok(first_row) = u32::try_from(rows.destination) else {
        return;
    };
    let Ok(first_column) = u32::try_from(columns.destination) else {
        return;
    };
    let Ok(span) = u32::try_from(columns.source.len()) else {
        return;
    };
    for (source_row, destination_row) in rows.source.zip(first_row..) {
        let Some(coverage) = glyph_row(glyph, source_row, &columns.source) else {
            continue;
        };
        let Some((drawn_from, destination)) =
            surface.row_span_mut(destination_row, first_column, span)
        else {
            continue;
        };
        // Whatever leading columns a clip window withheld are skipped in the
        // coverage too, so mask and destination stay in step.
        let Ok(withheld) = usize::try_from(drawn_from - first_column) else {
            continue;
        };
        let Some(coverage) = coverage.get(withheld..) else {
            continue;
        };
        for (&level, pixel) in coverage.iter().zip(destination.iter_mut()) {
            if level == 0 {
                continue;
            }
            *pixel = sources[usize::from(level)].over(*pixel);
        }
    }
}

/// The part of one glyph axis that lands on the surface: the half-open source
/// range of glyph rows (or columns) to read, and the surface row (or column)
/// the first of them writes to.
struct VisibleSpan {
    source: Range<usize>,
    destination: usize,
}

/// Clip `count` glyph rows (or columns) drawn at `origin` against a surface
/// extent of `limit`, or `None` when none of them lands on it.
///
/// The arithmetic is widened so a glyph drawn far off either edge clips to
/// nothing instead of wrapping onto the wrong pixels.
fn visible_span(origin: i32, count: u32, limit: u32) -> Option<VisibleSpan> {
    let origin = i64::from(origin);
    let first = (-origin).max(0);
    let last = (i64::from(limit) - origin).min(i64::from(count));
    if first >= last {
        return None;
    }
    Some(VisibleSpan {
        source: usize::try_from(first).ok()?..usize::try_from(last).ok()?,
        destination: usize::try_from(origin + first).ok()?,
    })
}

/// Glyph row `row`'s coverage bytes over the `columns` the surface can show.
///
/// A decoded reply carries exactly `width * height` bytes, so this yields
/// `None` only for a structurally impossible short bitmap — which skips the
/// row rather than reading past it.
fn glyph_row<'a>(
    glyph: &'a crate::glyph_cache::CachedGlyph,
    row: usize,
    columns: &Range<usize>,
) -> Option<&'a [u8]> {
    let width = usize::try_from(glyph.width).ok()?;
    let base = row.checked_mul(width)?;
    glyph
        .data
        .get(base.checked_add(columns.start)?..base.checked_add(columns.end)?)
}

/// The pen advance for one character as an `i32` step, saturating.
fn advance_step(advance: u32) -> i32 {
    i32::try_from(advance).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod blit_tests {
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use tairix_raster::{Color, Pixel, Surface};

    use super::{coverage_sources, draw_coverage_glyph};
    use crate::glyph_cache::CachedGlyph;

    /// The straightforward blit: walk every glyph pixel, clip it, and
    /// composite it through the surface's per-pixel accessors.
    /// [`draw_coverage_glyph`] clips both axes up front and writes row
    /// slices instead, which must be a pure cost change; this loop is the
    /// yardstick that proves it and lives only here, so production keeps one
    /// definition of the blit.
    fn reference_coverage_glyph(
        surface: &mut Surface,
        x: i32,
        y: i32,
        glyph: &CachedGlyph,
        visible: u32,
        sources: &[Pixel; 256],
    ) {
        let width = glyph.width;
        for row in 0..glyph.height {
            let py = y.saturating_add(i32::try_from(row).unwrap_or(i32::MAX));
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
                let px = x.saturating_add(i32::try_from(col).unwrap_or(i32::MAX));
                let Ok(ux) = u32::try_from(px) else { continue };
                if let Some(dst) = surface.get(ux, uy) {
                    surface.set(ux, uy, sources[usize::from(coverage)].over(dst));
                }
            }
        }
    }

    /// A glyph whose coverage spans transparent, partial, and full levels, so
    /// a blit that mishandles any of them shows up.
    fn varied_glyph(width: u32, height: u32) -> CachedGlyph {
        let data: Vec<u8> = (0..width * height)
            .map(|index| match index % 5 {
                0 => 0,
                1 => 255,
                other => u8::try_from((index * 37 + other) % 256).unwrap_or(0),
            })
            .collect();
        CachedGlyph {
            width,
            height,
            data: Box::from(data.as_slice()),
        }
    }

    /// A surface whose every pixel differs, so a blit that composites against
    /// the wrong destination cannot hide behind a uniform background.
    fn patterned_surface(width: u32, height: u32) -> Surface {
        let mut surface = Surface::new(width, height).expect("allocates");
        for y in 0..height {
            for x in 0..width {
                let channel = |factor: u32| u8::try_from((x * factor + y * 7) % 256).unwrap_or(0);
                let color = Color::rgba(channel(3), channel(11), channel(29), channel(53));
                surface.set(x, y, color.premultiply());
            }
        }
        surface
    }

    /// A glyph is confined by the surface's clip window, and every surviving
    /// pixel is exactly the one an unclipped blit produced: a blitter that
    /// skipped the destination columns a window withheld without skipping the
    /// same coverage bytes would slide the glyph sideways into the window.
    #[test]
    fn coverage_blit_is_confined_by_the_clip_window() {
        let glyph = varied_glyph(10, 14);
        let sources = coverage_sources(Color::rgba(240, 20, 90, 255));
        // Windows that cut the glyph on each side, through its middle, and
        // one that misses it entirely.
        let windows = [
            (0, 0, 24, 18),
            (5, 0, 4, 18),
            (0, 6, 24, 3),
            (7, 7, 3, 2),
            (20, 0, 8, 18),
        ];
        let untouched = patterned_surface(24, 18);
        for &(cx, cy, cw, ch) in &windows {
            let mut clipped = untouched.clone();
            let mut whole = untouched.clone();
            clipped.with_clip(cx, cy, cw, ch, |surface| {
                draw_coverage_glyph(surface, 3, 5, &glyph, 10, &sources);
            });
            draw_coverage_glyph(&mut whole, 3, 5, &glyph, 10, &sources);
            for y in 0..18 {
                for x in 0..24 {
                    let inside = (cx..cx + cw).contains(&x) && (cy..cy + ch).contains(&y);
                    let want = if inside { &whole } else { &untouched };
                    assert_eq!(
                        clipped.get(x, y),
                        want.get(x, y),
                        "pixel ({x}, {y}) with clip ({cx}, {cy}, {cw}, {ch})"
                    );
                }
            }
        }
    }

    #[test]
    fn coverage_blit_matches_the_per_pixel_reference() {
        let glyph = varied_glyph(10, 14);
        // Origins on, straddling, and wholly off each edge, plus the extremes
        // where the old per-pixel offset arithmetic saturated.
        let origins = [i32::MIN, -40, -9, -1, 0, 1, 13, 23, 24, 90, i32::MAX];
        for &color in &[Color::rgba(240, 20, 90, 255), Color::rgba(240, 20, 90, 180)] {
            let sources = coverage_sources(color);
            for &visible in &[0u32, 1, 6, 10, 40] {
                for &x in &origins {
                    for &y in &origins {
                        let mut actual = patterned_surface(24, 18);
                        let mut expected = actual.clone();
                        draw_coverage_glyph(&mut actual, x, y, &glyph, visible, &sources);
                        reference_coverage_glyph(&mut expected, x, y, &glyph, visible, &sources);
                        for (index, (got, want)) in
                            actual.pixels().iter().zip(expected.pixels()).enumerate()
                        {
                            assert_eq!(
                                got, want,
                                "pixel {index} differs at ({x},{y}) \
                                 visible {visible} alpha {}",
                                color.a
                            );
                        }
                    }
                }
            }
        }
    }
}
