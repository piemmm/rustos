//! The computed geometry of the taskbar and pointer hit-testing.
//!
//! [`BarLayout::compute`] turns a [`TaskbarConfig`] plus the current task and
//! icon counts into the screen [`Rect`] of every region: the bar itself, the
//! start button, the task list (and a slot per task), the notification area
//! (and a slot per icon), and the clock. All arithmetic saturates, so a
//! pathological screen size or extent fails closed inside the bar rather than
//! wrapping (`AGENTS.md` §2.9).
//!
//! The layout also carries the taskbar's [`corner_radius`](BarLayout::corner_radius),
//! sourced from the active theme. The taskbar does not round its own corners:
//! the window manager applies this radius through its single anti-aliased
//! rounded-corner path, exactly as it rounds windows (`AGENTS.md` §2.2).

use alloc::vec::Vec;

use rustos_geometry::{Point, Rect, Scale};

use crate::edge::{Edge, Orientation};
use crate::taskbar::TaskbarConfig;

/// Width of the start-menu popup, in *logical* pixels at the reference
/// density (scaled to physical pixels by [`Scale`]).
const MENU_WIDTH: u32 = 200;
/// Height of one start-menu entry row, in *logical* pixels at the reference
/// density (scaled to physical pixels by [`Scale`]).
const MENU_ROW_HEIGHT: u32 = 32;

/// The element of the taskbar under a pointer, returned by
/// [`BarLayout::hit_test`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Hit {
    /// The start-menu button.
    StartButton,
    /// The task slot at this index into [`BarLayout::tasks`].
    Task(usize),
    /// The notification icon at this index into [`BarLayout::notifications`].
    Notification(usize),
    /// The clock.
    Clock,
}

/// The fully computed geometry of a taskbar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BarLayout {
    /// The whole bar.
    pub bar: Rect,
    /// The corner radius the window manager applies to the bar (`AGENTS.md`
    /// §2.2). `0` is square.
    pub corner_radius: u32,
    /// The start-menu button, at the leading end.
    pub start_button: Rect,
    /// The region available to the task list, between the start button and
    /// the notification area.
    pub task_list: Rect,
    /// One slot per running task, in task order. A task that does not fit in
    /// the task-list region is [`Rect::EMPTY`].
    pub tasks: Vec<Rect>,
    /// The notification-icon area, immediately before the clock.
    pub notification_area: Rect,
    /// One slot per icon, in icon order. An icon that does not fit is
    /// [`Rect::EMPTY`].
    pub notifications: Vec<Rect>,
    /// The clock, anchored to the trailing end.
    pub clock: Rect,
}

impl BarLayout {
    /// Compute the layout for `config` with `task_count` running tasks and
    /// `icon_count` notification icons, applying `corner_radius` at the
    /// desktop `scale`.
    ///
    /// The screen dimensions in `config` are *physical* pixels (the real
    /// framebuffer), while the bar's extents, thickness, and the
    /// `corner_radius` are *logical* pixels authored at the reference
    /// density; `scale` converts those logical lengths into physical pixels
    /// so the bar stays a comfortable physical size across panel densities
    /// (`AGENTS.md` §10).
    #[must_use]
    pub fn compute(
        config: &TaskbarConfig,
        corner_radius: u32,
        scale: Scale,
        task_count: usize,
        icon_count: usize,
    ) -> Self {
        let scaled = config.scaled(scale);
        let config = &scaled;
        let corner_radius = scale.scale_length(corner_radius);
        let orientation = config.edge.orientation();
        let (main_total, cross_size) = match orientation {
            Orientation::Horizontal => (config.screen_width, config.screen_height),
            Orientation::Vertical => (config.screen_height, config.screen_width),
        };
        let thickness = config.thickness.min(cross_size);
        let cross_origin = if config.edge.at_trailing_cross_edge() {
            to_i32(cross_size.saturating_sub(thickness))
        } else {
            0
        };
        let placer = Placer {
            orientation,
            cross_origin,
            thickness,
        };

        let bar = placer.place(0, main_total);

        let start_len = config.start_button_extent.min(main_total);
        let start_button = placer.place(0, start_len);

        let clock_start = main_total.saturating_sub(config.clock_extent);
        let clock = placer.place(clock_start, main_total.saturating_sub(clock_start));

        let notif_total = config.icon_extent.saturating_mul(to_u32(icon_count));
        let notif_start = clock_start.saturating_sub(notif_total);
        let notification_area = placer.place(notif_start, clock_start.saturating_sub(notif_start));
        let notifications = slots(
            &placer,
            notif_start,
            config.icon_extent,
            icon_count,
            notif_start,
            clock_start,
        );

        let task_start = start_len.min(notif_start);
        let task_list = placer.place(task_start, notif_start.saturating_sub(task_start));
        let tasks = slots(
            &placer,
            task_start,
            config.task_extent,
            task_count,
            task_start,
            notif_start,
        );

        Self {
            bar,
            corner_radius,
            start_button,
            task_list,
            tasks,
            notification_area,
            notifications,
            clock,
        }
    }

