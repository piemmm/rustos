//! The breadcrumb trail: [`Crumb`] and [`Breadcrumb`] (spec §11.12 sibling —
//! the navigation-control family).
//!
//! A breadcrumb names the reader's location as a path of crumbs, so a deep
//! surface can say where it is and let the reader step back up to any
//! ancestor. The trailing crumb is always the current location: it reads in
//! the emphasised foreground and never activates, no matter how it is
//! addressed, so a reader can never "navigate" to where they already are.
//! Every earlier crumb is an activatable ancestor — muted at rest, emphasised
//! on hover, carrying the same focus ring [`crate::tabs::Tabs`] and
//! [`crate::menu::Menu`] draw around their own rows — and a disabled or
//! denied crumb keeps its slot in the trail exactly as a [`crate::menu::Menu`]
//! row does, rather than vanishing or collapsing the path.
//!
//! When the trail is wider than the space it is given, whole crumbs are
//! elided from the front — the oldest ancestors — behind a single leading
//! ellipsis crumb that itself activates the newest ancestor it stands in for.
//! The current location is never elided away: only once even the ellipsis
//! plus the current crumb cannot fit is the ellipsis dropped too, leaving the
//! current crumb alone with its own label shrunk to fit. The one layout walk
//! that decides this is shared by [`Breadcrumb::render`] and
//! [`Breadcrumb::crumb_at`], so a hit test can never disagree with what was
//! painted. Every colour, gap, and font metric
//! resolves from the active [`Theme`] and [`Scale`]; the separator chevron
//! and the focus ring reuse the shared `crate::paint` core rather than
//! inventing a second copy of either.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    draw_outline, heavy_contrast, key_activation, paint_bead, paint_chevron, plate_border,
    resolve_bead, surface_rect, to_i32, ChevronDir,
};
use crate::state::{ControlState, FocusState, RenderInvariant};

/// The glyph a leading elision crumb draws in place of the ancestors it
/// hides. Three plain periods, not the single Unicode ellipsis character, so
/// the mark renders correctly under any font coverage the console atlas or a
/// proportional family happens to ship.
const ELLIPSIS: &str = "...";

/// The outcome of feeding input to a [`Breadcrumb`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BreadcrumbAction {
    /// The crumb at `index` was activated and the trail should navigate to
    /// it. For the leading ellipsis this is the newest ancestor it hides.
    Activate {
        /// The zero-based index into [`Breadcrumb::crumbs`].
        index: usize,
    },
}

/// One crumb in a [`Breadcrumb`] trail: a location name and its composed
/// state (spec §13).
///
/// A crumb renders state and never dispatches; whether it is the trailing
/// (current, non-activatable) crumb or an activatable ancestor is a property
/// of its *position* in the trail, not of the crumb itself, so [`Crumb`]
/// carries no flag of its own for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Crumb {
    label: String,
    state: ControlState,
}

impl Crumb {
    /// A neutral, enabled crumb with the given label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: ControlState::idle(),
        }
    }

    /// This crumb with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// The crumb's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The crumb's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the crumb's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }
}

/// One cell the trail lays out: either an ordinary crumb at its original
/// index, or the leading ellipsis standing in for the elided ancestors before
/// it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Cell {
    /// The crumb at this index into [`Breadcrumb::crumbs`].
    Crumb(usize),
    /// The elision mark; activating it navigates to the newest (highest
    /// index) ancestor it hides.
    Ellipsis {
        /// The newest hidden ancestor's index.
        newest_hidden: usize,
    },
}

/// One laid-out cell: its content and the surface rectangle it occupies.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Placement {
    cell: Cell,
    rect: (u32, u32, u32, u32),
}

