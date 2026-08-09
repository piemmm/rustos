//! The `Run` entry-point binary of the graphical login screen, installed at
//! `/System/Services/greeter.app/Run` — the screen the `login` authority
//! spawns as the `greeter` service account to ask who is at the machine
//! (`plans/NEW-DESKTOP-LOGIN.md` G3).
//!
//! This is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` for `_start`, the stack canary, the panic handler, the
//! `#[global_allocator]`, and the seat / shared-memory / wait-set / IPC /
//! clock syscall wrappers. `tairix_rt::entry!` names this program's `main`.
//!
//! # What it does
//!
//! Acquire the boot seat's exclusive lease, query the display mode, map a
//! double-buffered frame region and grant it to the display service, page the
//! offerable accounts off `SESSION_ENDPOINT`, paint the first frame, and then
//! **park**: one wait set holding the seat's input, with a timeout set to the
//! next thing that actually needs a repaint. An untouched login screen arms no
//! timer at all and consumes no CPU.
//!
//! A verified secret fades the screen to black and then exits `0`; the
//! authority is watching for that exit and starts the session itself, and the
//! desktop comes up out of the same black. The fade is bounded and cannot
//! fail: a lost seat, a refused present, or a stopped clock ends it early and
//! the exit is still `0`. Everything else keeps asking.
//!
//! The shipped wallpaper is untrusted input, so it is decoded by re-entering
//! this same binary as a capability-empty sandbox worker — never in the
//! address space that owns the seat. The worker role is checked before
//! anything else in `main`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the optional
// `tairix-rt` runtime through the default `program` feature. Host tooling
// builds only this crate's *library*, so this module never enters that build.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::string::String;
    use alloc::vec::Vec;

    use tairix_abi::display_ipc::DISPLAY_ENDPOINT;
    use tairix_abi::driver::display::{Display, DisplayMode};
    use tairix_abi::fs::OpenFlags;
    use tairix_abi::input::{KeyInput, PointerInput};
    use tairix_abi::seat::ReleaseSurface;
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_abi::session_ipc::SESSION_ENDPOINT;
    use tairix_abi::sysinfo::{SysinfoQueryId, SystemIdentity};
    use tairix_abi::time::{Time64, WallTimeState};
    use tairix_abi::{Errno, WaitSetOp, WaitSourceKind};
    use tairix_display::{DisplayClient, DisplayTransport, RemoteDisplay};
    use tairix_geometry::Scale;
    use tairix_greeter::Verdict;
    use tairix_greeter_service::accounts::{load_accounts, DirectoryError, SessionTransport};
    use tairix_greeter_service::cursor::pointer_image;
    use tairix_greeter_service::events::{
        ACCOUNTS_UNAVAILABLE, AUTHORITY_UNREACHABLE, POINTER_UNAVAILABLE, SCREEN_READY,
        SCREEN_UNAVAILABLE, VERDICT_RECEIVED, WALLPAPER_UNAVAILABLE,
    };
    use tairix_greeter_service::frame::{Present, Scanout};
    use tairix_greeter_service::screen::LoginScreen;
    use tairix_log::{Event, EventId, Field, FieldValue, Level};
    use tairix_procinfo::{call, IpcTransport};
    use tairix_raster::Surface;
    use tairix_rt::io::{Stderr, Write};
    use tairix_rt::LogSink;
    use tairix_sandbox::imagerender::{render_wallpaper, ImageRenderService};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_theme::ThemeRegistry;
    use tairix_wallpaper::{default_wallpaper_path, WallpaperFit, MAX_WALLPAPER_BYTES};

    /// Frames in the presented ring: one being scanned out, one being written.
    const FRAME_COUNT: u32 = 2;

    /// Wait-set token for "the seat has input waiting".
    const SEAT_TOKEN: u64 = 0;

    /// The seat's lease was refused, or taken away while the screen ran, so
    /// there is no screen to own.
    const EXIT_NO_SEAT: i32 = 3;

    /// The display could not describe a screen with pixels on it.
    const EXIT_NO_DISPLAY: i32 = 4;

    /// The shared frame region was refused.
    const EXIT_NO_FRAMES: i32 = 5;

    /// The wait set could not be built, so the loop could only busy-poll.
    const EXIT_NO_WAITSET: i32 = 6;

    /// State one reason on `stderr` and answer `code`, so no abnormal exit is
    /// a bare number. The reason never names an account or a secret.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "greeter: {reason}");
        record(SCREEN_UNAVAILABLE, Level::Error, reason);
        code
    }

    /// The audit sink every record goes through.
    static LOG_SINK: LogSink = LogSink;

    /// Emit one audit record.
    fn record(id: EventId, level: Level, message: &str) {
        let _ = tairix_log::log(
            &LOG_SINK,
            &Event {
                level,
                id,
                message,
                fields: &[Field {
                    key: "service",
                    value: FieldValue::Str("greeter"),
                }],
            },
        );
    }

    /// The production [`DisplayTransport`]: one synchronous `ipc_call` to the
    /// reserved display endpoint. The service re-checks this task's live seat
    /// lease kernel-side on every request, so the transport carries no claimed
    /// authority.
    struct RtDisplayTransport;

    impl DisplayTransport for RtDisplayTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(DISPLAY_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The production [`SessionTransport`]: one synchronous `ipc_call` to the
    /// session authority. The authority checks this task's *attested* uid and
    /// console on every request; nothing here claims anything.
    struct RtSessionTransport;

    impl SessionTransport for RtSessionTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(SESSION_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// Read the machine's name, or an empty string when it cannot be had.
    ///
    /// Display chrome, so an unreachable or refusing information service
    /// leaves that line blank rather than inventing a name.
    fn host_name() -> String {
        let Ok(payload) = call(&IpcTransport, SysinfoQueryId::SYSTEM_IDENTITY, &[]) else {
            return String::new();
        };
        let Ok(identity) = SystemIdentity::from_bytes(&payload) else {
            return String::new();
        };
        core::str::from_utf8(identity.hostname_bytes())
            .map(String::from)
            .unwrap_or_default()
    }

    /// The wall clock, or `None` when no trusted time has been set.
    fn wall_now() -> Option<Time64> {
        let reading = tairix_rt::wall_time().ok()?;
        (reading.state() != WallTimeState::Unset).then(|| reading.time())
    }

    /// Read a file up to `cap` bytes, refusing anything longer.
    fn read_file(path: &str, cap: usize) -> Result<Vec<u8>, Errno> {
        let ret = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        if ret < 0 {
            return Err(Errno::from_syscall(ret));
        }
        let Ok(fd) = u32::try_from(ret) else {
            return Err(Errno::LengthOutOfRange);
        };
        let outcome = read_to_end(fd, cap);
        let _ = tairix_rt::fs_close(fd);
        outcome
    }

    /// Read `fd` to end-of-file, stopping one chunk past `cap`.
    fn read_to_end(fd: u32, cap: usize) -> Result<Vec<u8>, Errno> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 1024];
        while bytes.len() <= cap {
            let at = u64::try_from(bytes.len()).map_err(|_| Errno::LengthOutOfRange)?;
            let read = tairix_rt::fs_read(fd, at, &mut chunk).map_err(Errno::from_syscall)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        Ok(bytes)
    }

    /// Decode the shipped wallpaper, screen-fitted, in a capability-empty
    /// sandbox worker.
    ///
    /// `None` for every failure — absent, oversize, undecodable, or a decode
    /// that did not fill the screen — because the flat desktop colour is a
    /// perfectly good backdrop and a login screen must appear regardless.
    fn wallpaper(mode: &DisplayMode) -> Option<Surface> {
        let path = default_wallpaper_path();
        let bytes = match read_file(&path, MAX_WALLPAPER_BYTES) {
            Ok(bytes) if bytes.len() > MAX_WALLPAPER_BYTES => {
                record(
                    WALLPAPER_UNAVAILABLE,
                    Level::Info,
                    "greeter: the wallpaper is larger than any this screen renders",
                );
                return None;
            }
            Ok(bytes) => bytes,
            Err(_) => {
                record(
                    WALLPAPER_UNAVAILABLE,
                    Level::Info,
                    "greeter: the wallpaper could not be read",
                );
                return None;
            }
        };
        let mut sandbox = ParserSandbox::new(RtLauncher::own_binary(), LogSink);
        let placed = render_wallpaper(
            &mut sandbox,
            mode.width_px,
            mode.height_px,
            WallpaperFit::Fill,
            &bytes,
        );
        let Ok(placed) = placed else {
            record(
                WALLPAPER_UNAVAILABLE,
                Level::Info,
                "greeter: the wallpaper could not be decoded",
            );
            return None;
        };
        Surface::from_rgba8(mode.width_px, mode.height_px, &placed)
    }

    /// The machine's offerable accounts as chooser tiles.
    ///
    /// An unreadable directory is logged and becomes an empty list: the
    /// chooser always carries its typed-name tile, so a user can still log in.
    fn tiles() -> Vec<tairix_greeter::AccountTile> {
        let mut transport = RtSessionTransport;
        match load_accounts(&mut transport) {
            Ok(tiles) => tiles,
            Err(err) => {
                unavailable_directory(err);
                Vec::new()
            }
        }
    }

    /// Audit an account directory that could not be read.
    fn unavailable_directory(err: DirectoryError) {
        let _ = tairix_log::log(
            &LOG_SINK,
            &Event {
                level: Level::Warn,
                id: ACCOUNTS_UNAVAILABLE,
                message: "greeter: the account directory could not be read",
                fields: &[
                    Field {
                        key: "service",
                        value: FieldValue::Str("greeter"),
                    },
                    Field {
                        key: "reason",
                        value: FieldValue::Str(err.reason()),
                    },
                ],
            },
        );
    }

    /// Audit one answer the authority gave.
    fn audit(answer: Verdict) {
        let (id, level, message) = match answer {
            Verdict::Verified => (
                VERDICT_RECEIVED,
                Level::Info,
                "greeter: a secret was verified",
            ),
            Verdict::Refused => (
                VERDICT_RECEIVED,
                Level::Info,
                "greeter: a secret was refused",
            ),
            Verdict::Unreachable => (
                AUTHORITY_UNREACHABLE,
                Level::Warn,
                "greeter: the authority gave no answer",
            ),
        };
        record(id, level, message);
    }

    /// Hand `present` to the display.
    fn show<D: Display>(display: &mut D, frame: &[u8], present: Present) {
        match present {
            Present::Nothing => {}
            Present::Region(region) => {
                let _ = display.present_region(frame, region);
            }
            Present::Whole => {
                let _ = display.present(frame);
            }
        }
    }

    /// How draining one of the seat's channels ended.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Drained {
        /// The channel is empty; the loop may park.
        Empty,
        /// A secret was verified, so the screen is finished.
        Verified,
        /// The seat gave neither a whole record nor an empty channel. The
        /// lease is gone or the channel is unreadable, and a lost lease reads
        /// ready forever, so parking again would spin.
        Lost,
    }

    /// Drain the seat's pointer channel into the screen and present the
    /// union of everything it changed, once.
    ///
    /// A burst of motion is a stream of reports the seat already holds, and
    /// each present is a round trip to the display service, so the whole
    /// burst is applied and then shown as one frame. A drain that changed
    /// nothing shows nothing.
    fn drain_pointer<D: Display, T: SessionTransport>(
        screen: &mut LoginScreen<T>,
        display: &mut D,
        mode: &DisplayMode,
    ) -> Drained {
        let mut wire = [0u8; PointerInput::WIRE_LEN];
        let mut pending = Present::Nothing;
        let drained = loop {
            let read = tairix_rt::pointer_read(SEAT_PRIMARY, &mut wire);
            if read == 0 {
                break Drained::Empty;
            }
            // Anything but one whole record is unreadable: a short count would
            // decode fresh bytes together with the tail of the last record.
            if usize::try_from(read).ok() != Some(PointerInput::WIRE_LEN) {
                break Drained::Lost;
            }
            let Ok(input) = PointerInput::from_bytes(&wire) else {
                continue;
            };
            let step = screen.on_pointer(&input, tairix_rt::clock_get());
            pending = pending.merged(step.present, mode);
            if let Some(answer) = step.answer {
                audit(answer.verdict);
            }
            if step.verified {
                break Drained::Verified;
            }
        };
        show(display, screen.frame(), pending);
        drained
    }

    /// Drain the seat's keyboard channel into the screen and present the
    /// union of everything it changed, once.
    fn drain_keyboard<D: Display, T: SessionTransport>(
        screen: &mut LoginScreen<T>,
        display: &mut D,
        mode: &DisplayMode,
    ) -> Drained {
        let mut wire = [0u8; KeyInput::WIRE_LEN];
        let mut pending = Present::Nothing;
        let drained = loop {
            let read = tairix_rt::keyboard_read(SEAT_PRIMARY, &mut wire);
            if read == 0 {
                break Drained::Empty;
            }
            // Anything but one whole record is unreadable: a short count would
            // decode fresh bytes together with the tail of the last record.
            if usize::try_from(read).ok() != Some(KeyInput::WIRE_LEN) {
                break Drained::Lost;
            }
            let Ok(input) = KeyInput::from_bytes(&wire) else {
                continue;
            };
            let event = tairix_window::key_input_event(input);
            let step = screen.on_input(&event, tairix_rt::clock_get());
            pending = pending.merged(step.present, mode);
            if let Some(answer) = step.answer {
                audit(answer.verdict);
            }
            if step.verified {
                break Drained::Verified;
            }
        };
        show(display, screen.frame(), pending);
        drained
    }

    /// Map the frame ring and hand it to the display service.
    ///
    /// `Err` is the exit code the failure earns, its reason already stated.
    fn frame_ring(
        client: &mut DisplayClient<RtDisplayTransport>,
        mode: &DisplayMode,
        frame_len: usize,
    ) -> Result<&'static mut [u8], i32> {
        let Some(total) = usize::try_from(FRAME_COUNT)
            .ok()
            .and_then(|count| frame_len.checked_mul(count))
        else {
            return Err(fail(EXIT_NO_FRAMES, "the frame ring's size overflows"));
        };
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return Err(fail(EXIT_NO_FRAMES, "the shared frame region was refused"));
        }
        // A zero handle is the kernel's refusal, not a usable grant.
        let grant = tairix_rt::shm_grant(region_id, DISPLAY_ENDPOINT);
        let grant = u64::try_from(grant).ok().filter(|handle| *handle != 0);
        let Some(grant) = grant else {
            return Err(fail(EXIT_NO_FRAMES, "the frame region grant was refused"));
        };
        if client.configure(grant, FRAME_COUNT, mode).is_err() {
            return Err(fail(
                EXIT_NO_DISPLAY,
                "the display service refused the frame configuration",
            ));
        }
        let Ok(base) = usize::try_from(base) else {
            return Err(fail(
                EXIT_NO_FRAMES,
                "the frame region lies outside the address width",
            ));
        };
        // SAFETY: the kernel mapped exactly `total` zeroed bytes read/write
        // into this process at `base`, and that mapping stays live for the
        // rest of the process — nothing unmaps or aliases it, which is what
        // makes the `'static` borrow honest. The display service maps the
        // same frames read-only for its blit, and the protocol serialises
        // access: this task is parked inside its present call while the
        // service reads, so the two never race.
        Ok(unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) })
    }

    /// Own the seat and run the screen until a secret is verified.
    ///
    /// Split from `main` so every exit after the acquire flows back through
    /// the one owner-checked release.
    fn session() -> i32 {
        let mut client = DisplayClient::new(RtDisplayTransport, SEAT_PRIMARY);
        let Ok(mode) = client.query() else {
            return fail(
                EXIT_NO_DISPLAY,
                "the display service refused the mode query, so there is no screen to draw on",
            );
        };
        let Some(scanout) = Scanout::new(mode) else {
            return fail(
                EXIT_NO_DISPLAY,
                "the queried mode describes no drawable screen",
            );
        };
        let frames = match frame_ring(&mut client, &mode, scanout.frame().len()) {
            Ok(frames) => frames,
            Err(code) => return code,
        };
        let Ok(mut display) = RemoteDisplay::new(client, mode, frames, FRAME_COUNT) else {
            return fail(EXIT_NO_FRAMES, "the frame ring rejected the queried mode");
        };

        let theme = ThemeRegistry::with_builtins().active().clone();
        let mut screen = LoginScreen::new(
            scanout,
            theme,
            Scale::ONE,
            host_name(),
            tiles(),
            RtSessionTransport,
        );
        if let Some(image) = wallpaper(&mode) {
            screen.set_wallpaper(image);
        }
        match pointer_image(Scale::ONE) {
            Some(image) => screen.set_pointer(image),
            None => record(
                POINTER_UNAVAILABLE,
                Level::Info,
                "greeter: the pointer cursor could not be drawn",
            ),
        }
        // The chrome goes up before the opening frame, so the screen appears
        // with its clock rather than gaining one a moment later.
        screen.refresh(tairix_rt::clock_get(), wall_now());
        // The opening frame is the veil at full black, so the screen appears
        // out of the black the seat was handed over cleared to rather than
        // snapping onto it; the park loop below runs the fade off the same
        // deadline as every other animation. A theme that fades instantly has
        // nothing to cover, and opens on the screen itself.
        let opening = match screen.begin_entry_fade(tairix_rt::clock_get()) {
            Present::Nothing => screen.repaint(),
            veiled => veiled,
        };
        show(&mut display, screen.frame(), opening);
        record(SCREEN_READY, Level::Info, "greeter: the login screen is up");

        // A negative return is the kernel's refusal, and it is exactly what
        // the conversion rejects, so one check covers both.
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            return fail(EXIT_NO_WAITSET, "the wait set could not be created");
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::SeatInput,
            SEAT_PRIMARY,
            SEAT_TOKEN,
        ) < 0
        {
            return fail(EXIT_NO_WAITSET, "the seat's input could not be waited on");
        }
        park_loop(&mut screen, &mut display, &mode, set)
    }

    /// Serve the screen until it is finished, parking between wakes.
    ///
    /// Each round drains the seat, brings the clock and any lockout up to
    /// date, and then parks on `set` until either input arrives or the
    /// nearest repaint falls due. An untouched screen arms no timer at all.
    fn park_loop<D: Display, T: SessionTransport>(
        screen: &mut LoginScreen<T>,
        display: &mut D,
        mode: &DisplayMode,
        set: u64,
    ) -> i32 {
        loop {
            let drained = match drain_keyboard(screen, display, mode) {
                Drained::Empty => drain_pointer(screen, display, mode),
                ended => ended,
            };
            match drained {
                Drained::Verified => {
                    fade_out(screen, display, mode, set);
                    return 0;
                }
                Drained::Lost => return fail(
                    EXIT_NO_SEAT,
                    "the seat stopped delivering readable input, so the screen is no longer ours",
                ),
                Drained::Empty => {}
            }
            let now = tairix_rt::clock_get();
            let wall = wall_now();
            // The clock and a running lockout ask the authority nothing, so
            // this round has a frame to show and no verdict to audit.
            let refreshed = screen.refresh(now, wall);
            show(display, screen.frame(), refreshed.present);
            let timeout = screen.park_timeout(tairix_rt::clock_get(), wall);
            let mut token = 0u64;
            let woken = tairix_rt::waitset_wait(set, timeout, &mut token);
            // Ready and the armed deadline are the two ways round the loop;
            // any other answer would leave nothing to park on, so exit rather
            // than spin.
            if woken < 0 && Errno::from_syscall(woken) != Errno::TimedOut {
                return fail(EXIT_NO_WAITSET, "the seat's input wait failed");
            }
        }
    }

    /// Take the screen to black, then let the caller leave.
    ///
    /// The desktop cannot appear until this process exits, so the screen goes
    /// black first and the desktop comes up out of the same black. It is a
    /// cosmetic step on an already-made decision, so every way it can go
    /// wrong ends it rather than the login: the loop is bounded by the frames
    /// the fade can ask for, a wait that fails or a seat that stops
    /// delivering ends it, and a refused present is ignored like every other.
    ///
    /// The seat's channels are still drained, though nothing acts on them:
    /// input left unread reads ready forever, and the park would return at
    /// once instead of pacing the fade.
    fn fade_out<D: Display, T: SessionTransport>(
        screen: &mut LoginScreen<T>,
        display: &mut D,
        mode: &DisplayMode,
        set: u64,
    ) {
        let opening = screen.begin_session_fade(tairix_rt::clock_get());
        show(display, screen.frame(), opening);
        for _ in 0..screen.session_fade_budget() {
            let Some(due) = screen.session_fade_due(tairix_rt::clock_get()) else {
                return;
            };
            if drain_keyboard(screen, display, mode) == Drained::Lost
                || drain_pointer(screen, display, mode) == Drained::Lost
            {
                return;
            }
            let mut token = 0u64;
            let woken = tairix_rt::waitset_wait(set, due, &mut token);
            if woken < 0 && Errno::from_syscall(woken) != Errno::TimedOut {
                return;
            }
            let darkened = screen.session_fade_step(tairix_rt::clock_get());
            show(display, screen.frame(), darkened);
        }
    }

    /// The program's entry point.
    ///
    /// Exit codes: `0` once a secret is verified and the screen has faded to
    /// black, and otherwise the bring-up code that names what was missing,
    /// each stated on `stderr`.
    fn main() -> i32 {
        // The sandbox worker role first, before any seat work: decoding the
        // wallpaper re-enters this same binary with the reserved role
        // argument, and that capability-empty child serves parses only.
        if worker_role() {
            let mut service = ImageRenderService::default();
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }
        if tairix_rt::display_acquire(SEAT_PRIMARY) < 1 {
            return fail(
                EXIT_NO_SEAT,
                "the seat is already held, so there is no screen to own",
            );
        }
        let code = session();
        // Owner-checked release on every exit path: a lease already lost
        // refuses with a typed error, ignored — heal, never widen.
        //
        // A clean exit means a verified secret and a screen already faded to
        // black, with the authority about to start a desktop on this seat, so
        // the screen is handed on cleared rather than replaying the text
        // console into the gap. Every other exit is going back to the text
        // login, which needs its console to say why.
        let next = if code == 0 {
            ReleaseSurface::Handover
        } else {
            ReleaseSurface::Text
        };
        let _ = tairix_rt::display_release(SEAT_PRIMARY, next);
        code
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the freestanding `tairix-rt` `_start` path is not compiled — on the
// host (`cargo build --workspace`, clippy, fmt), or for a `program`-less build
// of this crate — this inert `main` keeps the crate building under the host
// tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
