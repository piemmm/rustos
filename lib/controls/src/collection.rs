//! The collection controls: [`ListRow`], [`TableRow`], [`TableCell`],
//! [`TableHeader`], [`Card`], and [`Panel`] (spec §11.13–§11.16).
//!
//! Rows, tables, cards, and panels are the surfaces that *group* other state
//! and actions. They are controls in their own right: a row can be hovered,
//! selected, focused, and activated; a table keeps its columns aligned while a
//! row's state changes; a [`TableHeader`] names those same columns and, for a
//! sortable one, reports a sort request without ever reordering anything
//! itself; a card carries a dominant state on its leading edge, progress on
//! its bottom edge, and a count/alert on its top-trailing corner; a panel is a
//! stable-layout container with a header, grouped actions, and a content
//! region, optionally pointing an anchor notch back at its invoker.
//!
//! [`TableHeader`] and [`TableRow`] share exactly one column-width model — the
//! private `column_spans` helper both call over the shared `row_content_span`
//! leading/trailing reservation — so a header can never drift out of
//! alignment with the rows it names, and a row's own state (a Signal Bead
//! appearing or disappearing) can never shift that same row's columns either.
//!
//! Every visible property resolves from the active [`Theme`] and [`Scale`], and
//! every shared recipe (plate rounding, Signal Bead, Pressure Rail, focus ring)
//! comes from the shared `crate::paint` core rather than a second copy. A
//! control renders state and emits a typed action; it performs no privileged
//! work — the owning service enforces authority. A denied
//! surface keeps its value and shows an Authority Mark rather than collapsing
//! into a plain disabled look (spec §13).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::{BitmapFont, ELLIPSIS};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{div255, round_rect_coverage, BlurScratch, Color, Surface};
use tairix_theme::{Rgba, TextRole, Theme};

use crate::button::{Button, ButtonAction};
use crate::paint::{
    dominant_color, draw_outline, foreground, grab_after, heavy_contrast, icon_slot_side, inset,
    key_activation, paint_bead, paint_chevron, paint_count_badge, paint_icon_slot, plate_border,
    pointer_activation, press_latch, rail_thickness, resolve_bead, resolve_rail, role_font,
    route_pointer, seam_thickness, seam_width, surface_rect, to_i32, ChevronDir,
};
use crate::state::{
    ControlDisposition, ControlRole, ControlState, FocusState, PointerState, RenderInvariant,
    SelectionState,
};

/// The outcome of feeding input to a [`ListRow`] or [`TableRow`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RowAction {
    /// The row was activated (released over it, or Space/Enter while focused);
    /// the owner selects or opens it.
    Activated,
}

// --- Shared row visuals ------------------------------------------------
//
// [`ListRow`] and [`TableRow`] paint the same background, rails, activity
// seam, Signal Bead, and focus ring; only their *content* (a label vs a set
// of aligned cells) differs. That shared recipe lives here once so a change
// to how a selected or pressured row reads cannot diverge between the two.

/// The fixed two-rail gutter width reserved on a row's leading edge, always
/// present regardless of the row's own state (spec §11.13). [`TableHeader`]
/// reserves this identical gutter before laying its columns out, so its
/// column titles begin exactly where an ordinary row's cells do — never a
/// second, independently-derived gutter width that could drift out of step.
#[must_use]
fn row_gutter(theme: &Theme, scale: Scale, w: u32) -> u32 {
    rail_thickness(theme, scale).saturating_mul(2).min(w)
}

/// The fixed trailing Signal Bead band width reserved on a row's trailing
/// edge, sized from `h`/`theme`/`scale` alone — never from whether *this*
/// row's particular state actually has a bead to draw right now.
///
/// [`paint_row`] reserves this band unconditionally, exactly as it reserves
/// [`row_gutter`] unconditionally on the leading edge: a bead only ever
/// paints inside the band, it never changes the band's width. That is what
/// lets a row's disposition, recovery, or activity change without shifting
/// its own columns (spec §11.13) — a row that merely becomes denied, or
/// gains a recovery mark, keeps every cell exactly where it was.
#[must_use]
fn bead_band(theme: &Theme, scale: Scale, h: u32) -> u32 {
    let border = plate_border(theme, scale);
    scale
        .scale_length(theme.metrics().bead_size)
        .max(3)
        .min(h.saturating_sub(border.saturating_mul(2)))
}

/// The `(x, width)` content span a row's (or [`TableHeader`]'s) cells are laid
/// out across, given its `(x, w, h)` surface-pixel bounds.
///
/// The reservation is state-independent on both edges: the fixed leading
/// [`row_gutter`] plus the theme's control padding, and that same padding
/// plus the fixed trailing [`bead_band`] plus a second padding gap before the
/// content — every one of those sized from `w`/`h`/`theme`/`scale` alone,
/// never from a row's actual disposition, recovery, or activity. [`paint_row`],
/// [`TableHeader::column_at`]/[`TableHeader::render`] (which draw no leading
/// rails or trailing bead of their own but must reserve the identical space
/// to stay lined up with the rows they name), and [`TableRow::cell_rects`]
/// all derive their column rectangles from this one span, so a header, a
/// plain row, and a bead-bearing row can never drift out of alignment (spec
/// §11.13/§11.14). `None` when `w`/`h` are too small to hold any content past
/// the reserved edges.
#[must_use]
fn row_content_span(scale: Scale, theme: &Theme, x: u32, w: u32, h: u32) -> Option<(u32, u32)> {
    let gutter = row_gutter(theme, scale, w);
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let band = bead_band(theme, scale, h);
    let content_x = x.saturating_add(gutter).saturating_add(pad);
    let content_right = x
        .saturating_add(w)
        .saturating_sub(pad)
        .saturating_sub(band)
        .saturating_sub(pad);
    if content_right <= content_x {
        return None;
    }
    Some((content_x, content_right - content_x))
}

/// Paint the shared row chrome — background tint, leading pressure and
/// selection rails, the bottom activity Heat Seam, the trailing Signal Bead,
/// and the keyboard focus ring — into `rect`, returning the inner content
/// rectangle `(x, y, w, h)` the caller draws its label or cells within.
///
/// Returning the content rect (already inset past the leading rails and the
/// trailing bead band) keeps the column-alignment contract in one place: the
/// caller lays content out relative to this rect, so a row's state changing
/// never shifts where its content begins or ends (spec §11.13 "keep columns
/// aligned"). The rect itself comes from `row_content_span`, whose
/// reservation on both edges is fixed by the row's own size — a bead only
/// ever paints *inside* the already-reserved trailing band, it never resizes
/// it, so a row that merely becomes denied or gains a recovery mark never
/// shifts its own cells, let alone its neighbours'.
fn paint_row(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    state: ControlState,
) -> Option<(u32, u32, u32, u32)> {
    let (x, y, w, h) = rect;
    if w == 0 || h == 0 {
        return None;
    }
    let palette = theme.palette();
    let selected = matches!(
        state.selection,
        SelectionState::Selected | SelectionState::Mixed
    );

    // Background tint: a pressed row recesses; a selected row lifts to the
    // raised surface (and is further distinguished by its accent rail below); a
    // hovered row takes the shared pointer wash, so the pointer never imitates
    // selection; a resting row is the base surface.
    let fill = match state.pointer {
        crate::state::PointerState::Pressed => palette.surface_pressed,
        _ if selected => palette.surface_raised,
        crate::state::PointerState::Hover => palette.surface_hover,
        _ => palette.surface,
    };
    surface.fill_rect(x, y, w, h, Color::from(fill));

    // Leading rails: a *fixed* two-rail gutter is always reserved on the
    // leading edge so a row's content never shifts when its selection or
    // pressure changes — the table stays aligned (spec §11.13). Within that
    // reserved gutter the resource-pressure semantic rail draws in the outer
    // half and the selection accent rail in the inner half, so both read at
    // once without overlap and without moving the content.
    let rail_w = rail_thickness(theme, scale);
    let gutter = row_gutter(theme, scale, w);
    let lead = x.saturating_add(gutter);
    let outer_w = rail_w.min(gutter);
    if let Some(color) = resolve_rail(theme, state) {
        surface.fill_rect(x, y, outer_w, h, color);
    }
    if selected {
        let inner_w = rail_w.min(gutter.saturating_sub(outer_w));
        if inner_w > 0 {
            surface.fill_rect(x + outer_w, y, inner_w, h, Color::from(palette.accent));
        }
    }

    // The bottom Heat Seam for live activity (proportional for known work).
    let seam_h = seam_thickness(theme, scale).min(h);
    let seam_w = seam_width(state.activity, w);
    if seam_w > 0 {
        surface.fill_rect(
            x,
            y + h - seam_h,
            seam_w,
            seam_h,
            Color::from(palette.accent),
        );
    }

    // The trailing Signal Bead band (denied lock / recovery diamond /
    // complete check) is reserved unconditionally, exactly like the leading
    // gutter above: only whether a bead actually paints inside it depends on
    // the row's state, never the band's own width (§13, §15).
    let border = plate_border(theme, scale);
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let band = bead_band(theme, scale, h);
    if let Some((color, shape)) = resolve_bead(theme, state) {
        let bead_right = x.saturating_add(w).saturating_sub(pad);
        if band > 0 && bead_right > lead.saturating_add(band) {
            let bx = bead_right.saturating_sub(band);
            let by = y + (h.saturating_sub(band)) / 2;
            paint_bead(surface, bx, by, band, color, shape);
        }
    }

    // The keyboard focus ring, distinct from a pointer hover tint (§15).
    if state.focus.focused {
        draw_outline(
            surface,
            x,
            y,
            w,
            h,
            border.max(1),
            Color::from(palette.rim_active),
        );
    }

    row_content_span(scale, theme, x, w, h).map(|(cx, cw)| (cx, y, cw, h))
}

/// The baseline `y` that vertically centres one line of `font` in a
/// `(y, h)` band.
fn centred_text_y(font: BitmapFont, y: u32, h: u32) -> i32 {
    to_i32(y) + (to_i32(h) - to_i32(font.glyph_height())).max(0) / 2
}

// --- ListRow -----------------------------------------------------------

