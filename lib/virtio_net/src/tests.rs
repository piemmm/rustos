//! virtio-net unit tests against the in-process [`MockTransport`].

extern crate alloc;

use super::*;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::driver::net::NetOffloads;
use tairix_abi::driver::net_ring::{FrameOffload, RingGeometry};
use tairix_abi::driver::BufferClass;
use tairix_virtio::{ChainView, DmaHost, MockHost, MockTransport};

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
    fn alloc_dma_zeroed(&self, size: usize) -> Result<tairix_virtio::DmaSlab, DriverError> {
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

/// A `VirtioHost` whose `notify_wait` panics. The non-blocking `service`
/// doorbell must never wait on the device — it may run inside a
/// cross-process `Service` call, where parking would block the reply and
/// the serve loop — so any `notify_wait` from the service path is a defect
/// this host turns into a loud test failure.
struct NoWaitHost {
    inner: MockHost,
}

impl DmaHost for NoWaitHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<tairix_virtio::DmaSlab, DriverError> {
        self.inner.alloc_dma_zeroed(size)
    }
}

impl VirtioHost for NoWaitHost {
    fn notify_wait(&self, _queue_index: u16) {
        panic!("service() must never wait on the device");
    }
}

fn open_net_no_wait(t: MockTransport) -> Box<VirtioNet<'static, MockTransport>> {
    let host = Box::leak(Box::new(NoWaitHost {
        inner: MockHost::new(),
    }));
    Box::new(VirtioNet::open(t, host).expect("open"))
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

/// Ring geometry for the tests: four slots, each wide enough to let
/// a deliberately over-MTU frame *into* the ring so the driver-side
/// drop policy is what the test exercises.
fn test_geometry() -> RingGeometry {
    RingGeometry::new(4, 2048, 2048).expect("test geometry")
}

fn rings_region() -> Vec<u8> {
    vec![0u8; test_geometry().region_len()]
}

fn bind_rings(region: &mut [u8], class: BufferClass) -> FrameRings<'_> {
    FrameRings::bind(region, test_geometry(), class).expect("bind rings")
}

/// Simulate the device completing the driver's posted receive chain and
/// raising its interrupt: drain the RX virtqueue so the completion lands in
/// the used ring for the next `service` to harvest. In the live system the
/// device does this and the driver's IRQ handler wakes the stack; the
/// non-blocking `service` doorbell never waits for a receive event itself,
/// so a test must post the completion the same way the hardware would.
fn deliver_rx(net: &mut VirtioNet<'static, MockTransport>) {
    let _ = net.transport_mut().drain_queue(wire::RX_QUEUE);
}

/// Simulate the device consuming a posted transmit chain and raising its
/// interrupt: drain the TX virtqueue so the completion lands in the used
/// ring for the next `service` to reap (and the TX shim records the frame).
/// The non-blocking `service` doorbell hands a frame to the device and
/// returns without waiting, exactly as it must across the live process
/// boundary; a later `service` (which the completion's interrupt drives)
/// reaps the staging. A test therefore drives the device the same way the
/// hardware would, between the transmitting `service` and the reaping one.
fn deliver_tx(net: &mut VirtioNet<'static, MockTransport>) {
    let _ = net.transport_mut().drain_queue(wire::TX_QUEUE);
}

#[test]
fn open_reports_device_facts() {
    let (t, _, _) = build_device();
    let net = open_net(t);
    let facts = net.device_facts().expect("facts");
    facts.validate().expect("facts validate");
    assert_eq!(facts.mac, MacAddress::new(DEVICE_MAC));
    assert_eq!(facts.mtu, 1500);
    assert_eq!(facts.link, LinkState::Up);
    assert_eq!(facts.offloads, NetOffloads::empty());
    assert_eq!(facts.rx_queues, 1);
}

#[test]
fn service_transmits_queued_frames_to_the_peer() {
    let (t, tx_log, _) = build_device();
    let mut net = open_net(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    let frame = arp_frame();
    rings.tx.push(&frame).expect("queue");
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1);
    // The doorbell handed the frame to the device without waiting; driving
    // the device now delivers it to the peer.
    deliver_tx(&mut net);
    let log = tx_log.borrow();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], frame);
}

