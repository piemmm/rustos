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
    use tairix_font::BitmapFont;
    use tairix_keymap::{encode_key_input, MAX_KEY_BYTES};
    use tairix_rt::io::{Stderr, Write};
    use tairix_terminal::grid::MAX_DIMENSION;
    use tairix_terminal::render::render;
    use tairix_terminal::{
        shell_env, shell_load_failure, shell_wires, ShellSource, StreamShellSource, Terminal, COLS,
        ROWS, TERM, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_users::DEFAULT_SHELL;
    use tairix_window::{
        event_endpoint_for, Desktop, WindowClient, WindowTransport, EVENT_MAILBOX_CAPACITY,
    };

    /// Exit code when the shell could not be hosted (the pty or the spawn
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

    /// The wait-set token of the event-mailbox member.
    const EVENT_TOKEN: u64 = 1;

    /// The wait-set token of the shell-output stream member.
    const SHELL_TOKEN: u64 = 2;

    /// The wait-set token of the shell-child member.
    const CHILD_TOKEN: u64 = 3;

    /// The RGBA8888 window surface `width_px` × `height_px`, its stride the
    /// tightly-packed four-bytes-per-pixel row. One definition so the initial
    /// window and every resize build the surface identically (§2.2).
    fn mode_for(width_px: u32, height_px: u32) -> DisplayMode {
        DisplayMode {
            width_px,
            height_px,
            stride_bytes: width_px.saturating_mul(4),
            format: DisplayFormat::Rgba8888,
        }
    }

    /// The character grid `(cols, rows)` that fits a `width_px` × `height_px`
    /// client, from the shared monospace face's advance and line height (the
    /// same metrics [`WIN_WIDTH`]/[`WIN_HEIGHT`] and the renderer derive the
    /// grid from, so window sizing and rendering can never disagree). Floored
    /// so the grid never exceeds the surface (no clipped cell), at least
    /// `1`×`1`, and capped at [`MAX_DIMENSION`] so a huge window never asks
    /// for an unbounded grid (fail closed).
    fn grid_dims(width_px: u32, height_px: u32, font: BitmapFont) -> (u16, u16) {
        let advance = font.cell_width().max(1);
        let line_height = font.line_height().max(1);
        // The `clamp` upper bound is `MAX_DIMENSION` itself (a `u16`), so
        // the clamped value always fits; `unwrap_or(MAX_DIMENSION)` names a
        // fallback that is unreachable but still the correct value, rather
        // than lying with an `as` truncation.
        let cols = u16::try_from((width_px / advance).clamp(1, u32::from(MAX_DIMENSION)))
            .unwrap_or(MAX_DIMENSION);
        let rows = u16::try_from((height_px / line_height).clamp(1, u32::from(MAX_DIMENSION)))
            .unwrap_or(MAX_DIMENSION);
        (cols, rows)
    }

    /// Re-map the window `window` onto a fresh frame region shaped as
    /// `new_mode`, fail-closed. Returns the adopted region's `(base, len)` on
    /// success — the old region (`old_base` / `old_len`) already unmapped — or
    /// `None` when the region could not be allocated or the session refused the
    /// re-map, in which case the old region is left intact and still mapped so
    /// the current surface stays valid (never a crash or a blank window, §5.4).
    ///
    /// The fresh region is created and granted first and adopted only once
    /// [`WindowClient::resize`] succeeds; the old mapping is released only
    /// after adoption, and a refused resize releases the freshly-allocated
    /// region so nothing leaks.
    fn resize_frames(
        client: &mut WindowClient<RtWindowTransport>,
        window: u64,
        old_base: usize,
        old_len: usize,
        new_mode: &DisplayMode,
    ) -> Option<(usize, usize)> {
        let new_len = (new_mode.stride_bytes as usize)
            .checked_mul(new_mode.height_px as usize)?
            .checked_mul(FRAME_COUNT as usize)?;
        let mut region_id: u64 = 0;
        let new_base = tairix_rt::shm_create(new_len, &mut region_id);
        if new_base < 0 {
            return None;
        }
        let Ok(new_base) = usize::try_from(new_base) else {
            return None;
        };
        let grant = tairix_rt::shm_grant(region_id, WINDOW_ENDPOINT);
        if grant < 1 {
            let _ = tairix_rt::shm_unmap(new_base as u64, new_len);
            return None;
        }
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let accepted = client
            .resize(window, grant as u64, FRAME_COUNT, new_mode)
            .is_ok();
        if !accepted {
            let _ = tairix_rt::shm_unmap(new_base as u64, new_len);
            return None;
        }
        let _ = tairix_rt::shm_unmap(old_base as u64, old_len);
        Some((new_base, new_len))
    }

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
        let _ = writeln!(Stderr, "terminal: {reason}");
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
        font: BitmapFont,
    ) -> Result<(), Errno>
    where
        S: ShellSource,
        T: WindowTransport,
    {
        let viewport = tairix_geometry::Rect::new(0, 0, mode.width_px, mode.height_px);
        let surface = render(terminal, theme, viewport, font).ok_or(Errno::LengthOutOfRange)?;
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
        // --- The hosted shell: one pseudo-terminal, then the spawn wiring
        // the child's standard streams onto the slave end. The terminal
        // holds the master; the shell's fd 0/1/2 are the slave, a
        // console-class tty, so the shell runs its full interactive editor
        // (local echo, line editing, `Ctrl-C`/`Ctrl-Z`, `ONLCR`) exactly as
        // on the hardware console (`plans/PTY.md`). The pty is created at the
        // terminal's own grid geometry, so `terminal_size` reports it.
        let Ok((pty_master, pty_slave)) = tairix_rt::pty_create(ROWS, COLS) else {
            return fail(EXIT_NO_SHELL, "pty refused");
        };
        let attach = shell_wires(pty_slave);
        // Forward this terminal's own inherited environment (USER, HOME,
        // LOGNAME, PATH, LANG, ...) to the shell, exactly as the desktop
        // session forwards it to every app it launches: the hosted shell is
        // the logged-in user's shell, so its prompt and its children need the
        // same identity and locale the session runs under — otherwise the
        // prompt falls back to the anonymous "user@host" default. The one
        // variable this terminal owns is TERM, naming the emulator it
        // presents, so its own value replaces any inherited TERM (the shared
        // `shell_env` rule, host-tested). The environment is data and carries
        // no authority (§4, §5.4).
        let env_owned = shell_env(TERM, (0..tairix_rt::env_count()).filter_map(tairix_rt::env));
        let env: alloc::vec::Vec<&[u8]> = env_owned.iter().map(alloc::vec::Vec::as_slice).collect();
        let shell_pid =
            tairix_rt::spawn_attached(DEFAULT_SHELL.as_bytes(), &attach, &[b"elsh"], &env);
        if shell_pid < 0 {
            return fail(EXIT_NO_SHELL, "shell spawn refused");
        }
        // Close this process's own slave end: the spawn cloned it into the
        // shell (behind its fd 0/1/2), and keeping it here would mask the
        // shell's exit — a live slave end would keep the master read's
        // end-of-stream from ever arriving.
        let _ = tairix_rt::fs_close(pty_slave);

        // --- The screen model over the live pty master. Reads drain the
        // shell's cooked output; writes feed keystrokes through the input
        // discipline.
        let source = StreamShellSource::new(
            |buf: &mut [u8]| tairix_rt::fs_read(pty_master, 0, buf).map_err(errno_from),
            |bytes: &[u8]| tairix_rt::fs_write(pty_master, 0, bytes).map_err(errno_from),
        );
        let Some(mut terminal) = Terminal::new(COLS, ROWS, source) else {
            return fail(EXIT_NO_SHELL, "screen grid refused");
        };

        // --- Open the window and paint the first frame.
        let mut client = WindowClient::new(RtWindowTransport);
        // The desktop this window will be shown on: the screen, the density,
        // and the appearance, before anything is sized or painted, so the
        // first frame is right rather than a guess corrected once the user
        // has seen it.
        let info = match client.desktop() {
            Ok(info) => info,
            Err(err) => {
                let _ = writeln!(Stderr, "terminal: desktop query refused: {err}");
                return EXIT_NO_WINDOW;
            }
        };
        let mut desktop = match Desktop::new(info) {
            Ok(desktop) => desktop,
            Err(err) => {
                let _ = writeln!(Stderr, "terminal: cannot draw this desktop: {err}");
                return EXIT_NO_WINDOW;
            }
        };
        let (w, h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mut mode = mode_for(w, h);
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
        let mut frames = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) };
        let mut region_base = base;
        let mut region_len = total;

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
            || tairix_rt::port_bind(
                event_endpoint,
                WindowEvent::WIRE_LEN,
                EVENT_MAILBOX_CAPACITY,
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
        let members = [
            (WaitSourceKind::Port, event_endpoint, EVENT_TOKEN),
            (WaitSourceKind::Stream, u64::from(pty_master), SHELL_TOKEN),
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

        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) = client.create(
            grant as u64,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            "Terminal",
            true,
        ) else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        let mut themes = tairix_theme::ThemeRegistry::with_builtins();
        themes.set_appearance(desktop.appearance());
        let mut theme = themes.active();
        let mut font = BitmapFont::monospace(
            desktop
                .scale()
                .scale_length(tairix_font::atlas::CELL_HEIGHT),
        );
        if present_frame(&terminal, theme, &mut client, window, frames, &mode, font).is_err() {
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
                    match drain_events(&mut terminal, &mut desktop, event_endpoint, server) {
                        EventOutcome::Continue => {}
                        EventOutcome::Resized {
                            width_px,
                            height_px,
                        } => {
                            // Re-map the frame region at the new client size,
                            // reshape the grid, and tell the shell (via the pty
                            // window size) so its prompt and any full-screen
                            // program re-lay-out. A refused or unallocatable
                            // re-map keeps the current window rather than
                            // failing the app (fail closed): the grid and pty
                            // size are only updated once the new region is
                            // adopted, so the screen never claims a geometry
                            // the surface cannot hold.
                            let new_mode = mode_for(width_px, height_px);
                            if let Some((new_base, new_len)) = resize_frames(
                                &mut client,
                                window,
                                region_base,
                                region_len,
                                &new_mode,
                            ) {
                                region_base = new_base;
                                region_len = new_len;
                                mode = new_mode;
                                // SAFETY: `resize_frames` mapped `region_len`
                                // zeroed R/W bytes at `region_base` and the
                                // session adopted them; the old region is now
                                // unmapped. Same invariants as the initial
                                // mapping — nothing else aliases it, and the
                                // present below serialises access.
                                frames = unsafe {
                                    core::slice::from_raw_parts_mut(
                                        region_base as *mut u8,
                                        region_len,
                                    )
                                };
                                let (cols, rows) = grid_dims(width_px, height_px, font);
                                let _ = terminal.resize(cols, rows);
                                let _ = tairix_rt::pty_set_size(pty_master, rows, cols);
                                if present_frame(
                                    &terminal,
                                    theme,
                                    &mut client,
                                    window,
                                    frames,
                                    &mode,
                                    font,
                                )
                                .is_err()
                                {
                                    return fail(EXIT_CHANNEL_LOST, "present refused");
                                }
                            }
                        }
                        EventOutcome::DesktopChanged => {
                            // The scale and/or appearance changed: re-apply
                            // the theme, re-derive the monospace font from
                            // the new scale, reshape the grid to match (the
                            // pty follows), and repaint. `desktop` itself was
                            // already updated inside `drain_events`.
                            themes.set_appearance(desktop.appearance());
                            theme = themes.active();
                            font = BitmapFont::monospace(
                                desktop
                                    .scale()
                                    .scale_length(tairix_font::atlas::CELL_HEIGHT),
                            );
                            let (cols, rows) = grid_dims(mode.width_px, mode.height_px, font);
                            let _ = terminal.resize(cols, rows);
                            let _ = tairix_rt::pty_set_size(pty_master, rows, cols);
                            if present_frame(
                                &terminal,
                                theme,
                                &mut client,
                                window,
                                frames,
                                &mode,
                                font,
                            )
                            .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                        }
                        EventOutcome::Redraw => {
                            // The session reclaimed the retained pixels; the
                            // grid is still live, so re-render it in full.
                            if present_frame(
                                &terminal,
                                theme,
                                &mut client,
                                window,
                                frames,
                                &mode,
                                font,
                            )
                            .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                        }
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
                        if present_frame(&terminal, theme, &mut client, window, frames, &mode, font)
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
                    let _ =
                        present_frame(&terminal, theme, &mut client, window, frames, &mode, font);
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
        /// The window manager resized the window to this new client size (a
        /// drag-resize that settled, or a maximize/restore); the caller
        /// re-maps its frame region, reshapes the grid, and updates the pty
        /// window size. Any events queued behind it re-report on the next
        /// wake (the port readiness is level-triggered).
        Resized {
            /// New client width in pixels.
            width_px: u32,
            /// New client height in pixels.
            height_px: u32,
        },
        /// The desktop changed (screen size, scale, or appearance); already
        /// adopted by [`Desktop::apply`] before this is returned.
        DesktopChanged,
        /// The session released this window's retained pixels to reclaim
        /// memory and needs them presented again. The terminal decodes its
        /// mailbox itself, so no library re-present happens on its behalf:
        /// it repaints the live screen in full. Nothing is lost — the screen
        /// is rendered from the shell's grid, which the terminal still holds.
        Redraw,
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
        desktop: &mut Desktop,
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
                    // Every delivered event is offered to the desktop first,
                    // so a scale or appearance change is adopted whether or
                    // not this app otherwise reacts to the event that
                    // carried it. A real change ends this drain (the caller
                    // relays out and repaints); a refusal is stated and the
                    // last good desktop stands.
                    match desktop.apply(&event) {
                        Ok(true) => return EventOutcome::DesktopChanged,
                        Ok(false) => {}
                        Err(err) => {
                            let _ =
                                writeln!(Stderr, "terminal: could not apply desktop change: {err}");
                        }
                    }
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
                        // The session dropped the window's pixels under
                        // memory pressure: repaint the whole screen. Returning
                        // hands the present to the caller (which owns the
                        // frame region and the window client); events queued
                        // behind it re-report on the next wake.
                        WindowEvent::RedrawRequested { .. } => return EventOutcome::Redraw,
                        // The window manager resized the window (a settled
                        // drag-resize, or a maximize/restore): hand the new
                        // client size back to the caller, which re-maps the
                        // frame region, reshapes the grid, and updates the pty
                        // window size. Returning here leaves any events queued
                        // behind it for the next wake (level-triggered peek).
                        WindowEvent::Resized {
                            width_px,
                            height_px,
                            ..
                        } => {
                            return EventOutcome::Resized {
                                width_px,
                                height_px,
                            }
                        }
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
                        // shell on demand). A desktop change was already
                        // adopted above (or, on refusal, stated and left the
                        // last good state standing); either way there is
                        // nothing further to do with the event itself. These
                        // are honest no-ops, not deferred work.
                        WindowEvent::Focus { .. }
                        | WindowEvent::Pointer { .. }
                        | WindowEvent::Scrolled { .. }
                        | WindowEvent::Minimized { .. }
                        | WindowEvent::FilePicked { .. }
                        | WindowEvent::PickCancelled { .. }
                        | WindowEvent::DesktopChanged { .. } => {}
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
