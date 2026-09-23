//! The text-entry family: [`TextField`] and [`SearchField`] (spec §11.8).
//!
//! Both are single-line text controls built on a quiet Alloy Plate with a clear
//! focus ring, a caret, selection, and horizontally-scrolled clipped text. A
//! [`TextField`] is the general single-line entry; a [`SearchField`] is the same
//! editor behind a leading magnifier that reads as *active* when a query is
//! present (spec §11.8). Both resolve every colour/metric/radius from the active
//! [`Theme`] and [`Scale`], round their plate through the shared drawing core
//! the button/selector/value families use, and emit a typed [`TextAction`] — the
//! owning service enforces authority.
//!
//! A read-only field is enabled and legible (its text stays full-contrast and
//! selectable for copy) but refuses edits; that is deliberately distinct from a
//! disabled field (muted plate and text) and from an authority-denied field
//! (which keeps its value and shows an Authority Mark), per spec §13.
//!
//! A [`TextField`] additionally has a secret (masked) mode for credential
//! entry: [`TextField::secret`] bounds the buffer and switches its rendering
//! to one filled bead per `char` in place of the glyph it would otherwise
//! draw, so the drawn width depends only on the buffer's length and never on
//! its content. [`SearchField`] has no such mode — a search query is not a
//! credential.

use alloc::string::String;
use core::fmt;
use core::mem;
use core::ops::Range;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};
use tairix_util::secret::wipe;

use crate::damage;
use crate::paint::{
    ground_fill, line_budget, paint_bead, paint_filled_circle, paint_plate, paint_run,
    plate_border, resolve_bead, resolve_frame, role_font, surface_rect, text_plate_height, to_i32,
    withheld, ChromeLayer, PlateStyle, TextBlock,
};
use crate::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use crate::scrollbar::{ScrollAction, ScrollBar};
use crate::state::{
    ControlDisposition, ControlRole, ControlState, PointerState, RenderInvariant, ValidationState,
};

/// The outcome of feeding input to a text control.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TextAction {
    /// The control's text content changed; the owner reads it with
    /// [`TextField::text`] / [`SearchField::text`] and validates it.
    Edited,
    /// The user requested the field's value be committed (Enter).
    Submitted,
    /// The user dismissed the field (Escape). A search field additionally
    /// clears a non-empty query first, reporting [`TextAction::Edited`] for
    /// that clear and [`TextAction::Cancelled`] only when already empty.
    Cancelled,
}

/// The widest a single UTF-8-encoded `char` can ever be, in bytes.
const MAX_UTF8_LEN: usize = 4;

/// Overwrite `range` of `text`'s bytes with zero, in place, without changing
/// the buffer's length or capacity.
///
/// Every caller passes a `char`-boundary-aligned range — a selection or a
/// caret byte index always is one — so replacing those complete scalars with
/// the single-byte `0x00` scalar can never leave `text` malformed UTF-8.
/// This is the one place the editor is allowed to discard bytes it must not
/// leave lying around: [`TextEditor::set_text`], an overwritten selection,
/// [`TextEditor::clear`], [`TextEditor::truncate_to_len`], and
/// [`TextEditor`]'s `Drop` all route through it. It never allocates: the
/// buffer is moved out as a `Vec<u8>`, erased in place, and moved back in,
/// so there is no need to reach for `String::as_mut_vec`'s `unsafe` escape
/// hatch.
///
/// The erasure itself is the workspace's shared
/// [`wipe`], not a plain `slice::fill(0)`.
/// Nothing reads the bytes back — on the `Drop` path they are freed
/// immediately afterwards — so an ordinary store is dead by the language's
/// own rules and a release build is entitled to delete it, leaving the
/// credential in the released block. The shared wipe writes volatile and
/// fences, so the erasure survives optimisation.
pub(crate) fn zeroize_range(text: &mut String, range: Range<usize>) {
    let mut bytes = mem::take(text).into_bytes();
    if let Some(slice) = bytes.get_mut(range) {
        wipe(slice);
    }
    // An all-`0x00` byte sequence is always valid UTF-8, so the zeroed
    // buffer can never fail to convert back; the fallback only guards
    // against a `get_mut` that returned `None` leaving `bytes` untouched
    // and therefore still exactly what `text` held.
    *text = String::from_utf8(bytes).unwrap_or_default();
}

/// A [`fmt::Debug`] stand-in for a secret buffer: prints the character count
/// it holds, never its content, so a debug dump of a masked field cannot
/// leak the credential it is protecting.
struct RedactedLen(usize);

impl fmt::Debug for RedactedLen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted: {} chars>", self.0)
    }
}

/// A single-line text buffer with a caret and a selection.
///
/// The [`caret`](Self::caret) and [`anchor`](Self::anchor) are byte indices
/// that always land on a `char` boundary of [`text`](Self::text); the selection
/// is the (possibly empty) range between them. Editing operations clamp to the
/// optional character limit and can never leave the caret mid-scalar, so a
/// renderer never has to defend against an invalid index (illegal states
/// unrepresentable).
///
/// [`secret`](Self::secret) switches the editor into bounded masked mode for
/// credential entry (see [`TextField::secret`]); every buffer-discarding
/// operation zeroises the bytes it drops through [`zeroize_range`] regardless
/// of mode, since doing so is cheap and harmless for a plain field too.
#[derive(Clone, Eq, PartialEq)]
struct TextEditor {
    text: String,
    caret: usize,
    anchor: usize,
    max_len: Option<usize>,
    /// Whether this editor is in bounded masked (secret) mode.
    secret: bool,
}

impl fmt::Debug for TextEditor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("TextEditor");
        if self.secret {
            s.field("text", &RedactedLen(self.char_count()));
        } else {
            s.field("text", &self.text);
        }
        s.field("caret", &self.caret)
            .field("anchor", &self.anchor)
            .field("max_len", &self.max_len)
            .field("secret", &self.secret)
            .finish()
    }
}