/// One selectable, focusable, activatable row of a list (spec §11.13).
///
/// A list row carries a label, an optional leading icon, and an optional
/// trailing caption. Its hover, selection, live activity, resource pressure,
/// recovery, and authority states are read from its composed [`ControlState`]
/// and drawn by the shared row chrome. The row renders state and never
/// dispatches — a completed click or Space/Enter reports [`RowAction`] and the
/// owner selects or opens it.
///
/// Equal rows draw the same pixels, so a host may use `==` as its repaint
/// gate: the label, icon, trailing caption, role, and every visible part of
/// the composed state compare, while the pointer coordinate and press latch
/// beneath them — which no render path reads — do not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListRow {
    label: String,
    trailing: Option<String>,
    icon: Option<IconKind>,
    role: ControlRole,
    state: ControlState,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The press latch; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl ListRow {
    /// A neutral, enabled, unselected row with the given label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            trailing: None,
            icon: None,
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// This row with a leading icon glyph on a reserved column.
    #[must_use]
    pub fn with_icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    /// This row with a muted, right-aligned trailing caption (e.g. a size or
    /// timestamp).
    #[must_use]
    pub fn with_trailing(mut self, trailing: impl Into<String>) -> Self {
        self.trailing = Some(trailing.into());
        self
    }

    /// This row with a non-default role (e.g. [`ControlRole::Destructive`]).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This row with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// The row's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The row's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.role
    }

    /// The row's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the row's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the row's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Set whether the row belongs to the highlighted Focus Field — the group
    /// of related controls around whichever one holds keyboard focus.
    ///
    /// Orthogonal to [`set_focused`](Self::set_focused): a row whose own
    /// action button holds the keyboard is a field member without holding the
    /// ring itself.
    pub fn set_in_focus_field(&mut self, member: bool) {
        self.state.focus.in_focus_field = member;
    }

    /// Set the row's selection.
    pub fn set_selected(&mut self, selected: bool) {
        self.state.selection = if selected {
            SelectionState::Selected
        } else {
            SelectionState::Unselected
        };
    }

    /// Whether the row is selected.
    #[must_use]
    pub fn is_selected(&self) -> bool {
        matches!(
            self.state.selection,
            SelectionState::Selected | SelectionState::Mixed
        )
    }

    /// The pixel side the row's leading icon paints at inside `bounds`.
    ///
    /// This is the render geometry itself, exposed so an owner rasterising
    /// per-entry artwork produces it at exactly the size [`Self::render`] will
    /// place — the two can never disagree. The icon column is sized off the
    /// text line so it lines up with the label, and the side is the same
    /// whether or not the row carries an icon (the reserved column is fixed),
    /// so a caller can size artwork before deciding to supply it.
    #[must_use]
    pub fn icon_side(&self, bounds: Rect, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        // The list row's icon column is sized purely off the text line and the
        // row height; the scale and theme are accepted only so the query
        // matches the shared collection-control shape.
        let _ = (scale, theme);
        let Some((_, _, _, h)) = surface_rect(bounds) else {
            return 0;
        };
        icon_slot_side(font, h)
    }

    /// Paint the row into `surface` at `bounds` for the active theme.
    ///
    /// `artwork` is the entry's own icon, pre-rasterised by the owner (at
    /// [`Self::icon_side`], through its cache); `None` falls back to the row's
    /// built-in class glyph. It is used only when the row carries an icon; a
    /// row with no reserved icon ignores it. The artwork is decoded and
    /// rasterised long before it reaches this call — a control never parses
    /// image bytes.
    ///
    /// The trailing caption takes what it needs of the content and the label
    /// takes the rest; either one too long for its share ends with
    /// [`ELLIPSIS`], so a reader can tell a hidden tail from a name that
    /// simply ends there.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<&Surface>,
    ) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some(rect) = surface_rect(bounds) else {
            return;
        };
        let Some((cx, cy, cw, ch)) = paint_row(surface, rect, scale, theme, self.state) else {
            return;
        };
        let disposition = self.state.disposition();
        let fg = foreground(theme, disposition);
        let muted = Color::from(theme.palette().on_surface_muted);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let text_y = centred_text_y(font, cy, ch);

        let mut left = cx;
        // The leading icon on a reserved column, so labels line up whether or
        // not a row has an icon (text stability, spec §14).
        let icon_slot = icon_slot_side(font, ch);
        if let Some(kind) = self.icon {
            if icon_slot > 0 {
                let iy = cy + (ch.saturating_sub(icon_slot)) / 2;
                paint_icon_slot(surface, left, iy, icon_slot, kind, fg, artwork);
            }
            left = left.saturating_add(icon_slot).saturating_add(pad);
        }

        let mut right = cx.saturating_add(cw);
        // The trailing caption, muted and right-aligned inside the content.
        if let Some(text) = &self.trailing {
            if right > left {
                let run = font.elide_to_width(text, right - left);
                let tw = run_width(font, run);
                paint_run(
                    surface,
                    font,
                    run,
                    (to_i32(right) - to_i32(tw), text_y),
                    muted,
                );
                right = right.saturating_sub(tw).saturating_sub(pad);
            }
        }

        // The label, left-aligned in the remaining space.
        if right > left {
            let run = font.elide_to_width(&self.label, right - left);
            paint_run(surface, font, run, (to_i32(left), text_y), fg);
        }
    }

    /// Feed a pointer event, given the row's `bounds`; a completed primary
    /// click over an actionable row reports [`RowAction::Activated`]. The row
    /// reports `bounds` into `damage` when the event changed how it draws.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<RowAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        pointer_activation(
            &mut self.state,
            &mut self.armed,
            event,
            inside,
            bounds,
            damage,
        )
        .then_some(RowAction::Activated)
    }

    /// Feed a key event; Space/Enter activates a focused, actionable row.
    pub fn on_key(&mut self, key: Key) -> Option<RowAction> {
        key_activation(self.state, key).then_some(RowAction::Activated)
    }
}

// --- TableCell and TableRow --------------------------------------------

/// How a [`TableCell`]'s text is aligned within its column.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CellAlign {
    /// Left-aligned (the default for label columns).
    Leading,
    /// Centred.
    Center,
    /// Right-aligned (the default for numeric columns, so digits line up in
    /// the monospace tabular figures).
    Trailing,
}

/// One cell of a [`TableRow`] (spec §11.14).
///
/// A cell draws its text aligned within its column, with an optional leading
/// icon naming what the cell's value *is* (a file-type glyph beside a name, a
/// resource glyph beside a reading) — the same optional identity-icon
/// [`MetricTile`](crate::metric::MetricTile) already carries on its own
/// leading edge, never a second convention. Row-wide state belongs to the
/// row; a cell exposes its *own* composed state only when that state is
/// cell-specific (e.g. one invalid field in an otherwise-valid row), in which
/// case the cell draws its own Signal Bead and takes its own foreground.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableCell {
    text: String,
    align: CellAlign,
    icon: Option<IconKind>,
    state: Option<ControlState>,
}

impl TableCell {
    /// A leading-aligned cell with the given text, no icon, and no
    /// cell-specific state.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            align: CellAlign::Leading,
            icon: None,
            state: None,
        }
    }

    /// A trailing-aligned numeric cell, so digits line up in the tabular
    /// monospace figures (spec §11.14).
    #[must_use]
    pub fn numeric(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            align: CellAlign::Trailing,
            icon: None,
            state: None,
        }
    }

    /// This cell with the given alignment.
    #[must_use]
    pub fn with_align(mut self, align: CellAlign) -> Self {
        self.align = align;
        self
    }

    /// This cell with a leading icon identifying what its value is, drawn on
    /// a fixed slot ahead of the text regardless of the cell's own alignment
    /// — so a leading, centred, or trailing cell's icon always sits at the
    /// same edge. A column too narrow to hold it simply omits it rather than
    /// overlapping or overflowing the text (fail closed).
    #[must_use]
    pub fn with_icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    /// This cell's leading icon, if any.
    #[must_use]
    pub fn icon(&self) -> Option<IconKind> {
        self.icon
    }

    /// This cell with its own cell-specific state (drawn instead of inheriting
    /// the row's). Use only when the state is genuinely cell-specific (§11.14).
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = Some(state);
        self
    }

    /// The cell's text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cell's alignment.
    #[must_use]
    pub fn align(&self) -> CellAlign {
        self.align
    }

    /// The pixel side of the square slot this cell's leading icon (if any)
    /// draws in: the text line, never taller than the cell height it is
    /// given. One definition so the reserved slot and the drawn glyph can
    /// never disagree.
    #[must_use]
    fn icon_side(font: BitmapFont, h: u32) -> u32 {
        font.glyph_height().min(h)
    }

    /// Paint this cell into the column rectangle `(x, y, w, h)`, taking its
    /// foreground from its own state when it has one, otherwise from the
    /// row's `row_disposition`. A leading icon (if any) draws first, on a
    /// fixed slot carved out of the text budget ahead of it — never inside
    /// it, so it can never overlap the truncated text, and never past the
    /// column's own edge, so it can never overflow into a neighbour.
    fn paint(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        row_disposition: ControlDisposition,
    ) {
        let (x, y, w, h) = rect;
        if w == 0 || h == 0 {
            return;
        }
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let disposition = self
            .state
            .map_or(row_disposition, ControlState::disposition);
        let fg = foreground(theme, disposition);
        let text_y = centred_text_y(font, y, h);

        let mut right = x.saturating_add(w).saturating_sub(pad);
        // A cell-specific Signal Bead at the trailing edge (only for a cell
        // that carries its own state, spec §11.14).
        if let Some(state) = self.state {
            if let Some((color, shape)) = resolve_bead(theme, state) {
                let size = scale.scale_length(theme.metrics().bead_size).max(3).min(h);
                if size > 0 && right > x.saturating_add(size) {
                    let bx = right.saturating_sub(size);
                    let by = y + (h.saturating_sub(size)) / 2;
                    paint_bead(surface, bx, by, size, color, shape);
                    right = bx.saturating_sub(pad);
                }
            }
        }

        let mut left = x.saturating_add(pad);
        // The leading identity icon, on a fixed slot ahead of the text
        // regardless of the cell's own alignment (spec §11.14). A column too
        // narrow to hold the icon and still leave the padding gap before
        // whatever follows it simply omits the icon; the text then keeps the
        // full budget rather than a truncated icon overlapping it.
        if let Some(kind) = self.icon {
            let side = Self::icon_side(font, h);
            if side > 0 && right > left.saturating_add(side).saturating_add(pad) {
                let iy = y + (h.saturating_sub(side)) / 2;
                paint_icon_slot(surface, left, iy, side, kind, fg, None);
                left = left.saturating_add(side).saturating_add(pad);
            }
        }

        if right <= left {
            return;
        }
        let budget = right - left;
        let fitted = font.truncate_to_width(&self.text, budget);
        let tw = font.text_width(fitted);
        let tx = match self.align {
            CellAlign::Leading => to_i32(left),
            CellAlign::Center => to_i32(left) + (to_i32(budget) - to_i32(tw)).max(0) / 2,
            CellAlign::Trailing => to_i32(right) - to_i32(tw),
        };
        font.draw_text(surface, tx, text_y, fitted, fg);
    }
}

