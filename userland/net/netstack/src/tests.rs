//! Engine-level end-to-end tests over a loopback fake, plus the
//! audited capability-refusal matrix of the request dispatcher.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use rustos_abi::driver::net::{DeviceFacts, LinkState, MacAddress, Net, NetOffloads};
use rustos_abi::driver::net_ring::{FrameRings, RingGeometry, ServiceReport};
use rustos_abi::driver::BufferClass;
use rustos_abi::net_ipc::{
    decode_counters_reply, decode_page_reply, NetAddrFamily, NetIfKind, NetInterfaceFactsRecord,
    NetInterfaceStateRecord, NetstackRequest, IF_NAME_LEN, NETSTACK_MAX_REPLY,
};
use rustos_abi::reply::decode_status_reply;
use rustos_abi::{
    CapabilityId, CapabilitySummary, DriverError, Duration64, Errno, Origin, ProcId, TrustDomain,
};
use rustos_log::{Event, EventId, Level, Sink};
use rustos_net::addr::{IpAddr, Ipv4Addr};
use rustos_net::stack::{Stack, StackConfig, StackEvent};

use crate::{serve, Caller, Netstack};

const MAC_A: MacAddress = MacAddress([0x02, 0xAA, 0, 0, 0, 0x01]);
const MAC_B: MacAddress = MacAddress([0x02, 0xBB, 0, 0, 0, 0x02]);
const IID_A: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0xA1];
const IID_B: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0xB2];
const V4_A: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const V4_B: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

fn t(secs: i64) -> Duration64 {
    Duration64::from_secs(secs)
}

fn facts(mac: MacAddress) -> DeviceFacts {
    DeviceFacts {
        mac,
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
    }
}

fn name(text: &str) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..text.len()].copy_from_slice(text.as_bytes());
    out
}

/// Ring geometry ample for the control-plane exchanges the tests run.
const GEOMETRY: RingGeometry = match RingGeometry::new(16, 1514) {
    Ok(g) => g,
    Err(_) => panic!("valid test geometry"),
};

fn rings_region() -> Vec<u8> {
    vec![0u8; GEOMETRY.region_len()]
}

fn bind_rings(region: &mut [u8]) -> FrameRings<'_> {
    FrameRings::bind(region, GEOMETRY, BufferClass::NonSensitive).expect("bind rings")
}

/// A loopback [`Net`] fake: the "device" is a full peer `Stack`, so a
/// frame queued for transmit is answered exactly as a live peer host
/// would answer it (ARP, ND, DAD, echo).
struct PeerNet {
    stack: Stack,
    now: Duration64,
    scratch: Vec<u8>,
}

impl PeerNet {
    fn new(now: Duration64) -> Self {
        let mut stack =
            Stack::new(&StackConfig::new(facts(MAC_B), IID_B, 0x4242), now).expect("peer stack");
        stack.set_ipv4_config(V4_B, 24, None).expect("peer v4");
        Self {
            stack,
            now,
            scratch: vec![0u8; 2048],
        }
    }
}

impl Net for PeerNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(facts(MAC_B))
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        // The peer's own timers first (its DAD probes, solicitations).
        let out = self.stack.advance(self.now);
        for frame in &out.frames {
            if rings.rx.push(frame).is_ok() {
                report.received += 1;
            }
        }
        // Answer every queued transmit as the peer host.
        loop {
            match rings.tx.pop(&mut self.scratch) {
                Ok(Some(len)) => {
                    report.transmitted += 1;
                    let out = self.stack.on_frame(&self.scratch[..len], self.now);
                    for frame in &out.frames {
                        if rings.rx.push(frame).is_ok() {
                            report.received += 1;
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => return Err(DriverError::BadMagic),
            }
        }
        Ok(report)
    }
}

/// Records every event it receives so tests can assert on audit output.
struct RecordingSink {
    events: RefCell<Vec<(Level, EventId)>>,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            events: RefCell::new(Vec::new()),
        }
    }

    fn ids(&self) -> Vec<u32> {
        self.events.borrow().iter().map(|(_, id)| id.0).collect()
    }
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.events.borrow_mut().push((event.level, event.id));
    }
}

