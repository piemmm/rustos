//! Presenting the taskbar, its program-library popup, its hover window
//! picker, and its popovers through the window manager.
//!
//! The taskbar models the desktop's bar and produces a *rectangular* pixel
//! [`Surface`], and the window manager composites and
//! rounds windows; neither knows about the other. Joining
//! them is session glue, and [`TaskbarPresenter`] is that join: it paints the
//! bar (and, while open, the program-library popup and the hover window
//! picker) with the taskbar's own [`TaskbarRenderer`] and presents each as a
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
//! A surface already on screen keeps its pixels: the presenter paints what it
//! owes into the buffer the window holds ([`Compositor::repaint_window`]),
//! so the compositor marks the control that changed rather than the window's
//! whole bounds — the same treatment the desktop's menu plates get.
//!
//! It is **total and fails closed**: a paint whose pixels cannot be
//! allocated leaves whatever is already on screen untouched rather
//! than blanking the bar or panicking, and keeps what that surface owed so it
//! is drawn on a later present; the popup and picker windows are
//! removed from the compositor the moment each closes. If a presented window
//! has disappeared from the compositor (an embedder removed it), the next
//! present re-creates it rather than silently doing nothing.

use tairix_controls::damage::Repaint;
use tairix_icon::IconArtwork;
use tairix_taskbar::{Taskbar, TaskbarRenderer, TaskbarRepaint};
use tairix_theme::Theme;
use tairix_wm::{Compositor, Corners, Point, Rect, Scale, Surface, WindowId};

/// Presents a taskbar, its program-library popup, its hover window picker,
/// the notification popover, and the Switchboard capsule's instrument readout
/// as window-manager windows.
///
/// Build one with [`TaskbarPresenter::new`], then call
/// [`present`](Self::present) whenever the taskbar's model or the active theme
/// changes, naming the surfaces that changed; the presenter creates the bar,
/// popup, and picker windows on first sight and keeps their surface,
/// position, and corner radius in step thereafter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarPresenter {
    bar: Option<WindowId>,
    popup: Option<WindowId>,
    picker: Option<WindowId>,
    notifications: Option<WindowId>,
    readout: Option<WindowId>,
    /// The density the surfaces on screen were laid out at, so a runtime
    /// DPI change is caught even though it never touches the taskbar
    /// model that drives the repaint latch.
    presented_scale: Option<Scale>,
    /// What each surface still owes the screen. A present the heap refuses
    /// puts back what it could not draw, so a refusal can never leave a
    /// surface stale with nothing left to say so.
    owed: TaskbarRepaint,
}

impl TaskbarPresenter {
    /// A presenter that has not yet placed any window.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bar: None,
            popup: None,
            picker: None,
            notifications: None,
            readout: None,
            presented_scale: None,
            owed: TaskbarRepaint::NONE,
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