/// Regression: the transmit doorbell must never wait on the device. The
/// device is never driven here (no `drain_queue`), and `notify_wait` would
/// panic, yet `service` still hands the frame over and returns — the
/// pre-fix code parked on `notify_wait`, which across the live process
/// boundary blocks the reply and the whole serve loop.
#[test]
fn transmit_doorbell_never_waits_on_the_device() {
    let (t, tx_log, _) = build_device();
    let mut net = open_net_no_wait(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    let frame = arp_frame();
    rings.tx.push(&frame).expect("queue");
    let report = net.service(&mut rings).expect("service never waits");
    assert_eq!(report.transmitted, 1);
    // Nothing waited: the frame is in flight, not yet consumed by the
    // (undriven) device.
    assert!(tx_log.borrow().is_empty());
    // Driving the device now completes it, and the next doorbell reaps.
    deliver_tx(&mut net);
    assert_eq!(tx_log.borrow().len(), 1);
    let report = net.service(&mut rings).expect("reap");
    assert_eq!(report, ServiceReport::default());
}

/// Build an ARP-shaped frame tagged in its first byte so a test can tell
/// several queued frames apart in the transmit log.
fn tagged_frame(tag: u8) -> Frame {
    let mut f = arp_frame();
    f[0] = tag;
    f
}

/// Regression (D11): a burst of frames queued before a single `service`
/// all egress in that one call, without waiting for any device completion
/// in between. The device is never driven here (no `drain_queue`) and the
/// host would panic on any wait, yet every queued frame is handed to the
/// device: the depth-1 predecessor sent only the first and stranded the
/// rest in the frame ring until an unrelated interrupt drove the next
/// service — the RTO-cadence crawl this driver must not reintroduce (a TCP
/// data segment and the ACK queued right behind it must go out together).
#[test]
fn service_egresses_a_burst_in_one_call_without_a_completion() {
    let (t, tx_log, _) = build_device();
    let mut net = open_net_no_wait(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    // Three frames — a data segment and the two frames queued behind it —
    // all fit under the transmit pool depth, so one service drains them all.
    let frames = [tagged_frame(0x01), tagged_frame(0x02), tagged_frame(0x03)];
    for f in &frames {
        rings.tx.push(f).expect("queue");
    }
    let report = net.service(&mut rings).expect("service never waits");
    assert_eq!(
        report.transmitted, 3,
        "the whole burst must egress in one service, not one frame per call"
    );
    // Nothing waited on the device: the frames are in flight, not yet
    // consumed by the (undriven) device.
    assert!(tx_log.borrow().is_empty());
    // Driving the device now completes all three, in order.
    deliver_tx(&mut net);
    let log = tx_log.borrow();
    assert_eq!(log.len(), 3);
    for (sent, expected) in log.iter().zip(frames.iter()) {
        assert_eq!(sent, expected);
    }
}

/// Back-pressure is applied only when the transmit ring is genuinely full
/// (every staging pair in flight) — never as an artificial one-frame-at-a-
/// time limit. Filling the pool, then queueing another full ring's worth
/// without draining, holds the second batch (transmitted 0) rather than
/// dropping it; draining the device then lets the held frames egress on
/// the next service, in order.
#[test]
fn transmit_back_pressure_only_when_the_ring_is_full() {
    let (t, tx_log, _) = build_device();
    let mut net = open_net_no_wait(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);

    // First batch: exactly the pool depth. One service puts them all in
    // flight and empties the frame ring.
    let first: Vec<Frame> = (0..TX_INFLIGHT_MAX)
        .map(|i| tagged_frame(u8::try_from(i).unwrap()))
        .collect();
    for f in &first {
        rings.tx.push(f).expect("queue first batch");
    }
    assert_eq!(
        net.service(&mut rings).expect("service").transmitted,
        u32::try_from(TX_INFLIGHT_MAX).unwrap()
    );

    // Second batch (frame ring now empty again): with every staging pair
    // still in flight, this doorbell sends nothing (ring-full back-pressure)
    // and drops nothing.
    let second: Vec<Frame> = (0..TX_INFLIGHT_MAX)
        .map(|i| tagged_frame(u8::try_from(0x80 + i).unwrap()))
        .collect();
    for f in &second {
        rings.tx.push(f).expect("queue second batch");
    }
    assert_eq!(net.service(&mut rings).expect("service").transmitted, 0);

    // Drive the device: the first batch completes; the next service reaps
    // the pool and drains the held second batch.
    deliver_tx(&mut net);
    assert_eq!(
        net.service(&mut rings).expect("service").transmitted,
        u32::try_from(TX_INFLIGHT_MAX).unwrap()
    );
    deliver_tx(&mut net);

    let log = tx_log.borrow();
    let expected: Vec<Frame> = first.iter().chain(second.iter()).cloned().collect();
    assert_eq!(log.len(), expected.len());
    for (sent, want) in log.iter().zip(expected.iter()) {
        assert_eq!(sent, want);
    }
}

#[test]
fn service_delivers_a_queued_frame_into_the_rx_ring() {
    let (t, _, rx_queue) = build_device();
    let mut net = open_net(t);
    let payload = arp_frame();
    rx_queue.borrow_mut().push_back(payload.clone());
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    // The device completes the posted receive chain and raises its IRQ;
    // only then does the non-blocking `service` doorbell harvest it.
    deliver_rx(&mut net);
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.received, 1);
    let mut buf = vec![0u8; 2048];
    let n = rings.rx.pop(&mut buf).expect("pop").expect("frame");
    assert_eq!(n, payload.len());
    assert_eq!(&buf[..n], payload.as_slice());
}

#[test]
fn idle_service_reports_nothing_moved() {
    let (t, _, _) = build_device();
    let mut net = open_net(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report, ServiceReport::default());
}

#[test]
fn runt_and_oversize_tx_frames_are_dropped_without_wedging() {
    let (t, tx_log, _) = build_device();
    let mut net = open_net(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    // A runt, an over-MTU frame (fits the 2048-byte slot, exceeds
    // the 1514-byte device maximum), then a good frame.
    rings.tx.push(&[0u8; 8]).expect("queue runt");
    rings.tx.push(&[0u8; 2000]).expect("queue oversize");
    let good = arp_frame();
    rings.tx.push(&good).expect("queue good");
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1);
    deliver_tx(&mut net);
    let log = tx_log.borrow();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], good);
}

