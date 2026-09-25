//! States a figure is in rather than things it does: falling, dying,
//! sitting, swimming and climbing.
//!
//! Each plays for as long as the state lasts, so all but the death cycle. A
//! falling, swimming or climbing figure has no floor under its feet, and its
//! clip is held to the rule that it never puts one through the floor instead
//! of the rule that it lands on it.

use super::locomotion::{IDLE_CROUCH, REST_ELBOW, REST_SPLAY};
use super::{phases, rooted, smooth, sunk, Keyed, Sink};
use crate::clip::{Easing, Key};
use crate::pose::Param;
use crate::socket::Side;

/// Arms thrown up and out, legs loose beneath: a figure with nothing under
/// it, flailing a little.
pub(super) const FALL_SECONDS: f64 = 0.9;

const FALL_SPLAY_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.620_000),
    Key::new(0.125_000, 0.676_569),
    Key::new(0.250_000, 0.700_000),
    Key::new(0.375_000, 0.676_569),
    Key::new(0.500_000, 0.620_000),
    Key::new(0.625_000, 0.563_431),
    Key::new(0.750_000, 0.540_000),
    Key::new(0.875_000, 0.563_431),
    Key::new(1.000_000, 0.620_000),
];
const FALL_SPLAY_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.620_000),
    Key::new(0.125_000, 0.563_431),
    Key::new(0.250_000, 0.540_000),
    Key::new(0.375_000, 0.563_431),
    Key::new(0.500_000, 0.620_000),
    Key::new(0.625_000, 0.676_569),
    Key::new(0.750_000, 0.700_000),
    Key::new(0.875_000, 0.676_569),
    Key::new(1.000_000, 0.620_000),
];
const FALL_ARM_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.450_000),
    Key::new(0.125_000, 0.391_421),
    Key::new(0.250_000, 0.250_000),
    Key::new(0.375_000, 0.108_579),
    Key::new(0.500_000, 0.050_000),
    Key::new(0.625_000, 0.108_579),
    Key::new(0.750_000, 0.250_000),
    Key::new(0.875_000, 0.391_421),
    Key::new(1.000_000, 0.450_000),
];
const FALL_ARM_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.050_000),
    Key::new(0.125_000, 0.108_579),
    Key::new(0.250_000, 0.250_000),
    Key::new(0.375_000, 0.391_421),
    Key::new(0.500_000, 0.450_000),
    Key::new(0.625_000, 0.391_421),
    Key::new(0.750_000, 0.250_000),
    Key::new(0.875_000, 0.108_579),
    Key::new(1.000_000, 0.050_000),
];
const FALL_ELBOW: [Key; 9] = [
    Key::new(0.000_000, 0.300_000),
    Key::new(0.125_000, 0.356_569),
    Key::new(0.250_000, 0.380_000),
    Key::new(0.375_000, 0.356_569),
    Key::new(0.500_000, 0.300_000),
    Key::new(0.625_000, 0.243_431),
    Key::new(0.750_000, 0.220_000),
    Key::new(0.875_000, 0.243_431),
    Key::new(1.000_000, 0.300_000),
];
const FALL_HIP_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.200_000),
    Key::new(0.125_000, 0.327_279),
    Key::new(0.250_000, 0.380_000),
    Key::new(0.375_000, 0.327_279),
    Key::new(0.500_000, 0.200_000),
    Key::new(0.625_000, 0.072_721),
    Key::new(0.750_000, 0.020_000),
    Key::new(0.875_000, 0.072_721),
    Key::new(1.000_000, 0.200_000),
];
const FALL_HIP_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.200_000),
    Key::new(0.125_000, 0.072_721),
    Key::new(0.250_000, 0.020_000),
    Key::new(0.375_000, 0.072_721),
    Key::new(0.500_000, 0.200_000),
    Key::new(0.625_000, 0.327_279),
    Key::new(0.750_000, 0.380_000),
    Key::new(0.875_000, 0.327_279),
    Key::new(1.000_000, 0.200_000),
];
const FALL_KNEE_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.520_000),
    Key::new(0.125_000, 0.484_853),
    Key::new(0.250_000, 0.400_000),
    Key::new(0.375_000, 0.315_147),
    Key::new(0.500_000, 0.280_000),
    Key::new(0.625_000, 0.315_147),
    Key::new(0.750_000, 0.400_000),
    Key::new(0.875_000, 0.484_853),
    Key::new(1.000_000, 0.520_000),
];
const FALL_KNEE_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.280_000),
    Key::new(0.125_000, 0.315_147),
    Key::new(0.250_000, 0.400_000),
    Key::new(0.375_000, 0.484_853),
    Key::new(0.500_000, 0.520_000),
    Key::new(0.625_000, 0.484_853),
    Key::new(0.750_000, 0.400_000),
    Key::new(0.875_000, 0.315_147),
    Key::new(1.000_000, 0.280_000),
];
const FALL_TAIL: [Key; 9] = [
    Key::new(0.000_000, 0.600_000),
    Key::new(0.125_000, 0.741_421),
    Key::new(0.250_000, 0.800_000),
    Key::new(0.375_000, 0.741_421),
    Key::new(0.500_000, 0.600_000),
    Key::new(0.625_000, 0.458_579),
    Key::new(0.750_000, 0.400_000),
    Key::new(0.875_000, 0.458_579),
    Key::new(1.000_000, 0.600_000),
];
const FALL_LEAN: [Key; 1] = [Key::new(0.0, -0.12)];
const FALL_NOD: [Key; 1] = [Key::new(0.0, -0.20)];
const FALL_HIP_SPLAY: [Key; 1] = [Key::new(0.0, 0.25)];
const FALL_ANKLE: [Key; 1] = [Key::new(0.0, -0.25)];