/// Zeroes the buffer before it is freed, so a dropped field — secret or
/// not — leaves no plaintext behind in its former heap allocation.
impl Drop for TextEditor {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl TextEditor {
    /// An empty editor with no character limit.
    fn new() -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: 0,
            max_len: None,
            secret: false,
        }
    }

    /// Turn this editor into bounded secret (masked) mode with a character
    /// limit of `max`, truncating any existing content to fit.
    ///
    /// Secret mode is inseparable from a bound: it immediately reserves the
    /// buffer's full worst-case UTF-8 byte capacity for `max` characters, so
    /// every following [`insert_char`](Self::insert_char) up to the limit
    /// finds capacity already available and can never trigger a
    /// reallocation that would leave a copy of a prior character behind in a
    /// freed heap block.
    fn make_secret(&mut self, max: usize) {
        self.secret = true;
        self.set_max_len(max);
    }

    /// Set the character limit to `max`, truncating any existing content to
    /// fit and moving the caret to the end. Re-affirms the reserved secret
    /// capacity when this editor is in secret mode, so the no-reallocation
    /// guarantee holds even if the limit changes after
    /// [`make_secret`](Self::make_secret).
    fn set_max_len(&mut self, max: usize) {
        self.max_len = Some(max);
        self.truncate_to_len(max);
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.reserve_secret_capacity();
    }

    /// Reserve capacity for the worst case this editor's character limit
    /// allows — every remaining slot filled by the widest UTF-8 scalar — so
    /// a secret field's buffer never has to grow while it fills. A no-op
    /// outside secret mode or with no limit set.
    fn reserve_secret_capacity(&mut self) {
        if !self.secret {
            return;
        }
        let Some(max) = self.max_len else {
            return;
        };
        let want = max.saturating_mul(MAX_UTF8_LEN);
        let have = self.text.capacity();
        if want > have {
            self.text.reserve_exact(want - have);
        }
    }

    /// Zero the whole buffer without changing its length — the exact
    /// operation `Drop` performs, factored out into its own method so
    /// `Drop::drop` and every editor operation that discards the buffer
    /// share one definition and can never drift apart.
    fn zeroize(&mut self) {
        let len = self.text.len();
        zeroize_range(&mut self.text, 0..len);
    }

    /// Replace the whole buffer, placing the caret at the end and collapsing
    /// the selection. The text is truncated to any character limit.
    ///
    /// The previous content is zeroised before it is discarded. In secret
    /// mode the replacement is pushed one `char` at a time up to the limit
    /// (never pushed in full and truncated after), so it can never need more
    /// than the capacity [`reserve_secret_capacity`](Self::reserve_secret_capacity)
    /// already reserved.
    fn set_text(&mut self, text: &str) {
        self.zeroize();
        self.text.clear();
        if self.secret {
            let max = self.max_len.unwrap_or(usize::MAX);
            for ch in text.chars().take(max) {
                self.text.push(ch);
            }
        } else {
            self.text.push_str(text);
            if let Some(max) = self.max_len {
                self.truncate_to_len(max);
            }
        }
        self.caret = self.text.len();
        self.anchor = self.caret;
    }

    /// Drop trailing characters until the buffer holds at most `max`
    /// scalars, zeroising the discarded tail first so no truncated scalar
    /// survives in the buffer's slack capacity.
    fn truncate_to_len(&mut self, max: usize) {
        if let Some((idx, _)) = self.text.char_indices().nth(max) {
            let len = self.text.len();
            zeroize_range(&mut self.text, idx..len);
            self.text.truncate(idx);
        }
    }

    /// The number of `char`s currently held.
    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// The selection as an ordered byte range, or `None` when it is empty.
    fn selection(&self) -> Option<(usize, usize)> {
        let (a, b) = (self.caret.min(self.anchor), self.caret.max(self.anchor));
        (a != b).then_some((a, b))
    }

    /// The caret's position measured in whole characters rather than bytes —
    /// the coordinate secret mode's fixed bead-cell layout uses in place of
    /// a glyph-width pixel offset.
    fn caret_cell(&self) -> usize {
        self.text[..self.caret].chars().count()
    }

    /// The selection as an ordered *character* cell range, or `None` when it
    /// is empty — the secret-mode equivalent of [`selection`](Self::selection),
    /// which measures in bytes.
    fn selection_cells(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection()?;
        Some((
            self.text[..a].chars().count(),
            self.text[..b].chars().count(),
        ))
    }

    /// The byte offset of the `char` boundary at cell `idx` (clamped to the
    /// buffer's end), or the buffer's length if `idx` is at or past the last
    /// character — the byte index secret mode's fixed-cell pointer hit test
    /// resolves to, so it can never land off a `char` boundary.
    fn byte_at_cell(&self, idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(idx)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// The byte index of the `char` boundary before `byte`, or `byte` at the
    /// start.
    fn prev_boundary(&self, byte: usize) -> usize {
        self.text[..byte]
            .char_indices()
            .next_back()
            .map_or(byte, |(i, _)| i)
    }

    /// The byte index of the `char` boundary after `byte`, or `byte` at the
    /// end.
    fn next_boundary(&self, byte: usize) -> usize {
        self.text[byte..]
            .chars()
            .next()
            .map_or(byte, |c| byte + c.len_utf8())
    }

    /// Delete the current selection, leaving the caret at its start. Returns
    /// whether anything was removed.
    ///
    /// The removed range is zeroised first, so replacing an entire
    /// selection (e.g. Ctrl+A then type) can never leave the overwritten
    /// content behind in the buffer's slack capacity.
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            return false;
        };
        zeroize_range(&mut self.text, a..b);
        self.text.replace_range(a..b, "");
        self.caret = a;
        self.anchor = a;
        true
    }

    /// Insert one character at the caret (replacing any selection), honouring
    /// the character limit. Returns whether the buffer changed.
    fn insert_char(&mut self, ch: char) -> bool {
        let removed = self.delete_selection();
        if let Some(max) = self.max_len {
            if self.char_count() >= max {
                return removed;
            }
        }
        self.text.insert(self.caret, ch);
        self.caret += ch.len_utf8();
        self.anchor = self.caret;
        true
    }

    /// Backspace: delete the selection, else the character before the caret.
    fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.caret == 0 {
            return false;
        }
        let start = self.prev_boundary(self.caret);
        self.text.replace_range(start..self.caret, "");
        self.caret = start;
        self.anchor = start;
        true
    }

    /// Forward-delete: delete the selection, else the character at the caret.
    fn delete_forward(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.caret >= self.text.len() {
            return false;
        }
        let end = self.next_boundary(self.caret);
        self.text.replace_range(self.caret..end, "");
        true
    }

    /// Move the caret one character left; `select` extends the selection,
    /// otherwise a non-empty selection collapses to its start.
    fn move_left(&mut self, select: bool) {
        if !select {
            if let Some((a, _)) = self.selection() {
                self.caret = a;
                self.anchor = a;
                return;
            }
        }
        self.caret = self.prev_boundary(self.caret);
        if !select {
            self.anchor = self.caret;
        }
    }

    /// Move the caret one character right; `select` extends the selection,
    /// otherwise a non-empty selection collapses to its end.
    fn move_right(&mut self, select: bool) {
        if !select {
            if let Some((_, b)) = self.selection() {
                self.caret = b;
                self.anchor = b;
                return;
            }
        }
        self.caret = self.next_boundary(self.caret);
        if !select {
            self.anchor = self.caret;
        }
    }

    /// Move the caret to the start; `select` extends the selection.
    fn home(&mut self, select: bool) {
        self.caret = 0;
        if !select {
            self.anchor = 0;
        }
    }

    /// Move the caret to the end; `select` extends the selection.
    fn end(&mut self, select: bool) {
        self.caret = self.text.len();
        if !select {
            self.anchor = self.caret;
        }
    }

    /// Select the whole buffer.
    fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Clear the buffer and reset the caret. Returns whether anything was
    /// removed.
    ///
    /// The discarded content is zeroised first, exactly like `set_text`.
    fn clear(&mut self) -> bool {
        let changed = !self.text.is_empty();
        self.zeroize();
        self.text.clear();
        self.caret = 0;
        self.anchor = 0;
        changed
    }

    /// Set the caret to `byte` (clamped to a boundary), collapsing the
    /// selection unless `select` is set.
    fn place_caret(&mut self, byte: usize, select: bool) {
        let byte = byte.min(self.text.len());
        // Snap onto a boundary in case the hit test rounded into a scalar.
        let byte = if self.text.is_char_boundary(byte) {
            byte
        } else {
            self.prev_boundary(byte)
        };
        self.caret = byte;
        if !select {
            self.anchor = byte;
        }
    }
}

/// The resolved surface geometry of a field within its bounds: the field row
/// (the plate) and the clipped inner text region, plus the message row below.
struct FieldGeom {
    /// The field-row plate rectangle `(x, y, w, h)` in surface pixels.
    row: (u32, u32, u32, u32),
    /// The surface-x where clipped text begins (after border, inset, leading).
    text_x0: u32,
    /// The clipped text region width in pixels.
    avail_w: u32,
    /// The message-row rectangle below the field, if there is room for one.
    message: Option<(u32, u32, u32, u32)>,
}

/// Resolve a field's geometry for `bounds`, reserving `leading` pixels at the
/// start of the text region (a search magnifier), or `None` if it collapses.
fn field_geom(
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
    leading: u32,
) -> Option<FieldGeom> {
    let (x, y, w, h) = surface_rect(bounds)?;
    if w == 0 || h == 0 {
        return None;
    }
    let metrics = theme.metrics();
    let border = plate_border(theme, scale);
    let pad = scale.scale_length(metrics.control_inset);
    let edge = border.saturating_add(pad);

    let control_h = scale.scale_length(metrics.control_height).max(1);
    let row_h = if control_h < h { control_h } else { h };

    let text_x0 = x + edge.saturating_add(leading).min(w);
    let avail_w = w.saturating_sub(edge.saturating_mul(2).saturating_add(leading));

    let message = {
        let below = h.saturating_sub(row_h);
        let glyph_h = font.glyph_height();
        let mw = w.saturating_sub(edge.saturating_mul(2));
        if below >= glyph_h.saturating_add(pad) && mw > 0 {
            let my = y + row_h + pad;
            Some((x + edge, my, mw, below.saturating_sub(pad)))
        } else {
            None
        }
    };

    Some(FieldGeom {
        row: (x, y, w, row_h),
        text_x0,
        avail_w,
        message,
    })
}

/// The horizontal text scroll (pixels hidden at the left) that keeps the caret
/// visible: zero until the caret would pass the right edge, then just enough to
/// pin the caret to that edge. Deterministic from the caret alone, so `render`
/// needs no stored scroll state.
fn text_scroll(font: BitmapFont, text: &str, caret: usize, avail_w: u32) -> u32 {
    font.width_to_offset(text, caret).saturating_sub(avail_w)
}

/// The byte index whose `char` boundary is nearest text-space x `rel` (pixels
/// from the text start, i.e. pointer-x minus the text origin plus the scroll).
fn byte_from_x(font: BitmapFont, text: &str, rel: i32) -> usize {
    font.offset_at_width(text, u32::try_from(rel.max(0)).unwrap_or(u32::MAX))
}

// --- Secret-mode bead geometry ----------------------------------------------
//
// A masked field never lays a character's glyph, so it cannot measure a run
// by glyph width the way `text_scroll`/`byte_from_x` do above.
// Instead every `char` occupies one fixed-width cell, sized from the active
// theme and scale rather than the font, and every position below is counted
// in *cells* until it is finally converted to a pixel offset.

/// The diameter of one secret-mode bead: the theme's boolean-selector glyph
/// extent, scaled, and never taller than the text row — so a run of beads
/// centres on the same baseline plain text uses and a secret field measures
/// exactly as tall as a plain one.
fn bead_diameter(theme: &Theme, scale: Scale, row_h: u32) -> u32 {
    scale
        .scale_length(theme.metrics().selector_extent)
        .max(1)
        .min(row_h)
}

/// The fixed pixel advance between adjacent secret-mode bead cells: the
/// bead plus a gap half its own diameter (never less than one physical
/// pixel), so a run of beads reads as separate marks rather than a solid
/// bar — deliberately independent of any character's actual glyph width.
fn bead_advance(diameter: u32) -> u32 {
    diameter.saturating_add((diameter / 2).max(1))
}

/// The pixel x of bead cell `cell` at the given per-cell `advance`,
/// saturating rather than overflowing for a very long buffer.
fn cell_x(cell: usize, advance: u32) -> i32 {
    to_i32(
        u32::try_from(cell)
            .unwrap_or(u32::MAX)
            .saturating_mul(advance),
    )
}

/// The horizontal cell-scroll (pixels hidden at the left) that keeps the
/// caret visible in secret mode: zero until the caret's cell would pass the
/// right edge, then just enough to pin it there. The secret-mode mirror of
/// [`text_scroll`], measured in cells rather than glyph pixels.
fn secret_scroll(caret_cell: usize, advance: u32, avail_w: u32) -> u32 {
    u32::try_from(cell_x(caret_cell, advance))
        .unwrap_or(0)
        .saturating_sub(avail_w)
}

