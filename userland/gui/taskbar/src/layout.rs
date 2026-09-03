//! The computed geometry of the taskbar and pointer hit-testing.
//!
//! [`BarLayout::compute`] turns a [`TaskbarConfig`] plus the current
//! application and icon counts into the screen [`Rect`] of every region: the
//! bar itself, the permanent leading launcher button (Library — never
//! reordered, never removed), the separator rule that divides it from
//! everything after it, the application strip (and
//! a slot per running application), the notification area (and a slot per
//! icon), the clock, and the Switchboard tray capsule anchored at the very
//! trailing end. All arithmetic saturates, so a
//! pathological screen size or extent fails closed inside the bar rather
//! than wrapping: a leading button that does not fit is [`Rect::EMPTY`] and
//! can never be hit.
//!
//! The bar **floats**: it stands off the three screen edges it faces by the
//! theme's [`taskbar_margin`](tairix_theme::Metrics::taskbar_margin), so the
//! wallpaper runs unbroken around it. Every region — and every popup opened
//! from one — is placed from [`BarLayout::bar`], so the margin is spent in
//! one place and everything follows it.
//!
//! Inside the bar, every region is laid out within the bar's own rim (one
//! [`plate_border`] on each side), so a hovered or pressed slot washes up to
//! the rim and never over it. [`BarLayout::bar`] itself is the whole bar,
//! rim included.
//!
//! The layout also carries the taskbar's [`corner_radius`](BarLayout::corner_radius).
//! The bar is a stadium: its two ends are semicircles, so the radius is half
//! the bar's own thickness rather than a themed number that could only
//! accidentally equal it. The taskbar does not round its own corners — the
//! window manager applies this radius through its single anti-aliased
//! rounded-corner path, exactly as it rounds windows — and the bar's rim is
//! painted with the same radius, so rim and silhouette trace one curve.

use alloc::vec::Vec;

