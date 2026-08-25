//! Host tests for the receive pre-filter.
//!
//! Two properties matter and are checked in both directions: the noise a
//! real LAN generates is shed, and nothing the stack could have wanted is
//! ever dropped — including on every path where the classifier cannot be
//! sure.

use super::{RxClassifier, IPV4_BROADCAST};
use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::arp::{self, ArpPacket};
use crate::eth::{ETHERNET_HEADER_LEN, ETHERTYPE_ARP, ETHERTYPE_IPV4, ETHERTYPE_IPV6};
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::driver::net::MacAddress;
use tairix_abi::driver::net_channel::{RxFilterPolicy, MAX_FILTER_V4, MAX_FILTER_V6};
use tairix_abi::driver::net_ring::RxAdmit;

/// This host's IPv4 address and its subnet's directed broadcast.
const OURS_V4: [u8; 4] = [10, 0, 2, 15];
const OUR_BROADCAST_V4: [u8; 4] = [10, 0, 2, 255];

/// Another host on the same segment.
const THEIRS_V4: [u8; 4] = [10, 0, 2, 99];

/// This host's IPv6 address, and another host's.
const OURS_V6: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15];
const THEIRS_V6: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x99];

/// A classifier for an interface holding both of this host's addresses.
fn ours() -> RxClassifier {
    RxClassifier::new(RxFilterPolicy::new(
        &[(OURS_V4, OUR_BROADCAST_V4)],
        &[OURS_V6],
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

/// An IPv4 frame addressed to `destination`. Only the fields the filter
/// reads are filled; the stack's own parse is what validates the rest.
fn ipv4_to(destination: [u8; 4]) -> Vec<u8> {
    let mut header = vec![0u8; 20];
    header[0] = 0x45;
    header[16..20].copy_from_slice(&destination);
    frame(ETHERTYPE_IPV4, &header)
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
fn an_unrecognised_ethertype_is_admitted_not_shed() {
    // The bias to admit, on the rule most likely to be wrong about real
    // hardware: a link that frames differently from the assumption here (a
    // Broadcom switch tag ahead of the Ethernet header, say) makes every
    // ethertype unrecognisable, and shedding on that would silently kill
    // the interface. The stack drops what it cannot consume anyway.
    for ethertype in [0x0842_u16, 0x88CC, 0x888E, 0x8137, 0x22F0] {
        assert!(
            ours().admit(&frame(ethertype, &[0u8; 40])),
            "ethertype {ethertype:#06x} must be admitted, not guessed about"
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
fn broadcast_and_multicast_are_admitted() {
    assert!(ours().admit(&ipv4_to(IPV4_BROADCAST.octets())));
    assert!(
        ours().admit(&ipv4_to(OUR_BROADCAST_V4)),
        "a subnet's directed broadcast reaches every host on it"
    );
    // DHCP offers arrive at the limited broadcast before an address exists,
    // and neighbour discovery lives entirely on multicast.
    assert!(ours().admit(&ipv4_to([224, 0, 0, 1])));
    let all_nodes = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
    assert!(ours().admit(&ipv6_to(all_nodes.octets())));
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
    let policy = RxFilterPolicy::new(&v4, &[OURS_V6]);
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
    let policy = RxFilterPolicy::new(&[(OURS_V4, OUR_BROADCAST_V4)], &v6);
    assert!(!policy.is_exhaustive());
    assert!(RxClassifier::new(policy).admit(&ipv6_to(THEIRS_V6)));
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
    let boot = RxClassifier::new(RxFilterPolicy::new(&[], &[OURS_V6]));
    assert!(boot.policy().is_exhaustive());
    assert!(
        boot.admit(&ipv4_to(THEIRS_V4)),
        "an IPv4 unicast must be admitted while the interface has no v4 address"
    );
    // The broadcast path a DHCP offer may instead take stays admitted too.
    assert!(boot.admit(&ipv4_to(IPV4_BROADCAST.octets())));
    // And an ARP request cannot be judged either, so it is admitted.
    assert!(boot.admit(&arp_frame(arp::OP_REQUEST, THEIRS_V4)));
}

#[test]
fn an_interface_with_no_ipv6_address_admits_ipv6_unicast() {
    let boot = RxClassifier::new(RxFilterPolicy::new(&[(OURS_V4, OUR_BROADCAST_V4)], &[]));
    assert!(boot.policy().is_exhaustive());
    assert!(boot.admit(&ipv6_to(THEIRS_V6)));
    // The family that *does* have an address still filters.
    assert!(!boot.admit(&ipv4_to(THEIRS_V4)));
    assert!(boot.admit(&ipv4_to(OURS_V4)));
}

#[test]
fn a_policy_with_no_address_at_all_filters_nothing_addressable() {
    let bare = RxClassifier::new(RxFilterPolicy::new(&[], &[]));
    assert!(bare.admit(&ipv4_to(THEIRS_V4)));
    assert!(bare.admit(&ipv6_to(THEIRS_V6)));
    assert!(bare.admit(&arp_frame(arp::OP_REQUEST, THEIRS_V4)));
}
