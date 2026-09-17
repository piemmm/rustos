//! Cinder's body: the mascot palette, the skeleton, and the pose it is placed
//! by.
//!
//! The skeleton is **data**. A pose is a handful of numbers — gait phase,
//! crouch, head yaw and pitch, ear angle, tail sway, eye state — and building
//! the part list is a pure function of the two. There is no per-direction
//! variant and no branch on where the creature faces: the camera's depth sort
//! turns the body round on its own.
//!
//! # What he is
//!
//! A cat, from the mascot sheet: long legs, a narrow wedge of a skull, tall
//! leaned ears set close, and large tall eyes. His coat is **rust orange** all
//! over — the dark mass on screen is the garment he wears, never his body. The
//! cowl at his neck and the cape over his shoulders and flanks are modelled as
//! cloth in their own slate tones, draped over a body that has its own colour
//! underneath.
//!
//! # Why the limbs connect
//!
//! Each limb is a tapered outline whose origin sits *inside* a shoulder or
//! haunch mass that belongs to the trunk, and it pivots about that origin as
//! it swings. A limb that merely translated below the body — which is all a
//! stack of discs can do — reads as a detached peg.

use alloc::vec::Vec;

use tairix_raster::Color;
use tairix_util::mathf;

use crate::project::{project, Body, Ground};
use crate::shape::{Placed, Shape};

/// Cinder's coat and costume, from the mascot sheet's palette swatch.
///
/// Named tones rather than raw literals at each use, so the character's
/// colours are stated once and a re-tint is a change to this block. The slate
/// tones are named for what they *are* — a neutral dark — because the cowl,
/// the cape and the pen's own furniture all legitimately draw in them.
pub mod palette {
    use tairix_raster::Color;

    /// The brightest rust orange: the lit crown, ruff, and tail bands.
    pub const FUR_BRIGHT: Color = Color::rgb(0xF0, 0x78, 0x2A);
    /// Mid rust orange: the body of the coat.
    pub const FUR_MID: Color = Color::rgb(0xD2, 0x4B, 0x17);
    /// Deep rust: the shaded underside, the limbs, and the tail's dark bands.
    pub const FUR_DEEP: Color = Color::rgb(0xBB, 0x3A, 0x10);
    /// Cream: the face mask, muzzle, chest blaze, brows, and inner ears.
    pub const CREAM: Color = Color::rgb(0xF2, 0xE1, 0xC8);
    /// Warm shadow on the cream, where the mask turns away.
    pub const CREAM_SHADE: Color = Color::rgb(0xBD, 0xA1, 0x8A);
    /// The lit face of the cloth, and the pen's bowl.
    pub const SLATE: Color = Color::rgb(0x2A, 0x2B, 0x2F);
    /// Cloth in shadow: the cowl's folds and the cape's flanks.
    pub const SLATE_SHADE: Color = Color::rgb(0x1A, 0x1C, 0x1F);
    /// The deepest fold, and the pen's shadowed furniture.
    pub const SLATE_DEEP: Color = Color::rgb(0x0E, 0x10, 0x13);
    /// The paws, dark enough to read as pads against the rust of the limb.
    pub const PAW: Color = Color::rgb(0x5A, 0x2A, 0x14);
    /// The amber of the eyes.
    pub const EYE_AMBER: Color = Color::rgb(0xE0, 0x8A, 0x22);
    /// The lash line around each eye. Without it the amber sits on cream and
    /// the eyes stop reading at a companion's size.
    pub const EYE_RING: Color = Color::rgb(0x3A, 0x1E, 0x12);
    /// The pupil and the nose.
    pub const INK: Color = Color::rgb(0x0A, 0x0A, 0x0C);
    /// A catchlight, so the eyes read as wet rather than as flat discs.
    pub const CATCHLIGHT: Color = Color::rgb(0xFF, 0xF4, 0xE6);
}

/// How tall Cinder stands, in pixels, at the reference density.
///
/// The creature is authored at this height and every offset below is in the
/// same pixels, so scaling the character is scaling one number. The skeleton
/// is held to it by its own test rather than the two being asserted equal by
/// hand.
pub const STANDING_HEIGHT: f64 = 76.0;

/// The ground radius the contact shadow is sized from.
pub const BODY_RADIUS: f64 = 22.0;

