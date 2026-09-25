//! Actions and the reactions to them: a dodge, the two melee blows, the
//! bow's draw and loose, a cast and its channel, a flinch and a stagger.
//!
//! An action is windup, active and recovery. Each clip here is authored
//! across all three in phase, and the phases its segments meet at are stated
//! beside it; how long each segment lasts is supplied from outside, so the
//! action that owns the timing stretches the clip rather than the clip
//! deciding it. The durations stated here are each action's reference
//! timing, which `WinterSun`'s action documents replace.
//!
//! Every segment eases in and out, so a segment boundary is a change of rate
//! and never a kink: a fast active segment is a short stretch of time over a
//! smooth curve, not a sharp curve.
//!
//! The legs of a clip whose feet stay down while its body rises and falls are
//! each stated foot path put through the planting layer's own two-bone solve,
//! as locomotion's are, and the motion set's tests solve every key again.

use super::locomotion::{IDLE_CROUCH, IDLE_SPLAY, REST_ELBOW, REST_SPLAY};
use super::{phases, smooth, sunk, Action, Keyed, Sink};
use crate::clip::{Easing, Event, Key};
use crate::pose::Param;
use crate::socket::Side;

/// A crouch, a dash with both feet off the ground, and a landing.
///
/// The move is the simulation's; the clip only paces it. Both feet are down
/// only where the travel is flat, before the dash and after it, so the planted
/// foot cannot slide whatever distance the dodge was granted.
pub(super) const DODGE: Action = Action::new(0.30, 0.70, [0.10, 0.28, 0.22]);

/// The landing, where the feet take the body back.
pub(super) const DODGE_EVENTS: [Event; 1] = [Event::new("footstep", DODGE.recovery)];

/// None of the move before the feet leave, all of it by the time they land.
pub(super) const DODGE_TRAVEL: [Key; 4] = [
    Key::new(0.000_000, 0.000_000),
    Key::new(0.300_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.700_000, 1.000_000),
    Key::new(1.000_000, 1.000_000),
];
/// How deep the dodging body stands into its legs: down into the crouch,
/// over the flight's parabola, and back up out of the landing. Negative is
/// above a straight leg, which only the flight reaches.
pub(super) const fn dodge_depth(phase: f64) -> f64 {
    if phase <= DODGE.active {
        IDLE_CROUCH + (DODGE_CROUCH - IDLE_CROUCH) * smooth(phase / DODGE.active)
    } else if phase < DODGE.recovery {
        let middle = f64::midpoint(DODGE.active, DODGE.recovery);
        let t = (phase - middle) / (middle - DODGE.active);
        DODGE_CROUCH - DODGE_RISE * (1.0 - t * t)
    } else {
        DODGE_CROUCH
            - (DODGE_CROUCH - IDLE_CROUCH)
                * smooth((phase - DODGE.recovery) / (1.0 - DODGE.recovery))
    }
}

/// How deep the dodge crouches to spring, and lands into.
const DODGE_CROUCH: f64 = 9.0;

/// How far the flight carries the body above the crouch.
const DODGE_RISE: f64 = 13.0;

/// The dodge's root height, keyed through the crouch, the flight and the
/// landing.
pub(super) const DODGE_LIFT: [Key; 25] = sunk(
    [
        0.0, 0.0375, 0.075, 0.1125, 0.15, 0.1875, 0.225, 0.2625, 0.30, 0.35, 0.40, 0.45, 0.50,
        0.55, 0.60, 0.65, 0.70, 0.7375, 0.775, 0.8125, 0.85, 0.8875, 0.925, 0.9625, 1.0,
    ],
    Sink::Dodge,
);

