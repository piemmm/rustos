//! Locomotion: standing, walking and running.
//!
//! The clips the gait paces. Their leg curves are each clip's stated foot
//! path put through the planting layer's own two-bone solve, and the tests
//! beside the motion set solve every key again from its path.

use super::{opposite, rooted, Keyed};
use crate::clip::{Event, Key};
use crate::pose::Param;
use crate::socket::Side;

/// Each foot meeting the ground, at the head of its own stance.
pub(super) const FOOTSTEPS: [Event; 2] = [
    Event::new("footstep_left", 0.0),
    Event::new("footstep_right", 0.5),
];

/// How long one idle cycle lasts, in seconds.
pub(super) const IDLE_SECONDS: f64 = 4.0;

/// How long one walk cycle — two steps — lasts, in seconds.
pub(super) const WALK_SECONDS: f64 = 1.1;

/// How long one run cycle lasts, in seconds.
pub(super) const RUN_SECONDS: f64 = 0.62;

/// How far in front of the hip the walking foot strikes, in figure-local
/// units.
pub(super) const WALK_HALF_STEP: f64 = 14.0;

/// What fraction of the walk cycle each foot is on the ground for.
///
/// Exactly half, so the figure is always on one foot and never on none: a
/// walk is the gait with no flight phase.
pub(super) const WALK_STANCE: f64 = 0.5;

/// How far in front of the hip the running foot strikes.
pub(super) const RUN_HALF_STEP: f64 = 18.0;

/// What fraction of the run cycle each foot is on the ground for.
///
/// Under a half, so there is a moment with neither foot down — which is
/// what makes it a run rather than a fast walk.
pub(super) const RUN_STANCE: f64 = 0.375;

/// How deep each path stands into its own legs as a foot strikes, in
/// figure-local units.
///
/// The idle and the walk hold it throughout, and the run sinks further from
/// it into each stance: the root sinks by exactly this so the planted foot
/// reaches the floor. `quality::grounding` measures the two against each
/// other rather than either being trusted.
pub(super) const IDLE_CROUCH: f64 = 1.2;
pub(super) const WALK_CROUCH: f64 = 3.5;
pub(super) const RUN_CROUCH: f64 = 6.0;

/// How far the running body rises between toe-off and mid-flight.
///
/// A run has a moment with neither foot down, and where the body is then is
/// not in its articulation — both legs tucked reads identically to a deep
/// crouch — so the rise is stated here: a fortieth of the figure's height,
/// two or three pixels of lift at the largest size the desktop draws one.
/// Enough to read as a bound rather than a glide, and far inside the stride
/// the feet are pacing.
pub(super) const RUN_FLIGHT_RISE: f64 = 2.5;

/// How far the running body sinks from a strike to midstance, rising again
/// to toe-off: the stance leg folding into the landing and straightening out
/// of it into the flight.
///
/// As deep as the flight rises, so the body's bob is centred on the height it
/// lands at and spans a twentieth of the figure, five pixels at the largest
/// size. Its ends are flat rather than at the flight's own slope: that join
/// would have the leg shortening at the landing's full speed as it strikes,
/// which keys a thirty-second of a cycle apart cannot follow without the
/// planted foot sinking past the grounding bound.
pub(super) const RUN_STANCE_DIP: f64 = RUN_FLIGHT_RISE;

/// The run's root height `t` of the way through a flight window, `t` running
/// from `-1` at toe-off through `0` at mid-flight to `1` at the next strike.
///
/// A parabola, which is the arc a body with nothing holding it up follows;
/// it meets the stance height at both ends, so the height never steps where
/// a foot takes over.
const fn flight(t: f64) -> f64 {
    rooted(-RUN_CROUCH + RUN_FLIGHT_RISE * (1.0 - t * t))
}

/// The run's root height `u` of the way through a stance, strike to toe-off.
const fn stance(u: f64) -> f64 {
    rooted(-RUN_CROUCH - RUN_STANCE_DIP * swell(u))
}

