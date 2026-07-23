//! Engine-level end-to-end tests over a loopback fake, plus the
//! audited capability-refusal matrix of the request dispatcher.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, Net, NetOffloads};
use tairix_abi::driver::net_ring::{FrameRings, RingGeometry, ServiceReport};
use tairix_abi::driver::BufferClass;
use tairix_abi::net::{
    decode_bind_reply, decode_send_reply, decode_socket_reply, SocketAddr, SocketRequest,
    SocketStreamEvent, SocketType,
};
use tairix_abi::net_ipc::{
    decode_counters_reply, decode_page_reply, NetAddrFamily, NetIfKind, NetInterfaceFactsRecord,
    NetInterfaceStateRecord, NetstackRequest, IF_NAME_LEN, NETSTACK_MAX_REPLY,
};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::{
    CapabilityId, CapabilitySummary, DriverError, Duration64, Errno, Origin, ProcId, TrustDomain,
};
use tairix_log::{Event, EventId, Level, Sink};
use tairix_net::addr::{IpAddr, Ipv4Addr};
use tairix_net::stack::{Stack, StackConfig, StackEvent};

use crate::channel::{FrameService, LocalFrameService};
use crate::socket::{Delivery, SocketService, MAX_SOCKETS_PER_PRINCIPAL};
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

/// Wrap a loopback [`PeerNet`] as the in-process [`LocalFrameService`] the
/// pump drives.
fn local_service(net: PeerNet, region: &mut [u8]) -> LocalFrameService<'_, PeerNet> {
    LocalFrameService::new(net, region, GEOMETRY, BufferClass::NonSensitive).expect("frame service")
}

/// Queue engine output onto the service's TX ring before pumping, exactly
/// as the service layer would.
fn push_tx<N: Net>(fs: &mut LocalFrameService<'_, N>, frames: &[Vec<u8>]) {
    let mut rings =
        FrameRings::bind(fs.region_mut(), GEOMETRY, BufferClass::NonSensitive).expect("bind");
    for frame in frames {
        rings.tx.push(frame).expect("queue");
    }
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
        tairix_abi::ORIGIN_CONSOLE_NONE,
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
    let mut region = rings_region();
    let mut fs = local_service(PeerNet::new(t(2)), &mut region);

    // Queue an echo request; it parks on ARP resolution, so the
    // queued frame is the ARP request the pump must round-trip.
    let out = stack
        .interface_mut(name("wan"))
        .expect("iface")
        .stack_mut()
        .send_echo_request(IpAddr::V4(V4_B), 0x77, 1, b"tairix-netstack", t(2))
        .expect("send echo");
    push_tx(&mut fs, &out.frames);

    // One pump settles the ARP exchange and the echo round-trip: the
    // peer answers inside the same service call.
    let mut events = Vec::new();
    for _ in 0..4 {
        events.extend(
            stack
                .service_interface(name("wan"), &mut fs, t(2))
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
        payload: b"tairix-netstack".to_vec(),
    }));

    // The counters observed the exchange.
    let counters = stack.counters(name("wan")).expect("counters");
    assert!(counters.tx_frames >= 2, "ARP request + echo request");
    assert!(counters.rx_frames >= 2, "ARP reply + echo reply");
}

