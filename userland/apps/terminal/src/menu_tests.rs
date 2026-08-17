//! Unit tests for the terminal's right-click context menu.

use alloc::vec::Vec;

use tairix_controls::testkit::text_ladder;
use tairix_controls::{damage, Menu, MenuItem};
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_theme::{Fonts, Theme};

use super::{Command, ContextMenu, MenuOutcome};

/// A type ladder above the theme's standard control height, so the two
/// fixtures the face test compares differ in both plate axes.
const LARGER_BASE_SIZE_PX: u16 = 48;

fn viewport() -> Rect {
    Rect::new(0, 0, 400, 300)
}

fn ctrl() -> Modifiers {
    Modifiers {
        ctrl: true,
        ..Modifiers::default()
    }
}

fn ctrl_shift() -> Modifiers {
    Modifiers {
        ctrl: true,
        shift: true,
        ..Modifiers::default()
    }
}

/// The same row list [`ContextMenu::open`] builds, so a test that needs the
/// shared `Menu`'s own geometry (`row_rect`) can measure exactly what the
/// terminal's menu will draw.
fn menu_rows() -> Menu {
    let items: Vec<MenuItem> = Command::ALL
        .into_iter()
        .map(|command| {
            MenuItem::new(command.label())
                .with_shortcut(command.shortcut())
                .with_group_break(command.opens_group())
        })
        .collect();
    Menu::new(items)
}

/// The centre point of row `index`, in the window-local coordinates a
/// `ContextMenu` opened at `anchor` places it at.
fn row_center(anchor: Point, index: usize, theme: &Theme) -> Point {
    let context_menu = ContextMenu::open(anchor);
    let bounds = context_menu.bounds(viewport(), Scale::ONE, theme);
    let rect = menu_rows()
        .row_rect(index, bounds, Scale::ONE, theme)
        .expect("row in range");
    Point::new(
        rect.left() + to_i32(rect.width) / 2,
        rect.top() + to_i32(rect.height) / 2,
    )
}

// --- Command --------------------------------------------------------------

#[test]
fn every_command_label_and_shortcut_is_non_empty_and_distinct() {
    let labels: Vec<&str> = Command::ALL.iter().map(|c| c.label()).collect();
    let shortcuts: Vec<&str> = Command::ALL.iter().map(|c| c.shortcut()).collect();
    for label in &labels {
        assert!(!label.is_empty());
    }
    for shortcut in &shortcuts {
        assert!(!shortcut.is_empty());
    }
    for (i, label) in labels.iter().enumerate() {
        for other in labels.iter().skip(i + 1) {
            assert_ne!(label, other, "labels must be distinct");
        }
    }
    for (i, shortcut) in shortcuts.iter().enumerate() {
        for other in shortcuts.iter().skip(i + 1) {
            assert_ne!(shortcut, other, "shortcuts must be distinct");
        }
    }
}

#[test]
fn accelerator_maps_every_documented_shortcut_to_its_command() {
    assert_eq!(
        Command::accelerator(Key::Char(','), ctrl()),
        Some(Command::Settings)
    );
    assert_eq!(
        Command::accelerator(Key::Char('+'), ctrl()),
        Some(Command::Larger)
    );
    assert_eq!(
        Command::accelerator(Key::Char('='), ctrl()),
        Some(Command::Larger)
    );
    assert_eq!(
        Command::accelerator(Key::Char('-'), ctrl()),
        Some(Command::Smaller)
    );
    assert_eq!(
        Command::accelerator(Key::Char('_'), ctrl()),
        Some(Command::Smaller)
    );
    assert_eq!(
        Command::accelerator(Key::Char('0'), ctrl()),
        Some(Command::ActualSize)
    );
    assert_eq!(
        Command::accelerator(Key::Char('k'), ctrl_shift()),
        Some(Command::Clear)
    );
    assert_eq!(
        Command::accelerator(Key::Char('K'), ctrl_shift()),
        Some(Command::Clear)
    );
    assert_eq!(
        Command::accelerator(Key::Char('w'), ctrl_shift()),
        Some(Command::Close)
    );
    assert_eq!(
        Command::accelerator(Key::Char('W'), ctrl_shift()),
        Some(Command::Close)
    );
}

#[test]
fn accelerator_requires_ctrl() {
    assert_eq!(
        Command::accelerator(Key::Char(','), Modifiers::default()),
        None
    );
    assert_eq!(
        Command::accelerator(
            Key::Char('k'),
            Modifiers {
                shift: true,
                ..Modifiers::default()
            }
        ),
        None
    );
}