#[test]
fn sensitive_class_round_trip_scrubs_staging() {
    // End-to-end: transmit and receive over Sensitive-class rings;
    // the payload round-trips and the persistent staging is zeroed
    // once the frames have moved.
    let (t, tx_log, rx_queue) = build_device();
    let mut net = open_net(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::Sensitive);
    let tx_frame = arp_frame();
    rings.tx.push(&tx_frame).expect("queue");
    let report = net.service(&mut rings).expect("service tx");
    assert_eq!(report.transmitted, 1);
    deliver_tx(&mut net);
    assert_eq!(tx_log.borrow()[0], tx_frame);
    let rx_frame = arp_frame();
    rx_queue.borrow_mut().push_back(rx_frame.clone());
    deliver_rx(&mut net);
    let report = net.service(&mut rings).expect("service rx");
    assert_eq!(report.received, 1);
    let mut buf = vec![0u8; 2048];
    let n = rings.rx.pop(&mut buf).expect("pop").expect("frame");
    assert_eq!(&buf[..n], rx_frame.as_slice());
}

#[test]
fn steady_state_traffic_allocates_no_new_dma() {
    // The staging buffers are carved once at open; idle services,
    // delivered frames, and transmits must all reuse them — the
    // per-call `dma_alloc`/`dma_free` churn (and its audit-log spam)
    // is exactly the defect this driver must not reintroduce.
    let (t, tx_log, rx_queue) = build_device();
    let (mut net, host) = open_net_with_host(t);
    let after_open = host.bytes_allocated();
    let frame = arp_frame();
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    let mut buf = vec![0u8; 2048];
    assert_eq!(
        net.service(&mut rings).expect("idle service"),
        ServiceReport::default()
    );
    for _ in 0..8 {
        rx_queue.borrow_mut().push_back(frame.clone());
        rings.tx.push(&frame).expect("queue");
        let report = net.service(&mut rings).expect("service");
        assert_eq!(report.transmitted, 1);
        // The device consumes the transmit chain and completes the receive
        // chain, each raising the (shared) IRQ; the next doorbell reaps the
        // transmit staging and harvests the frame (the non-blocking
        // `service` never waits for either event itself).
        deliver_tx(&mut net);
        deliver_rx(&mut net);
        let report = net.service(&mut rings).expect("service rx");
        assert_eq!(report.received, 1);
        while rings.rx.pop(&mut buf).expect("pop").is_some() {}
    }
    assert_eq!(tx_log.borrow().len(), 8);
    assert_eq!(
        host.bytes_allocated(),
        after_open,
        "steady-state traffic must not allocate DMA"
    );
}

