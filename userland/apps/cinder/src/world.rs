//! The desktop as Cinder walks it: the work area, the terrain plates, and the
//! over/under/around state machine that gets him past a window.
//!
//! The route planner is what makes a companion *inhabit* the desktop rather
//! than float over it. Meeting a window, Cinder either climbs onto it, slips
//! under it, or walks around it — and the depth he is drawn at flips at the
//! moment that reads correctly, which is the apex of the leap rather than its
//! start or its finish.

use alloc::vec::Vec;

use tairix_abi::window_ipc::{LayerDepth, TerrainPlate};
use tairix_util::mathf;

use crate::gait;
use crate::project::{ground_distance, wrap_angle, Ground};

/// A rectangle of the desktop, in screen pixels.
///
/// The terrain plate, kept in the game's own terms so the model does not
/// depend on the wire type's field names.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Plate {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl Plate {
    /// The plate a wire terrain rectangle describes, or `None` when it names
    /// no area or does not fit the coordinate space.
    #[must_use]
    pub fn from_wire(plate: TerrainPlate) -> Option<Self> {
        let right = plate.x.checked_add(i32::try_from(plate.width_px).ok()?)?;
        let bottom = plate.y.checked_add(i32::try_from(plate.height_px).ok()?)?;
        (right > plate.x && bottom > plate.y).then_some(Self {
            left: plate.x,
            top: plate.y,
            right,
            bottom,
        })
    }

    /// The point on this plate nearest the ground point `at`.
    ///
    /// Both what "has he reached it?" measures against and where an approach
    /// walks to, so a crossing cannot aim at one point and be judged by
    /// another.
    #[must_use]
    pub fn nearest(&self, at: Ground) -> Ground {
        Ground::new(
            mathf::clamp(at.x, f64::from(self.left), f64::from(self.right - 1)),
            mathf::clamp(at.y, f64::from(self.top), f64::from(self.bottom - 1)),
        )
    }

    /// Whether the ground point `at` stands on this plate.
    #[must_use]
    pub fn contains(&self, at: Ground) -> bool {
        let x = at.x;
        let y = at.y;
        x >= f64::from(self.left)
            && x < f64::from(self.right)
            && y >= f64::from(self.top)
            && y < f64::from(self.bottom)
    }
}

/// The desktop Cinder is loose on.
#[derive(Default)]
pub struct World {
    area: Area,
    plates: Vec<Plate>,
}

/// The rectangle Cinder may walk in.
///
/// Empty by default: until the session says otherwise there is nowhere to
/// stand, and a creature with nowhere to stand simply does not move.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Area {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl Area {
    /// Whether the area has room to stand in.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    /// `heading` reflected off whichever edges a step to `wanted` crossed.
    ///
    /// An edge turns a creature round; it does not pin him against it. Clamping
    /// alone leaves him pressed to the boundary walking on the spot, and the
    /// destination he was walking to is outside the area, so he would never
    /// arrive and never stop. A corner reflects both edges, which is a turn
    /// back the way he came.
    #[must_use]
    pub fn deflect(&self, heading: f64, wanted: Ground) -> f64 {
        if self.is_empty() {
            return heading;
        }
        let mut turned = heading;
        if wanted.x < f64::from(self.left) || wanted.x > f64::from(self.right - 1) {
            turned = core::f64::consts::PI - turned;
        }
        if wanted.y < f64::from(self.top) || wanted.y > f64::from(self.bottom - 1) {
            turned = -turned;
        }
        wrap_angle(turned)
    }

    /// `at` pulled back inside the area.
    #[must_use]
    pub fn clamp(&self, at: Ground) -> Ground {
        if self.is_empty() {
            return at;
        }
        Ground::new(
            mathf::clamp(at.x, f64::from(self.left), f64::from(self.right - 1)),
            mathf::clamp(at.y, f64::from(self.top), f64::from(self.bottom - 1)),
        )
    }
}

impl World {
    /// The desktop with no terrain and no area yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt a new work area.
    pub fn set_area(&mut self, area: Area) {
        self.area = area;
    }

    /// The work area.
    #[must_use]
    pub const fn area(&self) -> Area {
        self.area
    }

    /// Adopt a fresh terrain answer, dropping plates that name no area.
    pub fn adopt_terrain(&mut self, plates: &[TerrainPlate]) {
        self.plates.clear();
        self.plates
            .extend(plates.iter().copied().filter_map(Plate::from_wire));
    }