#[test]
fn accelerator_requires_shift_where_documented() {
    assert_eq!(Command::accelerator(Key::Char('k'), ctrl()), None);
    assert_eq!(Command::accelerator(Key::Char('w'), ctrl()), None);
}

#[test]
fn accelerator_returns_none_for_a_plain_character() {
    assert_eq!(Command::accelerator(Key::Char('a'), ctrl()), None);
}

#[test]
fn accelerator_returns_none_for_a_named_key() {
    assert_eq!(
        Command::accelerator(Key::Named(NamedKey::Enter), ctrl()),
        None
    );
    assert_eq!(
        Command::accelerator(Key::Named(NamedKey::Escape), ctrl_shift()),
        None
    );
}

#[test]
fn accelerator_returns_none_for_an_unrelated_ctrl_combination() {
    assert_eq!(Command::accelerator(Key::Char('z'), ctrl()), None);
    assert_eq!(Command::accelerator(Key::Char('q'), ctrl_shift()), None);
}

// --- ContextMenu::open -------------------------------------------------------

/// One row per [`Command::ALL`], in order: proven by keyboard activation
/// below, since [`ContextMenu`] exposes no public row accessor of its own.
#[test]
fn activating_a_row_by_keyboard_yields_the_matching_command() {
    let theme = Theme::dark();
    for (index, command) in Command::ALL.into_iter().enumerate() {
        let mut menu = ContextMenu::open(Point::new(10, 10));
        for _ in 0..=index {
            let outcome = menu.on_key(
                Key::Named(NamedKey::Down),
                viewport(),
                Scale::ONE,
                &theme,
                &mut damage::sink(),
            );
            assert_eq!(outcome, MenuOutcome::Changed);
        }
        let outcome = menu.on_key(
            Key::Named(NamedKey::Enter),
            viewport(),
            Scale::ONE,
            &theme,
            &mut damage::sink(),
        );
        assert_eq!(outcome, MenuOutcome::Chose(command), "row {index}");
    }
}

// --- ContextMenu::on_key ------------------------------------------------------

#[test]
fn escape_dismisses() {
    let theme = Theme::dark();
    let mut menu = ContextMenu::open(Point::new(10, 10));
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Escape),
            viewport(),
            Scale::ONE,
            &theme,
            &mut damage::sink()
        ),
        MenuOutcome::Dismissed
    );
}

// --- ContextMenu::on_pointer ---------------------------------------------------

#[test]
fn a_primary_press_outside_the_plate_dismisses_without_choosing() {
    let theme = Theme::dark();
    let anchor = Point::new(10, 10);
    let mut menu = ContextMenu::open(anchor);
    let bounds = menu.bounds(viewport(), Scale::ONE, &theme);
    // A point well outside the plate on every side.
    let outside = Point::new(bounds.right() + 50, bounds.bottom() + 50);

    let moved = InputEvent::PointerMoved { to: outside };
    assert_eq!(
        menu.on_pointer(&moved, viewport(), Scale::ONE, &theme, &mut damage::sink()),
        MenuOutcome::Ignored,
        "no row was highlighted and none is now, so nothing was redrawn"
    );
    let pressed = InputEvent::PointerPressed {
        button: PointerButton::Primary,
    };
    assert_eq!(
        menu.on_pointer(
            &pressed,
            viewport(),
            Scale::ONE,
            &theme,
            &mut damage::sink()
        ),
        MenuOutcome::Dismissed
    );
}

#[test]
fn a_press_inside_on_a_row_chooses_it() {
    let theme = Theme::dark();
    let anchor = Point::new(10, 10);
    let target_row = 2;
    let point = row_center(anchor, target_row, &theme);

    let mut menu = ContextMenu::open(anchor);
    let moved = InputEvent::PointerMoved { to: point };
    assert_eq!(
        menu.on_pointer(&moved, viewport(), Scale::ONE, &theme, &mut damage::sink()),
        MenuOutcome::Changed
    );
    let pressed = InputEvent::PointerPressed {
        button: PointerButton::Primary,
    };
    assert_eq!(
        menu.on_pointer(
            &pressed,
            viewport(),
            Scale::ONE,
            &theme,
            &mut damage::sink()
        ),
        MenuOutcome::Ignored,
        "the row was already highlighted, and a press draws nothing further"
    );
    let released = InputEvent::PointerReleased {
        button: PointerButton::Primary,
    };
    assert_eq!(
        menu.on_pointer(
            &released,
            viewport(),
            Scale::ONE,
            &theme,
            &mut damage::sink()
        ),
        MenuOutcome::Chose(Command::ALL[target_row])
    );
}

