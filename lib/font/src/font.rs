//! The system bitmap font and the glyph blitter that draws it onto a
//! [`Surface`].
//!
//! [`BitmapFont`] is a thin, cached front end to the sandboxed OS font
//! service (`fontd`): it names a **family**, a pixel height, and a weight,
//! and fetches both a family's line metrics and each glyph's coverage
//! bitmap from the service over [`crate::client`]. No font outline or face
//! lives in this process.
//!
//! # Monospace and proportional families draw through one path
//!
//! A family is either fixed-pitch (every glyph shares one advance,
//! [`BitmapFont::monospace_advance`] reports it) or proportional (each
//! glyph advances by its own reported width). [`BitmapFont::advance`],
//! [`BitmapFont::text_width`], [`BitmapFont::truncate_to_width`], and
//! [`BitmapFont::draw_text`] all measure through the per-glyph advance the
//! service reports, so the same code lays out either kind of family — a
//! monospace family simply reports the same advance for every glyph. A
//! caller that must draw a character grid (a terminal, a hex view) uses
//! [`BitmapFont::monospace`] or [`BitmapFont::new`] with a monospace family
//! and reads [`BitmapFont::cell_width`] for its column width; desktop chrome
//! measures with [`BitmapFont::text_width`]/[`BitmapFont::advance`] instead
//! of multiplying a character count by a cell width.
//!
//! [`BitmapFont::draw_text`] composites each fetched glyph onto a `lib/raster`
//! [`Surface`] through that crate's single premultiplied-alpha
//! [`Pixel::over`] path: the text colour is premultiplied once, scaled per
//! 8-bit coverage level into a 256-entry table, and blended per lit pixel —
//! so anti-aliased edges and translucent text both come out right with no
//! colour arithmetic duplicated here.
//!
//! # Fitting a label to its box
//!
//! [`BitmapFont::elide_to_width`] and [`BitmapFont::wrap_to_width`] build on
//! that one measurement: the first reserves room for [`ELLIPSIS`] and cuts,
//! the second breaks a label at whitespace across a bounded number of lines
//! and elides only the last. Both borrow slices of the caller's text and
//! allocate nothing, so a label laid out every repaint costs no heap
//! traffic.

use core::ops::Range;

use tairix_abi::font_ipc::{FamilyKey, FontMetrics, FontWeight};
use tairix_geometry::Scale;
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Fonts, TextRole};
use tairix_vt::char_width;

use crate::atlas;
use crate::client::{self, GlyphClient};
use crate::measure::MeasuredText;

/// The mark that ends a line the text outgrew: HORIZONTAL ELLIPSIS.
///
/// One definition serves both halves of the job —
/// [`BitmapFont::text_width`] reserves room for it and
/// [`BitmapFont::draw_text`] paints it — so the mark measured and the mark
/// drawn can never disagree. It is a `&str` for exactly that reason: a
/// `char` would have to be encoded at every call site.
pub const ELLIPSIS: &str = "\u{2026}";

/// A family, pixel height, and weight to draw with: the reference a client
/// needs to fetch a family's line metrics and any glyph's coverage bitmap
/// from the sandboxed font service.
///
/// A font renders at a chosen **pixel height in physical pixels**.
/// [`console`](Self::console) keeps the compiled-in console-atlas cell
/// height (what the text console draws), [`monospace`](Self::monospace)
/// renders the fixed-pitch [`FamilyKey::MONO`] family at any other size, and
/// [`new`](Self::new) renders any family at any size — the desktop resolves
/// a comfortable physical size from the theme's logical font size and the
/// DPI scale. Every glyph is rasterised by the font service **directly from
/// the TrueType outline** at the requested size, so text is crisp whether
/// tiny or very large — never a stretched bitmap.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BitmapFont {
    /// The family to render glyphs from.
    family: FamilyKey,
    /// The line-box height this font renders at, in physical pixels, always
    /// in [`MIN_PIXEL_HEIGHT`](Self::MIN_PIXEL_HEIGHT)..=[`MAX_PIXEL_HEIGHT`](Self::MAX_PIXEL_HEIGHT).
    pixel_height: u32,
    /// The weight glyphs are requested in.
    weight: FontWeight,
}

