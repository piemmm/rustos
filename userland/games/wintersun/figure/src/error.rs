//! What this crate refuses, and why.

use core::fmt;

/// A refusal from the figure engine.
///
/// Every variant is a rig or a request that could only draw something nobody
/// authored, so each is refused at construction rather than drawn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FigureError {
    /// More joints than a rig holds.
    TooManyJoints,
    /// More parts than a rig holds.
    TooManyParts,
    /// A second mount for a socket that already has one.
    DuplicateSocket,
    /// More fitted equipment than one figure carries.
    TooMuchEquipment,
    /// A joint naming a parent the rig does not have.
    NoSuchParent,
    /// A joint naming a parent that does not precede it.
    ///
    /// Parents first makes the hierarchy a forest — no cycle can be spelled —
    /// and lets a posture resolve in one forward pass.
    ParentNotEarlier,
    /// A rotation limit whose lower bound exceeds its upper one.
    LimitInverted,
    /// A rotation limit reaching beyond a full turn, or not a finite number.
    LimitUnreal,
    /// A rotation limit that excludes zero, so the joint cannot rest.
    LimitExcludesRest,
    /// A rotation outside the limit its joint documents.
    RotationOutsideLimit,
    /// A part, socket or posture entry naming a joint the rig does not have.
    NoSuchJoint,
    /// A joint that bears a child but carries no part of its own.
    ///
    /// The mass a limb grows out of: without it a swinging shank opens a gap
    /// where it meets the body.
    BearingJointWithoutMass,
    /// A child joint whose origin lies beyond everything its parent draws,
    /// which is a gap in the silhouette however the figure is posed.
    JointBeyondParentReach,
    /// A geometry value that is not a finite number.
    ///
    /// Refused because it would otherwise reach the depth sort and the
    /// rasteriser as a position no pixel corresponds to.
    GeometryUnreal,
    /// Equipment naming a socket this rig does not offer.
    NoSuchSocket,
    /// A scale that is not a finite positive number.
    ScaleUnreal,
    /// A pose value outside the range its parameter is authored in.
    ParamOutsideRange,
    /// Two drives on one joint axis, which could sum past its limit.
    DriveCollision,
    /// A curve with no keys, which has no value to sample.
    CurveEmpty,
    /// Keys that do not strictly ascend by phase.
    KeysNotAscending,
    /// A phase outside a clip's own `0..=1`.
    PhaseOutsideClip,
    /// A second curve on a parameter that already has one.
    DuplicateCurve,
    /// Clip events that do not ascend by phase, so a lap would report them
    /// out of the order they happen in.
    EventsNotAscending,
    /// One event name used twice in a clip, so a consumer could not tell
    /// which moment fired.
    DuplicateEventName,
    /// A clip duration that is not a finite positive number.
    DurationUnreal,
    /// An elapsed time or step that is not a finite non-negative number.
    ElapsedUnreal,
    /// A blend weight that is not a finite non-negative number.
    WeightUnreal,
    /// A transition machine with no states.
    NoStates,
    /// More states than a machine holds.
    TooManyStates,
    /// A state naming a clip the machine does not have.
    NoSuchClip,
    /// An initial state or edge end the machine does not have.
    NoSuchState,
    /// No edge from the current state to the one asked for.
    NoSuchEdge,
    /// A cross-fade that is not a finite positive number of seconds.
    BlendNotPositive,
    /// Edges out of order or repeated.
    EdgesNotAscending,
    /// A state nothing leads to, which a figure could never enter.
    StateUnreachable,
    /// A springing value or velocity that is not finite.
    MotionUnreal,
    /// A spring rate or damping ratio that is not a real one.
    SpringUnreal,
    /// A gait stride that is not a finite positive distance.
    StrideUnreal,
    /// A travelled distance that is not finite.
    DistanceUnreal,
    /// Leg joints that are not a hip-knee-ankle chain, so no two-bone solve
    /// could reach a foot along them.
    LegNotAChain,
    /// A terrain height that is not finite.
    GroundUnreal,
    /// A root-motion value outside the fraction of the move it must be.
    TravelOutsideRange,
    /// A root-motion curve that does not run from none of the move to all of
    /// it, so playing the clip out would not deliver what was authorised.
    TravelNotSpanning,
    /// A root-height value beyond the legs' own fold range either way.
    LiftOutsideRange,
    /// A root-height curve that does not cover the whole cycle, leaving the
    /// height at some phase to be guessed from an end key.
    LiftNotSpanning,
    /// A looping clip whose root-height curve ends somewhere other than it
    /// began, so the figure would hitch vertically on every lap.
    LiftNotClosing,
    /// A light that is not a real bearing and elevation above the horizon.
    LightUnreal,
    /// Too few samples to measure a cycle over.
    SamplesTooFew,
}

