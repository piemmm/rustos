//! A frame covers every pixel exactly once, draws its figures over the lit
//! ground, and measures what it spent.

use super::*;
use tairix_wintersun_figure::actor::Actor;
use tairix_wintersun_figure::motion::Set;
use tairix_wintersun_figure::reference as figures;
use tairix_wintersun_figure::species::Species;
use tairix_wintersun_net::value::{EntityId, Facing, WorldPoint};
use tairix_wintersun_world::chunk::{Chunk, ChunkBuild};
use tairix_wintersun_world::params::{RealmParams, RealmSpec};
use tairix_wintersun_world::realm::RealmField;

use crate::camera::{realm_bounds, Zoom};

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

/// Everything a frame of the test realm is drawn from.
struct Ground {
    field: RealmField,
    held: alloc::vec::Vec<Chunk>,
    camera: Camera,
}

impl Ground {
    fn new(view: &Viewport, zoom: Zoom) -> Self {
        let field = RealmField::generate(realm()).expect("the realm generates");
        let camera = Camera::new(WorldPoint { x: 0, y: 0 }, zoom, realm_bounds(realm()));
        let held = crate::terrain::visible_chunks(camera.visible(view))
            .filter(|c| field.params().holds_chunk(c.x, c.y))
            .map(|coord| {
                ChunkBuild::new(coord)
                    .expect("a chunk fits")
                    .finish(&field)
                    .expect("a chunk generates")
            })
            .collect();
        Self {
            field,
            held,
            camera,
        }
    }

    /// Draw one frame into `target`.
    fn render(
        &self,
        target: &mut Surface,
        view: &Viewport,
        ladder: Ladder,
        cast: &Cast<'_>,
        renderer: &mut Renderer,
        clock: &dyn Clock,
    ) -> Result<FrameTimes, ClientError> {
        let borrowed: alloc::vec::Vec<&Chunk> = self.held.iter().collect();
        let chunks = ChunkWindow::new(&borrowed).expect("generated in order");
        let roads = crate::terrain::RoadDecals::from_realm(&self.field).expect("the roads fit");
        let decals = roads.decals().expect("the decals fit");
        let warp = Warp::new(self.field.params().seed());
        let fray = Fray::new(self.field.params().seed());
        PRESSURE.report(tairix_reclaim::PressureBand::Normal);
        let mut cache =
            MaterialCache::new("wintersun-frame-test", 32 * 1024 * 1024, &PRESSURE, &SINK);
        renderer.render(
            target,
            view,
            &Scene {
                camera: self.camera,
                chunks,
                decals: &decals,
                fray: &fray,
                warp: &warp,
                sun: Sun::winter(),
                sky: Sky::winter(),
                ladder,
                cast,
            },
            &mut cache,
            &tairix_parallel::SERIAL,
            clock,
        )
    }
}

fn draw(view: &Viewport, ladder: Ladder, cast: &Cast<'_>) -> (Surface, usize) {
    let ground = Ground::new(view, Zoom::FURTHEST);
    let (width, height) = view.render();
    let mut target = Surface::new(width, height).expect("a target");
    let mut renderer = Renderer::new();
    ground
        .render(&mut target, view, ladder, cast, &mut renderer, &Stopped)
        .expect("the frame draws");
    (target, renderer.figures())
}

fn view() -> Viewport {
    Viewport::new(128, 96, crate::quality::RenderScale::ONE).expect("a real window")
}

#[test]
fn every_pixel_is_written() {
    let (target, _) = draw(&view(), Ladder::FULL, &Cast::new());
    assert!(
        target.pixels().iter().all(|p| p.a == 255),
        "the frame left transparent pixels"
    );
}

#[test]
fn a_target_of_the_wrong_size_is_refused_rather_than_partly_drawn() {
    let view = view();
    let ground = Ground::new(&view, Zoom::FURTHEST);
    let (width, height) = view.render();
    let mut target = Surface::new(width - 1, height).expect("a target");
    let refused = ground.render(
        &mut target,
        &view,
        Ladder::FULL,
        &Cast::new(),
        &mut Renderer::new(),
        &Stopped,
    );
    assert_eq!(refused.err(), Some(ClientError::Viewport));
    assert!(
        target.pixels().iter().all(|p| p.a == 0),
        "a refused frame drew"
    );
}

