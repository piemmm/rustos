use super::{reference, REFERENCE_DIGEST};

#[test]
fn the_scripted_scene_folds_to_the_reference() {
    let digest = reference().expect("the scene fits in test memory");
    assert_eq!(
        digest, REFERENCE_DIGEST,
        "the art moved: {digest:#018X} against {REFERENCE_DIGEST:#018X}",
    );
}

#[test]
fn folding_twice_gives_the_same_answer() {
    assert_eq!(
        reference().expect("the scene fits"),
        reference().expect("the scene fits"),
    );
}
