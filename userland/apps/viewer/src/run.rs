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
    use tairix_abi::window_ipc::{AppBarClick, PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, DOCUMENT_ROLE_ARG, ORIGIN_WIRE_LEN, STDIN,
    };
    use tairix_display::{winframe, SERIAL};
    use tairix_geometry::{Point, Region, Scale};
    use tairix_raster::Surface;
    use tairix_rt::io::{Stderr, Write};
    use tairix_theme::ThemeRegistry;
    use tairix_viewer::{
        Viewer, ViewerLayout, ViewerPointerOutcome, CONTENT_MAX, MIN_WIN_HEIGHT, MIN_WIN_WIDTH,
        WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_window::{
        pointer_input_events, pointer_point, present_damage, Desktop, EventSource, Repaint,
        WindowClient, WindowEvents, WindowFrames, WindowSizing, WindowTransport,
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
    /// with the session raising the window when there is one and asking the
    /// viewer to open another when there is not.
    ///
    /// A refused declaration is an answer, not a death: the viewer says so
    /// and carries on with no slot of its own — its window is still reachable
    /// through the one the session derives from it.
    fn declare_app_bar(client: &mut WindowClient<RtWindowTransport>, endpoint: u64) {
        match tairix_window::info_and_quit(endpoint, AppBarClick::RaiseOrOpen) {
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

    /// One open viewer window: the session-assigned id, the shared frame
    /// region, and the negotiated display mode. Grouped because the present
    /// and resize paths always need every one of these together.
    struct Pane {
        /// This app's window id, assigned by the session at create.
        window: u64,
        /// The shared frame region the app paints into.
        frames: WindowFrames,
        /// The region's current shape; every resize replaces it together
        /// with `frames`.
        mode: DisplayMode,
        /// The window-sized surface every frame is drawn into, held for the
        /// life of the window: allocating and zeroing one per present would be
        /// a whole-window pass of its own, and holding it is what makes a
        /// clipped repaint sound — every pixel outside the clip is the one
        /// already on screen.
        surface: Surface,
    }

    /// The live window channel this app owns and the window it may or may
    /// not have open.
    ///
    /// The viewer is on the icon bar whether or not a window is open:
    /// closing one puts it away and a click on its slot picks another file,
    /// so the channel outlives every window that crosses it.
    struct WindowSurface {
        /// The synchronous channel to the desktop session.
        client: WindowClient<RtWindowTransport>,
        /// The open window, or `None` while the viewer sits on the bar.
        pane: Option<Pane>,
    }

    impl WindowSurface {
        /// Draw `damage` of the viewer window under `theme`/`scale`, convert
        /// that rectangle into the shared frame region and present it.
        ///
        /// A region the session released while the window was hidden is
        /// re-attached first and presented whole, because it holds none of the
        /// pixels a partial present would leave standing.
        fn present(
            &mut self,
            viewer: &Viewer,
            theme: &tairix_theme::Theme,
            scale: Scale,
            damage: DamageRect,
        ) -> Result<(), Errno> {
            let Some(pane) = self.pane.as_mut() else {
                return Ok(());
            };
            let damage = if pane.frames.is_released() {
                DamageRect::full(&pane.mode)
            } else {
                damage
            };
            pane.surface.with_clip(
                damage.x,
                damage.y,
                damage.width_px,
                damage.height_px,
                |surface| viewer.render_into(surface, theme, scale),
            );
            let pixels = self
                .client
                .frame_pixels(&mut pane.frames, pane.window, FRAME_COUNT, &pane.mode)
                .ok_or(Errno::NotAttached)?;
            winframe::encode(&pane.surface, pixels, &pane.mode, damage, &SERIAL)?;
            self.client.present(pane.window, 0, damage)
        }

        /// The open window's current shape, or `None` with none open.
        const fn mode(&self) -> Option<&DisplayMode> {
            match &self.pane {
                Some(pane) => Some(&pane.mode),
                None => None,
            }
        }

        /// Close the open window, if any, leaving the viewer on the icon
        /// bar. The frame region is unmapped by its own drop, so nothing is
        /// left pinned.
        fn close(&mut self) {
            if let Some(pane) = self.pane.take() {
                let _ = self.client.close(pane.window);
            }
        }

        /// Re-map the frame region onto `new_mode` and repaint at the new
        /// size, keeping the reader's place.
        ///
        /// The ordering is fail-closed: a fresh region and a fresh drawing
        /// surface are allocated and the region granted first, then adopted
        /// only if the session accepts the [`WindowClient::resize`]. On success
        /// the *old* region is unmapped (never before, so a refused resize
        /// leaves the current surface intact); on refusal the freshly-allocated
        /// region is unmapped so nothing leaks. Anything that cannot be
        /// allocated at all keeps the current size rather than crashing or
        /// presenting nothing. The caller repaints the whole window afterward,
        /// since even a refused resize leaves the reported client size
        /// unchanged and the current picture already matches it.
        fn resize(
            &mut self,
            new_mode: DisplayMode,
            theme: &tairix_theme::Theme,
            scale: Scale,
            viewer: &mut Viewer,
        ) {
            let Some(pane) = self.pane.as_mut() else {
                return;
            };
            let Some(spare) = WindowFrames::create(region_bytes(&new_mode)) else {
                // Out of memory for a new region: honestly keep the
                // current window rather than fail the whole app.
                return;
            };
            let Some(canvas) = Surface::new(new_mode.width_px, new_mode.height_px) else {
                return;
            };
            let Some(grant) = spare.grant() else {
                return;
            };
            if self
                .client
                .resize(pane.window, grant, FRAME_COUNT, &new_mode)
                .is_err()
            {
                // The session refused the re-map: the spare drops here, which
                // unmaps it, and the app stands on the old geometry (fail
                // closed, no crash).
                return;
            }
            // The session adopted the new region; adopting it here drops the
            // old one, which unmaps it.
            pane.frames = spare;
            pane.mode = new_mode;
            pane.surface = canvas;
            // Re-wrap the open file (if any) to the new width, keeping the
            // reader near their place; a status message needs no
            // re-wrapping.
            viewer.relayout(pane.mode.width_px, pane.mode.height_px, theme, scale);
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
    /// Returns the desktop session's [`ProcId`] from the create reply, or
    /// the reserved exit code for the refusal — stated on `stderr` either
    /// way, so a caller that carries on has already reported it.
    ///
    /// Only the *first* window carries a handed-over document; a later one
    /// is the icon-bar slot asking for another file, so it starts on the
    /// picker exactly as a plain launch does.
    fn open_view(
        surface: &mut WindowSurface,
        event_endpoint: u64,
        mode: &DisplayMode,
        viewer: &mut Viewer,
        theme: &tairix_theme::Theme,
        scale: Scale,
        document: Document,
    ) -> Result<ProcId, i32> {
        if surface.pane.is_some() {
            return Err(EXIT_NO_WINDOW);
        }
        let title = match document {
            Document::HandedOver => tairix_rt::arg(2)
                .and_then(|name| core::str::from_utf8(name).ok())
                .unwrap_or("Viewer"),
            Document::Pick => "Viewer",
        };
        let Some(frames) = WindowFrames::create(region_bytes(mode)) else {
            return Err(fail(EXIT_NO_FRAMES, "shared frame region refused"));
        };
        let Some(grant) = frames.grant() else {
            return Err(fail(EXIT_NO_FRAMES, "shared frame region refused"));
        };
        let Some(canvas) = Surface::new(mode.width_px, mode.height_px) else {
            return Err(fail(EXIT_NO_WINDOW, "no memory for the window surface"));
        };
        let sizing = WindowSizing::Resizable {
            min_width_px: MIN_WIN_WIDTH,
            min_height_px: MIN_WIN_HEIGHT,
        };
        let Ok((window, server)) =
            surface
                .client
                .create(grant, event_endpoint, FRAME_COUNT, mode, title, sizing)
        else {
            return Err(fail(EXIT_NO_WINDOW, "desktop session refused the window"));
        };
        surface.pane = Some(Pane {
            window,
            frames,
            mode: *mode,
            surface: canvas,
        });

        match document {
            // The launcher handed us the file on STDIN; display it now
            // instead of prompting. A refused read is stated honestly,
            // never faked.
            Document::HandedOver => match read_document() {
                Some(bytes) => {
                    viewer.open(bytes, mode.width_px, mode.height_px, theme, scale);
                }
                None => viewer.show_status("Document read refused."),
            },
            // A refused pick (another pick showing, or a session without
            // filesystem reach) is not fatal: the viewer stays open and
            // the "Open…" button or Enter asks again.
            Document::Pick => {
                if surface.client.pick_file(window).is_err() {
                    viewer.show_status("Pick refused.");
                }
            }
        }
        Ok(server)
    }

    /// Where a fresh window's content comes from.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Document {
        /// The file manager handed a document over on `STDIN`.
        HandedOver,
        /// Ask the session's trusted picker.
        Pick,
    }

    /// What handling one delivered window event concluded: whether the
    /// viewer changed and needs a repaint, whether it asked the session
    /// for a fresh pick, and whether the window is already closed.
    /// [`apply_window_event`] performs the event's own side effects
    /// (loading a picked file, resizing the frame region, closing the
    /// window) itself; this only reports what the event loop must still
    /// do.
    struct ViewerOutcome {
        /// What the event redraws, which is what decides the rectangle the
        /// round presents.
        repaint: Repaint,
        /// The "Open…" button or Enter asked for a fresh pick; a refusal
        /// is not fatal, so the caller issues it and moves on.
        request_pick: bool,
        /// What the event asks the program to do with its window.
        then: Next,
    }

    /// What one delivered event asks of the window, beyond repainting it.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Next {
        /// Nothing; carry on with the window as it is.
        Carry,
        /// Close the window, leaving the viewer on the icon bar.
        Close,
        /// Open a window, the viewer having none.
        Open,
        /// End the program.
        Quit,
    }

    impl ViewerOutcome {
        /// Nothing changed.
        const IDLE: Self = Self {
            repaint: Repaint::Nothing,
            request_pick: false,
            then: Next::Carry,
        };

        /// A model refresh no control round could describe — new file
        /// content, or a window the manager resized — so the whole window is
        /// redrawn.
        const WHOLE: Self = Self {
            repaint: Repaint::Whole,
            request_pick: false,
            then: Next::Carry,
        };

        /// Ask for a fresh pick; the outcome arrives as a later event.
        const REQUEST_PICK: Self = Self {
            repaint: Repaint::Nothing,
            request_pick: true,
            then: Next::Carry,
        };

        /// Close the window; the viewer stays on the icon bar.
        const CLOSE: Self = Self {
            repaint: Repaint::Nothing,
            request_pick: false,
            then: Next::Close,
        };

        /// Open a window: the icon-bar slot was clicked with none open.
        const OPEN: Self = Self {
            repaint: Repaint::Nothing,
            request_pick: false,
            then: Next::Open,
        };

        /// End the program: *Quit* was chosen.
        const QUIT: Self = Self {
            repaint: Repaint::Nothing,
            request_pick: false,
            then: Next::Quit,
        };

        /// Present what the round reported, but only when it changed
        /// something.
        const fn from_reported(changed: bool) -> Self {
            Self {
                repaint: if changed {
                    Repaint::Reported
                } else {
                    Repaint::Nothing
                },
                request_pick: false,
                then: Next::Carry,
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
        damage: &mut Region,
    ) -> ViewerOutcome {
        // The two application-scoped events are addressed to the process
        // rather than to a window, so they arrive whether or not one is open
        // and are answered before anything is laid out. A row the
        // declaration never carried names no command (fail closed).
        match event {
            WindowEvent::AppBarDefault => return ViewerOutcome::OPEN,
            WindowEvent::AppBarMenu { item } => {
                return if tairix_window::is_quit(item) {
                    ViewerOutcome::QUIT
                } else {
                    ViewerOutcome::IDLE
                };
            }
            _ => {}
        }
        // Every remaining event is window-scoped, so with none open there is
        // nothing to lay out and nothing to apply it to.
        let Some(mode) = surface.mode().copied() else {
            return ViewerOutcome::IDLE;
        };
        let layout = ViewerLayout::for_window(mode.width_px, mode.height_px, theme, scale);
        match event {
            WindowEvent::FilePicked { handle, .. } => {
                match read_picked(handle) {
                    Some(bytes) => {
                        viewer.open(bytes, mode.width_px, mode.height_px, theme, scale);
                    }
                    // A refused redemption or read delegated nothing the
                    // viewer can show; state it honestly.
                    None => viewer.show_status("Delegated read refused."),
                }
                ViewerOutcome::WHOLE
            }
            WindowEvent::PickCancelled { .. } => {
                viewer.show_status("No file chosen.");
                ViewerOutcome::WHOLE
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
                KeyValue::Named(nav) => {
                    ViewerOutcome::from_reported(navigate(nav, viewer, &layout, damage))
                }
                KeyValue::Char(_) => ViewerOutcome::IDLE,
            },
            // A wheel gesture the desktop forwarded because this window
            // owns its own content scrolling: drive the shared model by
            // its vertical ticks and repaint only when the view moved.
            WindowEvent::Scrolled { dy, .. } => {
                ViewerOutcome::from_reported(viewer.scroll_ticks(dy, &layout, damage))
            }
            // A pointer event over the client area: sync the hover
            // position, then apply the press/release the action names,
            // exactly as the widget gallery's own window channel does.
            // The button and the scrollbar are the only interactive
            // regions, so this is the pointer's whole route into the
            // viewer.
            WindowEvent::Pointer { x, y, action, .. } => {
                let at = pointer_point(x, y);
                let outcome = apply_pointer(viewer, at, action, theme, &layout, scale, damage);
                ViewerOutcome {
                    repaint: if outcome.changed {
                        Repaint::Reported
                    } else {
                        Repaint::Nothing
                    },
                    request_pick: outcome.open_requested,
                    then: Next::Carry,
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
                ViewerOutcome::WHOLE
            }
            // The desktop asked: close the window, which leaves the viewer
            // on the icon bar rather than ending it. *Quit* on that slot is
            // what ends it.
            WindowEvent::CloseRequested { .. } => ViewerOutcome::CLOSE,
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
            // primary press already closes it. No menu outcome can arrive:
            // it answers an open the viewer never asks for. The two
            // application-scoped events were answered above.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::AppBarDefault
            | WindowEvent::AppBarMenu { .. }
            | WindowEvent::MenuClosed { .. }
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
                if let Some(pane) = surface.pane.as_mut() {
                    pane.frames.release();
                }
                ViewerOutcome::IDLE
            }
        }
    }

    /// The event loop: park, apply, repaint. A dead channel ends the app
    /// fail-loud; *Quit* on its icon-bar slot ends it at zero.
    fn run_event_loop(
        surface: &mut WindowSurface,
        desktop: &mut Desktop,
        themes: &mut ThemeRegistry,
        viewer: &mut Viewer,
        event_endpoint: u64,
        initial_mode: DisplayMode,
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
            let re_themed = match desktop.apply(&event) {
                Ok(true) => {
                    themes.set_appearance(desktop.appearance());
                    if let Some(mode) = surface.mode().copied() {
                        viewer.relayout(
                            mode.width_px,
                            mode.height_px,
                            themes.active(),
                            desktop.scale(),
                        );
                    }
                    true
                }
                Ok(false) => false,
                Err(err) => {
                    let _ = writeln!(Stderr, "viewer: desktop change refused: {err}");
                    false
                }
            };

            // One sink per round: every control the event reaches, and the
            // viewer for the scroll it commits itself, reports its repainted
            // bounds into this one, which is what the present is clipped to.
            let mut damage = tairix_controls::damage::sink();
            let event_outcome = apply_window_event(
                event,
                viewer,
                surface,
                themes.active(),
                desktop.scale(),
                &mut damage,
            );
            match event_outcome.then {
                Next::Quit => {
                    surface.close();
                    return 0;
                }
                Next::Close => {
                    surface.close();
                    continue;
                }
                Next::Open => {
                    // A fresh window is the slot asking for another file, so
                    // it starts on the picker whatever this process was
                    // handed at launch, and at the size a launch would give.
                    // A refusal is already stated; the slot is still there
                    // to try again from.
                    let _ = open_view(
                        surface,
                        event_endpoint,
                        &initial_mode,
                        viewer,
                        themes.active(),
                        desktop.scale(),
                        Document::Pick,
                    );
                    continue;
                }
                Next::Carry => {}
            }
            if event_outcome.request_pick {
                // A refused pick (another pick showing) leaves the
                // current content on screen; the outcome arrives as a
                // later event.
                if let Some(pane) = surface.pane.as_ref() {
                    let window = pane.window;
                    let _ = surface.client.pick_file(window);
                }
            }

            // An adopted desktop change re-themes and re-densifies every
            // pixel, so no report could describe it.
            let repaint = if re_themed {
                Repaint::Whole
            } else {
                event_outcome.repaint
            };
            let Some(mode) = surface.mode().copied() else {
                continue;
            };
            let Some(damage) = present_damage(&mode, repaint, &damage) else {
                continue;
            };
            if surface
                .present(viewer, themes.active(), desktop.scale(), damage)
                .is_err()
            {
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

        // --- The window's shape. The viewer is resizable, so the frame
        // region is re-created (and the old one unmapped) whenever the
        // window manager reports a new client size; this is the size every
        // window it opens starts at.
        let (initial_w, initial_h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mode = mode_for(initial_w, initial_h);

        // --- The event mailbox the app parks on.
        let (event_endpoint, set) = match bind_event_mailbox() {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        // The icon-bar presence first: a declared presence belongs to the
        // process, so declaring it before this process owns a window is what
        // makes its slot carry this menu from the moment it appears rather
        // than being a slot the session derived from a window, which opens
        // nothing.
        declare_app_bar(&mut client, event_endpoint);
        let mut surface = WindowSurface { client, pane: None };
        // The whole window's pointer- and keyboard-driven state: the current
        // file view (or the status message shown in its place), the "Open…"
        // button, and the scrollbar, all composed in the host-tested engine.
        let mut viewer = Viewer::new();
        // How the viewer starts depends on how it was launched: handed a
        // document on `STDIN` (the file manager's open hand-off), it shows
        // that file at once; launched on its own, it asks the session's
        // trusted picker.
        let document = if tairix_rt::arg(1).is_some_and(|arg| arg == DOCUMENT_ROLE_ARG) {
            Document::HandedOver
        } else {
            Document::Pick
        };
        // The viewer was started to show something, so a first window that
        // will not open leaves it nothing to do and it ends fail-loud; every
        // later one is a click on its slot, which reports and carries on.
        let server = match open_view(
            &mut surface,
            event_endpoint,
            &mode,
            &mut viewer,
            themes.active(),
            desktop.scale(),
            document,
        ) {
            Ok(server) => server,
            Err(code) => return code,
        };
        let first = DamageRect::full(&mode);
        if surface
            .present(&viewer, themes.active(), desktop.scale(), first)
            .is_err()
        {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The event loop: park, apply, repaint. A dead channel ends
        // the app fail-loud; *Quit* ends it at zero.
        let events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        run_event_loop(
            &mut surface,
            &mut desktop,
            &mut themes,
            &mut viewer,
            event_endpoint,
            mode,
            events,
        )
    }

    /// Route one wire pointer event into the viewer through the one shared
    /// wire-to-control translation ([`pointer_input_events`]): a move to
    /// `(x, y)` first, then the press/release `action` names, so the button
    /// and the scrollbar are never asked about a transition at a position
    /// they have not been told about.
    fn apply_pointer(
        viewer: &mut Viewer,
        point: Point,
        action: PointerAction,
        theme: &tairix_theme::Theme,
        layout: &ViewerLayout,
        scale: Scale,
        damage: &mut Region,
    ) -> ViewerPointerOutcome {
        let mut outcome = ViewerPointerOutcome {
            changed: false,
            open_requested: false,
        };
        for input in pointer_input_events(action, point) {
            let step = viewer.on_pointer(&input, layout, theme, scale, damage);
            outcome.changed |= step.changed;
            outcome.open_requested |= step.open_requested;
        }
        outcome
    }

    /// Apply a navigation key to the viewer, returning whether the view moved.
    fn navigate(
        key: NamedKeyCode,
        viewer: &mut Viewer,
        layout: &ViewerLayout,
        damage: &mut Region,
    ) -> bool {
        match key {
            NamedKeyCode::Up => viewer.line_up(layout, damage),
            NamedKeyCode::Down => viewer.line_down(layout, damage),
            NamedKeyCode::PageUp => viewer.page_up(layout, damage),
            NamedKeyCode::PageDown => viewer.page_down(layout, damage),
            NamedKeyCode::Home => viewer.to_top(layout, damage),
            NamedKeyCode::End => viewer.to_bottom(layout, damage),
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