#[test]
fn loopback_v6_link_local_ping_round_trips_after_dad() {
    let mut stack = managed_stack();
    let mut region = rings_region();
    let mut fs = local_service(PeerNet::new(t(0)), &mut region);

    // Bring both link-local addresses through DAD: the pump at t0
    // exchanges the DAD probes, t1 confirms.
    let mut events = Vec::new();
    for step in 0..2 {
        fs.net_mut().now = t(step);
        events.extend(
            stack
                .service_interface(name("wan"), &mut fs, t(step))
                .expect("pump"),
        );
    }
    let ours = link_local(IID_A);
    assert!(events.contains(&StackEvent::AddressPreferred { addr: ours }));

    // Ping the peer's link-local address.
    fs.net_mut().now = t(3);
    let dest = link_local(IID_B);
    let out = stack
        .interface_mut(name("wan"))
        .expect("iface")
        .stack_mut()
        .send_echo_request(IpAddr::V6(dest), 0x42, 7, b"ping6", t(3))
        .expect("send echo");
    push_tx(&mut fs, &out.frames);
    let mut events = Vec::new();
    for _ in 0..4 {
        events.extend(
            stack
                .service_interface(name("wan"), &mut fs, t(3))
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

fn link_local(iid: [u8; 8]) -> tairix_net::addr::Ipv6Addr {
    let mut octets = [0u8; 16];
    octets[0] = 0xFE;
    octets[1] = 0x80;
    octets[8..].copy_from_slice(&iid);
    tairix_net::addr::Ipv6Addr::from(octets)
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

// --- The socket service ---------------------------------------------

/// A caller holding `CAP_NET` **and** `CAP_NET_BIND_PRIVILEGED`, keyed to
/// process instance `proc_byte` so tests can model distinct principals.
/// The privileged-bind grant lets these tests bind well-known ports (e.g.
/// the echo port 7); the *denial* of an unprivileged bind is covered by
/// its own test with a bare `CAP_NET` caller.
fn net_caller(proc_byte: u8) -> Caller {
    let mut summary = CapabilitySummary::EMPTY;
    summary.insert(CapabilityId::NET);
    summary.insert(CapabilityId::NET_BIND_PRIVILEGED);
    Caller::new(Origin::new(
        TrustDomain::User,
        1000,
        100,
        u64::from(proc_byte),
        ProcId::from_raw([proc_byte; 16]),
        summary,
        tairix_abi::ORIGIN_CONSOLE_NONE,
    ))
}

/// A deterministic entropy source for ephemeral-port draws.
fn counter_entropy() -> impl FnMut() -> u32 {
    let mut seed: u32 = 0xC0FF_EE00;
    move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed
    }
}

fn encode_request(request: &SocketRequest<'_>) -> Vec<u8> {
    let mut buf = vec![0u8; 128];
    let len = request.encode(&mut buf).expect("encode request");
    buf.truncate(len);
    buf
}

fn v4_addr(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&[a, b, c, d]);
    SocketAddr {
        family: NetAddrFamily::V4,
        addr,
        port,
    }
}

/// A `Netstack` with `wan` configured with `V4_A/24` (a connected route
/// covering `V4_B`) so socket sends have somewhere to go.
fn routed_stack() -> Netstack {
    let mut stack = managed_stack();
    stack
        .addr_add(name("wan"), NetAddrFamily::V4, 24, v4_bytes(V4_A), t(1))
        .expect("addr add");
    stack
}

#[test]
fn socket_open_requires_cap_net() {
    let mut svc = SocketService::new();
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    // A caller with only CAP_NET_ADMIN cannot open a socket.
    assert_eq!(
        svc.serve(
            &mut stack,
            &admin(),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2)
        ),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(sink.ids(), vec![16_006], "the denial is audited");
    assert!(svc.is_empty());
}

#[test]
fn socket_open_and_bind_assigns_a_port() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];

    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open");
    let id = decode_socket_reply(&reply[..out.len]).expect("socket id");
    assert_eq!(svc.len(), 1);

    // Bind to an explicit port.
    let request = encode_request(&SocketRequest::Bind {
        socket: id,
        local: v4_addr(0, 0, 0, 0, 7000),
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("bind");
    assert_eq!(decode_bind_reply(&reply[..out.len]).expect("port"), 7000);

    // A second socket cannot take the same port (globally unique).
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5001,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open 2");
    let id2 = decode_socket_reply(&reply[..out.len]).expect("id2");
    let request = encode_request(&SocketRequest::Bind {
        socket: id2,
        local: v4_addr(0, 0, 0, 0, 7000),
    });
    assert_eq!(
        svc.serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2)
        ),
        Err(Errno::AddressInUse)
    );
}

#[test]
fn socket_open_rejects_zero_delivery_port() {
    let mut svc = SocketService::new();
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0,
    });
    assert_eq!(
        svc.serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2)
        ),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_handle_is_scoped_to_its_creating_principal() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open");
    let id = decode_socket_reply(&reply[..out.len]).expect("id");
    // A different principal cannot touch the handle: absent, not denied.
    let request = encode_request(&SocketRequest::Bind {
        socket: id,
        local: v4_addr(0, 0, 0, 0, 7000),
    });
    assert_eq!(
        svc.serve(
            &mut stack,
            &net_caller(2),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2)
        ),
        Err(Errno::NotFound)
    );
}

