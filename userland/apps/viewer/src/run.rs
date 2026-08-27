//! The `viewer.app` bundle's `Run` entry point (`plans/APPWIN.md` AW5):
//! the windowed read-only file viewer — the first consumer of the desktop
//! session's trusted file picker and the CU6 one-shot file delegation
//! (`plans/CAPABILITY_USE.md`).
//!
//! # The capability story (why this app exists)
//!
//! The viewer's manifest requests **no filesystem capability**: on its
//! own it can open, list, and stat nothing. At startup it asks the
//! session to run its trusted picker (`WindowClient::pick_file`); the
//! user browses in the *session's* UI under the *session's* authority,
//! and the viewer receives exactly one conclusion on its event channel —
//! a `FilePicked` carrying a one-shot `fd_grant` handle, or a
//! `PickCancelled`. Redeeming the handle (`fd_redeem`) installs a
//! read-only descriptor whose reads the kernel authorises under the
//! session's captured identity, so the viewer reads exactly the one file
//! the user chose and nothing else — the user-mediated file capability,
//! end to end.
//!
//! # What the program wires (and what stays in the library)
//!
//! The bounded, sanitising byte→line model, the pointer- and
//! keyboard-driven [`tairix_viewer::Viewer`] composition, and the themed
//! renderers all live in the host-tested `tairix_viewer` engine; this
//! binary composes them over the live syscalls exactly as the files app
//! does: one `shm_create`d frame region granted to the window endpoint,
//! one `port_bind`-bound event mailbox parked on through a wait-set
//! (every accepted event authenticated against the session identity the
//! create reply named), and the `WindowClient` calls over `ipc_call`.
//! Wire pointer events are translated into `tairix_input::InputEvent`s
//! through the one shared `tairix_window::pointer_input_events` mapping and
//! routed into the viewer's single pointer entry point, so clicking the
//! window's "Open…" button, dragging its scrollbar, or turning the wheel
//! all work exactly as they draw; pressing `Enter` asks for a pick the same
//! way the button does. A `CloseRequested` from the desktop ends the
//! program cleanly.
//! Every bring-up refusal exits fail-loud with a reserved code and a
//! stated reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    extern crate alloc;

    use alloc::vec::Vec;

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode};
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, DOCUMENT_ROLE_ARG, ORIGIN_WIRE_LEN, STDIN,
    };
    use tairix_display::{winframe, SERIAL};
    use tairix_geometry::{Point, Scale};
    use tairix_rt::io::{Stderr, Write};
    use tairix_theme::ThemeRegistry;
    use tairix_viewer::{
        Viewer, ViewerPointerOutcome, CONTENT_MAX, MIN_WIN_HEIGHT, MIN_WIN_WIDTH, WIN_HEIGHT,
        WIN_WIDTH,
    };
    use tairix_window::{
        pointer_input_events, Desktop, EventSource, WindowClient, WindowEvents, WindowFrames,
        WindowSizing, WindowTransport,
    };

    /// Exit code when the shared frame region could not be created or
    /// granted to the window endpoint. A reserved, fail-closed value.
    const EXIT_NO_FRAMES: i32 = 81;

    /// Exit code when the event mailbox could not be bound or observed
    /// through the wait-set. A reserved, fail-closed value: the app
    /// exits rather than degrade into a busy re-poll.
    const EXIT_NO_EVENTS: i32 = 82;

    /// Exit code when the desktop session refused the window create (no
    /// graphical session, or the channel refused the geometry). A
    /// reserved, fail-closed value.
    const EXIT_NO_WINDOW: i32 = 83;

    /// Exit code when a present was refused or the event channel died
    /// (the session went away). A reserved, fail-closed value.
    const EXIT_CHANNEL_LOST: i32 = 84;

    /// Frames in the shared region. The window protocol serialises a
    /// present (the app is parked in the call while the session reads),
    /// so a single frame is race-free; the constant names the choice.
    const FRAME_COUNT: u32 = 1;

    /// The wait-set token of the event-mailbox member.
    const EVENT_TOKEN: u64 = 1;

    /// The wait-set token of the memory-pressure member: the kernel wakes the
    /// park when the machine's pressure band changes, so the glyph cache is
    /// trimmed as memory tightens instead of being held until something else
    /// is starved.
    const PRESSURE_TOKEN: u64 = 2;

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit
    /// code alone is not a diagnosis) and hand back `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "viewer: {reason}");
        code
    }

    /// Declare this viewer's presence on the desktop's icon bar: the shared
    /// convention's two rows — the session-drawn information row and *Quit* —
    /// with the primary click left to the session so it raises the window.
    ///
    /// A refused declaration is an answer, not a death: the viewer says so
    /// and carries on with no slot of its own — its window is still reachable
    /// through the one the session derives from it.
    fn declare_app_bar(client: &mut WindowClient<RtWindowTransport>, endpoint: u64) {
        match tairix_window::info_and_quit(endpoint) {
            Ok(bar) => {
                if let Err(err) = client.set_app_bar(&bar) {
                    let _ = writeln!(
                        Stderr,
                        "viewer: the desktop refused this application's icon-bar presence \
                         ({err}); carrying on without one"
                    );
                }
            }
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "viewer: this application's icon-bar menu is invalid ({err:?}); carrying \
                     on without one"
                );
            }
        }
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to
    /// the reserved window endpoint per request. The session attests the
    /// caller kernel-side on every request, so the transport carries no
    /// claimed authority.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The production [`EventSource`]: drain the app's own event
    /// mailbox, parking on the wait-set whenever it is empty, and accept
    /// only events whose kernel-attested sender is the desktop session
    /// named by the create reply — anything else is dropped (fail
    /// closed), so no other process can feed the app forged input or a
    /// forged pick conclusion.
    struct RtEventSource {
        /// The app's event-mailbox endpoint id.
        endpoint: u64,
        /// The wait-set handle the app parks on.
        set: u64,
        /// The only sender whose events are accepted.
        server: ProcId,
    }

    impl EventSource for RtEventSource {
        fn next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno> {
            loop {
                let mut sender = [0u8; ORIGIN_WIRE_LEN];
                match tairix_rt::ipc_recv(self.endpoint, event, &mut sender) {
                    Ok(len) => {
                        // A short frame or a foreign sender is dropped,
                        // never delivered: the mailbox is open to any
                        // capable sender, so the kernel-attested origin
                        // is the authentication.
                        if len != WindowEvent::WIRE_LEN {
                            continue;
                        }
                        let Ok(origin) = Origin::from_bytes(&sender) else {
                            continue;
                        };
                        if origin.proc_id() != self.server {
                            continue;
                        }
                        return Ok(());
                    }
                    Err(err) if Errno::from_syscall(err) == Errno::WouldBlock => {
                        // Nothing queued: park until the session's next
                        // delivery wakes the wait-set — never a spin.
                        let mut token = 0u64;
                        if tairix_rt::waitset_wait(self.set, u64::MAX, &mut token) != 0 {
                            return Err(Errno::NotFound);
                        }
                        if token == PRESSURE_TOKEN && tairix_procinfo::pressure::refresh() {
                            tairix_font::trim_glyph_cache();
                        }
                    }
                    Err(err) => return Err(Errno::from_syscall(err)),
                }
            }
        }
    }

    /// Redeem the picked file's one-shot delegation and read its (bounded)
    /// content through the delegated descriptor — the only filesystem
    /// reach this program has. Fails closed to `None`: nothing is
    /// fabricated, and the owned handle closes the descriptor on every path
    /// out.
    fn read_picked(handle: u64) -> Option<Vec<u8>> {
        let file = tairix_rt::File::from_delegation(handle).ok()?;
        read_open_fd(file.fd())
    }

    /// Read the document this viewer was handed on [`STDIN`] by its launcher
    /// (the inherited-document hand-off, `plans/NEW-FILEMANAGER.md` FM6b):
    /// a read-only descriptor the kernel cloned into this process at spawn,
    /// so the viewer reads its file with **no filesystem capability of its
    /// own**. Bounded and fail-closed to `None` exactly like [`read_picked`];
    /// the descriptor is left for the runtime to reclaim on exit.
    fn read_document() -> Option<Vec<u8>> {
        read_open_fd(STDIN)
    }

    /// Read the (bounded) content of an already-open, authorised descriptor.
    /// Shared by the delegated pick ([`read_picked`]) and the inherited
    /// document ([`read_document`]) so the bounded read has one definition.
    /// Fails closed to `None` on a read error; a short or empty file reads
    /// back as `Some` of what was there, never a fabricated byte.
    fn read_open_fd(fd: u32) -> Option<Vec<u8>> {
        let mut content = Vec::new();
        let mut chunk = [0u8; 1024];
        while content.len() < CONTENT_MAX {
            let want = chunk.len().min(CONTENT_MAX - content.len());
            let got = tairix_rt::fs_read(fd, content.len() as u64, &mut chunk[..want]).ok()?;
            if got == 0 {
                break;
            }
            content.extend_from_slice(&chunk[..got]);
        }
        Some(content)
    }

    /// Copy `surface` into the shared window frame and present it whole.
    fn present_surface<T: WindowTransport>(
        surface: &tairix_raster::Surface,
        client: &mut WindowClient<T>,
        window: u64,
        frame: &mut [u8],
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        let damage = DamageRect::full(mode);
        winframe::encode(surface, frame, mode, damage, &SERIAL)?;
        client.present(window, 0, damage)
    }

    /// A `width_px` × `height_px` RGBA window mode, one frame's worth per
    /// row. The one place the viewer's mode is shaped, so the create and
    /// every resize agree on stride and format.
    fn mode_for(width_px: u32, height_px: u32) -> DisplayMode {
        DisplayMode {
            width_px,
            height_px,
            stride_bytes: width_px * 4,
            format: DisplayFormat::Rgba8888,
        }
    }

    /// Total bytes a `FRAME_COUNT`-frame region shaped as `mode` needs.
    fn region_bytes(mode: &DisplayMode) -> usize {
        (mode.stride_bytes as usize) * (mode.height_px as usize) * FRAME_COUNT as usize
    }

    /// Draw the whole viewer window — the header, the "Open…" button, the
    /// text area, and the scrollbar — and present it.
    fn present_viewer<T: WindowTransport>(
        viewer: &Viewer,
        theme: &tairix_theme::Theme,
        scale: Scale,
        client: &mut WindowClient<T>,
        window: u64,
        frames: &mut WindowFrames,
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        let surface = viewer
            .render(theme, scale, mode.width_px, mode.height_px)
            .ok_or(Errno::NoSpace)?;
        let pixels = client
            .frame_pixels(frames, window, FRAME_COUNT, mode)
            .ok_or(Errno::NotAttached)?;
        present_surface(&surface, client, window, pixels, mode)
    }

    /// The live window channel this app owns: the transport to the desktop
    /// session, the session-assigned window id, the shared frame region,
    /// and the negotiated display mode. Grouped because the present and
    /// resize paths always need every one of these together; a method on
    /// this type reads at the call site rather than scattering the same
    /// four parameters through every call.
    struct WindowSurface {
        /// The synchronous channel to the desktop session.
        client: WindowClient<RtWindowTransport>,
        /// This app's window id, assigned by the session at create.
        window: u64,
        /// The shared frame region the app paints into.
        frames: WindowFrames,
        /// The region's current shape; every resize replaces it together
        /// with `frames`.
        mode: DisplayMode,
    }

    impl WindowSurface {
        /// Draw the whole viewer window under `theme`/`scale` and present it.
        fn present(
            &mut self,
            viewer: &Viewer,
            theme: &tairix_theme::Theme,
            scale: Scale,
        ) -> Result<(), Errno> {
            present_viewer(
                viewer,
                theme,
                scale,
                &mut self.client,
                self.window,
                &mut self.frames,
                &self.mode,
            )
        }

        /// Re-map the frame region onto `new_mode` and repaint at the new
        /// size, keeping the reader's place.
        ///
        /// The ordering is fail-closed: a fresh region is created and
        /// granted first, then adopted only if the session accepts the
        /// [`WindowClient::resize`]. On success the *old* region is
        /// unmapped (never before, so a refused resize leaves the current
        /// surface intact); on refusal the freshly-allocated region is
        /// unmapped so nothing leaks. A region that cannot be allocated at
        /// all keeps the current size rather than crashing or presenting
        /// nothing. The caller repaints unconditionally afterward, since
        /// even a refused resize leaves the reported client size unchanged
        /// and the current picture already matches it.
        fn resize(
            &mut self,
            new_mode: DisplayMode,
            theme: &tairix_theme::Theme,
            scale: Scale,
            viewer: &mut Viewer,
        ) {
            let Some(spare) = WindowFrames::create(region_bytes(&new_mode)) else {
                // Out of memory for a new region: honestly keep the
                // current window rather than fail the whole app.
                return;
            };
            let Some(grant) = spare.grant() else {
                return;
            };
            if self
                .client
                .resize(self.window, grant, FRAME_COUNT, &new_mode)
                .is_err()
            {
                // The session refused the re-map: the spare drops here, which
                // unmaps it, and the app stands on the old geometry (fail
                // closed, no crash).
                return;
            }
            // The session adopted the new region; adopting it here drops the
            // old one, which unmaps it.
            self.frames = spare;
            self.mode = new_mode;
            // Re-wrap the open file (if any) to the new width, keeping the
            // reader near their place; a status message needs no
            // re-wrapping.
            viewer.relayout(self.mode.width_px, self.mode.height_px, theme, scale);
        }
    }

    /// Ask the session for its desktop and build this app's local
    /// [`Desktop`] model and [`ThemeRegistry`], with the session's current
    /// appearance already applied: the screen, the density, and the look
    /// are current before anything is sized or painted, so the first frame
    /// is right rather than a guess corrected once the user has seen it.
    ///
    /// On any refusal states the reason on `stderr` and returns the
    /// reserved [`EXIT_NO_WINDOW`] code for `main`.
    fn bring_up_desktop(
        client: &mut WindowClient<RtWindowTransport>,
    ) -> Result<(Desktop, ThemeRegistry), i32> {
        let info = match client.desktop() {
            Ok(info) => info,
            Err(err) => {
                let _ = writeln!(Stderr, "viewer: desktop query refused: {err}");
                return Err(EXIT_NO_WINDOW);
            }
        };
        let desktop = match Desktop::new(info) {
            Ok(desktop) => desktop,
            Err(err) => {
                let _ = writeln!(Stderr, "viewer: cannot draw this desktop: {err}");
                return Err(EXIT_NO_WINDOW);
            }
        };
        let mut themes = ThemeRegistry::with_builtins();
        themes.set_appearance(desktop.appearance());
        Ok((desktop, themes))
    }

    /// Bind the app's own event mailbox and add it to a fresh wait-set the
    /// event loop parks on, returning `(endpoint, set)`. The id is unique
    /// by construction (the shared `event_endpoint_for` naming rule: this
    /// task's never-reused kernel id under a fixed tag) and never
    /// reserved; the bind is refused otherwise. On any refusal it states
    /// the reason on `stderr` and returns the reserved fail-closed
    /// [`EXIT_NO_EVENTS`] code for `main`.
    fn bind_event_mailbox() -> Result<(u64, u64), i32> {
        let Ok(origin) = tairix_rt::self_origin() else {
            return Err(fail(EXIT_NO_EVENTS, "own identity unavailable"));
        };
        let event_endpoint = tairix_window::event_endpoint_for(origin.pid());
        if tairix_abi::ipc::is_reserved_endpoint(event_endpoint)
            || tairix_rt::port_bind(
                event_endpoint,
                WindowEvent::WIRE_LEN,
                tairix_window::EVENT_MAILBOX_CAPACITY,
            ) != 0
        {
            return Err(fail(EXIT_NO_EVENTS, "event mailbox bind refused"));
        }
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return Err(fail(EXIT_NO_EVENTS, "wait-set refused"));
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel handle.
        let set = set as u64;
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            event_endpoint,
            EVENT_TOKEN,
        ) != 0
        {
            return Err(fail(EXIT_NO_EVENTS, "event mailbox wait refused"));
        }
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            return Err(fail(EXIT_NO_EVENTS, "memory-pressure wake refused"));
        }
        Ok((event_endpoint, set))
    }

    /// Open the viewer's window and load its initial content.
    ///
    /// How the viewer starts depends on how it was launched: handed a
    /// document on `STDIN` (the file manager's open hand-off), it shows
    /// that file at once; launched on its own, it asks the session's
    /// trusted picker. The title names the handed-over document, else the
    /// generic app name.
    ///
    /// Returns the created window's id, the desktop session's [`ProcId`],
    /// and the initialised [`Viewer`], or the reserved [`EXIT_NO_WINDOW`]
    /// code for `main` when the session refuses the create itself.
    fn open_initial_view(
        client: &mut WindowClient<RtWindowTransport>,
        grant: u64,
        event_endpoint: u64,
        mode: &DisplayMode,
        theme: &tairix_theme::Theme,
        scale: Scale,
    ) -> Result<(u64, ProcId, Viewer), i32> {
        let document_mode = tairix_rt::arg(1).is_some_and(|arg| arg == DOCUMENT_ROLE_ARG);
        let title = if document_mode {
            tairix_rt::arg(2)
                .and_then(|name| core::str::from_utf8(name).ok())
                .unwrap_or("Viewer")
        } else {
            "Viewer"
        };
        // The icon-bar presence first: a declared presence belongs to the
        // process, so declaring it before this process owns a window is what
        // makes its slot carry this menu from the moment it appears rather
        // than being a slot the session derived from a window, which opens
        // nothing.
        declare_app_bar(client, event_endpoint);
        let sizing = WindowSizing::Resizable {
            min_width_px: MIN_WIN_WIDTH,
            min_height_px: MIN_WIN_HEIGHT,
        };
        let Ok((window, server)) =
            client.create(grant, event_endpoint, FRAME_COUNT, mode, title, sizing)
        else {
            return Err(fail(EXIT_NO_WINDOW, "desktop session refused the window"));
        };

        // The whole window's pointer- and keyboard-driven state: the
        // current file view (or the status message shown in its place),
        // the "Open…" button, and the scrollbar, all composed in the
        // host-tested engine.
        let mut viewer = Viewer::new();
        if document_mode {
            // The launcher handed us the file on STDIN; display it now
            // instead of prompting. A refused read is stated honestly,
            // never faked.
            match read_document() {
                Some(bytes) => {
                    viewer.open(bytes, mode.width_px, mode.height_px, theme, scale);
                }
                None => viewer.show_status("Document read refused."),
            }
        } else if client.pick_file(window).is_err() {
            // A refused pick (another pick showing, or a session without
            // filesystem reach) is not fatal: the viewer stays open and
            // the "Open…" button or Enter asks again.
            viewer.show_status("Pick refused.");
        }
        Ok((window, server, viewer))
    }

    /// What handling one delivered window event concluded: whether the
    /// viewer changed and needs a repaint, whether it asked the session
    /// for a fresh pick, and whether the window is already closed.
    /// [`apply_window_event`] performs the event's own side effects
    /// (loading a picked file, resizing the frame region, closing the
    /// window) itself; this only reports what the event loop must still
    /// do.
    struct ViewerOutcome {
        /// The view changed; the caller repaints.
        repaint: bool,
        /// The "Open…" button or Enter asked for a fresh pick; a refusal
        /// is not fatal, so the caller issues it and moves on.
        request_pick: bool,
        /// The window is already closed and unmapped; the caller ends the
        /// program at exit code `0`.
        close: bool,
    }

    impl ViewerOutcome {
        /// Nothing changed.
        const IDLE: Self = Self {
            repaint: false,
            request_pick: false,
            close: false,
        };

        /// The view changed; repaint.
        const REPAINT: Self = Self {
            repaint: true,
            request_pick: false,
            close: false,
        };

        /// Ask for a fresh pick; the outcome arrives as a later event.
        const REQUEST_PICK: Self = Self {
            repaint: false,
            request_pick: true,
            close: false,
        };

        /// The window is closed; end the program.
        const CLOSE: Self = Self {
            repaint: false,
            request_pick: false,
            close: true,
        };

        /// Repaint only when `changed`.
        const fn from_repaint(changed: bool) -> Self {
            Self {
                repaint: changed,
                request_pick: false,
                close: false,
            }
        }
    }

    /// Apply one delivered window event to the viewer, once the desktop
    /// change it may have carried is already adopted by the caller (so
    /// every derived value — scale, theme — is current here). Performs
    /// the event's own side effects directly (loading a picked file,
    /// resizing the frame region and re-wrapping the open file, closing
    /// and unmapping the window) and reports what the caller must still
    /// do.
    fn apply_window_event(
        event: WindowEvent,
        viewer: &mut Viewer,
        surface: &mut WindowSurface,
        theme: &tairix_theme::Theme,
        scale: Scale,
    ) -> ViewerOutcome {
        match event {
            WindowEvent::FilePicked { handle, .. } => {
                match read_picked(handle) {
                    Some(bytes) => {
                        viewer.open(
                            bytes,
                            surface.mode.width_px,
                            surface.mode.height_px,
                            theme,
                            scale,
                        );
                    }
                    // A refused redemption or read delegated nothing the
                    // viewer can show; state it honestly.
                    None => viewer.show_status("Delegated read refused."),
                }
                ViewerOutcome::REPAINT
            }
            WindowEvent::PickCancelled { .. } => {
                viewer.show_status("No file chosen.");
                ViewerOutcome::REPAINT
            }
            WindowEvent::Key {
                key: KeyInput::Pressed { key, .. },
                ..
            } => match key {
                // Enter asks for another pick — the same request the
                // "Open…" button sends; a refusal (one already showing)
                // leaves the current content on screen.
                KeyValue::Named(NamedKeyCode::Enter) => ViewerOutcome::REQUEST_PICK,
                // Navigation keys drive the shared scroll model and
                // repaint only when the view actually moved.
                KeyValue::Named(nav) => ViewerOutcome::from_repaint(navigate(nav, viewer)),
                KeyValue::Char(_) => ViewerOutcome::IDLE,
            },
            // A wheel gesture the desktop forwarded because this window
            // owns its own content scrolling: drive the shared model by
            // its vertical ticks and repaint only when the view moved.
            WindowEvent::Scrolled { dy, .. } => {
                ViewerOutcome::from_repaint(viewer.scroll_ticks(dy))
            }
            // A pointer event over the client area: sync the hover
            // position, then apply the press/release the action names,
            // exactly as the widget gallery's own window channel does.
            // The button and the scrollbar are the only interactive
            // regions, so this is the pointer's whole route into the
            // viewer.
            WindowEvent::Pointer { x, y, action, .. } => {
                let outcome = apply_pointer(viewer, x, y, action, theme, &surface.mode, scale);
                ViewerOutcome {
                    repaint: outcome.changed,
                    request_pick: outcome.open_requested,
                    close: false,
                }
            }
            // The window manager resized (or maximized/restored) the
            // window. Re-map the frame region at the new client size,
            // then re-wrap the file and repaint so the content fills the
            // new window rather than leaving stale or clipped pixels.
            //
            // The reported size is adopted exactly: the declared minimum
            // is the window manager's to hold, and an app that pushed back
            // here would fight the drag frame by frame.
            WindowEvent::Resized {
                width_px,
                height_px,
                ..
            } => {
                surface.resize(mode_for(width_px, height_px), theme, scale, viewer);
                ViewerOutcome::REPAINT
            }
            // The desktop asked, or *Quit* was chosen on the viewer's own
            // icon-bar slot: close the window and end; the frame region is
            // unmapped by its own drop, so nothing is left pinned. A row the
            // declaration never carried names no command and is ignored (fail
            // closed).
            WindowEvent::CloseRequested { .. } => {
                let _ = surface.client.close(surface.window);
                ViewerOutcome::CLOSE
            }
            WindowEvent::AppBarMenu { item } if tairix_window::is_quit(item) => {
                let _ = surface.client.close(surface.window);
                ViewerOutcome::CLOSE
            }
            // `DesktopChanged` is adopted by the caller before this
            // dispatch runs, which is also where the repaint a real
            // change needs is decided, so it asks for nothing further
            // here; focus changes, key releases, and minimize repaint
            // nothing the viewer draws; and a redraw request is already
            // answered by the client library re-presenting the last
            // frame, which is still what the viewer would draw. Listed
            // rather than caught by a wildcard so a new event forces a
            // decision here.
            //
            // A secondary press on Close asks to leave what the window is
            // showing; the viewer has nothing to leave but itself, and a
            // primary press already closes it.
            // The viewer declares no default action, so the session raises
            // its window on a click rather than telling it — an
            // `AppBarDefault` therefore cannot arrive, and an `AppBarMenu`
            // naming any other row names no command of the viewer's.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::AppBarDefault
            | WindowEvent::AppBarMenu { .. }
            | WindowEvent::DesktopChanged { .. }
            | WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::RedrawRequested { .. } => ViewerOutcome::IDLE,
            // Nobody can see the window, so the session gave its copy of the
            // pixels back and unmapped the region. Let go of this side too —
            // the pages go only when both do — and ask for no repaint: the
            // redraw request that follows the window being shown again is
            // what re-attaches a fresh region and fills it.
            WindowEvent::ContentReleased { .. } => {
                surface.frames.release();
                ViewerOutcome::IDLE
            }
        }
    }

    /// The event loop: park, apply, repaint. A dead channel ends the app
    /// fail-loud; a clean close ends it at zero.
    fn run_event_loop(
        surface: &mut WindowSurface,
        desktop: &mut Desktop,
        themes: &mut ThemeRegistry,
        viewer: &mut Viewer,
        mut events: WindowEvents<RtEventSource>,
    ) -> i32 {
        loop {
            let event = match events.wait(&mut surface.client) {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };

            // Apply the desktop change before the app-specific event
            // logic so every derived value (scale, theme) is current for
            // the repaint. A refused change logs the reason and stands on
            // the last good state.
            let mut repaint = match desktop.apply(&event) {
                Ok(true) => {
                    themes.set_appearance(desktop.appearance());
                    viewer.relayout(
                        surface.mode.width_px,
                        surface.mode.height_px,
                        themes.active(),
                        desktop.scale(),
                    );
                    true
                }
                Ok(false) => false,
                Err(err) => {
                    let _ = writeln!(Stderr, "viewer: desktop change refused: {err}");
                    false
                }
            };

            let event_outcome =
                apply_window_event(event, viewer, surface, themes.active(), desktop.scale());
            if event_outcome.close {
                return 0;
            }
            repaint |= event_outcome.repaint;
            if event_outcome.request_pick {
                // A refused pick (another pick showing) leaves the
                // current content on screen; the outcome arrives as a
                // later event.
                let _ = surface.client.pick_file(surface.window);
            }

            let present_result = if repaint {
                surface.present(viewer, themes.active(), desktop.scale())
            } else {
                Ok(())
            };
            if present_result.is_err() {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    fn main() -> i32 {
        let mut client = WindowClient::new(RtWindowTransport);

        // --- The desktop this window will be shown on, established before
        // anything is sized or painted so the first frame is right rather
        // than a guess corrected once the user has seen it.
        let (mut desktop, mut themes) = match bring_up_desktop(&mut client) {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        // --- The shared window surface: FRAME_COUNT frames shaped as the
        // initial window mode, created here and granted to the session. The
        // viewer is resizable, so the region is re-created (and the old one
        // unmapped) whenever the window manager reports a new client size.
        let (initial_w, initial_h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mode = mode_for(initial_w, initial_h);
        let Some(frames) = WindowFrames::create(region_bytes(&mode)) else {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        };
        let Some(grant) = frames.grant() else {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        };

        // --- The event mailbox the app parks on.
        let (event_endpoint, set) = match bind_event_mailbox() {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        // --- Open the window (resizable: the viewer re-lays-out its text to
        // each new client size) and load its initial content.
        let (window, server, mut viewer) = match open_initial_view(
            &mut client,
            grant,
            event_endpoint,
            &mode,
            themes.active(),
            desktop.scale(),
        ) {
            Ok(triple) => triple,
            Err(code) => return code,
        };

        let mut surface = WindowSurface {
            client,
            window,
            frames,
            mode,
        };
        if surface
            .present(&viewer, themes.active(), desktop.scale())
            .is_err()
        {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The event loop: park, apply, repaint. A dead channel ends
        // the app fail-loud; a clean close ends it at zero.
        let events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        run_event_loop(&mut surface, &mut desktop, &mut themes, &mut viewer, events)
    }

    /// Route one wire pointer event into the viewer through the one shared
    /// wire-to-control translation ([`pointer_input_events`]): a move to
    /// `(x, y)` first, then the press/release `action` names, so the button
    /// and the scrollbar are never asked about a transition at a position
    /// they have not been told about.
    fn apply_pointer(
        viewer: &mut Viewer,
        x: u32,
        y: u32,
        action: PointerAction,
        theme: &tairix_theme::Theme,
        mode: &DisplayMode,
        scale: Scale,
    ) -> ViewerPointerOutcome {
        let point = Point::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        );
        let mut outcome = ViewerPointerOutcome {
            changed: false,
            open_requested: false,
        };
        // One sink for the whole round: both synthesised events reach the same
        // two controls, which report their repainted bounds into it.
        let mut damage = tairix_controls::damage::sink();
        for input in pointer_input_events(action, point) {
            let step = viewer.on_pointer(
                &input,
                mode.width_px,
                mode.height_px,
                theme,
                scale,
                &mut damage,
            );
            outcome.changed |= step.changed;
            outcome.open_requested |= step.open_requested;
        }
        outcome
    }

    /// Apply a navigation key to the viewer, returning whether the view moved.
    fn navigate(key: NamedKeyCode, viewer: &mut Viewer) -> bool {
        match key {
            NamedKeyCode::Up => viewer.line_up(),
            NamedKeyCode::Down => viewer.line_down(),
            NamedKeyCode::PageUp => viewer.page_up(),
            NamedKeyCode::PageDown => viewer.page_down(),
            NamedKeyCode::Home => viewer.to_top(),
            NamedKeyCode::End => viewer.to_bottom(),
            _ => false,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
