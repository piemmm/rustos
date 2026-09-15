//! The `sapper.app` bundle's `Run` entry point: the windowed mine-clearing
//! game.
//!
//! Everything with behaviour lives in the host-tested model (`tairix_sapper`);
//! this binary composes it over the live window channel, exactly as the widget
//! gallery composes its own:
//!
//! * one `shm_create`d frame region granted to the reserved window endpoint;
//! * one `port_bind`-bound event mailbox the app **parks** on, accepting only
//!   events whose kernel-attested sender is the session the create reply named;
//! * a park that carries the game's own one-shot deadline — the next animation
//!   frame, or the clock's next whole second — and *no* deadline at all when
//!   the game owes neither, so an idle board costs no wake;
//! * a worker the best-times write is handed to, because that write is an IPC
//!   round trip to the settings service and this loop owes the window a frame.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
extern crate alloc;

#[cfg(freestanding)]
mod program {
    use core::cell::Cell;

    use alloc::sync::Arc;

    use tairix_abi::driver::display::{DamageRect, DisplayMode};
    use tairix_abi::input::KeyInput;
    use tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS;
    use tairix_abi::window_ipc::{
        AppBarClick, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuMark, AppMenuRow,
        AppMenuShortcut, WindowEvent, WindowSizing,
    };
    use tairix_abi::{Errno, ProcId, WaitSetOp, WaitSourceKind};
    use tairix_appdata::{RtHost, Settings};
    use tairix_font::BitmapFont;
    use tairix_geometry::{Rect, Region, Scale};
    use tairix_input::InputEvent;
    use tairix_rng::{FastRng, RandU64};
    use tairix_rt::io::{Stderr, Write};
    use tairix_sapper::board::Difficulty;
    use tairix_sapper::game::{Game, Reaction};
    use tairix_sapper::scores::{BestTimes, SaveError};
    use tairix_theme::{TextRole, Theme, ThemeRegistry};
    use tairix_util::defer::JobDesk;
    use tairix_window::app::{self, AppWindow, ShellError, Wake, EXIT_CHANNEL_LOST};
    use tairix_window::{
        key_input_event, pointer_input_events, pointer_point, present_damage, Desktop, EventDrain,
        EventError, EventMailbox, EventSource, Parked, Repaint, WindowClient, WindowEvents,
    };

    /// The wait-set token the best-times worker's answer wake arrives under.
    const WRITER_TOKEN: u64 = app::FIRST_APP_TOKEN;

    /// The icon-bar rows this application adds between the convention's fixed
    /// ends, numbered from the shared *Quit* id so the two can never collide.
    const ROW_NEW_GAME: u16 = tairix_window::QUIT_ROW + 1;
    /// The row selecting the beginner board. The two harder boards follow it,
    /// so a preset's row is its position in `Difficulty::PRESETS` past this.
    const ROW_FIRST_PRESET: u16 = tairix_window::QUIT_ROW + 2;
    /// The row toggling the question mark in the mark cycle.
    const ROW_QUESTIONS: u16 = ROW_FIRST_PRESET + 3;