#[test]
fn per_principal_socket_quota_fails_closed() {
    let mut svc = SocketService::new();
    let mut stack = managed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    for _ in 0..MAX_SOCKETS_PER_PRINCIPAL {
        svc.serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open within quota");
    }
    assert_eq!(
        svc.serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2)
        ),
        Err(Errno::LimitExceeded)
    );
    // A different principal still has its own quota.
    svc.serve(
        &mut stack,
        &net_caller(2),
        &sink,
        &mut ent,
        &request,
        &mut reply,
        t(2),
    )
    .expect("other principal unaffected");
}

#[test]
fn unicast_send_originates_frames() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open");
    let id = decode_socket_reply(&reply[..out.len]).expect("id");
    // Send to V4_B (on-link); the engine parks on ARP and emits the
    // resolution frame now, so `tx` is non-empty on `wan`.
    let request = encode_request(&SocketRequest::Send {
        socket: id,
        dest: Some(v4_addr(10, 0, 2, 2, 7)),
        payload: b"hello",
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("send");
    assert_eq!(out.tx.len(), 1, "one interface carried it");
    assert_eq!(out.tx[0].0, name("wan"));
    assert!(!out.tx[0].1.is_empty(), "ARP request emitted");
}

#[test]
fn send_without_a_peer_or_dest_is_not_connected() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open");
    let id = decode_socket_reply(&reply[..out.len]).expect("id");
    let request = encode_request(&SocketRequest::Send {
        socket: id,
        dest: None,
        payload: b"x",
    });
    assert_eq!(
        svc.serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2)
        ),
        Err(Errno::NotConnected)
    );
}

#[test]
fn inbound_datagram_is_delivered_to_the_bound_socket() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    // Open + bind port 7 for delivery port 0x5000.
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open");
    let id = decode_socket_reply(&reply[..out.len]).expect("id");
    let request = encode_request(&SocketRequest::Bind {
        socket: id,
        local: v4_addr(0, 0, 0, 0, 7),
    });
    svc.serve(
        &mut stack,
        &net_caller(1),
        &sink,
        &mut ent,
        &request,
        &mut reply,
        t(2),
    )
    .expect("bind");

    let event = StackEvent::UdpDatagram {
        source: IpAddr::V4(V4_B),
        destination: IpAddr::V4(V4_A),
        source_port: 40000,
        destination_port: 7,
        payload: b"payload".to_vec(),
    };
    let deliveries = svc.deliver(&event);
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].deliver_port, 0x5000);
    let parsed = tairix_abi::net::SocketDatagram::parse(&deliveries[0].datagram).expect("datagram");
    assert_eq!(parsed.socket, id);
    assert_eq!(parsed.source.port, 40000);
    assert_eq!(parsed.payload, b"payload");

    // A datagram to an unbound port reaches no socket.
    let other = StackEvent::UdpDatagram {
        source: IpAddr::V4(V4_B),
        destination: IpAddr::V4(V4_A),
        source_port: 40000,
        destination_port: 9,
        payload: b"x".to_vec(),
    };
    assert!(svc.deliver(&other).is_empty());
}

#[test]
fn a_connected_socket_only_receives_from_its_peer() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open");
    let id = decode_socket_reply(&reply[..out.len]).expect("id");
    let request = encode_request(&SocketRequest::Bind {
        socket: id,
        local: v4_addr(0, 0, 0, 0, 7),
    });
    svc.serve(
        &mut stack,
        &net_caller(1),
        &sink,
        &mut ent,
        &request,
        &mut reply,
        t(2),
    )
    .expect("bind");
    let request = encode_request(&SocketRequest::Connect {
        socket: id,
        peer: v4_addr(10, 0, 2, 2, 40000),
    });
    svc.serve(
        &mut stack,
        &net_caller(1),
        &sink,
        &mut ent,
        &request,
        &mut reply,
        t(2),
    )
    .expect("connect");

    // From the connected peer: delivered.
    let from_peer = StackEvent::UdpDatagram {
        source: IpAddr::V4(V4_B),
        destination: IpAddr::V4(V4_A),
        source_port: 40000,
        destination_port: 7,
        payload: b"ok".to_vec(),
    };
    assert_eq!(svc.deliver(&from_peer).len(), 1);
    // From a stranger: dropped.
    let from_other = StackEvent::UdpDatagram {
        source: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 9)),
        destination: IpAddr::V4(V4_A),
        source_port: 40000,
        destination_port: 7,
        payload: b"no".to_vec(),
    };
    assert!(svc.deliver(&from_other).is_empty());
}

