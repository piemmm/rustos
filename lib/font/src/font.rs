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
use tairix_geometry::{to_i32, Scale};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Fonts, TextRole};
use tairix_vt::char_width;

use crate::atlas;
use crate::client::{self, FontClient};
use crate::measure::MeasuredText;

/// The mark that ends a line the text outgrew: HORIZONTAL ELLIPSIS.
///
/// One definition serves both halves of the job —
/// [`BitmapFont::text_width`] reserves room for it and
/// [`BitmapFont::draw_text`] paints it — so the mark measured and the mark
/// drawn can never disagree. It is a `&str` for exactly that reason: a
/// `char` would have to be encoded at every call site.
pub const ELLIPSIS: &str = "\u{2026}";

/// A shadow drawn behind a text run, for text that sits on ground the drawer
/// does not control — a label over a wallpaper, a caption over a photograph.
///
/// One definition of "the run once in the shadow colour, then once in the
/// ink", so every surface that needs legible text over a picture gets the
/// same offset and the same order rather than deriving either for itself.
/// The offset is [`Self::new`]'s alone.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TextShadow {
    color: Color,
    offset: u32,
}

impl TextShadow {
    /// A shadow in `color`, offset down and right by one logical pixel at
    /// `scale`.
    ///
    /// Never less than one physical pixel: a fraction of a pixel rounds to
    /// nothing, and a shadow that vanishes at some densities is worse than
    /// none at all.
    #[must_use]
    pub fn new(color: Color, scale: Scale) -> Self {
        Self {
            color,
            offset: scale.scale_length(1).max(1),
        }
    }

    /// The colour the run is drawn in behind the ink.
    #[must_use]
    pub const fn color(self) -> Color {
        self.color
    }