    /// The compositor window currently showing the open hover window
    /// picker, or `None` while it is closed (or before its first
    /// presentation).
    #[must_use]
    pub const fn picker_window(&self) -> Option<WindowId> {
        self.picker
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

    /// Whether `window` is one of the surfaces this presenter placed: the bar
    /// itself, or any of the popups and popovers it opens.
    ///
    /// Only the presenter knows which compositor window each taskbar surface
    /// *is* — the taskbar model knows their geometry and nothing more — so
    /// this is the one place that answer is given. The session's input router
    /// asks it of whatever the compositor finds on top at the pointer: `false`
    /// means something else is drawn over the bar there, and the press belongs
    /// to that instead ([`SessionInputRouter::handle`]).
    ///
    /// [`SessionInputRouter::handle`]: crate::SessionInputRouter::handle
    #[must_use]
    pub fn owns_window(&self, window: WindowId) -> bool {
        [
            self.bar,
            self.popup,
            self.picker,
            self.notifications,
            self.readout,
        ]
        .into_iter()
        .flatten()
        .any(|placed| placed == window)
    }

    /// Bring the compositor up to date with the taskbar's current model and
    /// its owned theme, repainting only what `parts` says each surface owes.
    ///
    /// The bar is repainted and re-placed at [`BarLayout::bar`]'s origin,
    /// rounded with [`BarLayout::corner_radius`]. If the program-library
    /// popup is open it is painted and presented above the bar at
    /// [`LibraryLayout::panel`]'s origin, rounded with
    /// [`LibraryLayout::corner_radius`]; when the popup is closed any popup
    /// window is removed.
    ///
    /// Repainting only what is owed is what keeps a pointer crossing the bar
    /// cheap, and it is scoped twice over. Naming the *surface* means the
    /// others are not re-rendered or re-pushed at all: re-rendering every one
    /// costs milliseconds and damages a window rectangle each, where the
    /// popover alone costs a fraction of that and damages one. Naming the
    /// *rectangles* on it means a surface already on screen keeps its pixels
    /// and only those rectangles are painted into the buffer it holds, so the
    /// compositor marks the control that changed instead of the window's whole
    /// bounds — which for a slot taking the pointer's wash is its own 1600
    /// pixels rather than the bar's 40 560, over a frosted backdrop that would
    /// otherwise be re-blurred.
    ///
    /// A surface with no window yet is always presented, whatever it owes, so
    /// the first paint after startup (or after a [`teardown`](Self::teardown))
    /// puts everything on screen. A change of desktop density is the one input
    /// the taskbar's own account cannot see — the scale belongs to the output,
    /// not the model — and it invalidates every rectangle owed, since each was
    /// measured at the old density, so it repaints everything.
    ///
    /// `artwork` is the shipped raster-icon lookup the bar draws its
    /// launcher and application icons from; every slot falls back to its
    /// built-in glyph when the lookup has nothing, so a system with no
    /// installed graphics still presents a complete bar.
    ///
    /// Fails closed: a paint whose pixels cannot be allocated leaves the
    /// existing on-screen window untouched **and keeps what that surface
    /// owed**, so the next present draws it rather than reporting a stale
    /// surface as current.
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
        self.owed |= parts;
        if self.presented_scale != Some(scale) {
            // Every rectangle owed was measured at the old density, so none of
            // them describes a pixel of the surfaces about to be laid out.
            self.owed |= TaskbarRepaint::ALL;
        }
        self.presented_scale = Some(scale);
        let owed = core::mem::take(&mut self.owed);
        if due(&owed.bar, self.bar)
            && !self.present_bar(compositor, renderer, taskbar, scale, artwork, &owed.bar)
        {
            self.owed.bar.merge(owed.bar);
        }
        if due(&owed.library, self.popup)
            && !self.present_popup(compositor, renderer, taskbar, scale, &owed.library)
        {
            self.owed.library.merge(owed.library);
        }
        if due(&owed.picker, self.picker)
            && !self.present_picker(compositor, renderer, taskbar, scale, &owed.picker)
        {
            self.owed.picker.merge(owed.picker);
        }
        if due(&owed.notifications, self.notifications)
            && !self.present_notifications(
                compositor,
                renderer,
                taskbar,
                scale,
                &owed.notifications,
            )
        {
            self.owed.notifications.merge(owed.notifications);
        }
        if due(&owed.readout, self.readout)
            && !self.present_readout(compositor, renderer, taskbar, scale, &owed.readout)
        {
            self.owed.readout.merge(owed.readout);
        }
    }

    /// Remove the bar, popup, picker, and popover windows from `compositor`
    /// and forget them, so a later [`present`](Self::present) starts fresh.
    /// Tearing the desktop session down leaves no orphaned windows behind.
    pub fn teardown(&mut self, compositor: &mut Compositor) {
        self.presented_scale = None;
        self.owed = TaskbarRepaint::NONE;
        if let Some(id) = self.readout.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.notifications.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.picker.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.popup.take() {
            compositor.remove(id);
        }
        if let Some(id) = self.bar.take() {
            compositor.remove(id);
        }
    }

