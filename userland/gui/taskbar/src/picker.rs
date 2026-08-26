//! The window picker an icon-bar slot opens on hover.
//!
//! An icon-bar slot is one *application*, so a slot whose application owns
//! more than one window needs somewhere to choose between them. Resting the
//! pointer on the slot for [`PICKER_OPEN_DELAY_NS`] opens the picker: one
//! captioned thumbnail per window, and a primary press on a cell raises and
//! focuses that window.
//!
//! **It opens only above one window, and only on a deliberate rest.** A slot
//! with a single window has nothing to pick — a click on the slot already
//! reaches that window — and a pointer merely sweeping across a slot on its
//! way somewhere else has asked for nothing, so the dwell is what separates a
//! hover from a passing pointer. Leaving the slot does not close the picker
//! at once either: it survives [`PICKER_CLOSE_GRACE_NS`], which is what lets
//! the pointer cross the gap between the bar and the panel to make a choice.
//!
//! **Every window is reachable, however many there are.** The cells wrap into
//! a grid sized to the space beside the bar, and a grid that still does not
//! fit scrolls, so a cell is never laid out where it cannot be clicked.
//!
//! The thumbnails are the session's: it maps every application's frame region
//! already, so it scales the last presented frame down to the cell through the
//! shared rasteriser and hands the result over — one window per turn of its
//! serve loop while the dwell runs, so populating a picker never blocks the
//! desktop. The bar never touches an application's live pixels, and a window
//! whose thumbnail has not landed yet draws its application's glyph instead of
//! a hole.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{
    plate_border, ControlState, PointerState, ScrollAction, ScrollBar, ScrollModel,
    ScrollOrientation, ScrollRange, WindowPreview,
};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::IconKind;
use tairix_input::InputEvent;
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::edge::Edge;
use crate::tasks::TaskId;

/// Logical width of one picker cell at the reference density.
///
/// A landscape cell, so a window's thumbnail keeps a recognisable shape
/// rather than being squeezed into an icon.
const CELL_WIDTH: u32 = 168;

/// Logical height of one picker cell at the reference density.
const CELL_HEIGHT: u32 = 118;

/// How long the pointer must rest on an application's slot before its window
/// picker opens, in monotonic nanoseconds.
///
/// A second: long enough that sweeping the pointer along the bar on the way
/// to something else pops nothing up, short enough that a deliberate hover
/// does not feel stuck. It is also the budget the embedder scales the
/// windows' thumbnails in, so an ordinary application's picker is fully
/// drawn by the time it appears.
pub const PICKER_OPEN_DELAY_NS: u64 = 1_000_000_000;

/// How long an open picker survives the pointer resting on neither its
/// application's slot nor the panel itself, in monotonic nanoseconds.
///
/// The panel opens a gap away from the bar, so the pointer *must* cross
/// surface that belongs to neither on its way to a cell; closing on the first
/// sample outside would make choosing a window impossible. A fifth of a
/// second covers that crossing without leaving a panel hanging over the
/// desktop after the pointer has genuinely left.
pub const PICKER_CLOSE_GRACE_NS: u64 = 200_000_000;

/// One window a picker offers.
#[derive(Clone, Debug)]
pub struct PickerEntry {
    window: TaskId,
    title: String,
    thumbnail: Option<Surface>,
}

impl PickerEntry {
    /// An entry for `window`, captioned with its title, with no thumbnail.
    #[must_use]
    pub fn new(window: TaskId, title: impl Into<String>) -> Self {
        Self {
            window,
            title: title.into(),
            thumbnail: None,
        }
    }

    /// This entry showing `thumbnail` — the window's last presented frame,
    /// scaled by the session to the cell's own thumbnail rectangle.
    #[must_use]
    pub fn with_thumbnail(mut self, thumbnail: Surface) -> Self {
        self.thumbnail = Some(thumbnail);
        self
    }

    /// The window this entry stands for.
    #[must_use]
    pub fn window(&self) -> TaskId {
        self.window
    }

    /// The window's title, as the cell's caption states it.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The window's scaled frame, if the session supplied one.
    #[must_use]
    pub fn thumbnail(&self) -> Option<&Surface> {
        self.thumbnail.as_ref()
    }
}

/// The computed geometry of an open picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerLayout {
    /// The picker's plate, in screen coordinates.
    pub panel: Rect,
    /// One cell per entry, in order. A cell outside the visible rows is
    /// [`Rect::EMPTY`] and can never be hit; scrolling brings it into view.
    pub cells: Vec<Rect>,
    /// The vertical scrollbar, when the grid holds more rows than the panel
    /// shows.
    pub scrollbar: Option<Rect>,
    /// Cell columns the grid is laid out in; never zero.
    pub columns: u32,
    /// Cell rows the panel shows at once; never zero.
    pub visible_rows: u32,
    /// The corner radius the window manager applies to the picker window —
    /// the popup radius, so the window rounding and the painted plate agree.
    pub corner_radius: u32,
}

