//! The `WinterSun.app` bundle's `Run` entry point: the game client.
//!
//! Everything with behaviour lives in the host-tested library
//! (`tairix_wintersun_app`); this binary composes it over the live window
//! channel:
//!
//! * one window, whose three size states are asked for and adopted from
//!   the compositor's answer rather than assumed;
//! * one `port_bind`-bound event mailbox the client parks on, carrying
//!   the next frame's deadline so a still window costs no spin;
//! * a worker the chunk generation is handed to, because a frame owes
//!   the window a picture and cannot wait on a realm being solved;
//! * the frame drawn straight into the window's own surface where the
//!   render scale is native, and through a resample only when the
//!   degradation ladder has shrunk the target;
//! * the player's figure: the default preset read once, before the window
//!   opens, from the bundle's own `Resources/`, and posed each frame at the
//!   moment that frame shows;
//! * `--reference-scene`: the one fixed scene (`reference`) in the same
//!   window, drawn only when its extent or its retained pixels change, so a
//!   picture of the window can be checked against the scene drawn elsewhere.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(all(freestanding, feature = "run"))]
extern crate alloc;

#[cfg(all(freestanding, feature = "run"))]
mod program {
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use tairix_abi::driver::display::{DamageRect, DisplayMode};
    use tairix_abi::fs::OpenFlags;
    use tairix_abi::window_ipc::WindowEvent;
    use tairix_abi::{Errno, ProcId, WaitSetOp, WaitSourceKind};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_log::{Event, Sink};
    use tairix_parallel::{JobRunner, Pool};
    use tairix_raster::surface::Surface;
    use tairix_rt::io::{Stderr, Stdout, Write};
    use tairix_rt::File;
    use tairix_util::defer::JobDesk;
    use tairix_window::app::{self, AppWindow, ShellError, Wake, EXIT_CHANNEL_LOST};
    use tairix_window::desktop::Desktop;
    use tairix_window::{EventDrain, EventError, EventMailbox, EventSource, Parked, WindowEvents};
    use tairix_wintersun_app::budget::{FrameTimes, Governor};
    use tairix_wintersun_app::camera::{realm_bounds, Camera, Zoom};
    use tairix_wintersun_app::cli::{self, CliError, Launch, USAGE};
    use tairix_wintersun_app::error::ClientError;
    use tairix_wintersun_app::figures::{submerged, Cast};
    use tairix_wintersun_app::frame::{Clock, Renderer, Scene};
    use tairix_wintersun_app::input::{Command, Controls, Zoom as ZoomWay};
    use tairix_wintersun_app::light::{Sky, Sun};
    use tairix_wintersun_app::pacing::{Motion, Pacer};
    use tairix_wintersun_app::presets;
    use tairix_wintersun_app::quality::{Ladder, RenderScale};
    use tairix_wintersun_app::reference;
    use tairix_wintersun_app::shell::{self, Shell};
    use tairix_wintersun_app::terrain::{visible_chunks, HeldGround, RoadDecals};
    use tairix_wintersun_app::view::Viewport;
    use tairix_wintersun_art::cache::MaterialCache;
    use tairix_wintersun_art::decal::Fray;
    use tairix_wintersun_art::splat::Warp;
    use tairix_wintersun_figure::actor::Actor;
    use tairix_wintersun_figure::identity::{Identity, RECORD_LEN};
    use tairix_wintersun_figure::motion::{Clips, Set};
    use tairix_wintersun_figure::reference as figures;
    use tairix_wintersun_figure::species::Species;
    use tairix_wintersun_net::client::{Intent, IntentKind};
    use tairix_wintersun_net::value::{
        ChunkCoord, EntityId, EntityKind, Facing, TickInstant, TickPhase, WorldPoint,
    };
    use tairix_wintersun_rules::clock::TickRate;
    use tairix_wintersun_rules::entity::SpawnSpec;
    use tairix_wintersun_rules::stat::Stats;
    use tairix_wintersun_rules::terrain::ChunkTerrain;
    use tairix_wintersun_rules::zone::Zone;
    use tairix_wintersun_world::chunk::{Chunk, ChunkBuild, ChunkWindow};
    use tairix_wintersun_world::params::RealmParams;
    use tairix_wintersun_world::realm::RealmField;

    /// The wait-set token the chunk worker's answer wake arrives under.
    const QUARRY_TOKEN: u64 = app::FIRST_APP_TOKEN;

    /// Exit code for a realm that would not generate.
    const EXIT_NO_REALM: i32 = 85;

    /// Exit code for a player whose figure could not be built.
    const EXIT_NO_FIGURE: i32 = 86;

    /// Exit code for a reference scene that could not be drawn.
    const EXIT_NO_SCENE: i32 = 87;

    /// Exit code for a command line outside the grammar.
    const EXIT_USAGE: i32 = 2;