#[test]
fn corrupt_tx_slot_is_consumed_and_flow_continues() {
    let (t, tx_log, _) = build_device();
    let mut net = open_net(t);
    let mut region = rings_region();
    // Queue a good frame after a slot whose length prefix is
    // corrupted beyond the slot capacity.
    {
        let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
        rings.tx.push(&[0u8; 60]).expect("queue victim");
        rings.tx.push(&arp_frame()).expect("queue good");
    }
    // The TX ring is the second half of the region; its first slot's
    // length prefix sits after the 8-byte ring header and the 9-byte
    // per-frame offload-metadata prefix (tag + two u16 checksum offsets
    // + two u16 segmentation fields).
    let tx_ring_base = test_geometry().rx_ring_len();
    let len_prefix = tx_ring_base + 8 + 9;
    region[len_prefix..len_prefix + 4].copy_from_slice(&8000u32.to_le_bytes());
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1);
    deliver_tx(&mut net);
    assert_eq!(tx_log.borrow().len(), 1);
}

/// Build a virtio-net `MockTransport` that offers `VIRTIO_NET_F_GUEST_CSUM`
/// and whose RX shim stamps `flags` (plus the checksum offsets) into the
/// `virtio_net_hdr` of every delivered frame, so the driver's per-frame
/// receive-offload tagging can be exercised.
fn build_device_rx_flags(flags: u8, csum_start: u16, csum_offset: u16) -> (MockTransport, RxQueue) {
    let mut t = MockTransport::new(2, 8, wire::VIRTIO_NET_F_GUEST_CSUM, 6);
    t.set_config(0, &DEVICE_MAC);
    let rx_queue: RxQueue = Rc::new(RefCell::new(VecDeque::new()));
    let rx_for_shim = Rc::clone(&rx_queue);
    t.install_shim(
        wire::RX_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            if chain.device_write.len() < 2 {
                return Err(VirtioError::DeviceFault);
            }
            let hdr = &mut chain.device_write[0];
            for b in hdr.iter_mut() {
                *b = 0;
            }
            if hdr.len() >= wire::HEADER_LEN {
                hdr[wire::HDR_FLAGS_OFFSET] = flags;
                hdr[wire::HDR_CSUM_START_OFFSET..wire::HDR_CSUM_START_OFFSET + 2]
                    .copy_from_slice(&csum_start.to_le_bytes());
                hdr[wire::HDR_CSUM_OFFSET_OFFSET..wire::HDR_CSUM_OFFSET_OFFSET + 2]
                    .copy_from_slice(&csum_offset.to_le_bytes());
            }
            let header_len = hdr.len();
            let Some(frame) = rx_for_shim.borrow_mut().pop_front() else {
                return Ok(0);
            };
            let dst = &mut chain.device_write[1];
            let copy_len = core::cmp::min(dst.len(), frame.len());
            dst[..copy_len].copy_from_slice(&frame[..copy_len]);
            Ok(u32::try_from(header_len + copy_len).unwrap_or(u32::MAX))
        }),
    );
    (t, rx_queue)
}

