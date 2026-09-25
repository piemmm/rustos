//! The frame budget, measured rather than asserted about.
//!
//! `plans/WINTERSUN.md` states a per-pass allocation at 1280×720 on a
//! four-core reference machine and says plainly that it "is the single
//! most likely number in this plan to be wrong". This is where it stops
//! being a guess: the passes are timed at the baseline resolution over
//! generated terrain with the plan's sixty-four rigged figures standing on
//! it, and the numbers are printed so a run says what the renderer actually
//! costs.
//!
//! # Why no elapsed time is asserted here
//!
//! A wall-clock threshold in a test is a claim about the machine, not
//! about the renderer. The same unchanged code passes on a fast host and
//! fails on a slower one, so the failure says only that the host was
//! slow — a flake with the machine as its seed, and one that cannot be
//! fixed by retrying. The budget is therefore *evidence*: the milestone's
//! exit criterion is read from this output and recorded in the plan, and
//! nothing here fails because a host took longer. `cargo xtask bench`
//! states the same rule for the raster and compositor families, and
//! `kernel/mem`'s ramzip tiers and `kernel/core`'s reclaim integration
//! print their costs the same way.
//!
//! # What is gated, and where
//!
//! The deterministic claims about this code live where they can be made
//! without a clock: pass attribution against a controlled clock in the
//! crate's own `frame` tests, and the band decomposition in
//! `tests/bands.rs`. That file cuts a frame into bands on a *serial*
//! runner, so it tests the decomposition and says nothing about
//! threading. This is the only place a genuinely concurrent runner draws,
//! so the claim it can make — and does, below — is that handing the bands
//! to real threads yields the identical picture. A torn hand-off or an
//! overlapping slice changes a pixel on any machine, fast or slow.
//!
//! Read the numbers with:
//! `cargo test -p tairix-wintersun-app --release --test budget -- --nocapture`

use std::hint::black_box;
use std::time::Instant;

use tairix_parallel::{JobRunner, Threaded};
use tairix_raster::color::Pixel;
use tairix_raster::surface::Surface;
use tairix_reclaim::{PressureBand, ReportedPressure};
use tairix_wintersun_app::budget::{FrameTimes, Pass, BASELINE_HEIGHT, BASELINE_WIDTH, FRAME_NS};
use tairix_wintersun_app::camera::{realm_bounds, Camera, Zoom};
use tairix_wintersun_app::figures::Cast;
use tairix_wintersun_app::frame::{Clock, Renderer, Scene};
use tairix_wintersun_app::light::{Sky, Sun};
use tairix_wintersun_app::quality::{Ladder, RenderScale};
use tairix_wintersun_app::terrain::{visible_chunks, RoadDecals};
use tairix_wintersun_app::view::Viewport;
use tairix_wintersun_art::cache::MaterialCache;
use tairix_wintersun_art::decal::{Bounds, Fray};
use tairix_wintersun_art::splat::Warp;
use tairix_wintersun_figure::actor::Actor;
use tairix_wintersun_figure::motion::{Clips, Kind, Set};
use tairix_wintersun_figure::reference;
use tairix_wintersun_figure::species::Species;
use tairix_wintersun_net::value::{EntityId, Facing, WorldPoint};
use tairix_wintersun_world::chunk::{Chunk, ChunkBuild, ChunkWindow};
use tairix_wintersun_world::params::{RealmParams, RealmSpec};
use tairix_wintersun_world::realm::RealmField;

/// Frames timed, so a single scheduling hiccup does not decide the
/// answer. The best is reported, because the question is what the
/// renderer costs, not what the machine was doing at the time.
const RUNS: usize = 5;

/// How many cores the budget is stated for, and so how many threads the
/// bands are handed to.
const REFERENCE_CORES: usize = 4;

/// How many figures carrying a full rig the budget is stated for.
const RIGS: u64 = 64;

/// What the rigs are doing, a figure each in turn: a mixture of strides,
/// both action layers and a wader, so the pass is costed at the poses the
/// game draws rather than at one.
const ACTIVITIES: [(Option<Kind>, i32, i32); 6] = [
    (None, 20, 0),
    (None, 61, 0),
    (Some(Kind::Cast), 20, 0),
    (Some(Kind::MeleeLight), 0, 0),
    (Some(Kind::Dodge), 0, 0),
    (None, 0, 60),
];

/// A clock that reads the host's monotonic time.
struct Host(Instant);

