//! Driving the whole desktop from one live input stream.
//!
//! The session crate already holds the desktop's pieces apart: the
//! [`DesktopSession`] (the theme registry + taskbar model), the
//! [`SessionInputRouter`] (which fans one pointer-event stream to the window
//! manager and taskbar routers), the [`TaskbarPresenter`] (which paints the
//! bar and its start-menu popup into compositor windows), and the
//! [`TaskbarRenderer`] (which holds the across-frame glyph cache). Composing
//! them into one event-driven frontend was the long-open "feed the router and
//! presenter from live device events" thread; [`DesktopShell`] is that
//! composition.
//!
//! A real desktop runs a loop: read the pending pointer events, route each to
//! the right part of the desktop, perform the session-level effect of a
//! taskbar action (the light/dark toggle), and bring the on-screen bar back in
//! step. [`DesktopShell`] owns the four pieces and runs exactly that loop over
//! an injected [`InputSource`] seam — a real pointer/keyboard channel on a
//! running system, an in-memory queue in tests, the same
//! injected-seam shape the default apps use for their backing channels.
//!
//! The shell holds no framebuffer and grants itself no authority: the
//! [`Compositor`] is the embedder's (it owns the framebuffer capability) and is passed in on each call. Composing the taskbar
//! and window-manager GUI crates this way is the permitted `userland/gui/*`
//! edge; nothing outside `userland/gui/*` depends on this glue.
//! It never panics: every routed sub-call and every present is itself total and
//! fails closed. A session-level effect the shell cannot
//! perform with its own state — relaying the switched theme to the window
//! manager and apps, performing a session control, launching an app — is
//! surfaced as a [`ShellOutcome`] for the embedder, which holds those
//! capabilities, to act on.
//!
//! [`DesktopSession`]: crate::DesktopSession
//! [`SessionInputRouter`]: crate::SessionInputRouter
//! [`TaskbarPresenter`]: crate::TaskbarPresenter
//! [`TaskbarRenderer`]: tairix_taskbar::TaskbarRenderer

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_cursor::{CursorRegistry, CursorSetId, CursorTheme};
use tairix_icon::IconSet;
use tairix_taskbar::{TaskbarConfig, TaskbarRenderer, TaskbarResponse};
use tairix_wm::{
    Color, Compositor, CursorController, InputEvent, InputResponse, Point, Scale, Surface, WindowId,
};

use crate::input::{SessionInputResponse, SessionInputRouter};
use crate::presenter::TaskbarPresenter;
use crate::session::{DesktopSession, SessionEvent};
use crate::tasks::TaskBridge;

/// A source of live pointer/keyboard events for the desktop.
///
/// On a running system this is backed by the kernel's input channel; tests
/// back it with an in-memory queue. It is the only seam
/// through which device events reach the desktop, so the `no_std` GUI crates
/// hold no input capability of their own.
pub trait InputSource {
    /// Take the next pending input event, or `None` when the stream is
    /// momentarily drained.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the source itself faults
    /// (for example the channel was closed). A faulting source ends the
    /// current [`pump`](DesktopShell::pump) without disturbing the desktop
    /// state already applied; the embedder replaces or re-polls the source.
    fn poll(&mut self) -> Result<Option<InputEvent>, Errno>;
}

/// What the [`DesktopShell`] did with one input event.
///
/// Each event is routed to exactly one part of the desktop, so its outcome is
/// either nothing, a window-manager action the embedder may observe (focus
/// change, a window move), or a session-level [`SessionEvent`] the embedder
/// must act on (relay the switched theme, perform a forwarded control).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellOutcome {
    /// The event changed no desktop state (a non-primary button, or a press or
    /// motion that no router acted on).
    Ignored,
    /// The event drove the window manager (focus, click-to-activate, a window
    /// move). The embedder may observe it; the compositor is already updated.
    WindowManager(InputResponse),
    /// The event drove the taskbar and produced a session-level event for the
    /// embedder: an [`AppearanceChanged`](SessionEvent::AppearanceChanged) the
    /// shell already applied (relay the new theme to the window manager and
    /// apps), or a [`Forward`](SessionEvent::Forward)ed control the embedder
    /// holds the capability to perform.
    Session(SessionEvent),
}

