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

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::notify_ipc::NotifyRequest;
use tairix_abi::switchboard_ipc::TraySummary;
use tairix_abi::Errno;
use tairix_browse::{DirectorySource, GridView};
use tairix_cursor::{CursorRegistry, CursorSetId, CursorTheme};
use tairix_icon::{
    artwork_cache, ArtworkCache, ArtworkRasteriser, ArtworkReader, IconArtworkSource, IconSet,
};
use tairix_log::Sink;
use tairix_proglib::Catalog;
use tairix_reclaim::PressureGauge;
use tairix_taskbar::{
    icon_cache, ActivateOutcome, PinView, TaskbarConfig, TaskbarRenderer, TaskbarResponse,
    TransientNotification,
};
use tairix_wallpaper::Backdrop;
use tairix_wm::{
    cursor_cache, Color, Compositor, Corners, CursorController, InputEvent, InputResponse, Point,
    Rect, Scale, Surface, WindowActivationState, WindowFrame, WindowFurnitureState, WindowId,
    WindowSizeState,
};

use crate::desktop::Desktop;
use crate::input::{SessionInputResponse, SessionInputRouter};
use crate::pinboard::PinboardMenu;
use crate::pins::resolve_library_icons;
use crate::presenter::{place, TaskbarPresenter};
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
/// Not `Clone`: the shell owns the seat's rasterised-asset caches, and
/// each holds a live pressure gauge and audit sink behind trait objects.
/// Copying a shell would double-count their charged bytes against the
/// seat and give the copy a cache the reclaim ledger does not know about.
pub struct DesktopShell {
    session: DesktopSession,
    router: SessionInputRouter,
    presenter: TaskbarPresenter,
    renderer: TaskbarRenderer,
    tasks: TaskBridge,
    cursor: CursorController,
    /// The seat's decoded desktop icon artwork — the shipped
    /// `/System/Graphics` masters and each application bundle's own icon —
    /// keyed by asset path and pixel side.
    artwork: ArtworkCache,
    /// Where the artwork bytes are read from, and what turns them into
    /// pixels. Boxed because an embedder installs its own real seams (the
    /// VFS reader and the parser sandbox) over the starting pair, which
    /// find and decode nothing so a shell that was never given them draws
    /// entirely from built-in glyphs rather than reaching for I/O it does
    /// not hold.
    artwork_reader: Box<dyn ArtworkReader>,
    artwork_rasteriser: Box<dyn ArtworkRasteriser>,
    /// The decorated window currently shown with its active frame rim, kept
    /// in step with the window manager's focused window by
    /// [`sync_active_frame`](Self::sync_active_frame). `None` when focus rests
    /// on the desktop or an undecorated window.
    active_frame: Option<WindowId>,
    /// The screen-sized, already-fitted wallpaper the desktop layer is painted
    /// over, or `None` while the backdrop colour is the whole base. The
    /// embedder prepares it (it holds the read capability and the decode
    /// sandbox); the shell only blits it.
    wallpaper: Option<Surface>,
    /// The compositor window the pinboard's open context menu is shown in, if
    /// one is placed.
    pinboard_window: Option<WindowId>,
}

impl core::fmt::Debug for DesktopShell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The artwork seams are caller-supplied trait objects with no
        // formatting of their own; the cache is reported by what it costs.
        f.debug_struct("DesktopShell")
            .field("session", &self.session)
            .field("router", &self.router)
            .field("presenter", &self.presenter)
            .field("renderer", &self.renderer)
            .field("tasks", &self.tasks)
            .field("cursor", &self.cursor)
            .field("artwork_bytes", &self.artwork.charged_bytes())
            .field("active_frame", &self.active_frame)
            .finish_non_exhaustive()
    }
}

/// The artwork seams a shell starts with: nothing is found and nothing is
/// decoded, so every icon falls back to its built-in glyph until the
/// embedder installs the real filesystem and sandbox seams with
/// [`DesktopShell::set_artwork_source`].
///
/// One type for both halves because both are the same refusal; two would be
/// the same emptiness written twice.
struct NoArtworkSeam;