    /// How far down and right of the ink the shadow sits, in physical pixels.
    #[must_use]
    pub const fn offset(self) -> u32 {
        self.offset
    }
}

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
            weight: FontWeight::REGULAR,
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
            weight: FontWeight::REGULAR,
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
        client::with_client(|client| match self.monospace_advance_on(client) {
            Some(advance) => advance,
            None => self.glyph_advance_on(client, '0'),
        })
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
        client::with_client(|client| self.advance_on(client, ch))
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

    /// The pen position at the `char` boundary `byte` bytes into `text`.
    ///
    /// This is where a caret sits, and where a selection's highlight starts
    /// and stops. It reads the *whole* string's memoised measurement rather
    /// than measuring the prefix as a string of its own: a caret walking
    /// through a line would otherwise leave one memoised measurement per
    /// position behind it, each one a walk of everything before it.
    ///
    /// A `byte` past the end, or off a `char` boundary, answers for the
    /// boundary at or before it, so a caller that rounded cannot be handed a
    /// position inside a scalar.
    #[must_use]
    pub fn width_to_offset(self, text: &str, byte: usize) -> u32 {
        client::with_client(|client| self.width_to_offset_on(client, text, byte))
    }

    /// The `char` boundary in `text` whose pen position is nearest `x`.
    ///
    /// This is the pointer hit test every text region shares: a click lands
    /// on the boundary it is closest to, so the caret appears where the
    /// pointer pointed rather than always before the character under it. A
    /// click past the end answers the end.
    ///
    /// One binary search over the string's one memoised measurement, so a
    /// click costs a search rather than a measurement per character.
    #[must_use]
    pub fn offset_at_width(self, text: &str, x: u32) -> usize {
        client::with_client(|client| self.offset_at_width_on(client, text, x))
    }

    /// [`width_to_offset`](Self::width_to_offset) against a client the caller
    /// already holds.
    fn width_to_offset_on(self, client: &mut impl FontClient, text: &str, byte: usize) -> u32 {
        let mut end = byte.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let head = &text[..end];
        if self.monospace_advance_on(client).is_some() {
            return self.width_on(client, head);
        }
        let chars = head.chars().count();
        client.with_measurement(
            text,
            self.family,
            self.pixel_height,
            self.weight,
            |measured| measured.pen_at(chars),
        )
    }

    /// [`offset_at_width`](Self::offset_at_width) against a client the caller
    /// already holds.
    fn offset_at_width_on(self, client: &mut impl FontClient, text: &str, x: u32) -> usize {
        // The last boundary at or before `x`, and the one after it: the pen
        // never decreases, so the nearest boundary is one of those two.
        let before = self.fitting_end_on(client, text, Cursor::START, x);
        if before >= text.len() {
            return text.len();
        }
        let after = text[before..]
            .chars()
            .next()
            .map_or(before, |ch| before + ch.len_utf8());
        let low = self.width_to_offset_on(client, text, before);
        let high = self.width_to_offset_on(client, text, after);
        // A tie takes the later boundary, so a click on the exact midpoint of
        // a glyph lands after it rather than before.
        if high.saturating_sub(x) <= x.saturating_sub(low) {
            after
        } else {
            before
        }
    }

    /// The pen advance a fixed-pitch face gives `ch`: its shared `cell` once
    /// per terminal column the scalar reserves.
    fn cell_step(cell: u32, ch: char) -> u32 {
        cell.saturating_mul(u32::from(char_width(ch)))
    }

    /// [`advance`](Self::advance) against a client the caller already holds.
    pub(crate) fn advance_on(self, client: &mut impl FontClient, ch: char) -> u32 {
        match self.monospace_advance_on(client) {
            Some(cell) => Self::cell_step(cell, ch),
            None => self.glyph_advance_on(client, ch),
        }
    }

    /// The advance the face's own glyph for `ch` reports, or zero when the
    /// service cannot supply it (fail closed, never a guessed width).
    fn glyph_advance_on(self, client: &mut impl FontClient, ch: char) -> u32 {
        client
            .with_glyph(ch, self.family, self.pixel_height, self.weight, |glyph| {
                glyph.advance
            })
            .unwrap_or(0)
    }

    /// [`monospace_advance`](Self::monospace_advance) against a client the
    /// caller already holds.
    pub(crate) fn monospace_advance_on(self, client: &mut impl FontClient) -> Option<u32> {
        let advance = client
            .metrics(self.family, self.pixel_height, self.weight)
            .monospace_advance;
        (advance != 0).then_some(advance)
    }

    /// [`text_width`](Self::text_width) against a client the caller already
    /// holds.
    pub(crate) fn width_on(self, client: &mut impl FontClient, text: &str) -> u32 {
        if let Some(cell) = self.monospace_advance_on(client) {
            return text.chars().fold(0, |width, ch| {
                width.saturating_add(Self::cell_step(cell, ch))
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
    pub(crate) fn fitting_bytes_on(
        self,
        client: &mut impl FontClient,
        text: &str,
        width: u32,
    ) -> usize {
        self.fitting_end_on(client, text, Cursor::START, width)
    }

    /// Where the longest run of `text` that starts at `from` and fits
    /// `width` ends, as a byte offset into the whole of `text`.
    ///
    /// Both branches cut on a `char` boundary: the monospace one through the
    /// shared column truncation, the proportional one at the boundary after
    /// the last character the memo says fits. The proportional branch
    /// measures the **whole** string and asks the memo for a suffix's fit,
    /// so laying a paragraph out line by line costs one measurement rather
    /// than one per line — and the walk from `from` to the answer is the
    /// same walk the next line's cursor continues from, keeping a whole
    /// wrap linear in the text.
    pub(crate) fn fitting_end_on(
        self,
        client: &mut impl FontClient,
        text: &str,
        from: Cursor,
        width: u32,
    ) -> usize {
        let Some(rest) = text.get(from.byte..) else {
            return text.len();
        };
        if let Some(cell) = self.monospace_advance_on(client) {
            let columns = (width / cell.max(1)) as usize;
            return from
                .byte
                .saturating_add(tairix_vt::truncate_to_width(rest, columns).len());
        }
        let fitting = client.with_measurement(
            text,
            self.family,
            self.pixel_height,
            self.weight,
            |measured| measured.chars_within_from(from.chars, width),
        );
        rest.char_indices()
            .nth(fitting)
            .map_or(text.len(), |(offset, _)| from.byte.saturating_add(offset))
    }

    /// [`elide_to_width`](Self::elide_to_width) against a client the caller
    /// already holds, as a byte length and the flag.
    pub(crate) fn elision_on(
        self,
        client: &mut impl FontClient,
        text: &str,
        width: u32,
    ) -> (usize, bool) {
        self.elide_run_on(client, text, Cursor::START, text.len(), false, width)
    }

    /// Fit `text[from.byte..stop]` into `width`, reserving room for
    /// [`ELLIPSIS`] and reporting it whenever anything at all is dropped:
    /// text of the run that did not fit, or — when the caller says there is
    /// `more` past `stop` — that.
    ///
    /// The one elision policy, shared by the single-line fitter and by a
    /// wrap's last line, so a cut label and a cut paragraph mark what they
    /// dropped identically. What lies beyond `stop` is the caller's question
    /// rather than this one's: a wrap's `stop` is the end of the paragraph it
    /// is laying out, which may or may not be the end of the text.
    fn elide_run_on(
        self,
        client: &mut impl FontClient,
        text: &str,
        from: Cursor,
        stop: usize,
        more: bool,
        width: u32,
    ) -> (usize, bool) {
        let fitted = self.fitting_end_on(client, text, from, width).min(stop);
        if fitted == stop && !more {
            return (fitted, false);
        }
        let Some(room) = width.checked_sub(self.width_on(client, ELLIPSIS)) else {
            return (from.byte, false);
        };
        (
            self.fitting_end_on(client, text, from, room).min(stop),
            true,
        )
    }

    /// Lay `text` out over at most `max_lines` lines of `width` pixels,
    /// yielding one [`TextLine`] per line **to draw**.
    ///
    /// This is the shared fitter every text region too narrow for its text
    /// uses — a desktop icon's caption, a dialog's message, a notification's
    /// body — so no consumer writes its own break loop. The iterator is lazy
    /// and its lines borrow `text`, so a caller counts a `clone` of it to
    /// place the block vertically and then walks it to draw, allocating
    /// nothing.
    ///
    /// A line breaks at whitespace wherever one is available, so a word
    /// starts the next line rather than being split; a word too long for
    /// `width` on its own is broken mid-word on a `char` boundary, since the
    /// alternatives are a line that overflows and a line that never
    /// advances. A **newline is a forced break**: a paragraph ends where its
    /// author ended it, and a blank line between two of them is drawn as a
    /// blank line rather than closed up.
    ///
    /// Every line is trimmed, so no whitespace a break consumed is drawn and
    /// no gap opens before an elision mark. Leading and trailing whitespace
    /// of the whole text is likewise not drawn and costs no line, so a text
    /// ending in a newline does not end in a blank line and a text of
    /// nothing but whitespace yields nothing at all.
    ///
    /// The last permitted line carries what is left of its own paragraph,
    /// elided when anything at all is dropped — the rest of the line, or the
    /// paragraphs after it. A `max_lines` of `0`, a blank `text`, and a
    /// `width` too narrow for even one glyph all yield nothing at all.
    ///
    /// To draw a line centred in a `box_width`: measure
    /// [`text_width`](Self::text_width) of its text plus, when it is
    /// `elided`, `text_width(ELLIPSIS)`; draw the text; and draw
    /// [`ELLIPSIS`] at the pen [`draw_text`](Self::draw_text) returned.
    #[must_use]
    pub fn wrap_to_width(self, text: &str, width: u32, max_lines: usize) -> TextWrap<'_> {
        let content = text.trim();
        let leading = &text[..text.len() - text.trim_start().len()];
        TextWrap {
            lines: TextLines {
                font: self,
                text,
                width,
                at: Cursor::START.over(leading),
                limit: leading.len().saturating_add(content.len()),
                trailing: false,
                done: false,
            },
            remaining: if content.is_empty() { 0 } else { max_lines },
        }
    }

    /// Lay `text` out over lines of `width` pixels that **tile** it: every
    /// byte belongs to exactly one line, in order, and one further empty line
    /// holds the position after a final newline.
    ///
    /// This is the editing counterpart of
    /// [`wrap_to_width`](Self::wrap_to_width) and shares its break rules —
    /// whitespace where there is one, mid-word where there is not, forced at
    /// a newline. What it does *not* share is the display policy: nothing is
    /// trimmed, nothing is elided, and no line is suppressed, because a caret
    /// has to be able to sit on every position of the buffer including the
    /// spaces at a wrap point and the empty line after a trailing newline.
    /// A line therefore runs from its first byte to the first byte of the
    /// next, trailing whitespace and the newline that ended it included; a
    /// caller draws [`str::trim_end`] of it and maps a caret through
    /// [`TextLine::start`].
    ///
    /// An empty `text` is one empty line, not none: the caret still has a
    /// home. A `width` too narrow for a character takes one anyway, so the
    /// text stays covered rather than disappearing — the line overflows, and
    /// the caller clips.
    ///
    /// The iterator is lazy and allocates nothing, so drawing a viewport's
    /// worth of a long buffer costs only the lines up to the last one drawn.
    #[must_use]
    pub fn lines_to_width(self, text: &str, width: u32) -> TextLines<'_> {
        TextLines {
            font: self,
            text,
            width,
            at: Cursor::START,
            limit: text.len(),
            trailing: text.is_empty(),
            done: false,
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
    ///
    /// A run costs one glyph lookup per character. Whether the face is
    /// fixed-pitch is a property of the face, so it is resolved once for the
    /// whole run, and a proportional glyph's advance is read from the very
    /// coverage the blit is about to composite rather than fetched a second
    /// time for it.
    ///
    /// The two runs are written out rather than sharing one glyph-blitting
    /// call: a fixed-pitch run must not pay for an advance it discards, and
    /// sharing the call gives both a closure that returns one. Measured over
    /// a 74-character row, sharing it costs either face around 7%.
    pub fn draw_text(self, surface: &mut Surface, x: i32, y: i32, text: &str, color: Color) -> i32 {
        client::with_client(|client| self.draw_on(client, surface, x, y, text, color))
    }

    /// Draw `text` as [`draw_text`](Self::draw_text) does, `shadow` behind
    /// it, returning the very same pen.
    ///
    /// The run is drawn twice — once displaced by the shadow's offset in its
    /// colour, then in `color` — so text keeps its separation over ground the
    /// drawer does not control: an account name over a photograph, a caption
    /// over a wallpaper. Only the ink decides the pen, so a caller advancing
    /// a run lays it out identically with the shadow on or off.
    ///
    /// Both passes share one client, so the second costs no lock of its own
    /// and every glyph it composites is already cached by the first.
    pub fn draw_text_shadowed(
        self,
        surface: &mut Surface,
        x: i32,
        y: i32,
        text: &str,
        color: Color,
        shadow: TextShadow,
    ) -> i32 {
        let offset = to_i32(shadow.offset());
        client::with_client(|client| {
            self.draw_on(
                client,
                surface,
                x.saturating_add(offset),
                y.saturating_add(offset),
                text,
                shadow.color(),
            );
            self.draw_on(client, surface, x, y, text, color)
        })
    }

    /// [`draw_text`](Self::draw_text) against a client the caller already
    /// holds.
    pub(crate) fn draw_on(
        self,
        client: &mut impl FontClient,
        surface: &mut Surface,
        x: i32,
        y: i32,
        text: &str,
        color: Color,
    ) -> i32 {
        let sources = coverage_sources(color);
        let mut pen = x;
        client.warm(text, self.family, self.pixel_height, self.weight);
        match self.monospace_advance_on(client) {
            Some(cell) => {
                for ch in text.chars() {
                    client.with_glyph(ch, self.family, self.pixel_height, self.weight, |glyph| {
                        let origin_x = pen.saturating_add(glyph.left);
                        draw_coverage_glyph(surface, origin_x, y, glyph, glyph.width, &sources);
                    });
                    pen = pen.saturating_add(advance_step(Self::cell_step(cell, ch)));
                }
            }
            None => {
                for ch in text.chars() {
                    let advance = client
                        .with_glyph(ch, self.family, self.pixel_height, self.weight, |glyph| {
                            let origin_x = pen.saturating_add(glyph.left);
                            draw_coverage_glyph(surface, origin_x, y, glyph, glyph.width, &sources);
                            glyph.advance
                        })
                        .unwrap_or(0);
                    pen = pen.saturating_add(advance_step(advance));
                }
            }
        }
        pen
    }
}

/// One laid-out line of wrapped text.
///
/// [`wrap_to_width`](BitmapFont::wrap_to_width) yields lines to **draw**:
/// trimmed, and the last one marked when it dropped something.
/// [`lines_to_width`](BitmapFont::lines_to_width) yields lines to **edit**:
/// untrimmed and tiling the text, so [`start`](Self::start) and the line's
/// own length locate it exactly within the buffer it came from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TextLine<'a> {
    /// The line's text.
    pub text: &'a str,
    /// The byte offset of [`text`](Self::text) within the laid-out text.
    pub start: usize,
    /// Whether [`ELLIPSIS`] is drawn after [`text`](Self::text).
    pub elided: bool,
}

impl TextLine<'_> {
    /// The byte range of the laid-out text this line covers.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.start..self.end()
    }

    /// The byte offset just past this line.
    #[must_use]
    pub fn end(&self) -> usize {
        self.start.saturating_add(self.text.len())
    }
}