impl Default for BitmapFont {
    /// The console family at its native size ([`console`](Self::console)).
    fn default() -> Self {
        Self::console()
    }
}

impl BitmapFont {
    /// The smallest pixel height a font may render at, in physical pixels.
    ///
    /// Below this a glyph loses the distinguishing strokes that keep text
    /// legible, so [`new`](Self::new) never renders smaller.
    pub const MIN_PIXEL_HEIGHT: u32 = 8;

    /// The largest pixel height a font may render at, in physical pixels.
    ///
    /// The outline rasteriser produces a crisp glyph at any size, but a line
    /// box this tall is already a large heading; the bound caps the size of
    /// a single cached bitmap so a pathological request cannot demand an
    /// unbounded rasterisation.
    pub const MAX_PIXEL_HEIGHT: u32 = 512;

    /// The fixed-pitch [`FamilyKey::MONO`] family at the compiled-in
    /// console-atlas cell height: what the text console (`lib/fbcon`) and
    /// the boot console draw.
    #[must_use]
    pub const fn console() -> Self {
        Self {
            family: FamilyKey::MONO,
            pixel_height: atlas::CELL_HEIGHT,
            weight: FontWeight::Regular,
        }
    }

    /// The fixed-pitch [`FamilyKey::MONO`] family rendered at `pixel_height`
    /// physical pixels, clamped to
    /// [`MIN_PIXEL_HEIGHT`](Self::MIN_PIXEL_HEIGHT)..=[`MAX_PIXEL_HEIGHT`](Self::MAX_PIXEL_HEIGHT).
    ///
    /// A character-grid drawer (the terminal, a hex view) that needs a
    /// specific size but is not drawing from a theme uses this rather than
    /// [`new`](Self::new).
    #[must_use]
    pub const fn monospace(pixel_height: u32) -> Self {
        Self::new(FamilyKey::MONO, pixel_height)
    }

    /// `family` rendered at `pixel_height` physical pixels, clamped to
    /// [`MIN_PIXEL_HEIGHT`](Self::MIN_PIXEL_HEIGHT)..=[`MAX_PIXEL_HEIGHT`](Self::MAX_PIXEL_HEIGHT).
    ///
    /// Every height rasterises each glyph from the outline (in the font
    /// service) at that exact size, so both smaller and larger text stay
    /// crisply anti-aliased rather than stretched from a fixed bitmap.
    #[must_use]
    pub const fn new(family: FamilyKey, pixel_height: u32) -> Self {
        let pixel_height = clamp_pixel_height(pixel_height);
        Self {
            family,
            pixel_height,
            weight: FontWeight::Regular,
        }
    }

    /// The font a theme's `role` resolves to at `scale`: the role's authored
    /// family and logical size converted to a physical pixel height through
    /// the one shared DPI scale, set in the weight the theme names.
    ///
    /// This is the only place a themed text role becomes a drawable font, so
    /// every surface — window furniture, the taskbar, a control label, an
    /// app's own text — sizes, families, and weights a role identically and
    /// none of them repeats the logical-to-physical conversion.
    #[must_use]
    pub fn for_role(fonts: &Fonts, role: TextRole, scale: Scale) -> Self {
        let spec = fonts.spec(role);
        Self::new(spec.family, scale.scale_length(u32::from(spec.size_px))).with_weight(spec.weight)
    }

    /// The same font set in `weight`.
    ///
    /// The desktop draws a text role in the weight its theme names
    /// (`tairix_theme::FontSpec::weight`); a heavier weight is a different
    /// raster of the same outline at (for a variable face) its own advance,
    /// so switching weight never moves a glyph laid out with the weight it
    /// was measured in.
    #[must_use]
    pub const fn with_weight(self, weight: FontWeight) -> Self {
        Self { weight, ..self }
    }

