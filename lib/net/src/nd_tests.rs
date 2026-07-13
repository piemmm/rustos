//! Unit tests for the Neighbour Discovery codecs and table glue.

use super::*;
use crate::neigh::{NeighborConfig, NeighborState};
use alloc::vec;

const MAC: MacAddress = MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]);
const TARGET: Ipv6Addr = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1);
const PEER: Ipv6Addr = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 2);

fn reparse(message: &NdMessage) -> NdMessage {
    let mut body = [0u8; 64];
    let len = message.write_body(&mut body).expect("host message writes");
    NdMessage::parse(message.message_type(), 0, ND_HOP_LIMIT, false, &body[..len])
        .expect("written message parses")
}

#[test]
fn host_messages_round_trip() {
    for source_ll in [None, Some(MAC)] {
        let rs = NdMessage::RouterSolicitation { source_ll };
        assert_eq!(reparse(&rs), rs);
        let ns = NdMessage::NeighborSolicitation {
            target: TARGET,
            source_ll,
        };
        assert_eq!(reparse(&ns), ns);
        let na = NdMessage::NeighborAdvertisement {
            router: false,
            solicited: true,
            override_flag: true,
            target: TARGET,
            target_ll: source_ll,
        };
        assert_eq!(reparse(&na), na);
    }
}

#[test]
fn router_messages_are_not_emitted() {
    let mut out = [0u8; 64];
    let ra = NdMessage::RouterAdvertisement {
        cur_hop_limit: 64,
        managed: false,
        other: false,
        router_lifetime: 1800,
        reachable_time: 0,
        retrans_timer: 0,
        source_ll: None,
        mtu: None,
        prefixes: vec![],
    };
    assert!(ra.write_body(&mut out).is_none());
    let redirect = NdMessage::Redirect {
        target: TARGET,
        destination: PEER,
        target_ll: None,
    };
    assert!(redirect.write_body(&mut out).is_none());
}

#[test]
fn parse_enforces_hop_limit_and_code() {
    let ns = NdMessage::NeighborSolicitation {
        target: TARGET,
        source_ll: Some(MAC),
    };
    let mut body = [0u8; 64];
    let len = ns.write_body(&mut body).expect("writes");
    assert!(NdMessage::parse(
        TYPE_NEIGHBOR_SOLICITATION,
        0,
        ND_HOP_LIMIT - 1,
        false,
        &body[..len]
    )
    .is_none());
    assert!(NdMessage::parse(
        TYPE_NEIGHBOR_SOLICITATION,
        1,
        ND_HOP_LIMIT,
        false,
        &body[..len]
    )
    .is_none());
    assert!(NdMessage::parse(99, 0, ND_HOP_LIMIT, false, &body[..len]).is_none());
}

#[test]
fn parse_rejects_multicast_targets() {
    let multicast = Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 0, 1);
    let mut body = [0u8; 20];
    body[4..20].copy_from_slice(&multicast.octets());
    assert!(NdMessage::parse(TYPE_NEIGHBOR_SOLICITATION, 0, ND_HOP_LIMIT, false, &body).is_none());
    assert!(NdMessage::parse(TYPE_NEIGHBOR_ADVERTISEMENT, 0, ND_HOP_LIMIT, false, &body).is_none());
}

#[test]
fn parse_rejects_solicited_advertisement_to_multicast() {
    let na = NdMessage::NeighborAdvertisement {
        router: false,
        solicited: true,
        override_flag: false,
        target: TARGET,
        target_ll: Some(MAC),
    };
    let mut body = [0u8; 64];
    let len = na.write_body(&mut body).expect("writes");
    assert!(NdMessage::parse(
        TYPE_NEIGHBOR_ADVERTISEMENT,
        0,
        ND_HOP_LIMIT,
        true,
        &body[..len]
    )
    .is_none());
    // An unsolicited advertisement to multicast is fine.
    let unsolicited = NdMessage::NeighborAdvertisement {
        router: false,
        solicited: false,
        override_flag: false,
        target: TARGET,
        target_ll: Some(MAC),
    };
    let len = unsolicited.write_body(&mut body).expect("writes");
    assert!(NdMessage::parse(
        TYPE_NEIGHBOR_ADVERTISEMENT,
        0,
        ND_HOP_LIMIT,
        true,
        &body[..len]
    )
    .is_some());
}