/// What ended one laid-out line.
///
/// Deliberately not public: every one of these is an internal layout
/// decision, and a consumer's needs — where a line sits, what to draw, where
/// a caret goes — are answered by [`TextLine`] alone.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LineBreak {
    /// A newline ended the line and belongs to it.
    Hard,
    /// Whitespace at the wrap point ended the line and belongs to it.
    Soft,
    /// No break point fitted, so the line ends inside a word.
    Word,
    /// Not even the line's first character fitted the width, and was taken
    /// regardless so the text stays covered.
    Overflow,
    /// The text ended.
    End,
}

/// A position in a laid-out text, held as both coordinates a layout needs:
/// the byte offset that slices the text and the `char` index that indexes its
/// measurement.
///
/// Carrying both is what keeps a wrap linear in the text. Deriving either
/// from the other costs a walk from the start, so a line-by-line layout that
/// kept only one would pay that walk again for every line.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Cursor {
    pub(crate) byte: usize,
    pub(crate) chars: usize,
}

impl Cursor {
    /// The start of a text.
    pub(crate) const START: Self = Self { byte: 0, chars: 0 };

    /// This cursor advanced over `run`, which begins at it.
    fn over(self, run: &str) -> Self {
        Self {
            byte: self.byte.saturating_add(run.len()),
            chars: self.chars.saturating_add(run.chars().count()),
        }
    }
}

