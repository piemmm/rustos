//! virtio-input unit tests against the in-process [`MockTransport`].

extern crate alloc;

use super::*;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use core::cell::RefCell;
use rustos_virtio::{ChainView, DmaSlab, MockHost, MockTransport};

/// A queued raw `virtio_input_event` the mock device will deliver:
/// `(type, code, value)`.
type RawEvent = (u16, u16, i32);
type EventQueue = Rc<RefCell<VecDeque<RawEvent>>>;

/// Build a `MockTransport` configured as a virtio-input device with two
/// queues (eventq + statusq). The returned `Rc` shares the queue of
/// events the device will deliver when the driver posts a device-write
/// buffer on the eventq.
fn build_device() -> (MockTransport, EventQueue) {
    // Two queues (eventq = 0, statusq = 1), eight descriptors each, no
    // feature bits, no device-config window.
    let mut t = MockTransport::new(2, 8, 0, 0);
    let events: EventQueue = Rc::new(RefCell::new(VecDeque::new()));
    // Eventq shim (queue 0): on each posted device-write buffer, pop one
    // queued event and write its 8 little-endian bytes into the buffer.
    // An empty queue completes the buffer with zero bytes written (the
    // "no event pending" case).
    let events_for_shim = Rc::clone(&events);
    t.install_shim(
        wire::EVENT_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            let dst = chain
                .device_write
                .first_mut()
                .ok_or(VirtioError::DeviceFault)?;
            if dst.len() < wire::EVENT_LEN as usize {
                return Err(VirtioError::DeviceFault);
            }
            let Some((etype, code, value)) = events_for_shim.borrow_mut().pop_front() else {
                return Ok(0);
            };
            dst[0..2].copy_from_slice(&etype.to_le_bytes());
            dst[2..4].copy_from_slice(&code.to_le_bytes());
            dst[4..8].copy_from_slice(&value.to_le_bytes());
            Ok(wire::EVENT_LEN)
        }),
    );
    (t, events)
}

/// `VirtioHost` that drains the eventq through its shim when the driver
/// waits, mirroring the IRQ-driven completion of a real device.
struct AutoDrainHost {
    inner: MockHost,
    transport: core::cell::UnsafeCell<*mut MockTransport>,
}

impl AutoDrainHost {
    fn new() -> Self {
        Self {
            inner: MockHost::new(),
            transport: core::cell::UnsafeCell::new(core::ptr::null_mut()),
        }
    }
    fn install_transport(&self, t: *mut MockTransport) {
        // SAFETY: `auto_host()` leaks a fresh instance per call, so no
        // aliasing borrow of `self.transport` exists when this write runs.
        unsafe {
            *self.transport.get() = t;
        }
    }
}

impl VirtioHost for AutoDrainHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        self.inner.alloc_dma_zeroed(size)
    }
    fn notify_wait(&self, queue_index: u16) {
        // SAFETY: the driver releases its `&mut self.transport` borrow
        // between `kick` and `notify_wait`; the pointer was installed
        // while no live borrow existed and is unique here for the
        // duration of `drain_queue`.
        let t_ptr = unsafe { *self.transport.get() };
        if !t_ptr.is_null() {
            let t = unsafe { &mut *t_ptr };
            let _ = t.drain_queue(queue_index);
        }
        self.inner.notify_wait(queue_index);
    }
}

fn auto_host() -> &'static AutoDrainHost {
    Box::leak(Box::new(AutoDrainHost::new()))
}

fn open_input(t: MockTransport) -> Box<VirtioInput<'static, MockTransport>> {
    let host = auto_host();
    let mut dev = Box::new(VirtioInput::open(t, host).expect("open"));
    host.install_transport(dev.transport_mut() as *mut MockTransport);
    dev
}

/// Scancode-neutral keycode for the `A` key (Linux `KEY_A`).
const KEY_A: u16 = 30;

#[test]
fn decode_maps_key_press_and_release() {
    let press = decode_event(wire::EV_KEY, KEY_A, 1).expect("key press decodes");
    assert_eq!(press.kind, InputEventKind::Key);
    assert_eq!(press.code, KEY_A);
    assert_eq!(press.value, 1);
    let release = decode_event(wire::EV_KEY, KEY_A, 0).expect("key release decodes");
    assert_eq!(release.kind, InputEventKind::Key);
    assert_eq!(release.value, 0);
}