#[test]
fn parse_rejects_truncated_bodies() {
    assert!(NdMessage::parse(TYPE_ROUTER_SOLICITATION, 0, ND_HOP_LIMIT, false, &[0; 3]).is_none());
    assert!(
        NdMessage::parse(TYPE_ROUTER_ADVERTISEMENT, 0, ND_HOP_LIMIT, false, &[0; 11]).is_none()
    );
    assert!(
        NdMessage::parse(TYPE_NEIGHBOR_SOLICITATION, 0, ND_HOP_LIMIT, false, &[0; 19]).is_none()
    );
    assert!(NdMessage::parse(TYPE_REDIRECT, 0, ND_HOP_LIMIT, false, &[0; 35]).is_none());
}

/// A full Router Advertisement body: fixed part, source link-layer,
/// MTU, and one prefix-information option.
fn ra_body() -> alloc::vec::Vec<u8> {
    let mut body = vec![0u8; 12];
    body[0] = 64; // cur hop limit
    body[1] = 0x80; // managed
    body[2..4].copy_from_slice(&1800u16.to_be_bytes());
    body[4..8].copy_from_slice(&30_000u32.to_be_bytes());
    body[8..12].copy_from_slice(&1_000u32.to_be_bytes());
    // Source link-layer option.
    body.extend_from_slice(&[1, 1]);
    body.extend_from_slice(&MAC.0);
    // MTU option.
    body.extend_from_slice(&[5, 1, 0, 0]);
    body.extend_from_slice(&1500u32.to_be_bytes());
    // Prefix information: 2001:db8::/64, on-link + autonomous.
    let mut prefix = vec![3u8, 4, 64, 0xC0];
    prefix.extend_from_slice(&86_400u32.to_be_bytes());
    prefix.extend_from_slice(&14_400u32.to_be_bytes());
    prefix.extend_from_slice(&0u32.to_be_bytes());
    prefix.extend_from_slice(&Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0).octets());
    body.extend_from_slice(&prefix);
    body
}

#[test]
fn router_advertisement_parses_options() {
    let body = ra_body();
    let parsed =
        NdMessage::parse(TYPE_ROUTER_ADVERTISEMENT, 0, ND_HOP_LIMIT, false, &body).expect("parses");
    let NdMessage::RouterAdvertisement {
        cur_hop_limit,
        managed,
        other,
        router_lifetime,
        reachable_time,
        retrans_timer,
        source_ll,
        mtu,
        prefixes,
    } = parsed
    else {
        panic!("wrong message kind");
    };
    assert_eq!(cur_hop_limit, 64);
    assert!(managed);
    assert!(!other);
    assert_eq!(router_lifetime, 1800);
    assert_eq!(reachable_time, 30_000);
    assert_eq!(retrans_timer, 1_000);
    assert_eq!(source_ll, Some(MAC));
    assert_eq!(mtu, Some(1500));
    assert_eq!(
        prefixes,
        vec![PrefixInformation {
            prefix: Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0),
            prefix_len: 64,
            on_link: true,
            autonomous: true,
            valid_lifetime: 86_400,
            preferred_lifetime: 14_400,
        }]
    );
}

#[test]
fn redirect_parses_target_and_destination() {
    let mut body = vec![0u8; 36];
    body[4..20].copy_from_slice(&TARGET.octets());
    body[20..36].copy_from_slice(&PEER.octets());
    body.extend_from_slice(&[2, 1]);
    body.extend_from_slice(&MAC.0);
    let parsed = NdMessage::parse(TYPE_REDIRECT, 0, ND_HOP_LIMIT, false, &body).expect("parses");
    assert_eq!(
        parsed,
        NdMessage::Redirect {
            target: TARGET,
            destination: PEER,
            target_ll: Some(MAC),
        }
    );
}