/// The cell index nearest text-space x `rel` (pixels from the text start,
/// after scroll) at the given per-cell `advance`, clamped to `count` cells.
///
/// This is secret mode's pointer hit test: it only ever divides a pixel
/// offset by the fixed cell advance, never measuring by a character's
/// glyph width the way [`byte_from_x`] does for a plain field.
fn cell_from_x(rel: i32, advance: u32, count: usize) -> usize {
    if advance == 0 {
        return 0;
    }
    let rel = u32::try_from(rel.max(0)).unwrap_or(u32::MAX);
    // Round to the nearest cell boundary (rather than always flooring) so a
    // click past a cell's midpoint lands after it, matching the plain
    // field's nearest-boundary behaviour in `byte_from_x`.
    let cell = (rel.saturating_add(advance / 2)) / advance;
    usize::try_from(cell).unwrap_or(usize::MAX).min(count)
}

/// The shared single-line field: editor, role, composed state, read-only flag,
/// placeholder, and inline message, plus the render and input behaviour every
/// text control reuses. [`TextField`] and [`SearchField`] wrap one of these so
/// the editing model, clipped scrolling, caret/selection drawing, and the spec §13
/// disposition rendering are defined once.
///
/// Sharing the core also gives both fields the same render-equivalence
/// equality: the text, caret, selection endpoints, role, visible state,
/// placeholder, and message all compare — while the pointer coordinate and the
/// selection-drag latch, which no render path reads, do not.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FieldCore {
    editor: TextEditor,
    role: ControlRole,
    state: ControlState,
    read_only: bool,
    placeholder: Option<String>,
    message: Option<String>,
    /// The last pointer position, mapped to a byte index when a press or a
    /// drag places the caret — hit-testing input, never a drawn property.
    pointer: RenderInvariant<Point>,
    /// Whether a press is still extending a selection; what that produces —
    /// the caret and the selection endpoints — lives in `editor`.
    selecting: RenderInvariant<bool>,
}

impl FieldCore {
    /// An empty, resting neutral field.
    fn new() -> Self {
        Self {
            editor: TextEditor::new(),
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            read_only: false,
            placeholder: None,
            message: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
            selecting: RenderInvariant::new(false),
        }
    }

    /// Whether the caller may navigate/select within the field (enabled,
    /// allowed, not pending — the fail-closed gate every control uses).
    fn actionable(&self) -> bool {
        self.state.is_actionable()
    }

    /// Whether the field accepts edits: actionable and not read-only.
    fn editable(&self) -> bool {
        self.actionable() && !self.read_only
    }

    /// Whether the caret should be drawn: focused and actionable.
    fn show_caret(&self) -> bool {
        self.state.focus.focused && self.actionable()
    }