pub(super) const FALL_CURVES: [Keyed; 17] = [
    (Param::SpineBend, &FALL_LEAN),
    (Param::HeadNod, &FALL_NOD),
    (Param::ShoulderSwing(Side::Left), &FALL_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &FALL_ARM_RIGHT),
    (Param::ShoulderSplay(Side::Left), &FALL_SPLAY_LEFT),
    (Param::ShoulderSplay(Side::Right), &FALL_SPLAY_RIGHT),
    (Param::ElbowBend(Side::Left), &FALL_ELBOW),
    (Param::ElbowBend(Side::Right), &FALL_ELBOW),
    (Param::HipSwing(Side::Left), &FALL_HIP_LEFT),
    (Param::HipSwing(Side::Right), &FALL_HIP_RIGHT),
    (Param::HipSplay(Side::Left), &FALL_HIP_SPLAY),
    (Param::HipSplay(Side::Right), &FALL_HIP_SPLAY),
    (Param::KneeBend(Side::Left), &FALL_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &FALL_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &FALL_ANKLE),
    (Param::AnkleAngle(Side::Right), &FALL_ANKLE),
    (Param::TailLift, &FALL_TAIL),
];

/// A brief recoil, the knees giving way into a squat with the feet where
/// they stood, and the trunk slumping over them. Plays once and holds.
pub(super) const DIE_SECONDS: f64 = 1.4;

/// How deep the dying body stands into its legs: at rest through the
/// recoil, then sinking into the squat and staying there.
pub(super) const fn die_depth(phase: f64) -> f64 {
    IDLE_CROUCH + (DIE_SQUAT - IDLE_CROUCH) * smooth((phase - DIE_BUCKLE) / (DIE_DOWN - DIE_BUCKLE))
}

/// Where the knees give way, where the body has sunk, and how deep.
const DIE_BUCKLE: f64 = 0.15;
const DIE_DOWN: f64 = 0.70;
const DIE_SQUAT: f64 = 22.0;