/// A path of crumbs naming the reader's location (spec §11.12 family).
///
/// The trailing crumb is the current location and never activates; every
/// earlier crumb is an activatable ancestor. The trail owns keyboard
/// navigation (Left/Right move focus among the activatable crumbs, Home/End
/// jump to the first/last one, Enter/Space activate the focused crumb) and
/// pointer hover/click, emitting a typed [`BreadcrumbAction`]; it performs no
/// privileged work and enforces no authority of its own.
///
/// Equal trails draw the same pixels, so a host may use `==` as its repaint
/// gate: the crumbs, the focused index, and the hovered index all compare.
/// The pointer coordinate and the pressed-crumb latch do not — no render path
/// reads either, and the *visible* consequence of a press is the crumb state
/// the same event sets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Breadcrumb {
    crumbs: Vec<Crumb>,
    focus: Option<usize>,
    /// The activatable ancestor the pointer currently sits over, if any —
    /// drawn (an ancestor emphasises on hover), unlike the raw coordinate
    /// below.
    hover: Option<usize>,
    /// The last pointer position, mapped to a crumb on the next press or
    /// release — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The crumb a primary press landed on, held until release so a click
    /// that slides onto another crumb does not activate it.
    armed: RenderInvariant<Option<usize>>,
}

impl Breadcrumb {
    /// A trail over the given crumbs, with no crumb focused or hovered.
    #[must_use]
    pub fn new(crumbs: Vec<Crumb>) -> Self {
        Self {
            crumbs,
            focus: None,
            hover: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(None),
        }
    }

    /// The trail's crumbs.
    #[must_use]
    pub fn crumbs(&self) -> &[Crumb] {
        &self.crumbs
    }

    /// Mutable access to the trail's crumbs (e.g. to update a crumb's state).
    pub fn crumbs_mut(&mut self) -> &mut [Crumb] {
        &mut self.crumbs
    }