    /// Paint the field plate, clipped scrolling text, caret/selection, Signal
    /// Bead, and inline message, reserving `leading` pixels for a search glyph.
    fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        leading: u32,
    ) {
        let Some(geom) = field_geom(bounds, scale, theme, font, leading) else {
            return;
        };
        let (x, y, w, h) = geom.row;
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(metrics.control_corner_radius).min(h / 2);
        let disposition = self.state.disposition();
        // The ground a field's plate takes, where the recipe's plate is a
        // plain background. A field the user may *write* in is a page — paper
        // on a light appearance — because the ground is the affordance; a
        // read-only one recesses onto the window ground so it reads as a value
        // shown rather than entered, keeping full-contrast text so it is still
        // not a muted disabled field. Neither substitutes a plate the recipe
        // put a colour on: a disabled, denied, or failed-closed field is
        // stating something there. `ground_fill` is what lets either ground
        // sit on floating chrome as glass rather than as an opaque patch.
        let ground = if self.editable() {
            Some(palette.document)
        } else if self.read_only && disposition != ControlDisposition::DisabledByState {
            Some(palette.surface)
        } else {
            None
        };
        let mut frame = resolve_frame(theme, self.role, self.state);
        if let Some(ground) = ground {
            frame = frame.grounded_on(Color::from(ground_fill(theme, ground, ChromeLayer::Plate)));
        }

        // Validation drives the rim segment on an otherwise-interactive field;
        // a denied/disabled/failed field keeps its disposition rim untouched.
        let rim = match disposition {
            ControlDisposition::Interactive
            | ControlDisposition::NeedsConfirmation
            | ControlDisposition::PendingCheck => match self.state.validation {
                ValidationState::Invalid => Color::from(palette.danger),
                ValidationState::Warning => Color::from(palette.warning),
                _ => frame.rim,
            },
            _ => frame.rim,
        };

        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: frame.plate,
                rim,
                focused: frame.focused,
                ring: Color::from(palette.rim_active),
            },
        );

        self.paint_text(surface, &geom, scale, theme, font, frame.label);

        if let Some((color, shape)) = resolve_bead(theme, self.state) {
            let size = scale.scale_length(metrics.bead_size).max(3).min(w).min(h);
            paint_bead(
                surface,
                x + w - border - size,
                y + border,
                size,
                color,
                shape,
            );
        }

        self.paint_message(surface, &geom, theme, font);
    }

    /// Paint the clipped, horizontally-scrolled text (or placeholder), the
    /// selection highlight, and the caret into the text region.
    ///
    /// A non-empty secret-mode buffer never reaches [`BitmapFont::draw_text`]
    /// here — it is delegated to [`paint_secret`](Self::paint_secret) instead,
    /// which draws bead cells at a fixed advance rather than the buffer's
    /// characters, so a masked field's drawn width and pixels never depend on
    /// the secret it holds. An empty buffer still shows its placeholder
    /// normally in secret mode: a placeholder is not a secret.
    fn paint_text(
        &self,
        surface: &mut Surface,
        geom: &FieldGeom,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        label: Color,
    ) {
        let (_, y, _, row_h) = geom.row;
        let avail_w = geom.avail_w;
        if avail_w == 0 || row_h == 0 {
            return;
        }
        let Some(mut layer) = Surface::new(avail_w, row_h) else {
            return;
        };
        let palette = theme.palette();
        let text = self.editor.text.as_str();
        let glyph_h = font.glyph_height();
        let baseline = to_i32(row_h.saturating_sub(glyph_h)) / 2;

        if text.is_empty() {
            if let Some(placeholder) = &self.placeholder {
                paint_run(
                    &mut layer,
                    font,
                    font.elide_to_width(placeholder, avail_w),
                    (0, baseline),
                    Color::from(palette.on_surface_muted),
                    None,
                );
            }
        } else if self.editor.secret {
            self.paint_secret(&mut layer, scale, theme, row_h, avail_w, label);
        } else {
            let scroll = text_scroll(font, text, self.editor.caret, avail_w);
            let base_x = -to_i32(scroll);
            if let Some((a, b)) = self.editor.selection() {
                let sa = to_i32(font.width_to_offset(text, a)) + base_x;
                let sb = to_i32(font.width_to_offset(text, b)) + base_x;
                let clamped_a = sa.clamp(0, to_i32(avail_w));
                let clamped_b = sb.clamp(0, to_i32(avail_w));
                let sel_w = u32::try_from(clamped_b - clamped_a).unwrap_or(0);
                if sel_w > 0 {
                    layer.fill_rect(
                        u32::try_from(clamped_a).unwrap_or(0),
                        0,
                        sel_w,
                        row_h,
                        Color::from(palette.accent),
                    );
                }
                font.draw_text(&mut layer, base_x, baseline, &text[..a], label);
                font.draw_text(
                    &mut layer,
                    sa,
                    baseline,
                    &text[a..b],
                    Color::from(palette.on_accent),
                );
                font.draw_text(&mut layer, sb, baseline, &text[b..], label);
            } else {
                font.draw_text(&mut layer, base_x, baseline, text, label);
            }
        }

        if !self.editor.secret && self.show_caret() && self.editor.selection().is_none() {
            let scroll = text_scroll(font, text, self.editor.caret, avail_w);
            let cx = to_i32(font.width_to_offset(text, self.editor.caret)) - to_i32(scroll);
            let caret_w = scale.scale_length(1).max(1);
            let cx = cx.clamp(0, to_i32(avail_w.saturating_sub(caret_w)));
            layer.fill_rect(
                u32::try_from(cx).unwrap_or(0),
                0,
                caret_w,
                row_h,
                Color::from(palette.on_surface),
            );
        }

        surface.blit(to_i32(geom.text_x0), to_i32(y), &layer);
    }

    /// Paint secret-mode content into `layer`: one filled bead per `char`
    /// (never the characters), the caret between bead cells, and the
    /// selection highlight over whole cells.
    ///
    /// Drawing beads at a fixed per-`char` advance — rather than the glyph a
    /// plain field would draw — makes the run's width depend only on the
    /// buffer's *length*, never on which characters it holds, and needs no
    /// particular glyph to exist in the font: exactly the two properties a
    /// masked field needs so its rendered shape alone cannot leak anything
    /// about the secret it hides.
    fn paint_secret(
        &self,
        layer: &mut Surface,
        scale: Scale,
        theme: &Theme,
        row_h: u32,
        avail_w: u32,
        label: Color,
    ) {
        let palette = theme.palette();
        let count = self.editor.char_count();
        let diameter = bead_diameter(theme, scale, row_h);
        let advance = bead_advance(diameter);
        let scroll = secret_scroll(self.editor.caret_cell(), advance, avail_w);
        let base_x = -to_i32(scroll);
        let cell_y = u32::try_from(to_i32(row_h.saturating_sub(diameter)) / 2).unwrap_or(0);
        let selection = self.editor.selection_cells();

        if let Some((a, b)) = selection {
            let sa = cell_x(a, advance) + base_x;
            let sb = cell_x(b, advance) + base_x;
            let clamped_a = sa.clamp(0, to_i32(avail_w));
            let clamped_b = sb.clamp(0, to_i32(avail_w));
            let sel_w = u32::try_from(clamped_b - clamped_a).unwrap_or(0);
            if sel_w > 0 {
                layer.fill_rect(
                    u32::try_from(clamped_a).unwrap_or(0),
                    0,
                    sel_w,
                    row_h,
                    Color::from(palette.accent),
                );
            }
        }

        for i in 0..count {
            let cx = cell_x(i, advance) + base_x;
            if cx + to_i32(diameter) <= 0 || cx >= to_i32(avail_w) {
                continue;
            }
            let color = match selection {
                Some((a, b)) if i >= a && i < b => Color::from(palette.on_accent),
                _ => label,
            };
            paint_filled_circle(
                layer,
                u32::try_from(cx.max(0)).unwrap_or(0),
                cell_y,
                diameter,
                color,
            );
        }

        if self.show_caret() && selection.is_none() {
            let cx = cell_x(self.editor.caret_cell(), advance) + base_x;
            let caret_w = scale.scale_length(1).max(1);
            let cx = cx.clamp(0, to_i32(avail_w.saturating_sub(caret_w)));
            layer.fill_rect(
                u32::try_from(cx).unwrap_or(0),
                0,
                caret_w,
                row_h,
                Color::from(palette.on_surface),
            );
        }
    }

    /// Paint the inline validation/help message below the field, coloured by
    /// the validation state (danger/warning), else a quiet hint.
    ///
    /// A message is prose — it says what is wrong and often what to do about
    /// it — so it wraps across the field's own width over as many lines as
    /// the band below the field holds, rather than being cut at the edge
    /// halfway through the instruction.
    fn paint_message(
        &self,
        surface: &mut Surface,
        geom: &FieldGeom,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let Some(message) = &self.message else {
            return;
        };
        let Some((mx, my, mw, mh)) = geom.message else {
            return;
        };
        self.message_block(font, mw, line_budget(font, mh), theme)
            .paint(surface, message, (mx, my));
    }

    /// The block an inline message is laid out in: prose in the tone its
    /// validation state calls for — danger, warning, else a quiet hint.
    ///
    /// One definition for every member of the family, so a refused value
    /// reads the same under a one-line field and a multi-line box, and the
    /// height a box reserves for its message is the height it draws.
    fn message_block(
        &self,
        font: BitmapFont,
        width: u32,
        lines: usize,
        theme: &Theme,
    ) -> TextBlock {
        let palette = theme.palette();
        let color = match self.state.validation {
            ValidationState::Invalid => Color::from(palette.danger),
            ValidationState::Warning => Color::from(palette.warning),
            _ => Color::from(palette.on_surface_muted),
        };
        TextBlock::prose(font, width, lines, color)
    }

    /// Apply `edit` to the editor, reporting `bounds` when it changed anything
    /// the field draws, and pass on the edit's own answer.
    ///
    /// An edit changes the drawn field in two ways: the buffer, which the edit
    /// itself answers for, and the caret or selection, which the caret/anchor
    /// pair answers for. The buffer's bytes are deliberately **not** compared:
    /// a secret field's characters must not be copied anywhere, even into a
    /// temporary a comparison would drop.
    fn edit(
        &mut self,
        bounds: Rect,
        damage: &mut Region,
        edit: impl FnOnce(&mut TextEditor) -> bool,
    ) -> bool {
        let before = (self.editor.caret, self.editor.anchor);
        let changed = edit(&mut self.editor);
        if changed || (self.editor.caret, self.editor.anchor) != before {
            damage.add(bounds);
        }
        changed
    }

    /// Feed a pointer event; a press places the caret (and starts a selection
    /// drag), motion while dragging extends the selection, release ends it.
    /// A denied/disabled/pending field ignores pointer editing (fail closed).
    fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        leading: u32,
        damage: &mut Region,
    ) -> Option<TextAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        // The face is a function of the theme and the scale, so the input path
        // asks for it rather than taking a derived value as an argument.
        let font = role_font(theme, scale, TextRole::Body);
        let geom = field_geom(bounds, scale, theme, font, leading)?;
        let inside = bounds.contains(*self.pointer);
        let hover_or_none = if inside {
            PointerState::Hover
        } else {
            PointerState::None
        };
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if inside && self.actionable() {
                    *self.selecting = true;
                    damage::set(
                        &mut self.state.pointer,
                        PointerState::Pressed,
                        bounds,
                        damage,
                    );
                    let byte = self.byte_at(&geom, scale, theme, font);
                    self.edit(bounds, damage, |editor| {
                        editor.place_caret(byte, false);
                        false
                    });
                }
                None
            }
            InputEvent::PointerMoved { .. } => {
                if *self.selecting {
                    let byte = self.byte_at(&geom, scale, theme, font);
                    self.edit(bounds, damage, |editor| {
                        editor.place_caret(byte, true);
                        false
                    });
                } else {
                    damage::set(&mut self.state.pointer, hover_or_none, bounds, damage);
                }
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                *self.selecting = false;
                damage::set(&mut self.state.pointer, hover_or_none, bounds, damage);
                None
            }
            _ => None,
        }
    }

    /// The byte index the current pointer x maps to within the text region.
    ///
    /// Secret mode never derives this from a glyph width: it divides the
    /// pointer offset by the fixed bead-cell advance to get a cell index,
    /// then resolves that cell to its `char`-boundary byte offset — the same
    /// two-step conversion [`FieldCore::render`] uses to draw the caret,
    /// so a click always lands where the caret would be drawn.
    fn byte_at(&self, geom: &FieldGeom, scale: Scale, theme: &Theme, font: BitmapFont) -> usize {
        let text = self.editor.text.as_str();
        if self.editor.secret {
            let (_, _, _, row_h) = geom.row;
            let diameter = bead_diameter(theme, scale, row_h);
            let advance = bead_advance(diameter);
            let scroll = secret_scroll(self.editor.caret_cell(), advance, geom.avail_w);
            let rel = self.pointer.x - to_i32(geom.text_x0) + to_i32(scroll);
            let cell = cell_from_x(rel, advance, self.editor.char_count());
            self.editor.byte_at_cell(cell)
        } else {
            let scroll = text_scroll(font, text, self.editor.caret, geom.avail_w);
            let rel = self.pointer.x - to_i32(geom.text_x0) + to_i32(scroll);
            byte_from_x(font, text, rel)
        }
    }

    /// Feed a key event. Editing keys require an editable field; navigation and
    /// selection require an actionable one; Enter/Escape report submit/cancel.
    /// `clear_on_escape` clears a non-empty buffer first (a search field).
    fn on_key(
        &mut self,
        key: Key,
        mods: Modifiers,
        clear_on_escape: bool,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<TextAction> {
        if !self.state.focus.focused || !self.actionable() {
            return None;
        }
        match key {
            Key::Char('a' | 'A') if mods.ctrl => {
                self.edit(bounds, damage, |editor| {
                    editor.select_all();
                    false
                });
                None
            }
            Key::Char(ch) if self.editable() && !mods.ctrl && !mods.alt && !mods.meta => {
                if ch.is_control() {
                    return None;
                }
                self.edit(bounds, damage, |editor| editor.insert_char(ch))
                    .then_some(TextAction::Edited)
            }
            Key::Named(NamedKey::Backspace) if self.editable() => self
                .edit(bounds, damage, TextEditor::backspace)
                .then_some(TextAction::Edited),
            Key::Named(NamedKey::Delete) if self.editable() => self
                .edit(bounds, damage, TextEditor::delete_forward)
                .then_some(TextAction::Edited),
            Key::Named(NamedKey::Left) => {
                self.edit(bounds, damage, |editor| {
                    editor.move_left(mods.shift);
                    false
                });
                None
            }
            Key::Named(NamedKey::Right) => {
                self.edit(bounds, damage, |editor| {
                    editor.move_right(mods.shift);
                    false
                });
                None
            }
            Key::Named(NamedKey::Home) => {
                self.edit(bounds, damage, |editor| {
                    editor.home(mods.shift);
                    false
                });
                None
            }
            Key::Named(NamedKey::End) => {
                self.edit(bounds, damage, |editor| {
                    editor.end(mods.shift);
                    false
                });
                None
            }
            Key::Named(NamedKey::Enter) => Some(TextAction::Submitted),
            Key::Named(NamedKey::Escape) => {
                if clear_on_escape && self.edit(bounds, damage, TextEditor::clear) {
                    Some(TextAction::Edited)
                } else {
                    Some(TextAction::Cancelled)
                }
            }
            _ => None,
        }
    }
}

/// A single-line text entry (spec §11.8).
///
/// A `TextField` owns its typed [`ControlState`], its [`ControlRole`], and its
/// text buffer; it renders itself into a [`Surface`] and consumes pointer and
/// keyboard input, emitting a [`TextAction`] when the content changes or the
/// user submits/cancels. It performs no privileged work — the owning container
/// validates the value and enforces authority. A read-only
/// field stays legible and selectable but refuses edits, distinct from a
/// disabled field (muted) and a denied field (Authority Mark), per spec §13.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextField {
    core: FieldCore,
}

impl Default for TextField {
    fn default() -> Self {
        Self::new()
    }
}

