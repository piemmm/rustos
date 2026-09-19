//! Held keys are state, commands fire once, and a diagonal is a
//! direction the protocol will accept.

use super::*;
use tairix_abi::input::Modifiers;
use tairix_wintersun_net::bounds::MAX_DIRECTION_MAGNITUDE_SQ;

fn press(key: KeyValue) -> KeyInput {
    KeyInput::Pressed {
        key,
        modifiers: Modifiers::default(),
    }
}

fn release(key: KeyValue) -> KeyInput {
    KeyInput::Released {
        key,
        modifiers: Modifiers::default(),
    }
}

fn arrow(code: NamedKeyCode) -> KeyValue {
    KeyValue::Named(code)
}

fn magnitude_sq(direction: Direction) -> i64 {
    i64::from(direction.x()) * i64::from(direction.x())
        + i64::from(direction.y()) * i64::from(direction.y())
}

#[test]
fn nothing_held_is_standing_still() {
    let controls = Controls::new();
    assert_eq!(controls.direction(), Direction::still());
    assert_eq!(controls.pointer(), None);
}

#[test]
fn a_movement_key_is_state_and_commands_nothing() {
    let mut controls = Controls::new();
    assert_eq!(controls.apply_key(&press(arrow(NamedKeyCode::Right))), None);
    assert!(controls.direction().x() > 0);
    assert_eq!(controls.direction().y(), 0);
    assert_eq!(
        controls.apply_key(&release(arrow(NamedKeyCode::Right))),
        None
    );
    assert_eq!(controls.direction(), Direction::still());
}

#[test]
fn both_key_clusters_move_the_same_way() {
    for (named, letter) in [
        (NamedKeyCode::Up, 'w'),
        (NamedKeyCode::Down, 's'),
        (NamedKeyCode::Left, 'a'),
        (NamedKeyCode::Right, 'd'),
    ] {
        let mut arrows = Controls::new();
        arrows.apply_key(&press(arrow(named)));
        let mut letters = Controls::new();
        letters.apply_key(&press(KeyValue::Char(letter)));
        assert_eq!(
            arrows.direction(),
            letters.direction(),
            "{named:?} and '{letter}' disagreed"
        );
        let mut upper = Controls::new();
        upper.apply_key(&press(KeyValue::Char(letter.to_ascii_uppercase())));
        assert_eq!(
            upper.direction(),
            letters.direction(),
            "shift changed a heading"
        );
    }
}

#[test]
fn opposing_keys_cancel() {
    let mut controls = Controls::new();
    controls.apply_key(&press(arrow(NamedKeyCode::Left)));
    controls.apply_key(&press(arrow(NamedKeyCode::Right)));
    assert_eq!(controls.direction(), Direction::still());
}

#[test]
fn a_diagonal_is_normalised_and_the_protocol_accepts_it() {
    let mut controls = Controls::new();
    controls.apply_key(&press(arrow(NamedKeyCode::Right)));
    let cardinal = magnitude_sq(controls.direction());
    controls.apply_key(&press(arrow(NamedKeyCode::Down)));
    let diagonal = magnitude_sq(controls.direction());

    assert!(
        diagonal <= MAX_DIRECTION_MAGNITUDE_SQ,
        "a diagonal of {diagonal} is past the magnitude the wire admits"
    );
    // Within a per-mille of the cardinal speed: a diagonal that is
    // faster is the classic bug, and one that is much slower is a
    // different one.
    let ratio = diagonal * 1000 / cardinal.max(1);
    assert!(
        (995..=1000).contains(&ratio),
        "a diagonal moves at {ratio}/1000 of a cardinal's speed"
    );
}

#[test]
fn every_held_combination_produces_a_direction_the_protocol_accepts() {
    let keys = [
        NamedKeyCode::Up,
        NamedKeyCode::Down,
        NamedKeyCode::Left,
        NamedKeyCode::Right,
    ];
    for mask in 0..16u8 {
        let mut controls = Controls::new();
        for (bit, key) in keys.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                controls.apply_key(&press(arrow(*key)));
            }
        }
        let direction = controls.direction();
        assert!(
            magnitude_sq(direction) <= MAX_DIRECTION_MAGNITUDE_SQ,
            "held mask {mask:#06b} produced {direction:?}, which the wire refuses"
        );
        assert!(
            Direction::new(direction.x(), direction.y()).is_ok(),
            "held mask {mask:#06b} fell back to standing still"
        );
    }
}

#[test]
fn a_command_key_fires_on_the_press_and_not_the_release() {
    let mut controls = Controls::new();
    assert_eq!(
        controls.apply_key(&press(arrow(NamedKeyCode::F11))),
        Some(Command::Resize(WindowSizeState::Fullscreen))
    );
    assert_eq!(controls.apply_key(&release(arrow(NamedKeyCode::F11))), None);
    assert_eq!(
        controls.apply_key(&press(arrow(NamedKeyCode::Escape))),
        Some(Command::Resize(WindowSizeState::Restored))
    );
    assert_eq!(
        controls.apply_key(&press(KeyValue::Char('+'))),
        Some(Command::Zoom(Zoom::In))
    );
    assert_eq!(
        controls.apply_key(&press(KeyValue::Char('-'))),
        Some(Command::Zoom(Zoom::Out))
    );
    assert_eq!(
        controls.apply_key(&press(KeyValue::Char('q'))),
        Some(Command::Quit)
    );
}

#[test]
fn a_bare_modifier_change_moves_and_commands_nothing() {
    let mut controls = Controls::new();
    controls.apply_key(&press(arrow(NamedKeyCode::Right)));
    let held = controls.direction();
    assert_eq!(
        controls.apply_key(&KeyInput::ModifiersChanged {
            modifiers: Modifiers::default(),
        }),
        None
    );
    assert_eq!(
        controls.direction(),
        held,
        "a modifier let go of a held key"
    );
}

#[test]
fn losing_focus_lets_go_of_everything() {
    let mut controls = Controls::new();
    controls.apply_key(&press(arrow(NamedKeyCode::Right)));
    controls.apply_key(&press(arrow(NamedKeyCode::Down)));
    controls.release_all();
    assert_eq!(
        controls.direction(),
        Direction::still(),
        "a backgrounded client kept walking"
    );
}

#[test]
fn a_pointer_event_records_where_it_was() {
    let mut controls = Controls::new();
    assert_eq!(controls.apply_pointer(12, 34, PointerAction::Moved), None);
    assert_eq!(controls.pointer(), Some((12, 34)));
}
