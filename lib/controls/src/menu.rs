//! The menu command surface: [`MenuItem`] and [`Menu`] (spec §11.10).
//!
//! A menu is a *pinned command plate*, not floating ornament: an elevated
//! `surface_raised` plate with a Signal Rim, carrying a column of row controls.
//! Each [`MenuItem`] is a row with a label and an optional leading icon,
//! trailing shortcut, submenu marker, and reason. The [`Menu`] owns keyboard
//! navigation (Up/Down move the current row, Enter/Space activate it, Escape
//! dismisses), pointer hover/click, and emits a typed [`MenuAction`]; it
//! performs no privileged work — the owner enforces authority. Every colour,
//! metric, radius, and *face* resolves from the active [`Theme`]
//! and [`Scale`]; nothing here restates a recipe the shared `crate::paint`
//! core already owns.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::{builtin_icon, IconKind};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::damage;
use crate::paint::{
    draw_outline, heavy_contrast, inset, paint_bead, paint_chevron, plate_border, resolve_bead,
    role_font, surface_rect, text_plate_height, to_i32, ChevronDir,
};
use crate::state::{ControlDisposition, ControlRole, ControlState, RenderInvariant};

/// The outcome of feeding input to a [`Menu`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MenuAction {
    /// The item at `index` was activated and its command should be dispatched.
    Activated {
        /// The zero-based index of the activated row.
        index: usize,
    },
    /// The submenu owned by the item at `index` should be opened (the row is a
    /// submenu parent and was activated or entered from its edge).
    OpenSubmenu {
        /// The zero-based index of the submenu-parent row.
        index: usize,
    },
    /// The menu should be dismissed without activating anything (Escape).
    Dismissed,
}

/// One row of a menu (spec §11.10): a label with an optional leading icon,
/// trailing shortcut, submenu marker, and — for a disabled row — a reason.
///
/// A [`ControlRole::Destructive`] item draws a danger rail on its own row only.
/// A denied item keeps its slot and shows an Authority Mark rather than looking
/// merely disabled (spec §13). The row renders state and never dispatches.
///
/// An item may additionally open a *group*, drawing a divider rule in the gap
/// above it (see [`with_group_break`](Self::with_group_break)). Grouping is a
/// property of the row that begins the group rather than a row of its own, so
/// a divider can never be highlighted, hit-tested, or activated — the
/// invariant holds by construction instead of by a runtime guard — and every
/// index a [`Menu`] reports stays a direct index into the owner's own command
/// list, with no separator slots to translate around.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuItem {
    label: String,
    shortcut: Option<String>,
    icon: Option<IconKind>,
    submenu: bool,
    reason: Option<String>,
    role: ControlRole,
    state: ControlState,
    group_break: bool,
}

