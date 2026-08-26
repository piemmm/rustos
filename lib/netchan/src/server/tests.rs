//! Host tests for [`NetChannelServer`](super::NetChannelServer): the
//! detached/attached state machine, the geometry validation, and the
//! fail-closed service path, driven against an in-process loopback [`Net`].

extern crate alloc;

use alloc::vec::Vec;

use super::{Drain, DrainAction, DrainStep, Masked, NetChannelServer};
use tairix_abi::driver::net::{
    DeviceFacts, LinkState, MacAddress, McastFilter, Net, NetOffloads, ETHERNET_HEADER_LEN,
};
use tairix_abi::driver::net_channel::{
    decode_facts_reply, decode_service_reply, AttachParams, McastGroups, NetChannelRequest,
    RxFilterPolicy,
};
use tairix_abi::driver::net_ring::{
    aligned_region, FrameOffload, FrameRings, RingGeometry, RxDelivery, ServiceReport,
    REGION_ALIGN_PADDING,
};
use tairix_abi::driver::BufferClass;
use tairix_abi::reply::decode_status_reply;
use tairix_abi::DriverError;
use tairix_abi::Errno;

/// A representative notify endpoint id (any non-reserved value).
const NOTIFY_ENDPOINT: u64 = 0x4E45_5453_5430_3030;

/// The mock device's MTU (standard Ethernet).
const MTU: u32 = 1500;

/// A loopback [`Net`]: frames pushed to the TX ring come straight back on
/// the RX ring, so one service call round-trips a frame end to end.
struct LoopbackNet {
    facts_fault: bool,
}

impl LoopbackNet {
    fn new() -> Self {
        Self { facts_fault: false }
    }
}

impl Net for LoopbackNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        if self.facts_fault {
            return Err(DriverError::DeviceFault);
        }
        Ok(DeviceFacts {
            mac: MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
            mtu: MTU,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
            max_tx_frame: MTU + ETHERNET_HEADER_LEN,
            multicast_filter: McastFilter::Unfiltered,
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        let mut frame = [0u8; 2048];
        loop {
            match rings.tx.pop(&mut frame) {
                Ok(Some(len)) => {
                    report.transmitted += 1;
                    let rx0 = rings.rx_ring(0).map_err(|_| DriverError::BadMagic)?;
                    match rx0.push(&frame[..len]) {
                        Ok(()) => report.record_delivered(),
                        Err(Errno::NoSpace) => {
                            report.rx_ring_full = true;
                            break;
                        }
                        Err(_) => return Err(DriverError::BadMagic),
                    }
                }
                Ok(None) => break,
                Err(Errno::LengthOutOfRange) => {}
                Err(_) => return Err(DriverError::BadMagic),
            }
        }
        Ok(report)
    }
}

/// A geometry sized exactly to the device's largest frame, as the stack
/// derives it from the facts reply.
fn geometry() -> RingGeometry {
    let cap = MTU + ETHERNET_HEADER_LEN;
    RingGeometry::new(8, 8, cap, cap, 1).expect("valid geometry")
}

fn attach_params(geometry: RingGeometry) -> AttachParams {
    AttachParams {
        geometry,
        region_grant: 0x1234,
        class: BufferClass::NonSensitive,
        notify_endpoint: NOTIFY_ENDPOINT,
    }
}

#[test]
fn facts_reply_reports_the_device() {
    let server = NetChannelServer::new(LoopbackNet::new());
    let facts = decode_facts_reply(&server.facts_reply()).expect("facts");
    assert_eq!(facts.mtu, MTU);
    assert_eq!(facts.rx_queues, 1);
}

#[test]
fn facts_reply_carries_a_device_fault() {
    let mut net = LoopbackNet::new();
    net.facts_fault = true;
    let server = NetChannelServer::new(net);
    assert_eq!(
        decode_facts_reply(&server.facts_reply()),
        Err(Errno::DeviceFault)
    );
}

#[test]
fn service_before_attach_fails_closed() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    assert!(!server.is_attached());
    let mut buffer = alloc::vec![0u8; geometry().region_len() + REGION_ALIGN_PADDING];
    let region = aligned_region(&mut buffer, geometry().region_len()).expect("aligned region");
    assert_eq!(
        decode_service_reply(&server.service_reply(region)),
        Err(Errno::NotConnected)
    );
}

