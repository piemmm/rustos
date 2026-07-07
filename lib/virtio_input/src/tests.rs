//! virtio-input device-logic unit tests against the in-process
//! [`MockTransport`].

extern crate alloc;

use super::*;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use core::cell::RefCell;
use rustos_virtio::{ChainView, DmaHost, DmaSlab, MockHost, MockTransport};

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

impl DmaHost for AutoDrainHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        self.inner.alloc_dma_zeroed(size)
    }
}

impl VirtioHost for AutoDrainHost {
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
fn poll_acknowledges_the_device_interrupt_each_cycle() {
    // Regression: `poll` must acknowledge the device once per wait + drain
    // cycle, or a level-signalled transport (virtio-MMIO) keeps its line
    // asserted and every subsequent wait wakes immediately — the busy loop
    // that pegged a core under the curses login screen.
    let (t, events) = build_device();
    let mut dev = open_input(t);
    let mut buf = [InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    }; 4];
    // Delivered-event path: one poll, one acknowledge.
    events.borrow_mut().push_back((wire::EV_KEY, KEY_A, 1));
    assert_eq!(dev.poll(&mut buf), Ok(1));
    assert_eq!(dev.transport_mut().ack_interrupts, 1);
    // Empty wait path (a spurious wake): still exactly one acknowledge,
    // so a faulted or empty drain never leaves the line asserted.
    assert_eq!(dev.poll(&mut buf), Ok(0));
    assert_eq!(dev.transport_mut().ack_interrupts, 2);
}

#[test]
fn poll_rejects_empty_buffer() {
    let (t, _events) = build_device();
    let mut dev = open_input(t);
    let mut empty: [InputEvent; 0] = [];
    assert_eq!(dev.poll(&mut empty), Err(DriverError::BufferTooSmall));
}

#[test]
fn close_resets_the_device() {
    let (t, _events) = build_device();
    let dev = open_input(t);
    // `close` consumes the device and resets the transport status byte;
    // a clean teardown is the unload path.
    dev.close();
}

#[test]
fn open_armed_arms_only_after_the_event_queue_is_live() {
    // Regression: the arm step (the driver's `irq_bind` — the readiness
    // witness a harness or supervisor watches for) must run only once the
    // device can already accept an event. Arming first advertised a
    // keyboard whose eventq had no posted buffers, and a keystroke typed
    // in that window was silently dropped — the flaky autoload-input
    // vertical's lost keypress.
    let (t, events) = build_device();
    let host = auto_host();
    // A keystroke is already pending at the device when the arm step runs.
    events.borrow_mut().push_back((wire::EV_KEY, KEY_A, 1));
    let armed = core::cell::Cell::new(0u32);
    let mut dev = VirtioInput::open_armed(t, host, |dev| {
        armed.set(armed.get() + 1);
        let t = dev.transport_mut();
        // The device is live before the arm step runs...
        assert!(t.status().contains(Status::DRIVER_OK));
        // ...with every event buffer already posted and device-visible:
        // the device can deliver the pending keystroke (and complete the
        // remaining posted buffers) right now.
        assert_eq!(
            t.drain_queue(wire::EVENT_QUEUE).expect("posted buffers"),
            usize::from(wire::EVENT_QUEUE_SIZE)
        );
        Ok(())
    })
    .expect("open_armed");
    assert_eq!(armed.get(), 1);
    // The keystroke delivered while arming is not lost: the first poll's
    // pre-wait drain collects it.
    let mut buf = [InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    }; 4];
    assert_eq!(dev.poll(&mut buf), Ok(1));
    assert_eq!(buf[0].code, KEY_A);
    assert_eq!(buf[0].value, 1);
}

/// [`Transport`] wrapper that counts device resets while delegating to the
/// in-process mock, so a test can observe the teardown
/// [`VirtioInput::open_armed`] performs after a failed arm step even though
/// the device value is consumed by that teardown.
struct ResetProbe {
    inner: MockTransport,
    resets: Rc<core::cell::Cell<u32>>,
}

impl Transport for ResetProbe {
    fn reset(&mut self) {
        self.resets.set(self.resets.get() + 1);
        self.inner.reset();
    }
    fn status(&self) -> Status {
        self.inner.status()
    }
    fn set_status(&mut self, status: Status) {
        self.inner.set_status(status);
    }
    fn device_features(&self) -> u64 {
        self.inner.device_features()
    }
    fn set_driver_features(&mut self, features: u64) {
        self.inner.set_driver_features(features);
    }
    fn num_queues(&self) -> u16 {
        self.inner.num_queues()
    }
    fn queue_select(&mut self, queue: u16) -> Result<(), VirtioError> {
        self.inner.queue_select(queue)
    }
    fn queue_max_size(&self) -> u16 {
        self.inner.queue_max_size()
    }
    fn queue_set(
        &mut self,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<(), VirtioError> {
        self.inner.queue_set(size, desc, avail, used)
    }
    fn notify(&mut self, queue: u16) {
        self.inner.notify(queue);
    }
    fn read_config(&self, offset: usize, buf: &mut [u8]) {
        self.inner.read_config(offset, buf);
    }
    fn ack_interrupt(&mut self) {
        self.inner.ack_interrupt();
    }
}

#[test]
fn open_armed_surfaces_the_arm_error_and_resets_the_device() {
    // A failed arm step must tear the device down (a live device is never
    // left DMA-writing into a driver that is about to exit) and surface
    // the arm error unchanged.
    let (t, _events) = build_device();
    let resets = Rc::new(core::cell::Cell::new(0u32));
    let probe = ResetProbe {
        inner: t,
        resets: Rc::clone(&resets),
    };
    let host = auto_host();
    let Err(err) = VirtioInput::open_armed(probe, host, |_| Err(DriverError::PermissionDenied))
    else {
        panic!("arm failure must surface");
    };
    assert_eq!(err, DriverError::PermissionDenied);
    // `open`'s initialisation reset plus the arm-failure teardown.
    assert_eq!(resets.get(), 2);
}