impl TextField {
    /// An empty, resting neutral text field.
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: FieldCore::new(),
        }
    }

    /// This field with a non-default role (e.g. destructive).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.core.role = role;
        self
    }

    /// This field pre-filled with `text` (caret at the end).
    #[must_use]
    pub fn with_text(mut self, text: impl AsRef<str>) -> Self {
        self.core.editor.set_text(text.as_ref());
        self
    }

    /// This field with placeholder text shown while it is empty.
    #[must_use]
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.core.placeholder = Some(placeholder.into());
        self
    }

    /// This field limited to at most `max` characters (existing content is
    /// truncated to fit).
    #[must_use]
    pub fn with_max_len(mut self, max: usize) -> Self {
        self.core.editor.set_max_len(max);
        self
    }

    /// Turn this field into bounded secret (masked) mode for credential entry
    /// (a password, a passphrase, a PIN), with a character limit of `max`.
    ///
    /// A secret field never draws the buffer's characters: instead it draws
    /// one filled bead per `char` at a fixed advance, so the rendered width
    /// depends only on the buffer's length and never leaks which characters
    /// it holds (see the [module documentation](self)). Secret mode always
    /// carries a bound — there is no unbounded secret field — because the
    /// bound is what lets the editor reserve its full byte capacity up
    /// front and so guarantee it can never reallocate while filling: a
    /// reallocation would otherwise leave a copy of the credential behind in
    /// a freed heap block. There is deliberately no way to reveal the
    /// buffer through the control (no "show password" toggle); the owner
    /// that holds the plaintext may display it through its own means if it
    /// chooses to.
    #[must_use]
    pub fn secret(mut self, max_len: usize) -> Self {
        self.core.editor.make_secret(max_len);
        self
    }

    /// Whether this field is in secret (masked) mode.
    #[must_use]
    pub fn is_secret(&self) -> bool {
        self.core.editor.secret
    }

    /// This field marked read-only: legible and selectable, but not editable.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.core.read_only = read_only;
        self
    }

    /// This field with an inline validation/help message shown below it.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.core.message = Some(message.into());
        self
    }

    /// The height one line of field occupies at `scale`: the shared
    /// text-plate height every one-line control takes.
    ///
    /// Exposed because a field is not always laid out by a panel that already
    /// knows this — a menu chain's own entry surface sizes itself to one, and
    /// the file manager grows an item's name band to it so an in-place editor
    /// is legible over a short row.
    #[must_use]
    pub fn height(scale: Scale, theme: &Theme) -> u32 {
        text_plate_height(theme, scale, TextRole::Body)
    }

    /// The field's current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.core.editor.text
    }

    /// Replace the field's text (caret to the end), e.g. after the owner
    /// commits a change.
    pub fn set_text(&mut self, text: impl AsRef<str>) {
        self.core.editor.set_text(text.as_ref());
    }

    /// Whether the field is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.core.read_only
    }

    /// The field's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.core.role
    }

    /// The field's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.core.state
    }

    /// Replace the field's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.core.state = state;
    }

    /// Set the field's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.core.state.focus.focused = focused;
    }

    /// Set the inline validation/help message (or clear it with `None`).
    pub fn set_message(&mut self, message: Option<String>) {
        self.core.message = message;
    }

    /// Paint the field into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if withheld(surface, bounds) {
            return;
        }
        let font = role_font(theme, scale, TextRole::Body);
        self.core.render(surface, bounds, scale, theme, font, 0);
    }

    /// Feed a pointer event: a primary press positions the caret under the
    /// pointer and starts a selection, motion while pressed extends it, and
    /// release ends it. A denied/disabled/pending field ignores it (fail
    /// closed).
    ///
    /// The field reports `bounds` into `damage` when the event changed what it
    /// draws — the text, the caret, the selection, or its pointer look. A
    /// sample that stays inside a field it is already hovering reports nothing.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TextAction> {
        self.core.on_pointer(event, bounds, scale, theme, 0, damage)
    }

    /// Feed a key event: printable keys insert (replacing any selection),
    /// Backspace/Delete remove, arrows/Home/End move the caret (Shift extends
    /// the selection), Ctrl+A selects all, Enter submits, and Escape cancels.
    /// Editing keys require an editable (not read-only, not denied) field.
    ///
    /// A key that edits the buffer or moves the caret reports `bounds`; one
    /// that only submits or cancels, or moves a caret already at the end it
    /// moves toward, reports nothing.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<TextAction> {
        self.core.on_key(key, modifiers, false, bounds, damage)
    }
}

/// Test-only: the field's backing buffer's address and byte capacity, so a
/// test can prove that filling a secret field up to its limit never
/// reallocates (a reallocation would leave a copy of the credential behind
/// in a freed heap block).
#[cfg(test)]
pub(crate) fn debug_buffer_identity(field: &TextField) -> (*const u8, usize) {
    (
        field.core.editor.text.as_ptr(),
        field.core.editor.text.capacity(),
    )
}

/// Test-only: a copy of the field's raw buffer bytes, including any bytes
/// [`zeroize_range`] has overwritten — a plain [`TextField::text`] cannot
/// show that, since a zeroised buffer is always truncated or replaced before
/// a caller could read it back.
#[cfg(test)]
pub(crate) fn debug_bytes(field: &TextField) -> alloc::vec::Vec<u8> {
    field.core.editor.text.as_bytes().to_vec()
}

/// Test-only: zero the field's buffer without dropping it, through the exact
/// method [`TextEditor`]'s `Drop` implementation calls.
///
/// A dropped `String`'s allocation cannot be read afterwards without
/// `unsafe`, which this crate forbids outright, so a test cannot observe a
/// real drop's effect directly. This hook instead proves the two are the
/// same operation: [`TextEditor::drop`] delegates to the private `zeroize`
/// method, and this is that same method, called without triggering an
/// actual drop.
#[cfg(test)]
pub(crate) fn debug_zeroize(field: &mut TextField) {
    field.core.editor.zeroize();
}

/// Test-only: the secret-mode cell layout for `bounds` under the given theme
/// and scale — the surface x the first bead cell starts at, and the fixed
/// advance between cells.
///
/// Both come from the exact geometry [`FieldCore::paint_secret`] draws
/// through, so a test aiming a pointer click at a bead cell boundary cannot
/// drift from where that cell is actually painted.
#[cfg(test)]
pub(crate) fn debug_secret_cell_layout(
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
) -> Option<(u32, u32)> {
    let font = role_font(theme, scale, TextRole::Body);
    let geom = field_geom(bounds, scale, theme, font, 0)?;
    let (_, _, _, row_h) = geom.row;
    Some((
        geom.text_x0,
        bead_advance(bead_diameter(theme, scale, row_h)),
    ))
}

/// Draw a magnifier glyph (a ring with a short handle) of `size` at `(x, y)`,
/// the search field's leading affordance. `hole` is the plate colour showing
/// through the ring.
fn paint_magnifier(surface: &mut Surface, x: u32, y: u32, size: u32, color: Color, hole: Color) {
    if size < 4 {
        return;
    }
    let ring = (size * 3 / 4).max(3);
    let rx = x + (size - ring) / 2;
    let ry = y + (size - ring) / 2;
    surface.fill_round_rect(rx, ry, ring, ring, ring / 2, color);
    let t = (ring / 5).max(1);
    let inner = ring.saturating_sub(t.saturating_mul(2));
    if inner > 0 {
        surface.fill_round_rect(rx + t, ry + t, inner, inner, inner / 2, hole);
    }
    let hs = (size / 3).max(2);
    let hx = (rx + ring).saturating_sub(hs / 2).min(x + size - hs);
    let hy = (ry + ring).saturating_sub(hs / 2).min(y + size - hs);
    surface.fill_round_rect(hx, hy, hs, hs, hs / 4, color);
}

/// A single-line text entry specialised for queries (spec §11.8).
///
/// A `SearchField` is a [`TextField`] behind a leading magnifier that reads as
/// *active* (accent-tinted) when a query is present and quiet when it is empty,
/// so the query state is legible from the leading affordance. Escape clears a
/// non-empty query (reporting [`TextAction::Edited`]) before dismissing the
/// field; every other behaviour matches [`TextField`], over one shared editing
/// and rendering core.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchField {
    core: FieldCore,
}

impl Default for SearchField {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchField {
    /// An empty, resting search field.
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: FieldCore::new(),
        }
    }

    /// This search field pre-filled with a query (caret at the end).
    #[must_use]
    pub fn with_text(mut self, text: impl AsRef<str>) -> Self {
        self.core.editor.set_text(text.as_ref());
        self
    }

    /// This search field with placeholder text shown while it is empty.
    #[must_use]
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.core.placeholder = Some(placeholder.into());
        self
    }

    /// This search field limited to at most `max` characters.
    #[must_use]
    pub fn with_max_len(mut self, max: usize) -> Self {
        self.core.editor.set_max_len(max);
        self
    }

    /// The current query text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.core.editor.text
    }

    /// Replace the query text (caret to the end).
    pub fn set_text(&mut self, text: impl AsRef<str>) {
        self.core.editor.set_text(text.as_ref());
    }

    /// Whether a query is present (non-empty).
    #[must_use]
    pub fn has_query(&self) -> bool {
        !self.core.editor.text.is_empty()
    }

    /// The field's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.core.state
    }

    /// Replace the field's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.core.state = state;
    }

    /// Set the field's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.core.state.focus.focused = focused;
    }

    /// The leading magnifier region width for `bounds` (a square the height of
    /// the field row, capped to half the width so text always has room).
    fn leading(bounds: Rect, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        field_geom(bounds, scale, theme, font, 0).map_or(0, |g| {
            let (_, _, w, row_h) = g.row;
            row_h.min(w / 2)
        })
    }

    /// Paint the search field into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if withheld(surface, bounds) {
            return;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let leading = Self::leading(bounds, scale, theme, font);
        self.core
            .render(surface, bounds, scale, theme, font, leading);

        let Some(geom) = field_geom(bounds, scale, theme, font, leading) else {
            return;
        };
        if leading == 0 {
            return;
        }
        let (x, y, _, row_h) = geom.row;
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset);
        let edge = border.saturating_add(pad);
        // The magnifier reads as active (accent) when a query is present and
        // the field is actionable, quiet (muted) otherwise.
        let color = if self.has_query() && self.core.actionable() {
            Color::from(palette.accent)
        } else {
            Color::from(palette.on_surface_muted)
        };
        let size = leading
            .saturating_sub(pad)
            .min(row_h.saturating_sub(border.saturating_mul(2)));
        let gx = x + edge;
        let gy = y + (row_h.saturating_sub(size)) / 2;
        paint_magnifier(surface, gx, gy, size, color, Color::from(palette.surface));
    }

    /// Feed a pointer event; see [`TextField::on_pointer`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TextAction> {
        let font = role_font(theme, scale, TextRole::Body);
        let leading = Self::leading(bounds, scale, theme, font);
        self.core
            .on_pointer(event, bounds, scale, theme, leading, damage)
    }

    /// Feed a key event; Escape clears a non-empty query before dismissing.
    /// Reports like [`TextField::on_key`], and a cleared query reports too —
    /// the magnifier goes quiet with it.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<TextAction> {
        self.core.on_key(key, modifiers, true, bounds, damage)
    }
}

