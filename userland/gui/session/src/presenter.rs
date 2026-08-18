//! Presenting the taskbar, its program-library popup, its context menu, and
//! its popovers through the window manager.
//!
//! The taskbar models the desktop's bar and produces a *rectangular* pixel
//! [`Surface`], and the window manager composites and
//! rounds windows; neither knows about the other. Joining
//! them is session glue, and [`TaskbarPresenter`] is that join: it paints the
//! bar (and, while open, the program-library popup and the bar's context
//! menu) with the taskbar's own [`TaskbarRenderer`] and presents each as a
//! window in the [`Compositor`], placed at its computed screen origin
//! and rounded with the theme's corner radius through the compositor's single
//! anti-aliased rounded-corner path — the same path it uses for application
//! windows, never a second one.
//!
//! The presenter owns only the compositor [`WindowId`]
//! tokens it minted; the taskbar model, the renderer (which holds the
//! across-frame glyph cache), and the compositor are the embedder's, passed in
//! on each [`present`](TaskbarPresenter::present) so the session crate composes
//! the GUI crates without owning the window-manager handle (composing `userland/gui/*` crates is the permitted edge).
//!
//! It is **total and fails closed**: a render that cannot
//! allocate its surface leaves whatever is already on screen untouched rather
//! than blanking the bar or panicking, and the popup and menu windows are
//! removed from the compositor the moment each closes. If a presented window
//! has disappeared from the compositor (an embedder removed it), the next
//! present re-creates it rather than silently doing nothing.

use tairix_icon::IconArtwork;
use tairix_taskbar::{Taskbar, TaskbarRenderer, TaskbarRepaint};
use tairix_theme::Theme;
use tairix_wm::{Compositor, Corners, Point, Scale, Surface, WindowId};

/// Presents a taskbar, its program-library popup, its context menu, the
/// notification popover, and the Switchboard capsule's instrument readout as
/// window-manager windows.
///
/// Build one with [`TaskbarPresenter::new`], then call
/// [`present`](Self::present) whenever the taskbar's model or the active theme
/// changes, naming the surfaces that changed; the presenter creates the bar,
/// popup, and menu windows on first sight and keeps their surface, position,
/// and corner radius in step thereafter.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarPresenter {
    bar: Option<WindowId>,
    popup: Option<WindowId>,
    menu: Option<WindowId>,
    notifications: Option<WindowId>,
    readout: Option<WindowId>,
    /// The density the surfaces on screen were laid out at, so a runtime
    /// DPI change is caught even though it never touches the taskbar
    /// model that drives the repaint latch.
    presented_scale: Option<Scale>,
}

