//! What a hanging element does when what carries it moves.

use tairix_util::mathf;

use super::Sway;
use crate::error::FigureError;
use crate::frame::Body;
use crate::humanoid::{self, Bone};
use crate::identity::Identity;
use crate::pose::{Param, Pose};
use crate::reference;
use crate::species::Species;
use crate::spring::Spring;

const GIVE: f64 = 0.02;
const LIMIT: f64 = 0.9;

fn sway() -> Sway {
    Sway::new(Spring::new(11.0, 0.45).expect("a real spring"), GIVE, LIMIT).expect("a real sway")
}

fn settle(sway: &mut Sway, acceleration: Body, wind: Body, steps: usize) {
    for _ in 0..steps {
        sway.advance(acceleration, wind, 1.0 / 60.0)
            .expect("it sways");
    }
}

#[test]
fn an_unreal_give_or_limit_is_refused() {
    let spring = Spring::new(11.0, 0.45).expect("a real spring");
    assert_eq!(
        Sway::new(spring, f64::NAN, LIMIT).map(|_| ()),
        Err(FigureError::GeometryUnreal)
    );
    for limit in [0.0, -0.5, f64::NAN, f64::INFINITY] {
        assert_eq!(
            Sway::new(spring, GIVE, limit).map(|_| ()),
            Err(FigureError::LimitUnreal),
            "a limit of {limit} must be refused"
        );
    }
}

#[test]
fn an_unreal_drive_or_step_is_refused() {
    let mut sway = sway();
    let bad = Body::new(f64::NAN, 0.0, 0.0);
    assert_eq!(
        sway.advance(bad, Body::ORIGIN, 0.1),
        Err(FigureError::GeometryUnreal)
    );
    assert_eq!(
        sway.advance(Body::ORIGIN, bad, 0.1),
        Err(FigureError::GeometryUnreal)
    );
    assert_eq!(
        sway.advance(Body::ORIGIN, Body::ORIGIN, -0.1),
        Err(FigureError::ElapsedUnreal)
    );
}

/// A figure moving steadily has its cloak hanging straight: it is the
/// *change* in motion that throws cloth, which is why the drive is
/// acceleration and not velocity.
#[test]
fn steady_motion_leaves_it_hanging() {
    let mut sway = sway();
    settle(&mut sway, Body::ORIGIN, Body::ORIGIN, 600);
    assert!(sway.settled());
    let turn = sway.turn();
    assert!(mathf::fabs(turn.pitch) < 1e-6 && mathf::fabs(turn.roll) < 1e-6);
}

/// The whole reason cloth reads as cloth: it is left behind by the
/// acceleration rather than rotating with the body.
#[test]
fn it_leans_against_an_acceleration_and_with_a_wind() {
    let mut forward = sway();
    settle(&mut forward, Body::new(20.0, 0.0, 0.0), Body::ORIGIN, 800);
    // Accelerating forward leaves the hem behind, which swings it backward.
    assert!(forward.turn().pitch > 0.0, "pitch {}", forward.turn().pitch);

    let mut blown = sway();
    settle(&mut blown, Body::ORIGIN, Body::new(20.0, 0.0, 0.0), 800);
    assert!(blown.turn().pitch < 0.0, "a wind blows it forward");

    let mut leftward = sway();
    settle(&mut leftward, Body::new(0.0, 20.0, 0.0), Body::ORIGIN, 800);
    assert!(leftward.turn().roll < 0.0, "roll {}", leftward.turn().roll);
}

/// A hem that can swing past its own mounting has come off, whatever the
/// input asked for.
#[test]
fn nothing_can_swing_it_past_its_limit() {
    let mut sway = sway();
    let mut step = 0;
    while step < 2_000 {
        // A drive that changes sign hard every few frames is the worst case
        // for a spring: it is always being thrown, never settling.
        let sign = if (step / 7) % 2 == 0 { 1.0 } else { -1.0 };
        sway.advance(
            Body::new(sign * 4_000.0, sign * -4_000.0, 0.0),
            Body::new(sign * 900.0, 0.0, 0.0),
            1.0 / 60.0,
        )
        .expect("it sways");
        let turn = sway.turn();
        assert!(
            mathf::fabs(turn.pitch) <= LIMIT + 1e-12 && mathf::fabs(turn.roll) <= LIMIT + 1e-12,
            "turn {turn:?} left the limit at step {step}"
        );
        assert!(turn.is_real(), "turn {turn:?} must stay a number");
        step += 1;
    }
}

/// Stability under a bounded input, which is what keeps a cloak from
/// exploding on a frame that arrived late.
#[test]
fn a_long_frame_does_not_make_it_flail() {
    let mut sway = sway();
    for seconds in [1.0 / 60.0, 0.5, 4.0, 1.0 / 240.0, 30.0] {
        sway.advance(Body::new(15.0, -8.0, 0.0), Body::ORIGIN, seconds)
            .expect("it sways");
        assert!(sway.turn().is_real());
    }
    settle(&mut sway, Body::ORIGIN, Body::ORIGIN, 1_200);
    assert!(sway.settled(), "it must come back to rest");
}

/// A tail hangs from a joint of its own, so the sway reaches it through the
/// pose: the overlay turns the tail joint by exactly the sway's lean, and
/// leaves every other joint where the clip put it.
#[test]
fn a_sway_turns_a_tail_through_its_parameters() {
    let identity = Identity::new(reference::spec(Species::Beastkin)).expect("a tailed figure");
    let rig = humanoid::rig(&identity).expect("it builds");
    let rigging = humanoid::rigging(&rig).expect("it binds");

    let mut tail = sway();
    settle(&mut tail, Body::new(4.0, -6.0, 0.0), Body::ORIGIN, 12);
    let lean = tail.turn();
    assert!(
        lean.pitch != 0.0 && lean.roll != 0.0,
        "the drive must lean it both ways"
    );

    let overlay = tail
        .overlay(&rigging, Param::TailLift, Param::TailSwing)
        .expect("a real lean");
    let posed = overlay.applied(&Pose::REST).expect("in range");
    let posture = rigging.posture(&posed).expect("posturable");
    let turned = posture
        .get(Bone::Tail.joint())
        .expect("every figure has a tail root");
    assert!(mathf::fabs(turned.pitch - lean.pitch) < 1e-12);
    assert!(mathf::fabs(turned.roll - lean.roll) < 1e-12);
    for bone in Bone::ALL.into_iter().filter(|bone| *bone != Bone::Tail) {
        assert_eq!(
            posture.get(bone.joint()),
            rigging
                .posture(&Pose::REST)
                .expect("rest")
                .get(bone.joint()),
            "the tail's sway moved {bone:?}"
        );
    }
}

/// A parameter that turns nothing on the rig adds nothing, rather than a
/// delta nothing could ever apply.
#[test]
fn a_sway_through_parameters_the_rig_does_not_drive_adds_nothing() {
    let identity = Identity::new(reference::spec(Species::Human)).expect("a real figure");
    let rig = humanoid::rig(&identity).expect("it builds");
    let drives: [crate::rigging::Drive; 0] = [];
    let bare = crate::rigging::Rigging::new(&rig, &drives).expect("an empty table binds");
    let mut hem = sway();
    settle(&mut hem, Body::new(5.0, 5.0, 0.0), Body::ORIGIN, 12);
    let overlay = hem
        .overlay(&bare, Param::TailLift, Param::TailSwing)
        .expect("a real lean");
    assert!(overlay.is_empty());
}
