//! The text-entry family: [`TextField`] and [`SearchField`] (spec §11.8).
//!
//! Both are single-line text controls built on a quiet Alloy Plate with a clear
//! focus ring, a caret, selection, and horizontally-scrolled clipped text. A
//! [`TextField`] is the general single-line entry; a [`SearchField`] is the same
//! editor behind a leading magnifier that reads as *active* when a query is
//! present (spec §11.8). Both resolve every colour/metric/radius from the active
//! [`Theme`] and [`Scale`], round their plate through the shared drawing core
//! the button/selector/value families use, and emit a typed [`TextAction`] — the
//! owning service enforces authority (`AGENTS.md` §5.4).
//!
//! A read-only field is enabled and legible (its text stays full-contrast and
//! selectable for copy) but refuses edits; that is deliberately distinct from a
//! disabled field (muted plate and text) and from an authority-denied field
//! (which keeps its value and shows an Authority Mark), per spec §13.

use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    paint_bead, paint_plate, plate_border, resolve_bead, resolve_frame, surface_rect, to_i32,
    PlateStyle,
};
use crate::state::{ControlDisposition, ControlRole, ControlState, PointerState, ValidationState};

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

/// A single-line text buffer with a caret and a selection.
///
/// The [`caret`](Self::caret) and [`anchor`](Self::anchor) are byte indices
/// that always land on a `char` boundary of [`text`](Self::text); the selection
/// is the (possibly empty) range between them. Editing operations clamp to the
/// optional character limit and can never leave the caret mid-scalar, so a
/// renderer never has to defend against an invalid index (illegal states
/// unrepresentable, `AGENTS.md` §2.11).
#[derive(Clone, Debug, Eq, PartialEq)]
struct TextEditor {
    text: String,
    caret: usize,
    anchor: usize,
    max_len: Option<usize>,
}