#[test]
fn multicast_join_gates_group_delivery() {
    const GROUP: Ipv4Addr = Ipv4Addr::new(239, 1, 2, 3);
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type: SocketType::Datagram,
        deliver_port: 0x5000,
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("open");
    let id = decode_socket_reply(&reply[..out.len]).expect("id");
    let request = encode_request(&SocketRequest::Bind {
        socket: id,
        local: v4_addr(0, 0, 0, 0, 7000),
    });
    svc.serve(
        &mut stack,
        &net_caller(1),
        &sink,
        &mut ent,
        &request,
        &mut reply,
        t(2),
    )
    .expect("bind");

    let event = StackEvent::UdpDatagram {
        source: IpAddr::V4(V4_B),
        destination: IpAddr::V4(GROUP),
        source_port: 5000,
        destination_port: 7000,
        payload: b"m".to_vec(),
    };
    // Not joined: no delivery even though the port matches.
    assert!(svc.deliver(&event).is_empty());
    // Join, then it is delivered; the join emitted an IGMP report.
    let request = encode_request(&SocketRequest::JoinMulticast {
        socket: id,
        group: local_group(GROUP),
    });
    let out = svc
        .serve(
            &mut stack,
            &net_caller(1),
            &sink,
            &mut ent,
            &request,
            &mut reply,
            t(2),
        )
        .expect("join");
    assert!(!out.tx.is_empty(), "IGMP report emitted on join");
    assert_eq!(svc.deliver(&event).len(), 1);
}

fn local_group(addr: Ipv4Addr) -> SocketAddr {
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&addr.octets());
    SocketAddr {
        family: NetAddrFamily::V4,
        addr: bytes,
        port: 0,
    }
}

/// Open a socket of `sock_type` for `who`, returning its handle.
fn open_socket(
    svc: &mut SocketService,
    stack: &mut Netstack,
    who: &Caller,
    sock_type: SocketType,
) -> Result<u32, Errno> {
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let request = encode_request(&SocketRequest::Socket {
        family: NetAddrFamily::V4,
        sock_type,
        deliver_port: 0x5000,
    });
    let out = svc.serve(stack, who, &sink, &mut ent, &request, &mut reply, t(2))?;
    decode_socket_reply(&reply[..out.len])
}

/// Serve one already-built request for `who`, discarding the reply frame.
fn serve_req(
    svc: &mut SocketService,
    stack: &mut Netstack,
    who: &Caller,
    request: &SocketRequest<'_>,
) -> Result<(), Errno> {
    let sink = RecordingSink::new();
    let mut ent = counter_entropy();
    let mut reply = [0u8; 64];
    let bytes = encode_request(request);
    svc.serve(stack, who, &sink, &mut ent, &bytes, &mut reply, t(2))
        .map(|_| ())
}

#[test]
fn binding_a_privileged_port_requires_the_capability() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    // A bare CAP_NET principal; `caller` keys every summary to one ProcId,
    // so the privileged caller below owns the same socket.
    let bare = caller(&[CapabilityId::NET]);
    let id = open_socket(&mut svc, &mut stack, &bare, SocketType::Stream).expect("open");

    // Binding a well-known port without CAP_NET_BIND_PRIVILEGED is denied.
    assert_eq!(
        serve_req(
            &mut svc,
            &mut stack,
            &bare,
            &SocketRequest::Bind {
                socket: id,
                local: v4_addr(0, 0, 0, 0, 80),
            },
        ),
        Err(Errno::PermissionDenied),
    );
    // An ephemeral (port 0) bind is never privileged.
    serve_req(
        &mut svc,
        &mut stack,
        &bare,
        &SocketRequest::Bind {
            socket: id,
            local: v4_addr(0, 0, 0, 0, 0),
        },
    )
    .expect("ephemeral bind is unprivileged");

    // The same well-known bind succeeds for a principal holding the cap.
    let privd = caller(&[CapabilityId::NET, CapabilityId::NET_BIND_PRIVILEGED]);
    let id2 = open_socket(&mut svc, &mut stack, &privd, SocketType::Stream).expect("open");
    serve_req(
        &mut svc,
        &mut stack,
        &privd,
        &SocketRequest::Bind {
            socket: id2,
            local: v4_addr(0, 0, 0, 0, 80),
        },
    )
    .expect("privileged bind allowed with the capability");
}