    /// Repaint what the bar owes and present it, leaving the existing window
    /// untouched if the pixels cannot be allocated. Reports whether it was
    /// drawn.
    fn present_bar(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
        artwork: &mut dyn IconArtwork,
        owed: &Repaint,
    ) -> bool {
        let layout = taskbar.layout(scale);
        let corners = Corners::from_radius(layout.corner_radius);
        let placed = place(
            compositor,
            self.bar,
            (layout.bar.origin, (layout.bar.width, layout.bar.height)),
            owed,
            (corners, chrome_blur(taskbar.theme())),
            |surface, rects| renderer.paint(taskbar, scale, artwork, surface, rects),
        );
        if let Some(id) = placed {
            self.bar = Some(id);
        }
        placed.is_some()
    }

    /// Present the program-library popup while it is open, or remove it once
    /// closed.
    fn present_popup(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
        owed: &Repaint,
    ) -> bool {
        if !taskbar.library().is_open() {
            if let Some(id) = self.popup.take() {
                compositor.remove(id);
            }
            return true;
        }
        let layout = taskbar.library_layout(scale);
        let corners = Corners::from_radius(layout.corner_radius);
        let placed = place(
            compositor,
            self.popup,
            (
                layout.panel.origin,
                (layout.panel.width, layout.panel.height),
            ),
            owed,
            (corners, chrome_blur(taskbar.theme())),
            |surface, rects| renderer.paint_library(taskbar, scale, surface, rects),
        );
        if let Some(id) = placed {
            self.popup = Some(id);
        }
        placed.is_some()
    }

    /// Present the hover window picker while it is open, or remove it once
    /// closed.
    ///
    /// The picker opens outward from the hovered application's slot at
    /// [`PickerLayout::panel`]'s origin, rounded with
    /// [`PickerLayout::corner_radius`]. Fails closed like the others: a
    /// render whose surface cannot be allocated leaves the existing window
    /// untouched.
    ///
    /// [`PickerLayout::panel`]: tairix_taskbar::PickerLayout::panel
    /// [`PickerLayout::corner_radius`]: tairix_taskbar::PickerLayout::corner_radius
    fn present_picker(
        &mut self,
        compositor: &mut Compositor,
        renderer: &mut TaskbarRenderer,
        taskbar: &Taskbar,
        scale: Scale,
        owed: &Repaint,
    ) -> bool {
        let Some(layout) = taskbar.picker_layout(scale) else {
            if let Some(id) = self.picker.take() {
                compositor.remove(id);
            }
            return true;
        };
        let corners = Corners::from_radius(layout.corner_radius);
        let placed = place(
            compositor,
            self.picker,
            (
                layout.panel.origin,
                (layout.panel.width, layout.panel.height),
            ),
            owed,
            (corners, chrome_blur(taskbar.theme())),
            |surface, rects| renderer.paint_picker(taskbar, scale, surface, rects),
        );
        if let Some(id) = placed {
            self.picker = Some(id);
        }
        placed.is_some()
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
        owed: &Repaint,
    ) -> bool {
        let Some(layout) = taskbar.notifications_layout(scale) else {
            if let Some(id) = self.notifications.take() {
                compositor.remove(id);
            }
            return true;
        };
        let corners = Corners::from_radius(layout.corner_radius);
        let placed = place(
            compositor,
            self.notifications,
            (
                layout.panel.origin,
                (layout.panel.width, layout.panel.height),
            ),
            owed,
            (corners, chrome_blur(taskbar.theme())),
            |surface, rects| renderer.paint_notifications(taskbar, scale, surface, rects),
        );
        if let Some(id) = placed {
            self.notifications = Some(id);
        }
        placed.is_some()
    }

