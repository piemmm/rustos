//! Presenting the taskbar, its program-library popup, and its context menu
//! through the window manager.
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

use tairix_taskbar::{Taskbar, TaskbarRenderer};
use tairix_wm::{Compositor, Corners, Point, Scale, Surface, WindowId};

/// Presents a taskbar, its program-library popup, and its context menu as
/// window-manager windows.
///
/// Build one with [`TaskbarPresenter::new`], then call
/// [`present`](Self::present) whenever the taskbar's model or the active theme
/// changes; the presenter creates the bar, popup, and menu windows on first
/// sight and keeps their surface, position, and corner radius in step
/// thereafter.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarPresenter {
    bar: Option<WindowId>,
    popup: Option<WindowId>,
    menu: Option<WindowId>,
    notifications: Option<WindowId>,
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

    /// Bring the compositor up to date with the taskbar's current model and
    /// its owned theme.
    ///
    /// The bar is repainted and re-placed at [`BarLayout::bar`]'s origin,
    /// rounded with [`BarLayout::corner_radius`]. If the program-library
    /// popup is open it is painted and presented above the bar at
    /// [`LibraryLayout::panel`]'s origin, rounded with
    /// [`LibraryLayout::corner_radius`]; when the popup is closed any popup
    /// window is removed.
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
    ) {
        // The desktop density belongs to the output the bar is on, so it is
        // read from the compositor here and laid out at present time — a
        // runtime DPI change re-presents the bar at the new scale with no
        // taskbar state to update.
        let scale = compositor.scale();
        self.present_bar(compositor, renderer, taskbar, scale);
        self.present_popup(compositor, renderer, taskbar, scale);
        self.present_menu(compositor, renderer, taskbar, scale);
        self.present_notifications(compositor, renderer, taskbar, scale);
    }

    /// Remove the bar, popup, and menu windows from `compositor` and forget
    /// them, so a later [`present`](Self::present) starts fresh. Tearing the
    /// desktop session down leaves no orphaned windows behind.
    pub fn teardown(&mut self, compositor: &mut Compositor) {
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
    ) {
        let Some(surface) = renderer.render(taskbar, scale) else {
            return;
        };
        let layout = taskbar.layout(scale);
        let corners = Corners::from_radius(layout.corner_radius);
        self.bar = Some(place(
            compositor,
            self.bar,
            layout.bar.origin,
            surface,
            corners,
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
            corners,
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
            corners,
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
            corners,
        ));
    }
}

/// Update an existing compositor window in place, or add a new one, and apply
/// `corners`. Returns the window's id.
///
/// An `existing` id that the compositor no longer knows (the embedder removed
/// the window) is treated as absent, so the window is re-created rather than
/// the update being silently dropped.
fn place(
    compositor: &mut Compositor,
    existing: Option<WindowId>,
    origin: Point,
    surface: Surface,
    corners: Corners,
) -> WindowId {
    if let Some(id) = existing {
        if compositor.window(id).is_some() {
            compositor.set_surface(id, surface);
            compositor.move_window(id, origin);
            compositor.set_corners(id, corners);
            return id;
        }
    }
    let id = compositor.add_window(origin, surface);
    compositor.set_corners(id, corners);
    id
}
