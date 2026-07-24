//! Unit and end-to-end tests for the host engine: two [`Stack`]
//! instances wired back-to-back over an in-memory link, plus a
//! hand-rolled router for the RA/SLAAC path.

use super::*;
use crate::ipv6::HBH_ROUTER_ALERT_LEN;
use alloc::collections::VecDeque;

const MAC_A: MacAddress = MacAddress([0x02, 0xAA, 0, 0, 0, 0x01]);
const MAC_B: MacAddress = MacAddress([0x02, 0xBB, 0, 0, 0, 0x02]);
const ROUTER_MAC: MacAddress = MacAddress([0x02, 0xCC, 0, 0, 0, 0x03]);
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
        offloads: tairix_abi::driver::net::NetOffloads::empty(),
        rx_queues: 1,
    }
}

fn stack(mac: MacAddress, iid: [u8; 8]) -> Stack {
    Stack::new(&StackConfig::new(facts(mac), iid, 0x1234), t(0)).expect("valid facts")
}

fn link_local(iid: [u8; 8]) -> Ipv6Addr {
    let mut octets = [0u8; 16];
    octets[0] = 0xFE;
    octets[1] = 0x80;
    octets[8..].copy_from_slice(&iid);
    Ipv6Addr::from(octets)
}

/// The emitted frames' bytes, dropping the transmit-offload metadata a
/// live device would consume — most tests pump raw bytes back through
/// `on_frame` and inspect them, so this keeps the harness on `Vec<u8>`.
/// Takes the `frames` field (not the whole [`StackOutput`]) so a caller
/// may still read `events` from the same output.
fn tx_bytes(frames: Vec<TxFrame>) -> Vec<Vec<u8>> {
    frames.into_iter().map(|f| f.bytes).collect()
}

/// Run both stacks' timers forward to `now` and exchange every frame
/// until the link is quiet, collecting each side's events.
fn pump(
    a: &mut Stack,
    b: &mut Stack,
    now: Duration64,
    events_a: &mut Vec<StackEvent>,
    events_b: &mut Vec<StackEvent>,
) {
    let mut to_b: VecDeque<TxFrame> = VecDeque::new();
    let mut to_a: VecDeque<TxFrame> = VecDeque::new();
    let out_a = a.advance_collect(now);
    events_a.extend(out_a.events);
    to_b.extend(out_a.frames);
    let out_b = b.advance_collect(now);
    events_b.extend(out_b.events);
    to_a.extend(out_b.frames);
    // Bounded exchange: control-plane conversations settle quickly.
    for _ in 0..64 {
        if to_a.is_empty() && to_b.is_empty() {
            break;
        }
        if let Some(frame) = to_b.pop_front() {
            let out = b.on_frame_collect(&frame.bytes, now);
            events_b.extend(out.events);
            to_a.extend(out.frames);
        }
        if let Some(frame) = to_a.pop_front() {
            let out = a.on_frame_collect(&frame.bytes, now);
            events_a.extend(out.events);
            to_b.extend(out.frames);
        }
    }
    assert!(
        to_a.is_empty() && to_b.is_empty(),
        "conversation did not settle"
    );
}

/// Bring both stacks through DAD (t0: DAD NS, t1: preferred + RS).
fn bring_up(a: &mut Stack, b: &mut Stack) -> (Vec<StackEvent>, Vec<StackEvent>) {
    let mut events_a = Vec::new();
    let mut events_b = Vec::new();
    pump(a, b, t(0), &mut events_a, &mut events_b);
    pump(a, b, t(1), &mut events_a, &mut events_b);
    assert!(events_a.contains(&StackEvent::AddressPreferred {
        addr: link_local(IID_A)
    }));
    assert!(events_b.contains(&StackEvent::AddressPreferred {
        addr: link_local(IID_B)
    }));
    (events_a, events_b)
}

#[test]
fn construction_refuses_bad_device_facts() {
    let mut bad = facts(MAC_A);
    bad.mtu = 0;
    assert_eq!(
        Stack::new(&StackConfig::new(bad, IID_A, 0), t(0)).err(),
        Some(StackError::BadDeviceFacts)
    );
}

#[test]
fn ipv4_ping_resolves_arp_and_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    let mut events_a = Vec::new();
    let mut events_b = Vec::new();

    // The echo request parks on ARP resolution and the request goes
    // out after the exchange settles.
    let out = a
        .send_echo_request_collect(IpAddr::V4(V4_B), 0x77, 1, b"tairix-stack", t(2))
        .expect("send");
    assert!(out.events.is_empty());
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(2));
            events_a.extend(out_a.events);
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert_eq!(
        events_a,
        [StackEvent::EchoReply {
            source: IpAddr::V4(V4_B),
            identifier: 0x77,
            sequence: 1,
            payload: b"tairix-stack".to_vec(),
        }]
    );
    // The responder observed the inbound request it answered.
    assert!(events_b.contains(&StackEvent::EchoRequestServed {
        source: IpAddr::V4(V4_A),
        identifier: 0x77,
        sequence: 1,
    }));
    // Both ends resolved each other.
    assert!(matches!(
        a.neighbors.entry(IpAddr::V4(V4_B)),
        Some((_, Some(mac))) if mac == MAC_B
    ));
    assert!(matches!(
        b.neighbors.entry(IpAddr::V4(V4_A)),
        Some((_, Some(mac))) if mac == MAC_A
    ));
}

#[test]
fn ipv6_link_local_ping_resolves_nd_and_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    let (mut events_a, mut events_b) = bring_up(&mut a, &mut b);
    events_a.clear();
    events_b.clear();

    let out = a
        .send_echo_request_collect(IpAddr::V6(link_local(IID_B)), 0x42, 7, b"ping6", t(3))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(3));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(3));
            events_a.extend(out_a.events);
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert_eq!(
        events_a,
        [StackEvent::EchoReply {
            source: IpAddr::V6(link_local(IID_B)),
            identifier: 0x42,
            sequence: 7,
            payload: b"ping6".to_vec(),
        }]
    );
    // The responder observed the inbound request it answered.
    assert!(events_b.contains(&StackEvent::EchoRequestServed {
        source: IpAddr::V6(link_local(IID_A)),
        identifier: 0x42,
        sequence: 7,
    }));
    assert!(matches!(
        a.neighbors.entry(IpAddr::V6(link_local(IID_B))),
        Some((_, Some(mac))) if mac == MAC_B
    ));
    assert!(matches!(
        b.neighbors.entry(IpAddr::V6(link_local(IID_A))),
        Some((_, Some(mac))) if mac == MAC_A
    ));
}

// --- Hand-rolled router frames (RA emission is router behaviour the
// --- host codec deliberately does not provide).

const ROUTER_LL: Ipv6Addr = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 0xCC);

fn slaac_prefix() -> Ipv6Addr {
    Ipv6Addr::new(0x2001, 0x0DB8, 0, 0, 0, 0, 0, 0)
}

