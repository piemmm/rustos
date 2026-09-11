//! Tests for the eight reorientations and the surface operation they drive.
//!
//! The group law is checked *against the position map* rather than against a
//! second table of it: a composition is right when setting a picture down
//! one way and then the other lands every pixel where the single combined
//! reorientation does. That is what makes `then` and `inverse` trustworthy
//! without either restating the other.

use alloc::vec::Vec;

use super::Reorient;
use crate::color::Color;
use crate::surface::Surface;

/// A surface whose every pixel is distinguishable from every other, so a
/// permutation of it cannot accidentally look right.
fn numbered(width: u32, height: u32) -> Surface {
    let mut surface = Surface::new(width, height).expect("allocates");
    for y in 0..height {
        for x in 0..width {
            let r = u8::try_from(x & 0xFF).unwrap_or(0);
            let g = u8::try_from(y & 0xFF).unwrap_or(0);
            surface.set(x, y, Color::rgba(r, g, 0x5A, 0xFF).premultiply());
        }
    }
    surface
}

#[test]
fn the_eight_are_distinct_and_decompose_uniquely() {
    let mut seen: Vec<(u32, bool)> = Vec::new();
    for how in Reorient::ALL {
        let parts = how.parts();
        assert!(parts.0 < 4, "{how:?} turns {}", parts.0);
        assert!(!seen.contains(&parts), "{how:?} repeats {parts:?}");
        assert_eq!(Reorient::from_parts(parts.0, parts.1), how, "{how:?}");
        seen.push(parts);
    }
    assert_eq!(seen.len(), 8);
}

#[test]
fn only_the_quarter_turns_swap_the_axes() {
    for how in Reorient::ALL {
        let transposes = matches!(
            how,
            Reorient::Transpose
                | Reorient::QuarterTurnRight
                | Reorient::Antitranspose
                | Reorient::QuarterTurnLeft
        );
        assert_eq!(how.transposes(), transposes, "{how:?}");
        let expected = if transposes { (2, 7) } else { (7, 2) };
        assert_eq!(how.applied_size(7, 2), expected, "{how:?}");
    }
}

#[test]
fn every_reorientation_permutes_the_rectangle_exactly() {
    let (width, height) = (5u32, 3u32);
    for how in Reorient::ALL {
        let (dest_width, dest_height) = how.applied_size(width, height);
        let mut seen = alloc::vec![false; (dest_width * dest_height) as usize];
        for y in 0..height {
            for x in 0..width {
                let (dx, dy) = how.place(x, y, width, height);
                assert!(dx < dest_width && dy < dest_height, "{how:?} ({x},{y})");
                let slot = &mut seen[(dy * dest_width + dx) as usize];
                assert!(!*slot, "{how:?} put two pixels at ({dx},{dy})");
                *slot = true;
            }
        }
        assert!(seen.into_iter().all(|hit| hit), "{how:?} left a hole");
    }
}

#[test]
fn each_named_reorientation_moves_the_first_pixel_where_its_name_says() {
    // A 4-wide, 2-high picture, so a turn is visibly different from a
    // mirror. Stated as the destination the source's top-left lands at.
    let corners = [
        (Reorient::None, (0, 0)),
        (Reorient::FlipHorizontal, (3, 0)),
        (Reorient::HalfTurn, (3, 1)),
        (Reorient::FlipVertical, (0, 1)),
        (Reorient::Transpose, (0, 0)),
        (Reorient::QuarterTurnRight, (1, 0)),
        (Reorient::Antitranspose, (1, 3)),
        (Reorient::QuarterTurnLeft, (0, 3)),
    ];
    for (how, want) in corners {
        assert_eq!(how.place(0, 0, 4, 2), want, "{how:?}");
    }
}

#[test]
fn a_quarter_turn_right_puts_the_top_row_down_the_right_edge() {
    let source = numbered(4, 2);
    let turned = source
        .reoriented(Reorient::QuarterTurnRight)
        .expect("allocates");
    assert_eq!((turned.width(), turned.height()), (2, 4));
    for x in 0..4 {
        assert_eq!(turned.get(1, x), source.get(x, 0), "column entry {x}");
    }
    for x in 0..4 {
        assert_eq!(turned.get(0, x), source.get(x, 1), "column entry {x}");
    }
}

#[test]
fn composing_two_reorientations_lands_where_the_single_one_does() {
    let source = numbered(4, 3);
    for first in Reorient::ALL {
        for second in Reorient::ALL {
            let stepwise = source
                .reoriented(first)
                .expect("allocates")
                .reoriented(second)
                .expect("allocates");
            let combined = source.reoriented(first.then(second)).expect("allocates");
            assert_eq!(stepwise, combined, "{first:?} then {second:?}");
        }
    }
}

#[test]
fn composition_stays_within_the_eight() {
    for first in Reorient::ALL {
        for second in Reorient::ALL {
            let combined = first.then(second);
            assert!(
                Reorient::ALL.contains(&combined),
                "{first:?} then {second:?}"
            );
        }
    }
}

#[test]
fn doing_nothing_is_the_identity_on_either_side() {
    for how in Reorient::ALL {
        assert_eq!(how.then(Reorient::None), how, "{how:?}");
        assert_eq!(Reorient::None.then(how), how, "{how:?}");
    }
}

#[test]
fn every_reorientation_is_undone_by_its_inverse() {
    let source = numbered(5, 2);
    for how in Reorient::ALL {
        assert_eq!(how.then(how.inverse()), Reorient::None, "{how:?}");
        assert_eq!(how.inverse().then(how), Reorient::None, "{how:?}");
        let there_and_back = source
            .reoriented(how)
            .expect("allocates")
            .reoriented(how.inverse())
            .expect("allocates");
        assert_eq!(there_and_back, source, "{how:?}");
    }
}