/// Whether the eyes are open, half, or shut.
///
/// A closed set: blinking, dozing, and sleeping are the three states the face
/// actually has, and a continuous "openness" would let the face rest at
/// values that read as neither.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Eyes {
    /// Wide open.
    Open,
    /// Half closed: drowsy, or mid-blink.
    Half,
    /// Shut: asleep, or the closed frame of a blink.
    Shut,
}

impl Eyes {
    /// How far a lid has come down, as the fraction of full height the eye
    /// still shows.
    ///
    /// A lid *flattens* an eye rather than shrinking it, and a shut eye keeps
    /// a sliver so it reads as a closed lid instead of leaving a hole in the
    /// mask.
    #[must_use]
    pub const fn openness(self) -> f64 {
        match self {
            Self::Open => 1.0,
            Self::Half => 0.45,
            Self::Shut => 0.08,
        }
    }
}

/// Everything that varies about Cinder from one frame to the next.
///
/// Parameters, not code paths: the skeleton reads them and places itself, so a
/// new behaviour is new numbers rather than a new drawing routine.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Pose {
    /// Where the feet are.
    pub at: Ground,
    /// Which way the body faces, in radians (zero is screen-right).
    pub heading: f64,
    /// How far through the leg cycle, in radians.
    pub gait_phase: f64,
    /// How far the body is lowered towards the floor, `0.0` standing and
    /// `1.0` flat.
    pub crouch: f64,
    /// Height above the floor: a jump, or a perch's rise.
    pub lift: f64,
    /// How far the head is turned from the body, in radians.
    pub head_yaw: f64,
    /// How far the head is tipped, in radians; positive looks up.
    pub head_pitch: f64,
    /// How far the ears are laid back, `0.0` pricked and `1.0` flat.
    pub ear_flop: f64,
    /// How far the tail is swung from the body's axis, in radians.
    pub tail_sway: f64,
    /// How broadly the mouth curves up, `0.0` level and `1.0` a full grin.
    pub smile: f64,
    /// Eye state.
    pub eyes: Eyes,
}

impl Default for Pose {
    fn default() -> Self {
        Self {
            at: Ground::new(0.0, 0.0),
            heading: 0.0,
            gait_phase: 0.0,
            crouch: 0.0,
            lift: 0.0,
            head_yaw: 0.0,
            head_pitch: 0.0,
            ear_flop: 0.0,
            tail_sway: 0.0,
            smile: 0.5,
            eyes: Eyes::Open,
        }
    }
}

/// One entry of the skeleton, before the pose places it.
struct Part {
    /// Where it sits in the body frame when the creature stands square.
    rest: Body,
    /// What it is drawn as.
    shape: Shape,
    /// Its colour.
    color: Color,
    /// Which of the pose's parameters move it.
    attach: Attach,
    /// Whether a lid flattens it.
    ///
    /// A flag on the part rather than an index range over the table: a range
    /// silently stops meaning the eyes the moment the skeleton is reordered.
    blinks: bool,
}

/// What a part is carried by.
///
/// A closed set, because a part belongs to exactly one thing that moves — the
/// body, the head, one limb, or the tail — and a part carried by two would
/// need a blend rule nothing here wants.
#[derive(Copy, Clone)]
enum Attach {
    /// Rides the body: bobs with the gait and drops with the crouch.
    Trunk,
    /// Rides the head: additionally turns and tips.
    Head,
    /// One limb, indexed so each takes its own quarter of the gait cycle. The
    /// part's offset from that limb's joint rotates with the swing, and its
    /// outline turns with it, so the limb pivots where it meets the body.
    Limb(u8),
    /// The tail, which swings and lags.
    Tail(f64),
    /// One point of the mouth, at the given offset from its centre. Rides the
    /// head, and curves up with the pose's smile.
    Smile(f64),
}

/// Where each limb meets the body, indexed as [`Attach::Limb`] is.
///
/// The joints are the pivots the limbs rotate about *and* where the trunk's
/// shoulder and haunch masses sit, so the two cannot drift apart and leave a
/// limb growing out of thin air.
const LIMB_JOINTS: [Body; 4] = [
    Body::new(-13.0, 9.5, 33.0),
    Body::new(-13.0, -9.5, 33.0),
    Body::new(11.0, 8.5, 31.0),
    Body::new(11.0, -8.5, 31.0),
];