// --- TextArea ---------------------------------------------------------------

/// The most lines of wrapped message a multi-line entry reserves beneath
/// itself: one sentence about the text above it, not a document.
const AREA_MESSAGE_LINES: usize = 2;

/// The resolved surface geometry of a [`TextArea`] within its bounds.
struct AreaGeom {
    /// The plate rectangle `(x, y, w, h)` in surface pixels.
    plate: (u32, u32, u32, u32),
    /// The text viewport inside the plate, past the scrollbar gutter.
    text: (u32, u32, u32, u32),
    /// How many whole wrapped lines the viewport shows.
    rows: usize,
    /// How many wrapped lines the text takes at the viewport's width.
    lines: usize,
    /// The scrollbar's rectangle, when the text does not fit the viewport.
    bar: Option<Rect>,
    /// The message band below the plate, where there is one and it fits.
    message: Option<(u32, u32, u32, u32)>,
}

impl AreaGeom {
    /// The scroll offset `want` clamped to what the viewport can show.
    fn clamp_scroll(&self, want: usize) -> usize {
        want.min(self.lines.saturating_sub(self.rows))
    }
}

/// A multi-line text entry that wraps its text (spec §11.42).
///
/// A `TextArea` is the text-entry family's multi-line member: the same plate,
/// caret, selection, read-only/denied/validation rendering and typed
/// [`TextAction`] as a [`TextField`], over a text that **wraps at the box's
/// own width** rather than scrolling sideways. That is the difference between
/// the two, and it is why both exist: a single-line field holds a value and
/// scrolls; a box that holds a paragraph wraps it, because a paragraph read
/// through a one-line window is not read at all.
///
/// - **Wrapping is the behaviour, not an option.** There is no horizontal
///   scroll and no wrap toggle: the text is laid out to the viewport's width
///   through the one shared fitter, breaking at whitespace, and a newline the
///   user typed is a forced break.
/// - **The caret and selection work in *visual* lines.** Up and Down move
///   between the lines the reader sees and keep the column they started from,
///   Home and End go to the ends of the visual line, and Ctrl+Home/Ctrl+End
///   to the ends of the text. A click lands on the character nearest the
///   pointer on the line it fell on.
/// - **Enter inserts a newline** and reports [`TextAction::Edited`]; it does
///   not submit, because in a box that holds paragraphs Enter is a paragraph.
///   Escape still reports [`TextAction::Cancelled`].
/// - **It scrolls vertically, and shows that it does.** The caret is kept in
///   view as it moves, the wheel and PageUp/PageDown scroll the viewport, and
///   a text longer than the box grows the shared [`ScrollBar`] in a trailing
///   gutter — so a reader can see there is more.
/// - **There is no masked mode.** A credential is a single value, so masking
///   belongs to [`TextField::secret`]; a multi-line masked box would be a
///   credential no one could check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextArea {
    core: FieldCore,
    /// The first wrapped line the viewport shows.
    scroll: usize,
    /// The scrollbar drawn when the text outgrows the viewport. Its own
    /// hover and drag state lives here; the offset it reports is applied to
    /// [`scroll`](Self::scroll), which stays the one answer.
    bar: ScrollBar,
    /// The x a vertical caret move aims at, in pixels from the line's start.
    /// Kept so walking Up and Down through a short line does not strand the
    /// caret at that line's end — nothing drawn reads it.
    goal_x: RenderInvariant<Option<u32>>,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArea {
    /// An empty, resting neutral text area.
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: FieldCore::new(),
            scroll: 0,
            bar: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::new(0, 0, 0), 1, 1),
            ),
            goal_x: RenderInvariant::new(None),
        }
    }

    /// This area with a non-default role (e.g. destructive).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.core.role = role;
        self
    }

    /// This area pre-filled with `text` (caret at the end).
    #[must_use]
    pub fn with_text(mut self, text: impl AsRef<str>) -> Self {
        self.core.editor.set_text(text.as_ref());
        self
    }

    /// This area with placeholder text shown while it is empty.
    #[must_use]
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.core.placeholder = Some(placeholder.into());
        self
    }

    /// This area limited to at most `max` characters (existing content is
    /// truncated to fit).
    #[must_use]
    pub fn with_max_len(mut self, max: usize) -> Self {
        self.core.editor.set_max_len(max);
        self
    }

    /// This area marked read-only: legible and selectable, but not editable.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.core.read_only = read_only;
        self
    }

    /// This area with an inline validation/help message shown below it.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.core.message = Some(message.into());
        self
    }

    /// The area's current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.core.editor.text
    }

    /// Replace the area's text (caret to the end), e.g. after the owner
    /// commits a change. The viewport returns to the top.
    pub fn set_text(&mut self, text: impl AsRef<str>) {
        self.core.editor.set_text(text.as_ref());
        self.scroll = 0;
        *self.goal_x = None;
    }

    /// Whether the area is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.core.read_only
    }

    /// The area's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.core.role
    }

    /// The area's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.core.state
    }

    /// Replace the area's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.core.state = state;
        self.bar.set_state(state);
    }

    /// Set the area's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.core.state.focus.focused = focused;
    }

    /// Set the inline validation/help message (or clear it with `None`).
    pub fn set_message(&mut self, message: Option<String>) {
        self.core.message = message;
    }

    /// The first wrapped line the viewport shows.
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// The height an area showing `rows` whole lines of text needs, message
    /// band included.
    ///
    /// An owner seats a box by how much of the text it wants visible, which
    /// is the only figure it can sensibly choose: how *tall* that is depends
    /// on the theme's type ladder and the DPI scale, and this is where that
    /// arithmetic lives.
    #[must_use]
    pub fn measured_height(&self, rows: u32, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset);
        let edge = plate_border(theme, scale).saturating_add(pad);
        let plate = font
            .line_height()
            .saturating_mul(rows.max(1))
            .saturating_add(edge.saturating_mul(2));
        let message = self.core.message.as_ref().map_or(0, |message| {
            let width = width.saturating_sub(edge.saturating_mul(2));
            pad.saturating_add(
                self.core
                    .message_block(font, width, AREA_MESSAGE_LINES, theme)
                    .height(message),
            )
        });
        plate.saturating_add(message)
    }
}

