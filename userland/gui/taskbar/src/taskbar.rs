//! The taskbar itself: its placement configuration and live state.
//!
//! [`Taskbar`] ties the permanent leading launcher button (Library), the
//! [`LibraryPopup`], the [`TaskList`], the [`NotificationArea`],
//! and the [`SwitchboardTray`] capsule to a [`TaskbarConfig`] (which screen
//! edge, how thick, and the per-region extents) and the active [`Theme`]. It
//! produces a [`BarLayout`] on demand from the current state and answers
//! pointer hits for input routing.
//!
//! The bar owns a copy of the active theme so its layout, hit-testing, and
//! painting all read one definition — the radius a hit-test assumes and the
//! radius the painter draws can never disagree. It also carries a per-surface
//! [`TaskbarRepaint`] latch: a state change that alters only what one of the
//! bar's five rendered surfaces draws sets that surface's flag (and every
//! surface it genuinely touches — never fewer), and the embedder drains it
//! with [`take_repaint`](Taskbar::take_repaint) to re-present exactly the
//! surfaces that changed.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::TraySummary;
use tairix_abi::window_ipc::AppMenuItemId;
use tairix_controls::{
    ControlRole, IconButton, PlatePlacement, PlateSeating, PointerState, TaskbarItem,
    TraySignalAction,
};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::IconKind;
use tairix_input::InputEvent;
use tairix_proglib::EntryId;
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::apps::{AppSlot, AppStrip};
use crate::clock::Clock;
use crate::clock_menu::ClockPermits;
use crate::edge::Edge;
use crate::input::TaskbarResponse;
use crate::layout::{BarLayout, Hit, NotificationsLayout, TrayReadoutLayout};
use crate::library::{LibraryIconRequest, LibraryLayout, LibraryPopup};
use crate::menu::{self, MenuRequest, MenuSubject};
use crate::notifications::{NotificationArea, StatusSignal, TransientNotification};
use crate::picker::{PickerEntry, PickerLayout, WindowPicker};
use crate::repaint::TaskbarRepaint;
use crate::system::{self, SystemPermits};
use crate::tasks::TaskList;
use crate::tray::SwitchboardTray;

/// Where the taskbar sits and how big each region is.
///
/// The screen dimensions are *physical* pixels (the real framebuffer). The
/// extents and `thickness` are *logical* pixels authored at the reference
/// density (`tairix_geometry::REFERENCE_DPI`); the desktop's [`Scale`]
/// converts them to physical pixels at layout time, so the bar stays a
/// comfortable physical size across panel densities.
///
/// Extents are measured along the bar's main axis (width for a horizontal
/// bar, height for a vertical one); `thickness` is the cross-axis size.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TaskbarConfig {
    /// The screen edge the bar is pinned to.
    pub edge: Edge,
    /// Screen width in pixels.
    pub screen_width: u32,
    /// Screen height in pixels.
    pub screen_height: u32,
    /// Bar thickness (height for a horizontal bar, width for a vertical one).
    pub thickness: u32,
    /// Main-axis length of the leading launcher button (Library).
    pub launcher_extent: u32,
    /// Main-axis length of each running-application slot. An icon-only
    /// slot, so a run of applications reads as one strip of equal icons
    /// rather than a row of captions.
    pub app_extent: u32,
    /// Main-axis length of each notification icon.
    pub icon_extent: u32,
    /// Main-axis length of the clock.
    pub clock_extent: u32,
    /// Main-axis length of the Switchboard tray capsule.
    pub switch_extent: u32,
}

impl TaskbarConfig {
    /// This configuration with every *logical* extent and the thickness
    /// converted to physical pixels at `scale`, leaving the physical screen
    /// dimensions and the edge untouched.
    ///
    /// [`BarLayout::compute`](crate::layout::BarLayout::compute) uses this so
    /// the logical→physical conversion is the one in
    /// [`Scale::scale_length`], never re-derived here.
    #[must_use]
    pub fn scaled(&self, scale: Scale) -> Self {
        Self {
            edge: self.edge,
            screen_width: self.screen_width,
            screen_height: self.screen_height,
            thickness: scale.scale_length(self.thickness),
            launcher_extent: scale.scale_length(self.launcher_extent),
            app_extent: scale.scale_length(self.app_extent),
            icon_extent: scale.scale_length(self.icon_extent),
            clock_extent: scale.scale_length(self.clock_extent),
            switch_extent: scale.scale_length(self.switch_extent),
        }
    }

    /// A conventional horizontal bottom bar for a `screen_width` ×
    /// `screen_height` screen, using the house-style extents.
    #[must_use]
    pub const fn bottom_bar(screen_width: u32, screen_height: u32) -> Self {
        Self {
            edge: Edge::Bottom,
            screen_width,
            screen_height,
            thickness: 40,
            launcher_extent: 48,
            app_extent: 48,
            icon_extent: 24,
            clock_extent: 80,
            switch_extent: 44,
        }
    }
}