/// Build a Router Advertisement frame from the fake router: one
/// on-link + autonomous /64 prefix, source link-layer option.
fn router_advertisement_frame(dest: Ipv6Addr, dest_mac: MacAddress) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(64); // cur hop limit
    body.push(0); // flags
    body.extend_from_slice(&1800u16.to_be_bytes()); // router lifetime
    body.extend_from_slice(&0u32.to_be_bytes()); // reachable
    body.extend_from_slice(&0u32.to_be_bytes()); // retrans
                                                 // Source link-layer option.
    body.extend_from_slice(&[1, 1]);
    body.extend_from_slice(ROUTER_MAC.as_octets());
    // Prefix information option (type 3, len 4).
    body.push(3);
    body.push(4);
    body.push(64); // prefix length
    body.push(0xC0); // on-link | autonomous
    body.extend_from_slice(&3600u32.to_be_bytes()); // valid
    body.extend_from_slice(&1800u32.to_be_bytes()); // preferred
    body.extend_from_slice(&0u32.to_be_bytes()); // reserved
    body.extend_from_slice(&slaac_prefix().octets());
    icmpv6_frame_from_router(dest, dest_mac, crate::nd::TYPE_ROUTER_ADVERTISEMENT, &body)
}

/// Wrap an `ICMPv6` body in IPv6 + Ethernet from the fake router.
fn icmpv6_frame_from_router(
    dest: Ipv6Addr,
    dest_mac: MacAddress,
    message_type: u8,
    body: &[u8],
) -> Vec<u8> {
    let message = IcmpMessage {
        message_type,
        code: 0,
        body,
    };
    let context = IcmpContext::V6 {
        source: ROUTER_LL,
        destination: dest,
    };
    let mut icmp = vec![0u8; crate::icmp::ICMP_FIXED_HEADER_LEN + body.len()];
    message.write(context, &mut icmp).expect("icmp fits");
    let mut header = Ipv6Header::new(ROUTER_LL, dest, NEXT_HEADER_ICMPV6);
    header.hop_limit = ND_HOP_LIMIT;
    let packet = ipv6_packet(&header, &icmp).expect("packet fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, dest_mac, ROUTER_MAC, ETHERTYPE_IPV6).expect("header fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

#[test]
fn router_advertisement_configures_slaac_and_default_route() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);

    let ra = router_advertisement_frame(ALL_NODES, ipv6_multicast_mac(&ALL_NODES));
    let out = a.on_frame_collect(&ra, t(2));
    assert!(out.frames.is_empty());
    // The SLAAC address forms and completes DAD.
    let mut expected = slaac_prefix().octets();
    expected[8..].copy_from_slice(&IID_A);
    let slaac_addr = Ipv6Addr::from(expected);
    assert!(a.iface().is_tentative(slaac_addr));
    a.advance_collect(t(2)); // DAD transmit
    let out = a.advance_collect(t(3)); // DAD completion
    assert!(out
        .events
        .contains(&StackEvent::AddressPreferred { addr: slaac_addr }));

    // An off-link destination now routes via the advertised router.
    let off_link = Ipv6Addr::new(0x2001, 0x0DB8, 0xFF, 0, 0, 0, 0, 1);
    // (2001:db8:ff::1 is outside the /64, so it uses the default
    // router learned from the RA.)
    let out = a
        .send_echo_request_collect(IpAddr::V6(off_link), 1, 1, b"x", t(3))
        .expect("routed via default router");
    // The parked echo triggers a unicast NS to the router (its MAC is
    // already learned from the RA's source option, entry Stale).
    assert!(!out.frames.is_empty());
}

#[test]
fn hostile_ra_from_non_link_local_source_is_ignored() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    let mut frame = router_advertisement_frame(ALL_NODES, ipv6_multicast_mac(&ALL_NODES));
    // Rewrite the source to a global address (and refresh the
    // checksum by rebuilding is overkill: the checksum covers the
    // pseudo-header, so this corruption must fail the parse or the
    // source rule — either way, no SLAAC address may form).
    frame[ETHERNET_HEADER_LEN + 8] = 0x20;
    a.on_frame_collect(&frame, t(2));
    a.advance_collect(t(2));
    a.advance_collect(t(3));
    let mut expected = slaac_prefix().octets();
    expected[8..].copy_from_slice(&IID_A);
    assert!(!a.iface().is_assigned(Ipv6Addr::from(expected)));
}

#[test]
fn dad_probe_from_another_node_fails_our_tentative_address() {
    let mut a = stack(MAC_A, IID_A);
    a.advance_collect(t(0)); // link-local DAD NS out (still tentative)
    let target = link_local(IID_A);
    // Another node's DAD probe for the same address: NS from the
    // unspecified source to the solicited-node group.
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&target.octets());
    let group = solicited_node_multicast(&target);
    let message = IcmpMessage {
        message_type: crate::nd::TYPE_NEIGHBOR_SOLICITATION,
        code: 0,
        body: &body,
    };
    let context = IcmpContext::V6 {
        source: Ipv6Addr::from([0u8; 16]),
        destination: group,
    };
    let mut icmp = vec![0u8; crate::icmp::ICMP_FIXED_HEADER_LEN + body.len()];
    message.write(context, &mut icmp).expect("fits");
    let mut header = Ipv6Header::new(Ipv6Addr::from([0u8; 16]), group, NEXT_HEADER_ICMPV6);
    header.hop_limit = ND_HOP_LIMIT;
    let packet = ipv6_packet(&header, &icmp).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv6_multicast_mac(&group),
        MAC_B,
        ETHERTYPE_IPV6,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);

    let out = a.on_frame_collect(&frame, t(0));
    assert_eq!(out.events, [StackEvent::DadFailed { addr: target }]);
    assert!(a.iface().v6_disabled());
}

#[test]
fn multicast_echo_request_is_refused() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    // An echo request to all-nodes must not be answered.
    let echo = IcmpEcho {
        kind: crate::icmp::EchoKind::Request,
        identifier: 1,
        sequence: 1,
        payload: b"amplify?",
    };
    let context = IcmpContext::V6 {
        source: link_local(IID_B),
        destination: ALL_NODES,
    };
    let mut message = vec![0u8; echo.wire_len()];
    echo.write(context, &mut message).expect("fits");
    let mut header = Ipv6Header::new(link_local(IID_B), ALL_NODES, NEXT_HEADER_ICMPV6);
    header.hop_limit = 64;
    let packet = ipv6_packet(&header, &message).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv6_multicast_mac(&ALL_NODES),
        MAC_B,
        ETHERTYPE_IPV6,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let out = a.on_frame_collect(&frame, t(2));
    assert!(out.frames.is_empty());
    assert!(out.events.is_empty());
}

