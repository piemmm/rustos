//! Pen-geometry tests: the bands, the furniture, and the companion surface's
//! relationship to the desktop's bound.

use super::{pen, strip_height, PEN_HEIGHT, PEN_MIN_HEIGHT, PEN_MIN_WIDTH, PEN_WIDTH};
use tairix_controls::Button;
use tairix_geometry::{Rect, Scale};
use tairix_theme::{Theme, ThemeRegistry};

fn scale() -> Scale {
    Scale::from_percent(100).expect("the reference density")
}

fn theme() -> Theme {
    ThemeRegistry::with_builtins().active().clone()
}

fn at(width: u32, height: u32) -> super::PenLayout {
    pen(Rect::new(0, 0, width, height), scale(), &theme())
}

#[test]
fn the_wall_the_floor_and_the_strip_tile_the_client_exactly() {
    let layout = at(PEN_WIDTH, PEN_HEIGHT);
    assert_eq!(layout.wall.top(), layout.client.top());
    assert_eq!(layout.wall.bottom(), layout.floor.top());
    assert_eq!(layout.floor.bottom(), layout.strip.top());
    assert_eq!(layout.strip.bottom(), layout.client.bottom());
    for band in [layout.wall, layout.floor, layout.strip] {
        assert_eq!(band.width, layout.client.width);
    }
}

#[test]
fn the_button_sits_inside_the_strip() {
    let layout = at(PEN_WIDTH, PEN_HEIGHT);
    assert!(layout.button.left() >= layout.strip.left());
    assert!(layout.button.right() <= layout.strip.right());
    assert!(layout.button.top() >= layout.strip.top());
    assert!(layout.button.bottom() <= layout.strip.bottom());
    assert!(
        layout.button.width > 0 && layout.button.height > 0,
        "an action the user cannot click is not an action"
    );
}

#[test]
fn the_strip_never_overlaps_the_floor_the_furniture_is_on() {
    let layout = at(PEN_WIDTH, PEN_HEIGHT);
    for piece in [layout.bed, layout.bowl, layout.toy] {
        assert!(
            piece.bottom() <= layout.strip.top(),
            "furniture must not sit under the control strip: {piece:?}"
        );
    }
}

#[test]
fn the_furniture_sits_on_the_floor_rather_than_through_its_edges() {
    let layout = at(PEN_WIDTH, PEN_HEIGHT);
    for piece in [layout.bed, layout.bowl] {
        assert!(piece.left() >= layout.floor.left(), "{piece:?}");
        assert!(piece.right() <= layout.floor.right(), "{piece:?}");
        assert!(piece.bottom() <= layout.floor.bottom(), "{piece:?}");
        assert!(piece.top() >= layout.floor.top(), "{piece:?}");
    }
}

#[test]
fn the_bed_and_the_bowl_are_at_opposite_ends() {
    let layout = at(PEN_WIDTH, PEN_HEIGHT);
    assert!(
        layout.bed.right() <= layout.bowl.left(),
        "the two must not overlap, or the pen reads as one object"
    );
}

#[test]
fn a_pen_at_its_minimum_still_lays_out_with_a_floor() {
    let layout = at(PEN_MIN_WIDTH, PEN_MIN_HEIGHT);
    assert!(layout.floor.height > 0);
    assert!(layout.wall.height > 0);
}

#[test]
fn a_denser_screen_gets_a_proportionally_larger_pen() {
    let one = Scale::from_percent(100).expect("reference");
    let two = Scale::from_percent(200).expect("doubled");
    assert_eq!(two.scale_length(PEN_WIDTH), one.scale_length(PEN_WIDTH) * 2);
}

#[test]
fn a_degenerate_client_does_not_produce_a_negative_band() {
    // A window manager may hand a client one pixel tall during a resize; the
    // layout must answer something drawable rather than wrapping.
    let layout = at(1, 1);
    assert_eq!(
        layout.wall.height + layout.floor.height + layout.strip.height,
        1,
        "the bands must still tile the client"
    );
}

#[test]
fn the_button_is_never_shorter_than_the_theme_says_a_button_is() {
    // The defect this closes: a hand-picked strip height left the button
    // shorter than a line of the theme's own body text, and the control then
    // drew a plate with no label on it. Sizing the strip from the control is
    // what makes the two unable to disagree.
    for percent in [50, 75, 100, 150, 200, 300] {
        let Some(scale) = Scale::from_percent(percent) else {
            continue;
        };
        let theme = theme();
        let height = strip_height(scale, &theme);
        let layout = pen(Rect::new(0, 0, PEN_WIDTH * 2, height + 200), scale, &theme);
        assert!(
            layout.button.height >= Button::height(scale, &theme),
            "at {percent}% the button is {} tall and a button needs {}",
            layout.button.height,
            Button::height(scale, &theme)
        );
    }
}