/// The death's root height, keyed where its legs are.
pub(super) const DIE_LIFT: [Key; DIE_HIP.len()] = sunk(phases(&DIE_HIP), Sink::Die);

/// Feet under the hips, re-solved as the body sinks over them; the heel
/// lifts off as a squat deepens.
pub(super) const DIE_HIP: [Key; 14] = [
    Key::new(0.000_000, 0.115_706),
    Key::new(0.150_000, 0.115_706),
    Key::new(0.200_000, 0.137_227),
    Key::new(0.250_000, 0.184_045),
    Key::new(0.300_000, 0.238_067),
    Key::new(0.350_000, 0.292_305),
    Key::new(0.400_000, 0.343_969),
    Key::new(0.450_000, 0.391_531),
    Key::new(0.500_000, 0.433_790),
    Key::new(0.550_000, 0.469_527),
    Key::new(0.600_000, 0.497_335),
    Key::new(0.650_000, 0.515_526),
    Key::new(0.700_000, 0.522_091),
    Key::new(1.000_000, 0.522_091),
];
pub(super) const DIE_KNEE: [Key; 14] = [
    Key::new(0.000_000, 0.188_757),
    Key::new(0.150_000, 0.188_757),
    Key::new(0.200_000, 0.223_831),
    Key::new(0.250_000, 0.300_067),
    Key::new(0.300_000, 0.387_874),
    Key::new(0.350_000, 0.475_787),
    Key::new(0.400_000, 0.559_206),
    Key::new(0.450_000, 0.635_610),
    Key::new(0.500_000, 0.703_060),
    Key::new(0.550_000, 0.759_668),
    Key::new(0.600_000, 0.803_356),
    Key::new(0.650_000, 0.831_723),
    Key::new(0.700_000, 0.841_914),
    Key::new(1.000_000, 0.841_914),
];
pub(super) const DIE_ANKLE: [Key; 14] = [
    Key::new(0.000_000, -0.189_947),
    Key::new(0.150_000, -0.189_947),
    Key::new(0.200_000, -0.225_206),
    Key::new(0.250_000, -0.301_774),
    Key::new(0.300_000, -0.389_797),
    Key::new(0.350_000, -0.477_668),
    Key::new(0.400_000, -0.560_705),
    Key::new(0.450_000, -0.636_344),
    Key::new(0.500_000, -0.702_655),
    Key::new(0.550_000, -0.757_841),
    Key::new(0.600_000, -0.800_043),
    Key::new(0.650_000, -0.827_215),
    Key::new(0.700_000, -0.836_923),
    Key::new(1.000_000, -0.836_923),
];
const DIE_LEAN: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, -0.250_000).eased(Easing::EaseInOut),
    Key::new(0.500_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(0.850_000, 0.850_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.850_000),
];
const DIE_TILT: [Key; 3] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.500_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.180_000),
];
const DIE_NOD: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(0.500_000, 0.200_000).eased(Easing::EaseInOut),
    Key::new(0.850_000, 0.800_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.800_000),
];
const DIE_ROLL: [Key; 3] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.850_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.300_000),
];
const DIE_ARM: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.150_000).eased(Easing::EaseInOut),
    Key::new(0.850_000, 0.250_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.250_000),
];
const DIE_SPLAY: [Key; 3] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.850_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.100_000),
];
const DIE_ELBOW: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.200_000).eased(Easing::EaseInOut),
    Key::new(0.850_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.120_000),
];
const DIE_TAIL: [Key; 3] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.850_000, -0.550_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, -0.550_000),
];