impl Clock for Host {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

struct Quiet;

impl tairix_log::Sink for Quiet {
    fn write_event(&self, _: &tairix_log::Event<'_>) {}
}

static SINK: Quiet = Quiet;
static PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// The rigs, spread evenly across `visible` on an eight-by-eight grid, and
/// how far each moves east a frame and how deep the water it stands in is.
fn rigs<'a>(clips: &'a Clips<'a>, visible: Bounds) -> (Cast<'a>, Vec<(EntityId, i32, i32)>) {
    let mut cast = Cast::new();
    let mut walks = Vec::new();
    for id in 0..RIGS {
        let index = usize::try_from(id).expect("a small id");
        let identity =
            reference::identity(Species::ALL[index % Species::ALL.len()]).expect("a record");
        let (performs, dx, depth) = ACTIVITIES[index % ACTIVITIES.len()];
        let mut actor = Actor::new(&identity, clips, Facing(0)).expect("a figure");
        if let Some(kind) = performs {
            actor.perform(kind).expect("it plays");
        }
        let (column, row) = (
            i32::try_from(id % 8).expect("a column"),
            i32::try_from(id / 8).expect("a row"),
        );
        let at = WorldPoint {
            x: visible.min_x + (visible.max_x - visible.min_x) * (2 * column + 1) / 16,
            y: visible.min_y + (visible.max_y - visible.min_y) * (2 * row + 1) / 16,
        };
        cast.join(EntityId(id), actor, at).expect("it joins");
        walks.push((EntityId(id), dx, depth));
    }
    (cast, walks)
}

/// Draw the baseline frame [`RUNS`] times, returning the cheapest run's
/// per-pass costs and the picture every run drew.
fn measure(runner: &dyn JobRunner) -> (FrameTimes, Vec<Pixel>) {
    let params = RealmParams::new(RealmSpec {
        extent_chunks: 64,
        coarse_samples: 64,
        plates: 8,
        ..RealmParams::winter_default(0x4255_4447_4554).spec()
    })
    .expect("the spec is in range");
    let field = RealmField::generate(params).expect("the realm generates");
    let camera = Camera::new(
        WorldPoint { x: 0, y: 0 },
        Zoom::DEFAULT,
        realm_bounds(params),
    );
    let view = Viewport::new(BASELINE_WIDTH, BASELINE_HEIGHT, RenderScale::ONE)
        .expect("the baseline is a real window");
    let (w, h) = view.render();
    let held: Vec<Chunk> = visible_chunks(camera.visible(&view))
        .filter(|c| params.holds_chunk(c.x, c.y))
        .map(|coord| {
            ChunkBuild::new(coord)
                .expect("a chunk fits")
                .finish(&field)
                .expect("a chunk generates")
        })
        .collect();
    let borrowed: Vec<&Chunk> = held.iter().collect();
    let chunks = ChunkWindow::new(&borrowed).expect("generated in coordinate order");
    let roads = RoadDecals::from_realm(&field).expect("the roads fit");
    let decals = roads.decals().expect("the decals fit");
    let warp = Warp::new(params.seed());
    let fray = Fray::new(params.seed());

    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let (mut cast, walks) = rigs(&clips, camera.visible(&view));

    PRESSURE.report(PressureBand::Normal);
    let mut cache = MaterialCache::new("wintersun-budget", 64 * 1024 * 1024, &PRESSURE, &SINK);
    let mut renderer = Renderer::new();
    let mut target = Surface::new(w, h).expect("the baseline frame fits");
    let clock = Host(Instant::now());

    let mut best = FrameTimes::new();
    for run in 0..RUNS {
        for &(id, dx, depth) in &walks {
            let figure = cast.get_mut(id).expect("it is there");
            let to = WorldPoint {
                x: figure.ground().x + dx,
                y: figure.ground().y,
            };
            figure
                .step(FRAME_NS, to, Facing(0), depth)
                .expect("a frame");
        }
        let times = renderer
            .render(
                &mut target,
                &view,
                &Scene {
                    camera,
                    chunks,
                    decals: &decals,
                    fray: &fray,
                    warp: &warp,
                    sun: Sun::winter(),
                    sky: Sky::winter(),
                    ladder: Ladder::FULL,
                    cast: &cast,
                },
                &mut cache,
                runner,
                &clock,
            )
            .expect("the frame draws");
        assert_eq!(
            renderer.figures() as u64,
            RIGS,
            "a rig was culled from the view it stands in"
        );
        // The frame is timed and then not read until the last run, so the
        // measurement is only honest if the optimiser cannot see that.
        black_box(&target);
        if run == 0 || times.total() < best.total() {
            best = times;
        }
    }
    (best, target.pixels().to_vec())
}

/// Print one measurement's per-pass costs against their budgets.
fn report(label: &str, times: &FrameTimes) {
    let micros = |ns: u64| ns / 1_000;
    println!("WinterSun frame at {BASELINE_WIDTH}x{BASELINE_HEIGHT}, {label}:");
    for pass in Pass::ALL {
        let spent = times.spent(pass);
        // A derived `Debug` ignores the width, so the name is rendered
        // before it is padded or the columns come out ragged.
        let name = format!("{pass:?}");
        println!(
            "  {name:<10} {:>7} us  (budget {:>7} us, {:>4}%)",
            micros(spent),
            micros(pass.budget_ns()),
            spent.saturating_mul(100) / pass.budget_ns().max(1),
        );
    }
    println!(
        "  {:<10} {:>7} us  (frame {:>7} us, {:>4}%)",
        "total",
        micros(times.total()),
        micros(FRAME_NS),
        times.total().saturating_mul(100) / FRAME_NS.max(1),
    );
}

#[test]
fn the_baseline_frame_is_measured_and_threading_does_not_change_it() {
    let (times, serial) = measure(&tairix_parallel::SERIAL);
    report("one thread", &times);

    let (threaded, concurrent) = measure(&Threaded::new(REFERENCE_CORES));
    report(&format!("{REFERENCE_CORES} threads"), &threaded);
    println!(
        "  speedup {}.{:02}x on {REFERENCE_CORES} threads (bench estimate, not a guarantee)",
        times.total() / threaded.total().max(1),
        (times.total() * 100 / threaded.total().max(1)) % 100,
    );

    // The claim that does not depend on the machine: the bands are
    // disjoint, so running them at once draws what running them in turn
    // drew. `tests/bands.rs` cuts the frame on a serial runner and so
    // cannot see a torn hand-off; this runner genuinely races.
    assert_eq!(
        serial.len(),
        concurrent.len(),
        "the two runners drew different extents"
    );
    // Two blank frames are identical, so the comparison below only means
    // something once the baseline frame is known to be painted.
    assert!(
        serial.iter().all(|p| p.a == 255),
        "the baseline frame left holes"
    );
    let differing = serial
        .iter()
        .zip(&concurrent)
        .enumerate()
        .find(|(_, (a, b))| a != b);
    assert!(
        differing.is_none(),
        "handing the bands to {REFERENCE_CORES} threads changed the picture: {differing:?}"
    );
}