/// The capabilities a test caller's attested origin should summarise.
fn caller(caps: &[CapabilityId]) -> Caller {
    let mut summary = CapabilitySummary::EMPTY;
    for cap in caps {
        summary.insert(*cap);
    }
    Caller::new(Origin::new(
        TrustDomain::User,
        1000,
        100,
        10,
        ProcId::from_raw([0x10; 16]),
        summary,
        rustos_abi::ORIGIN_CONSOLE_NONE,
    ))
}

fn admin() -> Caller {
    caller(&[CapabilityId::NET_ADMIN])
}

fn broker() -> Caller {
    caller(&[CapabilityId::SYSINFO_INTROSPECT])
}

/// A `Netstack` managing one Ethernet interface named `wan`.
fn managed_stack() -> Netstack {
    let mut stack = Netstack::new();
    stack
        .add_interface(
            name("wan"),
            NetIfKind::Ethernet,
            facts(MAC_A),
            IID_A,
            7,
            t(0),
        )
        .expect("add interface");
    stack
}

fn serve_ok(
    stack: &mut Netstack,
    who: &Caller,
    request: &NetstackRequest,
    reply: &mut [u8],
) -> usize {
    let sink = RecordingSink::new();
    serve(stack, who, &sink, &request.to_le_bytes(), reply, t(2)).expect("served")
}

// --- Engine-level end-to-end over the loopback fake -----------------

#[test]
fn loopback_v4_ping_round_trips_through_the_pump() {
    let mut stack = managed_stack();
    stack
        .addr_add(name("wan"), NetAddrFamily::V4, 24, v4_bytes(V4_A), t(1))
        .expect("addr add");
    let mut peer = PeerNet::new(t(2));
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region);

    // Queue an echo request; it parks on ARP resolution, so the
    // queued frame is the ARP request the pump must round-trip.
    let out = stack
        .interface_mut(name("wan"))
        .expect("iface")
        .stack_mut()
        .send_echo_request(IpAddr::V4(V4_B), 0x77, 1, b"rustos-netstack", t(2))
        .expect("send echo");
    for frame in &out.frames {
        rings.tx.push(frame).expect("queue");
    }

    // One pump settles the ARP exchange and the echo round-trip: the
    // peer answers inside the same service call.
    let mut events = Vec::new();
    for _ in 0..4 {
        events.extend(
            stack
                .service_interface(name("wan"), &mut peer, &mut rings, t(2))
                .expect("pump"),
        );
        if !events.is_empty() {
            break;
        }
    }
    assert!(events.contains(&StackEvent::EchoReply {
        source: IpAddr::V4(V4_B),
        identifier: 0x77,
        sequence: 1,
        payload: b"rustos-netstack".to_vec(),
    }));

    // The counters observed the exchange.
    let counters = stack.counters(name("wan")).expect("counters");
    assert!(counters.tx_frames >= 2, "ARP request + echo request");
    assert!(counters.rx_frames >= 2, "ARP reply + echo reply");
}

#[test]
fn loopback_v6_link_local_ping_round_trips_after_dad() {
    let mut stack = managed_stack();
    let mut peer = PeerNet::new(t(0));
    let mut region = rings_region();
    let mut rings = bind_rings(&mut region);

    // Bring both link-local addresses through DAD: the pump at t0
    // exchanges the DAD probes, t1 confirms.
    let mut events = Vec::new();
    for step in 0..2 {
        peer.now = t(step);
        events.extend(
            stack
                .service_interface(name("wan"), &mut peer, &mut rings, t(step))
                .expect("pump"),
        );
    }
    let ours = link_local(IID_A);
    assert!(events.contains(&StackEvent::AddressPreferred { addr: ours }));

    // Ping the peer's link-local address.
    peer.now = t(3);
    let dest = link_local(IID_B);
    let out = stack
        .interface_mut(name("wan"))
        .expect("iface")
        .stack_mut()
        .send_echo_request(IpAddr::V6(dest), 0x42, 7, b"ping6", t(3))
        .expect("send echo");
    for frame in &out.frames {
        rings.tx.push(frame).expect("queue");
    }
    let mut events = Vec::new();
    for _ in 0..4 {
        events.extend(
            stack
                .service_interface(name("wan"), &mut peer, &mut rings, t(3))
                .expect("pump"),
        );
        if !events.is_empty() {
            break;
        }
    }
    assert!(events.contains(&StackEvent::EchoReply {
        source: IpAddr::V6(dest),
        identifier: 0x42,
        sequence: 7,
        payload: b"ping6".to_vec(),
    }));
}