/// How many parts the creature is built from.
///
/// Stated so a reader knows the cost without counting the table, and so a
/// test can hold the skeleton to it.
pub const PART_COUNT: usize = 64;

/// The skeleton: every part in the body frame, authored far-to-near within its
/// own group. The camera's depth sort decides the final order, and the table's
/// own order breaks a tie — so a piece of piping authored before the flap it
/// edges stays behind it however the creature turns.
///
/// The face carries the detail, because that is what makes the mascot
/// recognisable at a companion's size: a cream mask for the eyes to read
/// against, a lash line around each, tall amber irises with tall pupils and
/// two catchlights, cream brows, and a small cat muzzle over a curving mouth.
#[rustfmt::skip]
const SKELETON: [Part; PART_COUNT] = {
    use palette::{
        CATCHLIGHT, CREAM, CREAM_SHADE, EYE_AMBER, EYE_RING, FUR_BRIGHT, FUR_DEEP, FUR_MID, INK,
        PAW, SLATE, SLATE_DEEP, SLATE_SHADE,
    };

    /// A part that neither blinks nor needs naming twice.
    const fn part(rest: Body, shape: Shape, color: Color, attach: Attach) -> Part {
        Part { rest, shape, color, attach, blinks: false }
    }

    /// An eye part, which a lid flattens.
    const fn eye(rest: Body, shape: Shape, color: Color) -> Part {
        Part { rest, shape, color, attach: Attach::Head, blinks: true }
    }

    /// A rounded volume.
    const fn mass(rx: f64, ry: f64, square: f64) -> Shape {
        Shape::Mass { rx, ry, square }
    }

    /// A soft splat of fur.
    const fn fur(radius: f64) -> Shape {
        Shape::Fur { radius }
    }

    [
        // --- the tail: a thick plume in the sheet's alternating bands, curling
        // --- up behind him and forward over his back ---
        part(Body::new(-19.0, 0.0, 29.0), fur( 9.5), FUR_DEEP,   Attach::Tail(0.0)),
        part(Body::new(-25.0, 0.0, 34.0), fur(10.0), FUR_BRIGHT, Attach::Tail(0.2)),
        part(Body::new(-30.0, 0.0, 40.0), fur(10.5), FUR_DEEP,   Attach::Tail(0.4)),
        part(Body::new(-33.0, 0.0, 47.0), fur(10.0), FUR_BRIGHT, Attach::Tail(0.6)),
        part(Body::new(-33.0, 0.0, 54.0), fur( 9.0), FUR_DEEP,   Attach::Tail(0.8)),
        part(Body::new(-30.0, 0.0, 60.0), fur( 7.5), FUR_BRIGHT, Attach::Tail(1.0)),
        part(Body::new(-26.0, 0.0, 64.0), fur( 5.8), FUR_DEEP,   Attach::Tail(1.15)),

        // --- the four limbs: a tapered shank pivoting in its joint, with a
        // --- dark pad at the foot. The joint masses below are what they grow
        // --- out of ---
        part(LIMB_JOINTS[0], Shape::Limb { length: 31.0, top: 5.4, foot: 3.6 }, FUR_DEEP, Attach::Limb(0)),
        part(Body::new(-13.0,  9.5,  3.4), mass(3.9, 2.7, 0.40), PAW, Attach::Limb(0)),
        part(LIMB_JOINTS[1], Shape::Limb { length: 31.0, top: 5.4, foot: 3.6 }, FUR_DEEP, Attach::Limb(1)),
        part(Body::new(-13.0, -9.5,  3.4), mass(3.9, 2.7, 0.40), PAW, Attach::Limb(1)),
        part(LIMB_JOINTS[2], Shape::Limb { length: 29.0, top: 4.6, foot: 3.4 }, FUR_DEEP, Attach::Limb(2)),
        part(Body::new( 11.0,  8.5,  3.4), mass(3.6, 2.5, 0.40), PAW, Attach::Limb(2)),
        part(LIMB_JOINTS[3], Shape::Limb { length: 29.0, top: 4.6, foot: 3.4 }, FUR_DEEP, Attach::Limb(3)),
        part(Body::new( 11.0, -8.5,  3.4), mass(3.6, 2.5, 0.40), PAW, Attach::Limb(3)),

        // --- the trunk: a chain of rounded masses along the spine, squarer at
        // --- the rump and fuller at the chest, all of it rust ---
        part(Body::new(-15.0, 0.0, 31.0), mass(13.5, 13.0, 0.30), FUR_MID,   Attach::Trunk),
        part(Body::new( -4.0, 0.0, 31.0), mass(12.5, 12.0, 0.24), FUR_MID,   Attach::Trunk),
        part(Body::new(  6.0, 0.0, 33.0), mass(11.5, 12.5, 0.20), FUR_BRIGHT, Attach::Trunk),
        part(Body::new( -2.0, 0.0, 23.0), mass(11.5,  6.5, 0.36), FUR_DEEP,  Attach::Trunk),

        // --- the joint caps: part of the body, not of the limb, so a swinging
        // --- shank never opens a gap at the shoulder ---
        part(LIMB_JOINTS[0], mass(6.4, 6.0, 0.30), FUR_MID,  Attach::Trunk),
        part(LIMB_JOINTS[1], mass(6.4, 6.0, 0.30), FUR_MID,  Attach::Trunk),
        part(LIMB_JOINTS[2], mass(5.6, 5.6, 0.28), FUR_BRIGHT, Attach::Trunk),
        part(LIMB_JOINTS[3], mass(5.6, 5.6, 0.28), FUR_BRIGHT, Attach::Trunk),

        // --- the cape: a panel gathered over the shoulders and one down each
        // --- flank, with the sheet's flap and its orange piping ---
        part(Body::new( -4.0,   0.0, 38.0), Shape::Drape { rx: 12.5, ry: 16.0, folds: 3 }, SLATE_SHADE, Attach::Trunk),
        part(Body::new( -3.0,  10.5, 34.0), Shape::Drape { rx:  7.0, ry: 15.0, folds: 2 }, SLATE_SHADE, Attach::Trunk),
        part(Body::new( -3.0, -10.5, 34.0), Shape::Drape { rx:  7.0, ry: 15.0, folds: 2 }, SLATE_SHADE, Attach::Trunk),
        part(Body::new( -7.0,  11.0, 27.0), mass(5.0, 5.8, 0.78), FUR_MID,   Attach::Trunk),
        part(Body::new( -7.0,  11.4, 27.0), mass(4.3, 5.1, 0.78), SLATE_DEEP, Attach::Trunk),
        part(Body::new(  4.0,   0.0, 39.0), Shape::Drape { rx: 12.0, ry: 12.0, folds: 3 }, SLATE,      Attach::Trunk),

        // --- the cream chest blaze, over the cape's front edge so it reads as
        // --- fur showing above a garment ---
        part(Body::new( 13.0, 0.0, 32.0), mass(4.8, 7.6, 0.22), CREAM, Attach::Trunk),

        // --- the cowl: a wrap at the neck with a fold hanging at the front ---
        part(Body::new(  9.0, 0.0, 45.0), mass(9.0, 4.6, 0.46), SLATE_SHADE, Attach::Trunk),
        part(Body::new( 13.0, 0.0, 44.0), mass(9.5, 5.0, 0.46), SLATE,       Attach::Trunk),
        part(Body::new( 15.0, 0.0, 40.0), Shape::Drape { rx: 6.5, ry: 7.0, folds: 2 }, SLATE_SHADE, Attach::Trunk),

        // --- the skull: a narrow wedge, and the layered crown the sheet draws
        // --- as a rosette of guard hairs ---
        part(Body::new( 16.0,  0.0, 52.0), mass(12.0, 11.2, 0.28), FUR_MID,    Attach::Head),
        part(Body::new( 20.0,  0.0, 51.0), mass(10.5, 10.0, 0.20), FUR_BRIGHT, Attach::Head),
        part(Body::new( 17.0,  0.0, 58.0), fur(8.6),               FUR_BRIGHT, Attach::Head),

        // --- the ears: tall wedges leaning apart, set close on the crown ---
        part(Body::new( 14.0,  8.0, 62.0), Shape::Ear { half_width: 6.4, height: 13.0, lean:  3.4 }, FUR_MID,    Attach::Head),
        part(Body::new( 14.0, -8.0, 62.0), Shape::Ear { half_width: 6.4, height: 13.0, lean: -3.4 }, FUR_MID,    Attach::Head),
        part(Body::new( 15.2,  8.2, 62.6), Shape::Ear { half_width: 3.9, height:  9.2, lean:  2.6 }, CREAM_SHADE, Attach::Head),
        part(Body::new( 15.4,  8.2, 62.4), Shape::Ear { half_width: 3.2, height:  8.0, lean:  2.4 }, CREAM,      Attach::Head),
        part(Body::new( 15.2, -8.2, 62.6), Shape::Ear { half_width: 3.9, height:  9.2, lean: -2.6 }, CREAM_SHADE, Attach::Head),
        part(Body::new( 15.4, -8.2, 62.4), Shape::Ear { half_width: 3.2, height:  8.0, lean: -2.4 }, CREAM,      Attach::Head),

        // --- the cream mask: spiky cheek ruffs the eyes read against ---
        part(Body::new( 17.0,  9.8, 47.0), fur(6.8), CREAM,       Attach::Head),
        part(Body::new( 17.0, -9.8, 47.0), fur(6.8), CREAM,       Attach::Head),
        part(Body::new( 19.5,  0.0, 46.0), mass(7.6, 5.4, 0.24), CREAM_SHADE, Attach::Head),

        // --- the brow markings the sheet gives every expression ---
        part(Body::new( 19.0,  7.0, 59.0), mass(3.4, 2.0, 0.30), CREAM, Attach::Head),
        part(Body::new( 19.0, -7.0, 59.0), mass(3.4, 2.0, 0.30), CREAM, Attach::Head),

        // --- the eyes: tall and large, a lash line, a tall pupil, and two
        // --- catchlights from one light so the pair reads as wet ---
        eye(Body::new( 21.5,  6.0, 53.0), mass(5.6, 7.2, 0.30), EYE_RING),
        eye(Body::new( 21.5, -6.0, 53.0), mass(5.6, 7.2, 0.30), EYE_RING),
        eye(Body::new( 22.6,  6.0, 53.0), mass(4.6, 6.2, 0.28), EYE_AMBER),
        eye(Body::new( 22.6, -6.0, 53.0), mass(4.6, 6.2, 0.28), EYE_AMBER),
        eye(Body::new( 23.4,  6.0, 52.6), mass(2.5, 4.4, 0.22), INK),
        eye(Body::new( 23.4, -6.0, 52.6), mass(2.5, 4.4, 0.22), INK),
        eye(Body::new( 24.0,  7.2, 55.2), mass(1.8, 2.2, 0.10), CATCHLIGHT),
        eye(Body::new( 24.0, -4.8, 55.2), mass(1.8, 2.2, 0.10), CATCHLIGHT),
        eye(Body::new( 24.0,  4.6, 50.8), mass(0.95, 1.05, 0.0), CATCHLIGHT),
        eye(Body::new( 24.0, -7.4, 50.8), mass(0.95, 1.05, 0.0), CATCHLIGHT),

        // --- the muzzle: small and short, as a cat's is ---
        part(Body::new( 24.0, 0.0, 44.0), mass(6.0, 4.6, 0.26), CREAM, Attach::Head),
        part(Body::new( 26.8, 0.0, 46.4), mass(2.2, 1.7, 0.16), INK,   Attach::Head),

        // --- the mouth: five ink points whose corners lift with the smile, so
        // --- it curves rather than sliding upward ---
        part(Body::new( 26.6,  0.0, 42.6), mass(0.9, 0.9, 0.0), INK, Attach::Smile( 0.0)),
        part(Body::new( 26.1,  2.1, 42.3), mass(0.9, 0.9, 0.0), INK, Attach::Smile( 1.0)),
        part(Body::new( 26.1, -2.1, 42.3), mass(0.9, 0.9, 0.0), INK, Attach::Smile(-1.0)),
        part(Body::new( 25.3,  3.7, 43.1), mass(0.8, 0.8, 0.0), INK, Attach::Smile( 2.0)),
        part(Body::new( 25.3, -3.7, 43.1), mass(0.8, 0.8, 0.0), INK, Attach::Smile(-2.0)),
    ]
};