pub(super) const DIE_CURVES: [Keyed; 17] = [
    (Param::SpineBend, &DIE_LEAN),
    (Param::SpineTilt, &DIE_TILT),
    (Param::HeadNod, &DIE_NOD),
    (Param::HeadTilt, &DIE_ROLL),
    (Param::ShoulderSwing(Side::Left), &DIE_ARM),
    (Param::ShoulderSwing(Side::Right), &DIE_ARM),
    (Param::ShoulderSplay(Side::Left), &DIE_SPLAY),
    (Param::ShoulderSplay(Side::Right), &DIE_SPLAY),
    (Param::ElbowBend(Side::Left), &DIE_ELBOW),
    (Param::ElbowBend(Side::Right), &DIE_ELBOW),
    (Param::HipSwing(Side::Left), &DIE_HIP),
    (Param::HipSwing(Side::Right), &DIE_HIP),
    (Param::KneeBend(Side::Left), &DIE_KNEE),
    (Param::KneeBend(Side::Right), &DIE_KNEE),
    (Param::AnkleAngle(Side::Left), &DIE_ANKLE),
    (Param::AnkleAngle(Side::Right), &DIE_ANKLE),
    (Param::TailLift, &DIE_TAIL),
];

/// Seated low with the knees up and the forearms reaching toward them,
/// breathing and looking about.
pub(super) const SIT_SECONDS: f64 = 3.0;

/// How deep the seated body is held into its legs, throughout.
pub(super) const SIT_DEPTH: f64 = 26.0;

pub(super) const SIT_LIFT: [Key; 2] = [
    Key::new(0.0, rooted(-SIT_DEPTH)),
    Key::new(1.0, rooted(-SIT_DEPTH)),
];

/// The feet out in front and a little apart, solved once: both sides read
/// the same table, since an outward splay is handed by the drive.
pub(super) const SIT_HIP: [Key; 1] = [Key::new(0.000_000, 0.811_153)];
pub(super) const SIT_SPLAY: [Key; 1] = [Key::new(0.000_000, 0.121_072)];
pub(super) const SIT_KNEE: [Key; 1] = [Key::new(0.000_000, 0.512_499)];
pub(super) const SIT_ANKLE: [Key; 1] = [Key::new(0.000_000, 0.560_439)];
const SIT_LEAN: [Key; 9] = [
    Key::new(0.000_000, 0.220_000),
    Key::new(0.125_000, 0.214_142),
    Key::new(0.250_000, 0.200_000),
    Key::new(0.375_000, 0.185_858),
    Key::new(0.500_000, 0.180_000),
    Key::new(0.625_000, 0.185_858),
    Key::new(0.750_000, 0.200_000),
    Key::new(0.875_000, 0.214_142),
    Key::new(1.000_000, 0.220_000),
];
const SIT_LOOK: [Key; 9] = [
    Key::new(0.000_000, 0.000_000),
    Key::new(0.125_000, 0.070_711),
    Key::new(0.250_000, 0.100_000),
    Key::new(0.375_000, 0.070_711),
    Key::new(0.500_000, 0.000_000),
    Key::new(0.625_000, -0.070_711),
    Key::new(0.750_000, -0.100_000),
    Key::new(0.875_000, -0.070_711),
    Key::new(1.000_000, 0.000_000),
];
const SIT_NOD: [Key; 1] = [Key::new(0.0, 0.08)];
const SIT_ARM: [Key; 1] = [Key::new(0.0, 0.38)];
const SIT_ARM_SPLAY: [Key; 1] = [Key::new(0.0, 0.12)];
const SIT_ELBOW: [Key; 1] = [Key::new(0.0, 0.20)];

pub(super) const SIT_CURVES: [Keyed; 17] = [
    (Param::SpineBend, &SIT_LEAN),
    (Param::HeadTurn, &SIT_LOOK),
    (Param::HeadNod, &SIT_NOD),
    (Param::ShoulderSwing(Side::Left), &SIT_ARM),
    (Param::ShoulderSwing(Side::Right), &SIT_ARM),
    (Param::ShoulderSplay(Side::Left), &SIT_ARM_SPLAY),
    (Param::ShoulderSplay(Side::Right), &SIT_ARM_SPLAY),
    (Param::ElbowBend(Side::Left), &SIT_ELBOW),
    (Param::ElbowBend(Side::Right), &SIT_ELBOW),
    (Param::HipSwing(Side::Left), &SIT_HIP),
    (Param::HipSwing(Side::Right), &SIT_HIP),
    (Param::HipSplay(Side::Left), &SIT_SPLAY),
    (Param::HipSplay(Side::Right), &SIT_SPLAY),
    (Param::KneeBend(Side::Left), &SIT_KNEE),
    (Param::KneeBend(Side::Right), &SIT_KNEE),
    (Param::AnkleAngle(Side::Left), &SIT_ANKLE),
    (Param::AnkleAngle(Side::Right), &SIT_ANKLE),
];