/// One row of a table: a set of column-aligned [`TableCell`]s sharing one row
/// state (spec §11.13–§11.14).
///
/// The row draws the shared row chrome (hover/selection/activity/pressure/
/// recovery/authority) once, then lays its cells out across the caller-supplied
/// column widths. Because the cells are laid out relative to the shared content
/// rect, a row's state changing never shifts a column — the table stays aligned
/// (spec §11.13). A completed click or Space/Enter reports [`RowAction`].
///
/// Its equality is the render-equivalence relation [`ListRow`] documents: the
/// cells, role, and visible state compare; the pointer coordinate and press
/// latch do not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableRow {
    cells: Vec<TableCell>,
    role: ControlRole,
    state: ControlState,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The press latch; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl TableRow {
    /// A neutral, enabled, unselected row over the given cells.
    #[must_use]
    pub fn new(cells: Vec<TableCell>) -> Self {
        Self {
            cells,
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// This row with a non-default role.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This row with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// The row's cells.
    #[must_use]
    pub fn cells(&self) -> &[TableCell] {
        &self.cells
    }

    /// Mutable access to the row's cells.
    pub fn cells_mut(&mut self) -> &mut [TableCell] {
        &mut self.cells
    }

    /// The row's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.role
    }

    /// The row's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the row's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the row's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Set whether the row belongs to the highlighted Focus Field — the group
    /// of related controls around whichever one holds keyboard focus.
    ///
    /// Orthogonal to [`set_focused`](Self::set_focused): a table row whose own
    /// trailing action button holds the keyboard is a field member without
    /// holding the ring itself, exactly as [`ListRow::set_in_focus_field`]
    /// expresses for a list row. Both row kinds carry the same guarantee so a
    /// composer can move a section between them without a row silently losing
    /// its ability to read as part of the focused group.
    pub fn set_in_focus_field(&mut self, member: bool) {
        self.state.focus.in_focus_field = member;
    }

    /// Set the row's selection.
    pub fn set_selected(&mut self, selected: bool) {
        self.state.selection = if selected {
            SelectionState::Selected
        } else {
            SelectionState::Unselected
        };
    }

    /// Whether the row is selected.
    #[must_use]
    pub fn is_selected(&self) -> bool {
        matches!(
            self.state.selection,
            SelectionState::Selected | SelectionState::Mixed
        )
    }

    /// Paint the row into `surface` at `bounds`, laying its cells out across
    /// `columns` (physical pixel widths) through the shared `column_spans`
    /// helper — the same one [`TableHeader::render`] derives its column
    /// rectangles from, so a header can never drift out of alignment with the
    /// rows beneath it. The last cell absorbs any rounding remainder so the
    /// columns fill the content width exactly.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        columns: &[u32],
    ) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some(rect) = surface_rect(bounds) else {
            return;
        };
        let Some((cx, cy, cw, ch)) = paint_row(surface, rect, scale, theme, self.state) else {
            return;
        };
        let disposition = self.state.disposition();
        let spans = column_spans(cx, cw, columns, self.cells.len());
        for (cell, &(col_x, col_w)) in self.cells.iter().zip(spans.iter()) {
            cell.paint(
                surface,
                (col_x, cy, col_w, ch),
                scale,
                theme,
                font,
                disposition,
            );
        }
    }

    /// Where each of this row's cells is laid out at `bounds`, in `bounds`'
    /// own coordinate space — the physical-pixel rectangle
    /// [`Self::render`] draws that cell's text within, laid out across
    /// `columns` (physical pixel widths) exactly as `render` is called with.
    ///
    /// A composer that needs to draw its own content inside a column — a
    /// sparkline chart beside a cell's number, say — calls this instead of
    /// re-deriving the row's layout, so the two can never disagree: both
    /// [`Self::render`] and this query resolve the row's content rect through
    /// the same `row_content_span`/`column_spans` pair, and that
    /// reservation is fixed by the row's own size, never by whether this
    /// particular row currently draws a Signal Bead. The vector holds one
    /// rect per cell that could be laid out, in cell order; it is shorter
    /// than [`Self::cells`] (`Vec::new()` in the extreme) when `bounds`
    /// cannot seat every declared column, exactly as `render` then simply
    /// draws fewer cells.
    #[must_use]
    pub fn cell_rects(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        columns: &[u32],
    ) -> Vec<Rect> {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return Vec::new();
        };
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let Some((content_x, content_w)) = row_content_span(scale, theme, x, w, h) else {
            return Vec::new();
        };
        column_spans(content_x, content_w, columns, self.cells.len())
            .into_iter()
            .map(|(cx, cw)| Rect::new(to_i32(cx), to_i32(y), cw, h))
            .collect()
    }

    /// Feed a pointer event, given the row's `bounds`; a completed primary
    /// click over an actionable row reports [`RowAction::Activated`]. The row
    /// reports `bounds` into `damage` when the event changed how it draws.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<RowAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        pointer_activation(
            &mut self.state,
            &mut self.armed,
            event,
            inside,
            bounds,
            damage,
        )
        .then_some(RowAction::Activated)
    }

    /// Feed a key event; Space/Enter activates a focused, actionable row.
    pub fn on_key(&mut self, key: Key) -> Option<RowAction> {
        key_activation(self.state, key).then_some(RowAction::Activated)
    }
}

/// The on-surface `(x, width)` span of each of the first `item_count` items
/// laid out across `columns` (physical pixel widths), within a content region
/// `content_w` pixels wide starting at `content_x`.
///
/// [`TableRow::render`]/[`TableRow::cell_rects`] and
/// [`TableHeader::render`]/[`TableHeader::column_at`] all derive a column's
/// rectangle through this one function, fed the identical `content_x`/
/// `content_w` `row_content_span` computes — never independently — so a
/// header can never drift out of alignment with the rows it names (spec
/// §11.14 "keep columns aligned"). The declared widths are scaled
/// proportionally into the actual content width (so columns stay
/// proportional even when the content rect is narrower than the sum of the
/// declared widths), and the last item absorbs the rounding remainder so the
/// columns fill the content width exactly. Fewer than `item_count` spans come
/// back when the declared widths run out or the content width is exhausted
/// first — the caller then simply has fewer columns to draw.
fn column_spans(
    content_x: u32,
    content_w: u32,
    columns: &[u32],
    item_count: usize,
) -> Vec<(u32, u32)> {
    let declared: u32 = columns.iter().copied().fold(0, u32::saturating_add);
    if declared == 0 {
        return Vec::new();
    }
    let count = item_count.min(columns.len());
    let content_end = content_x.saturating_add(content_w);
    let mut spans = Vec::with_capacity(count);
    let mut col_x = content_x;
    for (i, &declared_w) in columns.iter().take(count).enumerate() {
        if col_x >= content_end {
            break;
        }
        let scaled_w =
            u32::try_from(u64::from(declared_w) * u64::from(content_w) / u64::from(declared))
                .unwrap_or(content_w);
        let is_last = i + 1 == count;
        let col_w = if is_last {
            content_end.saturating_sub(col_x)
        } else {
            scaled_w.min(content_end.saturating_sub(col_x))
        };
        spans.push((col_x, col_w));
        col_x = col_x.saturating_add(col_w);
    }
    spans
}

// --- TableHeader ---------------------------------------------------------

/// Which way a [`TableHeader`] column is sorted.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SortOrder {
    /// Lowest to highest.
    Ascending,
    /// Highest to lowest.
    Descending,
}

impl SortOrder {
    /// The other order — what pressing an already-sorted column flips to.
    #[must_use]
    fn flipped(self) -> Self {
        match self {
            SortOrder::Ascending => SortOrder::Descending,
            SortOrder::Descending => SortOrder::Ascending,
        }
    }
}

/// The outcome of feeding input to a [`TableHeader`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HeaderAction {
    /// The reader asked to sort by `column`, in `order`.
    ///
    /// The header only *reports* this request; it never reorders anything
    /// itself and never assumes the request was honoured. The owner commits
    /// whatever sort it actually applied through [`TableHeader::set_sort`],
    /// which is what the header then draws.
    Sort {
        /// The zero-based index of the column to sort by.
        column: usize,
        /// The direction to sort in.
        order: SortOrder,
    },
}

/// One column heading of a [`TableHeader`] (spec §11.14).
///
/// A column names one of a [`TableRow`]'s aligned cells and, when sortable,
/// reports a request to sort by it. Its own composed [`ControlState`] carries
/// disabled/denied/etc. exactly like a [`TableCell`]'s cell-specific state,
/// down to the trailing Authority Mark a denied column shows through the same
/// bead resolution the row family draws from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderColumn {
    title: String,
    align: CellAlign,
    sortable: bool,
    state: ControlState,
}

impl HeaderColumn {
    /// A leading-aligned, sortable, enabled column with the given title.
    ///
    /// Sortable is the default: most columns of a table are meaningful to
    /// order by, so a column opts *out* of sorting ([`HeaderColumn::fixed`])
    /// rather than in.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            align: CellAlign::Leading,
            sortable: true,
            state: ControlState::idle(),
        }
    }

    /// A column that cannot be sorted (an actions or icon column).
    #[must_use]
    pub fn fixed(title: impl Into<String>) -> Self {
        Self {
            sortable: false,
            ..Self::new(title)
        }
    }

    /// This column with the given alignment.
    #[must_use]
    pub fn with_align(mut self, align: CellAlign) -> Self {
        self.align = align;
        self
    }

    /// This column with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// The column's title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The column's alignment.
    #[must_use]
    pub fn align(&self) -> CellAlign {
        self.align
    }

    /// Whether pressing this column requests a sort.
    #[must_use]
    pub fn is_sortable(&self) -> bool {
        self.sortable
    }

    /// The column's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the column's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }
}

