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
//! The bounded, sanitising byte→line model and the themed renderers live
//! in the host-tested `tairix_viewer` engine; this binary composes them
//! over the live syscalls exactly as the files app does: one
//! `shm_create`d frame region granted to the window endpoint, one
//! `port_bind`-bound event mailbox parked on through a wait-set (every
//! accepted event authenticated against the session identity the create
//! reply named), and the `WindowClient` calls over `ipc_call`. `Enter`
//! asks for another pick; a `CloseRequested` from the desktop ends the
//! program cleanly. Every bring-up refusal exits fail-loud with a
//! reserved code and a stated reason on `stderr`.
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
    use tairix_abi::window_ipc::{WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, DOCUMENT_ROLE_ARG, ORIGIN_WIRE_LEN, STDIN,
    };
    use tairix_theme::ThemeRegistry;
    use tairix_viewer::{
        content_lines, render_lines, render_status, visible_cols_for, visible_rows_for, ScrollView,
        CONTENT_MAX, MAX_LINES, MIN_WIN_HEIGHT, MIN_WIN_WIDTH, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_window::{EventSource, WindowClient, WindowEvents, WindowTransport};

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
        let _ = tairix_rt::stderr(b"viewer: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
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

    /// Paint the one-line status message `text` for the current window.
    fn show_status<T: WindowTransport>(
        text: &str,
        theme: &tairix_theme::Theme,
        client: &mut WindowClient<T>,
        window: u64,
        frames: &mut Frames,
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        render_status(text, theme, mode.width_px, mode.height_px)
            .ok_or(Errno::NoSpace)
            .and_then(|surface| present_surface(&surface, client, window, frames.as_mut(), mode))
    }

    /// Repaint the current window of the scrolled file for the current
    /// window size.
    fn repaint_view<T: WindowTransport>(
        scroll: &ScrollView,
        theme: &tairix_theme::Theme,
        client: &mut WindowClient<T>,
        window: u64,
        frames: &mut Frames,
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        render_lines(scroll.visible(), theme, mode.width_px, mode.height_px)
            .ok_or(Errno::NoSpace)
            .and_then(|surface| present_surface(&surface, client, window, frames.as_mut(), mode))
    }

    /// The waiting/prompt status shown before a file is chosen and after a
    /// resize while no file is open. One definition so the prompt reads the
    /// same everywhere.
    const WAITING: &str = "Choose a file...";

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

        // The currently viewed file, scrolled through the shared engine. None
        // until content arrives; a re-pick or refusal replaces it. The raw
        // bytes are kept alongside the view so a resize can re-wrap the file
        // to the new column count rather than losing content past the old
        // width.
        let mut view: Option<ScrollView> = None;
        let mut content: Option<Vec<u8>> = None;

        if document_mode {
            // The launcher handed us the file on STDIN; display it now instead
            // of prompting. A refused read is stated honestly, never faked.
            match read_document() {
                Some(bytes) => {
                    let lines = content_lines(&bytes, MAX_LINES, visible_cols_for(mode.width_px));
                    let scroll = ScrollView::new(lines, visible_rows_for(mode.height_px));
                    let present =
                        repaint_view(&scroll, theme, &mut client, window, &mut frames, &mode);
                    view = Some(scroll);
                    content = Some(bytes);
                    if present.is_err() {
                        return fail(EXIT_CHANNEL_LOST, "first present refused");
                    }
                }
                None => {
                    if show_status(
                        "Document read refused",
                        theme,
                        &mut client,
                        window,
                        &mut frames,
                        &mode,
                    )
                    .is_err()
                    {
                        return fail(EXIT_CHANNEL_LOST, "first present refused");
                    }
                }
            }
        } else {
            if show_status(WAITING, theme, &mut client, window, &mut frames, &mode).is_err() {
                return fail(EXIT_CHANNEL_LOST, "first present refused");
            }
            if client.pick_file(window).is_err() {
                // A refused pick (another pick showing, or a session without
                // filesystem reach) is not fatal: the viewer stays open and
                // Enter asks again.
                let _ = show_status(
                    "Pick refused - Enter retries",
                    theme,
                    &mut client,
                    window,
                    &mut frames,
                    &mode,
                );
            }
        }

        // --- The event loop: park, apply, repaint. A dead channel ends
        // the app fail-loud; a clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        loop {
            let event = match events.wait() {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };
            let outcome = match event {
                WindowEvent::FilePicked { handle, .. } => match read_picked(handle) {
                    Some(bytes) => {
                        // Keep every line (bounded) so the file can be
                        // scrolled, not just its first screenful, and keep the
                        // raw bytes so a resize can re-wrap them.
                        let lines =
                            content_lines(&bytes, MAX_LINES, visible_cols_for(mode.width_px));
                        view = Some(ScrollView::new(lines, visible_rows_for(mode.height_px)));
                        content = Some(bytes);
                        match view.as_ref() {
                            Some(scroll) => {
                                repaint_view(scroll, theme, &mut client, window, &mut frames, &mode)
                            }
                            None => Ok(()),
                        }
                    }
                    // A refused redemption or read delegated nothing the
                    // viewer can show; state it honestly.
                    None => {
                        view = None;
                        content = None;
                        show_status(
                            "Delegated read refused",
                            theme,
                            &mut client,
                            window,
                            &mut frames,
                            &mode,
                        )
                    }
                },
                WindowEvent::PickCancelled { .. } => show_status(
                    "No file chosen - Enter retries",
                    theme,
                    &mut client,
                    window,
                    &mut frames,
                    &mode,
                ),
                WindowEvent::Key {
                    key: KeyInput::Pressed { key, .. },
                    ..
                } => match key {
                    // Enter asks for another pick; a refusal (one already
                    // showing) leaves the current content on screen.
                    KeyValue::Named(NamedKeyCode::Enter) => {
                        let _ = client.pick_file(window);
                        Ok(())
                    }
                    // Navigation keys drive the shared scroll model and
                    // repaint only when the view actually moved.
                    KeyValue::Named(nav) => scroll_view(nav, view.as_mut())
                        .filter(|moved| *moved)
                        .map_or(Ok(()), |_| match view.as_ref() {
                            Some(scroll) => {
                                repaint_view(scroll, theme, &mut client, window, &mut frames, &mode)
                            }
                            None => Ok(()),
                        }),
                    KeyValue::Char(_) => Ok(()),
                },
                // A wheel gesture the desktop forwarded because this window
                // owns its own content scrolling: drive the shared model by
                // its vertical ticks and repaint only when the view moved.
                WindowEvent::Scrolled { dy, .. } => {
                    if view.as_mut().is_some_and(|scroll| scroll.scroll_ticks(dy)) {
                        match view.as_ref() {
                            Some(scroll) => {
                                repaint_view(scroll, theme, &mut client, window, &mut frames, &mode)
                            }
                            None => Ok(()),
                        }
                    } else {
                        Ok(())
                    }
                }
                // The window manager resized (or maximized/restored) the
                // window. Re-map the frame region at the new client size, then
                // re-wrap the file and repaint so the content fills the new
                // window rather than leaving stale or clipped pixels.
                WindowEvent::Resized {
                    width_px,
                    height_px,
                    ..
                } => resize_window(
                    mode_for(width_px.max(MIN_WIN_WIDTH), height_px.max(MIN_WIN_HEIGHT)),
                    theme,
                    &mut client,
                    window,
                    &mut frames,
                    &mut mode,
                    content.as_deref(),
                    &mut view,
                ),
                WindowEvent::CloseRequested { .. } => {
                    // The desktop asked; close the window and end cleanly.
                    let _ = client.close(window);
                    // Free the frame region before exiting so nothing is left
                    // pinned (the runtime reclaims on exit, but the app is
                    // explicit about the mapping it owns).
                    let _ = tairix_rt::shm_unmap(frames.base as u64, frames.len);
                    return 0;
                }
                // Focus changes, key releases, and pointer events repaint
                // nothing; the viewer is picker- and keyboard-driven.
                _ => Ok(()),
            };
            if outcome.is_err() {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
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
    /// than crashing or presenting nothing.
    #[allow(clippy::too_many_arguments)] // The resize touches every piece of the window's live state exactly once.
    fn resize_window(
        new_mode: DisplayMode,
        theme: &tairix_theme::Theme,
        client: &mut WindowClient<RtWindowTransport>,
        window: u64,
        frames: &mut Frames,
        mode: &mut DisplayMode,
        content: Option<&[u8]>,
        view: &mut Option<ScrollView>,
    ) -> Result<(), Errno> {
        let total = region_bytes(&new_mode);
        let Some((_region_id, new_base, new_grant)) = allocate_frames(total) else {
            // Out of memory for a new region: honestly keep the current
            // window rather than fail the whole app.
            return Ok(());
        };
        if client
            .resize(window, new_grant, FRAME_COUNT, &new_mode)
            .is_err()
        {
            // The session refused the re-map: drop the new region and stand on
            // the old geometry (fail closed, no crash).
            let _ = tairix_rt::shm_unmap(new_base as u64, total);
            return Ok(());
        }
        // The session adopted the new region; release the old mapping and
        // switch the app onto the new one.
        let _ = tairix_rt::shm_unmap(frames.base as u64, frames.len);
        *frames = Frames {
            base: new_base,
            len: total,
        };
        *mode = new_mode;
        // Re-wrap the stored file to the new width and repaint, keeping the
        // reader near their place; with no file open, redraw the prompt.
        match content {
            Some(bytes) => {
                let lines = content_lines(bytes, MAX_LINES, visible_cols_for(mode.width_px));
                let rows = visible_rows_for(mode.height_px);
                match view.as_mut() {
                    Some(scroll) => scroll.relayout(lines, rows),
                    None => *view = Some(ScrollView::new(lines, rows)),
                }
                match view.as_ref() {
                    Some(scroll) => repaint_view(scroll, theme, client, window, frames, mode),
                    None => Ok(()),
                }
            }
            None => show_status(WAITING, theme, client, window, frames, mode),
        }
    }

    /// Apply a navigation key to the scroll `view`, returning whether the view
    /// moved (or `None` when the key is not a scroll key or there is no view).
    fn scroll_view(key: NamedKeyCode, view: Option<&mut ScrollView>) -> Option<bool> {
        let view = view?;
        let moved = match key {
            NamedKeyCode::Up => view.line_up(),
            NamedKeyCode::Down => view.line_down(),
            NamedKeyCode::PageUp => view.page_up(),
            NamedKeyCode::PageDown => view.page_down(),
            NamedKeyCode::Home => view.to_top(),
            NamedKeyCode::End => view.to_bottom(),
            _ => return None,
        };
        Some(moved)
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