impl MenuItem {
    /// A neutral, enabled menu item with the given label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            shortcut: None,
            icon: None,
            submenu: false,
            reason: None,
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            group_break: false,
        }
    }

    /// This item with a trailing shortcut caption (e.g. `"Ctrl+S"`).
    #[must_use]
    pub fn with_shortcut(mut self, shortcut: impl Into<String>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// This item with a leading icon glyph.
    #[must_use]
    pub fn with_icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    /// This item marked as a submenu parent (draws a trailing anchor chevron).
    #[must_use]
    pub fn with_submenu(mut self, submenu: bool) -> Self {
        self.submenu = submenu;
        self
    }

    /// This item with a concise reason shown when it is disabled and current
    /// (spec §11.10). The reason is user-facing text, never a secret.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// This item with a non-default role (e.g. [`ControlRole::Destructive`]).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This item marked as the first of a new group, drawing a divider rule
    /// in the gap above it.
    ///
    /// A group break on the first row draws nothing: there is no preceding
    /// group to divide it from, and a rule flush against the plate rim would
    /// read as a second rim.
    #[must_use]
    pub fn with_group_break(mut self, group_break: bool) -> Self {
        self.group_break = group_break;
        self
    }

    /// This item with the given composed state (its authority/enabled fields).
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// The item's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The item's disabled-row reason, if one was set.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The item's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.role
    }

    /// The item's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the item's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Whether the item is a submenu parent.
    #[must_use]
    pub fn is_submenu(&self) -> bool {
        self.submenu
    }

    /// Whether the item opens a new group (drawing a divider above it).
    #[must_use]
    pub fn is_group_break(&self) -> bool {
        self.group_break
    }

    /// Whether activating this row (by pointer or keyboard) will dispatch.
    fn is_actionable(&self) -> bool {
        self.state.is_actionable()
    }

    /// Paint this row into `surface` at `rect` for the active theme.
    ///
    /// `current` marks the highlighted row; `focused` additionally marks that
    /// the highlight came from the keyboard, drawing a distinct focus ring so
    /// keyboard focus reads differently from a pointer hover (spec §15).
    #[allow(clippy::too_many_arguments)]
    fn paint(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        current: bool,
        focused: bool,
    ) {
        let (x, y, w, h) = rect;
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let actionable = self.is_actionable();

        // The row highlight for the current item. An actionable row lifts to
        // its emphasis colour (danger for a destructive item, else accent); a
        // non-actionable current row (denied, disabled, pending) shows only a
        // quiet pressed tint so it never masquerades as an available action.
        if current {
            let fill = if actionable {
                match self.role {
                    ControlRole::Destructive => palette.danger,
                    ControlRole::Recovery => palette.recovery,
                    _ => palette.accent,
                }
            } else {
                palette.surface_pressed
            };
            surface.fill_rect(x, y, w, h, Color::from(fill));
        }

        // A destructive row carries a danger rail on its own leading edge only.
        if self.role == ControlRole::Destructive {
            let rail_w = scale
                .scale_length(theme.metrics().rail_thickness)
                .max(1)
                .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
                .min(w);
            surface.fill_rect(x, y, rail_w, h, Color::from(palette.danger));
        }

        // The keyboard focus ring: an inset outline distinct from a hover fill.
        if focused {
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

        self.paint_content(surface, rect, scale, theme, font, current);
    }

    /// Paint the row foreground — icon, label, caption, Signal Bead, and the
    /// submenu chevron — over whatever background [`MenuItem::paint`] has
    /// already laid down.
    fn paint_content(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        current: bool,
    ) {
        let (x, y, w, h) = rect;
        let palette = theme.palette();
        let disposition = self.state.disposition();
        let actionable = self.is_actionable();
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);

        // Foreground colours: an actionable current row reads on its emphasis
        // fill; a disabled row mutes; everything else is the normal foreground.
        let (label_color, muted_color) = if current && actionable {
            (palette.on_accent, palette.on_accent)
        } else if disposition == ControlDisposition::DisabledByState {
            (palette.on_surface_muted, palette.on_surface_muted)
        } else {
            (palette.on_surface, palette.on_surface_muted)
        };

        let glyph_h = font.glyph_height();
        let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
        let left = x.saturating_add(border).saturating_add(pad);
        let mut cursor = left;
        let icon_slot = glyph_h.min(h.saturating_sub(border.saturating_mul(2)));

        // Leading icon (optional). The icon column is reserved even for a row
        // with no icon, so a menu's labels line up (text stability, spec §14).
        if let Some(kind) = self.icon {
            if icon_slot > 0 {
                if let Some(image) =
                    builtin_icon(kind, Color::from(label_color)).rasterise(icon_slot)
                {
                    let iy = to_i32(y) + (to_i32(h) - to_i32(icon_slot)).max(0) / 2;
                    surface.blit(to_i32(cursor), iy, &image);
                }
            }
        }
        cursor = cursor.saturating_add(icon_slot).saturating_add(pad);

        // Trailing region: the submenu chevron sits at the far edge; the
        // shortcut (or a disabled row's reason) sits just inside it.
        let right = x
            .saturating_add(w)
            .saturating_sub(border)
            .saturating_sub(pad);
        let chevron_w = if self.submenu { glyph_h } else { 0 };
        let mut trailing = right.saturating_sub(chevron_w);

        // The Signal Bead (denied lock / recovery / complete) at the trailing
        // edge, before the chevron.
        if let Some((color, shape)) = resolve_bead(theme, self.state) {
            let size = scale
                .scale_length(theme.metrics().bead_size)
                .max(3)
                .min(h.saturating_sub(border.saturating_mul(2)));
            if size > 0 && trailing > cursor.saturating_add(size) {
                let bx = trailing.saturating_sub(size);
                let by = y + (h.saturating_sub(size)) / 2;
                paint_bead(surface, bx, by, size, color, shape);
                trailing = bx.saturating_sub(pad);
            }
        }

        // The caption: a disabled current row shows its reason; otherwise the
        // shortcut. Both are muted and right-aligned inside the trailing edge.
        let caption = if disposition == ControlDisposition::DisabledByState && current {
            self.reason.as_deref().or(self.shortcut.as_deref())
        } else {
            self.shortcut.as_deref()
        };
        if let Some(text) = caption {
            if trailing > cursor {
                let budget = trailing - cursor;
                let fitted = font.truncate_to_width(text, budget);
                let tw = font.text_width(fitted);
                let tx = to_i32(trailing) - to_i32(tw);
                font.draw_text(surface, tx, text_y, fitted, Color::from(muted_color));
                trailing = trailing.saturating_sub(tw).saturating_sub(pad);
            }
        }

        // The label, left-aligned, filling the space up to the trailing region.
        if trailing > cursor {
            let budget = trailing - cursor;
            let fitted = font.truncate_to_width(&self.label, budget);
            font.draw_text(
                surface,
                to_i32(cursor),
                text_y,
                fitted,
                Color::from(label_color),
            );
        }

        // The submenu anchor chevron at the far trailing edge.
        if self.submenu && chevron_w > 0 {
            let cx = right.saturating_sub(chevron_w);
            paint_chevron(
                surface,
                Rect::new(to_i32(cx), to_i32(y), chevron_w, h),
                ChevronDir::Right,
                Color::from(label_color),
            );
        }
    }
}

