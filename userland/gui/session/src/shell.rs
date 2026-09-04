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
use tairix_controls::damage::{self, Repaint};
use tairix_cursor::{CursorRegistry, CursorSetId, CursorTheme};
use tairix_geometry::Region;
use tairix_icon::{
    artwork_cache, ArtworkCache, ArtworkRasteriser, ArtworkReader, ArtworkResolver,
    IconArtworkSource, IconKind, IconRequest, IconSet, InlineArtwork,
};
use tairix_log::Sink;
use tairix_proglib::Catalog;
use tairix_reclaim::PressureGauge;
use tairix_taskbar::{
    icon_cache, AppSlot, Edge, TaskbarConfig, TaskbarRenderer, TaskbarResponse,
    TransientNotification,
};
use tairix_theme::MotionInteraction;
use tairix_wallpaper::Backdrop;
use tairix_wm::{
    cursor_cache, Color, Compositor, Corners, CursorController, InputEvent, InputResponse,
    Modifiers, Point, Rect, Scale, Surface, WindowActivationState, WindowFrame,
    WindowFurnitureState, WindowId, WindowSizeState,
};

use crate::apps::{picker_cells, prefetch_bar_icons, resolve_library_icons, thumbnail};
use crate::desktop::Desktop;
use crate::fade::BackdropFade;
use crate::input::{SessionInputResponse, SessionInputRouter};
use crate::menu::{MenuChain, SurfaceKind};
use crate::presenter::{chrome_blur, TaskbarPresenter};
use crate::session::DesktopSession;
use crate::tasks::TaskBridge;
use crate::thumbs::WindowThumbnails;
use crate::windows::chain_geometry;

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
    /// How a miss in that cache becomes pixels — a read of the asset plus a
    /// decode of it, on a worker thread where the embedder has one. Boxed
    /// because the embedder installs its own over a starting resolver that
    /// finds and decodes nothing, so a shell that was never given one draws
    /// entirely from built-in glyphs rather than reaching for I/O it does not
    /// hold.
    artwork_resolver: Box<dyn ArtworkResolver>,
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
    /// The ground [`wallpaper`](Self::wallpaper) is dissolving out of, while
    /// it is: the whole screen changing at once is never cut to.
    backdrop_fade: BackdropFade,
    /// The compositor window each surface of the open menu chain is drawn in.
    ///
    /// Reconciled against the chain's own list on every present, so a plate
    /// the chain no longer has cannot be left on the screen and none has to
    /// be taken down by a second path.
    menu_windows: Vec<(SurfaceKind, WindowId)>,
    /// The owner window those surfaces were opened under, so a chain that
    /// displaced another under a different owner cannot inherit them.
    menu_owner: Option<WindowId>,
    /// The hover window picker's thumbnails, scaled a window at a time while
    /// the pointer rests out the picker's opening dwell.
    thumbs: WindowThumbnails,
    /// The per-frame shell work counted so far, so a test can prove a
    /// drained batch settles once rather than once per sample. Test-only:
    /// the product carries no counter.
    #[cfg(test)]
    settled: SettleWork,
}