    /// The weight glyphs are requested in.
    #[must_use]
    pub const fn weight(self) -> FontWeight {
        self.weight
    }

    /// The family glyphs are requested from.
    #[must_use]
    pub const fn family(self) -> FamilyKey {
        self.family
    }

    /// The line-box height this font renders at, in physical pixels.
    #[must_use]
    pub const fn pixel_height(self) -> u32 {
        self.pixel_height
    }

    /// This font's line metrics, fetched from the font service once per
    /// `(family, pixel_height, weight)` and cached in this process
    /// ([`crate::client`]).
    ///
    /// When no transport is installed, or the service refuses the request,
    /// this falls back to the compiled-in console-atlas geometry scaled to
    /// [`pixel_height`](Self::pixel_height) — exactly the scaling the
    /// monospace-only client used before a font service existed. This keeps
    /// `lib/fbcon` and the boot console (which never install a transport)
    /// laying text out correctly with no service running at all, and leaves
    /// a desktop whose font service has died drawing at a sane approximate
    /// size instead of collapsing to zero.
    #[must_use]
    pub fn metrics(self) -> FontMetrics {
        client::metrics(self.family, self.pixel_height, self.weight)
    }

    /// The vertical distance between baselines in pixels.
    #[must_use]
    pub fn line_height(self) -> u32 {
        self.metrics().line_height
    }

    /// The baseline row within the line box (pixel rows below its top).
    #[must_use]
    pub fn baseline(self) -> u32 {
        self.metrics().baseline
    }

    /// The glyph line-box height in pixels (same as
    /// [`pixel_height`](Self::pixel_height)).
    #[must_use]
    pub const fn glyph_height(self) -> u32 {
        self.pixel_height
    }

    /// The advance every glyph of this font shares, or `None` when the
    /// family is proportional.
    #[must_use]
    pub fn monospace_advance(self) -> Option<u32> {
        client::with_client(|client| self.monospace_advance_on(client))
    }

    /// The column width a grid-drawing caller should use: the family's
    /// monospace advance, or — for a proportional family — the advance of
    /// `'0'` (digits are tabular in the shipped faces, so this is a sane
    /// column width even though the family is not truly fixed-pitch).
    #[must_use]
    pub fn cell_width(self) -> u32 {
        match self.monospace_advance() {
            Some(advance) => advance,
            None => self.advance('0'),
        }
    }

    /// The pen advance for one character, in pixels.
    ///
    /// A monospace family advances by its shared cell width times
    /// [`char_width`] (so a wide CJK scalar reserves two cells); a
    /// proportional family advances by the glyph's own reported width,
    /// fetched (and cached) from the font service. A glyph the service
    /// cannot supply (no transport installed, a refused request) advances by
    /// zero rather than composing a guessed width.
    #[must_use]
    pub fn advance(self, ch: char) -> u32 {
        if let Some(cell) = self.monospace_advance() {
            return cell.saturating_mul(u32::from(char_width(ch)));
        }
        client::with_glyph(ch, self.family, self.pixel_height, self.weight, |glyph| {
            glyph.advance
        })
        .unwrap_or(0)
    }

    /// The pixel width of `text` rendered on one line: the sum of each
    /// character's [`advance`](Self::advance).
    ///
    /// Arithmetic saturates, so a pathologically long string reports
    /// [`u32::MAX`] rather than wrapping. A monospace family takes the O(1)
    /// fast path of multiplying by the shared cell width instead of fetching
    /// each character's advance individually.
    ///
    /// A proportional family's per-character walk is memoised, so repainting
    /// text that has not changed measures nothing; a monospace family
    /// multiplies and never consults the memo, because there is no
    /// per-character lookup there to save.
    #[must_use]
    pub fn text_width(self, text: &str) -> u32 {
        client::with_client(|client| self.width_on(client, text))
    }