    /// The body the camera follows: an ordinary entity of the local zone,
    /// so it walks around hills rather than through them.
    const PLAYER_KIND: EntityKind = EntityKind(1);

    /// How long the client waits for the next frame when it is running.
    ///
    /// The refresh the budget is stated at. A display protocol that
    /// serialises presents paces the client below this on its own; the
    /// deadline is what stops a still window spinning above it.
    const FRAME_INTERVAL_NS: u64 = 1_000_000_000 / 60;

    /// State an abnormal exit's reason on `stderr` and hand `code` back.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "wintersun: {reason}");
        code
    }

    /// State a shared-shell bring-up refusal and hand its reserved code
    /// back.
    fn fail_shell(err: ShellError) -> i32 {
        let _ = writeln!(Stderr, "wintersun: {err}");
        err.code()
    }

    /// Report a refusal the game carries on from.
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "wintersun: {reason}");
    }

    /// The client's park: its event mailbox, the chunk worker's answer
    /// wake, and the deadline of the next frame it owes.
    struct Park<'a> {
        mailbox: EventMailbox,
        set: u64,
        /// The chunk worker, or `None` for a scene whose ground is solved
        /// before it is drawn.
        quarry: Option<&'a Quarry>,
        /// When the next frame is due, or `None` when the client owes
        /// none — a window with no seat, where the park carries no
        /// deadline at all and the CPU is given up entirely.
        ///
        /// Shared with the loop through a cell because the loop owns the
        /// deadline and the source owns the park: one writes it just
        /// before the other reads it, on the one thread both run on.
        deadline_ns: &'a core::cell::Cell<Option<u64>>,
        /// Set when the park woke for a change of memory-pressure band,
        /// cleared when the loop gives the caches back. Shared through a
        /// cell for the same reason the deadline is: the park writes it,
        /// the loop reads it, on one thread.
        pressure_moved: &'a core::cell::Cell<bool>,
    }

    impl EventDrain for Park<'_> {
        fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
            self.mailbox.try_next(event)
        }
    }

    impl EventSource for Park<'_> {
        fn park(&mut self) -> Result<Parked, Errno> {
            let woken = match self.deadline_ns.get() {
                // One-shot, to the frame actually owed: no periodic tick,
                // and no timer armed at all while the game is not running.
                Some(deadline) => match app::park_until(self.set, deadline)? {
                    Some(woken) => woken,
                    None => return Ok(Parked::Interrupted),
                },
                None => app::park(self.set)?,
            };
            match woken {
                Wake::App(QUARRY_TOKEN) => {
                    // The readiness is a level peek, so leaving it
                    // undrained would report ready for ever and turn the
                    // park into a spin.
                    if let Some(quarry) = self.quarry {
                        quarry.wake.drain();
                    }
                    Ok(Parked::Interrupted)
                }
                // The band moved, so the material tiles the frame holds
                // are given back before the next one asks for more. The
                // cache is the loop's, so the loop does it.
                Wake::PressureChanged => {
                    self.pressure_moved.set(true);
                    Ok(Parked::Interrupted)
                }
                Wake::DesktopChanged | Wake::Event | Wake::PressureUnchanged | Wake::App(_) => {
                    Ok(Parked::Served)
                }
            }
        }
    }

    /// The monotonic clock the per-pass measurement reads.
    struct Monotonic;

    impl Clock for Monotonic {
        fn now_ns(&self) -> u64 {
            tairix_rt::clock_get()
        }
    }

    /// The audit sink the material cache charges a refusal through.
    struct Journal;

    impl Sink for Journal {
        fn write_event(&self, event: &Event<'_>) {
            let _ = writeln!(Stderr, "wintersun: {}", event.message);
        }
    }

    /// The machine's memory, which the material cache is a small share of.
    ///
    /// Asked once, before the window opens. A system that will not say
    /// admits no tiles rather than a guessed amount, and the materials are
    /// drawn in their flat tones.
    fn cache_backing_bytes() -> usize {
        match tairix_procinfo::memory_total_bytes(&tairix_procinfo::IpcTransport) {
            Ok(total) => usize::try_from(total).unwrap_or(usize::MAX),
            Err(err) => {
                report(&alloc::format!(
                    "memory size unavailable ({err:?}); materials drawn in their flat tones"
                ));
                0
            }
        }
    }

    /// The chunk generator, run off the frame loop.
    ///
    /// Solving a chunk is tens of milliseconds of relief, hydrology and
    /// biome work — several frames' worth — so the loop *asks* and
    /// collects what has arrived. A view whose ground has not come back
    /// yet draws it as ground the client does not hold, which is what it
    /// is.
    /// What the quarry answers with.
    ///
    /// A refusal names its coordinate so the loop can stop asking: a
    /// chunk the generator will not produce is ground the client does
    /// not hold, drawn as such, rather than a request re-submitted every
    /// frame for ever.
    enum Quarried {
        /// The ground, solved.
        Ready(Chunk),
        /// The generator refused this coordinate.
        Refused(ChunkCoord),
    }

    struct Quarry {
        desk: tairix_rt::sync::Mutex<JobDesk<ChunkCoord, Quarried>>,
        signal: tairix_rt::sync::Condvar,
        wake: tairix_rt::sync::WorkerWake,
        field: RealmField,
    }

    impl Quarry {
        fn new(field: RealmField) -> Self {
            Self {
                desk: tairix_rt::sync::Mutex::new(JobDesk::new()),
                signal: tairix_rt::sync::Condvar::new(),
                wake: tairix_rt::sync::WorkerWake::create(),
                field,
            }
        }

        /// Ask for `coord`, or — with no worker to take it — solve it
        /// here. Slower on a single core, never a chunk that never comes.
        ///
        /// The desk holds one request, so a newer ask displaces an older
        /// one that has not been taken. That is the right policy here:
        /// the nearest missing chunk is always the best thing to be
        /// solving, and a displaced one is simply asked for again next
        /// frame if it is still wanted.
        fn request(&self, coord: ChunkCoord, armed: bool) -> Option<Quarried> {
            if !armed {
                return Some(Self::solve(&self.field, coord));
            }
            if self.desk.lock().submit(coord).wake {
                self.signal.notify_one();
            }
            None
        }

        fn collect(&self) -> Option<Quarried> {
            self.desk.lock().collect()
        }

        /// Ask the worker to leave, and wake it so it can.
        fn stop(&self) {
            self.desk.lock().stop();
            self.signal.notify_all();
        }

        fn solve(field: &RealmField, coord: ChunkCoord) -> Quarried {
            ChunkBuild::new(coord)
                .ok()
                .and_then(|build| build.finish(field).ok())
                .map_or(Quarried::Refused(coord), Quarried::Ready)
        }

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
                let answer = Self::solve(&self.field, job);
                if self.desk.lock().deliver(answer) {
                    self.wake.nudge();
                }
            }
        }
    }

    /// The realm the client is looking at, and the ground it holds.
    struct World {
        params: RealmParams,
        roads: RoadDecals,
        warp: Warp,
        fray: Fray,
        ground: HeldGround,
        refused: Vec<ChunkCoord>,
        /// Whether the last solved chunk found no room to be held, so a run
        /// of them is reported once.
        unheld: bool,
    }

    impl World {
        fn new(params: RealmParams, field: &RealmField) -> Result<Self, ClientError> {
            Ok(Self {
                params,
                roads: RoadDecals::from_realm(field)?,
                warp: Warp::new(params.seed()),
                fray: Fray::new(params.seed()),
                ground: HeldGround::new(),
                refused: Vec::new(),
                unheld: false,
            })
        }

        /// Ask the quarry for the nearest chunk the view needs and has
        /// not got.
        ///
        /// One, because the desk holds one: asking for the nearest every
        /// frame fills the view outward from the player and re-asks for
        /// anything a newer ask displaced, with no list of outstanding
        /// requests to keep in step with what is still visible.
        fn request_visible(
            &mut self,
            camera: &Camera,
            view: &Viewport,
            quarry: &Quarry,
            armed: bool,
        ) {
            let centre = camera.centre(view);
            let nearest = visible_chunks(camera.visible(view))
                .filter(|coord| {
                    self.params.holds_chunk(coord.x, coord.y)
                        && !self.ground.holds(*coord)
                        && !self.refused.contains(coord)
                })
                .min_by_key(|coord| chunk_distance(*coord, centre));
            if let Some(coord) = nearest {
                if let Some(answer) = quarry.request(coord, armed) {
                    self.take(answer);
                }
            }
        }

        /// Record what the quarry answered.
        fn take(&mut self, answer: Quarried) {
            match answer {
                Quarried::Ready(chunk) => {
                    let held = self.ground.adopt(chunk).is_ok();
                    if !held && !self.unheld {
                        report("no memory to hold solved ground; it is drawn as unmapped");
                    }
                    self.unheld = !held;
                }
                Quarried::Refused(coord) => {
                    if !self.refused.contains(&coord) {
                        self.refused.push(coord);
                        report("ground refused by the generator; drawn as unmapped");
                    }
                }
            }
        }
    }

    /// How far a chunk's centre is from a world point, squared, so the
    /// nearest missing ground is solved first.
    fn chunk_distance(coord: ChunkCoord, from: WorldPoint) -> i64 {
        let cells = i64::from(tairix_wintersun_world::geom::CHUNK_CELLS);
        let cell = i64::from(tairix_wintersun_world::geom::CELL_SUB_UNITS);
        let side = cells * cell;
        let cx = i64::from(coord.x) * side + side / 2;
        let cy = i64::from(coord.y) * side + side / 2;
        let (dx, dy) = (cx - i64::from(from.x), cy - i64::from(from.y));
        dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
    }

    /// Everything the loop owns between frames.
    struct Session<'a> {
        window: AppWindow,
        shell: Shell,
        camera: Camera,
        follow: Motion,
        /// The way the player's body faced at the last tick.
        facing: Facing,
        pacer: Pacer,
        controls: Controls,
        governor: Governor,
        renderer: Renderer,
        cache: MaterialCache,
        scaled: Option<Surface>,
        times: FrameTimes,
        cast: Cast<'a>,
        /// When the player's figure was last posed.
        posed_ns: Option<u64>,
        /// Whether the last frame was refused, so a run of them is reported
        /// once.
        refusing: bool,
    }

    /// Advance the simulation by `ticks` and record where the player got
    /// to.
    fn simulate(
        zone: &mut Zone,
        player: EntityId,
        world: &World,
        session: &mut Session<'_>,
        ticks: u32,
    ) {
        if ticks == 0 {
            return;
        }
        let Ok(borrowed) = world.ground.borrow() else {
            return;
        };
        let Ok(terrain) = ChunkTerrain::new(&borrowed) else {
            return;
        };
        for _ in 0..ticks {
            let intent = Intent {
                sequence: zone.tick() + 1,
                sampled: TickInstant {
                    tick: zone.tick(),
                    phase: TickPhase(0),
                },
                kind: IntentKind::Move(session.controls.direction()),
            };
            let _ = zone.submit(player, &intent);
            if zone.step(&terrain).is_err() {
                return;
            }
            if let Some(entity) = zone.entity(player) {
                session.follow.observe(entity.at());
                session.facing = entity.facing();
            }
        }
    }

    /// Move the player's figure to where this frame shows its body, over the
    /// real time since it was last posed, in the water it stands in there.
    fn pose_player(session: &mut Session<'_>, chunks: &[&Chunk], player: EntityId, now: u64) {
        let at = session.follow.at(session.pacer.alpha());
        // A paused game shows the moment it paused at, so no time passes for
        // its figure either.
        let nanos = match session.posed_ns {
            Some(then) if !session.pacer.paused() => now.saturating_sub(then),
            _ => 0,
        };
        session.posed_ns = Some(now);
        let depth = ChunkTerrain::new(chunks).map_or(0, |terrain| submerged(&terrain, at));
        let facing = session.facing;
        if let Some(figure) = session.cast.get_mut(player) {
            if let Err(err) = figure.step(nanos, at, facing, depth) {
                report(&alloc::format!("the player's figure could not move: {err}"));
            }
        }
    }

    /// Pose the player's figure for the moment this frame shows, then draw
    /// and present the frame, no deeper down the ladder than the window and
    /// the zoom let figures stay readable.
    ///
    /// Answers whether a frame reached the window, so only a frame that was
    /// drawn is measured against the budget.
    fn draw(
        session: &mut Session<'_>,
        world: &World,
        player: EntityId,
        runner: &dyn JobRunner,
        now: u64,
    ) -> Result<bool, Errno> {
        let Some(mode) = session.window.mode().copied() else {
            return Ok(false);
        };
        session.governor.hold(Ladder::floor(
            mode.width_px,
            mode.height_px,
            session.camera.zoom(),
        ));
        match draw_frame(session, world, player, runner, now, &mode) {
            Ok(()) => {
                session.refusing = false;
                Ok(true)
            }
            // Said once for a run of refused frames rather than once a frame.
            Err(Unpresented::Refused(err)) => {
                if !core::mem::replace(&mut session.refusing, true) {
                    report(&alloc::format!("frames refused: {err}"));
                }
                Ok(false)
            }
            Err(Unpresented::Lost(err)) => Err(err),
        }
    }

    /// Why a frame did not reach the window.
    enum Unpresented {
        /// It could not be drawn, and nothing was presented.
        Refused(ClientError),
        /// The session would not take it.
        Lost(Errno),
    }

    impl From<ClientError> for Unpresented {
        fn from(err: ClientError) -> Self {
            Self::Refused(err)
        }
    }

    impl From<Errno> for Unpresented {
        fn from(err: Errno) -> Self {
            Self::Lost(err)
        }
    }

    /// Draw the frame `mode`'s window shows and present it, withholding one
    /// that could not be drawn so the window keeps the last one that was.
    fn draw_frame(
        session: &mut Session<'_>,
        world: &World,
        player: EntityId,
        runner: &dyn JobRunner,
        now: u64,
        mode: &DisplayMode,
    ) -> Result<(), Unpresented> {
        let scale = session.governor.ladder().render_scale();
        let view = Viewport::new(mode.width_px, mode.height_px, scale)?;
        let borrowed = world.ground.borrow()?;
        let chunks = ChunkWindow::new(&borrowed).map_err(|_| ClientError::World)?;
        let decals = world.roads.decals()?;
        pose_player(session, &borrowed, player, now);
        let scene = Scene {
            camera: session.camera,
            chunks,
            decals: &decals,
            fray: &world.fray,
            warp: &world.warp,
            sun: Sun::winter(),
            sky: Sky::winter(),
            ladder: session.governor.ladder(),
            cast: &session.cast,
        };

        let (renderer, cache, scaled) = (
            &mut session.renderer,
            &mut session.cache,
            &mut session.scaled,
        );
        let mut times = FrameTimes::new();
        session
            .window
            .try_present(DamageRect::full(mode), |surface| {
                view.draw_into(surface, scaled, |target| {
                    renderer.render(target, &view, &scene, cache, runner, &Monotonic)
                })
                .map(|measured| times = measured)
            })??;
        session.times = times;
        Ok(())
    }

    /// What one read of the event stream came to.
    enum Served {
        /// An event was read, and applied or refused.
        Applied,
        /// Nothing was waiting.
        Empty,
        /// The client stops, with this exit code.
        Stop(i32),
    }

    /// Apply one read of the event stream.
    fn serve(session: &mut Session<'_>, read: Result<Option<WindowEvent>, EventError>) -> Served {
        match read {
            Ok(Some(event)) => {
                if apply(session, &event) {
                    let _ = session.window.close();
                    return Served::Stop(0);
                }
                Served::Applied
            }
            // A malformed frame is consumed and refused; the stream goes on.
            Err(EventError::Undecodable(_)) => Served::Applied,
            Ok(None) => Served::Empty,
            Err(EventError::Mailbox(_)) => {
                Served::Stop(fail(EXIT_CHANNEL_LOST, "event channel lost"))
            }
        }
    }

    /// Apply one delivered window event, answering whether the client
    /// should stop.
    fn apply(session: &mut Session<'_>, event: &WindowEvent) -> bool {
        match *event {
            WindowEvent::Resized {
                width_px,
                height_px,
                state,
                ..
            } => {
                session.shell.resized(width_px, height_px, state);
                session.window.resize(app::mode_for(width_px, height_px));
            }
            WindowEvent::Focus { focused, .. } => {
                if session.shell.focus(focused) {
                    session.controls.release_all();
                }
            }
            WindowEvent::Key { key, .. } => {
                if let Some(command) = session.controls.apply_key(&key) {
                    return command_applied(session, command);
                }
            }
            WindowEvent::Pointer { x, y, action, .. } => {
                session.controls.apply_pointer(x, y, action);
            }
            WindowEvent::Minimized { .. } => session.shell.minimized(),
            WindowEvent::CloseRequested { .. } => return true,
            WindowEvent::ContentReleased { .. } => session.window.release_frames(),
            _ => {}
        }
        false
    }

    /// Act on a client command, answering whether the client should stop.
    fn command_applied(session: &mut Session<'_>, command: Command) -> bool {
        match command {
            Command::Quit => return true,
            Command::Zoom(way) => {
                let moved = match way {
                    ZoomWay::In => session.camera.zoom().nearer(),
                    ZoomWay::Out => session.camera.zoom().further(),
                };
                if let Some(zoom) = moved {
                    session.camera.set_zoom(zoom);
                }
            }
            Command::Resize(want) => {
                let want = if want.is_fullscreen() {
                    session.shell.fullscreen_toggle()
                } else {
                    want
                };
                if let Some(ask) = session.shell.request(want) {
                    if let Some(id) = session.window.window_id() {
                        if let Err(err) = session.window.client().set_size_state(id, ask) {
                            report(&alloc::format!("size state refused: {err}"));
                        }
                    }
                }
            }
        }
        false
    }

    /// Print the bundle's own short help, or the usage banner where its Help
    /// tree cannot be read.
    fn short_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("wintersun"), locale, "wintersun")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Open the game's window at the size the budget is stated for, as the
    /// desktop's scale lays it out and no larger than its screen, answering
    /// the serving session's id or the exit code a refusal ends the client
    /// with.
    fn open_window(
        window: &mut AppWindow,
        desktop: &Desktop,
        endpoint: u64,
    ) -> Result<ProcId, i32> {
        let (width, height) = desktop.window_size(shell::OPEN_WIDTH, shell::OPEN_HEIGHT);
        let mode = app::mode_for(width, height);
        window
            .open(endpoint, &mode, "WinterSun", shell::SIZING)
            .map_err(fail_shell)
    }

    /// The client's whole life.
    ///
    /// Exit codes: `0` when the player leaves or short help is served, `2` for
    /// a command line outside the grammar, and otherwise the reason the
    /// client could not go on, stated on `stderr`.
    fn main() -> i32 {
        let launch = match tairix_rt::args().as_deref().map(cli::parse) {
            Some(Ok(launch)) => launch,
            Some(Err(CliError::Usage)) | None => return fail(EXIT_USAGE, USAGE),
        };
        if launch == Launch::Help {
            return short_help();
        }
        let mut window = AppWindow::new();
        let desktop = match app::bring_up_desktop(window.client()) {
            Ok((desktop, _themes)) => desktop,
            Err(err) => return fail_shell(err),
        };
        let binding = match app::bind_event_mailbox() {
            Ok(binding) => binding,
            Err(err) => return fail_shell(err),
        };
        if launch == Launch::ReferenceScene {
            return reference_scene(window, &desktop, binding.endpoint(), binding.set());
        }

        let params = RealmParams::winter_default(tairix_rt::clock_get());
        let Ok(field) = RealmField::generate(params) else {
            return fail(EXIT_NO_REALM, "the realm could not be generated");
        };
        let Ok(mut world) = World::new(params, &field) else {
            return fail(EXIT_NO_REALM, "the realm's roads did not fit");
        };
        let quarry = Arc::new(Quarry::new(field));
        let armed = start_quarry(&quarry, binding.set());

        let Ok(set) = Set::new() else {
            return fail(EXIT_NO_FIGURE, "the motion set could not be built");
        };
        let Ok(clips) = set.clips() else {
            return fail(EXIT_NO_FIGURE, "the motion set's clips could not be built");
        };
        let mut zone = Zone::new(TickRate::default_rate());
        let start = WorldPoint { x: 0, y: 0 };
        let (player, actor) = match spawn_player(&clips, &mut zone, start) {
            Ok(spawned) => spawned,
            Err((code, reason)) => return fail(code, reason),
        };
        let mut cast = Cast::new();
        if cast.join(player, actor, start).is_err() {
            return fail(
                EXIT_NO_FIGURE,
                "the player's figure could not join the scene",
            );
        }

        let mut session = Session {
            window,
            shell: Shell::new(),
            camera: Camera::new(start, Zoom::DEFAULT, realm_bounds(params)),
            follow: Motion::still(start),
            facing: Facing(0),
            pacer: Pacer::new(TickRate::default_rate()),
            controls: Controls::new(),
            governor: Governor::new(),
            renderer: Renderer::new(),
            cache: MaterialCache::new(
                "wintersun",
                cache_backing_bytes(),
                tairix_rt::pressure::gauge(),
                &Journal,
            ),
            scaled: None,
            times: FrameTimes::new(),
            cast,
            posed_ns: None,
            refusing: false,
        };

        let server = match open_window(&mut session.window, &desktop, binding.endpoint()) {
            Ok(server) => server,
            Err(code) => return code,
        };

        let pool = Pool::for_cpus(online_cpus());
        let deadline = core::cell::Cell::new(None);
        let pressure_moved = core::cell::Cell::new(false);
        let events = WindowEvents::new(Park {
            mailbox: EventMailbox::new(binding.endpoint(), server),
            set: binding.set(),
            quarry: Some(&quarry),
            deadline_ns: &deadline,
            pressure_moved: &pressure_moved,
        });
        let _guard = QuarryGuard(Arc::clone(&quarry));
        run_loop(
            &mut session,
            &mut world,
            &mut zone,
            player,
            &quarry,
            armed,
            &pool,
            events,
            &deadline,
            &pressure_moved,
        )
    }

    /// The player: its figure, built from the preset it walks as, and its
    /// body in `zone` at `start`, as wide as the figure it is drawn as.
    ///
    /// # Errors
    ///
    /// The exit code and reason for whichever of the two could not be made.
    fn spawn_player<'a>(
        clips: &'a Clips<'a>,
        zone: &mut Zone,
        start: WorldPoint,
    ) -> Result<(EntityId, Actor<'a>), (i32, &'static str)> {
        let identity = player_identity()
            .ok_or((EXIT_NO_FIGURE, "the player's figure could not be described"))?;
        let actor = Actor::new(&identity, clips, Facing(0))
            .map_err(|_| (EXIT_NO_FIGURE, "the player's figure could not be built"))?;
        let stats = Stats::new(40, 40, 40, 20, 20)
            .map_err(|_| (EXIT_NO_REALM, "the player's stats are out of range"))?;
        let spec = SpawnSpec::new(PLAYER_KIND, start, stats, 0, actor.footprint())
            .map_err(|_| (EXIT_NO_REALM, "the player could not be described"))?;
        let player = zone
            .spawn(spec)
            .map_err(|_| (EXIT_NO_REALM, "the player could not be spawned"))?;
        Ok((player, actor))
    }

    /// The figure the player walks as: the default preset the bundle ships,
    /// or, where that cannot be read, the reference figure the art harness
    /// measures every other against.
    fn player_identity() -> Option<Identity> {
        let path = presets::installed(presets::DEFAULT);
        match read_preset(&path) {
            Ok(identity) => Some(identity),
            Err(reason) => {
                report(&alloc::format!("{reason}; walking as the reference figure"));
                figures::identity(Species::Human).ok()
            }
        }
    }

    /// The preset record at `path`, refused unless the file is exactly one.
    fn read_preset(path: &str) -> Result<Identity, alloc::string::String> {
        let file = File::open(path.as_bytes(), OpenFlags::READ)
            .map_err(|ret| alloc::format!("cannot open {path}: {}", Errno::from_syscall(ret)))?;
        let bytes = tairix_rt::read_fd_to_end(file.fd(), RECORD_LEN)
            .map_err(|ret| alloc::format!("cannot read {path}: {}", Errno::from_syscall(ret)))?;
        Identity::decode(&bytes).map_err(|err| alloc::format!("{path} is refused: {err}"))
    }

    /// Stops the chunk worker on every way out, so it is not left mid-
    /// chunk for a window that has gone.
    ///
    /// The thread is detached rather than joined: a worker part-way
    /// through a chunk would otherwise hold the teardown for as long as
    /// that takes, and it leaves at its next turn round its loop anyway.
    struct QuarryGuard(Arc<Quarry>);

    impl Drop for QuarryGuard {
        fn drop(&mut self) {
            self.0.stop();
        }
    }

    /// How many cores the machine reported, discovered rather than
    /// assumed, so the same binary uses a Pi's four and a server's many.
    fn online_cpus() -> usize {
        tairix_procinfo::cpu_info(&tairix_procinfo::IpcTransport).map_or(1, |cpus| cpus.len())
    }

    /// Start the chunk worker, answering whether one is running.
    fn start_quarry(quarry: &Arc<Quarry>, set: u64) -> bool {
        let Some(read) = quarry.wake.read_end() else {
            report("no chunk-worker wake pipe; ground is solved on the frame loop");
            return false;
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Stream,
            u64::from(read),
            QUARRY_TOKEN,
        ) != 0
        {
            report("chunk-worker wake refused; ground is solved on the frame loop");
            return false;
        }
        let worker = Arc::clone(quarry);
        match tairix_rt::thread::Thread::spawn(move || worker.serve()) {
            Ok(_) => true,
            Err(err) => {
                report(&alloc::format!(
                    "no chunk-worker thread ({err:?}); ground is solved on the frame loop"
                ));
                false
            }
        }
    }

    /// Serve the window until the player leaves or the channel does.
    #[allow(
        clippy::too_many_arguments,
        reason = "the loop's state is deliberately owned by `main` rather than a struct \
                  that would exist only to shorten this signature"
    )]
    fn run_loop(
        session: &mut Session<'_>,
        world: &mut World,
        zone: &mut Zone,
        player: EntityId,
        quarry: &Arc<Quarry>,
        armed: bool,
        pool: &Pool,
        mut events: WindowEvents<Park<'_>>,
        deadline: &core::cell::Cell<Option<u64>>,
        pressure_moved: &core::cell::Cell<bool>,
    ) -> i32 {
        loop {
            // Written before the park reads it: a running game owes the
            // next frame, a paused one owes nothing and parks without a
            // timer at all.
            deadline.set(
                session
                    .shell
                    .running()
                    .then(|| tairix_rt::clock_get().saturating_add(FRAME_INTERVAL_NS)),
            );
            let waited = events.wait(session.window.client());
            if pressure_moved.replace(false) {
                session.cache.enforce_pressure();
            }
            while let Some(answer) = quarry.collect() {
                world.take(answer);
            }
            if let Served::Stop(code) = serve(session, waited) {
                return code;
            }
            // Everything already queued is applied before the frame is drawn
            // from the state it leaves, so a burst of input is one paint.
            loop {
                let read = events.try_wait(session.window.client());
                match serve(session, read) {
                    Served::Stop(code) => return code,
                    Served::Empty => break,
                    Served::Applied => {}
                }
            }
            let now = tairix_rt::clock_get();
            if session.shell.running() {
                if session.pacer.paused() {
                    session.pacer.resume(now);
                }
                let ticks = session.pacer.advance(now);
                simulate(zone, player, world, session, ticks);
            } else if !session.pacer.paused() {
                session.pacer.pause();
            }

            let alpha = session.pacer.alpha();
            session.camera.look_at(session.follow.at(alpha));
            if let Some(mode) = session.window.mode().copied() {
                if let Ok(view) = Viewport::new(mode.width_px, mode.height_px, RenderScale::ONE) {
                    world.ground.release_distant(session.camera.visible(&view));
                    world.request_visible(&session.camera, &view, quarry, armed);
                }
            }

            match draw(session, world, player, pool, now) {
                Ok(true) => {
                    let floored = session.governor.floored();
                    session.governor.observe(&session.times);
                    if session.governor.floored() && !floored {
                        report(
                            "frames overrun at the least detail that keeps figures readable; \
                             the frame rate is giving way",
                        );
                    }
                }
                Ok(false) => {}
                Err(_) => return fail(EXIT_CHANNEL_LOST, "present refused"),
            }
        }
    }

    /// The reference scene in the game's window, held still until the player
    /// leaves.
    fn reference_scene(mut window: AppWindow, desktop: &Desktop, endpoint: u64, set: u64) -> i32 {
        let Ok(mut world) = reference::World::generate() else {
            return fail(EXIT_NO_REALM, "the reference realm could not be generated");
        };
        let Ok(motion) = Set::new() else {
            return fail(EXIT_NO_FIGURE, "the motion set could not be built");
        };
        let Ok(clips) = motion.clips() else {
            return fail(EXIT_NO_FIGURE, "the motion set's clips could not be built");
        };
        let server = match open_window(&mut window, desktop, endpoint) {
            Ok(server) => server,
            Err(code) => return code,
        };
        let start = WorldPoint { x: 0, y: 0 };
        let mut session = Session {
            window,
            shell: Shell::new(),
            camera: Camera::new(start, Zoom::DEFAULT, realm_bounds(world.params())),
            follow: Motion::still(start),
            facing: Facing(0),
            pacer: Pacer::new(TickRate::default_rate()),
            controls: Controls::new(),
            governor: Governor::new(),
            renderer: Renderer::new(),
            cache: reference::cache(tairix_rt::pressure::gauge()),
            scaled: None,
            times: FrameTimes::new(),
            cast: Cast::new(),
            posed_ns: None,
            refusing: false,
        };
        // Never armed: nothing in the scene moves, so no frame is ever owed.
        let deadline = core::cell::Cell::new(None);
        let pressure_moved = core::cell::Cell::new(false);
        let events = WindowEvents::new(Park {
            mailbox: EventMailbox::new(endpoint, server),
            set,
            quarry: None,
            deadline_ns: &deadline,
            pressure_moved: &pressure_moved,
        });
        let pool = Pool::for_cpus(online_cpus());
        reference_loop(
            &mut session,
            &mut world,
            &clips,
            &pool,
            events,
            &pressure_moved,
        )
    }

    /// Serve the reference scene's window: the scene is drawn whenever the
    /// window owes it — first, then after its extent changed or the session
    /// gave its copy of the pixels back — and the client parks until an event
    /// changes that, never drawing for input that moves nothing.
    ///
    /// Drawn before the first park, because the session shows a window only
    /// on its first present: parked first, the client would wait for an event
    /// the session sends only to a window it has shown.
    fn reference_loop(
        session: &mut Session<'_>,
        world: &mut reference::World,
        clips: &Clips<'_>,
        pool: &Pool,
        mut events: WindowEvents<Park<'_>>,
        pressure_moved: &core::cell::Cell<bool>,
    ) -> i32 {
        let mut drawn: Option<(u32, u32)> = None;
        loop {
            if let Some(mode) = session.window.mode().copied() {
                let extent = (mode.width_px, mode.height_px);
                if drawn != Some(extent) || session.window.content_released() {
                    match draw_reference(session, world, clips, pool, &mode) {
                        Ok(()) => drawn = Some(extent),
                        // Holding a picture that is not the scene would
                        // defeat the one thing this mode is for.
                        Err(Unpresented::Refused(err)) => {
                            return fail(
                                EXIT_NO_SCENE,
                                &alloc::format!("the reference scene could not be drawn: {err}"),
                            )
                        }
                        Err(Unpresented::Lost(_)) => {
                            return fail(EXIT_CHANNEL_LOST, "present refused")
                        }
                    }
                }
            }
            let waited = events.wait(session.window.client());
            if pressure_moved.replace(false) {
                session.cache.enforce_pressure();
            }
            if let Served::Stop(code) = serve(session, waited) {
                return code;
            }
            loop {
                let read = events.try_wait(session.window.client());
                match serve(session, read) {
                    Served::Stop(code) => return code,
                    Served::Empty => break,
                    Served::Applied => {}
                }
            }
        }
    }

    /// Draw the reference scene into the whole window and present it, or
    /// present nothing where it could not be drawn exactly, a refused tile
    /// included.
    fn draw_reference(
        session: &mut Session<'_>,
        world: &mut reference::World,
        clips: &Clips<'_>,
        runner: &dyn JobRunner,
        mode: &DisplayMode,
    ) -> Result<(), Unpresented> {
        let (renderer, cache, reduced) = (
            &mut session.renderer,
            &mut session.cache,
            &mut session.scaled,
        );
        session
            .window
            .try_present(DamageRect::full(mode), |surface| {
                world.draw_window(clips, surface, reduced, renderer, cache, runner)
            })??;
        Ok(())
    }

    tairix_rt::entry!(main);
}

// Host stub. The binary is only meaningful on the bare-metal target; on
// the host an empty `main` keeps `cargo build` and `cargo test` green.
#[cfg(not(all(freestanding, feature = "run")))]
fn main() {}
