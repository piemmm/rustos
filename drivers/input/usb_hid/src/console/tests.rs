//! Unit tests for the boot-keyboard console-input producer: HID-usage→`Key`
//! resolution, modifier / lock state, and the full decode→record→sink chain.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::*;
use crate::keyboard::MODIFIER_USAGE_BASE;
use crate::{BootKeyboard, ReportSource};
use rustos_abi::input::{KeyValue, Modifiers as AbiModifiers, NamedKeyCode};

/// HID usage of the `A` key (page `0x07`); letters run `0x04`..`0x1D`.
const USAGE_A: u16 = 0x04;

fn key_event(code: u16, value: i32) -> InputEvent {
    InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code,
        value,
    }
}

/// Feed one key press (usage `code`) and return the [`KeyInput`] record it
/// produced.
fn press(console: &mut KeyboardConsole, code: u16) -> KeyInput {
    console
        .feed(key_event(code, 1))
        .expect("a resolvable key press produces a record")
}

/// The [`KeyValue`] a key press resolves to.
fn press_value(console: &mut KeyboardConsole, code: u16) -> KeyValue {
    press(console, code).key()
}

/// Hold or release a modifier usage (`0xE0 + n`), asserting it produces no
/// record (modifier state is folded into the next key edge).
fn modifier(console: &mut KeyboardConsole, code: u16, down: bool) {
    assert_eq!(console.feed(key_event(code, i32::from(down))), None);
}

#[test]
fn lowercase_letter_is_itself() {
    let mut console = KeyboardConsole::new();
    assert_eq!(press_value(&mut console, USAGE_A), KeyValue::Char('a'));
}

#[test]
fn shift_held_uppercases_a_letter_and_marks_the_modifier() {
    let mut console = KeyboardConsole::new();
    modifier(&mut console, MODIFIER_USAGE_BASE + 1, true); // LeftShift down
    let record = press(&mut console, USAGE_A);
    assert_eq!(record.key(), KeyValue::Char('A'));
    assert_eq!(
        record.modifiers(),
        AbiModifiers {
            shift: true,
            ..AbiModifiers::default()
        }
    );
    modifier(&mut console, MODIFIER_USAGE_BASE + 1, false); // LeftShift up
    assert_eq!(press_value(&mut console, USAGE_A), KeyValue::Char('a'));
}

#[test]
fn caps_lock_uppercases_letters_only() {
    let mut console = KeyboardConsole::new();
    // The Caps Lock key itself produces no record, only state.
    assert_eq!(console.feed(key_event(0x39, 1)), None);
    assert_eq!(press_value(&mut console, USAGE_A), KeyValue::Char('A'));
    // A digit is unaffected by caps lock.
    assert_eq!(press_value(&mut console, 0x1E), KeyValue::Char('1'));
    // Shift with caps lock cancels for letters.
    modifier(&mut console, MODIFIER_USAGE_BASE + 5, true); // RightShift down
    assert_eq!(press_value(&mut console, USAGE_A), KeyValue::Char('a'));
}

#[test]
fn ctrl_letter_carries_the_ctrl_modifier() {
    let mut console = KeyboardConsole::new();
    modifier(&mut console, MODIFIER_USAGE_BASE, true); // LeftControl down
                                                       // usage 0x06 = 'c'; the control-code encoding now lives in the kernel
                                                       // arbiter, so the driver emits the character plus the held ctrl modifier.
    let record = press(&mut console, 0x06);
    assert_eq!(record.key(), KeyValue::Char('c'));
    assert_eq!(
        record.modifiers(),
        AbiModifiers {
            ctrl: true,
            ..AbiModifiers::default()
        }
    );
}

#[test]
fn shifted_symbols_use_the_us_layout() {
    let mut console = KeyboardConsole::new();
    assert_eq!(press_value(&mut console, 0x1F), KeyValue::Char('2')); // '2', unshifted
    modifier(&mut console, MODIFIER_USAGE_BASE + 1, true); // shift
    assert_eq!(press_value(&mut console, 0x1F), KeyValue::Char('@')); // Shift-2 = '@'
    assert_eq!(press_value(&mut console, 0x2D), KeyValue::Char('_')); // Shift-'-' = '_'
}