/// A swell across `0..=1`: none at either end, where it is flat, and all of
/// it at the middle.
///
/// A polynomial, so a height keyed from it is exact at compile time.
const fn swell(u: f64) -> f64 {
    let arch = 4.0 * u * (1.0 - u);
    arch * arch
}

/// The run's root height at `phase`: each half of the cycle is one step, a
/// stance and then a flight.
const fn run_root(phase: f64) -> f64 {
    let step = if phase < 0.5 { phase } else { phase - 0.5 };
    if step < RUN_STANCE {
        stance(step / RUN_STANCE)
    } else {
        flight(2.0 * (step - RUN_STANCE) / (0.5 - RUN_STANCE) - 1.0)
    }
}

/// [`run_root`] keyed at the phases `legs` are keyed at.
const fn keyed_like<const N: usize>(legs: &[Key; N]) -> [Key; N] {
    let mut keys = *legs;
    let mut index = 0;
    while index < N {
        keys[index] = Key::new(legs[index].phase, run_root(legs[index].phase));
        index += 1;
    }
    keys
}

/// The idle and walk hold one height throughout: both keep a foot down for
/// every phase of the cycle, so the body never leaves it.
pub(super) const IDLE_LIFT: [Key; 2] = [
    Key::new(0.0, rooted(-IDLE_CROUCH)),
    Key::new(1.0, rooted(-IDLE_CROUCH)),
];

pub(super) const WALK_LIFT: [Key; 2] = [
    Key::new(0.0, rooted(-WALK_CROUCH)),
    Key::new(1.0, rooted(-WALK_CROUCH)),
];

/// The run's height, keyed where its legs are.
pub(super) const RUN_LIFT: [Key; RUN_HIP_LEFT.len()] = keyed_like(&RUN_HIP_LEFT);

/// The authored leg cycles, left side, solved from each clip's foot path —
/// stated whole, and every key solved again from it, by this module's tests.
pub(super) const WALK_HIP_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.276_264),
    Key::new(0.031_250, 0.279_460),
    Key::new(0.062_500, 0.277_239),
    Key::new(0.093_750, 0.271_023),
    Key::new(0.125_000, 0.261_568),
    Key::new(0.156_250, 0.249_338),
    Key::new(0.187_500, 0.234_634),
    Key::new(0.218_750, 0.217_667),
    Key::new(0.250_000, 0.198_579),
    Key::new(0.281_250, 0.177_459),
    Key::new(0.312_500, 0.154_348),
    Key::new(0.343_750, 0.129_229),
    Key::new(0.375_000, 0.102_017),
    Key::new(0.406_250, 0.072_522),
    Key::new(0.437_500, 0.040_390),
    Key::new(0.468_750, 0.004_959),
    Key::new(0.500_000, -0.117_023),
    Key::new(0.531_250, -0.175_336),
    Key::new(0.562_500, -0.080_078),
    Key::new(0.593_750, 0.029_406),
    Key::new(0.625_000, 0.093_555),
    Key::new(0.656_250, 0.161_984),
    Key::new(0.687_500, 0.230_536),
    Key::new(0.718_750, 0.294_827),
    Key::new(0.750_000, 0.349_500),
    Key::new(0.781_250, 0.389_000),
    Key::new(0.812_500, 0.409_319),
    Key::new(0.843_750, 0.409_312),
    Key::new(0.875_000, 0.390_736),
    Key::new(0.906_250, 0.357_722),
    Key::new(0.937_500, 0.317_416),
    Key::new(0.968_750, 0.283_854),
    Key::new(1.000_000, 0.276_264),
];