/// The desktop session frontend: the session state, the input router, the
/// taskbar presenter, and the taskbar renderer, driven by an [`InputSource`].
///
/// Build one with [`DesktopShell::new`], call [`present`](Self::present) once
/// to place the bar, then call [`pump`](Self::pump) each frame to apply the
/// pending input. Application windows are opened and closed as running tasks
/// with [`open_window`](Self::open_window) / [`close_window`](Self::close_window);
/// a title-bar drag is armed with [`begin_move`](Self::begin_move);
/// a loaded notification-icon set is installed with [`set_icons`](Self::set_icons);
/// tearing the desktop down is [`teardown`](Self::teardown).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopShell {
    session: DesktopSession,
    router: SessionInputRouter,
    presenter: TaskbarPresenter,
    renderer: TaskbarRenderer,
    tasks: TaskBridge,
    cursor: CursorController,
}

impl DesktopShell {
    /// Build a desktop shell for a taskbar placed by `config`, starting from
    /// the built-in themes with the default dark theme active and the start
    /// menu's light/dark entry labelled `appearance_label`.
    ///
    /// The router starts with the pointer at the screen origin and no focus,
    /// the presenter has placed no window yet, and the renderer draws the
    /// built-in icon set until [`set_icons`](Self::set_icons) installs a loaded
    /// one.
    #[must_use]
    pub fn new(config: TaskbarConfig, appearance_label: &str) -> Self {
        Self {
            session: DesktopSession::new(config, appearance_label),
            router: SessionInputRouter::new(),
            presenter: TaskbarPresenter::new(),
            renderer: TaskbarRenderer::new(),
            tasks: TaskBridge::new(),
            cursor: CursorController::new(),
        }
    }

    /// The desktop session (theme registry + taskbar model).
    #[must_use]
    pub const fn session(&self) -> &DesktopSession {
        &self.session
    }

    /// The desktop session, mutably — e.g. to update the task list or clock
    /// before the next [`present`](Self::present).
    pub fn session_mut(&mut self) -> &mut DesktopSession {
        &mut self.session
    }

    /// The session input router (its tracked pointer and focused window).
    #[must_use]
    pub const fn router(&self) -> &SessionInputRouter {
        &self.router
    }

    /// The taskbar presenter (its bar and popup window ids).
    #[must_use]
    pub const fn presenter(&self) -> &TaskbarPresenter {
        &self.presenter
    }

    /// The task bridge (the window↔task correspondence).
    #[must_use]
    pub const fn tasks(&self) -> &TaskBridge {
        &self.tasks
    }

    /// The pointer-cursor controller: the on-screen pointer shape and the
    /// cursor sets it draws from.
    #[must_use]
    pub const fn cursor(&self) -> &CursorController {
        &self.cursor
    }

    /// Bring the pointer cursor up to date and, the first time, install it —
    /// so the desktop shows a pointer from its very first frame.
    ///
    /// Runs the window manager's cursor-shape policy against the current
    /// interaction state (the plain arrow over the desktop background, a
    /// window's own [`cursor_hint`](tairix_wm::Window::cursor_hint) under the
    /// pointer, the move cursor during an interactive grab), re-rasterising at
    /// the output density only when the chosen shape, the active cursor set,
    /// or the [`scale`](Compositor::scale) actually changed, and repositions
    /// the overlay so its hotspot tracks the pointer. It is called for you as
    /// each input event is [`handle`](Self::handle)d and on a runtime
    /// [`set_scale`](Self::set_scale); the embedder calls it once after the
    /// first [`present`](Self::present) to show the pointer at start-up.
    ///
    /// Fails closed: if the chosen shape cannot be rasterised the current
    /// cursor is left untouched rather than blanking the pointer.
    pub fn refresh_cursor(&mut self, compositor: &mut Compositor) {
        self.cursor.refresh(self.router.wm(), compositor);
        compositor.move_cursor(self.router.pointer());
    }

    /// Install a loaded cursor set as the active pointer artwork, replacing
    /// the built-in set, and re-render the current shape so the new artwork
    /// shows without waiting for the next interaction.
    ///
    /// The `theme` is the [`CursorTheme`] the session assembled from the
    /// on-disk `/System/Graphics` SVG assets
    /// ([`DesktopSession::load_cursors`](crate::DesktopSession::load_cursors)),
    /// already failed closed per kind to the built-in cursor, so this cannot
    /// blank the pointer. It is the cursor counterpart of
    /// [`set_icons`](Self::set_icons).
    pub fn set_cursors(&mut self, theme: CursorTheme, compositor: &mut Compositor) {
        let mut registry = CursorRegistry::with_builtin();
        let id = CursorSetId::new("desktop");
        if registry.register(id, theme).is_ok() {
            let _ = registry.set_active(id);
        }
        self.cursor
            .set_registry(registry, self.router.wm(), compositor);
    }