/// How many times the shell has run each piece of its per-frame work.
///
/// A drained batch of pointer samples must cost one taskbar present, one
/// active-frame sync, and one cursor refresh however many samples it held,
/// and "how much work" is the only load-independent way to assert that.
#[cfg(test)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SettleWork {
    /// Calls into [`DesktopShell::present`].
    pub(crate) presents: u32,
    /// Calls into `DesktopShell::sync_active_frame`.
    pub(crate) active_frame_syncs: u32,
    /// Calls into [`DesktopShell::refresh_cursor`].
    pub(crate) cursor_refreshes: u32,
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
/// embedder installs the real resolver with
/// [`DesktopShell::set_artwork_resolver`].
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
    /// and decode it with [`set_artwork_resolver`](Self::set_artwork_resolver).
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
            artwork_resolver: Box::new(InlineArtwork::new(NoArtworkSeam, NoArtworkSeam)),
            active_frame: None,
            wallpaper: None,
            backdrop_fade: BackdropFade::default(),
            menu_windows: Vec::new(),
            menu_owner: None,
            thumbs: WindowThumbnails::new(),
            #[cfg(test)]
            settled: SettleWork::default(),
        }
    }

    /// The per-frame shell work run so far, which a test diffs across a
    /// drain to count what one batch cost.
    #[cfg(test)]
    pub(crate) const fn settle_work(&self) -> SettleWork {
        self.settled
    }

    /// Install what the desktop's shipped and bundle-supplied icon artwork is
    /// produced through: a read of the asset bytes plus a decode of them.
    ///
    /// The embedder supplies the real one — the session's file reader and the
    /// parser sandbox that decodes untrusted image bytes outside this address
    /// space, either on the calling thread or on a worker. Until it does,
    /// resolution refuses and every icon draws its built-in glyph; a refusal
    /// after it is installed does the same, so no slot can blank whatever the
    /// store holds.
    pub fn set_artwork_resolver(&mut self, resolver: Box<dyn ArtworkResolver>) {
        self.artwork_resolver = resolver;
    }

    /// The artwork cache and the resolver it produces misses through,
    /// borrowed together.
    ///
    /// One accessor rather than two because a caller needs both at once (they
    /// are the arguments of a single resolution) and two separate borrows of
    /// the same shell cannot be held together.
    pub fn artwork_parts(&mut self) -> (&mut ArtworkCache, &mut dyn ArtworkResolver) {
        (&mut self.artwork, self.artwork_resolver.as_mut())
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
            .saturating_add(compositor.trim_frost())
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
    /// pointer, the double arrow of a resize edge the pointer is on or a
    /// resize-grab is dragging, the move cursor during an interactive
    /// move-grab), re-rasterising at
    /// the output density only when the chosen shape, the active cursor set,
    /// or the [`scale`](Compositor::scale) actually changed, and repositions
    /// the overlay so its hotspot tracks the pointer. It is called for you
    /// once per settled frame ([`handle`](Self::handle) / [`pump`](Self::pump))
    /// and on a runtime
    /// [`set_scale`](Self::set_scale); the embedder calls it once after the
    /// first [`present`](Self::present) to show the pointer at start-up.
    ///
    /// Fails closed: if the chosen shape cannot be rasterised the current
    /// cursor is left untouched rather than blanking the pointer.
    pub fn refresh_cursor(&mut self, compositor: &mut Compositor) {
        #[cfg(test)]
        {
            self.settled.cursor_refreshes += 1;
        }
        let at = self.router.pointer();
        self.cursor.refresh(at, self.router.wm(), compositor);
        compositor.move_cursor(at);
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
            .set_registry(registry, self.router.pointer(), compositor);
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

    /// Open `surface` as an undecorated window at `origin` belonging to
    /// `parent`, and focus it — *without* listing it on the taskbar.
    ///
    /// This is how an app-owned popup surface (a context menu, a settings
    /// sheet) is placed. A popup belongs to the window that opened it, so it is
    /// not a running task of its own: it carries no taskbar entry, no title
    /// bar, and no window controls, and the owning app dismisses it. Focus
    /// moves to it so the keys the popup exists to collect arrive there, which
    /// deactivates the parent's frame exactly as the session's own trusted
    /// modal surfaces do.
    ///
    /// It is opened as its parent's *transient*
    /// ([`Compositor::add_transient_window`]), so the window manager stacks
    /// the pair together from here on and nothing has to re-assert the
    /// arrangement per frame. Returns `None`, opening nothing, for a parent
    /// the compositor does not know.
    pub fn open_popup_window(
        &mut self,
        compositor: &mut Compositor,
        parent: WindowId,
        origin: Point,
        surface: Surface,
    ) -> Option<WindowId> {
        let window = compositor.add_transient_window(parent, origin, surface)?;
        self.router.focus(window, compositor);
        self.sync_active_frame(compositor);
        Some(window)
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
        #[cfg(test)]
        {
            self.settled.active_frame_syncs += 1;
        }
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
    /// (spec §5), so it never covers the taskbar. The bar occupies a band on
    /// one screen edge, so the work area is the screen minus that band.
    #[must_use]
    pub fn work_area(&self, compositor: &Compositor) -> Rect {
        let screen = compositor.screen_rect();
        let taskbar = self.session.taskbar();
        let bar = taskbar.layout(compositor.scale()).bar;
        work_area_excluding(screen, bar, taskbar.config().edge)
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
        #[cfg(test)]
        {
            self.settled.presents += 1;
        }
        // Which surface the pointer rests on is resolved against the window
        // stack, so it goes stale whenever the stack does — a window opened,
        // closed, raised or hidden, a popover of the bar's placed or removed —
        // and none of those is a pointer event. Every such change ends here,
        // because that is how the screen is brought up to date, so this is the
        // one place the answer can be refreshed without a caller having to
        // remember to. It runs *before* the paint for two reasons: the stack
        // it resolves against is the one currently on screen, which is what
        // the pointer is actually resting on; and a hover it drops or takes up
        // latches a repaint that this very present then draws, rather than one
        // waiting for a frame that may never come.
        self.router
            .refresh_pointer_focus(compositor, self.session.taskbar_mut(), &self.presenter);
        // An open popup's shown rows get their applications' own icons
        // resolved before the paint, so a row that has just scrolled into
        // view is drawn with its icon in the same frame.
        resolve_library_icons(
            self.session.taskbar_mut(),
            compositor.scale(),
            self.artwork_resolver.as_mut(),
            &mut self.artwork,
        );
        let parts = self.session.taskbar_mut().take_repaint();
        let mut artwork = IconArtworkSource::new(&mut self.artwork, self.artwork_resolver.as_mut());
        self.presenter.present(
            compositor,
            &mut self.renderer,
            self.session.taskbar(),
            parts,
            &mut artwork,
        );
    }

    /// Start decoding every icon the bar's surfaces will draw, so the surface
    /// drawing them is never the first thing to ask for them.
    ///
    /// A decode is a read plus a sandbox round trip. Left to the paint, a
    /// launcher opening on twenty applications shows twenty built-in glyphs and
    /// replaces them a round trip per icon later; asked for here — when the
    /// catalog or the application strip that names them changes, which is long
    /// before
    /// either surface is shown — the wait is already over. Nothing is drawn and
    /// nothing is waited for, and a shell whose resolver produces on the
    /// calling thread prefetches nothing at all.
    pub fn warm_icon_artwork(&mut self, compositor: &Compositor) {
        prefetch_bar_icons(
            self.session.taskbar(),
            compositor.scale(),
            self.artwork_resolver.as_mut(),
            &mut self.artwork,
        );
    }

    /// Start decoding the icon of every application `bundles` names, at the two
    /// sides a window of it will wear it in — its title band and its taskbar
    /// slot.
    ///
    /// A window opens a good while after its application was launched: a spawn,
    /// a load, and the app's own bring-up. Asking on the wake that recorded the
    /// launch therefore has the picture ready by the time there is a window to
    /// put it on; left to the window-open pass, a window appears wearing the
    /// shared application glyph and swaps it for its own a decode later.
    pub fn warm_launched_artwork<'a>(
        &mut self,
        compositor: &Compositor,
        bundles: impl Iterator<Item = &'a str>,
    ) {
        let sides = [
            self.session.taskbar().app_icon_side(compositor.scale()),
            compositor.title_identity_icon_side(),
        ];
        for bundle in bundles {
            let request = IconRequest::bundle(IconKind::AppBundle, bundle);
            for side in sides {
                self.artwork
                    .prefetch(self.artwork_resolver.as_mut(), request, side);
            }
        }
    }

    /// Repaint the bar's icon-bearing surfaces, because artwork a previous
    /// paint fell back to a built-in glyph for has since been decoded.
    ///
    /// The desktop's own icon column is the embedder's to repaint (it owns the
    /// model the column is laid out from); this is the taskbar half of the same
    /// answer, so a decode that lands is drawn wherever it belongs without the
    /// embedder having to know which of the bar's five surfaces show icons.
    pub fn present_icon_artwork(&mut self, compositor: &mut Compositor) {
        self.session.taskbar_mut().request_icon_repaint();
        self.present(compositor);
    }

    /// Install the screen-sized, already-fitted `wallpaper` the desktop layer
    /// is painted over, or `None` to leave the backdrop colour as the whole
    /// base. The next [`present_desktop`](Self::present_desktop) shows it.
    ///
    /// The embedder prepares the pixels: it holds the capability to read the
    /// user's chosen image and the sandbox that decodes it, and it fits them
    /// to the screen through the one shared placement. The shell blits what it
    /// is handed and parses nothing.
    ///
    /// The new ground dissolves into place rather than replacing the old one
    /// between two frames — see [`BackdropFade`] — so this needs the clock and
    /// the `backdrop` the settings name, which is both the colour under a
    /// picture's margins and the whole ground when there is no picture. The
    /// desktop layer then owes a repaint per frame until
    /// [`backdrop_settled`](Self::backdrop_settled).
    pub fn set_wallpaper(&mut self, wallpaper: Option<Surface>, backdrop: Backdrop, now_ns: u64) {
        let leaving = self
            .wallpaper
            .take()
            .and_then(|picture| flatten_ground(&picture, self.backdrop_colour(backdrop)));
        self.wallpaper = wallpaper;
        // Neither ground has a picture in it, so there is nothing to dissolve
        // between: a desktop with no wallpaper, told again that it has none,
        // arms no timer and repaints nothing.
        if leaving.is_none() && self.wallpaper.is_none() {
            return;
        }
        let span = self
            .session
            .active_theme()
            .motion()
            .duration(MotionInteraction::BackdropChange);
        self.backdrop_fade.begin(now_ns, span, leaving);
    }

    /// Whether the backdrop has finished arriving, so the desktop layer owes
    /// no further repaint of its own.
    #[must_use]
    pub const fn backdrop_settled(&self) -> bool {
        self.backdrop_fade.settled()
    }

    /// Step the backdrop's crossfade to `now_ns`, answering whether the ground
    /// changed and the desktop layer owes a repaint.
    pub fn advance_backdrop(&mut self, now_ns: u64) -> bool {
        self.backdrop_fade.advance(now_ns)
    }

    /// `park_ns` shortened to the backdrop crossfade's next frame, or left as
    /// it is when nothing is dissolving.
    #[must_use]
    pub fn backdrop_park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        self.backdrop_fade.park_deadline_ns(now_ns, park_ns)
    }

    /// Repaint the whole desktop layer — the wallpaper (or the backdrop colour
    /// the settings name) with the icon column over it — beneath every window.
    ///
    /// This is for the changes that genuinely alter the whole layer: bring-up,
    /// a new wallpaper, a theme switch, a re-list that moved the icons. A
    /// change confined to some icons goes through
    /// [`present_desktop_area`](Self::present_desktop_area) instead.
    ///
    /// The layer is repainted in place, into the screen-sized buffer the
    /// compositor already holds, so even a whole-layer repaint costs a paint
    /// rather than a paint plus a multi-megabyte allocation. Icons are laid out
    /// in the work area — which excludes the taskbar's band — so nothing is
    /// ever drawn under the bar. Each icon's artwork is resolved through the
    /// same seat-wide cache and seams the taskbar draws from, at exactly the
    /// slot side the tile will paint it in, and an icon the store cannot supply
    /// falls back to its built-in glyph.
    ///
    /// Fails closed: a screen-sized layer the heap will not give back leaves
    /// the desktop exactly as it was rather than blanking it.
    pub fn present_desktop<S: DirectorySource>(
        &mut self,
        compositor: &mut Compositor,
        desktop: &Desktop<S>,
    ) {
        let whole = Region::from(compositor.screen_rect());
        self.present_desktop_area(compositor, desktop, &whole);
    }

    /// Repaint just the parts of the desktop layer covered by `area`.
    ///
    /// This is the ordinary path, and [`present_desktop`](Self::present_desktop)
    /// is the whole-screen case of it: the desktop's own gestures report the
    /// icon cells they changed, so taking the hover, moving a selection, or
    /// gaining and losing the keyboard costs those cells. Repainting the layer
    /// whole for a highlight would recomposite every window above it and blur
    /// every frosted backdrop over it again, which is why the model reports
    /// cells rather than a flag.
    ///
    /// Every rectangle is painted the same way, so a partial repaint and a
    /// whole one cannot disagree about a pixel: the backdrop colour goes down
    /// first, the ground composites over it, and the icons that reach the
    /// rectangle draw over that. Laying the colour down under the wallpaper is
    /// what makes a letterboxed or centred picture show the user's chosen
    /// backdrop in the margins it does not cover, rather than whatever the
    /// previous frame happened to leave there. Mid-crossfade the ground is the
    /// mix of the two grounds, laid down by the one ground routine every
    /// rectangle paints through, so a rectangle repainted for a hover during a
    /// fade still comes out the same as its neighbours.
    pub fn present_desktop_area<S: DirectorySource>(
        &mut self,
        compositor: &mut Compositor,
        desktop: &Desktop<S>,
        area: &Region,
    ) {
        let screen = compositor.screen_rect();
        let scale = compositor.scale();
        let layout = self.desktop_layout(compositor, desktop);
        let backdrop = self.backdrop_colour(desktop.settings().backdrop);
        let theme = self.session.active_theme();
        let wallpaper = self.wallpaper.as_ref();
        let fade = &self.backdrop_fade;
        let cache = &mut self.artwork;
        let resolver = self.artwork_resolver.as_mut();
        compositor.repaint_desktop(area, |surface, rects| {
            let mut artwork = IconArtworkSource::new(cache, resolver);
            for rect in rects {
                // The layer sits at the screen origin and every rectangle is
                // clipped to it, so its corner is a surface coordinate; a
                // rectangle that somehow is not addressable is left alone
                // rather than painted somewhere else.
                let (Ok(x), Ok(y)) = (u32::try_from(rect.left()), u32::try_from(rect.top())) else {
                    continue;
                };
                surface.with_clip(x, y, rect.width, rect.height, |surface| {
                    surface.fill_rect(0, 0, screen.width, screen.height, backdrop);
                    paint_ground(surface, wallpaper, fade);
                    desktop.render(surface, &layout, scale, theme, &mut artwork, *rect);
                });
            }
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

    /// Bring the compositor's windows into line with the open menu chain:
    /// draw every plate and the information panel it lists, and take down
    /// everything it no longer has.
    ///
    /// Reconciliation rather than a set of add/remove calls, so a surface can
    /// never outlive the state that placed it and no path has to remember to
    /// take one down. A closed chain lists nothing, which is what clears the
    /// screen.
    ///
    /// Plates are composited as **transients of the owner window** when the
    /// chain has one, so they ride its stacking rather than re-asserting an
    /// arrangement per frame. A chain the desktop opened for itself has no
    /// owner and its plates are ordinary session surfaces.
    ///
    /// Every surface of the chain is desktop chrome like the bar and its
    /// popups: it is painted on the session's floating theme and asks the
    /// compositor to frost what is behind it by the same
    /// `chrome_backdrop_blur`. The rounding and the frosting are applied to
    /// every surface on one path, whichever way its window was obtained, so a
    /// re-used plate cannot keep a look the chain no longer asks for.
    ///
    /// **A surface already on screen keeps its pixels and is repainted only
    /// where the chain says they changed.** A plate is retained chrome, so
    /// moving a highlight costs the row the mark left and the row it arrived
    /// on — two rows re-derived and two rectangles marked — where re-handing
    /// the compositor a freshly rendered plate would allocate one, paint all
    /// of it, and mark the whole rectangle for recomposition over a frosted
    /// backdrop. A surface that has not changed at all is not touched, which
    /// is what keeps a highlight moving on the root plate from repainting its
    /// open submenu and the information panel beside it.
    ///
    /// Fails closed: a plate surface the heap will not give back leaves what
    /// is on screen untouched rather than showing an empty window, and the
    /// chain keeps what that surface owed so the next pass paints it.
    /// Returns `false` when a plate could not be given a surface, which the
    /// caller answers the chain's owner `NoResources` for: a chain the desktop
    /// cannot draw is refused honestly rather than left half on the screen.
    pub fn present_menu_chain(
        &mut self,
        compositor: &mut Compositor,
        chain: &mut MenuChain,
        owner: Option<WindowId>,
    ) -> bool {
        // A plate is composited as its owner's transient, so a chain opened
        // under a different owner cannot inherit the displaced chain's
        // windows: re-used, they would ride the wrong family's stacking.
        if self.menu_owner != owner {
            for (_, id) in core::mem::take(&mut self.menu_windows) {
                self.drop_menu_window(compositor, id);
            }
            self.menu_owner = owner;
        }
        let scale = compositor.scale();
        let geom = chain_geometry(&self.session, compositor);
        let corners =
            Corners::from_radius(scale.scale_length(geom.theme.metrics().popup_corner_radius));
        let blur = chrome_blur(geom.theme);
        let mut kept: Vec<(SurfaceKind, WindowId)> = Vec::new();
        let mut drawn = true;
        for placed in chain.surfaces() {
            let size = (placed.rect.width, placed.rect.height);
            let live = self
                .menu_windows
                .iter()
                .find(|(kind, _)| *kind == placed.kind)
                .map(|(_, id)| *id)
                .filter(|id| compositor.window(*id).is_some());
            let painted = if let Some(id) = live {
                // Moved before it is painted, so the rectangles the repaint
                // marks are the ones the surface now occupies.
                compositor.move_window(id, placed.rect.origin);
                let area = match placed.repaint {
                    Repaint::Whole => Region::from(Rect::new(0, 0, size.0, size.1)),
                    Repaint::Parts(parts) => parts,
                };
                compositor
                    .repaint_window(id, size, &area, |surface, rects| {
                        damage::paint_parts(surface, rects, |surface| {
                            chain.render_surface(placed.kind, surface, &geom);
                        });
                    })
                    .then_some(id)
            } else {
                let Some(mut pixels) = Surface::new(size.0, size.1) else {
                    drawn = false;
                    continue;
                };
                chain.render_surface(placed.kind, &mut pixels, &geom);
                match owner {
                    Some(parent) => {
                        compositor.add_transient_window(parent, placed.rect.origin, pixels)
                    }
                    None => Some(compositor.add_window(placed.rect.origin, pixels)),
                }
            };
            let Some(id) = painted else {
                drawn = false;
                continue;
            };
            compositor.set_corners(id, corners);
            compositor.set_backdrop_blur(id, blur);
            kept.push((placed.kind, id));
            chain.presented(placed.kind);
        }
        for (kind, id) in core::mem::take(&mut self.menu_windows) {
            if !kept.iter().any(|(live, _)| *live == kind) {
                self.drop_menu_window(compositor, id);
            }
        }
        self.menu_windows = kept;
        self.sync_active_frame(compositor);
        drawn
    }

    /// Take one of the chain's compositor windows down, giving the keyboard
    /// back if the router was pointing at it.
    fn drop_menu_window(&mut self, compositor: &mut Compositor, id: WindowId) {
        compositor.remove(id);
        if self.router.focused() == Some(id) {
            self.router.unfocus();
        }
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

    /// Hand the taskbar's application strip the session's freshly resolved
    /// slots and re-present, so the strip reflects a launch, an exit, a
    /// window opening or closing, or a re-declaration in the same frame.
    pub fn set_apps(&mut self, compositor: &mut Compositor, apps: Vec<AppSlot>) {
        self.session.taskbar_mut().set_apps(apps);
        self.present(compositor);
    }

    /// Open the bar's hover window picker over the application at strip
    /// index `app` with one cell per window, and re-present.
    ///
    /// The answer to
    /// [`ShowWindowPicker`](TaskbarResponse::ShowWindowPicker): the session
    /// owns the windows' pixels, so it builds the cells and the bar places
    /// and draws them. The thumbnails are whatever
    /// [`advance_window_thumbnails`](Self::advance_window_thumbnails)
    /// prepared while the pointer rested out the dwell — ordinarily all of
    /// them; a cell whose window was not reached yet opens on its
    /// application's glyph and is filled by a later slice. Refused, changing
    /// nothing, for fewer cells than there is a choice between.
    pub fn show_window_picker(&mut self, compositor: &mut Compositor, app: usize) {
        let scale = compositor.scale();
        let entries = {
            let (taskbar, thumbs) = (self.session.taskbar(), &mut self.thumbs);
            picker_cells(taskbar, app, |window| thumbs.take(window))
        };
        self.session
            .taskbar_mut()
            .show_window_picker(app, entries, scale);
        self.present(compositor);
    }

    /// Scale one window frame the hover window picker wants, returning
    /// whether the screen changed.
    ///
    /// The embedder calls this once per turn of its serve loop. It prepares
    /// the application the pointer is resting its dwell out on — so the
    /// picker opens already drawn — and goes on filling the cells of an open
    /// picker whose slices had not all landed. One window per turn is what
    /// keeps a screenful of windows from stopping the loop: the desktop
    /// serves input, presents, and IPC between the slices.
    ///
    /// A pointer resting on nothing drops the preparation, so prepared pixels
    /// are held only while there is a picker to show them in.
    pub fn advance_window_thumbnails(&mut self, compositor: &mut Compositor) -> bool {
        let open = self.session.taskbar().picker().app();
        let Some(app) = self.router.dwelling_app().or(open) else {
            self.thumbs.forget();
            return false;
        };
        let Some(windows) = self
            .session
            .taskbar()
            .apps()
            .get(app)
            .map(|slot| slot.windows().to_vec())
        else {
            self.thumbs.forget();
            return false;
        };
        self.thumbs.aim(app, &windows);
        let Some(window) = self.thumbs.next_owed() else {
            return false;
        };
        let (width, height) = self
            .session
            .taskbar()
            .picker_thumbnail_size(compositor.scale());
        // A window that has not presented, or whose content the pressure
        // ladder released, has no pixels to scale: its cell keeps the
        // application's glyph and the remaining slices carry on.
        let Some(scaled) = self
            .tasks
            .window_for(window)
            .and_then(|wm| compositor.window(wm))
            .and_then(tairix_wm::Window::content)
            .and_then(|frame| thumbnail(frame, width, height))
        else {
            return false;
        };
        if open != Some(app) {
            self.thumbs.store(window, scaled);
            return false;
        }
        // The picker is already up over this application, so the cell that
        // has been showing a glyph takes the thumbnail now.
        let cell = self
            .session
            .taskbar()
            .picker()
            .entries()
            .iter()
            .position(|entry| entry.window() == window);
        let Some(cell) = cell else {
            return false;
        };
        if !self
            .session
            .taskbar_mut()
            .set_picker_thumbnail(cell, scaled)
        {
            return false;
        }
        self.present(compositor);
        true
    }

    /// Whether the hover window picker is owed a thumbnail, so the embedder's
    /// next park is due immediately.
    ///
    /// True from the moment the pointer comes to rest on a multi-window slot
    /// — the queue for it has not been built yet, which is itself work — until
    /// the last of that application's windows has been scaled.
    #[must_use]
    pub fn window_thumbnails_owed(&self) -> bool {
        let open = self.session.taskbar().picker().app();
        match self.router.dwelling_app().or(open) {
            Some(app) => self.thumbs.owed() || !self.thumbs.aimed_at(app),
            None => false,
        }
    }

    /// `park_ns` shortened to the next moment a time-driven bar gesture is
    /// due — the hover picker's opening dwell or closing grace, a held
    /// Switchboard capsule press — or left exactly as it is when none is
    /// pending.
    #[must_use]
    pub fn taskbar_park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        self.router.park_deadline_ns(now_ns, park_ns)
    }

    /// Resolve whatever timed bar gesture has come due at `now_ns`,
    /// re-presenting whatever it changed.
    ///
    /// The embedder calls this when the deadline
    /// [`taskbar_park_deadline_ns`](Self::taskbar_park_deadline_ns) asked for
    /// expires. A tick resolves the hover picker's own two edges — opening
    /// once the pointer has rested out its dwell, closing once it has rested
    /// elsewhere out the grace — and both are the shell's to carry out, so
    /// there is nothing here for the embedder to route.
    pub fn tick_taskbar(&mut self, compositor: &mut Compositor, now_ns: u64) {
        let response = self.router.tick(self.session.taskbar_mut(), now_ns);
        self.resolve_taskbar(&response, compositor);
        // A picker that went down is a surface going away: the pointer's
        // focus is re-resolved and the latched repaint drawn by the one
        // settling present.
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
        if self.session.taskbar_mut().set_tray_summary(summary) {
            self.present(compositor);
        }
    }

    /// Adopt the session's own count of unresponsive applications into the
    /// taskbar capsule, re-presenting when it changed the rendering.
    ///
    /// The count comes from the desktop's delivery evidence (the
    /// [`HangTracker`](crate::HangTracker)), not from the Switchboard
    /// service — the session is the one component that observes whether an
    /// app drains its window events.
    pub fn set_tray_unresponsive(&mut self, compositor: &mut Compositor, count: u16) {
        if self.session.taskbar_mut().set_tray_unresponsive(count) {
            self.present(compositor);
        }
    }

    /// Name the signed-in account to the taskbar, so its trailing capsule
    /// wears that account's identity disc, re-presenting when it changed the
    /// rendering.
    ///
    /// Only the embedder knows who is signed in. A session that never calls
    /// this leaves the capsule on the mark a nameless account gets, rather
    /// than an initial nobody chose.
    pub fn set_account(&mut self, compositor: &mut Compositor, name: &str) {
        if self.session.taskbar_mut().set_account(name) {
            self.present(compositor);
        }
    }

    /// Attest to the taskbar whether this session's console has a
    /// re-authentication broker, so every menu row that needs one is
    /// offered only where choosing it could really act: the system menu's
    /// *Lock Screen* row, and the clock menu's set-time row.
    ///
    /// Only the embedder knows: the answer turns on the console the session
    /// runs on, which it reads from the kernel. The bar refuses those rows
    /// until told otherwise, so a session that never calls this offers
    /// neither a lock with no way back nor a clock command that could only
    /// fail.
    pub fn set_elevation_available(&mut self, compositor: &mut Compositor, available: bool) {
        self.session
            .taskbar_mut()
            .set_elevation_available(available);
        self.present(compositor);
    }

    /// Put `label` on the taskbar clock and re-present the bar.
    ///
    /// The embedder owns the wall clock — the bar carries no time ABI of its
    /// own — so it spells the reading and hands over the text. A machine whose
    /// wall time has never been set is spelled as the bar's own placeholder
    /// rather than a fabricated reading, so the clock stays visible and
    /// pressable.
    pub fn set_clock_label(&mut self, compositor: &mut Compositor, label: &str) {
        self.session.taskbar_mut().clock_mut().set_label(label);
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
    /// window forward instead of launching a second copy (the file manager's
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
        let raised = self.tasks.raise(compositor, &mut self.router, task);
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
    /// The event is applied and the frame it changed is then settled — one
    /// taskbar present, one active-frame sync, one cursor refresh. This is the
    /// single-event entry point; a whole batch of queued events goes through
    /// [`pump`](Self::pump), which applies each and settles the batch once, or
    /// through [`apply`](Self::apply) plus one [`settle`](Self::settle) where
    /// the embedder drains a channel itself.
    pub fn handle(
        &mut self,
        event: InputEvent,
        compositor: &mut Compositor,
        now_ns: u64,
    ) -> ShellOutcome {
        let outcome = self.apply(event, compositor, now_ns);
        self.settle(compositor);
        outcome
    }

    /// Apply one input `event` to the desktop's state, without presenting.
    ///
    /// The event is fanned to the window manager or taskbar by the
    /// [`SessionInputRouter`]'s policy, and a taskbar action is applied where
    /// the shell's own state suffices (the task activate/minimise outcome
    /// drives the compositor). Everything a surface *draws* is left latched on
    /// the taskbar model for `settle` to drain, so applying a
    /// run of samples costs one repaint of each surface that moved rather than
    /// one per sample.
    ///
    /// An embedder that drains its own channel (so it can pair each event
    /// with the device record the event carries) applies each event here in
    /// order and calls [`settle`](Self::settle) once for the batch.
    pub fn apply(
        &mut self,
        event: InputEvent,
        compositor: &mut Compositor,
        now_ns: u64,
    ) -> ShellOutcome {
        match self.router.handle(
            event,
            compositor,
            self.session.taskbar_mut(),
            &self.presenter,
            now_ns,
        ) {
            SessionInputResponse::Ignored => ShellOutcome::Ignored,
            SessionInputResponse::WindowManager(response) => {
                self.mirror_focus(&response);
                ShellOutcome::WindowManager(response)
            }
            SessionInputResponse::Taskbar(response) => {
                self.resolve_taskbar(&response, compositor);
                ShellOutcome::Taskbar(response)
            }
        }
    }

    /// Carry out the part of a taskbar `response` the shell's own state
    /// suffices for, before the rest is handed to the embedder.
    ///
    /// The one site: an event and a timed
    /// [`tick_taskbar`](Self::tick_taskbar) resolve the same response the
    /// same way.
    fn resolve_taskbar(&mut self, response: &TaskbarResponse, compositor: &mut Compositor) {
        match *response {
            // Choosing a window — in the hover picker, or through the
            // Switchboard capsule's cycle and previous-task gestures —
            // raises it. The bar has already restored and highlighted it in
            // its own model.
            TaskbarResponse::WindowChosen { id } => {
                self.tasks.raise(compositor, &mut self.router, id);
            }
            // A user dismiss clears the notification from the model the
            // session owns; the repaint latch it sets drives the settling
            // present, closing the popover when the last one goes.
            TaskbarResponse::DismissNotification { producer, key } => {
                self.session.taskbar_mut().clear_notification(producer, key);
            }
            // The pointer finished its dwell on a multi-window slot: the
            // shell holds the thumbnails it scaled while that dwell ran, so
            // it opens the picker itself.
            TaskbarResponse::ShowWindowPicker { app } => {
                self.show_window_picker(compositor, app);
            }
            _ => {}
        }
    }

    /// Give the pointer up, because the embedder is taking the input stream
    /// away from the shell for a modal surface of its own — the screen lock, or
    /// the pinboard's backdrop menu.
    ///
    /// Both of those drain the seat's channels straight into themselves so that
    /// nothing behind the plate can be reached, which means the shell will not
    /// see the release that ends a gesture already in flight. This says so: the
    /// gesture ends, and both routers are told the pointer has left, so nothing
    /// sits there with a control lit under a plate the user is looking at. It is
    /// idempotent — the drain says it on every pass rather than working out
    /// which pass was the first — and the stream coming back needs no
    /// announcement, because the next event resolves the pointer afresh.
    pub fn yield_pointer(&mut self, compositor: &mut Compositor) {
        self.router
            .yield_pointer(compositor, self.session.taskbar_mut());
    }

    /// Bring the screen up to date with everything the applied events left
    /// changed: one taskbar present, one active-frame sync, one cursor
    /// refresh.
    ///
    /// All three read current state rather than an event, so running them
    /// once after a run of samples lands the same desktop as running them
    /// after each: the present repaints the union of surfaces the samples
    /// latched, the sync compares the *current* focus against the shown
    /// active frame, and the cursor's shape and hotspot follow the pointer's
    /// latest position. Skipping the intermediate passes therefore drops work
    /// nothing could observe — no frame was published between them.
    pub fn settle(&mut self, compositor: &mut Compositor) {
        // Every change to what a taskbar surface draws — an acted response
        // opening a popup, or a pixel-only hover that produced no response at
        // all — latches that surface on the model, so presenting straight from
        // the latch repaints exactly what moved and nothing when the pointer
        // merely crossed dead space.
        self.present(compositor);
        // Whatever the events did to focus — a click-to-activate, a taskbar
        // activate/minimise, a desktop press — keep the decorated active frame
        // in step, so exactly the focused window shows its active title bar.
        self.sync_active_frame(compositor);
        // The hotspot follows pointer motion and the shape follows what is
        // under it (or the move cursor during a drag), so the desktop always
        // shows a live pointer.
        self.refresh_cursor(compositor);
    }

    /// Keep the bar's highlighted task in step when the window manager moves
    /// focus by a direct click on a window or the desktop.
    ///
    /// A press that focused a window or the desktop relays the new focus into
    /// the task list, which latches the bar's repaint only when the
    /// highlighted task actually moved; a drag ([`Moved`](InputResponse::Moved)
    /// / [`MoveEnded`](InputResponse::MoveEnded)), a no-op, or a press on a
    /// window that owns no task changes no highlight and is left cheap.
    fn mirror_focus(&mut self, response: &InputResponse) {
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
        let _ = self.tasks.sync_focus(self.session.taskbar_mut(), focus);
    }

    /// Drain every pending event from `source`, applying each against the
    /// monotonic `now_ns`, and return their outcomes in order — folding an
    /// adjacent run of one continuing gesture over the same window into a
    /// single outcome.
    ///
    /// One drain is one instant: every event of this batch resolves against
    /// the same `now_ns`, which the embedder read when the source woke it.
    /// Every drained event is still applied in order, so the window
    /// manager's own hover, drag, and cursor state track the full sample
    /// stream; only the *returned* outcome list is compressed. That
    /// is what makes the folding safe: it is the app-ward forwarding path
    /// (the embedder turns each outcome into one event to the owning app)
    /// that a dense gesture would otherwise flood with samples the app must
    /// then drain one at a time from a bounded mailbox.
    ///
    /// The shell's *own* per-frame work is folded by the same reasoning: the
    /// batch is settled once at the end rather than
    /// after each event, so a burst of N motion samples costs one taskbar
    /// present, one active-frame sync, and one cursor refresh instead of N of
    /// each. Nothing observes the intermediate passes — the embedder publishes
    /// one frame per drain — and all three read current state, so the desktop
    /// this leaves is the one N settles would have left. A drain that found no
    /// event settles nothing, keeping an idle wake free.
    ///
    /// Three gestures fold, each by the rule its own quantity obeys: pointer
    /// motion carries a position, which is level-triggered, so the newest
    /// sample supersedes the ones before it; an interactive resize names a
    /// geometry, likewise level-triggered, and the embedder reads the window's
    /// *current* client extent when it forwards one — so every sample of a run
    /// would carry the size the last one settled on, and all but the last are
    /// duplicates by construction; wheel ticks carry a delta, which is
    /// additive, so a run in one direction sums into a single tick that leaves
    /// the app's scroll model exactly where the run would. A reversal is a
    /// distinct gesture — a tick that clamps at a range end is not recovered
    /// by the tick back — so it ends the run.
    ///
    /// [`ResizeEnded`](InputResponse::ResizeEnded) is not part of that run: it
    /// is the settle the app must witness, so it ends one like a release does.
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
        let mut applied = false;
        let drained = loop {
            match source.poll() {
                Ok(Some(event)) => {
                    let outcome = self.apply(event, compositor, now_ns);
                    applied = true;
                    let unfolded = match outcomes.last_mut() {
                        Some(last) => fold_outcome(last, outcome),
                        None => Some(outcome),
                    };
                    if let Some(outcome) = unfolded {
                        outcomes.push(outcome);
                    }
                }
                Ok(None) => break Ok(outcomes),
                Err(err) => break Err(err),
            }
        };
        // The events applied before a fault are already in the desktop's
        // state, so the frame is settled either way: a faulting source must
        // not leave the bar and cursor showing a state the model has left.
        if applied {
            self.settle(compositor);
        }
        drained
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

    /// The modifiers the seat currently holds, for stamping onto the pointer
    /// events the session delivers to applications.
    #[must_use]
    pub const fn modifiers(&self) -> Modifiers {
        self.router.modifiers()
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
        compositor.teardown_frost();
        compositor.teardown_content();
    }
}

/// Lay the desktop layer's ground over the backdrop colour already filled:
/// the `wallpaper`, and — while one ground is dissolving into another — the
/// ground being left, over it.
///
/// Both mid-fade cases come out as the straight mix of the two grounds. The
/// arriving picture lands at the fade's strength when the ground being left is
/// the plain colour the layer is already filled with; otherwise the arriving
/// ground is painted whole and the ground being left goes back over it at the
/// inverse strength. Either way an arrived fade costs one blit, exactly as it
/// did before there was a fade at all.
fn paint_ground(surface: &mut Surface, wallpaper: Option<&Surface>, fade: &BackdropFade) {
    let Some(strength) = fade.arriving() else {
        if let Some(paper) = wallpaper {
            surface.blit(0, 0, paper);
        }
        return;
    };
    match fade.leaving() {
        None => {
            if let Some(paper) = wallpaper {
                surface.blit_faded(0, 0, paper, strength);
            }
        }
        Some(previous) => {
            if let Some(paper) = wallpaper {
                surface.blit(0, 0, paper);
            }
            surface.blit_faded(0, 0, previous, u8::MAX - strength);
        }
    }
}

/// The ground a `picture` makes over `colour`: the two flattened into one
/// opaque surface, or `None` when the heap will not give one back.
///
/// A crossfade lays the ground it is leaving back over the ground it is
/// arriving at, and that only mixes to the right colour if what it lays is
/// opaque — a picture that letterboxes, or one with alpha of its own, would
/// otherwise let the arriving ground through at full strength in exactly the
/// places the outgoing one showed the backdrop colour.
///
/// Fails closed: a desktop that cannot afford the copy fades the arriving
/// ground in over the colour instead of holding a wrong one.
fn flatten_ground(picture: &Surface, colour: Color) -> Option<Surface> {
    let mut ground = Surface::new(picture.width(), picture.height())?;
    ground.fill(colour);
    ground.blit(0, 0, picture);
    Some(ground)
}

/// Fold `next` into `last` when the two are one continuing gesture over the
/// same window — motion and resize by latest-wins, wheel ticks by summing a
/// run in one direction — returning [`None`] once folded and `Some(next)`
/// when it must be kept as its own outcome. [`DesktopShell::pump`] states the
/// rules.
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
            ShellOutcome::WindowManager(InputResponse::Resized { window: into }),
            ShellOutcome::WindowManager(InputResponse::Resized { window }),
        ) if *into == window => None,
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

/// The screen rectangle with the band the taskbar occupies on `edge` removed.
///
/// Public because a host-side observer must reconstruct the same rectangle a
/// window is placed onto ([`windows::placed_outer`](crate::windows::placed_outer)),
/// and re-deriving it there would be a second definition of where the bar's
/// band ends.
///
/// The band runs from that screen edge to the bar's inner side, so it covers
/// the bar *and* the wallpaper margin the bar floats above: the gap is behind
/// the bar from the work area's point of view, and a window given it could
/// only be reached through the bar.
///
/// The bar's own edge is asked for rather than inferred from its rectangle,
/// because a floating bar spans no screen edge to infer it from. A bar the
/// screen does not contain leaves the whole screen usable rather than
/// guessing.
#[must_use]
pub fn work_area_excluding(screen: Rect, bar: Rect, edge: Edge) -> Rect {
    let clamp = |value: i32, lo: i32, hi: i32| value.max(lo).min(hi);
    let span = |from: i32, to: i32| u32::try_from(to.saturating_sub(from)).unwrap_or(0);
    match edge {
        Edge::Top => {
            let top = clamp(bar.bottom(), screen.top(), screen.bottom());
            Rect::new(screen.left(), top, screen.width, span(top, screen.bottom()))
        }
        Edge::Bottom => {
            let bottom = clamp(bar.top(), screen.top(), screen.bottom());
            Rect::new(
                screen.left(),
                screen.top(),
                screen.width,
                span(screen.top(), bottom),
            )
        }
        Edge::Left => {
            let left = clamp(bar.right(), screen.left(), screen.right());
            Rect::new(
                left,
                screen.top(),
                span(left, screen.right()),
                screen.height,
            )
        }
        Edge::Right => {
            let right = clamp(bar.left(), screen.left(), screen.right());
            Rect::new(
                screen.left(),
                screen.top(),
                span(screen.left(), right),
                screen.height,
            )
        }
    }
}