    /// Present the Switchboard capsule's instrument readout while it is
    /// expanded (hovered or keyboard-focused), or remove it once collapsed.
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
        owed: &Repaint,
    ) -> bool {
        let Some(layout) = taskbar.tray_readout_layout(scale) else {
            if let Some(id) = self.readout.take() {
                compositor.remove(id);
            }
            return true;
        };
        let corners = Corners::from_radius(layout.corner_radius);
        let placed = place(
            compositor,
            self.readout,
            (
                layout.panel.origin,
                (layout.panel.width, layout.panel.height),
            ),
            owed,
            (corners, chrome_blur(taskbar.theme())),
            |surface, rects| renderer.paint_tray_readout(taskbar, scale, surface, rects),
        );
        if let Some(id) = placed {
            self.readout = Some(id);
        }
        placed.is_some()
    }
}

/// Whether a chrome surface is due a present: it owes pixels, or it has no
/// window yet.
///
/// A surface with nothing on screen is presented whatever its account says, so
/// the first paint after startup (or after a [`teardown`]) puts everything up.
///
/// [`teardown`]: TaskbarPresenter::teardown
fn due(owed: &Repaint, window: Option<WindowId>) -> bool {
    !owed.is_clean() || window.is_none()
}

/// How far the backdrop behind the desktop's floating chrome is blurred, in
/// the *logical* pixels the compositor resolves against the output's density.
///
/// The bar, every popup it opens and every menu plate are drawn with the
/// theme's floating ground, which only reads as frosted glass over a blurred
/// backdrop — so the two are the same theme's decision and are taken from it
/// here rather than restated per surface.
pub(crate) fn chrome_blur(theme: &Theme) -> u16 {
    u16::try_from(theme.metrics().chrome_backdrop_blur).unwrap_or(u16::MAX)
}

/// Repaint what a chrome surface owes into the compositor window it already
/// has, or add a new one painted whole, and give it the `(corners, backdrop
/// blur)` look it is placed with. Returns the window's id, or `None` when the
/// pixels could not be allocated.
///
/// **A surface already on screen keeps its pixels and is repainted only where
/// it owes them.** That is the whole of this change's cost model: handing the
/// compositor a freshly rendered surface instead would allocate one, paint all
/// of it, and mark the window's entire bounds for recomposition — 1014 × 40
/// screen pixels for a 40 × 40 slot taking the pointer's wash, over a frosted
/// backdrop. `paint` receives the rectangles already clipped to the surface
/// and disjoint, in its own local pixels, and writes inside them and nowhere
/// else.
///
/// An `existing` id that the compositor no longer knows (the embedder removed
/// the window) is treated as absent, so the window is re-created rather than
/// the update being silently dropped. A window whose extent no longer matches
/// the layout is repainted whole into a buffer of the new size, however little
/// was owed — a buffer of the wrong size has nothing for a partial paint to
/// keep.
///
/// The blur is in logical pixels, `0` for a surface that covers what is behind
/// it. A caller states it here rather than after placing, so a surface can
/// never be shown for a frame wearing the frosting of whatever was last placed.
fn place(
    compositor: &mut Compositor,
    existing: Option<WindowId>,
    placement: (Point, (u32, u32)),
    owed: &Repaint,
    look: (Corners, u16),
    paint: impl FnOnce(&mut Surface, &[Rect]),
) -> Option<WindowId> {
    let (origin, (width, height)) = placement;
    let (corners, blur_px) = look;
    let id = if let Some(id) = existing.filter(|id| compositor.window(*id).is_some()) {
        // Moved before it is painted, so the rectangles the repaint marks are
        // the ones the surface now occupies.
        compositor.move_window(id, origin);
        if !compositor.repaint_window(id, (width, height), &owed.area(width, height), paint) {
            return None;
        }
        id
    } else {
        let mut pixels = Surface::new(width, height)?;
        paint(&mut pixels, &[Rect::new(0, 0, width, height)]);
        compositor.add_window(origin, pixels)
    };
    compositor.set_corners(id, corners);
    compositor.set_backdrop_blur(id, blur_px);
    Some(id)
}