/// The column names above a [`TableRow`]'s cells, sharing that row's
/// column-width model through the shared `column_spans` helper so a header can
/// never drift out of alignment with the rows it names (spec §11.14).
///
/// The header only *reports* a sort request through [`HeaderAction::Sort`];
/// it never reorders anything itself and never assumes a reported request was
/// honoured. The owner commits the sort it actually applied with
/// [`TableHeader::set_sort`], which is what the header then draws.
///
/// Equal headers draw the same pixels, so a host may use `==` as its repaint
/// gate: the columns, the committed sort, and the keyboard-focused column all
/// compare, while the pointer coordinate and press latch beneath them — which
/// no render path reads — do not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableHeader {
    columns: Vec<HeaderColumn>,
    sort: Option<(usize, SortOrder)>,
    focus: Option<usize>,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The column a primary press landed on, held until release so a click
    /// that slides onto another column does not sort it.
    armed: RenderInvariant<Option<usize>>,
}

impl TableHeader {
    /// A header over the given columns, with no committed sort and no focus.
    #[must_use]
    pub fn new(columns: Vec<HeaderColumn>) -> Self {
        Self {
            columns,
            sort: None,
            focus: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(None),
        }
    }

    /// The header's columns.
    #[must_use]
    pub fn columns(&self) -> &[HeaderColumn] {
        &self.columns
    }

    /// Mutable access to the header's columns.
    pub fn columns_mut(&mut self) -> &mut [HeaderColumn] {
        &mut self.columns
    }

    /// The committed sort, if any.
    #[must_use]
    pub fn sort(&self) -> Option<(usize, SortOrder)> {
        self.sort
    }

    /// Adopt the owner's committed sort.
    ///
    /// The header never applies a sort itself; this is how the owner tells it
    /// which sort it actually committed, so the header draws the caret
    /// against the real outcome rather than assuming its own report was
    /// honoured. Ignored (fail closed) for an out-of-range or unsortable
    /// column, rather than clamping the request onto a different column.
    pub fn set_sort(&mut self, sort: Option<(usize, SortOrder)>) {
        self.sort = match sort {
            None => None,
            Some((index, order)) => match self.columns.get(index) {
                Some(column) if column.is_sortable() => Some((index, order)),
                _ => return,
            },
        };
    }

    /// The keyboard-focused column, if any.
    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// Focus `index` (or clear focus with `None`); an out-of-range index
    /// clears focus (fail closed).
    pub fn set_focus(&mut self, index: Option<usize>) {
        self.focus = index.filter(|&i| i < self.columns.len());
    }

    /// The scaled height the header needs, from the font's line height and
    /// the theme's control padding, floored at the theme's standard control
    /// height so a header never reads shorter than an ordinary row.
    #[must_use]
    pub fn measured_height(scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let text_band = font.glyph_height().saturating_add(pad.saturating_mul(2));
        text_band.max(scale.scale_length(theme.metrics().control_height).max(1))
    }

    /// The column index under `point`, given the header's `bounds`, the
    /// active `scale`/`theme` (needed to reserve the same leading/trailing
    /// span an ordinary row does), and the same declared `columns` widths the
    /// owner renders with.
    #[must_use]
    pub fn column_at(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        columns: &[u32],
        point: Point,
    ) -> Option<usize> {
        let (x, y, w, h) = surface_rect(bounds)?;
        if w == 0 || h == 0 {
            return None;
        }
        let (content_x, content_w) = row_content_span(scale, theme, x, w, h)?;
        column_spans(content_x, content_w, columns, self.columns.len())
            .into_iter()
            .position(|(cx, cw)| {
                point.x >= to_i32(cx)
                    && point.x < to_i32(cx.saturating_add(cw))
                    && point.y >= to_i32(y)
                    && point.y < to_i32(y.saturating_add(h))
            })
    }

    /// Paint the header into `surface` at `bounds`, laying its columns out
    /// across `columns` (physical pixel widths) — the same declared widths
    /// [`TableRow::render`] is called with — through the shared
    /// `column_spans` helper over the shared `row_content_span`
    /// leading/trailing reservation, so a header column begins and ends
    /// exactly where the cell beneath it does, whether or not that cell's row
    /// currently draws a Signal Bead, and can never drift out of alignment.
    /// A `bounds` too small to hold anything omits content rather than
    /// clipping or painting outside it.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        columns: &[u32],
    ) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let Some((content_x, content_w)) = row_content_span(scale, theme, x, w, h) else {
            return;
        };
        for (i, &(cx, cw)) in column_spans(content_x, content_w, columns, self.columns.len())
            .iter()
            .enumerate()
        {
            let Some(column) = self.columns.get(i) else {
                break;
            };
            let sort = self.sort.and_then(|(si, order)| (si == i).then_some(order));
            let focused = self.focus == Some(i);
            Self::paint_column(
                surface,
                (cx, y, cw, h),
                scale,
                theme,
                font,
                column,
                sort,
                focused,
            );
        }
    }

    /// Paint one column heading into `rect`, from its title, alignment, and
    /// composed state, plus the sort caret when it is the currently sorted
    /// column (`sort`) and the shared keyboard focus ring when `focused`.
    ///
    /// This mirrors [`TableCell::paint`] closely — muted-vs-emphasised text
    /// and the trailing Authority Mark / recovery / completion bead through
    /// the same [`resolve_bead`] the row family uses — so a header column
    /// reads as one family with the cells beneath it.
    // A private layout helper naturally takes every dimension the row
    // family's own `TableCell::paint` already threads through a
    // similarly-shaped function; it is not a public API surface.
    #[allow(clippy::too_many_arguments)]
    fn paint_column(
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        column: &HeaderColumn,
        sort: Option<SortOrder>,
        focused: bool,
    ) {
        let (x, y, w, h) = rect;
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let disposition = column.state.disposition();
        // Titles read muted by default; the sorted column alone takes the
        // full (disposition-aware) foreground so it stands out from its
        // neighbours without relying on the caret alone.
        let fg = if sort.is_some() {
            foreground(theme, disposition)
        } else {
            Color::from(palette.on_surface_muted)
        };

        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let mut left = x.saturating_add(pad);
        let mut right = x.saturating_add(w).saturating_sub(pad);

        // The trailing Authority Mark / recovery / completion bead, exactly
        // as the row family draws a cell-specific state.
        if let Some((color, shape)) = resolve_bead(theme, column.state) {
            let size = scale.scale_length(theme.metrics().bead_size).max(3).min(h);
            if size > 0 && right > left.saturating_add(size) {
                let bx = right.saturating_sub(size);
                let by = y + (h.saturating_sub(size)) / 2;
                paint_bead(surface, bx, by, size, color, shape);
                right = bx.saturating_sub(pad);
            }
        }

        // The sort caret, beside the title on the side its alignment
        // implies: a leading or centred title reads left-to-right toward the
        // caret at the trailing edge; a trailing-aligned (numeric) title
        // already hugs the trailing edge, so the caret takes the leading
        // side instead. Either way it is carved out of the column's own
        // width before the title is laid out, so the two can never overlap.
        //
        // The caret's square region is the row's own height, not a bead's
        // diameter: a bead fills the region it is given, while a chevron
        // insets its triangle well inside one, so a bead-sized region leaves a
        // triangle barely two pixels across — a grey smudge whose direction,
        // the whole point of the mark, cannot be read.
        if let Some(order) = sort {
            let caret_size = h;
            if right > left.saturating_add(caret_size) {
                let dir = match order {
                    SortOrder::Ascending => ChevronDir::Up,
                    SortOrder::Descending => ChevronDir::Down,
                };
                if column.align == CellAlign::Trailing {
                    paint_chevron(
                        surface,
                        Rect::new(to_i32(left), to_i32(y), caret_size, caret_size),
                        dir,
                        fg,
                    );
                    left = left.saturating_add(caret_size).saturating_add(pad);
                } else {
                    let cx = right.saturating_sub(caret_size);
                    paint_chevron(
                        surface,
                        Rect::new(to_i32(cx), to_i32(y), caret_size, caret_size),
                        dir,
                        fg,
                    );
                    right = cx.saturating_sub(pad);
                }
            }
        }

        if right > left {
            let budget = right - left;
            let fitted = font.truncate_to_width(&column.title, budget);
            let tw = font.text_width(fitted);
            let text_y = centred_text_y(font, y, h);
            let tx = match column.align {
                CellAlign::Leading => to_i32(left),
                CellAlign::Center => to_i32(left) + (to_i32(budget) - to_i32(tw)).max(0) / 2,
                CellAlign::Trailing => to_i32(right) - to_i32(tw),
            };
            font.draw_text(surface, tx, text_y, fitted, fg);
        }

        // The keyboard focus ring, distinct from the sort emphasis above.
        if focused {
            draw_outline(
                surface,
                x,
                y,
                w,
                h,
                plate_border(theme, scale).max(1),
                Color::from(palette.rim_active),
            );
        }
    }

    /// The sort request pressing `index` would emit — the documented default
    /// order ([`SortOrder::Ascending`]) when it is not already the sorted
    /// column, else the flipped order — or `None` when the column is not
    /// sortable or not actionable (fail closed).
    fn choose(&self, index: usize) -> Option<HeaderAction> {
        let column = self.columns.get(index)?;
        if !column.is_sortable() || !column.state.is_actionable() {
            return None;
        }
        let order = match self.sort {
            Some((current, order)) if current == index => order.flipped(),
            _ => SortOrder::Ascending,
        };
        Some(HeaderAction::Sort {
            column: index,
            order,
        })
    }

    /// Feed a pointer event, given the header's `bounds`, the active
    /// `scale`/`theme`, and the same declared `columns` widths it is rendered
    /// with; a completed primary click over a sortable, actionable column
    /// reports [`HeaderAction::Sort`].
    ///
    /// The scale and theme are the ones [`Self::render`] was called with:
    /// hit-testing resolves a column through the very [`Self::column_at`] the
    /// render path lays its columns out with, so a press can never sort a
    /// column that was not drawn where the reader saw it.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        columns: &[u32],
    ) -> Option<HeaderAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let over = self.column_at(bounds, scale, theme, columns, *self.pointer);
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                *self.armed = over;
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let armed = self.armed.take();
                match (armed, over) {
                    (Some(a), Some(o)) if a == o => self.choose(o),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Feed a key event: Left/Right move the focused column (wrapping),
    /// Home/End jump to the ends, and Space/Enter sort the focused column.
    pub fn on_key(&mut self, key: Key) -> Option<HeaderAction> {
        if self.columns.is_empty() {
            return None;
        }
        let last = self.columns.len() - 1;
        match key {
            Key::Named(NamedKey::Right) => {
                self.focus = Some(match self.focus {
                    Some(i) if i < last => i + 1,
                    _ => 0,
                });
                None
            }
            Key::Named(NamedKey::Left) => {
                self.focus = Some(match self.focus {
                    Some(0) | None => last,
                    Some(i) => i - 1,
                });
                None
            }
            Key::Named(NamedKey::Home) => {
                self.focus = Some(0);
                None
            }
            Key::Named(NamedKey::End) => {
                self.focus = Some(last);
                None
            }
            _ => {
                let index = self.focus?;
                let column = self.columns.get(index)?;
                let activated = column.state.with_focus(FocusState::FOCUSED);
                if key_activation(activated, key) {
                    self.choose(index)
                } else {
                    None
                }
            }
        }
    }
}