#[test]
fn attach_stores_state_and_service_round_trips_a_frame() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let geom = geometry();
    assert!(decode_status_reply(&server.attach(attach_params(geom))).is_ok());
    assert!(server.is_attached());
    assert_eq!(server.geometry(), Some(geom));
    assert_eq!(server.notify_endpoint(), Some(NOTIFY_ENDPOINT));

    // Queue a frame in TX from the stack side, then doorbell.
    let mut buffer = alloc::vec![0u8; geom.region_len() + REGION_ALIGN_PADDING];
    let region = aligned_region(&mut buffer, geom.region_len()).expect("aligned region");
    {
        let mut rings = FrameRings::bind(region, geom, BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0xAB; 64]).expect("queue tx");
    }
    let report = decode_service_reply(&server.service_reply(region)).expect("service");
    assert_eq!(report.transmitted, 1);
    assert_eq!(report.received, 1);

    // The frame looped back onto RX.
    let mut rings = FrameRings::bind(region, geom, BufferClass::NonSensitive).expect("bind");
    let mut out = [0u8; 2048];
    assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut out), Ok(Some(64)));
    assert_eq!(&out[..64], &[0xAB; 64]);
}

#[test]
fn attach_rejects_a_geometry_too_small_for_the_device() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    // A ring whose slots cannot carry the device's full frame (MTU+header).
    let small = RingGeometry::new(8, 8, MTU, MTU, 1).expect("valid but too small");
    assert_eq!(
        decode_status_reply(&server.attach(attach_params(small))),
        Err(Errno::OutOfRange)
    );
    assert!(!server.is_attached());
}

#[test]
fn attach_rejects_a_transmit_slot_larger_than_the_driver_can_stage() {
    // The stack sizes a segmentation super-frame slot from the machine it
    // was attested; this driver reports staging for one link frame only.
    // Accepting the wider slot would drop every super-frame at the pop,
    // silently, so the attach is refused instead.
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let cap = MTU + ETHERNET_HEADER_LEN;
    let wide = RingGeometry::new(8, 2, cap, cap * 8, 1).expect("valid geometry");
    assert_eq!(
        decode_status_reply(&server.attach(attach_params(wide))),
        Err(Errno::OutOfRange)
    );
    assert!(!server.is_attached());
}

#[test]
fn service_rejects_a_wrong_length_region() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let geom = geometry();
    let _ = server.attach(attach_params(geom));
    // A region a byte short of the agreed geometry is refused whole.
    let mut buffer = alloc::vec![0u8; geom.region_len() - 1 + REGION_ALIGN_PADDING];
    let region = aligned_region(&mut buffer, geom.region_len() - 1).expect("aligned region");
    assert_eq!(
        decode_service_reply(&server.service_reply(region)),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn detach_returns_to_the_detached_state() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let _ = server.attach(attach_params(geometry()));
    assert!(server.is_attached());
    assert!(decode_status_reply(&server.detach()).is_ok());
    assert!(!server.is_attached());
    assert_eq!(server.notify_endpoint(), None);
    assert_eq!(server.geometry(), None);
}

/// A device whose group filter is programmable, so the server's
/// `SetMulticast` path can be driven end to end.
struct FilterNet {
    programmed: Option<alloc::vec::Vec<MacAddress>>,
}

impl Net for FilterNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(DeviceFacts {
            mac: MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
            mtu: MTU,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
            max_tx_frame: MTU + ETHERNET_HEADER_LEN,
            multicast_filter: McastFilter::Slots(2),
        })
    }

    fn service(&mut self, _rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        Ok(ServiceReport::default())
    }

    fn set_multicast_groups(&mut self, groups: &[MacAddress]) -> Result<(), DriverError> {
        if groups.len() > 2 {
            return Err(DriverError::LengthOutOfRange);
        }
        self.programmed = Some(groups.to_vec());
        Ok(())
    }
}

#[test]
fn the_group_filter_is_programmable_before_the_channel_is_attached() {
    let mut server = NetChannelServer::new(FilterNet { programmed: None });
    let group = MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]);
    let set = McastGroups::new(&[group]).expect("group set");
    // The filter is device state, not channel state: the stack may program
    // it before frames flow, so a set arriving detached is honoured rather
    // than refused with `NotConnected`.
    assert_eq!(
        decode_status_reply(&server.set_multicast_reply(&set)),
        Ok(())
    );
}