/// Feet under the hips while down; tucked, and clear of the floor, in the
/// air.
pub(super) const DODGE_HIP: [Key; 23] = [
    Key::new(0.000_000, 0.115_706),
    Key::new(0.037_500, 0.130_963),
    Key::new(0.075_000, 0.164_696),
    Key::new(0.112_500, 0.203_362),
    Key::new(0.150_000, 0.240_552),
    Key::new(0.187_500, 0.273_114),
    Key::new(0.225_000, 0.298_941),
    Key::new(0.262_500, 0.316_108),
    Key::new(0.300_000, 0.322_417),
    Key::new(0.350_000, 0.360_000),
    Key::new(0.400_000, 0.400_000),
    Key::new(0.500_000, 0.420_000),
    Key::new(0.600_000, 0.400_000),
    Key::new(0.650_000, 0.360_000),
    Key::new(0.700_000, 0.322_417),
    Key::new(0.737_500, 0.316_108),
    Key::new(0.775_000, 0.298_941),
    Key::new(0.812_500, 0.273_114),
    Key::new(0.850_000, 0.240_552),
    Key::new(0.887_500, 0.203_362),
    Key::new(0.925_000, 0.164_696),
    Key::new(0.962_500, 0.130_963),
    Key::new(1.000_000, 0.115_706),
];
pub(super) const DODGE_KNEE: [Key; 23] = [
    Key::new(0.000_000, 0.188_757),
    Key::new(0.037_500, 0.213_624),
    Key::new(0.075_000, 0.268_573),
    Key::new(0.112_500, 0.331_488),
    Key::new(0.150_000, 0.391_908),
    Key::new(0.187_500, 0.444_714),
    Key::new(0.225_000, 0.486_523),
    Key::new(0.262_500, 0.514_266),
    Key::new(0.300_000, 0.524_454),
    Key::new(0.350_000, 0.600_000),
    Key::new(0.400_000, 0.700_000),
    Key::new(0.500_000, 0.740_000),
    Key::new(0.600_000, 0.700_000),
    Key::new(0.650_000, 0.600_000),
    Key::new(0.700_000, 0.524_454),
    Key::new(0.737_500, 0.514_266),
    Key::new(0.775_000, 0.486_523),
    Key::new(0.812_500, 0.444_714),
    Key::new(0.850_000, 0.391_908),
    Key::new(0.887_500, 0.331_488),
    Key::new(0.925_000, 0.268_573),
    Key::new(0.962_500, 0.213_624),
    Key::new(1.000_000, 0.188_757),
];
pub(super) const DODGE_ANKLE: [Key; 23] = [
    Key::new(0.000_000, -0.253_263),
    Key::new(0.037_500, -0.286_597),
    Key::new(0.075_000, -0.360_209),
    Key::new(0.112_500, -0.444_395),
    Key::new(0.150_000, -0.525_113),
    Key::new(0.187_500, -0.595_528),
    Key::new(0.225_000, -0.651_168),
    Key::new(0.262_500, -0.688_027),
    Key::new(0.300_000, -0.701_547),
    Key::new(0.350_000, -0.450_000),
    Key::new(0.400_000, -0.350_000),
    Key::new(0.500_000, -0.350_000),
    Key::new(0.600_000, -0.350_000),
    Key::new(0.650_000, -0.550_000),
    Key::new(0.700_000, -0.701_547),
    Key::new(0.737_500, -0.688_027),
    Key::new(0.775_000, -0.651_168),
    Key::new(0.812_500, -0.595_528),
    Key::new(0.850_000, -0.525_113),
    Key::new(0.887_500, -0.444_395),
    Key::new(0.925_000, -0.360_209),
    Key::new(0.962_500, -0.286_597),
    Key::new(1.000_000, -0.253_263),
];
const DODGE_LEAN: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.300_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(0.500_000, 0.450_000).eased(Easing::EaseInOut),
    Key::new(0.700_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const DODGE_ARM: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.300_000, -0.400_000).eased(Easing::EaseInOut),
    Key::new(0.500_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(0.700_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const DODGE_ELBOW: [Key; 5] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.300_000, 0.250_000).eased(Easing::EaseInOut),
    Key::new(0.500_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(0.700_000, 0.250_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];
const DODGE_NOD: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.300_000, -0.150_000).eased(Easing::EaseInOut),
    Key::new(0.700_000, -0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const DODGE_TAIL: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.300_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(0.500_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(0.700_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];

pub(super) const DODGE_CURVES: [Keyed; 15] = [
    (Param::SpineBend, &DODGE_LEAN),
    (Param::HeadNod, &DODGE_NOD),
    (Param::ShoulderSwing(Side::Left), &DODGE_ARM),
    (Param::ShoulderSwing(Side::Right), &DODGE_ARM),
    (Param::ShoulderSplay(Side::Left), &IDLE_SPLAY),
    (Param::ShoulderSplay(Side::Right), &IDLE_SPLAY),
    (Param::ElbowBend(Side::Left), &DODGE_ELBOW),
    (Param::ElbowBend(Side::Right), &DODGE_ELBOW),
    (Param::HipSwing(Side::Left), &DODGE_HIP),
    (Param::HipSwing(Side::Right), &DODGE_HIP),
    (Param::KneeBend(Side::Left), &DODGE_KNEE),
    (Param::KneeBend(Side::Right), &DODGE_KNEE),
    (Param::AnkleAngle(Side::Left), &DODGE_ANKLE),
    (Param::AnkleAngle(Side::Right), &DODGE_ANKLE),
    (Param::TailLift, &DODGE_TAIL),
];

/// A horizontal slash with the right arm, the chest winding up away from it
/// and turning through it. Upper body only, so it plays over the legs of
/// whatever the figure is doing.
pub(super) const MELEE_LIGHT: Action = Action::new(0.35, 0.55, [0.14, 0.08, 0.24]);

/// The blade entering its arc, when a hitbox can land.
pub(super) const MELEE_LIGHT_EVENTS: [Event; 1] = [Event::new("hit_frame", MELEE_LIGHT.active)];

const LIGHT_TWIST: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, -0.500_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, 0.450_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LIGHT_LEAN: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.050_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, 0.150_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LIGHT_LOOK: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LIGHT_ARM_RIGHT: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, -0.150_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, 0.500_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LIGHT_SPLAY_RIGHT: [Key; 4] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.600_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_SPLAY),
];
const LIGHT_ELBOW_RIGHT: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.500_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];
const LIGHT_WRIST_RIGHT: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LIGHT_ARM_LEFT: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.250_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LIGHT_ELBOW_LEFT: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.450_000).eased(Easing::EaseInOut),
    Key::new(0.550_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];

pub(super) const MELEE_LIGHT_CURVES: [Keyed; 10] = [
    (Param::SpineBend, &LIGHT_LEAN),
    (Param::SpineTwist, &LIGHT_TWIST),
    (Param::HeadTurn, &LIGHT_LOOK),
    (Param::ShoulderSwing(Side::Left), &LIGHT_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &LIGHT_ARM_RIGHT),
    (Param::ShoulderSplay(Side::Left), &IDLE_SPLAY),
    (Param::ShoulderSplay(Side::Right), &LIGHT_SPLAY_RIGHT),
    (Param::ElbowBend(Side::Left), &LIGHT_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &LIGHT_ELBOW_RIGHT),
    (Param::WristAngle(Side::Right), &LIGHT_WRIST_RIGHT),
];

/// An overhead chop with both arms, the body loading into the legs as the
/// blow comes down. The feet stay under the hips, so it can begin from a
/// standing figure with nothing to slide.
pub(super) const MELEE_HEAVY: Action = Action::new(0.40, 0.60, [0.40, 0.15, 0.45]);

/// The blow starting down, when a hitbox can land.
pub(super) const MELEE_HEAVY_EVENTS: [Event; 1] = [Event::new("hit_frame", MELEE_HEAVY.active)];