    /// The terrain plates, back to front.
    #[must_use]
    pub fn plates(&self) -> &[Plate] {
        &self.plates
    }

    /// The frontmost plate `at` stands on, if any.
    ///
    /// Frontmost because the terrain arrives back-to-front and the window the
    /// user sees at a point is the last one covering it.
    #[must_use]
    pub fn plate_at(&self, at: Ground) -> Option<Plate> {
        self.plates.iter().rev().find(|p| p.contains(at)).copied()
    }

    /// The plate `steps` ahead of `at` along `heading` that is not already
    /// underfoot, if any.
    ///
    /// What "is something in my way?" means: the creature looks a stride
    /// ahead rather than at its own feet, so it decides to climb before it has
    /// walked into the edge.
    #[must_use]
    pub fn obstacle_ahead(&self, at: Ground, heading: f64, steps: f64) -> Option<Plate> {
        let (dx, dy) = crate::project::step(heading, steps);
        let ahead = Ground::new(at.x + dx, at.y + dy);
        let here = self.plate_at(at);
        let there = self.plate_at(ahead)?;
        (here != Some(there)).then_some(there)
    }
}

/// Where Cinder is in the business of getting past a window.
///
/// A closed state machine, so a creature is always exactly one of these and a
/// route can never be half-abandoned.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Route {
    /// On the desktop floor, below every window.
    Floor,
    /// Walking up to a plate he means to cross.
    Approach {
        /// The plate being approached.
        plate: Plate,
        /// How he means to cross it.
        by: Crossing,
    },
    /// Mid-leap onto a plate.
    Climb {
        /// The plate being climbed.
        plate: Plate,
        /// Seconds into the leap.
        elapsed: f64,
    },
    /// Standing on a plate.
    Perch {
        /// The plate being stood on.
        plate: Plate,
    },
    /// Mid-leap back down to the floor.
    Descend {
        /// Seconds into the leap.
        elapsed: f64,
    },
    /// Flattened, slipping under a plate.
    Burrow {
        /// The plate being passed under.
        plate: Plate,
        /// Seconds into the burrow.
        elapsed: f64,
    },
    /// Walking around a plate rather than over or under it.
    Skirt {
        /// The plate being walked around.
        plate: Plate,
        /// Which way round.
        clockwise: bool,
    },
}

/// How Cinder means to get past a plate.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Crossing {
    /// Over the top.
    Over,
    /// Underneath.
    Under,
    /// Around the side.
    Around,
}

impl Route {
    /// Which desktop stacking layer this state is drawn in.
    ///
    /// The whole point of the state machine, and the reason the depth is
    /// derived here rather than set by whoever changed the state: a climb
    /// flips to `Above` at the **apex** of the leap, so Cinder appears over
    /// the window edge as he comes down onto it rather than blinking in front
    /// of it as he takes off. A descent flips back at its own apex for the
    /// same reason.
    #[must_use]
    pub fn depth(&self) -> LayerDepth {
        match self {
            Self::Floor | Self::Approach { .. } | Self::Burrow { .. } | Self::Skirt { .. } => {
                LayerDepth::Below
            }
            Self::Perch { .. } => LayerDepth::Above,
            Self::Climb { elapsed, .. } => {
                past_apex(*elapsed, LayerDepth::Above, LayerDepth::Below)
            }
            Self::Descend { elapsed } => past_apex(*elapsed, LayerDepth::Below, LayerDepth::Above),
        }
    }

    /// Whether this state is a leap, which the pose draws with a lift.
    #[must_use]
    pub const fn is_airborne(&self) -> bool {
        matches!(self, Self::Climb { .. } | Self::Descend { .. })
    }

    /// The seconds elapsed in this state, for the states that time out.
    #[must_use]
    pub const fn elapsed(&self) -> f64 {
        match self {
            Self::Climb { elapsed, .. }
            | Self::Descend { elapsed }
            | Self::Burrow { elapsed, .. } => *elapsed,
            Self::Floor | Self::Approach { .. } | Self::Perch { .. } | Self::Skirt { .. } => 0.0,
        }
    }
}

/// `after` once a leap `elapsed` seconds in has passed its apex, else
/// `before`.
fn past_apex(elapsed: f64, after: LayerDepth, before: LayerDepth) -> LayerDepth {
    if gait::jump_progress(elapsed) >= 0.5 {
        after
    } else {
        before
    }
}