/// The taskbar: placement configuration plus the leading launcher button,
/// the program-library popup, the task list, the notification area, and the
/// Switchboard tray capsule, themed by the active [`Theme`].
///
/// The bar does **not** own a UI scale: the desktop density belongs to the
/// output, so the scale is supplied by the compositor at layout, hit-test,
/// and render time. A runtime DPI change is therefore
/// transparent to the taskbar model — the bar is simply laid out and
/// re-presented at the new density, with no state to update here.
#[derive(Clone, Debug)]
pub struct Taskbar {
    config: TaskbarConfig,
    theme: Theme,
    library_button: IconButton,
    library: LibraryPopup,
    apps: AppStrip,
    picker: WindowPicker,
    tasks: TaskList,
    notifications: NotificationArea,
    clock: Clock,
    tray: SwitchboardTray,
    elevation_available: bool,
    switch_user_available: bool,
    repaint: TaskbarRepaint,
}

impl Taskbar {
    /// Build a taskbar for `config`, adopting `theme` as its active theme.
    ///
    /// The permanent leading button is seeded here: the Library button. It is
    /// fixed — nothing can move or remove it.
    ///
    /// It is an ordinary quiet peer seated *in* the bar
    /// ([`PlateSeating::Bar`]): on an icon strip no single icon is the primary
    /// action of the surface, so it wears no role fill and no perimeter of its
    /// own. It rests as a bare glyph on the bar, washes lighter under the
    /// pointer, and reads as held down while its popup is open.
    #[must_use]
    pub fn new(config: TaskbarConfig, theme: &Theme) -> Self {
        Self {
            config,
            theme: theme.clone(),
            library_button: IconButton::new(IconKind::Library, ControlRole::Neutral)
                .seated(PlateSeating::Bar),
            library: LibraryPopup::new(),
            apps: AppStrip::new(),
            picker: WindowPicker::new(),
            tasks: TaskList::new(),
            notifications: NotificationArea::new(),
            clock: Clock::new(),
            tray: SwitchboardTray::new(),
            elevation_available: false,
            switch_user_available: false,
            repaint: TaskbarRepaint::NONE,
        }
    }

    /// The placement configuration.
    #[must_use]
    pub const fn config(&self) -> &TaskbarConfig {
        &self.config
    }

    /// The active theme the bar lays out and paints with.
    ///
    /// It is the theme its embedder handed it, ground and all: whoever puts a
    /// surface on screen is the only party that knows what is behind it, so the
    /// desktop hands the bar the *floating* form it derives once for all of its
    /// chrome, and the bar adopts it whole. Holding one theme for the bar and
    /// every popup it opens is what makes that true of everything it draws — no
    /// control is told separately, and none can be left an opaque patch.
    #[must_use]
    pub const fn theme(&self) -> &Theme {
        &self.theme
    }

    /// The corner radius the window manager applies to the bar.
    #[must_use]
    pub fn corner_radius(&self) -> u32 {
        self.theme.metrics().taskbar_corner_radius
    }

    /// The Library launcher button, for painting.
    #[must_use]
    pub const fn library_button(&self) -> &IconButton {
        &self.library_button
    }

    /// The program-library popup.
    #[must_use]
    pub const fn library(&self) -> &LibraryPopup {
        &self.library
    }

    /// The program-library popup, mutably — how the session hands it the
    /// resolved catalog ([`LibraryPopup::set_catalog`]) — latching the popup
    /// and the bar.
    ///
    /// Handing out a mutable view is indistinguishable from changing it: the
    /// bar cannot see what the caller does through the borrow, and a missed
    /// latch leaves stale pixels, so the borrow itself latches. The bar
    /// latches with the popup because the same view can open or close it,
    /// which changes how the Library button draws. Callers take this borrow
    /// to make a real state change — a resolved catalog — never once per
    /// input sample, so the conservative latch costs nothing on the hot
    /// path; the input router borrows the popup through a crate-internal
    /// seam instead and latches from what the popup reports.
    pub fn library_mut(&mut self) -> &mut LibraryPopup {
        self.repaint |= TaskbarRepaint::LIBRARY | TaskbarRepaint::BAR;
        &mut self.library
    }

    /// The program-library popup, mutably, for routing one input event into
    /// it.
    ///
    /// Unlike [`library_mut`](Self::library_mut) this latches nothing,
    /// because the router latches from the outcome the popup reports: every
    /// pointer sample over the open popup routes through here, and a sample
    /// that changes no pixel must repaint nothing.
    pub(crate) fn library_routing_mut(&mut self) -> &mut LibraryPopup {
        &mut self.library
    }