#[test]
fn listen_requires_a_bound_stream_socket() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let who = net_caller(1);

    // A datagram socket cannot listen.
    let dg = open_socket(&mut svc, &mut stack, &who, SocketType::Datagram).expect("open dg");
    assert_eq!(
        serve_req(
            &mut svc,
            &mut stack,
            &who,
            &SocketRequest::Listen { socket: dg }
        ),
        Err(Errno::OutOfRange),
    );

    // An unbound stream socket cannot listen.
    let st = open_socket(&mut svc, &mut stack, &who, SocketType::Stream).expect("open st");
    assert_eq!(
        serve_req(
            &mut svc,
            &mut stack,
            &who,
            &SocketRequest::Listen { socket: st }
        ),
        Err(Errno::AddressUnavailable),
    );

    // Bound, it listens; a second listen is refused (no longer a stream).
    serve_req(
        &mut svc,
        &mut stack,
        &who,
        &SocketRequest::Bind {
            socket: st,
            local: v4_addr(0, 0, 0, 0, 8080),
        },
    )
    .expect("bind");
    serve_req(
        &mut svc,
        &mut stack,
        &who,
        &SocketRequest::Listen { socket: st },
    )
    .expect("listen");
    assert_eq!(
        serve_req(
            &mut svc,
            &mut stack,
            &who,
            &SocketRequest::Listen { socket: st }
        ),
        Err(Errno::OutOfRange),
    );
}

#[test]
fn accept_without_a_ready_connection_would_block() {
    let mut svc = SocketService::new();
    let mut stack = routed_stack();
    let who = net_caller(1);
    let st = open_socket(&mut svc, &mut stack, &who, SocketType::Stream).expect("open");
    serve_req(
        &mut svc,
        &mut stack,
        &who,
        &SocketRequest::Bind {
            socket: st,
            local: v4_addr(0, 0, 0, 0, 8080),
        },
    )
    .expect("bind");
    serve_req(
        &mut svc,
        &mut stack,
        &who,
        &SocketRequest::Listen { socket: st },
    )
    .expect("listen");
    // Nothing has connected: accept is the retryable WouldBlock, not an error.
    assert_eq!(
        serve_req(
            &mut svc,
            &mut stack,
            &who,
            &SocketRequest::Accept {
                socket: st,
                deliver_port: 0x6000,
            },
        ),
        Err(Errno::WouldBlock),
    );
    // Accept on a non-listening socket is refused.
    let dg = open_socket(&mut svc, &mut stack, &who, SocketType::Datagram).expect("open dg");
    assert_eq!(
        serve_req(
            &mut svc,
            &mut stack,
            &who,
            &SocketRequest::Accept {
                socket: dg,
                deliver_port: 0x6000,
            },
        ),
        Err(Errno::OutOfRange),
    );
}

// --- NetChannelClient ↔ NetChannelServer round-trip -----------------
//
// The cross-process `netchan-v1` control plane, proven in-process: the
// real `NetChannelClient` drives the real driver-side `NetChannelServer`
// over a loopback transport. In production the client's region and the
// driver's mapping alias one shm region; the host mock keeps a single
// region owned by the server rig and treats it as that shared truth, so a
// frame written on the stack side crosses the ring, through the device,
// and back — exactly what the two-process QEMU vertical exercises over a
// live kernel.

use crate::channel::NetChannelClient;
use alloc::rc::Rc;
use tairix_abi::driver::net_channel::NetChannelRequest;
use tairix_virtio_net::NetChannelServer;

/// A representative notify endpoint id the stack would `port_bind`.
const NOTIFY_ENDPOINT: u64 = 0x4E45_5453_5430_3030;

/// An echo device: every frame the stack queues on TX is handed straight
/// back on RX, so one service call round-trips a raw frame through the
/// driver-side rings.
struct EchoNet;

