//! What a blend weighs, and what it leaves alone.

use tairix_util::mathf;

use super::Blend;
use crate::clip::{Clip, Curve, Key, Loop};
use crate::error::FigureError;
use crate::pose::{Mask, Param, Pose};
use crate::socket::Side;

const SLACK: f64 = 1e-9;

const NOD: Param = Param::HeadNod;
const BEND: Param = Param::SpineBend;
const KNEE: Param = Param::KneeBend(Side::Left);

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

fn only(param: Param) -> Mask {
    Mask::NONE.with(param)
}

#[test]
fn a_blend_of_nothing_is_rest() {
    let blend = Blend::EMPTY;
    assert!(blend.is_empty());
    let pose = blend.resolve().expect("rest is in range");
    for param in Param::ALL {
        assert!(close(pose.get(param), 0.0));
    }
}

#[test]
fn one_pose_at_any_weight_comes_back_unchanged() {
    let source = Pose::REST.with(NOD, 0.5).expect("in range");
    for weight in [0.25, 1.0, 7.0] {
        let mut blend = Blend::EMPTY;
        blend
            .add_pose(&source, only(NOD), weight)
            .expect("a real weight");
        let pose = blend.resolve().expect("in range");
        assert!(
            close(pose.get(NOD), 0.5),
            "weight {weight} changed the value"
        );
    }
}

#[test]
fn equal_weights_average() {
    let low = Pose::REST.with(NOD, -1.0).expect("in range");
    let high = Pose::REST.with(NOD, 1.0).expect("in range");
    let mut blend = Blend::EMPTY;
    blend.add_pose(&low, only(NOD), 1.0).expect("a real weight");
    blend
        .add_pose(&high, only(NOD), 1.0)
        .expect("a real weight");
    assert!(close(blend.resolve().expect("in range").get(NOD), 0.0));
}

#[test]
fn unequal_weights_lean_toward_the_heavier() {
    let low = Pose::REST.with(NOD, 0.0).expect("in range");
    let high = Pose::REST.with(NOD, 1.0).expect("in range");
    let mut blend = Blend::EMPTY;
    blend.add_pose(&low, only(NOD), 1.0).expect("a real weight");
    blend
        .add_pose(&high, only(NOD), 3.0)
        .expect("a real weight");
    assert!(close(blend.resolve().expect("in range").get(NOD), 0.75));
}

#[test]
fn a_parameter_nothing_claimed_rests() {
    let source = Pose::REST.with(NOD, 1.0).expect("in range");
    let mut blend = Blend::EMPTY;
    blend
        .add_pose(&source, only(NOD), 1.0)
        .expect("a real weight");
    let pose = blend.resolve().expect("in range");
    assert!(close(pose.get(BEND), 0.0));
    assert_eq!(blend.written(), only(NOD));
}

#[test]
fn a_mask_keeps_a_pose_out_of_what_it_does_not_cover() {
    let source = Pose::REST
        .with(NOD, 1.0)
        .expect("in range")
        .with(BEND, 1.0)
        .expect("in range");
    let mut blend = Blend::EMPTY;
    blend
        .add_pose(&source, only(NOD), 1.0)
        .expect("a real weight");
    let pose = blend.resolve().expect("in range");
    assert!(close(pose.get(NOD), 1.0));
    assert!(
        close(pose.get(BEND), 0.0),
        "a masked-out parameter was written"
    );
}

#[test]
fn an_overlay_does_not_drag_what_it_does_not_cover() {
    // A "walk" over the whole body, and a "cast" claiming only the head.
    let walk = Pose::REST
        .with(NOD, 0.2)
        .expect("in range")
        .with(KNEE, 0.9)
        .expect("in range");
    let cast = Pose::REST.with(NOD, 1.0).expect("in range");

    let mut blend = Blend::EMPTY;
    blend
        .add_pose(&walk, Mask::ALL, 1.0)
        .expect("a real weight");
    blend
        .add_pose(&cast, only(NOD), 1.0)
        .expect("a real weight");

    let pose = blend.resolve().expect("in range");
    assert!(close(pose.get(NOD), 0.6), "the head did not take both");
    assert!(
        close(pose.get(KNEE), 0.9),
        "the legs were dragged by the overlay"
    );
}

