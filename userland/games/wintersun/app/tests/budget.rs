//! The frame budget, measured rather than asserted about.
//!
//! `plans/WINTERSUN.md` states a per-pass allocation at 1280×720 on a
//! four-core reference machine and says plainly that it "is the single
//! most likely number in this plan to be wrong". This is where it stops
//! being a guess: the passes are timed at the baseline resolution over
//! generated terrain, and the numbers are printed so a run says what the
//! renderer actually costs rather than only whether it passed.
//!
//! # What this can and cannot assert
//!
//! The host is not the reference machine and a debug build is not a
//! release one, so a tight assertion here would fail for reasons that
//! say nothing about the renderer. What is asserted is the *shape*:
//! every pass is timed, the terrain pass is the expensive one (it has
//! the largest allocation for a reason, and a frame where something else
//! dominates means the balance has moved), and neither pass exceeds a
//! ceiling generous enough that only a real regression — an order of
//! magnitude, not a percentage — reaches it.
//!
//! The budget is stated for a *four-core* machine, so the measurement is
//! taken on one thread and on four: the first says what the renderer
//! costs, the second says whether distributing the bands buys what the
//! plan assumed it would. The pool the real client uses is `lib/rt`'s
//! and is bare-metal only, so the threaded runner here is the host's own
//! — the subject is how the work divides, not which threads run it.
//!
//! Run it alone to read the numbers:
//! `cargo test -p tairix-wintersun-app --release --test budget -- --nocapture`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tairix_parallel::JobRunner;
use tairix_raster::color::Pixel;
use tairix_reclaim::{PressureBand, ReportedPressure};
use tairix_wintersun_app::budget::{FrameTimes, Pass, BASELINE_HEIGHT, BASELINE_WIDTH, FRAME_NS};
use tairix_wintersun_app::camera::{realm_bounds, Camera, Zoom};
use tairix_wintersun_app::frame::{Clock, Renderer, Scene};
use tairix_wintersun_app::light::{Sky, Sun};
use tairix_wintersun_app::quality::{Ladder, RenderScale};
use tairix_wintersun_app::terrain::{visible_chunks, RoadDecals};
use tairix_wintersun_app::view::Viewport;
use tairix_wintersun_art::cache::MaterialCache;
use tairix_wintersun_art::decal::Fray;
use tairix_wintersun_art::splat::Warp;
use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::chunk::{Chunk, ChunkBuild, ChunkWindow};
use tairix_wintersun_world::params::{RealmParams, RealmSpec};
use tairix_wintersun_world::realm::RealmField;

/// The ceiling a pass must stay under, as a multiple of its budget.
///
/// Wide on purpose: a developer's host under a debug profile is not the
/// reference machine, so anything tighter would be measuring the machine.
/// An order of magnitude is not a slow host, it is a regression.
const CEILING: u64 = 10;

/// Frames timed, so a single scheduling hiccup does not decide the
/// answer. The best is reported, because the question is what the
/// renderer costs, not what the machine was doing at the time.
const RUNS: usize = 5;

/// How many cores the budget is stated for.
const REFERENCE_CORES: usize = 4;

/// A runner that spreads jobs over `threads` host threads, claiming
/// indices from a shared cursor exactly as the real pool does.
struct Threaded(usize);

// SAFETY: every index of `0..count` is handed to `job` at most once —
// the cursor's `fetch_add` gives each index to exactly one thread — and
// `scope` joins every thread before it returns, so no invocation
// outlives the call.
unsafe impl JobRunner for Threaded {
    fn width(&self) -> usize {
        self.0
    }

    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..self.0 {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        return;
                    }
                    job(index);
                });
            }
        });
    }
}

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

/// Measure a whole frame at the baseline, best of [`RUNS`].
fn measure(runner: &dyn JobRunner) -> FrameTimes {
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
    let held: Vec<Chunk> = visible_chunks(camera.visible(w, h))
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

    PRESSURE.report(PressureBand::Normal);
    let mut cache = MaterialCache::new("wintersun-budget", 64 * 1024 * 1024, &PRESSURE, &SINK);
    let mut renderer = Renderer::new();
    let mut target = vec![Pixel::TRANSPARENT; view.render_pixels()];
    let clock = Host(Instant::now());

    let mut best = FrameTimes::new();
    for run in 0..RUNS {
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
                },
                &mut cache,
                runner,
                &clock,
            )
            .expect("the frame draws");
        if run == 0 || times.total() < best.total() {
            best = times;
        }
    }
    best
}

/// Print one measurement's per-pass costs against their budgets.
fn report(label: &str, times: &FrameTimes) {
    let micros = |ns: u64| ns / 1_000;
    println!("WinterSun frame at {BASELINE_WIDTH}x{BASELINE_HEIGHT}, {label}:");
    for pass in Pass::ALL {
        let spent = times.spent(pass);
        println!(
            "  {pass:<10?} {:>7} us  (budget {:>7} us, {:>4}%)",
            micros(spent),
            micros(pass.budget_ns()),
            spent.saturating_mul(100) / pass.budget_ns().max(1),
        );
    }
    println!(
        "  {:<10} {:>7} us  (frame {:>7} us)",
        "total",
        micros(times.total()),
        micros(FRAME_NS)
    );
}

#[test]
fn the_frame_budget_is_measured_per_pass_at_the_baseline() {
    let times = measure(&tairix_parallel::SERIAL);
    let micros = |ns: u64| ns / 1_000;
    report("one thread", &times);

    let threaded = measure(&Threaded(REFERENCE_CORES));
    report(&format!("{REFERENCE_CORES} threads"), &threaded);
    println!(
        "  speedup {}.{:02}x on {REFERENCE_CORES} threads",
        times.total() / threaded.total().max(1),
        (times.total() * 100 / threaded.total().max(1)) % 100,
    );

    // The claim the budget rests on: with the bands distributed, the
    // drawing fits inside one frame with room for the passes later items
    // add. Stated against the *whole* frame rather than each pass's own
    // allocation, because this host is not the reference machine and a
    // per-pass assertion here would be measuring the machine — while a
    // drawing pass that no longer fits a frame at all is a regression on
    // any machine.
    assert!(
        threaded.total() <= FRAME_NS,
        "the drawing passes cost {} us of a {} us frame on {REFERENCE_CORES} threads",
        micros(threaded.total()),
        micros(FRAME_NS)
    );

    assert!(
        threaded.total() < times.total(),
        "distributing the bands over {REFERENCE_CORES} threads did not make the \
         frame faster ({} us against {} us); the budget assumes it does",
        micros(threaded.total()),
        micros(times.total())
    );

    assert!(
        times.spent(Pass::Terrain) > 0 && times.spent(Pass::Light) > 0,
        "a pass that did work reported none"
    );
    for pass in [Pass::Terrain, Pass::Light] {
        let ceiling = pass.budget_ns().saturating_mul(CEILING);
        assert!(
            times.spent(pass) <= ceiling,
            "{pass:?} cost {} us against a {} us ceiling — that is a regression, \
             not a slow host",
            micros(times.spent(pass)),
            micros(ceiling)
        );
    }
    assert!(
        times.spent(Pass::Terrain) > times.spent(Pass::Light),
        "the ground is no longer the expensive pass ({} us against {} us); the \
         budget's balance has moved and the plan's table should move with it",
        micros(times.spent(Pass::Terrain)),
        micros(times.spent(Pass::Light))
    );
}
