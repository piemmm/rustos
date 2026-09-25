//! The figures standing on the ground: who is in the scene, where each
//! stands, and the pass that draws them.
//!
//! # Placed on every core, painted on every band
//!
//! A figure's pose and placement are its own business, so every figure the
//! view can see is posed and placed at once, one to a core. Painting is the
//! frame's: each band draws every figure that reaches it, far to near,
//! confined to its own rows, so a figure straddling two bands is two halves
//! that meet exactly and no band waits on another.
//!
//! # Drawn over the light, veiled by the air
//!
//! The light buffer holds the ground's own relief shading, which is not a
//! figure's light: a figure is shaded from its own surfaces by the same sun.
//! So figures are drawn after the light composite, and each takes the mist
//! at its feet as a veil, so a figure standing in a hollow is as misted as
//! the ground around it.

use alloc::vec::Vec;

use tairix_parallel::{for_each, JobRunner};
use tairix_raster::surface::RowBand;
use tairix_wintersun_art::decal::Bounds;
use tairix_wintersun_figure::actor::{Actor, Drawn, Shade, REACH};
use tairix_wintersun_figure::paint::{self, Brush, Veil};
use tairix_wintersun_figure::rig::Placement;
use tairix_wintersun_figure::shadow::Light;
use tairix_wintersun_net::value::{EntityId, Facing, WorldPoint};
use tairix_wintersun_rules::terrain::{cell_at, Terrain, TerrainCell};
use tairix_wintersun_world::geom::{CELL_SUB_UNITS, ELEVATION_SUB_UNITS};

use crate::camera;
use crate::error::ClientError;
use crate::light::{LightBuffer, Sky};

/// One figure of the scene: whose it is, the actor playing it, and where it
/// stands.
#[derive(Debug)]
pub struct Figure<'a> {
    id: EntityId,
    actor: Actor<'a>,
    ground: WorldPoint,
}

impl<'a> Figure<'a> {
    /// Whose figure it is.
    #[must_use]
    pub const fn id(&self) -> EntityId {
        self.id
    }

    /// Where it stands.
    #[must_use]
    pub const fn ground(&self) -> WorldPoint {
        self.ground
    }

    /// The actor playing it.
    #[must_use]
    pub const fn actor(&self) -> &Actor<'a> {
        &self.actor
    }

    /// The actor playing it, to be asked for an action or a state.
    pub fn actor_mut(&mut self) -> &mut Actor<'a> {
        &mut self.actor
    }

    /// Move the figure to `to` over `nanos` of real time, facing `toward`,
    /// standing in water `submerged` world sub-units deep: the ground it
    /// covers is what paces its feet.
    ///
    /// # Errors
    ///
    /// [`ClientError::Figure`] if the actor refuses the step, which for a
    /// real time it cannot.
    pub fn step(
        &mut self,
        nanos: u64,
        to: WorldPoint,
        toward: Facing,
        submerged: i32,
    ) -> Result<(), ClientError> {
        let moved = (
            to.x.saturating_sub(self.ground.x),
            to.y.saturating_sub(self.ground.y),
        );
        self.actor
            .advance(nanos, moved, toward, submerged)
            .map_err(|_| ClientError::Figure)?;
        self.ground = to;
        Ok(())
    }

    /// Put the figure at `to` without walking it there: a spawn, a
    /// teleport, a handover — anything the display must not stride across.
    pub fn stand(&mut self, to: WorldPoint) {
        self.ground = to;
    }
}

/// World sub-units per elevation sub-unit: a height carried into the units
/// the world's positions are stated in.
const WORLD_PER_ELEVATION: i32 = {
    assert!(
        CELL_SUB_UNITS % ELEVATION_SUB_UNITS == 0,
        "a whole number of world sub-units to an elevation one"
    );
    CELL_SUB_UNITS / ELEVATION_SUB_UNITS
};

/// How deep the water a body standing at `at` is in, in world sub-units: the
/// rules' own depth at its cell, and none where the ground is not held.
#[must_use]
pub fn submerged(terrain: &impl Terrain, at: WorldPoint) -> i32 {
    terrain
        .cell(cell_at(at))
        .map_or(0, TerrainCell::depth)
        .saturating_mul(WORLD_PER_ELEVATION)
}

/// Everyone in the scene.
#[derive(Debug, Default)]
pub struct Cast<'a> {
    figures: Vec<Figure<'a>>,
}

impl<'a> Cast<'a> {
    /// Nobody yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            figures: Vec::new(),
        }
    }

    /// Bring `actor` into the scene as `id`'s figure, standing at `ground`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Figure`] for an entity already in the scene, which
    /// would be one body drawn twice, and [`ClientError::OutOfMemory`] where
    /// there is no room for another.
    pub fn join(
        &mut self,
        id: EntityId,
        actor: Actor<'a>,
        ground: WorldPoint,
    ) -> Result<(), ClientError> {
        if self.get(id).is_some() {
            return Err(ClientError::Figure);
        }
        self.figures
            .try_reserve(1)
            .map_err(|_| ClientError::OutOfMemory)?;
        self.figures.push(Figure { id, actor, ground });
        Ok(())
    }

    /// Take `id`'s figure out of the scene, answering whether it was there.
    pub fn leave(&mut self, id: EntityId) -> bool {
        let before = self.figures.len();
        self.figures.retain(|figure| figure.id != id);
        self.figures.len() != before
    }

    /// `id`'s figure.
    #[must_use]
    pub fn get(&self, id: EntityId) -> Option<&Figure<'a>> {
        self.figures.iter().find(|figure| figure.id == id)
    }

    /// `id`'s figure, to be moved or directed.
    pub fn get_mut(&mut self, id: EntityId) -> Option<&mut Figure<'a>> {
        self.figures.iter_mut().find(|figure| figure.id == id)
    }

    /// How many figures are in the scene.
    #[must_use]
    pub fn len(&self) -> usize {
        self.figures.len()
    }

    /// Whether nobody is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.figures.is_empty()
    }
}

