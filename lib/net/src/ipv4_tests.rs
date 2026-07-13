//! Unit tests for the IPv4 codec and emit-side fragmentation.

use super::*;
use alloc::vec;

const SRC: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

fn header() -> Ipv4Header {
    Ipv4Header::new(SRC, DST, PROTOCOL_ICMP)
}

#[test]
fn write_then_parse_round_trips() {
    let mut out = [0u8; IPV4_HEADER_LEN + 4];
    let written = Ipv4Header {
        identification: 0x4242,
        ttl: 17,
        dont_fragment: true,
        ..header()
    };
    let len = written.write(&mut out, 4).expect("fits");
    assert_eq!(len, IPV4_HEADER_LEN);
    out[IPV4_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);

    let (parsed, options, payload) = Ipv4Header::parse(&out).expect("parses");
    assert_eq!(parsed, written);
    assert!(options.is_empty());
    assert_eq!(payload, &[1, 2, 3, 4]);
}

#[test]
fn fragment_flags_and_offset_round_trip() {
    let mut out = [0u8; IPV4_HEADER_LEN];
    let written = Ipv4Header {
        more_fragments: true,
        fragment_offset: 1480,
        ..header()
    };
    written.write(&mut out, 0).expect("fits");
    let (parsed, _, _) = Ipv4Header::parse(&out).expect("parses");
    assert!(parsed.more_fragments);
    assert_eq!(parsed.fragment_offset, 1480);
    assert!(parsed.is_fragment());
}

#[test]
fn written_header_has_valid_checksum() {
    let mut out = [0u8; IPV4_HEADER_LEN];
    header().write(&mut out, 0).expect("fits");
    // The one's-complement sum of a header including its checksum
    // field is zero.
    assert_eq!(internet_checksum(&out), 0);
}

#[test]
fn parse_tolerates_options() {
    // 24-byte header (IHL = 6) carrying one NOP-padded options word.
    let mut bytes = vec![0u8; 24 + 2];
    header().write(&mut bytes[..], 0).expect("fits");
    bytes[0] = 0x46;
    let total = 26u16;
    bytes[2..4].copy_from_slice(&total.to_be_bytes());
    bytes[20..24].copy_from_slice(&[0x01, 0x01, 0x01, 0x00]); // NOP NOP NOP EOL
    bytes[24..26].copy_from_slice(&[0xAA, 0xBB]);
    bytes[10..12].copy_from_slice(&0u16.to_be_bytes());
    let checksum = internet_checksum(&bytes[..24]);
    bytes[10..12].copy_from_slice(&checksum.to_be_bytes());

    let (parsed, options, payload) = Ipv4Header::parse(&bytes).expect("parses");
    assert_eq!(parsed.source, SRC);
    assert_eq!(options, &[0x01, 0x01, 0x01, 0x00]);
    assert_eq!(payload, &[0xAA, 0xBB]);
}

#[test]
fn parse_rejects_wrong_version_and_short_ihl() {
    let mut out = [0u8; IPV4_HEADER_LEN];
    header().write(&mut out, 0).expect("fits");
    let reseal = |bytes: &mut [u8]| {
        bytes[10..12].copy_from_slice(&0u16.to_be_bytes());
        let checksum = internet_checksum(&bytes[..IPV4_HEADER_LEN]);
        bytes[10..12].copy_from_slice(&checksum.to_be_bytes());
    };
    out[0] = 0x65; // version 6
    reseal(&mut out);
    assert!(Ipv4Header::parse(&out).is_none());
    out[0] = 0x44; // IHL = 4 words: below the minimum header
    reseal(&mut out);
    assert!(Ipv4Header::parse(&out).is_none());
}

#[test]
fn parse_rejects_ihl_beyond_input() {
    let mut out = [0u8; IPV4_HEADER_LEN];
    header().write(&mut out, 0).expect("fits");
    out[0] = 0x4F; // IHL = 15 words = 60 bytes, but only 20 supplied
    assert!(Ipv4Header::parse(&out).is_none());
}

#[test]
fn parse_rejects_total_length_overrun_and_underrun() {
    let mut out = [0u8; IPV4_HEADER_LEN];
    let reseal = |bytes: &mut [u8]| {
        bytes[10..12].copy_from_slice(&0u16.to_be_bytes());
        let checksum = internet_checksum(&bytes[..IPV4_HEADER_LEN]);
        bytes[10..12].copy_from_slice(&checksum.to_be_bytes());
    };
    header().write(&mut out, 0).expect("fits");
    out[2..4].copy_from_slice(&100u16.to_be_bytes());
    reseal(&mut out);
    assert!(Ipv4Header::parse(&out).is_none());
    out[2..4].copy_from_slice(&10u16.to_be_bytes());
    reseal(&mut out);
    assert!(Ipv4Header::parse(&out).is_none());
}

