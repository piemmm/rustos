//! The host leg of the cross-target claim.

use super::{reference, REFERENCE_DIGEST};

/// The constant every Tier-1 target asserts. Agreement between targets is a
/// consequence of each agreeing with this, so a target that has never been
/// run cannot pass by accident.
#[test]
fn the_reference_grid_folds_to_the_reference_digest() {
    let produced = reference().expect("the reference grid");
    assert_eq!(
        produced, REFERENCE_DIGEST,
        "produced {produced:#018X}, expected {REFERENCE_DIGEST:#018X}"
    );
}

/// A digest that were not a pure function of the code would be no claim at
/// all.
#[test]
fn the_grid_folds_the_same_way_twice() {
    assert_eq!(
        reference().expect("the reference grid"),
        reference().expect("the reference grid")
    );
}