/// One row's laid-out vertical geometry, relative to a [`Menu`]'s inner
/// content top.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct RowBand {
    /// The row's index in the menu.
    index: usize,
    /// The height of the group-divider band immediately above the row, or `0`
    /// when the row does not open a group.
    divider: u32,
    /// The row's own top edge, below any divider band.
    top: u32,
    /// The row's height.
    height: u32,
}

/// A pinned command plate carrying a column of [`MenuItem`] rows (spec §11.10).
///
/// The menu owns highlight state and input: Up/Down move the current row
/// (wrapping), Home/End jump to the ends, Enter/Space activate the current row
/// (opening a submenu parent), and Escape dismisses. Pointer hover sets the
/// current row and a primary click activates it. Every activation is a typed
/// [`MenuAction`] the owner dispatches; the menu enforces no authority. A
/// non-actionable row (disabled, denied, pending, failed-closed) can be
/// highlighted — so its reason and Authority Mark are legible — but never
/// activates (fail closed).
///
/// Equal menus draw the same pixels, so a host may use `==` as its repaint
/// gate: the rows, the highlighted row, and whether that highlight came from
/// the keyboard all compare. The pointer coordinate and the pressed-row latch
/// do not — no render path reads either, and the *visible* consequence of a
/// press is the highlight the same event sets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Menu {
    items: Vec<MenuItem>,
    current: Option<usize>,
    keyboard_focus: bool,
    /// The last pointer position, mapped to a row on the next press or
    /// release — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The row a primary press landed on, held until release so a click that
    /// slides onto a different row does not activate it; the pressed row's
    /// *look* is `current`.
    armed: RenderInvariant<Option<usize>>,
}

impl Menu {
    /// A menu over the given rows, with no row highlighted.
    #[must_use]
    pub fn new(items: Vec<MenuItem>) -> Self {
        Self {
            items,
            current: None,
            keyboard_focus: false,
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(None),
        }
    }

    /// This menu with the given row highlighted from the keyboard.
    #[must_use]
    pub fn with_current(mut self, index: usize) -> Self {
        if index < self.items.len() {
            self.current = Some(index);
            self.keyboard_focus = true;
        }
        self
    }