impl Net for EchoNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(facts(MAC_B))
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        let mut frame = vec![0u8; GEOMETRY.slot_capacity() as usize];
        loop {
            match rings.tx.pop(&mut frame) {
                Ok(Some(len)) => {
                    report.transmitted += 1;
                    match rings.rx.push(&frame[..len]) {
                        Ok(()) => report.received += 1,
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

/// The driver-side rig: the server plus its mapping of the shared frame
/// region (the single source of truth the mock keeps coherent).
struct ChannelRig {
    server: NetChannelServer<EchoNet>,
    region: Vec<u8>,
}

/// A loopback channel transport: each `ipc_call` decodes the request and
/// runs one pass of the real `NetChannelServer` over the shared region,
/// exactly as the driver process would on a doorbell.
struct ChannelLoopback {
    rig: Rc<RefCell<ChannelRig>>,
}

impl crate::channel::NetChannelTransport for ChannelLoopback {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        let req = NetChannelRequest::decode(request)?;
        let mut rig = self.rig.borrow_mut();
        match req {
            NetChannelRequest::Facts => copy_reply(reply, &rig.server.facts_reply()),
            NetChannelRequest::Attach(params) => copy_reply(reply, &rig.server.attach(params)),
            NetChannelRequest::Service => {
                let ChannelRig { server, region } = &mut *rig;
                copy_reply(reply, &server.service_reply(region))
            }
            NetChannelRequest::Detach => copy_reply(reply, &rig.server.detach()),
        }
    }
}

/// Copy an encoded reply into the caller's buffer and return its length,
/// failing closed if it cannot hold it — mirrors the kernel's `ipc_call`
/// length check.
fn copy_reply(reply: &mut [u8], out: &[u8]) -> Result<usize, Errno> {
    if reply.len() < out.len() {
        return Err(Errno::BufferTooSmall);
    }
    reply[..out.len()].copy_from_slice(out);
    Ok(out.len())
}

#[test]
fn net_channel_client_round_trips_a_frame_through_the_server() {
    // The stack sizes the geometry from the facts; the echo device's MTU
    // is the standard 1500 so `GEOMETRY` (1514-byte slots) fits.
    let rig = Rc::new(RefCell::new(ChannelRig {
        server: NetChannelServer::new(EchoNet),
        region: rings_region(),
    }));
    let mut transport = ChannelLoopback {
        rig: Rc::clone(&rig),
    };

    // Facts, then attach the region (the stack would `shm_grant` it; the
    // grant handle is opaque to the mock).
    let facts = NetChannelClient::query_facts(&mut transport).expect("facts");
    assert_eq!(facts.mtu, 1500);
    let mut view = rings_region();
    let mut client = NetChannelClient::attach(
        transport,
        &mut view,
        GEOMETRY,
        BufferClass::NonSensitive,
        0xC0FE,
        NOTIFY_ENDPOINT,
    )
    .expect("attach");

    // A frame written on the stack side (its mapping = the shared region)
    // crosses to the device and echoes back.
    {
        let mut rig = rig.borrow_mut();
        let mut rings =
            FrameRings::bind(&mut rig.region, GEOMETRY, BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0xEE; 128]).expect("queue tx");
    }
    let report = client.service().expect("doorbell");
    assert_eq!(report.transmitted, 1);
    assert_eq!(report.received, 1);
    {
        let mut rig = rig.borrow_mut();
        let mut rings =
            FrameRings::bind(&mut rig.region, GEOMETRY, BufferClass::NonSensitive).expect("bind");
        let mut out = vec![0u8; GEOMETRY.slot_capacity() as usize];
        assert_eq!(rings.rx.pop(&mut out), Ok(Some(128)));
        assert_eq!(&out[..128], &[0xEE; 128]);
    }

    client.detach().expect("detach");
    assert!(!rig.borrow().server.is_attached());
}

#[test]
fn net_channel_client_attach_rejects_a_short_region() {
    let rig = Rc::new(RefCell::new(ChannelRig {
        server: NetChannelServer::new(EchoNet),
        region: rings_region(),
    }));
    let transport = ChannelLoopback {
        rig: Rc::clone(&rig),
    };
    // A stack view a byte short of the agreed geometry is refused before
    // any request is sent.
    let mut short = vec![0u8; GEOMETRY.region_len() - 1];
    assert_eq!(
        NetChannelClient::attach(
            transport,
            &mut short,
            GEOMETRY,
            BufferClass::NonSensitive,
            0xC0FE,
            NOTIFY_ENDPOINT,
        )
        .err(),
        Some(Errno::BufferTooSmall)
    );
    assert!(!rig.borrow().server.is_attached());
}

// --- TCP stream sockets end to end (N5c Layer 3) -----------------------
//
// A client `SocketService` + `Netstack` (one interface `wan`, V4_A) opens a
// stream socket and connects to a peer at V4_B:80. The peer is a
// `PeerTcpNet` echo server: a peer `Stack` whose TCP segments drive a
// passive `Tcb` that echoes every received byte. Frames flow through the
// real `service_interface` pump and the real `on_tcp_segment` /
// `advance_streams` paths, so the test exercises exactly the code the live
// `Run` binary drives.

use crate::iface::FrameBatch;
use tairix_net::checksum::Pseudo;
use tairix_net::tcp::conn::{Tcb, TcpConfig};
use tairix_net::tcp::TcpSegment;

/// The client's async delivery port for stream events (the tests decode
/// the delivered frames directly rather than doing IPC).
const DELIVER_PORT: u64 = 0x9999;
/// The peer's listening port.
const PEER_PORT: u16 = 80;

/// A loopback [`Net`] whose "device" is a peer host running a passive TCP
/// server that echoes every byte it receives — the far end of the client's
/// connection.
struct PeerTcpNet {
    stack: Stack,
    tcb: Tcb,
    now: Duration64,
    scratch: Vec<u8>,
}

impl PeerTcpNet {
    fn new(now: Duration64) -> Self {
        let mut stack =
            Stack::new(&StackConfig::new(facts(MAC_B), IID_B, 0x4242), now).expect("peer stack");
        stack.set_ipv4_config(V4_B, 24, None).expect("peer v4");
        Self {
            stack,
            // Passive open; the listener learns the client's port from the SYN.
            tcb: Tcb::listen(TcpConfig::default(), PEER_PORT, 0, 0x5000),
            now,
            scratch: vec![0u8; 2048],
        }
    }

    /// Drain the server connection's outbound segments through the peer
    /// stack (folding the checksum toward the client), pushing the frames
    /// onto the receive ring.
    fn drive_egress(&mut self, rings: &mut FrameRings<'_>, report: &mut ServiceReport) {
        let Self {
            stack, tcb, now, ..
        } = self;
        let dest = IpAddr::V4(V4_A);
        tcb.poll_transmit(*now, |seg| {
            if let Ok(out) = stack.send_tcp(dest, &seg.meta, seg.payload, *now) {
                for frame in &out.frames {
                    if rings.rx.push(frame).is_ok() {
                        report.received += 1;
                    }
                }
            }
            true
        });
    }
}

impl Net for PeerTcpNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(facts(MAC_B))
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        let out = self.stack.advance(self.now);
        for frame in &out.frames {
            if rings.rx.push(frame).is_ok() {
                report.received += 1;
            }
        }
        self.drive_egress(rings, &mut report);
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
                    for event in &out.events {
                        if let StackEvent::TcpSegment {
                            source,
                            destination,
                            segment,
                        } = event
                        {
                            if let (IpAddr::V4(s), IpAddr::V4(d)) = (source, destination) {
                                let pseudo = Pseudo::V4 {
                                    source: *s,
                                    destination: *d,
                                };
                                if let Some(seg) = TcpSegment::parse(pseudo, segment) {
                                    self.tcb.on_segment(&seg, self.now);
                                }
                            }
                        }
                    }
                    // Echo every received byte back to the client.
                    let mut buf = [0u8; 2048];
                    loop {
                        let n = self.tcb.recv(&mut buf);
                        if n == 0 {
                            break;
                        }
                        let _ = self.tcb.send(&buf[..n]);
                    }
                }
                Ok(None) => break,
                Err(_) => return Err(DriverError::BadMagic),
            }
        }
        self.drive_egress(rings, &mut report);
        Ok(report)
    }
}

