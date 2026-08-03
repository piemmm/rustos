//! The taskbar itself: its placement configuration and live state.
//!
//! [`Taskbar`] ties the two permanent leading launcher buttons (Library and
//! Files), the [`LibraryPopup`], the [`TaskList`], the [`NotificationArea`],
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

use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::TraySummary;
use tairix_controls::{
    ControlRole, IconButton, PlateSeating, PointerState, TaskbarItem, TaskbarPresentation,
    TraySignalAction,
};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::InputEvent;
use tairix_proglib::EntryId;
use tairix_raster::Surface;
use tairix_theme::{TextRole, Theme};

use crate::clock::Clock;
use crate::edge::Edge;
use crate::layout::{BarLayout, Hit, NotificationsLayout, TrayReadoutLayout};
use crate::library::{LibraryLayout, LibraryPopup};
use crate::menu::{BarMenu, MenuLayout, MenuSubject};
use crate::notifications::{NotificationArea, StatusSignal, TransientNotification};
use crate::pins::{PinStrip, PinView};
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
    /// Main-axis length of each leading launcher button (Library, Files).
    pub launcher_extent: u32,
    /// Main-axis length of each pinned-shortcut slot.
    pub pin_extent: u32,
    /// Main-axis length of each task slot.
    pub task_extent: u32,
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
            pin_extent: scale.scale_length(self.pin_extent),
            task_extent: scale.scale_length(self.task_extent),
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
            pin_extent: 48,
            task_extent: 160,
            icon_extent: 24,
            clock_extent: 80,
            switch_extent: 44,
        }
    }
}

/// The taskbar: placement configuration plus the leading launcher buttons,
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
    files_button: IconButton,
    library: LibraryPopup,
    pins: PinStrip,
    menu: BarMenu,
    tasks: TaskList,
    task_hover: Option<usize>,
    notifications: NotificationArea,
    clock: Clock,
    tray: SwitchboardTray,
    lock_available: bool,
    repaint: TaskbarRepaint,
}

impl Taskbar {
    /// Build a taskbar for `config`, adopting `theme` as its active theme.
    ///
    /// The two permanent leading buttons are seeded here: the Library button
    /// and the Files button. They are fixed: nothing can reorder or remove
    /// them.
    ///
    /// Both are ordinary quiet peers seated *in* the bar
    /// ([`PlateSeating::Bar`]): on an icon strip no single icon is the primary
    /// action of the surface, so neither wears a role fill or a perimeter of
    /// its own. They rest as bare glyphs on the bar, wash lighter under the
    /// pointer, and — for the Library button — read as held down while its
    /// popup is open.
    #[must_use]
    pub fn new(config: TaskbarConfig, theme: &Theme) -> Self {
        Self {
            config,
            theme: theme.clone(),
            library_button: IconButton::new(IconKind::Library, ControlRole::Neutral)
                .seated(PlateSeating::Bar),
            files_button: IconButton::new(IconKind::Folder, ControlRole::Neutral)
                .seated(PlateSeating::Bar),
            library: LibraryPopup::new(),
            pins: PinStrip::new(),
            menu: BarMenu::new(),
            tasks: TaskList::new(),
            task_hover: None,
            notifications: NotificationArea::new(),
            clock: Clock::new(),
            tray: SwitchboardTray::new(),
            lock_available: false,
            repaint: TaskbarRepaint::NONE,
        }
    }

    /// The placement configuration.
    #[must_use]
    pub const fn config(&self) -> &TaskbarConfig {
        &self.config
    }

    /// The active theme the bar lays out and paints with.
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

