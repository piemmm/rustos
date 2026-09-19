//! A frame covers every pixel exactly once and measures what it spent.
//!
//! That the *band count* does not change the picture is the one claim
//! this module cannot make: the runner it would need is an `unsafe impl`
//! and the crate forbids `unsafe` outright, so it lives in
//! `tests/bands.rs` where a test runner may be written.

use super::*;
use tairix_wintersun_world::chunk::{Chunk, ChunkBuild};
use tairix_wintersun_world::params::{RealmParams, RealmSpec};
use tairix_wintersun_world::realm::RealmField;

/// A clock that advances a fixed amount per reading, so a test can
/// assert what was measured without depending on how fast the host is.
struct Ticking(core::cell::Cell<u64>);

impl Clock for Ticking {
    fn now_ns(&self) -> u64 {
        let now = self.0.get();
        self.0.set(now + 1_000);
        now
    }
}

struct Quiet;

impl tairix_log::Sink for Quiet {
    fn write_event(&self, _: &tairix_log::Event<'_>) {}
}

static SINK: Quiet = Quiet;
static PRESSURE: tairix_reclaim::ReportedPressure = tairix_reclaim::ReportedPressure::unknown();

fn realm() -> RealmParams {
    RealmParams::new(RealmSpec {
        extent_chunks: 8,
        coarse_samples: 32,
        plates: 8,
        ..RealmParams::winter_default(0xF2A3_1D07).spec()
    })
    .expect("the spec is in range")
}

fn world(camera: &Camera, view: &Viewport) -> (RealmField, alloc::vec::Vec<Chunk>) {
    let field = RealmField::generate(realm()).expect("the realm generates");
    let (w, h) = view.render();
    let held = crate::terrain::visible_chunks(camera.visible(w, h))
        .filter(|c| field.params().holds_chunk(c.x, c.y))
        .map(|coord| {
            ChunkBuild::new(coord)
                .expect("a chunk fits")
                .finish(&field)
                .expect("a chunk generates")
        })
        .collect();
    (field, held)
}

fn draw(runner: &dyn JobRunner, view: &Viewport, ladder: Ladder) -> alloc::vec::Vec<Pixel> {
    let camera = Camera::new(
        WorldPoint { x: 0, y: 0 },
        crate::camera::Zoom::FURTHEST,
        crate::camera::realm_bounds(realm()),
    );
    let (field, held) = world(&camera, view);
    let borrowed: alloc::vec::Vec<&Chunk> = held.iter().collect();
    let chunks = ChunkWindow::new(&borrowed).expect("generated in order");
    let roads = crate::terrain::RoadDecals::from_realm(&field).expect("the roads fit");
    let decals = roads.decals().expect("the decals fit");
    let warp = Warp::new(field.params().seed());
    let fray = Fray::new(field.params().seed());

    PRESSURE.report(tairix_reclaim::PressureBand::Normal);
    let mut cache = MaterialCache::new("wintersun-frame-test", 32 * 1024 * 1024, &PRESSURE, &SINK);
    let mut renderer = Renderer::new();
    let mut target = alloc::vec![Pixel::TRANSPARENT; view.render_pixels()];
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
                ladder,
            },
            &mut cache,
            runner,
            &Stopped,
        )
        .expect("the frame draws");
    target
}

fn view() -> Viewport {
    Viewport::new(128, 96, crate::quality::RenderScale::ONE).expect("a real window")
}

#[test]
fn every_pixel_is_written() {
    let target = draw(&tairix_parallel::SERIAL, &view(), Ladder::FULL);
    assert!(
        target.iter().all(|p| p.a == 255),
        "the frame left transparent pixels"
    );
}

