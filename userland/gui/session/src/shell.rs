//! Driving the whole desktop from one live input stream.
//!
//! The session crate already holds the desktop's pieces apart: the
//! [`DesktopSession`] (the theme registry + taskbar model), the
//! [`SessionInputRouter`] (which fans one pointer-event stream to the window
//! manager and taskbar routers), the [`TaskbarPresenter`] (which paints the
//! bar and its program-library popup into compositor windows), and the
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

use tairix_abi::notify_ipc::NotifyRequest;
use tairix_abi::switchboard_ipc::TraySummary;
use tairix_abi::Errno;
use tairix_cursor::{CursorRegistry, CursorSetId, CursorTheme};
use tairix_icon::IconSet;
use tairix_log::Sink;
use tairix_proglib::Catalog;
use tairix_reclaim::PressureGauge;
use tairix_taskbar::{
    icon_cache, ActivateOutcome, PinView, TaskbarConfig, TaskbarRenderer, TaskbarResponse,
    TransientNotification,
};
use tairix_wm::{
    cursor_cache, Color, Compositor, CursorController, InputEvent, InputResponse, Point, Rect,
    Scale, Surface, WindowActivationState, WindowFrame, WindowFurnitureState, WindowId,
    WindowSizeState,
};

use crate::input::{SessionInputResponse, SessionInputRouter};
use crate::presenter::TaskbarPresenter;
use crate::session::DesktopSession;
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
/// change, a window move), or a taskbar response the embedder holds the
/// capabilities to act on (launch a library entry, open the file manager).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellOutcome {
    /// The event changed no desktop state (a non-primary button, or a press or
    /// motion that no router acted on).
    Ignored,
    /// The event drove the window manager (focus, click-to-activate, a window
    /// move). The embedder may observe it; the compositor is already updated.
    WindowManager(InputResponse),
    /// The event drove the taskbar. The shell has already applied what it
    /// can with its own state (a task activate/minimise, the popup's own
    /// state changes, the re-present); a response needing a capability the
    /// shell does not hold — launching an application, opening the file
    /// manager — is the embedder's to perform.
    Taskbar(TaskbarResponse),
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
///
/// Not `Clone`: the shell owns the seat's two rasterised-asset caches, and
/// each holds a live pressure gauge and audit sink behind trait objects.
/// Copying a shell would double-count their charged bytes against the
/// seat and give the copy a cache the reclaim ledger does not know about.
#[derive(Debug)]
pub struct DesktopShell {
    session: DesktopSession,
    router: SessionInputRouter,
    presenter: TaskbarPresenter,
    renderer: TaskbarRenderer,
    tasks: TaskBridge,
    cursor: CursorController,
    /// The decorated window currently shown with its active frame rim, kept
    /// in step with the window manager's focused window by
    /// [`sync_active_frame`](Self::sync_active_frame). `None` when focus rests
    /// on the desktop or an undecorated window.
    active_frame: Option<WindowId>,
}

