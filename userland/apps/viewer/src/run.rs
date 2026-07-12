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
//! in the host-tested `rustos_viewer` engine; this binary composes them
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

    use rustos_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use rustos_abi::input::{KeyInput, KeyValue, NamedKeyCode};
    use rustos_abi::window_ipc::{WindowEvent, WINDOW_ENDPOINT};
    use rustos_abi::{Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, ORIGIN_WIRE_LEN};
    use rustos_theme::ThemeRegistry;
    use rustos_viewer::{
        content_lines, render_lines, render_status, visible_cols, visible_rows, CONTENT_MAX,
        WIN_HEIGHT, WIN_WIDTH,
    };
    use rustos_window::{EventSource, WindowClient, WindowEvents, WindowTransport};

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
        let _ = rustos_rt::stderr(b"viewer: ");
        let _ = rustos_rt::stderr(reason.as_bytes());
        let _ = rustos_rt::stderr(b"\n");
        code
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to
    /// the reserved window endpoint per request. The session attests the
    /// caller kernel-side on every request, so the transport carries no
    /// claimed authority.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            rustos_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(errno_from)
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
                match rustos_rt::ipc_recv(self.endpoint, event, &mut sender) {
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
                        if rustos_rt::waitset_wait(self.set, u64::MAX, &mut token) != 0 {
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
        let fd = u32::try_from(rustos_rt::fd_redeem(handle)).ok()?;
        let mut content = Vec::new();
        let mut chunk = [0u8; 1024];
        while content.len() < CONTENT_MAX {
            let want = chunk.len().min(CONTENT_MAX - content.len());
            let Ok(got) = rustos_rt::fs_read(fd, content.len() as u64, &mut chunk[..want]) else {
                let _ = rustos_rt::fs_close(fd);
                return None;
            };
            if got == 0 {
                break;
            }
            content.extend_from_slice(&chunk[..got]);
        }
        let _ = rustos_rt::fs_close(fd);
        Some(content)
    }

    /// Copy `surface` into the shared window frame and present it whole.
    fn present_surface<T: WindowTransport>(
        surface: &rustos_raster::Surface,
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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    fn main() -> i32 {
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
        let base = rustos_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        }
        let grant = rustos_rt::shm_grant(region_id, WINDOW_ENDPOINT);
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
        let Ok(origin) = rustos_rt::self_origin() else {
            return fail(EXIT_NO_EVENTS, "own identity unavailable");
        };
        let event_endpoint = rustos_window::event_endpoint_for(origin.pid());
        if rustos_abi::ipc::is_reserved_endpoint(event_endpoint)
            || rustos_rt::port_bind(event_endpoint, WindowEvent::WIRE_LEN, EVENT_CAPACITY) != 0
        {
            return fail(EXIT_NO_EVENTS, "event mailbox bind refused");
        }
        let set = rustos_rt::waitset_create();
        if set < 0 {
            return fail(EXIT_NO_EVENTS, "wait-set refused");
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel handle.
        let set = set as u64;
        if rustos_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            event_endpoint,
            EVENT_TOKEN,
        ) != 0
        {
            return fail(EXIT_NO_EVENTS, "event mailbox wait refused");
        }

        // --- Open the window, show the waiting state, and immediately
        // ask the session's trusted picker for a file.
        let mut client = WindowClient::new(RtWindowTransport);
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) =
            client.create(grant as u64, event_endpoint, FRAME_COUNT, &mode, "Viewer")
        else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        let themes = ThemeRegistry::with_builtins();
        let theme = themes.active();
        let show = |text: &str, client: &mut WindowClient<RtWindowTransport>, frames: &mut [u8]| {
            render_status(text, theme)
                .ok_or(Errno::NoSpace)
                .and_then(|surface| present_surface(&surface, client, window, frames, &mode))
        };
        if show("Choose a file...", &mut client, frames).is_err() {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }
        if client.pick_file(window).is_err() {
            // A refused pick (another pick showing, or a session without
            // filesystem reach) is not fatal: the viewer stays open and
            // Enter asks again.
            let _ = show("Pick refused - Enter retries", &mut client, frames);
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
                    Some(content) => {
                        let lines = content_lines(&content, visible_rows(), visible_cols());
                        render_lines(&lines, theme)
                            .ok_or(Errno::NoSpace)
                            .and_then(|surface| {
                                present_surface(&surface, &mut client, window, frames, &mode)
                            })
                    }
                    // A refused redemption or read delegated nothing the
                    // viewer can show; state it honestly.
                    None => show("Delegated read refused", &mut client, frames),
                },
                WindowEvent::PickCancelled { .. } => {
                    show("No file chosen - Enter retries", &mut client, frames)
                }
                WindowEvent::Key {
                    key:
                        KeyInput::Pressed {
                            key: KeyValue::Named(NamedKeyCode::Enter),
                            ..
                        },
                    ..
                } => {
                    // Ask for another pick; a refusal (one already
                    // showing) leaves the current content on screen.
                    let _ = client.pick_file(window);
                    Ok(())
                }
                WindowEvent::CloseRequested { .. } => {
                    // The desktop asked; close the window and end cleanly.
                    let _ = client.close(window);
                    return 0;
                }
                // Focus changes, other keys, and pointer events repaint
                // nothing; the viewer is picker-driven.
                _ => Ok(()),
            };
            if outcome.is_err() {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