/// Build the part list for `pose`, sorted far-to-near so painting them in
/// order composites the creature correctly.
///
/// The sort is what makes the turnaround free: facing away, the face parts
/// fall behind the skull and are covered; facing the camera they come forward.
/// Nothing here asks which way the creature is pointing.
#[must_use]
pub fn parts(pose: &Pose) -> Vec<Placed> {
    let mut placed = Vec::with_capacity(PART_COUNT);
    // The whole body drops with the crouch and rises with a jump.
    let base_lift = pose.lift - pose.crouch * CROUCH_DROP;
    // A gentle bob so a walk has weight; twice the leg rate, as a real gait's
    // centre of mass rises on each step rather than each stride.
    let bob = mathf::sin(pose.gait_phase * 2.0) * BOB_HEIGHT * (1.0 - pose.crouch);

    for (index, part) in SKELETON.iter().enumerate() {
        let (offset, turn) = place(part, pose, base_lift + bob);
        let projected = project(pose.at, pose.heading, offset);
        placed.push((
            projected.depth,
            index,
            Placed {
                x: projected.x,
                y: projected.y,
                turn,
                shape: lidded(part, pose),
                color: part.color,
                // The index is the part's identity, so its fur ripples the
                // same way every frame.
                seed: u16::try_from(index).unwrap_or(0).wrapping_mul(2_654),
            },
        ));
    }
    // Far-first, on the camera's own depth rather than on the screen row: a
    // raised part draws higher without becoming further away, so sorting on
    // the row would put a lifted paw behind the body it belongs to.
    //
    // `total_cmp` orders every float, so no comparison can be indeterminate;
    // the table index breaks a tie, so two parts at the same depth paint in
    // the order they were authored — which is how a piece of piping stays
    // behind the flap it edges.
    placed.sort_by(|(a_depth, a_index, _), (b_depth, b_index, _)| {
        a_depth
            .total_cmp(b_depth)
            .then_with(|| a_index.cmp(b_index))
    });
    placed.into_iter().map(|(_, _, part)| part).collect()
}