#[test]
fn unknown_ipv4_protocol_gets_rate_limited_protocol_unreachable() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    // Prime A's neighbour cache: B's ARP request teaches A the
    // sender's binding, so the errors below transmit immediately.
    let request = ArpPacket {
        operation: OP_REQUEST,
        sender_hardware: MAC_B,
        sender_protocol: V4_B,
        target_hardware: MacAddress([0; 6]),
        target_protocol: V4_A,
    };
    let mut arp = [0u8; crate::arp::ARP_PACKET_LEN];
    request.write(&mut arp).expect("fits");
    let mut arp_frame = vec![0u8; ETHERNET_HEADER_LEN + arp.len()];
    write_header(&mut arp_frame, BROADCAST, MAC_B, ETHERTYPE_ARP).expect("fits");
    arp_frame[ETHERNET_HEADER_LEN..].copy_from_slice(&arp);
    let out = a.on_frame_collect(&arp_frame, t(1));
    assert_eq!(out.frames.len(), 1, "A answers the ARP request");
    // Craft a datagram with an unknown transport protocol.
    let header = Ipv4Header::new(V4_B, V4_A, 253);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + 4];
    header.write(&mut packet, 4).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);

    let mut sent = 0;
    for _ in 0..40 {
        let out = a.on_frame_collect(&frame, t(2));
        sent += out.frames.len();
    }
    let counters = a.counters();
    assert!(counters.icmp_errors_sent > 0, "some errors emitted");
    assert!(
        counters.icmp_errors_suppressed > 0,
        "the token bucket capped the rest"
    );
    // Every emitted error left as exactly one frame (the neighbour
    // was primed above).
    assert_eq!(
        u64::try_from(sent).expect("fits"),
        counters.icmp_errors_sent
    );
}

#[test]
fn unknown_ipv6_upper_protocol_reports_parameter_problem_pointer() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    // B sends A a datagram with an unknown upper protocol; A's report
    // must point at byte 6 (the fixed header's next-header field).
    // The report may park behind an NS/NA exchange, so run the
    // conversation to quiescence and inspect B's events.
    let header = Ipv6Header::new(link_local(IID_B), link_local(IID_A), 200);
    let packet = ipv6_packet(&header, &[0xAB; 4]).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV6).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let mut events_b = Vec::new();
    let mut to_b: VecDeque<Vec<u8>> = tx_bytes(a.on_frame_collect(&frame, t(2)).frames).into();
    for _ in 0..16 {
        let Some(frame) = to_b.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            to_b.extend(tx_bytes(a.on_frame_collect(&reply, t(2)).frames));
        }
    }
    assert_eq!(
        events_b,
        [StackEvent::IcmpErrorReceived {
            source: IpAddr::V6(link_local(IID_A)),
            kind: IcmpErrorKind::ParameterProblem {
                code: PARAM_PROBLEM_NEXT_HEADER,
                pointer: 6,
            },
        }]
    );
}

#[test]
fn send_refusals_are_typed() {
    let mut a = stack(MAC_A, IID_A);
    // No v4 configuration.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Off-subnet with no gateway.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 1, 1, b"x", t(0)),
        Err(SendError::NoRoute)
    );
    // Non-unicast destinations.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(Ipv4Addr::BROADCAST), 1, 1, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V6(ALL_NODES), 1, 1, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    // No usable v6 source before DAD completes.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V6(link_local(IID_B)), 1, 1, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    // Link down refuses everything.
    a.set_link(LinkState::Down);
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(0)),
        Err(SendError::LinkDown)
    );
}

#[test]
fn oversize_v6_echo_is_refused_too_large() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    let payload = vec![0u8; 1600];
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V6(link_local(IID_B)), 1, 1, &payload, t(2)),
        Err(SendError::TooLarge)
    );
}

#[test]
fn unreachable_neighbor_drops_parked_packets_with_event() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    let out = a
        .send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(0))
        .expect("parked");
    assert_eq!(out.frames.len(), 1, "first ARP request");
    // Nothing answers: three multicast solicitations, then failure.
    let mut events = Vec::new();
    for secs in 1..8 {
        events.extend(a.advance_collect(t(secs)).events);
    }
    assert!(events.contains(&StackEvent::NeighborUnreachable {
        ip: IpAddr::V4(V4_B)
    }));
    assert!(a.counters().pending_dropped > 0);
}

#[test]
fn next_deadline_spans_all_components() {
    let a = stack(MAC_A, IID_A);
    // Bring-up work (link-local DAD) is due immediately.
    assert_eq!(a.next_deadline(), Some(t(0)));
}

#[test]
fn ipv4_udp_datagram_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    let mut events_b = Vec::new();

    // The datagram parks on ARP resolution; it flows once the exchange
    // settles, exactly like the echo path.
    let out = a
        .send_datagram_collect(IpAddr::V4(V4_B), 5000, 7, b"udp-payload", t(2))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(2));
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert!(events_b.contains(&StackEvent::UdpDatagram {
        source: IpAddr::V4(V4_A),
        destination: IpAddr::V4(V4_B),
        source_port: 5000,
        destination_port: 7,
        payload: b"udp-payload".to_vec(),
    }));
}

#[test]
fn ipv6_udp_datagram_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    let mut events_b = Vec::new();

    let out = a
        .send_datagram_collect(IpAddr::V6(link_local(IID_B)), 6000, 9, b"udp6", t(3))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(3));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(3));
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert!(events_b.contains(&StackEvent::UdpDatagram {
        source: IpAddr::V6(link_local(IID_A)),
        destination: IpAddr::V6(link_local(IID_B)),
        source_port: 6000,
        destination_port: 9,
        payload: b"udp6".to_vec(),
    }));
}

#[test]
fn send_datagram_refusals_are_typed() {
    let mut a = stack(MAC_A, IID_A);
    // No v4 configuration yet: no usable source address.
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(V4_B), 1, 2, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // The limited broadcast and the unspecified address are refused:
    // neither is a meaningful datagram destination; fail closed.
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(Ipv4Addr::BROADCAST), 1, 2, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1, 2, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    // Off-subnet unicast with no gateway.
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 1, 2, b"x", t(0)),
        Err(SendError::NoRoute)
    );
    // Link down refuses everything.
    a.set_link(LinkState::Down);
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(V4_B), 1, 2, b"x", t(0)),
        Err(SendError::LinkDown)
    );
}

#[test]
fn oversize_v6_udp_is_refused_too_large() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    let payload = vec![0u8; 1600];
    assert_eq!(
        a.send_datagram_collect(IpAddr::V6(link_local(IID_B)), 1, 2, &payload, t(2)),
        Err(SendError::TooLarge)
    );
}

#[test]
fn corrupt_udp_checksum_is_dropped() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Hand-build a UDP-over-IPv4 datagram to A, then corrupt its payload
    // so the checksum no longer verifies: it must be dropped, not surfaced.
    let mut udp_msg = vec![0u8; udp::UDP_HEADER_LEN + 4];
    udp::write(
        udp::Pseudo::V4 {
            source: V4_B,
            destination: V4_A,
        },
        1111,
        2222,
        &[1, 2, 3, 4],
        &mut udp_msg,
    )
    .expect("write");
    udp_msg[udp::UDP_HEADER_LEN] ^= 0xFF;
    let header = Ipv4Header::new(V4_B, V4_A, PROTOCOL_UDP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + udp_msg.len()];
    header.write(&mut packet, udp_msg.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udp_msg);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_B, MAC_A, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let out = a.on_frame_collect(&frame, t(0));
    assert!(out.events.is_empty());
    assert!(a.counters().rx_dropped >= 1);
}

// ---- Receive-checksum offload (RX_CSUM_VALIDATED) --------------------

/// A stack whose device advertised (and it opted into) receive-checksum
/// validation.
fn stack_with_rx_csum(mac: MacAddress, iid: [u8; 8]) -> Stack {
    let mut f = facts(mac);
    f.offloads = tairix_abi::driver::net::NetOffloads::RX_CSUM_VALIDATED;
    Stack::new(&StackConfig::new(f, iid, 0x1234), t(0)).expect("valid facts")
}