#[test]
fn a_device_that_does_not_filter_groups_refuses_the_set() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let group = MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]);
    let set = McastGroups::new(&[group]).expect("group set");
    // The default refusal makes a driver that claims slots without
    // implementing the filter fail loudly instead of silently dropping every
    // group frame.
    assert_eq!(
        decode_status_reply(&server.set_multicast_reply(&set)),
        Err(Errno::NotImplemented)
    );
}

#[test]
fn an_over_large_group_set_is_refused_by_the_device() {
    let mut server = NetChannelServer::new(FilterNet { programmed: None });
    let groups = [
        MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]),
        MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x02]),
        MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x03]),
    ];
    let set = McastGroups::new(&groups).expect("group set");
    assert_eq!(
        decode_status_reply(&server.set_multicast_reply(&set)),
        Err(Errno::LengthOutOfRange)
    );
}

// --- The drain policy the interrupt path applies ------------------------

/// A report with the given receive count and back-pressure flag, where
/// every harvested frame reached the ring.
fn report(received: u32, rx_ring_full: bool) -> ServiceReport {
    ServiceReport {
        transmitted: 0,
        received,
        harvested: received,
        filtered: 0,
        rx_ring_full,
        link: LinkState::Up,
    }
}

/// A report for a pass that harvested `harvested` frames and shed every one.
fn all_shed(harvested: u32) -> ServiceReport {
    ServiceReport {
        harvested,
        filtered: u64::from(harvested),
        ..report(0, false)
    }
}

#[test]
fn a_device_still_handing_over_frames_keeps_draining() {
    assert_eq!(DrainStep::of(&report(1, false)), DrainStep::Continue);
    assert_eq!(DrainStep::of(&report(64, false)), DrainStep::Continue);
}

#[test]
fn a_pass_that_shed_every_frame_still_counts_as_progress() {
    // The device handed those frames over and may have more, so the drain
    // must stay masked and keep going. Reading `received` here would call
    // this "quiet" and re-arm, and a burst of shed frames — the normal case
    // once the pre-filter is doing its job — would cost one interrupt each.
    assert_eq!(DrainStep::of(&all_shed(1)), DrainStep::Continue);
    assert_eq!(DrainStep::of(&all_shed(64)), DrainStep::Continue);
}

#[test]
fn a_quiet_device_re_arms_its_completion_sources() {
    // Nothing received and room in the ring: the level condition has
    // cleared, so unmasking cannot re-fire immediately.
    assert_eq!(DrainStep::of(&report(0, false)), DrainStep::Quiet);
}

#[test]
fn a_full_receive_ring_holds_the_sources_masked() {
    // Frames are still in the device, so the condition is asserted:
    // unmasking here is exactly the interrupt storm. It stays masked
    // whether or not this pass moved anything.
    assert_eq!(DrainStep::of(&report(0, true)), DrainStep::BackPressure);
    assert_eq!(DrainStep::of(&report(8, true)), DrainStep::BackPressure);
}

// --- The whole drain, as the state machine the serve loop shells ---------

/// Rounds a drain test allows, small enough to exhaust deliberately.
const ROUNDS: u32 = 3;

/// A report carrying a receive count, a back-pressure flag, and a link.
fn linked(received: u32, rx_ring_full: bool, link: LinkState) -> ServiceReport {
    ServiceReport {
        link,
        ..report(received, rx_ring_full)
    }
}

#[test]
fn a_quiet_device_is_re_armed_after_one_look_more() {
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    assert_eq!(
        drain.observe(&report(0, false)),
        DrainAction::UnmaskAndService
    );
    assert_eq!(drain.observe(&report(0, false)), DrainAction::Stop);
    let outcome = drain.outcome();
    assert_eq!(outcome.masked, Masked::No);
    assert!(!outcome.moved);
    assert!(!outcome.masked.needs_release());
}

#[test]
fn a_frame_landing_in_the_re_arm_window_resumes_the_drain_masked() {
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    assert_eq!(
        drain.observe(&report(0, false)),
        DrainAction::UnmaskAndService
    );
    // The look-once-more found work, so the sources go back down and the
    // drain resumes exactly as the interrupt entered it.
    assert_eq!(
        drain.observe(&report(1, false)),
        DrainAction::MaskAndService
    );
    assert_eq!(
        drain.observe(&report(0, false)),
        DrainAction::UnmaskAndService
    );
    assert_eq!(drain.observe(&report(0, false)), DrainAction::Stop);
    assert_eq!(drain.outcome().masked, Masked::No);
    assert!(drain.outcome().moved);
}

