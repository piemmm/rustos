//! The `settings.app` bundle's `Run` entry point: the windowed Settings
//! application (`plans/NEW-DESKTOP-SETTINGS.md`).
//!
//! Everything with behaviour worth testing lives in the host-tested shell
//! (`tairix_settings`); this binary only composes it over the live window
//! channel, exactly as `userland/apps/widgets` composes its gallery:
//!
//! * one `shm_create`d frame region granted to the reserved window endpoint;
//! * one `port_bind`-bound event mailbox the app **parks** on through its
//!   wait-set — never a poll loop — accepting only events whose
//!   kernel-attested sender is the desktop session the create reply named;
//! * the `WindowClient` calls and the `WindowEvents` typed wait over it.
//!
//! The window is resizable: a `Resized` event re-maps the frame region and
//! the shell lays itself out to the new client, shedding the strip where it
//! no longer fits. Every bring-up refusal exits fail-loud with a reserved
//! code and a stated reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use core::cell::Cell;

    use tairix_abi::driver::display::{DamageRect, DisplayMode};
    use tairix_abi::input::KeyInput;
    use tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS;
    use tairix_abi::window_ipc::{AppBarClick, PointerAction, WindowEvent, WindowSizing};
    use tairix_abi::{Errno, ProcId};
    use tairix_geometry::{Point, Rect, Region, Scale};
    use tairix_icon::NoArtwork;
    use tairix_input::InputEvent;
    use tairix_rt::io::{Stderr, Write};
    use tairix_settings::Shell;
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_window::app::{self, AppWindow, ShellError, Wake, EXIT_CHANNEL_LOST};
    use tairix_window::{
        key_input_event, pointer_input_events, pointer_point, present_damage, Desktop, EventDrain,
        EventError, EventMailbox, EventSource, Parked, Repaint, WindowClient, WindowEvents,
    };

    /// The window's logical width at the reference density: the strip plus a
    /// content column wide enough for a pane's widest row.
    const WIN_WIDTH: u32 = 780;
    /// The window's logical height at the reference density.
    const WIN_HEIGHT: u32 = 600;
    /// The narrowest logical client the window may be resized to: enough for
    /// the content column alone, the strip having been shed.
    const MIN_WIDTH: u32 = 320;
    /// The shortest logical client the window may be resized to.
    const MIN_HEIGHT: u32 = 240;

    /// State the abnormal-exit reason on `stderr` (fail loud) and hand back
    /// `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "settings: {reason}");
        code
    }

    /// State a shared-shell bring-up refusal and hand its reserved code back.
    fn fail_shell(err: ShellError) -> i32 {
        let _ = writeln!(Stderr, "settings: {err}");
        err.code()
    }

    /// Declare this application's icon-bar presence: the shared convention's
    /// information row and *Quit*.
    ///
    /// A refused declaration is an answer, not a death: the application says
    /// so and carries on with no slot of its own.
    fn declare_app_bar(client: &mut WindowClient<app::RtWindowTransport>, endpoint: u64) {
        match tairix_window::info_and_quit(endpoint, AppBarClick::RaiseOrOpen) {
            Ok(bar) => {
                if let Err(err) = client.set_app_bar(&bar) {
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop refused this application's icon-bar presence \
                         ({err}); carrying on without one"
                    );
                }
            }
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: this application's icon-bar menu is invalid ({err:?}); carrying \
                     on without one"
                );
            }
        }
    }

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty.
    struct RtEventSource<'a> {
        mailbox: EventMailbox,
        set: u64,
        /// Set when the park woke for a desktop change, cleared when the loop
        /// adopts it.
        desktop_moved: &'a Cell<bool>,
    }

    impl EventDrain for RtEventSource<'_> {
        fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
            self.mailbox.try_next(event)
        }
    }

    impl EventSource for RtEventSource<'_> {
        fn park(&mut self) -> Result<Parked, Errno> {
            match app::park(self.set)? {
                Wake::PressureChanged => {
                    tairix_font::trim_glyph_cache();
                    Ok(Parked::Served)
                }
                Wake::DesktopChanged => {
                    self.desktop_moved.set(true);
                    Ok(Parked::Interrupted)
                }
                Wake::Event | Wake::PressureUnchanged | Wake::App(_) => Ok(Parked::Served),
            }
        }
    }

    /// The app's channel to the desktop, the window it may or may not have
    /// open, and the client extent that window is currently showing.
    struct SettingsWindow {
        window: AppWindow,
        mode: DisplayMode,
    }

    impl SettingsWindow {
        /// Open a window at the current mode and present the shell's first
        /// frame, answering the session's [`ProcId`] or the reserved exit code
        /// for the refusal.
        fn open(
            &mut self,
            event_endpoint: u64,
            shell: &Shell,
            theme: &Theme,
            scale: Scale,
        ) -> Result<ProcId, i32> {
            let sizing = WindowSizing::Resizable {
                min_width_px: scale.scale_length(MIN_WIDTH),
                min_height_px: scale.scale_length(MIN_HEIGHT),
                // No ceiling: a wider window seats more of a pane's rows and
                // a taller one scrolls less, at every size it is given.
                max_width_px: 0,
                max_height_px: 0,
            };
            let server = self
                .window
                .open(event_endpoint, &self.mode, "settings", sizing)
                .map_err(fail_shell)?;
            if self
                .present(shell, theme, scale, DamageRect::full(&self.mode))
                .is_err()
            {
                self.close();
                return Err(fail(EXIT_CHANNEL_LOST, "present refused"));
            }
            Ok(server)
        }

        /// Close the open window, leaving the app on the icon bar.
        fn close(&mut self) {
            let _ = self.window.close();
        }

        /// The client rectangle the window is showing.
        fn viewport(&self) -> Rect {
            Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
        }

        /// Draw the shell and present `damage`.
        fn present(
            &mut self,
            shell: &Shell,
            theme: &Theme,
            scale: Scale,
            damage: DamageRect,
        ) -> Result<(), Errno> {
            let viewport = self.viewport();
            self.window.present(damage, |surface| {
                shell.render(surface, viewport, scale, theme, &mut NoArtwork);
            })
        }
    }

    /// What one delivered event concluded.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Acted {
        /// Nothing on screen changed.
        Idle,
        /// The shell changed and must be re-presented.
        Changed,
        /// The whole client changed and no report could describe it.
        Whole,
        /// Close the window, leaving the app on the icon bar.
        Close,
        /// Open a window, the app having none.
        Open,
        /// End the program.
        Quit,
    }

    /// Apply one delivered event to the shell.
    fn apply_event(
        surface: &mut SettingsWindow,
        shell: &mut Shell,
        theme: &Theme,
        scale: Scale,
        event: &WindowEvent,
        damage: &mut Region,
    ) -> Acted {
        let viewport = surface.viewport();
        let changed = |acted: bool| {
            if acted {
                Acted::Changed
            } else {
                Acted::Idle
            }
        };
        match event {
            WindowEvent::CloseRequested { .. } => Acted::Close,
            WindowEvent::AppBarMenu { item } if tairix_window::is_quit(*item) => Acted::Quit,
            WindowEvent::AppBarDefault => Acted::Open,
            // The desktop resized the client: re-map the frame region to the
            // new extent, then lay the shell out to it.
            WindowEvent::Resized {
                width_px,
                height_px,
                ..
            } => {
                let mode = app::mode_for(*width_px, *height_px);
                if surface.window.resize(mode) {
                    surface.mode = mode;
                } else {
                    // A refused resize leaves the old geometry standing and
                    // still drawable, so the window keeps the size it had.
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop refused a resize; the window keeps its size"
                    );
                }
                // The reported client extent is what the shell lays out to
                // either way, so the whole window is redrawn regardless.
                shell.lay_out(surface.viewport(), scale, theme);
                Acted::Whole
            }
            WindowEvent::Key {
                key: pressed @ KeyInput::Pressed { .. },
                ..
            } => match key_input_event(*pressed) {
                InputEvent::KeyPressed { key, modifiers } => changed(
                    shell
                        .on_key(key, modifiers, viewport, scale, theme, damage)
                        .changed(),
                ),
                _ => Acted::Idle,
            },
            WindowEvent::Pointer { x, y, action, .. } => changed(apply_pointer(
                shell,
                pointer_point(*x, *y),
                *action,
                viewport,
                scale,
                theme,
                damage,
            )),
            WindowEvent::Scrolled { dx, dy, .. } => {
                let scroll = InputEvent::PointerScrolled { dx: *dx, dy: *dy };
                changed(
                    shell
                        .on_pointer(&scroll, viewport, scale, theme, damage)
                        .changed(),
                )
            }
            // A redraw needs nothing here: the client library re-presents the
            // last frame and the shell it drew has not changed. The rest are
            // events this surface does not act on — it opens no chain of the
            // desktop's, declares no file association, and owns no desktop
            // layer — and `ContentReleased` is the caller's, which owns the
            // region it lets go of.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::AppBarMenu { .. }
            | WindowEvent::MenuClosed { .. }
            | WindowEvent::TerrainChanged { .. }
            | WindowEvent::LayerPointer { .. }
            | WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::RedrawRequested { .. }
            | WindowEvent::ContentReleased { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. }
            | WindowEvent::OpenRequested => Acted::Idle,
        }
    }

    /// Route one wire pointer event: a move to `at` to sync the pointer, then
    /// the press or release the action names.
    fn apply_pointer(
        shell: &mut Shell,
        at: Point,
        action: PointerAction,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let mut acted = false;
        for input in pointer_input_events(action, at) {
            acted |= shell
                .on_pointer(&input, viewport, scale, theme, damage)
                .changed();
        }
        acted
    }

    /// Adopt the desktop the session published, if the park said it moved.
    fn adopt_desktop(
        desktop: &mut Desktop,
        themes: &mut ThemeRegistry,
        moved: &Cell<bool>,
    ) -> bool {
        if !moved.replace(false) {
            return false;
        }
        match app::adopt_desktop(desktop, themes) {
            Ok(changed) => changed,
            Err(err) => {
                let _ = writeln!(Stderr, "settings: desktop change refused: {err}");
                false
            }
        }
    }

    /// The event loop: park, apply, repaint.
    fn run_event_loop(
        surface: &mut SettingsWindow,
        desktop: &mut Desktop,
        themes: &mut ThemeRegistry,
        shell: &mut Shell,
        event_endpoint: u64,
        desktop_moved: &Cell<bool>,
        mut events: WindowEvents<RtEventSource<'_>>,
    ) -> i32 {
        loop {
            let event = match events.wait(surface.window.client()) {
                Ok(Some(event)) => event,
                // A wait that ended without an event is the desktop notice; a
                // malformed frame from the authenticated session is refused
                // rather than guessed at. Either way the round is the
                // re-theme alone.
                Ok(None) | Err(EventError::Undecodable(_)) => {
                    if adopt_desktop(desktop, themes, desktop_moved) {
                        // A new density or theme re-measures every length the
                        // layout is derived from.
                        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                        let whole = DamageRect::full(&surface.mode);
                        if surface
                            .present(shell, themes.active(), desktop.scale(), whole)
                            .is_err()
                        {
                            return fail(EXIT_CHANNEL_LOST, "present refused");
                        }
                    }
                    continue;
                }
                Err(EventError::Mailbox(_)) => {
                    return fail(EXIT_CHANNEL_LOST, "event channel lost")
                }
            };

            let redraw = adopt_desktop(desktop, themes, desktop_moved);
            if redraw {
                shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
            }
            let mut damage = tairix_controls::damage::sink();
            let acted = apply_event(
                surface,
                shell,
                themes.active(),
                desktop.scale(),
                &event,
                &mut damage,
            );
            match acted {
                Acted::Quit => {
                    surface.close();
                    return 0;
                }
                Acted::Close => {
                    surface.close();
                    continue;
                }
                Acted::Open => {
                    // A refusal is already stated; the slot is still there to
                    // try again from.
                    let _ = surface.open(event_endpoint, shell, themes.active(), desktop.scale());
                    continue;
                }
                Acted::Idle | Acted::Changed | Acted::Whole => {}
            }
            if matches!(event, WindowEvent::ContentReleased { .. }) {
                surface.window.release_frames();
                continue;
            }
            let repaint = match (redraw || acted == Acted::Whole, acted == Acted::Changed) {
                (true, _) => Repaint::Whole,
                (false, true) => Repaint::Reported,
                (false, false) => Repaint::Nothing,
            };
            let Some(area) = present_damage(&surface.mode, repaint, &damage) else {
                continue;
            };
            if surface
                .present(shell, themes.active(), desktop.scale(), area)
                .is_err()
            {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
    }

    /// Program entry point.
    fn main() -> i32 {
        let _ = tairix_rt::latency_watch(DEFAULT_FRAME_BUDGET_NS);

        let mut window = AppWindow::new();
        let (mut desktop, mut themes) = match app::bring_up_desktop(window.client()) {
            Ok(pair) => pair,
            Err(err) => return fail_shell(err),
        };
        let (initial_w, initial_h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mut surface = SettingsWindow {
            window,
            mode: app::mode_for(initial_w, initial_h),
        };

        let binding = match app::bind_event_mailbox() {
            Ok(binding) => binding,
            Err(err) => return fail_shell(err),
        };
        let event_endpoint = binding.endpoint();
        declare_app_bar(surface.window.client(), event_endpoint);

        // An empty registry would leave the window nothing to show at all, so
        // it ends fail-loud rather than opening a blank frame.
        let Some(mut shell) = Shell::new() else {
            return fail(
                EXIT_CHANNEL_LOST,
                "the settings registry holds no categories",
            );
        };
        let server = match surface.open(event_endpoint, &shell, themes.active(), desktop.scale()) {
            Ok(server) => server,
            Err(code) => return code,
        };

        let desktop_moved = Cell::new(false);
        let events = WindowEvents::new(RtEventSource {
            mailbox: EventMailbox::new(event_endpoint, server),
            set: binding.set(),
            desktop_moved: &desktop_moved,
        });
        run_event_loop(
            &mut surface,
            &mut desktop,
            &mut themes,
            &mut shell,
            event_endpoint,
            &desktop_moved,
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
