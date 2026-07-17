//! Unit tests for the PS/2 keyboard driver against an in-process mock
//! [`PortIo8`] controller (mirrors the bus / display drivers' mock seams).

extern crate alloc;

use alloc::collections::VecDeque;

use core::cell::{Cell, RefCell};

use super::*;
use tairix_abi::driver::input::{InputEvent, InputEventKind};
use tairix_abi::driver::DriverKind;
use tairix_abi::CapabilityId;

/// Mock i8042: a FIFO of `(byte, is_aux)` entries modelling the
/// controller's output buffer. Reading the status port reports
/// output-buffer-full while the FIFO is non-empty and tags the front
/// byte's source port; reading the data port pops the front byte. The
/// driver never writes the controller, so `write8` only records that it
/// stayed silent.
struct MockController {
    queue: RefCell<VecDeque<(u8, bool)>>,
    data_reads: Cell<usize>,
    writes: Cell<usize>,
}

impl MockController {
    fn new() -> Self {
        Self {
            queue: RefCell::new(VecDeque::new()),
            data_reads: Cell::new(0),
            writes: Cell::new(0),
        }
    }

    /// Enqueue keyboard-port bytes.
    fn push_keyboard(&self, bytes: &[u8]) {
        let mut q = self.queue.borrow_mut();
        for &b in bytes {
            q.push_back((b, false));
        }
    }

    /// Enqueue a single auxiliary (mouse) port byte.
    fn push_aux(&self, byte: u8) {
        self.queue.borrow_mut().push_back((byte, true));
    }

    fn pending(&self) -> usize {
        self.queue.borrow().len()
    }
}

impl PortIo8 for MockController {
    fn read8(&self, port: u16) -> u8 {
        match port {
            STATUS_PORT => match self.queue.borrow().front() {
                None => 0,
                Some(&(_, aux)) => STATUS_OUTPUT_FULL | if aux { STATUS_AUX_DATA } else { 0 },
            },
            DATA_PORT => {
                self.data_reads.set(self.data_reads.get() + 1);
                self.queue.borrow_mut().pop_front().map_or(0, |(b, _)| b)
            }
            other => panic!("driver touched unexpected port {other:#06x}"),
        }
    }

    fn write8(&self, _port: u16, _value: u8) {
        self.writes.set(self.writes.get() + 1);
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

#[test]
fn register_requires_drv_load() {
    assert!(register(&MockHost { drv_load: true }).is_ok());
    assert_eq!(
        register(&MockHost { drv_load: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn poll_rejects_empty_buffer() {
    let mut kbd = Ps2Keyboard::new(MockController::new());
    let mut empty: [InputEvent; 0] = [];
    assert_eq!(kbd.poll(&mut empty), Err(DriverError::BufferTooSmall));
}

#[test]
fn poll_returns_zero_when_buffer_empty() {
    let mut kbd = Ps2Keyboard::new(MockController::new());
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(0));
}

#[test]
fn decodes_press_and_release() {
    let ctrl = MockController::new();
    // 'A' make (0x1E) then break (0x9E).
    ctrl.push_keyboard(&[0x1E, 0x9E]);
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(0x1E, 1));
    assert_eq!(out[1], key(0x1E, 0));
}

#[test]
fn decodes_extended_keycode() {
    let ctrl = MockController::new();
    // Right-arrow: E0 4D (press), E0 CD (release).
    ctrl.push_keyboard(&[EXTENDED_PREFIX, 0x4D, EXTENDED_PREFIX, 0xCD]);
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(EXTENDED_KEYCODE_BASE | 0x4D, 1));
    assert_eq!(out[1], key(EXTENDED_KEYCODE_BASE | 0x4D, 0));
}

#[test]
fn extended_prefix_latches_across_polls() {
    let ctrl = MockController::new();
    ctrl.push_keyboard(&[EXTENDED_PREFIX]);
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 1];
    // First poll consumes only the prefix: no event yet.
    assert_eq!(kbd.poll(&mut out), Ok(0));
    // The code arrives later; the latched prefix must still apply.
    kbd.port.push_keyboard(&[0x4D]);
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(EXTENDED_KEYCODE_BASE | 0x4D, 1));
}

#[test]
fn skips_detection_error_markers() {
    let ctrl = MockController::new();
    // 0x00 (detection error) and 0x80 (break of make 0) are not keys.
    ctrl.push_keyboard(&[0x00, 0x80, 0x1E]);
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(0x1E, 1));
}

#[test]
fn stops_at_auxiliary_byte_without_consuming_it() {
    let ctrl = MockController::new();
    ctrl.push_keyboard(&[0x1E]);
    ctrl.push_aux(0x08); // a mouse packet byte behind the keystroke
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(1));
    assert_eq!(out[0], key(0x1E, 1));
    // The aux byte is left in the buffer for a pointer driver.
    assert_eq!(kbd.port.pending(), 1);
}

#[test]
fn fills_buffer_and_resumes_on_next_poll() {
    let ctrl = MockController::new();
    ctrl.push_keyboard(&[0x1E, 0x9E, 0x1E, 0x9E]);
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 2];
    assert_eq!(kbd.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(0x1E, 1));
    assert_eq!(out[1], key(0x1E, 0));
    assert_eq!(kbd.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(0x1E, 1));
    assert_eq!(out[1], key(0x1E, 0));
    assert_eq!(kbd.poll(&mut out), Ok(0));
}

#[test]
fn read_budget_bounds_a_stuck_controller() {
    let ctrl = MockController::new();
    // A controller that never yields a completed key (endless E0 prefixes)
    // must not spin: the per-call budget caps the data reads.
    let prefixes = alloc::vec![EXTENDED_PREFIX; 1024];
    ctrl.push_keyboard(&prefixes);
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 4];
    assert_eq!(kbd.poll(&mut out), Ok(0));
    // budget = len*2 + 2 = 10 data reads for a 4-slot buffer.
    assert_eq!(kbd.port.data_reads.get(), 10);
}

#[test]
fn driver_never_writes_the_controller() {
    let ctrl = MockController::new();
    ctrl.push_keyboard(&[0x1E, 0x9E]);
    let mut kbd = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 4];
    let _ = kbd.poll(&mut out);
    assert_eq!(kbd.port.writes.get(), 0);
}

#[test]
fn unload_then_reload_decodes_again() {
    let host = MockHost { drv_load: true };
    assert!(register(&host).is_ok());
    {
        let ctrl = MockController::new();
        ctrl.push_keyboard(&[0x1E]);
        let mut kbd = Ps2Keyboard::new(ctrl);
        let mut out = [key(0, 0); 1];
        assert_eq!(kbd.poll(&mut out), Ok(1));
        // `kbd` drops here — the unload step.
    }
    let ctrl = MockController::new();
    ctrl.push_keyboard(&[0x20, 0xA0]); // 'D' make/break
    let mut reloaded = Ps2Keyboard::new(ctrl);
    let mut out = [key(0, 0); 2];
    assert_eq!(reloaded.poll(&mut out), Ok(2));
    assert_eq!(out[0], key(0x20, 1));
    assert_eq!(out[1], key(0x20, 0));
}
