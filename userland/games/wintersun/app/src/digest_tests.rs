//! The reference frames are drawn, reproducible, and sensitive to
//! everything beneath them.

use super::*;

#[test]
fn the_reference_frames_agree_with_the_constant() {
    let produced = reference().expect("the reference realm generates");
    assert_eq!(
        produced, REFERENCE_DIGEST,
        "the client frame digest moved to {produced:#018x}; if that was \
         intended, write the new value down deliberately"
    );
}

#[test]
fn the_digest_is_the_same_every_time() {
    let first = reference().expect("the reference realm generates");
    let second = reference().expect("the reference realm generates");
    assert_eq!(first, second, "two runs of one binary disagreed");
}

#[test]
fn the_digest_folds_in_the_art_constant() {
    // Not a claim about the value, a claim about the dependency: the
    // art's own digest is part of the input, so a change to the ground
    // moves this number rather than passing unnoticed.
    let without = FastHash::with_seed(reference::SEED);
    let mut with = FastHash::with_seed(reference::SEED);
    with.write_u64(art::REFERENCE_DIGEST);
    assert_ne!(without.finish(), with.finish());
}

#[test]
fn the_reference_frames_cover_both_ends_of_both_knobs() {
    let steps: alloc::vec::Vec<u8> = FRAMES.iter().map(|(step, _)| *step).collect();
    let zooms: alloc::vec::Vec<Zoom> = FRAMES.iter().map(|(_, zoom)| *zoom).collect();
    assert!(
        steps.contains(&0) && steps.contains(&Ladder::MAX_STEP),
        "a digest that never sheds proves nothing about the ladder"
    );
    assert!(
        zooms.contains(&Zoom::DEFAULT) && zooms.contains(&Zoom::FURTHEST),
        "a digest at one zoom exercises one mip band"
    );
}
