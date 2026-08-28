//! The `datetime.app` bundle's `Run` entry point: the windowed Date & Time
//! application the desktop clock's *Set Date & Time…* row starts.
//!
//! # What this binary is, and what stays in the library
//!
//! The fields, their validation, the instant they compose, and the window's
//! geometry and paint all live in the host-tested `tairix_datetime` engine.
//! This binary composes them over the live syscalls exactly as the other
//! windowed apps do: one `shm_create`d frame region granted to the window
//! endpoint, one `port_bind`-bound event mailbox parked on through a
//! wait-set (every accepted event authenticated against the session identity
//! the create reply named), and the `WindowClient` calls over `ipc_call`.
//!
//! # It is *given* the authority, and never assumes it
//!
//! Stepping the machine's clock needs `CAP_TIME_SET`, which this bundle's
//! signed manifest requests and the kernel grants only as
//! `manifest ∩ the launching account's ceiling`. The desktop that starts
//! this app holds no such capability: it re-authenticates an account that
//! does, through the console's elevation broker, and the broker starts this
//! program as that account.
//!
//! So a refused set is an ordinary outcome, not a bug: an account whose
//! ceiling withholds `CAP_TIME_SET` gets `PermissionDenied`, and the app
//! **says so in its window and on `stderr` and keeps running**. It never
//! reports a clock it did not change as changed.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    extern crate alloc;

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::input::KeyInput;
    use tairix_abi::time::WallTimeState;
    use tairix_abi::window_ipc::{WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, ORIGIN_WIRE_LEN};
    use tairix_datetime::view;
    use tairix_datetime::{Editor, Status};
    use tairix_display::{winframe, SERIAL};
    use tairix_geometry::Scale;
    use tairix_input::{InputEvent, Key, NamedKey};
    use tairix_raster::Surface;
    use tairix_rt::io::{Stderr, Write};
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_window::{
        key_input_event, pointer_point, Desktop, EventSource, WindowClient, WindowEvents,
        WindowFrames, WindowSizing, WindowTransport,
    };

    /// Exit code when the shared frame region could not be created or granted
    /// to the window endpoint. A reserved, fail-closed value.
    const EXIT_NO_FRAMES: i32 = 81;

    /// Exit code when the event mailbox could not be bound or observed through
    /// the wait-set. A reserved, fail-closed value: the app exits rather than
    /// degrade into a busy re-poll.
    const EXIT_NO_EVENTS: i32 = 82;

    /// Exit code when the desktop session refused the window create (no
    /// graphical session, or the channel refused the geometry). A reserved,
    /// fail-closed value.
    const EXIT_NO_WINDOW: i32 = 83;

    /// Exit code when a present was refused or the event channel died (the
    /// session went away). A reserved, fail-closed value.
    const EXIT_CHANNEL_LOST: i32 = 84;

    /// Frames in the shared region. The window protocol serialises a present
    /// (the app is parked in the call while the session reads), so a single
    /// frame is race-free; the constant names the choice.
    const FRAME_COUNT: u32 = 1;

    /// The wait-set token of the event-mailbox member.
    const EVENT_TOKEN: u64 = 1;

    /// The wait-set token of the memory-pressure member: the kernel wakes the
    /// park when the machine's pressure band changes, so the glyph cache is
    /// trimmed as memory tightens instead of being held until something else
    /// is starved.
    const PRESSURE_TOKEN: u64 = 2;

    /// State a reason on `stderr`: an exit code alone is not a diagnosis, and
    /// a refused optional step still says so.
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "datetime: {reason}");
    }

    /// State the abnormal-exit reason and hand `code` back for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        report(reason);
        code
    }

    /// Declare this application's presence on the desktop's icon bar: the
    /// shared convention's two rows — the session-drawn information row and
    /// *Quit* — with the primary click left to the session so it raises the
    /// window.
    ///
    /// A refused declaration is an answer, not a death: the app says so and
    /// carries on with no slot of its own — its window is still reachable
    /// through the one the session derives from it.
    fn declare_app_bar(client: &mut WindowClient<RtWindowTransport>, endpoint: u64) {
        match tairix_window::info_and_quit(endpoint) {
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

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to the
    /// reserved window endpoint per request. The session attests the caller
    /// kernel-side on every request, so the transport carries no claimed
    /// authority.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty, and accept only events
    /// whose kernel-attested sender is the desktop session named by the create
    /// reply — anything else is dropped (fail closed), so no other process can
    /// feed the app forged input.
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
                        // A short frame or a foreign sender is dropped, never
                        // delivered: the mailbox is open to any capable
                        // sender, so the kernel-attested origin is the
                        // authentication.
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

    /// Bind the app's own event mailbox and add it to a fresh wait-set,
    /// returning both. Fails closed with the reserved exit code on any refusal
    /// rather than degrading into a re-poll.
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

    /// A `width_px` × `height_px` RGBA window mode, one frame's worth per row.
    /// The one place this app's mode is shaped, so the create and every
    /// present agree on stride and format.
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

    /// Paint the editor and present the whole frame.
    ///
    /// The pixels come through the client, which re-attaches the region first
    /// if the session released it while the window was hidden.
    fn repaint<T: WindowTransport>(
        editor: &Editor,
        theme: &Theme,
        scale: Scale,
        client: &mut WindowClient<T>,
        window: u64,
        frames: &mut WindowFrames,
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        let surface = view::render(editor, scale, theme).ok_or(Errno::NoSpace)?;
        let pixels = client
            .frame_pixels(frames, window, FRAME_COUNT, mode)
            .ok_or(Errno::NotAttached)?;
        present_surface(&surface, client, window, pixels, mode)
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

    /// Read the machine's wall clock, or `None` when the read itself was
    /// refused.
    ///
    /// A refused *read* is distinct from an unset clock: the first is a
    /// failure to state, the second is the honest answer that no time has been
    /// established.
    fn read_clock() -> Option<tairix_abi::time::WallClockReading> {
        tairix_rt::wall_time().ok()
    }

    /// Commit the editor's fields: validate, compose, and ask the kernel to
    /// step the clock.
    ///
    /// Every outcome is stated. A field fault never reaches the kernel; a
    /// refused set is reported as refused and the clock is left alone. The
    /// provenance is [`WallTimeState::Adjusted`], which is what a human at the
    /// keyboard actually is — a step correction, not a synchronised source.
    fn commit(editor: &mut Editor) {
        let instant = match editor.compose() {
            Ok(instant) => instant,
            Err(fault) => {
                report(fault.message());
                editor.set_status(Status::Rejected(fault));
                return;
            }
        };
        let ret = tairix_rt::wall_time_set(instant, WallTimeState::Adjusted);
        if ret == 0 {
            editor.set_status(Status::Applied);
            return;
        }
        let err = Errno::from_syscall(ret);
        let status = if err == Errno::PermissionDenied {
            Status::Denied
        } else {
            Status::Failed("The clock could not be set.")
        };
        // Loud on both channels: the window states it for the user in front
        // of it, `stderr` for whoever started the app.
        if let Some(message) = status.message() {
            report(message);
        }
        editor.set_status(status);
    }

    /// Apply one key press to the editor, answering whether the window should
    /// close.
    ///
    /// `Tab` moves between fields, `Enter` commits, `Escape` closes, and a
    /// digit or `Backspace` edits the focused field. Nothing else is
    /// interpreted: a key with no meaning here is ignored rather than guessed
    /// at.
    fn apply_key(editor: &mut Editor, key: Key) -> bool {
        match key {
            Key::Named(NamedKey::Escape) => return true,
            Key::Named(NamedKey::Tab) => editor.focus_next(),
            Key::Named(NamedKey::Enter) => commit(editor),
            Key::Named(NamedKey::Backspace) => editor.backspace(editor.focus()),
            Key::Char(ch) => editor.push(editor.focus(), ch),
            // Every other named key means nothing in a six-field form; it is
            // ignored rather than guessed at.
            Key::Named(_) => {}
        }
        false
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    #[allow(clippy::too_many_lines)] // One linear bring-up plus one event loop; splitting would separate the frame-region grant from the create it must precede.
    fn main() -> i32 {
        // --- What the clock says now. An unset clock leaves the fields empty
        // and says so; a refused read says that instead, so the two are never
        // confused for one another.
        let mut editor = Editor::new();
        match read_clock() {
            Some(reading) => editor.seed(reading),
            None => editor.set_status(Status::Failed("The clock could not be read.")),
        }

        // --- The desktop the app must fit on: screen extent, UI scale, and
        // light/dark appearance. Queried once, before any window is created,
        // so the first frame is correctly sized and themed at the real
        // screen's own scale.
        let mut client = WindowClient::new(RtWindowTransport);
        let info = match client.desktop() {
            Ok(info) => info,
            Err(err) => {
                return fail(
                    EXIT_NO_WINDOW,
                    &alloc::format!("desktop query refused: {err}"),
                )
            }
        };
        let mut desktop = match Desktop::new(info) {
            Ok(desktop) => desktop,
            Err(err) => {
                return fail(
                    EXIT_NO_WINDOW,
                    &alloc::format!("cannot draw this desktop: {err}"),
                )
            }
        };
        let mut themes = ThemeRegistry::with_builtins();
        themes.set_appearance(desktop.appearance());
        let mut theme = themes.active();

        // --- The shared window surface, shaped at the desktop's own scale.
        let bounds = view::window_bounds(desktop.scale());
        let mode = mode_for(bounds.width, bounds.height);
        let Some(mut frames) = WindowFrames::create(region_bytes(&mode)) else {
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

        // --- The icon-bar presence first: a declared presence belongs to the
        // process, so declaring it before this process owns a window is what
        // makes its slot carry this menu from the moment it appears.
        declare_app_bar(&mut client, event_endpoint);
        // Fixed size: the window is a short form, and a resizable one would
        // only stretch six fields across empty space.
        let Ok((window, server)) = client.create(
            grant,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            view::TITLE,
            WindowSizing::Fixed,
        ) else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        if repaint(
            &editor,
            theme,
            desktop.scale(),
            &mut client,
            window,
            &mut frames,
            &mode,
        )
        .is_err()
        {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The event loop: park, apply, repaint. A dead channel ends the
        // app fail-loud; a clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        loop {
            let event = match events.wait(&mut client) {
                Ok(event) => event,
                // A malformed frame from the authenticated session is refused
                // and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };

            // A desktop change (scale, appearance) is applied before the
            // app-specific handling, so the repaint below draws in the
            // appearance now in use. The window keeps its pixel extent: it
            // was granted at the scale in force when it opened, and a fixed
            // form cannot re-shape its own frame region.
            match desktop.apply(&event) {
                Ok(true) => {
                    themes.set_appearance(desktop.appearance());
                    theme = themes.active();
                }
                Ok(false) => {}
                Err(err) => report(&alloc::format!("desktop change refused: {err}")),
            }

            match event {
                WindowEvent::Pointer { x, y, .. } => {
                    // A press inside a field gives it the keyboard; the
                    // actions are reached with Enter and Escape, which every
                    // form here answers to.
                    let at = pointer_point(x, y);
                    if let Some(field) = view::field_at(desktop.scale(), at.x, at.y) {
                        editor.set_focus(field);
                    }
                }
                WindowEvent::Key {
                    key: pressed @ KeyInput::Pressed { .. },
                    ..
                } => {
                    if let InputEvent::KeyPressed { key, .. } = key_input_event(pressed) {
                        if apply_key(&mut editor, key) {
                            return 0;
                        }
                    }
                }
                // The session asked the window to close: a clean end.
                WindowEvent::CloseRequested { .. } => return 0,
                // Nobody can see the window, so the session gave its copy of
                // the pixels back and unmapped the region. Let go of this side
                // too — the pages go only when both do — and paint nothing
                // until the redraw request that follows the window being shown
                // again, which re-attaches a fresh region.
                WindowEvent::ContentReleased { .. } => {
                    frames.release();
                    continue;
                }
                _ => {}
            }

            if repaint(
                &editor,
                theme,
                desktop.scale(),
                &mut client,
                window,
                &mut frames,
                &mode,
            )
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