    /// Hand the popup the owner-resolved icon artwork for one of the rows it
    /// shows ([`LibraryPopup::set_row_artwork`]), for the session that
    /// resolved it from the entry's own bundle.
    ///
    /// Unlike [`library_mut`](Self::library_mut) this latches nothing, and
    /// deliberately so: the session resolves a shown row's icon immediately
    /// *before* painting a popup that some real change has already latched,
    /// so latching here would re-dirty the popup on every frame it is drawn
    /// and repaint it forever. The artwork is on the surface the same frame
    /// because it is set before the paint, not because it asked for another
    /// one.
    pub fn set_library_row_artwork(&mut self, row: usize, artwork: Option<Surface>) {
        self.library.set_row_artwork(row, artwork);
    }

    /// The application strip.
    #[must_use]
    pub const fn apps(&self) -> &AppStrip {
        &self.apps
    }

    /// The window picker, open over one application's windows or closed.
    #[must_use]
    pub const fn picker(&self) -> &WindowPicker {
        &self.picker
    }

    /// Replace the strip's resolved application slots — how the session
    /// hands the bar its running applications (their identities, artwork,
    /// windows, and declarations) whenever a process starts, exits, opens or
    /// closes a window, or re-declares its icon-bar presence.
    ///
    /// A picker open over an application the new set no longer has more than
    /// one window for is closed with it, so the bar can never show a picker
    /// for windows that are gone. The strip draws on the bar itself, so this
    /// latches [`bar`](TaskbarRepaint::bar) (and the picker when one closes).
    pub fn set_apps(&mut self, apps: Vec<AppSlot>) {
        let stale = self.picker.app().is_none_or(|index| {
            apps.get(index)
                .is_none_or(|app| app.windows().len() < crate::picker::PICKER_MIN_WINDOWS)
        });
        if stale {
            self.close_picker();
        }
        self.apps.set_apps(apps);
        self.repaint |= TaskbarRepaint::BAR;
    }

    /// The running-task list.
    #[must_use]
    pub const fn tasks(&self) -> &TaskList {
        &self.tasks
    }

    /// The running-task list, mutably — how the session adds, removes,
    /// focuses, and minimises task slots — latching the bar.
    ///
    /// Handing out a mutable view is indistinguishable from changing it, so
    /// the borrow itself latches; the bar cannot see what the caller does
    /// through it, and a missed latch leaves stale pixels. Task slots draw
    /// on the bar and nowhere else, so only the bar latches. Callers take
    /// this borrow to make a real state change — a window opened, closed, or
    /// focused — never once per input sample, so the conservative latch
    /// costs nothing on the hot path.
    pub fn tasks_mut(&mut self) -> &mut TaskList {
        self.repaint |= TaskbarRepaint::BAR;
        &mut self.tasks
    }

    /// The notification area.
    #[must_use]
    pub const fn notifications(&self) -> &NotificationArea {
        &self.notifications
    }

    /// Replace the notification area's status signals — how the session hands
    /// the bar its tray signals (network, volume, battery). The persistent
    /// signal glyphs draw on the bar itself, so this latches only
    /// [`bar`](TaskbarRepaint::bar).
    pub fn set_status_signals(&mut self, signals: Vec<StatusSignal>) {
        self.notifications.set_signals(signals);
        self.repaint |= TaskbarRepaint::BAR;
    }

    /// Raise (or update in place) a transient notification, latching a repaint
    /// when it changed the shown set — how the session relays a producer's
    /// raise over the notification IPC. A raise changes both the popover that
    /// shows the card and the bar's notification-area icon (the count it
    /// implies), so both latch together. Returns whether anything changed.
    pub fn raise_notification(&mut self, note: TransientNotification) -> bool {
        let changed = self.notifications.raise(note);
        if changed {
            self.repaint |= TaskbarRepaint::NOTIFICATIONS | TaskbarRepaint::BAR;
        }
        changed
    }

    /// Clear the transient notification identified by `(producer, key)`,
    /// latching a repaint when one was removed — how the session relays a
    /// producer's clear and resolves a user dismiss. A clear changes both the
    /// popover and the bar's notification-area icon, so both latch together.
    /// Returns whether one was removed.
    pub fn clear_notification(&mut self, producer: u64, key: u32) -> bool {
        let changed = self.notifications.clear(producer, key);
        if changed {
            self.repaint |= TaskbarRepaint::NOTIFICATIONS | TaskbarRepaint::BAR;
        }
        changed
    }

    /// Clear every transient notification raised by `producer`, latching a
    /// repaint when any were removed — how the session drops a dead
    /// producer's notifications when it exits. A clear changes both the
    /// popover and the bar's notification-area icon, so both latch together.
    /// Returns whether any were removed.
    pub fn clear_producer_notifications(&mut self, producer: u64) -> bool {
        let changed = self.notifications.clear_producer(producer);
        if changed {
            self.repaint |= TaskbarRepaint::NOTIFICATIONS | TaskbarRepaint::BAR;
        }
        changed
    }