/// Treading water upright: an alternating kick beneath and the arms sculling
/// at the surface.
pub(super) const SWIM_SECONDS: f64 = 1.4;

const SWIM_HIP_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.300_000),
    Key::new(0.125_000, 0.441_421),
    Key::new(0.250_000, 0.500_000),
    Key::new(0.375_000, 0.441_421),
    Key::new(0.500_000, 0.300_000),
    Key::new(0.625_000, 0.158_579),
    Key::new(0.750_000, 0.100_000),
    Key::new(0.875_000, 0.158_579),
    Key::new(1.000_000, 0.300_000),
];
const SWIM_HIP_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.300_000),
    Key::new(0.125_000, 0.158_579),
    Key::new(0.250_000, 0.100_000),
    Key::new(0.375_000, 0.158_579),
    Key::new(0.500_000, 0.300_000),
    Key::new(0.625_000, 0.441_421),
    Key::new(0.750_000, 0.500_000),
    Key::new(0.875_000, 0.441_421),
    Key::new(1.000_000, 0.300_000),
];
const SWIM_KNEE_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.800_000),
    Key::new(0.125_000, 0.726_777),
    Key::new(0.250_000, 0.550_000),
    Key::new(0.375_000, 0.373_223),
    Key::new(0.500_000, 0.300_000),
    Key::new(0.625_000, 0.373_223),
    Key::new(0.750_000, 0.550_000),
    Key::new(0.875_000, 0.726_777),
    Key::new(1.000_000, 0.800_000),
];
const SWIM_KNEE_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.300_000),
    Key::new(0.125_000, 0.373_223),
    Key::new(0.250_000, 0.550_000),
    Key::new(0.375_000, 0.726_777),
    Key::new(0.500_000, 0.800_000),
    Key::new(0.625_000, 0.726_777),
    Key::new(0.750_000, 0.550_000),
    Key::new(0.875_000, 0.373_223),
    Key::new(1.000_000, 0.300_000),
];
const SWIM_SPLAY: [Key; 9] = [
    Key::new(0.000_000, 0.380_000),
    Key::new(0.125_000, 0.450_711),
    Key::new(0.250_000, 0.480_000),
    Key::new(0.375_000, 0.450_711),
    Key::new(0.500_000, 0.380_000),
    Key::new(0.625_000, 0.309_289),
    Key::new(0.750_000, 0.280_000),
    Key::new(0.875_000, 0.309_289),
    Key::new(1.000_000, 0.380_000),
];
const SWIM_ARM: [Key; 9] = [
    Key::new(0.000_000, 0.300_000),
    Key::new(0.125_000, 0.256_066),
    Key::new(0.250_000, 0.150_000),
    Key::new(0.375_000, 0.043_934),
    Key::new(0.500_000, 0.000_000),
    Key::new(0.625_000, 0.043_934),
    Key::new(0.750_000, 0.150_000),
    Key::new(0.875_000, 0.256_066),
    Key::new(1.000_000, 0.300_000),
];
const SWIM_ELBOW: [Key; 9] = [
    Key::new(0.000_000, 0.350_000),
    Key::new(0.125_000, 0.420_711),
    Key::new(0.250_000, 0.450_000),
    Key::new(0.375_000, 0.420_711),
    Key::new(0.500_000, 0.350_000),
    Key::new(0.625_000, 0.279_289),
    Key::new(0.750_000, 0.250_000),
    Key::new(0.875_000, 0.279_289),
    Key::new(1.000_000, 0.350_000),
];
const SWIM_TAIL: [Key; 9] = [
    Key::new(0.000_000, 0.000_000),
    Key::new(0.125_000, 0.282_843),
    Key::new(0.250_000, 0.400_000),
    Key::new(0.375_000, 0.282_843),
    Key::new(0.500_000, 0.000_000),
    Key::new(0.625_000, -0.282_843),
    Key::new(0.750_000, -0.400_000),
    Key::new(0.875_000, -0.282_843),
    Key::new(1.000_000, 0.000_000),
];
const SWIM_LEAN: [Key; 1] = [Key::new(0.0, 0.10)];
const SWIM_NOD: [Key; 1] = [Key::new(0.0, -0.20)];
const SWIM_HIP_SPLAY: [Key; 1] = [Key::new(0.0, 0.40)];
const SWIM_ANKLE: [Key; 1] = [Key::new(0.0, -0.35)];

