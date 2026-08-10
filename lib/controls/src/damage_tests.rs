//! Unit tests for the damage sink every control reports through.
//!
//! These pin the contract the host depends on: a control reports its own
//! bounds exactly when a drawn field changed, a container reports the union of
//! what its children reported and nothing wider, and a sink that runs out of
//! rectangles over-covers rather than losing one.

use alloc::vec;

use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, PointerButton};
use tairix_theme::Theme;

use crate::button::{Button, ButtonContent, IconButton, SplitButton};
use crate::damage::{set, sink};
use crate::rail::ActionRail;
use crate::shell::TraySignal;
use crate::state::{ControlRole, PointerState};

const W: u32 = 160;

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};

fn plate() -> Rect {
    Rect::new(0, 0, W, 28)
}

/// Every pixel of `rect` that the region must still cover, sampled at the
/// corners a half-open rectangle actually owns.
fn corners(rect: &Rect) -> [Point; 4] {
    [
        Point::new(rect.left(), rect.top()),
        Point::new(rect.right() - 1, rect.top()),
        Point::new(rect.left(), rect.bottom() - 1),
        Point::new(rect.right() - 1, rect.bottom() - 1),
    ]
}

/// A three-item rail with the bounds it fills, so the item rectangles a report
/// is compared against come from the rail's own layout.
fn rail(theme: &Theme) -> (ActionRail, Rect) {
    let rail = ActionRail::new(vec![
        Button::labelled("One"),
        Button::labelled("Two"),
        Button::labelled("Three"),
    ]);
    let height = rail.measured_height(Scale::ONE, theme);
    (rail, Rect::new(0, 0, W, height))
}

// --- The guarded write -------------------------------------------------

#[test]
fn a_write_that_changes_nothing_reports_nothing() {
    let mut damage = sink();
    let mut field = PointerState::Hover;
    set(&mut field, PointerState::Hover, plate(), &mut damage);
    assert!(damage.is_empty(), "an unchanged field is not repainted");

    set(&mut field, PointerState::Pressed, plate(), &mut damage);
    assert_eq!(damage.rects(), &[plate()]);
    assert_eq!(field, PointerState::Pressed);
}

// --- One control ------------------------------------------------------

#[test]
fn hover_enter_and_press_each_report_one_rect() {
    let mut button = Button::labelled("OK");

    let mut entered = sink();
    button.on_pointer(&moved(10, 10), plate(), &mut entered);
    assert_eq!(entered.rects(), &[plate()], "the hover highlight is drawn");

    let mut pressed = sink();
    button.on_pointer(&PRESS, plate(), &mut pressed);
    assert_eq!(pressed.rects(), &[plate()], "the press wash is drawn");
}

#[test]
fn motion_within_one_control_reports_nothing() {
    let mut button = Button::labelled("OK");
    button.on_pointer(&moved(10, 10), plate(), &mut sink());

    let mut damage = sink();
    button.on_pointer(&moved(12, 11), plate(), &mut damage);
    button.on_pointer(&moved(90, 20), plate(), &mut damage);
    assert!(
        damage.is_empty(),
        "the pointer stayed inside, so the hover look never changed"
    );
}

#[test]
fn a_render_invariant_change_reports_nothing() {
    let mut button = IconButton::new(IconKind::Bell, ControlRole::Neutral);

    // Two samples clear of the plate: only the recorded coordinate moves, and
    // a coordinate is hit-testing input rather than a drawn property.
    let mut damage = sink();
    button.on_pointer(&moved(400, 400), plate(), &mut damage);
    button.on_pointer(&moved(900, 120), plate(), &mut damage);
    assert!(damage.is_empty());

    // The same control still reports the moment something drawn does change.
    button.on_pointer(&moved(10, 10), plate(), &mut damage);
    assert_eq!(damage.rects(), &[plate()]);
}

#[test]
fn a_split_button_reports_its_whole_plate_for_either_region() {
    let theme = Theme::dark();
    let mut split = SplitButton::new(ButtonContent::Label("Run".into()), ControlRole::Primary);

    let mut damage = sink();
    split.on_pointer(
        &moved(i32::try_from(W).expect("width") - 5, 14),
        plate(),
        Scale::ONE,
        &theme,
        &mut damage,
    );
    assert_eq!(
        damage.rects(),
        &[plate()],
        "one plate resolves from both region states, so both repaint together"
    );
}