    /// The clock.
    #[must_use]
    pub const fn clock(&self) -> &Clock {
        &self.clock
    }

    /// The clock, mutably, so the caller can update its label — latching the
    /// bar.
    ///
    /// Handing out a mutable view is indistinguishable from changing it, so
    /// the borrow itself latches; the bar cannot see what the caller does
    /// through it, and a missed latch leaves stale pixels. The clock draws
    /// on the bar and nowhere else, so only the bar latches. Callers take
    /// this borrow to make a real state change — the minute advancing —
    /// never once per input sample, so the conservative latch costs nothing
    /// on the hot path.
    pub fn clock_mut(&mut self) -> &mut Clock {
        self.repaint |= TaskbarRepaint::BAR;
        &mut self.clock
    }

    /// The Switchboard tray capsule.
    #[must_use]
    pub const fn tray(&self) -> &SwitchboardTray {
        &self.tray
    }

    /// Adopt the latest Switchboard tray summary — or its absence, when the
    /// service is gone — latching whichever of the capsule's two surfaces the
    /// new reading actually redraws. This is how the session relays the
    /// summary the Switchboard service publishes.
    ///
    /// Returns whether anything latched, so an embedder re-presents only when
    /// there is something new to show: the service publishes on a cadence,
    /// and most readings move nothing the bar draws.
    pub fn set_tray_summary(&mut self, summary: Option<TraySummary>) -> bool {
        let parts = self.tray.set_summary(summary);
        self.repaint |= parts;
        parts.any()
    }

    /// Adopt the session's count of unresponsive applications, latching a
    /// repaint on the surfaces it redraws. See
    /// [`set_tray_summary`](Self::set_tray_summary) for what is returned.
    pub fn set_tray_unresponsive(&mut self, count: u16) -> bool {
        let parts = self.tray.set_unresponsive(count);
        self.repaint |= parts;
        parts.any()
    }

    /// Adopt the session's attestation that its console has a
    /// re-authentication broker, so every row that needs one is offered
    /// only where choosing it could really act.
    ///
    /// One fact, two rows: the system menu's *Lock Screen* row needs the
    /// broker to re-verify the signed-in user before the screen unlocks
    /// again, and the clock menu's set-time row needs it to authenticate an
    /// account holding `CAP_TIME_SET`. The bar cannot know it — whether the
    /// broker exists is a property of the session's console, which the
    /// session reads from the kernel — so it defaults to refusing: a bar
    /// that was never told offers neither a lock with no way back nor a
    /// clock command that could only fail. Only a menu renders it, and a
    /// menu is built at the moment it is asked for, so nothing on the bar
    /// changes with it.
    pub fn set_elevation_available(&mut self, available: bool) {
        self.elevation_available = available;
    }

    /// Adopt the session's attestation that it can step aside for another
    /// user, so the *Switch User…* row exists only where switching really
    /// works.
    ///
    /// The bar cannot know this either: it depends on the session holding
    /// the mailbox an authority resumes it through, which only the session
    /// can establish. It defaults to refusing, so a bar that was never told
    /// offers no switch at all rather than one that would strand the user
    /// on a login screen with no way back. Only the system menu renders it,
    /// and it is built at the moment it is asked for, so nothing on the bar
    /// changes with it.
    pub fn set_switch_user_available(&mut self, available: bool) {
        self.switch_user_available = available;
    }

    /// Feed a primary press or release `event` to the Switchboard readout's
    /// "Open Switchboard" safe action, latching a repaint when the
    /// capsule's visual state changed. Returns the action the readout's
    /// control reports, if the click completed on it.
    ///
    /// This only ever fires for an event that already landed inside the open
    /// readout, so both it and the bar's capsule — which shares the same
    /// underlying control state — latch together.
    pub(crate) fn tray_pointer(
        &mut self,
        event: &InputEvent,
        scale: Scale,
        damage: &mut Region,
    ) -> Option<TraySignalAction> {
        let capsule = self.layout(scale).switchboard;
        let readout = self
            .tray_readout_layout(scale)
            .map_or(Rect::EMPTY, |readout| readout.panel);
        let (changed, action) =
            self.tray
                .on_pointer(event, capsule, readout, scale, &self.theme, damage);
        if changed {
            self.repaint |= TaskbarRepaint::BAR | TaskbarRepaint::READOUT;
        }
        action
    }