// --- Card --------------------------------------------------------------

/// The outcome of feeding input to a [`Card`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CardAction {
    /// The footer action at `index` was activated.
    FooterActivated {
        /// The zero-based index of the activated footer button.
        index: usize,
    },
    /// A completed primary click landed inside the card's own bounds,
    /// outside every footer button; the owner marks this card the selected
    /// cause (e.g. of a master/detail screen) rather than the card washing
    /// itself.
    Pressed,
}

/// A grouped state-and-actions surface (spec §11.15).
///
/// A card carries its dominant state on its leading edge, its progress on its
/// bottom edge, and a count or alert bead on its top-trailing corner, with a
/// title, optional body text, and a row of footer action [`Button`]s. The
/// footer actions share the card's semantic state (the owner sets it) but keep
/// their own pointer and focus states, so hovering one action does not disturb
/// the card. The card routes input to its footer, and a completed click on
/// its own body (outside every footer button) reports [`CardAction::Pressed`]
/// so a master/detail screen can select it — the card carries no hover or
/// pressed wash of its own for that press, only the owner's later choice to
/// mark it selected.
///
/// A card's plate bounds the group it owns. One item of an icon view is not
/// such a group and wears no plate of its own: that is an [`IconTile`].
///
/// Equal cards draw the same pixels, so a host may use `==` as its repaint
/// gate: the title, body, role, count, footer, and every visible part of the
/// composed state compare, while the pointer coordinate and press latch
/// beneath them — which no render path reads — do not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
    title: String,
    body: Option<String>,
    role: ControlRole,
    state: ControlState,
    count: Option<u32>,
    footer: Vec<Button>,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The body press latch; a completed press over the body is reported as
    /// [`CardAction::Pressed`] and never grows a visual of its own.
    armed: RenderInvariant<bool>,
    /// The footer button the pointer was last over, so a motion sample
    /// reaches the button it left and the one it entered rather than the
    /// whole footer. It mirrors the hover the buttons themselves carry.
    footer_hovered: RenderInvariant<Option<usize>>,
    /// The footer button holding a press, which keeps receiving the stream
    /// wherever the pointer goes.
    footer_armed: RenderInvariant<Option<usize>>,
}

impl Card {
    /// A neutral, enabled card with the given title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: None,
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            count: None,
            footer: Vec::new(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
            footer_hovered: RenderInvariant::new(None),
            footer_armed: RenderInvariant::new(None),
        }
    }

    /// This card with a body line below the title.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// This card with a non-default role (drives the leading dominant state).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This card with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// This card with a top-trailing count badge (drawn as an accent pill).
    #[must_use]
    pub fn with_count(mut self, count: u32) -> Self {
        self.count = Some(count);
        self
    }

    /// This card with the given footer action buttons.
    #[must_use]
    pub fn with_footer(mut self, footer: Vec<Button>) -> Self {
        self.footer = footer;
        self
    }

    /// The card's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the card's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set whether the card belongs to the highlighted Focus Field — the group
    /// of related controls around whichever one holds keyboard focus.
    ///
    /// A card never holds the ring itself (its footer buttons do), so this is
    /// how a card whose action group has the keyboard reads as part of it.
    pub fn set_in_focus_field(&mut self, member: bool) {
        self.state.focus.in_focus_field = member;
    }

    /// The card's footer action buttons.
    #[must_use]
    pub fn footer(&self) -> &[Button] {
        &self.footer
    }

    /// Mutable access to the footer action buttons (e.g. to update their state).
    pub fn footer_mut(&mut self) -> &mut [Button] {
        &mut self.footer
    }

    /// The inner plate rectangle (inside the rim) as surface pixels.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        inset(x, y, w, h, plate_border(theme, scale))
    }

    /// The footer button rectangles, laid out equal-width across the bottom of
    /// the card's content, above the progress seam.
    ///
    /// This is the one definition of where the footer buttons are: the card
    /// draws them into these rectangles and hit-tests
    /// [`on_pointer`](Card::on_pointer) against them, so the two can never
    /// disagree, and a composer embedding the card reads the same answer the
    /// card itself acts on rather than re-deriving the layout. The rectangles
    /// are in the same coordinate space as `bounds`, one per
    /// [`footer`](Card::footer) button in order; the result is empty when the
    /// card has no footer or `bounds` is too small to seat one.
    #[must_use]
    pub fn footer_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let mut rects = Vec::new();
        if self.footer.is_empty() {
            return rects;
        }
        let Some((ix, iy, iw, ih)) = Self::inner(bounds, scale, theme) else {
            return rects;
        };
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let rail_w = rail_thickness(theme, scale);
        let seam_h = seam_thickness(theme, scale);
        let footer_h = scale.scale_length(theme.metrics().control_height).max(1);
        let left = ix.saturating_add(rail_w).saturating_add(pad);
        let right = ix.saturating_add(iw).saturating_sub(pad);
        if right <= left || ih <= seam_h.saturating_add(footer_h).saturating_add(pad) {
            return rects;
        }
        let top = iy + ih - seam_h - footer_h - pad;
        let n = u32::try_from(self.footer.len()).unwrap_or(1).max(1);
        let total_gap = gap.saturating_mul(n.saturating_sub(1));
        let avail = (right - left).saturating_sub(total_gap);
        let each = avail / n;
        if each == 0 {
            return rects;
        }
        let mut bx = left;
        for i in 0..self.footer.len() {
            let w = if i + 1 == self.footer.len() {
                right.saturating_sub(bx)
            } else {
                each
            };
            rects.push(Rect::new(to_i32(bx), to_i32(top), w, footer_h));
            bx = bx.saturating_add(w).saturating_add(gap);
        }
        rects
    }

    /// Paint the card into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().control_corner_radius)
            .min(w / 2)
            .min(h / 2);

        // The plate: Signal Rim then the raised card surface.
        surface.fill_round_rect(x, y, w, h, radius, Color::from(palette.rim));
        let Some((ix, iy, iw, ih)) = inset(x, y, w, h, border) else {
            return;
        };
        surface.fill_round_rect(
            ix,
            iy,
            iw,
            ih,
            radius.saturating_sub(border),
            Color::from(palette.surface_raised),
        );

        // The leading dominant-state rail down the card's leading edge.
        let rail_w = rail_thickness(theme, scale).min(iw);
        surface.fill_rect(
            ix,
            iy,
            rail_w,
            ih,
            dominant_color(theme, self.role, self.state),
        );

        // The bottom progress seam (proportional for known work).
        let seam_h = seam_thickness(theme, scale).min(ih);
        let seam_w = seam_width(self.state.activity, iw);
        if seam_w > 0 {
            surface.fill_rect(
                ix,
                iy + ih - seam_h,
                seam_w,
                seam_h,
                Color::from(palette.accent),
            );
        }

        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let content_left = ix.saturating_add(rail_w).saturating_add(pad);
        let content_right = ix.saturating_add(iw).saturating_sub(pad);

        // The top-trailing count pill / alert bead first, then the title and
        // body up to whatever leading edge the badge left, then the footer
        // actions.
        let title_right = self.paint_badge(
            surface,
            (ix, iy, iw, ih),
            (rail_w, content_left, content_right),
            scale,
            theme,
            font,
        );
        self.paint_title(
            surface,
            (iy, content_left, title_right, content_right),
            pad,
            theme,
            font,
        );
        for (button, rect) in self
            .footer
            .iter()
            .zip(self.footer_rects(bounds, scale, theme))
        {
            button.render(surface, rect, scale, theme);
        }
    }

    /// Paint the top-trailing count pill (an accent plate with on-accent
    /// digits) or, when there is no count, the alert Signal Bead, returning the
    /// x the title text must stop before so it never runs under the badge.
    fn paint_badge(
        &self,
        surface: &mut Surface,
        inner: (u32, u32, u32, u32),
        cols: (u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> u32 {
        let (_, iy, iw, ih) = inner;
        let (rail_w, content_left, content_right) = cols;
        let palette = theme.palette();
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        if let Some(count) = self.count {
            let buf = itoa(count);
            let text: &str = &buf;
            let pill_h = font
                .glyph_height()
                .saturating_add(pad)
                .min(ih.saturating_sub(pad));
            let pill_w = font
                .text_width(text)
                .saturating_add(pad)
                .min(iw.saturating_sub(rail_w).saturating_sub(pad));
            if pill_w > 0 && pill_h > 0 && content_right > content_left.saturating_add(pill_w) {
                let px = content_right.saturating_sub(pill_w);
                let py = iy.saturating_add(pad);
                paint_count_badge(
                    surface,
                    (px, py, pill_w, pill_h),
                    Color::from(palette.accent),
                    Color::from(palette.on_accent),
                    font,
                    text,
                );
                return px.saturating_sub(pad);
            }
        } else if let Some((color, shape)) = resolve_bead(theme, self.state) {
            let size = scale.scale_length(theme.metrics().bead_size).max(3).min(ih);
            if content_right > content_left.saturating_add(size) {
                let bx = content_right.saturating_sub(size);
                paint_bead(surface, bx, iy.saturating_add(pad), size, color, shape);
                return bx.saturating_sub(pad);
            }
        }
        content_right
    }

    /// Paint the card title and its optional body line within the content
    /// columns `(title_top, content_left, title_right, content_right)`,
    /// leading-aligned at the content's left edge.
    fn paint_title(
        &self,
        surface: &mut Surface,
        cols: (u32, u32, u32, u32),
        pad: u32,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let (title_top, content_left, title_right, content_right) = cols;
        let fg = foreground(theme, self.state.disposition());
        let title_y = to_i32(title_top) + to_i32(pad);
        if title_right > content_left {
            let fitted = font.truncate_to_width(&self.title, title_right - content_left);
            font.draw_text(surface, to_i32(content_left), title_y, fitted, fg);
        }
        if let Some(body) = &self.body {
            let body_y = title_y + to_i32(font.line_height()) + to_i32(pad) / 2;
            if content_right > content_left {
                let fitted = font.truncate_to_width(body, content_right - content_left);
                font.draw_text(
                    surface,
                    to_i32(content_left),
                    body_y,
                    fitted,
                    Color::from(theme.palette().on_surface_muted),
                );
            }
        }
    }

    /// Feed a pointer event to the card, given its `bounds`: the first footer
    /// button that completes a click reports [`CardAction::FooterActivated`],
    /// and otherwise a completed primary click landing inside the card's own
    /// bounds — outside every footer button — reports [`CardAction::Pressed`].
    ///
    /// The footer is routed first, from one hit test: the event reaches only
    /// the button the pointer left, the one it entered, and any button
    /// holding a press. The body press is considered once none of them
    /// claimed it, and only over the area the footer buttons do not occupy,
    /// so a click cannot report both.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<CardAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let rects = self.footer_rects(bounds, scale, theme);
        let over = rects.iter().position(|rect| rect.contains(*self.pointer));
        let route = route_pointer(&mut self.footer_hovered, *self.footer_armed, over);
        *self.footer_armed = grab_after(*self.footer_armed, event, over);

        let mut action = None;
        for i in route.into_iter().flatten() {
            let (Some(button), Some(rect)) = (self.footer.get_mut(i), rects.get(i)) else {
                continue;
            };
            if button.on_pointer(event, *rect, damage) == Some(ButtonAction::Activated)
                && action.is_none()
            {
                action = Some(CardAction::FooterActivated { index: i });
            }
        }
        if action.is_some() {
            return action;
        }

        let over_footer = over.is_some();
        let inside = bounds.contains(*self.pointer) && !over_footer;
        let actionable = self.state.is_actionable();
        press_latch(&mut self.armed, event, inside, actionable).then_some(CardAction::Pressed)
    }

    /// Feed a key event to the card's footer buttons; a focused footer button
    /// activated with Space/Enter reports [`CardAction::FooterActivated`].
    pub fn on_key(&mut self, key: Key) -> Option<CardAction> {
        let mut action = None;
        for (i, button) in self.footer.iter_mut().enumerate() {
            if button.on_key(key) == Some(ButtonAction::Activated) && action.is_none() {
                action = Some(CardAction::FooterActivated { index: i });
            }
        }
        action
    }
}