// --- Containers -------------------------------------------------------

#[test]
fn crossing_a_boundary_reports_exactly_the_two_children() {
    let theme = Theme::dark();
    let (mut rail, bounds) = rail(&theme);
    let first = rail
        .item_rect(bounds, 0, Scale::ONE, &theme)
        .expect("first item");
    let last = rail
        .item_rect(bounds, 2, Scale::ONE, &theme)
        .expect("last item");

    rail.on_pointer(
        &moved(10, first.top() + 4),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );

    let mut damage = sink();
    rail.on_pointer(
        &moved(10, last.top() + 4),
        bounds,
        Scale::ONE,
        &theme,
        &mut damage,
    );
    assert_eq!(
        damage.rects(),
        &[first, last],
        "the child left and the child entered, and nothing between them"
    );
    assert!(
        damage.bounds() != bounds,
        "a container reports its children, not itself"
    );
}

#[test]
fn a_container_reports_nothing_when_no_child_changed() {
    let theme = Theme::dark();
    let (mut rail, bounds) = rail(&theme);
    let outside = i32::try_from(bounds.height).expect("height") + 40;
    rail.on_pointer(&moved(10, outside), bounds, Scale::ONE, &theme, &mut sink());

    let mut damage = sink();
    rail.on_pointer(
        &moved(20, outside + 10),
        bounds,
        Scale::ONE,
        &theme,
        &mut damage,
    );
    assert!(damage.is_empty());
}

#[test]
fn a_tray_signal_reports_the_readout_it_opened() {
    let theme = Theme::dark();
    let capsule = Rect::new(0, 0, 24, 24);
    let readout = Rect::new(0, 40, 120, 60);
    let mut signal = TraySignal::new(IconKind::Battery, "Battery").with_value("82%");

    let mut damage = sink();
    let _ = signal.on_pointer(
        &moved(10, 10),
        capsule,
        readout,
        Scale::ONE,
        &theme,
        &mut damage,
    );
    assert!(signal.is_expanded());
    assert_eq!(
        damage.rects(),
        &[capsule, readout],
        "the popup that just appeared has to be painted too"
    );
}

// --- The rectangle budget ---------------------------------------------

#[test]
fn a_sink_at_its_budget_degrades_to_the_box_it_still_covers() {
    let mut damage = sink();
    assert_eq!(damage.budget(), Some(8));

    let rows: [Rect; 9] =
        core::array::from_fn(|i| Rect::new(0, i32::try_from(i).expect("small index") * 10, 4, 4));
    for (n, rect) in rows.iter().enumerate().take(8) {
        damage.add(*rect);
        assert_eq!(
            damage.rects().len(),
            n + 1,
            "under budget the sink is exact"
        );
    }

    damage.add(rows[8]);
    let box_of_all = rows.iter().fold(Rect::EMPTY, |acc, rect| acc.union(rect));
    assert_eq!(damage.rects(), &[box_of_all]);
    for rect in &rows {
        for corner in corners(rect) {
            assert!(
                damage.contains(corner),
                "degrading may over-cover, never lose a reported pixel"
            );
        }
    }
}

/// A degraded sink is still what a container hands back, so the union of what
/// the children reported stays covered even when the count runs out.
#[test]
fn a_degraded_container_report_still_covers_every_child() {
    let theme = Theme::dark();
    let (mut rail, bounds) = rail(&theme);
    let mut damage = Region::with_budget(1);
    let first = rail
        .item_rect(bounds, 0, Scale::ONE, &theme)
        .expect("first item");
    let last = rail
        .item_rect(bounds, 2, Scale::ONE, &theme)
        .expect("last item");

    rail.on_pointer(
        &moved(10, first.top() + 4),
        bounds,
        Scale::ONE,
        &theme,
        &mut damage,
    );
    rail.on_pointer(
        &moved(10, last.top() + 4),
        bounds,
        Scale::ONE,
        &theme,
        &mut damage,
    );

    assert_eq!(damage.rects().len(), 1);
    for rect in [first, last] {
        for corner in corners(&rect) {
            assert!(damage.contains(corner));
        }
    }
}