#[test]
fn decode_maps_relative_pointer_and_wheel() {
    let x = decode_event(wire::EV_REL, wire::REL_X, -3).expect("rel-x decodes");
    assert_eq!(x.kind, InputEventKind::Pointer);
    assert_eq!(x.code, wire::AXIS_X);
    assert_eq!(x.value, -3);
    let y = decode_event(wire::EV_REL, wire::REL_Y, 7).expect("rel-y decodes");
    assert_eq!(y.kind, InputEventKind::Pointer);
    assert_eq!(y.code, wire::AXIS_Y);
    assert_eq!(y.value, 7);
    let wheel = decode_event(wire::EV_REL, wire::REL_WHEEL, 1).expect("wheel decodes");
    assert_eq!(wheel.kind, InputEventKind::Scroll);
    assert_eq!(wheel.value, 1);
}

#[test]
fn decode_discards_frame_markers_and_unmodelled_events() {
    // EV_SYN frame separator: no surfaced event.
    assert!(decode_event(wire::EV_SYN, 0, 0).is_none());
    // An unmapped EV_REL code (e.g. REL_Z): no surfaced event.
    assert!(decode_event(wire::EV_REL, 0x02, 5).is_none());
    // An entirely unmodelled type (EV_ABS = 3): no surfaced event.
    assert!(decode_event(0x03, 0, 0).is_none());
}

#[test]
fn poll_returns_queued_key_press() {
    let (t, events) = build_device();
    let mut dev = open_input(t);
    events.borrow_mut().push_back((wire::EV_KEY, KEY_A, 1));
    let mut buf = [InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    }; 4];
    assert_eq!(dev.poll(&mut buf), Ok(1));
    assert_eq!(buf[0].kind, InputEventKind::Key);
    assert_eq!(buf[0].code, KEY_A);
    assert_eq!(buf[0].value, 1);
}

#[test]
fn poll_drains_press_then_release_in_order() {
    let (t, events) = build_device();
    let mut dev = open_input(t);
    events.borrow_mut().push_back((wire::EV_KEY, KEY_A, 1));
    events.borrow_mut().push_back((wire::EV_KEY, KEY_A, 0));
    let mut buf = [InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    }; 1];
    assert_eq!(dev.poll(&mut buf), Ok(1));
    assert_eq!(buf[0].value, 1);
    assert_eq!(dev.poll(&mut buf), Ok(1));
    assert_eq!(buf[0].value, 0);
}

#[test]
fn poll_skips_frame_marker_as_no_event() {
    let (t, events) = build_device();
    let mut dev = open_input(t);
    // An EV_SYN completion is consumed but surfaces no event.
    events.borrow_mut().push_back((wire::EV_SYN, 0, 0));
    let mut buf = [InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    }; 1];
    assert_eq!(dev.poll(&mut buf), Ok(0));
}

#[test]
fn poll_with_no_pending_event_returns_zero() {
    let (t, _events) = build_device();
    let mut dev = open_input(t);
    let mut buf = [InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    }; 1];
    assert_eq!(dev.poll(&mut buf), Ok(0));
}

#[test]
fn poll_rejects_empty_buffer() {
    let (t, _events) = build_device();
    let mut dev = open_input(t);
    let mut empty: [InputEvent; 0] = [];
    assert_eq!(dev.poll(&mut empty), Err(DriverError::BufferTooSmall));
}

#[test]
fn register_requires_drv_load() {
    struct H {
        grant: bool,
    }
    impl DriverHost for H {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            cap == CapabilityId::DRV_LOAD && self.grant
        }
        fn kind(&self) -> rustos_abi::driver::DriverKind {
            rustos_abi::driver::DriverKind::UserSpace
        }
    }
    assert_eq!(
        register(&H { grant: false }),
        Err(DriverError::PermissionDenied)
    );
    assert!(register(&H { grant: true }).is_ok());
}

#[test]
fn bind_table_matches_a_virtio_input_node() {
    use rustos_abi::HwMatchKey;

    // One entry at the declared exact-match priority, matching a
    // discovered virtio node whose probed device id is `virtio-input`
    // (`AGENTS.md` §18.3).
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let input = HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID);
    assert!(BIND_KEYS[0].key.matches(&input));

    // A different virtio device (e.g. virtio-blk, device id 2) and a
    // non-virtio node both fail the match — the caller leaves them
    // unbound rather than guessing (`AGENTS.md` §18.4 / §2.9).
    let blk = HwMatchKey::virtio(2);
    assert!(!BIND_KEYS[0].key.matches(&blk));
    let pci_input = HwMatchKey::pci(0x1234, 0x5678, 0x09_00_00);
    assert!(!BIND_KEYS[0].key.matches(&pci_input));
}

#[test]
fn close_resets_the_device() {
    let (t, _events) = build_device();
    let dev = open_input(t);
    // `close` consumes the device and resets the transport status byte;
    // a clean teardown is the unload path (`AGENTS.md` §8).
    dev.close();
}