#[test]
fn the_inverse_reads_a_shown_position_back_to_the_stored_one() {
    // What a viewer does with a pointer: the position on screen, mapped
    // back through the undoing, is the position in the picture as stored.
    let (width, height) = (5u32, 3u32);
    for how in Reorient::ALL {
        let (dest_width, dest_height) = how.applied_size(width, height);
        for y in 0..height {
            for x in 0..width {
                let (dx, dy) = how.place(x, y, width, height);
                let back = how.inverse().place(dx, dy, dest_width, dest_height);
                assert_eq!(back, (x, y), "{how:?} at ({x},{y})");
            }
        }
    }
}

#[test]
fn four_quarter_turns_come_back_round() {
    let mut how = Reorient::None;
    for _ in 0..4 {
        how = how.then(Reorient::QuarterTurnRight);
    }
    assert_eq!(how, Reorient::None);

    let mut how = Reorient::None;
    for _ in 0..4 {
        how = how.then(Reorient::QuarterTurnLeft);
    }
    assert_eq!(how, Reorient::None);
}

#[test]
fn a_mirror_applied_twice_is_no_mirror() {
    for mirror in [
        Reorient::FlipHorizontal,
        Reorient::FlipVertical,
        Reorient::Transpose,
        Reorient::Antitranspose,
    ] {
        assert_eq!(mirror.then(mirror), Reorient::None, "{mirror:?}");
        assert_eq!(mirror.inverse(), mirror, "{mirror:?}");
    }
}

#[test]
fn turning_a_picture_resamples_nothing() {
    // Every pixel of the source appears in the destination exactly once,
    // unchanged: a reorientation invents no colour and loses none.
    let source = numbered(6, 4);
    for how in Reorient::ALL {
        let turned = source.reoriented(how).expect("allocates");
        let mut want: Vec<_> = source.pixels().to_vec();
        let mut got: Vec<_> = turned.pixels().to_vec();
        want.sort_unstable_by_key(|p| (p.r, p.g, p.b, p.a));
        got.sort_unstable_by_key(|p| (p.r, p.g, p.b, p.a));
        assert_eq!(got, want, "{how:?}");
    }
}

#[test]
fn setting_a_picture_down_unchanged_is_the_picture() {
    let source = numbered(3, 5);
    assert_eq!(source.reoriented(Reorient::None), Some(source.clone()));
}

#[test]
fn a_square_picture_keeps_its_geometry_under_every_reorientation() {
    let source = numbered(4, 4);
    for how in Reorient::ALL {
        let turned = source.reoriented(how).expect("allocates");
        assert_eq!((turned.width(), turned.height()), (4, 4), "{how:?}");
    }
}

#[test]
fn a_degenerate_picture_is_reoriented_without_complaint() {
    for how in Reorient::ALL {
        let single = numbered(1, 1);
        assert_eq!(single.reoriented(how), Some(single.clone()), "{how:?}");

        let empty = Surface::new(0, 0).expect("allocates");
        let turned = empty.reoriented(how).expect("allocates");
        assert_eq!((turned.width(), turned.height()), (0, 0), "{how:?}");

        let line = numbered(4, 1);
        let turned = line.reoriented(how).expect("allocates");
        let expected = how.applied_size(4, 1);
        assert_eq!((turned.width(), turned.height()), expected, "{how:?}");
    }
}

#[test]
fn placing_a_coordinate_outside_the_picture_is_total() {
    for how in Reorient::ALL {
        // Answers rather than panicking, for anything at all.
        let _ = how.place(u32::MAX, u32::MAX, 4, 4);
        assert_eq!(how.place(0, 0, 0, 0), (0, 0), "{how:?}");
    }
    // A mirrored axis clamps at the near edge rather than wrapping past it.
    assert_eq!(Reorient::FlipHorizontal.place(u32::MAX, 0, 4, 4), (0, 0));
    assert_eq!(Reorient::HalfTurn.place(u32::MAX, u32::MAX, 4, 4), (0, 0));
    assert_eq!(Reorient::QuarterTurnRight.place(0, u32::MAX, 4, 4), (0, 0));
}

#[test]
fn reorienting_into_a_held_destination_gives_what_allocating_one_gives() {
    let source = numbered(5, 3);
    for how in Reorient::ALL {
        let allocated = source.reoriented(how).expect("allocates");
        let (width, height) = how.applied_size(5, 3);
        let mut held = Surface::new(width, height).expect("allocates");
        assert!(source.reorient_into(&mut held, how), "{how:?}");
        assert_eq!(held.pixels(), allocated.pixels(), "{how:?}");
    }
}

#[test]
fn reorienting_into_a_wrongly_shaped_destination_writes_nothing() {
    let source = numbered(4, 2);
    // The transposing cases want 2x4, the rest 4x2, so one destination of
    // each shape is refused by exactly the four that do not want it.
    for how in Reorient::ALL {
        let (width, height) = how.applied_size(4, 2);
        let mut swapped = Surface::filled(height, width, Color::rgba(9, 9, 9, 255).premultiply())
            .expect("allocates");
        let untouched = swapped.clone();
        if width == height {
            continue;
        }
        assert!(!source.reorient_into(&mut swapped, how), "{how:?}");
        assert_eq!(swapped, untouched, "{how:?}");
    }
}

#[test]
fn a_held_destination_is_refilled_rather_than_blended_into() {
    let mut held =
        Surface::filled(2, 2, Color::rgba(200, 0, 0, 255).premultiply()).expect("allocates");
    let source = numbered(2, 2);
    assert!(source.reorient_into(&mut held, Reorient::None));
    assert_eq!(held.pixels(), source.pixels());
}