    /// The menu's rows.
    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// Mutable access to the menu's rows (e.g. to update a row's state).
    pub fn items_mut(&mut self) -> &mut [MenuItem] {
        &mut self.items
    }

    /// The number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the menu has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The currently highlighted row, if any.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Highlight `index` from the keyboard (or clear the highlight with
    /// `None`); an out-of-range index clears it (fail closed).
    pub fn set_current(&mut self, index: Option<usize>) {
        self.current = index.filter(|&i| i < self.items.len());
        self.keyboard_focus = self.current.is_some();
    }

    /// The scaled height of one menu row.
    fn row_height(scale: Scale, theme: &Theme) -> u32 {
        text_plate_height(theme, scale, TextRole::Body)
    }

    /// The scaled height of the band a group divider occupies: the rule with
    /// a gap either side, so groups read as separated bands rather than a
    /// hairline crowded between two labels.
    fn divider_band(scale: Scale, theme: &Theme) -> u32 {
        Self::divider_rule(scale, theme)
            .saturating_add(Self::divider_gap(scale, theme).saturating_mul(2))
    }

    /// The scaled thickness of the divider rule itself.
    fn divider_rule(scale: Scale, theme: &Theme) -> u32 {
        scale
            .scale_length(theme.metrics().border_thickness)
            .max(1)
            .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
    }

    /// The scaled clearance between the divider rule and the rows either side.
    fn divider_gap(scale: Scale, theme: &Theme) -> u32 {
        scale.scale_length(theme.metrics().control_gap).max(1)
    }