/// How far the body drops at full crouch.
const CROUCH_DROP: f64 = 13.0;

/// How far the body bobs at a full-speed walk.
const BOB_HEIGHT: f64 = 2.2;

/// How far a limb swings fore-and-aft at a full-speed walk, in radians.
const LIMB_SWING: f64 = 0.30;

/// How far a limb's joint lifts at the top of its swing.
const LIMB_LIFT: f64 = 3.0;

/// Place `part` for `pose`, with the whole-body `lift` already resolved,
/// answering both where it goes and how far its outline turns.
fn place(part: &Part, pose: &Pose, lift: f64) -> (Body, f64) {
    match part.attach {
        Attach::Trunk => (
            Body::new(part.rest.forward, part.rest.side, part.rest.up + lift),
            0.0,
        ),
        Attach::Head => (place_on_head(&part.rest, pose, lift), 0.0),
        Attach::Limb(index) => place_on_limb(part, pose, lift, index),
        Attach::Smile(offset) => {
            // The corners lift further than the centre, which is what makes a
            // curve rather than a mouth that merely moves up.
            let lifted = Body::new(
                part.rest.forward,
                part.rest.side,
                part.rest.up + pose.smile * mathf::fabs(offset) * SMILE_LIFT,
            );
            (place_on_head(&lifted, pose, lift), 0.0)
        }
        Attach::Tail(along) => {
            // The tail lags the body's turn, more towards the tip, so it
            // trails rather than swinging rigidly with the hips.
            let sway = pose.tail_sway * (TAIL_LAG_BASE + along * TAIL_LAG_TIP);
            let (sin, cos) = (mathf::sin(sway), mathf::cos(sway));
            let forward = part.rest.forward * cos - part.rest.side * sin;
            let side = part.rest.forward * sin + part.rest.side * cos;
            (Body::new(forward, side, part.rest.up + lift), 0.0)
        }
    }
}