/// How deep the chopping body stands into its legs: loading a little as the
/// arms rise, dropping into the blow, and rising out of it.
pub(super) const fn heavy_depth(phase: f64) -> f64 {
    let (start, end) = (MELEE_HEAVY.active, MELEE_HEAVY.recovery);
    if phase <= start {
        IDLE_CROUCH + (HEAVY_LOAD - IDLE_CROUCH) * smooth(phase / start)
    } else if phase <= end {
        HEAVY_LOAD + (HEAVY_DROP - HEAVY_LOAD) * smooth((phase - start) / (end - start))
    } else {
        HEAVY_DROP - (HEAVY_DROP - IDLE_CROUCH) * smooth((phase - end) / (1.0 - end))
    }
}

/// How far the body loads as the arms go up, and how far it drops into the
/// blow.
const HEAVY_LOAD: f64 = 2.5;
const HEAVY_DROP: f64 = 8.0;

/// The chop's root height, keyed where its legs are.
pub(super) const HEAVY_LIFT: [Key; HEAVY_HIP.len()] = sunk(phases(&HEAVY_HIP), Sink::Heavy);

/// Feet under the hips, re-solved as the body sinks.
pub(super) const HEAVY_HIP: [Key; 21] = [
    Key::new(0.000_000, 0.115_706),
    Key::new(0.050_000, 0.118_382),
    Key::new(0.100_000, 0.125_170),
    Key::new(0.150_000, 0.134_194),
    Key::new(0.200_000, 0.143_863),
    Key::new(0.250_000, 0.152_939),
    Key::new(0.300_000, 0.160_452),
    Key::new(0.350_000, 0.165_567),
    Key::new(0.400_000, 0.167_469),
    Key::new(0.450_000, 0.194_490),
    Key::new(0.500_000, 0.244_145),
    Key::new(0.550_000, 0.285_950),
    Key::new(0.600_000, 0.303_268),
    Key::new(0.650_000, 0.297_477),
    Key::new(0.700_000, 0.281_722),
    Key::new(0.750_000, 0.258_030),
    Key::new(0.800_000, 0.228_204),
    Key::new(0.850_000, 0.194_260),
    Key::new(0.900_000, 0.159_235),
    Key::new(0.950_000, 0.129_106),
    Key::new(1.000_000, 0.115_706),
];
pub(super) const HEAVY_KNEE: [Key; 21] = [
    Key::new(0.000_000, 0.188_757),
    Key::new(0.050_000, 0.193_120),
    Key::new(0.100_000, 0.204_183),
    Key::new(0.150_000, 0.218_890),
    Key::new(0.200_000, 0.234_642),
    Key::new(0.250_000, 0.249_427),
    Key::new(0.300_000, 0.261_662),
    Key::new(0.350_000, 0.269_991),
    Key::new(0.400_000, 0.273_087),
    Key::new(0.450_000, 0.317_058),
    Key::new(0.500_000, 0.397_740),
    Key::new(0.550_000, 0.465_502),
    Key::new(0.600_000, 0.493_519),
    Key::new(0.650_000, 0.484_154),
    Key::new(0.700_000, 0.458_658),
    Key::new(0.750_000, 0.420_264),
    Key::new(0.800_000, 0.371_858),
    Key::new(0.850_000, 0.316_685),
    Key::new(0.900_000, 0.259_680),
    Key::new(0.950_000, 0.210_597),
    Key::new(1.000_000, 0.188_757),
];
pub(super) const HEAVY_ANKLE: [Key; 21] = [
    Key::new(0.000_000, -0.253_263),
    Key::new(0.050_000, -0.259_112),
    Key::new(0.100_000, -0.273_942),
    Key::new(0.150_000, -0.293_654),
    Key::new(0.200_000, -0.314_762),
    Key::new(0.250_000, -0.334_567),
    Key::new(0.300_000, -0.350_954),
    Key::new(0.350_000, -0.362_108),
    Key::new(0.400_000, -0.366_253),
    Key::new(0.450_000, -0.425_097),
    Key::new(0.500_000, -0.532_897),
    Key::new(0.550_000, -0.623_206),
    Key::new(0.600_000, -0.660_467),
    Key::new(0.650_000, -0.648_019),
    Key::new(0.700_000, -0.614_096),
    Key::new(0.750_000, -0.562_942),
    Key::new(0.800_000, -0.498_343),
    Key::new(0.850_000, -0.424_598),
    Key::new(0.900_000, -0.348_300),
    Key::new(0.950_000, -0.282_540),
    Key::new(1.000_000, -0.253_263),
];
const HEAVY_LEAN: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.400_000, -0.350_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.600_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HEAVY_NOD: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.400_000, -0.250_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.200_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HEAVY_ARM: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.400_000, 0.820_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.200_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HEAVY_SPLAY: [Key; 4] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.400_000, -0.100_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, -0.120_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_SPLAY),
];
const HEAVY_ELBOW: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.400_000, 0.620_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];
const HEAVY_WRIST: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.400_000, 0.400_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, -0.200_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HEAVY_TAIL: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.400_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, -0.200_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];

