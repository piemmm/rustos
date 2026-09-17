//! The `cinder.app` bundle's `Run` entry point: the desktop companion.
//!
//! Everything with behaviour lives in the host-tested model (`tairix_cinder`);
//! this binary composes it over the live window channel:
//!
//! * a playpen window, opened exactly as any application's is;
//! * a **desktop layer surface** for when Cinder is let out, opened through
//!   the capability-gated `OpenLayer` and placed in screen coordinates;
//! * one `port_bind`-bound event mailbox the app **parks** on, accepting only
//!   events whose kernel-attested sender is the session the create reply
//!   named;
//! * a park carrying the next animation frame's deadline, and *no* deadline at
//!   all when Cinder is asleep and nothing moves, so a dozing companion costs
//!   no wake;
//! * a worker the saved-mood write is handed to, because that write is an IPC
//!   round trip to the settings service and this loop owes a frame.
//!
//! Closing the pen never quits: Cinder is a resident icon-bar application, so
//! closing puts him away (or leaves him roaming) and *Quit* is what ends him.
//! Being refused a place on the desktop is likewise not fatal — the reason is
//! stated and the pen carries on.
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
    use alloc::sync::Arc;

    use tairix_abi::driver::display::{DamageRect, DisplayMode};
    use tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS;
    use tairix_abi::window_ipc::{
        AppBarClick, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuRow, LayerDepth,
        PointerAction, TerrainPlate, WindowEvent, WindowSizing, DESKTOP_LAYER_MAX_PLATES,
    };
    use tairix_abi::{Errno, ProcId, WaitSetOp, WaitSourceKind};
    use tairix_appdata::{RtHost, Settings};
    use tairix_cinder::cinder::{Eyes, Pose};
    use tairix_cinder::gait;
    use tairix_cinder::layout::{self, PenLayout, COMPANION_SIDE};
    use tairix_cinder::mind::{Intent, Mind};
    use tairix_cinder::paint::{companion_origin, pen_client, Painter};
    use tairix_cinder::pen::{Pen, PenAction, Refusal, Whereabouts};
    use tairix_cinder::project::Ground;
    use tairix_cinder::roam::Roam;
    use tairix_cinder::state::Saved;
    use tairix_cinder::world::{Area, World};
    use tairix_geometry::{Rect, Region, Scale};
    use tairix_input::InputEvent;
    use tairix_raster::Surface;
    use tairix_rng::FastRng;
    use tairix_rt::io::{Stderr, Write};
    use tairix_theme::{Theme, ThemeRegistry, Timeline};
    use tairix_util::defer::JobDesk;
    use tairix_util::mathf;
    use tairix_window::app::{self, AppWindow, ShellError, Wake, WindowPane, EXIT_CHANNEL_LOST};
    use tairix_window::{
        pointer_input_events, pointer_point, Desktop, EventDrain, EventMailbox, WindowClient,
    };

    /// The wait-set token the saved-mood worker's answer wake arrives under.
    const WRITER_TOKEN: u64 = app::FIRST_APP_TOKEN;

    /// The icon-bar row that lets Cinder out or puts him away, numbered from
    /// the shared *Quit* id so the two can never collide.
    const ROW_OUT: u16 = tairix_window::QUIT_ROW + 1;

    /// How long one animation frame is, in nanoseconds.
    ///
    /// The desktop's own animation period, which is also what the session's
    /// frame pacer admits at — so a companion is never woken for a frame the
    /// pacer would then refuse, and there is no second frame-period constant.
    /// *Not* the stall budget below: that says how long a frame may take
    /// before it is reported, which is an order of magnitude longer.
    const FRAME_NS: u64 = Timeline::FRAME_NS;

    /// One frame, in seconds, for the model's own arithmetic.
    #[allow(clippy::cast_precision_loss)] // A frame budget is far below 2^53 ns.
    const FRAME_SECONDS: f64 = FRAME_NS as f64 / 1_000_000_000.0;

    /// State the abnormal-exit reason on `stderr` (fail loud) and hand `code`
    /// back for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "cinder: {reason}");
        code
    }

    /// State a shared-shell bring-up refusal and hand its reserved code back.
    fn fail_shell(err: ShellError) -> i32 {
        let _ = writeln!(Stderr, "cinder: {err}");
        err.code()
    }

    /// Report a refusal that is not fatal: the companion carries on with
    /// whatever authority it has.
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "cinder: {reason}");
    }

    // ---- the saved-mood writer -----------------------------------------

    /// The desk the loop submits a saved-mood write to and the worker takes it
    /// from.
    ///
    /// Latest-wins with at most one write in flight, so a companion whose mood
    /// keeps moving costs one further write rather than a backlog, and two
    /// writes can never race for what the store ends up saying.
    struct Writer {
        desk: tairix_rt::sync::Mutex<JobDesk<Saved, Result<(), Errno>>>,
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
        fn submit(&self, saved: Saved, armed: bool) {
            if !armed {
                if let Err(err) = Self::write(saved) {
                    report(&alloc::format!("could not save Cinder's mood ({err:?})"));
                }
                return;
            }
            let submitted = self.desk.lock().submit(saved);
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
                // that would otherwise have stalled the frame.
                let outcome = Self::write(job);
                if self.desk.lock().deliver(outcome) {
                    self.wake.nudge();
                }
            }
        }

        /// Open the store and write `saved` into it.
        fn write(saved: Saved) -> Result<(), Errno> {
            let document = saved.to_document().map_err(|_| Errno::OutOfRange)?;
            let mut host = RtHost;
            let mut settings = Settings::open_without_defaults(&mut host);
            settings.replace(&document).map_err(|_| Errno::NoSpace)
        }

        /// Take a landed answer, if one has.
        fn collect(&self) -> Option<Result<(), Errno>> {
            self.desk.lock().collect()
        }

        /// Ask the worker to leave and wake it.
        fn stop(&self) {
            self.desk.lock().stop();
            self.signal.notify_all();
        }
    }

    /// Stop the worker's desk when the program ends, so the thread is not left
    /// parked on a condition nothing will signal.
    struct WriterGuard(Arc<Writer>);

    impl Drop for WriterGuard {
        fn drop(&mut self) {
            self.0.stop();
        }
    }

    /// Start the worker, answering whether one is actually running.
    fn start_writer(writer: &Arc<Writer>, set: u64) -> bool {
        let Some(read) = writer.wake.read_end() else {
            report("no writer wake pipe; Cinder's mood is saved on the frame loop");
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
            report("writer wake refused; Cinder's mood is saved on the frame loop");
            return false;
        }
        let served = Arc::clone(writer);
        match tairix_rt::thread::Thread::spawn(move || served.serve()) {
            Ok(_) => true,
            Err(err) => {
                report(&alloc::format!(
                    "no writer thread ({err:?}); Cinder's mood is saved on the frame loop"
                ));
                false
            }
        }
    }

    /// Read the saved mood, falling back on a fresh companion.
    fn load_saved() -> Saved {
        let mut host = RtHost;
        let settings = Settings::open_without_defaults(&mut host);
        match settings.document() {
            Ok(document) => Saved::from_document(&document),
            // A store the service could not serve leaves a fresh companion,
            // which is the ordinary first run rather than a fault.
            Err(_) => Saved::default(),
        }
    }

    /// A generator seeded from the kernel, so two companions on one desktop do
    /// not behave identically.
    fn seeded_rng() -> FastRng {
        let mut seed = [0u8; 32];
        if tairix_rt::random_get(&mut seed, tairix_abi::RandomFlags::empty()).is_err() {
            // A companion seeded from the clock is a less varied companion,
            // not a broken one, and refusing to start over it would be dying
            // of an unpredictability requirement a pet does not have.
            report("the system generator is unavailable; seeding this session from the clock");
            return FastRng::seed_from_u64(tairix_rt::clock_get());
        }
        FastRng::from_key(&seed)
    }

    // ---- the companion on the desktop -----------------------------------

    /// Cinder out on the desktop: his surface, where he is, and what he is
    /// doing about the window in front of him.
    struct Loose {
        pane: WindowPane,
        surface: Surface,
        /// Where he is and where he is going: the whole of the movement, and
        /// host-tested, because this binary is reachable by no test at all.
        roam: Roam,
        /// The terrain generation last pulled, so a pull is made only when
        /// the answer would differ.
        terrain_seen: Option<u64>,
    }

    impl Loose {
        /// Open the layer surface and put Cinder on the desktop at `at`.
        fn open(
            client: &mut WindowClient<tairix_window::app::RtWindowTransport>,
            server: ProcId,
            event_endpoint: u64,
            scale: Scale,
            at: Ground,
        ) -> Result<Self, Refusal> {
            let side = scale.scale_length(COMPANION_SIDE);
            let mode = app::mode_for(side, side);
            let Some(surface) = Surface::new(side, side) else {
                return Err(Refusal::NoDesktop);
            };
            let origin = companion_origin(at, side);
            let pane = WindowPane::open_layer(
                client,
                server,
                event_endpoint,
                &mode,
                (origin.x, origin.y),
                LayerDepth::Below,
            )
            .map_err(|err| match err.errno() {
                Errno::PermissionDenied => Refusal::NotPermitted,
                Errno::LimitExceeded => Refusal::SeatFull,
                _ => Refusal::NoDesktop,
            })?;
            Ok(Self {
                pane,
                surface,
                roam: Roam::new(at),
                terrain_seen: None,
            })
        }
    }

    /// Everything the frame loop reads and writes, in one place so the loop's
    /// own body stays legible.
    struct Companion {
        pen: Pen,
        mind: Mind<FastRng>,
        world: World,
        pose: Pose,
        painter: Painter,
        loose: Option<Loose>,
        /// Seconds since the program started, for the idle animations.
        elapsed: f64,
        /// Whether the mood has moved since the last save.
        dirty: bool,
    }

    impl Companion {
        /// Whether anything is moving, which is what decides between a frame
        /// deadline and no deadline at all.
        fn is_animated(&self) -> bool {
            if self.loose.is_some() {
                return self.mind.intent() != Intent::Nap;
            }
            // In the pen he still breathes and his tail still moves unless he
            // is asleep; a sleeping companion in a closed pen owes nothing.
            self.mind.intent() != Intent::Nap
        }

        /// What Cinder is saved as right now.
        fn saved(&self) -> Saved {
            Saved {
                needs: self.mind.needs(),
                was_loose: self.pen.whereabouts() == Whereabouts::Loose,
            }
        }
    }

    /// Advance the whole companion by one frame.
    ///
    /// Returns whether the pen needs repainting; the layer surface presents
    /// itself, because only it knows whether it moved.
    fn advance_frame(
        companion: &mut Companion,
        client: &mut WindowClient<tairix_window::app::RtWindowTransport>,
        layout: &PenLayout,
    ) -> bool {
        companion.elapsed += FRAME_SECONDS;
        let dt = FRAME_SECONDS;

        let Some(loose) = companion.loose.as_mut() else {
            return advance_in_pen(companion, layout, dt);
        };

        let stepped = loose
            .roam
            .advance(&mut companion.mind, &companion.world, dt);
        let intent = stepped.intent;
        let advanced = stepped.advanced;

        // Pose, then present, then place: the depth flip and the move travel
        // together so a frame is never shown at the old depth.
        let cheer = companion.mind.needs().cheer();
        companion.pose = Pose {
            at: loose.roam.at(),
            heading: loose.roam.heading(),
            crouch: advanced.crouch,
            lift: advanced.lift,
            smile: smile_for(intent, cheer),
            eyes: eyes_for(intent, companion.elapsed),
            ..companion.pose
        };
        gait::animate(
            &mut companion.pose,
            stepped.travelled,
            stepped.turn_rate,
            dt,
            companion.elapsed,
        );

        let side = loose.surface.width();
        companion
            .painter
            .draw_companion(&mut loose.surface, &companion.pose, side);
        let damage = DamageRect::full(loose.pane.mode());
        if loose.pane.present(client, &loose.surface, damage).is_err() {
            // The session refused the frame; the next one will re-attach, and
            // a channel that has really gone is caught by the event drain.
            return false;
        }
        let origin = companion_origin(loose.roam.at(), side);
        let _ = loose
            .pane
            .place(client, (origin.x, origin.y), advanced.route.depth());
        companion.dirty = true;
        false
    }

    /// Advance Cinder inside the pen, answering whether it needs repainting.
    fn advance_in_pen(companion: &mut Companion, layout: &PenLayout, dt: f64) -> bool {
        let intent = companion.mind.tick(dt, None, false);
        let cheer = companion.mind.needs().cheer();
        companion.pose = Pose {
            at: companion.pen.at(),
            heading: PEN_HEADING,
            smile: smile_for(intent, cheer),
            eyes: eyes_for(intent, companion.elapsed),
            ..companion.pose
        };
        gait::animate(&mut companion.pose, 0.0, 0.0, dt, companion.elapsed);
        let _ = layout;
        companion.dirty = true;
        true
    }

    /// Which way Cinder faces in the pen: slightly towards the camera, so the
    /// user sees his face rather than his flank.
    const PEN_HEADING: f64 = -core::f64::consts::FRAC_PI_2 - 0.35;

    /// How broadly Cinder is smiling, given what he is doing and how cheerful
    /// he is.
    ///
    /// A sleeping face is level; everything else curves with his mood, and
    /// playing lifts it further than his mood alone would.
    fn smile_for(intent: Intent, cheer: f64) -> f64 {
        match intent {
            Intent::Nap => 0.0,
            Intent::Chase | Intent::Pounce => mathf::clamp(cheer + 0.35, 0.0, 1.0),
            _ => cheer,
        }
    }

    /// The eye state for `intent`, with a blink folded in.
    fn eyes_for(intent: Intent, elapsed: f64) -> Eyes {
        if intent == Intent::Nap {
            return Eyes::Shut;
        }
        // A blink every few seconds, lasting a fraction of one: a face that
        // never blinks reads as a mask.
        let phase = elapsed - mathf::floor(elapsed / BLINK_PERIOD) * BLINK_PERIOD;
        if phase < BLINK_SECONDS {
            Eyes::Half
        } else {
            Eyes::Open
        }
    }

    /// How often Cinder blinks, in seconds.
    const BLINK_PERIOD: f64 = 4.3;

    /// How long one blink lasts.
    const BLINK_SECONDS: f64 = 0.16;

    /// Declare the icon-bar presence: the *Out*/*Home* row between the
    /// convention's fixed ends.
    fn declare_app_bar(
        client: &mut WindowClient<tairix_window::app::RtWindowTransport>,
        event_endpoint: u64,
        loose: bool,
    ) {
        let text = if loose {
            "Bring Cinder home"
        } else {
            "Let Cinder out"
        };
        let Some(row) = AppMenuItemId::new(ROW_OUT)
            .ok()
            .zip(AppMenuLabel::new(text).ok())
            .map(|(id, label)| AppMenuRow::Item(AppMenuItem::new(id, label)))
        else {
            report("this application's icon-bar menu is invalid; carrying on without one");
            return;
        };
        let rows = alloc::vec![row];
        match tairix_window::declaration(event_endpoint, AppBarClick::RaiseOrOpen, &rows) {
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // From here this task drives a user-facing loop, so declare the frame
        // it owes. The *budget* is how long a frame may take before a debug
        // image reports it, which is far longer than the period the loop
        // actually animates at.
        let _ = tairix_rt::latency_watch(DEFAULT_FRAME_BUDGET_NS);

        let mut window = AppWindow::new();
        let (mut desktop, mut themes) = match app::bring_up_desktop(window.client()) {
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
        let _guard = WriterGuard(Arc::clone(&writer));

        let saved = load_saved();
        let mut companion = Companion {
            pen: Pen::new(),
            mind: Mind::resuming(seeded_rng(), saved.needs),
            world: World::new(),
            pose: Pose::default(),
            painter: Painter::new(),
            loose: None,
            elapsed: 0.0,
            dirty: false,
        };

        declare_app_bar(window.client(), event_endpoint, saved.was_loose);

        let (pen_w, pen_h) = pen_client(desktop.scale());
        let mode = app::mode_for(pen_w, pen_h);
        let server = match window.open(event_endpoint, &mode, "Cinder", pen_sizing(desktop.scale()))
        {
            Ok(server) => server,
            Err(err) => return fail_shell(err),
        };
        let mut pen_layout = layout::pen(
            Rect::new(0, 0, pen_w, pen_h),
            desktop.scale(),
            themes.active(),
        );
        companion.pen.settle(&pen_layout);

        // A companion who was out when the session ended goes back out, which
        // is what "where you left him" means. A refusal here is stated and
        // the pen carries on.
        if saved.was_loose {
            let_out(
                &mut companion,
                &mut window,
                server,
                event_endpoint,
                &desktop,
            );
        }

        run_loop(
            &mut companion,
            &mut window,
            &mut desktop,
            &mut themes,
            &mut pen_layout,
            &writer,
            armed,
            binding.set(),
            event_endpoint,
            server,
        )
    }

    /// Let Cinder out, stating the reason if the desktop will not have him.
    fn let_out(
        companion: &mut Companion,
        window: &mut AppWindow,
        server: ProcId,
        event_endpoint: u64,
        desktop: &Desktop,
    ) {
        let start = start_point(desktop);
        match Loose::open(
            window.client(),
            server,
            event_endpoint,
            desktop.scale(),
            start,
        ) {
            Ok(loose) => {
                companion.loose = Some(loose);
                companion.pen.let_out(None);
                companion.world.set_area(area_of(desktop));
            }
            Err(refusal) => {
                companion.pen.let_out(Some(refusal));
                report(refusal.reason());
            }
        }
    }

    /// Where a freshly loosed companion starts: the middle of the desktop, so
    /// he is visible wherever the pen happens to be.
    fn start_point(desktop: &Desktop) -> Ground {
        let info = desktop.info();
        Ground::new(
            f64::from(info.screen_width_px()) / 2.0,
            f64::from(info.screen_height_px()) * 0.6,
        )
    }

    /// The area Cinder may walk in: the whole screen, since the session clamps
    /// him onto its own work area anyway and a companion that stopped short of
    /// the icon bar would look fenced.
    fn area_of(desktop: &Desktop) -> Area {
        let info = desktop.info();
        Area {
            left: 0,
            top: 0,
            right: i32::try_from(info.screen_width_px()).unwrap_or(i32::MAX),
            bottom: i32::try_from(info.screen_height_px()).unwrap_or(i32::MAX),
        }
    }

    /// Send Cinder out, or bring him home — whichever he is not.
    ///
    /// The one place both the strip's button and the icon-bar row resolve to,
    /// so the two can never disagree about what the action does or leave the
    /// bar's label behind.
    fn toggle_whereabouts(
        companion: &mut Companion,
        window: &mut AppWindow,
        desktop: &Desktop,
        pen_layout: &PenLayout,
        event_endpoint: u64,
        server: ProcId,
    ) {
        if companion.loose.is_some() {
            put_away(companion, window, pen_layout);
        } else {
            let_out(companion, window, server, event_endpoint, desktop);
        }
        declare_app_bar(window.client(), event_endpoint, companion.loose.is_some());
        companion.dirty = true;
    }

    /// Take Cinder off the desktop, closing his surface.
    fn put_away(companion: &mut Companion, window: &mut AppWindow, pen_layout: &PenLayout) {
        if let Some(loose) = companion.loose.take() {
            let _ = loose.pane.close(window.client());
        }
        companion.pen.put_away(pen_layout);
        companion.dirty = true;
    }

    /// The frame loop.
    #[allow(clippy::too_many_arguments)] // The loop's whole surround, threaded explicitly.
    #[allow(clippy::too_many_lines)] // One loop, one place to read it.
    fn run_loop(
        companion: &mut Companion,
        window: &mut AppWindow,
        desktop: &mut Desktop,
        themes: &mut ThemeRegistry,
        pen_layout: &mut PenLayout,
        writer: &Arc<Writer>,
        armed: bool,
        set: u64,
        event_endpoint: u64,
        server: ProcId,
    ) -> i32 {
        let mut mailbox = EventMailbox::new(event_endpoint, server);
        let mut pen_open = true;
        let mut plates = [TerrainPlate {
            x: 0,
            y: 0,
            width_px: 1,
            height_px: 1,
        }; DESKTOP_LAYER_MAX_PLATES as usize];

        loop {
            // A frame deadline while anything moves; none at all when nothing
            // does, so a dozing companion costs no wake.
            let wake = if companion.is_animated() {
                let deadline = tairix_rt::clock_get().saturating_add(FRAME_NS);
                match app::park_until(set, deadline) {
                    Ok(wake) => wake,
                    Err(_) => return fail(EXIT_CHANNEL_LOST, "the wait set was torn down"),
                }
            } else {
                match app::park(set) {
                    Ok(wake) => Some(wake),
                    Err(_) => return fail(EXIT_CHANNEL_LOST, "the wait set was torn down"),
                }
            };

            match wake {
                Some(Wake::Event) => {}
                Some(Wake::DesktopChanged) => {
                    if app::adopt_desktop(desktop, themes).unwrap_or(false) {
                        companion.world.set_area(area_of(desktop));
                        companion.dirty = true;
                    }
                    continue;
                }
                Some(Wake::App(WRITER_TOKEN)) => {
                    if let Some(Err(err)) = writer.collect() {
                        report(&alloc::format!("could not save Cinder's mood ({err:?})"));
                    }
                    continue;
                }
                Some(Wake::PressureChanged | Wake::PressureUnchanged | Wake::App(_)) => continue,
                // The frame deadline: advance and present.
                None => {
                    let repaint = advance_frame(companion, window.client(), pen_layout);
                    if repaint && pen_open {
                        present_pen(
                            companion,
                            window,
                            pen_layout,
                            themes.active(),
                            desktop.scale(),
                        );
                    }
                    continue;
                }
            }

            // Drain every event the mailbox holds.
            let mut frame = [0u8; WindowEvent::WIRE_LEN];
            loop {
                match mailbox.try_next(&mut frame) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(_) => return fail(EXIT_CHANNEL_LOST, "the event channel was lost"),
                }
                let Ok(event) = WindowEvent::from_bytes(&frame) else {
                    // A frame this build will not decode is already consumed
                    // and nothing was guessed at; the next read moves past it.
                    continue;
                };
                match event {
                    WindowEvent::CloseRequested { .. } => {
                        // Closing the pen never quits: a resident icon-bar
                        // application stays on the bar, and *Quit* is what
                        // ends it. Closing with Cinder inside puts him away;
                        // closing while he is out leaves him roaming.
                        let _ = window.close();
                        pen_open = false;
                    }
                    WindowEvent::AppBarDefault => {
                        if !pen_open {
                            pen_open = reopen_pen(
                                window,
                                desktop,
                                themes.active(),
                                event_endpoint,
                                pen_layout,
                            );
                            if pen_open {
                                companion.pen.settle(pen_layout);
                                companion.dirty = true;
                            }
                        }
                    }
                    WindowEvent::AppBarMenu { item, .. } => {
                        if item.get() == tairix_window::QUIT_ROW {
                            // *Quit* ends him, and takes him off the desktop
                            // on the way out.
                            put_away(companion, window, pen_layout);
                            writer.submit(companion.saved(), armed);
                            return 0;
                        }
                        if item.get() == ROW_OUT {
                            toggle_whereabouts(
                                companion,
                                window,
                                desktop,
                                pen_layout,
                                event_endpoint,
                                server,
                            );
                        }
                    }
                    WindowEvent::Resized {
                        width_px,
                        height_px,
                        ..
                    } => {
                        if window.resize(app::mode_for(width_px, height_px)) {
                            *pen_layout = layout::pen(
                                Rect::new(0, 0, width_px, height_px),
                                desktop.scale(),
                                themes.active(),
                            );
                            companion.pen.settle(pen_layout);
                            companion.dirty = true;
                        }
                    }
                    WindowEvent::RedrawRequested { .. } | WindowEvent::ContentReleased { .. } => {
                        companion.dirty = true;
                    }
                    WindowEvent::TerrainChanged { generation, .. } => {
                        // Pulled only when the answer would differ from the
                        // one already held.
                        if companion.loose.as_ref().and_then(|l| l.terrain_seen) != Some(generation)
                        {
                            pull_terrain(companion, window.client(), &mut plates, generation);
                        }
                    }
                    WindowEvent::LayerPointer { x, y, .. } => {
                        if let Some(loose) = companion.loose.as_mut() {
                            loose
                                .roam
                                .see_pointer(Some(Ground::new(f64::from(x), f64::from(y))));
                        }
                    }
                    WindowEvent::Pointer {
                        window_id,
                        x,
                        y,
                        action,
                        ..
                    } => {
                        let mut toggle = false;
                        handle_pointer(companion, pen_layout, window_id, x, y, action, &mut toggle);
                        if toggle {
                            toggle_whereabouts(
                                companion,
                                window,
                                desktop,
                                pen_layout,
                                event_endpoint,
                                server,
                            );
                        }
                    }
                    _ => {}
                }
            }

            if companion.dirty && pen_open {
                present_pen(
                    companion,
                    window,
                    pen_layout,
                    themes.active(),
                    desktop.scale(),
                );
            }
        }
    }

    /// Pull the terrain and adopt it.
    fn pull_terrain(
        companion: &mut Companion,
        client: &mut WindowClient<tairix_window::app::RtWindowTransport>,
        plates: &mut [TerrainPlate],
        generation: u64,
    ) {
        let Some(loose) = companion.loose.as_mut() else {
            return;
        };
        match client.take_terrain(loose.pane.id(), plates) {
            Ok(answer) => {
                companion.world.adopt_terrain(answer);
                loose.terrain_seen = Some(generation);
            }
            Err(_) => {
                // A refused pull leaves the terrain he already knows, which is
                // stale rather than wrong: he walks the desktop he last saw.
                loose.terrain_seen = None;
            }
        }
    }

    /// Route a pointer event into the pen.
    fn handle_pointer(
        companion: &mut Companion,
        pen_layout: &PenLayout,
        window_id: u64,
        x: u32,
        y: u32,
        action: PointerAction,
        toggle: &mut bool,
    ) {
        let point = pointer_point(x, y);
        // A press anywhere on the companion surface is a pet: the session has
        // already hit-tested his silhouette, so a press that arrives at all
        // landed on fur.
        if companion
            .loose
            .as_ref()
            .is_some_and(|loose| loose.pane.id() == window_id)
        {
            if matches!(action, PointerAction::Pressed(_)) {
                companion.mind.petted();
                companion.dirty = true;
            }
            return;
        }
        let mut damage = Region::new();
        for input in pointer_input_events(action, point) {
            // The control sees every event first: it owns its own hover and
            // press state, and the strip is never the floor.
            if let Some(fired) = companion
                .pen
                .button_pointer(&input, pen_layout, &mut damage)
            {
                *toggle = fired == PenAction::Toggled;
            }
            if !damage.is_empty() {
                companion.dirty = true;
                damage.clear();
            }
            let outcome = match input {
                InputEvent::PointerPressed { .. } => companion.pen.press(point, pen_layout),
                InputEvent::PointerReleased { .. } => companion.pen.release(point, pen_layout),
                InputEvent::PointerMoved { .. } => {
                    if companion.pen.motion(point, pen_layout) {
                        companion.dirty = true;
                    }
                    PenAction::Nothing
                }
                _ => PenAction::Nothing,
            };
            if outcome == PenAction::Petted {
                companion.mind.petted();
            }
            if outcome != PenAction::Nothing {
                companion.dirty = true;
            }
        }
    }

    /// The pen's sizing contract: resizable, with the floor the layout needs
    /// to still hold its furniture.
    fn pen_sizing(scale: Scale) -> WindowSizing {
        WindowSizing::Resizable {
            min_width_px: scale.scale_length(layout::PEN_MIN_WIDTH),
            min_height_px: scale.scale_length(layout::PEN_MIN_HEIGHT),
        }
    }

    /// Re-open the pen after it was closed from the icon bar.
    fn reopen_pen(
        window: &mut AppWindow,
        desktop: &Desktop,
        theme: &Theme,
        event_endpoint: u64,
        pen_layout: &mut PenLayout,
    ) -> bool {
        let (w, h) = pen_client(desktop.scale());
        let mode = app::mode_for(w, h);
        match window.open(event_endpoint, &mode, "Cinder", pen_sizing(desktop.scale())) {
            Ok(_) => {
                *pen_layout = layout::pen(Rect::new(0, 0, w, h), desktop.scale(), theme);
                true
            }
            Err(err) => {
                // A refused re-open is reported and the application stays on
                // the bar: it is a click that did not work, not a fault.
                report(&alloc::format!("{err}"));
                false
            }
        }
    }

    /// Paint and present the pen.
    fn present_pen(
        companion: &mut Companion,
        window: &mut AppWindow,
        pen_layout: &PenLayout,
        theme: &Theme,
        scale: Scale,
    ) {
        companion.dirty = false;
        let pen = &companion.pen;
        let painter = &mut companion.painter;
        let pose = Pose {
            at: pen.at(),
            ..companion.pose
        };
        let _ = window.present(DamageRect::full(&pen_mode(pen_layout)), |surface| {
            painter.draw_pen(surface, pen_layout, pen, &pose, theme, scale);
        });
    }

    /// The mode the pen's damage is expressed against.
    fn pen_mode(pen_layout: &PenLayout) -> DisplayMode {
        app::mode_for(pen_layout.client.width, pen_layout.client.height)
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
/// On a hosted target the bundle's `Run` binary is inert: the freestanding
/// program above is what the image carries, and this keeps the file inside
/// `cargo build --workspace`, clippy, and fmt.
#[cfg(not(freestanding))]
fn main() {}