/// Place a limb part: rotate its offset from the limb's joint by the swing,
/// and turn its outline by the same swing as the camera sees it.
///
/// Diagonal pairs move together, which is what a four-legged walk looks like;
/// the quarter-cycle offsets give the lateral sequence.
fn place_on_limb(part: &Part, pose: &Pose, lift: f64, index: u8) -> (Body, f64) {
    let joint = LIMB_JOINTS
        .get(usize::from(index))
        .copied()
        .unwrap_or(Body::new(0.0, 0.0, 0.0));
    let phase = pose.gait_phase + f64::from(index) * core::f64::consts::FRAC_PI_2;
    let swing = mathf::sin(phase) * LIMB_SWING;
    let raise = mathf::fmax(0.0, mathf::cos(phase)) * LIMB_LIFT;
    let (sin, cos) = (mathf::sin(swing), mathf::cos(swing));
    // In the creature's own fore-and-aft plane, about the joint.
    let along = part.rest.forward - joint.forward;
    let above = part.rest.up - joint.up;
    let forward = joint.forward + along * cos + above * sin;
    let up = joint.up - along * sin + above * cos;
    // The screen rotation the same swing amounts to. Facing the camera the
    // limb swings in depth, `cos(heading)` goes to zero, and the outline
    // correctly stops turning — so the billboard follows the projection rather
    // than needing a branch on which way he faces.
    let turn = -swing * mathf::cos(pose.heading);
    (
        Body::new(forward, part.rest.side, up + raise + lift * LIMB_LIFT_SHARE),
        turn,
    )
}

