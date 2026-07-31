//! The collection controls: [`ListRow`], [`TableRow`], [`TableCell`],
//! [`Card`], and [`Panel`] (spec §11.13–§11.16).
//!
//! Rows, tables, cards, and panels are the surfaces that *group* other state
//! and actions. They are controls in their own right: a row can be hovered,
//! selected, focused, and activated; a table keeps its columns aligned while a
//! row's state changes; a card carries a dominant state on its leading edge,
//! progress on its bottom edge, and a count/alert on its top-trailing corner;
//! a panel is a stable-layout container with a header, grouped actions, and a
//! content region, optionally pointing an anchor notch back at its invoker.
//!
//! Every visible property resolves from the active [`Theme`] and [`Scale`], and
//! every shared recipe (plate rounding, Signal Bead, Pressure Rail, focus ring)
//! comes from the shared `crate::paint` core rather than a second copy. A
//! control renders state and emits a typed action; it performs no privileged
//! work — the owning service enforces authority (`AGENTS.md` §5.4). A denied
//! surface keeps its value and shows an Authority Mark rather than collapsing
//! into a plain disabled look (spec §13).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::{builtin_icon, IconKind};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::button::{Button, ButtonAction};
use crate::paint::{
    dominant_color, draw_outline, foreground, inset, key_activation, paint_bead, paint_count_badge,
    plate_border, pointer_activation, rail_thickness, resolve_bead, resolve_rail, seam_thickness,
    seam_width, surface_rect, to_i32,
};
use crate::state::{ControlDisposition, ControlRole, ControlState, SelectionState};

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
// to how a selected or pressured row reads cannot diverge between the two
// (`AGENTS.md` §2.2).

