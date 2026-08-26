//! Host tests for the receive pre-filter.
//!
//! Two properties matter and are checked in both directions: the noise a
//! real LAN generates is shed, and nothing the stack could have wanted is
//! ever dropped — including on every path where the classifier cannot be
//! sure.

use super::RxClassifier;
use crate::addr::{solicited_node_multicast, Ipv4Addr, Ipv6Addr, ALL_NODES};
use crate::arp::{self, ArpPacket};
use crate::eth::{ETHERNET_HEADER_LEN, ETHERTYPE_ARP, ETHERTYPE_IPV4, ETHERTYPE_IPV6};
use crate::ipv4::Ipv4Header;
use crate::udp::PROTOCOL_UDP;
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::driver::net::MacAddress;
use tairix_abi::driver::net_channel::{
    RxFilterPolicy, MAX_FILTER_GROUPS_V4, MAX_FILTER_GROUPS_V6, MAX_FILTER_V4, MAX_FILTER_V6,
};
use tairix_abi::driver::net_ring::RxAdmit;

/// The one IPv4 group every multicast-capable host joins (RFC 1112), so the
/// tests below name it as *the* joined group.
const ALL_SYSTEMS_V4: [u8; 4] = [224, 0, 0, 1];

/// A group this host has not joined — mDNS, the noise this sheds.
const UNJOINED_V4: [u8; 4] = [224, 0, 0, 251];

/// The IPv6 counterpart: mDNS over v6, never joined here.
const UNJOINED_V6: [u8; 16] = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xfb];

/// This host's IPv4 address and its subnet's directed broadcast.
const OURS_V4: [u8; 4] = [10, 0, 2, 15];
const OUR_BROADCAST_V4: [u8; 4] = [10, 0, 2, 255];

/// Another host on the same segment.
const THEIRS_V4: [u8; 4] = [10, 0, 2, 99];

/// This host's IPv6 address, and another host's.
const OURS_V6: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15];
const THEIRS_V6: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x99];

/// A classifier for an interface holding both of this host's addresses, the
/// all-systems group every host joins, and one broadcast-consuming port (a
/// running DHCPv4 client's).
fn ours() -> RxClassifier {
    RxClassifier::new(RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &[OURS_V6],
        &[ALL_SYSTEMS_V4],
        &[],
        &[crate::dhcp::CLIENT_PORT],
    ))
}

/// Build an Ethernet frame with `ethertype` over `payload`.
fn frame(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; ETHERNET_HEADER_LEN + payload.len()];
    out[..6].copy_from_slice(MacAddress::BROADCAST.as_octets());
    out[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 1]);
    out[12..14].copy_from_slice(&ethertype.to_be_bytes());
    out[ETHERNET_HEADER_LEN..].copy_from_slice(payload);
    out
}

/// An IPv4 frame of `protocol` addressed to `destination` over `payload`.
///
/// A real header, checksum and all: the classifier parses it now, and a
/// header it cannot parse is admitted, so a hand-rolled one would make every
/// shedding assertion below pass for the wrong reason.
fn ipv4_proto_to(destination: [u8; 4], protocol: u8, payload: &[u8]) -> Vec<u8> {
    let header = Ipv4Header::new(
        Ipv4Addr::from(THEIRS_V4),
        Ipv4Addr::from(destination),
        protocol,
    );
    let mut packet = vec![0u8; crate::ipv4::IPV4_HEADER_LEN + payload.len()];
    header.write(&mut packet, payload.len()).expect("fits");
    packet[crate::ipv4::IPV4_HEADER_LEN..].copy_from_slice(payload);
    frame(ETHERTYPE_IPV4, &packet)
}

/// An IPv4 frame addressed to `destination`, carrying a protocol with no
/// port to match on (ICMP), so the destination alone decides.
fn ipv4_to(destination: [u8; 4]) -> Vec<u8> {
    ipv4_proto_to(destination, crate::ipv4::PROTOCOL_ICMP, &[0u8; 8])
}

/// A UDP datagram to `destination` port `port`.
fn udp_to(destination: [u8; 4], port: u16) -> Vec<u8> {
    let mut udp = [0u8; 8];
    udp[2..4].copy_from_slice(&port.to_be_bytes());
    udp[4..6].copy_from_slice(&8u16.to_be_bytes());
    ipv4_proto_to(destination, PROTOCOL_UDP, &udp)
}