pub(super) const SWIM_CURVES: [Keyed; 17] = [
    (Param::SpineBend, &SWIM_LEAN),
    (Param::HeadNod, &SWIM_NOD),
    (Param::ShoulderSwing(Side::Left), &SWIM_ARM),
    (Param::ShoulderSwing(Side::Right), &SWIM_ARM),
    (Param::ShoulderSplay(Side::Left), &SWIM_SPLAY),
    (Param::ShoulderSplay(Side::Right), &SWIM_SPLAY),
    (Param::ElbowBend(Side::Left), &SWIM_ELBOW),
    (Param::ElbowBend(Side::Right), &SWIM_ELBOW),
    (Param::HipSwing(Side::Left), &SWIM_HIP_LEFT),
    (Param::HipSwing(Side::Right), &SWIM_HIP_RIGHT),
    (Param::HipSplay(Side::Left), &SWIM_HIP_SPLAY),
    (Param::HipSplay(Side::Right), &SWIM_HIP_SPLAY),
    (Param::KneeBend(Side::Left), &SWIM_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &SWIM_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &SWIM_ANKLE),
    (Param::AnkleAngle(Side::Right), &SWIM_ANKLE),
    (Param::TailSwing, &SWIM_TAIL),
];

/// Hand over hand up a face in front of the figure, each hand reaching as the
/// opposite foot steps.
pub(super) const CLIMB_SECONDS: f64 = 1.2;