#[test]
fn a_zero_weight_contributes_nothing_and_claims_nothing() {
    let source = Pose::REST.with(NOD, 1.0).expect("in range");
    let mut blend = Blend::EMPTY;
    blend
        .add_pose(&source, only(NOD), 0.0)
        .expect("zero is real");
    assert!(blend.is_empty());
    assert!(close(blend.resolve().expect("in range").get(NOD), 0.0));
}

#[test]
fn a_weight_that_is_not_a_real_number_is_refused() {
    let source = Pose::REST;
    for weight in [-1.0, f64::NAN, f64::INFINITY] {
        let mut blend = Blend::EMPTY;
        assert_eq!(
            blend.add_pose(&source, Mask::ALL, weight).err(),
            Some(FigureError::WeightUnreal),
            "a weight of {weight} was accepted"
        );
    }
}

#[test]
fn a_blend_of_values_inside_their_ranges_stays_inside_them() {
    let low = Pose::REST
        .with(NOD, -1.0)
        .expect("in range")
        .with(KNEE, 0.0)
        .expect("in range");
    let high = Pose::REST
        .with(NOD, 1.0)
        .expect("in range")
        .with(KNEE, 1.0)
        .expect("in range");

    for step in 0..=100 {
        let weight = f64::from(step) / 100.0;
        let mut blend = Blend::EMPTY;
        blend
            .add_pose(&low, Mask::ALL, weight)
            .expect("a real weight");
        blend
            .add_pose(&high, Mask::ALL, 1.0 - weight)
            .expect("a real weight");
        let pose = blend
            .resolve()
            .expect("a mean of values in range is in range");
        for param in Param::ALL {
            assert!(
                param.range().holds(pose.get(param)),
                "{param:?} left its range at weight {weight}"
            );
        }
    }
}

#[test]
fn a_clip_blends_the_parameters_it_keys() {
    let knees = [Key::new(0.0, 0.0), Key::new(1.0, 1.0)];
    let curves = [Curve::new(KNEE, &knees).expect("a well-formed curve")];
    let clip = Clip::new(1.0, Loop::Hold, &curves, &[]).expect("a well-formed clip");

    let mut blend = Blend::EMPTY;
    blend.add_clip(clip, 0.5, 1.0).expect("a phase inside");
    let pose = blend.resolve().expect("in range");
    assert!(close(pose.get(KNEE), 0.5));
    assert_eq!(blend.written(), only(KNEE));
}

#[test]
fn two_clips_cross_fade() {
    let low = [Key::new(0.0, 0.0)];
    let high = [Key::new(0.0, 1.0)];
    let low_curves = [Curve::new(KNEE, &low).expect("a well-formed curve")];
    let high_curves = [Curve::new(KNEE, &high).expect("a well-formed curve")];
    let from = Clip::new(1.0, Loop::Hold, &low_curves, &[]).expect("a well-formed clip");
    let to = Clip::new(1.0, Loop::Hold, &high_curves, &[]).expect("a well-formed clip");

    let mut blend = Blend::EMPTY;
    blend.add_clip(from, 0.0, 0.25).expect("a phase inside");
    blend.add_clip(to, 0.0, 0.75).expect("a phase inside");
    assert!(close(blend.resolve().expect("in range").get(KNEE), 0.75));
}

#[test]
fn blending_a_clip_outside_its_phase_is_refused() {
    let clip = Clip::new(1.0, Loop::Hold, &[], &[]).expect("a well-formed clip");
    let mut blend = Blend::EMPTY;
    for phase in [-0.1, 1.1, f64::NAN] {
        assert_eq!(
            blend.add_clip(clip, phase, 1.0).err(),
            Some(FigureError::PhaseOutsideClip),
            "a phase of {phase} was accepted"
        );
    }
}

#[test]
fn a_clip_at_a_bad_weight_is_refused_before_its_phase_is_read() {
    let clip = Clip::new(1.0, Loop::Hold, &[], &[]).expect("a well-formed clip");
    let mut blend = Blend::EMPTY;
    assert_eq!(
        blend.add_clip(clip, 0.5, -1.0).err(),
        Some(FigureError::WeightUnreal)
    );
}

#[test]
fn the_default_blend_is_the_empty_one() {
    assert_eq!(Blend::default(), Blend::EMPTY);
}