/// A UDP/IPv4 datagram from `V4_B` to `V4_A` (Ethernet destination
/// `MAC_A`). When `corrupt`, the first payload byte is flipped *after*
/// the checksum is computed, so the on-wire checksum no longer verifies.
fn v4_udp_frame_to_a(payload: &[u8], corrupt: bool) -> Vec<u8> {
    let mut udp_msg = vec![0u8; udp::UDP_HEADER_LEN + payload.len()];
    udp::write(
        udp::Pseudo::V4 {
            source: V4_B,
            destination: V4_A,
        },
        1111,
        2222,
        payload,
        &mut udp_msg,
    )
    .expect("write");
    if corrupt {
        udp_msg[udp::UDP_HEADER_LEN] ^= 0xFF;
    }
    let header = Ipv4Header::new(V4_B, V4_A, PROTOCOL_UDP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + udp_msg.len()];
    header.write(&mut packet, udp_msg.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udp_msg);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

#[test]
fn rx_checksum_offload_matches_the_software_path_byte_for_byte() {
    // The offloaded path (device-validated, fold skipped) must produce
    // output identical to the canonical software path — the oracle.
    let mut soft = stack(MAC_A, IID_A);
    let mut off = stack_with_rx_csum(MAC_A, IID_A);
    soft.set_ipv4_config(V4_A, 24, None)
        .expect("configure soft");
    off.set_ipv4_config(V4_A, 24, None).expect("configure off");
    let frame = v4_udp_frame_to_a(b"conformance", false);
    let soft_out = soft.on_frame_collect(&frame, t(0));
    let off_out = off.on_frame_meta_collect(&frame, RxMeta::validated(), t(0));
    assert_eq!(soft_out, off_out);
    assert!(matches!(
        soft_out.events.first(),
        Some(StackEvent::UdpDatagram { .. })
    ));
}

#[test]
fn rx_validated_offload_skips_the_fold_but_still_delivers() {
    // A frame the device validated is accepted even if its on-wire
    // checksum is (now) wrong: the software fold is skipped. Trust is in
    // the device; the offload is what is being exercised.
    let mut off = stack_with_rx_csum(MAC_A, IID_A);
    off.set_ipv4_config(V4_A, 24, None).expect("configure");
    let corrupt = v4_udp_frame_to_a(b"trust-the-nic", true);
    let out = off.on_frame_meta_collect(&corrupt, RxMeta::validated(), t(0));
    assert!(matches!(
        out.events.first(),
        Some(StackEvent::UdpDatagram { .. })
    ));
    // The *same* corrupt frame without the device's assurance is folded
    // in software and dropped — the offload is never assumed.
    let out = off.on_frame_meta_collect(&corrupt, RxMeta::none(), t(0));
    assert!(out.events.is_empty());
}

#[test]
fn rx_validated_claim_is_ignored_when_offload_not_negotiated() {
    // A stack that did not negotiate the offload folds in software even
    // when a frame claims the device validated it: a corrupt frame is
    // dropped. A per-frame claim is honoured only under a negotiated
    // offload.
    let mut soft = stack(MAC_A, IID_A);
    soft.set_ipv4_config(V4_A, 24, None).expect("configure");
    let corrupt = v4_udp_frame_to_a(b"no-offload", true);
    let out = soft.on_frame_meta_collect(&corrupt, RxMeta::validated(), t(0));
    assert!(out.events.is_empty());
}

#[test]
fn frames_for_other_hosts_are_dropped() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Unicast Ethernet destination that is not ours.
    let header = Ipv4Header::new(V4_B, V4_A, PROTOCOL_ICMP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN];
    header.write(&mut packet, 0).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_B, MAC_B, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let out = a.on_frame_collect(&frame, t(0));
    assert!(out.frames.is_empty() && out.events.is_empty());
    assert_eq!(a.counters().rx_dropped, 1);
}

// ---- Multicast membership (IGMPv2 / MLDv2) --------------------------

const GROUP_V4: Ipv4Addr = Ipv4Addr::new(239, 1, 2, 3);

/// A link-local-scope IPv6 multicast group used by the transmit tests.
const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x1234);

/// A UDP datagram from `V4_B` to the IPv4 multicast group `GROUP_V4`,
/// addressed to the group's multicast MAC.
fn v4_group_udp_frame(payload: &[u8]) -> Vec<u8> {
    let mut udp_msg = vec![0u8; udp::UDP_HEADER_LEN + payload.len()];
    udp::write(
        udp::Pseudo::V4 {
            source: V4_B,
            destination: GROUP_V4,
        },
        5000,
        7000,
        payload,
        &mut udp_msg,
    )
    .expect("write");
    let header = Ipv4Header::new(V4_B, GROUP_V4, PROTOCOL_UDP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + udp_msg.len()];
    header.write(&mut packet, udp_msg.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udp_msg);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv4_multicast_mac(&GROUP_V4),
        MAC_B,
        ETHERTYPE_IPV4,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// An IGMP General Query from `V4_B` to the all-systems group.
fn igmp_general_query_frame(max_resp_deciseconds: u8) -> Vec<u8> {
    let all_systems = Ipv4Addr::new(224, 0, 0, 1);
    let message = IgmpMessage::MembershipQuery {
        max_resp_deciseconds,
        group: Ipv4Addr::UNSPECIFIED,
    };
    let mut body = [0u8; crate::igmp::IGMP_MESSAGE_LEN];
    message.write(&mut body).expect("write");
    let mut header = Ipv4Header::new(V4_B, all_systems, PROTOCOL_IGMP);
    header.ttl = 1;
    let mut packet = vec![0u8; IPV4_HEADER_LEN + body.len()];
    header.write(&mut packet, body.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&body);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv4_multicast_mac(&all_systems),
        MAC_B,
        ETHERTYPE_IPV4,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// The first IGMP message in `frames`, with its Ethernet destination,
/// TTL, and IPv4 options length.
fn first_igmp(frames: &[TxFrame]) -> Option<(MacAddress, u8, usize, IgmpMessage)> {
    for f in frames {
        let Some(eth) = EthernetFrame::parse(&f.bytes) else {
            continue;
        };
        if eth.ethertype != ETHERTYPE_IPV4 {
            continue;
        }
        let Some((header, options, payload)) = Ipv4Header::parse(eth.payload) else {
            continue;
        };
        if header.protocol != PROTOCOL_IGMP {
            continue;
        }
        let Some(message) = IgmpMessage::parse(payload) else {
            continue;
        };
        return Some((eth.destination, header.ttl, options.len(), message));
    }
    None
}

#[test]
fn joining_v4_group_emits_igmp_report_with_router_alert() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    assert!(a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join"));
    let out = a.advance_collect(t(0));
    let (dest_mac, ttl, options_len, message) = first_igmp(&out.frames).expect("igmp report");
    assert_eq!(message, IgmpMessage::V2Report { group: GROUP_V4 });
    assert_eq!(ttl, 1, "membership messages never leave the link");
    assert_eq!(options_len, 4, "Router Alert option present (IHL = 6)");
    assert_eq!(dest_mac, ipv4_multicast_mac(&GROUP_V4));
}

#[test]
fn counters_track_bytes_and_count_each_emitted_frame_once() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    // Drive the membership traffic and tally what actually reached the wire.
    let mut emitted_frames = 0u64;
    let mut emitted_bytes = 0u64;
    for s in 0..40 {
        let out = a.advance_collect(t(s));
        emitted_frames += out.frames.len() as u64;
        emitted_bytes += out.frames.iter().map(|f| f.bytes.len() as u64).sum::<u64>();
    }
    assert!(
        emitted_frames > 0,
        "a v4 join emits at least one IGMP report"
    );
    let counters = a.counters();
    // Every emitted frame — IGMP reports included — is counted exactly once.
    // This guards against the IGMP/MLD transmit double-count.
    assert_eq!(counters.tx_frames, emitted_frames);
    assert_eq!(counters.tx_bytes, emitted_bytes);

    // A received frame's whole Ethernet length is counted, dropped or not.
    let frame = v4_group_udp_frame(b"hello");
    let before = a.counters();
    let _ = a.on_frame_collect(&frame, t(40));
    let after = a.counters();
    assert_eq!(after.rx_frames, before.rx_frames + 1);
    assert_eq!(after.rx_bytes, before.rx_bytes + frame.len() as u64);
}

#[test]
fn joined_group_receives_udp_and_others_are_dropped() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Not a member yet: the multicast datagram is dropped.
    let out = a.on_frame_collect(&v4_group_udp_frame(b"hi"), t(0));
    assert!(out.events.is_empty());
    assert!(a.counters().rx_dropped >= 1);
    // After joining, the same datagram is delivered.
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    let out = a.on_frame_collect(&v4_group_udp_frame(b"hi"), t(0));
    assert!(out.events.iter().any(|event| matches!(
        event,
        StackEvent::UdpDatagram { destination: IpAddr::V4(d), payload, .. }
            if *d == GROUP_V4 && payload == b"hi"
    )));
}