/// An IPv6 frame addressed to `destination`.
fn ipv6_to(destination: [u8; 16]) -> Vec<u8> {
    let mut header = vec![0u8; 40];
    header[0] = 0x60;
    header[24..40].copy_from_slice(&destination);
    frame(ETHERTYPE_IPV6, &header)
}

/// An ARP frame of `operation` asking after `target`.
fn arp_frame(operation: u16, target: [u8; 4]) -> Vec<u8> {
    let packet = ArpPacket {
        operation,
        sender_hardware: MacAddress::new([0x02, 0, 0, 0, 0, 2]),
        sender_protocol: Ipv4Addr::from(THEIRS_V4),
        target_hardware: MacAddress::new([0; 6]),
        target_protocol: Ipv4Addr::from(target),
    };
    let mut body = [0u8; arp::ARP_PACKET_LEN];
    packet
        .write(&mut body)
        .expect("the buffer is exactly one packet");
    frame(ETHERTYPE_ARP, &body)
}

// --- The noise that is shed ---------------------------------------------

#[test]
fn an_arp_request_for_another_host_is_dropped() {
    // The dominant broadcast load on a busy segment, and the whole reason
    // this filter exists.
    assert!(!ours().admit(&arp_frame(arp::OP_REQUEST, THEIRS_V4)));
}

#[test]
fn traffic_directed_at_another_host_is_dropped() {
    assert!(!ours().admit(&ipv4_to(THEIRS_V4)));
    assert!(!ours().admit(&ipv6_to(THEIRS_V6)));
}

#[test]
fn an_ethertype_the_stack_does_not_speak_is_dropped() {
    // Wake-on-LAN, LLDP, 802.1X, IPX, AoE: nothing local consumes them, and
    // recognising that is a positive identification rather than a parse the
    // filter is unsure of.
    for ethertype in [0x0842_u16, 0x88CC, 0x888E, 0x8137, 0x22F0] {
        assert!(
            !ours().admit(&frame(ethertype, &[0u8; 40])),
            "ethertype {ethertype:#06x} has no local consumer"
        );
    }
}

// --- Everything the stack could want ------------------------------------

#[test]
fn traffic_addressed_to_us_is_admitted() {
    assert!(ours().admit(&ipv4_to(OURS_V4)));
    assert!(ours().admit(&ipv6_to(OURS_V6)));
}

#[test]
fn a_joined_group_is_admitted_and_an_unjoined_one_is_shed() {
    // The stack delivers a group destination only to a member, so this is
    // the mirror of its own rule. mDNS and its kin are the bulk of the noise
    // and are shed here without a stack wake.
    assert!(ours().admit(&ipv4_to(ALL_SYSTEMS_V4)));
    assert!(!ours().admit(&ipv4_to(UNJOINED_V4)));
    assert!(!ours().admit(&ipv6_to(UNJOINED_V6)));
}

#[test]
fn the_groups_derived_from_our_addresses_are_admitted() {
    // All-nodes and solicited-node are not carried in the policy: both follow
    // from the addresses it already names, so there is no second set to keep
    // in step — but they must still be admitted.
    assert!(ours().admit(&ipv6_to(ALL_NODES.octets())));
    let solicited = solicited_node_multicast(&Ipv6Addr::from(OURS_V6));
    assert!(ours().admit(&ipv6_to(solicited.octets())));
}

#[test]
fn broadcast_is_shed_unless_a_local_consumer_holds_its_port() {
    // A broadcast datagram to a port something is bound to must survive
    // (a DHCPv4 reply arrives this way before any address exists); the
    // segment's `NetBIOS`/SSDP background noise, to ports nothing holds, is
    // shed without waking the stack.
    assert!(ours().admit(&udp_to(
        Ipv4Addr::BROADCAST.octets(),
        crate::dhcp::CLIENT_PORT
    )));
    assert!(ours().admit(&udp_to(OUR_BROADCAST_V4, crate::dhcp::CLIENT_PORT)));
    assert!(!ours().admit(&udp_to(Ipv4Addr::BROADCAST.octets(), 137)));
    assert!(!ours().admit(&udp_to(OUR_BROADCAST_V4, 138)));
    // Non-UDP broadcast is refused outright: the stack consumes a broadcast
    // destination as UDP or not at all.
    assert!(!ours().admit(&ipv4_to(Ipv4Addr::BROADCAST.octets())));
}