/// The lazy iterator [`BitmapFont::lines_to_width`] returns.
#[derive(Clone, Debug)]
pub struct TextLines<'a> {
    font: BitmapFont,
    text: &'a str,
    width: u32,
    at: Cursor,
    /// The byte past the last one laid out. A wrap for drawing stops at the
    /// end of the text's own content, so its lines still report where they
    /// sit in the string the caller handed over rather than in a trimmed
    /// copy of it.
    limit: usize,
    /// Whether one further, empty line remains — the position after a final
    /// newline, or the sole position of an empty text. A caret lives there,
    /// so a layout that left it out could not place one.
    trailing: bool,
    done: bool,
}

impl<'a> TextLines<'a> {
    /// The text being laid out.
    #[must_use]
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Yield the line the cursor is on, and what ended it.
    fn advance(&mut self) -> Option<(TextLine<'a>, LineBreak)> {
        if self.done {
            return None;
        }
        if self.at.byte >= self.limit {
            self.done = true;
            let trailing = self.trailing;
            self.trailing = false;
            return trailing.then_some((
                TextLine {
                    text: "",
                    start: self.limit,
                    elided: false,
                },
                LineBreak::End,
            ));
        }
        let at = self.at;
        let (end, ended_by) = client::with_client(|client| self.line_end_on(client, at));
        let text = self.text.get(at.byte..end).unwrap_or_default();
        self.at = at.over(text);
        // A newline at the very end leaves a position no line covers.
        self.trailing = ended_by == LineBreak::Hard && self.at.byte >= self.limit;
        Some((
            TextLine {
                text,
                start: at.byte,
                elided: false,
            },
            ended_by,
        ))
    }

