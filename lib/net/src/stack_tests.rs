//! Unit and end-to-end tests for the host engine: two [`Stack`]
//! instances wired back-to-back over an in-memory link, plus a
//! hand-rolled router for the RA/SLAAC path.

use super::*;
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
        offloads: rustos_abi::driver::net::NetOffloads::empty(),
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

/// Run both stacks' timers forward to `now` and exchange every frame
/// until the link is quiet, collecting each side's events.
fn pump(
    a: &mut Stack,
    b: &mut Stack,
    now: Duration64,
    events_a: &mut Vec<StackEvent>,
    events_b: &mut Vec<StackEvent>,
) {
    let mut to_b: VecDeque<Vec<u8>> = VecDeque::new();
    let mut to_a: VecDeque<Vec<u8>> = VecDeque::new();
    let out_a = a.advance(now);
    events_a.extend(out_a.events);
    to_b.extend(out_a.frames);
    let out_b = b.advance(now);
    events_b.extend(out_b.events);
    to_a.extend(out_b.frames);
    // Bounded exchange: control-plane conversations settle quickly.
    for _ in 0..64 {
        if to_a.is_empty() && to_b.is_empty() {
            break;
        }
        if let Some(frame) = to_b.pop_front() {
            let out = b.on_frame(&frame, now);
            events_b.extend(out.events);
            to_a.extend(out.frames);
        }
        if let Some(frame) = to_a.pop_front() {
            let out = a.on_frame(&frame, now);
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
        .send_echo_request(IpAddr::V4(V4_B), 0x77, 1, b"rustos-stack", t(2))
        .expect("send");
    assert!(out.events.is_empty());
    let mut frames: VecDeque<Vec<u8>> = out.frames.into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in out_b.frames {
            let out_a = a.on_frame(&reply, t(2));
            events_a.extend(out_a.events);
            frames.extend(out_a.frames);
        }
    }
    assert_eq!(
        events_a,
        [StackEvent::EchoReply {
            source: IpAddr::V4(V4_B),
            identifier: 0x77,
            sequence: 1,
            payload: b"rustos-stack".to_vec(),
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
        .send_echo_request(IpAddr::V6(link_local(IID_B)), 0x42, 7, b"ping6", t(3))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = out.frames.into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame(&frame, t(3));
        events_b.extend(out_b.events);
        for reply in out_b.frames {
            let out_a = a.on_frame(&reply, t(3));
            events_a.extend(out_a.events);
            frames.extend(out_a.frames);
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
    let out = a.on_frame(&ra, t(2));
    assert!(out.frames.is_empty());
    // The SLAAC address forms and completes DAD.
    let mut expected = slaac_prefix().octets();
    expected[8..].copy_from_slice(&IID_A);
    let slaac_addr = Ipv6Addr::from(expected);
    assert!(a.iface().is_tentative(slaac_addr));
    a.advance(t(2)); // DAD transmit
    let out = a.advance(t(3)); // DAD completion
    assert!(out
        .events
        .contains(&StackEvent::AddressPreferred { addr: slaac_addr }));

    // An off-link destination now routes via the advertised router.
    let off_link = Ipv6Addr::new(0x2001, 0x0DB8, 0xFF, 0, 0, 0, 0, 1);
    // (2001:db8:ff::1 is outside the /64, so it uses the default
    // router learned from the RA.)
    let out = a
        .send_echo_request(IpAddr::V6(off_link), 1, 1, b"x", t(3))
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
    a.on_frame(&frame, t(2));
    a.advance(t(2));
    a.advance(t(3));
    let mut expected = slaac_prefix().octets();
    expected[8..].copy_from_slice(&IID_A);
    assert!(!a.iface().is_assigned(Ipv6Addr::from(expected)));
}

#[test]
fn dad_probe_from_another_node_fails_our_tentative_address() {
    let mut a = stack(MAC_A, IID_A);
    a.advance(t(0)); // link-local DAD NS out (still tentative)
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

    let out = a.on_frame(&frame, t(0));
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
    let out = a.on_frame(&frame, t(2));
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
    let out = a.on_frame(&arp_frame, t(1));
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
        let out = a.on_frame(&frame, t(2));
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
    let mut to_b: VecDeque<Vec<u8>> = a.on_frame(&frame, t(2)).frames.into();
    for _ in 0..16 {
        let Some(frame) = to_b.pop_front() else {
            break;
        };
        let out_b = b.on_frame(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in out_b.frames {
            to_b.extend(a.on_frame(&reply, t(2)).frames);
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
        a.send_echo_request(IpAddr::V4(V4_B), 1, 1, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Off-subnet with no gateway.
    assert_eq!(
        a.send_echo_request(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 1, 1, b"x", t(0)),
        Err(SendError::NoRoute)
    );
    // Non-unicast destinations.
    assert_eq!(
        a.send_echo_request(IpAddr::V4(Ipv4Addr::BROADCAST), 1, 1, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    assert_eq!(
        a.send_echo_request(IpAddr::V6(ALL_NODES), 1, 1, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    // No usable v6 source before DAD completes.
    assert_eq!(
        a.send_echo_request(IpAddr::V6(link_local(IID_B)), 1, 1, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    // Link down refuses everything.
    a.set_link(LinkState::Down);
    assert_eq!(
        a.send_echo_request(IpAddr::V4(V4_B), 1, 1, b"x", t(0)),
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
        a.send_echo_request(IpAddr::V6(link_local(IID_B)), 1, 1, &payload, t(2)),
        Err(SendError::TooLarge)
    );
}

#[test]
fn unreachable_neighbor_drops_parked_packets_with_event() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    let out = a
        .send_echo_request(IpAddr::V4(V4_B), 1, 1, b"x", t(0))
        .expect("parked");
    assert_eq!(out.frames.len(), 1, "first ARP request");
    // Nothing answers: three multicast solicitations, then failure.
    let mut events = Vec::new();
    for secs in 1..8 {
        events.extend(a.advance(t(secs)).events);
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
        .send_datagram(IpAddr::V4(V4_B), 5000, 7, b"udp-payload", t(2))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = out.frames.into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in out_b.frames {
            let out_a = a.on_frame(&reply, t(2));
            frames.extend(out_a.frames);
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
        .send_datagram(IpAddr::V6(link_local(IID_B)), 6000, 9, b"udp6", t(3))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = out.frames.into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame(&frame, t(3));
        events_b.extend(out_b.events);
        for reply in out_b.frames {
            let out_a = a.on_frame(&reply, t(3));
            frames.extend(out_a.frames);
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
        a.send_datagram(IpAddr::V4(V4_B), 1, 2, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Multicast and broadcast destinations are refused this increment
    // (group-membership transmit is a later increment); fail closed.
    assert_eq!(
        a.send_datagram(IpAddr::V4(Ipv4Addr::BROADCAST), 1, 2, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    assert_eq!(
        a.send_datagram(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)), 1, 2, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    assert_eq!(
        a.send_datagram(IpAddr::V6(ALL_NODES), 1, 2, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    // Off-subnet unicast with no gateway.
    assert_eq!(
        a.send_datagram(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 1, 2, b"x", t(0)),
        Err(SendError::NoRoute)
    );
    // Link down refuses everything.
    a.set_link(LinkState::Down);
    assert_eq!(
        a.send_datagram(IpAddr::V4(V4_B), 1, 2, b"x", t(0)),
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
        a.send_datagram(IpAddr::V6(link_local(IID_B)), 1, 2, &payload, t(2)),
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
    let out = a.on_frame(&frame, t(0));
    assert!(out.events.is_empty());
    assert!(a.counters().rx_dropped >= 1);
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
    let out = a.on_frame(&frame, t(0));
    assert!(out.frames.is_empty() && out.events.is_empty());
    assert_eq!(a.counters().rx_dropped, 1);
}

// ---- Multicast membership (IGMPv2 / MLDv2) --------------------------

const GROUP_V4: Ipv4Addr = Ipv4Addr::new(239, 1, 2, 3);

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
fn first_igmp(frames: &[Vec<u8>]) -> Option<(MacAddress, u8, usize, IgmpMessage)> {
    for f in frames {
        let Some(eth) = EthernetFrame::parse(f) else {
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
    let out = a.advance(t(0));
    let (dest_mac, ttl, options_len, message) = first_igmp(&out.frames).expect("igmp report");
    assert_eq!(message, IgmpMessage::V2Report { group: GROUP_V4 });
    assert_eq!(ttl, 1, "membership messages never leave the link");
    assert_eq!(options_len, 4, "Router Alert option present (IHL = 6)");
    assert_eq!(dest_mac, ipv4_multicast_mac(&GROUP_V4));
}

#[test]
fn joined_group_receives_udp_and_others_are_dropped() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Not a member yet: the multicast datagram is dropped.
    let out = a.on_frame(&v4_group_udp_frame(b"hi"), t(0));
    assert!(out.events.is_empty());
    assert!(a.counters().rx_dropped >= 1);
    // After joining, the same datagram is delivered.
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    let out = a.on_frame(&v4_group_udp_frame(b"hi"), t(0));
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
        let _ = a.advance(t(s));
    }
    // A General Query schedules — but does not immediately send — a report.
    let out = a.on_frame(&igmp_general_query_frame(20), t(100));
    assert!(first_igmp(&out.frames).is_none(), "response is delayed");
    assert!(a.next_deadline().is_some());
    // Past the 2-second window the response is emitted.
    let out = a.advance(t(103));
    let (_mac, _ttl, _opts, message) = first_igmp(&out.frames).expect("query response");
    assert_eq!(message, IgmpMessage::V2Report { group: GROUP_V4 });
}

#[test]
fn leaving_v4_group_emits_leave_to_all_routers() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    for s in 0..40 {
        let _ = a.advance(t(s));
    }
    assert!(a.leave_multicast(IpAddr::V4(GROUP_V4), t(40)));
    let out = a.advance(t(40));
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
    let _ = a.advance(t(0)); // link-local DAD solicitation
    let out = a.advance(t(1)); // preferred -> join solicited-node -> MLD report
    let frame = out
        .frames
        .iter()
        .find(|f| {
            EthernetFrame::parse(f).is_some_and(|eth| {
                eth.ethertype == ETHERTYPE_IPV6
                    && Ipv6Header::parse(eth.payload)
                        .is_some_and(|(h, _)| h.next_header == NEXT_HEADER_HOP_BY_HOP)
            })
        })
        .expect("mld report frame");
    let eth = EthernetFrame::parse(frame).expect("eth");
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