impl TextEditor {
    /// An empty editor with no character limit.
    fn new() -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: 0,
            max_len: None,
        }
    }

    /// Replace the whole buffer, placing the caret at the end and collapsing
    /// the selection. The text is truncated to any character limit.
    fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        if let Some(max) = self.max_len {
            self.truncate_to_len(max);
        }
        self.caret = self.text.len();
        self.anchor = self.caret;
    }

    /// Drop trailing characters until the buffer holds at most `max` scalars.
    fn truncate_to_len(&mut self, max: usize) {
        if let Some((idx, _)) = self.text.char_indices().nth(max) {
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
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            return false;
        };
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
    fn clear(&mut self) -> bool {
        let changed = !self.text.is_empty();
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
        if below >= glyph_h.saturating_add(pad) {
            let my = y + row_h + pad;
            let mw = w.saturating_sub(edge.saturating_mul(2));
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

/// The pixel x of the `char` boundary at byte `idx` within `text`.
fn caret_px(font: BitmapFont, text: &str, idx: usize) -> u32 {
    font.text_width(&text[..idx.min(text.len())])
}

/// The horizontal text scroll (pixels hidden at the left) that keeps the caret
/// visible: zero until the caret would pass the right edge, then just enough to
/// pin the caret to that edge. Deterministic from the caret alone, so `render`
/// needs no stored scroll state.
fn text_scroll(font: BitmapFont, text: &str, caret: usize, avail_w: u32) -> u32 {
    caret_px(font, text, caret).saturating_sub(avail_w)
}

/// The byte index whose `char` boundary is nearest text-space x `rel` (pixels
/// from the text start, i.e. pointer-x minus the text origin plus the scroll).
fn byte_from_x(font: BitmapFont, text: &str, rel: i32) -> usize {
    let rel = u32::try_from(rel.max(0)).unwrap_or(u32::MAX);
    let mut best_byte = 0;
    let mut best_dist = rel;
    let mut end = 0;
    for ch in text.chars() {
        end += ch.len_utf8();
        let dist = caret_px(font, text, end).abs_diff(rel);
        if dist <= best_dist {
            best_dist = dist;
            best_byte = end;
        }
    }
    best_byte
}

/// The shared single-line field: editor, role, composed state, read-only flag,
/// placeholder, and inline message, plus the render and input behaviour every
/// text control reuses. [`TextField`] and [`SearchField`] wrap one of these so
/// the editing model, clipped scrolling, caret/selection drawing, and the §13
/// disposition rendering are defined once (`AGENTS.md` §2.2).
#[derive(Clone, Debug, Eq, PartialEq)]
struct FieldCore {
    editor: TextEditor,
    role: ControlRole,
    state: ControlState,
    read_only: bool,
    placeholder: Option<String>,
    message: Option<String>,
    pointer: Point,
    selecting: bool,
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
            pointer: Point::ORIGIN,
            selecting: false,
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
        let frame = resolve_frame(theme, self.role, self.state);

        // A read-only field recesses its plate (surface, not surface_raised)
        // while keeping full-contrast text, so it reads differently from a
        // muted disabled field and from a denied field's Authority Mark.
        let plate = if self.read_only && disposition != ControlDisposition::DisabledByState {
            Color::from(palette.surface)
        } else {
            frame.plate
        };

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
                plate,
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
                let fitted = font.truncate_to_width(placeholder, avail_w);
                font.draw_text(
                    &mut layer,
                    0,
                    baseline,
                    fitted,
                    Color::from(palette.on_surface_muted),
                );
            }
        } else {
            let scroll = text_scroll(font, text, self.editor.caret, avail_w);
            let base_x = -to_i32(scroll);
            if let Some((a, b)) = self.editor.selection() {
                let sa = to_i32(caret_px(font, text, a)) + base_x;
                let sb = to_i32(caret_px(font, text, b)) + base_x;
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

        if self.show_caret() && self.editor.selection().is_none() {
            let scroll = text_scroll(font, text, self.editor.caret, avail_w);
            let cx = to_i32(caret_px(font, text, self.editor.caret)) - to_i32(scroll);
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

    /// Paint the inline validation/help message below the field, coloured by
    /// the validation state (danger/warning), else a quiet hint.
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
        let Some((mx, my, mw, _)) = geom.message else {
            return;
        };
        if mw == 0 {
            return;
        }
        let palette = theme.palette();
        let color = match self.state.validation {
            ValidationState::Invalid => Color::from(palette.danger),
            ValidationState::Warning => Color::from(palette.warning),
            _ => Color::from(palette.on_surface_muted),
        };
        let fitted = font.truncate_to_width(message, mw);
        font.draw_text(surface, to_i32(mx), to_i32(my), fitted, color);
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
        font: BitmapFont,
        leading: u32,
    ) -> Option<TextAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let geom = field_geom(bounds, scale, theme, font, leading)?;
        let inside = bounds.contains(self.pointer);
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if inside && self.actionable() {
                    self.selecting = true;
                    self.state.pointer = PointerState::Pressed;
                    let byte = self.byte_at(geom.text_x0, geom.avail_w, font);
                    self.editor.place_caret(byte, false);
                }
                None
            }
            InputEvent::PointerMoved { .. } => {
                if self.selecting {
                    let byte = self.byte_at(geom.text_x0, geom.avail_w, font);
                    self.editor.place_caret(byte, true);
                } else {
                    self.state.pointer = if inside {
                        PointerState::Hover
                    } else {
                        PointerState::None
                    };
                }
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                self.selecting = false;
                self.state.pointer = if inside {
                    PointerState::Hover
                } else {
                    PointerState::None
                };
                None
            }
            _ => None,
        }
    }

    /// The byte index the current pointer x maps to within the text region.
    fn byte_at(&self, text_x0: u32, avail_w: u32, font: BitmapFont) -> usize {
        let text = self.editor.text.as_str();
        let scroll = text_scroll(font, text, self.editor.caret, avail_w);
        let rel = self.pointer.x - to_i32(text_x0) + to_i32(scroll);
        byte_from_x(font, text, rel)
    }

    /// Feed a key event. Editing keys require an editable field; navigation and
    /// selection require an actionable one; Enter/Escape report submit/cancel.
    /// `clear_on_escape` clears a non-empty buffer first (a search field).
    fn on_key(&mut self, key: Key, mods: Modifiers, clear_on_escape: bool) -> Option<TextAction> {
        if !self.state.focus.focused || !self.actionable() {
            return None;
        }
        match key {
            Key::Char('a' | 'A') if mods.ctrl => {
                self.editor.select_all();
                None
            }
            Key::Char(ch) if self.editable() && !mods.ctrl && !mods.alt && !mods.meta => {
                if ch.is_control() {
                    return None;
                }
                self.editor.insert_char(ch).then_some(TextAction::Edited)
            }
            Key::Named(NamedKey::Backspace) if self.editable() => {
                self.editor.backspace().then_some(TextAction::Edited)
            }
            Key::Named(NamedKey::Delete) if self.editable() => {
                self.editor.delete_forward().then_some(TextAction::Edited)
            }
            Key::Named(NamedKey::Left) => {
                self.editor.move_left(mods.shift);
                None
            }
            Key::Named(NamedKey::Right) => {
                self.editor.move_right(mods.shift);
                None
            }
            Key::Named(NamedKey::Home) => {
                self.editor.home(mods.shift);
                None
            }
            Key::Named(NamedKey::End) => {
                self.editor.end(mods.shift);
                None
            }
            Key::Named(NamedKey::Enter) => Some(TextAction::Submitted),
            Key::Named(NamedKey::Escape) => {
                if clear_on_escape && self.editor.clear() {
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
/// validates the value and enforces authority (`AGENTS.md` §5.4). A read-only
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
        self.core.editor.max_len = Some(max);
        self.core.editor.truncate_to_len(max);
        self.core.editor.caret = self.core.editor.text.len();
        self.core.editor.anchor = self.core.editor.caret;
        self
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
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        self.core.render(surface, bounds, scale, theme, font, 0);
    }

    /// Feed a pointer event: a primary press positions the caret under the
    /// pointer and starts a selection, motion while pressed extends it, and
    /// release ends it. A denied/disabled/pending field ignores it (fail
    /// closed).
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<TextAction> {
        self.core.on_pointer(event, bounds, scale, theme, font, 0)
    }

    /// Feed a key event: printable keys insert (replacing any selection),
    /// Backspace/Delete remove, arrows/Home/End move the caret (Shift extends
    /// the selection), Ctrl+A selects all, Enter submits, and Escape cancels.
    /// Editing keys require an editable (not read-only, not denied) field.
    pub fn on_key(&mut self, key: Key, modifiers: Modifiers) -> Option<TextAction> {
        self.core.on_key(key, modifiers, false)
    }
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
/// and rendering core (`AGENTS.md` §2.2).
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
        self.core.editor.max_len = Some(max);
        self.core.editor.truncate_to_len(max);
        self.core.editor.caret = self.core.editor.text.len();
        self.core.editor.anchor = self.core.editor.caret;
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
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
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
        font: BitmapFont,
    ) -> Option<TextAction> {
        let leading = Self::leading(bounds, scale, theme, font);
        self.core
            .on_pointer(event, bounds, scale, theme, font, leading)
    }

    /// Feed a key event; Escape clears a non-empty query before dismissing.
    pub fn on_key(&mut self, key: Key, modifiers: Modifiers) -> Option<TextAction> {
        self.core.on_key(key, modifiers, true)
    }
}
