//! The reference scene: one realm, one cast and one moment, drawn the same
//! wherever it is drawn.
//!
//! Three consumers draw it, and they must agree to the bit: the
//! cross-target digest folds it at two fixed sizes, `Run --reference-scene`
//! holds it in a live window, and the host renders it at that window's size
//! to check the pixels the compositor scanned out. So everything that decides
//! a pixel is fixed here rather than taken from the machine: the realm's
//! seed, the cast and the moment it is caught at, and the material cache's
//! size. Pressure alone is the caller's, and a drawing it cost a tile is
//! refused rather than returned, so no caller can hold one as the reference.

use tairix_log::{Event, Sink};
use tairix_parallel::JobRunner;
use tairix_raster::surface::Surface;
use tairix_reclaim::{GrowthAllowance, PressureBand, PressureGauge};
use tairix_wintersun_art::cache::MaterialCache;
use tairix_wintersun_art::decal::{Bounds, Fray};
use tairix_wintersun_art::splat::Warp;
use tairix_wintersun_figure::actor::Actor;
use tairix_wintersun_figure::motion::{Clips, Kind};
use tairix_wintersun_figure::reference as figures;
use tairix_wintersun_figure::species::Species;
use tairix_wintersun_net::value::{EntityId, Facing, WorldPoint};
use tairix_wintersun_world::chunk::{ChunkBuild, ChunkWindow};
use tairix_wintersun_world::params::{RealmParams, RealmSpec};
use tairix_wintersun_world::realm::RealmField;

use crate::budget::FRAME_NS;
use crate::camera::{realm_bounds, Camera, Zoom};
use crate::error::ClientError;
use crate::figures::Cast;
use crate::frame::{Renderer, Scene, Stopped};
use crate::light::{Sky, Sun};
use crate::quality::{Ladder, RenderScale};
use crate::terrain::{self, HeldGround, RoadDecals};
use crate::view::Viewport;

/// The seed the reference realm is generated from.
pub const SEED: u64 = 0x5749_4E54_4552_4652;

/// Memory the reference scene's material cache is sized from.
///
/// Stated rather than discovered: a cache that refused a tile would draw that
/// material flat and move the picture, so its size cannot be the machine's.
/// It admits every tile a full-screen view of the scene needs several times
/// over, and is a budget rather than an allocation.
pub const CACHE_BACKING_BYTES: usize = 64 * 1024 * 1024;

/// How the reference scene is looked at: through which view, at what zoom,
/// with how much of the ladder shed.
#[derive(Copy, Clone, Debug)]
pub struct Shot<'v> {
    /// The window, and the render scale the ladder chose for it.
    pub view: &'v Viewport,
    /// How much of the world the view covers.
    pub zoom: Zoom,
    /// The detail the frame is drawn at.
    pub ladder: Ladder,
}

impl<'v> Shot<'v> {
    /// How a window holding the scene frames it: at the default zoom with
    /// nothing shed, so its pixels depend on its size alone.
    #[must_use]
    pub const fn window(view: &'v Viewport) -> Self {
        Self {
            view,
            zoom: Zoom::DEFAULT,
            ladder: Ladder::FULL,
        }
    }
}

/// The reference realm, generated once, the ground art every view of it
/// shares, and the ground the last view needed.
#[derive(Debug)]
pub struct World {
    field: RealmField,
    roads: RoadDecals,
    warp: Warp,
    fray: Fray,
    ground: HeldGround,
}

impl World {
    /// Generate the reference realm.
    ///
    /// # Errors
    ///
    /// [`ClientError::World`] if the realm does not generate, and whatever
    /// its roads refuse.
    pub fn generate() -> Result<Self, ClientError> {
        let params = params()?;
        let field = RealmField::generate(params).map_err(|_| ClientError::World)?;
        Ok(Self {
            roads: RoadDecals::from_realm(&field)?,
            warp: Warp::new(params.seed()),
            fray: Fray::new(params.seed()),
            field,
            ground: HeldGround::new(),
        })
    }

    /// The realm's parameters.
    #[must_use]
    pub fn params(&self) -> RealmParams {
        self.field.params()
    }

