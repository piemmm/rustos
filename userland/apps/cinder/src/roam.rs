//! Cinder loose on the desktop: where he is, where he is going, and one
//! frame of getting there.
//!
//! This is the part that joins the other three together — what he wants
//! ([`crate::mind`]), what is in his way ([`crate::world`]), and how speed
//! becomes legs ([`crate::gait`]) — and it is here rather than in the `Run`
//! binary because it is behaviour, and behaviour has to be testable without a
//! screen. A frame advance that lived in the freestanding binary was reachable
//! by no host test at all, which is how a companion that walked on the spot
//! survived a green pipeline three times over.
//!
//! It holds no window, no surface, and no channel: the caller owns those and
//! draws whatever this answers.

use tairix_rng::RandU64;
use tairix_util::mathf;

use crate::gait;
use crate::mind::{Intent, Mind};
use crate::project::{ground_distance, heading_towards, step, turn_towards, Ground};
use crate::world::{self, Advanced, Crossing, Route, World};

/// How far ahead he looks for something in his way, in ground pixels.
pub const LOOK_AHEAD: f64 = 30.0;

/// How near a destination counts as arrived, in ground pixels.
pub const ARRIVED: f64 = 12.0;

/// Cinder's whereabouts out on the desktop.
pub struct Roam {
    at: Ground,
    home: Ground,
    heading: f64,
    route: Route,
    target: Option<Ground>,
    pointer: Option<Ground>,
}

/// What one frame of roaming produced.
///
/// The caller needs all four to draw him, and they are answered together so a
/// frame cannot be posed from a stale part of the last one.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Stepped {
    /// What he decided to do.
    pub intent: Intent,
    /// How far he actually travelled, in ground pixels a second.
    ///
    /// The ground he *covered*, never the pace he intended. It is what drives
    /// the legs, so a companion held at an edge, blocked by a window, standing
    /// at his destination, or airborne mid-leap stops stepping rather than
    /// treading air — and any future way of being stopped is covered by the
    /// same one rule.
    pub travelled: f64,
    /// How fast he turned, in radians a second.
    pub turn_rate: f64,
    /// The route's state, height, crouch, and whether its depth changed.
    pub advanced: Advanced,
}

impl Roam {
    /// Put Cinder on the desktop at `at`, which becomes the home he walks
    /// back to.
    ///
    /// The pen's own screen position is deliberately not knowable — an
    /// application is never told where its window sits — so the place he came
    /// out is the only home the desktop actually gave him.
    #[must_use]
    pub const fn new(at: Ground) -> Self {
        Self {
            at,
            home: at,
            heading: 0.0,
            route: Route::Floor,
            target: None,
            pointer: None,
        }
    }

    /// Where he is standing.
    #[must_use]
    pub const fn at(&self) -> Ground {
        self.at
    }

    /// The spot he was let out at.
    #[must_use]
    pub const fn home(&self) -> Ground {
        self.home
    }

    /// Which way he faces, in radians.
    #[must_use]
    pub const fn heading(&self) -> f64 {
        self.heading
    }

    /// How he is crossing the desktop.
    #[must_use]
    pub const fn route(&self) -> Route {
        self.route
    }

    /// Where he is walking to, if anywhere.
    #[must_use]
    pub const fn target(&self) -> Option<Ground> {
        self.target
    }

    /// Note where the pointer is, or that there is no longer a sample.
    pub fn see_pointer(&mut self, at: Option<Ground>) {
        self.pointer = at;
    }

    /// Advance him by `dt` seconds.
    ///
    /// A non-positive or non-finite step moves nothing: the rates answered
    /// below are per-second and divide by it, so a zero would put a NaN
    /// through the gait and every part position with it — and a NaN screen
    /// coordinate converts to zero, which draws the creature in the corner of
    /// his own surface rather than failing.
    pub fn advance<R: RandU64>(&mut self, mind: &mut Mind<R>, world: &World, dt: f64) -> Stepped {
        if dt <= 0.0 || !dt.is_finite() {
            let advanced = world::advance(self.route, self.at, world, 0.0);
            self.route = advanced.route;
            return Stepped {
                intent: mind.intent(),
                travelled: 0.0,
                turn_rate: 0.0,
                advanced,
            };
        }
        let blocked = world
            .obstacle_ahead(self.at, self.heading, LOOK_AHEAD)
            .is_some();
        let pointer = self.pointer.map(|to| ground_distance(self.at, to));
        let intent = mind.tick(dt, pointer, blocked);

        self.retire_reached_target();
        let target = self.resolve_target(intent, mind, world);
        let turn_rate = self.turn_towards_target(target, dt);
        let stood_at = self.at;
        self.walk(target, intent, world, dt);
        self.begin_crossing(intent, world);

        let advanced = world::advance(self.route, self.at, world, dt);
        self.route = advanced.route;
        Stepped {
            intent,
            travelled: ground_distance(stood_at, self.at) / dt,
            turn_rate,
            advanced,
        }
    }