impl TextArea {
    /// Resolve the area's geometry for `bounds`: the plate, the text
    /// viewport, the scrollbar gutter, and the message band.
    ///
    /// The bar appears only when the text does not fit the rows the box has.
    /// Reserving its gutter narrows the column, which can only *add* wrapped
    /// lines, so a text that overflowed the full width still overflows the
    /// narrowed one — the decision settles in one pass and cannot flicker.
    fn geom(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<AreaGeom> {
        let (x, y, w, h) = surface_rect(bounds)?;
        if w == 0 || h == 0 {
            return None;
        }
        let metrics = theme.metrics();
        let pad = scale.scale_length(metrics.control_inset);
        let edge = plate_border(theme, scale).saturating_add(pad);
        let line = font.line_height().max(1);

        let inner_w = w.saturating_sub(edge.saturating_mul(2));
        // The message takes the lines it needs out of the bottom, but never
        // out of the box itself: a note about the text is worth less than one
        // line of the text.
        let box_floor = edge.saturating_mul(2).saturating_add(line);
        let message_h = self.core.message.as_ref().map_or(0, |message| {
            let wanted = self
                .core
                .message_block(font, inner_w, AREA_MESSAGE_LINES, theme)
                .height(message);
            if wanted == 0 {
                return 0;
            }
            pad.saturating_add(wanted)
                .min(h.saturating_sub(box_floor.min(h)))
        });
        let plate_h = h.saturating_sub(message_h);
        let (tx, ty, full_w, text_h) = (
            x.saturating_add(edge),
            y.saturating_add(edge),
            inner_w,
            plate_h.saturating_sub(edge.saturating_mul(2)),
        );
        let rows = (text_h / line) as usize;
        if rows == 0 || full_w == 0 {
            return None;
        }

        // The overflow test stops one line past the viewport, so a text that
        // fits is never walked beyond what is drawn — and its line count is
        // that same answer. Only a text that does outgrow the box pays for
        // the full count, which is what its proportional thumb needs.
        let breadth = scale.scale_length(metrics.scrollbar_breadth).max(1);
        let probe = font
            .lines_to_width(self.text(), full_w)
            .take(rows.saturating_add(1))
            .count();
        let (text_w, bar) = if probe > rows && full_w > breadth.saturating_mul(2) {
            (
                full_w.saturating_sub(breadth),
                Some(Rect::new(
                    to_i32(tx.saturating_add(full_w).saturating_sub(breadth)),
                    to_i32(ty),
                    breadth,
                    text_h,
                )),
            )
        } else {
            (full_w, None)
        };
        let lines = if probe > rows {
            font.lines_to_width(self.text(), text_w).count()
        } else {
            probe
        };

        let message = self.core.message.as_ref().and_then(|_| {
            (message_h > pad).then_some((
                x.saturating_add(edge),
                y.saturating_add(plate_h).saturating_add(pad),
                inner_w,
                message_h.saturating_sub(pad),
            ))
        });
        Some(AreaGeom {
            plate: (x, y, w, plate_h),
            text: (tx, ty, text_w, text_h),
            rows,
            lines,
            bar,
            message,
        })
    }

    /// The index of the wrapped line the caret sits on, and where that line
    /// starts.
    ///
    /// The lines tile the text, so a caret inside it is on the line whose
    /// span holds it; a caret at the very end is on the **last** line, which
    /// is what puts it on the empty line a trailing newline opens rather than
    /// back at the end of the line before it. Every position therefore
    /// resolves, and to exactly one line.
    fn caret_line(&self, font: BitmapFont, width: u32) -> (usize, usize) {
        let caret = self.core.editor.caret;
        let mut last = (0, 0);
        for (index, line) in font.lines_to_width(self.text(), width).enumerate() {
            last = (index, line.start);
            if caret < line.end() {
                return last;
            }
        }
        last
    }

    /// The byte offset of the visible end of the wrapped line at `index`,
    /// and its start — the two positions Home and End move the caret to.
    fn line_bounds(&self, font: BitmapFont, width: u32, index: usize) -> Option<(usize, usize)> {
        let line = font.lines_to_width(self.text(), width).nth(index)?;
        Some((
            line.start,
            line.start.saturating_add(line.text.trim_end().len()),
        ))
    }

    /// The byte offset nearest `x` pixels along the wrapped line at `index`,
    /// clamped to the line's visible text so a click past the end of a line
    /// does not land on the next one.
    fn byte_at(&self, font: BitmapFont, width: u32, index: usize, x: i32) -> usize {
        let Some(line) = font.lines_to_width(self.text(), width).nth(index) else {
            return self.text().len();
        };
        let visible = line.text.trim_end();
        line.start.saturating_add(byte_from_x(font, visible, x))
    }

    /// The caret's x offset along its own wrapped line, in pixels.
    fn caret_x(&self, font: BitmapFont, width: u32) -> u32 {
        let (_, start) = self.caret_line(font, width);
        let line = self.text().get(start..).unwrap_or_default();
        font.width_to_offset(line, self.core.editor.caret.saturating_sub(start))
    }
}

impl TextArea {
    /// Paint the area into `surface` at `bounds` for the active theme: the
    /// plate, the wrapped text with its selection and caret, the scrollbar
    /// when the text outgrows the box, and the inline message below it.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if withheld(surface, bounds) {
            return;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let Some(geom) = self.geom(bounds, scale, theme, font) else {
            return;
        };
        let (x, y, w, h) = geom.plate;
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(metrics.control_corner_radius).min(h / 2);
        let disposition = self.core.state.disposition();
        // A box the user may write in is a page, exactly as a one-line field
        // is; a read-only one recesses onto the window ground so it reads as
        // text shown rather than text entered.
        let ground = if self.core.editable() {
            Some(palette.document)
        } else if self.core.read_only && disposition != ControlDisposition::DisabledByState {
            Some(palette.surface)
        } else {
            None
        };
        let mut frame = resolve_frame(theme, self.core.role, self.core.state);
        if let Some(ground) = ground {
            frame = frame.grounded_on(Color::from(ground_fill(theme, ground, ChromeLayer::Plate)));
        }
        let rim = match disposition {
            ControlDisposition::Interactive
            | ControlDisposition::NeedsConfirmation
            | ControlDisposition::PendingCheck => match self.core.state.validation {
                ValidationState::Invalid => Color::from(palette.danger),
                ValidationState::Warning => Color::from(palette.warning),
                _ => frame.rim,
            },
            _ => frame.rim,
        };
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: frame.plate,
                rim,
                focused: frame.focused,
                ring: Color::from(palette.rim_active),
            },
        );

        let (tx, ty, tw, th) = geom.text;
        surface.with_clip(tx, ty, tw, th, |surface| {
            self.paint_text(surface, &geom, scale, theme, font, frame.label);
        });

        if let Some(rect) = geom.bar {
            let mut bar = self.bar;
            bar.set_model(self.scroll_model(&geom));
            bar.render(surface, rect, scale, theme);
        }

        if let Some((mx, my, mw, mh)) = geom.message {
            if let Some(message) = &self.core.message {
                self.core
                    .message_block(font, mw, line_budget(font, mh), theme)
                    .paint(surface, message, (mx, my));
            }
        }
    }

    /// The scroll model for `geom`: wrapped lines of content, the rows the
    /// viewport shows, and where in them the viewport sits.
    ///
    /// The unit is the *wrapped line*, so one wheel tick and one arrow step
    /// move the viewport by one line the reader can see rather than by a
    /// pixel count that depends on the face.
    fn scroll_model(&self, geom: &AreaGeom) -> ScrollModel {
        let content = u64::try_from(geom.lines).unwrap_or(u64::MAX);
        let viewport = u64::try_from(geom.rows).unwrap_or(u64::MAX);
        let offset = u64::try_from(geom.clamp_scroll(self.scroll)).unwrap_or(0);
        ScrollModel::new(
            ScrollRange::new(content, viewport, offset),
            1,
            viewport.max(1),
        )
    }

    /// Paint the wrapped text — or the placeholder — with its selection
    /// highlight and caret, from the first visible line down.
    ///
    /// Only the lines the viewport shows are laid out: the layout is a lazy
    /// walk, so a long note costs the lines above the viewport and the lines
    /// in it, never the whole text.
    fn paint_text(
        &self,
        surface: &mut Surface,
        geom: &AreaGeom,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        label: Color,
    ) {
        let (tx, ty, tw, _) = geom.text;
        let palette = theme.palette();
        let line_h = font.line_height().max(1);
        if self.text().is_empty() {
            if let Some(placeholder) = &self.core.placeholder {
                TextBlock::prose(font, tw, geom.rows, Color::from(palette.on_surface_muted)).paint(
                    surface,
                    placeholder,
                    (tx, ty),
                );
            }
        }
        let selection = self.core.editor.selection();
        let scroll = geom.clamp_scroll(self.scroll);
        let caret_w = scale.scale_length(1).max(1);
        // Resolved once: the caret is on one line, and asking each drawn line
        // whether it holds it invites two of them to say yes.
        let caret_row =
            (self.core.show_caret() && selection.is_none()).then(|| self.caret_line(font, tw).0);
        for (row, line) in font
            .lines_to_width(self.text(), tw)
            .enumerate()
            .skip(scroll)
            .take(geom.rows)
        {
            let top = ty.saturating_add(
                u32::try_from(row - scroll)
                    .unwrap_or(0)
                    .saturating_mul(line_h),
            );
            let visible = line.text.trim_end();
            let pen = to_i32(tx);
            if let Some((a, b)) = selection {
                // The highlight covers this line's share of the selection,
                // which for a line wholly inside it is the whole line.
                let from = a.clamp(line.start, line.end());
                let to = b.clamp(line.start, line.end());
                let sa = font.width_to_offset(visible, from.saturating_sub(line.start));
                let sb = font.width_to_offset(visible, to.saturating_sub(line.start));
                if sb > sa {
                    surface.fill_rect(
                        tx.saturating_add(sa),
                        top,
                        sb - sa,
                        line_h,
                        Color::from(palette.accent),
                    );
                }
                let head = visible
                    .get(..from.saturating_sub(line.start).min(visible.len()))
                    .unwrap_or_default();
                let body = visible
                    .get(
                        from.saturating_sub(line.start).min(visible.len())
                            ..to.saturating_sub(line.start).min(visible.len()),
                    )
                    .unwrap_or_default();
                let tail = visible
                    .get(to.saturating_sub(line.start).min(visible.len())..)
                    .unwrap_or_default();
                font.draw_text(surface, pen, to_i32(top), head, label);
                font.draw_text(
                    surface,
                    pen + to_i32(sa),
                    to_i32(top),
                    body,
                    Color::from(palette.on_accent),
                );
                font.draw_text(surface, pen + to_i32(sb), to_i32(top), tail, label);
            } else {
                font.draw_text(surface, pen, to_i32(top), visible, label);
            }
            if caret_row == Some(row) {
                let cx = font
                    .width_to_offset(visible, self.core.editor.caret.saturating_sub(line.start));
                surface.fill_rect(
                    tx.saturating_add(cx.min(tw.saturating_sub(caret_w))),
                    top,
                    caret_w,
                    line_h,
                    Color::from(palette.on_surface),
                );
            }
        }
    }
}