use tairix_controls::{plate_border, ControlRole, Panel, TraySignal};
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
    /// The running application at this index into [`BarLayout::apps`].
    App(usize),
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
    /// The whole bar — rim included — standing off the three screen edges it
    /// faces by the theme's taskbar margin. Every other region here is laid
    /// out inside that rim.
    pub bar: Rect,
    /// The corner radius the window manager applies to the bar: half its
    /// thickness, which makes each end a semicircle.
    pub corner_radius: u32,
    /// The Library launcher button, at the leading end. [`Rect::EMPTY`] when
    /// the bar is too short to hold it.
    pub library: Rect,
    /// The rule dividing the Library launcher from everything after it: one
    /// border thickness along the bar's main axis, spanning the cross axis
    /// inset from both long edges. Decoration only — [`hit_test`](Self::hit_test)
    /// never reports it, so a press here lands on the bare bar. [`Rect::EMPTY`]
    /// when the bar is too short to reach it or too thin to inset it.
    pub separator: Rect,
    /// The region the running applications occupy, between the leading
    /// launcher and the notification area.
    pub app_strip: Rect,
    /// One slot per running application, in strip order. An application that
    /// does not fit in the strip is [`Rect::EMPTY`].
    pub apps: Vec<Rect>,
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
    /// Compute the layout for `config` with `app_count` running
    /// applications and `icon_count` notification icons, taking the bar's
    /// separator rule and the rule's spacing from `theme` at the desktop
    /// `scale`.
    ///
    /// The screen dimensions in `config` are *physical* pixels (the real
    /// framebuffer), while the bar's extents and thickness and the theme's
    /// metrics are *logical* pixels authored at the reference density;
    /// `scale` converts those logical lengths into physical pixels so the bar
    /// stays a comfortable physical size across panel densities.
    ///
    /// The bar is inset from the three screen edges it faces by the theme's
    /// [`taskbar_margin`](tairix_theme::Metrics::taskbar_margin) — for a
    /// bottom bar the left, right, and bottom. Its thickness is unchanged:
    /// the fourth side faces the work area and the margin never widens it.
    ///
    /// Regions are then laid out inside the bar's rim, one [`plate_border`]
    /// in on every side, so nothing the bar seats can paint over the edge the
    /// renderer draws. [`bar`](Self::bar) keeps the whole rectangle.
    #[must_use]
    pub fn compute(
        config: &TaskbarConfig,
        theme: &Theme,
        scale: Scale,
        app_count: usize,
        icon_count: usize,
    ) -> Self {
        let scaled = config.scaled(scale);
        let config = &scaled;
        let metrics = theme.metrics();
        let orientation = config.edge.orientation();
        let (main_screen, cross_screen) = match orientation {
            Orientation::Horizontal => (config.screen_width, config.screen_height),
            Orientation::Vertical => (config.screen_height, config.screen_width),
        };
        // The gap the bar floats in, spent once: at both ends of the main
        // axis and against the cross edge the bar faces. A screen too small
        // to afford it keeps the bar rather than the gap — the margin never
        // takes half an axis, nor the bar's last pixel of length.
        let margin = scale
            .scale_length(metrics.taskbar_margin)
            .min(main_screen.saturating_sub(1) / 2)
            .min(cross_screen / 2);
        let main_total = main_screen.saturating_sub(margin.saturating_mul(2));
        let thickness = config.thickness.min(cross_screen.saturating_sub(margin));
        // The bar's own rim, spent once: it belongs to the bar rather than to
        // what stands on it, so every region is laid out inside it and a
        // hovered slot cannot wash over the surface's edge. A bar too thin to
        // spare two rims keeps its content rather than the inset.
        let border = plate_border(theme, scale)
            .min(main_total.saturating_sub(1) / 2)
            .min(thickness.saturating_sub(1) / 2);
        let cross_origin = if config.edge.at_trailing_cross_edge() {
            to_i32(
                cross_screen
                    .saturating_sub(thickness)
                    .saturating_sub(margin),
            )
        } else {
            to_i32(margin)
        };
        let bar_placer = Placer {
            orientation,
            main_origin: margin,
            cross_origin,
            thickness,
        };

        let bar = bar_placer.place(0, main_total);
        // A stadium: rounding by half the shorter side turns the bar's two
        // ends into semicircles, whichever edge it is pinned to.
        let corner_radius = bar.width.min(bar.height) / 2;

        let placer = bar_placer.inside(border);
        let content_total = main_total.saturating_sub(border.saturating_mul(2));

        // The rule divides two adjacent controls of one strip: the theme's
        // control gap spaces it, and the padding a control keeps from its
        // plate edge insets it so the bar's rounded ends stay clear. Flooring
        // at one physical pixel keeps it from vanishing at a small scale.
        let rule = scale.scale_length(metrics.border_thickness).max(1);
        let rule_gap = scale.scale_length(metrics.control_gap);
        let inset = scale.scale_length(metrics.control_inset);
        let gutter = rule.saturating_add(rule_gap.saturating_mul(2));

        let library = slot(&placer, 0, config.launcher_extent, 0, content_total);
        let rule_start = config.launcher_extent.saturating_add(rule_gap);
        let separator = clip(rule_start, rule, 0, content_total)
            .map_or(Rect::EMPTY, |(off, len)| {
                placer.place_inset(off, len, inset)
            });
        let leading_len = config
            .launcher_extent
            .saturating_add(gutter)
            .min(content_total);

        let trailing = trailing_end(&placer, config, (leading_len, content_total), icon_count);

        let app_start = leading_len.min(trailing.start);
        let app_strip = placer.place(app_start, trailing.start.saturating_sub(app_start));
        let apps = slots(
            &placer,
            app_start,
            config.app_extent,
            app_count,
            app_start,
            trailing.start,
        );

        Self {
            bar,
            corner_radius,
            library,
            separator,
            app_strip,
            apps,
            notification_area: trailing.notification_area,
            notifications: trailing.notifications,
            clock: trailing.clock,
            switchboard: trailing.switchboard,
        }
    }

    /// The taskbar element under `point`, or `None` for a point outside the
    /// bar or on a gap between regions.
    ///
    /// The [`separator`](Self::separator) is not a region: it is painted
    /// decoration, so a point on the rule (or in the gutter around it) reports
    /// no element and the press reaches the bare bar.
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<Hit> {
        if !self.bar.contains(point) {
            return None;
        }
        if self.library.contains(point) {
            return Some(Hit::Library);
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
        if let Some(index) = index_of(&self.apps, point) {
            return Some(Hit::App(index));
        }
        None
    }
}

/// The regions anchored to the bar's trailing end.
struct TrailingEnd {
    switchboard: Rect,
    clock: Rect,
    notification_area: Rect,
    notifications: Vec<Rect>,
    /// Where the group begins: the application strip before it stops here.
    start: u32,
}