#[test]
fn a_target_a_clip_window_cuts_is_refused_rather_than_drawn_wrongly() {
    let view = view();
    let ground = Ground::new(&view, Zoom::FURTHEST);
    let (width, height) = view.render();
    let mut target = Surface::new(width, height).expect("a target");
    let mut renderer = Renderer::new();
    for (x, y, w, h) in [(1, 0, width - 1, height), (0, 1, width, height - 1)] {
        let mut outcome = None;
        target.with_clip(x, y, w, h, |clipped| {
            outcome = Some(ground.render(
                clipped,
                &view,
                Ladder::FULL,
                &Cast::new(),
                &mut renderer,
                &Stopped,
            ));
        });
        assert_eq!(outcome.and_then(Result::err), Some(ClientError::Viewport));
    }
    assert!(
        target.pixels().iter().all(|p| p.a == 0),
        "a refused frame drew"
    );
}

#[test]
fn every_pass_with_work_is_measured_and_the_rest_report_none() {
    let view = Viewport::new(64, 48, crate::quality::RenderScale::ONE).expect("a real window");
    let ground = Ground::new(&view, Zoom::FURTHEST);
    let mut target = Surface::new(64, 48).expect("a target");
    let times = ground
        .render(
            &mut target,
            &view,
            Ladder::FULL,
            &Cast::new(),
            &mut Renderer::new(),
            &Ticking(core::cell::Cell::new(0)),
        )
        .expect("the frame draws");
    for pass in [Pass::Terrain, Pass::Light, Pass::Scenery] {
        assert!(times.spent(pass) > 0, "{pass:?} was not timed");
    }
    for pass in [Pass::Particles, Pass::Ui] {
        assert_eq!(
            times.spent(pass),
            0,
            "a pass with no work reports none rather than a guess"
        );
    }
}

#[test]
fn shedding_the_ladder_changes_the_picture_rather_than_breaking_it() {
    let (drawn, _) = draw(&view(), Ladder::FULL, &Cast::new());
    let shed_ladder = Ladder::new(Ladder::MAX_STEP);
    let shed_view = Viewport::new(128, 96, shed_ladder.render_scale()).expect("a real window");
    let (shed, _) = draw(&shed_view, shed_ladder, &Cast::new());
    assert!(
        shed.pixels().len() < drawn.pixels().len(),
        "the last rung did not shrink the target"
    );
    assert!(
        shed.pixels().iter().all(|p| p.a == 255),
        "the shed frame left holes"
    );
}

#[test]
fn figures_are_drawn_over_the_ground_where_they_stand_and_nowhere_else() {
    let view = Viewport::new(128, 96, crate::quality::RenderScale::ONE).expect("a real window");
    let camera = Ground::new(&view, Zoom::FURTHEST).camera;
    let centre = camera.centre(&view);
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let mut cast = Cast::new();
    let identity = figures::identity(Species::Human).expect("a record");
    let actor = Actor::new(&identity, &clips, Facing(0x4000)).expect("a figure");
    cast.join(EntityId(1), actor, centre).expect("it joins");

    let (bare, none) = draw(&view, Ladder::FULL, &Cast::new());
    let (peopled, one) = draw(&view, Ladder::FULL, &cast);
    assert_eq!((none, one), (0, 1));
    let (width, _) = view.render();
    let (cx, cy) = camera.screen_at(&view, centre);
    let mut changed = 0usize;
    for (index, (was, now)) in bare.pixels().iter().zip(peopled.pixels()).enumerate() {
        if was == now {
            continue;
        }
        changed += 1;
        let x = i32::try_from(index % width as usize).expect("a column");
        let y = i32::try_from(index / width as usize).expect("a row");
        assert!(
            (x - cx).abs() < 24 && (y - cy).abs() < 24,
            "a figure at ({cx},{cy}) drew at ({x},{y})"
        );
        assert_eq!(now.a, 255, "a figure left the ground translucent");
    }
    assert!(
        changed > 20,
        "the figure was barely drawn: {changed} pixels"
    );
}