#[test]
fn a_socket_binding_widens_the_broadcast_ports_the_filter_admits() {
    // The service republishes the port summary as its datagram sockets come
    // and go, so a bound port that used to be shed is admitted.
    let before = RxClassifier::new(RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &[OURS_V6],
        &[],
        &[],
        &[],
    ));
    assert!(!before.admit(&udp_to(Ipv4Addr::BROADCAST.octets(), 9999)));
    let after = RxClassifier::new(RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &[OURS_V6],
        &[],
        &[],
        &[9999],
    ));
    assert!(after.admit(&udp_to(Ipv4Addr::BROADCAST.octets(), 9999)));
    assert!(after.admit(&udp_to(OUR_BROADCAST_V4, 9999)));
}

#[test]
fn a_broadcast_fragment_is_admitted_without_inspection() {
    // The port lives in the first fragment and reassembly is the stack's
    // job, so guessing here could shed a reply.
    let mut udp = [0u8; 8];
    udp[2..4].copy_from_slice(&137u16.to_be_bytes());
    let mut header = Ipv4Header::new(Ipv4Addr::from(THEIRS_V4), Ipv4Addr::BROADCAST, PROTOCOL_UDP);
    header.fragment_offset = 8;
    header.more_fragments = true;
    let mut packet = vec![0u8; crate::ipv4::IPV4_HEADER_LEN + udp.len()];
    header.write(&mut packet, udp.len()).expect("fits");
    packet[crate::ipv4::IPV4_HEADER_LEN..].copy_from_slice(&udp);
    assert!(ours().admit(&frame(ETHERTYPE_IPV4, &packet)));
}

#[test]
fn an_arp_request_for_us_is_admitted() {
    assert!(ours().admit(&arp_frame(arp::OP_REQUEST, OURS_V4)));
}

#[test]
fn every_arp_reply_is_admitted() {
    // A reply may answer a request this host sent, and only the stack's
    // neighbour cache knows whether it did.
    assert!(ours().admit(&arp_frame(arp::OP_REPLY, THEIRS_V4)));
    assert!(ours().admit(&arp_frame(arp::OP_REPLY, OURS_V4)));
}

// --- Fail open, on every path -------------------------------------------

#[test]
fn a_policy_with_no_addresses_admits_everything_addressable() {
    // The state before the stack has published an address set: an interface
    // still doing DHCP must not have its offer filtered away.
    let empty = RxClassifier::new(RxFilterPolicy::admit_all());
    assert!(empty.admit(&ipv4_to(THEIRS_V4)));
    assert!(empty.admit(&ipv6_to(THEIRS_V6)));
    assert!(empty.admit(&arp_frame(arp::OP_REQUEST, THEIRS_V4)));
}

#[test]
fn a_policy_that_could_not_hold_every_address_admits_all_unicast() {
    // More addresses than the fixed bound: the filter widens rather than
    // dropping traffic to an address it was never told about.
    let v4: Vec<([u8; 4], [u8; 4])> = (0..=MAX_FILTER_V4)
        .map(|n| {
            let last = u8::try_from(n).expect("small");
            ([10, 0, 2, last], OUR_BROADCAST_V4)
        })
        .collect();
    let policy = RxFilterPolicy::new(&v4, &[OURS_V6], &[ALL_SYSTEMS_V4], &[], &[]);
    assert!(!policy.is_exhaustive());
    let wide = RxClassifier::new(policy);
    assert!(wide.admit(&ipv4_to(THEIRS_V4)));
    assert!(wide.admit(&arp_frame(arp::OP_REQUEST, THEIRS_V4)));

    let v6: Vec<[u8; 16]> = (0..=MAX_FILTER_V6)
        .map(|n| {
            let mut address = OURS_V6;
            address[15] = u8::try_from(n).expect("small");
            address
        })
        .collect();
    let policy = RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &v6,
        &[ALL_SYSTEMS_V4],
        &[],
        &[],
    );
    assert!(!policy.is_exhaustive());
    assert!(RxClassifier::new(policy).admit(&ipv6_to(THEIRS_V6)));
}

