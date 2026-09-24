//! Unit tests for the plate column: placement, the height it measures, the
//! width it inverts, and the reveal that scrolls a plate into it.

use alloc::vec::Vec;

use tairix_geometry::{to_i32, Rect, Scale};
use tairix_theme::Theme;

use crate::stack::{as_extent, column_width, gap, height, place, plate_width, reveal_from};

const HEIGHTS: [u32; 3] = [40, 70, 25];

fn plate(index: usize) -> u32 {
    HEIGHTS.get(index).copied().unwrap_or(0)
}

#[test]
fn plates_sit_a_gap_apart_and_a_gap_inside_the_column() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let g = gap(scale, &theme);
    let bounds = Rect::new(10, 20, 300, 1000);
    let placed = place(bounds, 0, HEIGHTS.len(), scale, &theme, plate);
    assert_eq!(placed.len(), HEIGHTS.len());
    let mut top = bounds.top() + to_i32(g);
    for (index, rect) in &placed {
        assert_eq!(rect.left(), bounds.left() + to_i32(g));
        assert_eq!(rect.width, plate_width(bounds.width, scale, &theme));
        assert_eq!(rect.top(), top, "plate {index}");
        assert_eq!(rect.height, plate(*index));
        top += to_i32(plate(*index) + g);
    }
}

#[test]
fn a_measured_column_seats_every_plate_and_a_shorter_one_does_not() {
    for scale in [Scale::ONE, Scale::from_percent(200).expect("scale")] {
        let theme = Theme::dark();
        let g = gap(scale, &theme);
        let tall = height(HEIGHTS, scale, &theme);
        assert_eq!(tall, HEIGHTS.iter().sum::<u32>() + g * 4);
        let seated = |h: u32| {
            place(
                Rect::new(0, 0, 300, h),
                0,
                HEIGHTS.len(),
                scale,
                &theme,
                plate,
            )
            .len()
        };
        assert_eq!(seated(tall), HEIGHTS.len());
        // The measurement keeps a gap beneath the last plate; a column any
        // shorter than the plates and the gaps between them loses the last.
        assert_eq!(seated(tall - g), HEIGHTS.len());
        assert_eq!(seated(tall - g - 1), HEIGHTS.len() - 1);
    }
}

#[test]
fn the_first_plate_always_draws_and_later_ones_only_whole() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let short = Rect::new(0, 0, 300, 10);
    let placed = place(short, 0, HEIGHTS.len(), scale, &theme, plate);
    let indices: Vec<usize> = placed.iter().map(|(index, _)| *index).collect();
    assert_eq!(
        indices,
        [0],
        "a blank column would be worse than a cut plate"
    );
    let from_second = place(short, 1, HEIGHTS.len(), scale, &theme, plate);
    assert_eq!(from_second.first().map(|(index, _)| *index), Some(1));
}

#[test]
fn the_column_width_inverts_the_plate_width() {
    for scale in [Scale::ONE, Scale::from_percent(150).expect("scale")] {
        let theme = Theme::dark();
        for width in [0, 1, 17, 300] {
            assert_eq!(
                plate_width(column_width(width, scale, &theme), scale, &theme),
                width
            );
        }
    }
}

#[test]
fn a_plate_below_the_column_is_revealed_as_the_last_one_drawn() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let g = gap(scale, &theme);
    // Seats the first two plates and not the third.
    let bounds = Rect::new(0, 0, 300, HEIGHTS[0] + HEIGHTS[1] + g * 3);
    assert_eq!(
        place(bounds, 0, HEIGHTS.len(), scale, &theme, plate).len(),
        2
    );
    let first = reveal_from(0, 2, bounds, HEIGHTS.len(), scale, &theme, plate);
    let seated: Vec<usize> = place(bounds, first, HEIGHTS.len(), scale, &theme, plate)
        .iter()
        .map(|(index, _)| *index)
        .collect();
    assert!(seated.contains(&2), "plate 2 is seated from {first}");
    assert_eq!(
        reveal_from(2, 0, bounds, HEIGHTS.len(), scale, &theme, plate),
        0,
        "a plate above the column becomes the first drawn"
    );
}

#[test]
fn a_plate_count_is_its_own_extent() {
    assert_eq!(as_extent(0), 0);
    assert_eq!(as_extent(7), 7);
}