// --- IconTile ----------------------------------------------------------

/// One item of an icon view: a picture with its name beneath it, and no plate
/// of its own (spec §11.34).
///
/// This is the tile a file manager's icon view and the desktop's icon field are
/// made of. A resting tile paints nothing but its picture and its label, so a
/// folder of items reads as a field of pictures over whatever lies behind them
/// — a window's surface, or the desktop wallpaper — rather than as a grid of
/// boxes. That is what separates it from a [`Card`]: a card is a *grouped
/// state-and-actions surface* and wears a plate to bound the group it owns; a
/// tile is one item among many and would only add a box per picture.
///
/// State is what makes a tile paint anything behind its picture:
/// * the pointer wash while hovered or pressed, in the shared plate colours;
/// * the selection fill, the palette's accent at three tenths opacity over the
///   whole tile, rounded like every other plate and laid crisply over a
///   frosted backdrop, so a selected item is plainly the accent while the
///   surface or wallpaper under it still reads through — the pointer washes
///   are different colours entirely, so neither can imitate it. An owner
///   animating a selection draws the mark part-way through
///   [`with_selection_fade`](Self::with_selection_fade);
/// * the keyboard Focus Ring, through the one shared outline every row and tab
///   family draws, so a focused tile reads distinctly from a hovered one. A
///   tile already wearing the selection fill takes no ring on top of it;
/// * the Signal Bead of an authority or recovery state, so a denied or
///   unhealthy item is legible without relying on colour.
///
/// The tile renders state and never dispatches. An icon view lays its tiles out
/// on a geometry it owns and hit-tests pointer input against that same
/// geometry, so a tile holds no pointer position or press latch of its own —
/// unlike a [`ListRow`], which is a control the user clicks directly.
///
/// Equal tiles draw the same pixels, so a host may use `==` as its repaint
/// gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IconTile {
    label: String,
    icon: IconKind,
    state: ControlState,
    selection_fade: Option<u8>,
}

impl IconTile {
    /// A resting, unselected tile drawing `icon` above `label`.
    #[must_use]
    pub fn new(label: impl Into<String>, icon: IconKind) -> Self {
        Self {
            label: label.into(),
            icon,
            state: ControlState::idle(),
            selection_fade: None,
        }
    }

    /// This tile with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// This tile with its selection mark drawn at `fade` strength: `0`
    /// unmarked, [`u8::MAX`] fully marked.
    ///
    /// For an owner moving a selection between items over the theme's
    /// [`SelectionChange`](tairix_theme::MotionInteraction::SelectionChange)
    /// duration: the item being left is already unselected while its mark
    /// decays, and the item arrived at is already selected while its mark
    /// grows, so the strength is the owner's to state and not the composed
    /// state's to infer. A host that does not animate sets nothing and the
    /// mark follows the selection — full when selected, absent when not.
    ///
    /// Set independently of [`with_state`](Self::with_state), so the two may
    /// be applied in either order.
    #[must_use]
    pub fn with_selection_fade(mut self, fade: u8) -> Self {
        self.selection_fade = Some(fade);
        self
    }

    /// The pixel side of the square picture a tile occupying `bounds` draws.
    ///
    /// This is the render geometry itself, exposed so an owner rasterising
    /// per-entry artwork produces it at exactly the size [`Self::render`] will
    /// place it — the two can never disagree. It depends only on the tile's
    /// bounds and the theme's metrics, never on which item the tile shows, so
    /// an owner can size a whole row of artwork from one query. `0` when the
    /// bounds are off-surface or leave no room for a picture.
    #[must_use]
    pub fn icon_side(bounds: Rect, scale: Scale, theme: &Theme) -> u32 {
        Self::icon_slot(bounds, scale, theme).map_or(0, |(_, _, side)| side)
    }

