//! virtio-net unit tests against the in-process [`MockTransport`].

extern crate alloc;

use super::*;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use rustos_virtio::{ChainView, DmaHost, MockHost, MockTransport};

/// MAC address the mock device exposes through its config window.
const DEVICE_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// Build a `MockTransport` configured as a virtio-net device. The
/// returned `Rc`s share state with the in-process peer:
///   - `tx_log` collects every transmitted frame.
///   - `rx_queue` is the queue of frames the device will deliver
///     when the driver posts a receive descriptor.
type Frame = Vec<u8>;
type TxLog = Rc<RefCell<Vec<Frame>>>;
type RxQueue = Rc<RefCell<VecDeque<Frame>>>;
fn build_device() -> (MockTransport, TxLog, RxQueue) {
    let mut t = MockTransport::new(2, 8, 0, 6);
    t.set_config(0, &DEVICE_MAC);
    let tx_log: Rc<RefCell<Vec<Frame>>> = Rc::new(RefCell::new(Vec::new()));
    let rx_queue: Rc<RefCell<VecDeque<Frame>>> = Rc::new(RefCell::new(VecDeque::new()));
    // RX shim (queue 0): consume one queued frame, write header + frame
    // into device-write descriptors, return total bytes written.
    let rx_for_shim = Rc::clone(&rx_queue);
    t.install_shim(
        wire::RX_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            if chain.device_write.len() < 2 {
                return Err(VirtioError::DeviceFault);
            }
            // Zero the header (Stage 4 negotiates no offloads).
            for b in chain.device_write[0].iter_mut() {
                *b = 0;
            }
            let header_len = chain.device_write[0].len();
            let Some(frame) = rx_for_shim.borrow_mut().pop_front() else {
                return Ok(0);
            };
            let dst = &mut chain.device_write[1];
            let copy_len = core::cmp::min(dst.len(), frame.len());
            dst[..copy_len].copy_from_slice(&frame[..copy_len]);
            let total = header_len + copy_len;
            Ok(u32::try_from(total).unwrap_or(u32::MAX))
        }),
    );
    // TX shim (queue 1): copy the device-read frame payload into
    // `tx_log`; nothing is written back so return 0.
    let tx_for_shim = Rc::clone(&tx_log);
    t.install_shim(
        wire::TX_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            // device_read = [header, frame]. Stage 4 always emits a
            // 10-byte zero header; the frame is the second segment.
            if chain.device_read.len() < 2 {
                return Err(VirtioError::DeviceFault);
            }
            tx_for_shim.borrow_mut().push(chain.device_read[1].to_vec());
            Ok(0)
        }),
    );
    (t, tx_log, rx_queue)
}

/// `VirtioHost` that auto-drains *both* queues when notified.
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
        // SAFETY: `auto_host()` allocates a fresh leaked instance
        // per call, so no aliasing borrow of `self.transport` can
        // exist when this write runs.
        unsafe {
            *self.transport.get() = t;
        }
    }
    fn bytes_allocated(&self) -> usize {
        self.inner.bytes_allocated()
    }
}

impl DmaHost for AutoDrainHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<rustos_virtio::DmaSlab, DriverError> {
        self.inner.alloc_dma_zeroed(size)
    }
}

impl VirtioHost for AutoDrainHost {
    fn notify_wait(&self, queue_index: u16) {
        // SAFETY: the driver releases its `&mut self.transport`
        // borrow between `kick` and `notify_wait`; the pointer was
        // installed while no live borrow existed and is unique
        // here for the duration of `drain_queue`.
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

fn open_net_with_host(
    t: MockTransport,
) -> (
    Box<VirtioNet<'static, MockTransport>>,
    &'static AutoDrainHost,
) {
    let host = auto_host();
    let mut net = Box::new(VirtioNet::open(t, host).expect("open"));
    host.install_transport(net.transport_mut() as *mut MockTransport);
    (net, host)
}

fn open_net(t: MockTransport) -> Box<VirtioNet<'static, MockTransport>> {
    open_net_with_host(t).0
}

/// Minimal 14-byte Ethernet frame: dst MAC, src MAC, ethertype.
fn arp_frame() -> Vec<u8> {
    let mut f = vec![0u8; 60];
    f[0..6].copy_from_slice(&[0xFF; 6]); // dst = broadcast
    f[6..12].copy_from_slice(&DEVICE_MAC); // src
    f[12..14].copy_from_slice(&[0x08, 0x06]); // ethertype = ARP
    f
}

#[test]
fn open_reads_mac_from_device_config() {
    let (t, _, _) = build_device();
    let net = open_net(t);
    assert_eq!(net.mac_address().unwrap(), MacAddress::new(DEVICE_MAC));
}

#[test]
fn transmit_records_frame_on_peer() {
    let (t, tx_log, _) = build_device();
    let mut net = open_net(t);
    let frame = arp_frame();
    net.transmit(&frame).expect("transmit");
    let log = tx_log.borrow();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], frame);
}