/// The window picker: closed, or open over one application's windows.
#[derive(Clone, Debug)]
pub struct WindowPicker {
    app: Option<usize>,
    anchor: Rect,
    /// The owning application's class glyph, drawn in a cell whose window
    /// has no thumbnail — so the fallback is the *application's* picture,
    /// never a generic one.
    icon: IconKind,
    entries: Vec<PickerEntry>,
    hover: Option<usize>,
    /// The grid's vertical scroll, in cell rows.
    scroll: ScrollBar,
}

/// The pixel size a cell's thumbnail is drawn at, at the desktop `scale`.
///
/// Asked of the control that will paint it, so the size the session
/// rasterises a window's frame to and the rectangle the cell blits it into
/// can never drift apart — the same rule the application slot's
/// [`app_icon_side`](crate::Taskbar::app_icon_side) follows.
#[must_use]
pub fn thumbnail_size(scale: Scale, theme: &Theme) -> (u32, u32) {
    let bounds = Rect::new(
        0,
        0,
        scale.scale_length(CELL_WIDTH).max(1),
        scale.scale_length(CELL_HEIGHT).max(1),
    );
    let thumb = WindowPreview::new("", IconKind::AppBundle).thumbnail_bounds(bounds, scale, theme);
    (thumb.width, thumb.height)
}

/// The smallest number of windows an application must own for its slot to
/// open a picker on hover.
///
/// Two: with one window there is nothing to choose, and the slot's own click
/// already reaches it.
pub const PICKER_MIN_WINDOWS: usize = 2;

