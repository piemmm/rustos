//! Tests for the pose parameter vocabulary.

use tairix_util::mathf;

use super::*;
use crate::error::FigureError;
use crate::socket::Side;

#[test]
fn every_parameter_has_its_own_slot() {
    for (slot, param) in Param::ALL.iter().enumerate() {
        assert_eq!(param.index(), slot, "{param:?} is not at its own position");
    }
}

#[test]
fn the_table_is_exactly_as_long_as_it_claims() {
    assert_eq!(Param::ALL.len(), Param::COUNT);
}

#[test]
fn a_sided_parameter_is_two_distinct_parameters() {
    assert_ne!(
        Param::ElbowBend(Side::Left).index(),
        Param::ElbowBend(Side::Right).index()
    );
}

#[test]
fn only_the_joints_that_fold_one_way_are_unit_ranged() {
    for param in Param::ALL {
        let expected = matches!(param, Param::ElbowBend(_) | Param::KneeBend(_));
        assert_eq!(
            param.range() == Range::Unit,
            expected,
            "{param:?} has the wrong range"
        );
    }
}

#[test]
fn a_range_holds_its_own_ends_and_nothing_past_them() {
    for range in [Range::Unit, Range::Signed] {
        assert!(range.holds(range.min()));
        assert!(range.holds(range.max()));
        assert!(!range.holds(range.max() + 0.5));
        assert!(!range.holds(range.min() - 0.5));
    }
}

#[test]
fn a_range_holds_no_value_that_is_not_a_number() {
    for range in [Range::Unit, Range::Signed] {
        assert!(!range.holds(f64::NAN));
        assert!(!range.holds(f64::INFINITY));
        assert!(!range.holds(f64::NEG_INFINITY));
    }
}

#[test]
fn a_unit_range_excludes_the_negative_half() {
    assert!(!Range::Unit.holds(-0.5));
    assert!(Range::Signed.holds(-0.5));
}