impl DesktopShell {
    /// Build a desktop shell for a taskbar placed by `config`, starting from
    /// the built-in themes with the default dark theme active.
    ///
    /// The router starts with the pointer at the screen origin and no focus,
    /// the presenter has placed no window yet, the renderer draws the
    /// built-in icon set until [`set_icons`](Self::set_icons) installs a loaded
    /// one, and the program library is empty until
    /// [`set_library`](Self::set_library) hands it the resolved catalog.
    /// `output_bytes` is the byte size of one frame of the output this
    /// session drives; both rasterised-asset caches derive their budget
    /// from it, so a 4K desktop is allowed proportionately more cached
    /// pixels than a small panel and neither carries a hand-picked
    /// ceiling. `seat` owns those bytes in the reclaim ledger, `pressure`
    /// is the gauge the kernel reports the memory-pressure band into, and
    /// `sink` receives the caches' audit records.
    #[must_use]
    pub fn new(
        config: TaskbarConfig,
        seat: u64,
        output_bytes: usize,
        pressure: &'static (dyn PressureGauge + 'static),
        sink: &'static (dyn Sink + Sync),
    ) -> Self {
        Self {
            session: DesktopSession::new(config),
            router: SessionInputRouter::new(),
            presenter: TaskbarPresenter::new(),
            renderer: TaskbarRenderer::new(icon_cache(seat, output_bytes, pressure, sink)),
            tasks: TaskBridge::new(),
            cursor: CursorController::new(cursor_cache(seat, output_bytes, pressure, sink)),
            active_frame: None,
        }
    }

    /// Give back every rasterised pixel the current memory-pressure band
    /// requires, returning the bytes released across the seat's caches.
    ///
    /// Called when the kernel wakes the session with a deepened band, so
    /// the desktop releases its share at the moment pressure rises rather
    /// than at whatever later frame happens to touch a cache. A band that
    /// demands nothing releases nothing, so a spurious wake is cheap.
    ///
    /// `compositor` is trimmed alongside the shell's own two caches
    /// because the desktop's third disposable-UI cache — the decorated
    /// windows' rendered furniture — belongs to the output, not the
    /// shell, and pressure is answered by the whole session at once.
    ///
    /// The same call runs the window-*content* release ladder
    /// ([`Compositor::release_content_under_pressure`]), which is a policy
    /// rather than a cache: the compositor holds the only copy of a
    /// window's pixels, so the focused window is spared and every window
    /// whose pixels do go is queued for a redraw request the embedder
    /// drains with [`Compositor::pending_redraws`]. The focused window is
    /// the one the router already tracks, so the shell has a single notion
    /// of focus rather than a second one for reclaim.
    pub fn trim_caches(&mut self, compositor: &mut Compositor) -> usize {
        let focused = self.router.focused();
        self.cursor
            .trim()
            .saturating_add(self.renderer.trim())
            .saturating_add(compositor.trim_chrome())
            .saturating_add(compositor.release_content_under_pressure(focused))
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

    /// Open `surface` as a top-level window at `origin`, list it on the taskbar
    /// titled `title`, and focus it, re-presenting the bar so the new task
    /// appears.
    ///
    /// Returns the new [`WindowId`]. Returns `None`, opening nothing, only if
    /// the task-id space is exhausted. The compositor is the
    /// embedder's, passed in here; the shell holds no framebuffer.
    ///
    /// This opens the bare window; a served application window is additionally
    /// decorated with the window-manager frame furniture through
    /// [`decorate_window`](Self::decorate_window). The session's own trusted
    /// modal surfaces (the file picker) open undecorated — they are session
    /// chrome, dismissed by their own keys, not app windows the window manager
    /// dresses with a title bar.
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
        // Focus moved to the new window; keep the decorated active frame in
        // step (a no-op for this bare window, but it deactivates whatever
        // decorated window previously held focus).
        self.sync_active_frame(compositor);
        self.present(compositor);
        Some(window)
    }

    /// Decorate the already-open window `window` with the window-manager frame
    /// furniture — the title bar (with the Close / Minimize / `PutToBack` /
    /// `SizeToggle` command controls), the frame rim, and the resize edges —
    /// labelled `title`, and bring its active frame in step with focus.
    /// Returns `false`, changing nothing, for a window the compositor does not
    /// know.
    ///
    /// The decoration is server-side: the window manager composes the furniture
    /// *around* the app's content surface (the window's origin becomes the
    /// outer top-left and the content insets to the client rectangle), so the
    /// app never draws its own chrome and its content never overlaps the
    /// furniture. Served application windows are decorated (the serve loop
    /// calls this when a window opens) and are always movable by their title
    /// bar.
    ///
    /// `resizable` is the opening app's own request (carried on its window
    /// create): when set, the window manager draws a resize grabber and a live
    /// maximize/restore size toggle, and the app re-lays-out to each new client
    /// size it reports. A fixed-size app passes `false` — no grabber is drawn
    /// and the size-toggle is inert — so the mechanism is per-app opt-in, never
    /// forced on an app that renders at one size.
    pub fn decorate_window(
        &mut self,
        compositor: &mut Compositor,
        window: WindowId,
        title: &str,
        resizable: bool,
    ) -> bool {
        let frame = WindowFrame::new(WindowFurnitureState {
            activation: WindowActivationState::Active,
            size: WindowSizeState::Restored,
            movable: true,
            resizable,
        });
        if !compositor.set_window_frame(window, frame) {
            return false;
        }
        compositor.set_window_title(window, title);
        self.sync_active_frame(compositor);
        true
    }

    /// Bring the compositor's decorated-window activation in step with the
    /// window manager's focused window: the newly focused decorated window
    /// shows its active frame rim, title, and controls, and the
    /// previously-active one reverts to inactive. Focus onto the desktop or an
    /// undecorated window leaves no window active.
    ///
    /// This is the single place activation follows focus, so every focus
    /// change — a click-to-activate press, a taskbar activation, an
    /// open/close/minimize — keeps exactly one active frame. It early-returns
    /// when focus has not moved and is a no-op for an undecorated focus
    /// ([`set_active_frame`](Compositor::set_active_frame) returns `false`), so
    /// it costs nothing when no decorated window is involved.
    fn sync_active_frame(&mut self, compositor: &mut Compositor) {
        let focus = self.router.focused();
        if self.active_frame == focus {
            return;
        }
        if let Some(old) = self.active_frame {
            compositor.set_active_frame(old, false);
        }
        self.active_frame = match focus {
            Some(window) if compositor.set_active_frame(window, true) => Some(window),
            _ => None,
        };
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
        self.sync_active_frame(compositor);
        self.present(compositor);
        true
    }

    /// Minimise `window`: mark its taskbar entry minimised, hide it in the
    /// compositor, drop focus if it held it, and re-present the bar so the
    /// entry shows its minimised state.
    ///
    /// Returns `false`, changing nothing, when `window` is not a tracked task.
    /// This is the title-bar minimize control's path — it minimises
    /// unconditionally, where a taskbar click toggles.
    pub fn minimize_window(&mut self, compositor: &mut Compositor, window: WindowId) -> bool {
        if !self.tasks.minimize(
            compositor,
            &mut self.router,
            self.session.taskbar_mut(),
            window,
        ) {
            return false;
        }
        self.sync_active_frame(compositor);
        self.present(compositor);
        true
    }

    /// The session work area: the screen with the taskbar's band removed, in
    /// physical pixels at the output's density.
    ///
    /// A maximized window fills this rectangle, not the whole physical display
    /// (spec §5), so it never covers the taskbar. The bar hugs one full screen
    /// edge, so the work area is the screen minus that edge band.
    #[must_use]
    pub fn work_area(&self, compositor: &Compositor) -> Rect {
        let screen = compositor.screen_rect();
        let bar = self.session.taskbar().layout(compositor.scale()).bar;
        work_area_excluding(screen, bar)
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
    /// An embedder that switches the theme programmatically (through
    /// [`session_mut`](Self::session_mut) and
    /// [`set_theme`](DesktopSession::set_theme)) calls it, then
    /// [`present`](Self::present), to relay the switch itself.
    pub fn sync_background(&mut self, compositor: &mut Compositor) -> bool {
        compositor.set_background(self.desktop_background())
    }

    /// Bring the compositor up to date with the taskbar's current model and
    /// its owned theme: repaint and place the bar and, while the
    /// program-library popup is open, its panel.
    ///
    /// Only the surfaces the taskbar latched as changed are repainted. The
    /// latch is the taskbar's own record of what its model altered, so a
    /// pointer crossing one open menu repaints that menu and leaves the
    /// bar, popup, popover, and readout exactly as they are — the whole
    /// point being that re-rendering all five and pushing them back into
    /// the compositor costs milliseconds and damages five window
    /// rectangles. Draining the latch here, at the one site that presents,
    /// means no caller has to reason about which surfaces its edit
    /// touched, and nothing can be presented twice.
    ///
    /// Fails closed: a render whose surface cannot be
    /// allocated leaves the existing on-screen window untouched.
    pub fn present(&mut self, compositor: &mut Compositor) {
        let parts = self.session.taskbar_mut().take_repaint();
        self.presenter.present(
            compositor,
            &mut self.renderer,
            self.session.taskbar(),
            parts,
        );
    }

    /// Hand the taskbar's program-library popup the resolved `catalog` (the
    /// machine store merged with the user's overlay) and re-present, so an
    /// open popup refreshes in the same frame it is re-catalogued.
    pub fn set_library(&mut self, compositor: &mut Compositor, catalog: Catalog) {
        self.session
            .taskbar_mut()
            .library_mut()
            .set_catalog(catalog);
        self.present(compositor);
    }

    /// Hand the taskbar's pin strip the session's freshly resolved pin views
    /// and re-present, so the strip reflects an edit or a changed
    /// running-window match in the same frame.
    pub fn set_pins(&mut self, compositor: &mut Compositor, pins: Vec<PinView>) {
        self.session.taskbar_mut().set_pins(pins);
        self.present(compositor);
    }

    /// Relay a decoded notification request from an attested `producer` to the
    /// taskbar model, re-presenting when it changed the shown set.
    ///
    /// A [`Raise`](NotifyRequest::Raise) adds or updates the producer's
    /// notification in place; a [`Clear`](NotifyRequest::Clear) removes it.
    /// `producer` is the kernel-attested identity of the calling service — the
    /// embedder resolves it from the endpoint, never from the wire — so one
    /// producer can neither replace nor clear another's notification, and the
    /// title/body were validated by the notification-IPC decoder before this
    /// call. This holds no authority: the model only renders the message.
    pub fn apply_notify(
        &mut self,
        compositor: &mut Compositor,
        producer: u64,
        request: NotifyRequest,
    ) {
        let changed = match request {
            NotifyRequest::Raise {
                key,
                severity,
                title,
                body,
            } => self
                .session
                .taskbar_mut()
                .raise_notification(TransientNotification::new(
                    producer,
                    key,
                    severity,
                    title.as_str(),
                    body.as_str(),
                )),
            NotifyRequest::Clear { key } => {
                self.session.taskbar_mut().clear_notification(producer, key)
            }
        };
        if changed {
            self.present(compositor);
        }
    }

    /// Clear every notification a `producer` raised, re-presenting when any
    /// were removed — how the embedder drops a launched application's
    /// notifications when that child exits and can no longer clear them
    /// itself (the notification counterpart of a window client teardown).
    pub fn clear_producer_notifications(&mut self, compositor: &mut Compositor, producer: u64) {
        if self
            .session
            .taskbar_mut()
            .clear_producer_notifications(producer)
        {
            self.present(compositor);
        }
    }

    /// Relay a tray-signal summary from the attested Switchboard service to
    /// the taskbar capsule, re-presenting when it changed the rendering.
    ///
    /// `None` is the honest "no feed" — the service is absent or exited —
    /// and derives the calm state; the capsule never fabricates. The
    /// embedder attests the producer on the endpoint before this call, so
    /// the model only ever renders what the desktop's own service published.
    pub fn set_tray_summary(&mut self, compositor: &mut Compositor, summary: Option<TraySummary>) {
        self.session.taskbar_mut().set_tray_summary(summary);
        self.present(compositor);
    }

    /// Adopt the session's own count of unresponsive applications into the
    /// taskbar capsule, re-presenting when it changed the rendering.
    ///
    /// The count comes from the desktop's delivery evidence (the
    /// [`HangTracker`](crate::HangTracker)), not from the Switchboard
    /// service — the session is the one component that observes whether an
    /// app drains its window events.
    pub fn set_tray_unresponsive(&mut self, compositor: &mut Compositor, count: u16) {
        self.session.taskbar_mut().set_tray_unresponsive(count);
        self.present(compositor);
    }

    /// Attest to the taskbar whether this session can put a password prompt
    /// in front of the screen, so the system menu's *Lock Screen* row is
    /// offered only where locking could really be undone.
    ///
    /// Only the embedder knows: the answer turns on the console the session
    /// runs on, which it reads from the kernel. The bar refuses the row
    /// until told otherwise, so a session that never calls this offers no
    /// lock rather than one with no way back.
    pub fn set_lock_available(&mut self, compositor: &mut Compositor, available: bool) {
        self.session.taskbar_mut().set_lock_available(available);
        self.present(compositor);
    }

    /// Show, raise, and focus the running task shown as `window`, restoring
    /// it if it was minimised — how the embedder brings an already-open
    /// window forward instead of launching a second copy (the Files button's
    /// idempotent open). Returns `false`, changing nothing, when `window` is
    /// not a tracked task.
    pub fn raise_window(&mut self, compositor: &mut Compositor, window: WindowId) -> bool {
        let Some(task) = self.tasks.task_for(window) else {
            return false;
        };
        // Restore the model first (a minimised task is shown again), then
        // apply the activation to the compositor and highlight the task.
        self.session
            .taskbar_mut()
            .tasks_mut()
            .set_focused(Some(task));
        let raised = self.tasks.activate(
            compositor,
            &mut self.router,
            task,
            ActivateOutcome::Activated,
        );
        if raised {
            self.sync_active_frame(compositor);
            self.present(compositor);
        }
        raised
    }

    /// Route one input `event` to the desktop, resolving any time-driven
    /// taskbar gesture against the monotonic `now_ns`, and return what it
    /// did.
    ///
    /// The event is fanned to the window manager or taskbar by the
    /// [`SessionInputRouter`]'s policy; a taskbar action is applied where the
    /// shell's own state suffices (the task activate/minimise outcome drives
    /// the compositor) and the bar is re-presented so the screen reflects
    /// the change (an opened/closed popup, a changed task highlight, a
    /// hover). A window-manager action re-presents only when it moved focus
    /// (a press), keeping the highlight in step; motion and drags change no
    /// highlight and stay cheap. Pixel-only taskbar changes (hover, popup
    /// scroll or edit) latch the taskbar's repaint flag, drained here so
    /// exactly one present follows each visual change.
    ///
    /// Whatever the event did, the pointer cursor is then brought up to date
    /// ([`refresh_cursor`](Self::refresh_cursor)): its hotspot follows pointer
    /// motion and its shape follows what is under it (or the move cursor
    /// during a drag), so the desktop always shows a live pointer.
    pub fn handle(
        &mut self,
        event: InputEvent,
        compositor: &mut Compositor,
        now_ns: u64,
    ) -> ShellOutcome {
        let outcome =
            match self
                .router
                .handle(event, compositor, self.session.taskbar_mut(), now_ns)
            {
                SessionInputResponse::Ignored => ShellOutcome::Ignored,
                SessionInputResponse::WindowManager(response) => {
                    self.mirror_focus(&response, compositor);
                    ShellOutcome::WindowManager(response)
                }
                SessionInputResponse::Taskbar(response) => {
                    match response {
                        TaskbarResponse::TaskActivated { id, outcome } => {
                            self.tasks
                                .activate(compositor, &mut self.router, id, outcome);
                        }
                        // A user dismiss clears the notification from the model
                        // the session owns; the repaint latch it sets drives the
                        // one present below, closing the popover when the last one
                        // goes.
                        TaskbarResponse::DismissNotification { producer, key } => {
                            self.session.taskbar_mut().clear_notification(producer, key);
                        }
                        _ => {}
                    }
                    ShellOutcome::Taskbar(response)
                }
            };
        // One present per event, at one site. Every change to what a
        // taskbar surface draws — an acted response opening a popup, or a
        // pixel-only hover that produced no response at all — latches that
        // surface on the model, so presenting straight from the latch
        // repaints exactly what moved and nothing when the pointer merely
        // crossed dead space.
        self.present(compositor);
        // Whatever the event did to focus — a click-to-activate, a taskbar
        // activate/minimise, a desktop press — keep the decorated active frame
        // in step, so exactly the focused window shows its active title bar.
        self.sync_active_frame(compositor);
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
            // client press does, so the highlighted task follows it too; a
            // secondary (right) press activates+focuses the same way.
            InputResponse::Activated { window, .. }
            | InputResponse::SecondaryActivated { window, .. }
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
            // A client-area hover/drag/release acts on the already-focused
            // window (the press that armed the grab moved focus), so none of
            // them changes the highlighted task.
            | InputResponse::ClientPointerMoved { .. }
            | InputResponse::ClientPointerReleased { .. }
            | InputResponse::Ignored => return,
        };
        if self.tasks.sync_focus(self.session.taskbar_mut(), focus) {
            self.present(compositor);
        }
    }

    /// Drain every pending event from `source`, routing each through
    /// [`handle`](Self::handle) against the monotonic `now_ns`, and return
    /// their outcomes in order — coalescing an adjacent run of pointer
    /// motion over the same window into the newest sample.
    ///
    /// One drain is one instant: every event of this batch resolves against
    /// the same `now_ns`, which the embedder read when the source woke it.
    /// Every drained event still runs through [`handle`](Self::handle), so
    /// the window manager's own hover, drag, and cursor state track the full
    /// sample stream; only the *returned* outcome list is compressed. That
    /// is what makes the coalescing safe: it is the app-ward forwarding path
    /// (the embedder turns a [`ClientPointerMoved`](InputResponse::ClientPointerMoved)
    /// outcome into one event to the owning app) that a dense mouse sweep
    /// would otherwise flood with samples the app can only ever act on the
    /// newest of — pointer position is level-triggered, not edge-triggered.
    /// A run collapses only while it stays an unbroken sequence of
    /// [`ClientPointerMoved`](InputResponse::ClientPointerMoved) outcomes
    /// naming the *same* window: a press, a release, a scroll, a taskbar
    /// response, an [`Ignored`](ShellOutcome::Ignored), or a motion over a
    /// different window ends the run, so nothing the app must see (a
    /// `Moved, Moved, Released` sequence still delivers both a `Moved` and
    /// the `Released`) can ever be reordered or dropped.
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
        now_ns: u64,
    ) -> Result<Vec<ShellOutcome>, Errno>
    where
        S: InputSource + ?Sized,
    {
        let mut outcomes: Vec<ShellOutcome> = Vec::new();
        while let Some(event) = source.poll()? {
            let outcome = self.handle(event, compositor, now_ns);
            let collapses_into_last = pointer_moved_window(&outcome)
                .zip(outcomes.last().and_then(pointer_moved_window))
                .is_some_and(|(window, last_window)| window == last_window);
            if collapses_into_last {
                if let Some(last) = outcomes.last_mut() {
                    *last = outcome;
                }
            } else {
                outcomes.push(outcome);
            }
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
    /// later [`present`](Self::present) starts fresh, and wipe every rasterised
    /// cursor image, notification glyph, and window-furniture strip the
    /// seat's disposable-UI caches hold, along with any window content the
    /// compositor is still holding. Tearing the session down leaves no
    /// orphaned windows and no rendered user pixel data behind — the
    /// furniture carries window titles and the content is whatever the user
    /// was looking at, so both are overwritten rather than merely dropped.
    pub fn teardown(&mut self, compositor: &mut Compositor) {
        self.presenter.teardown(compositor);
        self.cursor.teardown();
        self.renderer.teardown();
        compositor.teardown_chrome();
        compositor.teardown_content();
    }
}

/// The window a `ClientPointerMoved` outcome names, or `None` for every
/// other outcome — the key [`pump`](DesktopShell::pump) collapses an
/// adjacent motion run on.
fn pointer_moved_window(outcome: &ShellOutcome) -> Option<WindowId> {
    match outcome {
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { window, .. }) => {
            Some(*window)
        }
        _ => None,
    }
}

/// The screen rectangle with the taskbar's edge band removed.
///
/// The taskbar hugs one full screen edge (top, bottom, left, or right), so the
/// work area is the screen minus that band. A bar that does not span a full
/// edge (it never should) leaves the whole screen usable rather than guessing.
fn work_area_excluding(screen: Rect, bar: Rect) -> Rect {
    if bar.width >= screen.width && bar.height < screen.height {
        // A horizontal bar on the top or bottom edge.
        let height = screen.height.saturating_sub(bar.height);
        if bar.top() <= screen.top() {
            Rect::new(screen.left(), bar.bottom(), screen.width, height)
        } else {
            Rect::new(screen.left(), screen.top(), screen.width, height)
        }
    } else if bar.height >= screen.height && bar.width < screen.width {
        // A vertical bar on the left or right edge.
        let width = screen.width.saturating_sub(bar.width);
        if bar.left() <= screen.left() {
            Rect::new(bar.right(), screen.top(), width, screen.height)
        } else {
            Rect::new(screen.left(), screen.top(), width, screen.height)
        }
    } else {
        screen
    }
}