    /// Paint the tile into `surface` at `bounds` for the active theme.
    ///
    /// `artwork` is the item's own picture, pre-rasterised by the owner at
    /// [`Self::icon_side`] through its cache; `None` falls back to the built-in
    /// glyph for the tile's [`IconKind`], tinted like the label so the tile
    /// reads as one unit. Artwork is decoded and rasterised long before it
    /// reaches this call — a control never parses image bytes.
    ///
    /// Nothing is drawn outside `bounds`, so a view whose viewport cuts a tile
    /// short need only clip the surface it hands over.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<&Surface>,
    ) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let ink = self.label_color(theme);
        self.paint_backdrop(surface, (x, y, w, h), scale, theme);
        if let Some((ix, iy, side)) = Self::icon_slot(bounds, scale, theme) {
            paint_icon_slot(surface, ix, iy, side, self.icon, ink, artwork);
        }
        self.paint_label(surface, bounds, scale, theme, font, ink);
        self.paint_bead(surface, (x, y, w, h), scale, theme);
    }

    /// Paint whatever the tile's state puts *behind* its picture: the pointer
    /// wash or the selection fill, then the keyboard Focus Ring. A resting,
    /// unselected, unfocused tile paints nothing at all, which is what keeps an
    /// icon view a field of pictures rather than a grid of plates.
    fn paint_backdrop(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) {
        let (x, y, w, h) = rect;
        let palette = theme.palette();
        // A pressed tile recesses; otherwise the selection paints, and only a
        // tile that neither wears a mark nor is selected takes the shared
        // pointer wash, so the pointer never imitates selection and no wash
        // flashes under a mark that has begun arriving but has no strength
        // yet. The drag states carry no wash of their own here, exactly as
        // they carry no tint in the shared plate colours every other control
        // paints through: the drag vocabulary is one decision for the whole
        // control set, not one a tile invents for itself.
        if self.state.pointer == PointerState::Pressed {
            fill_panel(surface, rect, scale, theme, palette.surface_pressed);
        } else {
            let marked = self.paint_selection(surface, rect, scale, theme);
            if !marked && !self.wears_selection_mark() && self.state.pointer == PointerState::Hover
            {
                fill_panel(surface, rect, scale, theme, palette.surface_hover);
            }
        }
        // The ring is what tells a focused tile from a hovered one; a selected
        // tile needs no second edge, and an outline drawn for however long its
        // mark takes to arrive would read as a border flickering under the
        // pointer. Presence follows selection, never the mark's strength.
        if self.state.focus.focused && !self.wears_selection_mark() {
            draw_outline(
                surface,
                x,
                y,
                w,
                h,
                plate_border(theme, scale).max(1),
                Color::from(palette.rim_active),
            );
        }
    }

    /// Paint the tile's selection, reporting whether anything was drawn.
    ///
    /// A heavier contrast policy takes the crisp opaque accent panel, and
    /// takes it the instant the item is selected rather than fading in: a
    /// half-arrived plate under inverted ink is the very contrast that policy
    /// exists to guarantee.
    fn paint_selection(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) -> bool {
        if heavy_contrast(theme) {
            if !self.is_selected() {
                return false;
            }
            fill_panel(surface, rect, scale, theme, theme.palette().accent);
            return true;
        }
        let fade = self.selection_mark();
        if fade == 0 {
            return false;
        }
        paint_selection_fill(surface, rect, scale, theme, fade);
        true
    }

    /// How many whole lines of its name a tile occupying `bounds` draws.
    ///
    /// This is the render geometry itself, exposed so an owner sizing its tiles
    /// — a login chooser widening an account tile until a two-word display name
    /// fits — asks the tile rather than re-deriving its label layout. `0` when
    /// the tile is off-surface or its band cannot hold a whole line, which is
    /// exactly when [`Self::render`] draws no name at all.
    #[must_use]
    pub fn label_lines(bounds: Rect, scale: Scale, theme: &Theme) -> usize {
        Self::label_band(
            bounds,
            scale,
            theme,
            role_font(theme, scale, TextRole::Body),
        )
        .map_or(0, |band| band.lines)
    }

    /// Paint the tile's name under its picture: wrapped over as many whole
    /// lines as the band holds, each centred, the last elided when the name
    /// runs past them. A band with no room for a whole line draws nothing
    /// rather than clipping a glyph.
    fn paint_label(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        color: Color,
    ) {
        let Some(band) = Self::label_band(bounds, scale, theme, font) else {
            return;
        };
        let mut top = band.top;
        for line in font.wrap_to_width(&self.label, band.right - band.left, band.lines) {
            let run = (line.text, line.elided);
            let x = to_i32(centre_x(run_width(font, run), band.left, band.right));
            paint_run(surface, font, run, (x, to_i32(top)), color);
            top = top.saturating_add(font.line_height());
        }
    }

    /// Paint the Signal Bead of an authority or recovery state in the tile's
    /// top-trailing corner, so a denied or unhealthy item is legible without
    /// relying on colour. A tile in an ordinary state has no bead.
    fn paint_bead(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) {
        let Some((color, shape)) = resolve_bead(theme, self.state) else {
            return;
        };
        let (x, y, w, h) = rect;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let size = scale
            .scale_length(theme.metrics().bead_size)
            .max(3)
            .min(w)
            .min(h);
        let bx = x.saturating_add(w).saturating_sub(size).saturating_sub(pad);
        paint_bead(surface, bx, y.saturating_add(pad), size, color, shape);
    }

    /// Whether the tile is selected (a mixed selection counts, as it does for
    /// every other collection control).
    fn is_selected(&self) -> bool {
        matches!(
            self.state.selection,
            SelectionState::Selected | SelectionState::Mixed
        )
    }

    /// How strongly the tile draws its selection mark: the strength its owner
    /// set, else full for a selected tile and nothing for an unselected one.
    fn selection_mark(&self) -> u8 {
        self.selection_fade
            .unwrap_or(if self.is_selected() { u8::MAX } else { 0 })
    }

    /// Whether the selection is the tile's, whatever strength its mark is
    /// currently drawn at: selected, and not pressed — a press takes the
    /// recessed plate instead. What suppresses the pointer wash and the focus
    /// ring, so neither appears while a mark is arriving or leaving.
    fn wears_selection_mark(&self) -> bool {
        self.is_selected() && self.state.pointer != PointerState::Pressed
    }

    /// The colour the tile's label and built-in glyph take: the shared surface
    /// foreground for the tile's disposition, or the on-accent foreground on
    /// the one mark that is an opaque accent plate.
    ///
    /// The ordinary selection fill is half-opaque, so it tints what lies behind
    /// it rather than replacing it: the theme's own foreground keeps its
    /// separation whichever way the theme is lit, while a near-white on-accent
    /// ink would leave a light theme's name at barely twice the contrast of the
    /// pale orange under it.
    fn label_color(&self, theme: &Theme) -> Color {
        if heavy_contrast(theme) && self.wears_selection_mark() {
            return Color::from(theme.palette().on_accent);
        }
        foreground(theme, self.state.disposition())
    }

    /// The band a tile occupying `bounds` draws its name in: the column
    /// `[left, right)` each line is centred in, the top of the first line, and
    /// how many whole lines fit beneath the picture. `None` when the tile is
    /// off-surface or the band cannot hold one whole line.
    ///
    /// The one definition of that geometry, so the budget an owner reads
    /// ([`Self::label_lines`]) is the budget [`Self::render`] lays out to.
    fn label_band(
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<LabelBand> {
        let (x, y, w, h) = surface_rect(bounds)?;
        let top = match Self::icon_slot(bounds, scale, theme) {
            Some((_, iy, side)) => iy.saturating_add(side),
            None => y,
        };
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let left = x.saturating_add(pad);
        let right = x.saturating_add(w).saturating_sub(pad);
        let top = top.saturating_add(pad);
        let whole = y.saturating_add(h).saturating_sub(top) / font.line_height().max(1);
        let lines = usize::try_from(whole).unwrap_or(0);
        (right > left && lines > 0).then_some(LabelBand {
            left,
            right,
            top,
            lines,
        })
    }

    /// The square picture slot a tile occupying `bounds` reserves at the top of
    /// its content: `(x, y, side)`, centred across the tile and capped so at
    /// least the lower two-fifths of the tile stays for the label. `None` when
    /// the tile is off-surface or leaves no room for a picture.
    ///
    /// The one definition of that geometry, so the side an owner rasterises
    /// artwork at ([`Self::icon_side`]) is exactly the slot [`Self::render`]
    /// paints it into.
    fn icon_slot(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let (_, _, avail_w, avail_h) = inset(x, y, w, h, pad)?;
        let side = avail_w.min(avail_h.saturating_mul(3) / 5);
        if side == 0 {
            return None;
        }
        Some((
            x.saturating_add(pad).saturating_add((avail_w - side) / 2),
            y.saturating_add(pad),
            side,
        ))
    }
}

/// The x at which a run `width` pixels wide sits centred in the column
/// `[left, right)`, pinned to `left` when it is wider than the column.
fn centre_x(width: u32, left: u32, right: u32) -> u32 {
    let avail = right.saturating_sub(left);
    left.saturating_add((avail - width.min(avail)) / 2)
}

/// The band an [`IconTile`] draws its name in.
#[derive(Copy, Clone)]
struct LabelBand {
    /// The left edge of the column each line is centred in.
    left: u32,
    /// One past the right edge of that column.
    right: u32,
    /// The top of the first line's box.
    top: u32,
    /// How many whole lines the band holds.
    lines: usize,
}

/// The drawn width of a fitted run — the pair a fitter hands back, text and
/// whether [`ELLIPSIS`] follows it — mark included.
fn run_width(font: BitmapFont, run: (&str, bool)) -> u32 {
    let (text, elided) = run;
    let width = font.text_width(text);
    if elided {
        return width.saturating_add(font.text_width(ELLIPSIS));
    }
    width
}

/// Draw a fitted run at `at`, its mark included.
///
/// The one "text, then the mark" recipe every collection control paints cut
/// text through, so a hidden tail always says so rather than stopping
/// mid-word as if the text ended there. A run the fitter marked unelided —
/// including a box too narrow for the mark itself — draws its text alone.
fn paint_run(
    surface: &mut Surface,
    font: BitmapFont,
    run: (&str, bool),
    at: (i32, i32),
    color: Color,
) {
    let (text, elided) = run;
    let (x, y) = at;
    let pen = font.draw_text(surface, x, y, text, color);
    if elided {
        font.draw_text(surface, pen, y, ELLIPSIS, color);
    }
}

/// Fill an [`IconTile`]'s whole `rect` with one plate colour, rounded by the
/// theme's control radius (which the shared fill clamps to the shape).
fn fill_panel(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    fill: Rgba,
) {
    let (x, y, w, h) = rect;
    surface.fill_round_rect(x, y, w, h, plate_radius(scale, theme), Color::from(fill));
}

/// The corner radius an [`IconTile`]'s plate is rounded by, in physical pixels
/// at the active density. One definition, so the shape a mark's frost is
/// confined to is the shape its fill is drawn in.
fn plate_radius(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().control_corner_radius)
}

/// Paint the mark a selected [`IconTile`] wears: the pixels *behind* the tile
/// frosted through the shared region blur, then the theme's selection fill —
/// its accent at three tenths opacity, scaled by `fade` — laid over them with a
/// crisp, rounded edge.
///
/// The frost is what marks the item; the fill only tints it, which is why the
/// fill is as light as it is.
///
/// This is the same effect, through the same filter, the compositor frosts a
/// window's backdrop with: what the tile covers — a window's surface, the
/// desktop wallpaper — reads as blurred glass, while the fill's own edge stays
/// sharp so the mark keeps the shape every other plate has. Both the frost and
/// the fill are confined to that one rounded shape, so nothing is drawn
/// outside `rect` and no square edge shows around the rounded fill. `fade`
/// scales both, so a mark moving between items frosts as it colours and leaves
/// nothing behind it.
///
/// The frost's scratch is allocated per call: a tile rasterises its mark once
/// per repaint rather than once per frame, so the buffer reuse the compositor
/// needs would buy nothing here.
fn paint_selection_fill(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    fade: u8,
) {
    let (x, y, w, h) = rect;
    let base = theme.palette().selection_fill;
    let radius = plate_radius(scale, theme);
    let blur = scale.scale_length(theme.metrics().selection_backdrop_blur);
    let mut scratch = BlurScratch::new();
    surface.frost_region(x, y, w, h, blur, &mut scratch, |lx, ly| {
        div255(u32::from(round_rect_coverage(lx, ly, w, h, radius)) * u32::from(fade))
    });
    fill_panel(
        surface,
        rect,
        scale,
        theme,
        base.with_alpha(div255(u32::from(base.a) * u32::from(fade))),
    );
}

/// Render a `u32` as decimal into a small stack buffer, `no_std`-friendly.
fn itoa(mut value: u32) -> String {
    if value == 0 {
        return String::from("0");
    }
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    while value > 0 {
        i -= 1;
        digits[i] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
    }
    String::from_utf8_lossy(&digits[i..]).into_owned()
}

// --- Panel -------------------------------------------------------------

/// The edge of a [`Panel`] its anchor notch points from, toward the invoker.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PanelEdge {
    /// The panel's top edge (the invoker is above it).
    Top,
    /// The panel's bottom edge (the invoker is below it).
    Bottom,
    /// The panel's leading edge (the invoker is to its left).
    Left,
    /// The panel's trailing edge (the invoker is to its right).
    Right,
}

/// The outcome of feeding input to a [`Panel`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PanelAction {
    /// The grouped header action at `index` was activated.
    HeaderActivated {
        /// The zero-based index of the activated header action.
        index: usize,
    },
}

/// A stable-layout container surface (spec §11.16).
///
/// A panel is a plate with a header (a title, a header-state Signal Bead, and a
/// group of action [`Button`]s) above a content region the owner draws into
/// (`content_rect`). A panel opened from a taskbar or tray item keeps an anchor
/// notch pointing back at its invoker while open, so its origin is legible. The
/// panel routes input to its header actions and reports [`PanelAction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Panel {
    title: String,
    role: ControlRole,
    header_state: ControlState,
    actions: Vec<Button>,
    anchor: Option<Point>,
    /// The last pointer position, resolved against the header-action rects —
    /// hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The header action the pointer was last over, so a motion sample
    /// reaches the action it left and the one it entered rather than the
    /// whole header.
    hovered: RenderInvariant<Option<usize>>,
    /// The header action holding a press, which keeps receiving the stream
    /// wherever the pointer goes.
    armed: RenderInvariant<Option<usize>>,
}