#[test]
fn a_full_ring_stops_masked_and_asks_for_release() {
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    assert_eq!(drain.observe(&report(4, true)), DrainAction::Stop);
    let outcome = drain.outcome();
    assert_eq!(outcome.masked, Masked::BackPressure);
    assert!(outcome.masked.needs_release());
}

#[test]
fn a_spent_round_budget_stops_masked_and_asks_for_release() {
    // The defect this closes: the budget bounds the drain, but a device
    // still handing over frames when it runs out leaves the sources masked.
    // Reporting "flowing" there means the stack never doorbells, nothing
    // releases the mask, and the interface goes quietly dead.
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    for _ in 0..ROUNDS {
        assert_eq!(drain.observe(&report(1, false)), DrainAction::Service);
    }
    assert_eq!(drain.observe(&report(1, false)), DrainAction::Stop);
    let outcome = drain.outcome();
    assert_eq!(outcome.masked, Masked::BudgetSpent);
    assert!(
        outcome.masked.needs_release(),
        "the stack must be told to doorbell, or the device stays masked for good"
    );
}

#[test]
fn a_budget_spent_during_the_re_check_leaves_the_sources_up() {
    // The sources are already unmasked here, so nothing is stranded: the
    // work the look-once-more found raises its own interrupt. Claiming
    // "masked" would make the stack chase a release that is not needed.
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    for _ in 0..ROUNDS {
        assert_eq!(drain.observe(&report(1, false)), DrainAction::Service);
    }
    assert_eq!(
        drain.observe(&report(0, false)),
        DrainAction::UnmaskAndService
    );
    assert_eq!(drain.observe(&report(1, false)), DrainAction::Stop);
    assert_eq!(drain.outcome().masked, Masked::No);
}

#[test]
fn a_fault_stops_masked_so_a_broken_device_cannot_storm() {
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    assert_eq!(drain.observe(&report(1, false)), DrainAction::Service);
    drain.fault();
    let outcome = drain.outcome();
    assert_eq!(outcome.masked, Masked::Fault);
    assert!(outcome.masked.needs_release());
}

#[test]
fn only_a_fault_forbids_re_arming_the_completion_sources() {
    // The doorbell path has no notify to carry a release request — a
    // `ServiceReport`'s `rx_ring_full` asks the stack for nothing — so a
    // caller there must re-arm on everything but a fault. Answering "no" for
    // back-pressure leaves the sources down with nothing scheduled to lift
    // them, and the interface silently stops being interrupt-driven.
    assert!(Masked::No.may_rearm());
    assert!(Masked::BackPressure.may_rearm());
    assert!(Masked::BudgetSpent.may_rearm());
    assert!(
        !Masked::Fault.may_rearm(),
        "re-arming into a device that just faulted would storm"
    );
    // Distinct from asking the stack to come and release them, which every
    // masked state does need.
    assert!(!Masked::No.needs_release());
    assert!(Masked::BackPressure.needs_release());
    assert!(Masked::BudgetSpent.needs_release());
    assert!(Masked::Fault.needs_release());
}

#[test]
fn a_link_change_is_reported_even_when_no_frame_moved() {
    // An idle interface's cable pull is exactly this case, and a bond
    // failover keys on the report.
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    assert_eq!(
        drain.observe(&linked(0, false, LinkState::Down)),
        DrainAction::UnmaskAndService
    );
    assert_eq!(
        drain.observe(&linked(0, false, LinkState::Down)),
        DrainAction::Stop
    );
    let outcome = drain.outcome();
    assert!(!outcome.moved);
    assert!(outcome.link_changed);
    assert_eq!(outcome.link, LinkState::Down);
}

#[test]
fn the_pre_filter_count_reaches_the_notify() {
    // The stack reads it from the notify alone on a pure receive, so a
    // drain that loses it freezes the operator's `rx.filtered`.
    let mut drain = Drain::new(LinkState::Up, ROUNDS);
    let mut first = report(1, false);
    first.filtered = 7;
    assert_eq!(drain.observe(&first), DrainAction::Service);
    let mut second = report(0, false);
    second.filtered = 11;
    assert_eq!(drain.observe(&second), DrainAction::UnmaskAndService);
    assert_eq!(drain.observe(&second), DrainAction::Stop);
    assert_eq!(drain.outcome().filtered, 11);
}