#[test]
fn named_keys_resolve_to_their_codes() {
    let mut console = KeyboardConsole::new();
    assert_eq!(
        press_value(&mut console, 0x28),
        KeyValue::Named(NamedKeyCode::Enter)
    );
    assert_eq!(
        press_value(&mut console, 0x2A),
        KeyValue::Named(NamedKeyCode::Backspace)
    );
    assert_eq!(
        press_value(&mut console, 0x2B),
        KeyValue::Named(NamedKeyCode::Tab)
    );
    assert_eq!(
        press_value(&mut console, 0x52),
        KeyValue::Named(NamedKeyCode::Up)
    );
    assert_eq!(
        press_value(&mut console, 0x4C),
        KeyValue::Named(NamedKeyCode::Delete)
    );
    assert_eq!(
        press_value(&mut console, 0x3A),
        KeyValue::Named(NamedKeyCode::F1)
    );
}

#[test]
fn num_lock_switches_the_keypad_between_digits_and_navigation() {
    let mut console = KeyboardConsole::new();
    // Num Lock defaults off: KP1 is End, and KP5 produces no record.
    assert_eq!(
        press_value(&mut console, 0x59),
        KeyValue::Named(NamedKeyCode::End)
    );
    assert_eq!(console.feed(key_event(0x5D, 1)), None); // KP5 with num lock off
    assert_eq!(console.feed(key_event(0x53, 1)), None); // Num Lock on (no record)
    assert_eq!(press_value(&mut console, 0x59), KeyValue::Char('1')); // KP1 digit
    assert_eq!(press_value(&mut console, 0x5D), KeyValue::Char('5')); // KP5 digit
}

#[test]
fn a_release_of_a_printable_key_is_a_released_record() {
    let mut console = KeyboardConsole::new();
    let record = console
        .feed(key_event(USAGE_A, 0))
        .expect("a release of a printable key produces a record");
    assert_eq!(
        record,
        KeyInput::Released {
            key: KeyValue::Char('a'),
            modifiers: AbiModifiers::default(),
        }
    );
}

#[test]
fn a_non_key_event_produces_no_record() {
    let mut console = KeyboardConsole::new();
    let pointer = InputEvent {
        kind: InputEventKind::Pointer,
        reserved0: 0,
        code: 0,
        value: 5,
    };
    assert_eq!(console.feed(pointer), None);
}

#[test]
fn unknown_usage_fails_closed() {
    let mut console = KeyboardConsole::new();
    assert_eq!(console.feed(key_event(0xFF, 1)), None);
}

/// Mock interrupt-IN endpoint: a FIFO of boot keyboard reports.
struct MockReports {
    queue: VecDeque<Vec<u8>>,
}

impl ReportSource for MockReports {
    fn next_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
        let Some(report) = self.queue.pop_front() else {
            return Ok(None);
        };
        let n = report.len().min(buf.len());
        buf[..n].copy_from_slice(&report[..n]);
        Ok(Some(n))
    }
}

/// Recording [`ConsoleSink`]: decodes and accumulates every injected record.
struct Recorder {
    records: Vec<KeyInput>,
}

impl ConsoleSink for Recorder {
    fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        // The producer always writes exactly one whole record per call.
        let record = KeyInput::from_bytes(bytes).expect("a whole, valid record");
        self.records.push(record);
        Ok(())
    }
}

fn kbd_report(keys: &[u8]) -> Vec<u8> {
    let mut report = alloc::vec![0u8; 8];
    report[2..2 + keys.len()].copy_from_slice(keys);
    report
}

#[test]
fn pump_drives_the_full_decode_record_chain() {
    // Type "hi": 'h' (usage 0x0B) then 'i' (0x0C), each released in turn.
    let mut queue = VecDeque::new();
    queue.push_back(kbd_report(&[0x0B])); // h down
    queue.push_back(kbd_report(&[0x0C])); // h up, i down
    queue.push_back(kbd_report(&[])); // i up
    let mut keyboard = BootKeyboard::new(MockReports { queue });
    let mut console = KeyboardConsole::new();
    let mut sink = Recorder {
        records: Vec::new(),
    };

    // Drain until the source is exhausted.
    while pump_once(&mut keyboard, &mut console, &mut sink).expect("pump") > 0 {}

    // Every edge is delivered as a record; the *presses* spell "hi".
    let typed: Vec<char> = sink
        .records
        .iter()
        .filter_map(|record| match record {
            KeyInput::Pressed {
                key: KeyValue::Char(c),
                ..
            } => Some(*c),
            _ => None,
        })
        .collect();
    assert_eq!(typed, ['h', 'i']);
    // Both key-up edges were emitted too (key-down + key-up for each key).
    let releases = sink
        .records
        .iter()
        .filter(|record| matches!(record, KeyInput::Released { .. }))
        .count();
    assert_eq!(releases, 2);
}
