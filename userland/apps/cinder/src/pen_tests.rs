//! Playpen tests: letting Cinder out, petting, dragging, and the toy.

use super::{Pen, PenAction, Refusal, Whereabouts, BRING_HOME_LABEL, LET_OUT_LABEL, PET_RADIUS};
use crate::layout::{pen as lay_out, PenLayout, PEN_HEIGHT, PEN_WIDTH};
use tairix_controls::ButtonContent;
use tairix_geometry::Region;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, PointerButton};
use tairix_theme::{Theme, ThemeRegistry};

fn theme() -> Theme {
    ThemeRegistry::with_builtins().active().clone()
}

fn layout() -> PenLayout {
    lay_out(
        Rect::new(0, 0, PEN_WIDTH, PEN_HEIGHT),
        Scale::from_percent(100).expect("the reference density"),
        &theme(),
    )
}

fn settled() -> (Pen, PenLayout) {
    let layout = layout();
    let mut pen = Pen::new();
    pen.settle(&layout);
    (pen, layout)
}

fn at_feet(pen: &Pen) -> Point {
    Point::new(
        tairix_util::mathf::round_i32(pen.at().x),
        tairix_util::mathf::round_i32(pen.at().y),
    )
}

#[test]
fn cinder_starts_in_the_pen_standing_on_the_floor() {
    let (pen, layout) = settled();
    assert_eq!(pen.whereabouts(), Whereabouts::Inside);
    assert!(layout.floor.contains(at_feet(&pen)));
}

#[test]
fn letting_him_out_works_and_a_refusal_is_stated_rather_than_fatal() {
    let (mut pen, _) = settled();
    assert!(pen.let_out(None));
    assert_eq!(pen.whereabouts(), Whereabouts::Loose);
    assert_eq!(pen.refusal(), None);

    let (mut pen, _) = settled();
    assert!(!pen.let_out(Some(Refusal::NotPermitted)));
    assert_eq!(
        pen.whereabouts(),
        Whereabouts::Inside,
        "a refused optional action leaves the program working"
    );
    assert_eq!(pen.refusal(), Some(Refusal::NotPermitted));
    assert!(!pen.refusal().expect("stated").reason().is_empty());
}

#[test]
fn every_refusal_states_a_reason() {
    for refusal in [Refusal::NotPermitted, Refusal::NoDesktop, Refusal::SeatFull] {
        assert!(
            !refusal.reason().is_empty(),
            "a silent refusal is a program failing without saying why"
        );
    }
}

#[test]
fn putting_him_away_brings_him_back_to_the_floor() {
    let (mut pen, layout) = settled();
    assert!(pen.let_out(None));
    pen.put_away(&layout);
    assert_eq!(pen.whereabouts(), Whereabouts::Inside);
    assert!(layout.floor.contains(at_feet(&pen)));
}

#[test]
fn a_press_and_release_on_him_is_a_pet() {
    let (mut pen, layout) = settled();
    let on_him = at_feet(&pen);
    assert_eq!(pen.press(on_him, &layout), PenAction::DragBegan);
    assert_eq!(pen.release(on_him, &layout), PenAction::Petted);
    assert!(!pen.is_dragging());
}

#[test]
fn a_press_well_away_from_him_does_not_reach_him() {
    let (mut pen, layout) = settled();
    let far = Point::new(
        at_feet(&pen).x + tairix_util::mathf::round_i32(PET_RADIUS) * 3,
        layout.wall.top() + 2,
    );
    assert_eq!(pen.press(far, &layout), PenAction::Nothing);
    assert!(!pen.is_dragging());
}

#[test]
fn a_drag_moves_him_and_keeps_him_on_the_floor() {
    let (mut pen, layout) = settled();
    let start = at_feet(&pen);
    assert_eq!(pen.press(start, &layout), PenAction::DragBegan);
    // Drag far past the floor's edge; he must stop at it rather than leave.
    assert!(pen.motion(Point::new(start.x + 10_000, start.y + 10_000), &layout));
    assert!(layout.floor.contains(at_feet(&pen)));
    let _ = pen.release(at_feet(&pen), &layout);
    assert!(layout.floor.contains(at_feet(&pen)));
}

