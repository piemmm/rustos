//! Locomotion: how speed becomes a leg cycle, a bob, a tail swing, and an ear
//! set, and how a jump arcs.
//!
//! Pure functions of speed and elapsed time, so the whole of Cinder's movement
//! is reproducible from a pose and a duration — which is what makes it
//! testable without a screen.

use tairix_util::mathf;

use crate::cinder::Pose;

/// How fast Cinder walks, in screen pixels a second, at an easy pace.
pub const WALK_SPEED: f64 = 34.0;

/// How fast Cinder runs when chasing.
pub const RUN_SPEED: f64 = 96.0;

/// How fast Cinder can turn, in radians a second.
///
/// Fast enough to follow a pointer that jumps across the screen, slow enough
/// that the turn reads as a turn rather than as a teleport.
pub const TURN_RATE: f64 = 6.0;

/// How many leg cycles a second Cinder takes at [`WALK_SPEED`].
const STRIDE_RATE_AT_WALK: f64 = 2.1;

/// How high a jump rises, in pixels.
pub const JUMP_HEIGHT: f64 = 26.0;

/// How long a jump lasts, in seconds.
pub const JUMP_SECONDS: f64 = 0.62;

/// Advance the gait phase for `speed` over `dt` seconds.
///
/// The stride rate scales with speed, so the legs never skate: a slow walk
/// takes slow steps and a run takes quick ones, from the one relationship.
#[must_use]
pub fn advance_phase(phase: f64, speed: f64, dt: f64) -> f64 {
    let rate = STRIDE_RATE_AT_WALK * (speed / WALK_SPEED);
    let mut advanced = phase + rate * core::f64::consts::TAU * dt;
    // Wrapped so the phase never grows without bound; a creature left walking
    // for a week must not lose precision in its legs. Subtraction rather than
    // a remainder, because a leg cycle only ever advances by a frame's worth.
    while advanced >= core::f64::consts::TAU {
        advanced -= core::f64::consts::TAU;
    }
    while advanced < 0.0 {
        advanced += core::f64::consts::TAU;
    }
    advanced
}

/// The height of a jump `elapsed` seconds in, or `None` once it has landed.
///
/// A parabola rather than a sine: a jump is under gravity, and the sine's
/// lazy top reads as a float.
#[must_use]
pub fn jump_height(elapsed: f64) -> Option<f64> {
    if elapsed < 0.0 || elapsed >= JUMP_SECONDS {
        return None;
    }
    let t = elapsed / JUMP_SECONDS;
    Some(4.0 * JUMP_HEIGHT * t * (1.0 - t))
}

/// How far through a jump `elapsed` seconds is, `0.0` at the leap and `1.0`
/// at the landing.
#[must_use]
pub fn jump_progress(elapsed: f64) -> f64 {
    mathf::clamp(elapsed / JUMP_SECONDS, 0.0, 1.0)
}

/// The tail's swing for a creature at `speed` whose heading is changing at
/// `turn_rate` radians a second.
///
/// The tail counterweights a turn — swinging outwards against it — and wags
/// gently at rest, so it is never simply stiff.
#[must_use]
pub fn tail_sway(speed: f64, turn_rate: f64, elapsed: f64) -> f64 {
    let counterweight = mathf::clamp(-turn_rate * TAIL_COUNTERWEIGHT, -0.9, 0.9);
    let idle_wag = mathf::sin(elapsed * TAIL_IDLE_RATE) * TAIL_IDLE_SWING;
    let moving = mathf::clamp(speed / RUN_SPEED, 0.0, 1.0);
    counterweight + idle_wag * (1.0 - moving * 0.6)
}

/// How hard the tail swings against a turn.
const TAIL_COUNTERWEIGHT: f64 = 0.22;

/// How fast the tail wags when Cinder is still, in radians a second.
const TAIL_IDLE_RATE: f64 = 1.7;

/// How far the idle wag swings.
const TAIL_IDLE_SWING: f64 = 0.16;

/// How far the ears lay back at `speed`.
///
/// Pricked at rest and swept back at a run, which is what makes a run read as
/// effort rather than as a fast walk.
#[must_use]
pub fn ear_flop(speed: f64) -> f64 {
    mathf::clamp(speed / RUN_SPEED, 0.0, 1.0) * 0.8
}

/// Apply the locomotion parameters for `speed` and `turn_rate` to `pose`,
/// advancing it by `dt` seconds of animation at `elapsed` seconds into the
/// session.
///
/// One place the four derived parameters are set together, so a caller cannot
/// advance the legs and forget the tail.
pub fn animate(pose: &mut Pose, speed: f64, turn_rate: f64, dt: f64, elapsed: f64) {
    pose.gait_phase = advance_phase(pose.gait_phase, speed, dt);
    pose.tail_sway = tail_sway(speed, turn_rate, elapsed);
    pose.ear_flop = ear_flop(speed);
}

/// How far a burrowing creature has flattened, `elapsed` seconds into the
/// burrow.
///
/// Crouching all the way down takes a moment, so slipping under a window edge
/// reads as squeezing rather than as shrinking.
#[must_use]
pub fn burrow_crouch(elapsed: f64) -> f64 {
    mathf::clamp(elapsed / BURROW_SECONDS, 0.0, 1.0)
}

/// How long the flattening takes.
pub const BURROW_SECONDS: f64 = 0.45;

#[cfg(test)]
#[path = "gait_tests.rs"]
mod tests;