    /// The taskbar element under `point`, or `None` for a point outside the
    /// bar or on a gap between regions.
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<Hit> {
        if !self.bar.contains(point) {
            return None;
        }
        if self.start_button.contains(point) {
            return Some(Hit::StartButton);
        }
        if self.clock.contains(point) {
            return Some(Hit::Clock);
        }
        if let Some(index) = index_of(&self.notifications, point) {
            return Some(Hit::Notification(index));
        }
        if let Some(index) = index_of(&self.tasks, point) {
            return Some(Hit::Task(index));
        }
        None
    }
}

/// The computed geometry of the start-menu popup.
///
/// The popup is the transient surface the start button opens. Like the bar
/// itself it is a *rectangular* buffer the window manager places and rounds:
/// [`MenuLayout::compute`] reports the popup [`panel`](Self::panel), the
/// [`corner_radius`](Self::corner_radius) the compositor applies through its
/// single anti-aliased rounded-corner path (`AGENTS.md` §2.2), and one
/// [`Rect`] per menu entry stacked along the panel. The popup opens *outward*
/// from the bar: above a bottom bar, below a top bar, and to the inner side of
/// a left/right bar, with its leading edge aligned to the start button.
///
/// All arithmetic saturates, so a pathological screen or scale fails closed
/// rather than wrapping (`AGENTS.md` §2.9).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuLayout {
    /// The whole popup panel.
    pub panel: Rect,
    /// The corner radius the window manager applies to the popup (`AGENTS.md`
    /// §2.2). `0` is square.
    pub corner_radius: u32,
    /// One row per menu entry, in entry order, stacked from the panel's top.
    pub entries: Vec<Rect>,
}

impl MenuLayout {
    /// Compute the popup geometry for a menu of `entry_count` entries opened
    /// from `start_button` on the `bar` pinned to `edge`, applying
    /// `popup_corner_radius` at the desktop `scale`.
    ///
    /// `bar` and `start_button` are the already-scaled *physical* rectangles
    /// from a [`BarLayout`]; the popup's own width, row height, and corner
    /// radius are *logical* lengths converted through `scale`, so the
    /// logical→physical conversion is the one in [`Scale::scale_length`],
    /// never re-derived (`AGENTS.md` §2.2 / §10).
    #[must_use]
    pub fn compute(
        edge: Edge,
        bar: Rect,
        start_button: Rect,
        popup_corner_radius: u32,
        scale: Scale,
        entry_count: usize,
    ) -> Self {
        let width = scale.scale_length(MENU_WIDTH);
        let row_height = scale.scale_length(MENU_ROW_HEIGHT);
        let corner_radius = scale.scale_length(popup_corner_radius);
        let panel_height = row_height.saturating_mul(to_u32(entry_count));

        let (x, y) = match edge {
            Edge::Top => (start_button.left(), bar.bottom()),
            Edge::Bottom => (
                start_button.left(),
                bar.top().saturating_sub(to_i32(panel_height)),
            ),
            Edge::Left => (bar.right(), start_button.top()),
            Edge::Right => (bar.left().saturating_sub(to_i32(width)), start_button.top()),
        };
        let panel = Rect::new(x, y, width, panel_height);

        let mut entries = Vec::with_capacity(entry_count);
        for index in 0..entry_count {
            let offset = row_height.saturating_mul(to_u32(index));
            let row_top = y.saturating_add(to_i32(offset));
            entries.push(Rect::new(x, row_top, width, row_height));
        }

        Self {
            panel,
            corner_radius,
            entries,
        }
    }

    /// The index of the entry under `point`, or `None` for a point outside the
    /// panel or on a gap between rows.
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<usize> {
        if !self.panel.contains(point) {
            return None;
        }
        self.entries.iter().position(|row| row.contains(point))
    }
}

/// Maps a main-axis interval to a screen [`Rect`] for one bar orientation.
struct Placer {
    orientation: Orientation,
    cross_origin: i32,
    thickness: u32,
}

impl Placer {
    /// The rectangle for main-axis interval `[main_off, main_off + main_len)`.
    fn place(&self, main_off: u32, main_len: u32) -> Rect {
        let off = to_i32(main_off);
        match self.orientation {
            Orientation::Horizontal => Rect::new(off, self.cross_origin, main_len, self.thickness),
            Orientation::Vertical => Rect::new(self.cross_origin, off, self.thickness, main_len),
        }
    }
}

/// Lay `count` fixed-width slots out from `base`, each `extent` long, every
/// slot clipped to `[lo, hi)`; a slot that does not fit is [`Rect::EMPTY`].
fn slots(placer: &Placer, base: u32, extent: u32, count: usize, lo: u32, hi: u32) -> Vec<Rect> {
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let start = base.saturating_add(extent.saturating_mul(to_u32(index)));
        let end = start.saturating_add(extent);
        let clipped_start = start.max(lo);
        let clipped_end = end.min(hi);
        let rect = if clipped_end <= clipped_start {
            Rect::EMPTY
        } else {
            placer.place(clipped_start, clipped_end - clipped_start)
        };
        out.push(rect);
    }
    out
}

/// The index of the first slot containing `point`, ignoring empty slots.
fn index_of(rects: &[Rect], point: Point) -> Option<usize> {
    rects.iter().position(|rect| rect.contains(point))
}

/// Saturating `u32` → `i32`; an out-of-range coordinate clamps to
/// [`i32::MAX`] rather than wrapping (`AGENTS.md` §2.9).
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Saturating `usize` → `u32`.
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