/// How the view a frame's figures are placed into sits over the world.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Framing {
    /// The world point the render target's top-left pixel samples.
    pub origin: WorldPoint,
    /// World sub-units a render pixel.
    pub step: i32,
    /// The world extent the view covers.
    pub visible: Bounds,
    /// The sun the figures are shaded by.
    pub light: Light,
    /// How their contact shadows are drawn.
    pub shade: Shade,
}

/// One figure placed this frame, and where it falls in the paint order.
#[derive(Clone, Debug)]
struct Standing {
    placement: usize,
    drawn: Drawn,
    veil: Veil,
    depth: (i32, i32, u64),
}

/// The buffers a frame's figures are placed and painted through, held
/// across frames so a scene of figures costs no allocation once it has been
/// drawn at its size.
#[derive(Debug, Default)]
pub(crate) struct Stage {
    placements: Vec<Placement>,
    standing: Vec<Standing>,
}

/// One figure's share of the placing, handed to a core.
struct Work<'s, 'a> {
    figure: &'s Figure<'a>,
    placement: &'s mut Placement,
    drawn: Option<Drawn>,
}

impl Stage {
    /// Pose and place every figure of `cast` that can reach the view, one to
    /// a core, veil each by the mist at its feet, and order them far to near.
    ///
    /// A figure the actor cannot place is left out of the frame rather than
    /// failing it: one figure is not worth a blank window.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] where a placement buffer for another
    /// figure does not fit.
    pub(crate) fn place(
        &mut self,
        cast: &Cast<'_>,
        framing: &Framing,
        light: &LightBuffer,
        sky: Sky,
        runner: &dyn JobRunner,
    ) -> Result<(), ClientError> {
        let near = |figure: &Figure<'_>| {
            let at = figure.ground;
            let reach = i64::from(REACH);
            let inside = |value: i32, low: i32, high: i32| {
                i64::from(value) >= i64::from(low) - reach
                    && i64::from(value) <= i64::from(high) + reach
            };
            inside(at.x, framing.visible.min_x, framing.visible.max_x)
                && inside(at.y, framing.visible.min_y, framing.visible.max_y)
        };
        let wanted = cast.figures.iter().filter(|figure| near(figure)).count();
        if self.placements.len() < wanted {
            self.placements
                .try_reserve(wanted - self.placements.len())
                .map_err(|_| ClientError::OutOfMemory)?;
            self.placements.resize_with(wanted, Placement::new);
        }

        let mut work: Vec<Work<'_, '_>> = Vec::new();
        work.try_reserve(wanted)
            .map_err(|_| ClientError::OutOfMemory)?;
        let mut placements = self.placements.iter_mut();
        for figure in cast.figures.iter().filter(|figure| near(figure)) {
            let Some(placement) = placements.next() else {
                break;
            };
            work.push(Work {
                figure,
                placement,
                drawn: None,
            });
        }
        for_each(runner, &mut work, &|item: &mut Work<'_, '_>| {
            item.drawn = item
                .figure
                .actor
                .place(
                    item.figure.ground,
                    framing.origin,
                    framing.step,
                    framing.light,
                    framing.shade,
                    item.placement,
                )
                .ok();
        });

        self.standing.clear();
        self.standing
            .try_reserve(work.len())
            .map_err(|_| ClientError::OutOfMemory)?;
        for (index, item) in work.into_iter().enumerate() {
            let Some(drawn) = item.drawn else {
                continue;
            };
            let at = item.figure.ground;
            let (x, y) = camera::pixel_of(framing.origin, framing.step, at);
            self.standing.push(Standing {
                placement: index,
                drawn,
                veil: Veil {
                    tone: sky.mist,
                    amount: light.mist_at(x, y, sky),
                },
                depth: (at.y, at.x, item.figure.id.0),
            });
        }
        // Further up the screen is further from the viewer, and a tie is
        // broken the same way every frame.
        self.standing
            .sort_unstable_by_key(|standing| standing.depth);
        Ok(())
    }

    /// Paint every placed figure that reaches `band` onto it, far to near.
    pub(crate) fn paint(&self, band: &mut RowBand<'_>, brush: &mut Brush) {
        let rows = band.rows();
        let (top, bottom) = (i64::from(rows.start), i64::from(rows.end));
        for standing in &self.standing {
            let (reach_top, reach_bottom) = standing.drawn.rows;
            if i64::from(reach_bottom) <= top || i64::from(reach_top) >= bottom {
                continue;
            }
            let Some(placement) = self.placements.get(standing.placement) else {
                continue;
            };
            paint::ground(band, &standing.drawn.shadow, brush, standing.veil);
            match standing.drawn.waterline {
                Some(waterline) => {
                    let above = u32::try_from(waterline.max(0)).unwrap_or(0);
                    let mut dry = band.narrowed(rows.start..above);
                    paint::figure(&mut dry, placement, brush, standing.veil);
                }
                None => paint::figure(band, placement, brush, standing.veil),
            }
        }
    }

    /// How many figures this frame placed.
    pub(crate) fn placed(&self) -> usize {
        self.standing.len()
    }
}

#[cfg(test)]
#[path = "figures_tests.rs"]
mod tests;
