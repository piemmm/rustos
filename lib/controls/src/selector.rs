//! The boolean-selector family: [`Toggle`], [`Checkbox`], and [`Radio`].
//!
//! Each is a labelled boolean control that reads by *shape* as well as colour
//! (spec §11.4–§11.5): a toggle's thumb slides to the active side, a checkbox
//! draws a filled square when on and a horizontal bar when mixed, and a radio
//! draws a centre bead when selected. They share the button family's plate
//! helpers (`crate::paint`) and press/keyboard interaction model rather than a
//! second copy, resolve every colour/metric/radius from the active [`Theme`]
//! and [`Scale`], and emit a typed [`SelectorAction`] on activation — the
//! owning service applies the change and enforces authority. A denied selector
//! keeps its value and shows an Authority Mark rather than silently looking
//! disabled (spec §13).
//!
//! The glyph itself is compact — the theme's `selector_extent`, centred in the
//! row the owner lays the control out in — so the row stays a comfortable hit
//! target and a keyboard focus band while the mark stays small.

use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::paint::{
    key_activation, paint_bead, paint_plate, plate_border, pointer_activation, resolve_bead,
    resolve_frame, resolve_mark, resolve_rail, role_font, surface_rect, to_i32, PlateStyle,
};
use crate::state::{
    ControlDisposition, ControlRole, ControlState, RenderInvariant, SelectionState,
};

/// The outcome of activating a boolean selector.
///
/// A selector never applies its own change: it reports the value it would take
/// and the owning container performs the mutation under the caller's authority
/// A toggle or checkbox requests the flipped value; a mixed
/// checkbox resolves to `on = true`; a radio always requests `on = true`, since
/// a radio is cleared by selecting a sibling, never by clicking itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SelectorAction {
    /// The selector was activated and requests its value become `on`.
    Set {
        /// The requested new on/off value.
        on: bool,
    },
}

/// The glyph rectangle of a boolean selector: `logical_w` x `logical_h`
/// logical pixels resolved for density, clamped into `bounds`, and centred
/// vertically within the row.
///
/// A selector's mark is a compact glyph beside its label, never a
/// control-height plate: the row keeps a full-height hit target while the
/// glyph keeps the proportions the theme fixes.
fn glyph_rect(
    bounds: Rect,
    scale: Scale,
    logical_w: u32,
    logical_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (x, y, w, h) = surface_rect(bounds)?;
    let gw = scale.scale_length(logical_w).max(1).min(w);
    let gh = scale.scale_length(logical_h).max(1).min(h);
    if gw == 0 || gh == 0 {
        return None;
    }
    Some((x, y + (h - gh) / 2, gw, gh))
}

/// The track rectangle of a toggle: the theme's toggle track length by the
/// shared selector extent, clamped to the row and centred within it.
pub(crate) fn toggle_track_rect(
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
) -> Option<(u32, u32, u32, u32)> {
    let metrics = theme.metrics();
    glyph_rect(
        bounds,
        scale,
        metrics.toggle_track_length,
        metrics.selector_extent,
    )
}

/// The square glyph rectangle of a checkbox box or radio circle: the theme's
/// selector extent in both axes, clamped to the row and centred within it.
pub(crate) fn square_glyph_rect(
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
) -> Option<(u32, u32, u32, u32)> {
    let extent = theme.metrics().selector_extent;
    let (x, y, w, h) = glyph_rect(bounds, scale, extent, extent)?;
    let side = w.min(h);
    Some((x, y + (h - side) / 2, side, side))
}

/// Paint the label after the leading box, vertically centred, in `color`,
/// truncated to the remaining width.
fn paint_label(
    surface: &mut Surface,
    bounds: Rect,
    box_w: u32,
    gap: u32,
    font: BitmapFont,
    text: &str,
    color: Color,
) {
    let Some((x, y, _, h)) = surface_rect(bounds) else {
        return;
    };
    let start = box_w.saturating_add(gap.max(1));
    let avail = bounds.width.saturating_sub(start);
    if avail == 0 {
        return;
    }
    let fitted = font.truncate_to_width(text, avail);
    let glyph_h = font.glyph_height();
    let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
    font.draw_text(surface, to_i32(x) + to_i32(start), text_y, fitted, color);
}