    /// The number of crumbs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.crumbs.len()
    }

    /// Whether the trail has no crumbs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.crumbs.is_empty()
    }

    /// The index of the trailing crumb — the current location — or `None`
    /// when the trail is empty.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.crumbs.len().checked_sub(1)
    }

    /// The index of the last activatable ancestor: every crumb but the
    /// trailing (current) one. `None` when the trail has one crumb or fewer.
    fn last_activatable(&self) -> Option<usize> {
        self.crumbs.len().checked_sub(2)
    }

    /// The currently keyboard-focused crumb, if any.
    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// Focus `index` from the keyboard (or clear focus with `None`). An
    /// out-of-range index, or the trailing (current, non-activatable) crumb,
    /// clears focus instead (fail closed — focus never lands on the current
    /// location).
    pub fn set_focus(&mut self, index: Option<usize>) {
        self.focus = index.filter(|&i| Some(i) <= self.last_activatable());
    }

    /// Whether the crumb at `index` is the trailing (current, non-activatable)
    /// crumb.
    fn is_current(&self, index: usize) -> bool {
        self.current() == Some(index)
    }

    // --- Shared measurement/layout primitives ---------------------------

    /// The scaled horizontal padding inside a crumb's own cell, framing its
    /// label for a comfortable hit target and focus ring.
    fn cell_pad(scale: Scale, theme: &Theme) -> u32 {
        scale.scale_length(theme.metrics().control_inset).max(1)
    }

    /// The scaled gap either side of a separator chevron.
    fn chevron_gap(scale: Scale, theme: &Theme) -> u32 {
        scale.scale_length(theme.metrics().control_gap).max(1)
    }

    /// The width of a chevron slot: a square glyph the height of a line,
    /// doubled under heavier contrast so the separator strengthens exactly
    /// as `plate_border`/`rail_thickness` already do for their own edges.
    fn chevron_width(theme: &Theme, font: BitmapFont) -> u32 {
        font.glyph_height()
            .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
    }

    /// The width one chevron occupies together with the gap either side.
    fn chevron_span(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        Self::chevron_gap(scale, theme)
            .saturating_mul(2)
            .saturating_add(Self::chevron_width(theme, font))
    }

    /// The unelided cell width `text` needs: its own padding on both sides
    /// plus its full text width.
    fn cell_width(text: &str, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        Self::cell_pad(scale, theme)
            .saturating_mul(2)
            .saturating_add(font.text_width(text))
    }

    /// The label text a laid-out `cell` shows.
    fn cell_label(&self, cell: Cell) -> &str {
        match cell {
            Cell::Ellipsis { .. } => ELLIPSIS,
            Cell::Crumb(i) => self.crumbs.get(i).map_or("", Crumb::label),
        }
    }

    /// The composed state a cell paints from: the ellipsis always reads as
    /// an idle, enabled crumb (it is never disabled or denied on its own
    /// account); an ordinary crumb shows its own state.
    fn cell_state(&self, cell: Cell) -> ControlState {
        match cell {
            Cell::Ellipsis { .. } => ControlState::idle(),
            Cell::Crumb(i) => self
                .crumbs
                .get(i)
                .map_or(ControlState::idle(), Crumb::state),
        }
    }

    /// Whether the pointer is currently hovering `cell` — an exact match,
    /// since [`Breadcrumb::hover`] is always resolved from an actual pointer
    /// position over a rendered rectangle (the ellipsis's own rect included).
    fn cell_is_hovered(&self, cell: Cell) -> bool {
        self.hover == Some(Self::cell_index(cell))
    }

    /// Whether `cell` should show the keyboard focus ring: an ordinary crumb
    /// matches focus exactly, and the ellipsis also lights up when the
    /// keyboard focus sits on one of the ancestors it currently hides, so a
    /// focused crumb elided out of individual view never loses its visible
    /// indicator.
    fn cell_shows_focus(&self, cell: Cell) -> bool {
        match cell {
            Cell::Crumb(i) => self.focus == Some(i),
            Cell::Ellipsis { newest_hidden } => {
                matches!(self.focus, Some(f) if f <= newest_hidden)
            }
        }
    }

    /// The candidate cell list for showing crumbs `start..=last` plainly,
    /// with a leading ellipsis standing for `0..start` when `start > 0`.
    fn cells_from(start: usize, last: usize) -> Vec<Cell> {
        let mut cells = Vec::new();
        if start > 0 {
            cells.push(Cell::Ellipsis {
                newest_hidden: start - 1,
            });
        }
        cells.extend((start..=last).map(Cell::Crumb));
        cells
    }

    /// The unelided width a cell list needs: every cell's own width plus a
    /// chevron between each pair.
    fn cells_width(&self, cells: &[Cell], scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        let chevron = Self::chevron_span(scale, theme, font);
        let mut width = 0u32;
        for (i, &cell) in cells.iter().enumerate() {
            if i > 0 {
                width = width.saturating_add(chevron);
            }
            width =
                width.saturating_add(Self::cell_width(self.cell_label(cell), scale, theme, font));
        }
        width
    }

    /// The minimum height, in physical pixels, the trail needs at `scale` to
    /// draw one line of crumb text without clipping.
    #[must_use]
    pub fn measured_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        Self::cell_pad(scale, theme)
            .saturating_mul(2)
            .saturating_add(font.line_height())
    }

    /// The width the whole trail wants before any elision: every crumb's own
    /// cell plus a chevron (with its gaps) between each pair.
    #[must_use]
    pub fn measured_width(&self, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        let Some(last) = self.crumbs.len().checked_sub(1) else {
            return 0;
        };
        self.cells_width(&Self::cells_from(0, last), scale, theme, font)
    }

    /// Lay out the trail within `avail_w` physical pixels: the deterministic
    /// elision every hit test and render consults.
    ///
    /// Crumbs are dropped from the front (oldest first) behind a leading
    /// ellipsis until the remainder fits. If even the ellipsis together with
    /// the current crumb alone still overflows, the ellipsis is dropped too,
    /// leaving only the current crumb — its label is shrunk to fit by
    /// [`place`](Self::place), so it is never elided, only shrunk.
    fn layout(&self, avail_w: u32, scale: Scale, theme: &Theme, font: BitmapFont) -> Vec<Cell> {
        let Some(last) = self.crumbs.len().checked_sub(1) else {
            return Vec::new();
        };
        for start in 0..=last {
            let cells = Self::cells_from(start, last);
            if self.cells_width(&cells, scale, theme, font) <= avail_w {
                return cells;
            }
        }
        vec![Cell::Crumb(last)]
    }

    /// Place the laid-out `cells` left to right within `(x, y, w, h)`. A
    /// cell whose natural width would overflow the remaining space is
    /// clamped to what remains — the sole fallback cell (the lone current
    /// crumb) is the only one this can happen to, since the elision step
    /// already guarantees every other combination it returns fits in full.
    fn place(
        &self,
        cells: &[Cell],
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Vec<Placement> {
        let (x, y, w, h) = rect;
        let gap = Self::chevron_gap(scale, theme);
        let chevron_w = Self::chevron_width(theme, font);
        let mut cursor = x;
        let mut placements = Vec::new();
        for (position, &cell) in cells.iter().enumerate() {
            if position > 0 {
                cursor = cursor
                    .saturating_add(gap)
                    .saturating_add(chevron_w)
                    .saturating_add(gap);
            }
            let natural = Self::cell_width(self.cell_label(cell), scale, theme, font);
            let remaining = x.saturating_add(w).saturating_sub(cursor);
            let cell_w = natural.min(remaining);
            if cell_w == 0 {
                break;
            }
            placements.push(Placement {
                cell,
                rect: (cursor, y, cell_w, h),
            });
            cursor = cursor.saturating_add(cell_w);
        }
        placements
    }

    /// The full plan for painting or hit-testing the trail at `bounds`: lay
    /// out the elided cells, then place them, so [`render`](Self::render) and
    /// [`crumb_at`](Self::crumb_at) always agree.
    fn plan(&self, bounds: Rect, scale: Scale, theme: &Theme, font: BitmapFont) -> Vec<Placement> {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return Vec::new();
        };
        if w == 0 || h == 0 || self.crumbs.is_empty() {
            return Vec::new();
        }
        let cells = self.layout(w, scale, theme, font);
        self.place(&cells, (x, y, w, h), scale, theme, font)
    }

    /// The `crumbs` index a cell represents, resolving the ellipsis to the
    /// newest ancestor it hides.
    fn cell_index(cell: Cell) -> usize {
        match cell {
            Cell::Crumb(i) => i,
            Cell::Ellipsis { newest_hidden } => newest_hidden,
        }
    }

    /// The crumb index at `point` for the given bounds, resolved through the
    /// same elided layout [`render`](Self::render) paints. The ellipsis
    /// answers the newest ancestor it hides.
    #[must_use]
    pub fn crumb_at(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        point: Point,
    ) -> Option<usize> {
        self.plan(bounds, scale, theme, font)
            .into_iter()
            .find(|p| {
                let (x, y, w, h) = p.rect;
                point.x >= to_i32(x)
                    && point.x < to_i32(x + w)
                    && point.y >= to_i32(y)
                    && point.y < to_i32(y + h)
            })
            .map(|p| Self::cell_index(p.cell))
    }

    // --- Rendering --------------------------------------------------------

    /// Paint the trail into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let placements = self.plan(bounds, scale, theme, font);
        let last = placements.len().saturating_sub(1);
        for (i, placement) in placements.iter().enumerate() {
            self.paint_cell(surface, placement, scale, theme, font);
            if i < last {
                Self::paint_separator(surface, placement.rect, scale, theme, font);
            }
        }
    }

    /// Paint the chevron separator immediately after `crumb_rect`, in the
    /// muted foreground.
    fn paint_separator(
        surface: &mut Surface,
        crumb_rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let (x, y, w, h) = crumb_rect;
        let gap = Self::chevron_gap(scale, theme);
        let chevron_w = Self::chevron_width(theme, font);
        let cx = x.saturating_add(w).saturating_add(gap);
        let color = Color::from(theme.palette().on_surface_muted);
        paint_chevron(
            surface,
            Rect::new(to_i32(cx), to_i32(y), chevron_w, h),
            ChevronDir::Right,
            color,
        );
    }

    /// Paint one laid-out cell: its focus ring (for a focused, activatable
    /// ancestor), its label in the resolved foreground, and its Authority
    /// Mark or Signal Bead when its state carries one.
    fn paint_cell(
        &self,
        surface: &mut Surface,
        placement: &Placement,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let (x, y, w, h) = placement.rect;
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let current = matches!(placement.cell, Cell::Crumb(index) if self.is_current(index));
        let state = self.cell_state(placement.cell);
        let hovered = !current && self.cell_is_hovered(placement.cell);
        let focused = !current && self.cell_shows_focus(placement.cell);

        // A disabled crumb always mutes, regardless of position; the current
        // location reads at full strength; an ancestor lifts on hover or
        // focus and otherwise stays quiet — the same three-way emphasis the
        // tab strip and the menu row already draw.
        let label_color = if !state.enabled {
            palette.on_surface_muted
        } else if current || hovered || focused {
            palette.on_surface
        } else {
            palette.on_surface_muted
        };

        if focused {
            let border = plate_border(theme, scale);
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

        let text = self.cell_label(placement.cell);
        let pad = Self::cell_pad(scale, theme);
        let avail = w.saturating_sub(pad.saturating_mul(2));
        let fitted = font.truncate_to_width(text, avail);
        let glyph_h = font.glyph_height();
        let text_x = to_i32(x.saturating_add(pad));
        let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
        font.draw_text(surface, text_x, text_y, fitted, Color::from(label_color));

        if let Some((color, shape)) = resolve_bead(theme, state) {
            let border = plate_border(theme, scale);
            let size = scale
                .scale_length(theme.metrics().bead_size)
                .max(3)
                .min(w.saturating_sub(border))
                .min(h.saturating_sub(border));
            if size > 0 {
                let bx = x
                    .saturating_add(w)
                    .saturating_sub(border)
                    .saturating_sub(size);
                let by = y.saturating_add(border);
                paint_bead(surface, bx, by, size, color, shape);
            }
        }

        // A heavier-contrast theme strengthens the current crumb with an
        // underline seam, so the location reads without relying on the hue
        // difference between the muted and emphasised foregrounds alone.
        if current && heavy_contrast(theme) {
            let seam = scale.scale_length(theme.metrics().seam_thickness).max(1);
            surface.fill_rect(
                x,
                y.saturating_add(h).saturating_sub(seam),
                w,
                seam,
                Color::from(palette.accent),
            );
        }
    }

    // --- Input --------------------------------------------------------

    /// Feed a pointer event, given the trail's current `bounds`; hover
    /// emphasises an ancestor crumb and a completed primary click over an
    /// actionable, non-current one activates it.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<BreadcrumbAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let over = self
            .crumb_at(bounds, scale, theme, font, *self.pointer)
            .filter(|&i| !self.is_current(i));
        match event {
            InputEvent::PointerMoved { .. } => {
                if self.armed.is_none() {
                    self.hover = over;
                }
                None
            }
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
                    (Some(a), Some(o)) if a == o => self.activate(o),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Activate the crumb at `index`, reporting the choice if it is
    /// actionable and not the current location (fail closed).
    fn activate(&self, index: usize) -> Option<BreadcrumbAction> {
        if self.is_current(index) {
            return None;
        }
        let crumb = self.crumbs.get(index)?;
        crumb
            .state
            .is_actionable()
            .then_some(BreadcrumbAction::Activate { index })
    }

    /// Feed a key event: Left/Right move focus among the activatable
    /// ancestors (wrapping), Home/End jump to the first/last one, and
    /// Enter/Space activate the focused crumb. Focus never lands on the
    /// current (trailing) crumb.
    pub fn on_key(&mut self, key: Key) -> Option<BreadcrumbAction> {
        let last = self.last_activatable()?;
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
                let crumb = self.crumbs.get(index)?;
                let focused = crumb.state.with_focus(FocusState::FOCUSED);
                if key_activation(focused, key) {
                    self.activate(index)
                } else {
                    None
                }
            }
        }
    }
}