// --- The receive pre-filter on the harvest path -------------------------

/// A `Net` that hands one fixed frame over on each service, so a test can
/// see whether the pre-filter shed it.
struct OneFrameNet {
    frame: Vec<u8>,
    /// Services left to deliver on.
    remaining: u32,
    /// The device's cumulative pre-filter count.
    filtered: u64,
}

impl Net for OneFrameNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(DeviceFacts {
            mac: MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
            mtu: MTU,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
            max_tx_frame: MTU + ETHERNET_HEADER_LEN,
            multicast_filter: McastFilter::Unfiltered,
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        if self.remaining > 0 {
            self.remaining -= 1;
            match rings.deliver(0, FrameOffload::None, &self.frame) {
                Ok(RxDelivery::Accepted) => report.record_delivered(),
                Ok(RxDelivery::Filtered) => self.filtered += 1,
                Ok(RxDelivery::RingFull) => report.rx_ring_full = true,
                Err(_) => return Err(DriverError::BadMagic),
            }
        }
        report.filtered = self.filtered;
        Ok(report)
    }
}

/// An Ethernet frame carrying `ethertype` over `payload`.
/// An ICMP-carrying IPv4 packet addressed to `destination`.
///
/// A real header, checksum and all: the classifier parses it and admits
/// anything it cannot read, so a hand-rolled one would make every shedding
/// assertion pass for the wrong reason.
fn ipv4_packet(destination: [u8; 4]) -> Vec<u8> {
    use tairix_net::addr::Ipv4Addr;
    use tairix_net::ipv4::{Ipv4Header, IPV4_HEADER_LEN, PROTOCOL_ICMP};

    let payload = [0u8; 8];
    let header = Ipv4Header::new(
        Ipv4Addr::new(10, 0, 2, 99),
        Ipv4Addr::from(destination),
        PROTOCOL_ICMP,
    );
    let mut packet = alloc::vec![0u8; IPV4_HEADER_LEN + payload.len()];
    header
        .write(&mut packet, payload.len())
        .expect("the buffer is exactly one packet");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&payload);
    packet
}

fn eth_frame(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = alloc::vec![0u8; 14 + payload.len()];
    out[..6].copy_from_slice(&[0xff; 6]);
    out[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 2]);
    out[12..14].copy_from_slice(&ethertype.to_be_bytes());
    out[14..].copy_from_slice(payload);
    out
}

/// Attach a server over a fresh region and return both.
fn attached(net: OneFrameNet) -> (NetChannelServer<OneFrameNet>, alloc::vec::Vec<u8>) {
    let mut server = NetChannelServer::new(net);
    let params = AttachParams {
        geometry: geometry(),
        region_grant: 0x1234,
        class: BufferClass::NonSensitive,
        notify_endpoint: NOTIFY_ENDPOINT,
    };
    assert_eq!(decode_status_reply(&server.attach(params)), Ok(()));
    let buffer = alloc::vec![0u8; geometry().region_len() + REGION_ALIGN_PADDING];
    (server, buffer)
}

#[test]
fn a_frame_the_filter_sheds_never_reaches_the_ring() {
    // An IPv4 unicast addressed to another host on the segment: the rule
    // that carries the real noise reduction on a busy LAN.
    let net = OneFrameNet {
        frame: eth_frame(0x0800, &ipv4_packet([10, 0, 2, 99])),
        remaining: 1,
        filtered: 0,
    };
    let (mut server, mut buffer) = attached(net);
    // A published address set turns the filter on; until then it admits
    // everything.
    let policy = RxFilterPolicy::new(&[([10, 0, 2, 15], [10, 0, 2, 255])], &[], &[], &[], &[]);
    assert_eq!(
        decode_status_reply(&server.set_rx_filter_reply(policy)),
        Ok(())
    );

    let region = aligned_region(&mut buffer, geometry().region_len()).expect("aligned region");
    let report = server.service(region).expect("service");
    assert_eq!(report.received, 0, "the stack is never woken for it");
    assert_eq!(report.filtered, 1, "and the shedding is counted");
    let mut rings = FrameRings::bind(region, geometry(), BufferClass::NonSensitive).expect("bind");
    let mut out = alloc::vec![0u8; 2048];
    assert_eq!(
        rings.rx_ring(0).expect("rx0").pop(&mut out),
        Ok(None),
        "a shed frame is not even copied"
    );
}