/// Paint the overlay signals shared by every selector, drawn *after* the glyph
/// so nothing hides them: the Pressure Rail (leading edge, full height), the
/// pending Heat Seam (lower edge of the glyph while a check is pending, spec
/// §11.4), and the Signal Bead (trailing-top corner). `glyph` is the leading
/// glyph rectangle the seam spans.
fn paint_selector_signals(
    surface: &mut Surface,
    bounds: Rect,
    glyph: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    state: ControlState,
) {
    let Some((x, y, w, h)) = surface_rect(bounds) else {
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    let metrics = theme.metrics();
    let border = plate_border(theme, scale);

    if let Some(color) = resolve_rail(theme, state) {
        let rail_w = scale.scale_length(metrics.rail_thickness).max(1).min(w);
        surface.fill_rect(x, y, rail_w, h, color);
    }

    if state.disposition() == ControlDisposition::PendingCheck {
        let (glyph_x, glyph_y, glyph_w, glyph_h) = glyph;
        let seam_h = scale
            .scale_length(metrics.seam_thickness)
            .max(1)
            .min(glyph_h.saturating_sub(border.saturating_mul(2)));
        let seam_w = glyph_w.saturating_sub(border.saturating_mul(2));
        if seam_h > 0 && seam_w > 0 {
            let seam_y = glyph_y + glyph_h - border - seam_h;
            surface.fill_rect(
                glyph_x + border,
                seam_y,
                seam_w,
                seam_h,
                Color::from(theme.palette().rim_active),
            );
        }
    }

    if let Some((color, shape)) = resolve_bead(theme, state) {
        let size = scale.scale_length(metrics.bead_size).max(3).min(w).min(h);
        let bx = x + w - size;
        paint_bead(surface, bx, y, size, color, shape);
    }
}

/// Paint the leading box plate (rim + Alloy Plate + focus ring) for a selector
/// glyph.
///
/// Returns the inner content rectangle (inside the rim border) the mark is
/// drawn within, or `None` when the box collapses. The pending Heat Seam is an
/// overlay drawn later by `paint_selector_signals`, so a toggle's contact
/// cannot hide it.
#[allow(clippy::too_many_arguments)]
fn paint_box(
    surface: &mut Surface,
    box_rect: (u32, u32, u32, u32),
    radius: u32,
    scale: Scale,
    theme: &Theme,
    role: ControlRole,
    state: ControlState,
) -> Option<(u32, u32, u32, u32)> {
    let (_, _, w, h) = box_rect;
    if w == 0 || h == 0 {
        return None;
    }
    let frame = resolve_frame(theme, role, state);
    let border = plate_border(theme, scale);
    let ring = Color::from(theme.palette().rim_active);
    paint_plate(
        surface,
        box_rect,
        &PlateStyle {
            radius,
            border,
            plate: frame.plate,
            rim: frame.rim,
            focused: frame.focused,
            ring,
        },
    );

    inner_rect(box_rect, border)
}

/// The content rectangle inside a plate's rim border, or `None` when the plate
/// collapses under its own border.
fn inner_rect(box_rect: (u32, u32, u32, u32), border: u32) -> Option<(u32, u32, u32, u32)> {
    let (x, y, w, h) = box_rect;
    let iw = w.checked_sub(border.saturating_mul(2))?;
    let ih = h.checked_sub(border.saturating_mul(2))?;
    if iw == 0 || ih == 0 {
        return None;
    }
    Some((x + border, y + border, iw, ih))
}

/// The rectangle a square selector's value mark fills for `bounds` under the
/// active theme and density: the glyph, inside its rim border, inset by the
/// mark margin. Composed from the same helpers `render` paints through, so a
/// test probing the mark cannot drift from what is drawn.
#[cfg(test)]
pub(crate) fn selector_mark_rect(
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
) -> Option<(u32, u32, u32, u32)> {
    let glyph = square_glyph_rect(bounds, scale, theme)?;
    mark_rect(inner_rect(glyph, plate_border(theme, scale))?)
}

/// Inset a plate rectangle by a small proportional margin so a glyph mark
/// (checkbox square, radio bead) sits within the plate with the rim still
/// visible around it — a fraction of the plate, never a hard-coded gap, so it
/// scales with density. Returns `None` if the mark would collapse.
fn mark_rect(inner: (u32, u32, u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let (ix, iy, iw, ih) = inner;
    let margin = (iw.min(ih) / 5).max(1);
    let mw = iw.checked_sub(margin.saturating_mul(2))?;
    let mh = ih.checked_sub(margin.saturating_mul(2))?;
    if mw == 0 || mh == 0 {
        return None;
    }
    Some((ix + margin, iy + margin, mw, mh))
}

/// The label, role, composed state, and pointer/press latch shared by every
/// boolean selector.
///
/// The three families differ only in their value semantics and their glyph;
/// their focus/pointer/keyboard plumbing is identical, so it lives here once
/// (the press-latch and Space/Enter activation both come from `crate::paint`).
///
/// Sharing the core also gives all three selectors the same render-equivalence
/// equality: the label, role, and visible state compare, while the pointer
/// coordinate and press latch — hit-testing bookkeeping no render path reads
/// — do not.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectorCore {
    label: String,
    role: ControlRole,
    state: ControlState,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The press latch; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl SelectorCore {
    fn new(label: String, role: ControlRole) -> Self {
        Self {
            label,
            role,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// Feed a pointer event and report whether a primary click completed over
    /// the control's `bounds`.
    fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> bool {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        pointer_activation(&mut self.state, &mut self.armed, event, inside)
    }

    /// Report whether a focused, actionable selector is activated by `key`.
    fn on_key(&self, key: Key) -> bool {
        key_activation(self.state, key)
    }
}

/// A two-state powered contact: a track (Alloy Plate) with a thumb that slides
/// to the active side, and an accent contact filling the active side (spec
/// §11.4). A denied toggle keeps its value and shows an Authority Mark; a
/// pending toggle shows a Heat Seam while the backing service confirms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Toggle {
    core: SelectorCore,
    on: bool,
}

impl Toggle {
    /// A toggle with the given label and initial value, in the neutral role.
    #[must_use]
    pub fn new(label: impl Into<String>, on: bool) -> Self {
        Self {
            core: SelectorCore::new(label.into(), ControlRole::Neutral),
            on,
        }
    }

    /// This toggle with a non-default role (e.g. destructive or system).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.core.role = role;
        self
    }

    /// Whether the toggle is on.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Set the toggle's value (e.g. after the owner applies the change).
    pub fn set_on(&mut self, on: bool) {
        self.on = on;
    }

    /// The toggle's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.core.label
    }

    /// The toggle's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.core.role
    }

    /// The toggle's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.core.state
    }

    /// Replace the toggle's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.core.state = state;
    }

    /// Set the toggle's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.core.state.focus.focused = focused;
    }

    /// Paint the toggle into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let metrics = theme.metrics();
        let Some(track) = toggle_track_rect(bounds, scale, theme) else {
            return;
        };
        let (_, _, track_w, track_h) = track;
        let inner = paint_box(
            surface,
            track,
            track_h / 2,
            scale,
            theme,
            self.core.role,
            self.core.state,
        );

        if let Some((ix, iy, iw, ih)) = inner {
            let thumb_d = ih.min(iw);
            let thumb_x = if self.on {
                ix + iw.saturating_sub(thumb_d)
            } else {
                ix
            };
            if self.on {
                // The accent contact fills the active side behind the thumb.
                let contact_w = thumb_x.saturating_sub(ix).saturating_add(thumb_d / 2);
                if contact_w > 0 {
                    surface.fill_round_rect(
                        ix,
                        iy,
                        contact_w,
                        ih,
                        ih / 2,
                        resolve_mark(theme, self.core.role, self.core.state),
                    );
                }
            }
            let palette = theme.palette();
            let thumb_rgba = if self.on {
                palette.on_accent
            } else {
                palette.on_surface_muted
            };
            surface.fill_round_rect(
                thumb_x,
                iy,
                thumb_d,
                thumb_d,
                thumb_d / 2,
                Color::from(thumb_rgba),
            );
        }

        let label_color = resolve_frame(theme, self.core.role, self.core.state).label;
        let gap = scale.scale_length(metrics.control_gap);
        paint_label(
            surface,
            bounds,
            track_w,
            gap,
            font,
            &self.core.label,
            label_color,
        );
        paint_selector_signals(surface, bounds, track, scale, theme, self.core.state);
    }

    /// Feed a pointer event; a completed primary click over an actionable
    /// toggle requests the flipped value.
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<SelectorAction> {
        self.core
            .on_pointer(event, bounds)
            .then_some(SelectorAction::Set { on: !self.on })
    }

    /// Feed a key event; Space/Enter on a focused, actionable toggle requests
    /// the flipped value.
    pub fn on_key(&mut self, key: Key) -> Option<SelectorAction> {
        self.core
            .on_key(key)
            .then_some(SelectorAction::Set { on: !self.on })
    }
}