/// Sweeping the pointer across one row is the commonest thing a menu ever
/// sees, and every sample of it leaves the screen exactly as it was. The
/// caller repaints on [`MenuOutcome::Changed`], so reporting one here would
/// re-render and re-publish the whole plate per pointer sample.
#[test]
fn moving_within_the_highlighted_row_changes_nothing() {
    let theme = Theme::dark();
    let anchor = Point::new(10, 10);
    let center = row_center(anchor, 1, &theme);
    let mut menu = ContextMenu::open(anchor);

    let mut arriving = damage::sink();
    assert_eq!(
        menu.on_pointer(
            &InputEvent::PointerMoved { to: center },
            viewport(),
            Scale::ONE,
            &theme,
            &mut arriving,
        ),
        MenuOutcome::Changed,
        "arriving on the row highlights it"
    );
    assert!(!arriving.is_empty(), "the highlighted row was reported");

    // Every further sample inside the same row: the exact same point, and one
    // a pixel away that is still the same row.
    for to in [center, Point::new(center.x + 1, center.y)] {
        let mut damage = damage::sink();
        assert_eq!(
            menu.on_pointer(
                &InputEvent::PointerMoved { to },
                viewport(),
                Scale::ONE,
                &theme,
                &mut damage,
            ),
            MenuOutcome::Ignored,
            "a sample inside the highlighted row asked for a repaint"
        );
        assert!(damage.is_empty(), "nothing was reported for it either");
    }
}

/// Crossing into another row genuinely changes two rows, and only those two:
/// the highlight leaves one and arrives on the other.
#[test]
fn crossing_into_another_row_reports_only_the_two_rows() {
    let theme = Theme::dark();
    let anchor = Point::new(10, 10);
    let mut menu = ContextMenu::open(anchor);
    let plate = menu.bounds(viewport(), Scale::ONE, &theme);
    let first = row_center(anchor, 0, &theme);
    let next = row_center(anchor, 1, &theme);

    menu.on_pointer(
        &InputEvent::PointerMoved { to: first },
        viewport(),
        Scale::ONE,
        &theme,
        &mut damage::sink(),
    );
    let mut damage = damage::sink();
    assert_eq!(
        menu.on_pointer(
            &InputEvent::PointerMoved { to: next },
            viewport(),
            Scale::ONE,
            &theme,
            &mut damage,
        ),
        MenuOutcome::Changed
    );

    let reported = damage.bounds();
    assert!(!reported.is_empty(), "the two rows were reported");
    assert!(
        reported.height < plate.height,
        "two rows ({}) reported as tall as the whole plate ({})",
        reported.height,
        plate.height
    );
    assert!(
        plate.intersection(&reported) == reported,
        "the reported rows lie inside the plate"
    );
}

// --- ContextMenu::bounds -------------------------------------------------------

#[test]
fn the_menus_rect_stays_inside_the_viewport_when_opened_at_a_corner() {
    let theme = Theme::dark();
    let view = viewport();
    let corner = Point::new(view.right(), view.bottom());
    let menu = ContextMenu::open(corner);
    let bounds = menu.bounds(view, Scale::ONE, &theme);

    assert!(bounds.left() >= view.left());
    assert!(bounds.top() >= view.top());
    assert!(bounds.right() <= view.right());
    assert!(bounds.bottom() <= view.bottom());
}

/// The menu is desktop furniture, so its plate is measured with the *desktop's*
/// interface face named by the active theme — never the terminal's own
/// monospace grid face at the user's terminal text size.
///
/// The regression: the app used to hand the shared menu the very face it draws
/// its screen cells with, so the menu rendered monospace, resized with the
/// profile's text-size setting, and read nothing like the same menu on the
/// pinboard or in the file manager. With the face resolved from the theme, the
/// theme's own type ladder is the only thing that can move it.
#[test]
fn the_plate_follows_the_desktop_face_not_the_terminal_grid() {
    let small = text_ladder(Fonts::MIN_BASE_SIZE_PX);
    let large = text_ladder(LARGER_BASE_SIZE_PX);
    let menu = ContextMenu::open(Point::new(0, 0));
    // Wide enough that neither plate is clamped by the viewport instead.
    let room = Rect::new(0, 0, 4000, 3000);

    let narrow = menu.bounds(room, Scale::ONE, &small);
    let wide = menu.bounds(room, Scale::ONE, &large);
    assert!(
        wide.width > narrow.width && wide.height > narrow.height,
        "the desktop's type ladder must size the terminal's menu"
    );
}