pub(super) const MELEE_HEAVY_CURVES: [Keyed; 17] = [
    (Param::SpineBend, &HEAVY_LEAN),
    (Param::HeadNod, &HEAVY_NOD),
    (Param::ShoulderSwing(Side::Left), &HEAVY_ARM),
    (Param::ShoulderSwing(Side::Right), &HEAVY_ARM),
    (Param::ShoulderSplay(Side::Left), &HEAVY_SPLAY),
    (Param::ShoulderSplay(Side::Right), &HEAVY_SPLAY),
    (Param::ElbowBend(Side::Left), &HEAVY_ELBOW),
    (Param::ElbowBend(Side::Right), &HEAVY_ELBOW),
    (Param::WristAngle(Side::Left), &HEAVY_WRIST),
    (Param::WristAngle(Side::Right), &HEAVY_WRIST),
    (Param::HipSwing(Side::Left), &HEAVY_HIP),
    (Param::HipSwing(Side::Right), &HEAVY_HIP),
    (Param::KneeBend(Side::Left), &HEAVY_KNEE),
    (Param::KneeBend(Side::Right), &HEAVY_KNEE),
    (Param::AnkleAngle(Side::Left), &HEAVY_ANKLE),
    (Param::AnkleAngle(Side::Right), &HEAVY_ANKLE),
    (Param::TailLift, &HEAVY_TAIL),
];

/// Raising the bow and drawing the string back to the face, then holding at
/// full draw. Upper body only.
pub(super) const DRAW: Action = Action::new(0.60, 0.80, [0.45, 0.10, 0.05]);

const DRAW_TWIST: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, -0.350_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, -0.350_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, -0.350_000),
];
const DRAW_TILT: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.100_000),
];
const DRAW_LOOK: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.350_000),
];
const DRAW_ARM_LEFT: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.780_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.790_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.790_000),
];
const DRAW_SPLAY_LEFT: [Key; 4] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.120_000),
];
const DRAW_ELBOW_LEFT: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.040_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.040_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.040_000),
];
const DRAW_ARM_RIGHT: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.550_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.620_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.620_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.620_000),
];
const DRAW_SPLAY_RIGHT: [Key; 5] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.150_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.320_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.340_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.340_000),
];
const DRAW_ELBOW_RIGHT: [Key; 5] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.450_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.840_000).eased(Easing::EaseInOut),
    Key::new(0.800_000, 0.860_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.860_000),
];

pub(super) const DRAW_CURVES: [Keyed; 9] = [
    (Param::SpineTwist, &DRAW_TWIST),
    (Param::SpineTilt, &DRAW_TILT),
    (Param::HeadTurn, &DRAW_LOOK),
    (Param::ShoulderSwing(Side::Left), &DRAW_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &DRAW_ARM_RIGHT),
    (Param::ShoulderSplay(Side::Left), &DRAW_SPLAY_LEFT),
    (Param::ShoulderSplay(Side::Right), &DRAW_SPLAY_RIGHT),
    (Param::ElbowBend(Side::Left), &DRAW_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &DRAW_ELBOW_RIGHT),
];

/// Releasing the string from full draw, and lowering the bow. Begins exactly
/// where the draw holds, so the one follows the other with nothing to fade
/// across.
pub(super) const LOOSE: Action = Action::new(0.15, 0.35, [0.04, 0.08, 0.30]);

/// The string leaving the fingers, when the arrow does.
pub(super) const LOOSE_EVENTS: [Event; 1] = [Event::new("loose", LOOSE.active)];