#[test]
fn igmp_general_query_triggers_a_delayed_response() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    // Drain the unsolicited join reports.
    for s in 0..40 {
        let _ = a.advance_collect(t(s));
    }
    // A General Query schedules — but does not immediately send — a report.
    let out = a.on_frame_collect(&igmp_general_query_frame(20), t(100));
    assert!(first_igmp(&out.frames).is_none(), "response is delayed");
    assert!(a.next_deadline().is_some());
    // Past the 2-second window the response is emitted.
    let out = a.advance_collect(t(103));
    let (_mac, _ttl, _opts, message) = first_igmp(&out.frames).expect("query response");
    assert_eq!(message, IgmpMessage::V2Report { group: GROUP_V4 });
}

#[test]
fn leaving_v4_group_emits_leave_to_all_routers() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    for s in 0..40 {
        let _ = a.advance_collect(t(s));
    }
    assert!(a.leave_multicast(IpAddr::V4(GROUP_V4), t(40)));
    let out = a.advance_collect(t(40));
    let (dest_mac, _ttl, _opts, message) = first_igmp(&out.frames).expect("leave");
    assert_eq!(message, IgmpMessage::LeaveGroup { group: GROUP_V4 });
    assert_eq!(dest_mac, ipv4_multicast_mac(&Ipv4Addr::new(224, 0, 0, 2)));
}

#[test]
fn joining_a_non_multicast_group_fails_closed() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    assert_eq!(
        a.join_multicast(IpAddr::V4(V4_B), t(0)),
        Err(McastError::NotMulticast)
    );
    assert_eq!(
        a.join_multicast(IpAddr::V6(link_local(IID_B)), t(0)),
        Err(McastError::NotMulticast)
    );
}

#[test]
fn ipv6_address_reports_its_solicited_node_group_via_mld() {
    let mut a = stack(MAC_A, IID_A);
    let _ = a.advance_collect(t(0)); // link-local DAD solicitation
    let out = a.advance_collect(t(1)); // preferred -> join solicited-node -> MLD report
    let frame = out
        .frames
        .iter()
        .find(|f| {
            EthernetFrame::parse(&f.bytes).is_some_and(|eth| {
                eth.ethertype == ETHERTYPE_IPV6
                    && Ipv6Header::parse(eth.payload)
                        .is_some_and(|(h, _)| h.next_header == NEXT_HEADER_HOP_BY_HOP)
            })
        })
        .expect("mld report frame");
    let eth = EthernetFrame::parse(&frame.bytes).expect("eth");
    assert_eq!(eth.destination, ipv6_multicast_mac(&ALL_MLDV2_ROUTERS));
    let (header, payload) = Ipv6Header::parse(eth.payload).expect("ipv6");
    assert_eq!(header.destination, ALL_MLDV2_ROUTERS);
    assert_eq!(header.hop_limit, 1);
    // Hop-by-Hop header: names ICMPv6 and carries the Router Alert option.
    assert_eq!(payload[0], NEXT_HEADER_ICMPV6);
    assert_eq!(payload[2], 5, "Router Alert option type");
    // The ICMPv6 message after the 8-byte Hop-by-Hop header is a report.
    assert_eq!(payload[HBH_ROUTER_ALERT_LEN], TYPE_MLDV2_REPORT);
}

#[test]
fn ipv4_multicast_datagram_transmit_reaches_a_member() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    // The receiver must be a member; the sender need not be, and needs
    // no route to the group.
    b.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    let out = a
        .send_datagram_collect(IpAddr::V4(GROUP_V4), 5000, 7000, b"mcast4", t(0))
        .expect("send");
    assert_eq!(out.frames.len(), 1, "one unfragmented multicast frame");
    let eth = EthernetFrame::parse(&out.frames[0].bytes).expect("eth");
    assert_eq!(eth.destination, ipv4_multicast_mac(&GROUP_V4));
    let (header, _opts, _payload) = Ipv4Header::parse(eth.payload).expect("ipv4");
    assert_eq!(header.destination, GROUP_V4);
    assert_eq!(header.ttl, 1, "multicast data stays on the local link");
    // The member delivers it as a verbatim datagram event.
    let mut events = Vec::new();
    for frame in &out.frames {
        events.extend(b.on_frame_collect(&frame.bytes, t(0)).events);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        StackEvent::UdpDatagram {
            destination: IpAddr::V4(d),
            source_port: 5000,
            destination_port: 7000,
            payload,
            ..
        } if *d == GROUP_V4 && payload == b"mcast4"
    )));
}

#[test]
fn ipv6_multicast_datagram_transmit_reaches_a_member() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    b.join_multicast(IpAddr::V6(GROUP_V6), t(3)).expect("join");
    let out = a
        .send_datagram_collect(IpAddr::V6(GROUP_V6), 6000, 9000, b"mcast6", t(3))
        .expect("send");
    assert_eq!(out.frames.len(), 1, "one multicast frame, no resolution");
    let eth = EthernetFrame::parse(&out.frames[0].bytes).expect("eth");
    assert_eq!(eth.destination, ipv6_multicast_mac(&GROUP_V6));
    let (header, _payload) = Ipv6Header::parse(eth.payload).expect("ipv6");
    assert_eq!(header.destination, GROUP_V6);
    assert_eq!(
        header.hop_limit, 1,
        "multicast data stays on the local link"
    );
    let mut events = Vec::new();
    for frame in &out.frames {
        events.extend(b.on_frame_collect(&frame.bytes, t(3)).events);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        StackEvent::UdpDatagram {
            destination: IpAddr::V6(d),
            payload,
            ..
        } if *d == GROUP_V6 && payload == b"mcast6"
    )));
}