pub(super) const WALK_KNEE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.196_699),
    Key::new(0.031_250, 0.231_949),
    Key::new(0.062_500, 0.258_996),
    Key::new(0.093_750, 0.280_093),
    Key::new(0.125_000, 0.296_401),
    Key::new(0.156_250, 0.308_589),
    Key::new(0.187_500, 0.317_060),
    Key::new(0.218_750, 0.322_057),
    Key::new(0.250_000, 0.323_709),
    Key::new(0.281_250, 0.322_057),
    Key::new(0.312_500, 0.317_060),
    Key::new(0.343_750, 0.308_589),
    Key::new(0.375_000, 0.296_401),
    Key::new(0.406_250, 0.280_093),
    Key::new(0.437_500, 0.258_996),
    Key::new(0.468_750, 0.231_949),
    Key::new(0.500_000, 0.196_699),
    Key::new(0.531_250, 0.188_628),
    Key::new(0.562_500, 0.239_258),
    Key::new(0.593_750, 0.315_553),
    Key::new(0.625_000, 0.394_493),
    Key::new(0.656_250, 0.465_013),
    Key::new(0.687_500, 0.520_434),
    Key::new(0.718_750, 0.555_895),
    Key::new(0.750_000, 0.568_113),
    Key::new(0.781_250, 0.555_895),
    Key::new(0.812_500, 0.520_434),
    Key::new(0.843_750, 0.465_013),
    Key::new(0.875_000, 0.394_493),
    Key::new(0.906_250, 0.315_553),
    Key::new(0.937_500, 0.239_258),
    Key::new(0.968_750, 0.188_628),
    Key::new(1.000_000, 0.196_699),
];

pub(super) const WALK_ANKLE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.080_448),
    Key::new(0.031_250, 0.002_241),
    Key::new(0.062_500, -0.067_113),
    Key::new(0.093_750, -0.130_178),
    Key::new(0.125_000, -0.188_226),
    Key::new(0.156_250, -0.241_938),
    Key::new(0.187_500, -0.291_676),
    Key::new(0.218_750, -0.337_603),
    Key::new(0.250_000, -0.379_744),
    Key::new(0.281_250, -0.418_019),
    Key::new(0.312_500, -0.452_250),
    Key::new(0.343_750, -0.482_156),
    Key::new(0.375_000, -0.507_329),
    Key::new(0.406_250, -0.527_179),
    Key::new(0.437_500, -0.540_810),
    Key::new(0.468_750, -0.546_760),
    Key::new(0.500_000, -0.542_292),
    Key::new(0.531_250, -0.557_909),
    Key::new(0.562_500, -0.622_267),
    Key::new(0.593_750, -0.698_515),
    Key::new(0.625_000, -0.759_675),
    Key::new(0.656_250, -0.792_064),
    Key::new(0.687_500, -0.787_969),
    Key::new(0.718_750, -0.744_492),
    Key::new(0.750_000, -0.664_471),
    Key::new(0.781_250, -0.556_147),
    Key::new(0.812_500, -0.430_404),
    Key::new(0.843_750, -0.297_407),
    Key::new(0.875_000, -0.165_313),
    Key::new(0.906_250, -0.041_883),
    Key::new(0.937_500, 0.060_611),
    Key::new(0.968_750, 0.115_001),
    Key::new(1.000_000, 0.080_448),
];

pub(super) const RUN_HIP_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.364_662),
    Key::new(0.031_250, 0.376_626),
    Key::new(0.062_500, 0.383_561),
    Key::new(0.093_750, 0.381_932),
    Key::new(0.125_000, 0.369_915),
    Key::new(0.156_250, 0.346_831),
    Key::new(0.187_500, 0.312_966),
    Key::new(0.218_750, 0.269_341),
    Key::new(0.250_000, 0.217_360),
    Key::new(0.281_250, 0.158_422),
    Key::new(0.312_500, 0.093_668),
    Key::new(0.343_750, 0.024_052),
    Key::new(0.375_000, -0.163_425),
    Key::new(0.406_250, -0.340_846),
    Key::new(0.437_500, -0.354_357),
    Key::new(0.468_750, -0.236_043),
    Key::new(0.500_000, -0.048_890),
    Key::new(0.531_250, 0.052_488),
    Key::new(0.562_500, 0.127_020),
    Key::new(0.593_750, 0.207_321),
    Key::new(0.625_000, 0.291_509),
    Key::new(0.656_250, 0.375_545),
    Key::new(0.687_500, 0.452_226),
    Key::new(0.718_750, 0.512_642),
    Key::new(0.750_000, 0.549_942),
    Key::new(0.781_250, 0.562_059),
    Key::new(0.812_500, 0.551_197),
    Key::new(0.843_750, 0.521_753),
    Key::new(0.875_000, 0.478_971),
    Key::new(0.906_250, 0.429_349),
    Key::new(0.937_500, 0.383_758),
    Key::new(0.968_750, 0.360_187),
    Key::new(1.000_000, 0.364_662),
];