/// Place the trailing regions within `span` —
/// `(the offset the leading launcher and its rule end at, the content region's main-axis length)`.
///
/// They clip against the permanent leading launcher and never the reverse, so
/// a degenerate screen shrinks the clock and icons to nothing rather than
/// overlaying them on it. The Switchboard capsule is placed first among them:
/// the system readout survives preferentially, so the clock and icons
/// collapse before it does — only the leading launcher outranks it.
fn trailing_end(
    placer: &Placer,
    config: &TaskbarConfig,
    span: (u32, u32),
    icon_count: usize,
) -> TrailingEnd {
    let (leading_len, content_total) = span;
    let switch_start = content_total
        .saturating_sub(config.switch_extent)
        .max(leading_len);
    let switchboard = placer.place(switch_start, content_total.saturating_sub(switch_start));

    let clock_start = switch_start
        .saturating_sub(config.clock_extent)
        .max(leading_len);
    let clock = placer.place(clock_start, switch_start.saturating_sub(clock_start));

    let notif_total = config.icon_extent.saturating_mul(to_u32(icon_count));
    let start = clock_start.saturating_sub(notif_total).max(leading_len);
    let notification_area = placer.place(start, clock_start.saturating_sub(start));
    let notifications = slots(
        placer,
        start,
        config.icon_extent,
        icon_count,
        start,
        clock_start,
    );

    TrailingEnd {
        switchboard,
        clock,
        notification_area,
        notifications,
        start,
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
/// surface, not a stored-capacity ceiling that has to scale with the machine).
const NOTIF_MAX_CARDS: usize = 4;

/// The notification popover's window title.
const NOTIF_TITLE: &str = "Notifications";

/// The notification popover's chrome: the shared [`Panel`], anchored back at
/// the notification region. Built with the same role as the library popup so
/// the two popovers share one chrome recipe.
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
        let origin = panel_origin(edge, bar.bar, anchor_rect, width, panel_height);
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
        let origin = panel_origin(edge, bar.bar, bar.switchboard, width, height);
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

/// Maps a main-axis interval to a screen [`Rect`] for one bar orientation.
///
/// Intervals are local to the region being placed: it starts at main-axis
/// zero, and this is where its origin — the margin the bar floats in, plus
/// the rim the bar's contents sit inside — is added, so a region is offset
/// exactly once.
struct Placer {
    orientation: Orientation,
    main_origin: u32,
    cross_origin: i32,
    thickness: u32,
}

impl Placer {
    /// The rectangle for main-axis interval `[main_off, main_off + main_len)`.
    fn place(&self, main_off: u32, main_len: u32) -> Rect {
        let off = to_i32(main_off.saturating_add(self.main_origin));
        match self.orientation {
            Orientation::Horizontal => Rect::new(off, self.cross_origin, main_len, self.thickness),
            Orientation::Vertical => Rect::new(self.cross_origin, off, self.thickness, main_len),
        }
    }

    /// The placer for what stands *on* the surface: the same axis mapping
    /// pulled in by `border` on both axes, so a region laid out through it
    /// falls inside the surface's own rim instead of over it. The caller
    /// shortens the main-axis length to match.
    fn inside(&self, border: u32) -> Self {
        Self {
            orientation: self.orientation,
            main_origin: self.main_origin.saturating_add(border),
            cross_origin: self.cross_origin.saturating_add(to_i32(border)),
            thickness: self.thickness.saturating_sub(border.saturating_mul(2)),
        }
    }

    /// [`place`](Self::place), pulled in by `cross_inset` at both ends of the
    /// cross axis. A bar too thin to hold the inset yields [`Rect::EMPTY`]
    /// rather than a rule running edge to edge.
    fn place_inset(&self, main_off: u32, main_len: u32, cross_inset: u32) -> Rect {
        let breadth = self.thickness.saturating_sub(cross_inset.saturating_mul(2));
        if main_len == 0 || breadth == 0 {
            return Rect::EMPTY;
        }
        let off = to_i32(main_off.saturating_add(self.main_origin));
        let cross = self.cross_origin.saturating_add(to_i32(cross_inset));
        match self.orientation {
            Orientation::Horizontal => Rect::new(off, cross, main_len, breadth),
            Orientation::Vertical => Rect::new(cross, off, breadth, main_len),
        }
    }
}

/// The main-axis interval `[start, start + extent)` clipped to `[lo, hi)` as
/// an `(offset, length)` pair, or `None` when nothing of it survives.
fn clip(start: u32, extent: u32, lo: u32, hi: u32) -> Option<(u32, u32)> {
    let end = start.saturating_add(extent).min(hi);
    let start = start.max(lo);
    (end > start).then(|| (start, end - start))
}

/// One `extent`-long slot at `start`, clipped to `[lo, hi)`; a slot with
/// nothing left after clipping is [`Rect::EMPTY`].
fn slot(placer: &Placer, start: u32, extent: u32, lo: u32, hi: u32) -> Rect {
    clip(start, extent, lo, hi).map_or(Rect::EMPTY, |(off, len)| placer.place(off, len))
}

/// Lay `count` fixed-width slots out from `base`, each `extent` long, every
/// slot clipped to `[lo, hi)`.
fn slots(placer: &Placer, base: u32, extent: u32, count: usize, lo: u32, hi: u32) -> Vec<Rect> {
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let start = base.saturating_add(extent.saturating_mul(to_u32(index)));
        out.push(slot(placer, start, extent, lo, hi));
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
