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

/// A digest over a blank frame would be perfectly stable and perfectly
/// worthless, so each frame it folds is checked for real terrain: the window
/// shot's own check covers neither of these, one at the furthest zoom and
/// the other with every rung shed.
#[test]
fn the_frames_the_digest_folds_are_real_terrain_with_the_whole_cast() {
    let mut drawn = 0;
    draw_frames(|target, renderer| {
        drawn += 1;
        let pixels = target.pixels();
        assert!(
            pixels.iter().all(|p| p.a == 255),
            "frame {drawn} left transparent pixels"
        );
        assert_eq!(
            renderer.grid().unmapped(),
            0,
            "frame {drawn} drew ground it did not hold"
        );
        assert_eq!(
            renderer.figures(),
            reference::CAST_LEN,
            "frame {drawn} lost a figure"
        );
        let colours = pixels
            .iter()
            .map(|p| (p.r, p.g, p.b))
            .collect::<alloc::collections::BTreeSet<_>>();
        assert!(
            colours.len() > 16,
            "frame {drawn} is a flat fill of {} colours",
            colours.len()
        );
    })
    .expect("the reference frames draw");
    assert_eq!(drawn, FRAMES.len());
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