pub(super) const RUN_KNEE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.257_372),
    Key::new(0.031_250, 0.326_571),
    Key::new(0.062_500, 0.388_763),
    Key::new(0.093_750, 0.439_956),
    Key::new(0.125_000, 0.477_943),
    Key::new(0.156_250, 0.501_306),
    Key::new(0.187_500, 0.509_191),
    Key::new(0.218_750, 0.501_306),
    Key::new(0.250_000, 0.477_943),
    Key::new(0.281_250, 0.439_956),
    Key::new(0.312_500, 0.388_763),
    Key::new(0.343_750, 0.326_571),
    Key::new(0.375_000, 0.257_372),
    Key::new(0.406_250, 0.210_371),
    Key::new(0.437_500, 0.226_273),
    Key::new(0.468_750, 0.292_293),
    Key::new(0.500_000, 0.378_270),
    Key::new(0.531_250, 0.467_397),
    Key::new(0.562_500, 0.551_374),
    Key::new(0.593_750, 0.624_649),
    Key::new(0.625_000, 0.682_259),
    Key::new(0.656_250, 0.719_426),
    Key::new(0.687_500, 0.732_320),
    Key::new(0.718_750, 0.719_426),
    Key::new(0.750_000, 0.682_259),
    Key::new(0.781_250, 0.624_649),
    Key::new(0.812_500, 0.551_374),
    Key::new(0.843_750, 0.467_397),
    Key::new(0.875_000, 0.378_270),
    Key::new(0.906_250, 0.292_293),
    Key::new(0.937_500, 0.226_273),
    Key::new(0.968_750, 0.210_371),
    Key::new(1.000_000, 0.257_372),
];

pub(super) const RUN_ANKLE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.079_737),
    Key::new(0.031_250, -0.021_799),
    Key::new(0.062_500, -0.118_505),
    Key::new(0.093_750, -0.208_592),
    Key::new(0.125_000, -0.290_882),
    Key::new(0.156_250, -0.363_909),
    Key::new(0.187_500, -0.425_805),
    Key::new(0.218_750, -0.474_610),
    Key::new(0.250_000, -0.508_817),
    Key::new(0.281_250, -0.527_893),
    Key::new(0.312_500, -0.532_639),
    Key::new(0.343_750, -0.525_477),
    Key::new(0.375_000, -0.511_248),
    Key::new(0.406_250, -0.506_713),
    Key::new(0.437_500, -0.539_764),
    Key::new(0.468_750, -0.602_235),
    Key::new(0.500_000, -0.669_415),
    Key::new(0.531_250, -0.726_270),
    Key::new(0.562_500, -0.763_756),
    Key::new(0.593_750, -0.774_654),
    Key::new(0.625_000, -0.753_146),
    Key::new(0.656_250, -0.696_809),
    Key::new(0.687_500, -0.609_369),
    Key::new(0.718_750, -0.500_957),
    Key::new(0.750_000, -0.383_956),
    Key::new(0.781_250, -0.267_885),
    Key::new(0.812_500, -0.157_789),
    Key::new(0.843_750, -0.055_891),
    Key::new(0.875_000, 0.035_782),
    Key::new(0.906_250, 0.112_282),
    Key::new(0.937_500, 0.160_329),
    Key::new(0.968_750, 0.153_917),
    Key::new(1.000_000, 0.079_737),
];

