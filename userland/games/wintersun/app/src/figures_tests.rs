//! The cast is who is in the scene, and the stage draws exactly them: every
//! figure that can reach the view, far to near, cut at the waterline, and
//! the same picture however the frame is banded.

use tairix_parallel::{JobRunner, Reversed, SERIAL};
use tairix_raster::surface::Surface;
use tairix_wintersun_art::decal::Bounds;
use tairix_wintersun_figure::actor::{Actor, Shade, REACH};
use tairix_wintersun_figure::motion::{Clips, Kind, Set};
use tairix_wintersun_figure::paint::Brush;
use tairix_wintersun_figure::reference;
use tairix_wintersun_figure::species::Species;
use tairix_wintersun_net::value::{EntityId, Facing, WorldPoint};

use tairix_wintersun_rules::terrain::{SyntheticTerrain, WADE_DEPTH_SUB_UNITS};
use tairix_wintersun_world::geom::{CELL_SUB_UNITS, ELEVATION_SUB_UNITS};

use super::{submerged, Cast, Figure, Framing, Stage};
use crate::budget::FRAME_NS;
use crate::camera::{Camera, Zoom};
use crate::error::ClientError;
use crate::light::{LightBuffer, Sky, Sun};
use crate::quality::RenderScale;
use crate::view::Viewport;

/// A realm large enough that the camera is never held back by its edge.
const OPEN: Bounds = Bounds {
    min_x: -(1 << 22),
    min_y: -(1 << 22),
    max_x: 1 << 22,
    max_y: 1 << 22,
};

fn actor<'a>(clips: &'a Clips<'a>, species: Species) -> Actor<'a> {
    let identity = reference::identity(species).expect("a reference record");
    Actor::new(&identity, clips, Facing(0x4000)).expect("a figure")
}

/// A view centred on the world origin at the game's own zoom, over ground
/// the light leaves alone.
struct Shot {
    view: Viewport,
    camera: Camera,
    light: LightBuffer,
}

impl Shot {
    fn new() -> Self {
        let view = Viewport::new(160, 120, RenderScale::ONE).expect("a real window");
        let camera = Camera::new(WorldPoint { x: 0, y: 0 }, Zoom::DEFAULT, OPEN);
        let mut light = LightBuffer::new();
        light.resize(&view, 1).expect("the light fits");
        Self {
            view,
            camera,
            light,
        }
    }

    fn framing(&self) -> Framing {
        Framing {
            origin: self.camera.origin(&self.view),
            step: self.camera.step(&self.view),
            visible: self.camera.visible(&self.view),
            light: Sun::winter().light().expect("the sun"),
            shade: Shade::Soft,
        }
    }

    fn placed(&self, cast: &Cast<'_>, runner: &dyn JobRunner) -> Stage {
        let mut stage = Stage::default();
        stage
            .place(cast, &self.framing(), &self.light, Sky::winter(), runner)
            .expect("the figures place");
        stage
    }

    /// `cast` drawn onto a clear target cut into bands of `rows` rows.
    fn draw(&self, cast: &Cast<'_>, rows: u32, runner: &dyn JobRunner) -> Surface {
        let stage = self.placed(cast, runner);
        let (width, height) = self.view.render();
        let mut target = Surface::new(width, height).expect("a target");
        let mut brush = Brush::new();
        for mut band in target.row_bands_mut(0..height, rows) {
            stage.paint(&mut band, &mut brush);
        }
        target
    }

    /// The render row a world point falls in.
    fn row_of(&self, at: WorldPoint) -> i32 {
        let framing = self.framing();
        (at.y - framing.origin.y).div_euclid(framing.step)
    }
}

#[test]
fn the_cast_holds_each_entity_once() {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let mut cast = Cast::new();
    assert!(cast.is_empty());
    let here = WorldPoint { x: 40, y: -40 };
    cast.join(EntityId(7), actor(&clips, Species::Human), here)
        .expect("a first figure joins");
    assert_eq!(
        cast.join(EntityId(7), actor(&clips, Species::Elf), here),
        Err(ClientError::Figure),
        "one entity was brought in twice"
    );
    assert_eq!(cast.len(), 1);
    assert_eq!(cast.get(EntityId(7)).map(Figure::ground), Some(here));
    assert!(cast.get(EntityId(8)).is_none());
    assert!(cast.leave(EntityId(7)));
    assert!(!cast.leave(EntityId(7)), "a figure left twice");
    assert!(cast.is_empty());
}

#[test]
fn a_step_moves_the_figure_and_a_stand_does_not_walk_it() {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let mut cast = Cast::new();
    cast.join(
        EntityId(1),
        actor(&clips, Species::Human),
        WorldPoint::default(),
    )
    .expect("it joins");
    let figure = cast.get_mut(EntityId(1)).expect("it is there");
    let mut at = WorldPoint::default();
    for _ in 0..60 {
        at.x += 20;
        figure.step(FRAME_NS, at, Facing(0), 0).expect("a frame");
    }
    assert_eq!(figure.ground(), WorldPoint { x: 1200, y: 0 });
    assert_eq!(
        figure.actor().heading(),
        Facing(0),
        "it never turned to go east"
    );

    figure.stand(WorldPoint { x: -9000, y: 9000 });
    assert_eq!(figure.ground(), WorldPoint { x: -9000, y: 9000 });
    figure
        .actor_mut()
        .perform(Kind::Cast)
        .expect("an action plays");
    assert_eq!(figure.actor().performing(), (None, Some(Kind::Cast)));
}