    /// The Files launcher button, for painting.
    #[must_use]
    pub const fn files_button(&self) -> &IconButton {
        &self.files_button
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

    /// The pin strip.
    #[must_use]
    pub const fn pins(&self) -> &PinStrip {
        &self.pins
    }

    /// Replace the pin strip's resolved views — how the session hands the
    /// bar its pins (and their running-window matches) whenever the store,
    /// the catalog, or the launch table changes. The pin strip draws on the
    /// bar itself, so this latches only [`bar`](TaskbarRepaint::bar).
    pub fn set_pins(&mut self, pins: Vec<PinView>) {
        self.pins.set_pins(pins);
        self.repaint |= TaskbarRepaint::BAR;
    }

    /// The bar's context menu (open or closed).
    #[must_use]
    pub const fn menu(&self) -> &BarMenu {
        &self.menu
    }

    /// The bar's context menu, mutably, for routing one input event into it.
    ///
    /// This latches nothing: the router latches from the outcome the menu
    /// reports, and every pointer sample over the open menu routes through
    /// here, so a sample that changes no pixel must repaint nothing.
    pub(crate) fn menu_routing_mut(&mut self) -> &mut BarMenu {
        &mut self.menu
    }

    /// The hovered task-slot index, if the pointer rests on one.
    #[must_use]
    pub const fn task_hover(&self) -> Option<usize> {
        self.task_hover
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
    /// service is gone — latching a repaint when the capsule changed. This is
    /// how the session relays the summary the Switchboard service publishes.
    /// The capsule's badge and label live on the bar; while its readout is
    /// open the same derived state feeds the readout's value line too, so
    /// the readout latches alongside the bar exactly then.
    pub fn set_tray_summary(&mut self, summary: Option<TraySummary>) {
        if self.tray.set_summary(summary) {
            self.latch_tray();
        }
    }

    /// Adopt the session's count of unresponsive applications, latching a
    /// repaint when the capsule changed. See
    /// [`set_tray_summary`](Self::set_tray_summary) for which surfaces latch.
    pub fn set_tray_unresponsive(&mut self, count: u16) {
        if self.tray.set_unresponsive(count) {
            self.latch_tray();
        }
    }

    /// Adopt the session's attestation that it can put a password prompt in
    /// front of the screen, so the *Lock Screen* row is offered only when
    /// choosing it could really lock — and really unlock again.
    ///
    /// The bar cannot know this: whether a re-verification prompt exists is
    /// a property of the session's console, which the session reads from
    /// the kernel. It defaults to refusing, so a bar that was never told
    /// offers no lock rather than a lock with no way back. Only the system
    /// menu renders it, and only while open, so a change latches that
    /// surface alone.
    pub fn set_lock_available(&mut self, available: bool) {
        if self.lock_available != available {
            self.lock_available = available;
            self.repaint |= TaskbarRepaint::MENU;
        }
    }

    /// Latch the surfaces a tray-capsule change alters: the bar always (the
    /// capsule's badge, label, and furniture live there), and the readout
    /// too while it is currently expanded, since it renders the same derived
    /// state.
    fn latch_tray(&mut self) {
        self.repaint |= TaskbarRepaint::BAR;
        if self.tray.is_expanded() {
            self.repaint |= TaskbarRepaint::READOUT;
        }
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
    ) -> Option<TraySignalAction> {
        let capsule = self.layout(scale).switchboard;
        let readout = self
            .tray_readout_layout(scale)
            .map_or(Rect::EMPTY, |readout| readout.panel);
        let (changed, action) = self
            .tray
            .on_pointer(event, capsule, readout, scale, &self.theme);
        if changed {
            self.repaint |= TaskbarRepaint::BAR | TaskbarRepaint::READOUT;
        }
        action
    }

    /// Adopt a new theme. The rest of the taskbar's state is unchanged, so a
    /// runtime dark/light switch needs no relayout of the model. Every
    /// surface draws from the theme's palette, so every surface repaints.
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
    /// (the bar strip, the library popup, the context menu, the notification
    /// popover, and the Switchboard readout) changed since the last take, so
    /// the embedder re-presents exactly those and none of the rest. Reading
    /// it clears it.
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

    /// Compute the bar's geometry for its current pin, task, and icon counts
    /// at the desktop `scale` (the compositor's output density).
    #[must_use]
    pub fn layout(&self, scale: Scale) -> BarLayout {
        BarLayout::compute(
            &self.config,
            self.theme.metrics().taskbar_corner_radius,
            scale,
            self.pins.len(),
            self.tasks.len(),
            self.notifications.signal_count(),
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
    /// readout is collapsed (no hover, no pin) or the capsule's slot has no
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

    /// Compute the context menu's geometry, or `None` while it is closed.
    ///
    /// The row text is measured with the same body font the renderer paints
    /// with (one role, one scale), so the width the layout reserves and the
    /// width the paint fills can never disagree.
    #[must_use]
    pub fn menu_layout(&self, scale: Scale) -> Option<MenuLayout> {
        let font = BitmapFont::for_role(self.theme.fonts(), TextRole::Body, scale);
        self.menu.layout(
            self.config.edge,
            self.config.screen_width,
            self.config.screen_height,
            scale,
            &self.theme,
            font,
        )
    }

    /// The pixel side a pinned shortcut's icon paints at, at the desktop
    /// `scale` — the session rasterises per-application artwork at exactly
    /// this size, through the same control geometry the renderer paints
    /// with, so the artwork and the slot can never disagree.
    #[must_use]
    pub fn pin_icon_side(&self, scale: Scale) -> u32 {
        let scaled = self.config.scaled(scale);
        let bounds = Rect::new(0, 0, scaled.pin_extent.max(1), scaled.thickness.max(1));
        let font = BitmapFont::for_role(self.theme.fonts(), TextRole::Body, scale);
        TaskbarItem::new("", IconKind::AppBundle)
            .with_presentation(TaskbarPresentation::Icon)
            .icon_side(bounds, scale, &self.theme, font)
    }

    /// Open the context menu for the pin at `index`, anchored at its slot.
    /// The menu is its own overlay surface, so this latches only
    /// [`menu`](TaskbarRepaint::menu).
    pub(crate) fn open_pin_menu(&mut self, index: usize, anchor: Rect) {
        let running = self
            .pins
            .get(index)
            .and_then(PinView::window)
            .is_some_and(|id| self.tasks.entries().iter().any(|entry| entry.id == id));
        self.menu.open(MenuSubject::Pin { index, running }, anchor);
        self.repaint |= TaskbarRepaint::MENU;
    }

    /// Open the context menu for a program-library entry row, anchored at
    /// that row. Latches only [`menu`](TaskbarRepaint::menu): the popup
    /// beneath is unaffected by an overlay opening on top of it.
    pub(crate) fn open_entry_menu(&mut self, entry: EntryId, anchor: Rect) {
        let pinned = self.pins.position_of_entry(&entry);
        self.menu.open(MenuSubject::Entry { entry, pinned }, anchor);
        self.repaint |= TaskbarRepaint::MENU;
    }

    /// Open the desktop's system quick-actions menu, anchored at the
    /// Switchboard capsule's slot. Latches only
    /// [`menu`](TaskbarRepaint::menu).
    ///
    /// The rows' postures are read from what the bar already knows: the
    /// appearance it is painting with, whether the publishing service
    /// attested that it can power the machine, whether the terminal bundle
    /// is in the catalog the session handed it, and whether the session
    /// attested that it can prompt for this user's password. None of that
    /// is authority the bar holds — it renders what it was told, and every
    /// unknown reads as refused.
    pub(crate) fn open_system_menu(&mut self, anchor: Rect) {
        let permits = SystemPermits {
            appearance: self.theme.appearance(),
            power: self.tray.power_capable(),
            task_shell_installed: EntryId::new(system::TASK_SHELL_BUNDLE)
                .is_ok_and(|id| self.library.catalog().entry(&id).is_some()),
            lock_available: self.lock_available,
        };
        self.menu.open(MenuSubject::System { permits }, anchor);
        self.repaint |= TaskbarRepaint::MENU;
    }

    /// Close the context menu without acting. Latches only
    /// [`menu`](TaskbarRepaint::menu).
    pub(crate) fn close_menu(&mut self) {
        self.menu.close();
        self.repaint |= TaskbarRepaint::MENU;
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
    /// buttons, the pin slots, the task slots, and the Switchboard capsule
    /// (whose readout expands on hover) — latching a repaint when any visual
    /// state changes.
    ///
    /// While the popup is open the Library button stays visually pressed
    /// (it is "held open"); the Files button hovers as normal. Every hover
    /// target tracked here paints on the bar itself, so a change latches
    /// only [`bar`](TaskbarRepaint::bar) — except the Switchboard capsule,
    /// whose hover can also expand or collapse its readout.
    pub(crate) fn track_hover(&mut self, point: Point, scale: Scale) {
        let layout = self.layout(scale);
        let library_pointer = if self.library.is_open() {
            PointerState::Pressed
        } else if layout.library.contains(point) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        let files_pointer = if layout.files.contains(point) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        let mut bar_changed = set_pointer(&mut self.library_button, library_pointer);
        bar_changed |= set_pointer(&mut self.files_button, files_pointer);
        let pin_hover = layout.pins.iter().position(|slot| slot.contains(point));
        bar_changed |= self.pins.set_hover(pin_hover);
        let task_hover = layout.tasks.iter().position(|slot| slot.contains(point));
        if self.task_hover != task_hover {
            self.task_hover = task_hover;
            bar_changed = true;
        }
        if bar_changed {
            self.repaint |= TaskbarRepaint::BAR;
        }

        let was_expanded = self.tray.is_expanded();
        let readout = self
            .tray_readout_layout(scale)
            .map_or(Rect::EMPTY, |readout| readout.panel);
        let tray_changed = self
            .tray
            .track(point, layout.switchboard, readout, scale, &self.theme);
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