// --- TCP over the engine (N5c Layer 1) ---------------------------------
//
// The engine is stateless for TCP: it demultiplexes a checksum-valid
// inbound segment to a `StackEvent::TcpSegment` and originates a segment
// (folding the pseudo-header checksum over the source it selects) through
// `send_tcp`. The connection state machine is the `tcp::conn::Tcb`. These
// tests wire two `Tcb`s across two back-to-back `Stack`s, exactly as the
// live `netstack` does, and prove a real handshake and bidirectional data
// transfer flow through the new demux/originate paths.

use crate::tcp::conn::{State, Tcb, TcpConfig};
use crate::tcp::TcpSegment;

const A_PORT: u16 = 40000;
const B_PORT: u16 = 80;

/// One side of the TCP link: its engine, its connection, and the peer
/// address its segments are sent to.
struct TcpSide {
    stack: Stack,
    tcb: Tcb,
    peer: IpAddr,
}

impl TcpSide {
    /// Drain the connection's outbound segments through the engine,
    /// returning the frames to hand the peer's engine.
    fn transmit(&mut self, now: Duration64) -> Vec<Vec<u8>> {
        let stack = &mut self.stack;
        let peer = self.peer;
        let mut frames = Vec::new();
        self.tcb.poll_transmit(now, |seg| {
            // A momentary resolution/route miss parks the segment in the
            // engine, which emits it on resolution; treat it as sent.
            if let Ok(out) = stack.send_tcp_collect(peer, &seg.meta, seg.payload, seg.gso_size, now)
            {
                frames.extend(tx_bytes(out.frames));
            }
            true
        });
        frames
    }

    /// Feed one frame into the engine, feeding any surfaced TCP segment
    /// into the connection and returning the engine's reply frames.
    fn receive(&mut self, frame: &[u8], now: Duration64) -> Vec<Vec<u8>> {
        let out = self.stack.on_frame_collect(frame, now);
        for event in &out.events {
            if let StackEvent::TcpSegment {
                source,
                destination,
                segment,
            } = event
            {
                let pseudo = match (source, destination) {
                    (IpAddr::V4(s), IpAddr::V4(d)) => Pseudo::V4 {
                        source: *s,
                        destination: *d,
                    },
                    (IpAddr::V6(s), IpAddr::V6(d)) => Pseudo::V6 {
                        source: *s,
                        destination: *d,
                    },
                    _ => continue,
                };
                if let Some(seg) = TcpSegment::parse(pseudo, segment) {
                    self.tcb.on_segment(&seg, now);
                }
            }
        }
        tx_bytes(out.frames)
    }
}

/// Run the two sides until the link is quiet at `now`: transmit both
/// connections' due segments and the engines' timer output, then exchange
/// every frame (feeding surfaced TCP segments into the connections) until
/// nothing more moves. A frame `X.transmit()` produces is destined for the
/// *peer*, so it is always enqueued onto the peer's inbound queue.
fn drive_tcp(a: &mut TcpSide, b: &mut TcpSide, now: Duration64) {
    let mut to_b: VecDeque<Vec<u8>> = VecDeque::new();
    let mut to_a: VecDeque<Vec<u8>> = VecDeque::new();
    a.tcb.advance(now);
    b.tcb.advance(now);
    to_b.extend(tx_bytes(a.stack.advance_collect(now).frames));
    to_a.extend(tx_bytes(b.stack.advance_collect(now).frames));
    to_b.extend(a.transmit(now));
    to_a.extend(b.transmit(now));
    for _ in 0..256 {
        if to_a.is_empty() && to_b.is_empty() {
            // Both sides may still owe a segment (e.g. an ACK unlocked by
            // a delivered segment); pull once more before concluding.
            to_b.extend(a.transmit(now));
            to_a.extend(b.transmit(now));
            if to_a.is_empty() && to_b.is_empty() {
                break;
            }
        }
        if let Some(frame) = to_b.pop_front() {
            to_a.extend(b.receive(&frame, now));
            to_a.extend(b.transmit(now));
        }
        if let Some(frame) = to_a.pop_front() {
            to_b.extend(a.receive(&frame, now));
            to_b.extend(a.transmit(now));
        }
    }
}

fn tcp_pair() -> (TcpSide, TcpSide) {
    let mut sa = stack(MAC_A, IID_A);
    let mut sb = stack(MAC_B, IID_B);
    sa.set_ipv4_config(V4_A, 24, None).expect("configure A");
    sb.set_ipv4_config(V4_B, 24, None).expect("configure B");
    let cfg = TcpConfig::default();
    let a = TcpSide {
        stack: sa,
        tcb: Tcb::connect(cfg, A_PORT, B_PORT, 1000, t(0)),
        peer: IpAddr::V4(V4_B),
    };
    let b = TcpSide {
        stack: sb,
        // Passive open: the listener learns the client port from the SYN.
        tcb: Tcb::listen(cfg, B_PORT, 0, 5000),
        peer: IpAddr::V4(V4_A),
    };
    (a, b)
}

/// An IPv6 link-local TCP pair, brought through DAD so each side's
/// link-local address is preferred and reachable. Each connection's
/// `TcpConfig.local_mss` is seeded from [`Stack::tcp_local_mss`] — exactly
/// as the netstack seeds a real connection — so the segment size accounts
/// for the 40-byte IPv6 header against the 1500-byte link MTU.
fn tcp_pair_v6() -> (TcpSide, TcpSide) {
    let mut sa = stack(MAC_A, IID_A);
    let mut sb = stack(MAC_B, IID_B);
    bring_up(&mut sa, &mut sb);
    let a_addr = IpAddr::V6(link_local(IID_A));
    let b_addr = IpAddr::V6(link_local(IID_B));
    let a_mss = sa.tcp_local_mss(b_addr, t(1)).expect("A reaches B");
    let b_mss = sb.tcp_local_mss(a_addr, t(1)).expect("B reaches A");
    let a = TcpSide {
        stack: sa,
        tcb: Tcb::connect(
            TcpConfig {
                local_mss: a_mss,
                ..TcpConfig::default()
            },
            A_PORT,
            B_PORT,
            1000,
            t(1),
        ),
        peer: b_addr,
    };
    let b = TcpSide {
        stack: sb,
        // Passive open: the listener learns the client port from the SYN.
        tcb: Tcb::listen(
            TcpConfig {
                local_mss: b_mss,
                ..TcpConfig::default()
            },
            B_PORT,
            0,
            5000,
        ),
        peer: a_addr,
    };
    (a, b)
}