    /// Open `surface` as a top-level application window at `origin`, list it on
    /// the taskbar titled `title`, and focus it, re-presenting the bar so the
    /// new task appears.
    ///
    /// Returns the new [`WindowId`]. Returns `None`, opening nothing, only if
    /// the task-id space is exhausted. The compositor is the
    /// embedder's, passed in here; the shell holds no framebuffer.
    pub fn open_window(
        &mut self,
        compositor: &mut Compositor,
        origin: Point,
        surface: Surface,
        title: impl Into<String>,
    ) -> Option<WindowId> {
        let window = self.tasks.open(
            compositor,
            &mut self.router,
            self.session.taskbar_mut(),
            origin,
            surface,
            title,
        )?;
        self.present(compositor);
        Some(window)
    }

    /// Close `window`: remove it from the compositor and the taskbar, dropping
    /// focus if it held it, and re-present the bar so the task disappears.
    ///
    /// Returns `false`, changing nothing, when `window` is not a tracked task.
    pub fn close_window(&mut self, compositor: &mut Compositor, window: WindowId) -> bool {
        if !self.tasks.close(
            compositor,
            &mut self.router,
            self.session.taskbar_mut(),
            window,
        ) {
            return false;
        }
        self.present(compositor);
        true
    }

    /// Install a loaded notification-icon set on the renderer, replacing the
    /// one in use. The next [`present`](Self::present) re-rasterises the
    /// notification glyphs from the new set.
    pub fn set_icons(&mut self, set: IconSet) {
        self.renderer.set_icons(set);
    }

    /// Switch the desktop UI scale for the output this desktop is on and, if
    /// it changed, re-present the taskbar at the new density, returning
    /// whether it changed.
    ///
    /// The density belongs to the output, not the desktop session, so this
    /// drives the compositor's [`set_scale`](Compositor::set_scale) — the
    /// single source of truth — and re-lays the bar in place. No desktop
    /// restart is needed and the taskbar holds no scale of its own, so the
    /// rescale is transparent to it. On a `true`
    /// return the pointer cursor is re-rasterised at the new density here
    /// ([`refresh_cursor`](Self::refresh_cursor)), and the embedder brings the
    /// other scale-dependent overlays it owns up to date: any app that sizes
    /// in physical pixels (each reads its own window's density through
    /// [`Compositor::window_scale`], never sets it). Setting the scale already
    /// in effect re-presents nothing and returns `false`.
    pub fn set_scale(&mut self, scale: Scale, compositor: &mut Compositor) -> bool {
        if !compositor.set_scale(scale) {
            return false;
        }
        self.present(compositor);
        self.refresh_cursor(compositor);
        true
    }

    /// The active theme's desktop colour as the compositor's colour type,
    /// through the one shared theme→render edge (`From<Rgba> for Color`).
    ///
    /// The embedder builds the compositor over this colour
    /// ([`Compositor::new`]); after that,
    /// [`sync_background`](Self::sync_background) keeps the two in step.
    #[must_use]
    pub fn desktop_background(&self) -> Color {
        Color::from(self.session.active_theme().palette().desktop)
    }

    /// Bring the compositor's desktop background in step with the active
    /// theme, returning whether it changed.
    ///
    /// [`handle`](Self::handle) calls this itself when the start menu's
    /// appearance toggle switches the theme, so the desktop behind the
    /// windows re-paints in the same frame as the re-themed bar; an embedder
    /// that switches the theme programmatically (through
    /// [`session_mut`](Self::session_mut)) calls it, then
    /// [`present`](Self::present), to relay the switch itself.
    pub fn sync_background(&mut self, compositor: &mut Compositor) -> bool {
        compositor.set_background(self.desktop_background())
    }

    /// Bring the compositor up to date with the taskbar's current model and the
    /// active theme: repaint and place the bar and, while the start menu is
    /// open, its popup.
    ///
    /// Fails closed: a render whose surface cannot be
    /// allocated leaves the existing on-screen window untouched.
    pub fn present(&mut self, compositor: &mut Compositor) {
        self.presenter.present(
            compositor,
            &mut self.renderer,
            self.session.taskbar(),
            self.session.active_theme(),
        );
    }