impl TextArea {
    /// Feed a pointer event: a primary press inside the text places the caret
    /// on the line and character under the pointer and starts a selection,
    /// motion while pressed extends it, release ends it, and the wheel
    /// scrolls the viewport without moving the caret. A press on the
    /// scrollbar is the bar's. A denied/disabled/pending area ignores it
    /// (fail closed).
    ///
    /// The area reports `bounds` into `damage` when the event changed what it
    /// draws — the caret, the selection, or the viewport's position.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TextAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.core.pointer = *to;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let geom = self.geom(bounds, scale, theme, font)?;
        let (tx, ty, tw, _) = geom.text;
        let inside = bounds.contains(*self.core.pointer);
        let hover_or_none = if inside {
            PointerState::Hover
        } else {
            PointerState::None
        };

        // The bar owns the gutter and anything its drag is still holding, so
        // a pointer that slid off the thumb keeps scrolling rather than
        // placing a caret.
        if let Some(rect) = geom.bar {
            let on_bar = rect.contains(*self.core.pointer);
            if on_bar
                || matches!(event, InputEvent::PointerScrolled { .. })
                || self.bar.is_pressing()
            {
                self.bar.set_model(self.scroll_model(&geom));
                let scrolled = match event {
                    InputEvent::PointerScrolled { dx, dy } if inside => {
                        self.bar.wheel(*dx, *dy, rect, damage)
                    }
                    _ => self.bar.on_pointer(event, rect, scale, theme, damage),
                };
                if let Some(ScrollAction::ScrollTo { offset }) = scrolled {
                    self.scroll_to(
                        usize::try_from(offset).unwrap_or(usize::MAX),
                        bounds,
                        damage,
                    );
                }
                if on_bar || self.bar.is_pressing() {
                    return None;
                }
            }
        }

        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if inside && self.core.actionable() {
                    *self.core.selecting = true;
                    damage::set(
                        &mut self.core.state.pointer,
                        PointerState::Pressed,
                        bounds,
                        damage,
                    );
                    self.place_at_pointer(&geom, font, (tx, ty, tw), false, bounds, damage);
                }
                None
            }
            InputEvent::PointerMoved { .. } => {
                if *self.core.selecting {
                    self.place_at_pointer(&geom, font, (tx, ty, tw), true, bounds, damage);
                } else {
                    damage::set(&mut self.core.state.pointer, hover_or_none, bounds, damage);
                }
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                *self.core.selecting = false;
                damage::set(&mut self.core.state.pointer, hover_or_none, bounds, damage);
                None
            }
            _ => None,
        }
    }

    /// Place the caret at the pointer, extending the selection when `select`.
    fn place_at_pointer(
        &mut self,
        geom: &AreaGeom,
        font: BitmapFont,
        text: (u32, u32, u32),
        select: bool,
        bounds: Rect,
        damage: &mut Region,
    ) {
        let (tx, ty, tw) = text;
        let line_h = font.line_height().max(1);
        let down = self.core.pointer.y.saturating_sub(to_i32(ty)).max(0);
        let row = geom
            .clamp_scroll(self.scroll)
            .saturating_add((u32::try_from(down).unwrap_or(0) / line_h) as usize)
            .min(geom.lines.saturating_sub(1));
        let byte = self.byte_at(font, tw, row, self.core.pointer.x - to_i32(tx));
        self.core.edit(bounds, damage, |editor| {
            editor.place_caret(byte, select);
            false
        });
        *self.goal_x = None;
        self.reveal_caret(geom, font, bounds, damage);
    }

    /// Feed a key event.
    ///
    /// Printable keys insert (replacing any selection) and **Enter inserts a
    /// newline**; Backspace/Delete remove; Left/Right move by character and
    /// Up/Down by *visual* line, keeping the column they set out from;
    /// Home/End go to the ends of the visual line and Ctrl+Home/Ctrl+End to
    /// the ends of the text; PageUp/PageDown move by a viewport; Ctrl+A
    /// selects all; Shift extends the selection with every one of those
    /// moves; Escape reports [`TextAction::Cancelled`]. Editing keys need an
    /// editable (not read-only, not denied) area.
    ///
    /// A key that edits the text or moves the caret or the viewport reports
    /// `bounds`; one that moves a caret already at the end it moves toward
    /// reports nothing.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TextAction> {
        if !self.core.state.focus.focused || !self.core.actionable() {
            return None;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let geom = self.geom(bounds, scale, theme, font)?;
        let width = geom.text.2;
        let action = match key {
            Key::Named(NamedKey::Enter) if self.core.editable() => self
                .core
                .edit(bounds, damage, |editor| editor.insert_char('\n'))
                .then_some(TextAction::Edited),
            Key::Named(NamedKey::Up | NamedKey::Down) => {
                self.move_line(&geom, font, key, modifiers.shift, bounds, damage);
                None
            }
            Key::Named(NamedKey::PageUp | NamedKey::PageDown) => {
                self.move_page(&geom, font, key, modifiers.shift, bounds, damage);
                None
            }
            Key::Named(NamedKey::Home | NamedKey::End) if !modifiers.ctrl => {
                let (row, _) = self.caret_line(font, width);
                let ends = self.line_bounds(font, width, row);
                if let Some((start, end)) = ends {
                    let to = if key == Key::Named(NamedKey::Home) {
                        start
                    } else {
                        end
                    };
                    self.core.edit(bounds, damage, |editor| {
                        editor.place_caret(to, modifiers.shift);
                        false
                    });
                }
                *self.goal_x = None;
                None
            }
            _ => {
                let action = self
                    .core
                    .on_key(key, modifiers, false, bounds, damage)
                    .filter(|action| *action != TextAction::Submitted);
                *self.goal_x = None;
                action
            }
        };
        self.reveal_caret(&geom, font, bounds, damage);
        action
    }

    /// Move the caret one visual line up or down, keeping the column it set
    /// out from so a walk through a short line does not strand it there.
    fn move_line(
        &mut self,
        geom: &AreaGeom,
        font: BitmapFont,
        key: Key,
        select: bool,
        bounds: Rect,
        damage: &mut Region,
    ) {
        let width = geom.text.2;
        let goal = self.goal_x.unwrap_or_else(|| self.caret_x(font, width));
        let (row, _) = self.caret_line(font, width);
        let next = if key == Key::Named(NamedKey::Up) {
            row.checked_sub(1)
        } else {
            (row + 1 < geom.lines).then_some(row + 1)
        };
        if let Some(next) = next {
            let byte = self.byte_at(font, width, next, to_i32(goal));
            self.core.edit(bounds, damage, |editor| {
                editor.place_caret(byte, select);
                false
            });
        }
        *self.goal_x = Some(goal);
    }

    /// Move the caret a viewport's worth of lines, keeping its column.
    fn move_page(
        &mut self,
        geom: &AreaGeom,
        font: BitmapFont,
        key: Key,
        select: bool,
        bounds: Rect,
        damage: &mut Region,
    ) {
        let width = geom.text.2;
        let goal = self.goal_x.unwrap_or_else(|| self.caret_x(font, width));
        let (row, _) = self.caret_line(font, width);
        let next = if key == Key::Named(NamedKey::PageUp) {
            row.saturating_sub(geom.rows)
        } else {
            row.saturating_add(geom.rows)
                .min(geom.lines.saturating_sub(1))
        };
        let byte = self.byte_at(font, width, next, to_i32(goal));
        self.core.edit(bounds, damage, |editor| {
            editor.place_caret(byte, select);
            false
        });
        *self.goal_x = Some(goal);
    }

    /// Scroll so the caret's line is inside the viewport, reporting `bounds`
    /// when the viewport actually moved.
    ///
    /// The viewport follows the caret rather than the caret being confined to
    /// the viewport: typing at the end of a long note brings the end into
    /// view, which is what makes the box usable at all.
    fn reveal_caret(
        &mut self,
        geom: &AreaGeom,
        font: BitmapFont,
        bounds: Rect,
        damage: &mut Region,
    ) {
        let (row, _) = self.caret_line(font, geom.text.2);
        let first = geom.clamp_scroll(self.scroll);
        let want = if row < first {
            row
        } else if row >= first.saturating_add(geom.rows) {
            row.saturating_sub(geom.rows.saturating_sub(1))
        } else {
            first
        };
        self.set_scroll(geom.clamp_scroll(want), bounds, damage);
    }

    /// Scroll the viewport to `offset` wrapped lines, clamped to the text.
    fn scroll_to(&mut self, offset: usize, bounds: Rect, damage: &mut Region) {
        self.set_scroll(offset, bounds, damage);
    }

    /// Adopt `offset` as the viewport's position, reporting `bounds` when it
    /// changed what is drawn.
    fn set_scroll(&mut self, offset: usize, bounds: Rect, damage: &mut Region) {
        if self.scroll != offset {
            self.scroll = offset;
            damage.add(bounds);
        }
    }
}

/// Test-only: a [`TextArea`]'s laid-out geometry for `bounds` — the text
/// viewport, the scrollbar's rectangle where the text outgrew it, and the
/// rows and wrapped lines behind that decision.
///
/// Taken from the exact layout [`TextArea::render`] draws and
/// [`TextArea::on_pointer`] hit-tests through, so a test aiming a click at a
/// line, or looking for the bar's gutter, cannot drift from where the box
/// actually put them.
#[cfg(test)]
pub(crate) fn debug_area_layout(
    area: &TextArea,
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
) -> Option<(Rect, Option<Rect>, usize, usize)> {
    let font = role_font(theme, scale, TextRole::Body);
    let geom = area.geom(bounds, scale, theme, font)?;
    let (tx, ty, tw, th) = geom.text;
    Some((
        Rect::new(to_i32(tx), to_i32(ty), tw, th),
        geom.bar,
        geom.rows,
        geom.lines,
    ))
}