fn local_service_tcp(net: PeerTcpNet, region: &mut [u8]) -> LocalFrameService<'_, PeerTcpNet> {
    LocalFrameService::new(net, region, GEOMETRY, BufferClass::NonSensitive).expect("frame service")
}

fn sockaddr_v4(addr: Ipv4Addr, port: u16) -> SocketAddr {
    SocketAddr {
        family: NetAddrFamily::V4,
        addr: v4_bytes(addr),
        port,
    }
}

/// Serve one socket request against the client service, returning the
/// `SocketReply` (its `len` bytes written into `response`).
fn svc_serve(
    svc: &mut SocketService,
    ns: &mut Netstack,
    who: &Caller,
    entropy: &mut dyn FnMut() -> u32,
    request: SocketRequest<'_>,
    response: &mut [u8],
    now: Duration64,
) -> Result<crate::SocketReply, Errno> {
    let mut buf = vec![0u8; SocketRequest::MAX_WIRE_LEN];
    let n = request.encode(&mut buf).expect("encode request");
    let sink = RecordingSink::new();
    svc.serve(ns, who, &sink, entropy, &buf[..n], response, now)
}

/// Stage a frame batch onto the single interface's TX ring.
fn stage_batch(fs: &mut LocalFrameService<'_, PeerTcpNet>, batch: &FrameBatch) {
    for (_iface, frames) in batch {
        push_tx(fs, frames);
    }
}