const WALK_ARM_LEFT: [Key; 17] = [
    Key::new(0.000_000, -0.220_000),
    Key::new(0.062_500, -0.203_253),
    Key::new(0.125_000, -0.155_563),
    Key::new(0.187_500, -0.084_190),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.084_190),
    Key::new(0.375_000, 0.155_563),
    Key::new(0.437_500, 0.203_253),
    Key::new(0.500_000, 0.220_000),
    Key::new(0.562_500, 0.203_253),
    Key::new(0.625_000, 0.155_563),
    Key::new(0.687_500, 0.084_190),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.084_190),
    Key::new(0.875_000, -0.155_563),
    Key::new(0.937_500, -0.203_253),
    Key::new(1.000_000, -0.220_000),
];
const WALK_ELBOW_LEFT: [Key; 17] = [
    Key::new(0.000_000, 0.120_000),
    Key::new(0.062_500, 0.123_045),
    Key::new(0.125_000, 0.131_716),
    Key::new(0.187_500, 0.144_693),
    Key::new(0.250_000, 0.160_000),
    Key::new(0.312_500, 0.175_307),
    Key::new(0.375_000, 0.188_284),
    Key::new(0.437_500, 0.196_955),
    Key::new(0.500_000, 0.200_000),
    Key::new(0.562_500, 0.196_955),
    Key::new(0.625_000, 0.188_284),
    Key::new(0.687_500, 0.175_307),
    Key::new(0.750_000, 0.160_000),
    Key::new(0.812_500, 0.144_693),
    Key::new(0.875_000, 0.131_716),
    Key::new(0.937_500, 0.123_045),
    Key::new(1.000_000, 0.120_000),
];
const WALK_TWIST: [Key; 17] = [
    Key::new(0.000_000, -0.100_000),
    Key::new(0.062_500, -0.092_388),
    Key::new(0.125_000, -0.070_711),
    Key::new(0.187_500, -0.038_268),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.038_268),
    Key::new(0.375_000, 0.070_711),
    Key::new(0.437_500, 0.092_388),
    Key::new(0.500_000, 0.100_000),
    Key::new(0.562_500, 0.092_388),
    Key::new(0.625_000, 0.070_711),
    Key::new(0.687_500, 0.038_268),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.038_268),
    Key::new(0.875_000, -0.070_711),
    Key::new(0.937_500, -0.092_388),
    Key::new(1.000_000, -0.100_000),
];
const RUN_ARM_LEFT: [Key; 17] = [
    Key::new(0.000_000, -0.280_000),
    Key::new(0.062_500, -0.258_686),
    Key::new(0.125_000, -0.197_990),
    Key::new(0.187_500, -0.107_151),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.107_151),
    Key::new(0.375_000, 0.197_990),
    Key::new(0.437_500, 0.258_686),
    Key::new(0.500_000, 0.280_000),
    Key::new(0.562_500, 0.258_686),
    Key::new(0.625_000, 0.197_990),
    Key::new(0.687_500, 0.107_151),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.107_151),
    Key::new(0.875_000, -0.197_990),
    Key::new(0.937_500, -0.258_686),
    Key::new(1.000_000, -0.280_000),
];
const RUN_ELBOW_LEFT: [Key; 17] = [
    Key::new(0.000_000, 0.450_000),
    Key::new(0.062_500, 0.453_806),
    Key::new(0.125_000, 0.464_645),
    Key::new(0.187_500, 0.480_866),
    Key::new(0.250_000, 0.500_000),
    Key::new(0.312_500, 0.519_134),
    Key::new(0.375_000, 0.535_355),
    Key::new(0.437_500, 0.546_194),
    Key::new(0.500_000, 0.550_000),
    Key::new(0.562_500, 0.546_194),
    Key::new(0.625_000, 0.535_355),
    Key::new(0.687_500, 0.519_134),
    Key::new(0.750_000, 0.500_000),
    Key::new(0.812_500, 0.480_866),
    Key::new(0.875_000, 0.464_645),
    Key::new(0.937_500, 0.453_806),
    Key::new(1.000_000, 0.450_000),
];
const RUN_TWIST: [Key; 17] = [
    Key::new(0.000_000, -0.180_000),
    Key::new(0.062_500, -0.166_298),
    Key::new(0.125_000, -0.127_279),
    Key::new(0.187_500, -0.068_883),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.068_883),
    Key::new(0.375_000, 0.127_279),
    Key::new(0.437_500, 0.166_298),
    Key::new(0.500_000, 0.180_000),
    Key::new(0.562_500, 0.166_298),
    Key::new(0.625_000, 0.127_279),
    Key::new(0.687_500, 0.068_883),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.068_883),
    Key::new(0.875_000, -0.127_279),
    Key::new(0.937_500, -0.166_298),
    Key::new(1.000_000, -0.180_000),
];
const IDLE_TILT: [Key; 17] = [
    Key::new(0.000_000, 0.000_000),
    Key::new(0.062_500, 0.019_134),
    Key::new(0.125_000, 0.035_355),
    Key::new(0.187_500, 0.046_194),
    Key::new(0.250_000, 0.050_000),
    Key::new(0.312_500, 0.046_194),
    Key::new(0.375_000, 0.035_355),
    Key::new(0.437_500, 0.019_134),
    Key::new(0.500_000, 0.000_000),
    Key::new(0.562_500, -0.019_134),
    Key::new(0.625_000, -0.035_355),
    Key::new(0.687_500, -0.046_194),
    Key::new(0.750_000, -0.050_000),
    Key::new(0.812_500, -0.046_194),
    Key::new(0.875_000, -0.035_355),
    Key::new(0.937_500, -0.019_134),
    Key::new(1.000_000, 0.000_000),
];
const IDLE_HEAD: [Key; 17] = [
    Key::new(0.000_000, 0.060_000),
    Key::new(0.062_500, 0.055_433),
    Key::new(0.125_000, 0.042_426),
    Key::new(0.187_500, 0.022_961),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, -0.022_961),
    Key::new(0.375_000, -0.042_426),
    Key::new(0.437_500, -0.055_433),
    Key::new(0.500_000, -0.060_000),
    Key::new(0.562_500, -0.055_433),
    Key::new(0.625_000, -0.042_426),
    Key::new(0.687_500, -0.022_961),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, 0.022_961),
    Key::new(0.875_000, 0.042_426),
    Key::new(0.937_500, 0.055_433),
    Key::new(1.000_000, 0.060_000),
];

