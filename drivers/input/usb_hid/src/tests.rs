//! Unit tests for the USB-HID boot-protocol decoders against a mock
//! [`ReportSource`] queue (mirrors the `ps2` / `emmc2` mock seams).

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;

use core::cell::RefCell;

use super::keyboard::{BOOT_KEYBOARD_REPORT_LEN, MODIFIER_USAGE_BASE};
use super::mouse::BUTTON_CODE_BASE;
use super::*;
use rustos_abi::driver::input::Input;
use rustos_abi::driver::DriverKind;

/// Mock interrupt-IN endpoint: a FIFO of variable-length reports.
struct MockSource {
    queue: VecDeque<Vec<u8>>,
    /// When set, `next_report` claims this length regardless of the
    /// front report's true size (models a transport-contract breach).
    forged_len: Option<usize>,
    fail: bool,
}

impl MockSource {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            forged_len: None,
            fail: false,
        }
    }

    fn push(&mut self, report: &[u8]) {
        self.queue.push_back(report.to_vec());
    }

    fn pending(&self) -> usize {
        self.queue.len()
    }
}

impl ReportSource for MockSource {
    fn next_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
        if self.fail {
            return Err(DriverError::DeviceFault);
        }
        let Some(report) = self.queue.pop_front() else {
            return Ok(None);
        };
        if let Some(len) = self.forged_len {
            return Ok(Some(len));
        }
        let n = report.len().min(buf.len());
        buf[..n].copy_from_slice(&report[..n]);
        Ok(Some(n))
    }
}

/// A [`MockSource`] handle the test keeps after handing the source to
/// a decoder, for inspecting the undrained queue.
struct SharedSource(Rc<RefCell<MockSource>>);

impl ReportSource for SharedSource {
    fn next_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
        self.0.borrow_mut().next_report(buf)
    }
}

/// Mock driver host modelling the load-time `CAP_DRV_LOAD` grant.
struct MockHost {
    drv_load: bool,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => self.drv_load,
            _ => false,
        }
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

fn key(code: u16, value: i32) -> InputEvent {
    InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code,
        value,
    }
}

fn pointer(axis: u16, value: i32) -> InputEvent {
    InputEvent {
        kind: InputEventKind::Pointer,
        reserved0: 0,
        code: axis,
        value,
    }
}

fn scroll(axis: u16, value: i32) -> InputEvent {
    InputEvent {
        kind: InputEventKind::Scroll,
        reserved0: 0,
        code: axis,
        value,
    }
}

/// Boot keyboard report with `mods` and up to six key usages.
fn kbd_report(mods: u8, keys: &[u8]) -> [u8; BOOT_KEYBOARD_REPORT_LEN] {
    let mut report = [0u8; BOOT_KEYBOARD_REPORT_LEN];
    report[0] = mods;
    report[2..2 + keys.len()].copy_from_slice(keys);
    report
}

#[test]
fn register_requires_drv_load() {
    assert!(register(&MockHost { drv_load: true }).is_ok());
    assert_eq!(
        register(&MockHost { drv_load: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn keyboard_poll_rejects_empty_buffer() {
    let mut kbd = BootKeyboard::new(MockSource::new());
    let mut empty: [InputEvent; 0] = [];
    assert_eq!(kbd.poll(&mut empty), Err(DriverError::BufferTooSmall));
}

#[test]
fn keyboard_poll_returns_zero_when_idle() {
    let mut kbd = BootKeyboard::new(MockSource::new());
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(0));
}

#[test]
fn keyboard_decodes_press_and_release() {
    let mut src = MockSource::new();
    src.push(&kbd_report(0, &[0x04])); // A down
    src.push(&kbd_report(0, &[])); // A up
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(0x04, 1));
    assert_eq!(out[1], key(0x04, 0));
}

#[test]
fn keyboard_emits_one_edge_per_held_key() {
    let mut src = MockSource::new();
    src.push(&kbd_report(0, &[0x04]));
    src.push(&kbd_report(0, &[0x04])); // still held: no new edge
    src.push(&kbd_report(0, &[0x04, 0x05])); // B joins
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 8];
    assert_eq!(kbd.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(0x04, 1));
    assert_eq!(out[1], key(0x05, 1));
}

#[test]
fn keyboard_decodes_modifier_edges() {
    let mut src = MockSource::new();
    src.push(&kbd_report(0b0000_0010, &[])); // LeftShift down
    src.push(&kbd_report(0b1000_0010, &[])); // RightGUI joins
    src.push(&kbd_report(0, &[])); // both released
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 8];
    assert_eq!(kbd.poll(&mut out), Ok(4));
    assert_eq!(out[0], key(MODIFIER_USAGE_BASE + 1, 1));
    assert_eq!(out[1], key(MODIFIER_USAGE_BASE + 7, 1));
    assert_eq!(out[2], key(MODIFIER_USAGE_BASE + 1, 0));
    assert_eq!(out[3], key(MODIFIER_USAGE_BASE + 7, 0));
}

#[test]
fn keyboard_rollover_keeps_keys_and_diffs_modifiers() {
    let mut src = MockSource::new();
    src.push(&kbd_report(0, &[0x04]));
    // Rollover: array all-0x01, modifiers gain LeftControl. The held
    // 'A' must not be released, and no phantom keys appear.
    src.push(&kbd_report(0b0000_0001, &[0x01; 6]));
    // Recovery: 'A' still held, control still down.
    src.push(&kbd_report(0b0000_0001, &[0x04]));
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 8];
    assert_eq!(kbd.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(0x04, 1));
    assert_eq!(out[1], key(MODIFIER_USAGE_BASE, 1));
}