/// Place a head part at `rest` for `pose`, with the whole-body `lift`
/// resolved.
///
/// The head turns about the neck rather than about the body's centre, so a
/// look to the side swings the muzzle and not the whole skull sideways.
fn place_on_head(rest: &Body, pose: &Pose, lift: f64) -> Body {
    let local_forward = rest.forward - NECK_FORWARD;
    let (sin, cos) = (mathf::sin(pose.head_yaw), mathf::cos(pose.head_yaw));
    let forward = local_forward * cos - rest.side * sin;
    let side = local_forward * sin + rest.side * cos;
    // A tip is a rotation in the vertical plane; small angles, so the head
    // does not detach from the neck.
    let up_local = rest.up - NECK_UP;
    let (psin, pcos) = (mathf::sin(pose.head_pitch), mathf::cos(pose.head_pitch));
    let tipped_forward = forward * pcos - up_local * psin;
    let tipped_up = forward * psin + up_local * pcos;
    let ear = if rest.up > EAR_HEIGHT {
        pose.ear_flop * EAR_FOLD
    } else {
        0.0
    };
    Body::new(
        tipped_forward + NECK_FORWARD,
        side,
        tipped_up + NECK_UP + lift - ear,
    )
}

/// How far the mouth's corners lift at a full grin.
const SMILE_LIFT: f64 = 1.5;

/// How much of a whole-body lift a limb follows: less than the trunk, so the
/// legs trail on a jump instead of rising rigidly with the body.
const LIMB_LIFT_SHARE: f64 = 0.55;

/// Where the neck sits, forward of the body's centre.
const NECK_FORWARD: f64 = 12.0;

/// Where the neck sits above the floor.
const NECK_UP: f64 = 45.0;

/// Above this height a head part is an ear, and folds with `ear_flop`.
const EAR_HEIGHT: f64 = 60.0;

/// How far an ear drops when fully laid back.
const EAR_FOLD: f64 = 9.0;

/// How much of the tail's sway the root takes.
const TAIL_LAG_BASE: f64 = 0.35;

/// How much more of the sway the tip takes than the root.
const TAIL_LAG_TIP: f64 = 0.9;

/// `part`'s shape with the pose's lid applied, which flattens an eye rather
/// than shrinking it.
fn lidded(part: &Part, pose: &Pose) -> Shape {
    if !part.blinks {
        return part.shape;
    }
    let openness = pose.eyes.openness();
    match part.shape {
        Shape::Mass { rx, ry, square } => Shape::Mass {
            rx,
            ry: ry * openness,
            square,
        },
        other => other,
    }
}

/// The furthest any part reaches from the creature's ground point, in pixels.
///
/// An upper bound read off the skeleton, so the surface the companion asks for
/// can be *checked* against the body it has to hold rather than the two being
/// asserted to agree by hand. A limb's bound is its length plus its widest
/// half-width, which over-estimates the diagonal it actually reaches — safe
/// for a bound, and held to the outlines by this module's tests.
#[must_use]
pub fn reach() -> f64 {
    let mut furthest: f64 = 0.0;
    for part in &SKELETON {
        let radius = mathf::hypot(
            mathf::hypot(part.rest.forward, part.rest.side),
            part.rest.up,
        ) + part.shape.reach();
        furthest = mathf::fmax(furthest, radius);
    }
    furthest
}

#[cfg(test)]
#[path = "cinder_tests.rs"]
mod tests;