    /// The longest prefix of `text` whose rendered width fits within `width`
    /// pixels, truncated on a `char` boundary.
    ///
    /// This is the shared truncation every fixed-width text region uses to
    /// keep a label from spilling past its box (the taskbar's clock and task
    /// titles, the file browser's path bar and entry names), so the
    /// fit-to-width arithmetic lives in one place rather than being repeated
    /// per consumer. A `width` too small for even one glyph yields the empty
    /// string; a `text` that already fits is returned whole. A proportional
    /// family walks real per-glyph advances rather than a column count, so
    /// truncation respects each glyph's own width.
    #[must_use]
    pub fn truncate_to_width(self, text: &str, width: u32) -> &str {
        let end = client::with_client(|client| self.fitting_bytes_on(client, text, width));
        &text[..end]
    }

    /// The longest prefix of `text` that fits in `width` pixels **once room
    /// for [`ELLIPSIS`] is reserved**, and whether that mark is needed.
    ///
    /// `(text, false)` when the whole string already fits — draw it and
    /// nothing else. Otherwise `(prefix, true)`: draw the prefix, then the
    /// mark at the pen [`draw_text`](Self::draw_text) hands back.
    ///
    /// When the mark alone is wider than `width` the answer is `("", false)`:
    /// draw nothing. A mark that spills out of the very box it exists to keep
    /// text inside is worse than an empty box, and the pair is a drawing
    /// instruction rather than a report about the input, so the flag says
    /// "do not draw the mark" instead of leaving a caller to second-guess it.
    ///
    /// The prefix is cut on a `char` boundary by the shared
    /// [`truncate_to_width`](Self::truncate_to_width); this adds the
    /// ellipsis policy and no per-glyph walk of its own. It is the *longest*
    /// such prefix, trailing space included, so a caller that would rather
    /// not leave a gap before the mark trims it — as
    /// [`wrap_to_width`](Self::wrap_to_width) does.
    #[must_use]
    pub fn elide_to_width(self, text: &str, width: u32) -> (&str, bool) {
        let (end, elided) = client::with_client(|client| self.elision_on(client, text, width));
        (&text[..end], elided)
    }

    /// [`monospace_advance`](Self::monospace_advance) against a client the
    /// caller already holds.
    pub(crate) fn monospace_advance_on(self, client: &mut GlyphClient) -> Option<u32> {
        let advance = client
            .metrics(self.family, self.pixel_height, self.weight)
            .monospace_advance;
        (advance != 0).then_some(advance)
    }

    /// [`text_width`](Self::text_width) against a client the caller already
    /// holds.
    pub(crate) fn width_on(self, client: &mut GlyphClient, text: &str) -> u32 {
        if let Some(cell) = self.monospace_advance_on(client) {
            return text.chars().fold(0, |width, ch| {
                width.saturating_add(cell.saturating_mul(u32::from(char_width(ch))))
            });
        }
        client.with_measurement(
            text,
            self.family,
            self.pixel_height,
            self.weight,
            MeasuredText::width,
        )
    }

    /// The byte length of [`truncate_to_width`](Self::truncate_to_width)'s
    /// answer, against a client the caller already holds.
    ///
    /// Both branches cut on a `char` boundary: the monospace one through the
    /// shared column truncation, the proportional one at the boundary after
    /// the last character the memo says fits.
    pub(crate) fn fitting_bytes_on(
        self,
        client: &mut GlyphClient,
        text: &str,
        width: u32,
    ) -> usize {
        if let Some(cell) = self.monospace_advance_on(client) {
            return tairix_vt::truncate_to_width(text, (width / cell.max(1)) as usize).len();
        }
        let fitting = client.with_measurement(
            text,
            self.family,
            self.pixel_height,
            self.weight,
            |measured| measured.chars_within(width),
        );
        text.char_indices()
            .nth(fitting)
            .map_or(text.len(), |(offset, _)| offset)
    }