/// A boolean selector with a shape mark (spec §11.5): a filled square when
/// checked, a horizontal bar when mixed, empty when unchecked — legible
/// without colour. Activation requests checked from unchecked or mixed, and
/// unchecked from checked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkbox {
    core: SelectorCore,
    selection: SelectionState,
}

impl Checkbox {
    /// A checkbox with the given label and initial selection, neutral role.
    #[must_use]
    pub fn new(label: impl Into<String>, selection: SelectionState) -> Self {
        Self {
            core: SelectorCore::new(label.into(), ControlRole::Neutral),
            selection,
        }
    }

    /// This checkbox with a non-default role.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.core.role = role;
        self
    }

    /// The checkbox's selection (unchecked / checked / mixed).
    #[must_use]
    pub fn selection(&self) -> SelectionState {
        self.selection
    }

    /// Set the checkbox's selection (e.g. after the owner applies the change).
    pub fn set_selection(&mut self, selection: SelectionState) {
        self.selection = selection;
    }

    /// The checkbox's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.core.label
    }

    /// The checkbox's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.core.role
    }

    /// The checkbox's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.core.state
    }

    /// Replace the checkbox's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.core.state = state;
    }

    /// Set the checkbox's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.core.state.focus.focused = focused;
    }

    /// The value a checkbox activation requests: checked, unless it is already
    /// checked (then unchecked). A mixed checkbox resolves to checked.
    fn next_on(&self) -> bool {
        !matches!(self.selection, SelectionState::Selected)
    }

    /// Paint the checkbox into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some(glyph) = square_glyph_rect(bounds, scale, theme) else {
            return;
        };
        let side = glyph.2;
        let radius = scale
            .scale_length(theme.metrics().control_corner_radius)
            .min(side / 2);
        let inner = paint_box(
            surface,
            glyph,
            radius,
            scale,
            theme,
            self.core.role,
            self.core.state,
        );

        if let Some((ix, iy, iw, ih)) = inner.and_then(mark_rect) {
            let color = resolve_mark(theme, self.core.role, self.core.state);
            match self.selection {
                SelectionState::Selected | SelectionState::Current => {
                    surface.fill_rect(ix, iy, iw, ih, color);
                }
                SelectionState::Mixed => {
                    let bar_h = (ih / 3).max(1);
                    let bar_y = iy + (ih.saturating_sub(bar_h)) / 2;
                    surface.fill_rect(ix, bar_y, iw, bar_h, color);
                }
                SelectionState::Unselected => {}
            }
        }

        let label_color = resolve_frame(theme, self.core.role, self.core.state).label;
        let gap = scale.scale_length(theme.metrics().control_gap);
        paint_label(
            surface,
            bounds,
            side,
            gap,
            font,
            &self.core.label,
            label_color,
        );
        paint_selector_signals(surface, bounds, glyph, scale, theme, self.core.state);
    }

    /// Feed a pointer event; a completed primary click requests the next value.
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<SelectorAction> {
        self.core
            .on_pointer(event, bounds)
            .then_some(SelectorAction::Set { on: self.next_on() })
    }

    /// Feed a key event; Space/Enter requests the next value.
    pub fn on_key(&mut self, key: Key) -> Option<SelectorAction> {
        self.core
            .on_key(key)
            .then_some(SelectorAction::Set { on: self.next_on() })
    }
}