impl Panel {
    /// A neutral panel with the given title and no header actions.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            role: ControlRole::Neutral,
            header_state: ControlState::idle(),
            actions: Vec::new(),
            anchor: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
            hovered: RenderInvariant::new(None),
            armed: RenderInvariant::new(None),
        }
    }

    /// This panel with a non-default role (drives the header's dominant state).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This panel with the given header state (drives the header Signal Bead).
    #[must_use]
    pub fn with_header_state(mut self, state: ControlState) -> Self {
        self.header_state = state;
        self
    }

    /// This panel with the given grouped header action buttons.
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<Button>) -> Self {
        self.actions = actions;
        self
    }

    /// This panel with an anchor at `invoker` (in the same surface coordinates
    /// as its bounds), so it draws a notch pointing back toward it.
    #[must_use]
    pub fn with_anchor(mut self, invoker: Point) -> Self {
        self.anchor = Some(invoker);
        self
    }

    /// The panel's header state.
    #[must_use]
    pub fn header_state(&self) -> ControlState {
        self.header_state
    }

    /// The panel's header action buttons.
    #[must_use]
    pub fn actions(&self) -> &[Button] {
        &self.actions
    }

    /// Mutable access to the header action buttons.
    pub fn actions_mut(&mut self) -> &mut [Button] {
        &mut self.actions
    }

    /// The scaled header-bar height.
    fn header_height(scale: Scale, theme: &Theme) -> u32 {
        scale.scale_length(theme.metrics().title_bar_height).max(1)
    }

    /// The inner plate rectangle (inside the rim) as surface pixels.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        inset(x, y, w, h, plate_border(theme, scale))
    }

    /// The header rectangle (the title/actions band) for the active theme.
    #[must_use]
    pub fn header_rect(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
        let (ix, iy, iw, ih) = Self::inner(bounds, scale, theme)?;
        let hh = Self::header_height(scale, theme).min(ih);
        Some(Rect::new(to_i32(ix), to_i32(iy), iw, hh))
    }

    /// The content rectangle below the header, which the owner draws into
    /// (a panel's content is scrollable and owner-managed, spec §11.16).
    #[must_use]
    pub fn content_rect(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
        let (ix, iy, iw, ih) = Self::inner(bounds, scale, theme)?;
        let hh = Self::header_height(scale, theme).min(ih);
        let ch = ih.saturating_sub(hh);
        if ch == 0 {
            return None;
        }
        Some(Rect::new(to_i32(ix), to_i32(iy + hh), iw, ch))
    }

    /// The header action rectangles, right-aligned square buttons in the
    /// header. Shared by rendering and pointer routing so they never disagree.
    fn action_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let mut rects = Vec::new();
        let Some(header) = self.header_rect(bounds, scale, theme) else {
            return rects;
        };
        if self.actions.is_empty() {
            return rects;
        }
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let side = header.height.saturating_sub(pad).max(1);
        let mut right = u32::try_from(header.left())
            .unwrap_or(0)
            .saturating_add(header.width)
            .saturating_sub(pad);
        let top =
            u32::try_from(header.top()).unwrap_or(0) + (header.height.saturating_sub(side)) / 2;
        let left_bound = u32::try_from(header.left())
            .unwrap_or(0)
            .saturating_add(pad);
        for _ in 0..self.actions.len() {
            if right <= left_bound.saturating_add(side) {
                break;
            }
            let bx = right.saturating_sub(side);
            rects.push(Rect::new(to_i32(bx), to_i32(top), side, side));
            right = bx.saturating_sub(gap);
        }
        rects
    }

    /// The edge the anchor notch points from, given the panel's bounds, or
    /// `None` if the panel has no anchor.
    #[must_use]
    pub fn anchor_edge(&self, bounds: Rect) -> Option<PanelEdge> {
        let anchor = self.anchor?;
        let left = bounds.left();
        let right = bounds.left() + to_i32(bounds.width);
        let top = bounds.top();
        let bottom = bounds.top() + to_i32(bounds.height);
        if anchor.y < top {
            Some(PanelEdge::Top)
        } else if anchor.y >= bottom {
            Some(PanelEdge::Bottom)
        } else if anchor.x < left {
            Some(PanelEdge::Left)
        } else if anchor.x >= right {
            Some(PanelEdge::Right)
        } else {
            // The invoker overlaps the panel: default to a bottom notch so the
            // route line still reads rather than vanishing (fail safe).
            Some(PanelEdge::Bottom)
        }
    }

    /// Paint the panel into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().window_corner_radius)
            .min(w / 2)
            .min(h / 2);

        // The plate: Signal Rim then the base surface (content area).
        surface.fill_round_rect(x, y, w, h, radius, Color::from(palette.rim));
        let Some((ix, iy, iw, ih)) = inset(x, y, w, h, border) else {
            return;
        };
        surface.fill_round_rect(
            ix,
            iy,
            iw,
            ih,
            radius.saturating_sub(border),
            Color::from(palette.surface),
        );

        // The header band on the raised surface, with a leading dominant rail.
        let hh = Self::header_height(scale, theme).min(ih);
        surface.fill_rect(ix, iy, iw, hh, Color::from(palette.surface_raised));
        let rail_w = rail_thickness(theme, scale).min(iw);
        surface.fill_rect(
            ix,
            iy,
            rail_w,
            hh,
            dominant_color(theme, self.role, self.header_state),
        );

        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let action_rects = self.action_rects(bounds, scale, theme);
        let actions_left = action_rects
            .iter()
            .map(|r| u32::try_from(r.left()).unwrap_or(0))
            .min();
        let header_left = ix.saturating_add(rail_w).saturating_add(pad);
        let mut header_right = actions_left
            .map_or(ix.saturating_add(iw).saturating_sub(pad), |a| {
                a.saturating_sub(pad)
            });

        // The header-state Signal Bead just inside the actions.
        if let Some((color, shape)) = resolve_bead(theme, self.header_state) {
            let size = scale.scale_length(theme.metrics().bead_size).max(3).min(hh);
            if size > 0 && header_right > header_left.saturating_add(size) {
                let bx = header_right.saturating_sub(size);
                let by = iy + (hh.saturating_sub(size)) / 2;
                paint_bead(surface, bx, by, size, color, shape);
                header_right = bx.saturating_sub(pad);
            }
        }

        // The title, left-aligned in the header.
        let fg = foreground(theme, self.header_state.disposition());
        if header_right > header_left {
            let text_y = centred_text_y(font, iy, hh);
            let fitted = font.truncate_to_width(&self.title, header_right - header_left);
            font.draw_text(surface, to_i32(header_left), text_y, fitted, fg);
        }

        // The grouped header actions.
        for (button, rect) in self.actions.iter().zip(&action_rects) {
            button.render(surface, *rect, scale, theme);
        }

        // The anchor notch pointing back toward the invoker.
        if let Some(edge) = self.anchor_edge(bounds) {
            Self::paint_notch(surface, (x, y, w, h), scale, theme, edge);
        }
    }

    /// Paint the anchor notch: a small triangle on `edge` pointing outward,
    /// toward the invoker, in the panel's rim colour so it reads as part of the
    /// frame.
    fn paint_notch(
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        edge: PanelEdge,
    ) {
        let (x, y, w, h) = rect;
        let size = scale
            .scale_length(theme.metrics().bead_size)
            .max(4)
            .min(w / 2)
            .min(h / 2);
        if size == 0 {
            return;
        }
        let Some(mut glyph) = Surface::new(size, size) else {
            return;
        };
        let span = to_i32(size);
        // A triangle on a size×size grid pointing outward for the chosen edge.
        let points: [(i32, i32); 3] = match edge {
            PanelEdge::Top => [(0, span), (span, span), (span / 2, 0)],
            PanelEdge::Bottom => [(0, 0), (span, 0), (span / 2, span)],
            PanelEdge::Left => [(span, 0), (span, span), (0, span / 2)],
            PanelEdge::Right => [(0, 0), (0, span), (span, span / 2)],
        };
        glyph.fill_polygon(&points, size, Color::from(theme.palette().rim));
        // Position the notch centred along the chosen edge, protruding outward.
        let (nx, ny) = match edge {
            PanelEdge::Top => (x + (w.saturating_sub(size)) / 2, y.saturating_sub(size)),
            PanelEdge::Bottom => (x + (w.saturating_sub(size)) / 2, y + h),
            PanelEdge::Left => (x.saturating_sub(size), y + (h.saturating_sub(size)) / 2),
            PanelEdge::Right => (x + w, y + (h.saturating_sub(size)) / 2),
        };
        surface.blit(to_i32(nx), to_i32(ny), &glyph);
    }

    /// Route a pointer event to the header actions it concerns; one that
    /// completes a click reports [`PanelAction::HeaderActivated`].
    ///
    /// One hit test decides where the pointer is; the event then reaches only
    /// the action it left, the action it entered, and any action holding a
    /// press.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<PanelAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let rects = self.action_rects(bounds, scale, theme);
        let over = rects.iter().position(|rect| rect.contains(*self.pointer));
        let route = route_pointer(&mut self.hovered, *self.armed, over);
        *self.armed = grab_after(*self.armed, event, over);

        let mut action = None;
        for i in route.into_iter().flatten() {
            let (Some(button), Some(rect)) = (self.actions.get_mut(i), rects.get(i)) else {
                continue;
            };
            if button.on_pointer(event, *rect, damage) == Some(ButtonAction::Activated)
                && action.is_none()
            {
                action = Some(PanelAction::HeaderActivated { index: i });
            }
        }
        action
    }

    /// Feed a key event to the header actions; a focused header action
    /// activated with Space/Enter reports [`PanelAction::HeaderActivated`].
    pub fn on_key(&mut self, key: Key) -> Option<PanelAction> {
        let mut action = None;
        for (i, button) in self.actions.iter_mut().enumerate() {
            if button.on_key(key) == Some(ButtonAction::Activated) && action.is_none() {
                action = Some(PanelAction::HeaderActivated { index: i });
            }
        }
        action
    }
}