fn link_local(iid: [u8; 8]) -> rustos_net::addr::Ipv6Addr {
    let mut octets = [0u8; 16];
    octets[0] = 0xFE;
    octets[1] = 0x80;
    octets[8..].copy_from_slice(&iid);
    rustos_net::addr::Ipv6Addr::from(octets)
}

fn v4_bytes(addr: Ipv4Addr) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&addr.octets());
    out
}

// --- The dispatcher: capability gates, audit, replies ----------------

#[test]
fn admin_surface_is_denied_without_net_admin() {
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    for request in [
        NetstackRequest::InterfaceList,
        NetstackRequest::AddrAdd {
            iface: name("wan"),
            family: NetAddrFamily::V4,
            prefix: 24,
            addr: v4_bytes(V4_A),
        },
        NetstackRequest::RouteAdd {
            iface: name("wan"),
            family: NetAddrFamily::V4,
            prefix: 0,
            dest: [0; 16],
            next_hop: Some(v4_bytes(V4_B)),
        },
        NetstackRequest::Counters { iface: name("wan") },
    ] {
        // A broker capability does not open the admin surface.
        assert_eq!(
            serve(
                &mut stack,
                &broker(),
                &sink,
                &request.to_le_bytes(),
                &mut reply,
                t(2),
            ),
            Err(Errno::PermissionDenied)
        );
    }
    assert_eq!(sink.ids(), vec![16_002; 4], "every denial is audited");
}

#[test]
fn broker_reads_are_denied_without_sysinfo_introspect() {
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    for request in [
        NetstackRequest::InterfaceFacts {
            offset: 0,
            limit: 8,
        },
        NetstackRequest::InterfaceState {
            offset: 0,
            limit: 8,
        },
    ] {
        // An admin capability does not open the whole-state reads.
        assert_eq!(
            serve(
                &mut stack,
                &admin(),
                &sink,
                &request.to_le_bytes(),
                &mut reply,
                t(2),
            ),
            Err(Errno::PermissionDenied)
        );
    }
    assert_eq!(sink.ids(), vec![16_002; 2]);
}

#[test]
fn addr_and_route_add_apply_and_are_audited() {
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    let addr = NetstackRequest::AddrAdd {
        iface: name("wan"),
        family: NetAddrFamily::V4,
        prefix: 24,
        addr: v4_bytes(V4_A),
    };
    let len = serve(
        &mut stack,
        &admin(),
        &sink,
        &addr.to_le_bytes(),
        &mut reply,
        t(1),
    )
    .expect("addr add");
    decode_status_reply(&reply[..len]).expect("ok status");
    let route = NetstackRequest::RouteAdd {
        iface: name("wan"),
        family: NetAddrFamily::V4,
        prefix: 0,
        dest: [0; 16],
        next_hop: Some(v4_bytes(V4_B)),
    };
    let len = serve(
        &mut stack,
        &admin(),
        &sink,
        &route.to_le_bytes(),
        &mut reply,
        t(1),
    )
    .expect("route add");
    decode_status_reply(&reply[..len]).expect("ok status");
    assert_eq!(sink.ids(), vec![16_001; 2], "both mutations audited");

    // The state read reflects the assignment.
    let request = NetstackRequest::InterfaceState {
        offset: 0,
        limit: 8,
    };
    let len = serve_ok(&mut stack, &broker(), &request, &mut reply);
    let (count, body) =
        decode_page_reply(&reply[..len], NetInterfaceStateRecord::WIRE_LEN).expect("page");
    assert_eq!(count, 1);
    let record = NetInterfaceStateRecord::from_bytes(body).expect("record");
    assert_eq!(record.name, name("wan"));
    assert!(record.link_up);
    assert!((0..record.addr_count as usize).any(|i| {
        record.addrs[i].family == NetAddrFamily::V4 && record.addrs[i].addr == v4_bytes(V4_A)
    }));
}

