//! The `terminal.app` bundle's `Run` entry point (`plans/APPWIN.md` AW4,
//! `plans/GUI-TERMINAL.md`): the windowed terminal emulator hosting the
//! user's shells over the desktop session's window channel.
//!
//! # One process, many windows
//!
//! A terminal window is not an application: the application is the emulator,
//! and each of its windows is one hosted shell. So this is **one process
//! with a `Vec` of windows** — each carrying its own pseudo-terminal, shell
//! child, screen model, retained picture, look, and overlay — parked on one
//! wait-set that carries one event mailbox for the whole process plus that
//! window's own shell-output and child members. Opening another window costs
//! a pty, a spawn, a frame region, and two wait-set members; it costs no
//! second process, no second event mailbox, and no second icon-bar slot.
//!
//! That is what makes the desktop's icon bar honest. The bar shows
//! applications, so the terminal declares **one** presence
//! ([`tairix_terminal::appbar`]) whose slot stands for the emulator: a
//! primary click on it opens a fresh window, its menu offers *New window*
//! and *Quit*, and hovering it picks between the windows it owns. Closing
//! the last window puts the terminal away rather than ending it — the slot
//! stays, and a click there opens the next; only *Quit* closes them all and
//! ends the process.
//!
//! # What the program wires (and what stays in the libraries)
//!
//! Everything with behaviour worth testing lives in host-tested crates —
//! the screen model and its `lib/vt`-consuming parser (`tairix_terminal`),
//! the retained cell renderer (`tairix_terminal::render`), the user's profile
//! and its document, the screen-effect pipeline, the right-click menu, the
//! settings sheet, the spawned shell's pipe wiring, the icon-bar declaration,
//! and the window channel's client half (`tairix_window`). This binary only
//! composes them over the live syscalls:
//!
//! * Per window, two ends of one kernel pseudo-terminal: keystrokes flow to
//!   that shell's standard input and its cooked output flows back to that
//!   screen. The child-side end is closed here after the spawn, so the shell
//!   observes end-of-file the moment its window goes, and the window
//!   observes end-of-stream the moment the shell does.
//! * Per window, one `shm_create`d frame region granted to the reserved
//!   window endpoint (the zero-copy surface the session maps once at
//!   create).
//! * One wait-set the program **parks** on — never a poll loop — carrying
//!   its `port_bind`-bound event mailbox (every window's deliveries and the
//!   application-scoped icon-bar events, each accepted only from the session
//!   identity the squat-protected create reply named), one shell-output and
//!   one child member per window, and the memory-pressure wake. When an
//!   animated screen effect is in force the park carries a one-shot frame
//!   deadline, so the animation costs one timed wake per frame and nothing
//!   at all when it is switched off.
//! * The user's own profile document, read at start-up under this process's
//!   own identity and rewritten whenever a setting changes. It is the
//!   *user's* profile, so every window shares it.
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
    use alloc::vec::Vec;

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::window_ipc::{
        AppMenuItemId, MenuAnchor, MenuOutcome, PointerAction, WindowEvent, WINDOW_ENDPOINT,
    };
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
    use tairix_terminal::appbar::{self, BarCommand};
    use tairix_terminal::effects::{EffectState, Effects, Phase};
    use tairix_terminal::layout::{
        fit_font_size, grid_dims, grid_size, snap_to_cells, window_size,
    };
    use tairix_terminal::menu::{self, Command};
    // `Settings` here is the sheet UI; the app-data handle is `SettingsStore`.
    use tairix_appdata::{RtHost, Settings as SettingsStore};
    use tairix_terminal::profile::Profile;
    use tairix_terminal::render::Screen;
    use tairix_terminal::scheme::Painted;
    use tairix_terminal::settings::{preferred_extent, Settings, SheetOutcome};
    use tairix_terminal::{
        shell_env, shell_load_failure, shell_wires, win_sizing, ShellSource, Terminal, TERM,
    };
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_users::DEFAULT_SHELL;
    use tairix_window::{
        damage_in, event_endpoint_for, key_input_event, pointer_input_events, pointer_point,
        Desktop, PopupSpec, WindowClient, WindowFrames, WindowTransport, EVENT_MAILBOX_CAPACITY,
    };

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

    /// The wait-set token of the one event-mailbox member. One mailbox
    /// serves the whole process: every window's events and the
    /// application-scoped icon-bar events arrive through it, demuxed on the
    /// window id each carries (or its absence).
    const EVENT_TOKEN: u64 = 1;

    /// The wait-set token of the memory-pressure member: the kernel wakes the
    /// park when the machine's pressure band changes, so the glyph cache is
    /// trimmed as memory tightens instead of being held until something else
    /// is starved.
    const PRESSURE_TOKEN: u64 = 2;

    /// Where the per-window token pairs begin, clear of the fixed tokens
    /// above.
    ///
    /// Window `slot` owns `WINDOW_TOKEN_BASE + slot * 2` for its
    /// shell-output stream and the value after it for its shell child. The
    /// slot is a monotonic counter this process mints, never an index into a
    /// list that shifts, so a token names the same window for as long as
    /// that window lives and is never reused after it goes.
    const WINDOW_TOKEN_BASE: u64 = 16;

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
    /// The fresh region is created and granted first and returned only once
    /// [`WindowClient::resize`] has accepted it, so the caller's old region is
    /// dropped — and unmapped — by adopting the new one, while every refusal
    /// drops the fresh region here and leaves the window on its old geometry.
    fn resize_frames(
        client: &mut WindowClient<RtWindowTransport>,
        window: u64,
        new_mode: &DisplayMode,
    ) -> Option<WindowFrames> {
        let new_len = (new_mode.stride_bytes as usize)
            .checked_mul(new_mode.height_px as usize)?
            .checked_mul(FRAME_COUNT as usize)?;
        let frames = WindowFrames::create(new_len)?;
        client
            .resize(window, frames.grant()?, FRAME_COUNT, new_mode)
            .ok()?;
        Some(frames)
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
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The command word this application's bundle is installed under, which
    /// selects nothing but its own shipped defaults: the store itself is keyed
    /// on the bundle identity the kernel attests.
    const OWN_WORD: &str = "terminal";

    /// The profile in force for this user, and a note for every stored
    /// setting the registry refused.
    ///
    /// A store the app-data service cannot serve — no service bound, a volume
    /// still to be unlocked — leaves the bundle's shipped defaults standing
    /// and is reported once, rather than running on settings whose provenance
    /// the user cannot see. A key the registry refuses costs only itself and
    /// is named.
    fn load_profile(settings: &SettingsStore<'_>) -> Profile {
        if let Some(err) = settings.store_refusal() {
            report(&alloc::format!(
                "settings unavailable ({err:?}); running on this build's defaults"
            ));
        }
        if let Some(err) = settings.defaults_refusal() {
            report(&alloc::format!(
                "this bundle's shipped defaults could not be read ({err:?})"
            ));
        }
        let (profile, refused) = Profile::load(settings);
        for key in refused {
            report(&alloc::format!(
                "{}: not a value this setting accepts; using its default",
                key.name()
            ));
        }
        profile
    }

    /// Publish `profile` to the app-data store.
    ///
    /// A refused publish is reported and otherwise harmless: the terminal
    /// keeps showing what the user just chose, and the next session opens on
    /// whatever the store still holds.
    fn persist_profile(settings: &mut SettingsStore<'_>, profile: &Profile) {
        if let Err(err) = profile.save(settings) {
            report(&alloc::format!("the profile was not saved ({err:?})"));
        }
    }

    /// Forget the user's own settings and adopt what the store's lower layers
    /// then imply.
    ///
    /// *Restore defaults* deliberately does **not** write this build's default
    /// values into the user's document: it removes their opinions, so a
    /// machine-wide policy or a later shipped default applies rather than a
    /// frozen copy of today's. A refused clear is reported and leaves the
    /// profile the user is looking at untouched.
    fn restore_profile(settings: &mut SettingsStore<'_>, profile: &mut Profile) {
        if let Err(err) = Profile::clear(settings) {
            report(&alloc::format!("the defaults were not restored ({err:?})"));
            return;
        }
        *profile = load_profile(settings);
    }

    /// The settings sheet and the popup window it is drawn in.
    ///
    /// The sheet is never drawn into the terminal's own window: it lives in
    /// its own undecorated popup surface stacked directly above it, so
    /// shrinking the terminal cannot clip it. At most one is open at a time
    /// and it is modal — every event delivered for the popup's own window id
    /// routes to it, and a press that lands on the terminal instead dismisses
    /// it without reaching the shell.
    struct Overlay {
        /// The sheet itself. Boxed because it dwarfs everything around it,
        /// so the one allocation happens when Settings opens.
        sheet: Box<Settings>,
        /// The popup's window-channel id, which its events arrive under.
        window: u64,
        /// The popup's own shared frame region.
        frames: WindowFrames,
        /// The geometry the region is shaped as; also the popup-local
        /// viewport the overlay is laid out and hit-tested in.
        mode: DisplayMode,
        /// Set once the overlay has asked to go. The loop closes the popup
        /// and releases the region; the routing itself holds no window
        /// client.
        dismissed: bool,
    }

    impl Overlay {
        /// Hand the sheet a profile that came from somewhere other than its
        /// own widgets — the store's lower layers, after a restore — so the
        /// controls show what actually applies.
        fn adopt_profile(&mut self, profile: &Profile) {
            self.sheet.adopt(*profile);
        }

        /// The popup-local viewport the overlay occupies.
        fn viewport(&self) -> Rect {
            Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
        }

        /// Close the popup; its frame region is unmapped by its own drop.
        fn close(self, client: &mut WindowClient<RtWindowTransport>) {
            let _ = client.close(self.window);
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
    ) -> Option<(u64, WindowFrames)> {
        let Some(len) = (mode.stride_bytes as usize)
            .checked_mul(mode.height_px as usize)
            .and_then(|frame| frame.checked_mul(FRAME_COUNT as usize))
        else {
            report("popup frame region larger than the address width");
            return None;
        };
        let Some(frames) = WindowFrames::create(len) else {
            report("popup frame region refused");
            return None;
        };
        let Some(grant) = frames.grant() else {
            report("popup frame region grant refused");
            return None;
        };
        let created = client.create_popup(&PopupSpec {
            parent_window_id: parent,
            shm_handle: grant,
            event_endpoint,
            frame_count: FRAME_COUNT,
            surface: *mode,
            offset_x: offset.0,
            offset_y: offset.1,
        });
        // Every refusal below drops `frames`, which unmaps it, so no path
        // leaves a popup region pinned.
        match created {
            Ok((window, replied)) if replied == server => Some((window, frames)),
            Ok((window, _)) => {
                let _ = client.close(window);
                report("popup reply came from another sender; not shown");
                None
            }
            Err(err) => {
                report(&alloc::format!("popup refused ({err}); not shown"));
                None
            }
        }
    }

    /// Ask the desktop to open this window's menu at the press that asked
    /// for it.
    ///
    /// The anchor is the client-local point the press was reported at, which
    /// is the only space the terminal can speak truthfully: it is never told
    /// where its window sits. The desktop places, draws, grabs and dismisses;
    /// the answer arrives later as one `MenuClosed` naming the id minted
    /// here. A refusal is an answer — it is reported and the terminal carries
    /// on with no menu, never drawing one of its own.
    fn open_window_menu(
        client: &mut WindowClient<RtWindowTransport>,
        open: &mut TerminalWindow,
        at: Point,
    ) {
        let model = match menu::model() {
            Ok(model) => model,
            Err(err) => {
                report(&alloc::format!("menu model refused ({err}); not shown"));
                return;
            }
        };
        let anchor = match MenuAnchor::new(at.x, at.y, 0, 0) {
            Ok(anchor) => anchor,
            Err(err) => {
                report(&alloc::format!("menu anchor refused ({err}); not shown"));
                return;
            }
        };
        match client.open_menu(open.window, anchor, &model) {
            Ok(open_id) => open.menu = Some(open_id),
            Err(err) => report(&alloc::format!("menu refused ({err}); not shown")),
        }
    }

    /// The offset that centres an `inner` extent within an `outer` one,
    /// negative when the inner extent is the larger of the two.
    fn centre_offset(outer: u32, inner: u32) -> i32 {
        // Display extents halved stay far inside `i32`; a mode that says
        // otherwise centres at the origin rather than wrapping.
        i32::try_from((i64::from(outer) - i64::from(inner)) / 2).unwrap_or(0)
    }

    /// Open `sheet` in its own popup window above `parent`, drawn once.
    ///
    /// The popup is exactly the size the sheet wants — its full preferred
    /// panel, measured against the screen rather than the parent window, so
    /// it is never shrunk by its owner — centred over the parent's client. A
    /// window smaller than the sheet therefore yields a negative offset,
    /// which is a legitimate request: the session resolves it against the
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
        sheet: Box<Settings>,
        parent_mode: &DisplayMode,
        theme: &Theme,
        desktop: &Desktop,
    ) -> Option<Overlay> {
        let scale = desktop.scale();
        let screen = desktop.screen();
        let (want_w, want_h) = preferred_extent(scale);
        // Its own preferred size, capped only by the screen it must fit on;
        // the sheet's panel fills whatever the popup is.
        let extent = (want_w.min(screen.width), want_h.min(screen.height));
        let offset = (
            centre_offset(parent_mode.width_px, extent.0),
            centre_offset(parent_mode.height_px, extent.1),
        );
        if extent.0 == 0 || extent.1 == 0 {
            report("settings sheet has no drawable extent; not shown");
            return None;
        }
        let mode = mode_for(extent.0, extent.1);
        let (window, frames) = open_popup(client, parent, server, event_endpoint, &mode, offset)?;
        let mut overlay = Overlay {
            sheet,
            window,
            frames,
            mode,
            dismissed: false,
        };
        if present_overlay(&mut overlay, theme, scale, client).is_err() {
            report("settings sheet present refused; not shown");
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
        overlay: &mut Overlay,
        theme: &Theme,
        scale: Scale,
        client: &mut WindowClient<RtWindowTransport>,
    ) -> Result<(), Errno> {
        let viewport = overlay.viewport();
        let mut surface =
            Surface::new(viewport.width, viewport.height).ok_or(Errno::LengthOutOfRange)?;
        overlay.sheet.render(&mut surface, viewport, scale, theme);
        let frame = client
            .frame_pixels(
                &mut overlay.frames,
                overlay.window,
                FRAME_COUNT,
                &overlay.mode,
            )
            .ok_or(Errno::NotAttached)?;
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

    /// One window's shell channel: the master end of its own kernel
    /// pseudo-terminal.
    ///
    /// A named type rather than a pair of closures, because every window
    /// holds one and they all live in the same list — a closure's type is its
    /// own, so a list of them could not be built.
    struct PtyShell {
        /// The pty master descriptor. Reads drain the shell's cooked output;
        /// writes feed keystrokes through the input discipline.
        master: u32,
    }

    impl ShellSource for PtyShell {
        fn read(&mut self) -> Result<Vec<u8>, Errno> {
            let mut chunk = [0u8; 4096];
            let read =
                tairix_rt::fs_read(self.master, 0, &mut chunk).map_err(Errno::from_syscall)?;
            match chunk.get(..read) {
                Some(slice) => Ok(slice.to_vec()),
                None => Err(Errno::OutOfRange),
            }
        }

        fn write(&mut self, bytes: &[u8]) -> Result<(), Errno> {
            let mut sent = 0;
            while sent < bytes.len() {
                let slice = bytes.get(sent..).ok_or(Errno::OutOfRange)?;
                let wrote =
                    tairix_rt::fs_write(self.master, 0, slice).map_err(Errno::from_syscall)?;
                if wrote == 0 {
                    return Err(Errno::WouldBlock);
                }
                sent += wrote;
            }
            Ok(())
        }
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
        /// What the stateful passes carry between frames.
        state: EffectState,
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
                state: EffectState::new(),
                effected: None,
            }
        }

        /// Adopt a changed profile or desktop, forgetting what the passes
        /// remembered so a trail of the old screen cannot ghost over the new
        /// one, and the effect buffer so a terminal whose effects were
        /// switched off stops holding a screen's worth of pixels.
        fn refresh(&mut self, profile: &Profile, theme: &Theme, desktop: &Desktop) {
            let phase = self.phase;
            *self = Self::resolve(profile, theme, desktop);
            self.phase = phase;
        }
    }

    /// One terminal window: its hosted shell, its screen, and everything
    /// drawn for it.
    ///
    /// Each window is independent of its siblings in every way but the
    /// user's profile (which is the *user's*, not a window's) and the one
    /// event mailbox they share. So a shell exiting, a resize, or an overlay
    /// opening reaches exactly one of them.
    struct TerminalWindow {
        /// The window-channel id its events arrive under.
        window: u64,
        /// This process's slot for the window, naming its two wait-set
        /// tokens.
        slot: u64,
        /// The pty master descriptor, kept beside the screen model so a
        /// resize can tell the shell its new window size.
        pty_master: u32,
        /// The hosted shell's PID, for the reap.
        shell_pid: i64,
        /// The screen model over that shell.
        terminal: Terminal<PtyShell>,
        /// The retained window picture.
        screen: Screen,
        /// How this window's screen currently looks.
        look: Look,
        /// The geometry its frame region is shaped as.
        mode: DisplayMode,
        /// Its shared frame region, released when the session releases its
        /// side and re-attached by the next present.
        frames: WindowFrames,
        /// Its one open settings sheet, if any.
        overlay: Option<Overlay>,
        /// The open id of this window's unanswered menu, if one is up.
        ///
        /// The desktop mints one per gesture and never reuses it, so an
        /// answer that names anything else belongs to a gesture already
        /// settled and is not acted on.
        menu: Option<u64>,
    }

    impl TerminalWindow {
        /// Install `overlay` as this window's one sheet, closing whatever it
        /// replaces.
        ///
        /// Assigning over a live one would drop it without closing its popup,
        /// leaving a session-side window on screen for ever.
        fn set_overlay(
            &mut self,
            client: &mut WindowClient<RtWindowTransport>,
            overlay: Option<Overlay>,
        ) {
            if let Some(held) = self.overlay.take() {
                held.close(client);
            }
            self.overlay = overlay;
        }

        /// The wait-set token of this window's shell-output stream member.
        const fn shell_token(&self) -> u64 {
            WINDOW_TOKEN_BASE + self.slot * 2
        }

        /// The wait-set token of this window's shell-child member.
        const fn child_token(&self) -> u64 {
            WINDOW_TOKEN_BASE + self.slot * 2 + 1
        }

        /// Bring this window's picture up to date and present what changed.
        ///
        /// A region the session released while the window was hidden is
        /// re-attached first, so this paints into a live one.
        fn present(&mut self, client: &mut WindowClient<RtWindowTransport>) -> Result<(), Errno> {
            let (mode, window) = (self.mode, self.window);
            let frame = client
                .frame_pixels(&mut self.frames, window, FRAME_COUNT, &mode)
                .ok_or(Errno::NotAttached)?;
            present_frame(
                &self.terminal,
                &mut self.look,
                &mut self.screen,
                client,
                window,
                frame,
                &mode,
            )
        }

        /// Close this window, its sheet, and its shell; the frame regions
        /// are unmapped by their own drops.
        ///
        /// The pty master is closed here rather than left to process exit,
        /// because a process that keeps running must not hold a dead
        /// window's descriptors. Dropping it is what makes the shell observe
        /// end-of-file.
        fn close(mut self, client: &mut WindowClient<RtWindowTransport>, set: u64) {
            if let Some(open) = self.overlay.take() {
                open.close(client);
            }
            let _ = client.close(self.window);
            let _ = tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Del,
                WaitSourceKind::Stream,
                u64::from(self.pty_master),
                self.shell_token(),
            );
            let _ = tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Del,
                WaitSourceKind::Child,
                #[allow(clippy::cast_sign_loss)] // A PID, known non-negative.
                {
                    self.shell_pid as u64
                },
                self.child_token(),
            );
            let _ = tairix_rt::fs_close(self.pty_master);
        }
    }

    /// Everything the bring-up of one window needs from the process it joins.
    struct WindowContext<'a> {
        /// The window channel.
        client: &'a mut WindowClient<RtWindowTransport>,
        /// The one event mailbox every window reports through.
        event_endpoint: u64,
        /// The wait-set the process parks on.
        set: u64,
        /// The slot this window takes, naming its two tokens.
        slot: u64,
        /// The user's profile, shared by every window.
        profile: &'a Profile,
        /// The active theme.
        theme: &'a Theme,
        /// The desktop the window opens on.
        desktop: &'a Desktop,
        /// This terminal's inherited environment, forwarded to the shell.
        env: &'a [Vec<u8>],
    }

    /// Open one terminal window: its screen model and retained picture, its
    /// shared frame region, the desktop window itself, and then its
    /// pseudo-terminal, hosted shell, and two wait-set members.
    ///
    /// The desktop is asked for the window before anything is spawned, so a
    /// session that refuses one costs a single round trip rather than a pty
    /// and a whole shell process brought up and torn straight back down.
    ///
    /// Returns the window and the session identity the create reply named, or
    /// `None` with the reason already on `stderr`. Every refusal unwinds what
    /// it had allocated, so a window that could not be opened leaves nothing
    /// mapped, nothing spawned, and no member on the wait-set.
    #[allow(clippy::too_many_lines)]
    // One linear bring-up whose every refusal unwinds what it had; splitting it would separate an allocation from its release.
    #[allow(clippy::needless_pass_by_value)] // The context is a bundle of borrows, moved so the caller cannot reuse a stale one.
    fn open_window(ctx: WindowContext<'_>) -> Option<(TerminalWindow, ProcId)> {
        let look = Look::resolve(ctx.profile, ctx.theme, ctx.desktop);
        let output = (
            ctx.desktop.screen_width_px(),
            ctx.desktop.screen_height_px(),
        );
        let (w, h) = window_size(look.font, output, ctx.theme, ctx.desktop.scale());
        let (cols, rows) = grid_dims(w, h, look.font);

        let Some(screen) = Screen::new(w, h) else {
            report("screen surface refused; no window opened");
            return None;
        };

        let mode = mode_for(w, h);
        let Some(total) = (mode.stride_bytes as usize)
            .checked_mul(mode.height_px as usize)
            .and_then(|frame| frame.checked_mul(FRAME_COUNT as usize))
        else {
            report("frame region larger than the address width; no window opened");
            return None;
        };
        let Some(frames) = WindowFrames::create(total) else {
            report("shared frame region refused; no window opened");
            return None;
        };
        let Some(grant) = frames.grant() else {
            report("frame region grant refused; no window opened");
            return None;
        };

        // The desktop is asked *before* a pty is created or a shell spawned,
        // because the session can refuse (it bounds the windows one client
        // may hold) and a refusal must cost nothing: spawning a shell that is
        // immediately thrown away is a whole process load and teardown per
        // click, which is felt right across the desktop when a user keeps
        // asking for windows they cannot have.
        //
        // A character grid can show nothing at all below one whole cell, and
        // that is where the terminal's own snap to whole cells bottoms out,
        // so one cell of the face it opens in is its declared floor.
        let (min_width_px, min_height_px) = grid_size(1, 1, look.font);
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let created = ctx.client.create(
            grant,
            ctx.event_endpoint,
            FRAME_COUNT,
            &mode,
            "Terminal",
            win_sizing(min_width_px, min_height_px),
        );
        let Ok((window, server)) = created else {
            // `frames` drops here, which unmaps it.
            report("desktop session refused the window");
            return None;
        };
        let close_window = |client: &mut WindowClient<RtWindowTransport>| {
            let _ = client.close(window);
        };

        // The pty is created at the grid the window will actually show, so
        // `terminal_size` reports it. The shell's fd 0/1/2 are the slave, a
        // console-class tty, so it runs its full interactive editor exactly
        // as on the hardware console (`plans/PTY.md`).
        let Ok((pty_master, pty_slave)) = tairix_rt::pty_create(rows, cols) else {
            close_window(ctx.client);
            report("pty refused; no window opened");
            return None;
        };
        let attach = shell_wires(pty_slave);
        let env: Vec<&[u8]> = ctx.env.iter().map(Vec::as_slice).collect();
        let shell_pid =
            tairix_rt::spawn_attached(DEFAULT_SHELL.as_bytes(), &attach, &[b"elsh"], &env);
        // Close this process's own slave end either way: the spawn cloned it
        // into the shell, and keeping it here would mask the shell's exit.
        let _ = tairix_rt::fs_close(pty_slave);
        if shell_pid < 0 {
            let _ = tairix_rt::fs_close(pty_master);
            close_window(ctx.client);
            report("shell spawn refused; no window opened");
            return None;
        }

        let Some(terminal) = Terminal::new(cols, rows, PtyShell { master: pty_master }) else {
            let _ = tairix_rt::fs_close(pty_master);
            close_window(ctx.client);
            report("screen grid refused; no window opened");
            return None;
        };

        let mut opened = TerminalWindow {
            window,
            slot: ctx.slot,
            pty_master,
            shell_pid,
            terminal,
            screen,
            look,
            mode,
            frames,
            overlay: None,
            menu: None,
        };
        let members = [
            (
                WaitSourceKind::Stream,
                u64::from(pty_master),
                opened.shell_token(),
            ),
            (
                WaitSourceKind::Child,
                #[allow(clippy::cast_sign_loss)] // `shell_pid >= 0` checked above; it is a PID.
                {
                    shell_pid as u64
                },
                opened.child_token(),
            ),
        ];
        for (kind, id, token) in members {
            if tairix_rt::waitset_ctl(ctx.set, WaitSetOp::Add, kind, id, token) != 0 {
                report("wait-set member refused; no window opened");
                opened.close(ctx.client, ctx.set);
                return None;
            }
        }
        apply_blur(ctx.client, window, ctx.profile);
        if opened.present(ctx.client).is_err() {
            report("first present refused; no window opened");
            opened.close(ctx.client, ctx.set);
            return None;
        }
        Some((opened, server))
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
            &mut look.state,
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
        // once they have seen it. It is the user's, so every window shares
        // it.
        let mut host = RtHost;
        let mut store = SettingsStore::open(&mut host, OWN_WORD);
        let mut profile = load_profile(&store);

        // --- The desktop these windows will be shown on: the screen, the
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

        // --- The one event mailbox and the wait-set the process parks on.
        // The mailbox id is unique by construction (the shared
        // `event_endpoint_for` naming rule: this task's never-reused kernel
        // id under a fixed tag) and never reserved; the bind is refused
        // otherwise. One mailbox serves every window and the icon bar.
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
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            event_endpoint,
            EVENT_TOKEN,
        ) != 0
        {
            return fail(EXIT_NO_EVENTS, "wait-set member refused");
        }
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            return fail(EXIT_NO_EVENTS, "memory-pressure wake refused");
        }

        // Forward this terminal's own inherited environment (USER, HOME,
        // LOGNAME, PATH, LANG, ...) to every shell it hosts, exactly as the
        // desktop session forwards it to every app it launches: a hosted
        // shell is the logged-in user's shell, so its prompt and its children
        // need the same identity and locale the session runs under. The one
        // variable this terminal owns is TERM, naming the emulator it
        // presents, so its own value replaces any inherited TERM (the shared
        // `shell_env` rule, host-tested). The environment is data and carries
        // no authority.
        let env = shell_env(TERM, (0..tairix_rt::env_count()).filter_map(tairix_rt::env));

        // --- The icon-bar presence, before any window of this process
        // exists. A declared presence belongs to the *process*, so declaring
        // it first is what makes the slot carry this terminal's menu and its
        // primary-click action from the moment it appears; declare it after a
        // window and the session derives a slot from that window meanwhile —
        // one that opens no menu and does nothing when clicked.
        //
        // A refused declaration is an answer, not a death: the terminal
        // simply has no slot of its own and its windows are still reachable
        // through the one the session derives from them.
        match appbar::declaration(event_endpoint) {
            Ok(bar) => {
                if let Err(err) = client.set_app_bar(&bar) {
                    report(&alloc::format!(
                        "the desktop refused this terminal's icon-bar presence ({err}); \
                         carrying on without one"
                    ));
                }
            }
            Err(err) => report(&alloc::format!(
                "this terminal's icon-bar menu is invalid ({err:?}); carrying on without one"
            )),
        }

        // --- The first window.
        let mut next_slot: u64 = 0;
        let Some((first, server)) = open_window(WindowContext {
            client: &mut client,
            event_endpoint,
            set,
            slot: next_slot,
            profile: &profile,
            theme: themes.active(),
            desktop: &desktop,
            env: &env,
        }) else {
            return fail(EXIT_NO_WINDOW, "no terminal window could be opened");
        };
        next_slot += 1;
        let mut windows: Vec<TerminalWindow> = alloc::vec![first];

        // --- The event loop: park on the wait-set and dispatch on the woken
        // member's token (never drain every source per wake — a blocking
        // receive on an idle source would wedge the loop). Each member's
        // readiness is a level peek, so work left undrained re-reports on the
        // next wait. The park carries a frame deadline only while some window
        // has an animated effect in force.
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
                if Errno::from_syscall(waited) == Errno::TimedOut {
                    // The frame deadline elapsed: advance every window's
                    // animation and repaint. Nothing else changed.
                    for open in &mut windows {
                        open.look.phase = open.look.phase.advance();
                        if open.present(&mut client).is_err() {
                            return fail(EXIT_CHANNEL_LOST, "present refused");
                        }
                    }
                    continue;
                }
                return fail(EXIT_CHANNEL_LOST, "wait-set lost");
            }
            match token {
                EVENT_TOKEN => {
                    let outcome = drain_events(
                        &mut windows,
                        &mut profile,
                        &mut desktop,
                        themes.active(),
                        event_endpoint,
                        server,
                    );
                    // An overlay that has asked to go leaves before anything
                    // this same outcome opens, so a menu row that chose
                    // *Settings* replaces its popup rather than stacking a
                    // second one over it.
                    for open in &mut windows {
                        if open.overlay.as_ref().is_some_and(|held| held.dismissed) {
                            if let Some(held) = open.overlay.take() {
                                held.close(&mut client);
                            }
                        }
                    }
                    match apply_outcome(
                        outcome,
                        &mut windows,
                        &mut client,
                        AppContext {
                            set,
                            event_endpoint,
                            server,
                            next_slot: &mut next_slot,
                            profile: &mut profile,
                            themes: &mut themes,
                            desktop: &mut desktop,
                            store: &mut store,
                            env: &env,
                        },
                    ) {
                        Applied::Running => {}
                        Applied::Ended => return 0,
                        Applied::Lost(reason) => return fail(EXIT_CHANNEL_LOST, reason),
                    }
                }
                PRESSURE_TOKEN if tairix_procinfo::pressure::refresh() => {
                    tairix_font::trim_glyph_cache();
                    // The passes' buffers are whole screens of per-pixel
                    // state that only matter while an effect is on, so they
                    // give first under pressure; the next frame starts clean.
                    for open in &mut windows {
                        open.look.state.clear();
                    }
                }
                token => {
                    // A per-window member: its shell wrote, or its shell
                    // exited. A token outside the live windows' pairs cannot
                    // occur (each is removed with its window), so an unknown
                    // one simply re-parks rather than acting on a value this
                    // program never minted.
                    let Some(index) = windows.iter().position(|open| {
                        open.shell_token() == token || open.child_token() == token
                    }) else {
                        continue;
                    };
                    let ended = if windows[index].shell_token() == token {
                        pump_shell(&mut windows[index], &mut client)
                    } else {
                        ShellEnd::Exited(drain_and_reap(&mut windows[index], &mut client))
                    };
                    match ended {
                        ShellEnd::Running => {}
                        ShellEnd::Lost(reason) => return fail(EXIT_CHANNEL_LOST, reason),
                        ShellEnd::Exited(reason) => {
                            // The shell this window hosted is gone, so the
                            // window is: hosting it was the window's whole
                            // purpose. Its siblings keep running.
                            windows.remove(index).close(&mut client, set);
                            if let Some(reason) = reason {
                                let _ =
                                    writeln!(Stderr, "terminal: shell failed to launch: {reason}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// What a window's shell channel wake concluded.
    enum ShellEnd {
        /// The shell wrote and the window repainted.
        Running,
        /// The shell exited; the terse reason to report, if it never got off
        /// the ground.
        Exited(Option<&'static str>),
        /// The channel itself failed: end the process fail-loud.
        Lost(&'static str),
    }

    /// Drain what `open`'s shell wrote and repaint that window.
    fn pump_shell(
        open: &mut TerminalWindow,
        client: &mut WindowClient<RtWindowTransport>,
    ) -> ShellEnd {
        match open.terminal.pump() {
            Ok(_) => {
                if open.present(client).is_err() {
                    return ShellEnd::Lost("present refused");
                }
                ShellEnd::Running
            }
            // End-of-stream: the shell exited (a clean `exit`, it was
            // killed, or — admitted by `spawn` but then unable to load its
            // own image — it failed asynchronously). What it last wrote is
            // already on screen; reap it and, if it never got off the
            // ground, state why (fail loud).
            Err(Errno::NotFound) => ShellEnd::Exited(reap_shell(open.shell_pid)),
            Err(_) => ShellEnd::Lost("shell channel lost"),
        }
    }

    /// Reap `open`'s exited shell, paint whatever output it left, and hand
    /// back the terse reason to report when it never got off the ground.
    fn drain_and_reap(
        open: &mut TerminalWindow,
        client: &mut WindowClient<RtWindowTransport>,
    ) -> Option<&'static str> {
        let reason = reap_shell(open.shell_pid);
        while open.terminal.pump().is_ok() {}
        let _ = open.present(client);
        reason
    }

    /// The process-wide state applying one drained outcome may reach.
    struct AppContext<'a, 'store> {
        /// The wait-set every window's members live on.
        set: u64,
        /// The one event mailbox.
        event_endpoint: u64,
        /// The session identity every window's create reply named.
        server: ProcId,
        /// The next window slot to mint.
        next_slot: &'a mut u64,
        /// The user's profile, shared by every window.
        profile: &'a mut Profile,
        /// The theme registry the appearance is switched in, and whose
        /// active theme every window draws with.
        themes: &'a mut ThemeRegistry,
        /// The desktop the windows are shown on.
        desktop: &'a mut Desktop,
        /// The app-data store handle every save publishes through.
        ///
        /// Its own lifetime is separate from the context's: the handle borrows
        /// the syscall host for the whole process, while a context is built
        /// afresh for each drained outcome.
        store: &'a mut SettingsStore<'store>,
        /// This terminal's inherited environment.
        env: &'a [Vec<u8>],
    }

    /// Whether the process carries on after an outcome was applied.
    enum Applied {
        /// Keep serving.
        Running,
        /// *Quit* was chosen: end cleanly.
        Ended,
        /// A channel died: end fail-loud with this reason.
        Lost(&'static str),
    }

    /// Apply one drained outcome to the window it names, or to the process.
    #[allow(clippy::too_many_lines)] // One dispatch over the whole outcome vocabulary; splitting it would hide the ordering.
    #[allow(clippy::needless_pass_by_value)] // The outcome is consumed here: applying it is what ends its life.
    fn apply_outcome(
        outcome: EventOutcome,
        windows: &mut Vec<TerminalWindow>,
        client: &mut WindowClient<RtWindowTransport>,
        ctx: AppContext<'_, '_>,
    ) -> Applied {
        // A window-scoped outcome names its window; one the list no longer
        // holds is an outcome for a window that has just closed, and there is
        // nothing left to apply it to (fail closed).
        let index = |windows: &[TerminalWindow], window: u64| {
            windows.iter().position(|open| open.window == window)
        };
        match outcome {
            EventOutcome::Continue => Applied::Running,
            EventOutcome::NewWindow => {
                // No count of its own: every resource a window costs is
                // already bounded by something derived from the machine and
                // enforced with a typed refusal — the session's per-client
                // frame budget, and this process's own stream, process, and
                // address-space limits. `open_window` states the reason on
                // stderr and the terminal carries on, so a second
                // hand-picked ceiling in front of those would only refuse
                // windows the machine could have given.
                let slot = *ctx.next_slot;
                if let Some((opened, _)) = open_window(WindowContext {
                    client,
                    event_endpoint: ctx.event_endpoint,
                    set: ctx.set,
                    slot,
                    profile: ctx.profile,
                    theme: ctx.themes.active(),
                    desktop: ctx.desktop,
                    env: ctx.env,
                }) {
                    *ctx.next_slot = slot.saturating_add(1);
                    windows.push(opened);
                }
                Applied::Running
            }
            EventOutcome::Quit => {
                for open in windows.drain(..) {
                    open.close(client, ctx.set);
                }
                Applied::Ended
            }
            EventOutcome::CloseWindow { window } => {
                let Some(index) = index(windows, window) else {
                    return Applied::Running;
                };
                windows.remove(index).close(client, ctx.set);
                // The terminal is not its windows: it keeps its icon-bar
                // slot with none open, and a click there opens the next.
                // Only *Quit* ends it.
                Applied::Running
            }
            EventOutcome::OpenMenu { window, at } => {
                let Some(index) = index(windows, window) else {
                    return Applied::Running;
                };
                open_window_menu(client, &mut windows[index], at);
                Applied::Running
            }
            EventOutcome::OpenSheet { window } => {
                let Some(index) = index(windows, window) else {
                    return Applied::Running;
                };
                let mode = windows[index].mode;
                let sheet = Box::new(Settings::new(ctx.profile));
                let opened = open_overlay(
                    client,
                    window,
                    ctx.server,
                    ctx.event_endpoint,
                    sheet,
                    &mode,
                    ctx.themes.active(),
                    ctx.desktop,
                );
                windows[index].set_overlay(client, opened);
                Applied::Running
            }
            EventOutcome::OverlayChanged { window } => {
                let Some(index) = index(windows, window) else {
                    return Applied::Running;
                };
                // Only the overlay's own pixels moved, so the terminal's
                // window is left exactly as it is.
                let scale = ctx.desktop.scale();
                if let Some(open) = windows[index].overlay.as_mut() {
                    if present_overlay(open, ctx.themes.active(), scale, client).is_err() {
                        return Applied::Lost("overlay present refused");
                    }
                }
                Applied::Running
            }
            EventOutcome::Repaint { window } => {
                let Some(index) = index(windows, window) else {
                    return Applied::Running;
                };
                // The session dropped this window's pixels (a redraw
                // request) or the grid was blanked, so the retained picture
                // cannot be trusted.
                windows[index].screen.invalidate();
                if windows[index].present(client).is_err() {
                    return Applied::Lost("present refused");
                }
                Applied::Running
            }
            EventOutcome::ProfileChanged { window } | EventOutcome::ProfileRestored { window } => {
                // The user changed a setting, or asked for the defaults back.
                // The profile is the *user's*, so every window re-derives its
                // look, re-applies its blur, reshapes its grid (its pty
                // follows), and repaints; only the sheet that made the change
                // re-presents its own popup.
                if matches!(outcome, EventOutcome::ProfileRestored { .. }) {
                    restore_profile(ctx.store, ctx.profile);
                    // The sheet's own copy is stale now: what applies came
                    // back from the store's lower layers, not from the widget.
                    if let Some(index) = index(windows, window) {
                        if let Some(held) = windows[index].overlay.as_mut() {
                            held.adopt_profile(ctx.profile);
                        }
                    }
                } else {
                    persist_profile(ctx.store, ctx.profile);
                }
                for open in windows.iter_mut() {
                    open.look
                        .refresh(ctx.profile, ctx.themes.active(), ctx.desktop);
                    apply_blur(client, open.window, ctx.profile);
                    let (cols, rows) =
                        grid_dims(open.mode.width_px, open.mode.height_px, open.look.font);
                    let _ = open.terminal.resize(cols, rows);
                    let _ = tairix_rt::pty_set_size(open.pty_master, rows, cols);
                    // New colours or a new face: every retained pixel is
                    // stale, and the diff cannot see either.
                    open.screen.invalidate();
                    if open.present(client).is_err() {
                        return Applied::Lost("present refused");
                    }
                }
                let scale = ctx.desktop.scale();
                if let Some(index) = index(windows, window) {
                    if let Some(open) = windows[index].overlay.as_mut() {
                        if present_overlay(open, ctx.themes.active(), scale, client).is_err() {
                            return Applied::Lost("overlay present refused");
                        }
                    }
                }
                Applied::Running
            }
            EventOutcome::Resized {
                window,
                width_px,
                height_px,
            } => {
                let Some(index) = index(windows, window) else {
                    return Applied::Running;
                };
                // Re-map the frame region at the new client size, reshape the
                // grid, and tell the shell (via the pty window size) so its
                // prompt and any full-screen program re-lay-out. A refused or
                // unallocatable re-map keeps the current window rather than
                // failing the app: the grid and pty size are only updated
                // once the new region is adopted, so the screen never claims
                // a geometry the surface cannot hold.
                //
                // The granted client is first snapped down to a whole number
                // of cells, so no partial-cell strip of dead background is
                // left at the right or bottom edge. Snapping is idempotent,
                // so the size this re-maps to is already snapped and the
                // `Resized` it draws back snaps to itself: one step, and it
                // cannot oscillate. Re-mapping is skipped entirely when the
                // snapped size is the one already in force.
                let open = &mut windows[index];
                let (snapped_w, snapped_h) = snap_to_cells(width_px, height_px, open.look.font);
                if (snapped_w, snapped_h) == (open.mode.width_px, open.mode.height_px) {
                    return Applied::Running;
                }
                let new_mode = mode_for(snapped_w, snapped_h);
                if let Some(frames) = resize_frames(client, open.window, &new_mode) {
                    // Adopting drops the old region, which unmaps it.
                    open.frames = frames;
                    open.mode = new_mode;
                    let (cols, rows) = grid_dims(snapped_w, snapped_h, open.look.font);
                    let _ = open.terminal.resize(cols, rows);
                    let _ = tairix_rt::pty_set_size(open.pty_master, rows, cols);
                    // What the passes remember is the shape of the old
                    // screen; a resized one must not ghost it.
                    open.look.state.clear();
                    if open.present(client).is_err() {
                        return Applied::Lost("present refused");
                    }
                }
                Applied::Running
            }
            EventOutcome::DesktopChanged => {
                // The scale and/or appearance changed: re-apply the theme and
                // re-derive every window from the new scale (each pty
                // follows). `desktop` itself was already updated inside
                // `drain_events`.
                ctx.themes.set_appearance(ctx.desktop.appearance());
                for open in windows.iter_mut() {
                    open.look
                        .refresh(ctx.profile, ctx.themes.active(), ctx.desktop);
                    let (cols, rows) =
                        grid_dims(open.mode.width_px, open.mode.height_px, open.look.font);
                    let _ = open.terminal.resize(cols, rows);
                    let _ = tairix_rt::pty_set_size(open.pty_master, rows, cols);
                    // A new appearance or scale: new colours and a new face,
                    // so nothing retained still holds.
                    open.screen.invalidate();
                    if open.present(client).is_err() {
                        return Applied::Lost("present refused");
                    }
                }
                Applied::Running
            }
            EventOutcome::ChannelLost => Applied::Lost("event mailbox lost"),
        }
    }

    /// What the event-mailbox drain concluded.
    ///
    /// Every window-scoped variant names the window it belongs to, because
    /// one mailbox serves every window this process owns: an outcome that did
    /// not say which window it was for could be applied to the wrong one.
    enum EventOutcome {
        /// Every pending event was applied and nothing on screen changed.
        Continue,
        /// Open another terminal window: the icon-bar slot's primary click,
        /// or its *New window* row.
        NewWindow,
        /// Close every window and end the process: the icon-bar menu's
        /// *Quit* row.
        Quit,
        /// This window is done — the desktop asked, the user chose *Close*,
        /// or its shell's stdin is gone. Its siblings keep running, and so
        /// does the process when it was the last.
        CloseWindow {
            /// The window that is closing.
            window: u64,
        },
        /// This window must be repainted whole: the session dropped its
        /// pixels and asked for them again, or its grid was blanked outright.
        Repaint {
            /// The window to repaint.
            window: u64,
        },
        /// Only this window's open overlay's own pixels changed; re-present
        /// its popup and leave the window alone.
        OverlayChanged {
            /// The window whose overlay moved.
            window: u64,
        },
        /// A secondary press asked for this window's menu at this
        /// client-local point; the caller asks the desktop to open it there.
        OpenMenu {
            /// The window the press landed on.
            window: u64,
            /// Where the press landed, relative to the client origin.
            at: Point,
        },
        /// The *Settings* command asked for the settings sheet over this
        /// window; the caller opens its popup over the client.
        OpenSheet {
            /// The window the sheet belongs to.
            window: u64,
        },
        /// The user changed a setting from this window's sheet: re-derive
        /// every window's look, store the profile, and repaint.
        ProfileChanged {
            /// The window whose sheet made the change.
            window: u64,
        },
        /// The user asked this window's sheet for *Restore defaults*: remove
        /// the user's own opinions from the store, adopt the profile the
        /// layers beneath imply, and tell the sheet what it is.
        ProfileRestored {
            /// The window whose sheet asked.
            window: u64,
        },
        /// The window manager resized this window to a new client size (a
        /// drag-resize that settled, or a maximize/restore); the caller
        /// re-maps its frame region, reshapes its grid, and updates its pty
        /// window size. Any events queued behind it re-report on the next
        /// wake (the port readiness is level-triggered).
        Resized {
            /// The resized window.
            window: u64,
            /// New client width in pixels.
            width_px: u32,
            /// New client height in pixels.
            height_px: u32,
        },
        /// The desktop changed (screen size, scale, or appearance); already
        /// adopted by [`Desktop::apply`] before this is returned. It is a
        /// property of the seat, so it reaches every window.
        DesktopChanged,
        /// The mailbox itself failed: end fail-loud.
        ChannelLost,
    }

    /// Carry out `command` for the window it was chosen in, reporting what
    /// the caller must now do.
    fn run_command<S: ShellSource>(
        command: Command,
        window: u64,
        terminal: &mut Terminal<S>,
        profile: &mut Profile,
    ) -> EventOutcome {
        match command {
            Command::Settings => EventOutcome::OpenSheet { window },
            Command::Larger => {
                profile.enlarge();
                EventOutcome::ProfileChanged { window }
            }
            Command::Smaller => {
                profile.reduce();
                EventOutcome::ProfileChanged { window }
            }
            Command::ActualSize => {
                profile.font_size_px = Profile::default().font_size_px;
                EventOutcome::ProfileChanged { window }
            }
            Command::Clear => {
                terminal.clear();
                EventOutcome::Repaint { window }
            }
            Command::Close => EventOutcome::CloseWindow { window },
        }
    }

    /// Route one pointer event delivered for the open sheet's own popup
    /// window into that sheet.
    ///
    /// The coordinates in a popup's events are popup-local, so the sheet is
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
        let sheet = &mut overlay.sheet;
        for event in pointer_input_events(action, at) {
            match sheet.on_pointer(&event, viewport, scale, theme, &mut damage) {
                SheetOutcome::Ignored => {}
                SheetOutcome::Changed => routing = OverlayRouting::Redraw,
                SheetOutcome::Edited => {
                    *profile = *sheet.profile();
                    routing = OverlayRouting::Edited;
                }
                SheetOutcome::Restore => return OverlayRouting::Restore,
                SheetOutcome::Dismissed => {
                    *profile = *sheet.profile();
                    return OverlayRouting::Closed;
                }
            }
        }
        routing
    }

    /// Route one key press delivered for the open sheet's own popup window
    /// into that sheet.
    fn route_overlay_key(
        overlay: &mut Overlay,
        profile: &mut Profile,
        key: tairix_abi::input::KeyInput,
        scale: Scale,
        theme: &Theme,
    ) -> OverlayRouting {
        let InputEvent::KeyPressed { key, modifiers } = key_input_event(key) else {
            return OverlayRouting::Nothing;
        };
        let viewport = overlay.viewport();
        let mut damage = damage::sink();
        let sheet = &mut overlay.sheet;
        let outcome = sheet.on_key(key, modifiers, viewport, scale, theme, &mut damage);
        if matches!(outcome, SheetOutcome::Edited | SheetOutcome::Dismissed) {
            *profile = *sheet.profile();
        }
        match outcome {
            SheetOutcome::Ignored => OverlayRouting::Nothing,
            SheetOutcome::Changed => OverlayRouting::Redraw,
            SheetOutcome::Edited => OverlayRouting::Edited,
            SheetOutcome::Restore => OverlayRouting::Restore,
            SheetOutcome::Dismissed => OverlayRouting::Closed,
        }
    }

    /// What routing an event into the open settings sheet concluded.
    enum OverlayRouting {
        /// Nothing to do.
        Nothing,
        /// The sheet's own pixels changed; re-present its popup.
        Redraw,
        /// The sheet edited the profile.
        Edited,
        /// The sheet asked for *Restore defaults*: the user's own opinions
        /// are to be removed and the profile the remaining store layers
        /// imply read back.
        Restore,
        /// The sheet asked to close, having possibly edited the profile.
        Closed,
    }

    /// Drain every queued window event (non-blocking — the wait-set wake
    /// said at least one is pending).
    ///
    /// One mailbox serves the whole process, so each event is demuxed on the
    /// window id it carries — the terminal window, or the popup one of them
    /// has open — and an event that carries **no** window id is
    /// application-scoped: an icon-bar click or menu outcome, which names the
    /// emulator rather than any one of its windows.
    ///
    /// A short frame or a sender other than the desktop session is dropped,
    /// never applied: the mailbox is open to any capable sender, so the
    /// kernel-attested origin is the authentication. A malformed frame from
    /// the authenticated session is likewise refused (never guessed at).
    #[allow(clippy::too_many_lines)] // One dispatch over the whole window vocabulary; splitting it would hide the routing order.
    fn drain_events(
        windows: &mut [TerminalWindow],
        profile: &mut Profile,
        desktop: &mut Desktop,
        theme: &Theme,
        endpoint: u64,
        server: ProcId,
    ) -> EventOutcome {
        let scale = desktop.scale();
        let mut redrawn: Option<u64> = None;
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
                    // An application-scoped event names the emulator rather
                    // than a window: the icon bar's own click and menu
                    // outcomes. The row id is this terminal's own, so an id
                    // it never declared names no command and is dropped.
                    match event {
                        WindowEvent::AppBarDefault => return EventOutcome::NewWindow,
                        WindowEvent::AppBarMenu { item } => {
                            return match bar_command(item) {
                                Some(BarCommand::NewWindow) => EventOutcome::NewWindow,
                                Some(BarCommand::Quit) => EventOutcome::Quit,
                                None => EventOutcome::Continue,
                            };
                        }
                        _ => {}
                    }
                    // Everything else is window-scoped: resolve which of
                    // this process's windows (or which window's popup) it
                    // belongs to. An id neither names is a window that has
                    // just closed, and the event has nowhere to land.
                    let Some(window_id) = event.window_id() else {
                        continue;
                    };
                    let Some(index) = windows.iter().position(|open| {
                        open.window == window_id
                            || open
                                .overlay
                                .as_ref()
                                .is_some_and(|held| held.window == window_id)
                    }) else {
                        continue;
                    };
                    let open = &mut windows[index];
                    let window = open.window;
                    let for_popup = open.window != window_id;
                    match event {
                        WindowEvent::Key { key, .. } if for_popup => {
                            let Some(held) = open.overlay.as_mut() else {
                                continue;
                            };
                            match route_overlay_key(held, profile, key, scale, theme) {
                                OverlayRouting::Nothing => {}
                                OverlayRouting::Redraw => redrawn = Some(window),
                                OverlayRouting::Edited => {
                                    return EventOutcome::ProfileChanged { window }
                                }
                                OverlayRouting::Restore => {
                                    return EventOutcome::ProfileRestored { window }
                                }
                                OverlayRouting::Closed => {
                                    held.dismissed = true;
                                    return EventOutcome::ProfileChanged { window };
                                }
                            }
                        }
                        WindowEvent::Key { key, .. } => match route_key(&mut open.terminal, key) {
                            KeyRouting::Nothing => {}
                            KeyRouting::Command(command) => {
                                return finish(
                                    run_command(command, window, &mut open.terminal, profile),
                                    redrawn,
                                )
                            }
                            KeyRouting::ShellGone => return EventOutcome::CloseWindow { window },
                        },
                        WindowEvent::Pointer { x, y, action, .. } if for_popup => {
                            let Some(held) = open.overlay.as_mut() else {
                                continue;
                            };
                            let at = pointer_point(x, y);
                            match route_overlay_pointer(held, profile, action, at, scale, theme) {
                                OverlayRouting::Nothing => {}
                                OverlayRouting::Redraw => redrawn = Some(window),
                                OverlayRouting::Edited => {
                                    return EventOutcome::ProfileChanged { window }
                                }
                                OverlayRouting::Restore => {
                                    return EventOutcome::ProfileRestored { window }
                                }
                                OverlayRouting::Closed => {
                                    held.dismissed = true;
                                    return EventOutcome::ProfileChanged { window };
                                }
                            }
                        }
                        // The one answer the desktop owes an open. An id
                        // that names anything else answers a gesture already
                        // settled, so acting on it would run a stale command.
                        WindowEvent::MenuClosed {
                            open_id, outcome, ..
                        } if !for_popup && open.menu == Some(open_id) => {
                            open.menu = None;
                            match outcome {
                                MenuOutcome::Chosen(item) => {
                                    // A row id this terminal never declared
                                    // names no command and is dropped.
                                    if let Some(command) = Command::from_item(item) {
                                        return finish(
                                            run_command(
                                                command,
                                                window,
                                                &mut open.terminal,
                                                profile,
                                            ),
                                            redrawn,
                                        );
                                    }
                                }
                                MenuOutcome::Dismissed => {}
                                MenuOutcome::Refused(reason) => {
                                    report(&alloc::format!(
                                        "the desktop showed no menu ({reason:?})"
                                    ));
                                }
                            }
                        }
                        WindowEvent::Pointer { x, y, action, .. } => {
                            // The sheet is modal, so a press that lands on
                            // the terminal instead dismisses it and reaches
                            // nothing else. Otherwise a secondary press asks
                            // the desktop for this window's menu at that
                            // point, and every other pointer event is a
                            // no-op: the screen is shell-driven and the
                            // emulator keeps no scrollback for a wheel to
                            // move.
                            if let Some(held) = open.overlay.as_mut() {
                                if matches!(action, PointerAction::Pressed(_)) {
                                    held.dismissed = true;
                                    return EventOutcome::Continue;
                                }
                            } else if action
                                == PointerAction::Pressed(
                                    tairix_abi::input::PointerButtonCode::Secondary,
                                )
                            {
                                return EventOutcome::OpenMenu {
                                    window,
                                    at: pointer_point(x, y),
                                };
                            }
                        }
                        // A popup wears no close control, so a close asked of
                        // one can only be the session tearing it down: let the
                        // overlay go rather than closing the window.
                        WindowEvent::CloseRequested { .. } if for_popup => {
                            if let Some(held) = open.overlay.as_mut() {
                                held.dismissed = true;
                            }
                            return EventOutcome::Continue;
                        }
                        WindowEvent::CloseRequested { .. } => {
                            return EventOutcome::CloseWindow { window }
                        }
                        // The session dropped a window's pixels under memory
                        // pressure: repaint whichever window lost them.
                        WindowEvent::RedrawRequested { .. } if for_popup => {
                            return EventOutcome::OverlayChanged { window }
                        }
                        WindowEvent::RedrawRequested { .. } => {
                            return EventOutcome::Repaint { window }
                        }
                        // Nobody can see this window, so the session gave its
                        // copy of the pixels back and unmapped the region. Let
                        // go of this side too — the pages go only when both do
                        // — and paint nothing: the redraw request that follows
                        // the window being shown again re-attaches a fresh
                        // region and fills it. A popup is never hidden, so
                        // only the top-level window's region is released here.
                        WindowEvent::ContentReleased { .. } => {
                            if !for_popup {
                                open.frames.release();
                            }
                        }
                        // The window manager resized the window (a live
                        // drag-resize sample, or a maximize/restore): hand the
                        // new client size back to the caller, which re-maps
                        // the frame region, reshapes the grid, and updates the
                        // pty window size. A run of drag samples has already
                        // folded to its newest in the shared reader, so this
                        // reshapes once per size the grid actually takes.
                        // Returning here leaves any events queued behind it
                        // for the next wake (level-triggered peek).
                        WindowEvent::Resized {
                            width_px,
                            height_px,
                            ..
                        } if !for_popup => {
                            return EventOutcome::Resized {
                                window,
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
                        // still reachable through the bar's hover picker; the
                        // screen is redrawn from the shell on demand). A
                        // desktop change was already adopted above (or, on
                        // refusal, stated and left the last good state
                        // standing); either way there is nothing further to do
                        // with the event itself. An icon-bar event was handled
                        // before the window demux. These are honest no-ops,
                        // not deferred work.
                        //
                        // A `Resized` for a popup is likewise nothing: a popup
                        // is neither decorated nor resizable, so it has no size
                        // of its own for the session to change.
                        //
                        // A secondary press on Close asks to leave what the
                        // window is showing; the terminal has nothing to leave
                        // but the window itself, and a primary press already
                        // closes it.
                        //
                        // A `MenuClosed` reaching here failed the guard above
                        // — it names no open this window is waiting on — and a
                        // stale answer is dropped rather than acted on. The
                        // terminal's menu declares no panel row, so no chain
                        // of its own ever asks it for a surface.
                        WindowEvent::AlternateCloseRequested { .. }
                        | WindowEvent::AppBarDefault
                        | WindowEvent::AppBarMenu { .. }
                        | WindowEvent::MenuClosed { .. }
                        | WindowEvent::Focus { .. }
                        | WindowEvent::Scrolled { .. }
                        | WindowEvent::Minimized { .. }
                        | WindowEvent::Resized { .. }
                        | WindowEvent::FilePicked { .. }
                        | WindowEvent::PickCancelled { .. }
                        | WindowEvent::DesktopChanged { .. } => {}
                    }
                }
                Err(err) if Errno::from_syscall(err) == Errno::WouldBlock => {
                    return match redrawn {
                        Some(window) => EventOutcome::OverlayChanged { window },
                        None => EventOutcome::Continue,
                    };
                }
                Err(_) => return EventOutcome::ChannelLost,
            }
        }
    }

    /// The command an icon-bar menu row names, or `None` for an id this
    /// terminal never declared.
    ///
    /// The mapping is the declaration's own
    /// ([`tairix_terminal::appbar`]), so the rows the bar draws and the
    /// commands they run are stated once.
    fn bar_command(item: AppMenuItemId) -> Option<BarCommand> {
        BarCommand::from_item(item)
    }

    /// Fold a pending overlay redraw into `outcome`: a command that concluded
    /// nothing still re-presents the popup when an earlier event in the same
    /// drain changed that window's overlay pixels.
    fn finish(outcome: EventOutcome, redrawn: Option<u64>) -> EventOutcome {
        match (outcome, redrawn) {
            (EventOutcome::Continue, Some(window)) => EventOutcome::OverlayChanged { window },
            (other, _) => other,
        }
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