    /// Route one input `event` to the desktop and return what it did.
    ///
    /// The event is fanned to the window manager or taskbar by the
    /// [`SessionInputRouter`]'s policy; a taskbar action is then
    /// [`resolve`](DesktopSession::resolve)d (the light/dark toggle is applied
    /// here, the task activate/minimise outcome is applied to the compositor,
    /// everything else is forwarded) and the bar is re-presented so the screen
    /// reflects the change (an opened/closed menu, a re-themed bar, a changed
    /// task highlight). A window-manager action re-presents only when it moved
    /// focus (a press), keeping the highlight in step; motion and drags change
    /// no highlight and stay cheap.
    ///
    /// Whatever the event did, the pointer cursor is then brought up to date
    /// ([`refresh_cursor`](Self::refresh_cursor)): its hotspot follows pointer
    /// motion and its shape follows what is under it (or the move cursor
    /// during a drag), so the desktop always shows a live pointer.
    pub fn handle(&mut self, event: InputEvent, compositor: &mut Compositor) -> ShellOutcome {
        let outcome = match self
            .router
            .handle(event, compositor, self.session.taskbar_mut())
        {
            SessionInputResponse::Ignored => ShellOutcome::Ignored,
            SessionInputResponse::WindowManager(response) => {
                self.mirror_focus(&response, compositor);
                ShellOutcome::WindowManager(response)
            }
            SessionInputResponse::Taskbar(response) => {
                if let TaskbarResponse::TaskActivated { id, outcome } = response {
                    self.tasks
                        .activate(compositor, &mut self.router, id, outcome);
                }
                let event = self.session.resolve(response);
                if let SessionEvent::AppearanceChanged(_) = event {
                    self.sync_background(compositor);
                }
                self.present(compositor);
                ShellOutcome::Session(event)
            }
        };
        self.refresh_cursor(compositor);
        outcome
    }

    /// Keep the bar's highlighted task in step when the window manager moves
    /// focus by a direct click on a window or the desktop.
    ///
    /// A press that focused a window or the desktop relays the new focus into
    /// the task list and, only when the highlighted task actually moved,
    /// re-presents the bar; a drag ([`Moved`](InputResponse::Moved) /
    /// [`MoveEnded`](InputResponse::MoveEnded)), a no-op, or a press on a window
    /// that owns no task changes no highlight and is left cheap.
    fn mirror_focus(&mut self, response: &InputResponse, compositor: &mut Compositor) {
        let focus = match *response {
            // A furniture press (a scrollbar) activated its window exactly as a
            // client press does, so the highlighted task follows it too.
            InputResponse::Activated { window, .. }
            | InputResponse::FurniturePressed { window } => Some(window),
            InputResponse::DesktopPressed => None,
            // A drag, resize, command-control activation, key, or scroll acts
            // on the already-focused window (the press that started it moved
            // focus), so none of them changes the highlighted task.
            InputResponse::Moved { .. }
            | InputResponse::MoveEnded { .. }
            | InputResponse::Resized { .. }
            | InputResponse::ResizeEnded { .. }
            | InputResponse::WindowControl { .. }
            | InputResponse::Key { .. }
            | InputResponse::Scrolled { .. }
            | InputResponse::AppScroll { .. }
            | InputResponse::Ignored => return,
        };
        if self.tasks.sync_focus(self.session.taskbar_mut(), focus) {
            self.present(compositor);
        }
    }

    /// Drain every pending event from `source`, routing each through
    /// [`handle`](Self::handle), and return their outcomes in order.
    ///
    /// # Errors
    ///
    /// Returns the [`Errno`] from a faulting [`InputSource::poll`]. The events
    /// drained before the fault have already been applied to the desktop state
    /// and the compositor (the desktop never rolls back what it has shown); the
    /// embedder replaces or re-polls the source.
    pub fn pump<S>(
        &mut self,
        source: &mut S,
        compositor: &mut Compositor,
    ) -> Result<Vec<ShellOutcome>, Errno>
    where
        S: InputSource + ?Sized,
    {
        let mut outcomes = Vec::new();
        while let Some(event) = source.poll()? {
            outcomes.push(self.handle(event, compositor));
        }
        Ok(outcomes)
    }

    /// Arm an interactive move-grab on the focused window, anchored at the
    /// current pointer position; the next pointer motion then drags it.
    ///
    /// Returns `false` (arming nothing) when there is no focused window or it
    /// is no longer known to `compositor`. Window
    /// decorations call this on a title-bar press.
    pub fn begin_move(&mut self, compositor: &Compositor) -> bool {
        self.router.begin_move(compositor)
    }

    /// Remove the bar and popup windows from `compositor` and forget them, so a
    /// later [`present`](Self::present) starts fresh. Tearing the session down
    /// leaves no orphaned windows behind.
    pub fn teardown(&mut self, compositor: &mut Compositor) {
        self.presenter.teardown(compositor);
    }
}