#[test]
fn clamping_moves_only_what_is_outside() {
    assert!((Range::Signed.clamp(0.25) - 0.25).abs() < f64::EPSILON);
    assert!((Range::Signed.clamp(1.5) - 1.0).abs() < f64::EPSILON);
    assert!((Range::Unit.clamp(-0.5) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn rest_is_every_parameter_at_zero() {
    for param in Param::ALL {
        assert!(Pose::REST.get(param).abs() < f64::EPSILON);
    }
}

#[test]
fn a_set_parameter_reads_back() {
    let pose = Pose::REST
        .with(Param::KneeBend(Side::Left), 0.75)
        .expect("in range");
    assert!((pose.get(Param::KneeBend(Side::Left)) - 0.75).abs() < f64::EPSILON);
}

#[test]
fn setting_one_parameter_leaves_the_others_alone() {
    let pose = Pose::REST.with(Param::HeadNod, 0.5).expect("in range");
    for param in Param::ALL {
        if param != Param::HeadNod {
            assert!(pose.get(param).abs() < f64::EPSILON, "{param:?} moved");
        }
    }
}

#[test]
fn a_value_outside_its_range_is_refused() {
    let mut pose = Pose::REST;
    assert_eq!(
        pose.set(Param::HeadNod, 1.5),
        Err(FigureError::ParamOutsideRange)
    );
    assert_eq!(
        pose.set(Param::KneeBend(Side::Left), -0.25),
        Err(FigureError::ParamOutsideRange)
    );
}

#[test]
fn a_refused_value_does_not_land() {
    let mut pose = Pose::REST;
    assert!(pose.set(Param::HeadNod, 0.5).is_ok());
    assert!(pose.set(Param::HeadNod, 9.0).is_err());
    assert!((pose.get(Param::HeadNod) - 0.5).abs() < f64::EPSILON);
}

#[test]
fn a_value_that_is_not_a_number_is_refused() {
    let mut pose = Pose::REST;
    assert_eq!(
        pose.set(Param::HeadNod, f64::NAN),
        Err(FigureError::ParamOutsideRange)
    );
}

#[test]
fn an_empty_mask_holds_nothing_and_a_full_one_holds_everything() {
    for param in Param::ALL {
        assert!(!Mask::NONE.holds(param));
        assert!(
            Mask::ALL.holds(param),
            "{param:?} missing from the full mask"
        );
    }
    assert!(Mask::NONE.is_empty());
    assert_eq!(Mask::ALL.len(), Param::COUNT);
}

#[test]
fn a_mask_holds_only_what_was_put_in_it() {
    let mask = Mask::NONE.with(Param::HeadNod).with(Param::SpineBend);
    assert_eq!(mask.len(), 2);
    assert!(mask.holds(Param::HeadNod));
    assert!(mask.holds(Param::SpineBend));
    assert!(!mask.holds(Param::HeadTurn));
}

#[test]
fn removing_from_a_mask_undoes_adding_to_it() {
    let mask = Mask::NONE.with(Param::HeadNod).without(Param::HeadNod);
    assert_eq!(mask, Mask::NONE);
}

#[test]
fn mask_set_operations_agree_with_membership() {
    let left = Mask::NONE.with(Param::HeadNod).with(Param::SpineBend);
    let right = Mask::NONE.with(Param::SpineBend).with(Param::HeadTurn);

    assert_eq!(left.union(right).len(), 3);
    assert_eq!(left.intersection(right).len(), 1);
    assert!(left.intersection(right).holds(Param::SpineBend));
    assert_eq!(left.difference(right).len(), 1);
    assert!(left.difference(right).holds(Param::HeadNod));
}

#[test]
fn the_full_mask_leaves_no_bit_set_that_names_nothing() {
    let named = Param::ALL
        .iter()
        .fold(Mask::NONE, |mask, param| mask.with(*param));
    assert_eq!(named, Mask::ALL);
}

/// A layer states how far it moves a parameter, not where it puts it, so two
/// layers pulling on one parameter cooperate instead of the later one
/// erasing the earlier.
#[test]
fn overlay_deltas_from_different_layers_add_up() {
    let mut breath = Overlay::NONE;
    breath.add(Param::SpineBend, 0.10).expect("a real delta");
    let mut recoil = Overlay::NONE;
    recoil.add(Param::SpineBend, -0.04).expect("a real delta");
    recoil.add(Param::HeadNod, 0.2).expect("a real delta");

    let mut stack = breath;
    stack.absorb(&recoil);
    assert!(mathf::fabs(stack.get(Param::SpineBend) - 0.06) < 1e-12);
    assert!(mathf::fabs(stack.get(Param::HeadNod) - 0.2) < 1e-12);

    // And the other way round, since summing cannot depend on the order.
    let mut swapped = recoil;
    swapped.absorb(&breath);
    for param in Param::ALL {
        assert!(mathf::fabs(swapped.get(param) - stack.get(param)) < 1e-12);
    }
}

/// The delta itself is unbounded — two layers pulling opposite ways are
/// entitled to cancel — and it is the *sum* that must land in range.
#[test]
fn an_overlay_brings_its_sum_into_range_rather_than_each_delta() {
    let mut overlay = Overlay::NONE;
    overlay
        .add(Param::SpineBend, 5.0)
        .expect("a big delta is fine");
    overlay
        .add(Param::SpineBend, -5.0)
        .expect("and so is its opposite");
    assert!(mathf::fabs(overlay.get(Param::SpineBend)) < 1e-12);

    let mut crowded = Overlay::NONE;
    crowded
        .add(Param::KneeBend(Side::Left), 3.0)
        .expect("a real delta");
    let applied = crowded
        .applied(&Pose::REST)
        .expect("a sum is brought into range");
    assert!(mathf::fabs(applied.get(Param::KneeBend(Side::Left)) - 1.0) < 1e-12);
    for param in Param::ALL {
        assert!(param.range().holds(applied.get(param)));
    }
}

/// A knee at its own extreme cannot be pushed further, so a layer arriving
/// late fades rather than fighting the pose already there.
#[test]
fn a_layer_fades_against_a_parameter_already_at_its_extreme() {
    let folded = Pose::REST
        .with(Param::KneeBend(Side::Left), 1.0)
        .expect("a real pose");
    let mut overlay = Overlay::NONE;
    overlay
        .add(Param::KneeBend(Side::Left), 0.5)
        .expect("a real delta");
    let applied = overlay.applied(&folded).expect("it applies");
    assert!(mathf::fabs(applied.get(Param::KneeBend(Side::Left)) - 1.0) < 1e-12);
}

#[test]
fn an_overlay_reports_only_what_a_layer_actually_moved() {
    let mut overlay = Overlay::NONE;
    assert!(overlay.is_empty() && overlay.written().is_empty());
    overlay.add(Param::HeadTurn, 0.0).expect("a real delta");
    assert!(overlay.is_empty(), "moving nothing is not writing");
    overlay.add(Param::HeadTurn, 0.3).expect("a real delta");
    assert!(!overlay.is_empty());
    assert_eq!(overlay.written().len(), 1);
    assert!(overlay.written().holds(Param::HeadTurn));
    // A pose is left alone wherever nothing was written, which is the whole
    // pose rather than a scan of it.
    let applied = overlay.applied(&Pose::REST).expect("it applies");
    let only_turned = Pose::REST.with(Param::HeadTurn, 0.3).expect("a real pose");
    assert_eq!(
        applied, only_turned,
        "a layer touched what it did not write"
    );
}

#[test]
fn an_unreal_delta_is_refused() {
    let mut overlay = Overlay::NONE;
    for delta in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            overlay.add(Param::HeadNod, delta),
            Err(FigureError::ParamOutsideRange),
            "a delta of {delta} must be refused"
        );
    }
    assert!(overlay.is_empty(), "a refusal writes nothing");
}

#[test]
fn an_empty_overlay_is_the_default_and_changes_no_pose() {
    let pose = Pose::REST
        .with(Param::SpineTwist, -0.4)
        .expect("a real pose");
    assert_eq!(Overlay::default(), Overlay::NONE);
    assert_eq!(Overlay::NONE.applied(&pose).expect("it applies"), pose);
}
