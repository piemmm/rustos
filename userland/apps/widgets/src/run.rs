//! The `widgets.app` bundle's `Run` entry point: the windowed Reactive Alloy
//! widget gallery (`plans/GUI-CONTROLS-DESIGN.md`).
//!
//! Everything with behaviour worth testing lives in the host-tested gallery
//! model (`tairix_widgets`); this binary only composes it over the live window
//! channel, exactly as `userland/apps/files` composes `lib/browse`:
//!
//! * one `shm_create`d frame region granted to the reserved window endpoint
//!   (the zero-copy surface the session maps once at create);
//! * one `port_bind`-bound event mailbox the app **parks** on through its
//!   wait-set — never a poll loop. Every received event carries its sender's
//!   kernel-attested origin, and the app accepts only events from the session
//!   identity the create reply named, so no other process can feed it forged
//!   input (fail closed);
//! * the `WindowClient` calls (create / present / close) and the
//!   `WindowEvents` typed wait over the parked source.
//!
//! Delivered pointer and key events are mapped onto the shared desktop input
//! vocabulary and routed into the gallery, which draws the tab strip and the
//! selected family's panel of demo widgets and reflects each control's own
//! action back into it. A `CloseRequested` from the desktop closes the window
//! and ends the program cleanly; every bring-up refusal exits fail-loud with a
//! reserved code and a stated reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::input::KeyInput;
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{Errno, Origin, ProcId, ORIGIN_WIRE_LEN};
    use tairix_font::BitmapFont;
    use tairix_geometry::{Point, Rect, Region, Scale};
    use tairix_input::InputEvent;
    use tairix_raster::Surface;
    use tairix_rt::io::{Stderr, Write};
    use tairix_theme::{TextRole, Theme, ThemeRegistry};
    use tairix_widgets::Gallery;
    use tairix_window::{
        key_input_event, pointer_input_events, present_damage, Desktop, EventSource, Repaint,
        WindowClient, WindowEvents, WindowTransport,
    };

    /// The gallery window's logical width in physical pixels.
    const WIN_WIDTH: u32 = 820;
    /// The gallery window's logical height in physical pixels.
    const WIN_HEIGHT: u32 = 620;

    /// Frames in the shared region. The window protocol serialises a present
    /// (the app is parked in the call while the session reads), so a single
    /// frame is race-free.
    const FRAME_COUNT: u32 = 1;

    /// The wait-set token of the event-mailbox member.
    const EVENT_TOKEN: u64 = 1;

    /// The wait-set token of the memory-pressure member: the kernel wakes the
    /// park when the machine's pressure band changes, so the glyph cache is
    /// trimmed as memory tightens instead of being held until something else
    /// is starved.
    const PRESSURE_TOKEN: u64 = 2;

    /// Exit code when the shared frame region could not be created or granted.
    const EXIT_NO_FRAMES: i32 = 81;
    /// Exit code when the event mailbox could not be bound or observed.
    const EXIT_NO_EVENTS: i32 = 82;
    /// Exit code when the desktop session refused the window create.
    const EXIT_NO_WINDOW: i32 = 83;
    /// Exit code when a present was refused or the event channel died.
    const EXIT_CHANNEL_LOST: i32 = 84;

    /// Recover the [`Errno`] a syscall encoded as a negative register.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// State the abnormal-exit reason on `stderr` (fail loud) and hand back
    /// `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "widgets: {reason}");
        code
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to the
    /// reserved window endpoint per request.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(errno_from)
        }
    }

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty, and accept only events
    /// whose kernel-attested sender is the desktop session named by the create
    /// reply — anything else is dropped (fail closed).
    struct RtEventSource {
        endpoint: u64,
        set: u64,
        server: ProcId,
    }

    /// Whether a received mailbox frame is a genuine event from the desktop
    /// session: exactly one [`WindowEvent`] wide and from the attested origin.
    fn accept_frame(len: usize, sender: &[u8; ORIGIN_WIRE_LEN], server: ProcId) -> bool {
        len == WindowEvent::WIRE_LEN
            && Origin::from_bytes(sender).is_ok_and(|origin| origin.proc_id() == server)
    }

    impl EventSource for RtEventSource {
        fn next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno> {
            loop {
                let mut sender = [0u8; ORIGIN_WIRE_LEN];
                match tairix_rt::ipc_recv(self.endpoint, event, &mut sender) {
                    Ok(len) => {
                        if accept_frame(len, &sender, self.server) {
                            return Ok(());
                        }
                    }
                    Err(err) if errno_from(err) == Errno::WouldBlock => {
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

    /// Convert `damage`'s pixels out of `surface` into the shared `frame`.
    ///
    /// Only that rectangle is converted, because it is also the only one the
    /// present declares: the session copies exactly it and leaves the rest of
    /// the window as it already was.
    fn convert_damage(
        surface: &Surface,
        frame: &mut [u8],
        mode: &DisplayMode,
        damage: DamageRect,
    ) -> Result<(), Errno> {
        let stride = mode.stride_bytes as usize;
        let columns = surface.width() as usize;
        let x = damage.x as usize;
        let span = damage.width_px as usize;
        let short = || Errno::LengthOutOfRange;
        let bytes = span.checked_mul(4).ok_or_else(short)?;
        for y in damage.y..damage.y.saturating_add(damage.height_px) {
            let from = (y as usize)
                .checked_mul(columns)
                .and_then(|row| row.checked_add(x))
                .and_then(|lo| lo.checked_add(span).map(|hi| lo..hi))
                .ok_or_else(short)?;
            let at = (y as usize)
                .checked_mul(stride)
                .and_then(|row| row.checked_add(x * 4))
                .and_then(|lo| lo.checked_add(bytes).map(|hi| lo..hi))
                .ok_or_else(short)?;
            let (Some(row), Some(slot)) = (surface.pixels().get(from), frame.get_mut(at)) else {
                return Err(short());
            };
            // The slot is exactly `span` whole pixels by construction, so the
            // ragged tail this splits off is always empty.
            let (out, _tail) = slot.as_chunks_mut::<4>();
            for (pixel, target) in row.iter().zip(out) {
                let color = pixel.unpremultiply();
                *target = [color.r, color.g, color.b, color.a];
            }
        }
        Ok(())
    }

    /// The live window channel this app owns: the transport to the desktop
    /// session, its session-assigned window id, and the surface it draws into.
    /// Grouped so the event loop and the first present take one receiver
    /// instead of scattering the same parameters through every call.
    ///
    /// The surface is held rather than built per frame: a window-sized buffer
    /// allocated and zeroed on every pointer sample is a whole-window pass of
    /// its own, and holding it is what makes a clipped repaint sound — every
    /// pixel outside the clip is the one already on screen.
    struct GalleryWindow {
        /// The synchronous channel to the desktop session.
        client: WindowClient<RtWindowTransport>,
        /// This app's window id, assigned by the session at create.
        window: u64,
        /// The window-sized surface every frame is drawn into.
        surface: Surface,
    }

    impl GalleryWindow {
        /// Draw the gallery, convert `damage` into `frames` (the shared window
        /// surface, shaped as `mode`) and present that rectangle.
        ///
        /// The draw is clipped to `damage` too: everything outside it is already
        /// in the surface from the last frame, and neither the conversion nor
        /// the present would carry it.
        fn present(
            &mut self,
            gallery: &Gallery,
            theme: &Theme,
            scale: Scale,
            frames: &mut [u8],
            mode: &DisplayMode,
            damage: DamageRect,
        ) -> Result<(), Errno> {
            let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);
            let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
            self.surface.with_clip(
                damage.x,
                damage.y,
                damage.width_px,
                damage.height_px,
                |surface| gallery.render(surface, viewport, scale, theme, font),
            );
            convert_damage(&self.surface, frames, mode, damage)?;
            self.client.present(self.window, 0, damage)
        }
    }

    /// Apply one delivered event to the gallery, reporting whether the view
    /// changed (and must re-present) and whether the app should end.
    ///
    /// Every control the event reaches, and the gallery for what it changes
    /// itself, reports its own repainted bounds into `damage` — the round's one
    /// sink, which is what the present is then clipped to.
    fn apply_event(
        gallery: &mut Gallery,
        theme: &Theme,
        scale: Scale,
        mode: &DisplayMode,
        event: &WindowEvent,
        damage: &mut Region,
    ) -> (bool, bool) {
        let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);
        match event {
            WindowEvent::CloseRequested { .. } => (false, true),
            WindowEvent::Key {
                key: pressed @ KeyInput::Pressed { .. },
                ..
            } => match key_input_event(*pressed) {
                InputEvent::KeyPressed { key, modifiers } => (
                    gallery.on_key(key, modifiers, viewport, scale, theme, damage),
                    false,
                ),
                _ => (false, false),
            },
            WindowEvent::Pointer { x, y, action, .. } => (
                apply_pointer(
                    gallery,
                    client_point(*x, *y),
                    *action,
                    viewport,
                    scale,
                    theme,
                    damage,
                ),
                false,
            ),
            WindowEvent::Scrolled { dx, dy, .. } => {
                let scroll = InputEvent::PointerScrolled { dx: *dx, dy: *dy };
                (
                    gallery.on_pointer(&scroll, viewport, scale, theme, damage),
                    false,
                )
            }
            // A redraw request needs nothing here: the client library
            // re-presents the last frame, and the gallery it drew has not
            // changed. The rest are events the gallery does not act on: a
            // secondary press on Close asks to leave what the window is
            // showing, and the gallery has nothing to leave but itself.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::Resized { .. }
            | WindowEvent::RedrawRequested { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. }
            // The desktop change is adopted by the caller before this match,
            // which is also where the repaint it needs is decided.
            | WindowEvent::DesktopChanged { .. } => (false, false),
        }
    }

    /// Route one wire pointer event: a move to `at` to sync the pointer, then
    /// the press/release the action names. Returns whether the view changed.
    fn apply_pointer(
        gallery: &mut Gallery,
        at: Point,
        action: PointerAction,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let mut acted = false;
        for input in pointer_input_events(action, at) {
            acted |= gallery.on_pointer(&input, viewport, scale, theme, damage);
        }
        acted
    }

    /// The client-local point a wire pointer position names.
    fn client_point(x: u32, y: u32) -> Point {
        Point::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        )
    }

    /// Bind the app's own event mailbox and add it to a fresh wait-set the
    /// event loop parks on, returning `(endpoint, set)`. On any refusal it
    /// states the reason on `stderr` and returns the reserved fail-closed
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
            tairix_abi::WaitSetOp::Add,
            tairix_abi::WaitSourceKind::Port,
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
                let _ = writeln!(Stderr, "widgets: desktop query refused: {err}");
                return Err(EXIT_NO_WINDOW);
            }
        };
        let desktop = match Desktop::new(info) {
            Ok(desktop) => desktop,
            Err(err) => {
                let _ = writeln!(Stderr, "widgets: cannot draw this desktop: {err}");
                return Err(EXIT_NO_WINDOW);
            }
        };
        let mut themes = ThemeRegistry::with_builtins();
        themes.set_appearance(desktop.appearance());
        Ok((desktop, themes))
    }

    /// Create and grant a `mode`-shaped frame region, returning `(base,
    /// total, grant)`: the mapped base address, the region's byte length,
    /// and the endpoint-directed grant handle. Fails closed with the
    /// reserved [`EXIT_NO_FRAMES`] code for `main` on any refusal.
    fn create_frame_region(mode: &DisplayMode) -> Result<(usize, usize, u64), i32> {
        let frame_len = (mode.stride_bytes as usize) * (mode.height_px as usize);
        let total = frame_len * FRAME_COUNT as usize;
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return Err(fail(EXIT_NO_FRAMES, "shared frame region refused"));
        }
        let grant = tairix_rt::shm_grant(region_id, WINDOW_ENDPOINT);
        if grant < 1 {
            return Err(fail(EXIT_NO_FRAMES, "frame region grant refused"));
        }
        let Ok(base) = usize::try_from(base) else {
            return Err(fail(
                EXIT_NO_FRAMES,
                "frame region base outside the address width",
            ));
        };
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        Ok((base, total, grant as u64))
    }

    /// Open the gallery's window and present its first frame.
    ///
    /// Returns the window channel, the desktop session's [`ProcId`], and
    /// the initialised [`Gallery`], or the reserved exit code for `main`
    /// when the session refuses the create or the first present.
    fn open_gallery_window(
        mut client: WindowClient<RtWindowTransport>,
        grant: u64,
        event_endpoint: u64,
        mode: &DisplayMode,
        theme: &Theme,
        scale: Scale,
        frames: &mut [u8],
    ) -> Result<(GalleryWindow, ProcId, Gallery), i32> {
        let Ok((window, server)) =
            client.create(grant, event_endpoint, FRAME_COUNT, mode, "widgets", false)
        else {
            return Err(fail(EXIT_NO_WINDOW, "desktop session refused the window"));
        };
        let Some(pixels) = Surface::new(mode.width_px, mode.height_px) else {
            return Err(fail(EXIT_NO_WINDOW, "no memory for the window surface"));
        };
        let gallery = Gallery::new();
        let mut surface = GalleryWindow {
            client,
            window,
            surface: pixels,
        };
        if surface
            .present(&gallery, theme, scale, frames, mode, DamageRect::full(mode))
            .is_err()
        {
            return Err(fail(EXIT_CHANNEL_LOST, "first present refused"));
        }
        Ok((surface, server, gallery))
    }

    /// The event loop: park, apply, repaint. A dead channel ends the app
    /// fail-loud; a clean close ends it at zero.
    fn run_event_loop(
        surface: &mut GalleryWindow,
        desktop: &mut Desktop,
        themes: &mut ThemeRegistry,
        gallery: &mut Gallery,
        frames: &mut [u8],
        mode: &DisplayMode,
        mut events: WindowEvents<RtEventSource>,
    ) -> i32 {
        loop {
            let event = match events.wait(&mut surface.client) {
                Ok(event) => event,
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };

            // Adopt a desktop change before the app-specific event logic, so
            // the scale and theme everything below derives from are already
            // current. Only a real change costs a re-theme and a repaint; a
            // refused one states its reason and stands on the last good
            // desktop.
            let redraw = match desktop.apply(&event) {
                Ok(true) => {
                    themes.set_appearance(desktop.appearance());
                    true
                }
                Ok(false) => false,
                Err(err) => {
                    let _ = writeln!(Stderr, "widgets: desktop change refused: {err}");
                    false
                }
            };

            // One sink per round: every control the event reaches, and the
            // gallery for what it changes itself, reports into this one.
            let mut damage = tairix_controls::damage::sink();
            let (changed, close) = apply_event(
                gallery,
                themes.active(),
                desktop.scale(),
                mode,
                &event,
                &mut damage,
            );
            if close {
                let _ = surface.client.close(surface.window);
                return 0;
            }
            // An adopted desktop change re-themes and re-densifies every pixel,
            // so no report could describe it.
            let repaint = match (redraw, changed) {
                (true, _) => Repaint::Whole,
                (false, true) => Repaint::Reported,
                (false, false) => Repaint::Nothing,
            };
            let Some(damage) = present_damage(mode, repaint, &damage) else {
                continue;
            };
            if surface
                .present(
                    gallery,
                    themes.active(),
                    desktop.scale(),
                    frames,
                    mode,
                    damage,
                )
                .is_err()
            {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        let mut client = WindowClient::new(RtWindowTransport);

        // --- The desktop this window will be shown on, established before
        // anything is sized or painted so the first frame is right rather
        // than a guess corrected once the user has seen it.
        let (mut desktop, mut themes) = match bring_up_desktop(&mut client) {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        let (initial_w, initial_h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mode = DisplayMode {
            width_px: initial_w,
            height_px: initial_h,
            stride_bytes: initial_w * 4,
            format: DisplayFormat::Rgba8888,
        };
        let (base, total, grant) = match create_frame_region(&mode) {
            Ok(triple) => triple,
            Err(code) => return code,
        };
        // SAFETY: the kernel mapped exactly `total` zeroed bytes read/write
        // into this process at `base` (`shm_create` maps the length it was
        // asked for) and the mapping stays live for the life of the process —
        // nothing below unmaps or aliases it. The session maps the same frames
        // read-only for its blit, and the protocol serialises access: this app
        // is parked in its present call while the session reads.
        let frames = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) };

        let (event_endpoint, set) = match bind_event_mailbox() {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        let (mut surface, server, mut gallery) = match open_gallery_window(
            client,
            grant,
            event_endpoint,
            &mode,
            themes.active(),
            desktop.scale(),
            frames,
        ) {
            Ok(triple) => triple,
            Err(code) => return code,
        };

        let events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        run_event_loop(
            &mut surface,
            &mut desktop,
            &mut themes,
            &mut gallery,
            frames,
            &mode,
            events,
        )
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
