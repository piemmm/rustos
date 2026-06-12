//! Unit tests for the boot-keyboard console-input producer: HID-usage→`Key`
//! resolution, modifier / lock state, and the full decode→keymap→sink chain.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::*;
use crate::keyboard::MODIFIER_USAGE_BASE;
use crate::{BootKeyboard, ReportSource};

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

/// Feed one key press (usage `code`) and return the console bytes it produced.
fn press(console: &mut KeyboardConsole, code: u16) -> Vec<u8> {
    let mut out = [0u8; MAX_KEY_BYTES];
    let n = console
        .feed(key_event(code, 1), &mut out)
        .expect("buffer fits MAX_KEY_BYTES");
    out[..n].to_vec()
}

/// Hold or release a modifier usage (`0xE0 + n`), asserting it produces no
/// bytes.
fn modifier(console: &mut KeyboardConsole, code: u16, down: bool) {
    let mut out = [0u8; MAX_KEY_BYTES];
    assert_eq!(
        console.feed(key_event(code, i32::from(down)), &mut out),
        Ok(0)
    );
}

#[test]
fn lowercase_letter_is_itself() {
    let mut console = KeyboardConsole::new();
    assert_eq!(press(&mut console, USAGE_A), b"a");
}

#[test]
fn shift_held_uppercases_a_letter() {
    let mut console = KeyboardConsole::new();
    modifier(&mut console, MODIFIER_USAGE_BASE + 1, true); // LeftShift down
    assert_eq!(press(&mut console, USAGE_A), b"A");
    modifier(&mut console, MODIFIER_USAGE_BASE + 1, false); // LeftShift up
    assert_eq!(press(&mut console, USAGE_A), b"a");
}

#[test]
fn caps_lock_uppercases_letters_only() {
    let mut console = KeyboardConsole::new();
    press(&mut console, 0x39); // Caps Lock press toggles it on
    assert_eq!(press(&mut console, USAGE_A), b"A");
    // A digit is unaffected by caps lock.
    assert_eq!(press(&mut console, 0x1E), b"1");
    // Shift with caps lock cancels for letters.
    modifier(&mut console, MODIFIER_USAGE_BASE + 5, true); // RightShift down
    assert_eq!(press(&mut console, USAGE_A), b"a");
}

#[test]
fn ctrl_letter_sends_a_control_code() {
    let mut console = KeyboardConsole::new();
    modifier(&mut console, MODIFIER_USAGE_BASE, true); // LeftControl down
    assert_eq!(press(&mut console, 0x06), &[0x03]); // Ctrl-C (usage 0x06 = 'c')
}

#[test]
fn shifted_symbols_use_the_us_layout() {
    let mut console = KeyboardConsole::new();
    assert_eq!(press(&mut console, 0x1F), b"2"); // '2' key, unshifted
    modifier(&mut console, MODIFIER_USAGE_BASE + 1, true); // shift
    assert_eq!(press(&mut console, 0x1F), b"@"); // Shift-2 = '@'
    assert_eq!(press(&mut console, 0x2D), b"_"); // Shift-'-' = '_'
}

#[test]
fn named_keys_send_their_terminal_sequences() {
    let mut console = KeyboardConsole::new();
    assert_eq!(press(&mut console, 0x28), b"\r"); // Enter → CR
    assert_eq!(press(&mut console, 0x2A), &[0x7f]); // Backspace → DEL
    assert_eq!(press(&mut console, 0x2B), b"\t"); // Tab → HT
    assert_eq!(press(&mut console, 0x52), b"\x1b[A"); // Up arrow
    assert_eq!(press(&mut console, 0x4C), b"\x1b[3~"); // Delete
    assert_eq!(press(&mut console, 0x3A), b"\x1bOP"); // F1
}

#[test]
fn num_lock_switches_the_keypad_between_digits_and_navigation() {
    let mut console = KeyboardConsole::new();
    // Num Lock defaults off: KP1 is End, and KP5 sends nothing.
    assert_eq!(press(&mut console, 0x59), b"\x1b[4~"); // End
    assert_eq!(press(&mut console, 0x5D), b""); // KP5 with num lock off
    press(&mut console, 0x53); // Num Lock on
    assert_eq!(press(&mut console, 0x59), b"1"); // KP1 digit
    assert_eq!(press(&mut console, 0x5D), b"5"); // KP5 digit
}

#[test]
fn release_and_non_key_events_produce_nothing() {
    let mut console = KeyboardConsole::new();
    let mut out = [0u8; MAX_KEY_BYTES];
    // A release edge of a printable key.
    assert_eq!(console.feed(key_event(USAGE_A, 0), &mut out), Ok(0));
    // A pointer event is not keyboard input.
    let pointer = InputEvent {
        kind: InputEventKind::Pointer,
        reserved0: 0,
        code: 0,
        value: 5,
    };
    assert_eq!(console.feed(pointer, &mut out), Ok(0));
}

#[test]
fn unknown_usage_fails_closed() {
    let mut console = KeyboardConsole::new();
    assert_eq!(press(&mut console, 0xFF), b"");
}

#[test]
fn feed_rejects_a_buffer_below_max_key_bytes() {
    let mut console = KeyboardConsole::new();
    let mut tiny = [0u8; 1];
    assert_eq!(
        console.feed(key_event(0x52, 1), &mut tiny), // Up arrow needs 3 bytes
        Err(DriverError::BufferTooSmall)
    );
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

/// Recording [`ConsoleSink`]: accumulates every injected byte.
struct Recorder {
    bytes: Vec<u8>,
}

impl ConsoleSink for Recorder {
    fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

fn kbd_report(keys: &[u8]) -> Vec<u8> {
    let mut report = alloc::vec![0u8; 8];
    report[2..2 + keys.len()].copy_from_slice(keys);
    report
}

#[test]
fn pump_drives_the_full_decode_keymap_chain() {
    // Type "hi": 'h' (usage 0x0B) then 'i' (0x0C), each released in turn.
    let mut queue = VecDeque::new();
    queue.push_back(kbd_report(&[0x0B])); // h down
    queue.push_back(kbd_report(&[0x0C])); // h up, i down
    queue.push_back(kbd_report(&[])); // i up
    let mut keyboard = BootKeyboard::new(MockReports { queue });
    let mut console = KeyboardConsole::new();
    let mut sink = Recorder { bytes: Vec::new() };

    // Drain until the source is exhausted.
    while pump_once(&mut keyboard, &mut console, &mut sink).expect("pump") > 0 {}

    assert_eq!(sink.bytes, b"hi");
}
