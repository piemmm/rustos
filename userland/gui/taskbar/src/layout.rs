//! The computed geometry of the taskbar and pointer hit-testing.
//!
//! [`BarLayout::compute`] turns a [`TaskbarConfig`] plus the current task and
//! icon counts into the screen [`Rect`] of every region: the bar itself, the
//! two permanent leading launcher buttons (Library, then Files — never
//! reordered, never removed), the task list (and a slot per task), the
//! notification area (and a slot per icon), and the clock. All arithmetic
//! saturates, so a pathological screen size or extent fails closed inside
//! the bar rather than wrapping: a leading button that does not fit is
//! [`Rect::EMPTY`] and can never be hit.
//!
//! The layout also carries the taskbar's [`corner_radius`](BarLayout::corner_radius),
//! sourced from the active theme. The taskbar does not round its own corners:
//! the window manager applies this radius through its single anti-aliased
//! rounded-corner path, exactly as it rounds windows.

use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Scale};

use crate::edge::Orientation;
use crate::taskbar::TaskbarConfig;

/// The element of the taskbar under a pointer, returned by
/// [`BarLayout::hit_test`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Hit {
    /// The Library launcher button (the program-library popup's invoker).
    Library,
    /// The Files launcher button (opens the file manager).
    Files,
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
    /// The corner radius the window manager applies to the bar. `0` is square.
    pub corner_radius: u32,
    /// The Library launcher button, at the leading end. [`Rect::EMPTY`] when
    /// the bar is too short to hold it.
    pub library: Rect,
    /// The Files launcher button, immediately after the Library button.
    /// [`Rect::EMPTY`] when the bar is too short to hold it.
    pub files: Rect,
    /// The region available to the task list, between the leading launchers
    /// and the notification area.
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
    /// so the bar stays a comfortable physical size across panel densities.
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

        let launchers = slots(&placer, 0, config.launcher_extent, 2, 0, main_total);
        let library = launchers[0];
        let files = launchers[1];
        let leading_len = config.launcher_extent.saturating_mul(2).min(main_total);

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

        let task_start = leading_len.min(notif_start);
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
            library,
            files,
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
        if self.library.contains(point) {
            return Some(Hit::Library);
        }
        if self.files.contains(point) {
            return Some(Hit::Files);
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
/// [`i32::MAX`] rather than wrapping.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Saturating `usize` → `u32`.
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