const LOOSE_TWIST: [Key; 4] = [
    Key::new(0.000_000, -0.350_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, -0.350_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LOOSE_TILT: [Key; 4] = [
    Key::new(0.000_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.080_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LOOSE_LOOK: [Key; 4] = [
    Key::new(0.000_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LOOSE_ARM_LEFT: [Key; 4] = [
    Key::new(0.000_000, 0.790_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.790_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.720_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LOOSE_SPLAY_LEFT: [Key; 4] = [
    Key::new(0.000_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_SPLAY),
];
const LOOSE_ELBOW_LEFT: [Key; 4] = [
    Key::new(0.000_000, 0.040_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.040_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.060_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];
const LOOSE_ARM_RIGHT: [Key; 4] = [
    Key::new(0.000_000, 0.620_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.620_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.480_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const LOOSE_SPLAY_RIGHT: [Key; 4] = [
    Key::new(0.000_000, 0.340_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.340_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.580_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_SPLAY),
];
const LOOSE_ELBOW_RIGHT: [Key; 4] = [
    Key::new(0.000_000, 0.860_000).eased(Easing::EaseInOut),
    Key::new(0.150_000, 0.860_000).eased(Easing::EaseInOut),
    Key::new(0.350_000, 0.520_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];

pub(super) const LOOSE_CURVES: [Keyed; 9] = [
    (Param::SpineTwist, &LOOSE_TWIST),
    (Param::SpineTilt, &LOOSE_TILT),
    (Param::HeadTurn, &LOOSE_LOOK),
    (Param::ShoulderSwing(Side::Left), &LOOSE_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &LOOSE_ARM_RIGHT),
    (Param::ShoulderSplay(Side::Left), &LOOSE_SPLAY_LEFT),
    (Param::ShoulderSplay(Side::Right), &LOOSE_SPLAY_RIGHT),
    (Param::ElbowBend(Side::Left), &LOOSE_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &LOOSE_ELBOW_RIGHT),
];

/// Gathering power at the chest and thrusting it forward. Upper body only.
pub(super) const CAST: Action = Action::new(0.45, 0.62, [0.35, 0.10, 0.30]);

/// The thrust beginning, when the spell leaves the hands.
pub(super) const CAST_EVENTS: [Event; 1] = [Event::new("cast_release", CAST.active)];

const CAST_LEAN: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.180_000).eased(Easing::EaseInOut),
    Key::new(0.620_000, 0.280_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const CAST_NOD: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, 0.150_000).eased(Easing::EaseInOut),
    Key::new(0.620_000, -0.050_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const CAST_ARM: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, 0.320_000).eased(Easing::EaseInOut),
    Key::new(0.620_000, 0.720_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const CAST_SPLAY: [Key; 4] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(0.620_000, 0.060_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_SPLAY),
];
const CAST_ELBOW: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.450_000, 0.780_000).eased(Easing::EaseInOut),
    Key::new(0.620_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];
const CAST_WRIST: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(0.620_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];

pub(super) const CAST_CURVES: [Keyed; 10] = [
    (Param::SpineBend, &CAST_LEAN),
    (Param::HeadNod, &CAST_NOD),
    (Param::ShoulderSwing(Side::Left), &CAST_ARM),
    (Param::ShoulderSwing(Side::Right), &CAST_ARM),
    (Param::ShoulderSplay(Side::Left), &CAST_SPLAY),
    (Param::ShoulderSplay(Side::Right), &CAST_SPLAY),
    (Param::ElbowBend(Side::Left), &CAST_ELBOW),
    (Param::ElbowBend(Side::Right), &CAST_ELBOW),
    (Param::WristAngle(Side::Left), &CAST_WRIST),
    (Param::WristAngle(Side::Right), &CAST_WRIST),
];

/// Arms held out toward a sustained spell, pulsing with it. A cycle rather
/// than an action: it lasts as long as the channel does.
pub(super) const CHANNEL_SECONDS: f64 = 1.6;

const CHANNEL_LEAN: [Key; 9] = [
    Key::new(0.000_000, 0.140_000),
    Key::new(0.125_000, 0.134_142),
    Key::new(0.250_000, 0.120_000),
    Key::new(0.375_000, 0.105_858),
    Key::new(0.500_000, 0.100_000),
    Key::new(0.625_000, 0.105_858),
    Key::new(0.750_000, 0.120_000),
    Key::new(0.875_000, 0.134_142),
    Key::new(1.000_000, 0.140_000),
];
const CHANNEL_ARM: [Key; 9] = [
    Key::new(0.000_000, 0.650_000),
    Key::new(0.125_000, 0.641_213),
    Key::new(0.250_000, 0.620_000),
    Key::new(0.375_000, 0.598_787),
    Key::new(0.500_000, 0.590_000),
    Key::new(0.625_000, 0.598_787),
    Key::new(0.750_000, 0.620_000),
    Key::new(0.875_000, 0.641_213),
    Key::new(1.000_000, 0.650_000),
];
const CHANNEL_SPLAY: [Key; 9] = [
    Key::new(0.000_000, 0.140_000),
    Key::new(0.125_000, 0.182_426),
    Key::new(0.250_000, 0.200_000),
    Key::new(0.375_000, 0.182_426),
    Key::new(0.500_000, 0.140_000),
    Key::new(0.625_000, 0.097_574),
    Key::new(0.750_000, 0.080_000),
    Key::new(0.875_000, 0.097_574),
    Key::new(1.000_000, 0.140_000),
];
const CHANNEL_ELBOW: [Key; 9] = [
    Key::new(0.000_000, 0.220_000),
    Key::new(0.125_000, 0.208_284),
    Key::new(0.250_000, 0.180_000),
    Key::new(0.375_000, 0.151_716),
    Key::new(0.500_000, 0.140_000),
    Key::new(0.625_000, 0.151_716),
    Key::new(0.750_000, 0.180_000),
    Key::new(0.875_000, 0.208_284),
    Key::new(1.000_000, 0.220_000),
];
const CHANNEL_WRIST: [Key; 9] = [
    Key::new(0.000_000, 0.250_000),
    Key::new(0.125_000, 0.320_711),
    Key::new(0.250_000, 0.350_000),
    Key::new(0.375_000, 0.320_711),
    Key::new(0.500_000, 0.250_000),
    Key::new(0.625_000, 0.179_289),
    Key::new(0.750_000, 0.150_000),
    Key::new(0.875_000, 0.179_289),
    Key::new(1.000_000, 0.250_000),
];
const CHANNEL_NOD: [Key; 1] = [Key::new(0.0, 0.05)];

pub(super) const CHANNEL_CURVES: [Keyed; 10] = [
    (Param::SpineBend, &CHANNEL_LEAN),
    (Param::HeadNod, &CHANNEL_NOD),
    (Param::ShoulderSwing(Side::Left), &CHANNEL_ARM),
    (Param::ShoulderSwing(Side::Right), &CHANNEL_ARM),
    (Param::ShoulderSplay(Side::Left), &CHANNEL_SPLAY),
    (Param::ShoulderSplay(Side::Right), &CHANNEL_SPLAY),
    (Param::ElbowBend(Side::Left), &CHANNEL_ELBOW),
    (Param::ElbowBend(Side::Right), &CHANNEL_ELBOW),
    (Param::WristAngle(Side::Left), &CHANNEL_WRIST),
    (Param::WristAngle(Side::Right), &CHANNEL_WRIST),
];

/// A flinch: the trunk and head snap back and the arms come up to guard.
/// Upper body only, so a figure struck while running keeps its legs.
pub(super) const HIT: Action = Action::new(0.20, 0.45, [0.04, 0.10, 0.26]);

const HIT_LEAN: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.200_000, -0.400_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.350_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HIT_TWIST: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.200_000, 0.120_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, 0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HIT_NOD: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.200_000, -0.450_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.350_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HIT_ARM: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.200_000, 0.380_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, 0.320_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const HIT_SPLAY: [Key; 4] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.200_000, -0.120_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.100_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_SPLAY),
];
const HIT_ELBOW: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.200_000, 0.580_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, 0.500_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];

pub(super) const HIT_CURVES: [Keyed; 9] = [
    (Param::SpineBend, &HIT_LEAN),
    (Param::SpineTwist, &HIT_TWIST),
    (Param::HeadNod, &HIT_NOD),
    (Param::ShoulderSwing(Side::Left), &HIT_ARM),
    (Param::ShoulderSwing(Side::Right), &HIT_ARM),
    (Param::ShoulderSplay(Side::Left), &HIT_SPLAY),
    (Param::ShoulderSplay(Side::Right), &HIT_SPLAY),
    (Param::ElbowBend(Side::Left), &HIT_ELBOW),
    (Param::ElbowBend(Side::Right), &HIT_ELBOW),
];

/// Knocked back: the right foot steps back to catch the body, the trunk rocks
/// over it, and the foot steps back in. The left stays planted throughout.
pub(super) const STAGGER: Action = Action::new(0.25, 0.60, [0.08, 0.25, 0.40]);

/// The catching foot coming down.
pub(super) const STAGGER_EVENTS: [Event; 1] = [Event::new("footstep", STAGGER_CATCH)];

/// How deep the staggering body stands into its legs: dropping onto the
/// catching foot and rising off it again.
pub(super) const fn stagger_depth(phase: f64) -> f64 {
    let (start, end) = (STAGGER.active, STAGGER.recovery);
    if phase <= start {
        IDLE_CROUCH
    } else if phase <= STAGGER_CATCH {
        IDLE_CROUCH
            + (STAGGER_DROP - IDLE_CROUCH) * smooth((phase - start) / (STAGGER_CATCH - start))
    } else if phase <= end {
        STAGGER_DROP
    } else {
        STAGGER_DROP - (STAGGER_DROP - IDLE_CROUCH) * smooth((phase - end) / (1.0 - end))
    }
}

/// Where the catching foot comes down, and how far the body drops onto it.
pub(super) const STAGGER_CATCH: f64 = 0.45;
const STAGGER_DROP: f64 = 3.0;

/// The stagger's root height, keyed where its legs are.
pub(super) const STAGGER_LIFT: [Key; STAGGER_HIP_LEFT.len()] =
    sunk(phases(&STAGGER_HIP_LEFT), Sink::Stagger);

/// The planted leg, re-solved as the body sinks and rises over it.
pub(super) const STAGGER_HIP_LEFT: [Key; 41] = [
    Key::new(0.000_000, 0.115_706),
    Key::new(0.025_000, 0.115_706),
    Key::new(0.050_000, 0.115_706),
    Key::new(0.075_000, 0.115_706),
    Key::new(0.100_000, 0.115_706),
    Key::new(0.125_000, 0.115_706),
    Key::new(0.150_000, 0.115_706),
    Key::new(0.175_000, 0.115_706),
    Key::new(0.200_000, 0.115_706),
    Key::new(0.225_000, 0.115_706),
    Key::new(0.250_000, 0.115_706),
    Key::new(0.275_000, 0.119_396),
    Key::new(0.300_000, 0.128_629),
    Key::new(0.325_000, 0.140_675),
    Key::new(0.350_000, 0.153_357),
    Key::new(0.375_000, 0.165_099),
    Key::new(0.400_000, 0.174_724),
    Key::new(0.425_000, 0.181_237),
    Key::new(0.450_000, 0.183_650),
    Key::new(0.475_000, 0.183_650),
    Key::new(0.500_000, 0.183_650),
    Key::new(0.525_000, 0.183_650),
    Key::new(0.550_000, 0.183_650),
    Key::new(0.575_000, 0.183_650),
    Key::new(0.600_000, 0.183_650),
    Key::new(0.625_000, 0.183_022),
    Key::new(0.650_000, 0.181_237),
    Key::new(0.675_000, 0.178_429),
    Key::new(0.700_000, 0.174_724),
    Key::new(0.725_000, 0.170_240),
    Key::new(0.750_000, 0.165_099),
    Key::new(0.775_000, 0.159_425),
    Key::new(0.800_000, 0.153_357),
    Key::new(0.825_000, 0.147_047),
    Key::new(0.850_000, 0.140_675),
    Key::new(0.875_000, 0.134_452),
    Key::new(0.900_000, 0.128_629),
    Key::new(0.925_000, 0.123_498),
    Key::new(0.950_000, 0.119_396),
    Key::new(0.975_000, 0.116_682),
    Key::new(1.000_000, 0.115_706),
];
pub(super) const STAGGER_KNEE_LEFT: [Key; 41] = [
    Key::new(0.000_000, 0.188_757),
    Key::new(0.025_000, 0.188_757),
    Key::new(0.050_000, 0.188_757),
    Key::new(0.075_000, 0.188_757),
    Key::new(0.100_000, 0.188_757),
    Key::new(0.125_000, 0.188_757),
    Key::new(0.150_000, 0.188_757),
    Key::new(0.175_000, 0.188_757),
    Key::new(0.200_000, 0.188_757),
    Key::new(0.225_000, 0.188_757),
    Key::new(0.250_000, 0.188_757),
    Key::new(0.275_000, 0.194_772),
    Key::new(0.300_000, 0.209_820),
    Key::new(0.325_000, 0.229_449),
    Key::new(0.350_000, 0.250_107),
    Key::new(0.375_000, 0.269_229),
    Key::new(0.400_000, 0.284_897),
    Key::new(0.425_000, 0.295_497),
    Key::new(0.450_000, 0.299_423),
    Key::new(0.475_000, 0.299_423),
    Key::new(0.500_000, 0.299_423),
    Key::new(0.525_000, 0.299_423),
    Key::new(0.550_000, 0.299_423),
    Key::new(0.575_000, 0.299_423),
    Key::new(0.600_000, 0.299_423),
    Key::new(0.625_000, 0.298_402),
    Key::new(0.650_000, 0.295_497),
    Key::new(0.675_000, 0.290_928),
    Key::new(0.700_000, 0.284_897),
    Key::new(0.725_000, 0.277_599),
    Key::new(0.750_000, 0.269_229),
    Key::new(0.775_000, 0.259_991),
    Key::new(0.800_000, 0.250_107),
    Key::new(0.825_000, 0.239_830),
    Key::new(0.850_000, 0.229_449),
    Key::new(0.875_000, 0.219_310),
    Key::new(0.900_000, 0.209_820),
    Key::new(0.925_000, 0.201_459),
    Key::new(0.950_000, 0.194_772),
    Key::new(0.975_000, 0.190_347),
    Key::new(1.000_000, 0.188_757),
];
pub(super) const STAGGER_ANKLE_LEFT: [Key; 41] = [
    Key::new(0.000_000, -0.316_579),
    Key::new(0.025_000, -0.316_579),
    Key::new(0.050_000, -0.316_579),
    Key::new(0.075_000, -0.316_579),
    Key::new(0.100_000, -0.316_579),
    Key::new(0.125_000, -0.316_579),
    Key::new(0.150_000, -0.316_579),
    Key::new(0.175_000, -0.316_579),
    Key::new(0.200_000, -0.316_579),
    Key::new(0.225_000, -0.316_579),
    Key::new(0.250_000, -0.316_579),
    Key::new(0.275_000, -0.326_659),
    Key::new(0.300_000, -0.351_872),
    Key::new(0.325_000, -0.384_755),
    Key::new(0.350_000, -0.419_349),
    Key::new(0.375_000, -0.451_359),
    Key::new(0.400_000, -0.477_579),
    Key::new(0.425_000, -0.495_313),
    Key::new(0.450_000, -0.501_881),
    Key::new(0.475_000, -0.501_881),
    Key::new(0.500_000, -0.501_881),
    Key::new(0.525_000, -0.501_881),
    Key::new(0.550_000, -0.501_881),
    Key::new(0.575_000, -0.501_881),
    Key::new(0.600_000, -0.501_881),
    Key::new(0.625_000, -0.500_172),
    Key::new(0.650_000, -0.495_313),
    Key::new(0.675_000, -0.487_669),
    Key::new(0.700_000, -0.477_579),
    Key::new(0.725_000, -0.465_367),
    Key::new(0.750_000, -0.451_359),
    Key::new(0.775_000, -0.435_895),
    Key::new(0.800_000, -0.419_349),
    Key::new(0.825_000, -0.402_140),
    Key::new(0.850_000, -0.384_755),
    Key::new(0.875_000, -0.367_771),
    Key::new(0.900_000, -0.351_872),
    Key::new(0.925_000, -0.337_863),
    Key::new(0.950_000, -0.326_659),
    Key::new(0.975_000, -0.319_244),
    Key::new(1.000_000, -0.316_579),
];

/// The stepping leg: back, down, and in again.
pub(super) const STAGGER_HIP_RIGHT: [Key; 41] = [
    Key::new(0.000_000, 0.115_706),
    Key::new(0.025_000, 0.115_706),
    Key::new(0.050_000, 0.115_706),
    Key::new(0.075_000, 0.115_706),
    Key::new(0.100_000, 0.115_706),
    Key::new(0.125_000, 0.115_706),
    Key::new(0.150_000, 0.115_706),
    Key::new(0.175_000, 0.115_706),
    Key::new(0.200_000, 0.115_706),
    Key::new(0.225_000, 0.115_706),
    Key::new(0.250_000, 0.115_706),
    Key::new(0.275_000, 0.144_148),
    Key::new(0.300_000, 0.189_126),
    Key::new(0.325_000, 0.212_116),
    Key::new(0.350_000, 0.201_044),
    Key::new(0.375_000, 0.158_160),
    Key::new(0.400_000, 0.093_901),
    Key::new(0.425_000, 0.026_026),
    Key::new(0.450_000, -0.028_040),
    Key::new(0.475_000, -0.028_040),
    Key::new(0.500_000, -0.028_040),
    Key::new(0.525_000, -0.028_040),
    Key::new(0.550_000, -0.028_040),
    Key::new(0.575_000, -0.028_040),
    Key::new(0.600_000, -0.028_040),
    Key::new(0.625_000, -0.030_783),
    Key::new(0.650_000, -0.038_649),
    Key::new(0.675_000, -0.051_228),
    Key::new(0.700_000, -0.068_254),
    Key::new(0.725_000, 0.031_129),
    Key::new(0.750_000, 0.120_093),
    Key::new(0.775_000, 0.184_448),
    Key::new(0.800_000, 0.200_698),
    Key::new(0.825_000, 0.171_169),
    Key::new(0.850_000, 0.140_675),
    Key::new(0.875_000, 0.134_452),
    Key::new(0.900_000, 0.128_629),
    Key::new(0.925_000, 0.123_498),
    Key::new(0.950_000, 0.119_396),
    Key::new(0.975_000, 0.116_682),
    Key::new(1.000_000, 0.115_706),
];
pub(super) const STAGGER_KNEE_RIGHT: [Key; 41] = [
    Key::new(0.000_000, 0.188_757),
    Key::new(0.025_000, 0.188_757),
    Key::new(0.050_000, 0.188_757),
    Key::new(0.075_000, 0.188_757),
    Key::new(0.100_000, 0.188_757),
    Key::new(0.125_000, 0.188_757),
    Key::new(0.150_000, 0.188_757),
    Key::new(0.175_000, 0.188_757),
    Key::new(0.200_000, 0.188_757),
    Key::new(0.225_000, 0.188_757),
    Key::new(0.250_000, 0.188_757),
    Key::new(0.275_000, 0.244_442),
    Key::new(0.300_000, 0.343_749),
    Key::new(0.325_000, 0.420_751),
    Key::new(0.350_000, 0.448_859),
    Key::new(0.375_000, 0.421_123),
    Key::new(0.400_000, 0.346_791),
    Key::new(0.425_000, 0.253_640),
    Key::new(0.450_000, 0.203_440),
    Key::new(0.475_000, 0.203_440),
    Key::new(0.500_000, 0.203_440),
    Key::new(0.525_000, 0.203_440),
    Key::new(0.550_000, 0.203_440),
    Key::new(0.575_000, 0.203_440),
    Key::new(0.600_000, 0.203_440),
    Key::new(0.625_000, 0.202_004),
    Key::new(0.650_000, 0.197_889),
    Key::new(0.675_000, 0.191_323),
    Key::new(0.700_000, 0.182_460),
    Key::new(0.725_000, 0.255_059),
    Key::new(0.750_000, 0.367_340),
    Key::new(0.775_000, 0.419_525),
    Key::new(0.800_000, 0.387_368),
    Key::new(0.825_000, 0.295_523),
    Key::new(0.850_000, 0.229_449),
    Key::new(0.875_000, 0.219_310),
    Key::new(0.900_000, 0.209_820),
    Key::new(0.925_000, 0.201_459),
    Key::new(0.950_000, 0.194_772),
    Key::new(0.975_000, 0.190_347),
    Key::new(1.000_000, 0.188_757),
];
pub(super) const STAGGER_ANKLE_RIGHT: [Key; 41] = [
    Key::new(0.000_000, -0.221_605),
    Key::new(0.025_000, -0.221_605),
    Key::new(0.050_000, -0.221_605),
    Key::new(0.075_000, -0.221_605),
    Key::new(0.100_000, -0.221_605),
    Key::new(0.125_000, -0.221_605),
    Key::new(0.150_000, -0.221_605),
    Key::new(0.175_000, -0.221_605),
    Key::new(0.200_000, -0.221_605),
    Key::new(0.225_000, -0.221_605),
    Key::new(0.250_000, -0.221_605),
    Key::new(0.275_000, -0.298_365),
    Key::new(0.300_000, -0.446_747),
    Key::new(0.325_000, -0.585_569),
    Key::new(0.350_000, -0.675_175),
    Key::new(0.375_000, -0.694_376),
    Key::new(0.400_000, -0.644_496),
    Key::new(0.425_000, -0.556_686),
    Key::new(0.450_000, -0.505_080),
    Key::new(0.475_000, -0.505_080),
    Key::new(0.500_000, -0.505_080),
    Key::new(0.525_000, -0.505_080),
    Key::new(0.550_000, -0.505_080),
    Key::new(0.575_000, -0.505_080),
    Key::new(0.600_000, -0.505_080),
    Key::new(0.625_000, -0.503_279),
    Key::new(0.650_000, -0.498_124),
    Key::new(0.675_000, -0.489_912),
    Key::new(0.700_000, -0.478_857),
    Key::new(0.725_000, -0.549_884),
    Key::new(0.750_000, -0.641_430),
    Key::new(0.775_000, -0.637_965),
    Key::new(0.800_000, -0.528_287),
    Key::new(0.825_000, -0.366_918),
    Key::new(0.850_000, -0.269_328),
    Key::new(0.875_000, -0.257_440),
    Key::new(0.900_000, -0.246_311),
    Key::new(0.925_000, -0.236_504),
    Key::new(0.950_000, -0.228_661),
    Key::new(0.975_000, -0.223_471),
    Key::new(1.000_000, -0.221_605),
];
const STAGGER_LEAN: [Key; 5] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.250_000, -0.480_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.300_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, -0.150_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const STAGGER_TILT: [Key; 3] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, -0.150_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const STAGGER_NOD: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.250_000, -0.400_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, -0.150_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const STAGGER_ARM: [Key; 4] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.250_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.150_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];
const STAGGER_SPLAY: [Key; 5] = [
    Key::new(0.000_000, REST_SPLAY).eased(Easing::EaseInOut),
    Key::new(0.250_000, 0.500_000).eased(Easing::EaseInOut),
    Key::new(0.450_000, 0.620_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.350_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_SPLAY),
];
const STAGGER_ELBOW: [Key; 4] = [
    Key::new(0.000_000, REST_ELBOW).eased(Easing::EaseInOut),
    Key::new(0.250_000, 0.300_000).eased(Easing::EaseInOut),
    Key::new(0.600_000, 0.200_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, REST_ELBOW),
];
const STAGGER_TAIL: [Key; 3] = [
    Key::new(0.000_000, 0.000_000).eased(Easing::EaseInOut),
    Key::new(0.250_000, 0.450_000).eased(Easing::EaseInOut),
    Key::new(1.000_000, 0.000_000),
];

pub(super) const STAGGER_CURVES: [Keyed; 16] = [
    (Param::SpineBend, &STAGGER_LEAN),
    (Param::SpineTilt, &STAGGER_TILT),
    (Param::HeadNod, &STAGGER_NOD),
    (Param::ShoulderSwing(Side::Left), &STAGGER_ARM),
    (Param::ShoulderSwing(Side::Right), &STAGGER_ARM),
    (Param::ShoulderSplay(Side::Left), &STAGGER_SPLAY),
    (Param::ShoulderSplay(Side::Right), &STAGGER_SPLAY),
    (Param::ElbowBend(Side::Left), &STAGGER_ELBOW),
    (Param::ElbowBend(Side::Right), &STAGGER_ELBOW),
    (Param::HipSwing(Side::Left), &STAGGER_HIP_LEFT),
    (Param::HipSwing(Side::Right), &STAGGER_HIP_RIGHT),
    (Param::KneeBend(Side::Left), &STAGGER_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &STAGGER_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &STAGGER_ANKLE_LEFT),
    (Param::AnkleAngle(Side::Right), &STAGGER_ANKLE_RIGHT),
    (Param::TailLift, &STAGGER_TAIL),
];
