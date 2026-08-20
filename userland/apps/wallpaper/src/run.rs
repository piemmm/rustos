//! The `wallpaper.app` bundle's `Run` entry point (`plans/PINBOARD.md` P9):
//! the windowed desktop-backdrop chooser.
//!
//! # What the program wires (and what stays in the library)
//!
//! The chooser's model, layout, and themed painters live in the
//! host-tested `tairix_wallpaper_chooser` engine; this binary composes them
//! over the live syscalls exactly as the viewer and the file manager do:
//! one `shm_create`d frame region granted to the window endpoint, one
//! `port_bind`-bound event mailbox parked on through a wait-set (every
//! accepted event authenticated against the session identity the create
//! reply named), and the `WindowClient` calls over `ipc_call`.
//!
//! # Untrusted pixels are decoded elsewhere
//!
//! A wallpaper file is untrusted input, so the chooser decodes none of it
//! in its own address space. Each candidate's preview is produced by
//! `tairix_sandbox`'s image-render service running in a capability-empty
//! child this same binary is re-entered as (the reserved worker-role
//! argument): the chooser hands the child the file's bytes and receives
//! validated straight-alpha pixels back, and a refusal costs the candidate
//! a placeholder tile, never the app. Previews are rendered at the
//! currently selected fit, so a fit change re-renders them — a candidate
//! the worker already refused is not asked again.
//!
//! # It asks; the session decides
//!
//! Apply renders the settings document the engine owns and posts it to the
//! desktop session's pinboard rendezvous. The reply — adopted, refused
//! with its reason, or no session listening at all — is reported in the
//! window; nothing is ever reported as applied that the session did not
//! accept, and a refusal never ends the program. Every bring-up refusal
//! exits fail-loud with a reserved code and a stated reason on `stderr`.
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

    use alloc::string::String;
    use alloc::vec::Vec;

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::fs::OpenFlags;
    use tairix_abi::input::KeyInput;
    use tairix_abi::pinboard_ipc::{PinboardDocument, PinboardRequest, PINBOARD_ENDPOINT};
    use tairix_abi::reply::{decode_status_reply, STATUS_REPLY_LEN};
    use tairix_abi::window_ipc::{WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, ORIGIN_WIRE_LEN};
    use tairix_display::{winframe, SERIAL};
    use tairix_font::BitmapFont;
    use tairix_geometry::Point;
    use tairix_input::InputEvent;
    use tairix_raster::Surface;
    use tairix_rt::io::{Stderr, Write};
    use tairix_sandbox::imagerender::{render_wallpaper_for_screen, ImageRenderService};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_theme::{TextRole, Theme, ThemeRegistry};
    use tairix_wallpaper::{
        catalog_entries, user_settings_path, PinboardSettings, WallpaperFit, WallpaperPath,
        MAX_SETTINGS_LEN, MAX_WALLPAPER_BYTES, WALLPAPER_STORE,
    };
    use tairix_wallpaper_chooser::{
        candidates_from_catalog, ApplyOutcome, Chooser, ChooserAction, Style, MIN_WIN_HEIGHT,
        MIN_WIN_WIDTH, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_window::{
        key_input_event, pointer_input_events, Desktop, EventSource, WindowClient, WindowEvents,
        WindowSizing, WindowTransport,
    };

    /// Exit code when the shared frame region could not be created or
    /// granted to the window endpoint. A reserved, fail-closed value.
    const EXIT_NO_FRAMES: i32 = 81;

    /// Exit code when the event mailbox could not be bound or observed
    /// through the wait-set. A reserved, fail-closed value: the app exits
    /// rather than degrade into a busy re-poll.
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

    /// The window title the desktop lists this app under.
    const TITLE: &str = "Wallpaper";

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-ret`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// State a reason on `stderr` (fail loud: an exit code alone is not a
    /// diagnosis, and a refused optional step still says so).
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "wallpaper: {reason}");
    }

    /// Declare this application's presence on the desktop's icon bar: a
    /// *Quit* row and the session-drawn *About* row, with the primary click
    /// left to the session so it raises the window.
    ///
    /// A refused declaration is an answer, not a death: the application says
    /// so and carries on with no slot of its own — its window is still
    /// reachable through the one the session derives from it.
    fn declare_app_bar(client: &mut WindowClient<RtWindowTransport>, endpoint: u64) {
        match tairix_window::quit_and_about(endpoint) {
            Ok(bar) => {
                if let Err(err) = client.set_app_bar(&bar) {
                    report(&alloc::format!(
                        "the desktop refused this application's icon-bar presence ({err}); \
                         carrying on without one"
                    ));
                }
            }
            Err(err) => report(&alloc::format!(
                "this application's icon-bar menu is invalid ({err:?}); carrying on without one"
            )),
        }
    }

    /// State the abnormal-exit reason and hand `code` back for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        report(reason);
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

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty, and accept only
    /// events whose kernel-attested sender is the desktop session named by
    /// the create reply — anything else is dropped (fail closed), so no
    /// other process can feed the app forged input.
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
                        // capable sender, so the kernel-attested origin is
                        // the authentication.
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
                        if token == PRESSURE_TOKEN && tairix_procinfo::pressure::refresh() {
                            tairix_font::trim_glyph_cache();
                        }
                    }
                    Err(err) => return Err(errno_from(err)),
                }
            }
        }
    }

    /// Open `path` for reading under the launching user's own identity,
    /// surfacing the kernel's own refusal so the caller can tell an absent
    /// file from one it may not read.
    fn open_read(path: &str) -> Result<u32, Errno> {
        let raw = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        u32::try_from(raw).map_err(|_| errno_from(raw))
    }

    /// Read at most `max` bytes of the open descriptor `fd`, closing it
    /// either way. Fails closed to `None` on a read error; a short file
    /// reads back as `Some` of what was there, never a fabricated byte.
    fn read_bounded(fd: u32, max: usize) -> Option<Vec<u8>> {
        let outcome = read_open_fd(fd, max);
        let _ = tairix_rt::fs_close(fd);
        outcome
    }

    /// The bounded read itself; [`read_bounded`] owns closing the
    /// descriptor so every exit from here releases it.
    ///
    /// The streaming is the runtime's one whole-file policy, so the chooser
    /// and the desktop session cannot drift to different chunk sizes for the
    /// same wallpaper master. It answers one chunk past `max`, which is what
    /// lets an oversize file be *refused* here rather than silently truncated
    /// into a decode that would fail with no reason a user can act on.
    fn read_open_fd(fd: u32, max: usize) -> Option<Vec<u8>> {
        let content = tairix_rt::read_fd_to_end(fd, max).ok()?;
        (content.len() <= max).then_some(content)
    }

    /// The pinboard settings in effect for the launching user, so the
    /// chooser opens on what is actually on screen.
    ///
    /// An **absent** document means the documented defaults and is not an
    /// error — a fresh account has never applied one. Anything else that
    /// stops the document being used (no `HOME`, a refused read, bytes
    /// that are not UTF-8, a document the shared parser refuses) also
    /// yields the defaults, but says so on `stderr` rather than opening on
    /// settings the user cannot see the reason for.
    fn settings_in_effect() -> PinboardSettings {
        let Some(home) = tairix_rt::env_var(b"HOME").and_then(str_from) else {
            report("no home directory in the environment; showing the default settings");
            return PinboardSettings::default();
        };
        let Some(path) = user_settings_path(home) else {
            report("home directory names no settings store; showing the default settings");
            return PinboardSettings::default();
        };
        let fd = match open_read(&path) {
            Ok(fd) => fd,
            // Nothing applied yet: the defaults are the honest answer.
            Err(Errno::NotFound) => return PinboardSettings::default(),
            Err(err) => {
                report(&alloc::format!(
                    "{path}: {err:?}; showing the default settings"
                ));
                return PinboardSettings::default();
            }
        };
        let Some(bytes) = read_bounded(fd, MAX_SETTINGS_LEN) else {
            report(&alloc::format!(
                "{path}: read refused; showing the default settings"
            ));
            return PinboardSettings::default();
        };
        let Some(text) = str_from(&bytes) else {
            report(&alloc::format!(
                "{path}: not valid UTF-8; showing the default settings"
            ));
            return PinboardSettings::default();
        };
        match tairix_wallpaper::parse(text) {
            Ok(settings) => settings,
            Err(err) => {
                report(&alloc::format!(
                    "{path}: {err}; showing the default settings"
                ));
                PinboardSettings::default()
            }
        }
    }

    /// `bytes` as UTF-8, or `None` — the one spelling of "these bytes are
    /// text" this program uses.
    fn str_from(bytes: &[u8]) -> Option<&str> {
        core::str::from_utf8(bytes).ok()
    }

    /// The wallpapers the shipped store offers, discovered by listing it
    /// under the launching user's own identity.
    ///
    /// A store that cannot be listed is not fatal: the chooser still
    /// offers "no wallpaper" and every backdrop colour, so the refusal is
    /// stated and an empty candidate list returned.
    fn store_candidates() -> Vec<tairix_wallpaper_chooser::Candidate> {
        let stream = match tairix_rt::read_dir_all(WALLPAPER_STORE.as_bytes()) {
            Ok(stream) => stream,
            Err(err) => {
                report(&alloc::format!(
                    "{WALLPAPER_STORE}: {:?}; no shipped wallpapers offered",
                    errno_from(err)
                ));
                return Vec::new();
            }
        };
        let Ok(entries) = tairix_browse::vfs::entries_from_dir_stream(
            WALLPAPER_STORE,
            &stream,
            &mut tairix_browse::RtLinkReader,
        ) else {
            report(&alloc::format!(
                "{WALLPAPER_STORE}: listing not readable; no shipped wallpapers offered"
            ));
            return Vec::new();
        };
        // The shared catalog builder decides what counts as a wallpaper
        // (name shape, extension, ordering, and the listing bound); this
        // only drops the directories, which are never candidates.
        let catalog = catalog_entries(entries.iter().filter(|e| !e.is_directory_backed()).map(
            |entry| {
                (
                    entry.name(),
                    usize::try_from(entry.size()).unwrap_or(usize::MAX),
                )
            },
        ));
        candidates_from_catalog(&catalog)
    }

    /// Render one wallpaper through the sandboxed worker: read the file
    /// (bounded by the shared wallpaper byte bound) and ask the worker to
    /// place it into a `width` x `height` destination under `fit`, modelling
    /// a `screen`-sized composition (the gallery's thumbnails pass
    /// `screen == (width, height)`; the preview panel passes the desktop's
    /// own screen extent, so `Centre` and `Tile` preview at true scale).
    ///
    /// Fails closed to `None` — a file this app cannot read, a worker that
    /// refuses it, or pixels that do not fill the destination exactly all
    /// leave the caller to show a placeholder. The reason is stated on
    /// `stderr` once, since the refusal is remembered and never retried.
    fn render_placed(
        sandbox: &mut ParserSandbox<RtLauncher, tairix_rt::LogSink>,
        path: &WallpaperPath,
        screen: (u32, u32),
        fit: WallpaperFit,
        width: u32,
        height: u32,
    ) -> Option<Surface> {
        let spelled = path.as_str();
        let fd = match open_read(spelled) {
            Ok(fd) => fd,
            Err(err) => {
                report(&alloc::format!("{spelled}: {err:?}; not shown"));
                return None;
            }
        };
        let Some(bytes) = read_bounded(fd, MAX_WALLPAPER_BYTES) else {
            report(&alloc::format!(
                "{spelled}: unreadable, or larger than {MAX_WALLPAPER_BYTES} bytes; not shown"
            ));
            return None;
        };
        match render_wallpaper_for_screen(sandbox, screen, width, height, fit, &bytes) {
            Ok(rgba) => Surface::from_rgba8(width, height, &rgba),
            Err(failure) => {
                report(&alloc::format!("{spelled}: {failure}; not shown"));
                None
            }
        }
    }

    /// Ask the desktop session to adopt `document` and report what it
    /// said.
    ///
    /// Nothing is reported as applied that the session did not accept: a
    /// document this program cannot even encode, an unanswered rendezvous,
    /// and a typed refusal are three distinct outcomes.
    fn apply(document: &str) -> ApplyOutcome {
        let Ok(carried) = PinboardDocument::new(document) else {
            return ApplyOutcome::Refused(String::from("settings document out of range"));
        };
        let request = PinboardRequest::Apply { document: carried }.to_le_bytes();
        let mut reply = [0u8; STATUS_REPLY_LEN];
        let Ok(len) = tairix_rt::ipc_call(PINBOARD_ENDPOINT, &request, &mut reply) else {
            return ApplyOutcome::NoDesktop;
        };
        match decode_status_reply(&reply[..len.min(reply.len())]) {
            Ok(()) => ApplyOutcome::Applied,
            Err(err) => ApplyOutcome::Refused(alloc::format!("{err:?}")),
        }
    }

    /// Copy `surface` into the shared window frame and present it whole.
    fn present_surface<T: WindowTransport>(
        surface: &Surface,
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
    /// row. The one place the chooser's mode is shaped, so the create and
    /// every resize agree on stride and format.
    fn mode_for(width_px: u32, height_px: u32) -> DisplayMode {
        DisplayMode {
            width_px,
            height_px,
            stride_bytes: width_px.saturating_mul(4),
            format: DisplayFormat::Rgba8888,
        }
    }

    /// Total bytes a `FRAME_COUNT`-frame region shaped as `mode` needs.
    fn region_bytes(mode: &DisplayMode) -> usize {
        (mode.stride_bytes as usize) * (mode.height_px as usize) * FRAME_COUNT as usize
    }

    /// Create a `total`-byte frame region and grant it to the window
    /// endpoint, returning its mapped base and the endpoint-directed grant
    /// handle. Fails closed to `None` on any refusal, unmapping a region
    /// that mapped but could not be granted so a refused (re)allocation
    /// never leaves pinned memory behind.
    fn allocate_frames(total: usize) -> Option<(usize, u64)> {
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
        Some((base, grant as u64))
    }

    /// The live frame region: the once-granted shared surface the app
    /// paints into. Re-mapped on every resize; the old mapping is unmapped
    /// only after the session adopts the new one, so a refused resize
    /// keeps the current surface intact.
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

    /// The one style the chooser paints and hit-tests through: the active
    /// theme's interface face at the real desktop's own density and screen
    /// extent, exactly as the file manager and the control gallery resolve
    /// theirs — the screen extent is what lets the preview panel model the
    /// real screen rather than a guessed shape.
    fn style_for<'a>(theme: &'a Theme, desktop: &Desktop) -> Style<'a> {
        Style::new(
            theme,
            desktop.scale(),
            BitmapFont::for_role(theme.fonts(), TextRole::Body, desktop.scale()),
            (desktop.screen_width_px(), desktop.screen_height_px()),
        )
    }

    /// Repaint the chooser for the current window size.
    fn repaint<T: WindowTransport>(
        chooser: &mut Chooser,
        theme: &Theme,
        desktop: &Desktop,
        client: &mut WindowClient<T>,
        window: u64,
        frames: &mut Frames,
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        chooser
            .render(style_for(theme, desktop))
            .ok_or(Errno::NoSpace)
            .and_then(|surface| present_surface(&surface, client, window, frames.as_mut(), mode))
    }

    /// Re-map the window's frame region onto `new_mode` and re-lay the
    /// chooser out at the new client size. The caller repaints, through the
    /// one path every other change repaints through.
    ///
    /// The ordering is fail-closed: a fresh region is created and granted
    /// first, then adopted only if the session accepts the resize. On
    /// success the *old* region is unmapped (never before, so a refused
    /// resize leaves the current surface intact); on refusal the
    /// freshly-allocated region is unmapped so nothing leaks. A region
    /// that cannot be allocated at all keeps the current size rather than
    /// ending the app.
    fn resize_window(
        new_mode: DisplayMode,
        chooser: &mut Chooser,
        client: &mut WindowClient<RtWindowTransport>,
        window: u64,
        frames: &mut Frames,
        mode: &mut DisplayMode,
    ) {
        let total = region_bytes(&new_mode);
        let Some((new_base, new_grant)) = allocate_frames(total) else {
            return;
        };
        if client
            .resize(window, new_grant, FRAME_COUNT, &new_mode)
            .is_err()
        {
            let _ = tairix_rt::shm_unmap(new_base as u64, total);
            return;
        }
        let _ = tairix_rt::shm_unmap(frames.base as u64, frames.len);
        *frames = Frames {
            base: new_base,
            len: total,
        };
        *mode = new_mode;
        chooser.relayout(mode.width_px, mode.height_px);
    }

    /// The later of two things the chooser was asked for while handling one
    /// delivered event.
    ///
    /// A wire pointer event is a position and then, sometimes, a button
    /// transition, so two answers arrive for one event: whichever of them
    /// asked for something is what the event meant, and a transition's
    /// answer supersedes the move's.
    const fn latest(first: ChooserAction, second: ChooserAction) -> ChooserAction {
        match second {
            ChooserAction::None => first,
            asked => asked,
        }
    }

    /// Bind the app's own event mailbox and add it to a fresh wait-set,
    /// returning both. Fails closed with the reserved exit code on any
    /// refusal rather than degrading into a re-poll.
    fn bind_event_mailbox() -> Result<(u64, u64), i32> {
        let Ok(origin) = tairix_rt::self_origin() else {
            return Err(fail(EXIT_NO_EVENTS, "own identity unavailable"));
        };
        let endpoint = tairix_window::event_endpoint_for(origin.pid());
        if tairix_abi::ipc::is_reserved_endpoint(endpoint)
            || tairix_rt::port_bind(
                endpoint,
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
            endpoint,
            EVENT_TOKEN,
        ) != 0
        {
            return Err(fail(EXIT_NO_EVENTS, "event mailbox wait refused"));
        }
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            return Err(fail(EXIT_NO_EVENTS, "memory-pressure wake refused"));
        }
        Ok((endpoint, set))
    }

    /// Render one outstanding picture — the preview panel first, then the
    /// next gallery thumbnail — recording either its pixels or its refusal,
    /// and report whether anything was done.
    ///
    /// One picture per call, so the event loop stays responsive while a
    /// large store fills in, and the panel the user is actually looking at
    /// is always the next thing rendered. A refusal is remembered, so a
    /// wallpaper that will not decode costs exactly one attempt.
    ///
    /// A thumbnail is the wallpaper itself at tile size, not a preview of
    /// the fit, so it is always placed to fill its square: the gallery says
    /// *which* wallpaper each tile is, and the preview panel is where the
    /// chosen fit is shown.
    fn resolve_one_render(
        chooser: &mut Chooser,
        sandbox: &mut ParserSandbox<RtLauncher, tairix_rt::LogSink>,
        theme: &Theme,
        desktop: &Desktop,
    ) -> bool {
        let style = style_for(theme, desktop);
        if let Some(request) = chooser.next_preview(style) {
            match render_placed(
                sandbox,
                &request.path,
                request.screen,
                request.fit,
                request.width,
                request.height,
            ) {
                Some(surface) => chooser.set_preview(request, surface),
                None => chooser.mark_preview_refused(request),
            }
            return true;
        }
        let Some(request) = chooser.next_thumbnail(style) else {
            return false;
        };
        // A thumbnail answers *which* wallpaper it is and always fills its
        // own square, so it models a screen exactly as large as itself.
        match render_placed(
            sandbox,
            &request.path,
            (request.side, request.side),
            WallpaperFit::Fill,
            request.side,
            request.side,
        ) {
            Some(surface) => chooser.set_thumbnail(request.index, surface),
            None => chooser.mark_thumbnail_refused(request.index),
        }
        true
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    #[allow(clippy::too_many_lines)] // One linear bring-up plus one event loop; splitting would obscure the resize teardown ordering.
    fn main() -> i32 {
        // --- The sandbox-worker role, before anything else: a wallpaper
        // file is untrusted input, so it is decoded by a capability-empty
        // child this same binary is re-entered as with the reserved role
        // argument. That child serves render requests over its wired
        // standard streams and nothing else — it never becomes the chooser.
        if worker_role() {
            let mut service = ImageRenderService::default();
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }

        // --- What the user has now, and what they may choose.
        let settings = settings_in_effect();
        let mut chooser = Chooser::new(store_candidates(), &settings);

        // --- The desktop the app must fit on: screen extent, UI scale, and
        // light/dark appearance. Queried once, before any window is
        // created, so the first frame is correctly sized, themed, and
        // modelled at the real screen's own scale — never a guessed screen
        // or a default scale. A refused query, or a desktop this client
        // cannot draw at, is the same fail-closed outcome as a refused
        // window create.
        let mut client = WindowClient::new(RtWindowTransport);
        let info = match client.desktop() {
            Ok(info) => info,
            Err(err) => {
                let _ = writeln!(Stderr, "wallpaper: desktop query refused: {err}");
                return EXIT_NO_WINDOW;
            }
        };
        let mut desktop = match Desktop::new(info) {
            Ok(desktop) => desktop,
            Err(err) => {
                let _ = writeln!(Stderr, "wallpaper: cannot draw this desktop: {err}");
                return EXIT_NO_WINDOW;
            }
        };
        let mut themes = ThemeRegistry::with_builtins();
        themes.set_appearance(desktop.appearance());
        let mut theme = themes.active();

        // --- The shared window surface: FRAME_COUNT frames shaped as the
        // initial window mode (the desktop's own preferred size, capped to
        // its screen), created here and granted to the session.
        let (initial_w, initial_h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mut mode = mode_for(initial_w, initial_h);
        let Some((base, grant)) = allocate_frames(region_bytes(&mode)) else {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        };
        let mut frames = Frames {
            base,
            len: region_bytes(&mode),
        };

        // --- The event mailbox the app parks on.
        let (event_endpoint, set) = match bind_event_mailbox() {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        // --- Open the window (resizable: the grid re-lays out to each new
        // client size, down to the floor the window manager is told to hold)
        // and paint the first frame.
        // The icon-bar presence first: a declared presence belongs to the
        // process, so declaring it before this process owns a window is what
        // makes its slot carry this menu from the moment it appears rather
        // than being a slot the session derived from a window, which opens
        // nothing.
        declare_app_bar(&mut client, event_endpoint);
        let sizing = WindowSizing {
            resizable: true,
            min_width_px: MIN_WIN_WIDTH,
            min_height_px: MIN_WIN_HEIGHT,
        };
        let Ok((window, server)) =
            client.create(grant, event_endpoint, FRAME_COUNT, &mode, TITLE, sizing)
        else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        chooser.relayout(mode.width_px, mode.height_px);
        if repaint(
            &mut chooser,
            theme,
            &desktop,
            &mut client,
            window,
            &mut frames,
            &mode,
        )
        .is_err()
        {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The preview worker: one sandboxed child, started on the first
        // render and replaced by the seam if it ever fails.
        let mut sandbox = ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink);

        // --- The event loop: fill in previews while there is work, else
        // park, apply, repaint. A dead channel ends the app fail-loud; a
        // clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        loop {
            // Outstanding preview work is done before parking, so the grid
            // fills in as fast as the worker can render and the loop never
            // waits on work it already holds.
            if resolve_one_render(&mut chooser, &mut sandbox, theme, &desktop) {
                if repaint(
                    &mut chooser,
                    theme,
                    &desktop,
                    &mut client,
                    window,
                    &mut frames,
                    &mode,
                )
                .is_err()
                {
                    return fail(EXIT_CHANNEL_LOST, "present refused");
                }
                continue;
            }
            let event = match events.wait(&mut client) {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };

            // Apply the desktop change before the chooser-specific event
            // logic, so every derived value (scale, theme, screen extent)
            // is current for the repaint below. A screen-extent change
            // makes the true-scale model box a different size, which
            // alone makes the held preview stale — `next_preview` notices
            // through the request it now wants, so nothing further needs
            // invalidating by hand. A refused change states the reason and
            // stands on the last good desktop.
            let desktop_changed = match desktop.apply(&event) {
                Ok(true) => {
                    themes.set_appearance(desktop.appearance());
                    theme = themes.active();
                    chooser.relayout(mode.width_px, mode.height_px);
                    true
                }
                Ok(false) => false,
                Err(err) => {
                    report(&alloc::format!("desktop change refused: {err}"));
                    false
                }
            };

            // What the event means to the chooser. Every arm answers in
            // the one vocabulary the engine speaks, so the decision about
            // what to *do* about it is made once, below.
            let asked = match event {
                // The pointer is the chooser's primary input: one delivered
                // wire event is the position it happened at and, for a
                // press or a release, the button transition after it — the
                // shared translation every windowed app uses.
                WindowEvent::Pointer { x, y, action, .. } => {
                    let at = Point::new(
                        i32::try_from(x).unwrap_or(i32::MAX),
                        i32::try_from(y).unwrap_or(i32::MAX),
                    );
                    let mut asked = ChooserAction::None;
                    // One sink for the whole round: both synthesised events
                    // reach the same controls, which report into it.
                    let mut damage = tairix_controls::damage::sink();
                    for input in pointer_input_events(action, at) {
                        asked = latest(
                            asked,
                            chooser.on_pointer(&input, style_for(theme, &desktop), &mut damage),
                        );
                    }
                    asked
                }
                WindowEvent::Scrolled { dx, dy, .. } => chooser.on_pointer(
                    &InputEvent::PointerScrolled { dx, dy },
                    style_for(theme, &desktop),
                    &mut tairix_controls::damage::sink(),
                ),
                // The keyboard is the secondary path, and reaches
                // everything the pointer does.
                WindowEvent::Key {
                    key: pressed @ KeyInput::Pressed { .. },
                    ..
                } => match key_input_event(pressed) {
                    InputEvent::KeyPressed { key, modifiers } => {
                        chooser.on_key(key, modifiers, style_for(theme, &desktop))
                    }
                    _ => ChooserAction::None,
                },
                // The window manager resized (or maximized/restored) the
                // window: re-map the frame region at the new client size
                // and re-lay everything out. The repaint is the shared one
                // below, exactly as for any other change.
                //
                // The reported size is adopted exactly: the declared minimum
                // is the window manager's to hold, and an app that pushed
                // back here would fight the drag frame by frame.
                WindowEvent::Resized {
                    width_px,
                    height_px,
                    ..
                } => {
                    resize_window(
                        mode_for(width_px, height_px),
                        &mut chooser,
                        &mut client,
                        window,
                        &mut frames,
                        &mut mode,
                    );
                    ChooserAction::Changed
                }
                // The desktop asked, or *Quit* was chosen on the chooser's
                // own icon-bar slot. A row the declaration never carried
                // names no command and is ignored (fail closed).
                WindowEvent::CloseRequested { .. } => ChooserAction::Close,
                WindowEvent::AppBarMenu { item } if tairix_window::is_quit(item) => {
                    ChooserAction::Close
                }
                // A key release repaints nothing: every control acts on the
                // press. Focus changes and minimize leave the window's own
                // content exactly as it was. A redraw request is already
                // answered by the client library re-presenting the last
                // frame, which is still what the chooser would draw. A pick
                // conclusion can only arrive for a pick this app never asks
                // for. A desktop-change announcement is adopted above,
                // which is also where the repaint a real change needs is
                // decided. Listed rather than caught by a wildcard so a new
                // event forces a decision here.
                //
                // A secondary press on Close asks to leave what the window is
                // showing; the chooser has nothing to leave but itself, and a
                // primary press already closes it.
                // The chooser declares no default action, so the session
                // raises its window on a click rather than telling it — an
                // `AppBarDefault` therefore cannot arrive, and an
                // `AppBarMenu` naming any other row names no command of the
                // chooser's.
                WindowEvent::AlternateCloseRequested { .. }
                | WindowEvent::AppBarDefault
                | WindowEvent::AppBarMenu { .. }
                | WindowEvent::Key { .. }
                | WindowEvent::Focus { .. }
                | WindowEvent::Minimized { .. }
                | WindowEvent::RedrawRequested { .. }
                | WindowEvent::FilePicked { .. }
                | WindowEvent::PickCancelled { .. }
                | WindowEvent::DesktopChanged { .. } => ChooserAction::None,
            };
            let outcome = match asked {
                ChooserAction::None => {
                    if desktop_changed {
                        repaint(
                            &mut chooser,
                            theme,
                            &desktop,
                            &mut client,
                            window,
                            &mut frames,
                            &mode,
                        )
                    } else {
                        Ok(())
                    }
                }
                ChooserAction::Changed => repaint(
                    &mut chooser,
                    theme,
                    &desktop,
                    &mut client,
                    window,
                    &mut frames,
                    &mode,
                ),
                ChooserAction::Apply => {
                    chooser.set_apply_outcome(apply(&chooser.settings_document()));
                    repaint(
                        &mut chooser,
                        theme,
                        &desktop,
                        &mut client,
                        window,
                        &mut frames,
                        &mode,
                    )
                }
                // Close the window and end cleanly, freeing the region this
                // app owns rather than leaving it pinned for the runtime to
                // reclaim.
                ChooserAction::Close => {
                    let _ = client.close(window);
                    let _ = tairix_rt::shm_unmap(frames.base as u64, frames.len);
                    return 0;
                }
            };
            if outcome.is_err() {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
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