    /// Where the line starting at `at` ends, and what ended it.
    ///
    /// The answer is the byte the *next* line starts at, so the lines tile
    /// the text: the whitespace a soft break consumes and the newline a hard
    /// break consumes both belong to the line they ended.
    fn line_end_on(&self, client: &mut impl FontClient, at: Cursor) -> (usize, LineBreak) {
        let end = self
            .font
            .fitting_end_on(client, self.text, at, self.width)
            .min(self.limit);
        let span = self.text.get(at.byte..end).unwrap_or_default();
        // A newline inside the run, or one standing exactly where the run
        // stopped fitting — it draws nothing, so its own advance must not be
        // what pushes it onto the next line.
        if let Some(offset) = span.find('\n') {
            return (
                at.byte.saturating_add(offset).saturating_add(1),
                LineBreak::Hard,
            );
        }
        if end >= self.limit {
            return (self.limit, LineBreak::End);
        }
        if self.text.as_bytes().get(end) == Some(&b'\n') {
            return (end.saturating_add(1), LineBreak::Hard);
        }
        // Break on the whitespace nearest the line's end: the run the text
        // stopped fitting inside, else the last one within it.
        let broken = if self.text[end..].starts_with(char::is_whitespace) {
            Some(end)
        } else {
            span.rfind(char::is_whitespace).map(|o| at.byte + o)
        };
        match broken {
            Some(from) => self.consume_break(from),
            None if end > at.byte => (end, LineBreak::Word),
            // Not one character fits. Take it regardless: a line that
            // consumes nothing would never end, and dropping the character
            // would drop the rest of the text with it.
            None => (
                self.text[at.byte..]
                    .chars()
                    .next()
                    .map_or(self.limit, |ch| at.byte + ch.len_utf8()),
                LineBreak::Overflow,
            ),
        }
    }