#[test]
fn tcp_handshake_and_bidirectional_data_over_the_engine() {
    let (mut a, mut b) = tcp_pair();
    // A time step past the default delayed-ACK (100 ms) each round, so an
    // owed ACK is always released, keeping the conversation progressing.
    let mut clock = 1u64;
    let mut step = || {
        let now = Duration64::from_nanos(clock * 200_000_000);
        clock += 1;
        now
    };

    // Handshake.
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
        if a.tcb.is_established() && b.tcb.is_established() {
            break;
        }
    }
    assert_eq!(a.tcb.state(), State::Established, "client established");
    assert_eq!(b.tcb.state(), State::Established, "server established");
    // The listener learned the client's port from the SYN.
    assert_eq!(b.tcb.remote_port(), A_PORT);

    // Client -> server data.
    a.tcb.send(b"hello over tcp").expect("client send");
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
    }
    let mut got = [0u8; 64];
    let n = b.tcb.recv(&mut got);
    assert_eq!(&got[..n], b"hello over tcp", "server received client data");

    // Server -> client data.
    b.tcb.send(b"and back again").expect("server send");
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
    }
    let mut got = [0u8; 64];
    let n = a.tcb.recv(&mut got);
    assert_eq!(&got[..n], b"and back again", "client received server data");

    // Orderly close from the client: the server observes the peer FIN.
    a.tcb.close(step()).expect("client close");
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
    }
    assert!(
        matches!(
            b.tcb.state(),
            State::CloseWait | State::LastAck | State::Closing | State::Closed
        ),
        "server saw the peer FIN, state = {:?}",
        b.tcb.state()
    );
}

#[test]
fn tcp_bulk_transfer_over_the_engine() {
    let (mut a, mut b) = tcp_pair();
    let mut clock = 1u64;
    let mut step = || {
        let now = Duration64::from_nanos(clock * 200_000_000);
        clock += 1;
        now
    };
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
        if a.tcb.is_established() && b.tcb.is_established() {
            break;
        }
    }
    assert!(a.tcb.is_established() && b.tcb.is_established());

    // A payload well past one MSS, so it exercises segmentation, windowing,
    // and cumulative ACKs through the real engine.
    let payload: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
    let mut offered = 0usize;
    let mut received: Vec<u8> = Vec::new();
    for _ in 0..200 {
        if offered < payload.len() {
            offered += a.tcb.send(&payload[offered..]).expect("send");
        }
        drive_tcp(&mut a, &mut b, step());
        let mut buf = [0u8; 4096];
        loop {
            let n = b.tcb.recv(&mut buf);
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }
        if received.len() == payload.len() {
            break;
        }
    }
    assert_eq!(received, payload, "bulk stream arrived intact and in order");
}

#[test]
fn tcp_bulk_transfer_over_ipv6_respects_the_link_mtu() {
    // Regression (N5c): a full-size data segment on an IPv6 link must fit
    // the 1500-byte MTU once the 40-byte IPv6 header and the TCP options
    // are added. Before the send segment size was clamped to the path MSS
    // (link MTU minus the family's IP header and the fixed TCP header) and
    // reduced by the carried option bytes, every full-size segment
    // overflowed the MTU and was silently dropped by `send_tcp` — only each
    // burst's short trailing segment reached the peer and a bulk transfer
    // stalled to the user timeout. This drives a multi-segment transfer
    // over a real IPv6 link and asserts every byte arrives, in order.
    let (mut a, mut b) = tcp_pair_v6();
    let mut clock = 2u64;
    let mut step = || {
        let now = Duration64::from_nanos(clock * 200_000_000);
        clock += 1;
        now
    };
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
        if a.tcb.is_established() && b.tcb.is_established() {
            break;
        }
    }
    assert!(
        a.tcb.is_established() && b.tcb.is_established(),
        "v6 handshake established"
    );

    let payload: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
    let mut offered = 0usize;
    let mut received: Vec<u8> = Vec::new();
    for _ in 0..200 {
        if offered < payload.len() {
            offered += a.tcb.send(&payload[offered..]).expect("send");
        }
        drive_tcp(&mut a, &mut b, step());
        let mut buf = [0u8; 4096];
        loop {
            let n = b.tcb.recv(&mut buf);
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }
        if received.len() == payload.len() {
            break;
        }
    }
    assert_eq!(
        received, payload,
        "the whole IPv6 bulk stream arrived intact and in order"
    );
}

/// Engine-level transmit-checksum-offload conformance: a stack that
/// negotiated `TX_CSUM_TCP` emits a TCP frame carrying only the partial
/// (pseudo-header) checksum and a [`TxOffload::PartialChecksum`] descriptor
/// whose offsets address the transport checksum within the Ethernet frame;
/// completing that fold reproduces, byte-for-byte, the frame a stack
/// without the offload emits with a full software checksum.
#[test]
fn tcp_v4_tx_checksum_offload_matches_the_software_path() {
    use crate::tcp::{SeqNumber, TcpFlags, TcpOptions, TcpSegmentMeta};

    // Resolve B's MAC in `a`'s neighbour cache by exchanging one echo
    // round with a fresh peer B (the echo path drives ARP), so a later
    // `send_tcp` emits synchronously rather than parking.
    fn resolve_b(a: &mut Stack) {
        let mut b = stack(MAC_B, IID_B);
        b.set_ipv4_config(V4_B, 24, None).expect("cfg b");
        let out = a
            .send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(2))
            .expect("echo");
        let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
        for _ in 0..8 {
            let Some(f) = frames.pop_front() else { break };
            let ob = b.on_frame_collect(&f, t(2));
            for r in tx_bytes(ob.frames) {
                frames.extend(tx_bytes(a.on_frame_collect(&r, t(2)).frames));
            }
        }
    }

    let mut off_facts = facts(MAC_A);
    off_facts.offloads = tairix_abi::driver::net::NetOffloads::TX_CSUM_TCP;
    let mut off = Stack::new(&StackConfig::new(off_facts, IID_A, 0x1234), t(0)).expect("valid");
    let mut soft = stack(MAC_A, IID_A);
    off.set_ipv4_config(V4_A, 24, None).expect("cfg off");
    soft.set_ipv4_config(V4_A, 24, None).expect("cfg soft");
    resolve_b(&mut off);
    resolve_b(&mut soft);

    let meta = TcpSegmentMeta {
        source_port: A_PORT,
        destination_port: B_PORT,
        seq: SeqNumber::new(1),
        ack: SeqNumber::new(0),
        flags: TcpFlags::SYN,
        window: 1000,
        urgent: 0,
        options: TcpOptions::new(),
    };
    let off_out = off
        .send_tcp_collect(IpAddr::V4(V4_B), &meta, b"tcp-tx-offload", None, t(3))
        .expect("send off");
    let soft_out = soft
        .send_tcp_collect(IpAddr::V4(V4_B), &meta, b"tcp-tx-offload", None, t(3))
        .expect("send soft");
    assert_eq!(off_out.frames.len(), 1);
    assert_eq!(soft_out.frames.len(), 1);

    // The offloaded stack attaches the eth(14)+ipv4(20) checksum offsets;
    // the software stack attaches none.
    assert_eq!(
        off_out.frames[0].offload,
        TxOffload::PartialChecksum {
            csum_start: 34,
            csum_offset: 16,
        }
    );
    assert_eq!(soft_out.frames[0].offload, TxOffload::None);

    // Complete the device's fold over the offloaded frame and compare.
    let mut completed = off_out.frames[0].bytes.clone();
    let start = 34usize;
    let sum = crate::internet_checksum(&completed[start..]);
    completed[start + 16..start + 18].copy_from_slice(&sum.to_be_bytes());
    assert_eq!(completed, soft_out.frames[0].bytes);
}