    /// Draw the scene as `shot` sees it into `target`, which must be the
    /// shot's render extent, with every chunk the view needs generated first
    /// so no ground is drawn as missing.
    ///
    /// # Errors
    ///
    /// [`ClientError::World`] for a chunk that does not generate,
    /// [`ClientError::Figure`] for a figure that cannot be built,
    /// [`ClientError::OutOfMemory`] where the cache refused a tile, whose
    /// material was then drawn in its flat tone, and whatever the renderer
    /// refuses. `target` holds a partial picture after any of them.
    pub fn draw(
        &mut self,
        clips: &Clips<'_>,
        shot: Shot<'_>,
        target: &mut Surface,
        renderer: &mut Renderer,
        cache: &mut MaterialCache,
        runner: &dyn JobRunner,
    ) -> Result<(), ClientError> {
        let params = self.params();
        let camera = Camera::new(WorldPoint { x: 0, y: 0 }, shot.zoom, realm_bounds(params));
        self.hold_ground(camera.visible(shot.view))?;
        let borrowed = self.ground.borrow()?;
        let chunks = ChunkWindow::new(&borrowed).map_err(|_| ClientError::World)?;
        let decals = self.roads.decals()?;
        let cast = cast(clips, camera.centre(shot.view))?;
        renderer.render(
            target,
            shot.view,
            &Scene {
                camera,
                chunks,
                decals: &decals,
                fray: &self.fray,
                warp: &self.warp,
                sun: Sun::winter(),
                sky: Sky::winter(),
                ladder: shot.ladder,
                cast: &cast,
            },
            cache,
            runner,
            &Stopped,
        )?;
        if renderer.refused_tiles() != 0 {
            return Err(ClientError::OutOfMemory);
        }
        Ok(())
    }

    /// Draw the scene into `window`, a whole window's surface, exactly as a
    /// window of that size shows it: framed by [`Shot::window`], and past
    /// the software path's cap drawn into `reduced` and resampled up
    /// ([`Viewport::draw_into`]).
    ///
    /// # Errors
    ///
    /// [`ClientError::Viewport`] for a window with no pixels, and whatever
    /// [`Self::draw`] refuses.
    pub fn draw_window(
        &mut self,
        clips: &Clips<'_>,
        window: &mut Surface,
        reduced: &mut Option<Surface>,
        renderer: &mut Renderer,
        cache: &mut MaterialCache,
        runner: &dyn JobRunner,
    ) -> Result<(), ClientError> {
        let view = Viewport::new(window.width(), window.height(), RenderScale::ONE)?;
        view.draw_into(window, reduced, |target| {
            self.draw(clips, Shot::window(&view), target, renderer, cache, runner)
        })
    }

    /// Hold every chunk `visible` needs, solving only those not already held
    /// and giving back the ones it no longer needs, so no ground is drawn as
    /// missing and none is solved twice.
    fn hold_ground(&mut self, visible: Bounds) -> Result<(), ClientError> {
        self.ground.release_distant(visible);
        for coord in terrain::visible_chunks(visible) {
            if !self.field.params().holds_chunk(coord.x, coord.y) || self.ground.holds(coord) {
                continue;
            }
            let build = ChunkBuild::new(coord).map_err(|_| ClientError::World)?;
            let chunk = build.finish(&self.field).map_err(|_| ClientError::World)?;
            self.ground.adopt(chunk)?;
        }
        Ok(())
    }
}

/// A material cache sized from [`CACHE_BACKING_BYTES`], admitting tiles as
/// `pressure` allows.
///
/// A drawing whose pixels must not depend on the machine takes it under
/// [`Unpressured`]; a live window holding the scene takes it under the
/// process's own gauge, so it gives memory back like any other cache, and a
/// drawing that cost a tile is refused ([`World::draw`]).
#[must_use]
pub fn cache(pressure: &'static (dyn PressureGauge + 'static)) -> MaterialCache {
    MaterialCache::new(
        "wintersun-reference",
        CACHE_BACKING_BYTES,
        pressure,
        &Discard,
    )
}

/// A gauge that reads normal pressure whatever the machine is under: the
/// reading a reproducible drawing is taken at.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Unpressured;

impl PressureGauge for Unpressured {
    fn sample(&self) -> PressureBand {
        PressureBand::Normal
    }

    fn growth_allowance(&self) -> GrowthAllowance {
        GrowthAllowance::unbounded(PressureBand::Normal)
    }
}