/// Service the driver once and return the offload metadata tagged onto
/// the single delivered frame, asserting the frame bytes round-trip.
/// The caller has already queued `frame` on the device's RX queue.
fn deliver_and_pop_offload(
    net: &mut VirtioNet<'static, MockTransport>,
    frame: &[u8],
) -> FrameOffload {
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    deliver_rx(net);
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.received, 1);
    let mut buf = vec![0u8; 2048];
    let mut offload = FrameOffload::None;
    let n = rings
        .rx
        .pop_with(&mut offload, &mut buf)
        .expect("pop")
        .expect("frame");
    assert_eq!(&buf[..n], frame);
    offload
}

#[test]
fn guest_csum_negotiation_advertises_rx_validated() {
    // The device offers VIRTIO_NET_F_GUEST_CSUM: the driver negotiates it
    // and advertises the receive-checksum-validation offload.
    let (t, _rx) = build_device_rx_flags(0, 0, 0);
    let net = open_net(t);
    let facts = net.device_facts().expect("facts");
    assert!(facts.offloads.contains(NetOffloads::RX_CSUM_VALIDATED));
}

#[test]
fn rx_data_valid_frame_is_tagged_validated() {
    let (t, rx_queue) = build_device_rx_flags(wire::VIRTIO_NET_HDR_F_DATA_VALID, 0, 0);
    let mut net = open_net(t);
    let frame = arp_frame();
    rx_queue.borrow_mut().push_back(frame.clone());
    assert_eq!(
        deliver_and_pop_offload(&mut net, &frame),
        FrameOffload::Validated
    );
}

#[test]
fn rx_needs_csum_frame_carries_the_offsets() {
    let (t, rx_queue) = build_device_rx_flags(wire::VIRTIO_NET_HDR_F_NEEDS_CSUM, 34, 16);
    let mut net = open_net(t);
    let frame = arp_frame();
    rx_queue.borrow_mut().push_back(frame.clone());
    assert_eq!(
        deliver_and_pop_offload(&mut net, &frame),
        FrameOffload::NeedsChecksum {
            csum_start: 34,
            csum_offset: 16,
        }
    );
}

#[test]
fn rx_flags_ignored_when_guest_csum_not_negotiated() {
    // The default device offers no features: even a DATA_VALID flag on the
    // wire is ignored, because the driver did not negotiate GUEST_CSUM.
    let mut t = MockTransport::new(2, 8, 0, 6);
    t.set_config(0, &DEVICE_MAC);
    let rx_queue: RxQueue = Rc::new(RefCell::new(VecDeque::new()));
    let rx_for_shim = Rc::clone(&rx_queue);
    t.install_shim(
        wire::RX_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            if chain.device_write.len() < 2 {
                return Err(VirtioError::DeviceFault);
            }
            let hdr = &mut chain.device_write[0];
            for b in hdr.iter_mut() {
                *b = 0;
            }
            hdr[wire::HDR_FLAGS_OFFSET] = wire::VIRTIO_NET_HDR_F_DATA_VALID;
            let header_len = hdr.len();
            let Some(frame) = rx_for_shim.borrow_mut().pop_front() else {
                return Ok(0);
            };
            let dst = &mut chain.device_write[1];
            let copy_len = core::cmp::min(dst.len(), frame.len());
            dst[..copy_len].copy_from_slice(&frame[..copy_len]);
            Ok(u32::try_from(header_len + copy_len).unwrap_or(u32::MAX))
        }),
    );
    let mut net = open_net(t);
    let frame = arp_frame();
    rx_queue.borrow_mut().push_back(frame.clone());
    assert_eq!(
        deliver_and_pop_offload(&mut net, &frame),
        FrameOffload::None
    );
}

/// Captured `virtio_net_hdr` bytes of each transmitted frame.
type HdrLog = Rc<RefCell<Vec<[u8; wire::HEADER_LEN]>>>;