    /// Drop a destination he is already standing at.
    ///
    /// Retired *before* the next is resolved, so a wander draws its following
    /// leg and a walk home simply stops. Leaving it set is what had a
    /// travelling intent stepping on the spot at the place it had reached.
    fn retire_reached_target(&mut self) {
        if self
            .target
            .is_some_and(|to| ground_distance(self.at, to) < ARRIVED)
        {
            self.target = None;
        }
    }

    /// Where he is trying to get to this frame.
    ///
    /// Every journey but a chase is routed through the one target slot, so
    /// arriving retires it by the rule above rather than each intent needing
    /// its own stopping condition.
    fn resolve_target<R: RandU64>(
        &mut self,
        intent: Intent,
        mind: &mut Mind<R>,
        world: &World,
    ) -> Option<Ground> {
        match intent {
            // A chase follows a pointer that moves, so it reads the live
            // sample rather than a stored destination.
            Intent::Chase | Intent::Pounce => self
                .pointer
                .filter(|to| ground_distance(self.at, *to) >= ARRIVED),
            Intent::ComeHome => {
                if self.target.is_none() && ground_distance(self.at, self.home) >= ARRIVED {
                    self.target = Some(self.home);
                }
                self.target
            }
            Intent::Wander => {
                if self.target.is_none() {
                    let heading = mind.draw_heading();
                    let distance = mind.draw_wander_distance();
                    let (dx, dy) = step(heading, distance);
                    self.target = Some(
                        world
                            .area()
                            .clamp(Ground::new(self.at.x + dx, self.at.y + dy)),
                    );
                }
                self.target
            }
            Intent::Sit | Intent::Groom | Intent::Nap => {
                self.target = None;
                None
            }
            Intent::Climb | Intent::Burrow => self.target,
        }
    }

    /// Turn towards `target`, answering the turn rate in radians a second.
    fn turn_towards_target(&mut self, target: Option<Ground>, dt: f64) -> f64 {
        let Some(target) = target else {
            return 0.0;
        };
        let wanted = heading_towards(self.at, target);
        let delta = turn_towards(self.heading, wanted);
        let turned = mathf::clamp(delta, -gait::TURN_RATE * dt, gait::TURN_RATE * dt);
        self.heading += turned;
        turned / dt
    }

    /// Walk towards `target`, if there is one and he is on the floor.
    ///
    /// A pace is only a pace *towards somewhere*: with nothing to walk to he
    /// stands, which is what stops him drifting along whatever heading he last
    /// held.
    fn walk(&mut self, target: Option<Ground>, intent: Intent, world: &World, dt: f64) {
        let pace = target.map_or(0.0, |_| intent.pace() * gait::RUN_SPEED);
        if pace <= 0.0 || self.route.is_airborne() {
            return;
        }
        let (dx, dy) = step(self.heading, pace * dt);
        let wanted = Ground::new(self.at.x + dx, self.at.y + dy);
        let area = world.area();
        self.at = area.clamp(wanted);
        if self.at != wanted {
            // He met an edge. Turning him round is the whole of it: the
            // destination he was walking to lies outside the area, so pinning
            // him against the boundary left him pressed to it for good.
            self.heading = area.deflect(self.heading, wanted);
            self.target = None;
        }
    }

    /// Begin a crossing when the mind asked for one and he is on the floor.
    fn begin_crossing(&mut self, intent: Intent, world: &World) {
        if !matches!(intent, Intent::Climb | Intent::Burrow) || self.route != Route::Floor {
            return;
        }
        let Some(plate) = world.obstacle_ahead(self.at, self.heading, LOOK_AHEAD) else {
            return;
        };
        let by = if intent == Intent::Climb {
            Crossing::Over
        } else {
            Crossing::Under
        };
        // Walk to the plate itself, and to the very point the reach test
        // measures against, so an approach cannot inherit a stale destination
        // and stall short of the window it is crossing.
        self.target = Some(plate.nearest(self.at));
        self.route = Route::Approach { plate, by };
    }
}

#[cfg(test)]
#[path = "roam_tests.rs"]
mod tests;