    /// [`elide_to_width`](Self::elide_to_width) against a client the caller
    /// already holds, as a byte length and the flag.
    pub(crate) fn elision_on(
        self,
        client: &mut GlyphClient,
        text: &str,
        width: u32,
    ) -> (usize, bool) {
        if self.fitting_bytes_on(client, text, width) == text.len() {
            return (text.len(), false);
        }
        let Some(room) = width.checked_sub(self.width_on(client, ELLIPSIS)) else {
            return (0, false);
        };
        (self.fitting_bytes_on(client, text, room), true)
    }

    /// Lay `text` out over at most `max_lines` lines of `width` pixels,
    /// yielding one [`TextLine`] per line.
    ///
    /// This is the shared label fitter every text region too narrow for its
    /// label uses — an account tile's display name, a desktop icon's caption
    /// — so no consumer writes its own break loop. The iterator is lazy and
    /// its lines borrow `text`, so a caller counts a `clone` of it to place
    /// the block vertically and then walks it to draw, allocating nothing.
    ///
    /// A line breaks at whitespace wherever one is available, so a word
    /// starts the next line rather than being split; a word too long for
    /// `width` on its own is broken mid-word on a `char` boundary, since the
    /// alternatives are a line that overflows and a line that never
    /// advances. Whitespace a break consumes is not drawn, and none of it is
    /// a *forced* break: a newline is a break opportunity like any other
    /// space.
    ///
    /// The last permitted line carries everything left, elided through
    /// [`elide_to_width`](Self::elide_to_width) when that does not fit — and
    /// trimmed, so no gap opens between it and the mark. A `max_lines` of
    /// `0`, a blank `text`, and a `width` too narrow for even one glyph all
    /// yield nothing at all.
    ///
    /// To draw a line centred in a `box_width`: measure
    /// [`text_width`](Self::text_width) of its text plus, when it is
    /// `elided`, `text_width(ELLIPSIS)`; draw the text; and draw
    /// [`ELLIPSIS`] at the pen [`draw_text`](Self::draw_text) returned.
    #[must_use]
    pub fn wrap_to_width(self, text: &str, width: u32, max_lines: usize) -> TextWrap<'_> {
        TextWrap {
            font: self,
            rest: text,
            width,
            remaining: max_lines,
        }
    }

    /// Draw `text` onto `surface` with its pen starting at `(x, y)` in
    /// `color`, returning the pen x-coordinate after the last glyph.
    ///
    /// The pen advances by each character's own [`advance`](Self::advance).
    /// Each glyph's coverage is fetched from the font service (cached
    /// client-side) at this font's family, pixel height, and weight, and
    /// composited over the destination at its anti-aliased coverage, offset
    /// from the pen by its own left side bearing — so anti-aliased edges,
    /// translucent text, and a proportional family's varying bearings all
    /// come out right. Pixels that fall outside the surface (including at
    /// negative coordinates) are skipped, so off-screen text clips rather
    /// than panicking. A scalar the faces do not cover draws the U+FFFD
    /// replacement glyph (the service's fallback) rather than being silently
    /// dropped; if the service is unreachable the glyph composites nothing
    /// (fail closed) rather than reaching for any local font data.
    pub fn draw_text(self, surface: &mut Surface, x: i32, y: i32, text: &str, color: Color) -> i32 {
        let sources = coverage_sources(color);
        let mut pen = x;
        for ch in text.chars() {
            let advance = self.advance(ch);
            client::with_glyph(ch, self.family, self.pixel_height, self.weight, |glyph| {
                let origin_x = pen.saturating_add(glyph.left);
                draw_coverage_glyph(surface, origin_x, y, glyph, glyph.width, &sources);
            });
            pen = pen.saturating_add(advance_step(advance));
        }
        pen
    }
}

