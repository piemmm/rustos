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
    use tairix_geometry::{Point, Scale};
    use tairix_rt::io::{Stderr, Write};
    use tairix_theme::ThemeRegistry;
    use tairix_viewer::{
        Viewer, ViewerPointerOutcome, CONTENT_MAX, MIN_WIN_HEIGHT, MIN_WIN_WIDTH, WIN_HEIGHT,
        WIN_WIDTH,
    };
    use tairix_window::{
        pointer_input_events, EventSource, WindowClient, WindowEvents, WindowTransport,
    };

    /// The desktop scale the viewer draws at. The window's extents are
    /// authored in unscaled pixels ([`WIN_WIDTH`], [`WIN_HEIGHT`]), so every
    /// layout, render, and hit-test call in this program agrees on one
    /// density.
    const SCALE: Scale = Scale::ONE;

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

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-ret`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit
    /// code alone is not a diagnosis) and hand back `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "viewer: {reason}");
        code
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to
    /// the reserved window endpoint per request. The session attests the
    /// caller kernel-side on every request, so the transport carries no
    /// claimed authority.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(errno_from)
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
                    Err(err) if errno_from(err) == Errno::WouldBlock => {
                        // Nothing queued: park until the session's next
                        // delivery wakes the wait-set — never a spin.
                        let mut token = 0u64;
                        if tairix_rt::waitset_wait(self.set, u64::MAX, &mut token) != 0 {
                            return Err(Errno::NotFound);
                        }
                    }
                    Err(err) => return Err(errno_from(err)),
                }
            }
        }
    }

    /// Redeem the picked file's one-shot delegation and read its (bounded)
    /// content through the delegated descriptor — the only filesystem
    /// reach this program has. Every step fails closed to `None`: nothing
    /// is fabricated, and the descriptor is closed either way.
    fn read_picked(handle: u64) -> Option<Vec<u8>> {
        let fd = u32::try_from(tairix_rt::fd_redeem(handle)).ok()?;
        let content = read_open_fd(fd);
        let _ = tairix_rt::fs_close(fd);
        content
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
        for (i, pixel) in surface.pixels().iter().enumerate() {
            let color = pixel.unpremultiply();
            let at = i * 4;
            let Some(slot) = frame.get_mut(at..at + 4) else {
                return Err(Errno::LengthOutOfRange);
            };
            slot.copy_from_slice(&[color.r, color.g, color.b, color.a]);
        }
        client.present(window, 0, DamageRect::full(mode))
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

    /// Create a `total`-byte frame region and grant it to the window
    /// endpoint, returning the region id, its mapped base, and the
    /// endpoint-directed grant handle. Fails closed to `None` on any
    /// refusal, unmapping a region that mapped but could not be granted so
    /// a refused (re)allocation never leaves pinned memory behind.
    fn allocate_frames(total: usize) -> Option<(u64, usize, u64)> {
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return None;
        }
        let base = usize::try_from(base).ok()?;
        let grant = tairix_rt::shm_grant(region_id, WINDOW_ENDPOINT);
        if grant < 1 {
            let _ = tairix_rt::shm_unmap(base as u64, total);
            return None;
        }
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        Some((region_id, base, grant as u64))
    }

    /// The live frame region: the once-granted shared surface the app
    /// paints into. Re-mapped on every resize; the old mapping is unmapped
    /// only after the session adopts the new one, so a refused resize keeps
    /// the current surface intact.
    struct Frames {
        base: usize,
        len: usize,
    }

    impl Frames {
        /// The region as a mutable byte slice.
        fn as_mut(&mut self) -> &mut [u8] {
            // SAFETY: the kernel mapped exactly `len` zeroed bytes
            // read/write at `base` (`shm_create` maps the length it was
            // asked for) and the mapping stays live until the next resize
            // unmaps it — nothing else aliases it, and the window protocol
            // serialises access (the app is parked in `present` while the
            // session reads). A resize replaces `base`/`len` together only
            // after the old region is unmapped, so the pair is never stale.
            unsafe { core::slice::from_raw_parts_mut(self.base as *mut u8, self.len) }
        }
    }

    /// Draw the whole viewer window — the header, the "Open…" button, the
    /// text area, and the scrollbar — and present it.
    fn present_viewer<T: WindowTransport>(
        viewer: &Viewer,
        theme: &tairix_theme::Theme,
        client: &mut WindowClient<T>,
        window: u64,
        frames: &mut Frames,
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        viewer
            .render(theme, SCALE, mode.width_px, mode.height_px)
            .ok_or(Errno::NoSpace)
            .and_then(|surface| present_surface(&surface, client, window, frames.as_mut(), mode))
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    #[allow(clippy::too_many_lines)] // One linear bring-up plus one event loop; splitting would obscure the resize teardown ordering.
    fn main() -> i32 {
        // --- The shared window surface: FRAME_COUNT frames shaped as the
        // initial window mode, created here and granted to the session. The
        // viewer is resizable, so the region is re-created (and the old one
        // unmapped) whenever the window manager reports a new client size.
        let mut mode = mode_for(WIN_WIDTH, WIN_HEIGHT);
        let Some((_region_id, base, grant)) = allocate_frames(region_bytes(&mode)) else {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        };
        let mut frames = Frames {
            base,
            len: region_bytes(&mode),
        };

        // --- The event mailbox the app parks on. The id is unique by
        // construction (the shared `event_endpoint_for` naming rule: this
        // task's never-reused kernel id under a fixed tag) and never
        // reserved; the bind is refused otherwise.
        let Ok(origin) = tairix_rt::self_origin() else {
            return fail(EXIT_NO_EVENTS, "own identity unavailable");
        };
        let event_endpoint = tairix_window::event_endpoint_for(origin.pid());
        if tairix_abi::ipc::is_reserved_endpoint(event_endpoint)
            || tairix_rt::port_bind(
                event_endpoint,
                WindowEvent::WIRE_LEN,
                tairix_window::EVENT_MAILBOX_CAPACITY,
            ) != 0
        {
            return fail(EXIT_NO_EVENTS, "event mailbox bind refused");
        }
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return fail(EXIT_NO_EVENTS, "wait-set refused");
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
            return fail(EXIT_NO_EVENTS, "event mailbox wait refused");
        }

        // --- Open the window (resizable: the viewer re-lays-out its text to
        // each new client size). How the viewer starts depends on how it was
        // launched: handed a document on STDIN (the file manager's open
        // hand-off), it shows that file at once; launched on its own, it asks
        // the session's trusted picker. The title names the handed-over
        // document, else the generic app name.
        let document_mode = tairix_rt::arg(1).is_some_and(|arg| arg == DOCUMENT_ROLE_ARG);
        let title = if document_mode {
            tairix_rt::arg(2)
                .and_then(|name| core::str::from_utf8(name).ok())
                .unwrap_or("Viewer")
        } else {
            "Viewer"
        };
        let mut client = WindowClient::new(RtWindowTransport);
        let Ok((window, server)) =
            client.create(grant, event_endpoint, FRAME_COUNT, &mode, title, true)
        else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        let themes = ThemeRegistry::with_builtins();
        let theme = themes.active();

        // The whole window's pointer- and keyboard-driven state: the current
        // file view (or the status message shown in its place), the "Open…"
        // button, and the scrollbar, all composed in the host-tested engine.
        let mut viewer = Viewer::new();

        if document_mode {
            // The launcher handed us the file on STDIN; display it now instead
            // of prompting. A refused read is stated honestly, never faked.
            match read_document() {
                Some(bytes) => viewer.open(bytes, mode.width_px, mode.height_px, theme, SCALE),
                None => viewer.show_status("Document read refused."),
            }
        } else if client.pick_file(window).is_err() {
            // A refused pick (another pick showing, or a session without
            // filesystem reach) is not fatal: the viewer stays open and the
            // "Open…" button or Enter asks again.
            viewer.show_status("Pick refused.");
        }
        if present_viewer(&viewer, theme, &mut client, window, &mut frames, &mode).is_err() {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The event loop: park, apply, repaint. A dead channel ends
        // the app fail-loud; a clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        loop {
            let event = match events.wait(&mut client) {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };
            let mut request_pick = false;
            let repaint = match event {
                WindowEvent::FilePicked { handle, .. } => {
                    match read_picked(handle) {
                        Some(bytes) => {
                            viewer.open(bytes, mode.width_px, mode.height_px, theme, SCALE);
                        }
                        // A refused redemption or read delegated nothing the
                        // viewer can show; state it honestly.
                        None => viewer.show_status("Delegated read refused."),
                    }
                    true
                }
                WindowEvent::PickCancelled { .. } => {
                    viewer.show_status("No file chosen.");
                    true
                }
                WindowEvent::Key {
                    key: KeyInput::Pressed { key, .. },
                    ..
                } => match key {
                    // Enter asks for another pick — the same request the
                    // "Open…" button sends; a refusal (one already showing)
                    // leaves the current content on screen.
                    KeyValue::Named(NamedKeyCode::Enter) => {
                        request_pick = true;
                        false
                    }
                    // Navigation keys drive the shared scroll model and
                    // repaint only when the view actually moved.
                    KeyValue::Named(nav) => navigate(nav, &mut viewer),
                    KeyValue::Char(_) => false,
                },
                // A wheel gesture the desktop forwarded because this window
                // owns its own content scrolling: drive the shared model by
                // its vertical ticks and repaint only when the view moved.
                WindowEvent::Scrolled { dy, .. } => viewer.scroll_ticks(dy),
                // A pointer event over the client area: sync the hover
                // position, then apply the press/release the action names,
                // exactly as the widget gallery's own window channel does.
                // The button and the scrollbar are the only interactive
                // regions, so this is the pointer's whole route into the
                // viewer.
                WindowEvent::Pointer { x, y, action, .. } => {
                    let outcome = apply_pointer(&mut viewer, x, y, action, theme, &mode);
                    request_pick = outcome.open_requested;
                    outcome.changed
                }
                // The window manager resized (or maximized/restored) the
                // window. Re-map the frame region at the new client size, then
                // re-wrap the file and repaint so the content fills the new
                // window rather than leaving stale or clipped pixels.
                WindowEvent::Resized {
                    width_px,
                    height_px,
                    ..
                } => {
                    resize_window(
                        mode_for(width_px.max(MIN_WIN_WIDTH), height_px.max(MIN_WIN_HEIGHT)),
                        theme,
                        &mut client,
                        window,
                        &mut frames,
                        &mut mode,
                        &mut viewer,
                    );
                    true
                }
                WindowEvent::CloseRequested { .. } => {
                    // The desktop asked; close the window and end cleanly.
                    let _ = client.close(window);
                    // Free the frame region before exiting so nothing is left
                    // pinned (the runtime reclaims on exit, but the app is
                    // explicit about the mapping it owns).
                    let _ = tairix_rt::shm_unmap(frames.base as u64, frames.len);
                    return 0;
                }
                // Focus changes, key releases, and minimize repaint nothing.
                // A redraw request is already answered by the client library
                // re-presenting the last frame, which is still what the
                // viewer would draw. Listed rather than caught by a wildcard
                // so a new event forces a decision here.
                WindowEvent::Key { .. }
                | WindowEvent::Focus { .. }
                | WindowEvent::Minimized { .. }
                | WindowEvent::RedrawRequested { .. } => false,
            };
            if request_pick {
                // A refused pick (another pick showing) leaves the current
                // content on screen; the outcome arrives as a later event.
                let _ = client.pick_file(window);
            }
            let outcome = if repaint {
                present_viewer(&viewer, theme, &mut client, window, &mut frames, &mode)
            } else {
                Ok(())
            };
            if outcome.is_err() {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
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
    ) -> ViewerPointerOutcome {
        let point = Point::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        );
        let mut outcome = ViewerPointerOutcome {
            changed: false,
            open_requested: false,
        };
        for input in pointer_input_events(action, point) {
            let step = viewer.on_pointer(&input, mode.width_px, mode.height_px, theme, SCALE);
            outcome.changed |= step.changed;
            outcome.open_requested |= step.open_requested;
        }
        outcome
    }

    /// Re-map the window's frame region onto `new_mode` and repaint at the
    /// new size, keeping the reader's place.
    ///
    /// The ordering is fail-closed: a fresh region is created and granted
    /// first, then adopted only if the session accepts the
    /// [`WindowClient::resize`]. On success the *old* region is unmapped
    /// (never before, so a refused resize leaves the current surface intact);
    /// on refusal the freshly-allocated region is unmapped so nothing leaks.
    /// A region that cannot be allocated at all keeps the current size rather
    /// than crashing or presenting nothing. The caller repaints unconditionally
    /// afterward, since even a refused resize leaves the reported client size
    /// unchanged and the current picture already matches it.
    fn resize_window(
        new_mode: DisplayMode,
        theme: &tairix_theme::Theme,
        client: &mut WindowClient<RtWindowTransport>,
        window: u64,
        frames: &mut Frames,
        mode: &mut DisplayMode,
        viewer: &mut Viewer,
    ) {
        let total = region_bytes(&new_mode);
        let Some((_region_id, new_base, new_grant)) = allocate_frames(total) else {
            // Out of memory for a new region: honestly keep the current
            // window rather than fail the whole app.
            return;
        };
        if client
            .resize(window, new_grant, FRAME_COUNT, &new_mode)
            .is_err()
        {
            // The session refused the re-map: drop the new region and stand on
            // the old geometry (fail closed, no crash).
            let _ = tairix_rt::shm_unmap(new_base as u64, total);
            return;
        }
        // The session adopted the new region; release the old mapping and
        // switch the app onto the new one.
        let _ = tairix_rt::shm_unmap(frames.base as u64, frames.len);
        *frames = Frames {
            base: new_base,
            len: total,
        };
        *mode = new_mode;
        // Re-wrap the open file (if any) to the new width, keeping the
        // reader near their place; a status message needs no re-wrapping.
        viewer.relayout(mode.width_px, mode.height_px, theme, SCALE);
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