#[test]
fn option_parsing_fails_closed() {
    // Zero-length option.
    let mut body = vec![0u8; 20];
    body[4..20].copy_from_slice(&TARGET.octets());
    body.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    assert!(NdMessage::parse(TYPE_NEIGHBOR_SOLICITATION, 0, ND_HOP_LIMIT, false, &body).is_none());
    // Length overrunning the message.
    let mut body = vec![0u8; 20];
    body[4..20].copy_from_slice(&TARGET.octets());
    body.extend_from_slice(&[1, 4, 0, 0, 0, 0, 0, 0]);
    assert!(NdMessage::parse(TYPE_NEIGHBOR_SOLICITATION, 0, ND_HOP_LIMIT, false, &body).is_none());
    // A recognised option with the wrong length.
    let mut body = vec![0u8; 20];
    body[4..20].copy_from_slice(&TARGET.octets());
    body.extend_from_slice(&[1, 2]);
    body.extend_from_slice(&[0u8; 14]);
    assert!(NdMessage::parse(TYPE_NEIGHBOR_SOLICITATION, 0, ND_HOP_LIMIT, false, &body).is_none());
}

#[test]
fn option_count_is_bounded() {
    let mut body = vec![0u8; 20];
    body[4..20].copy_from_slice(&TARGET.octets());
    // MAX_ND_OPTIONS + 1 unknown (skipped) options still exceed the
    // count bound.
    for _ in 0..=MAX_ND_OPTIONS {
        body.extend_from_slice(&[200, 1, 0, 0, 0, 0, 0, 0]);
    }
    assert!(NdMessage::parse(TYPE_NEIGHBOR_SOLICITATION, 0, ND_HOP_LIMIT, false, &body).is_none());
}

#[test]
fn unknown_options_are_skipped() {
    let mut body = vec![0u8; 20];
    body[4..20].copy_from_slice(&TARGET.octets());
    body.extend_from_slice(&[200, 1, 0, 0, 0, 0, 0, 0]);
    body.extend_from_slice(&[1, 1]);
    body.extend_from_slice(&MAC.0);
    let parsed = NdMessage::parse(TYPE_NEIGHBOR_SOLICITATION, 0, ND_HOP_LIMIT, false, &body)
        .expect("parses");
    assert_eq!(
        parsed,
        NdMessage::NeighborSolicitation {
            target: TARGET,
            source_ll: Some(MAC),
        }
    );
}

#[test]
fn apply_solicitation_learns_sender_binding() {
    let mut table = NeighborTable::new(4, NeighborConfig::default());
    let now = Duration64::from_secs(1);
    let ns = NdMessage::NeighborSolicitation {
        target: TARGET,
        source_ll: Some(MAC),
    };
    apply_neighbor_solicitation(&ns, PEER, &mut table, now);
    assert_eq!(
        table.entry(IpAddr::V6(PEER)),
        Some((NeighborState::Stale, Some(MAC)))
    );
    // Duplicate address detection (unspecified source) creates nothing.
    let mut fresh = NeighborTable::new(4, NeighborConfig::default());
    apply_neighbor_solicitation(&ns, Ipv6Addr::UNSPECIFIED, &mut fresh, now);
    assert!(fresh.is_empty());
}

#[test]
fn apply_advertisement_confirms_resolution() {
    let mut table = NeighborTable::new(4, NeighborConfig::default());
    let now = Duration64::from_secs(1);
    // The host is resolving TARGET (an Incomplete entry exists).
    table.lookup(IpAddr::V6(TARGET), now);
    let na = NdMessage::NeighborAdvertisement {
        router: false,
        solicited: true,
        override_flag: false,
        target: TARGET,
        target_ll: Some(MAC),
    };
    apply_neighbor_advertisement(&na, &mut table, now);
    assert_eq!(
        table.entry(IpAddr::V6(TARGET)),
        Some((NeighborState::Reachable, Some(MAC)))
    );
    // An unsolicited advertisement about an unknown address creates
    // nothing.
    let mut fresh = NeighborTable::new(4, NeighborConfig::default());
    apply_neighbor_advertisement(&na, &mut fresh, now);
    assert!(fresh.is_empty());
}

#[test]
fn apply_redirect_learns_new_first_hop() {
    let mut table = NeighborTable::new(4, NeighborConfig::default());
    let now = Duration64::from_secs(1);
    let redirect = NdMessage::Redirect {
        target: TARGET,
        destination: PEER,
        target_ll: Some(MAC),
    };
    apply_redirect(&redirect, &mut table, now);
    assert_eq!(
        table.entry(IpAddr::V6(TARGET)),
        Some((NeighborState::Stale, Some(MAC)))
    );
}