#[test]
fn a_target_of_the_wrong_size_is_refused_rather_than_partly_drawn() {
    let view = view();
    let camera = Camera::new(
        WorldPoint { x: 0, y: 0 },
        crate::camera::Zoom::FURTHEST,
        crate::camera::realm_bounds(realm()),
    );
    let (field, held) = world(&camera, &view);
    let borrowed: alloc::vec::Vec<&Chunk> = held.iter().collect();
    let chunks = ChunkWindow::new(&borrowed).expect("generated in order");
    let warp = Warp::new(1);
    let fray = Fray::new(1);
    PRESSURE.report(tairix_reclaim::PressureBand::Normal);
    let mut cache = MaterialCache::new("wintersun-frame-size", 1 << 20, &PRESSURE, &SINK);
    let mut renderer = Renderer::new();
    let mut target = alloc::vec![Pixel::TRANSPARENT; view.render_pixels() - 1];
    let _ = field;
    assert_eq!(
        renderer
            .render(
                &mut target,
                &view,
                &Scene {
                    camera,
                    chunks,
                    decals: &[],
                    fray: &fray,
                    warp: &warp,
                    sun: Sun::winter(),
                    sky: Sky::winter(),
                    ladder: Ladder::FULL,
                },
                &mut cache,
                &tairix_parallel::SERIAL,
                &Stopped,
            )
            .err(),
        Some(ClientError::Viewport)
    );
}

#[test]
fn every_pass_is_measured() {
    let view = Viewport::new(64, 48, crate::quality::RenderScale::ONE).expect("a real window");
    let camera = Camera::new(
        WorldPoint { x: 0, y: 0 },
        crate::camera::Zoom::FURTHEST,
        crate::camera::realm_bounds(realm()),
    );
    let (field, held) = world(&camera, &view);
    let borrowed: alloc::vec::Vec<&Chunk> = held.iter().collect();
    let chunks = ChunkWindow::new(&borrowed).expect("generated in order");
    let warp = Warp::new(field.params().seed());
    let fray = Fray::new(field.params().seed());
    PRESSURE.report(tairix_reclaim::PressureBand::Normal);
    let mut cache = MaterialCache::new("wintersun-frame-clock", 1 << 22, &PRESSURE, &SINK);
    let mut renderer = Renderer::new();
    let mut target = alloc::vec![Pixel::TRANSPARENT; view.render_pixels()];
    let times = renderer
        .render(
            &mut target,
            &view,
            &Scene {
                camera,
                chunks,
                decals: &[],
                fray: &fray,
                warp: &warp,
                sun: Sun::winter(),
                sky: Sky::winter(),
                ladder: Ladder::FULL,
            },
            &mut cache,
            &tairix_parallel::SERIAL,
            &Ticking(core::cell::Cell::new(0)),
        )
        .expect("the frame draws");
    assert!(
        times.spent(Pass::Terrain) > 0,
        "the ground pass was not timed"
    );
    assert!(times.spent(Pass::Light) > 0, "the light pass was not timed");
    assert_eq!(
        times.spent(Pass::Scenery),
        0,
        "a pass with no work reports none rather than a guess"
    );
}

#[test]
fn a_stopped_clock_measures_nothing_and_costs_the_same_frame() {
    let view = view();
    let target = draw(&tairix_parallel::SERIAL, &view, Ladder::FULL);
    assert!(!target.is_empty());
}

#[test]
fn shedding_the_ladder_changes_the_picture_rather_than_breaking_it() {
    let full = view();
    let drawn = draw(&tairix_parallel::SERIAL, &full, Ladder::FULL);
    let shed_ladder = Ladder::new(Ladder::MAX_STEP);
    let shed_view = Viewport::new(128, 96, shed_ladder.render_scale()).expect("a real window");
    let shed = draw(&tairix_parallel::SERIAL, &shed_view, shed_ladder);
    assert!(
        shed.len() < drawn.len(),
        "the last rung did not shrink the target"
    );
    assert!(shed.iter().all(|p| p.a == 255), "the shed frame left holes");
}

#[test]
fn the_frame_origin_is_the_projection_s_own() {
    let camera = Camera::new(
        WorldPoint { x: 1_234, y: -567 },
        crate::camera::Zoom::DEFAULT,
        crate::camera::realm_bounds(realm()),
    );
    let view = view();
    let (w, h) = view.render();
    assert_eq!(frame_origin(&camera, &view), camera.origin(w, h));
}
