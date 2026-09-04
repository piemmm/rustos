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
    use tairix_abi::{Duration64, Errno, WaitSetOp, WaitSourceKind};
    use tairix_appdata::RtHost;
    use tairix_display::{winframe, SERIAL};
    use tairix_font::BitmapFont;
    use tairix_geometry::Region;
    use tairix_input::InputEvent;
    use tairix_log::{Event, Field, Level};
    use tairix_raster::Surface;
    use tairix_rt::io::{Stderr, Write};
    use tairix_sandbox::imagerender::{render_wallpaper_for_screen, ImageRenderService};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_theme::{TextRole, Theme, ThemeRegistry};
    use tairix_wallpaper::{
        catalog_categories, catalog_entries, category_path, PinboardSettings, WallpaperFit,
        WallpaperPath, MAX_WALLPAPER_BYTES, PINBOARD_PUBLISHER, WALLPAPER_STORE,
    };
    use tairix_wallpaper_chooser::{
        candidates_from_catalog, events::RENDER_TIMED, ApplyOutcome, Chooser, ChooserAction, Style,
        MIN_WIN_HEIGHT, MIN_WIN_WIDTH, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_window::{
        key_input_event, pointer_input_events, pointer_point, present_damage, Desktop, EventDrain,
        EventError, EventMailbox, EventSource, Parked, Repaint, WindowClient, WindowEvents,
        WindowFrames, WindowSizing, WindowTransport,
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

    /// The wait-set token of the applier's wake pipe: readable exactly when
    /// the desktop session has answered an apply, so the footer is written
    /// through the park the loop is already in rather than by waiting for it.
    const APPLY_TOKEN: u64 = 3;

    /// The window title the desktop lists this app under.
    const TITLE: &str = "Wallpaper";

    /// State a reason on `stderr` (fail loud: an exit code alone is not a
    /// diagnosis, and a refused optional step still says so).
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "wallpaper: {reason}");
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
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty, and accept only
    /// events whose kernel-attested sender is the desktop session named by
    /// the create reply — anything else is dropped (fail closed), so no
    /// other process can feed the app forged input.
    struct RtEventSource<'a> {
        /// The app's own event mailbox, which authenticates every frame it
        /// hands over.
        mailbox: EventMailbox,
        /// The wait-set handle the app parks on.
        set: u64,
        /// The applier's wake, drained on an [`APPLY_TOKEN`] wake. Its
        /// readiness is a level peek, so leaving it undrained would report
        /// ready for ever and turn the park into a spin.
        applier: &'a Applier,
    }

    impl EventDrain for RtEventSource<'_> {
        fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
            self.mailbox.try_next(event)
        }
    }

    impl EventSource for RtEventSource<'_> {
        fn park(&mut self) -> Result<Parked, Errno> {
            let mut token = 0u64;
            if tairix_rt::waitset_wait(self.set, u64::MAX, &mut token) != 0 {
                return Err(Errno::NotFound);
            }
            // The session answered the apply. Draining is the whole of
            // noticing it, and the answer is the loop's to show, so the wait
            // ends here rather than parking again on a source still ready.
            if token == APPLY_TOKEN {
                self.applier.wake().drain();
                return Ok(Parked::Interrupted);
            }
            if token == PRESSURE_TOKEN && tairix_procinfo::pressure::refresh() {
                tairix_font::trim_glyph_cache();
            }
            Ok(Parked::Served)
        }
    }

    /// Open `path` for reading under the launching user's own identity,
    /// surfacing the kernel's own refusal so the caller can tell an absent
    /// file from one it may not read.
    fn open_read(path: &str) -> Result<u32, Errno> {
        let raw = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        u32::try_from(raw).map_err(|_| Errno::from_syscall(raw))
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
    /// Read from the desktop session's **published** app-data scope
    /// (`plans/APPDATA.md` §3.11), which is the sanctioned channel one
    /// application reaches another's values through: this program names the
    /// publisher and nothing else, so the request shape it sends cannot ask
    /// for the session's private settings — that is not a frame that exists.
    /// It replaces opening the session's own file directly, which every
    /// application of that user could also *rewrite*.
    ///
    /// A desktop that has published **nothing** means the documented defaults
    /// and is not an error — a fresh account has never applied a setting, and
    /// so does an account whose session has never run. Anything else that
    /// stops the document being used (an unreachable store, a value this
    /// build's registry does not accept) also yields the defaults for the
    /// affected setting, but says so on `stderr` rather than opening on
    /// settings the user cannot see the reason for.
    fn settings_in_effect() -> PinboardSettings {
        let document = match tairix_appdata::read_published(&mut RtHost, PINBOARD_PUBLISHER) {
            Ok(document) => document,
            Err(err) => {
                report(&alloc::format!(
                    "the desktop's settings could not be read ({err:?}); showing the defaults"
                ));
                return PinboardSettings::default();
            }
        };
        let (settings, refused) = PinboardSettings::load(&document);
        for key in refused {
            report(&alloc::format!(
                "the desktop publishes a `{key}` this build does not accept; showing its default"
            ));
        }
        settings
    }

    /// The wallpapers the shipped store offers, discovered by listing each of
    /// its category directories under the launching user's own identity.
    ///
    /// A store that cannot be listed is not fatal: the chooser still
    /// offers "no wallpaper" and every backdrop colour, so the refusal is
    /// stated and an empty candidate list returned. One unreadable category
    /// costs only its own wallpapers, so the rest of the store is still
    /// offered.
    fn store_candidates() -> Vec<tairix_wallpaper_chooser::Candidate> {
        let Some(entries) = list_directory(WALLPAPER_STORE) else {
            return Vec::new();
        };
        // The store's own children are the categories; a stray file there is
        // planted by nothing and offered by nothing.
        let categories = catalog_categories(
            entries
                .iter()
                .filter(|entry| entry.is_directory_backed())
                .map(tairix_browse::Entry::name),
        );
        let mut candidates = Vec::new();
        for category in &categories {
            let path = category_path(category);
            let Some(listing) = list_directory(&path) else {
                continue;
            };
            // The shared catalog builder decides what counts as a wallpaper
            // (name shape, extension, ordering, and the listing bound); this
            // only drops the directories, which are never candidates.
            let catalog = catalog_entries(
                listing
                    .iter()
                    .filter(|entry| !entry.is_directory_backed())
                    .map(|entry| {
                        (
                            entry.name(),
                            usize::try_from(entry.size()).unwrap_or(usize::MAX),
                        )
                    }),
            );
            candidates.extend(candidates_from_catalog(category, &catalog));
        }
        candidates
    }

    /// One directory's entries, or `None` with the reason stated on `stderr`.
    fn list_directory(path: &str) -> Option<Vec<tairix_browse::Entry>> {
        let stream = match tairix_rt::read_dir_all(path.as_bytes()) {
            Ok(stream) => stream,
            Err(err) => {
                report(&alloc::format!(
                    "{path}: {:?}; its wallpapers are not offered",
                    Errno::from_syscall(err)
                ));
                return None;
            }
        };
        let Ok(entries) = tairix_browse::vfs::entries_from_dir_stream(
            path,
            &stream,
            &mut tairix_browse::RtLinkReader,
        ) else {
            report(&alloc::format!(
                "{path}: listing not readable; its wallpapers are not offered"
            ));
            return None;
        };
        Some(entries)
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
        let opened = tairix_rt::clock_get();
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
        let read_done = tairix_rt::clock_get();
        let placed = render_wallpaper_for_screen(sandbox, screen, width, height, fit, &bytes);
        report_render_timing(
            spelled,
            bytes.len(),
            (width, height),
            elapsed(opened, read_done),
            elapsed(read_done, tairix_rt::clock_get()),
        );
        match placed {
            Ok(rgba) => Surface::from_rgba8(width, height, &rgba),
            Err(failure) => {
                report(&alloc::format!("{spelled}: {failure}; not shown"));
                None
            }
        }
    }

    /// The span between two `clock_get` readings, saturating: a
    /// non-monotonic pair is a zero span rather than a huge one.
    fn elapsed(from: u64, to: u64) -> Duration64 {
        Duration64::from_nanos(to.saturating_sub(from))
    }

    /// Report one placement's read and render halves.
    ///
    /// `Info`, because a record no one can read diagnoses nothing: the level
    /// is a per-process default with no runtime knob, so anything below it
    /// would be dropped. One record per placement — never per frame — and a
    /// placement is a user-driven operation costing tens of milliseconds at
    /// best, so the record is far cheaper than the work it measures.
    fn report_render_timing(
        path: &str,
        source_bytes: usize,
        dest: (u32, u32),
        read: Duration64,
        render: Duration64,
    ) {
        tairix_log::log(
            &tairix_rt::LogSink,
            &Event {
                level: Level::Info,
                id: RENDER_TIMED,
                message: "wallpaper: placed",
                fields: &[
                    Field {
                        key: "path",
                        value: tairix_abi::FieldValue::Str(path),
                    },
                    Field {
                        key: "read",
                        value: tairix_abi::FieldValue::Duration(read),
                    },
                    Field {
                        key: "render",
                        value: tairix_abi::FieldValue::Duration(render),
                    },
                    Field {
                        key: "source_bytes",
                        value: tairix_abi::FieldValue::UnsignedInt(source_bytes as u64),
                    },
                    Field {
                        key: "dest_w",
                        value: tairix_abi::FieldValue::UnsignedInt(u64::from(dest.0)),
                    },
                    Field {
                        key: "dest_h",
                        value: tairix_abi::FieldValue::UnsignedInt(u64::from(dest.1)),
                    },
                ],
            },
        );
    }

    /// Ask the desktop session to adopt `document` and report what it
    /// said.
    ///
    /// Nothing is reported as applied that the session did not accept: a
    /// document this program cannot even encode, an unanswered rendezvous,
    /// and a typed refusal are three distinct outcomes.
    fn send_apply(document: &PinboardDocument) -> ApplyOutcome {
        let request = PinboardRequest::Apply {
            document: *document,
        }
        .to_le_bytes();
        let mut reply = [0u8; STATUS_REPLY_LEN];
        let Ok(len) = tairix_rt::ipc_call(PINBOARD_ENDPOINT, &request, &mut reply) else {
            return ApplyOutcome::NoDesktop;
        };
        match decode_status_reply(&reply[..len.min(reply.len())]) {
            Ok(()) => ApplyOutcome::Applied,
            Err(err) => ApplyOutcome::Refused(alloc::format!("{err:?}")),
        }
    }

    /// The chooser's applier: the session round trip an *Apply* costs, carried
    /// out on a worker thread.
    ///
    /// The session answers only once its own publisher has written the store,
    /// so making the click wait for it would freeze the window for a disk
    /// commit. The loop encodes the document (in memory, and refusable on the
    /// spot), submits, and shows the answer on the wake it nudges.
    type Applier = tairix_rt::work::Worker<PinboardDocument, ApplyOutcome>;

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

    /// The live window channel this app owns: the transport to the desktop
    /// session, the session-assigned window id, the shared frame region, the
    /// negotiated display mode, and the surface every frame is drawn into.
    ///
    /// Grouped because the present and resize paths always need every one of
    /// these together. The surface is held for the life of the window rather
    /// than built per frame: allocating and zeroing a window-sized buffer on
    /// every pointer sample is a whole-window pass of its own, and holding it
    /// is what makes a clipped repaint sound — every pixel outside the clip is
    /// the one already on screen.
    struct WindowSurface {
        client: WindowClient<RtWindowTransport>,
        window: u64,
        frames: WindowFrames,
        mode: DisplayMode,
        canvas: Surface,
    }

    impl WindowSurface {
        /// Draw `damage` of the chooser, convert that rectangle into the
        /// shared frame region and present it.
        ///
        /// A region the session released while the window was hidden is
        /// re-attached first and presented whole, because it holds none of the
        /// pixels a partial present would leave standing.
        fn present(
            &mut self,
            chooser: &mut Chooser,
            theme: &Theme,
            desktop: &Desktop,
            damage: DamageRect,
        ) -> Result<(), Errno> {
            let damage = if self.frames.is_released() {
                DamageRect::full(&self.mode)
            } else {
                damage
            };
            let style = style_for(theme, desktop);
            self.canvas.with_clip(
                damage.x,
                damage.y,
                damage.width_px,
                damage.height_px,
                |clipped| chooser.render_into(clipped, style),
            );
            let pixels = self
                .client
                .frame_pixels(&mut self.frames, self.window, FRAME_COUNT, &self.mode)
                .ok_or(Errno::NotAttached)?;
            winframe::encode(&self.canvas, pixels, &self.mode, damage, &SERIAL)?;
            self.client.present(self.window, 0, damage)
        }

        /// Re-map the frame region onto `new_mode` and re-lay the chooser out
        /// at the new client size. The caller repaints the whole window, since
        /// nothing of the old picture survives a re-layout.
        ///
        /// The ordering is fail-closed: a fresh region and a fresh surface are
        /// allocated and the region granted first, then adopted only if the
        /// session accepts the resize. On success the *old* region is unmapped
        /// (never before, so a refused resize leaves the current surface
        /// intact); on refusal the freshly-allocated region is unmapped so
        /// nothing leaks. Anything that cannot be allocated at all keeps the
        /// current size rather than ending the app.
        fn resize(&mut self, new_mode: DisplayMode, chooser: &mut Chooser) {
            let Some(spare) = WindowFrames::create(region_bytes(&new_mode)) else {
                return;
            };
            let Some(fresh) = Surface::new(new_mode.width_px, new_mode.height_px) else {
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
                return;
            }
            // Adopting drops the old region, which unmaps it — and a refusal
            // above drops the spare instead, so the ordering the surface
            // depends on is the ownership rather than a sequence a later edit
            // could reorder.
            self.frames = spare;
            self.mode = new_mode;
            self.canvas = fresh;
            chooser.relayout(self.mode.width_px, self.mode.height_px);
        }
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
        damage: &mut Region,
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
                Some(surface) => chooser.set_preview(request, surface, style, damage),
                None => chooser.mark_preview_refused(request, style, damage),
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
            Some(surface) => chooser.set_thumbnail(request.index, surface, style, damage),
            None => chooser.mark_thumbnail_refused(request.index, style, damage),
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

        // --- The applier. The session answers an *Apply* only once it has
        // written the store, so the click hands the round trip here and the
        // window keeps drawing. A kernel that grants no thread, or refuses the
        // pipe, leaves the call on this task — where it used to be, and stated
        // once.
        let applier = alloc::sync::Arc::new(Applier::new(
            send_apply,
            tairix_rt::sync::WorkerWake::create(),
        ));
        if let Err(reason) = Applier::start(&applier) {
            report(&alloc::format!(
                "no apply worker ({reason:?}); the desktop is asked on the event loop"
            ));
        }
        let _applier_guard = tairix_rt::work::WorkerGuard::new(&applier);
        // A refused add is fatal rather than tolerated: an apply whose answer
        // nobody collects would leave the footer claiming a state the session
        // may never have adopted.
        if let Some(read) = applier.wake().read_end() {
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Stream,
                u64::from(read),
                APPLY_TOKEN,
            ) != 0
            {
                return fail(EXIT_NO_EVENTS, "apply wake refused");
            }
        }

        // --- Open the window (resizable: the grid re-lays out to each new
        // client size, down to the floor the window manager is told to hold)
        // and paint the first frame.
        let sizing = WindowSizing::Resizable {
            min_width_px: MIN_WIN_WIDTH,
            min_height_px: MIN_WIN_HEIGHT,
        };
        let Ok((window, server)) =
            client.create(grant, event_endpoint, FRAME_COUNT, &mode, TITLE, sizing)
        else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        chooser.relayout(mode.width_px, mode.height_px);
        let Some(canvas) = Surface::new(mode.width_px, mode.height_px) else {
            return fail(EXIT_NO_WINDOW, "no memory for the window surface");
        };
        let mut surface = WindowSurface {
            client,
            window,
            frames,
            mode,
            canvas,
        };
        let first = DamageRect::full(&surface.mode);
        if surface
            .present(&mut chooser, theme, &desktop, first)
            .is_err()
        {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The preview worker: one sandboxed child, started on the first
        // render and replaced by the seam if it ever fails.
        let mut sandbox = ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink);

        // --- The event loop: serve input, render one outstanding picture,
        // repaint, and park only when there is nothing of either left. A dead
        // channel ends the app fail-loud; a clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            mailbox: EventMailbox::new(event_endpoint, server),
            set,
            applier: &applier,
        });
        loop {
            // Queued input first, then one outstanding render, and a park only
            // when neither has anything left. Serving input ahead of the
            // render is what keeps the window live while a store of 4K masters
            // fills in: a click or a key waits at most one picture, never the
            // whole gallery's worth. Nothing here spins — an idle chooser with
            // every visible tile rendered parks on its wait-set.
            let mut damage = tairix_controls::damage::sink();
            // What the session said about the last apply, shown the moment it
            // lands rather than at whatever later input happens to arrive.
            if let Some(outcome) = applier.collect() {
                chooser.set_apply_outcome(outcome, style_for(theme, &desktop), &mut damage);
                if let Some(damage) = present_damage(&surface.mode, Repaint::Reported, &damage) {
                    if surface
                        .present(&mut chooser, theme, &desktop, damage)
                        .is_err()
                    {
                        return fail(EXIT_CHANNEL_LOST, "present refused");
                    }
                }
                continue;
            }
            let delivered = match events.try_wait(&mut surface.client) {
                Ok(Some(event)) => Ok(Some(event)),
                Ok(None) => {
                    if resolve_one_render(&mut chooser, &mut sandbox, theme, &desktop, &mut damage)
                    {
                        // A delivered preview or thumbnail redraws exactly the
                        // box it fills, so the gallery fills in one tile at a
                        // time rather than repainting the window once per
                        // picture.
                        let Some(damage) =
                            present_damage(&surface.mode, Repaint::Reported, &damage)
                        else {
                            continue;
                        };
                        if surface
                            .present(&mut chooser, theme, &desktop, damage)
                            .is_err()
                        {
                            return fail(EXIT_CHANNEL_LOST, "present refused");
                        }
                        continue;
                    }
                    events.wait(&mut surface.client)
                }
                Err(err) => Err(err),
            };
            let event = match delivered {
                Ok(Some(event)) => event,
                // A park the applier interrupted has no event — the collect at
                // the head of the next turn is what shows the answer — and a
                // malformed frame from the authenticated session is refused
                // rather than guessed at. Either way the loop goes round.
                Ok(None) | Err(EventError::Undecodable(_)) => continue,
                Err(EventError::Mailbox(_)) => {
                    return fail(EXIT_CHANNEL_LOST, "event channel lost")
                }
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
            // what to *do* about it is made once, below. `damage` is that
            // round's one sink: every control the event reaches, and the
            // chooser for what it commits itself, reports into it.
            let mut resized = false;
            let asked = match event {
                // The pointer is the chooser's primary input: one delivered
                // wire event is the position it happened at and, for a
                // press or a release, the button transition after it — the
                // shared translation every windowed app uses.
                WindowEvent::Pointer { x, y, action, .. } => {
                    let at = pointer_point(x, y);
                    let mut asked = ChooserAction::None;
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
                    &mut damage,
                ),
                // The keyboard is the secondary path, and reaches
                // everything the pointer does.
                WindowEvent::Key {
                    key: pressed @ KeyInput::Pressed { .. },
                    ..
                } => match key_input_event(pressed) {
                    InputEvent::KeyPressed { key, modifiers } => {
                        chooser.on_key(key, modifiers, style_for(theme, &desktop), &mut damage)
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
                    surface.resize(mode_for(width_px, height_px), &mut chooser);
                    resized = true;
                    ChooserAction::Changed
                }
                // The desktop asked: the window's own Close control, or the
                // chooser having adopted a choice.
                WindowEvent::CloseRequested { .. } => ChooserAction::Close,
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
                // The chooser is part of the desktop rather than an
                // application the user manages: its signed manifest presents
                // no icon-bar slot and it declares none, so neither icon-bar
                // event can reach it. A chain outcome answers an open the
                // chooser never asks for.
                WindowEvent::AlternateCloseRequested { .. }
                | WindowEvent::AppBarDefault
                | WindowEvent::AppBarMenu { .. }
                | WindowEvent::MenuClosed { .. }
                | WindowEvent::Key { .. }
                | WindowEvent::Focus { .. }
                | WindowEvent::Minimized { .. }
                | WindowEvent::RedrawRequested { .. }
                | WindowEvent::FilePicked { .. }
                | WindowEvent::PickCancelled { .. }
                | WindowEvent::DesktopChanged { .. } => ChooserAction::None,
                // Nobody can see the window, so the session gave its copy of
                // the pixels back and unmapped the region. Let go of this side
                // too — the pages go only when both do — and paint nothing
                // until the redraw request that follows the window being shown
                // again re-attaches a fresh region.
                WindowEvent::ContentReleased { .. } => {
                    surface.frames.release();
                    continue;
                }
            };
            let repaint_kind = match asked {
                ChooserAction::None => Repaint::Nothing,
                ChooserAction::Changed => Repaint::Reported,
                ChooserAction::Apply => {
                    // Encoding is in-memory and refusable on the spot; only
                    // the session round trip goes to the worker.
                    let outcome = match PinboardDocument::new(&chooser.settings_document()) {
                        Ok(document) => {
                            if applier.submit(document) {
                                // No worker: the call was made on this thread
                                // and its answer is already on the desk.
                                applier.collect().unwrap_or(ApplyOutcome::Applying)
                            } else {
                                ApplyOutcome::Applying
                            }
                        }
                        Err(_) => {
                            ApplyOutcome::Refused(String::from("settings document out of range"))
                        }
                    };
                    chooser.set_apply_outcome(outcome, style_for(theme, &desktop), &mut damage);
                    Repaint::Reported
                }
                // Close the window and end cleanly; the region this app owns
                // is unmapped by its own drop rather than left pinned for the
                // runtime to reclaim.
                ChooserAction::Close => {
                    let _ = surface.client.close(surface.window);
                    return 0;
                }
            };
            // A re-theme redraws every pixel and a resize re-lays everything
            // out onto a fresh surface, so neither can be described by a
            // report.
            let repaint_kind = if desktop_changed || resized {
                Repaint::Whole
            } else {
                repaint_kind
            };
            let Some(damage) = present_damage(&surface.mode, repaint_kind, &damage) else {
                continue;
            };
            if surface
                .present(&mut chooser, theme, &desktop, damage)
                .is_err()
            {
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