impl TaskbarPresenter {
    /// A presenter that has not yet placed any window.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bar: None,
            popup: None,
            menu: None,
            notifications: None,
            readout: None,
            presented_scale: None,
        }
    }

    /// The compositor window currently showing the bar, or `None` before the
    /// first successful [`present`](Self::present).
    #[must_use]
    pub const fn bar_window(&self) -> Option<WindowId> {
        self.bar
    }

    /// The compositor window currently showing the open program-library
    /// popup, or `None` when it is closed (or before its first presentation).
    #[must_use]
    pub const fn popup_window(&self) -> Option<WindowId> {
        self.popup
    }

    /// The compositor window currently showing the open context menu, or
    /// `None` when it is closed (or before its first presentation).
    #[must_use]
    pub const fn menu_window(&self) -> Option<WindowId> {
        self.menu
    }

    /// The compositor window currently showing the notification popover, or
    /// `None` when no notification is raised (or before its first
    /// presentation).
    #[must_use]
    pub const fn notifications_window(&self) -> Option<WindowId> {
        self.notifications
    }

    /// The compositor window currently showing the Switchboard capsule's
    /// instrument readout, or `None` while it is collapsed (or before its
    /// first presentation).
    #[must_use]
    pub const fn readout_window(&self) -> Option<WindowId> {
        self.readout
    }

    /// Bring the compositor up to date with the taskbar's current model and
    /// its owned theme, repainting only the surfaces `parts` names.
    ///
    /// The bar is repainted and re-placed at [`BarLayout::bar`]'s origin,
    /// rounded with [`BarLayout::corner_radius`]. If the program-library
    /// popup is open it is painted and presented above the bar at
    /// [`LibraryLayout::panel`]'s origin, rounded with
    /// [`LibraryLayout::corner_radius`]; when the popup is closed any popup
    /// window is removed.
    ///
    /// Repainting only the latched surfaces is what keeps a pointer moving
    /// over one small open menu cheap: re-rendering all five surfaces and
    /// pushing them back into the compositor costs milliseconds and damages
    /// five window rectangles, where the menu alone costs a fraction of
    /// that and damages one. A surface with no window yet is always
    /// presented, whatever the latch says, so the first paint after startup
    /// (or after a [`teardown`](Self::teardown)) puts everything on screen.
    /// A change of desktop density is the one input the taskbar's own latch
    /// cannot see — the scale belongs to the output, not the model — so a
    /// scale that differs from the last presented one repaints everything.
    ///
    /// `artwork` is the shipped raster-icon lookup the bar draws its
    /// launcher, pin, and task icons from; every slot falls back to its
    /// built-in glyph when the lookup has nothing, so a system with no
    /// installed graphics still presents a complete bar.
    ///
    /// Fails closed: a render whose surface cannot be
    /// allocated leaves the existing on-screen window untouched.
    ///
    /// [`BarLayout::bar`]: tairix_taskbar::BarLayout::bar
    /// [`BarLayout::corner_radius`]: tairix_taskbar::BarLayout::corner_radius
    /// [`LibraryLayout::panel`]: tairix_taskbar::LibraryLayout::panel
    /// [`LibraryLayout::corner_radius`]: tairix_taskbar::LibraryLayout::corner_radius
    pub fn present(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        parts: TaskbarRepaint,
        artwork: &mut dyn IconArtwork,
    ) {
        // The desktop density belongs to the output the bar is on, so it is
        // read from the compositor here and laid out at present time — a
        // runtime DPI change re-presents the bar at the new scale with no
        // taskbar state to update.
        let scale = compositor.scale();
        let parts = if self.presented_scale == Some(scale) {
            parts
        } else {
            TaskbarRepaint::ALL
        };
        self.presented_scale = Some(scale);
        if parts.bar || self.bar.is_none() {
            self.present_bar(compositor, renderer, taskbar, scale, artwork);
        }
        if parts.library || self.popup.is_none() {
            self.present_popup(compositor, renderer, taskbar, scale);
        }
        if parts.menu || self.menu.is_none() {
            self.present_menu(compositor, renderer, taskbar, scale);
        }
        if parts.notifications || self.notifications.is_none() {
            self.present_notifications(compositor, renderer, taskbar, scale);
        }
        if parts.readout || self.readout.is_none() {
            self.present_readout(compositor, renderer, taskbar, scale);
        }
    }

    /// Remove the bar, popup, menu, and popover windows from `compositor`
    /// and forget them, so a later [`present`](Self::present) starts fresh.
    /// Tearing the desktop session down leaves no orphaned windows behind.
    pub fn teardown(&mut self, compositor: &mut Compositor) {
        self.presented_scale = None;
        if let Some(id) = self.readout.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.notifications.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.menu.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.popup.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.bar.take() {
            compositor.remove(id);
        }
    }

    /// Repaint the bar and present it, leaving the existing window untouched if
    /// the surface cannot be allocated.
    fn present_bar(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
        artwork: &mut dyn IconArtwork,
    ) {
        let Some(surface) = renderer.render(taskbar, scale, artwork) else {
            return;
        };
        let layout = taskbar.layout(scale);
        let corners = Corners::from_radius(layout.corner_radius);
        self.bar = Some(place(
            compositor,
            self.bar,
            layout.bar.origin,
            surface,
            (corners, chrome_blur(taskbar.theme())),
        ));
    }

    /// Present the program-library popup while it is open, or remove it once
    /// closed.
    fn present_popup(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
    ) {
        if !taskbar.library().is_open() {
            if let Some(id) = self.popup.take() {
                compositor.remove(id);
            }
            return;
        }
        let Some(surface) = renderer.render_library(taskbar, scale) else {
            return;
        };
        let layout = taskbar.library_layout(scale);
        let corners = Corners::from_radius(layout.corner_radius);
        self.popup = Some(place(
            compositor,
            self.popup,
            layout.panel.origin,
            surface,
            (corners, chrome_blur(taskbar.theme())),
        ));
    }

    /// Present the context menu while it is open, or remove it once closed.
    /// It is presented after the popup, so an entry menu sits above the
    /// panel it was opened from.
    fn present_menu(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
    ) {
        let Some(layout) = taskbar.menu_layout(scale) else {
            if let Some(id) = self.menu.take() {
                compositor.remove(id);
            }
            return;
        };
        let Some(surface) = renderer.render_menu(taskbar, scale) else {
            return;
        };
        let corners = Corners::from_radius(layout.corner_radius);
        self.menu = Some(place(
            compositor,
            self.menu,
            layout.panel.origin,
            surface,
            (corners, chrome_blur(taskbar.theme())),
        ));
    }

    /// Present the notification popover while any notification is raised, or
    /// remove it once none remain.
    ///
    /// The popover opens outward from the notification/clock region at
    /// [`NotificationsLayout::panel`]'s origin, rounded with
    /// [`NotificationsLayout::corner_radius`]; when no notification is raised
    /// the popover window is removed. Fails closed like the others: a render
    /// whose surface cannot be allocated leaves the existing window untouched.
    ///
    /// [`NotificationsLayout::panel`]: tairix_taskbar::NotificationsLayout::panel
    /// [`NotificationsLayout::corner_radius`]: tairix_taskbar::NotificationsLayout::corner_radius
    fn present_notifications(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
    ) {
        let Some(layout) = taskbar.notifications_layout(scale) else {
            if let Some(id) = self.notifications.take() {
                compositor.remove(id);
            }
            return;
        };
        let Some(surface) = renderer.render_notifications(taskbar, scale) else {
            return;
        };
        let corners = Corners::from_radius(layout.corner_radius);
        self.notifications = Some(place(
            compositor,
            self.notifications,
            layout.panel.origin,
            surface,
            (corners, chrome_blur(taskbar.theme())),
        ));
    }

    /// Present the Switchboard capsule's instrument readout while it is
    /// expanded (hovered or pinned), or remove it once collapsed.
    ///
    /// The readout opens outward from the capsule's slot at
    /// [`TrayReadoutLayout::panel`]'s origin, rounded with
    /// [`TrayReadoutLayout::corner_radius`]. Fails closed like the others: a
    /// render whose surface cannot be allocated leaves the existing window
    /// untouched.
    ///
    /// [`TrayReadoutLayout::panel`]: tairix_taskbar::TrayReadoutLayout::panel
    /// [`TrayReadoutLayout::corner_radius`]: tairix_taskbar::TrayReadoutLayout::corner_radius
    fn present_readout(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
    ) {
        let Some(layout) = taskbar.tray_readout_layout(scale) else {
            if let Some(id) = self.readout.take() {
                compositor.remove(id);
            }
            return;
        };
        let Some(surface) = renderer.render_tray_readout(taskbar, scale) else {
            return;
        };
        let corners = Corners::from_radius(layout.corner_radius);
        self.readout = Some(place(
            compositor,
            self.readout,
            layout.panel.origin,
            surface,
            (corners, chrome_blur(taskbar.theme())),
        ));
    }
}