#[test]
fn mutations_on_an_unknown_interface_are_refused_and_audited() {
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    let request = NetstackRequest::AddrAdd {
        iface: name("lan9"),
        family: NetAddrFamily::V4,
        prefix: 24,
        addr: v4_bytes(V4_A),
    };
    assert_eq!(
        serve(
            &mut stack,
            &admin(),
            &sink,
            &request.to_le_bytes(),
            &mut reply,
            t(1),
        ),
        Err(Errno::NotFound)
    );
    assert_eq!(sink.ids(), vec![16_004]);
}

#[test]
fn malformed_frames_are_refused_and_audited() {
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    let mut bad = NetstackRequest::InterfaceList.to_le_bytes();
    bad[0] ^= 0xFF;
    assert_eq!(
        serve(&mut stack, &admin(), &sink, &bad, &mut reply, t(1)),
        Err(Errno::BadMagic)
    );
    assert_eq!(sink.ids(), vec![16_003]);
}

#[test]
fn interface_list_names_the_managed_interfaces() {
    let mut stack = managed_stack();
    stack
        .add_interface(
            name("lan0"),
            NetIfKind::Ethernet,
            facts(MAC_B),
            IID_B,
            9,
            t(0),
        )
        .expect("second interface");
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    let len = serve_ok(
        &mut stack,
        &admin(),
        &NetstackRequest::InterfaceList,
        &mut reply,
    );
    let (count, body) = decode_page_reply(&reply[..len], IF_NAME_LEN).expect("page");
    assert_eq!(count, 2);
    assert_eq!(&body[..IF_NAME_LEN], &name("wan"));
    assert_eq!(&body[IF_NAME_LEN..], &name("lan0"));
}

#[test]
fn facts_page_reports_the_device_report() {
    let mut stack = managed_stack();
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    let request = NetstackRequest::InterfaceFacts {
        offset: 0,
        limit: 8,
    };
    let len = serve_ok(&mut stack, &broker(), &request, &mut reply);
    let (count, body) =
        decode_page_reply(&reply[..len], NetInterfaceFactsRecord::WIRE_LEN).expect("page");
    assert_eq!(count, 1);
    let record = NetInterfaceFactsRecord::from_bytes(body).expect("record");
    assert_eq!(record.name, name("wan"));
    assert_eq!(record.kind, NetIfKind::Ethernet);
    assert_eq!(record.mac, MAC_A.0);
    assert_eq!(record.mtu, 1500);
    assert_eq!(record.rx_queues, 1);
}

#[test]
fn counters_reply_round_trips() {
    let mut stack = managed_stack();
    let mut reply = [0u8; NETSTACK_MAX_REPLY];
    let request = NetstackRequest::Counters { iface: name("wan") };
    let len = serve_ok(&mut stack, &admin(), &request, &mut reply);
    let counters = decode_counters_reply(&reply[..len]).expect("counters");
    assert_eq!(counters.rx_frames, 0);
    assert_eq!(counters.tx_frames, 0);
}

#[test]
fn duplicate_interface_aliases_are_refused() {
    let mut stack = managed_stack();
    assert_eq!(
        stack.add_interface(
            name("wan"),
            NetIfKind::Ethernet,
            facts(MAC_B),
            IID_B,
            9,
            t(0)
        ),
        Err(Errno::AlreadyExists)
    );
}
