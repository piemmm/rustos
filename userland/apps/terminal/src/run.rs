//! The `terminal.app` bundle's `Run` entry point (`plans/APPWIN.md` AW4,
//! `plans/GUI-TERMINAL.md`): the windowed terminal emulator hosting the
//! user's shell over the desktop session's window channel.
//!
//! # What the program wires (and what stays in the libraries)
//!
//! Everything with behaviour worth testing lives in host-tested crates —
//! the screen model and its `lib/vt`-consuming parser (`tairix_terminal`),
//! the retained cell renderer (`tairix_terminal::render`), the user's profile
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

    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::fs::OpenFlags;
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, WaitStatus, ORIGIN_WIRE_LEN,
    };
    use tairix_controls::damage;
    use tairix_display::{winframe, SERIAL};
    use tairix_font::BitmapFont;
    use tairix_geometry::{Point, Rect, Scale};
    use tairix_input::InputEvent;
    use tairix_keymap::{encode_key_input, MAX_KEY_BYTES};
    use tairix_raster::Surface;
    use tairix_rt::io::{Stderr, Write};
    use tairix_terminal::effects::{Afterglow, Effects, Phase};
    use tairix_terminal::layout::{
        fit_font_size, grid_dims, grid_size, snap_to_cells, window_size,
    };
    use tairix_terminal::menu::{Command, ContextMenu, MenuOutcome};
    use tairix_terminal::profile::{
        parse as parse_profile, render as render_profile, user_profile_path, Profile,
        MAX_PROFILE_LEN,
    };
    use tairix_terminal::render::Screen;
    use tairix_terminal::scheme::Painted;
    use tairix_terminal::settings::{preferred_extent, Settings, SheetOutcome};
    use tairix_terminal::{
        shell_env, shell_load_failure, shell_wires, ShellSource, StreamShellSource, Terminal, TERM,
        WIN_RESIZABLE,
    };
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_users::DEFAULT_SHELL;
    use tairix_window::{
        damage_in, event_endpoint_for, key_input_event, pointer_input_events, Desktop, PopupSpec,
        WindowClient, WindowSizing, WindowTransport, EVENT_MAILBOX_CAPACITY,
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

    /// Bytes per pixel of the [`mode_for`] surface format: the one definition
    /// the stride and the frame writer both take it from.
    const BYTES_PER_PIXEL: u32 = 4;

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
            stride_bytes: width_px.saturating_mul(BYTES_PER_PIXEL),
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

    /// Which overlay is open.
    ///
    /// The sheet dwarfs the menu, so it is boxed: an [`Overlay`] costs the
    /// same either way, and the one allocation happens when Settings opens.
    enum Content {
        /// The right-click context menu.
        Menu(ContextMenu),
        /// The settings sheet.
        Sheet(Box<Settings>),
    }

    /// The one open overlay and the popup window it is drawn in.
    ///
    /// An overlay is never drawn into the terminal's own window: it lives in
    /// its own undecorated popup surface stacked directly above it, so
    /// shrinking the terminal cannot clip a menu or the settings sheet. At
    /// most one overlay exists at a time and it is modal — every event
    /// delivered for the popup's own window id routes to it, and a press that
    /// lands on the terminal instead dismisses it without reaching the shell.
    struct Overlay {
        /// The overlay's own state.
        content: Content,
        /// The popup's window-channel id, which its events arrive under.
        window: u64,
        /// Base address of the popup's own shared frame region.
        base: usize,
        /// Length of that region in bytes.
        len: usize,
        /// The geometry the region is shaped as; also the popup-local
        /// viewport the overlay is laid out and hit-tested in.
        mode: DisplayMode,
        /// Set once the overlay has asked to go. The loop closes the popup
        /// and releases the region; the routing itself holds no window
        /// client.
        dismissed: bool,
    }

    impl Overlay {
        /// The popup-local viewport the overlay occupies.
        fn viewport(&self) -> Rect {
            Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
        }

        /// Close the popup and release its frame region.
        ///
        /// Consuming the overlay is what makes `present_overlay`'s raw frame
        /// access sound: no one can hold an overlay whose region is gone.
        fn close(self, client: &mut WindowClient<RtWindowTransport>) {
            let _ = client.close(self.window);
            let _ = tairix_rt::shm_unmap(self.base as u64, self.len);
        }
    }

    /// Open a popup window of `mode` at `offset` from `parent`'s client
    /// origin, with its own shared frame region granted to the window
    /// endpoint.
    ///
    /// Returns `None` — stating why on `stderr` — when the region could not be
    /// created or granted, when the session refused the popup, or when the
    /// reply names a server other than the one that opened the parent window
    /// (an imposter reply is refused rather than trusted). The caller then
    /// simply shows no overlay; nothing is left mapped and the terminal keeps
    /// running.
    fn open_popup(
        client: &mut WindowClient<RtWindowTransport>,
        parent: u64,
        server: ProcId,
        event_endpoint: u64,
        mode: &DisplayMode,
        offset: (i32, i32),
    ) -> Option<(u64, usize, usize)> {
        let Some(len) = (mode.stride_bytes as usize)
            .checked_mul(mode.height_px as usize)
            .and_then(|frame| frame.checked_mul(FRAME_COUNT as usize))
        else {
            report("popup frame region larger than the address width");
            return None;
        };
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(len, &mut region_id);
        if base < 0 {
            report("popup frame region refused");
            return None;
        }
        let Ok(base) = usize::try_from(base) else {
            report("popup frame region base outside the address width");
            return None;
        };
        let grant = tairix_rt::shm_grant(region_id, WINDOW_ENDPOINT);
        if grant < 1 {
            let _ = tairix_rt::shm_unmap(base as u64, len);
            report("popup frame region grant refused");
            return None;
        }
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let created = client.create_popup(&PopupSpec {
            parent_window_id: parent,
            shm_handle: grant as u64,
            event_endpoint,
            frame_count: FRAME_COUNT,
            surface: *mode,
            offset_x: offset.0,
            offset_y: offset.1,
        });
        match created {
            Ok((window, replied)) if replied == server => Some((window, base, len)),
            Ok((window, _)) => {
                let _ = client.close(window);
                let _ = tairix_rt::shm_unmap(base as u64, len);
                report("popup reply came from another sender; not shown");
                None
            }
            Err(err) => {
                let _ = tairix_rt::shm_unmap(base as u64, len);
                report(&alloc::format!("popup refused ({err}); not shown"));
                None
            }
        }
    }

    /// Which overlay to open, and where.
    enum OverlayRequest {
        /// The context menu, anchored at this client-local point.
        Menu {
            /// Where the press that asked for it landed.
            at: Point,
        },
        /// The settings sheet, at its own preferred size. Boxed, and moved
        /// straight into [`Content::Sheet`], so the sheet is allocated once.
        Sheet(Box<Settings>),
    }

    /// The offset that centres an `inner` extent within an `outer` one,
    /// negative when the inner extent is the larger of the two.
    fn centre_offset(outer: u32, inner: u32) -> i32 {
        // Display extents halved stay far inside `i32`; a mode that says
        // otherwise centres at the origin rather than wrapping.
        i32::try_from((i64::from(outer) - i64::from(inner)) / 2).unwrap_or(0)
    }

    /// Open `request` in its own popup window above `parent`, drawn once.
    ///
    /// Each popup is exactly the size the overlay wants, measured against the
    /// screen rather than the parent window, so neither is ever shrunk by its
    /// owner: the menu's popup is its own plate at the pressed point, and the
    /// sheet's is its full preferred panel centred over the parent's client.
    /// A window smaller than the sheet therefore yields a negative offset,
    /// which is a legitimate request — the session resolves it against the
    /// parent's screen position and clamps the whole popup on screen, so the
    /// entire sheet stays visible however small the terminal is.
    ///
    /// `None` — with the reason already on `stderr` — leaves no overlay open:
    /// a refusal shows nothing and the terminal carries on.
    #[allow(clippy::too_many_arguments)] // Sizing a popup needs the whole drawing context.
    fn open_overlay(
        client: &mut WindowClient<RtWindowTransport>,
        parent: u64,
        server: ProcId,
        event_endpoint: u64,
        request: OverlayRequest,
        parent_mode: &DisplayMode,
        theme: &Theme,
        desktop: &Desktop,
    ) -> Option<Overlay> {
        let scale = desktop.scale();
        let (content, offset, extent) = match request {
            OverlayRequest::Menu { at } => {
                let menu = ContextMenu::open(Point::new(0, 0));
                let plate = menu.bounds(desktop.screen(), scale, theme);
                (
                    Content::Menu(menu),
                    (at.x, at.y),
                    (plate.width, plate.height),
                )
            }
            OverlayRequest::Sheet(sheet) => {
                let screen = desktop.screen();
                let (want_w, want_h) = preferred_extent(scale);
                // Its own preferred size, capped only by the screen it must
                // fit on; the sheet's panel fills whatever the popup is.
                let extent = (want_w.min(screen.width), want_h.min(screen.height));
                let offset = (
                    centre_offset(parent_mode.width_px, extent.0),
                    centre_offset(parent_mode.height_px, extent.1),
                );
                (Content::Sheet(sheet), offset, extent)
            }
        };
        if extent.0 == 0 || extent.1 == 0 {
            report("overlay has no drawable extent; not shown");
            return None;
        }
        let mode = mode_for(extent.0, extent.1);
        let (window, base, len) =
            open_popup(client, parent, server, event_endpoint, &mode, offset)?;
        let overlay = Overlay {
            content,
            window,
            base,
            len,
            mode,
            dismissed: false,
        };
        if present_overlay(&overlay, theme, scale, client).is_err() {
            report("overlay present refused; not shown");
            overlay.close(client);
            return None;
        }
        Some(overlay)
    }

    /// Draw `overlay` into its popup's frame and present the whole popup.
    ///
    /// The surface starts fully transparent, so an overlay that does not fill
    /// its popup (the settings sheet's centred panel) lets the terminal show
    /// through around it exactly as it did when the sheet was drawn in-window.
    fn present_overlay(
        overlay: &Overlay,
        theme: &Theme,
        scale: Scale,
        client: &mut WindowClient<RtWindowTransport>,
    ) -> Result<(), Errno> {
        let viewport = overlay.viewport();
        let mut surface =
            Surface::new(viewport.width, viewport.height).ok_or(Errno::LengthOutOfRange)?;
        match &overlay.content {
            Content::Menu(menu) => menu.render(&mut surface, viewport, scale, theme),
            Content::Sheet(sheet) => sheet.render(&mut surface, viewport, scale, theme),
        }
        // SAFETY: `open_popup` mapped `overlay.len` zeroed read/write bytes
        // at `overlay.base` and nothing has unmapped or aliased them since —
        // the region is released only by `close_popup`, which consumes the
        // overlay. The protocol serialises access: this app is parked in the
        // present call below while the session reads the same frame.
        let frame =
            unsafe { core::slice::from_raw_parts_mut(overlay.base as *mut u8, overlay.len) };
        write_frame(&surface, frame, &overlay.mode, viewport)?;
        client.present(overlay.window, 0, DamageRect::full(&overlay.mode))
    }

    /// Copy `area` of `surface` into the shared `frame` shaped as `mode`.
    ///
    /// `area` is clipped to the surface first, so a caller may name a
    /// rectangle the screen has since outgrown; the conversion itself is the
    /// one shared window-frame codec, on this thread (a terminal presents only
    /// what its grid changed, so there is nothing here worth another core).
    fn write_frame(
        surface: &Surface,
        frame: &mut [u8],
        mode: &DisplayMode,
        area: Rect,
    ) -> Result<(), Errno> {
        let clipped = area.intersection(&Rect::new(0, 0, surface.width(), surface.height()));
        if clipped.is_empty() {
            return Ok(());
        }
        let (Ok(x), Ok(y)) = (u32::try_from(clipped.left()), u32::try_from(clipped.top())) else {
            return Err(Errno::OutOfRange);
        };
        let damage = DamageRect {
            x,
            y,
            width_px: clipped.width,
            height_px: clipped.height,
        };
        winframe::encode(surface, frame, mode, damage, &SERIAL)
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
        /// The effect pipeline in force, held beside the colours it was
        /// resolved with so a frame cannot be drawn under one profile and
        /// post-processed under another.
        effects: Effects,
        /// The desktop scale everything above was sized at.
        scale: Scale,
        /// The animation step the effects are drawn at.
        phase: Phase,
        /// The persistence state the phosphor effect carries between frames.
        afterglow: Afterglow,
        /// Where the effect pipeline runs, so it never accumulates into the
        /// retained screen. Held only while an effect is in force.
        effected: Option<Surface>,
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
                effects: profile.effects,
                scale: desktop.scale(),
                phase: Phase::default(),
                afterglow: Afterglow::new(),
                effected: None,
            }
        }

        /// Adopt a changed profile or desktop, forgetting the afterglow so a
        /// trail of the old screen cannot ghost over the new one, and the
        /// effect buffer so a terminal whose effects were switched off stops
        /// holding a screen's worth of pixels.
        fn refresh(&mut self, profile: &Profile, theme: &Theme, desktop: &Desktop) {
            let phase = self.phase;
            *self = Self::resolve(profile, theme, desktop);
            self.phase = phase;
        }
    }

    /// Bring the retained screen up to date, copy what changed into `frame`,
    /// and present that rectangle.
    ///
    /// **Only the cells that changed are drawn, copied, and presented.** A
    /// keystroke costs the cell it wrote and the two the cursor moved
    /// between; a shell write that scrolls costs the grid. A wake that
    /// changed nothing presents nothing at all, so it costs the session no
    /// composite either.
    ///
    /// A screen effect is a whole-frame post-process — a wobble displaces
    /// rows and a phosphor trail decays every pixel — so when one is in force
    /// the finished screen is copied into [`Look::effected`], the passes run
    /// there, and the whole window is presented. The retained screen itself
    /// stays clean, so the next frame's diff still describes the *text*
    /// rather than the effect's own churn.
    ///
    /// An open overlay is *not* drawn here: it lives in its own popup window
    /// above this one, so the screen effects can never wobble a menu or a
    /// settings control, and shrinking this window cannot clip one.
    fn present_frame<S, T>(
        terminal: &Terminal<S>,
        look: &mut Look,
        screen: &mut Screen,
        client: &mut WindowClient<T>,
        window: u64,
        frame: &mut [u8],
        mode: &DisplayMode,
    ) -> Result<(), Errno>
    where
        S: ShellSource,
        T: WindowTransport,
    {
        // `mode` is the one truth about the window's extent, so the picture
        // is reconciled to it here rather than at each site that changes it:
        // a surface and a frame region of different shapes cannot arise.
        let shaped = screen.surface().width() == mode.width_px
            && screen.surface().height() == mode.height_px;
        if !shaped && !screen.resize(mode.width_px, mode.height_px) {
            return Err(Errno::LengthOutOfRange);
        }
        let damage = screen.paint(terminal.grid(), &look.painted, look.font);
        let (_, passes) = look.effects.passes(look.scale.percent());
        if passes == 0 {
            if damage.is_empty() {
                return Ok(());
            }
            write_frame(screen.surface(), frame, mode, damage)?;
            let Some(rect) = damage_in(mode, damage) else {
                // The damage lies outside the window the session knows about,
                // so these pixels were never shown: repaint whole next frame
                // rather than leave the surface silently ahead of the screen.
                screen.invalidate();
                return Ok(());
            };
            return client.present(window, 0, rect);
        }
        // Reused between frames, so an animated terminal allocates once
        // rather than once a frame; a resize is what makes it stale.
        let clean = screen.surface();
        let held = match look.effected.take() {
            Some(held) if held.width() == clean.width() && held.height() == clean.height() => {
                Some(held)
            }
            _ => Surface::new(clean.width(), clean.height()),
        };
        // A refused buffer costs the effect, never the terminal: present the
        // plain screen rather than exiting over decoration.
        let Some(mut effected) = held else {
            write_frame(
                clean,
                frame,
                mode,
                Rect::new(0, 0, mode.width_px, mode.height_px),
            )?;
            return client.present(window, 0, DamageRect::full(mode));
        };
        effected.overwrite(0, 0, clean);
        look.effects.apply(
            &mut effected,
            &mut look.afterglow,
            look.phase,
            look.scale.percent(),
        );
        let written = write_frame(
            &effected,
            frame,
            mode,
            Rect::new(0, 0, mode.width_px, mode.height_px),
        );
        look.effected = Some(effected);
        written?;
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
        let output = (desktop.screen_width_px(), desktop.screen_height_px());
        let (w, h) = window_size(look.font, output, theme, desktop.scale());
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

        // --- The retained window picture. Kept between frames so a repaint
        // costs the cells that changed rather than the whole window.
        let Some(mut screen) = Screen::new(w, h) else {
            return fail(EXIT_NO_FRAMES, "screen surface refused");
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

        // A character grid can show nothing at all below one whole cell, and
        // that is where the terminal's own snap to whole cells bottoms out,
        // so one cell of the face it opens in is its declared floor.
        let (min_width_px, min_height_px) = grid_size(1, 1, look.font);
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) = client.create(
            grant as u64,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            "Terminal",
            WindowSizing {
                resizable: WIN_RESIZABLE,
                min_width_px,
                min_height_px,
            },
        ) else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        apply_blur(&mut client, window, &profile);
        let mut overlay: Option<Overlay> = None;
        if present_frame(
            &terminal,
            &mut look,
            &mut screen,
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
                        &mut look,
                        &mut screen,
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
                        window,
                        event_endpoint,
                        server,
                    );
                    // An overlay that has asked to go leaves before anything
                    // this same outcome opens, so a menu row that chose
                    // *Settings* replaces its popup rather than stacking a
                    // second one over it.
                    if overlay.as_ref().is_some_and(|open| open.dismissed) {
                        if let Some(open) = overlay.take() {
                            open.close(&mut client);
                        }
                    }
                    match outcome {
                        EventOutcome::Continue => {}
                        EventOutcome::OpenMenu { at } => {
                            overlay = open_overlay(
                                &mut client,
                                window,
                                server,
                                event_endpoint,
                                OverlayRequest::Menu { at },
                                &mode,
                                theme,
                                &desktop,
                            );
                        }
                        EventOutcome::OpenSheet => {
                            overlay = open_overlay(
                                &mut client,
                                window,
                                server,
                                event_endpoint,
                                OverlayRequest::Sheet(Box::new(Settings::new(&profile))),
                                &mode,
                                theme,
                                &desktop,
                            );
                        }
                        EventOutcome::OverlayChanged => {
                            // Only the overlay's own pixels moved, so the
                            // terminal's window is left exactly as it is.
                            if let Some(open) = overlay.as_ref() {
                                if present_overlay(open, theme, desktop.scale(), &mut client)
                                    .is_err()
                                {
                                    return fail(EXIT_CHANNEL_LOST, "overlay present refused");
                                }
                            }
                        }
                        EventOutcome::Repaint => {
                            // The session dropped this window's pixels (a
                            // redraw request) or the grid was blanked, so the
                            // retained picture cannot be trusted.
                            screen.invalidate();
                            if present_frame(
                                &terminal,
                                &mut look,
                                &mut screen,
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
                            // New colours or a new face: every retained pixel
                            // is stale, and the diff cannot see either.
                            screen.invalidate();
                            if present_frame(
                                &terminal,
                                &mut look,
                                &mut screen,
                                &mut client,
                                window,
                                frames,
                                &mode,
                            )
                            .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                            // The sheet that made the change is still open and
                            // shows it (a moved slider, a chosen swatch), so
                            // its popup is re-presented too.
                            if let Some(open) = overlay.as_ref() {
                                if present_overlay(open, theme, desktop.scale(), &mut client)
                                    .is_err()
                                {
                                    return fail(EXIT_CHANNEL_LOST, "overlay present refused");
                                }
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
                            //
                            // The granted client is first snapped down to a
                            // whole number of cells, so no partial-cell strip
                            // of dead background is left at the right or bottom
                            // edge. Snapping is idempotent, so the size this
                            // re-maps to is already snapped and the `Resized`
                            // it draws back snaps to itself: one step, and it
                            // cannot oscillate. Re-mapping is skipped entirely
                            // when the snapped size is the one already in force.
                            let (snapped_w, snapped_h) =
                                snap_to_cells(width_px, height_px, look.font);
                            if (snapped_w, snapped_h) == (mode.width_px, mode.height_px) {
                                continue;
                            }
                            let new_mode = mode_for(snapped_w, snapped_h);
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
                                let (cols, rows) = grid_dims(snapped_w, snapped_h, look.font);
                                let _ = terminal.resize(cols, rows);
                                let _ = tairix_rt::pty_set_size(pty_master, rows, cols);
                                // The afterglow is the shape of the old
                                // screen; a resized one must not ghost it.
                                look.afterglow.clear();
                                if present_frame(
                                    &terminal,
                                    &mut look,
                                    &mut screen,
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
                            // A new appearance or scale: new colours and a new
                            // face, so nothing retained still holds.
                            screen.invalidate();
                            if present_frame(
                                &terminal,
                                &mut look,
                                &mut screen,
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
                            // cleanly. An open overlay's popup goes with it
                            // (the session would tear it down with its parent
                            // anyway; closing it here also releases its
                            // region). The pty master drops with this process,
                            // so the shell observes end-of-file and exits.
                            if let Some(open) = overlay.take() {
                                open.close(&mut client);
                            }
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
                            &mut look,
                            &mut screen,
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
                        &mut look,
                        &mut screen,
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
        /// The whole window must be repainted: the session dropped this
        /// window's pixels and asked for them again, or the grid was blanked
        /// outright. Anything the retained picture still holds is discarded.
        Repaint,
        /// Only the open overlay's own pixels changed; re-present its popup
        /// and leave the terminal's window alone.
        OverlayChanged,
        /// A secondary press asked for the context menu at this client-local
        /// point; the caller opens its popup there.
        OpenMenu {
            /// Where the press landed, relative to the client origin.
            at: Point,
        },
        /// The *Settings* command asked for the settings sheet; the caller
        /// opens its popup over the client.
        OpenSheet,
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
    ) -> EventOutcome {
        match command {
            Command::Settings => EventOutcome::OpenSheet,
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

    /// Route one pointer event delivered for the open overlay's own popup
    /// window into that overlay.
    ///
    /// The coordinates in a popup's events are popup-local, so the overlay is
    /// hit-tested against the popup's own viewport — the extent it was opened
    /// at — and never against the terminal window's.
    fn route_overlay_pointer(
        overlay: &mut Overlay,
        profile: &mut Profile,
        action: PointerAction,
        at: Point,
        scale: Scale,
        theme: &Theme,
    ) -> OverlayRouting {
        let viewport = overlay.viewport();
        let mut routing = OverlayRouting::Nothing;
        let mut damage = damage::sink();
        match &mut overlay.content {
            Content::Menu(menu) => {
                for event in pointer_input_events(action, at) {
                    match menu.on_pointer(&event, viewport, scale, theme, &mut damage) {
                        MenuOutcome::Ignored => {}
                        MenuOutcome::Changed => routing = OverlayRouting::Redraw,
                        MenuOutcome::Dismissed => return OverlayRouting::Dismissed,
                        MenuOutcome::Chose(command) => return OverlayRouting::Chose(command),
                    }
                }
            }
            Content::Sheet(sheet) => {
                for event in pointer_input_events(action, at) {
                    match sheet.on_pointer(&event, viewport, scale, theme, &mut damage) {
                        SheetOutcome::Ignored => {}
                        SheetOutcome::Changed => routing = OverlayRouting::Redraw,
                        SheetOutcome::Edited => {
                            *profile = *sheet.profile();
                            routing = OverlayRouting::Edited;
                        }
                        SheetOutcome::Dismissed => {
                            *profile = *sheet.profile();
                            return OverlayRouting::Closed;
                        }
                    }
                }
            }
        }
        routing
    }

    /// Route one key press delivered for the open overlay's own popup window
    /// into that overlay.
    fn route_overlay_key(
        overlay: &mut Overlay,
        profile: &mut Profile,
        key: tairix_abi::input::KeyInput,
        scale: Scale,
        theme: &Theme,
    ) -> OverlayRouting {
        let input = key_input_event(key);
        let viewport = overlay.viewport();
        let mut damage = damage::sink();
        match &mut overlay.content {
            Content::Menu(menu) => {
                let InputEvent::KeyPressed { key, .. } = input else {
                    return OverlayRouting::Nothing;
                };
                match menu.on_key(key, viewport, scale, theme, &mut damage) {
                    MenuOutcome::Ignored => OverlayRouting::Nothing,
                    MenuOutcome::Changed => OverlayRouting::Redraw,
                    MenuOutcome::Dismissed => OverlayRouting::Dismissed,
                    MenuOutcome::Chose(command) => OverlayRouting::Chose(command),
                }
            }
            Content::Sheet(sheet) => {
                let InputEvent::KeyPressed { key, modifiers } = input else {
                    return OverlayRouting::Nothing;
                };
                let outcome = sheet.on_key(key, modifiers, viewport, scale, theme, &mut damage);
                if matches!(outcome, SheetOutcome::Edited | SheetOutcome::Dismissed) {
                    *profile = *sheet.profile();
                }
                match outcome {
                    SheetOutcome::Ignored => OverlayRouting::Nothing,
                    SheetOutcome::Changed => OverlayRouting::Redraw,
                    SheetOutcome::Edited => OverlayRouting::Edited,
                    SheetOutcome::Dismissed => OverlayRouting::Closed,
                }
            }
        }
    }

    /// What routing an event into the open overlay concluded.
    enum OverlayRouting {
        /// Nothing to do.
        Nothing,
        /// The overlay's own pixels changed; re-present its popup.
        Redraw,
        /// The settings sheet edited the profile.
        Edited,
        /// A menu row or accelerator named this command; the overlay is done.
        Chose(Command),
        /// The overlay asked to go.
        Dismissed,
        /// The settings sheet asked to close, having possibly edited the
        /// profile.
        Closed,
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
        overlay: &mut Option<Overlay>,
        desktop: &mut Desktop,
        theme: &Theme,
        window: u64,
        endpoint: u64,
        server: ProcId,
    ) -> EventOutcome {
        let scale = desktop.scale();
        let mut redraw = false;
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
                    // Both windows this app owns — its own and, while one is
                    // open, its overlay's popup — report through this one
                    // mailbox, so every event is demuxed on the id it carries.
                    let popup = overlay.as_ref().map(|open| open.window);
                    let for_popup = popup == Some(event.window_id());
                    match event {
                        WindowEvent::Key { key, .. } if for_popup => {
                            let Some(open) = overlay.as_mut() else {
                                continue;
                            };
                            match route_overlay_key(open, profile, key, scale, theme) {
                                OverlayRouting::Nothing => {}
                                OverlayRouting::Redraw => redraw = true,
                                OverlayRouting::Edited => return EventOutcome::ProfileChanged,
                                OverlayRouting::Chose(command) => {
                                    open.dismissed = true;
                                    return finish(run_command(command, terminal, profile), redraw);
                                }
                                OverlayRouting::Dismissed => {
                                    open.dismissed = true;
                                    return EventOutcome::Continue;
                                }
                                OverlayRouting::Closed => {
                                    open.dismissed = true;
                                    return EventOutcome::ProfileChanged;
                                }
                            }
                        }
                        WindowEvent::Key { key, .. } => match route_key(terminal, key) {
                            KeyRouting::Nothing => {}
                            KeyRouting::Command(command) => {
                                return finish(run_command(command, terminal, profile), redraw)
                            }
                            KeyRouting::ShellGone => return EventOutcome::End,
                        },
                        WindowEvent::Pointer { x, y, action, .. } if for_popup => {
                            let Some(open) = overlay.as_mut() else {
                                continue;
                            };
                            let at = client_point(x, y);
                            match route_overlay_pointer(open, profile, action, at, scale, theme) {
                                OverlayRouting::Nothing => {}
                                OverlayRouting::Redraw => redraw = true,
                                OverlayRouting::Edited => return EventOutcome::ProfileChanged,
                                OverlayRouting::Chose(command) => {
                                    open.dismissed = true;
                                    return finish(run_command(command, terminal, profile), redraw);
                                }
                                OverlayRouting::Dismissed => {
                                    open.dismissed = true;
                                    return EventOutcome::Continue;
                                }
                                OverlayRouting::Closed => {
                                    open.dismissed = true;
                                    return EventOutcome::ProfileChanged;
                                }
                            }
                        }
                        WindowEvent::Pointer { x, y, action, .. } => {
                            // An overlay is modal, so a press that lands on
                            // the terminal instead dismisses it and reaches
                            // nothing else. Otherwise a secondary press asks
                            // for the context menu at that point, and every
                            // other pointer event is a no-op: the screen is
                            // shell-driven and the emulator keeps no
                            // scrollback for a wheel to move.
                            if let Some(open) = overlay.as_mut() {
                                if matches!(action, PointerAction::Pressed(_)) {
                                    open.dismissed = true;
                                    return EventOutcome::Continue;
                                }
                            } else if action
                                == PointerAction::Pressed(
                                    tairix_abi::input::PointerButtonCode::Secondary,
                                )
                            {
                                return EventOutcome::OpenMenu {
                                    at: client_point(x, y),
                                };
                            }
                        }
                        // A popup wears no close control, so a close asked of
                        // one can only be the session tearing it down: let the
                        // overlay go rather than ending the program.
                        WindowEvent::CloseRequested { .. } if for_popup => {
                            if let Some(open) = overlay.as_mut() {
                                open.dismissed = true;
                            }
                            return EventOutcome::Continue;
                        }
                        WindowEvent::CloseRequested { .. } => return EventOutcome::End,
                        // The session dropped a window's pixels under memory
                        // pressure: repaint whichever window lost them.
                        WindowEvent::RedrawRequested { .. } if for_popup => {
                            return EventOutcome::OverlayChanged
                        }
                        WindowEvent::RedrawRequested { .. } => return EventOutcome::Repaint,
                        // The window manager resized the window (a settled
                        // drag-resize, or a maximize/restore): hand the new
                        // client size back to the caller, which re-maps the
                        // frame region, reshapes the grid, and updates the pty
                        // window size. Returning here leaves any events queued
                        // behind it for the next wake (level-triggered peek).
                        WindowEvent::Resized {
                            window_id,
                            width_px,
                            height_px,
                            ..
                        } if window_id == window => {
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
                        //
                        // A `Resized` for anything but the terminal's own
                        // window is likewise nothing: a popup is neither
                        // decorated nor resizable, so it has no size of its
                        // own for the session to change.
                        //
                        // A secondary press on Close asks to leave what the
                        // window is showing; the terminal has nothing to leave
                        // but itself, and a primary press already closes it.
                        WindowEvent::AlternateCloseRequested { .. }
                        | WindowEvent::Focus { .. }
                        | WindowEvent::Scrolled { .. }
                        | WindowEvent::Minimized { .. }
                        | WindowEvent::Resized { .. }
                        | WindowEvent::FilePicked { .. }
                        | WindowEvent::PickCancelled { .. }
                        | WindowEvent::DesktopChanged { .. } => {}
                    }
                }
                Err(err) if errno_from(err) == Errno::WouldBlock => {
                    return if redraw {
                        EventOutcome::OverlayChanged
                    } else {
                        EventOutcome::Continue
                    };
                }
                Err(_) => return EventOutcome::ChannelLost,
            }
        }
    }

    /// Fold a pending overlay redraw into `outcome`: a command that concluded
    /// nothing still re-presents the popup when an earlier event in the same
    /// drain changed the overlay's own pixels.
    fn finish(outcome: EventOutcome, redraw: bool) -> EventOutcome {
        match outcome {
            EventOutcome::Continue if redraw => EventOutcome::OverlayChanged,
            other => other,
        }
    }

    /// The client-local point a wire pointer position names.
    fn client_point(x: u32, y: u32) -> Point {
        Point::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        )
    }

    /// What routing a key press delivered for the terminal's own window
    /// concluded.
    enum KeyRouting {
        /// Nothing to do.
        Nothing,
        /// A terminal accelerator named this command.
        Command(Command),
        /// The shell can no longer accept input.
        ShellGone,
    }

    /// Route one key press delivered for the terminal's own window: a
    /// terminal accelerator claims it, else it is shell input.
    ///
    /// A key for the open overlay's popup never reaches here — it arrives
    /// under the popup's own window id and is routed there.
    fn route_key<S: ShellSource>(
        terminal: &mut Terminal<S>,
        key: tairix_abi::input::KeyInput,
    ) -> KeyRouting {
        let input = key_input_event(key);
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