const CLIMB_ARM_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.820_000),
    Key::new(0.125_000, 0.784_853),
    Key::new(0.250_000, 0.700_000),
    Key::new(0.375_000, 0.615_147),
    Key::new(0.500_000, 0.580_000),
    Key::new(0.625_000, 0.615_147),
    Key::new(0.750_000, 0.700_000),
    Key::new(0.875_000, 0.784_853),
    Key::new(1.000_000, 0.820_000),
];
const CLIMB_ARM_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.580_000),
    Key::new(0.125_000, 0.615_147),
    Key::new(0.250_000, 0.700_000),
    Key::new(0.375_000, 0.784_853),
    Key::new(0.500_000, 0.820_000),
    Key::new(0.625_000, 0.784_853),
    Key::new(0.750_000, 0.700_000),
    Key::new(0.875_000, 0.615_147),
    Key::new(1.000_000, 0.580_000),
];
const CLIMB_ELBOW_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.150_000),
    Key::new(0.125_000, 0.208_579),
    Key::new(0.250_000, 0.350_000),
    Key::new(0.375_000, 0.491_421),
    Key::new(0.500_000, 0.550_000),
    Key::new(0.625_000, 0.491_421),
    Key::new(0.750_000, 0.350_000),
    Key::new(0.875_000, 0.208_579),
    Key::new(1.000_000, 0.150_000),
];
const CLIMB_ELBOW_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.550_000),
    Key::new(0.125_000, 0.491_421),
    Key::new(0.250_000, 0.350_000),
    Key::new(0.375_000, 0.208_579),
    Key::new(0.500_000, 0.150_000),
    Key::new(0.625_000, 0.208_579),
    Key::new(0.750_000, 0.350_000),
    Key::new(0.875_000, 0.491_421),
    Key::new(1.000_000, 0.550_000),
];
const CLIMB_HIP_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.300_000),
    Key::new(0.125_000, 0.476_777),
    Key::new(0.250_000, 0.550_000),
    Key::new(0.375_000, 0.476_777),
    Key::new(0.500_000, 0.300_000),
    Key::new(0.625_000, 0.123_223),
    Key::new(0.750_000, 0.050_000),
    Key::new(0.875_000, 0.123_223),
    Key::new(1.000_000, 0.300_000),
];
const CLIMB_HIP_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.300_000),
    Key::new(0.125_000, 0.123_223),
    Key::new(0.250_000, 0.050_000),
    Key::new(0.375_000, 0.123_223),
    Key::new(0.500_000, 0.300_000),
    Key::new(0.625_000, 0.476_777),
    Key::new(0.750_000, 0.550_000),
    Key::new(0.875_000, 0.476_777),
    Key::new(1.000_000, 0.300_000),
];
const CLIMB_KNEE_LEFT: [Key; 9] = [
    Key::new(0.000_000, 0.500_000),
    Key::new(0.125_000, 0.712_132),
    Key::new(0.250_000, 0.800_000),
    Key::new(0.375_000, 0.712_132),
    Key::new(0.500_000, 0.500_000),
    Key::new(0.625_000, 0.287_868),
    Key::new(0.750_000, 0.200_000),
    Key::new(0.875_000, 0.287_868),
    Key::new(1.000_000, 0.500_000),
];
const CLIMB_KNEE_RIGHT: [Key; 9] = [
    Key::new(0.000_000, 0.500_000),
    Key::new(0.125_000, 0.287_868),
    Key::new(0.250_000, 0.200_000),
    Key::new(0.375_000, 0.287_868),
    Key::new(0.500_000, 0.500_000),
    Key::new(0.625_000, 0.712_132),
    Key::new(0.750_000, 0.800_000),
    Key::new(0.875_000, 0.712_132),
    Key::new(1.000_000, 0.500_000),
];
const CLIMB_LEAN: [Key; 1] = [Key::new(0.0, 0.18)];
const CLIMB_NOD: [Key; 1] = [Key::new(0.0, -0.30)];
const CLIMB_SPLAY: [Key; 1] = [Key::new(0.0, 0.12)];
const CLIMB_ANKLE: [Key; 1] = [Key::new(0.0, 0.20)];
const CLIMB_TAIL: [Key; 1] = [Key::new(0.0, -0.30)];

pub(super) const CLIMB_CURVES: [Keyed; 15] = [
    (Param::SpineBend, &CLIMB_LEAN),
    (Param::HeadNod, &CLIMB_NOD),
    (Param::ShoulderSwing(Side::Left), &CLIMB_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &CLIMB_ARM_RIGHT),
    (Param::ShoulderSplay(Side::Left), &CLIMB_SPLAY),
    (Param::ShoulderSplay(Side::Right), &CLIMB_SPLAY),
    (Param::ElbowBend(Side::Left), &CLIMB_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &CLIMB_ELBOW_RIGHT),
    (Param::HipSwing(Side::Left), &CLIMB_HIP_LEFT),
    (Param::HipSwing(Side::Right), &CLIMB_HIP_RIGHT),
    (Param::KneeBend(Side::Left), &CLIMB_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &CLIMB_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &CLIMB_ANKLE),
    (Param::AnkleAngle(Side::Right), &CLIMB_ANKLE),
    (Param::TailLift, &CLIMB_TAIL),
];