#[test]
fn keyboard_duplicate_usage_presses_once() {
    let mut src = MockSource::new();
    src.push(&kbd_report(0, &[0x04, 0x04, 0x04]));
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 8];
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(0x04, 1));
}

#[test]
fn keyboard_rejects_short_report() {
    let mut src = MockSource::new();
    src.push(&[0u8; 7]);
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Err(DriverError::LengthOutOfRange));
}

#[test]
fn keyboard_rejects_forged_source_length() {
    let mut src = MockSource::new();
    src.push(&kbd_report(0, &[0x04]));
    src.forged_len = Some(REPORT_BUF_LEN + 1);
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Err(DriverError::DeviceFault));
}

#[test]
fn keyboard_propagates_source_fault() {
    let mut src = MockSource::new();
    src.fail = true;
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Err(DriverError::DeviceFault));
}

#[test]
fn keyboard_latches_overflow_across_polls() {
    let mut src = MockSource::new();
    // One report producing three edges: two presses + one modifier.
    src.push(&kbd_report(0b0000_0001, &[0x04, 0x05]));
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 1];
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(0x04, 1));
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(0x05, 1));
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(MODIFIER_USAGE_BASE, 1));
    assert_eq!(kbd.poll(&mut out), Ok(0));
}

#[test]
fn keyboard_poll_budget_bounds_one_call() {
    let src = Rc::new(RefCell::new(MockSource::new()));
    src.borrow_mut().push(&kbd_report(0, &[0x04]));
    // A flood of state-identical reports decodes to no further events.
    for _ in 0..(2 * REPORT_POLL_BUDGET) {
        src.borrow_mut().push(&kbd_report(0, &[0x04]));
    }
    let mut kbd = BootKeyboard::new(SharedSource(Rc::clone(&src)));
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(1));
    // The budget stopped the drain; the rest stay queued for later
    // polls rather than spinning this one forever.
    assert!(src.borrow().pending() >= REPORT_POLL_BUDGET);
    assert_eq!(kbd.poll(&mut out), Ok(0));
    assert!(src.borrow().pending() > 0);
}

#[test]
fn mouse_poll_rejects_empty_buffer() {
    let mut mouse = BootMouse::new(MockSource::new());
    let mut empty: [InputEvent; 0] = [];
    assert_eq!(mouse.poll(&mut empty), Err(DriverError::BufferTooSmall));
}

#[test]
fn mouse_decodes_motion_buttons_and_wheel() {
    let mut src = MockSource::new();
    // Left press, right+up motion, wheel down.
    src.push(&[0x01, 5, 0xFB, 0xFF]); // dx=5, dy=-5, wheel=-1
    let mut mouse = BootMouse::new(src);
    let mut out = [key(0, 0); 8];
    assert_eq!(mouse.poll(&mut out), Ok(4));
    assert_eq!(out[0], key(BUTTON_CODE_BASE, 1));
    assert_eq!(out[1], pointer(AXIS_X, 5));
    assert_eq!(out[2], pointer(AXIS_Y, -5));
    assert_eq!(out[3], scroll(AXIS_Y, -1));
}

#[test]
fn mouse_diffs_buttons_and_skips_zero_deltas() {
    let mut src = MockSource::new();
    src.push(&[0x03, 0, 0, 0]); // left+right down, no motion
    src.push(&[0x02, 0, 0, 0]); // left released, right held
    let mut mouse = BootMouse::new(src);
    let mut out = [key(0, 0); 8];
    assert_eq!(mouse.poll(&mut out), Ok(3));
    assert_eq!(out[0], key(BUTTON_CODE_BASE, 1));
    assert_eq!(out[1], key(BUTTON_CODE_BASE + 1, 1));
    assert_eq!(out[2], key(BUTTON_CODE_BASE, 0));
}

#[test]
fn mouse_accepts_three_byte_report_without_wheel() {
    let mut src = MockSource::new();
    src.push(&[0x00, 0x80, 0x7F]); // dx=-128, dy=127
    let mut mouse = BootMouse::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(mouse.poll(&mut out), Ok(2));
    assert_eq!(out[0], pointer(AXIS_X, -128));
    assert_eq!(out[1], pointer(AXIS_Y, 127));
}

#[test]
fn mouse_ignores_device_specific_button_bits() {
    let mut src = MockSource::new();
    src.push(&[0xF8, 0, 0, 0]); // only bits 3..8 set: no boot buttons
    let mut mouse = BootMouse::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(mouse.poll(&mut out), Ok(0));
}

#[test]
fn mouse_rejects_short_report() {
    let mut src = MockSource::new();
    src.push(&[0x01, 5]);
    let mut mouse = BootMouse::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(mouse.poll(&mut out), Err(DriverError::LengthOutOfRange));
}

#[test]
fn mouse_ignores_trailing_device_specific_bytes() {
    let mut src = MockSource::new();
    src.push(&[0x00, 1, 2, 0, 0xAA, 0xBB, 0xCC, 0xDD]);
    let mut mouse = BootMouse::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(mouse.poll(&mut out), Ok(2));
    assert_eq!(out[0], pointer(AXIS_X, 1));
    assert_eq!(out[1], pointer(AXIS_Y, 2));
}

#[test]
fn unload_then_reload_decodes_again() {
    let mut src = MockSource::new();
    src.push(&kbd_report(0, &[0x04]));
    let mut kbd = BootKeyboard::new(src);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(1));
    // Unload: drop the instance; reload: a fresh instance over a fresh
    // endpoint stream starts from the empty hold set.
    drop(kbd);
    let mut src = MockSource::new();
    src.push(&kbd_report(0, &[0x05]));
    let mut kbd = BootKeyboard::new(src);
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(0x05, 1));
}