/// One laid-out line of a wrapped label: the text to draw, and whether
/// [`ELLIPSIS`] follows it because the label ran out of lines.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TextLine<'a> {
    /// The line's text, already free of the whitespace a break consumed.
    pub text: &'a str,
    /// Whether [`ELLIPSIS`] is drawn after [`text`](Self::text).
    pub elided: bool,
}

/// The lazy iterator [`BitmapFont::wrap_to_width`] returns.
///
/// It holds the font, the unconsumed tail of the label, and the line budget
/// — nothing heap-allocated — so a caller counts a `clone` of it to size the
/// block and then walks the original to draw, for the cost of measuring
/// twice and no allocation at all. It is deliberately not `Copy`: a `for`
/// loop over one would silently duplicate rather than consume it.
#[derive(Clone, Debug)]
pub struct TextWrap<'a> {
    font: BitmapFont,
    rest: &'a str,
    width: u32,
    remaining: usize,
}

impl TextWrap<'_> {
    /// Yield nothing further: the line budget is spent, or the tail cannot
    /// advance.
    fn finish(&mut self) {
        self.rest = "";
        self.remaining = 0;
    }
}

impl<'a> Iterator for TextWrap<'a> {
    type Item = TextLine<'a>;

    fn next(&mut self) -> Option<TextLine<'a>> {
        // Leading whitespace belongs to the break that ended the previous
        // line, and the tail's trailing whitespace is drawn on no line at
        // all, so neither is measured against the width.
        let rest = self.rest.trim();
        if self.remaining == 0 || rest.is_empty() {
            self.finish();
            return None;
        }
        if self.remaining == 1 {
            let (text, elided) = self.font.elide_to_width(rest, self.width);
            // A cut can land inside a run of spaces, and a gap before the
            // mark would read as part of the missing text.
            let text = text.trim_end();
            self.finish();
            return (!text.is_empty() || elided).then_some(TextLine { text, elided });
        }
        let head = self.font.truncate_to_width(rest, self.width);
        if head.len() == rest.len() {
            self.finish();
            return Some(TextLine {
                text: rest,
                elided: false,
            });
        }
        let Some(split) = line_break(rest, head) else {
            self.finish();
            return None;
        };
        self.rest = &rest[split..];
        self.remaining -= 1;
        Some(TextLine {
            text: rest[..split].trim_end(),
            elided: false,
        })
    }
}

/// Where to break `rest`, given `head`: the longest prefix of it that fits
/// the line, already known to be shorter than `rest` itself.
///
/// The break is the last whitespace inside `head`, so a word that would
/// otherwise be split starts the next line instead. A run with no
/// whitespace in it breaks where it stopped fitting — a `char` boundary,
/// because that is what `head` ends on. `None` when not one glyph fits: no
/// break could then make progress, and a line that consumes nothing would
/// never end.
///
/// The offset is always at least one byte: `rest` is trimmed, so its first
/// character is not whitespace and no break can land at zero.
fn line_break(rest: &str, head: &str) -> Option<usize> {
    if head.is_empty() {
        return None;
    }
    if rest[head.len()..].starts_with(char::is_whitespace) {
        return Some(head.len());
    }
    Some(head.rfind(char::is_whitespace).unwrap_or(head.len()))
}

/// Clamp a requested pixel height into
/// [`BitmapFont::MIN_PIXEL_HEIGHT`]..=[`BitmapFont::MAX_PIXEL_HEIGHT`].
const fn clamp_pixel_height(pixels: u32) -> u32 {
    if pixels < BitmapFont::MIN_PIXEL_HEIGHT {
        BitmapFont::MIN_PIXEL_HEIGHT
    } else if pixels > BitmapFont::MAX_PIXEL_HEIGHT {
        BitmapFont::MAX_PIXEL_HEIGHT
    } else {
        pixels
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
            advance: width,
            left: 0,
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