/// How the route advanced over `dt` seconds.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Advanced {
    /// The state now.
    pub route: Route,
    /// The height above the floor to draw at.
    pub lift: f64,
    /// How flattened to draw, `0.0` standing and `1.0` flat.
    pub crouch: f64,
    /// Whether the depth changed and the session must be told.
    pub depth_changed: bool,
}

/// Advance `route` by `dt` seconds for a creature at `at`.
///
/// Pure: it reads the world and answers the next state, so the whole
/// over/under/around behaviour is testable without a screen.
#[must_use]
pub fn advance(route: Route, at: Ground, world: &World, dt: f64) -> Advanced {
    let before = route.depth();
    let next = match route {
        Route::Floor => {
            // Stepping onto a plate's footprint without having chosen to
            // climb means the plate moved over him: he is underneath it, which
            // is the honest answer and needs no leap.
            Route::Floor
        }
        Route::Approach { plate, by } => {
            if reached(at, plate) {
                match by {
                    Crossing::Over => Route::Climb {
                        plate,
                        elapsed: 0.0,
                    },
                    Crossing::Under => Route::Burrow {
                        plate,
                        elapsed: 0.0,
                    },
                    Crossing::Around => Route::Skirt {
                        plate,
                        clockwise: at.x < f64::from(plate.left + plate.right) / 2.0,
                    },
                }
            } else {
                Route::Approach { plate, by }
            }
        }
        Route::Climb { plate, elapsed } => {
            let elapsed = elapsed + dt;
            if gait::jump_height(elapsed).is_none() {
                Route::Perch { plate }
            } else {
                Route::Climb { plate, elapsed }
            }
        }
        Route::Perch { plate } => {
            // Stepping off the plate's top is a descent, not a fall through
            // it: he leaves the way he came on.
            if plate.contains(at) {
                Route::Perch { plate }
            } else {
                Route::Descend { elapsed: 0.0 }
            }
        }
        Route::Descend { elapsed } => {
            let elapsed = elapsed + dt;
            if gait::jump_height(elapsed).is_none() {
                Route::Floor
            } else {
                Route::Descend { elapsed }
            }
        }
        Route::Burrow { plate, elapsed } => {
            let elapsed = elapsed + dt;
            // Out the other side once the plate is behind him.
            if !plate.contains(at) && elapsed > gait::BURROW_SECONDS {
                Route::Floor
            } else {
                Route::Burrow { plate, elapsed }
            }
        }
        Route::Skirt { plate, clockwise } => {
            if plate.contains(at) || world.plate_at(at) == Some(plate) {
                Route::Skirt { plate, clockwise }
            } else {
                Route::Floor
            }
        }
    };
    Advanced {
        route: next,
        lift: gait::jump_height(next.elapsed()).unwrap_or(0.0) * f64::from(next.is_airborne()),
        crouch: match next {
            Route::Burrow { elapsed, .. } => gait::burrow_crouch(elapsed),
            _ => 0.0,
        },
        depth_changed: next.depth() != before,
    }
}

/// Whether `at` has reached `plate`'s near edge, close enough to act.
fn reached(at: Ground, plate: Plate) -> bool {
    ground_distance(at, plate.nearest(at)) <= REACH
}

/// How close to a plate's edge counts as having reached it.
const REACH: f64 = 8.0;

/// Choose how to cross `plate`, given how bold the creature is feeling.
///
/// `boldness` is a draw in `0.0..1.0`: a plate that is tall enough to be worth
/// climbing usually gets climbed, a wide flat one gets gone under, and now and
/// then he simply walks round — which is what stops the behaviour looking
/// mechanical.
#[must_use]
pub fn choose_crossing(plate: Plate, boldness: f64) -> Crossing {
    if boldness < AROUND_SHARE {
        return Crossing::Around;
    }
    let width = f64::from(plate.right - plate.left);
    let height = f64::from(plate.bottom - plate.top);
    // A tall narrow window is a thing to climb; a wide shallow one is a thing
    // to go under.
    if height >= width * CLIMBABLE_RATIO {
        Crossing::Over
    } else {
        Crossing::Under
    }
}

/// How often a crossing is a walk around rather than over or under.
const AROUND_SHARE: f64 = 0.2;

/// How tall a plate must be relative to its width to be worth climbing.
const CLIMBABLE_RATIO: f64 = 0.55;

#[cfg(test)]
#[path = "world_tests.rs"]
mod tests;
