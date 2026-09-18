//! The `settings.app` bundle's `Run` entry point: the windowed Settings
//! application (`plans/NEW-DESKTOP-SETTINGS.md`).
//!
//! # It browses pictures it may not read
//!
//! The Wallpaper pane offers the shipped store, and this application holds
//! no filesystem capability and no sandbox to decode a picture with. Both
//! are *served*: the desktop session lists the store once and answers a
//! catalog page, and renders one candidate at a time into a shared-memory
//! region this program created and granted — which is the one thing its
//! `CAP_SHM` already allows. So there is one sandboxed decode path on the
//! desktop rather than two, and no untrusted picture is ever decoded in the
//! address space of the application that browses them.
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
    extern crate alloc;

    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::Cell;

    use tairix_abi::driver::display::{DamageRect, DisplayMode};
    use tairix_abi::input::KeyInput;
    use tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS;
    use tairix_abi::pinboard_ipc::PinboardDocument;
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WindowSizing};
    use tairix_abi::{Errno, ProcId, WaitSetOp, WaitSourceKind};
    use tairix_appdata::RtHost;
    use tairix_geometry::{Point, Rect, Region, Scale};
    use tairix_icon::NoArtwork;
    use tairix_input::InputEvent;
    use tairix_rt::io::{Stderr, Write};
    use tairix_settings::{Shell, ShellOutcome};
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_wallpaper::{ApplyOutcome, CatalogItem, DesktopSettings, PINBOARD_PUBLISHER};
    use tairix_window::app::{self, AppWindow, ShellError, Wake, EXIT_CHANNEL_LOST};
    use tairix_window::{
        key_input_event, pointer_input_events, pointer_point, present_damage, Desktop, EventDrain,
        EventError, EventMailbox, EventSource, Parked, Repaint, Target, WindowEvents,
    };

    /// The wait-set token of the applier's wake pipe: readable exactly when
    /// the desktop session has answered an apply, so the rows are brought up
    /// to date through the park the loop is already in rather than by
    /// waiting for it.
    const APPLY_TOKEN: u64 = app::FIRST_APP_TOKEN;

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

    /// The desktop settings in effect for the launching user, so every
    /// composed row opens on what the desktop is actually drawn with.
    ///
    /// Read from the desktop session's **published** app-data scope, which
    /// is the sanctioned channel one application reaches another's values
    /// through: this program names the publisher and nothing else, so the
    /// request shape it sends cannot ask for the session's private
    /// settings. It never writes them — an application publishes only its
    /// own scope — so a change is a request the session decides on.
    ///
    /// A desktop that has published **nothing** means the documented
    /// defaults and is not an error: a fresh account has never applied a
    /// setting. Anything else that stops the document being used says so on
    /// `stderr` rather than showing values the user cannot account for.
    fn settings_in_effect() -> DesktopSettings {
        let document = match tairix_appdata::read_published(&mut RtHost, PINBOARD_PUBLISHER) {
            Ok(document) => document,
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: the desktop's settings could not be read ({err:?}); showing the \
                     defaults"
                );
                return DesktopSettings::default();
            }
        };
        let (settings, refused) = DesktopSettings::load(&document);
        for key in refused {
            let _ = writeln!(
                Stderr,
                "settings: the desktop publishes a `{key}` this build does not accept; showing \
                 its default"
            );
        }
        settings
    }

    /// The applier: the session round trip an apply costs, carried out on a
    /// worker thread.
    ///
    /// The session answers only once its own publisher has written the
    /// store, so making the choice wait for it would freeze this window for
    /// a disk commit — and freeze it again on every further choice. The loop
    /// encodes the document (in memory, and refusable on the spot), submits,
    /// and adopts the answer on the wake it nudges.
    type Applier = tairix_rt::work::Worker<(), PinboardDocument, ApplyOutcome>;

    /// The worker's body: the shared apply client's one round trip.
    fn send_apply(_: &mut (), document: &mut PinboardDocument) -> ApplyOutcome {
        tairix_wallpaper::apply(*document)
    }

    /// Ask the desktop session to adopt `document`, off the event loop.
    ///
    /// A document this program cannot even encode is refused here, where it
    /// costs nothing; everything else goes to the worker and is answered on
    /// a later wake.
    fn submit_apply(applier: &Applier, document: &str) -> Option<ApplyOutcome> {
        match PinboardDocument::new(document) {
            // With no worker the call was made on this thread and its answer
            // is already on the desk.
            Ok(document) if applier.submit(document) => applier.collect(),
            Ok(_) => None,
            Err(_) => Some(ApplyOutcome::Refused(String::from(
                "settings document out of range",
            ))),
        }
    }

    /// Adopt what the desktop answered: state a refusal, then re-read what
    /// the desktop actually holds into `shell`.
    ///
    /// Persist-then-adopt. The rows showed the reader's choice at once; the
    /// durable value is whatever the session answers with, so a refusal puts
    /// the row back rather than leaving a value on screen the next login
    /// would not restore.
    fn adopt_apply(shell: &mut Shell, outcome: ApplyOutcome) {
        match outcome {
            ApplyOutcome::Applied | ApplyOutcome::Applying => {}
            ApplyOutcome::Refused(reason) => {
                let _ = writeln!(Stderr, "settings: the desktop refused the change: {reason}");
            }
            ApplyOutcome::NoDesktop => {
                let _ = writeln!(
                    Stderr,
                    "settings: no desktop session answered; nothing was changed"
                );
            }
        }
        shell.adopt_settings(settings_in_effect());
    }

    /// The shipped picture catalog the desktop offers, read once.
    ///
    /// The store is on the read-only `/System` volume, so the session
    /// listed it at bring-up and answers from memory: this costs a round
    /// trip per page and no I/O either side, which is why it is done here
    /// rather than deferred. A page that cannot be read leaves the gallery
    /// with whatever arrived — an empty gallery still offers "no picture"
    /// — and states the reason once.
    fn fetch_catalog(
        client: &mut tairix_window::WindowClient<app::RtWindowTransport>,
    ) -> Vec<CatalogItem> {
        let mut page = [0u8; tairix_abi::window_ipc::WINDOW_WALLPAPERS_REPLY_MAX];
        let mut catalog: Vec<CatalogItem> = Vec::new();
        loop {
            let Ok(from) = u16::try_from(catalog.len()) else {
                return catalog;
            };
            let answered = match client.wallpapers(from, &mut page) {
                Ok(answered) => answered,
                Err(err) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop's picture catalog could not be read ({err}); the \
                         gallery shows what arrived"
                    );
                    return catalog;
                }
            };
            let total = answered.total;
            // An empty page with entries still outstanding is a session
            // that cannot answer them, not a queue to keep asking: stop
            // rather than turn the read into a spin.
            if answered.is_empty() {
                return catalog;
            }
            for entry in answered.entries() {
                let (Ok(category), Ok(file)) = (
                    core::str::from_utf8(entry.category),
                    core::str::from_utf8(entry.file),
                ) else {
                    continue;
                };
                catalog.push(CatalogItem {
                    category: String::from(category),
                    file: String::from(file),
                });
            }
            if catalog.len() >= usize::from(total) {
                return catalog;
            }
        }
    }

    /// The picture gallery's client half: the region the desktop renders
    /// into, and which render is outstanding.
    ///
    /// One region, created once at the side the tiles are drawn at and
    /// re-created when that side moves, because a grant is cheap only if it
    /// is not taken per tile. One render outstanding, because the desktop
    /// serves one at a time.
    struct Pictures {
        region: Option<tairix_rt::shm::SharedRegion>,
        /// The square side `region` was sized for.
        side: u16,
        /// The catalog position awaiting its answer.
        pending: Option<u16>,
    }

    impl Pictures {
        const fn new() -> Self {
            Self {
                region: None,
                side: 0,
                pending: None,
            }
        }

        /// Ask the desktop for the next picture the shell wants, if any.
        ///
        /// Requested, never awaited: the answer arrives as an ordinary
        /// window event. A refusal is stated once and the tile is marked
        /// refused, so a picture the desktop will not render never becomes
        /// a request loop.
        fn request(
            &mut self,
            shell: &mut Shell,
            surface: &mut SettingsWindow,
            theme: &Theme,
            scale: Scale,
        ) {
            if self.pending.is_some() {
                return;
            }
            let viewport = surface.viewport();
            let Some(wanted) = shell.next_picture_wanted(viewport, scale, theme) else {
                return;
            };
            let Some(window_id) = surface.window.window_id() else {
                return;
            };
            let Some(grant) = self.grant(wanted.side) else {
                let _ = writeln!(
                    Stderr,
                    "settings: no shared region for a picture preview; the gallery shows its \
                     placeholders"
                );
                shell.mark_picture_refused(wanted.index);
                return;
            };
            match surface.window.client().render_wallpaper(
                window_id,
                grant,
                wanted.index,
                wanted.side,
            ) {
                Ok(()) => self.pending = Some(wanted.index),
                Err(err) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop refused a picture preview ({err}); it is not \
                         shown"
                    );
                    shell.mark_picture_refused(wanted.index);
                }
            }
        }

        /// The grant handle of a region big enough for a `side` square,
        /// creating one when the side has moved.
        fn grant(&mut self, side: u16) -> Option<u64> {
            let want = usize::from(side)
                .checked_mul(usize::from(side))?
                .checked_mul(4)?;
            if self.side != side || self.region.is_none() {
                // Dropped before the new one is mapped, so a gallery that
                // re-renders at a new scale holds one region, not two.
                self.region = None;
                self.region = tairix_rt::shm::SharedRegion::create(want);
                self.side = side;
            }
            let region = self.region.as_ref()?;
            let handle = tairix_rt::shm_grant(region.id(), tairix_abi::window_ipc::WINDOW_ENDPOINT);
            u64::try_from(handle).ok().filter(|grant| *grant >= 1)
        }

        /// Adopt the conclusion of a render, answering whether the gallery
        /// changed.
        ///
        /// An answer for a position this program is not waiting on, or at a
        /// side its region is not, is dropped: the tile keeps waiting rather
        /// than drawing pixels of the wrong shape.
        fn settle(&mut self, shell: &mut Shell, index: u16, side: u16, rendered: bool) -> bool {
            if self.pending != Some(index) || self.side != side {
                return false;
            }
            self.pending = None;
            if !rendered {
                return shell.mark_picture_refused(index);
            }
            let Some(region) = self.region.as_mut() else {
                return shell.mark_picture_refused(index);
            };
            let pixels = region.bytes_mut();
            shell.set_picture(index, side, pixels)
        }

        /// Forget the outstanding render, because the desktop it was asked
        /// of has moved and every tile is being asked for again.
        const fn restart(&mut self) {
            self.pending = None;
        }
    }

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

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty.
    struct RtEventSource<'a> {
        mailbox: EventMailbox,
        set: u64,
        /// The applier's wake, drained on an [`APPLY_TOKEN`] wake. Its
        /// readiness is a level peek, so leaving it undrained would report
        /// ready for ever and turn the park into a spin.
        applier: &'a Applier,
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
                // The session answered an apply. Draining is the whole of
                // noticing it, and the answer is the loop's to adopt, so the
                // wait ends here rather than parking again on a ready source.
                Wake::App(APPLY_TOKEN) => {
                    self.applier.wake().drain();
                    Ok(Parked::Interrupted)
                }
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
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Acted {
        /// Nothing on screen changed.
        Idle,
        /// The shell changed and must be re-presented.
        Changed,
        /// The whole client changed and no report could describe it.
        Whole,
        /// The reader chose a setting: ask the desktop to adopt it, then
        /// re-read what it holds.
        Apply(String),
        /// The desktop queued a target: drain the queue and show what it
        /// named.
        Opened,
        /// A picture the gallery asked for is in the shared region, or was
        /// refused.
        Rendered {
            /// The catalog position that was asked for.
            index: u16,
            /// The square side it was rendered at.
            side: u16,
            /// Whether the region holds the picture.
            rendered: bool,
        },
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
        let concluded = |outcome: ShellOutcome| match outcome {
            ShellOutcome::Idle => Acted::Idle,
            ShellOutcome::Changed => Acted::Changed,
            ShellOutcome::Apply(document) => Acted::Apply(document),
        };
        match event {
            WindowEvent::CloseRequested { .. } => Acted::Quit,
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
                InputEvent::KeyPressed { key, modifiers } => {
                    concluded(shell.on_key(key, modifiers, viewport, scale, theme, damage))
                }
                _ => Acted::Idle,
            },
            WindowEvent::Pointer { x, y, action, .. } => concluded(apply_pointer(
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
                concluded(shell.on_pointer(&scroll, viewport, scale, theme, damage))
            }
            // The desktop queued at least one target for this instance.
            WindowEvent::OpenRequested => Acted::Opened,
            // A picture the gallery asked for. Answered by the loop, which
            // holds the region it was rendered into.
            WindowEvent::WallpaperRendered {
                index,
                side,
                rendered,
                ..
            } => Acted::Rendered {
                index: *index,
                side: *side,
                rendered: *rendered,
            },
            // A redraw needs nothing here: the client library re-presents the
            // last frame and the shell it drew has not changed. The rest are
            // events this surface does not act on — it opens no chain of the
            // desktop's, declares no file association, and owns no desktop
            // layer — and `ContentReleased` is the caller's, which owns the
            // region it lets go of.
            // Settings is part of the desktop rather than an application
            // the user manages: its signed manifest presents no icon-bar
            // slot and it declares none, so neither icon-bar event can
            // reach it. A secondary press on Close asks to leave what the
            // window shows, and this window shows only itself.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::AppBarDefault
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
            | WindowEvent::PickCancelled { .. } => Acted::Idle,
        }
    }

    /// Drain every target the desktop queued for this instance and show the
    /// last pane it named, answering whether the window moved.
    ///
    /// One event may cover several targets, and another may arrive while
    /// this drain is running, so it loops until the queue answers empty. A
    /// target that is not a pane is not one this application can act on —
    /// it declares no file association and holds no filesystem capability —
    /// and is stated rather than silently dropped. A pane it does not carry
    /// leaves the window where it is, which is the whole point of a target
    /// that confers nothing.
    fn drain_open_targets(
        shell: &mut Shell,
        surface: &mut SettingsWindow,
        theme: &Theme,
        scale: Scale,
    ) -> bool {
        let viewport = surface.viewport();
        let mut moved = false;
        loop {
            match surface.window.client().take_open_target() {
                Ok(Some(Target::Pane(pane))) => {
                    let mut sink = tairix_controls::damage::sink();
                    if shell.go_to_pane(&pane, viewport, scale, theme, &mut sink) {
                        moved = true;
                    } else {
                        let _ = writeln!(
                            Stderr,
                            "settings: there is no `{pane}` here; the window stays where it is"
                        );
                    }
                }
                Ok(Some(Target::Path(path))) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: {path} was handed over, but this window shows settings, not \
                         files"
                    );
                }
                Ok(Some(Target::Document { name, .. })) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: {name} was handed over as a document, which this window has \
                         nowhere to show"
                    );
                }
                Ok(None) => return moved,
                Err(err) => {
                    let _ = writeln!(Stderr, "settings: cannot take an open target: {err}");
                    return moved;
                }
            }
        }
    }

    /// Route one wire pointer event: a move to `at` to sync the pointer, then
    /// the press or release the action names.
    ///
    /// A press and its release are two inputs of one gesture, so the
    /// stronger of what they concluded is the gesture's: an apply the
    /// release asked for is not lost behind the press's bare repaint.
    fn apply_pointer(
        shell: &mut Shell,
        at: Point,
        action: PointerAction,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        let mut concluded = ShellOutcome::Idle;
        for input in pointer_input_events(action, at) {
            let acted = shell.on_pointer(&input, viewport, scale, theme, damage);
            if matches!(acted, ShellOutcome::Apply(_))
                || (acted.changed() && !matches!(concluded, ShellOutcome::Apply(_)))
            {
                concluded = acted;
            }
        }
        concluded
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

    /// Ask the gallery for every picture again, because the desktop's
    /// scale or theme moved and a rendered picture is square at one side
    /// only.
    ///
    /// The outstanding render is forgotten rather than waited for: its
    /// answer would be at the old side and the settle drops it, so the
    /// gallery asks afresh instead of stalling behind an answer it cannot
    /// use.
    fn restart_pictures(
        shell: &mut Shell,
        surface: &mut SettingsWindow,
        pictures: &mut Pictures,
        theme: &Theme,
        scale: Scale,
    ) {
        shell.invalidate_pictures();
        pictures.restart();
        pictures.request(shell, surface, theme, scale);
    }

    /// Present the whole client, answering whether the session took it.
    fn present_whole(
        surface: &mut SettingsWindow,
        shell: &Shell,
        themes: &ThemeRegistry,
        desktop: &Desktop,
    ) -> bool {
        let whole = DamageRect::full(&surface.mode);
        surface
            .present(shell, themes.active(), desktop.scale(), whole)
            .is_ok()
    }

    /// Adopt a desktop change the park reported, answering whether the
    /// window is still presentable.
    ///
    /// A new density or theme re-measures every length the layout is
    /// derived from, and stales every rendered picture — a picture is
    /// square at one side only.
    fn adopt_desktop_round(
        surface: &mut SettingsWindow,
        shell: &mut Shell,
        themes: &mut ThemeRegistry,
        desktop: &mut Desktop,
        moved: &Cell<bool>,
        pictures: &mut Pictures,
    ) -> bool {
        if !adopt_desktop(desktop, themes, moved) {
            return true;
        }
        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
        restart_pictures(shell, surface, pictures, themes.active(), desktop.scale());
        present_whole(surface, shell, themes, desktop)
    }

    /// The event loop: park, apply, repaint.
    /// The long-lived state one turn of the event loop reads and writes.
    ///
    /// Grouped because every path through the loop needs all of it: the
    /// window it presents to, the desktop and theme it draws with, the
    /// shell it routes into, and the channels it answers on.
    struct Session<'a> {
        surface: &'a mut SettingsWindow,
        desktop: &'a mut Desktop,
        themes: &'a mut ThemeRegistry,
        shell: &'a mut Shell,
        desktop_moved: &'a Cell<bool>,
        applier: &'a Applier,
        pictures: &'a mut Pictures,
    }

    fn run_event_loop(session: Session<'_>, mut events: WindowEvents<RtEventSource<'_>>) -> i32 {
        let Session {
            surface,
            desktop,
            themes,
            shell,
            desktop_moved,
            applier,
            pictures,
        } = session;
        loop {
            // An answer the park drained is the loop's to adopt, whether or
            // not an event came with it.
            if let Some(outcome) = applier.collect() {
                adopt_apply(shell, outcome);
                shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                if !present_whole(surface, shell, themes, desktop) {
                    return fail(EXIT_CHANNEL_LOST, "present refused");
                }
            }
            let event = match events.wait(surface.window.client()) {
                Ok(Some(event)) => event,
                // A wait that ended without an event is the desktop notice; a
                // malformed frame from the authenticated session is refused
                // rather than guessed at. Either way the round is the
                // re-theme alone.
                Ok(None) | Err(EventError::Undecodable(_)) => {
                    if !adopt_desktop_round(
                        surface,
                        shell,
                        themes,
                        desktop,
                        desktop_moved,
                        pictures,
                    ) {
                        return fail(EXIT_CHANNEL_LOST, "present refused");
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
                restart_pictures(shell, surface, pictures, themes.active(), desktop.scale());
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
                Acted::Apply(ref document) => {
                    // Submitted, not awaited: the answer arrives on the wake
                    // the worker nudges. With no worker to serve it the call
                    // was made here and its answer is already in hand.
                    if let Some(outcome) = submit_apply(applier, document) {
                        adopt_apply(shell, outcome);
                        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                    }
                }
                Acted::Opened => {
                    if drain_open_targets(shell, surface, themes.active(), desktop.scale()) {
                        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                    }
                }
                Acted::Rendered {
                    index,
                    side,
                    rendered,
                } => {
                    pictures.settle(shell, index, side, rendered);
                }
                Acted::Idle | Acted::Changed | Acted::Whole => {}
            }
            // Ask for the next picture the gallery wants, whatever this
            // round was: a navigation, a resize and an answered render all
            // change what it is waiting for.
            pictures.request(shell, surface, themes.active(), desktop.scale());
            if matches!(event, WindowEvent::ContentReleased { .. }) {
                surface.window.release_frames();
                continue;
            }
            // An apply re-read the desktop, so every row may have moved:
            // the whole client is redrawn rather than the one row the
            // choice reported.
            let whole = redraw
                || matches!(
                    acted,
                    Acted::Whole | Acted::Apply(_) | Acted::Opened | Acted::Rendered { .. }
                );
            let repaint = match (whole, matches!(acted, Acted::Changed)) {
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

        // An empty registry would leave the window nothing to show at all, so
        // it ends fail-loud rather than opening a blank frame.
        let Some(mut shell) = Shell::new(settings_in_effect()) else {
            return fail(
                EXIT_CHANNEL_LOST,
                "the settings registry holds no categories",
            );
        };
        // The apply worker. A machine that grants none leaves the round
        // trip on this task — where it would otherwise stall the window —
        // and says so once rather than silently.
        let applier = alloc::sync::Arc::new(Applier::new(
            send_apply,
            (),
            tairix_rt::sync::WorkerWake::create(),
        ));
        if let Err(reason) = Applier::start(&applier) {
            let _ = writeln!(
                Stderr,
                "settings: no apply worker ({reason:?}); the desktop is asked on the event loop"
            );
        }
        let _applier_guard = tairix_rt::work::WorkerGuard::new(&applier);
        // A refused add is fatal rather than tolerated: an answer nobody
        // collects would leave every row showing a value the desktop may
        // never have adopted.
        if let Some(read) = applier.wake().read_end() {
            if tairix_rt::waitset_ctl(
                binding.set(),
                WaitSetOp::Add,
                WaitSourceKind::Stream,
                u64::from(read),
                APPLY_TOKEN,
            ) != 0
            {
                return fail(app::EXIT_NO_EVENTS, "apply wake refused");
            }
        }

        // Before the window opens, so the Wallpaper pane has its pictures
        // to offer on its first frame rather than on a later wake. The
        // session lists the read-only store once at its own bring-up and
        // answers this from memory.
        shell.adopt_catalog(fetch_catalog(surface.window.client()));
        // A pane the launch named, if it named one: a fresh process is
        // given it as its one argument, exactly as a running instance is
        // handed it over the channel.
        if let Some(pane) = tairix_rt::args().as_deref().and_then(|argv| argv.get(1)) {
            let mut sink = tairix_controls::damage::sink();
            let viewport = surface.viewport();
            if !shell.go_to_pane(pane, viewport, desktop.scale(), themes.active(), &mut sink) {
                let _ = writeln!(
                    Stderr,
                    "settings: there is no `{pane}` here; the window opens where it always does"
                );
            }
        }
        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());

        let server = match surface.open(event_endpoint, &shell, themes.active(), desktop.scale()) {
            Ok(server) => server,
            Err(code) => return code,
        };

        let desktop_moved = Cell::new(false);
        let events = WindowEvents::new(RtEventSource {
            mailbox: EventMailbox::new(event_endpoint, server),
            set: binding.set(),
            applier: &applier,
            desktop_moved: &desktop_moved,
        });
        run_event_loop(
            Session {
                surface: &mut surface,
                desktop: &mut desktop,
                themes: &mut themes,
                shell: &mut shell,
                desktop_moved: &desktop_moved,
                applier: &applier,
                pictures: &mut Pictures::new(),
            },
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
