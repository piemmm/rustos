//! Tests for the shared animation walk, over a source that counts what it
//! was asked to composite: what a walk *costs* is the property here, and no
//! real container can be asked how many frames it decoded.

use alloc::vec;
use alloc::vec::Vec;

use super::{Animation, FrameSource};
use crate::DecodeError;

/// A source of `count` frames that records the index of every frame it is
/// asked to composite, and refuses the one at `refuses`.
struct Counting {
    count: u32,
    refuses: Option<u32>,
    composited: Vec<u32>,
    canvas: Vec<u8>,
}

impl Counting {
    fn new(count: u32) -> Self {
        Self {
            count,
            refuses: None,
            composited: Vec::new(),
            canvas: vec![0u8; 4],
        }
    }

    fn refusing(count: u32, refuses: u32) -> Self {
        Self {
            refuses: Some(refuses),
            ..Self::new(count)
        }
    }
}

impl FrameSource for Counting {
    fn width(&self) -> u32 {
        1
    }

    fn height(&self) -> u32 {
        1
    }

    fn count(&self) -> u32 {
        self.count
    }

    fn loop_count(&self) -> Option<u32> {
        None
    }

    fn canvas(&self) -> &[u8] {
        &self.canvas
    }

    fn advance(&mut self, _bytes: &[u8], index: u32) -> Result<u64, DecodeError> {
        if self.refuses == Some(index) {
            return Err(DecodeError::GifTruncated);
        }
        self.composited.push(index);
        // The canvas records the frame it now holds, so a test can tell one
        // composition from another rather than only counting them.
        self.canvas[0] = u8::try_from(index).unwrap_or(u8::MAX);
        Ok(u64::from(index))
    }

    fn restart(&mut self) {
        self.canvas.fill(0);
    }
}

#[test]
fn addressing_a_later_frame_composites_only_the_frames_in_between() {
    let mut animation = Animation::new(Counting::new(8));
    assert!(animation.frame(&[], 3).expect("frame 3 composites"));
    assert!(animation.frame(&[], 5).expect("frame 5 composites"));
    assert_eq!(
        animation.source.composited,
        vec![0, 1, 2, 3, 4, 5],
        "reaching frame 5 from frame 3 composites 4 and 5 and nothing again"
    );
}

#[test]
fn addressing_the_frame_already_held_composites_nothing_further() {
    let mut animation = Animation::new(Counting::new(4));
    assert!(animation.frame(&[], 2).expect("frame 2 composites"));
    assert!(animation.frame(&[], 2).expect("frame 2 is already held"));
    assert_eq!(animation.source.composited, vec![0, 1, 2]);
}

#[test]
fn addressing_an_earlier_frame_restarts_the_composition() {
    let mut animation = Animation::new(Counting::new(4));
    assert!(animation.frame(&[], 2).expect("frame 2 composites"));
    assert!(animation.frame(&[], 1).expect("frame 1 composites"));
    assert_eq!(
        animation.source.composited,
        vec![0, 1, 2, 0, 1],
        "a frame composites onto its predecessors, so going back replays them"
    );
}

#[test]
fn stepping_and_addressing_reach_the_same_canvas() {
    let mut stepped = Animation::new(Counting::new(6));
    for _ in 0..=4 {
        assert!(stepped.step(&[]).expect("the step composites"));
    }
    let mut addressed = Animation::new(Counting::new(6));
    assert!(addressed.frame(&[], 4).expect("frame 4 composites"));
    assert_eq!(stepped.canvas(), addressed.canvas());
    assert_eq!(stepped.index(), addressed.index());
}

#[test]
fn a_remembered_refusal_restarts_the_composition_rather_than_building_on_it() {
    let mut animation = Animation::new(Counting::refusing(6, 3));
    assert!(animation.frame(&[], 2).expect("frame 2 composites"));
    assert!(
        animation.frame(&[], 4).is_err(),
        "frame 3 refuses on the way"
    );
    // The canvas now describes no whole frame, so the next address may not
    // build on it however far forward it is.
    assert!(animation.frame(&[], 2).expect("frame 2 composites again"));
    assert_eq!(
        animation.source.composited,
        vec![0, 1, 2, 0, 1, 2],
        "the refusal is cleared by restarting, never by continuing"
    );
}

#[test]
fn addressing_past_the_last_frame_answers_none() {
    let mut animation = Animation::new(Counting::new(3));
    assert!(!animation.frame(&[], 3).expect("there is no frame 3"));
}