/// The relaxed stance the idle legs hold, solved once for a figure standing
/// a little into its own knees.
const IDLE_HIP_KEY: Key = Key::new(0.000_000, 0.115_706);
const IDLE_KNEE_KEY: Key = Key::new(0.000_000, 0.188_757);
const IDLE_ANKLE_KEY: Key = Key::new(0.000_000, -0.316_579);

/// The right leg, half a cycle behind the left.
const WALK_HIP_RIGHT: [Key; 33] = opposite(&WALK_HIP_LEFT);
const WALK_KNEE_RIGHT: [Key; 33] = opposite(&WALK_KNEE_LEFT);
const WALK_ANKLE_RIGHT: [Key; 33] = opposite(&WALK_ANKLE_LEFT);
const RUN_HIP_RIGHT: [Key; 33] = opposite(&RUN_HIP_LEFT);
const RUN_KNEE_RIGHT: [Key; 33] = opposite(&RUN_KNEE_LEFT);
const RUN_ANKLE_RIGHT: [Key; 33] = opposite(&RUN_ANKLE_LEFT);

/// The right arm, half a cycle behind the left — which is the same thing as
/// saying it swings with the left leg.
const WALK_ARM_RIGHT: [Key; 17] = opposite(&WALK_ARM_LEFT);
const WALK_ELBOW_RIGHT: [Key; 17] = opposite(&WALK_ELBOW_LEFT);
const RUN_ARM_RIGHT: [Key; 17] = opposite(&RUN_ARM_LEFT);
const RUN_ELBOW_RIGHT: [Key; 17] = opposite(&RUN_ELBOW_LEFT);