    /// Walk the rows in order, yielding each one's vertical geometry relative
    /// to the inner content top.
    ///
    /// The single definition of where a row sits: sizing, hit-testing, and
    /// painting all read this walk, so a divider band can never shift the
    /// rows one of them draws out of step with the rows another clicks.
    fn layout(&self, scale: Scale, theme: &Theme) -> impl Iterator<Item = RowBand> + '_ {
        let height = Self::row_height(scale, theme);
        let band = Self::divider_band(scale, theme);
        let mut cursor = 0u32;
        self.items.iter().enumerate().map(move |(index, item)| {
            // The first row opens no group: a rule flush against the plate
            // rim would read as a second rim, not as a division.
            let divider = if index > 0 && item.group_break {
                band
            } else {
                0
            };
            let top = cursor.saturating_add(divider);
            cursor = top.saturating_add(height);
            RowBand {
                index,
                divider,
                top,
                height,
            }
        })
    }

    /// The total height every row and divider band occupies, inside the rims.
    fn content_height(&self, scale: Scale, theme: &Theme) -> u32 {
        self.layout(scale, theme)
            .last()
            .map_or(0, |band| band.top.saturating_add(band.height))
    }

    /// The menu's preferred height for the active theme (plate rims plus every
    /// row and every group divider), so the owner can size the popup surface
    /// exactly.
    #[must_use]
    pub fn preferred_height(&self, scale: Scale, theme: &Theme) -> u32 {
        plate_border(theme, scale)
            .saturating_mul(2)
            .saturating_add(self.content_height(scale, theme))
    }

    /// The menu's preferred width for the active theme: wide enough for the
    /// widest row's icon column, label, caption, and submenu chevron.
    #[must_use]
    pub fn preferred_width(&self, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let row_h = Self::row_height(scale, theme);
        let icon_slot = font.glyph_height().min(row_h);
        let mut widest = 0;
        for item in &self.items {
            let label_w = font.text_width(&item.label);
            let caption = item
                .shortcut
                .as_deref()
                .or(item.reason.as_deref())
                .map_or(0, |c| font.text_width(c));
            let chevron = if item.submenu { font.glyph_height() } else { 0 };
            let w = border
                .saturating_add(pad)
                .saturating_add(icon_slot)
                .saturating_add(pad)
                .saturating_add(label_w)
                .saturating_add(pad)
                .saturating_add(caption)
                .saturating_add(pad)
                .saturating_add(chevron)
                .saturating_add(pad)
                .saturating_add(border);
            widest = widest.max(w);
        }
        widest
    }

    /// The bounds this menu occupies when opened at `anchor` (e.g. a
    /// right-click point), clamped so the whole menu stays inside `viewport`.
    ///
    /// A popup menu is opened by more than one owner — the directory browser's
    /// right-click menu and a terminal's are both plain [`Menu`]s — and every
    /// one of them needs the same answer to "where does the menu go", so the
    /// rule lives once, here, rather than being re-derived by each caller.
    /// The size comes from [`preferred_width`](Self::preferred_width) and
    /// [`preferred_height`](Self::preferred_height) clamped to the viewport;
    /// the top-left starts at `anchor` and shifts left/up only as far as
    /// needed to fit, never past the viewport's own origin. A degenerate
    /// (zero-sized) viewport still yields a drawable, if clipped, rectangle
    /// rather than panicking.
    #[must_use]
    pub fn anchored_rect(
        &self,
        anchor: Point,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Rect {
        let width = self
            .preferred_width(scale, theme)
            .clamp(1, viewport.width.max(1));
        let height = self
            .preferred_height(scale, theme)
            .clamp(1, viewport.height.max(1));
        let origin_x = viewport.origin.x;
        let origin_y = viewport.origin.y;
        let max_x = origin_x.saturating_add(to_i32(viewport.width.saturating_sub(width)));
        let max_y = origin_y.saturating_add(to_i32(viewport.height.saturating_sub(height)));
        let x = anchor.x.clamp(origin_x, max_x.max(origin_x));
        let y = anchor.y.clamp(origin_y, max_y.max(origin_y));
        Rect::new(x, y, width, height)
    }

    /// The inner content rectangle (inside the plate rim) as surface pixels.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        let border = plate_border(theme, scale);
        inset(x, y, w, h, border)
    }

    /// The row index under `point`, if any, for the given bounds.
    ///
    /// A point inside a group divider's band belongs to no row and answers
    /// [`None`], so the gap between two groups is inert rather than
    /// activating whichever neighbour happens to be nearer.
    #[must_use]
    pub fn row_at(&self, bounds: Rect, scale: Scale, theme: &Theme, point: Point) -> Option<usize> {
        let (ix, iy, iw, ih) = Self::inner(bounds, scale, theme)?;
        let px = point.x;
        let py = point.y;
        if px < to_i32(ix) || px >= to_i32(ix + iw) || py < to_i32(iy) || py >= to_i32(iy + ih) {
            return None;
        }
        let rel = u32::try_from(py - to_i32(iy)).ok()?;
        self.layout(scale, theme)
            .find(|band| rel >= band.top && rel < band.top.saturating_add(band.height))
            .map(|band| band.index)
    }

    /// The surface rectangle row `index` occupies for the given bounds, or
    /// `None` when the index is out of range or the plate has no room for rows.
    ///
    /// The forward mirror of [`row_at`](Self::row_at) over the same inner
    /// geometry, so a caller that must aim *at* a row — a test harness clicking
    /// a specific command — reads the exact rectangle [`render`](Self::render)
    /// paints and [`row_at`](Self::row_at) hit-tests, never a hand-copied
    /// position. Fails closed on an out-of-range index.
    #[must_use]
    pub fn row_rect(
        &self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        let (ix, iy, iw, _ih) = Self::inner(bounds, scale, theme)?;
        let band = self.layout(scale, theme).nth(index)?;
        Some(Rect::new(
            to_i32(ix),
            to_i32(iy).saturating_add(to_i32(band.top)),
            iw,
            band.height,
        ))
    }

    /// Paint the menu into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().popup_corner_radius)
            .min(w / 2)
            .min(h / 2);

        // The elevated command plate: Signal Rim then the raised surface.
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

        let rule = Self::divider_rule(scale, theme);
        let gap = Self::divider_gap(scale, theme);
        for band in self.layout(scale, theme) {
            let row_top = iy.saturating_add(band.top);
            if row_top.saturating_add(band.height) > iy + ih {
                break;
            }
            if band.divider > 0 {
                // The rule sits centred in its band, inset from both plate
                // rims so it reads as a division between groups rather than
                // as a full-width edge of the plate itself.
                let inset_x = gap.min(iw / 2);
                surface.fill_rect(
                    ix.saturating_add(inset_x),
                    row_top.saturating_sub(band.divider).saturating_add(gap),
                    iw.saturating_sub(inset_x.saturating_mul(2)),
                    rule,
                    Color::from(palette.border),
                );
            }
            let Some(item) = self.items.get(band.index) else {
                break;
            };
            let current = self.current == Some(band.index);
            let focused = current && self.keyboard_focus;
            item.paint(
                surface,
                (ix, row_top, iw, band.height),
                scale,
                theme,
                font,
                current,
                focused,
            );
        }
    }

    /// The typed action for activating (or opening) the current row, if it is
    /// actionable; `None` for a non-actionable row (fail closed).
    fn activate(&self, index: usize) -> Option<MenuAction> {
        let item = self.items.get(index)?;
        if !item.is_actionable() {
            return None;
        }
        if item.submenu {
            Some(MenuAction::OpenSubmenu { index })
        } else {
            Some(MenuAction::Activated { index })
        }
    }

    /// Move the highlight to `next`, `keyboard` when the keyboard put it there,
    /// reporting the row it left, the row it arrives on, and — when only the
    /// ring changed — that one row.
    fn highlight(
        &mut self,
        next: Option<usize>,
        keyboard: bool,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        if damage::move_mark(
            self.current,
            next,
            |index| self.row_rect(index, bounds, scale, theme),
            damage,
        ) {
            self.current = next;
        }
        let ring = self
            .current
            .and_then(|index| self.row_rect(index, bounds, scale, theme))
            .unwrap_or(Rect::EMPTY);
        damage::set(&mut self.keyboard_focus, keyboard, ring, damage);
    }

    /// Feed a pointer event; hover sets the current row and a completed primary
    /// click over an actionable row activates it (opening a submenu parent).
    ///
    /// A moved highlight reports the two rows it moved between, never the whole
    /// popup, and a sample that stays on one row reports nothing.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<MenuAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let over = self.row_at(bounds, scale, theme, *self.pointer);
        match event {
            InputEvent::PointerMoved { .. } => {
                if self.armed.is_none() {
                    self.highlight(over, false, bounds, scale, theme, damage);
                }
                None
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                *self.armed = over;
                if over.is_some() {
                    self.highlight(over, false, bounds, scale, theme, damage);
                }
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let armed = self.armed.take();
                match (armed, over) {
                    (Some(a), Some(o)) if a == o => self.activate(o),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Feed a key event: Up/Down move the current row (wrapping), Home/End jump
    /// to the ends, Right/Enter/Space activate (opening a submenu parent), and
    /// Escape dismisses.
    ///
    /// A moved highlight reports the two rows it moved between; the keys that
    /// only activate or dismiss report nothing, because the menu itself draws
    /// nothing differently for them.
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<MenuAction> {
        if self.items.is_empty() {
            return match key {
                Key::Named(NamedKey::Escape) => Some(MenuAction::Dismissed),
                _ => None,
            };
        }
        let last = self.items.len() - 1;
        match key {
            Key::Named(NamedKey::Down) => {
                let next = match self.current {
                    Some(i) if i < last => i + 1,
                    _ => 0,
                };
                self.highlight(Some(next), true, bounds, scale, theme, damage);
                None
            }
            Key::Named(NamedKey::Up) => {
                let prev = match self.current {
                    Some(0) | None => last,
                    Some(i) => i - 1,
                };
                self.highlight(Some(prev), true, bounds, scale, theme, damage);
                None
            }
            Key::Named(NamedKey::Home) => {
                self.highlight(Some(0), true, bounds, scale, theme, damage);
                None
            }
            Key::Named(NamedKey::End) => {
                self.highlight(Some(last), true, bounds, scale, theme, damage);
                None
            }
            Key::Named(NamedKey::Right) => {
                let i = self.current?;
                self.items
                    .get(i)
                    .filter(|it| it.submenu && it.is_actionable())?;
                Some(MenuAction::OpenSubmenu { index: i })
            }
            Key::Named(NamedKey::Enter) | Key::Char(' ') => self.activate(self.current?),
            Key::Named(NamedKey::Escape) => Some(MenuAction::Dismissed),
            _ => None,
        }
    }
}