/// Pump the client interface for a bounded number of rounds at `now`,
/// routing every engine event through the socket service (stream segments
/// and datagrams alike) and staging the resulting egress frames, returning
/// the deliveries. A doorbell may leave a SYN-ACK on the RX ring the same
/// `service_interface` did not re-harvest, so it iterates a fixed count
/// rather than stopping on a quiet round; at a fixed `now` the extra rounds
/// are idle.
fn pump_client(
    ns: &mut Netstack,
    svc: &mut SocketService,
    fs: &mut LocalFrameService<'_, PeerTcpNet>,
    now: Duration64,
) -> Vec<Delivery> {
    let secret = crate::CryptoCookieSecret::new([0x5A; 32]);
    let mut deliveries = Vec::new();
    for _ in 0..64 {
        let events = ns.service_interface(name("wan"), fs, now).expect("pump");
        let io = svc.advance_streams(ns, now);
        stage_batch(fs, &io.tx);
        deliveries.extend(io.deliveries);
        for event in &events {
            match event {
                StackEvent::UdpDatagram { .. } => deliveries.extend(svc.deliver(event)),
                StackEvent::TcpSegment {
                    source,
                    destination,
                    segment,
                } => {
                    let io = svc.on_tcp_segment(ns, *source, *destination, segment, now, &secret);
                    stage_batch(fs, &io.tx);
                    deliveries.extend(io.deliveries);
                }
                _ => {}
            }
        }
    }
    deliveries
}

#[test]
fn stream_connect_and_echo_over_the_pump() {
    let mut ns = managed_stack();
    ns.addr_add(name("wan"), NetAddrFamily::V4, 24, v4_bytes(V4_A), t(1))
        .expect("addr add");
    let mut region = rings_region();
    let mut fs = local_service_tcp(PeerTcpNet::new(t(2)), &mut region);
    let mut svc = SocketService::new();
    let who = caller(&[CapabilityId::NET]);
    let now = t(2);
    let mut entropy = || 0x1357_9BDFu32;
    let mut response = [0u8; 64];

    // Open a stream socket.
    let reply = svc_serve(
        &mut svc,
        &mut ns,
        &who,
        &mut entropy,
        SocketRequest::Socket {
            family: NetAddrFamily::V4,
            sock_type: SocketType::Stream,
            deliver_port: DELIVER_PORT,
        },
        &mut response,
        now,
    )
    .expect("open");
    let sid = decode_socket_reply(&response[..reply.len]).expect("socket id");

    // Connect: the SYN is emitted (parking on ARP); pump to established.
    let reply = svc_serve(
        &mut svc,
        &mut ns,
        &who,
        &mut entropy,
        SocketRequest::Connect {
            socket: sid,
            peer: sockaddr_v4(V4_B, PEER_PORT),
        },
        &mut response,
        now,
    )
    .expect("connect");
    decode_status_reply(&response[..reply.len]).expect("connect ok");
    stage_batch(&mut fs, &reply.tx);
    let deliveries = pump_client(&mut ns, &mut svc, &mut fs, now);
    assert!(
        deliveries.iter().any(|d| matches!(
            SocketStreamEvent::parse(&d.datagram),
            Ok(SocketStreamEvent::Connected { socket }) if socket == sid
        )),
        "client observed the connection establish"
    );

    // Send a payload; the echo server returns it.
    let reply = svc_serve(
        &mut svc,
        &mut ns,
        &who,
        &mut entropy,
        SocketRequest::Send {
            socket: sid,
            dest: None,
            payload: b"hello over tcp",
        },
        &mut response,
        now,
    )
    .expect("send");
    let accepted = decode_send_reply(&response[..reply.len]).expect("send accepted");
    assert_eq!(accepted, 14, "the whole payload was accepted");
    stage_batch(&mut fs, &reply.tx);
    let deliveries = pump_client(&mut ns, &mut svc, &mut fs, now);

    let mut echoed: Vec<u8> = Vec::new();
    for d in &deliveries {
        if let Ok(SocketStreamEvent::Data { socket, payload }) =
            SocketStreamEvent::parse(&d.datagram)
        {
            if socket == sid {
                echoed.extend_from_slice(payload);
            }
        }
    }
    assert_eq!(
        echoed, b"hello over tcp",
        "the peer echoed the stream bytes"
    );
}
