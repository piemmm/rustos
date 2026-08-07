//! The computed geometry of the taskbar and pointer hit-testing.
//!
//! [`BarLayout::compute`] turns a [`TaskbarConfig`] plus the current pin,
//! task, and icon counts into the screen [`Rect`] of every region: the bar
//! itself, the two permanent leading launcher buttons (Library, then Files —
//! never reordered, never removed), the pin strip (and a slot per pinned
//! shortcut), the task list (and a slot per task), the notification area
//! (and a slot per icon), the clock, and the Switchboard tray capsule
//! anchored at the very trailing end. All arithmetic saturates, so a
//! pathological screen size or extent fails closed inside the bar rather
//! than wrapping: a leading button that does not fit is [`Rect::EMPTY`] and
//! can never be hit.
//!
//! The layout also carries the taskbar's [`corner_radius`](BarLayout::corner_radius),
//! sourced from the active theme. The taskbar does not round its own corners:
//! the window manager applies this radius through its single anti-aliased
//! rounded-corner path, exactly as it rounds windows.

use alloc::vec::Vec;

use tairix_controls::{ControlRole, Panel, TraySignal};
use tairix_geometry::{Point, Rect, Scale};
use tairix_theme::Theme;

use crate::edge::{Edge, Orientation};
use crate::library::{panel_origin, probe_chrome};
use crate::taskbar::TaskbarConfig;

/// The element of the taskbar under a pointer, returned by
/// [`BarLayout::hit_test`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Hit {
    /// The Library launcher button (the program-library popup's invoker).
    Library,
    /// The Files launcher button (opens the file manager).
    Files,
    /// The pinned shortcut at this index into [`BarLayout::pins`].
    Pin(usize),
    /// The task slot at this index into [`BarLayout::tasks`].
    Task(usize),
    /// The notification icon at this index into [`BarLayout::notifications`].
    Notification(usize),
    /// The clock.
    Clock,
    /// The Switchboard tray capsule at the very trailing end.
    Switchboard,
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
    /// The pin strip, between the leading launchers and the task list.
    /// Zero-length (but still positioned) when nothing is pinned.
    pub pin_strip: Rect,
    /// One slot per pinned shortcut, in pin order. A pin that does not fit
    /// is [`Rect::EMPTY`].
    pub pins: Vec<Rect>,
    /// The region available to the task list, between the pin strip and the
    /// notification area.
    pub task_list: Rect,
    /// One slot per running task, in task order. A task that does not fit in
    /// the task-list region is [`Rect::EMPTY`].
    pub tasks: Vec<Rect>,
    /// The notification-icon area, immediately before the clock.
    pub notification_area: Rect,
    /// One slot per icon, in icon order. An icon that does not fit is
    /// [`Rect::EMPTY`].
    pub notifications: Vec<Rect>,
    /// The clock, immediately before the Switchboard capsule.
    pub clock: Rect,
    /// The Switchboard tray capsule, anchored to the very trailing end.
    /// Zero-length (and so unhittable) when the bar is too short to hold it.
    pub switchboard: Rect,
}

impl BarLayout {
    /// Compute the layout for `config` with `pin_count` pinned shortcuts,
    /// `task_count` running tasks, and `icon_count` notification icons,
    /// applying `corner_radius` at the desktop `scale`.
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
        pin_count: usize,
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

        // The trailing regions clip against the permanent leading launchers
        // (never the reverse), so a degenerate screen shrinks the clock and
        // icons to nothing rather than overlaying them on a launcher. The
        // Switchboard capsule is placed first among them: the system readout
        // survives preferentially, so the clock and icons collapse before it
        // does — only the leading launchers outrank it.
        let switch_start = main_total
            .saturating_sub(config.switch_extent)
            .max(leading_len);
        let switchboard = placer.place(switch_start, main_total.saturating_sub(switch_start));

        let clock_start = switch_start
            .saturating_sub(config.clock_extent)
            .max(leading_len);
        let clock = placer.place(clock_start, switch_start.saturating_sub(clock_start));

        let notif_total = config.icon_extent.saturating_mul(to_u32(icon_count));
        let notif_start = clock_start.saturating_sub(notif_total).max(leading_len);
        let notification_area = placer.place(notif_start, clock_start.saturating_sub(notif_start));
        let notifications = slots(
            &placer,
            notif_start,
            config.icon_extent,
            icon_count,
            notif_start,
            clock_start,
        );

        let pin_start = leading_len.min(notif_start);
        let pin_total = config.pin_extent.saturating_mul(to_u32(pin_count));
        let pin_end = pin_start.saturating_add(pin_total).min(notif_start);
        let pin_strip = placer.place(pin_start, pin_end.saturating_sub(pin_start));
        let pins = slots(
            &placer,
            pin_start,
            config.pin_extent,
            pin_count,
            pin_start,
            notif_start,
        );