#[test]
fn a_drag_does_not_snap_him_to_the_cursor() {
    let (mut pen, layout) = settled();
    let feet = at_feet(&pen);
    // Press at his edge rather than his centre; the first motion must move
    // him by the motion's delta, not centre him under the pointer.
    let edge = Point::new(feet.x + 12, feet.y + 4);
    assert_eq!(pen.press(edge, &layout), PenAction::DragBegan);
    pen.motion(edge, &layout);
    assert_eq!(
        at_feet(&pen),
        feet,
        "a press that has not moved must not move him"
    );
}

#[test]
fn motion_with_no_drag_in_flight_moves_nothing() {
    let (mut pen, layout) = settled();
    assert!(!pen.motion(Point::new(5, 5), &layout));
}

#[test]
fn a_release_with_no_drag_in_flight_does_nothing() {
    let (mut pen, layout) = settled();
    assert_eq!(pen.release(Point::new(5, 5), &layout), PenAction::Nothing);
}

#[test]
fn he_cannot_be_dragged_while_he_is_out() {
    let (mut pen, layout) = settled();
    assert!(pen.let_out(None));
    let where_he_was = at_feet(&pen);
    assert_ne!(pen.press(where_he_was, &layout), PenAction::DragBegan);
}

#[test]
fn the_toy_is_batted_along_the_floor_and_stays_on_it() {
    let (mut pen, layout) = settled();
    let before = pen.toy();
    let on_toy = Point::new(layout.toy.left() + 1, layout.toy.top() + 1);
    assert_eq!(pen.press(on_toy, &layout), PenAction::ToyBatted);
    assert_ne!(pen.toy(), before, "a bat must move it");
    for _ in 0..50 {
        pen.press(on_toy, &layout);
        assert!(
            pen.toy().x >= layout.floor.left() && pen.toy().x < layout.floor.right(),
            "the toy must not be batted out of the pen"
        );
    }
}

#[test]
fn a_press_clears_a_stated_refusal() {
    let (mut pen, layout) = settled();
    assert!(!pen.let_out(Some(Refusal::NoDesktop)));
    assert!(pen.refusal().is_some());
    pen.press(Point::new(2, 2), &layout);
    assert_eq!(pen.refusal(), None);
}

#[test]
fn the_button_says_what_it_will_do_and_follows_where_he_is() {
    let label = |pen: &Pen| match pen.button().content() {
        ButtonContent::Label(text) => text.clone(),
        _ => alloc::string::String::new(),
    };

    let (mut pen, layout) = settled();
    assert_eq!(
        label(&pen),
        LET_OUT_LABEL,
        "an unlabelled control tells the user nothing"
    );
    assert!(pen.let_out(None));
    assert_eq!(label(&pen), BRING_HOME_LABEL);
    pen.put_away(&layout);
    assert_eq!(label(&pen), LET_OUT_LABEL);
}

#[test]
fn a_refused_let_out_leaves_the_button_offering_it_again() {
    let (mut pen, _) = settled();
    assert!(!pen.let_out(Some(Refusal::NotPermitted)));
    assert!(matches!(
        pen.button().content(),
        ButtonContent::Label(text) if text == LET_OUT_LABEL
    ));
}

#[test]
fn clicking_the_button_fires_once_and_does_not_reach_the_floor() {
    let (mut pen, layout) = settled();
    let on_button = Point::new(
        layout.button.left() + i32::try_from(layout.button.width / 2).unwrap_or(0),
        layout.button.top() + i32::try_from(layout.button.height / 2).unwrap_or(0),
    );
    let mut damage = Region::new();
    let moved = InputEvent::PointerMoved { to: on_button };
    let pressed = InputEvent::PointerPressed {
        button: PointerButton::Primary,
    };
    let released = InputEvent::PointerReleased {
        button: PointerButton::Primary,
    };

    assert!(pen.button_pointer(&moved, &layout, &mut damage).is_none());
    assert!(pen.button_pointer(&pressed, &layout, &mut damage).is_none());
    assert_eq!(
        pen.button_pointer(&released, &layout, &mut damage),
        Some(PenAction::Toggled),
        "a completed click on the control must fire it"
    );

    // ...and the same press must not also be a press on the room behind it.
    assert_eq!(pen.press(on_button, &layout), PenAction::Nothing);
}

#[test]
fn a_press_in_the_strip_never_moves_cinder() {
    let (mut pen, layout) = settled();
    let before = pen.at();
    let in_strip = Point::new(layout.strip.left() + 2, layout.strip.top() + 2);
    assert_eq!(pen.press(in_strip, &layout), PenAction::Nothing);
    assert!(!pen.is_dragging());
    assert_eq!(pen.at(), before);
}