#[test]
fn a_policy_that_could_not_hold_every_group_admits_all_multicast() {
    // Truncation must never be silent: a policy that could not name every
    // joined group widens rather than shedding a group it was not told about.
    let groups: Vec<[u8; 4]> = (0..=MAX_FILTER_GROUPS_V4)
        .map(|n| [224, 0, 0, u8::try_from(n).expect("small")])
        .collect();
    let policy = RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &[OURS_V6],
        &groups,
        &[],
        &[],
    );
    assert!(!policy.is_exhaustive());
    assert!(RxClassifier::new(policy).admit(&ipv4_to(UNJOINED_V4)));

    let groups: Vec<[u8; 16]> = (0..=MAX_FILTER_GROUPS_V6)
        .map(|n| {
            let mut group = UNJOINED_V6;
            group[15] = u8::try_from(n).expect("small");
            group
        })
        .collect();
    let policy = RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &[OURS_V6],
        &[],
        &groups,
        &[],
    );
    assert!(!policy.is_exhaustive());
    assert!(RxClassifier::new(policy).admit(&ipv6_to(UNJOINED_V6)));
}

#[test]
fn a_frame_the_filter_cannot_parse_is_admitted() {
    let filter = ours();
    // Truncated beyond use: the stack decides why it is invalid.
    assert!(filter.admit(&[]));
    assert!(filter.admit(&[0u8; 8]));
    // A claimed ethertype with a payload too short to hold its header.
    assert!(filter.admit(&frame(ETHERTYPE_IPV4, &[0x45])));
    assert!(filter.admit(&frame(ETHERTYPE_IPV6, &[0x60])));
    assert!(filter.admit(&frame(ETHERTYPE_ARP, &[0u8; 4])));
    // A version nibble that does not match the ethertype.
    assert!(filter.admit(&frame(ETHERTYPE_IPV4, &[0x60u8; 20])));
    assert!(filter.admit(&frame(ETHERTYPE_IPV6, &[0x45u8; 40])));
}

// --- A family with no address must not filter that family --------------

#[test]
fn an_interface_with_no_ipv4_address_admits_ipv4_unicast() {
    // The bug this pins killed DHCP on real hardware. At boot the interface
    // truthfully has zero IPv4 addresses, so the policy is "exhaustive" — it
    // names all none of them — and matching a destination against an empty
    // list dropped every IPv4 unicast, including the OFFER/ACK addressed to
    // the very address being offered. No address, so no route, so every send
    // was refused. "Names them all" is not "can decide".
    let boot = RxClassifier::new(RxFilterPolicy::new(&[], &[OURS_V6], &[], &[], &[]));
    assert!(boot.policy().is_exhaustive());
    assert!(
        boot.admit(&ipv4_to(THEIRS_V4)),
        "an IPv4 unicast must be admitted while the interface has no v4 address"
    );
    // The broadcast path a DHCP offer may instead take stays admitted too.
    assert!(boot.admit(&ipv4_to(Ipv4Addr::BROADCAST.octets())));
    // And an ARP request cannot be judged either, so it is admitted.
    assert!(boot.admit(&arp_frame(arp::OP_REQUEST, THEIRS_V4)));
}

#[test]
fn an_interface_with_no_ipv6_address_admits_ipv6_unicast() {
    let boot = RxClassifier::new(RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &[],
        &[ALL_SYSTEMS_V4],
        &[],
        &[],
    ));
    assert!(boot.policy().is_exhaustive());
    assert!(boot.admit(&ipv6_to(THEIRS_V6)));
    // The family that *does* have an address still filters.
    assert!(!boot.admit(&ipv4_to(THEIRS_V4)));
    assert!(boot.admit(&ipv4_to(OURS_V4)));
}

#[test]
fn a_policy_with_no_address_at_all_filters_nothing_addressable() {
    let bare = RxClassifier::new(RxFilterPolicy::new(&[], &[], &[], &[], &[]));
    assert!(bare.admit(&ipv4_to(THEIRS_V4)));
    assert!(bare.admit(&ipv6_to(THEIRS_V6)));
    assert!(bare.admit(&arp_frame(arp::OP_REQUEST, THEIRS_V4)));
}