/// Engine-level TCP-segmentation-offload conformance: a stack that
/// negotiated segmentation emits one over-size super-segment plus a
/// [`TxOffload::TcpSegment`] descriptor; splitting that super-segment as
/// the device is contractually required to (per-`gso_size` payloads, the
/// header replicated with the sequence advanced, PSH only on the last
/// segment, each segment's checksum recomputed) reproduces, TCP-segment
/// for TCP-segment, exactly the segments the stack emits per-MSS with no
/// offload. Compared at the TCP layer so the check is independent of the
/// per-segment IPv4 identification the device assigns.
#[test]
#[allow(clippy::too_many_lines)]
fn tcp_v4_tx_segmentation_offload_matches_the_software_path() {
    use crate::checksum::Checksum;
    use crate::tcp::{SeqNumber, TcpFlags, TcpOptions, TcpSegmentMeta, PROTOCOL_TCP};

    fn resolve_b(a: &mut Stack) {
        let mut b = stack(MAC_B, IID_B);
        b.set_ipv4_config(V4_B, 24, None).expect("cfg b");
        let out = a
            .send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(2))
            .expect("echo");
        let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
        for _ in 0..8 {
            let Some(f) = frames.pop_front() else { break };
            let ob = b.on_frame_collect(&f, t(2));
            for r in tx_bytes(ob.frames) {
                frames.extend(tx_bytes(a.on_frame_collect(&r, t(2)).frames));
            }
        }
    }

    // The TCP segment (strip the 14-byte Ethernet + 20-byte IPv4 header).
    const IP_TCP: usize = 34;
    const MSS: u16 = 100;

    let mut facts_off = facts(MAC_A);
    facts_off.offloads = tairix_abi::driver::net::NetOffloads::from_bits(
        tairix_abi::driver::net::NetOffloads::TX_CSUM_TCP.bits()
            | tairix_abi::driver::net::NetOffloads::TX_SEGMENT_TCP.bits(),
    )
    .expect("defined bits");
    let mut off = Stack::new(&StackConfig::new(facts_off, IID_A, 0x4321), t(0)).expect("valid");
    let mut soft = stack(MAC_A, IID_A);
    off.set_ipv4_config(V4_A, 24, None).expect("cfg off");
    soft.set_ipv4_config(V4_A, 24, None).expect("cfg soft");
    resolve_b(&mut off);
    resolve_b(&mut soft);

    let base = SeqNumber::new(1);
    let payload: Vec<u8> = (0..250u32).map(|i| (i % 253) as u8).collect();

    // Reference: the per-MSS segments the software path would emit, each
    // built with the seq/flags the device must reproduce (PSH on the last).
    let mut reference: Vec<Vec<u8>> = Vec::new();
    let mut offset = 0usize;
    while offset < payload.len() {
        let end = (offset + usize::from(MSS)).min(payload.len());
        let last = end == payload.len();
        let mut flags = TcpFlags::ACK;
        if last {
            flags = flags | TcpFlags::PSH;
        }
        let meta = TcpSegmentMeta {
            source_port: A_PORT,
            destination_port: B_PORT,
            seq: base.add(u32::try_from(offset).expect("offset fits u32")),
            ack: SeqNumber::new(0),
            flags,
            window: 2000,
            urgent: 0,
            options: TcpOptions::new(),
        };
        let out = soft
            .send_tcp_collect(IpAddr::V4(V4_B), &meta, &payload[offset..end], None, t(3))
            .expect("send soft segment");
        assert_eq!(out.frames.len(), 1);
        reference.push(out.frames[0].bytes[IP_TCP..].to_vec());
        offset = end;
    }
    assert_eq!(
        reference.len(),
        3,
        "250 bytes over MSS 100 is three segments"
    );

    // Offloaded: one super-segment (PSH set — it ends the data run).
    let meta = TcpSegmentMeta {
        source_port: A_PORT,
        destination_port: B_PORT,
        seq: base,
        ack: SeqNumber::new(0),
        flags: TcpFlags::ACK | TcpFlags::PSH,
        window: 2000,
        urgent: 0,
        options: TcpOptions::new(),
    };
    let off_out = off
        .send_tcp_collect(IpAddr::V4(V4_B), &meta, &payload, Some(MSS), t(3))
        .expect("send super-segment");
    assert_eq!(off_out.frames.len(), 1, "TSO emits one frame");
    let TxOffload::TcpSegment {
        csum_start,
        csum_offset,
        gso_size,
        hdr_len,
        ipv6,
    } = off_out.frames[0].offload
    else {
        panic!("expected a TcpSegment offload");
    };
    assert_eq!(
        (csum_start, csum_offset, gso_size, hdr_len, ipv6),
        (34, 16, MSS, 54, false)
    );

    // Software-segment the super-frame exactly as the device must, then
    // compare each produced TCP segment to the reference.
    let super_tcp = &off_out.frames[0].bytes[IP_TCP..];
    // The super-segment's checksum field holds the length-0 pseudo-header
    // partial sum (Linux `CHECKSUM_PARTIAL` for GSO): the device adds each
    // segment's own length when it splits.
    let field = u16::from_be_bytes([super_tcp[16], super_tcp[17]]);
    let expected_partial = Checksum::ipv4_pseudo(V4_A, V4_B, PROTOCOL_TCP, 0).partial();
    assert_eq!(
        field, expected_partial,
        "GSO super-segment carries the length-0 partial checksum"
    );
    let tcp_header_len = usize::from(hdr_len) - IP_TCP;
    let header = &super_tcp[..tcp_header_len];
    let body = &super_tcp[tcp_header_len..];
    let mut produced: Vec<Vec<u8>> = Vec::new();
    let mut off = 0usize;
    while off < body.len() {
        let end = (off + usize::from(gso_size)).min(body.len());
        let last = end == body.len();
        let mut seg = header.to_vec();
        // Advance the sequence number by the payload already segmented.
        let seq = base.add(u32::try_from(off).expect("offset fits u32"));
        seg[4..8].copy_from_slice(&seq.value().to_be_bytes());
        // The device sets PSH/FIN only on the final segment.
        if !last {
            seg[13] &= !(0x08 | 0x01);
        }
        // Zero the checksum field, append the payload slice, recompute the
        // complete checksum over the per-segment pseudo-header.
        seg[16..18].copy_from_slice(&[0, 0]);
        seg.extend_from_slice(&body[off..end]);
        let tcp_len = u16::try_from(seg.len()).expect("segment fits");
        let mut sum = Checksum::ipv4_pseudo(V4_A, V4_B, PROTOCOL_TCP, tcp_len);
        sum.push(&seg);
        seg[16..18].copy_from_slice(&sum.finish().to_be_bytes());
        produced.push(seg);
        off = end;
    }
    assert_eq!(
        produced, reference,
        "device segmentation reproduces the per-MSS software segments byte-for-byte"
    );
}