impl Default for WindowPicker {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowPicker {
    /// A closed picker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            app: None,
            anchor: Rect::EMPTY,
            icon: IconKind::AppBundle,
            entries: Vec::new(),
            hover: None,
            scroll: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ),
        }
    }

    /// Whether the picker is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.app.is_some()
    }

    /// The strip index of the application whose windows are shown, if open.
    #[must_use]
    pub fn app(&self) -> Option<usize> {
        self.app
    }

    /// The offered windows, in order.
    #[must_use]
    pub fn entries(&self) -> &[PickerEntry] {
        &self.entries
    }

    /// The hovered cell index, if the pointer rests on one.
    #[must_use]
    pub fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// The grid's scrollbar, for painting.
    #[must_use]
    pub const fn scrollbar(&self) -> &ScrollBar {
        &self.scroll
    }

    /// Open the picker over the application at strip index `app`, anchored
    /// at its slot, offering `entries`.
    ///
    /// Refused — and the picker left exactly as it was — for fewer than
    /// [`PICKER_MIN_WINDOWS`] entries: a picker with nothing to choose is
    /// not a picker. Returns whether it is now open over `app`.
    pub(crate) fn open(
        &mut self,
        app: usize,
        anchor: Rect,
        icon: IconKind,
        entries: Vec<PickerEntry>,
    ) -> bool {
        if entries.len() < PICKER_MIN_WINDOWS {
            return false;
        }
        let same = self.app == Some(app) && self.entries.len() == entries.len();
        self.app = Some(app);
        self.anchor = anchor;
        self.icon = icon;
        self.entries = entries;
        // Already showing this application: keep the highlight and the
        // scroll position rather than dropping them on every pointer sample
        // over the slot.
        if !same {
            self.hover = None;
            self.scroll.set_model(self.scroll.model().to_start());
        }
        true
    }

    /// Close the picker without choosing. Returns whether it had been open.
    pub(crate) fn close(&mut self) -> bool {
        let was_open = self.app.is_some();
        self.app = None;
        self.anchor = Rect::EMPTY;
        self.icon = IconKind::AppBundle;
        self.entries.clear();
        self.hover = None;
        self.scroll.set_model(self.scroll.model().to_start());
        was_open
    }

    /// Track the hovered cell, reporting whether the visual state changed.
    pub(crate) fn set_hover(&mut self, hover: Option<usize>) -> bool {
        let hover = hover.filter(|&index| index < self.entries.len());
        if self.hover == hover {
            return false;
        }
        self.hover = hover;
        true
    }

    /// Show `thumbnail` in the cell at `index` — the window's frame, scaled
    /// by the embedder after the picker opened. Reports whether it landed.
    pub(crate) fn set_thumbnail(&mut self, index: usize, thumbnail: Surface) -> bool {
        let Some(entry) = self.entries.get_mut(index) else {
            return false;
        };
        entry.thumbnail = Some(thumbnail);
        true
    }

    /// Feed one pointer event to the grid's scrollbar, reporting whether the
    /// visuals changed.
    ///
    /// Only the scrollbar sees it: cell hover and cell activation are the
    /// caller's, and a picker whose grid fits lays out no bar and scrolls
    /// nothing.
    pub(crate) fn on_pointer(
        &mut self,
        event: &InputEvent,
        layout: &PickerLayout,
        pointer: Point,
        (scale, theme): (Scale, &Theme),
        damage: &mut Region,
    ) -> bool {
        self.sync_scroll(layout);
        let Some(bounds) = layout.scrollbar else {
            return false;
        };
        let acted = match event {
            InputEvent::PointerScrolled { dx, dy } => {
                if !layout.panel.contains(pointer) {
                    return false;
                }
                self.scroll.wheel(*dx, *dy, bounds, damage)
            }
            _ => self.scroll.on_pointer(event, bounds, scale, theme, damage),
        };
        let Some(ScrollAction::ScrollTo { offset }) = acted else {
            return false;
        };
        self.scroll.set_model(self.scroll.model().scroll_to(offset));
        true
    }

    /// Whether `point` rests on the grid's scrollbar, so a press there
    /// belongs to it rather than to a cell.
    #[must_use]
    pub(crate) fn over_scrollbar(layout: &PickerLayout, point: Point) -> bool {
        layout
            .scrollbar
            .is_some_and(|bounds| bounds.contains(point))
    }

    /// Bring the scroll range in step with the laid-out grid, so an offset
    /// left over from a larger grid cannot outrun the rows there are.
    fn sync_scroll(&mut self, layout: &PickerLayout) {
        let rows = grid_rows(self.entries.len(), layout.columns);
        let viewport = u64::from(layout.visible_rows);
        let offset = self.scroll.model().offset();
        self.scroll.set_model(ScrollModel::new(
            ScrollRange::new(rows, viewport, offset),
            1,
            viewport.max(1),
        ));
    }

    /// The picker's geometry: a grid of cells opening outward from the
    /// anchored slot on the bar's `edge`, clamped onto the screen. `None`
    /// while closed.
    #[must_use]
    pub fn layout(
        &self,
        edge: Edge,
        screen_width: u32,
        screen_height: u32,
        scale: Scale,
        theme: &Theme,
    ) -> Option<PickerLayout> {
        self.app?;
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);
        let gap = scale.scale_length(metrics.control_gap);
        let cell = (
            scale.scale_length(CELL_WIDTH).max(1),
            scale.scale_length(CELL_HEIGHT).max(1),
        );
        let inset = border.saturating_add(gap);
        let screen = (screen_width.max(1), screen_height.max(1));
        let available = self.available(edge, screen, gap);
        // The gutter takes width from the grid, so the columns are settled
        // once without it and again with it if the rows overflowed — a
        // narrower grid can only need *more* rows, so one pass settles it.
        let breadth = scale.scale_length(metrics.scrollbar_breadth);
        let plain = self.grid(available, cell, (inset, gap), 0);
        let scrollable = plain.rows > plain.visible_rows;
        let gutter = if scrollable {
            breadth.saturating_add(gap)
        } else {
            0
        };
        let grid = if scrollable {
            self.grid(available, cell, (inset, gap), gutter)
        } else {
            plain
        };

        let strip = |count: u32, extent: u32| {
            extent
                .saturating_mul(count)
                .saturating_add(gap.saturating_mul(count.saturating_sub(1)))
        };
        let width = strip(grid.columns, cell.0)
            .saturating_add(gutter)
            .saturating_add(inset.saturating_mul(2))
            .min(screen.0);
        let height = strip(grid.visible_rows, cell.1)
            .saturating_add(inset.saturating_mul(2))
            .min(screen.1);
        let (x, y) = match edge {
            Edge::Bottom => (
                self.anchor.left(),
                self.anchor.top() - to_i32(height) - to_i32(gap),
            ),
            Edge::Top => (self.anchor.left(), self.anchor.bottom() + to_i32(gap)),
            Edge::Left => (self.anchor.right() + to_i32(gap), self.anchor.top()),
            Edge::Right => (
                self.anchor.left() - to_i32(width) - to_i32(gap),
                self.anchor.top(),
            ),
        };
        let panel =
            Rect::new(x, y, width, height).clamped_onto(Rect::new(0, 0, screen.0, screen.1));

        // Clamped here, not merely on the next pointer event: a density change
        // re-columns the grid under a scrolled panel, and a first row past its
        // new last one would lay out no cell at all.
        let first_row = u32::try_from(self.scroll.model().offset())
            .unwrap_or(u32::MAX)
            .min(grid.rows.saturating_sub(grid.visible_rows));
        let mut cells = Vec::with_capacity(self.entries.len());
        for index in 0..self.entries.len() {
            let seat = u32::try_from(index).unwrap_or(u32::MAX);
            let (row, column) = (seat / grid.columns, seat % grid.columns);
            cells.push(
                if (first_row..first_row.saturating_add(grid.visible_rows)).contains(&row) {
                    let left =
                        inset.saturating_add(cell.0.saturating_add(gap).saturating_mul(column));
                    let top = inset.saturating_add(
                        cell.1
                            .saturating_add(gap)
                            .saturating_mul(row.saturating_sub(first_row)),
                    );
                    Rect::new(
                        panel.left() + to_i32(left),
                        panel.top() + to_i32(top),
                        cell.0,
                        cell.1,
                    )
                    .intersection(&panel)
                } else {
                    Rect::EMPTY
                },
            );
        }
        let scrollbar = (gutter > 0).then(|| {
            Rect::new(
                panel.right() - to_i32(inset.saturating_add(breadth)),
                panel.top() + to_i32(inset),
                breadth,
                height.saturating_sub(inset.saturating_mul(2)),
            )
            .intersection(&panel)
        });
        Some(PickerLayout {
            panel,
            cells,
            scrollbar,
            columns: grid.columns,
            visible_rows: grid.visible_rows,
            corner_radius: scale.scale_length(metrics.popup_corner_radius),
        })
    }

    /// The space the panel may take on its side of the bar: the whole screen
    /// across the bar's edge, and everything up to the anchored slot along
    /// the axis the panel opens outward on.
    fn available(&self, edge: Edge, screen: (u32, u32), gap: u32) -> (u32, u32) {
        let before = |edge_px: i32| u32::try_from(edge_px).unwrap_or(0).saturating_sub(gap);
        let after = |edge_px: i32, extent: u32| {
            extent
                .saturating_sub(u32::try_from(edge_px).unwrap_or(0))
                .saturating_sub(gap)
        };
        match edge {
            Edge::Bottom => (screen.0, before(self.anchor.top())),
            Edge::Top => (screen.0, after(self.anchor.bottom(), screen.1)),
            Edge::Left => (after(self.anchor.right(), screen.0), screen.1),
            Edge::Right => (before(self.anchor.left()), screen.1),
        }
    }

    /// How the entries wrap into `available`: as many columns as its width
    /// holds, the rows that follow from them, and how many of those rows the
    /// height shows at once. Every count is at least one, so a screen too
    /// small for a whole cell still lays one out (clamped onto the screen)
    /// rather than laying out nothing selectable.
    fn grid(
        &self,
        available: (u32, u32),
        cell: (u32, u32),
        (inset, gap): (u32, u32),
        gutter: u32,
    ) -> Grid {
        let fit = |extent: u32, side: u32| {
            extent
                .saturating_sub(inset.saturating_mul(2))
                .saturating_add(gap)
                / side.saturating_add(gap).max(1)
        };
        let count = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
        let columns = fit(available.0.saturating_sub(gutter), cell.0).clamp(1, count.max(1));
        let rows = u32::try_from(grid_rows(self.entries.len(), columns)).unwrap_or(u32::MAX);
        let visible_rows = fit(available.1, cell.1).clamp(1, rows.max(1));
        Grid {
            columns,
            rows,
            visible_rows,
        }
    }

    /// The cell index under `point`, or `None` for a point on the plate's
    /// own chrome or outside it.
    #[must_use]
    pub fn cell_at(&self, layout: &PickerLayout, point: Point) -> Option<usize> {
        layout
            .cells
            .iter()
            .position(|cell| !cell.is_empty() && cell.contains(point))
    }

    /// The shared control for the cell at `index`, ready to paint.
    #[must_use]
    pub(crate) fn preview(&self, index: usize) -> Option<WindowPreview> {
        let entry = self.entries.get(index)?;
        let pointer = if self.hover == Some(index) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        Some(
            WindowPreview::new(entry.title.clone(), self.icon)
                .with_state(ControlState::idle().with_pointer(pointer)),
        )
    }
}

/// One resolved grid: the columns the entries wrap into, the rows that
/// follow, and how many rows the panel shows at once.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Grid {
    columns: u32,
    rows: u32,
    visible_rows: u32,
}

/// The rows `count` cells occupy in `columns` columns.
fn grid_rows(count: usize, columns: u32) -> u64 {
    let columns = u64::from(columns.max(1));
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    count.div_ceil(columns)
}

/// Saturating `u32` → `i32`, clamping rather than wrapping.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