    /// Where the line whose break falls at `from` ends, and what ended it:
    /// the whitespace run starting there belongs to it, and a newline within
    /// that run ends it outright.
    ///
    /// Consuming the run is what keeps the lines tiling the text while
    /// drawing no whitespace at a wrap point, and stopping at a newline is
    /// what keeps a blank line between two paragraphs from being eaten by
    /// the spaces before it.
    fn consume_break(&self, from: usize) -> (usize, LineBreak) {
        let mut end = from;
        for ch in self.text[from..self.limit].chars() {
            if ch == '\n' {
                return (end.saturating_add(1), LineBreak::Hard);
            }
            if !ch.is_whitespace() {
                break;
            }
            end = end.saturating_add(ch.len_utf8());
        }
        (end, LineBreak::Soft)
    }
}

impl<'a> Iterator for TextLines<'a> {
    type Item = TextLine<'a>;

    fn next(&mut self) -> Option<TextLine<'a>> {
        self.advance().map(|(line, _)| line)
    }
}

/// The lazy iterator [`BitmapFont::wrap_to_width`] returns.
///
/// It holds the line cursor and the line budget — nothing heap-allocated —
/// so a caller counts a `clone` of it to size the block and then walks the
/// original to draw, for the cost of measuring twice and no allocation at
/// all. It is deliberately not `Copy`: a `for` loop over one would silently
/// duplicate rather than consume it.
#[derive(Clone, Debug)]
pub struct TextWrap<'a> {
    lines: TextLines<'a>,
    remaining: usize,
}

impl<'a> TextWrap<'a> {
    /// The last permitted line: what is left of the paragraph the cursor is
    /// in, elided when anything at all is dropped.
    ///
    /// It stops at a newline rather than running the paragraphs together,
    /// because a forced break is where its author ended the sentence — and
    /// because a newline drawn as a glyph is a defect in the making.
    fn last_line(&mut self) -> Option<TextLine<'a>> {
        let lines = &mut self.lines;
        let at = lines.at;
        if at.byte >= lines.limit {
            return None;
        }
        let stop = lines.text[at.byte..lines.limit]
            .find('\n')
            .map_or(lines.limit, |offset| at.byte + offset);
        // Whatever follows this paragraph is dropped along with it, so the
        // mark is owed even where the paragraph itself fits.
        let more = stop < lines.limit;
        let (end, elided) = client::with_client(|client| {
            lines
                .font
                .elide_run_on(client, lines.text, at, stop, more, lines.width)
        });
        let run = lines.text.get(at.byte..end).unwrap_or_default();
        let text = run.trim();
        let start = at.byte + (run.len() - run.trim_start().len());
        (!text.is_empty() || elided).then_some(TextLine {
            text,
            start,
            elided,
        })
    }
}

impl<'a> Iterator for TextWrap<'a> {
    type Item = TextLine<'a>;