    /// Adopt a new theme, ground and all (see [`theme`](Self::theme)). The rest
    /// of the taskbar's state is unchanged, so a runtime dark/light switch needs
    /// no relayout of the model. Every surface draws from the theme's palette,
    /// so every surface repaints.
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.repaint = TaskbarRepaint::ALL;
    }

    /// Reposition or resize the bar. Every surface is laid out from the
    /// config (the popups and menu anchor off the bar's own computed
    /// geometry), so every surface repaints.
    pub fn set_config(&mut self, config: TaskbarConfig) {
        self.config = config;
        self.repaint = TaskbarRepaint::ALL;
    }

    /// Take the repaint latch: which of the bar's five rendered surfaces
    /// (the bar strip, the library popup, the hover window picker, the
    /// notification popover, and the Switchboard readout) changed since the
    /// last take, so the embedder re-presents exactly those and none of the
    /// rest. Reading it clears it.
    ///
    /// The contract every mutator on this type upholds: a change that alters
    /// what a surface draws latches that surface, and a change touching more
    /// than one latches all of them. An embedder may therefore present
    /// strictly from the drained latch — and present nothing at all when it
    /// is empty. Every site in this crate that sets the latch is reviewed to
    /// err toward latching a surface it merely might affect rather than
    /// omitting one it does: a missed latch leaves stale pixels on screen,
    /// which is a correctness bug, while an extra latch only costs a
    /// redundant repaint.
    ///
    /// The `&mut` accessors ([`tasks_mut`](Self::tasks_mut),
    /// [`library_mut`](Self::library_mut), [`clock_mut`](Self::clock_mut))
    /// are no exception: the bar cannot see into a borrow, so each latches
    /// its surfaces the moment it hands one out, whether or not the caller
    /// goes on to change anything.
    ///
    /// One thing stays the caller's to present: the desktop [`Scale`], which
    /// the compositor supplies per layout and render call rather than
    /// storing here, so a scale change is the caller's own and it knows it
    /// dirtied everything.
    #[must_use]
    pub fn take_repaint(&mut self) -> TaskbarRepaint {
        core::mem::take(&mut self.repaint)
    }

    /// Latch `parts` (see [`take_repaint`](Self::take_repaint)).
    pub(crate) fn request_repaint(&mut self, parts: TaskbarRepaint) {
        self.repaint |= parts;
    }

    /// Latch the surfaces that draw an application's own picture, for an
    /// embedder whose icon artwork arrived after they were last painted.
    ///
    /// The bar (its application slots) and the library popup (its rows) are
    /// the two that do; the hover window picker, the notification popover, and
    /// the instrument readout draw no application artwork at all, so a decode
    /// landing must not cost their pixels. Which surfaces those are
    /// is the bar's own knowledge, so it says so here rather than an embedder
    /// guessing at a flag set.
    pub fn request_icon_repaint(&mut self) {
        self.request_repaint(TaskbarRepaint::BAR | TaskbarRepaint::LIBRARY);
    }

    /// Compute the bar's geometry for its current application and icon
    /// counts at the desktop `scale` (the compositor's output density).
    #[must_use]
    pub fn layout(&self, scale: Scale) -> BarLayout {
        BarLayout::compute(
            &self.config,
            &self.theme,
            scale,
            self.apps.len(),
            self.notifications.signal_count(),
        )
    }

    /// Compute the open window picker's geometry, or `None` while it is
    /// closed (nothing to present).
    #[must_use]
    pub fn picker_layout(&self, scale: Scale) -> Option<PickerLayout> {
        self.picker.layout(
            self.config.edge,
            self.config.screen_width,
            self.config.screen_height,
            scale,
            &self.theme,
        )
    }

    /// The taskbar element under `point` at the desktop `scale`, or `None` if
    /// the point misses every region.
    #[must_use]
    pub fn hit_test(&self, point: Point, scale: Scale) -> Option<Hit> {
        self.layout(scale).hit_test(point)
    }

    /// Compute the program-library popup's geometry for the current state.
    ///
    /// The popup opens outward from the Library button on the bar's edge;
    /// the window manager places and rounds it exactly as it does the bar.
    /// Meaningful only while [`LibraryPopup::is_open`]; the caller checks
    /// that.
    #[must_use]
    pub fn library_layout(&self, scale: Scale) -> LibraryLayout {
        let bar = self.layout(scale);
        self.library.layout(
            self.config.edge,
            &bar,
            self.config.screen_width,
            self.config.screen_height,
            scale,
            &self.theme,
        )
    }

    /// Which catalogued applications the bar's surfaces will draw an icon for,
    /// and at what pixel side, so an embedder can have those decoded before the
    /// surface drawing them is shown.
    ///
    /// One want per (application, side) the bar would resolve if it were
    /// painted now: the launcher popup's first screenful of rows at the row
    /// side they draw at — the rows a user sees the instant the popup opens,
    /// which is the one bar surface whose first paint happens long after
    /// bring-up. The side comes from the same layout the paint uses, so the
    /// two cannot disagree and a warmed icon is never the wrong size.
    ///
    /// The bar's own furniture is deliberately absent: its buttons,
    /// application slots, and tray are painted with the bar itself at
    /// bring-up, so their icons are already being resolved before a user has
    /// anything to look at. What needs asking for early is what a *later*
    /// gesture reveals.
    ///
    /// Naming the set is the bar's own knowledge, so it says so here rather
    /// than an embedder guessing which of its surfaces draw what.
    #[must_use]
    pub fn catalog_icon_wants(&self, scale: Scale) -> Vec<LibraryIconRequest> {
        self.library
            .visible_icon_requests(&self.library_layout(scale), scale, &self.theme)
    }

    /// Compute the notification popover's geometry for the current
    /// notifications, or `None` when none are raised (nothing to present).
    ///
    /// The popover opens outward from the notification/clock region on the
    /// bar's edge; the window manager places and rounds it exactly as it does
    /// the bar and the library popup. Meaningful only while
    /// [`NotificationArea::has_notifications`] holds, which this checks.
    #[must_use]
    pub fn notifications_layout(&self, scale: Scale) -> Option<NotificationsLayout> {
        if !self.notifications.has_notifications() {
            return None;
        }
        let bar = self.layout(scale);
        Some(NotificationsLayout::compute(
            self.config.edge,
            &bar,
            self.config.screen_width,
            self.config.screen_height,
            scale,
            &self.theme,
            self.notifications.notification_count(),
        ))
    }

    /// Compute the Switchboard readout's geometry, or `None` while the
    /// readout is collapsed (no hover, no keyboard focus) or the capsule's
    /// slot has no
    /// room on a degenerate bar.
    ///
    /// The readout opens outward from the Switchboard slot on the bar's
    /// edge; the window manager places it and rounds it with
    /// [`TrayReadoutLayout::corner_radius`], exactly as it does the bar's
    /// other popovers.
    #[must_use]
    pub fn tray_readout_layout(&self, scale: Scale) -> Option<TrayReadoutLayout> {
        if !self.tray.is_expanded() {
            return None;
        }
        let bar = self.layout(scale);
        if bar.switchboard.is_empty() {
            return None;
        }
        Some(TrayReadoutLayout::compute(
            self.config.edge,
            &bar,
            self.config.screen_width,
            self.config.screen_height,
            scale,
            &self.theme,
            self.tray.signal(),
        ))
    }

    /// The pixel side an application slot's icon paints at in a slot of
    /// `logical_extent`, asked of the control that will paint it so the
    /// answer can never drift from the drawn geometry.
    #[must_use]
    fn slot_icon_side(&self, logical_extent: u32, scale: Scale) -> u32 {
        let scaled = self.config.scaled(scale);
        let bounds = Rect::new(
            0,
            0,
            scale.scale_length(logical_extent).max(1),
            scaled.thickness.max(1),
        );
        TaskbarItem::new(IconKind::AppBundle).icon_side(bounds, scale, &self.theme)
    }

    /// The pixel side a running application's icon paints at, at the
    /// desktop `scale` — the session rasterises per-application artwork at
    /// exactly this size, through the same control geometry the renderer
    /// paints with, so the artwork and the slot can never disagree.
    #[must_use]
    pub fn app_icon_side(&self, scale: Scale) -> u32 {
        self.slot_icon_side(self.config.app_extent, scale)
    }

    /// The pixel size the session rasterises a window's frame to for one
    /// hover-picker cell, at the desktop `scale`.
    ///
    /// Asked of the control that will paint the cell, so the thumbnail the
    /// session scales and the rectangle the cell blits it into can never
    /// disagree.
    #[must_use]
    pub fn picker_thumbnail_size(&self, scale: Scale) -> (u32, u32) {
        crate::picker::thumbnail_size(scale, &self.theme)
    }

    /// The chain the desktop should open for the menu the application at
    /// `index` declared, anchored at its slot.
    ///
    /// An application that declared no menu asks for nothing at all — a
    /// secondary press on its slot is simply claimed — so the bar never shows
    /// an empty plate on the application's behalf. The plate is titled from
    /// the bundle's **signed** manifest, so a menu cannot be titled as an
    /// application it is not.
    pub(crate) fn app_menu(&self, index: usize, anchor: Rect, scale: Scale) -> Option<MenuRequest> {
        let app = self.apps.get(index)?;
        if app.menu().is_empty() {
            return None;
        }
        let identity = app.identity();
        Some(MenuRequest {
            subject: MenuSubject::App { app: index },
            model: menu::app_menu(&identity.name, app.menu(), identity),
            placement: self.menu_placement(anchor, scale),
        })
    }

    /// Open the window picker over the application at `index`, anchored at
    /// its slot, offering `entries` (one per window, in open order).
    ///
    /// Refused for an application with fewer than
    /// [`PICKER_MIN_WINDOWS`](crate::picker::PICKER_MIN_WINDOWS) windows:
    /// with one window there is nothing to choose. Returns whether the
    /// picker is now open.
    pub(crate) fn open_picker(
        &mut self,
        index: usize,
        anchor: Rect,
        entries: Vec<PickerEntry>,
    ) -> bool {
        let Some(app) = self.apps.get(index) else {
            return false;
        };
        let icon = app.icon();
        if self.picker.open(index, anchor, icon, entries) {
            self.repaint |= TaskbarRepaint::PICKER;
            return true;
        }
        false
    }

    /// Show `thumbnail` in the open picker's cell at `index`, latching the
    /// picker's own repaint when it landed.
    ///
    /// The embedder scales a window's frame one per turn of its serve loop,
    /// so a picker that opened before every thumbnail was ready fills in as
    /// they arrive rather than blocking on all of them.
    pub fn set_picker_thumbnail(&mut self, index: usize, thumbnail: Surface) -> bool {
        if self.picker.set_thumbnail(index, thumbnail) {
            self.repaint |= TaskbarRepaint::PICKER;
            return true;
        }
        false
    }

    /// Feed one pointer event to the open picker's grid scrollbar, latching
    /// the picker's repaint when it moved. `false` while the picker is
    /// closed or its grid needs no scrolling.
    pub(crate) fn scroll_picker(
        &mut self,
        event: &InputEvent,
        pointer: Point,
        scale: Scale,
        damage: &mut Region,
    ) -> bool {
        let Some(layout) = self.picker_layout(scale) else {
            return false;
        };
        if self
            .picker
            .on_pointer(event, &layout, pointer, (scale, &self.theme), damage)
        {
            self.repaint |= TaskbarRepaint::PICKER;
            return true;
        }
        false
    }

    /// Track the hovered picker cell, latching the picker's own repaint when
    /// the highlight moves.
    pub(crate) fn track_picker_hover(&mut self, cell: Option<usize>) {
        if self.picker.set_hover(cell) {
            self.repaint |= TaskbarRepaint::PICKER;
        }
    }

    /// Show the window picker over the application at `app`, with one cell
    /// per window as the embedder resolved them.
    ///
    /// The answer to [`TaskbarResponse::ShowWindowPicker`]: the embedder owns
    /// the windows' pixels, so it builds the cells and the bar places and
    /// draws them. Refused, changing nothing, for an unknown application or
    /// for fewer cells than there is a choice between.
    ///
    /// [`TaskbarResponse::ShowWindowPicker`]: crate::TaskbarResponse::ShowWindowPicker
    pub fn show_window_picker(&mut self, app: usize, entries: Vec<PickerEntry>, scale: Scale) {
        let anchor = self
            .layout(scale)
            .apps
            .get(app)
            .copied()
            .unwrap_or(Rect::EMPTY);
        self.open_picker(app, anchor, entries);
    }

    /// Close the window picker, latching a repaint if it had been open.
    pub(crate) fn close_picker(&mut self) -> bool {
        if self.picker.close() {
            self.repaint |= TaskbarRepaint::PICKER;
            return true;
        }
        false
    }

    /// The chain the desktop should open for a program-library entry row,
    /// anchored at that row.
    pub(crate) fn entry_menu(&self, entry: EntryId, anchor: Rect, scale: Scale) -> MenuRequest {
        MenuRequest {
            subject: MenuSubject::Entry { entry },
            model: menu::entry_menu(),
            placement: self.menu_placement(anchor, scale),
        }
    }

    /// The chain the desktop should open for the system quick actions,
    /// anchored at the Switchboard capsule's slot.
    ///
    /// The rows' postures are read from what the bar already knows: the
    /// appearance it is painting with, whether the publishing service
    /// attested that it can power the machine, whether the terminal bundle
    /// is in the catalog the session handed it, and whether the session
    /// attested that it can prompt for this user's password. None of that
    /// is authority the bar holds — it renders what it was told, and every
    /// unknown reads as refused.
    pub(crate) fn system_menu(&self, anchor: Rect, scale: Scale) -> MenuRequest {
        let permits = SystemPermits {
            appearance: self.theme.appearance(),
            power: self.tray.power_capable(),
            task_shell_installed: EntryId::new(system::TASK_SHELL_BUNDLE)
                .is_ok_and(|id| self.library.catalog().entry(&id).is_some()),
            lock_available: self.elevation_available,
            switch_user_available: self.switch_user_available,
        };
        MenuRequest {
            subject: MenuSubject::System,
            model: menu::system_menu(permits),
            placement: self.menu_placement(anchor, scale),
        }
    }

    /// The chain the desktop should open for the clock, anchored at it.
    ///
    /// The reading it states is the label the bar is already drawing, so the
    /// menu and the bar can never disagree about the time, and an unset
    /// clock says so rather than showing a fabricated one. Setting a clock
    /// needs a capability the bar does not hold, so the command is offered
    /// only where the session attested a broker to authenticate against.
    pub(crate) fn clock_menu(&self, anchor: Rect, scale: Scale) -> MenuRequest {
        let permits = ClockPermits {
            reading: String::from(self.clock.label()),
            set_available: self.elevation_available,
        };
        MenuRequest {
            subject: MenuSubject::Clock,
            model: menu::clock_menu(&permits),
            placement: self.menu_placement(anchor, scale),
        }
    }

    /// Where a menu anchored at `anchor` opens: away from the bar's own edge,
    /// clear of it by the shared control gap.
    fn menu_placement(&self, anchor: Rect, scale: Scale) -> PlatePlacement {
        menu::placement(
            anchor,
            self.config.edge,
            scale.scale_length(self.theme.metrics().control_gap),
        )
    }

    /// What choosing the row `item` of the bar's open `subject` asks the
    /// embedder for, closing the program-library popup where the chosen row
    /// acts somewhere the popup would stand in front of.
    ///
    /// The bar's half of the desktop's one menu answer: the chain places,
    /// draws and dismisses, and this reads the chosen row back through the
    /// same table the plate was built from.
    pub fn menu_chosen(
        &mut self,
        subject: &MenuSubject,
        item: AppMenuItemId,
    ) -> Option<TaskbarResponse> {
        let response = subject.chosen(item)?;
        if subject.closes_library() {
            self.close_library();
        }
        Some(response)
    }

    /// Open the program-library popup (fresh: search cleared, folders
    /// expanded, cursor at the top) and press the Library button in. This
    /// changes both the popup itself and the bar (the Library button reads
    /// as visually held open), so both latch.
    pub(crate) fn open_library(&mut self) {
        self.library.open();
        self.library_button.set_state(
            self.library_button
                .state()
                .with_pointer(PointerState::Pressed),
        );
        self.repaint |= TaskbarRepaint::LIBRARY | TaskbarRepaint::BAR;
    }

    /// Close the program-library popup and release the Library button. Both
    /// the popup and the bar's now-released Library button change, so both
    /// latch.
    pub(crate) fn close_library(&mut self) {
        self.library.close();
        self.library_button
            .set_state(self.library_button.state().with_pointer(PointerState::None));
        self.repaint |= TaskbarRepaint::LIBRARY | TaskbarRepaint::BAR;
    }

    /// Track the pointer for the bar's hover feedback — the leading
    /// launcher, the application slots, and the Switchboard capsule
    /// (whose readout expands on hover) — latching a repaint when any visual
    /// state changes.
    ///
    /// `point` is `None` when the pointer does not rest on the bar **at all**,
    /// which is a different fact from a position that misses every region: a
    /// window drawn over the bar takes the pointer with it while leaving it at
    /// the bar's own coordinates, so there is a position and it is not the
    /// bar's. Only the desktop's seat can tell the two apart (it owns the
    /// window stack), so it says which this is and the bar does not guess.
    /// Either way every hover here ends the same way — nothing on the bar is
    /// under the pointer — so one routine answers both and they cannot drift
    /// apart.
    ///
    /// While the popup is open the Library button stays visually pressed (it
    /// is "held open"). Every hover target tracked here paints on the bar
    /// itself, so a change latches only [`bar`](TaskbarRepaint::bar) — except
    /// the Switchboard capsule, whose hover can also expand or collapse its
    /// readout.
    pub(crate) fn track_hover(&mut self, point: Option<Point>, scale: Scale, damage: &mut Region) {
        let layout = self.layout(scale);
        let over = |rect: Rect| point.is_some_and(|at| rect.contains(at));
        let library_pointer = if self.library.is_open() {
            PointerState::Pressed
        } else if over(layout.library) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        let mut bar_changed = set_pointer(&mut self.library_button, library_pointer);
        let app_hover = point.and_then(|at| layout.apps.iter().position(|slot| slot.contains(at)));
        bar_changed |= self.apps.set_hover(app_hover);
        if bar_changed {
            self.repaint |= TaskbarRepaint::BAR;
        }

        let was_expanded = self.tray.is_expanded();
        let readout = self
            .tray_readout_layout(scale)
            .map_or(Rect::EMPTY, |readout| readout.panel);
        // The capsule's expansion rule is the shared control's, so both
        // directions are asked of it rather than re-derived here.
        let tray_changed = match point {
            Some(at) => {
                self.tray
                    .track(at, layout.switchboard, readout, scale, &self.theme, damage)
            }
            None => self.tray.pointer_left(layout.switchboard, readout, damage),
        };
        if tray_changed {
            self.repaint |= TaskbarRepaint::BAR;
            if was_expanded || self.tray.is_expanded() {
                self.repaint |= TaskbarRepaint::READOUT;
            }
        }
    }
}

/// Set `button`'s pointer state, reporting whether it changed.
fn set_pointer(button: &mut IconButton, pointer: PointerState) -> bool {
    let state = button.state();
    if state.pointer == pointer {
        return false;
    }
    button.set_state(state.with_pointer(pointer));
    true
}