/// Build a virtio-net `MockTransport` offering `VIRTIO_NET_F_CSUM`
/// (transmit-checksum offload) plus the transmit `features`, whose TX shim
/// records both the `virtio_net_hdr` and the frame of every transmitted
/// chain so the driver's per-frame transmit-offload header can be
/// exercised.
fn build_device_tx_csum() -> (MockTransport, HdrLog, TxLog) {
    build_device_tx_features(wire::VIRTIO_NET_F_CSUM)
}

/// [`build_device_tx_csum`] with an explicit device-feature set (so a TSO
/// test can add `HOST_TSO4`/`HOST_TSO6`), shared to avoid a second shim.
fn build_device_tx_features(features: u64) -> (MockTransport, HdrLog, TxLog) {
    let mut t = MockTransport::new(2, 8, features, 6);
    t.set_config(0, &DEVICE_MAC);
    let hdr_log: HdrLog = Rc::new(RefCell::new(Vec::new()));
    let tx_log: TxLog = Rc::new(RefCell::new(Vec::new()));
    let hdr_for_shim = Rc::clone(&hdr_log);
    let tx_for_shim = Rc::clone(&tx_log);
    t.install_shim(
        wire::TX_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            // device_read = [virtio_net_hdr, frame].
            if chain.device_read.len() < 2 {
                return Err(VirtioError::DeviceFault);
            }
            let mut hdr = [0u8; wire::HEADER_LEN];
            let src = &chain.device_read[0];
            let copy = core::cmp::min(hdr.len(), src.len());
            hdr[..copy].copy_from_slice(&src[..copy]);
            hdr_for_shim.borrow_mut().push(hdr);
            tx_for_shim.borrow_mut().push(chain.device_read[1].to_vec());
            Ok(0)
        }),
    );
    (t, hdr_log, tx_log)
}

#[test]
fn host_csum_negotiation_advertises_tx_csum_tcp() {
    // The device offers VIRTIO_NET_F_CSUM: the driver negotiates it and
    // advertises the TCP transmit-checksum offload (but not UDP).
    let (t, _hdr, _tx) = build_device_tx_csum();
    let net = open_net(t);
    let facts = net.device_facts().expect("facts");
    assert!(facts.offloads.contains(NetOffloads::TX_CSUM_TCP));
    assert!(!facts.offloads.contains(NetOffloads::TX_CSUM_UDP));
}

#[test]
fn tx_checksum_frame_sets_needs_csum_header() {
    let (t, hdr_log, tx_log) = build_device_tx_csum();
    let mut net = open_net(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    let frame = arp_frame();
    rings
        .tx
        .push_with(
            FrameOffload::TxChecksum {
                csum_start: 34,
                csum_offset: 16,
            },
            &frame,
        )
        .expect("queue");
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1);
    deliver_tx(&mut net);
    // The frame bytes are handed over verbatim; the header carries the
    // device's completion request.
    assert_eq!(tx_log.borrow()[0], frame);
    let hdr = hdr_log.borrow()[0];
    assert_eq!(
        hdr[wire::HDR_FLAGS_OFFSET],
        wire::VIRTIO_NET_HDR_F_NEEDS_CSUM
    );
    assert_eq!(
        u16::from_le_bytes([
            hdr[wire::HDR_CSUM_START_OFFSET],
            hdr[wire::HDR_CSUM_START_OFFSET + 1]
        ]),
        34
    );
    assert_eq!(
        u16::from_le_bytes([
            hdr[wire::HDR_CSUM_OFFSET_OFFSET],
            hdr[wire::HDR_CSUM_OFFSET_OFFSET + 1]
        ]),
        16
    );
}