/// Standing: the legs solved once for a relaxed, slightly-sunk stance, so
/// both sides read the same table.
pub(super) const IDLE_HIP: [Key; 1] = [IDLE_HIP_KEY];
pub(super) const IDLE_KNEE: [Key; 1] = [IDLE_KNEE_KEY];
pub(super) const IDLE_ANKLE: [Key; 1] = [IDLE_ANKLE_KEY];

/// Arms hanging clear of the trunk rather than through it.
///
/// The arm every clip begins or ends at when it leaves the arms at rest, so
/// a clip fades into and out of standing with nothing to jump across.
pub(super) const REST_SPLAY: f64 = 0.05;

/// A trace of bend, because a hanging arm is not a plank.
pub(super) const REST_ELBOW: f64 = 0.07;

pub(super) const IDLE_SPLAY: [Key; 1] = [Key::new(0.0, REST_SPLAY)];
const IDLE_ELBOW: [Key; 1] = [Key::new(0.0, REST_ELBOW)];

/// Walking leans in a little; running leans in a lot.
const WALK_LEAN: [Key; 1] = [Key::new(0.0, 0.06)];
const RUN_LEAN: [Key; 1] = [Key::new(0.0, 0.20)];

/// The trunk counter-rotating against the pelvis, in phase with the arms.
pub(super) const IDLE_CURVES: [Keyed; 12] = [
    (Param::SpineTilt, &IDLE_TILT),
    (Param::HeadTurn, &IDLE_HEAD),
    (Param::ShoulderSplay(Side::Left), &IDLE_SPLAY),
    (Param::ShoulderSplay(Side::Right), &IDLE_SPLAY),
    (Param::ElbowBend(Side::Left), &IDLE_ELBOW),
    (Param::ElbowBend(Side::Right), &IDLE_ELBOW),
    (Param::HipSwing(Side::Left), &IDLE_HIP),
    (Param::HipSwing(Side::Right), &IDLE_HIP),
    (Param::KneeBend(Side::Left), &IDLE_KNEE),
    (Param::KneeBend(Side::Right), &IDLE_KNEE),
    (Param::AnkleAngle(Side::Left), &IDLE_ANKLE),
    (Param::AnkleAngle(Side::Right), &IDLE_ANKLE),
];

pub(super) const WALK_CURVES: [Keyed; 12] = [
    (Param::SpineBend, &WALK_LEAN),
    (Param::SpineTwist, &WALK_TWIST),
    (Param::ShoulderSwing(Side::Left), &WALK_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &WALK_ARM_RIGHT),
    (Param::ElbowBend(Side::Left), &WALK_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &WALK_ELBOW_RIGHT),
    (Param::HipSwing(Side::Left), &WALK_HIP_LEFT),
    (Param::HipSwing(Side::Right), &WALK_HIP_RIGHT),
    (Param::KneeBend(Side::Left), &WALK_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &WALK_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &WALK_ANKLE_LEFT),
    (Param::AnkleAngle(Side::Right), &WALK_ANKLE_RIGHT),
];

pub(super) const RUN_CURVES: [Keyed; 12] = [
    (Param::SpineBend, &RUN_LEAN),
    (Param::SpineTwist, &RUN_TWIST),
    (Param::ShoulderSwing(Side::Left), &RUN_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &RUN_ARM_RIGHT),
    (Param::ElbowBend(Side::Left), &RUN_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &RUN_ELBOW_RIGHT),
    (Param::HipSwing(Side::Left), &RUN_HIP_LEFT),
    (Param::HipSwing(Side::Right), &RUN_HIP_RIGHT),
    (Param::KneeBend(Side::Left), &RUN_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &RUN_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &RUN_ANKLE_LEFT),
    (Param::AnkleAngle(Side::Right), &RUN_ANKLE_RIGHT),
];