/// A one-of-many selector (spec §11.5): a circular box with a centre bead when
/// selected. A radio is cleared by selecting a sibling, never by clicking
/// itself, so activation always requests selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Radio {
    core: SelectorCore,
    selected: bool,
}

impl Radio {
    /// A radio with the given label and initial selection, neutral role.
    #[must_use]
    pub fn new(label: impl Into<String>, selected: bool) -> Self {
        Self {
            core: SelectorCore::new(label.into(), ControlRole::Neutral),
            selected,
        }
    }

    /// This radio with a non-default role.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.core.role = role;
        self
    }

    /// Whether the radio is the selected one of its group.
    #[must_use]
    pub fn is_selected(&self) -> bool {
        self.selected
    }

    /// Set the radio's selection (e.g. after the owner applies the change).
    pub fn set_selected(&mut self, selected: bool) {
        self.selected = selected;
    }

    /// The radio's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.core.label
    }

    /// The radio's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.core.role
    }

    /// The radio's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.core.state
    }

    /// Replace the radio's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.core.state = state;
    }

    /// Set the radio's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.core.state.focus.focused = focused;
    }

    /// Paint the radio into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some(glyph) = square_glyph_rect(bounds, scale, theme) else {
            return;
        };
        let side = glyph.2;
        // A radio box is a circle: radius half the side.
        let inner = paint_box(
            surface,
            glyph,
            side / 2,
            scale,
            theme,
            self.core.role,
            self.core.state,
        );

        if self.selected {
            if let Some((ix, iy, iw, ih)) = inner.and_then(mark_rect) {
                let d = iw.min(ih);
                let cx = ix + (iw.saturating_sub(d)) / 2;
                let cy = iy + (ih.saturating_sub(d)) / 2;
                surface.fill_round_rect(
                    cx,
                    cy,
                    d,
                    d,
                    d / 2,
                    resolve_mark(theme, self.core.role, self.core.state),
                );
            }
        }

        let label_color = resolve_frame(theme, self.core.role, self.core.state).label;
        let gap = scale.scale_length(theme.metrics().control_gap);
        paint_label(
            surface,
            bounds,
            side,
            gap,
            font,
            &self.core.label,
            label_color,
        );
        paint_selector_signals(surface, bounds, glyph, scale, theme, self.core.state);
    }

    /// Feed a pointer event; a completed primary click requests selection.
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<SelectorAction> {
        self.core
            .on_pointer(event, bounds)
            .then_some(SelectorAction::Set { on: true })
    }

    /// Feed a key event; Space/Enter requests selection.
    pub fn on_key(&mut self, key: Key) -> Option<SelectorAction> {
        self.core
            .on_key(key)
            .then_some(SelectorAction::Set { on: true })
    }
}