        let task_start = pin_end.min(notif_start);
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
            pin_strip,
            pins,
            task_list,
            tasks,
            notification_area,
            notifications,
            clock,
            switchboard,
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
        if self.switchboard.contains(point) {
            return Some(Hit::Switchboard);
        }
        if self.clock.contains(point) {
            return Some(Hit::Clock);
        }
        if let Some(index) = index_of(&self.notifications, point) {
            return Some(Hit::Notification(index));
        }
        if let Some(index) = index_of(&self.pins, point) {
            return Some(Hit::Pin(index));
        }
        if let Some(index) = index_of(&self.tasks, point) {
            return Some(Hit::Task(index));
        }
        None
    }

    /// The pin-list insertion index for an application reference dropped at
    /// `point`, or `None` when the point is outside the strip's drop band.
    ///
    /// The drop band spans the pin strip *and* the task-list region beside
    /// it, so a first pin can land on an empty strip (whose own rectangle is
    /// zero-length) and a drop just past the last slot appends — matching
    /// the familiar desktop gesture. Within the strip the index is chosen by
    /// slot midpoints: dropping on the leading half of a pin inserts before
    /// it, on the trailing half after it.
    #[must_use]
    pub fn pin_drop_index(&self, point: Point) -> Option<usize> {
        if !self.pin_strip.contains(point) && !self.task_list.contains(point) {
            return None;
        }
        // The main axis is decided once from the bar's own shape (a bar spans
        // its screen edge, so the longer side is the main axis) — a slot's
        // shape cannot be trusted for this because a near-square slot, or the
        // zero-length strip of the very first drop, reads ambiguously.
        let horizontal = self.bar.width >= self.bar.height;
        let index = self
            .pins
            .iter()
            .take_while(|slot| !slot.is_empty() && !before_midpoint(slot, point, horizontal))
            .count();
        Some(index)
    }
}

/// Width of the notification popover panel, in *logical* pixels at the
/// reference density.
const NOTIF_POPUP_WIDTH: u32 = 300;

/// Height of one notification card, in *logical* pixels at the reference
/// density.
const NOTIF_CARD_HEIGHT: u32 = 64;

/// The most notification cards shown at once. Extra notifications are simply
/// not drawn — the model orders by severity then recency, so the ones that
/// matter most are the ones shown (this is a *display* cap on a transient
/// surface, not a stored-capacity ceiling, so §24 does not apply).
const NOTIF_MAX_CARDS: usize = 4;

/// The notification popover's window title.
const NOTIF_TITLE: &str = "Notifications";

/// The notification popover's chrome: the shared [`Panel`], anchored back at
/// the notification region. Built with the same role as the library popup so
/// the two popovers share one chrome recipe (§2.2).
pub(crate) fn notif_panel(anchor: Point) -> Panel {
    Panel::new(NOTIF_TITLE)
        .with_role(ControlRole::Navigation)
        .with_anchor(anchor)
}

/// The placed geometry of one notification card in the popover.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NotificationCard {
    /// The card's index into the notification area's display order
    /// ([`NotificationArea::notification`](crate::NotificationArea::notification)).
    pub index: usize,
    /// The card's screen rectangle.
    pub card: Rect,
}

/// The computed geometry of the notification popover: the whole panel, the
/// radius the window manager rounds it with, the anchor its notch points back
/// at, and one placed card per shown notification (top to bottom, highest
/// severity first).
///
/// Like [`BarLayout`], all arithmetic saturates: a degenerate screen or a bar
/// pressed against the opposite edge yields a small panel with no room for
/// cards rather than wrapping, and the window manager rounds and places the
/// panel exactly as it does the bar and the library popup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationsLayout {
    /// The whole popover panel.
    pub panel: Rect,
    /// The corner radius the window manager applies to the popover — the same
    /// radius the panel chrome is drawn with, so the two never disagree.
    pub corner_radius: u32,
    /// The screen point the panel's anchor notch points back at (the centre
    /// of the notification/clock region).
    pub anchor: Point,
    /// One placed card per shown notification, in display order.
    pub cards: Vec<NotificationCard>,
}