#[test]
fn parse_rejects_reserved_flag() {
    let mut out = [0u8; IPV4_HEADER_LEN];
    header().write(&mut out, 0).expect("fits");
    out[6] |= 0x80;
    out[10..12].copy_from_slice(&0u16.to_be_bytes());
    let checksum = internet_checksum(&out);
    out[10..12].copy_from_slice(&checksum.to_be_bytes());
    assert!(Ipv4Header::parse(&out).is_none());
}

#[test]
fn parse_rejects_truncated_and_corrupted() {
    assert!(Ipv4Header::parse(&[]).is_none());
    assert!(Ipv4Header::parse(&[0u8; IPV4_HEADER_LEN - 1]).is_none());
    let mut out = [0u8; IPV4_HEADER_LEN];
    header().write(&mut out, 0).expect("fits");
    // Corrupt one address bit without touching the checksum field: the
    // header no longer verifies and must be discarded.
    out[12] ^= 0x01;
    assert!(Ipv4Header::parse(&out).is_none());
}

#[test]
fn write_rejects_misaligned_offsets() {
    let mut out = [0u8; IPV4_HEADER_LEN];
    let misaligned = Ipv4Header {
        fragment_offset: 4,
        ..header()
    };
    assert!(misaligned.write(&mut out, 0).is_none());
}

#[test]
fn fragment_returns_single_part_when_datagram_fits() {
    let parts = fragment(header(), 100, 1500).expect("fits whole");
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].payload_start, 0);
    assert_eq!(parts[0].payload_end, 100);
    assert!(!parts[0].header.more_fragments);
}

#[test]
fn fragment_splits_on_eight_byte_boundaries() {
    // MTU 100 → 80-byte chunks (100 - 20, already a multiple of 8).
    let parts = fragment(header(), 200, 100).expect("fragments");
    assert_eq!(parts.len(), 3);
    assert_eq!(
        parts
            .iter()
            .map(|p| (p.payload_start, p.payload_end, p.header.more_fragments))
            .collect::<alloc::vec::Vec<_>>(),
        vec![(0, 80, true), (80, 160, true), (160, 200, false)]
    );
    for part in &parts {
        assert_eq!(part.header.fragment_offset as usize, part.payload_start);
        assert_eq!(part.header.identification, 0);
    }
}

#[test]
fn fragment_covers_payload_exactly_once() {
    for (payload_len, mtu) in [(1, 68), (48, 68), (65_000, 576), (9_000, 1500)] {
        let parts = fragment(header(), payload_len, mtu).expect("fragments");
        let mut expected_start = 0usize;
        for (i, part) in parts.iter().enumerate() {
            assert_eq!(part.payload_start, expected_start);
            assert!(part.payload_end > part.payload_start);
            assert!(IPV4_HEADER_LEN + (part.payload_end - part.payload_start) <= mtu);
            assert_eq!(part.header.more_fragments, i + 1 < parts.len());
            expected_start = part.payload_end;
        }
        assert_eq!(expected_start, payload_len);
    }
}

#[test]
fn fragment_fails_closed() {
    // Don't-Fragment set and too large.
    let df = Ipv4Header {
        dont_fragment: true,
        ..header()
    };
    assert!(fragment(df, 5000, 1500).is_none());
    // MTU below the IPv4 floor.
    assert!(fragment(header(), 5000, IPV4_MIN_MTU - 1).is_none());
    // Already a fragment.
    let frag = Ipv4Header {
        more_fragments: true,
        ..header()
    };
    assert!(fragment(frag, 5000, 1500).is_none());
    // Total length overflow.
    assert!(fragment(header(), 65_536, 1500).is_none());
}

#[test]
fn router_alert_header_carries_the_option() {
    // A 24-byte header (IHL = 6) carrying the RFC 2113 Router Alert
    // option, as IGMP membership messages require.
    let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
    let mut out = [0u8; IPV4_HEADER_LEN + 4 + 4];
    let written = header()
        .write_with_router_alert(&mut out, payload.len())
        .expect("router-alert header fits");
    assert_eq!(written, IPV4_HEADER_LEN + 4);
    out[written..].copy_from_slice(&payload);
    // IHL field says six 32-bit words.
    assert_eq!(out[0] & 0x0F, 6);
    // The option bytes are the Router Alert option (type 148, len 4).
    let (parsed, options, body) = Ipv4Header::parse(&out).expect("parses");
    assert_eq!(parsed.source, SRC);
    assert_eq!(parsed.destination, DST);
    assert_eq!(options, &[0x94, 0x04, 0x00, 0x00]);
    assert_eq!(body, &payload);
    // The header checksum still verifies over the whole 24-byte header.
    assert_eq!(internet_checksum(&out[..written]), 0);
}