    fn next(&mut self) -> Option<TextLine<'a>> {
        if self.remaining == 0 {
            return None;
        }
        if self.remaining == 1 {
            self.remaining = 0;
            return self.last_line();
        }
        let (line, ended_by) = self.lines.advance()?;
        // A box too narrow for a character draws none of the text rather
        // than a column of overflowing glyphs.
        if ended_by == LineBreak::Overflow {
            self.remaining = 0;
            return None;
        }
        self.remaining -= 1;
        let trimmed = line.text.trim();
        Some(TextLine {
            text: trimmed,
            start: line.start + (line.text.len() - line.text.trim_start().len()),
            elided: false,
        })
    }
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
    use tairix_reclaim::PressureBand;

    use super::{advance_step, coverage_sources, draw_coverage_glyph, BitmapFont};
    use crate::client::tests::LocalClient;
    use crate::client::tests::{caching_client, glyph_lookups, INTER};
    use crate::client::FontClient;
    use crate::glyph_cache::{glyph_cache_budget, CachedGlyph};

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

    /// The colour a label is drawn in.
    fn ink() -> Color {
        Color::rgba(230, 232, 238, 255)
    }

    /// The drawing loop as it stood before a glyph's own coverage carried its
    /// advance: one lookup to ask how far the pen moves, a second to blit it.
    /// The yardstick that proves the surviving single lookup draws the very
    /// same pixels and leaves the pen in the very same place; it lives only
    /// here, so production keeps one definition of the run.
    fn reference_draw(
        client: &mut impl FontClient,
        font: BitmapFont,
        surface: &mut Surface,
        x: i32,
        y: i32,
        text: &str,
    ) -> i32 {
        let sources = coverage_sources(ink());
        let mut pen = x;
        for ch in text.chars() {
            let advance = font.advance_on(client, ch);
            client.with_glyph(ch, font.family, font.pixel_height, font.weight, |glyph| {
                let origin_x = pen.saturating_add(glyph.left);
                draw_coverage_glyph(surface, origin_x, y, glyph, glyph.width, &sources);
            });
            pen = pen.saturating_add(advance_step(advance));
        }
        pen
    }

    /// A client whose glyph cache is installed and already holds `text`, so a
    /// lookup count is what the *run* costs rather than its first fetches.
    fn warm_client(font: BitmapFont, text: &str) -> LocalClient {
        let (mut client, _gauge) =
            caching_client(PressureBand::Normal, glyph_cache_budget(1 << 30));
        let mut scratch = Surface::new(4, 4).expect("allocates");
        font.draw_on(&mut client, &mut scratch, 0, 0, text, ink());
        client
    }

    /// The faces a desktop draws with: one proportional, one fixed-pitch.
    fn faces() -> [BitmapFont; 2] {
        [BitmapFont::new(INTER, 20), BitmapFont::monospace(20)]
    }

    /// A drawn run pays exactly one glyph lookup per character: the advance
    /// the pen needs is carried by the coverage the blit is already holding,
    /// so asking the cache for it again is pure waste.
    #[test]
    fn a_drawn_run_pays_one_glyph_lookup_per_character() {
        let text = "Documents";
        let chars = u64::try_from(text.chars().count()).expect("a test string");
        for font in faces() {
            let mut client = warm_client(font, text);
            let before = glyph_lookups(&client);
            let mut surface = patterned_surface(240, 32);
            font.draw_on(&mut client, &mut surface, 4, 6, text, ink());
            assert_eq!(
                glyph_lookups(&client) - before,
                chars,
                "a {chars}-character run cost more than one lookup each"
            );
        }
    }

    /// Reading the advance off the blitted coverage draws the same frame, to
    /// the pixel, and leaves the pen where fetching it separately did.
    #[test]
    fn a_drawn_run_matches_the_two_lookup_reference() {
        for font in faces() {
            // The last reserves two cells per scalar on a fixed-pitch face.
            for text in [
                "",
                "A",
                "Documents",
                "Attaché — Übung",
                "  spaced  ",
                "日本語",
            ] {
                // On the surface, straddling its left edge, and past its right.
                for &(x, y) in &[(4i32, 6i32), (-7, 3), (230, 6)] {
                    let mut client = warm_client(font, text);
                    let mut actual = patterned_surface(240, 32);
                    let mut expected = actual.clone();
                    let pen = font.draw_on(&mut client, &mut actual, x, y, text, ink());
                    let want = reference_draw(&mut client, font, &mut expected, x, y, text);
                    assert_eq!(pen, want, "the pen ended elsewhere: {text:?} at ({x},{y})");
                    assert_eq!(actual.pixels(), expected.pixels(), "{text:?} at ({x},{y})");
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