impl NotificationsLayout {
    /// Compute the popover for `count` raised notifications, opened outward
    /// from the notification/clock region of `bar` (pinned to `edge`) at the
    /// desktop `scale` under `theme`.
    ///
    /// The panel opens on the popover's side of the bar (above a bottom bar,
    /// below a top bar, to the inner side of a side bar), aligned to the
    /// notification/clock region and clamped to the screen. It sizes to the
    /// cards it shows (capped by [`NOTIF_MAX_CARDS`] and the space between the
    /// bar and the opposite screen edge), failing closed to no cards when
    /// there is no room.
    #[must_use]
    pub(crate) fn compute(
        edge: Edge,
        bar: &BarLayout,
        screen_width: u32,
        screen_height: u32,
        scale: Scale,
        theme: &Theme,
        count: usize,
    ) -> Self {
        let metrics = theme.metrics();
        let corner_radius = scale.scale_length(metrics.window_corner_radius);
        let pad = scale.scale_length(metrics.control_gap);
        let card_height = scale.scale_length(NOTIF_CARD_HEIGHT).max(1);
        let width = scale
            .scale_length(NOTIF_POPUP_WIDTH)
            .min(screen_width)
            .max(1);

        let chrome = probe_chrome(&notif_panel(Point::ORIGIN), width, scale, theme);

        // Space available for the panel on the popover's side of the bar.
        let available = match edge {
            Edge::Bottom => u32::try_from(bar.bar.top()).unwrap_or(0),
            Edge::Top => screen_height.saturating_sub(u32::try_from(bar.bar.bottom()).unwrap_or(0)),
            Edge::Left | Edge::Right => screen_height,
        };

        // How many cards fit: the model count, capped by the display maximum
        // and by the vertical room (chrome overhead + a pad above the first
        // card, then one card-plus-pad each).
        let per_card = card_height.saturating_add(pad).max(1);
        let fixed = chrome.overhead.saturating_add(pad);
        let room = available.saturating_sub(fixed) / per_card;
        let shown = count.min(NOTIF_MAX_CARDS).min(room as usize);

        let content_height = if shown == 0 {
            0
        } else {
            to_u32(shown)
                .saturating_mul(card_height)
                .saturating_add(to_u32(shown + 1).saturating_mul(pad))
        };
        let panel_height = chrome.overhead.saturating_add(content_height);

        let anchor_rect = bar.notification_area.union(&bar.clock);
        let origin = panel_origin(
            edge,
            bar.bar,
            anchor_rect,
            width,
            panel_height,
            screen_width,
            screen_height,
        );
        let panel = Rect::new(origin.x, origin.y, width, panel_height);
        let anchor = Point::new(
            anchor_rect
                .left()
                .saturating_add(to_i32(anchor_rect.width / 2)),
            anchor_rect
                .top()
                .saturating_add(to_i32(anchor_rect.height / 2)),
        );

        let inner_x = panel
            .left()
            .saturating_add(chrome.content_left)
            .saturating_add(to_i32(pad));
        let inner_width = chrome
            .content_width
            .saturating_sub(pad.saturating_mul(2))
            .max(1);
        let mut cards = Vec::with_capacity(shown);
        let mut y = panel
            .top()
            .saturating_add(chrome.content_top)
            .saturating_add(to_i32(pad));
        for index in 0..shown {
            cards.push(NotificationCard {
                index,
                card: Rect::new(inner_x, y, inner_width, card_height),
            });
            y = y
                .saturating_add(to_i32(card_height))
                .saturating_add(to_i32(pad));
        }

        Self {
            panel,
            corner_radius,
            anchor,
            cards,
        }
    }

    /// Whether `point` lies within the popover panel.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        self.panel.contains(point)
    }

    /// The display index of the notification card `point` lies on, if any.
    #[must_use]
    pub fn card_at(&self, point: Point) -> Option<usize> {
        self.cards
            .iter()
            .find(|placed| placed.card.contains(point))
            .map(|placed| placed.index)
    }
}

/// The computed geometry of the Switchboard capsule's expanded readout: the
/// popup panel and the radius the window manager rounds it with.
///
/// The readout opens outward from the Switchboard slot through the same
/// shared origin rule as the library popup and the notification popover,
/// sized from the capsule's own
/// [`readout_size`](tairix_controls::TraySignal::readout_size) and clamped
/// to the screen. The radius is the popup radius the readout plate itself
/// draws, so the window's rounding and the plate's can never disagree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TrayReadoutLayout {
    /// The whole readout panel.
    pub panel: Rect,
    /// The corner radius the window manager applies to the readout — the
    /// same radius the readout plate is drawn with.
    pub corner_radius: u32,
}

impl TrayReadoutLayout {
    /// Compute the readout panel for `signal`, opened outward from the
    /// Switchboard slot of `bar` (pinned to `edge`) at the desktop `scale`
    /// under `theme`.
    #[must_use]
    pub(crate) fn compute(
        edge: Edge,
        bar: &BarLayout,
        screen_width: u32,
        screen_height: u32,
        scale: Scale,
        theme: &Theme,
        signal: &TraySignal,
    ) -> Self {
        let corner_radius = scale.scale_length(theme.metrics().popup_corner_radius);
        let (width, height) = signal.readout_size(scale, theme);
        let width = width.min(screen_width).max(1);
        let height = height.min(screen_height).max(1);
        let origin = panel_origin(
            edge,
            bar.bar,
            bar.switchboard,
            width,
            height,
            screen_width,
            screen_height,
        );
        Self {
            panel: Rect::new(origin.x, origin.y, width, height),
            corner_radius,
        }
    }

    /// Whether `point` lies within the readout panel.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        self.panel.contains(point)
    }
}

/// Whether `point` sits before `slot`'s main-axis midpoint — the "insert
/// before this slot" half — on the given bar axis.
fn before_midpoint(slot: &Rect, point: Point, horizontal: bool) -> bool {
    let (position, start, extent) = if horizontal {
        (point.x, slot.left(), slot.width)
    } else {
        (point.y, slot.top(), slot.height)
    };
    position < start.saturating_add(to_i32(extent / 2))
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