/// A sink that drops what it is given: a refused tile already fails the
/// drawing it was refused for, and a reference drawing keeps no journal.
struct Discard;

impl Sink for Discard {
    fn write_event(&self, _: &Event<'_>) {}
}

/// The realm the reference scene stands in.
///
/// Small and coarse: the scene is about the rendering, and a realm large
/// enough to be played in would spend a guest's whole budget being generated.
fn params() -> Result<RealmParams, ClientError> {
    RealmParams::new(RealmSpec {
        seed: SEED,
        // Thirty-two chunks rather than the realm's own two hundred and
        // fifty-six: large enough that the ground around the origin has the
        // slopes and the half-dozen materials a real one does — measured, not
        // assumed — and small enough that a guest solves it in a moment.
        extent_chunks: 32,
        coarse_samples: 32,
        plates: 8,
        ocean_permille: 380,
        relief_units: 1800,
        north_celsius: -22,
        south_celsius: 14,
        wind: Facing(0x0800),
    })
    .map_err(|_| ClientError::World)
}

/// One figure of the reference cast: who it is, where it starts from the
/// view's centre and how far it moves each frame, in world sub-units, which
/// way it faces, what it performs, and how deep the water it stands in is.
struct Extra {
    species: Species,
    from: (i32, i32),
    per_frame: (i32, i32),
    toward: Facing,
    performs: Option<Kind>,
    submerged: i32,
}

/// Every species, and between them every part of the figure pass: a walk
/// and a run, an upper-body action over a stride, a whole-body action in the
/// air, and a figure wading.
const EXTRAS: [Extra; 5] = [
    Extra {
        species: Species::Human,
        from: (-1400, 200),
        per_frame: (20, 0),
        toward: Facing(0),
        performs: None,
        submerged: 0,
    },
    Extra {
        species: Species::Elf,
        from: (-500, -700),
        per_frame: (0, 20),
        toward: Facing(0x4000),
        performs: Some(Kind::Cast),
        submerged: 0,
    },
    Extra {
        species: Species::Dwarf,
        from: (500, 400),
        per_frame: (0, 0),
        toward: Facing(0xC000),
        performs: None,
        submerged: 48,
    },
    Extra {
        species: Species::Beastkin,
        from: (1300, -400),
        per_frame: (-61, 0),
        toward: Facing(0x8000),
        performs: None,
        submerged: 0,
    },
    Extra {
        species: Species::Dragonkin,
        from: (100, 1000),
        per_frame: (0, 0),
        toward: Facing(0x2000),
        performs: Some(Kind::Dodge),
        submerged: 0,
    },
];

/// How many frames the cast plays before it is caught: into the middle of the
/// dodge's flight and the cast's release.
const FRAMES_PLAYED: u32 = 12;

/// How many figures the reference cast has.
pub const CAST_LEN: usize = EXTRAS.len();

/// The reference cast, standing around `centre` and played through to the
/// reference moment.
///
/// # Errors
///
/// [`ClientError::Figure`] for a figure that cannot be built or moved, and
/// [`ClientError::OutOfMemory`] where the scene has no room for it.
pub fn cast<'a>(clips: &'a Clips<'a>, centre: WorldPoint) -> Result<Cast<'a>, ClientError> {
    let mut cast = Cast::new();
    for (index, extra) in (0u64..).zip(&EXTRAS) {
        let identity = figures::identity(extra.species).map_err(|_| ClientError::Figure)?;
        let mut actor =
            Actor::new(&identity, clips, extra.toward).map_err(|_| ClientError::Figure)?;
        if let Some(kind) = extra.performs {
            actor.perform(kind).map_err(|_| ClientError::Figure)?;
        }
        let id = EntityId(index);
        let mut at = WorldPoint {
            x: centre.x + extra.from.0,
            y: centre.y + extra.from.1,
        };
        cast.join(id, actor, at)?;
        let figure = cast.get_mut(id).ok_or(ClientError::Figure)?;
        for _ in 0..FRAMES_PLAYED {
            at.x += extra.per_frame.0;
            at.y += extra.per_frame.1;
            figure.step(FRAME_NS, at, extra.toward, extra.submerged)?;
        }
    }
    Ok(cast)
}

#[cfg(test)]
#[path = "reference_tests.rs"]
mod tests;