#[test]
fn a_frame_addressed_to_us_still_reaches_the_ring() {
    let net = OneFrameNet {
        frame: eth_frame(0x0800, &ipv4_packet([10, 0, 2, 15])),
        remaining: 1,
        filtered: 0,
    };
    let (mut server, mut buffer) = attached(net);
    let policy = RxFilterPolicy::new(&[([10, 0, 2, 15], [10, 0, 2, 255])], &[], &[], &[], &[]);
    assert_eq!(
        decode_status_reply(&server.set_rx_filter_reply(policy)),
        Ok(())
    );

    let region = aligned_region(&mut buffer, geometry().region_len()).expect("aligned region");
    let report = server.service(region).expect("service");
    assert_eq!(report.received, 1);
    assert_eq!(report.filtered, 0);
}

#[test]
fn without_a_published_policy_nothing_is_shed() {
    // The state before the stack has published an address set: an interface
    // still doing DHCP must not have its offer filtered away.
    // Addressed to another host, so a *published* policy would shed it: that
    // is what makes this a test of the unpublished state rather than of a
    // frame nothing could classify either way.
    let net = OneFrameNet {
        frame: eth_frame(0x0800, &ipv4_packet([10, 0, 2, 99])),
        remaining: 1,
        filtered: 0,
    };
    let (mut server, mut buffer) = attached(net);
    let region = aligned_region(&mut buffer, geometry().region_len()).expect("aligned region");
    let report = server.service(region).expect("service");
    assert_eq!(report.received, 1);
    assert_eq!(report.filtered, 0);
}

/// A `Net` whose link state the test controls, so the reported-link
/// tracking can be observed.
struct LinkNet {
    link: LinkState,
}

impl Net for LinkNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(DeviceFacts {
            mac: MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
            mtu: MTU,
            link: self.link,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
            max_tx_frame: MTU + ETHERNET_HEADER_LEN,
            multicast_filter: McastFilter::Unfiltered,
        })
    }

    fn service(&mut self, _rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        Ok(ServiceReport {
            link: self.link,
            ..ServiceReport::default()
        })
    }
}

#[test]
fn the_reported_link_tracks_what_the_device_last_said() {
    // The interrupt path tells a link *change* from a steady state by
    // comparing against this, and that is the only thing that surfaces a
    // cable pull on an interface with no traffic.
    let mut server = NetChannelServer::new(LinkNet {
        link: LinkState::Up,
    });
    let params = AttachParams {
        geometry: geometry(),
        region_grant: 0x1234,
        class: BufferClass::NonSensitive,
        notify_endpoint: NOTIFY_ENDPOINT,
    };
    assert_eq!(decode_status_reply(&server.attach(params)), Ok(()));
    let mut buffer = alloc::vec![0u8; geometry().region_len() + REGION_ALIGN_PADDING];
    let region = aligned_region(&mut buffer, geometry().region_len()).expect("aligned region");

    assert_eq!(server.reported_link(), LinkState::Up);
    server.net_mut().link = LinkState::Down;
    // Still the old value: nothing has serviced the device yet.
    assert_eq!(server.reported_link(), LinkState::Up);
    server.service(region).expect("service");
    assert_eq!(
        server.reported_link(),
        LinkState::Down,
        "a service updates it, so the next drain sees the change"
    );
}

