//! The window picker an icon-bar slot opens on hover.
//!
//! An icon-bar slot is one *application*, so a slot whose application owns
//! more than one window needs somewhere to choose between them. Hovering the
//! slot opens the picker: one captioned thumbnail per window, laid out along
//! the bar's main axis, and a primary press on a cell raises and focuses that
//! window.
//!
//! **It opens only above one window.** A slot with a single window has
//! nothing to pick — a click on the slot already reaches that window — so
//! sweeping the pointer along a bar of ordinary single-window applications
//! pops nothing up. That is what makes a hover-opened surface tolerable
//! without a dwell timer: the picker appears exactly where there is a choice
//! to make, and the bar needs no clock to decide it.
//!
//! The thumbnails are the session's: it maps every application's frame region
//! already, so it scales the last presented frame down to the cell through the
//! shared rasteriser and hands the result over. The bar never touches an
//! application's live pixels, and a window with no thumbnail yet draws its
//! application's glyph instead of a hole.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{plate_border, ControlState, PointerState, WindowPreview};
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
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
    /// One cell per entry, in order. A cell that does not fit is
    /// [`Rect::EMPTY`] and can never be hit.
    pub cells: Vec<Rect>,
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
    pub const fn new() -> Self {
        Self {
            app: None,
            anchor: Rect::EMPTY,
            icon: IconKind::AppBundle,
            entries: Vec::new(),
            hover: None,
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
        // Already showing this application: keep the highlight rather than
        // dropping it on every pointer sample over the slot.
        if !same {
            self.hover = None;
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

    /// The picker's geometry: a row of cells opening outward from the
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
        let cell_w = scale.scale_length(CELL_WIDTH).max(1);
        let cell_h = scale.scale_length(CELL_HEIGHT).max(1);
        let count = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
        let strip_w = cell_w
            .saturating_mul(count)
            .saturating_add(gap.saturating_mul(count.saturating_sub(1)));
        let width = strip_w
            .saturating_add(border.saturating_add(gap).saturating_mul(2))
            .min(screen_width.max(1));
        let height = cell_h
            .saturating_add(border.saturating_add(gap).saturating_mul(2))
            .min(screen_height.max(1));
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
        let panel = Rect::new(x, y, width, height).clamped_onto(Rect::new(
            0,
            0,
            screen_width,
            screen_height,
        ));
        let inset = border.saturating_add(gap);
        let mut cells = Vec::with_capacity(self.entries.len());
        for index in 0..self.entries.len() {
            let offset = cell_w
                .saturating_add(gap)
                .saturating_mul(u32::try_from(index).unwrap_or(u32::MAX));
            let left = inset.saturating_add(offset);
            let cell = if left.saturating_add(cell_w).saturating_add(inset) > panel.width {
                Rect::EMPTY
            } else {
                Rect::new(
                    panel.left() + to_i32(left),
                    panel.top() + to_i32(inset),
                    cell_w,
                    cell_h.min(panel.height.saturating_sub(inset.saturating_mul(2))),
                )
            };
            cells.push(cell);
        }
        Some(PickerLayout {
            panel,
            cells,
            corner_radius: scale.scale_length(metrics.popup_corner_radius),
        })
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

/// Saturating `u32` → `i32`, clamping rather than wrapping.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