impl ArtworkReader for NoArtworkSeam {
    fn read(&mut self, _path: &str) -> Option<Vec<u8>> {
        None
    }
}

impl ArtworkRasteriser for NoArtworkSeam {
    fn rasterise(&mut self, _side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        None
    }
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
    /// session drives; every rasterised-asset cache derives its budget
    /// from it, so a 4K desktop is allowed proportionately more cached
    /// pixels than a small panel and none carries a hand-picked
    /// ceiling. `seat` owns those bytes in the reclaim ledger, `pressure`
    /// is the gauge the kernel reports the memory-pressure band into, and
    /// `sink` receives the caches' audit records.
    ///
    /// The shipped icon artwork starts unreachable — every icon draws its
    /// built-in glyph — until the embedder installs the seams that can read
    /// and decode it with [`set_artwork_source`](Self::set_artwork_source).
    #[must_use]
    pub fn new(
        config: TaskbarConfig,
        seat: u64,
        output_bytes: usize,
        pressure: &'static (dyn PressureGauge + 'static),
        sink: &'static (dyn Sink + Sync),
    ) -> Self {
        let artwork = artwork_cache(
            "session.desktop-artwork",
            seat,
            output_bytes,
            pressure,
            sink,
        );
        let icons = icon_cache(seat, output_bytes, pressure, sink);
        let cursors = cursor_cache(seat, output_bytes, pressure, sink);
        // All three caches are *this* process's memory: the taskbar's glyphs
        // and the window manager's cursors are composed here, out of the seat
        // and output size only the session knows, and the crates that define
        // them are libraries linked into this binary rather than processes of
        // their own — so the session is what a cache monitor must charge, and
        // this is the one point that knows all three. A cache declared
        // unclassifiable has no ledger and simply contributes no row.
        //
        // Only the running session binary reports: the host-testable
        // construction path the unit tests share links no reporter at all.
        #[cfg(freestanding)]
        for ledger in [artwork.ledger(), icons.ledger(), cursors.ledger()]
            .into_iter()
            .flatten()
        {
            tairix_rt::cachereport::register(ledger);
        }
        Self {
            session: DesktopSession::new(config),
            router: SessionInputRouter::new(),
            presenter: TaskbarPresenter::new(),
            renderer: TaskbarRenderer::new(icons),
            tasks: TaskBridge::new(),
            cursor: CursorController::new(cursors),
            artwork,
            artwork_reader: Box::new(NoArtworkSeam),
            artwork_rasteriser: Box::new(NoArtworkSeam),
            active_frame: None,
            wallpaper: None,
            pinboard_window: None,
        }
    }

    /// Install the seams the desktop's shipped and bundle-supplied icon
    /// artwork is read and decoded through: `reader` fetches the asset
    /// bytes and `rasteriser` turns them into pixels.
    ///
    /// The embedder supplies the real pair — the session's file reader and
    /// the parser sandbox that decodes untrusted image bytes outside this
    /// address space. Until it does, both refuse, and every icon draws its
    /// built-in glyph; a refusal after they are installed does the same, so
    /// no slot can blank whatever the store holds.
    pub fn set_artwork_source(
        &mut self,
        reader: Box<dyn ArtworkReader>,
        rasteriser: Box<dyn ArtworkRasteriser>,
    ) {
        self.artwork_reader = reader;
        self.artwork_rasteriser = rasteriser;
    }

    /// The artwork cache and the two seams it resolves through, borrowed
    /// together.
    ///
    /// One accessor rather than three because a caller needs all three at
    /// once (they are the arguments of a single resolution) and three
    /// separate borrows of the same shell cannot be held together.
    pub fn artwork_parts(
        &mut self,
    ) -> (
        &mut ArtworkCache,
        &mut dyn ArtworkReader,
        &mut dyn ArtworkRasteriser,
    ) {
        (
            &mut self.artwork,
            self.artwork_reader.as_mut(),
            self.artwork_rasteriser.as_mut(),
        )
    }

    /// Give back every rasterised pixel the current memory-pressure band
    /// requires, returning the bytes released across the seat's caches.
    ///
    /// Called when the kernel wakes the session with a deepened band, so
    /// the desktop releases its share at the moment pressure rises rather
    /// than at whatever later frame happens to touch a cache. A band that
    /// demands nothing releases nothing, so a spurious wake is cheap.
    ///
    /// `compositor` is trimmed alongside the shell's own caches because
    /// the desktop's remaining disposable-UI cache — the decorated
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
            .saturating_add(self.artwork.trim())
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

    /// Open `surface` as an undecorated window at `origin`, raise it above
    /// everything, and focus it — *without* listing it on the taskbar.
    ///
    /// This is how an app-owned popup surface (a context menu, a settings
    /// sheet) is placed. A popup belongs to the window that opened it, so it is
    /// not a running task of its own: it carries no taskbar entry, no title
    /// bar, and no window controls, and the owning app dismisses it. Focus
    /// moves to it so the keys the popup exists to collect arrive there, which
    /// deactivates the parent's frame exactly as the session's own trusted
    /// modal surfaces do.
    ///
    /// Returns the new [`WindowId`]. It cannot fail: no task id is minted, so
    /// there is no id space to exhaust.
    pub fn open_popup_window(
        &mut self,
        compositor: &mut Compositor,
        origin: Point,
        surface: Surface,
    ) -> WindowId {
        let window = compositor.add_window(origin, surface);
        compositor.raise(window);
        self.router.focus(window, compositor);
        self.sync_active_frame(compositor);
        window
    }

    /// Close the popup window `window` opened by
    /// [`open_popup_window`](Self::open_popup_window): remove it from the
    /// compositor and drop focus if it held it.
    ///
    /// Returns `false`, changing nothing, for a window the compositor does not
    /// know. The taskbar is untouched — a popup never had an entry there —
    /// so the bar needs no re-present.
    pub fn close_popup_window(&mut self, compositor: &mut Compositor, window: WindowId) -> bool {
        if !compositor.remove(window) {
            return false;
        }
        if self.router.focused() == Some(window) {
            self.router.unfocus();
        }
        self.sync_active_frame(compositor);
        true
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
    /// create): when set, the window manager offers a live maximize/restore
    /// size toggle and the invisible resize edges — a grab zone over the
    /// client's own outer pixels, so an edge costs no visible space — and the
    /// app re-lays-out to each new client size it reports. A fixed-size app
    /// passes `false` — every client pixel reaches it and the size toggle is
    /// inert — so the mechanism is per-app opt-in, never forced on an app that
    /// renders at one size.
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

    /// Retitle `window`: relabel its taskbar entry and, when it wears the
    /// window-manager frame, its title bar, then re-present the bar so the
    /// new label shows.
    ///
    /// One call moves both, so an app that renames its window can never
    /// leave the bar naming the old subject. The session's own undecorated
    /// trusted surfaces (the file picker) relabel on the bar alone — they
    /// have no title bar to move. Returns `false`, changing nothing, when
    /// `window` is not a tracked task.
    pub fn retitle_window(
        &mut self,
        compositor: &mut Compositor,
        window: WindowId,
        title: &str,
    ) -> bool {
        if !self
            .tasks
            .retitle(compositor, self.session.taskbar_mut(), window, title)
        {
            return false;
        }
        self.present(compositor);
        true
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
        // An open popup's shown rows get their applications' own icons
        // resolved before the paint, so a row that has just scrolled into
        // view is drawn with its icon in the same frame.
        resolve_library_icons(
            self.session.taskbar_mut(),
            compositor.scale(),
            self.artwork_reader.as_mut(),
            self.artwork_rasteriser.as_mut(),
            &mut self.artwork,
        );
        let parts = self.session.taskbar_mut().take_repaint();
        let mut artwork = IconArtworkSource::new(
            &mut self.artwork,
            self.artwork_reader.as_mut(),
            self.artwork_rasteriser.as_mut(),
        );
        self.presenter.present(
            compositor,
            &mut self.renderer,
            self.session.taskbar(),
            parts,
            &mut artwork,
        );
    }

    /// Install the screen-sized, already-fitted `wallpaper` the desktop layer
    /// is painted over, or `None` to leave the backdrop colour as the whole
    /// base. The next [`present_desktop`](Self::present_desktop) shows it.
    ///
    /// The embedder prepares the pixels: it holds the capability to read the
    /// user's chosen image and the sandbox that decodes it, and it fits them
    /// to the screen through the one shared placement. The shell blits what it
    /// is handed and parses nothing.
    pub fn set_wallpaper(&mut self, wallpaper: Option<Surface>) {
        self.wallpaper = wallpaper;
    }

    /// Repaint the desktop layer — the wallpaper (or the backdrop colour the
    /// settings name) with the icon column over it — beneath every window.
    ///
    /// The layer is repainted in place, into the screen-sized buffer the
    /// compositor already holds, because a hover, a moved selection, or a
    /// re-list repaints it often and a whole screen of pixels is not something
    /// to re-allocate per frame. It is opaque and covers the screen: the
    /// wallpaper is blitted at the origin, and the backdrop colour is laid down
    /// first wherever the wallpaper does not reach (which, with no wallpaper at
    /// all, is everywhere). The icons are then drawn over it, laid out in the
    /// work area — which excludes the taskbar's band — so nothing is ever drawn
    /// under the bar. Each icon's artwork is resolved through the same
    /// seat-wide cache and seams the taskbar draws from, at exactly the slot
    /// side the tile will paint it in, and an icon the store cannot supply
    /// falls back to its built-in glyph.
    ///
    /// Fails closed: a screen-sized layer the heap will not give back leaves
    /// the desktop exactly as it was rather than blanking it.
    pub fn present_desktop<S: DirectorySource>(
        &mut self,
        compositor: &mut Compositor,
        desktop: &Desktop<S>,
    ) {
        let screen = compositor.screen_rect();
        let scale = compositor.scale();
        let layout = self.desktop_layout(compositor, desktop);
        let backdrop = self.backdrop_colour(desktop.settings().backdrop);
        let theme = self.session.active_theme();
        let wallpaper = self.wallpaper.as_ref();
        let cache = &mut self.artwork;
        let reader = self.artwork_reader.as_mut();
        let rasteriser = self.artwork_rasteriser.as_mut();
        compositor.repaint_desktop(|surface| {
            let covered = wallpaper.is_some_and(|paper| {
                paper.width() >= screen.width && paper.height() >= screen.height
            });
            if !covered {
                surface.fill_rect(0, 0, screen.width, screen.height, backdrop);
            }
            if let Some(paper) = wallpaper {
                surface.blit(0, 0, paper);
            }
            let mut artwork = IconArtworkSource::new(cache, reader, rasteriser);
            desktop.render(surface, &layout, scale, theme, &mut artwork);
        });
    }

    /// The colour the desktop layer shows wherever the wallpaper does not
    /// reach: the active theme's own desktop colour, or the flat colour the
    /// user's settings name.
    fn backdrop_colour(&self, backdrop: Backdrop) -> Color {
        match backdrop {
            Backdrop::Theme => self.desktop_background(),
            Backdrop::Colour(colour) => Color::rgb(colour.r, colour.g, colour.b),
        }
    }

    /// Show the pinboard's open context `menu` as its own compositor window,
    /// or take that window down when the menu is closed.
    ///
    /// The plate is placed through the same shared window placement the
    /// taskbar's own menu uses, and rounded with the popup radius the menu's
    /// plate is painted with, so the window and its pixels cannot disagree
    /// about where the rounding is.
    ///
    /// Fails closed: a plate surface the heap will not give back leaves what is
    /// on screen untouched rather than showing an empty window.
    pub fn present_pinboard_menu(&mut self, compositor: &mut Compositor, menu: &PinboardMenu) {
        let scale = compositor.scale();
        let theme = self.session.active_theme();
        let Some(bounds) = self.pinboard_menu_bounds(compositor, menu) else {
            if let Some(id) = self.pinboard_window.take() {
                compositor.remove(id);
            }
            return;
        };
        let Some(mut surface) = Surface::new(bounds.width, bounds.height) else {
            return;
        };
        let local = Rect::new(0, 0, bounds.width, bounds.height);
        menu.render(&mut surface, local, scale, theme);
        let corners = Corners::from_radius(scale.scale_length(theme.metrics().popup_corner_radius));
        self.pinboard_window = Some(place(
            compositor,
            self.pinboard_window,
            bounds.origin,
            surface,
            corners,
        ));
    }

    /// The compositor window the pinboard's context menu is shown in, if one is
    /// placed.
    #[must_use]
    pub const fn pinboard_window(&self) -> Option<WindowId> {
        self.pinboard_window
    }

    /// The plate the pinboard's open context `menu` occupies on this output,
    /// or `None` when the menu is closed.
    ///
    /// The one place the plate is measured, so what
    /// [`present_pinboard_menu`](Self::present_pinboard_menu) paints and what
    /// a press is routed against can never be two different rectangles. The
    /// shell owns the active theme the measurement needs, which is why it is
    /// asked here rather than reconstructed by the embedder.
    #[must_use]
    pub fn pinboard_menu_bounds(
        &self,
        compositor: &Compositor,
        menu: &PinboardMenu,
    ) -> Option<Rect> {
        menu.layout(
            compositor.screen_rect(),
            compositor.scale(),
            self.session.active_theme(),
        )
    }

    /// The grid the desktop's icons occupy on this output: the shared column
    /// laid out in the work area at the output's density, arranged from the
    /// edge the user's settings name.
    ///
    /// The one place the desktop's geometry is decided, so what a press is
    /// hit-tested against and what [`present_desktop`](Self::present_desktop)
    /// paints can never be two different grids.
    #[must_use]
    pub fn desktop_layout<S: DirectorySource>(
        &self,
        compositor: &Compositor,
        desktop: &Desktop<S>,
    ) -> GridView {
        desktop.layout(
            self.work_area(compositor),
            compositor.scale(),
            self.session.active_theme(),
        )
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

    /// Attest to the taskbar whether this session can step aside for another
    /// user, so the system menu's *Switch User…* row exists only where the
    /// session could really be resumed afterwards.
    ///
    /// Only the embedder knows: the answer turns on whether it holds the
    /// wake mailbox a session authority resumes this desktop through. The
    /// bar leaves the row out until told otherwise, so a session that never
    /// calls this offers no switch rather than a one-way trip to the login
    /// screen.
    pub fn set_switch_user_available(&mut self, compositor: &mut Compositor, available: bool) {
        self.session
            .taskbar_mut()
            .set_switch_user_available(available);
        self.present(compositor);
    }

    /// Re-lay the bar for an output whose extent changed under the session,
    /// and repaint every surface the desktop owns over the new screen.
    ///
    /// The compositor must already have adopted the new mode: the desktop's
    /// icons and the bar are placed against the extent it reports, so laying
    /// them out first would place them on the old screen. Used when a
    /// session that stepped aside comes back to a display the next account
    /// re-moded.
    pub fn set_output_layout(&mut self, config: TaskbarConfig, compositor: &mut Compositor) {
        self.session.taskbar_mut().set_config(config);
        self.present(compositor);
        self.refresh_cursor(compositor);
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
            | InputResponse::WindowControlAlternate { window, .. }
            | InputResponse::FurniturePressed { window } => Some(window),
            InputResponse::DesktopPressed => None,
            // A secondary press on the backdrop opens the pinboard's context
            // menu; it neither raises a window nor drops the focused one, so
            // the highlighted task stands.
            InputResponse::DesktopSecondaryPressed
            // A drag, resize, command-control activation, key, or scroll acts
            // on the already-focused window (the press that started it moved
            // focus); a hover or key that reached the desktop rather than a
            // window leaves focus where the last press put it. Neither moves
            // the highlighted task.
            | InputResponse::DesktopPointerMoved
            | InputResponse::DesktopKey { .. }
            | InputResponse::Moved { .. }
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
    /// their outcomes in order — folding an adjacent run of one continuing
    /// gesture over the same window into a single outcome.
    ///
    /// One drain is one instant: every event of this batch resolves against
    /// the same `now_ns`, which the embedder read when the source woke it.
    /// Every drained event still runs through [`handle`](Self::handle), so
    /// the window manager's own hover, drag, and cursor state track the full
    /// sample stream; only the *returned* outcome list is compressed. That
    /// is what makes the folding safe: it is the app-ward forwarding path
    /// (the embedder turns each outcome into one event to the owning app)
    /// that a dense gesture would otherwise flood with samples the app must
    /// then drain one at a time from a bounded mailbox.
    ///
    /// Two gestures fold, each by the rule its own quantity obeys: pointer
    /// motion carries a position, which is level-triggered, so the newest
    /// sample supersedes the ones before it; wheel ticks carry a delta,
    /// which is additive, so a run in one direction sums into a single
    /// tick that leaves the app's scroll model exactly where the run
    /// would. A reversal is a distinct gesture — a tick that clamps at a
    /// range end is not recovered by the tick back — so it ends the run.
    ///
    /// A run holds only while it stays an unbroken
    /// sequence of the same foldable outcome naming the *same* window: a
    /// press, a release, a taskbar response, an
    /// [`Ignored`](ShellOutcome::Ignored), a switch between motion and
    /// scrolling, or one over a different window ends the run, so nothing
    /// the app must see (a `Moved, Moved, Released` sequence still delivers
    /// both a `Moved` and the `Released`) can ever be reordered or dropped.
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
            let unfolded = match outcomes.last_mut() {
                Some(last) => fold_outcome(last, outcome),
                None => Some(outcome),
            };
            if let Some(outcome) = unfolded {
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
    /// cursor image, notification glyph, decoded icon, and window-furniture
    /// strip the seat's disposable-UI caches hold, along with any window
    /// content the compositor is still holding. Tearing the session down
    /// leaves no orphaned windows and no rendered user pixel data behind — the
    /// furniture carries window titles and the content is whatever the user
    /// was looking at, so both are overwritten rather than merely dropped.
    pub fn teardown(&mut self, compositor: &mut Compositor) {
        self.presenter.teardown(compositor);
        self.cursor.teardown();
        self.renderer.teardown();
        self.artwork.teardown();
        compositor.teardown_chrome();
        compositor.teardown_content();
    }
}

/// Fold `next` into `last` when the two are one continuing gesture over the
/// same window — motion by latest-wins, wheel ticks by summing a run in one
/// direction — returning [`None`] once folded and `Some(next)` when it must
/// be kept as its own outcome. [`DesktopShell::pump`] states the rules.
fn fold_outcome(last: &mut ShellOutcome, next: ShellOutcome) -> Option<ShellOutcome> {
    match (&mut *last, next) {
        (
            ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { window: into, .. }),
            moved @ ShellOutcome::WindowManager(InputResponse::ClientPointerMoved {
                window, ..
            }),
        ) if *into == window => {
            *last = moved;
            None
        }
        (
            ShellOutcome::WindowManager(InputResponse::AppScroll {
                window: into,
                dx: sideways,
                dy: downward,
            }),
            ShellOutcome::WindowManager(InputResponse::AppScroll { window, dx, dy }),
        ) if *into == window && continues(*sideways, dx) && continues(*downward, dy) => {
            *sideways = sideways.saturating_add(dx);
            *downward = downward.saturating_add(dy);
            None
        }
        (_, next) => Some(next),
    }
}

/// Whether `next` continues a run of `so_far` on one axis: a zero on either
/// side adds nothing to decide, and two ticks of the same sign are one
/// gesture. Opposite signs are not.
///
/// The one definition both wheel folds share — this drain's, and the
/// [`crate::holdback`] queue's, which folds again across drains when a
/// destination is behind.
pub(crate) const fn continues(so_far: i32, next: i32) -> bool {
    so_far == 0 || next == 0 || (so_far < 0) == (next < 0)
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
