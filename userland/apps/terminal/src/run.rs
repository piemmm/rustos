//! The `terminal.app` bundle's `Run` entry point (`plans/APPWIN.md` AW4,
//! `plans/GUI-TERMINAL.md`): the windowed terminal emulator hosting the
//! user's shell over the desktop session's window channel.
//!
//! # What the program wires (and what stays in the libraries)
//!
//! Everything with behaviour worth testing lives in host-tested crates —
//! the screen model and its `lib/vt`-consuming parser (`tairix_terminal`),
//! the themed cell renderer (`tairix_terminal::render`), the user's profile
//! and its document, the screen-effect pipeline, the right-click menu, the
//! settings sheet, the spawned shell's pipe wiring, and the window channel's
//! client half (`tairix_window`). This binary only composes them over the
//! live syscalls:
//!
//! * Two ends of one kernel pseudo-terminal: keystrokes flow to the shell's
//!   standard input and its cooked output flows back to the screen. The
//!   child-side end is closed here after the spawn, so the shell observes
//!   end-of-file the moment this terminal exits, and the terminal observes
//!   end-of-stream the moment the shell does.
//! * One `shm_create`d frame region, granted to the reserved window
//!   endpoint (the zero-copy surface the session maps once at create).
//! * One wait-set the program **parks** on — never a poll loop — with
//!   three members: its `port_bind`-bound event mailbox (the desktop's
//!   `Focus`/`Key`/`Pointer`/`CloseRequested` deliveries, each accepted
//!   only from the session identity the squat-protected create reply
//!   named), the shell-output stream, and the shell child itself. When an
//!   animated screen effect is in force the park carries a one-shot frame
//!   deadline, so the animation costs one timed wake per frame and nothing
//!   at all when it is switched off.
//! * The user's own profile document, read at start-up under this process's
//!   own identity and rewritten whenever a setting changes.
//!
//! A key press is either a terminal command (a menu accelerator, or input
//! the open menu or settings sheet owns) or shell input encoded through the
//! one shared `lib/keymap` rule. Every bring-up refusal exits fail-loud with
//! a reserved code and a stated reason on `stderr`.
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
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, WaitStatus, ORIGIN_WIRE_LEN,
    };
    use tairix_font::BitmapFont;
    use tairix_geometry::{Point, Rect, Scale};
    use tairix_input::InputEvent;
    use tairix_keymap::{encode_key_input, MAX_KEY_BYTES};
    use tairix_rt::io::{Stderr, Write};
    use tairix_terminal::effects::{Afterglow, Phase};
    use tairix_terminal::layout::{fit_font_size, grid_dims, window_size};
    use tairix_terminal::menu::{Command, ContextMenu, MenuOutcome};
    use tairix_terminal::profile::{
        parse as parse_profile, render as render_profile, user_profile_path, Profile,
        MAX_PROFILE_LEN,
    };
    use tairix_terminal::render::render;
    use tairix_terminal::scheme::Painted;
    use tairix_terminal::settings::{Settings, SheetOutcome};
    use tairix_terminal::{
        shell_env, shell_load_failure, shell_wires, ShellSource, StreamShellSource, Terminal, TERM,
        WIN_RESIZABLE,
    };
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_users::DEFAULT_SHELL;
    use tairix_window::{
        event_endpoint_for, key_input_event, pointer_input_events, Desktop, WindowClient,
        WindowTransport, EVENT_MAILBOX_CAPACITY,
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

    /// The wait-set token of the memory-pressure member: the kernel wakes the
    /// park when the machine's pressure band changes, so the glyph cache is
    /// trimmed as memory tightens instead of being held until something else
    /// is starved.
    const PRESSURE_TOKEN: u64 = 4;

    /// How long the program parks between frames of an animated screen
    /// effect: fifty milliseconds, twenty frames a second.
    ///
    /// Slow enough that an idle terminal with the effects on costs a
    /// twentieth of a repaint's work per second rather than a core, and fast
    /// enough that a travelling wobble and an analogue noise floor read as
    /// motion rather than as a stutter. The park is a one-shot deadline, so
    /// a terminal with no animated effect never wakes at all.
    const FRAME_INTERVAL_NS: u64 = 50_000_000;

    /// The RGBA8888 window surface `width_px` × `height_px`, its stride the
    /// tightly-packed four-bytes-per-pixel row. One definition so the initial
    /// window and every resize build the surface identically.
    fn mode_for(width_px: u32, height_px: u32) -> DisplayMode {
        DisplayMode {
            width_px,
            height_px,
            stride_bytes: width_px.saturating_mul(4),
            format: DisplayFormat::Rgba8888,
        }
    }

    /// Re-map the window `window` onto a fresh frame region shaped as
    /// `new_mode`, fail-closed. Returns the adopted region's `(base, len)` on
    /// success — the old region (`old_base` / `old_len`) already unmapped —
    /// or `None` when the region could not be allocated or the session
    /// refused the re-map, in which case the old region is left intact and
    /// still mapped so the current surface stays valid (never a crash or a
    /// blank window).
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

    /// Report a non-fatal refusal on `stderr` and carry on.
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "terminal: {reason}");
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

    /// Read the whole file at `path` under this process's own identity,
    /// stopping one chunk past `cap` so no document can make the terminal
    /// slurp an arbitrary number of bytes.
    fn read_file(path: &str, cap: usize) -> Result<Vec<u8>, Errno> {
        let ret = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        if ret < 0 {
            return Err(Errno::from_syscall(ret));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // `ret >= 0` checked above; it is a descriptor number.
        let fd = ret as u32;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 1024];
        let outcome = loop {
            if bytes.len() > cap {
                break Ok(bytes);
            }
            match tairix_rt::fs_read(fd, bytes.len() as u64, &mut chunk) {
                Ok(0) => break Ok(bytes),
                Ok(read) => match chunk.get(..read) {
                    Some(slice) => bytes.extend_from_slice(slice),
                    None => break Err(Errno::OutOfRange),
                },
                Err(err) => break Err(Errno::from_syscall(err)),
            }
        };
        let _ = tairix_rt::fs_close(fd);
        outcome
    }

    /// The user's own profile document path, or `None` when the session
    /// inherited no home (in which case nothing is read and nothing is
    /// stored: a terminal with no home runs on the defaults).
    fn profile_path() -> Option<String> {
        let home = tairix_rt::env_var(b"HOME")?;
        let home = core::str::from_utf8(home).ok()?;
        user_profile_path(home)
    }

    /// The profile in force for this user.
    ///
    /// An **absent** document is the ordinary state of a fresh account and
    /// silently yields [`Profile::default`]. Anything else that stops the
    /// document being used — no home, a refused read, bytes that are not
    /// UTF-8, a document the shared parser refuses — also yields the
    /// defaults, but says so on `stderr` rather than running on settings the
    /// user cannot see the reason for.
    fn load_profile(path: Option<&str>) -> Profile {
        let Some(path) = path else {
            return Profile::default();
        };
        let bytes = match read_file(path, MAX_PROFILE_LEN) {
            Ok(bytes) => bytes,
            Err(Errno::NotFound) => return Profile::default(),
            Err(err) => {
                report(&alloc::format!(
                    "{path}: read refused ({err:?}); using the default profile"
                ));
                return Profile::default();
            }
        };
        if bytes.len() > MAX_PROFILE_LEN {
            report(&alloc::format!(
                "{path}: longer than any valid profile document; using the default profile"
            ));
            return Profile::default();
        }
        let Ok(text) = core::str::from_utf8(&bytes) else {
            report(&alloc::format!(
                "{path}: not valid UTF-8; using the default profile"
            ));
            return Profile::default();
        };
        match parse_profile(text) {
            Ok(mut profile) => {
                profile.clamp();
                profile
            }
            Err(err) => {
                report(&alloc::format!("{path}: {err}; using the default profile"));
                Profile::default()
            }
        }
    }

    /// Replace the user's profile document with `profile`.
    ///
    /// A refused write is reported and otherwise harmless: the terminal
    /// keeps showing what the user just chose, and the next session simply
    /// opens on what is still on disk. The parent directory is created
    /// first — `~/Settings/Terminal` does not exist until the first change.
    fn persist_profile(path: Option<&str>, profile: &Profile) {
        let Some(path) = path else {
            return;
        };
        if let Some((parent, _)) = path.rsplit_once('/') {
            if !parent.is_empty() {
                let ret = tairix_rt::fs_mkdir(parent.as_bytes());
                if ret < 0 && Errno::from_syscall(ret) != Errno::AlreadyExists {
                    report(&alloc::format!(
                        "{parent}: settings directory refused ({:?}); the profile was not saved",
                        Errno::from_syscall(ret)
                    ));
                    return;
                }
            }
        }
        let document = render_profile(profile);
        let file = match tairix_rt::create(path.as_bytes()) {
            Ok(file) => file,
            Err(err) => {
                report(&alloc::format!(
                    "{path}: refused ({:?}); the profile was not saved",
                    Errno::from_syscall(err)
                ));
                return;
            }
        };
        match file.write_at(0, document.as_bytes()) {
            Ok(written) if written == document.len() => {}
            Ok(_) => report(&alloc::format!(
                "{path}: the volume stopped accepting bytes; the profile was not saved"
            )),
            Err(err) => report(&alloc::format!(
                "{path}: write refused ({:?}); the profile was not saved",
                Errno::from_syscall(err)
            )),
        }
    }

    /// What is drawn over the terminal screen, if anything.
    ///
    /// At most one overlay exists at a time and it is modal: while it is
    /// open every pointer and key event routes to it, so a click meant for
    /// the menu can never also reach the shell.
    enum Overlay {
        /// Nothing over the screen; input goes to the shell.
        None,
        /// The right-click context menu.
        Menu(ContextMenu),
        /// The settings sheet.
        Sheet(Settings),
    }

    /// Everything the program holds about how the screen currently looks.
    ///
    /// Derived from the [`Profile`] and the desktop, and re-derived whenever
    /// either changes, so the renderer, the window geometry, and the effect
    /// pipeline can never disagree about what is in force.
    struct Look {
        /// The face the screen is drawn in.
        font: BitmapFont,
        /// The colours the screen is painted with.
        painted: Painted,
        /// The animation step the effects are drawn at.
        phase: Phase,
        /// The persistence state the phosphor effect carries between frames.
        afterglow: Afterglow,
    }

    impl Look {
        /// The look `profile` implies on `desktop` under `theme`.
        fn resolve(profile: &Profile, theme: &Theme, desktop: &Desktop) -> Self {
            let screen = (desktop.screen_width_px(), desktop.screen_height_px());
            let size = fit_font_size(profile.font_size_px, screen, theme, desktop.scale());
            Self {
                font: BitmapFont::monospace(desktop.scale().scale_length(u32::from(size))),
                painted: Painted::resolve(
                    profile.scheme,
                    &profile.custom,
                    theme,
                    profile.effects.background_alpha(),
                ),
                phase: Phase::default(),
                afterglow: Afterglow::new(),
            }
        }

        /// Adopt a changed profile or desktop, forgetting the afterglow so a
        /// trail of the old screen cannot ghost over the new one.
        fn refresh(&mut self, profile: &Profile, theme: &Theme, desktop: &Desktop) {
            let phase = self.phase;
            *self = Self::resolve(profile, theme, desktop);
            self.phase = phase;
        }
    }

    /// Render the screen (and any overlay) into `frame` and present the whole
    /// window.
    ///
    /// The full-window damage is deliberate: a shell write can scroll the
    /// whole grid, and the surface is one window — not a screen — so the
    /// copy is small.
    fn present_frame<S, T>(
        terminal: &Terminal<S>,
        profile: &Profile,
        look: &mut Look,
        overlay: &Overlay,
        theme: &Theme,
        scale: Scale,
        client: &mut WindowClient<T>,
        window: u64,
        frame: &mut [u8],
        mode: &DisplayMode,
    ) -> Result<(), Errno>
    where
        S: ShellSource,
        T: WindowTransport,
    {
        let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);
        let mut surface =
            render(terminal, &look.painted, viewport, look.font).ok_or(Errno::LengthOutOfRange)?;
        profile.effects.apply(
            &mut surface,
            &mut look.afterglow,
            look.phase,
            scale.percent(),
        );
        // The overlay is drawn after the effects: a settings sheet that
        // wobbled with the screen behind it would be unusable, and its
        // controls must read exactly as they do everywhere else.
        match overlay {
            Overlay::None => {}
            Overlay::Menu(menu) => menu.render(&mut surface, viewport, scale, theme, look.font),
            Overlay::Sheet(sheet) => {
                sheet.render(&mut surface, viewport, scale, theme, look.font);
            }
        }
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

    /// Tell the session how far to blur what is behind this window.
    ///
    /// A refusal is reported and otherwise harmless: the window simply keeps
    /// the blur it already had, which is never worse than a sharp backdrop.
    fn apply_blur<T: WindowTransport>(
        client: &mut WindowClient<T>,
        window: u64,
        profile: &Profile,
    ) {
        if let Err(err) = client.set_backdrop_blur(window, profile.effects.blur_radius_px()) {
            report(&alloc::format!("backdrop blur refused: {err}"));
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    #[allow(clippy::too_many_lines)] // One linear bring-up plus one event loop; splitting would obscure the teardown ordering.
    fn main() -> i32 {
        // --- The user's own profile, before anything is sized or painted, so
        // the first frame is what they chose rather than a default corrected
        // once they have seen it.
        let store = profile_path();
        let mut profile = load_profile(store.as_deref());

        // --- The desktop this window will be shown on: the screen, the
        // density, and the appearance.
        let mut client = WindowClient::new(RtWindowTransport);
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
        let mut themes = ThemeRegistry::with_builtins();
        themes.set_appearance(desktop.appearance());
        let mut theme = themes.active();
        let mut look = Look::resolve(&profile, theme, &desktop);
        let screen = (desktop.screen_width_px(), desktop.screen_height_px());
        let (w, h) = window_size(look.font, screen, theme, desktop.scale());
        let (cols, rows) = grid_dims(w, h, look.font);

        // --- The hosted shell: one pseudo-terminal, then the spawn wiring
        // the child's standard streams onto the slave end. The terminal
        // holds the master; the shell's fd 0/1/2 are the slave, a
        // console-class tty, so the shell runs its full interactive editor
        // (local echo, line editing, `Ctrl-C`/`Ctrl-Z`, `ONLCR`) exactly as
        // on the hardware console (`plans/PTY.md`). The pty is created at the
        // grid the window will actually show, so `terminal_size` reports it.
        let Ok((pty_master, pty_slave)) = tairix_rt::pty_create(rows, cols) else {
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
        // no authority.
        let env_owned = shell_env(TERM, (0..tairix_rt::env_count()).filter_map(tairix_rt::env));
        let env: Vec<&[u8]> = env_owned.iter().map(Vec::as_slice).collect();
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
        let Some(mut terminal) = Terminal::new(cols, rows, source) else {
            return fail(EXIT_NO_SHELL, "screen grid refused");
        };

        // --- Open the window and paint the first frame.
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
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            return fail(EXIT_NO_EVENTS, "memory-pressure wake refused");
        }

        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) = client.create(
            grant as u64,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            "Terminal",
            WIN_RESIZABLE,
        ) else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        apply_blur(&mut client, window, &profile);
        let mut overlay = Overlay::None;
        if present_frame(
            &terminal,
            &profile,
            &mut look,
            &overlay,
            theme,
            desktop.scale(),
            &mut client,
            window,
            frames,
            &mode,
        )
        .is_err()
        {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The event loop: park on the wait-set and dispatch on the
        // woken member's token (never drain every source per wake — a
        // blocking receive on an idle source would wedge the loop). Each
        // member's readiness is a level peek, so work left undrained
        // re-reports on the next wait. The park carries a frame deadline
        // only while an animated effect is in force.
        loop {
            let animated = profile.effects.is_animated(desktop.scale().percent());
            let timeout = if animated {
                FRAME_INTERVAL_NS
            } else {
                u64::MAX
            };
            let mut token = 0u64;
            let waited = tairix_rt::waitset_wait(set, timeout, &mut token);
            if waited != 0 {
                if errno_from(waited) == Errno::TimedOut {
                    // The frame deadline elapsed: advance the animation and
                    // repaint. Nothing else changed.
                    look.phase = look.phase.advance();
                    if present_frame(
                        &terminal,
                        &profile,
                        &mut look,
                        &overlay,
                        theme,
                        desktop.scale(),
                        &mut client,
                        window,
                        frames,
                        &mode,
                    )
                    .is_err()
                    {
                        return fail(EXIT_CHANNEL_LOST, "present refused");
                    }
                    continue;
                }
                return fail(EXIT_CHANNEL_LOST, "wait-set lost");
            }
            match token {
                EVENT_TOKEN => {
                    let outcome = drain_events(
                        &mut terminal,
                        &mut profile,
                        &mut overlay,
                        &mut desktop,
                        theme,
                        look.font,
                        Rect::new(0, 0, mode.width_px, mode.height_px),
                        event_endpoint,
                        server,
                    );
                    match outcome {
                        EventOutcome::Continue => {}
                        EventOutcome::Repaint => {
                            if present_frame(
                                &terminal,
                                &profile,
                                &mut look,
                                &overlay,
                                theme,
                                desktop.scale(),
                                &mut client,
                                window,
                                frames,
                                &mode,
                            )
                            .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                        }
                        EventOutcome::ProfileChanged => {
                            // The user changed a setting: re-derive the
                            // colours and the face, tell the session how far
                            // to blur behind the window, reshape the grid to
                            // the new cell size (the pty follows), store the
                            // change, and repaint.
                            look.refresh(&profile, theme, &desktop);
                            apply_blur(&mut client, window, &profile);
                            let (cols, rows) = grid_dims(mode.width_px, mode.height_px, look.font);
                            let _ = terminal.resize(cols, rows);
                            let _ = tairix_rt::pty_set_size(pty_master, rows, cols);
                            persist_profile(store.as_deref(), &profile);
                            if present_frame(
                                &terminal,
                                &profile,
                                &mut look,
                                &overlay,
                                theme,
                                desktop.scale(),
                                &mut client,
                                window,
                                frames,
                                &mode,
                            )
                            .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                        }
                        EventOutcome::Resized {
                            width_px,
                            height_px,
                        } => {
                            // Re-map the frame region at the new client size,
                            // reshape the grid, and tell the shell (via the pty
                            // window size) so its prompt and any full-screen
                            // program re-lay-out. A refused or unallocatable
                            // re-map keeps the current window rather than
                            // failing the app: the grid and pty size are only
                            // updated once the new region is adopted, so the
                            // screen never claims a geometry the surface cannot
                            // hold.
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
                                let (cols, rows) = grid_dims(width_px, height_px, look.font);
                                let _ = terminal.resize(cols, rows);
                                let _ = tairix_rt::pty_set_size(pty_master, rows, cols);
                                // The afterglow is the shape of the old
                                // screen; a resized one must not ghost it.
                                look.afterglow.clear();
                                if present_frame(
                                    &terminal,
                                    &profile,
                                    &mut look,
                                    &overlay,
                                    theme,
                                    desktop.scale(),
                                    &mut client,
                                    window,
                                    frames,
                                    &mode,
                                )
                                .is_err()
                                {
                                    return fail(EXIT_CHANNEL_LOST, "present refused");
                                }
                            }
                        }
                        EventOutcome::DesktopChanged => {
                            // The scale and/or appearance changed: re-apply
                            // the theme, re-derive the look from the new
                            // scale, reshape the grid to match (the pty
                            // follows), and repaint. `desktop` itself was
                            // already updated inside `drain_events`.
                            themes.set_appearance(desktop.appearance());
                            theme = themes.active();
                            look.refresh(&profile, theme, &desktop);
                            let (cols, rows) = grid_dims(mode.width_px, mode.height_px, look.font);
                            let _ = terminal.resize(cols, rows);
                            let _ = tairix_rt::pty_set_size(pty_master, rows, cols);
                            if present_frame(
                                &terminal,
                                &profile,
                                &mut look,
                                &overlay,
                                theme,
                                desktop.scale(),
                                &mut client,
                                window,
                                frames,
                                &mode,
                            )
                            .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                        }
                        EventOutcome::End => {
                            // The desktop asked, the user chose Close, or the
                            // shell's stdin is gone: close the window and end
                            // cleanly. The pty master drops with this process,
                            // so the shell observes end-of-file and exits.
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
                        if present_frame(
                            &terminal,
                            &profile,
                            &mut look,
                            &overlay,
                            theme,
                            desktop.scale(),
                            &mut client,
                            window,
                            frames,
                            &mode,
                        )
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
                    let _ = present_frame(
                        &terminal,
                        &profile,
                        &mut look,
                        &overlay,
                        theme,
                        desktop.scale(),
                        &mut client,
                        window,
                        frames,
                        &mode,
                    );
                    let _ = client.close(window);
                    if let Some(reason) = reason {
                        return fail(
                            EXIT_NO_SHELL,
                            &alloc::format!("shell failed to launch: {reason}"),
                        );
                    }
                    return 0;
                }
                PRESSURE_TOKEN if tairix_procinfo::pressure::refresh() => {
                    tairix_font::trim_glyph_cache();
                    // The phosphor trail is a whole screen of per-pixel state
                    // that only matters while that effect is on, so it gives
                    // first under pressure; the next frame starts a new trail.
                    look.afterglow.clear();
                }
                // A band that did not move needs no trim, and a token outside
                // the registered members cannot occur (the set holds exactly
                // the four added above); either way, re-park rather than act
                // on a value this program never minted.
                _ => {}
            }
        }
    }

    /// What the event-mailbox drain concluded.
    enum EventOutcome {
        /// Every pending event was applied and nothing on screen changed.
        Continue,
        /// Something on screen changed; repaint and present.
        Repaint,
        /// The user changed a setting: re-derive the look, re-apply the
        /// backdrop blur, reshape the grid, store the profile, and repaint.
        ProfileChanged,
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
        /// The desktop asked the window to close, the user chose *Close*, or
        /// the shell can no longer accept input: end the program cleanly.
        End,
        /// The mailbox itself failed: end fail-loud.
        ChannelLost,
    }

    /// Carry out `command`, reporting what the caller must now do.
    fn run_command<S: ShellSource>(
        command: Command,
        terminal: &mut Terminal<S>,
        profile: &mut Profile,
        overlay: &mut Overlay,
    ) -> EventOutcome {
        match command {
            Command::Settings => {
                *overlay = Overlay::Sheet(Settings::new(profile));
                EventOutcome::Repaint
            }
            Command::Larger => {
                profile.enlarge();
                EventOutcome::ProfileChanged
            }
            Command::Smaller => {
                profile.reduce();
                EventOutcome::ProfileChanged
            }
            Command::ActualSize => {
                profile.font_size_px = Profile::default().font_size_px;
                EventOutcome::ProfileChanged
            }
            Command::Clear => {
                terminal.clear();
                EventOutcome::Repaint
            }
            Command::Close => EventOutcome::End,
        }
    }

    /// Route one pointer event, which the open overlay owns if there is one.
    ///
    /// With no overlay open, a secondary press opens the context menu at the
    /// pressed point and every other pointer event is a no-op: the screen is
    /// shell-driven and the emulator keeps no scrollback for a wheel to move.
    #[allow(clippy::too_many_arguments)] // The routing needs the whole drawing context to hit-test the overlay.
    fn route_pointer(
        overlay: &mut Overlay,
        profile: &mut Profile,
        action: PointerAction,
        at: Point,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> PointerRouting {
        match overlay {
            Overlay::None => {
                if action == PointerAction::Pressed(tairix_abi::input::PointerButtonCode::Secondary)
                {
                    *overlay = Overlay::Menu(ContextMenu::open(at));
                    return PointerRouting::Repaint;
                }
                PointerRouting::Nothing
            }
            Overlay::Menu(menu) => {
                let mut routing = PointerRouting::Nothing;
                for event in pointer_input_events(action, at) {
                    match menu.on_pointer(&event, viewport, scale, theme, font) {
                        MenuOutcome::Ignored => {}
                        MenuOutcome::Changed => routing = PointerRouting::Repaint,
                        MenuOutcome::Dismissed => {
                            *overlay = Overlay::None;
                            return PointerRouting::Repaint;
                        }
                        MenuOutcome::Chose(command) => {
                            *overlay = Overlay::None;
                            return PointerRouting::Chose(command);
                        }
                    }
                }
                routing
            }
            Overlay::Sheet(sheet) => {
                let mut routing = PointerRouting::Nothing;
                for event in pointer_input_events(action, at) {
                    match sheet.on_pointer(&event, viewport, scale, theme, font) {
                        SheetOutcome::Ignored => {}
                        SheetOutcome::Changed => routing = PointerRouting::Repaint,
                        SheetOutcome::Edited => {
                            *profile = *sheet.profile();
                            routing = PointerRouting::Edited;
                        }
                        SheetOutcome::Dismissed => {
                            *profile = *sheet.profile();
                            return PointerRouting::Close;
                        }
                    }
                }
                routing
            }
        }
    }

    /// What routing a pointer event concluded.
    enum PointerRouting {
        /// Nothing to do.
        Nothing,
        /// Repaint the window.
        Repaint,
        /// The settings sheet edited the profile.
        Edited,
        /// A menu row chose this command.
        Chose(Command),
        /// The settings sheet asked to close.
        Close,
    }

    /// Drain every queued window event (non-blocking — the wait-set wake
    /// said at least one is pending).
    ///
    /// A short frame or a sender other than the desktop session is
    /// dropped, never applied: the mailbox is open to any capable sender,
    /// so the kernel-attested origin is the authentication. A malformed
    /// frame from the authenticated session is likewise refused (never
    /// guessed at).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One dispatch over the whole window vocabulary; splitting it would hide the routing order.
    fn drain_events<S: ShellSource>(
        terminal: &mut Terminal<S>,
        profile: &mut Profile,
        overlay: &mut Overlay,
        desktop: &mut Desktop,
        theme: &Theme,
        font: BitmapFont,
        viewport: Rect,
        endpoint: u64,
        server: ProcId,
    ) -> EventOutcome {
        let scale = desktop.scale();
        let mut repaint = false;
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
                            match route_key(
                                terminal, profile, overlay, key, scale, theme, font, viewport,
                            ) {
                                KeyRouting::Nothing => {}
                                KeyRouting::Repaint => repaint = true,
                                KeyRouting::Edited => return EventOutcome::ProfileChanged,
                                KeyRouting::Command(command) => {
                                    return finish(
                                        run_command(command, terminal, profile, overlay),
                                        repaint,
                                    )
                                }
                                KeyRouting::CloseSheet => {
                                    *overlay = Overlay::None;
                                    return EventOutcome::ProfileChanged;
                                }
                                KeyRouting::ShellGone => return EventOutcome::End,
                            }
                        }
                        WindowEvent::Pointer { x, y, action, .. } => {
                            let at = Point::new(
                                i32::try_from(x).unwrap_or(i32::MAX),
                                i32::try_from(y).unwrap_or(i32::MAX),
                            );
                            match route_pointer(
                                overlay, profile, action, at, viewport, scale, theme, font,
                            ) {
                                PointerRouting::Nothing => {}
                                PointerRouting::Repaint => repaint = true,
                                PointerRouting::Edited => return EventOutcome::ProfileChanged,
                                PointerRouting::Chose(command) => {
                                    return finish(
                                        run_command(command, terminal, profile, overlay),
                                        repaint,
                                    )
                                }
                                PointerRouting::Close => {
                                    *overlay = Overlay::None;
                                    return EventOutcome::ProfileChanged;
                                }
                            }
                        }
                        WindowEvent::CloseRequested { .. } => return EventOutcome::End,
                        // The session dropped the window's pixels under
                        // memory pressure: repaint the whole screen.
                        WindowEvent::RedrawRequested { .. } => return EventOutcome::Repaint,
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
                        // Focus changes repaint nothing; the screen is
                        // shell-driven. A wheel likewise has nothing to move:
                        // the terminal renders the shell's live screen and
                        // keeps no scrollback, so there is no scrollable
                        // content a tick could reach. The terminal never
                        // requests a pick, so a pick conclusion is a session
                        // bug and is ignored (an unredeemed delegation is
                        // reclaimed by the kernel at exit).
                        //
                        // Minimized needs no action (the window is hidden and
                        // kept on the taskbar; the screen is redrawn from the
                        // shell on demand). A desktop change was already
                        // adopted above (or, on refusal, stated and left the
                        // last good state standing); either way there is
                        // nothing further to do with the event itself. These
                        // are honest no-ops, not deferred work.
                        WindowEvent::Focus { .. }
                        | WindowEvent::Scrolled { .. }
                        | WindowEvent::Minimized { .. }
                        | WindowEvent::FilePicked { .. }
                        | WindowEvent::PickCancelled { .. }
                        | WindowEvent::DesktopChanged { .. } => {}
                    }
                }
                Err(err) if errno_from(err) == Errno::WouldBlock => {
                    return if repaint {
                        EventOutcome::Repaint
                    } else {
                        EventOutcome::Continue
                    };
                }
                Err(_) => return EventOutcome::ChannelLost,
            }
        }
    }

    /// Fold a pending repaint into `outcome`: a command that concluded
    /// nothing still repaints when an earlier event in the same drain
    /// changed the screen.
    fn finish(outcome: EventOutcome, repaint: bool) -> EventOutcome {
        match outcome {
            EventOutcome::Continue if repaint => EventOutcome::Repaint,
            other => other,
        }
    }

    /// What routing a key press concluded.
    enum KeyRouting {
        /// Nothing to do.
        Nothing,
        /// Repaint the window.
        Repaint,
        /// The settings sheet edited the profile.
        Edited,
        /// A menu accelerator or row named this command.
        Command(Command),
        /// The settings sheet asked to close.
        CloseSheet,
        /// The shell can no longer accept input.
        ShellGone,
    }

    /// Route one key press: the open overlay owns it, else a terminal
    /// accelerator claims it, else it is shell input.
    #[allow(clippy::too_many_arguments)] // The routing needs the whole drawing context to hit-test the overlay.
    fn route_key<S: ShellSource>(
        terminal: &mut Terminal<S>,
        profile: &mut Profile,
        overlay: &mut Overlay,
        key: tairix_abi::input::KeyInput,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        viewport: Rect,
    ) -> KeyRouting {
        let input = key_input_event(key);
        if let Overlay::Menu(menu) = overlay {
            let InputEvent::KeyPressed { key, .. } = input else {
                return KeyRouting::Nothing;
            };
            return match menu.on_key(key) {
                MenuOutcome::Ignored => KeyRouting::Nothing,
                MenuOutcome::Changed => KeyRouting::Repaint,
                MenuOutcome::Dismissed => {
                    *overlay = Overlay::None;
                    KeyRouting::Repaint
                }
                MenuOutcome::Chose(command) => {
                    *overlay = Overlay::None;
                    KeyRouting::Command(command)
                }
            };
        }
        if let Overlay::Sheet(sheet) = overlay {
            let InputEvent::KeyPressed { key, modifiers } = input else {
                return KeyRouting::Nothing;
            };
            let outcome = sheet.on_key(key, modifiers, viewport, scale, theme, font);
            if matches!(outcome, SheetOutcome::Edited | SheetOutcome::Dismissed) {
                *profile = *sheet.profile();
            }
            return match outcome {
                SheetOutcome::Ignored => KeyRouting::Nothing,
                SheetOutcome::Changed => KeyRouting::Repaint,
                SheetOutcome::Edited => KeyRouting::Edited,
                SheetOutcome::Dismissed => KeyRouting::CloseSheet,
            };
        }
        if let InputEvent::KeyPressed { key, modifiers } = input {
            if let Some(command) = Command::accelerator(key, modifiers) {
                return KeyRouting::Command(command);
            }
        }
        // The one shared layout-to-tty rule; a release encodes zero bytes
        // and sends nothing.
        let mut bytes = [0u8; MAX_KEY_BYTES];
        let Ok(n) = encode_key_input(&key, &mut bytes) else {
            return KeyRouting::Nothing;
        };
        match bytes.get(..n) {
            Some(slice) if n > 0 => {
                if terminal.send(slice).is_err() {
                    return KeyRouting::ShellGone;
                }
                KeyRouting::Nothing
            }
            _ => KeyRouting::Nothing,
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