#[test]
fn receive_returns_queued_frame() {
    let (t, _, rx_queue) = build_device();
    let mut net = open_net(t);
    let payload = arp_frame();
    rx_queue.borrow_mut().push_back(payload.clone());
    let mut buf = vec![0u8; 1500];
    let n = net.receive(&mut buf).expect("receive");
    assert_eq!(n, payload.len());
    assert_eq!(&buf[..n], payload.as_slice());
}

#[test]
fn receive_with_no_pending_frame_returns_zero() {
    let (t, _, _) = build_device();
    let mut net = open_net(t);
    let mut buf = vec![0u8; 1500];
    let n = net.receive(&mut buf).expect("receive");
    assert_eq!(n, 0);
}

#[test]
fn transmit_rejects_undersized_frame() {
    let (t, _, _) = build_device();
    let mut net = open_net(t);
    let too_small = vec![0u8; 8];
    assert_eq!(net.transmit(&too_small), Err(DriverError::BufferTooSmall));
}

#[test]
fn transmit_rejects_oversized_frame() {
    let (t, _, _) = build_device();
    let mut net = open_net(t);
    let too_big = vec![0u8; 9000];
    assert_eq!(net.transmit(&too_big), Err(DriverError::LengthOutOfRange));
}

#[test]
fn receive_rejects_empty_buffer() {
    let (t, _, _) = build_device();
    let mut net = open_net(t);
    let mut empty: Vec<u8> = Vec::new();
    assert_eq!(net.receive(&mut empty), Err(DriverError::BufferTooSmall));
}

#[test]
fn sensitive_class_round_trip() {
    // End-to-end: transmit a sensitive frame, then receive a
    // sensitive frame the peer has queued; both calls succeed and
    // the device round-trips the payload (`BounceBuffer`'s drop
    // impl scrubs staging — covered directly in the transport
    // crate's tests).
    let (t, tx_log, rx_queue) = build_device();
    let mut net = open_net(t);
    let tx_frame = arp_frame();
    net.transmit_with_class(&tx_frame, BufferClass::Sensitive)
        .expect("tx");
    assert_eq!(tx_log.borrow()[0], tx_frame);
    let rx_frame = arp_frame();
    rx_queue.borrow_mut().push_back(rx_frame.clone());
    let mut buf = vec![0u8; 1500];
    let n = net
        .receive_with_class(&mut buf, BufferClass::Sensitive)
        .expect("rx");
    assert_eq!(&buf[..n], rx_frame.as_slice());
}

#[test]
fn steady_state_traffic_allocates_no_new_dma() {
    // The staging buffers are carved once at open; idle polls,
    // delivered frames, and transmits must all reuse them — the
    // per-poll `dma_alloc`/`dma_free` churn (and its audit-log spam)
    // is exactly the defect this driver must not reintroduce.
    let (t, tx_log, rx_queue) = build_device();
    let (mut net, host) = open_net_with_host(t);
    let after_open = host.bytes_allocated();
    let frame = arp_frame();
    let mut buf = vec![0u8; 1500];
    for _ in 0..8 {
        assert_eq!(net.receive(&mut buf).expect("idle receive"), 0);
        rx_queue.borrow_mut().push_back(frame.clone());
        let n = net.receive(&mut buf).expect("receive");
        assert_eq!(n, frame.len());
        net.transmit(&frame).expect("transmit");
    }
    assert_eq!(tx_log.borrow().len(), 8);
    assert_eq!(
        host.bytes_allocated(),
        after_open,
        "steady-state traffic must not allocate DMA"
    );
}

#[test]
fn receive_rearms_the_chain_after_buffer_too_small() {
    // A frame larger than the caller's buffer is refused, but the
    // receive chain must be re-posted so the next frame still lands.
    let (t, _, rx_queue) = build_device();
    let mut net = open_net(t);
    rx_queue.borrow_mut().push_back(vec![0xAB; 200]);
    let mut small = vec![0u8; 16];
    assert_eq!(net.receive(&mut small), Err(DriverError::BufferTooSmall));
    let frame = arp_frame();
    rx_queue.borrow_mut().push_back(frame.clone());
    let mut buf = vec![0u8; 1500];
    let n = net.receive(&mut buf).expect("receive after refusal");
    assert_eq!(n, frame.len());
    assert_eq!(&buf[..n], frame.as_slice());
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
