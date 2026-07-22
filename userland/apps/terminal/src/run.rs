//! The `terminal.app` bundle's `Run` entry point (`plans/APPWIN.md` AW4):
//! the windowed terminal emulator hosting the user's shell over the
//! desktop session's window channel.
//!
//! # What the program wires (and what stays in the libraries)
//!
//! Everything with behaviour worth testing lives in host-tested crates —
//! the screen model and its `lib/vt`-consuming parser (`tairix_terminal`),
//! the themed cell renderer (`tairix_terminal::render`), the spawned
//! shell's pipe wiring (`tairix_terminal::spawned`), and the window
//! channel's client half (`tairix_window`). This binary only composes
//! them over the live syscalls:
//!
//! * Two kernel pipes to a shell child spawned under this app's own
//!   `CAP_PROC_SPAWN`: keystrokes flow to the shell's standard input, and
//!   the shell's standard output *and* error flow back to the screen. The
//!   child-side ends are closed here after the spawn, so the shell
//!   observes end-of-file the moment this terminal exits, and the
//!   terminal observes end-of-stream the moment the shell does.
//! * One `shm_create`d frame region, granted to the reserved window
//!   endpoint (the zero-copy surface the session maps once at create).
//! * One wait-set the program **parks** on — never a poll loop — with
//!   three members: its `port_bind`-bound event mailbox (the desktop's
//!   `Focus`/`Key`/`Pointer`/`CloseRequested` deliveries, each accepted
//!   only from the session identity the squat-protected create reply
//!   named), the shell-output pipe's read end (the `Stream` source this
//!   stage added — ready on buffered bytes or end-of-stream), and the
//!   shell child itself (ready when it exited and awaits reaping).
//!
//! A key press is encoded through the one shared `lib/keymap` rule and
//! written to the shell; the shell's bytes are pumped into the grid and
//! the repainted frame presented. The shell exiting (end-of-stream or the
//! child's exit) ends the session cleanly, as does a `CloseRequested`
//! from the desktop. Every bring-up refusal exits fail-loud with a
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

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::window_ipc::{WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, WaitStatus, ORIGIN_WIRE_LEN,
    };
    use tairix_keymap::{encode_key_input, MAX_KEY_BYTES};
    use tairix_terminal::render::render;
    use tairix_terminal::{
        shell_load_failure, shell_wires, PipeShellSource, ShellSource, Terminal, COLS, ROWS, TERM,
        WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_users::DEFAULT_SHELL;
    use tairix_window::{event_endpoint_for, WindowClient, WindowTransport};

    /// Exit code when the shell could not be hosted (a pipe or the spawn
    /// itself was refused). A reserved, fail-closed value: the terminal
    /// never shows a window with no shell behind it.
    const EXIT_NO_SHELL: i32 = 80;

    /// Exit code when the shared frame region could not be created or
    /// granted to the window endpoint. A reserved, fail-closed value.
    const EXIT_NO_FRAMES: i32 = 81;

    /// Exit code when the event mailbox or a wait-set member could not be
    /// established. A reserved, fail-closed value: the app exits rather
    /// than degrade into a busy re-poll.
    const EXIT_NO_EVENTS: i32 = 82;

    /// Exit code when the desktop session refused the window create (no
    /// graphical session, or the channel refused the geometry). A
    /// reserved, fail-closed value.
    const EXIT_NO_WINDOW: i32 = 83;

    /// Exit code when a present was refused or a channel died (the
    /// session went away). A reserved, fail-closed value.
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

    /// The wait-set token of the shell-output stream member.
    const SHELL_TOKEN: u64 = 2;

    /// The wait-set token of the shell-child member.
    const CHILD_TOKEN: u64 = 3;

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
        let _ = tairix_rt::stderr(b"terminal: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
        code
    }

    /// Reap the exited hosted shell and, if it was admitted by `spawn` but
    /// then failed its own asynchronous image load, return the terse reason
    /// to report (fail loud); `None` for a clean or ordinary exit.
    ///
    /// The shell's exit becomes visible on both the output-stream member
    /// (end-of-stream, as its stdout/stderr write ends close) and the child
    /// member, and the wait-set may wake on either first — so both arms
    /// funnel through this one reap so a load-failure diagnosis can never be
    /// lost to whichever token happened to wake the loop. `shell_pid` is the
    /// kernel-minted PID, known non-negative here.
    fn reap_shell(shell_pid: i64) -> Option<&'static str> {
        let mut status = WaitStatus::Exited(0);
        let _ = tairix_rt::try_wait(
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            // The kernel-minted PID round-trips through the i32 wait ABI.
            {
                shell_pid as i32
            },
            &mut status,
        );
        shell_load_failure(status)
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

    /// Render the screen grid into `frame` (the shared window surface)
    /// and present the whole window.
    ///
    /// The full-window damage is deliberate: a shell write can scroll the
    /// whole grid, and the surface is one window — not a screen — so the
    /// copy is small.
    fn present_frame<S, T>(
        terminal: &Terminal<S>,
        theme: &tairix_theme::Theme,
        client: &mut WindowClient<T>,
        window: u64,
        frame: &mut [u8],
        mode: &DisplayMode,
    ) -> Result<(), Errno>
    where
        S: ShellSource,
        T: WindowTransport,
    {
        let viewport = tairix_geometry::Rect::new(0, 0, mode.width_px, mode.height_px);
        let surface = render(terminal, theme, viewport).ok_or(Errno::LengthOutOfRange)?;
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    #[allow(clippy::too_many_lines)] // One linear bring-up plus one event loop; splitting would obscure the teardown ordering.
    fn main() -> i32 {
        // --- The hosted shell: two pipes, then the spawn wiring the
        // child's standard streams onto their child-side ends.
        let Ok((shell_out_read, shell_out_write)) = tairix_rt::pipe_create() else {
            return fail(EXIT_NO_SHELL, "shell output pipe refused");
        };
        let Ok((shell_in_read, shell_in_write)) = tairix_rt::pipe_create() else {
            return fail(EXIT_NO_SHELL, "shell input pipe refused");
        };
        let attach = shell_wires(shell_in_read, shell_out_write);
        let term_env = alloc::format!("TERM={TERM}");
        let shell_pid = tairix_rt::spawn_attached(
            DEFAULT_SHELL.as_bytes(),
            &attach,
            &[b"elsh"],
            &[term_env.as_bytes()],
        );
        if shell_pid < 0 {
            return fail(EXIT_NO_SHELL, "shell spawn refused");
        }
        // Close the child-side ends this process no longer needs: the
        // spawn cloned them into the shell, and keeping them here would
        // mask the shell's exit (this process's own write end would keep
        // the output pipe's end-of-stream from ever arriving).
        let _ = tairix_rt::fs_close(shell_in_read);
        let _ = tairix_rt::fs_close(shell_out_write);

        // --- The screen model over the live pipe primitives.
        let source = PipeShellSource::new(
            |buf: &mut [u8]| tairix_rt::fs_read(shell_out_read, 0, buf).map_err(errno_from),
            |bytes: &[u8]| tairix_rt::fs_write(shell_in_write, 0, bytes).map_err(errno_from),
        );
        let Some(mut terminal) = Terminal::new(COLS, ROWS, source) else {
            return fail(EXIT_NO_SHELL, "screen grid refused");
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

        // --- The event mailbox and the wait-set the program parks on.
        // The mailbox id is unique by construction (the shared
        // `event_endpoint_for` naming rule: this task's never-reused
        // kernel id under a fixed tag) and never reserved; the bind is
        // refused otherwise.
        let Ok(origin) = tairix_rt::self_origin() else {
            return fail(EXIT_NO_EVENTS, "own identity unavailable");
        };
        let event_endpoint = event_endpoint_for(origin.pid());
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
        let members = [
            (WaitSourceKind::Port, event_endpoint, EVENT_TOKEN),
            (
                WaitSourceKind::Stream,
                u64::from(shell_out_read),
                SHELL_TOKEN,
            ),
            (
                WaitSourceKind::Child,
                #[allow(clippy::cast_sign_loss)] // `shell_pid >= 0` checked above; it is a PID.
                {
                    shell_pid as u64
                },
                CHILD_TOKEN,
            ),
        ];
        for (kind, id, token) in members {
            if tairix_rt::waitset_ctl(set, WaitSetOp::Add, kind, id, token) != 0 {
                return fail(EXIT_NO_EVENTS, "wait-set member refused");
            }
        }

        // --- Open the window and paint the first (blank) screen.
        let mut client = WindowClient::new(RtWindowTransport);
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) = client.create(
            grant as u64,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            "Terminal",
            false,
        ) else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        let themes = tairix_theme::ThemeRegistry::with_builtins();
        let theme = themes.active();
        if present_frame(&terminal, theme, &mut client, window, frames, &mode).is_err() {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The event loop: park on the wait-set and dispatch on the
        // woken member's token (never drain every source per wake — a
        // blocking receive on an idle source would wedge the loop). Each
        // member's readiness is a level peek, so work left undrained
        // re-reports on the next wait.
        loop {
            let mut token = 0u64;
            if tairix_rt::waitset_wait(set, u64::MAX, &mut token) != 0 {
                return fail(EXIT_CHANNEL_LOST, "wait-set lost");
            }
            match token {
                EVENT_TOKEN => {
                    match drain_events(&mut terminal, event_endpoint, server) {
                        EventOutcome::Continue => {}
                        EventOutcome::End => {
                            // The desktop asked, or the shell's stdin is
                            // gone: close the window and end cleanly. The
                            // keystroke pipe drops with this process, so
                            // the shell observes end-of-file and exits.
                            let _ = client.close(window);
                            return 0;
                        }
                        EventOutcome::ChannelLost => {
                            return fail(EXIT_CHANNEL_LOST, "event mailbox lost")
                        }
                    }
                }
                SHELL_TOKEN => match terminal.pump() {
                    Ok(_) => {
                        if present_frame(&terminal, theme, &mut client, window, frames, &mode)
                            .is_err()
                        {
                            return fail(EXIT_CHANNEL_LOST, "present refused");
                        }
                    }
                    // End-of-stream: the shell exited (a clean `exit`, it
                    // was killed, or — admitted by `spawn` but then unable
                    // to load its own image — it failed asynchronously).
                    // What it last wrote is already on screen; reap it and,
                    // if it never got off the ground, state why (fail loud)
                    // before ending.
                    Err(Errno::NotFound) => {
                        let reason = reap_shell(shell_pid);
                        let _ = client.close(window);
                        if let Some(reason) = reason {
                            return fail(
                                EXIT_NO_SHELL,
                                &alloc::format!("shell failed to launch: {reason}"),
                            );
                        }
                        return 0;
                    }
                    Err(_) => return fail(EXIT_CHANNEL_LOST, "shell channel lost"),
                },
                CHILD_TOKEN => {
                    // The shell exited: reap it (non-blocking — the
                    // readiness was a peek), drain and paint whatever
                    // output it left in the pipe, then end. A shell that
                    // `spawn` admitted but that then failed its own
                    // asynchronous image load exits with a reserved
                    // `LOAD_*` status (never a synchronous spawn refusal
                    // any longer), so the terminal states that reason
                    // fail-loud here — hosting the shell was its whole
                    // purpose — rather than closing silently.
                    let reason = reap_shell(shell_pid);
                    while terminal.pump().is_ok() {}
                    let _ = present_frame(&terminal, theme, &mut client, window, frames, &mode);
                    let _ = client.close(window);
                    if let Some(reason) = reason {
                        return fail(
                            EXIT_NO_SHELL,
                            &alloc::format!("shell failed to launch: {reason}"),
                        );
                    }
                    return 0;
                }
                // A token outside the registered members cannot occur (the
                // set holds exactly the three added above); re-park rather
                // than act on a value this program never minted.
                _ => {}
            }
        }
    }

    /// What the event-mailbox drain concluded.
    enum EventOutcome {
        /// Every pending event was applied; keep serving.
        Continue,
        /// The session asked the window to close, or the shell can no
        /// longer accept input: end the program cleanly.
        End,
        /// The mailbox itself failed: end fail-loud.
        ChannelLost,
    }

    /// Drain every queued window event (non-blocking — the wait-set wake
    /// said at least one is pending), applying key presses to the shell.
    ///
    /// A short frame or a sender other than the desktop session is
    /// dropped, never applied: the mailbox is open to any capable sender,
    /// so the kernel-attested origin is the authentication. A malformed
    /// frame from the authenticated session is likewise refused (never
    /// guessed at).
    fn drain_events<S: ShellSource>(
        terminal: &mut Terminal<S>,
        endpoint: u64,
        server: ProcId,
    ) -> EventOutcome {
        loop {
            let mut frame = [0u8; WindowEvent::WIRE_LEN];
            let mut sender = [0u8; ORIGIN_WIRE_LEN];
            match tairix_rt::ipc_recv(endpoint, &mut frame, &mut sender) {
                Ok(len) => {
                    if len != WindowEvent::WIRE_LEN {
                        continue;
                    }
                    let Ok(origin) = Origin::from_bytes(&sender) else {
                        continue;
                    };
                    if origin.proc_id() != server {
                        continue;
                    }
                    let Ok(event) = WindowEvent::from_bytes(&frame) else {
                        continue;
                    };
                    match event {
                        WindowEvent::Key { key, .. } => {
                            // The one shared layout-to-tty rule; a release
                            // encodes zero bytes and sends nothing.
                            let mut bytes = [0u8; MAX_KEY_BYTES];
                            let Ok(n) = encode_key_input(&key, &mut bytes) else {
                                continue;
                            };
                            if n > 0 && terminal.send(&bytes[..n]).is_err() {
                                // The shell can no longer accept input
                                // (it exited): the session is over.
                                return EventOutcome::End;
                            }
                        }
                        WindowEvent::CloseRequested { .. } => return EventOutcome::End,
                        // Focus changes and pointer events repaint nothing;
                        // the screen is shell-driven. A wheel likewise has
                        // nothing to move: the terminal renders the shell's
                        // live screen and keeps no scrollback, so there is no
                        // scrollable content a tick could reach. The terminal
                        // never requests a pick, so a pick conclusion is a
                        // session bug and is ignored (an unredeemed
                        // delegation is reclaimed by the kernel at exit).
                        //
                        // Minimized needs no action (the window is hidden and
                        // kept on the taskbar; the screen is redrawn from the
                        // shell on demand). Resized cannot reach this window:
                        // the terminal renders a fixed character grid and does
                        // not request resizable decoration, so the window
                        // manager offers it neither maximize nor a resize
                        // grabber and never sends it a new client size. Both
                        // are honest no-ops, not deferred work.
                        WindowEvent::Focus { .. }
                        | WindowEvent::Pointer { .. }
                        | WindowEvent::Scrolled { .. }
                        | WindowEvent::Minimized { .. }
                        | WindowEvent::Resized { .. }
                        | WindowEvent::FilePicked { .. }
                        | WindowEvent::PickCancelled { .. } => {}
                    }
                }
                Err(err) if errno_from(err) == Errno::WouldBlock => return EventOutcome::Continue,
                Err(_) => return EventOutcome::ChannelLost,
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