#[test]
fn only_figures_that_can_reach_the_view_are_placed() {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let shot = Shot::new();
    let visible = shot.framing().visible;
    let mut cast = Cast::new();
    let places = [
        (WorldPoint { x: 0, y: 0 }, true),
        (
            WorldPoint {
                x: visible.max_x + REACH / 2,
                y: 0,
            },
            true,
        ),
        (
            WorldPoint {
                x: 0,
                y: visible.min_y - REACH,
            },
            true,
        ),
        (
            WorldPoint {
                x: visible.max_x + REACH + 1,
                y: 0,
            },
            false,
        ),
        (
            WorldPoint {
                x: visible.min_x - REACH - 1,
                y: visible.max_y + REACH + 1,
            },
            false,
        ),
    ];
    for (id, (at, _)) in (0u64..).zip(places) {
        cast.join(EntityId(id), actor(&clips, Species::Dwarf), at)
            .expect("it joins");
    }
    let stage = shot.placed(&cast, &SERIAL);
    let expected = places.iter().filter(|(_, reaches)| *reaches).count();
    assert_eq!(stage.placed(), expected);
}

#[test]
fn figures_are_drawn_far_to_near_and_ties_the_same_way_every_frame() {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let shot = Shot::new();
    let mut cast = Cast::new();
    for (id, y) in [(2, 100), (1, 100), (0, -100), (3, 400)] {
        cast.join(
            EntityId(id),
            actor(&clips, Species::Human),
            WorldPoint { x: 0, y },
        )
        .expect("it joins");
    }
    let order: alloc::vec::Vec<u64> = shot
        .placed(&cast, &Reversed::new(3))
        .standing
        .iter()
        .map(|standing| standing.depth.2)
        .collect();
    assert_eq!(order, alloc::vec![0, 1, 2, 3]);
}

#[test]
fn a_wading_figure_is_drawn_only_above_the_surface() {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let shot = Shot::new();
    let ground = WorldPoint { x: 0, y: 0 };
    let mut cast = Cast::new();
    cast.join(EntityId(1), actor(&clips, Species::Human), ground)
        .expect("it joins");
    // Deep enough that the shadow has drowned too, so nothing at all is
    // drawn at or below the surface.
    cast.get_mut(EntityId(1))
        .expect("it is there")
        .step(FRAME_NS, ground, Facing(0x4000), 200)
        .expect("a frame");

    let (width, height) = shot.view.render();
    let target = shot.draw(&cast, height, &SERIAL);
    let surface = shot.row_of(ground);
    let mut above = 0usize;
    for (index, pixel) in target.pixels().iter().enumerate() {
        if pixel.a == 0 {
            continue;
        }
        let row = i32::try_from(index / width as usize).expect("a row");
        assert!(
            row < surface,
            "a pixel at row {row} is under the water at {surface}"
        );
        above += 1;
    }
    assert!(above > 0, "nothing of the wading figure was drawn");
}

#[test]
fn banding_does_not_change_the_picture() {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let shot = Shot::new();
    let mut cast = Cast::new();
    for (id, species) in (0u64..).zip(Species::ALL) {
        let at = WorldPoint {
            x: i32::try_from(id).expect("a handful") * 700 - 1400,
            y: i32::try_from(id % 2).expect("a bit") * 300 - 150,
        };
        cast.join(EntityId(id), actor(&clips, species), at)
            .expect("it joins");
        let figure = cast.get_mut(EntityId(id)).expect("it is there");
        let mut walked = at;
        for _ in 0..20 {
            walked.y += 25;
            figure
                .step(FRAME_NS, walked, Facing(0x4000), 0)
                .expect("a frame");
        }
    }
    let (_, height) = shot.view.render();
    let whole = shot.draw(&cast, height, &SERIAL);
    assert!(
        whole.pixels().iter().filter(|pixel| pixel.a != 0).count() > 400,
        "the figures were barely drawn"
    );
    for rows in [1, 7, 16, 33] {
        for runner in [&SERIAL as &dyn JobRunner, &Reversed::new(4)] {
            assert!(
                shot.draw(&cast, rows, runner) == whole,
                "bands of {rows} rows drew a different picture"
            );
        }
    }
}

#[test]
fn a_body_is_submerged_by_the_rules_own_depth_in_world_units() {
    let pools = SyntheticTerrain::lattice(0, 4);
    let cell = |x: i32, y: i32| WorldPoint {
        x: x * CELL_SUB_UNITS + CELL_SUB_UNITS / 2,
        y: y * CELL_SUB_UNITS + CELL_SUB_UNITS / 2,
    };
    assert_eq!(
        submerged(&pools, cell(0, 0)),
        0,
        "dry ground drowned a figure"
    );
    let deep = SyntheticTerrain::POOL_DEPTH_SUB_UNITS * CELL_SUB_UNITS / ELEVATION_SUB_UNITS;
    assert_eq!(submerged(&pools, cell(1, 1)), deep);
    assert_eq!(
        submerged(&pools, cell(-3, -3)),
        deep,
        "a pool west of the origin"
    );
    // The deepest water the rules let a body stand in is under a cell's
    // height, so a wading figure keeps its head above water.
    const { assert!(WADE_DEPTH_SUB_UNITS * CELL_SUB_UNITS / ELEVATION_SUB_UNITS < CELL_SUB_UNITS) };

    let empty: [&tairix_wintersun_world::chunk::Chunk; 0] = [];
    let unheld = tairix_wintersun_rules::terrain::ChunkTerrain::new(&empty).expect("sorted");
    assert_eq!(
        submerged(&unheld, cell(0, 0)),
        0,
        "ground not held drowned a figure"
    );
}