impl fmt::Display for FigureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooManyJoints => "more joints than a rig holds",
            Self::TooManyParts => "more parts than a rig holds",
            Self::DuplicateSocket => "socket already mounted",
            Self::TooMuchEquipment => "more equipment than a figure carries",
            Self::NoSuchParent => "no such parent joint",
            Self::ParentNotEarlier => "parent joint does not precede its child",
            Self::LimitInverted => "rotation limit inverted",
            Self::LimitUnreal => "rotation limit beyond a full turn or not finite",
            Self::LimitExcludesRest => "rotation limit excludes rest",
            Self::RotationOutsideLimit => "rotation outside its joint's limit",
            Self::NoSuchJoint => "no such joint",
            Self::BearingJointWithoutMass => "joint bears a child but carries no mass",
            Self::JointBeyondParentReach => "child joint beyond its parent's reach",
            Self::GeometryUnreal => "geometry value not finite",
            Self::NoSuchSocket => "rig offers no such socket",
            Self::ScaleUnreal => "scale not a finite positive number",
            Self::ParamOutsideRange => "pose value outside its parameter's range",
            Self::DriveCollision => "two drives on one joint axis",
            Self::CurveEmpty => "curve has no keys",
            Self::KeysNotAscending => "curve keys do not ascend by phase",
            Self::PhaseOutsideClip => "phase outside a clip's own range",
            Self::DuplicateCurve => "parameter already has a curve",
            Self::EventsNotAscending => "clip events do not ascend by phase",
            Self::DuplicateEventName => "event name used twice in one clip",
            Self::DurationUnreal => "clip duration not a finite positive number",
            Self::ElapsedUnreal => "elapsed time not a finite non-negative number",
            Self::WeightUnreal => "blend weight not a finite non-negative number",
            Self::NoStates => "transition machine has no states",
            Self::TooManyStates => "more states than a machine holds",
            Self::NoSuchClip => "no such clip",
            Self::NoSuchState => "no such state",
            Self::NoSuchEdge => "no edge to the state asked for",
            Self::BlendNotPositive => "cross-fade not a finite positive number of seconds",
            Self::EdgesNotAscending => "edges out of order or repeated",
            Self::StateUnreachable => "state unreachable from the initial one",
            Self::MotionUnreal => "springing value or velocity not finite",
            Self::SpringUnreal => "spring rate or damping ratio not a real one",
            Self::StrideUnreal => "gait stride not a finite positive distance",
            Self::DistanceUnreal => "travelled distance not finite",
            Self::LegNotAChain => "leg joints are not a hip-knee-ankle chain",
            Self::GroundUnreal => "terrain height not finite",
            Self::TravelOutsideRange => "root-motion value outside 0..=1",
            Self::TravelNotSpanning => "root-motion curve does not span the move",
            Self::LiftOutsideRange => "root height outside the legs' fold range",
            Self::LiftNotSpanning => "root-height curve does not span the cycle",
            Self::LiftNotClosing => "root-height curve does not close on itself",
            Self::LightUnreal => "light not a real bearing and elevation",
            Self::SamplesTooFew => "too few samples to measure a cycle over",
        };
        f.write_str(text)
    }
}
