//! The `files.app` bundle's `Run` entry point (`plans/APPWIN.md` AW3):
//! the windowed file browser, the first app served over the desktop
//! session's window channel.
//!
//! # What the program wires (and what stays in the libraries)
//!
//! Everything with behaviour worth testing lives in host-tested crates —
//! the shared browser engine and its validated path spelling
//! (`tairix_browse`), the themed listing renderer
//! (`tairix_browse::render`), and the window
//! channel's client half (`tairix_window`). This binary only composes
//! them over the live syscalls:
//!
//! * One `shm_create`d frame region, granted to the reserved window
//!   endpoint (the zero-copy surface the session maps once at create).
//! * One `port_bind`-bound event mailbox the app **parks** on through
//!   its wait-set — never a poll loop. Every received event carries its
//!   sender's kernel-attested origin, and the app accepts only events
//!   from the session identity the (squat-protected) create reply
//!   named: no other process can feed it forged input (fail closed).
//! * The `WindowClient` calls (create / present / close) over `ipc_call`
//!   and the `WindowEvents` typed wait over the parked source.
//!
//! Keyboard navigation drives the browser (`Down`/`Up` select, `Enter`
//! opens a directory, `Backspace` goes up); a `CloseRequested` from the
//! desktop closes the window and ends the program cleanly. Every
//! bring-up refusal exits fail-loud with a reserved code and a stated
//! reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode};
    use tairix_abi::window_ipc::{WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, ORIGIN_WIRE_LEN};
    use tairix_browse::render::render;
    use tairix_browse::{Browser, VfsDirectorySource, WIN_HEIGHT, WIN_WIDTH};
    use tairix_geometry::Rect;
    use tairix_theme::ThemeRegistry;
    use tairix_window::{EventSource, WindowClient, WindowEvents, WindowTransport};

    /// Exit code when the initial directory listing was refused (no
    /// filesystem reach, or a corrupt stream). A reserved, fail-closed
    /// value: the browser never shows a fabricated listing.
    const EXIT_NO_LISTING: i32 = 80;

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

    /// The event mailbox's bounded capacity: input-rate events, drained
    /// after every wake, so a small queue is ample and a stalled app
    /// costs the kernel a bounded mailbox — never unbounded memory.
    const EVENT_CAPACITY: usize = 32;

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
        let _ = tairix_rt::stderr(b"files: ");
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
    /// closed), so no other process can feed the app forged input.
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

    /// Render the browser into `frame` (the shared window surface) and
    /// present the whole window.
    ///
    /// The full-window damage is deliberate: a listing change repaints
    /// the path bar, the rows, and the selection highlight together, and
    /// the surface is one window — not a screen — so the copy is small.
    fn present_frame<S, T>(
        browser: &Browser<S>,
        theme: &tairix_theme::Theme,
        client: &mut WindowClient<T>,
        window: u64,
        frame: &mut [u8],
        mode: &DisplayMode,
    ) -> Result<(), Errno>
    where
        S: tairix_browse::DirectorySource,
        T: WindowTransport,
    {
        let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);
        let surface = render(browser, theme, viewport).ok_or(Errno::LengthOutOfRange)?;
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

    /// Apply one delivered event to the browser, reporting whether the
    /// listing changed (and must re-present) and whether the app should
    /// end (the desktop asked the window to close).
    fn apply_event<S: tairix_browse::DirectorySource>(
        browser: &mut Browser<S>,
        event: &WindowEvent,
    ) -> (bool, bool) {
        match event {
            WindowEvent::Key {
                key: KeyInput::Pressed { key, .. },
                ..
            } => match key {
                KeyValue::Named(NamedKeyCode::Down) => {
                    browser.select_next();
                    (true, false)
                }
                KeyValue::Named(NamedKeyCode::Up) => {
                    browser.select_previous();
                    (true, false)
                }
                KeyValue::Named(NamedKeyCode::Enter) => {
                    // Opening a file (or an unreadable directory) is a
                    // refused no-op today: the browser lists, it does
                    // not launch. The listing stays as it was.
                    (browser.open_selected().is_ok(), false)
                }
                KeyValue::Named(NamedKeyCode::Backspace) => {
                    (browser.go_up().unwrap_or(false), false)
                }
                _ => (false, false),
            },
            WindowEvent::CloseRequested { .. } => (false, true),
            // Focus changes, key releases, and pointer events repaint
            // nothing today; the selection model is keyboard-driven. The
            // browser never requests a pick, so a pick conclusion is a
            // session bug and is ignored rather than acted on (an
            // unredeemed delegation is reclaimed by the kernel at exit).
            WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Pointer { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. } => (false, false),
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    fn main() -> i32 {
        // --- The browser over the live, capability-checked listing call.
        let source = VfsDirectorySource::new(|path: &str| {
            tairix_rt::read_dir_all(path.as_bytes()).map_err(errno_from)
        });
        let Ok(mut browser) = Browser::open_root(source) else {
            return fail(EXIT_NO_LISTING, "root directory listing refused");
        };

        // --- The shared window surface: FRAME_COUNT frames shaped as the
        // window mode, created here and granted to the session.
        let mode = DisplayMode {
            width_px: WIN_WIDTH,
            height_px: WIN_HEIGHT,
            stride_bytes: WIN_WIDTH * 4,
            format: DisplayFormat::Rgba8888,
        };
        let frame_len = (mode.stride_bytes as usize) * (mode.height_px as usize);
        let total = frame_len * FRAME_COUNT as usize;
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        }
        let grant = tairix_rt::shm_grant(region_id, WINDOW_ENDPOINT);
        if grant < 1 {
            return fail(EXIT_NO_FRAMES, "frame region grant refused");
        }
        let Ok(base) = usize::try_from(base) else {
            return fail(
                EXIT_NO_FRAMES,
                "frame region base outside the address width",
            );
        };
        // SAFETY: the kernel mapped at least `total` zeroed bytes
        // read/write into this process at `base` (`shm_create` maps the
        // exact length it was asked for) and the mapping stays live for
        // the life of the process — nothing below unmaps or aliases it.
        // The session maps the same frames read-only for its blit, and
        // the protocol serialises access: this app is parked in its
        // present call while the session reads.
        let frames = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) };

        // --- The event mailbox the app parks on. The id is unique by
        // construction (the shared `event_endpoint_for` naming rule: this
        // task's never-reused kernel id under a fixed tag) and never
        // reserved; the bind is refused otherwise.
        let Ok(origin) = tairix_rt::self_origin() else {
            return fail(EXIT_NO_EVENTS, "own identity unavailable");
        };
        let event_endpoint = tairix_window::event_endpoint_for(origin.pid());
        if tairix_abi::ipc::is_reserved_endpoint(event_endpoint)
            || tairix_rt::port_bind(event_endpoint, WindowEvent::WIRE_LEN, EVENT_CAPACITY) != 0
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

        // --- Open the window and paint the first listing.
        let mut client = WindowClient::new(RtWindowTransport);
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) =
            client.create(grant as u64, event_endpoint, FRAME_COUNT, &mode, "Files")
        else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        let themes = ThemeRegistry::with_builtins();
        let theme = themes.active();
        if present_frame(&browser, theme, &mut client, window, frames, &mode).is_err() {
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
            let event = match events.wait() {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };
            let (changed, close) = apply_event(&mut browser, &event);
            if close {
                // The desktop asked; close the window and end cleanly.
                let _ = client.close(window);
                return 0;
            }
            if changed
                && present_frame(&browser, theme, &mut client, window, frames, &mode).is_err()
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