/// Paint the shared row chrome — background tint, leading pressure and
/// selection rails, the bottom activity Heat Seam, the trailing Signal Bead,
/// and the keyboard focus ring — into `rect`, returning the inner content
/// rectangle `(x, y, w, h)` the caller draws its label or cells within.
///
/// Returning the content rect (already inset past the leading rails and the
/// trailing bead) keeps the column-alignment contract in one place: the
/// caller lays content out relative to this rect, so a row's state changing
/// never shifts where its content begins (spec §11.13 "keep columns aligned").
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

    // Background tint: a pressed row recesses; a selected or hovered row lifts
    // to the raised surface (selection is further distinguished by its accent
    // rail below); a resting row is the base surface.
    let fill = match state.pointer {
        crate::state::PointerState::Pressed => palette.surface_pressed,
        _ if selected => palette.surface_raised,
        crate::state::PointerState::Hover => palette.surface_raised,
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
    let gutter = rail_w.saturating_mul(2).min(w);
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

    // The trailing Signal Bead (denied lock / recovery diamond / complete
    // check), so a recovery or authority state reads without colour (§13, §15).
    let border = plate_border(theme, scale);
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let mut right = x.saturating_add(w).saturating_sub(pad);
    if let Some((color, shape)) = resolve_bead(theme, state) {
        let size = scale
            .scale_length(theme.metrics().bead_size)
            .max(3)
            .min(h.saturating_sub(border.saturating_mul(2)));
        if size > 0 && right > lead.saturating_add(size) {
            let bx = right.saturating_sub(size);
            let by = y + (h.saturating_sub(size)) / 2;
            paint_bead(surface, bx, by, size, color, shape);
            right = bx.saturating_sub(pad);
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

    let content_left = lead.saturating_add(pad);
    let content_right = right;
    if content_right <= content_left {
        return None;
    }
    Some((content_left, y, content_right - content_left, h))
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListRow {
    label: String,
    trailing: Option<String>,
    icon: Option<IconKind>,
    role: ControlRole,
    state: ControlState,
    pointer: Point,
    armed: bool,
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
            pointer: Point::ORIGIN,
            armed: false,
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

    /// Paint the row into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
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
        let icon_slot = font.glyph_height().min(ch);
        if let Some(kind) = self.icon {
            if icon_slot > 0 {
                if let Some(image) = builtin_icon(kind, fg).rasterise(icon_slot) {
                    let iy = to_i32(cy) + (to_i32(ch) - to_i32(icon_slot)).max(0) / 2;
                    surface.blit(to_i32(left), iy, &image);
                }
            }
            left = left.saturating_add(icon_slot).saturating_add(pad);
        }

        let mut right = cx.saturating_add(cw);
        // The trailing caption, muted and right-aligned inside the content.
        if let Some(text) = &self.trailing {
            if right > left {
                let fitted = font.truncate_to_width(text, right - left);
                let tw = font.text_width(fitted);
                let tx = to_i32(right) - to_i32(tw);
                font.draw_text(surface, tx, text_y, fitted, muted);
                right = right.saturating_sub(tw).saturating_sub(pad);
            }
        }

        // The label, left-aligned in the remaining space.
        if right > left {
            let fitted = font.truncate_to_width(&self.label, right - left);
            font.draw_text(surface, to_i32(left), text_y, fitted, fg);
        }
    }

    /// Feed a pointer event, given the row's `bounds`; a completed primary
    /// click over an actionable row reports [`RowAction::Activated`].
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<RowAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let inside = bounds.contains(self.pointer);
        pointer_activation(&mut self.state, &mut self.armed, event, inside)
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
/// A cell draws its text aligned within its column. Row-wide state belongs to
/// the row; a cell exposes its *own* composed state only when that state is
/// cell-specific (e.g. one invalid field in an otherwise-valid row), in which
/// case the cell draws its own Signal Bead and takes its own foreground.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableCell {
    text: String,
    align: CellAlign,
    state: Option<ControlState>,
}

impl TableCell {
    /// A leading-aligned cell with the given text and no cell-specific state.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            align: CellAlign::Leading,
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
            state: None,
        }
    }

    /// This cell with the given alignment.
    #[must_use]
    pub fn with_align(mut self, align: CellAlign) -> Self {
        self.align = align;
        self
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

    /// Paint this cell into the column rectangle `(x, y, w, h)`, taking its
    /// foreground from its own state when it has one, otherwise from the row's
    /// `row_disposition`.
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

        let left = x.saturating_add(pad);
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableRow {
    cells: Vec<TableCell>,
    role: ControlRole,
    state: ControlState,
    pointer: Point,
    armed: bool,
}

impl TableRow {
    /// A neutral, enabled, unselected row over the given cells.
    #[must_use]
    pub fn new(cells: Vec<TableCell>) -> Self {
        Self {
            cells,
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            pointer: Point::ORIGIN,
            armed: false,
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
    /// `columns` (physical pixel widths). The last cell absorbs any rounding
    /// remainder so the columns fill the content width exactly.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        columns: &[u32],
    ) {
        let Some(rect) = surface_rect(bounds) else {
            return;
        };
        let Some((cx, cy, cw, ch)) = paint_row(surface, rect, scale, theme, self.state) else {
            return;
        };
        let disposition = self.state.disposition();
        let declared: u32 = columns.iter().copied().fold(0, u32::saturating_add);
        if declared == 0 {
            return;
        }
        let mut col_x = cx;
        let content_end = cx.saturating_add(cw);
        for (i, cell) in self.cells.iter().enumerate() {
            let Some(&declared_w) = columns.get(i) else {
                break;
            };
            if col_x >= content_end {
                break;
            }
            // Scale the declared column widths into the actual content width so
            // the columns stay proportional even when the content rect is
            // narrower than the sum of the declared widths.
            let scaled_w =
                u32::try_from(u64::from(declared_w) * u64::from(cw) / u64::from(declared))
                    .unwrap_or(cw);
            let is_last = i + 1 == self.cells.len() || i + 1 == columns.len();
            let col_w = if is_last {
                content_end.saturating_sub(col_x)
            } else {
                scaled_w.min(content_end.saturating_sub(col_x))
            };
            cell.paint(
                surface,
                (col_x, cy, col_w, ch),
                scale,
                theme,
                font,
                disposition,
            );
            col_x = col_x.saturating_add(col_w);
        }
    }

    /// Feed a pointer event, given the row's `bounds`; a completed primary
    /// click over an actionable row reports [`RowAction::Activated`].
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<RowAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let inside = bounds.contains(self.pointer);
        pointer_activation(&mut self.state, &mut self.armed, event, inside)
            .then_some(RowAction::Activated)
    }

    /// Feed a key event; Space/Enter activates a focused, actionable row.
    pub fn on_key(&mut self, key: Key) -> Option<RowAction> {
        key_activation(self.state, key).then_some(RowAction::Activated)
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
}

/// A grouped state-and-actions surface (spec §11.15).
///
/// A card carries its dominant state on its leading edge, its progress on its
/// bottom edge, and a count or alert bead on its top-trailing corner, with a
/// title, optional body text, and a row of footer action [`Button`]s. The
/// footer actions share the card's semantic state (the owner sets it) but keep
/// their own pointer and focus states, so hovering one action does not disturb
/// the card. The card routes input to its footer and reports [`CardAction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
    title: String,
    body: Option<String>,
    icon: Option<IconKind>,
    role: ControlRole,
    state: ControlState,
    count: Option<u32>,
    footer: Vec<Button>,
}

impl Card {
    /// A neutral, enabled card with the given title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: None,
            icon: None,
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            count: None,
            footer: Vec::new(),
        }
    }

    /// This card with an identifying glyph drawn above the title (e.g. a file
    /// manager grid tile's file-type icon). A card with no icon keeps its
    /// title at the top, so status/notification cards are unaffected.
    #[must_use]
    pub fn with_icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
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
    /// the card's content, above the progress seam. Shared by rendering and
    /// pointer routing so the two never disagree.
    fn footer_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
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
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
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

        // An optional identifying glyph centred in a top band; when present the
        // title sits below it and both centre under it. The top-trailing count
        // pill / alert bead follows, then the title and body up to whatever
        // leading edge the badge left, then the footer actions.
        let title_top = self.paint_icon(surface, (iy, ih, content_left, content_right), pad, theme);
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
            (title_top, content_left, title_right, content_right),
            pad,
            theme,
            font,
            self.icon.is_some(),
        );
        for (button, rect) in self
            .footer
            .iter()
            .zip(self.footer_rects(bounds, scale, theme))
        {
            button.render(surface, rect, scale, theme, font);
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

    /// Paint an optional identifying glyph centred in a band at the top of the
    /// content, returning the y the title should start at: the icon's bottom
    /// edge when an icon was drawn, else the content top `iy` unchanged. The
    /// icon is sized to the content width but capped so the title keeps room
    /// below it, and is tinted like the title so the tile reads as one unit.
    fn paint_icon(
        &self,
        surface: &mut Surface,
        inner: (u32, u32, u32, u32),
        pad: u32,
        theme: &Theme,
    ) -> u32 {
        let (iy, ih, content_left, content_right) = inner;
        let Some(kind) = self.icon else {
            return iy;
        };
        let avail_w = content_right.saturating_sub(content_left);
        // Leave at least the lower two-fifths of the tile for the label.
        let slot = avail_w.min(ih.saturating_mul(3) / 5);
        if slot == 0 {
            return iy;
        }
        let fg = foreground(theme, self.state.disposition());
        if let Some(image) = builtin_icon(kind, fg).rasterise(slot) {
            let ix_icon = content_left.saturating_add((avail_w.saturating_sub(slot)) / 2);
            surface.blit(to_i32(ix_icon), to_i32(iy.saturating_add(pad)), &image);
        }
        iy.saturating_add(pad).saturating_add(slot)
    }

    /// Paint the card title and its optional body line within the content
    /// columns `(title_top, content_left, title_right, content_right)`. When
    /// `centered` (an icon sits above), the title and body centre under it;
    /// otherwise they are leading-aligned at the content's left edge.
    fn paint_title(
        &self,
        surface: &mut Surface,
        cols: (u32, u32, u32, u32),
        pad: u32,
        theme: &Theme,
        font: BitmapFont,
        centered: bool,
    ) {
        let (title_top, content_left, title_right, content_right) = cols;
        let fg = foreground(theme, self.state.disposition());
        let title_y = to_i32(title_top) + to_i32(pad);
        if title_right > content_left {
            let fitted = font.truncate_to_width(&self.title, title_right - content_left);
            let tx = text_x(font, fitted, content_left, title_right, centered);
            font.draw_text(surface, tx, title_y, fitted, fg);
        }
        if let Some(body) = &self.body {
            let body_y = title_y + to_i32(font.line_height()) + to_i32(pad) / 2;
            if content_right > content_left {
                let fitted = font.truncate_to_width(body, content_right - content_left);
                let bx = text_x(font, fitted, content_left, content_right, centered);
                font.draw_text(
                    surface,
                    bx,
                    body_y,
                    fitted,
                    Color::from(theme.palette().on_surface_muted),
                );
            }
        }
    }

    /// Feed a pointer event to the card's footer buttons; the first footer
    /// button that completes a click reports [`CardAction::FooterActivated`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<CardAction> {
        let rects = self.footer_rects(bounds, scale, theme);
        let mut action = None;
        for (i, button) in self.footer.iter_mut().enumerate() {
            if let Some(rect) = rects.get(i) {
                if button.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    && action.is_none()
                {
                    action = Some(CardAction::FooterActivated { index: i });
                }
            }
        }
        action
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

/// The x at which to draw `text` within the column `[left, right)`: leading
/// (at `left`) normally, or centred within the column when `centered`.
fn text_x(font: BitmapFont, text: &str, left: u32, right: u32, centered: bool) -> i32 {
    if !centered {
        return to_i32(left);
    }
    let avail = right.saturating_sub(left);
    let tw = font.text_width(text).min(avail);
    to_i32(left) + (to_i32(avail) - to_i32(tw)) / 2
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
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
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
            button.render(surface, *rect, scale, theme, font);
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

    /// Feed a pointer event to the header actions; the first that completes a
    /// click reports [`PanelAction::HeaderActivated`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<PanelAction> {
        let rects = self.action_rects(bounds, scale, theme);
        let mut action = None;
        for (i, button) in self.actions.iter_mut().enumerate() {
            if let Some(rect) = rects.get(i) {
                if button.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    && action.is_none()
                {
                    action = Some(PanelAction::HeaderActivated { index: i });
                }
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
