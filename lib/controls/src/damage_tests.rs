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
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::Theme;

use crate::button::{Button, ButtonContent, IconButton, SplitButton};
use crate::damage::{paint_parts, set, sink, Repaint};
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

// ---- Repaint ----------------------------------------------------------

#[test]
fn a_clean_account_owes_nothing_and_one_rectangle_makes_it_owe_that() {
    let mut owed = Repaint::clean();
    assert!(owed.is_clean());
    assert_eq!(owed, Repaint::default());

    owed.add(Rect::new(4, 6, 10, 12));
    assert!(!owed.is_clean());
    assert_eq!(owed.area(64, 48), Region::from(Rect::new(4, 6, 10, 12)));
}

#[test]
fn a_whole_account_absorbs_every_rectangle_and_resolves_to_the_surface() {
    let mut owed = Repaint::Whole;
    owed.add(Rect::new(0, 0, 2, 2));
    assert_eq!(owed, Repaint::Whole, "nothing is owed beyond everything");
    assert!(!owed.is_clean());
    assert_eq!(owed.area(64, 48), Region::from(Rect::new(0, 0, 64, 48)));
}

#[test]
fn merging_keeps_both_sides_and_whole_outranks_parts_either_way() {
    let mut parts = Repaint::clean();
    parts.add(Rect::new(0, 0, 4, 4));
    let mut more = Repaint::clean();
    more.add(Rect::new(20, 20, 4, 4));

    let mut merged = parts.clone();
    merged.merge(more.clone());
    let mut both = Region::new();
    both.add(Rect::new(0, 0, 4, 4));
    both.add(Rect::new(20, 20, 4, 4));
    assert_eq!(merged.area(64, 64), both);

    let mut whole_first = Repaint::Whole;
    whole_first.merge(parts.clone());
    assert_eq!(whole_first, Repaint::Whole);

    let mut parts_first = parts;
    parts_first.merge(Repaint::Whole);
    assert_eq!(parts_first, Repaint::Whole);
}

#[test]
fn painting_parts_writes_inside_them_and_nowhere_else() {
    let mut surface = Surface::new(8, 8).expect("a small surface");
    let ink = Color::rgb(255, 0, 0);
    paint_parts(&mut surface, &[Rect::new(2, 2, 3, 3)], |surface| {
        surface.fill(ink);
    });
    for y in 0..8 {
        for x in 0..8 {
            let inside = (2..5).contains(&x) && (2..5).contains(&y);
            let pixel = surface.get(x, y).expect("in bounds");
            assert_eq!(
                pixel.unpremultiply() == ink,
                inside,
                "({x}, {y}): only the named rectangle is written"
            );
        }
    }
}

#[test]
fn painting_a_part_twice_lands_what_painting_the_whole_surface_lands() {
    // The rule a scoped chrome repaint rests on: re-deriving a rectangle over
    // pixels a previous paint left is idempotent, because a plate lays its
    // colour down rather than compositing it.
    let theme = Theme::dark().floating();
    let whole = Rect::new(0, 0, 40, 24);
    let recipe = |surface: &mut Surface| {
        let _ = crate::paint_surface_plate(
            surface,
            (0, 0, 40, 24),
            (6, crate::plate_border(&theme, Scale::ONE)),
            &theme,
            (theme.palette().surface_raised, crate::ChromeLayer::Ground),
        );
    };

    let mut once = Surface::new(40, 24).expect("a small surface");
    paint_parts(&mut once, &[whole], recipe);

    let mut twice = Surface::new(40, 24).expect("a small surface");
    paint_parts(&mut twice, &[whole], recipe);
    // Over a corner as well as the interior: a rounded plate's arc pixels are
    // coverage-blended, so laying the colour again would only mix them further
    // toward it.
    paint_parts(
        &mut twice,
        &[Rect::new(0, 0, 8, 8), Rect::new(8, 4, 10, 10)],
        recipe,
    );

    for y in 0..24 {
        for x in 0..40 {
            assert_eq!(
                once.get(x, y),
                twice.get(x, y),
                "({x}, {y}): a repainted part is the pixel a whole paint laid"
            );
        }
    }
}

#[test]
fn a_rectangle_before_the_surfaces_own_origin_is_skipped_not_moved() {
    let mut surface = Surface::new(4, 4).expect("a small surface");
    let ink = Color::rgb(0, 255, 0);
    paint_parts(&mut surface, &[Rect::new(-2, 0, 2, 2)], |surface| {
        surface.fill(ink);
    });
    for y in 0..4 {
        for x in 0..4 {
            assert_ne!(
                surface.get(x, y).map(Pixel::unpremultiply),
                Some(ink),
                "({x}, {y}): an unaddressable rectangle paints nowhere"
            );
        }
    }
}