#[test]
fn plain_tx_frame_emits_a_zero_header() {
    // A frame with no transmit offload carries its complete software
    // checksum: the device header is all zero (no completion requested).
    let (t, hdr_log, _tx) = build_device_tx_csum();
    let mut net = open_net(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    rings.tx.push(&arp_frame()).expect("queue");
    net.service(&mut rings).expect("service");
    deliver_tx(&mut net);
    assert_eq!(hdr_log.borrow()[0], [0u8; wire::HEADER_LEN]);
}

#[test]
fn host_tso_negotiation_advertises_tx_segment_tcp() {
    // The device offers CSUM + both HOST_TSO bits: the driver negotiates
    // segmentation offload and advertises it.
    let (t, _hdr, _tx) = build_device_tx_features(
        wire::VIRTIO_NET_F_CSUM | wire::VIRTIO_NET_F_HOST_TSO4 | wire::VIRTIO_NET_F_HOST_TSO6,
    );
    let net = open_net(t);
    let facts = net.device_facts().expect("facts");
    assert!(facts.offloads.contains(NetOffloads::TX_SEGMENT_TCP));
    assert!(facts.offloads.contains(NetOffloads::TX_CSUM_TCP));
}

#[test]
fn tso_without_both_host_tso_bits_is_not_advertised() {
    // Only one of the two HOST_TSO bits: the driver keeps TSO off (the
    // single advertised offload must serve both IP families).
    let (t, _hdr, _tx) =
        build_device_tx_features(wire::VIRTIO_NET_F_CSUM | wire::VIRTIO_NET_F_HOST_TSO4);
    let net = open_net(t);
    assert!(!net
        .device_facts()
        .expect("facts")
        .offloads
        .contains(NetOffloads::TX_SEGMENT_TCP));
}

#[test]
fn tx_segment_frame_sets_the_gso_header() {
    let (t, hdr_log, tx_log) = build_device_tx_features(
        wire::VIRTIO_NET_F_CSUM | wire::VIRTIO_NET_F_HOST_TSO4 | wire::VIRTIO_NET_F_HOST_TSO6,
    );
    let mut net = open_net(t);
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    // A frame larger than one segment (the shim only logs it; the real
    // device would split it against gso_size).
    let frame = vec![0x5Au8; 400];
    rings
        .tx
        .push_with(
            FrameOffload::TxSegment {
                csum_start: 54,
                csum_offset: 16,
                gso_size: 100,
                hdr_len: 74,
                ipv6: true,
            },
            &frame,
        )
        .expect("queue");
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1);
    deliver_tx(&mut net);
    assert_eq!(tx_log.borrow()[0], frame);
    let hdr = hdr_log.borrow()[0];
    assert_eq!(
        hdr[wire::HDR_FLAGS_OFFSET],
        wire::VIRTIO_NET_HDR_F_NEEDS_CSUM
    );
    assert_eq!(
        hdr[wire::HDR_GSO_TYPE_OFFSET],
        wire::VIRTIO_NET_HDR_GSO_TCPV6
    );
    assert_eq!(
        u16::from_le_bytes([
            hdr[wire::HDR_HDR_LEN_OFFSET],
            hdr[wire::HDR_HDR_LEN_OFFSET + 1]
        ]),
        74
    );
    assert_eq!(
        u16::from_le_bytes([
            hdr[wire::HDR_GSO_SIZE_OFFSET],
            hdr[wire::HDR_GSO_SIZE_OFFSET + 1]
        ]),
        100
    );
    assert_eq!(
        u16::from_le_bytes([
            hdr[wire::HDR_CSUM_START_OFFSET],
            hdr[wire::HDR_CSUM_START_OFFSET + 1]
        ]),
        54
    );
}

#[test]
fn tx_checksum_header_suppressed_when_host_csum_not_negotiated() {
    // The default device offers no features: a TxChecksum request on the
    // ring is ignored (the frame already carried a full software checksum
    // and the device was never asked to complete one).
    let (t, tx_log, _rx) = build_device();
    let mut net = open_net(t);
    // Confirm the driver did not advertise the offload.
    assert!(!net
        .device_facts()
        .expect("facts")
        .offloads
        .contains(NetOffloads::TX_CSUM_TCP));
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region, BufferClass::NonSensitive);
    let frame = arp_frame();
    rings
        .tx
        .push_with(
            FrameOffload::TxChecksum {
                csum_start: 34,
                csum_offset: 16,
            },
            &frame,
        )
        .expect("queue");
    // The frame is still transmitted verbatim (the default TX shim only
    // logs the frame, so the zero-header path is exercised without fault).
    let report = net.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1);
    deliver_tx(&mut net);
    assert_eq!(tx_log.borrow()[0], frame);
}
