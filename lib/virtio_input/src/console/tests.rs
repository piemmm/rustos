//! Unit tests for the virtio-input keyboard console producer.

use super::*;
use rustos_abi::input::{KeyInput, KeyValue, NamedKeyCode};

/// Build one keyboard [`InputEvent`] for `keycode` with the given `value`
/// (`1` = press, `0` = release, `2` = repeat).
fn key(keycode: u16, value: i32) -> InputEvent {
    InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: keycode,
        value,
    }
}

/// The [`KeyValue`] a `Pressed`/`Released` record carries, failing the test
/// if `feed` produced nothing.
fn pressed_value(record: Option<KeyInput>) -> KeyValue {
    match record.expect("a record was produced") {
        KeyInput::Pressed { key, .. } => key,
        KeyInput::Released { .. } => panic!("expected a press record"),
    }
}

#[test]
fn plain_letter_press_and_release_resolve_to_the_char() {
    let mut kbd = VirtioKeyboardConsole::new();
    // KEY_H = 35.
    assert_eq!(pressed_value(kbd.feed(key(35, 1))), KeyValue::Char('h'));
    match kbd.feed(key(35, 0)).expect("release record") {
        KeyInput::Released { key, .. } => assert_eq!(key, KeyValue::Char('h')),
        KeyInput::Pressed { .. } => panic!("expected a release record"),
    }
}

#[test]
fn shift_held_uppercases_the_letter_and_emits_no_record_itself() {
    let mut kbd = VirtioKeyboardConsole::new();
    // Pressing Left Shift (42) is a modifier edge: no record.
    assert_eq!(kbd.feed(key(42, 1)), None);
    // KEY_A = 30 with shift held → 'A', and the record carries shift.
    let record = kbd.feed(key(30, 1)).expect("record");
    assert_eq!(record.key(), KeyValue::Char('A'));
    assert!(record.modifiers().shift);
}

#[test]
fn caps_lock_toggles_letter_case_and_shift_xor_caps_holds() {
    let mut kbd = VirtioKeyboardConsole::new();
    // Caps Lock (58) press toggles state, no record.
    assert_eq!(kbd.feed(key(58, 1)), None);
    assert_eq!(pressed_value(kbd.feed(key(30, 1))), KeyValue::Char('A'));
    // Shift while caps is on → back to lowercase (shift XOR caps).
    assert_eq!(kbd.feed(key(42, 1)), None);
    assert_eq!(pressed_value(kbd.feed(key(30, 1))), KeyValue::Char('a'));
}

#[test]
fn shifted_digit_resolves_to_its_symbol() {
    let mut kbd = VirtioKeyboardConsole::new();
    assert_eq!(kbd.feed(key(42, 1)), None); // shift down
    assert_eq!(pressed_value(kbd.feed(key(2, 1))), KeyValue::Char('!')); // KEY_1
}

#[test]
fn named_keys_resolve() {
    let mut kbd = VirtioKeyboardConsole::new();
    for (keycode, named) in [
        (28u16, NamedKeyCode::Enter),
        (96, NamedKeyCode::Enter), // keypad Enter
        (1, NamedKeyCode::Escape),
        (14, NamedKeyCode::Backspace),
        (15, NamedKeyCode::Tab),
        (103, NamedKeyCode::Up),
        (105, NamedKeyCode::Left),
        (111, NamedKeyCode::Delete),
    ] {
        assert_eq!(
            pressed_value(kbd.feed(key(keycode, 1))),
            KeyValue::Named(named),
            "keycode {keycode}"
        );
    }
}

#[test]
fn function_keys_span_f1_through_f12() {
    let mut kbd = VirtioKeyboardConsole::new();
    // F1 = 59, F10 = 68 (contiguous), F11 = 87, F12 = 88.
    assert_eq!(
        pressed_value(kbd.feed(key(59, 1))),
        KeyValue::Named(NamedKeyCode::F1)
    );
    assert_eq!(
        pressed_value(kbd.feed(key(68, 1))),
        KeyValue::Named(NamedKeyCode::F10)
    );
    assert_eq!(
        pressed_value(kbd.feed(key(87, 1))),
        KeyValue::Named(NamedKeyCode::F11)
    );
    assert_eq!(
        pressed_value(kbd.feed(key(88, 1))),
        KeyValue::Named(NamedKeyCode::F12)
    );
}

#[test]
fn left_right_modifier_pair_stays_held_until_both_release() {
    let mut kbd = VirtioKeyboardConsole::new();
    // Hold both control keys, release the left one, control is still held.
    assert_eq!(kbd.feed(key(29, 1)), None); // left ctrl down
    assert_eq!(kbd.feed(key(97, 1)), None); // right ctrl down
    assert_eq!(kbd.feed(key(29, 0)), None); // left ctrl up
    let record = kbd.feed(key(46, 1)).expect("record"); // KEY_C
    assert!(record.modifiers().ctrl);
    // Releasing the right one too clears control.
    assert_eq!(kbd.feed(key(97, 0)), None);
    let record = kbd.feed(key(46, 1)).expect("record");
    assert!(!record.modifiers().ctrl);
}

#[test]
fn numeric_keypad_honours_num_lock() {
    let mut kbd = VirtioKeyboardConsole::new();
    // Num Lock off: KEY_KP7 (71) is the Home navigation key.
    assert_eq!(
        pressed_value(kbd.feed(key(71, 1))),
        KeyValue::Named(NamedKeyCode::Home)
    );
    // KEY_KP5 (76) sends nothing with Num Lock off.
    assert_eq!(kbd.feed(key(76, 1)), None);
    // Turn Num Lock on (69): now KP7 is the digit '7' and KP5 is '5'.
    assert_eq!(kbd.feed(key(69, 1)), None);
    assert_eq!(pressed_value(kbd.feed(key(71, 1))), KeyValue::Char('7'));
    assert_eq!(pressed_value(kbd.feed(key(76, 1))), KeyValue::Char('5'));
}

#[test]
fn non_key_events_and_unknown_or_repeat_edges_produce_nothing() {
    let mut kbd = VirtioKeyboardConsole::new();
    // A pointer event is not a key edge.
    let pointer = InputEvent {
        kind: InputEventKind::Pointer,
        reserved0: 0,
        code: 0,
        value: 5,
    };
    assert_eq!(kbd.feed(pointer), None);
    // An unmapped keycode.
    assert_eq!(kbd.feed(key(0xFFFF, 1)), None);
    // A key-repeat edge (`value == 2`) carries no press/release.
    assert_eq!(kbd.feed(key(35, 2)), None);
}