/// How far the backdrop behind the desktop's floating chrome is blurred, in
/// the *logical* pixels the compositor resolves against the output's density.
///
/// The bar and every popup it opens are drawn with the theme's floating
/// ground, which only reads as frosted glass over a blurred backdrop — so the
/// two are the same theme's decision and are taken from it here rather than
/// restated per surface.
fn chrome_blur(theme: &Theme) -> u16 {
    u16::try_from(theme.metrics().chrome_backdrop_blur).unwrap_or(u16::MAX)
}

/// Update an existing compositor window in place, or add a new one, and give
/// it the `(corners, backdrop blur)` look it is placed with. Returns the
/// window's id.
///
/// An `existing` id that the compositor no longer knows (the embedder removed
/// the window) is treated as absent, so the window is re-created rather than
/// the update being silently dropped.
///
/// The blur is in logical pixels, `0` for a surface that covers what is behind
/// it. A caller states it here rather than after placing, so a surface can
/// never be shown for a frame wearing the frosting of whatever was last placed.
///
/// Shared with the shell's own popup surfaces (the pinboard's context menu), so
/// every menu and popup window in the session is placed, re-surfaced, rounded,
/// and frosted by one definition.
pub(crate) fn place(
    compositor: &mut Compositor,
    existing: Option<WindowId>,
    origin: Point,
    surface: Surface,
    look: (Corners, u16),
) -> WindowId {
    let (corners, blur_px) = look;
    if let Some(id) = existing {
        if compositor.window(id).is_some() {
            compositor.set_surface(id, surface);
            compositor.move_window(id, origin);
            compositor.set_corners(id, corners);
            compositor.set_backdrop_blur(id, blur_px);
            return id;
        }
    }
    let id = compositor.add_window(origin, surface);
    compositor.set_corners(id, corners);
    compositor.set_backdrop_blur(id, blur_px);
    id
}