#[test]
fn every_request_variant_has_a_reply() {
    // The serve loop's match over decoded requests is compiled only for a
    // bare-metal target, so a variant added to the contract without a
    // handler there is invisible to a host build. This asserts that every
    // variant has a server reply, which is what the loop dispatches to.
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let params = AttachParams {
        geometry: geometry(),
        region_grant: 0x1234,
        class: BufferClass::NonSensitive,
        notify_endpoint: NOTIFY_ENDPOINT,
    };
    let requests = [
        NetChannelRequest::Facts,
        NetChannelRequest::Attach(params),
        NetChannelRequest::Service,
        NetChannelRequest::SetMulticast(McastGroups::empty()),
        NetChannelRequest::SetRxFilter(RxFilterPolicy::admit_all()),
        NetChannelRequest::Detach,
    ];
    let mut buffer = alloc::vec![0u8; geometry().region_len() + REGION_ALIGN_PADDING];
    for request in requests {
        // An exhaustive match: adding a variant without a reply here is a
        // compile error, exactly as it is in the serve loop.
        match request {
            NetChannelRequest::Facts => {
                assert!(decode_facts_reply(&server.facts_reply()).is_ok());
            }
            NetChannelRequest::Attach(params) => {
                assert_eq!(decode_status_reply(&server.attach(params)), Ok(()));
            }
            NetChannelRequest::Service => {
                let region =
                    aligned_region(&mut buffer, geometry().region_len()).expect("aligned region");
                assert!(decode_service_reply(&server.service_reply(region)).is_ok());
            }
            NetChannelRequest::SetMulticast(groups) => {
                // This device does not filter groups, so a typed refusal is
                // the correct reply — a reply nonetheless.
                assert_eq!(
                    decode_status_reply(&server.set_multicast_reply(&groups)),
                    Err(Errno::NotImplemented)
                );
            }
            NetChannelRequest::SetRxFilter(policy) => {
                assert_eq!(
                    decode_status_reply(&server.set_rx_filter_reply(policy)),
                    Ok(())
                );
            }
            NetChannelRequest::Detach => {
                assert_eq!(decode_status_reply(&server.detach()), Ok(()));
            }
        }
    }
}

// --- The re-arm race the drain loop must close -------------------------

/// A `Net` that hands a frame over only on its `deliver_on`th service, so a
/// test can place a completion in the window between "the device looked
/// quiet" and "the completion sources were re-armed".
struct LateFrameNet {
    services: u32,
    deliver_on: u32,
    /// Every `set_completion_interrupts` argument, in order.
    masking: alloc::vec::Vec<bool>,
}

impl Net for LateFrameNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(DeviceFacts {
            mac: MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
            mtu: MTU,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
            max_tx_frame: MTU + ETHERNET_HEADER_LEN,
            multicast_filter: McastFilter::Unfiltered,
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        self.services += 1;
        let mut report = ServiceReport::default();
        if self.services == self.deliver_on {
            let frame = eth_frame(0x0800, &ipv4_packet([10, 0, 2, 15]));
            if matches!(
                rings.deliver(0, FrameOffload::None, &frame),
                Ok(RxDelivery::Accepted)
            ) {
                report.record_delivered();
            }
        }
        Ok(report)
    }

    fn set_completion_interrupts(&mut self, enabled: bool) -> Result<(), DriverError> {
        self.masking.push(enabled);
        Ok(())
    }
}

#[test]
fn a_completion_landing_as_the_sources_re_arm_is_still_found() {
    // The lost wakeup this closes: service 1 reports nothing, so the drain
    // re-arms — and the frame arrives right then. A source that signals only
    // on a *new* completion raises no interrupt for it, so if the drain
    // believed "quiet" and stopped, the frame would sit in the device for
    // ever and the interface would go dead. Measured as a hung bond-failover
    // vertical on x86_64 and riscv64.
    let mut server = NetChannelServer::new(LateFrameNet {
        services: 0,
        deliver_on: 2,
        masking: alloc::vec::Vec::new(),
    });
    let params = AttachParams {
        geometry: geometry(),
        region_grant: 0x1234,
        class: BufferClass::NonSensitive,
        notify_endpoint: NOTIFY_ENDPOINT,
    };
    assert_eq!(decode_status_reply(&server.attach(params)), Ok(()));
    let mut buffer = alloc::vec![0u8; geometry().region_len() + REGION_ALIGN_PADDING];
    let region = aligned_region(&mut buffer, geometry().region_len()).expect("aligned region");

    // Drive the drain policy the serve loop applies: service, and on a
    // `Quiet` verdict re-arm and look once more.
    let first = server.service(region).expect("service 1");
    assert_eq!(DrainStep::of(&first), DrainStep::Quiet);
    server
        .net_mut()
        .set_completion_interrupts(true)
        .expect("re-arm");
    let second = server.service(region).expect("service 2");
    assert_eq!(
        second.received, 1,
        "the re-check after re-arming is what finds the frame"
    );
    assert_ne!(
        DrainStep::of(&second),
        DrainStep::Quiet,
        "so the drain must go round again rather than stop"
    );

    // And the frame really reached the ring.
    let mut rings = FrameRings::bind(region, geometry(), BufferClass::NonSensitive).expect("bind");
    let mut out = alloc::vec![0u8; 2048];
    assert!(matches!(
        rings.rx_ring(0).expect("rx0").pop(&mut out),
        Ok(Some(_))
    ));
}