    /// State the abnormal-exit reason on `stderr` (fail loud) and hand `code`
    /// back for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "sapper: {reason}");
        code
    }

    /// State a shared-shell bring-up refusal and hand its reserved code back.
    fn fail_shell(err: ShellError) -> i32 {
        let _ = writeln!(Stderr, "sapper: {err}");
        err.code()
    }

    /// Report a refusal that is not fatal: the game carries on with whatever
    /// authority it has.
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "sapper: {reason}");
    }

    // ---- the best-times writer -----------------------------------------

    /// The desk the loop submits a best-times write to and the worker takes it
    /// from.
    ///
    /// Latest-wins with at most one write in flight, so a run of quick wins
    /// costs one further write rather than a backlog, and two writes can never
    /// race for what the store ends up saying.
    struct Writer {
        desk: tairix_rt::sync::Mutex<JobDesk<BestTimes, Result<(), SaveError>>>,
        signal: tairix_rt::sync::Condvar,
        wake: tairix_rt::sync::WorkerWake,
    }

    impl Writer {
        fn new() -> Self {
            Self {
                desk: tairix_rt::sync::Mutex::new(JobDesk::new()),
                signal: tairix_rt::sync::Condvar::new(),
                wake: tairix_rt::sync::WorkerWake::create(),
            }
        }

        /// Hand a write to the worker, or — with no worker to take it — do it
        /// here. Slower under load, never wrong, and never a dropped record.
        fn submit(&self, times: BestTimes, armed: bool) {
            if !armed {
                if let Err(err) = Self::write(times) {
                    report(&alloc::format!("{err}"));
                }
                return;
            }
            let submitted = self.desk.lock().submit(times);
            if submitted.wake {
                self.signal.notify_one();
            }
        }

        /// The worker's whole life: park until a write is wanted, do it, wake
        /// the loop.
        fn serve(&self) {
            loop {
                let job = {
                    let mut desk = self.desk.lock();
                    loop {
                        if desk.stopping() {
                            return;
                        }
                        if let Some(job) = desk.next_job() {
                            break job;
                        }
                        desk = self.signal.wait(desk);
                    }
                };
                // The round trip itself, with no lock held: this is the call
                // that would otherwise have stalled the window.
                let outcome = Self::write(job);
                if self.desk.lock().deliver(outcome) {
                    self.wake.nudge();
                }
            }
        }

        /// Open the store and write `times` into it.
        fn write(times: BestTimes) -> Result<(), SaveError> {
            let mut host = RtHost;
            let mut settings = Settings::open_without_defaults(&mut host);
            times.save(&mut settings)
        }

        /// Take a landed answer, if one has.
        fn collect(&self) -> Option<Result<(), SaveError>> {
            self.desk.lock().collect()
        }

        /// Ask the worker to leave and wake it.
        fn stop(&self) {
            self.desk.lock().stop();
            self.signal.notify_all();
        }
    }

    /// Stops the writer on every way out, so it is not left mid-write for a
    /// window that has gone.
    ///
    /// The thread is *detached* rather than joined: a worker mid-round-trip to
    /// a slow service would otherwise hold the teardown for as long as that
    /// service takes, and it leaves at its next turn round its loop anyway.
    struct WriterGuard(Arc<Writer>);

    impl Drop for WriterGuard {
        fn drop(&mut self) {
            self.0.stop();
        }
    }

    // ---- the event source ----------------------------------------------

    /// The app's park: its event mailbox, the memory-pressure band, the
    /// writer's answer wake, and the game's own deadline.
    struct RtEventSource<'a> {
        mailbox: EventMailbox,
        set: u64,
        writer: &'a Writer,
        /// When the game next owes a frame or a clock tick, or `None` when it
        /// owes neither — in which case the park carries no deadline at all
        /// and the CPU is given up entirely.
        ///
        /// Shared with the loop through a cell because the loop owns the
        /// deadline and the source owns the park: one writes it just before
        /// the other reads it, on the one thread both run on.
        deadline_ns: &'a Cell<Option<u64>>,
        /// Set when the park woke for a desktop change, cleared when the loop
        /// adopts it. Shared through a cell for the same reason the deadline
        /// is: the park writes it, the loop reads it, on one thread.
        desktop_moved: &'a Cell<bool>,
    }

    impl EventDrain for RtEventSource<'_> {
        fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
            self.mailbox.try_next(event)
        }
    }

    impl EventSource for RtEventSource<'_> {
        fn park(&mut self) -> Result<Parked, Errno> {
            let woken = match self.deadline_ns.get() {
                // One-shot, to the next thing the game actually needs: no
                // periodic tick, and no timer armed while the board is still.
                Some(deadline) => match app::park_until(self.set, deadline)? {
                    Some(woken) => woken,
                    None => return Ok(Parked::Interrupted),
                },
                None => app::park(self.set)?,
            };
            match woken {
                Wake::App(WRITER_TOKEN) => {
                    // The readiness is a level peek, so leaving it undrained
                    // would report ready for ever and turn the park into a
                    // spin.
                    self.writer.wake.drain();
                    Ok(Parked::Interrupted)
                }
                Wake::PressureChanged => {
                    tairix_font::trim_glyph_cache();
                    Ok(Parked::Served)
                }
                // The theme, the density, and the reduced-motion policy the
                // board is drawn from all moved, so the wait ends and the
                // loop re-themes before the next frame.
                Wake::DesktopChanged => {
                    self.desktop_moved.set(true);
                    Ok(Parked::Interrupted)
                }
                Wake::Event | Wake::PressureUnchanged | Wake::App(_) => Ok(Parked::Served),
            }
        }
    }

    // ---- the icon bar ---------------------------------------------------

    /// Declare this application's icon-bar presence, with the marks reading the
    /// state they name.
    ///
    /// Idempotent-replace, so it is re-declared whenever a mark would change
    /// and the menu can never disagree with the game.
    fn declare_app_bar(
        client: &mut WindowClient<app::RtWindowTransport>,
        endpoint: u64,
        game: &Game,
    ) {
        let Some(rows) = menu_rows(game) else {
            report("this application's icon-bar menu is invalid; carrying on without one");
            return;
        };
        match tairix_window::declaration(endpoint, AppBarClick::RaiseOrOpen, &rows) {
            Ok(bar) => {
                if let Err(err) = client.set_app_bar(&bar) {
                    report(&alloc::format!(
                        "the desktop refused this application's icon-bar presence ({err}); \
                         carrying on without one"
                    ));
                }
            }
            Err(err) => report(&alloc::format!(
                "this application's icon-bar menu is invalid ({err:?}); carrying on without one"
            )),
        }
    }

    /// The rows between the convention's fixed ends: a new game, the three
    /// boards as a radio group, and the question-mark setting as a tick.
    fn menu_rows(game: &Game) -> Option<alloc::vec::Vec<AppMenuRow>> {
        let item = |id: u16, label: &str, shortcut: &str| -> Option<AppMenuItem> {
            Some(
                AppMenuItem::new(AppMenuItemId::new(id).ok()?, AppMenuLabel::new(label).ok()?)
                    .with_shortcut(AppMenuShortcut::new(shortcut).ok()?),
            )
        };
        let mut rows = alloc::vec![
            AppMenuRow::Item(item(ROW_NEW_GAME, "New game", "N")?),
            AppMenuRow::Separator,
        ];
        for (index, preset) in Difficulty::PRESETS.into_iter().enumerate() {
            let id = ROW_FIRST_PRESET + u16::try_from(index).ok()?;
            let shortcut = [b'1' + u8::try_from(index).ok()?];
            let mark = if game.difficulty() == preset {
                AppMenuMark::Radio
            } else {
                AppMenuMark::None
            };
            rows.push(AppMenuRow::Item(
                item(id, preset.title(), core::str::from_utf8(&shortcut).ok()?)?.with_mark(mark),
            ));
        }
        rows.push(AppMenuRow::Separator);
        let mark = if game.questions() {
            AppMenuMark::Check
        } else {
            AppMenuMark::None
        };
        rows.push(AppMenuRow::Item(
            AppMenuItem::new(
                AppMenuItemId::new(ROW_QUESTIONS).ok()?,
                AppMenuLabel::new("Question marks").ok()?,
            )
            .with_mark(mark),
        ));
        Some(rows)
    }

    // ---- the window ------------------------------------------------------

    /// The app's channel to the desktop and the window it may or may not have
    /// open.
    struct GameWindow {
        window: AppWindow,
    }

    impl GameWindow {
        /// Open a window shaped to `game`'s board and present its first frame,
        /// answering the session's [`ProcId`] or the reserved exit code.
        fn open(
            &mut self,
            event_endpoint: u64,
            game: &Game,
            theme: &Theme,
            scale: Scale,
        ) -> Result<(ProcId, DisplayMode), i32> {
            let (width, height) = game.preferred_size();
            let mode = app::mode_for(width, height);
            let (min_width_px, min_height_px) = game.minimum_size();
            let server = self
                .window
                .open(
                    event_endpoint,
                    &mode,
                    "Sapper",
                    WindowSizing::Resizable {
                        min_width_px,
                        min_height_px,
                    },
                )
                .map_err(fail_shell)?;
            if self
                .present(game, theme, scale, DamageRect::full(&mode), 0)
                .is_err()
            {
                self.close();
                return Err(fail(EXIT_CHANNEL_LOST, "present refused"));
            }
            Ok((server, mode))
        }

        fn close(&mut self) {
            let _ = self.window.close();
        }

        /// Draw `game` and present `damage`.
        fn present(
            &mut self,
            game: &Game,
            theme: &Theme,
            scale: Scale,
            damage: DamageRect,
            now_ns: u64,
        ) -> Result<(), Errno> {
            let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
            self.window.present(damage, |surface| {
                game.render(surface, theme, font, now_ns);
            })
        }
    }

    /// What one delivered event concluded for the loop.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Acted {
        /// Nothing on screen changed.
        Idle,
        /// The game changed and must be re-presented.
        Changed,
        /// Every pixel moved — a resize the window manager already applied, or
        /// a theme change — so the whole window is redrawn at its current size.
        Relaid,
        /// The *board* changed size, so the window itself must be re-shaped to
        /// suit it. Distinct from `Relaid` because re-shaping in answer to a
        /// resize the user is dragging would fight the drag, frame by frame.
        Reshaped,
        /// Close the window, leaving the game on the icon bar.
        Close,
        /// Open a window, the game having none.
        Open,
        /// End the program.
        Quit,
    }

    impl Acted {
        /// How decisive this conclusion is, so folding two is a maximum rather
        /// than a table of pairs.
        const fn rank(self) -> u8 {
            match self {
                Self::Idle => 0,
                Self::Changed => 1,
                Self::Relaid => 2,
                Self::Reshaped => 3,
                Self::Close => 4,
                Self::Open => 5,
                Self::Quit => 6,
            }
        }

        /// The stronger of two conclusions about the same event.
        const fn or(self, other: Self) -> Self {
            if other.rank() > self.rank() {
                other
            } else {
                self
            }
        }

        /// Whether the whole window must be redrawn.
        const fn whole(self) -> bool {
            matches!(self, Self::Relaid | Self::Reshaped)
        }

        /// This conclusion for a reaction that only changed the view.
        const fn from_changed(changed: bool) -> Self {
            if changed {
                Self::Changed
            } else {
                Self::Idle
            }
        }
    }

    /// Everything one delivered event may touch, threaded as one value.
    struct Round<'a> {
        game: &'a mut Game,
        desktop: &'a mut Desktop,
        themes: &'a mut ThemeRegistry,
        rng: &'a mut dyn RandU64,
        writer: &'a Writer,
        armed: bool,
    }

    /// Apply one delivered event, reporting what it concluded.
    fn apply_event(
        round: &mut Round<'_>,
        event: &WindowEvent,
        now_ns: u64,
        damage: &mut Region,
    ) -> Acted {
        match event {
            WindowEvent::CloseRequested { .. } => Acted::Close,
            WindowEvent::AppBarMenu { item } => menu_chosen(round, *item, damage),
            WindowEvent::AppBarDefault => Acted::Open,
            WindowEvent::Resized {
                width_px,
                height_px,
                ..
            } => {
                round.game.relayout(
                    Rect::new(0, 0, *width_px, *height_px),
                    round.desktop.scale(),
                    damage,
                );
                Acted::Relaid
            }
            WindowEvent::Key {
                key: pressed @ KeyInput::Pressed { .. },
                ..
            } => match key_input_event(*pressed) {
                InputEvent::KeyPressed { key, modifiers } => {
                    let reaction = round.game.on_key(key, modifiers, now_ns, round.rng, damage);
                    settle(round, reaction)
                }
                _ => Acted::Idle,
            },
            WindowEvent::Pointer { x, y, action, .. } => {
                let at = pointer_point(*x, *y);
                let mut acted = Acted::Idle;
                for input in pointer_input_events(*action, at) {
                    let reaction = round.game.on_pointer(&input, now_ns, round.rng, damage);
                    acted = acted.or(settle(round, reaction));
                }
                acted
            }
            // The rest are events the game does not act on. A secondary press
            // on Close asks to leave what the window shows, and the board is
            // the only thing it shows. The game declares no file association,
            // opens no menu chain of its own, and asks for no picker, so none
            // of those outcomes can arrive. `RedrawRequested` is answered by
            // the client library re-presenting the last frame, and the desktop
            // change and the released content are handled by the caller, which
            // owns the region and the theme.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::MenuClosed { .. }
            | WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::Scrolled { .. }
            | WindowEvent::RedrawRequested { .. }
            | WindowEvent::ContentReleased { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. }
            | WindowEvent::OpenRequested { .. } => Acted::Idle,
        }
    }

    /// Adopt a reaction: hand a new best time to the writer, and say whether
    /// the window's geometry moved with it.
    fn settle(round: &mut Round<'_>, reaction: Reaction) -> Acted {
        if reaction.record {
            round.writer.submit(round.game.best_times(), round.armed);
        }
        if reaction.resized {
            return Acted::Reshaped;
        }
        Acted::from_changed(reaction.changed)
    }

    /// Act on a chosen icon-bar row. A row the declaration never carried names
    /// no command and is ignored (fail closed).
    fn menu_chosen(
        round: &mut Round<'_>,
        item: tairix_abi::window_ipc::AppMenuItemId,
        damage: &mut Region,
    ) -> Acted {
        if tairix_window::is_quit(item) {
            return Acted::Quit;
        }
        let id = item.get();
        if id == ROW_NEW_GAME {
            round.game.restart(damage);
            return Acted::Changed;
        }
        if id == ROW_QUESTIONS {
            round.game.set_questions(!round.game.questions());
            return Acted::Changed;
        }
        let Some(index) = id.checked_sub(ROW_FIRST_PRESET) else {
            return Acted::Idle;
        };
        let Some(&preset) = Difficulty::PRESETS.get(usize::from(index)) else {
            return Acted::Idle;
        };
        let reaction = round.game.set_difficulty(preset, damage);
        settle(round, reaction)
    }

    // ---- the run ---------------------------------------------------------

    /// The event loop: park, apply, repaint. A dead channel ends the app
    /// fail-loud; a clean close ends it at zero.
    #[allow(clippy::too_many_arguments)] // The run's whole mutable state, threaded explicitly.
    fn run_event_loop(
        surface: &mut GameWindow,
        round: &mut Round<'_>,
        event_endpoint: u64,
        mode: &mut DisplayMode,
        deadline: &Cell<Option<u64>>,
        desktop_moved: &Cell<bool>,
        mut events: WindowEvents<RtEventSource<'_>>,
    ) -> i32 {
        // What the icon-bar declaration on screen currently says.
        let mut declared = (round.game.difficulty(), round.game.questions());
        loop {
            // The deadline the *next* park carries: written before the wait,
            // read inside it.
            deadline.set(round.game.deadline_ns(tairix_rt::clock_get()));
            let waited = events.wait(surface.window.client());
            let now = tairix_rt::clock_get();
            let mut damage = tairix_controls::damage::sink();

            // A landed write is reported and otherwise costs the game nothing:
            // a best time that could not be kept is still a best time played.
            if let Some(Err(err)) = round.writer.collect() {
                report(&alloc::format!("{err}"));
            }

            // Every wake advances the clock and the animation, whether it was
            // the deadline that fired or an event that arrived.
            let mut acted = Acted::from_changed(round.game.tick(now, &mut damage));
            // Adopted before the game acts, so the scale, the theme, and the
            // reduced-motion policy everything below derives from are already
            // current — and adopted whether or not an event came with it.
            if desktop_moved.replace(false) {
                acted = acted.or(adopt_desktop(round, mode, &mut damage));
            }

            match waited {
                Ok(Some(event)) => {
                    acted = acted.or(apply_event(round, &event, now, &mut damage));
                    if matches!(event, WindowEvent::ContentReleased { .. }) {
                        surface.window.release_frames();
                        continue;
                    }
                }
                // A deadline with no event, or a malformed frame from the
                // authenticated session, which is refused rather than guessed
                // at. Either way the tick above still stands.
                Ok(None) | Err(EventError::Undecodable(_)) => {}
                Err(EventError::Mailbox(_)) => {
                    return fail(EXIT_CHANNEL_LOST, "event channel lost")
                }
            }

            match acted {
                Acted::Quit => {
                    surface.close();
                    return 0;
                }
                Acted::Close => {
                    surface.close();
                    continue;
                }
                // A refused open is already stated, and the slot is still
                // there to try again from.
                Acted::Open => {
                    if let Ok((_, opened)) = surface.open(
                        event_endpoint,
                        round.game,
                        round.themes.active(),
                        round.desktop.scale(),
                    ) {
                        *mode = opened;
                    }
                    continue;
                }
                // A board of a new size: re-shape the frame region before
                // anything is drawn into it. A resize the *user* dragged is
                // already adopted and is deliberately not re-shaped here —
                // answering a drag with a size of our own would fight it.
                Acted::Reshaped => {
                    let (width, height) = round.game.preferred_size();
                    let reshaped = app::mode_for(width, height);
                    if reshaped.width_px != mode.width_px || reshaped.height_px != mode.height_px {
                        if surface.window.resize(reshaped) {
                            *mode = reshaped;
                        }
                        round.game.relayout(
                            Rect::new(0, 0, mode.width_px, mode.height_px),
                            round.desktop.scale(),
                            &mut damage,
                        );
                    }
                }
                Acted::Idle | Acted::Changed | Acted::Relaid => {}
            }

            // The menu's marks name the difficulty and the question-mark
            // setting, so the declaration is replaced exactly when one of them
            // moves — never on an ordinary frame.
            let marked = (round.game.difficulty(), round.game.questions());
            if marked != declared {
                declared = marked;
                declare_app_bar(surface.window.client(), event_endpoint, round.game);
            }

            let repaint = if acted.whole() {
                Repaint::Whole
            } else if acted == Acted::Changed {
                Repaint::Reported
            } else {
                Repaint::Nothing
            };
            let Some(damage) = present_damage(mode, repaint, &damage) else {
                continue;
            };
            if surface
                .present(
                    round.game,
                    round.themes.active(),
                    round.desktop.scale(),
                    damage,
                    now,
                )
                .is_err()
            {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
    }

    /// Adopt the desktop state the session published, re-laying the board at
    /// the new scale and re-reading the reduced-motion policy with it.
    ///
    /// A refused state is reported and the last good desktop stands, so the
    /// board keeps drawing correctly rather than at a nonsense density.
    fn adopt_desktop(round: &mut Round<'_>, mode: &DisplayMode, damage: &mut Region) -> Acted {
        match app::adopt_desktop(round.desktop, round.themes) {
            Ok(true) => {
                let reduced = round.themes.active().motion().reduced_motion();
                round.game.set_reduced_motion(reduced, damage);
                round.game.relayout(
                    Rect::new(0, 0, mode.width_px, mode.height_px),
                    round.desktop.scale(),
                    damage,
                );
                Acted::Relaid
            }
            Ok(false) => Acted::Idle,
            Err(err) => {
                report(&alloc::format!("desktop change refused: {err}"));
                Acted::Idle
            }
        }
    }

    /// Read the best times the store holds, reporting every entry it refused.
    fn load_best_times() -> BestTimes {
        let mut host = RtHost;
        let settings = Settings::open_without_defaults(&mut host);
        let (times, refused) = BestTimes::load(&settings);
        for entry in refused {
            report(&alloc::format!(
                "the stored best time `{}` is unreadable ({:?}); starting that board with none",
                entry.key,
                entry.reason
            ));
        }
        times
    }

    /// A generator seeded from the kernel's own CSPRNG, so no two boards are
    /// the same and none is predictable from the ones before it.
    ///
    /// Seeded once and drawn from thereafter, rather than a syscall per draw:
    /// laying an Expert board is ninety-nine draws.
    fn seeded_rng() -> FastRng {
        let mut seed = [0u8; 32];
        if tairix_rt::random_get(&mut seed, tairix_abi::RandomFlags::empty()).is_err() {
            // The kernel's generator is not ready. A game seeded from the
            // monotonic clock is a worse game, not a broken one — and the
            // alternative is refusing to start over an unpredictability
            // requirement a puzzle does not have.
            report("the system generator is unavailable; seeding this session from the clock");
            return FastRng::seed_from_u64(tairix_rt::clock_get());
        }
        FastRng::from_key(&seed)
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // From here this task drives a user-facing loop, so declare the frame
        // it owes. A debug image then reports any span that overruns; a
        // shippable one arms nothing and answers zero, which is why the result
        // is not examined.
        let _ = tairix_rt::latency_watch(DEFAULT_FRAME_BUDGET_NS);

        let mut surface = GameWindow {
            window: AppWindow::new(),
        };
        let (mut desktop, mut themes) = match app::bring_up_desktop(surface.window.client()) {
            Ok(pair) => pair,
            Err(err) => return fail_shell(err),
        };
        let binding = match app::bind_event_mailbox() {
            Ok(binding) => binding,
            Err(err) => return fail_shell(err),
        };
        let event_endpoint = binding.endpoint();

        let writer = Arc::new(Writer::new());
        let armed = start_writer(&writer, binding.set());
        // Declared after the thread, so it runs first: the desk stops, then the
        // handle detaches.
        let _guard = WriterGuard(Arc::clone(&writer));

        let reduced = themes.active().motion().reduced_motion();
        let dims = Difficulty::Beginner.dimensions();
        let (width, height) = tairix_sapper::layout::Layout::preferred(dims, desktop.scale());
        let mut game = Game::new(
            Difficulty::Beginner,
            load_best_times(),
            true,
            reduced,
            Rect::new(0, 0, width, height),
            desktop.scale(),
        );

        // The icon-bar presence first: a declared presence belongs to the
        // process, so declaring it before this process owns a window is what
        // makes its slot carry this menu from the moment it appears.
        declare_app_bar(surface.window.client(), event_endpoint, &game);

        // The game was started to be played, so a first window that will not
        // open leaves it nothing to be and it ends fail-loud; every later one
        // is a click on its slot, which reports and carries on.
        let (server, mut mode) =
            match surface.open(event_endpoint, &game, themes.active(), desktop.scale()) {
                Ok(opened) => opened,
                Err(code) => return code,
            };

        let deadline = Cell::new(None);
        let desktop_moved = Cell::new(false);
        let events = WindowEvents::new(RtEventSource {
            mailbox: EventMailbox::new(event_endpoint, server),
            set: binding.set(),
            writer: &writer,
            deadline_ns: &deadline,
            desktop_moved: &desktop_moved,
        });
        let mut rng = seeded_rng();
        let mut round = Round {
            game: &mut game,
            desktop: &mut desktop,
            themes: &mut themes,
            rng: &mut rng,
            writer: &writer,
            armed,
        };
        run_event_loop(
            &mut surface,
            &mut round,
            event_endpoint,
            &mut mode,
            &deadline,
            &desktop_moved,
            events,
        )
    }

    /// Start the best-times writer and join its wake to the loop's wait-set,
    /// reporting whether the loop may hand it work.
    ///
    /// A kernel that will not grant the thread or the pipe is not a failure:
    /// the write moves back onto the event loop, which is slower under load but
    /// never loses a record.
    fn start_writer(writer: &Arc<Writer>, set: u64) -> bool {
        let Some(read) = writer.wake.read_end() else {
            report("no writer wake pipe; best times are written on the event loop");
            return false;
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Stream,
            u64::from(read),
            WRITER_TOKEN,
        ) != 0
        {
            report("writer wake refused; best times are written on the event loop");
            return false;
        }
        let served = Arc::clone(writer);
        match tairix_rt::thread::Thread::spawn(move || served.serve()) {
            Ok(_) => true,
            Err(err) => {
                report(&alloc::format!(
                    "no writer thread ({err:?}); best times are written on the event loop"
                ));
                false
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so this
// inert `main` keeps the crate building under the host tooling. It performs no
// I/O.
#[cfg(not(freestanding))]
fn main() {}
