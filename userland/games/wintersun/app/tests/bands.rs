//! Cutting a frame into bands does not change it.
//!
//! The claim needs a [`JobRunner`] reporting several threads' width, and
//! `lib/parallel` owns the shared one: `Reversed` reports a width and
//! runs the pieces backwards on the calling thread. Taking it rather
//! than writing a runner here keeps this crate free of `unsafe` in every
//! target it builds, and puts the `unsafe impl` in the crate the
//! undefined-behaviour oracle interprets.
//!
//! Running the jobs on the calling thread is deliberate: the subject is
//! the *decomposition*, not the threading. A band split that drops a
//! row, overlaps two, or hands a pass the wrong first row shows up as a
//! different picture whether or not real threads ran it — and running
//! them backwards means an order dependency shows up here too. The
//! threaded claim belongs to `lib/parallel`, which owns the hand-off,
//! and to `tests/budget.rs`, which draws the baseline frame twice.

use tairix_parallel::{JobRunner, Reversed};
use tairix_raster::color::Pixel;
use tairix_reclaim::{PressureBand, ReportedPressure};
use tairix_wintersun_app::camera::{realm_bounds, Camera, Zoom};
use tairix_wintersun_app::frame::{Renderer, Scene, Stopped};
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

/// A sink this test does not read.
struct Quiet;

impl tairix_log::Sink for Quiet {
    fn write_event(&self, _: &tairix_log::Event<'_>) {}
}

static SINK: Quiet = Quiet;
static PRESSURE: ReportedPressure = ReportedPressure::unknown();

fn draw(runner: &dyn JobRunner, view: &Viewport) -> Vec<Pixel> {
    let params = RealmParams::new(RealmSpec {
        extent_chunks: 8,
        coarse_samples: 32,
        plates: 8,
        ..RealmParams::winter_default(0x8A11_0C0D).spec()
    })
    .expect("the spec is in range");
    let field = RealmField::generate(params).expect("the realm generates");
    let camera = Camera::new(
        WorldPoint { x: 0, y: 0 },
        Zoom::FURTHEST,
        realm_bounds(params),
    );
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
    let mut cache = MaterialCache::new("wintersun-bands-test", 32 * 1024 * 1024, &PRESSURE, &SINK);
    let mut renderer = Renderer::new();
    let mut target = vec![Pixel::TRANSPARENT; view.render_pixels()];
    renderer
        .render(
            &mut target,
            view,
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
            &Stopped,
        )
        .expect("the frame draws");
    target
}

#[test]
fn the_number_of_bands_does_not_change_the_picture() {
    let view = Viewport::new(160, 120, RenderScale::ONE).expect("a real window");
    let once = draw(&tairix_parallel::SERIAL, &view);
    assert!(
        once.iter().all(|p| p.a == 255),
        "the one-band frame left holes"
    );
    for width in [2usize, 3, 5, 8, 17] {
        let split = draw(&Reversed::new(width), &view);
        assert_eq!(
            split, once,
            "cutting the frame for {width} threads changed the picture"
        );
    }
}

#[test]
fn a_target_one_row_tall_still_bands() {
    let view = Viewport::new(160, 1, RenderScale::ONE).expect("a real window");
    let once = draw(&tairix_parallel::SERIAL, &view);
    let split = draw(&Reversed::new(16), &view);
    assert_eq!(split, once, "a single row cut sixteen ways changed");
    assert_eq!(once.len(), 160);
}
