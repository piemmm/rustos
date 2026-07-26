//! Unit tests for the IPv6 codec and extension-header walk.

use super::*;
use crate::addr::IpAddr;
use crate::frag::{FragKey, PushOutcome, Reassembler, ReassemblyConfig};
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::time::Duration64;

const SRC: Ipv6Addr = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1);
const DST: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 2);

#[test]
fn header_write_then_parse_round_trips() {
    let written = Ipv6Header {
        hop_limit: 255,
        traffic_class: 0xA5,
        flow_label: 0x000F_F00F,
        ..Ipv6Header::new(SRC, DST, NEXT_HEADER_ICMPV6)
    };
    let mut out = [0u8; IPV6_HEADER_LEN + 4];
    let len = written.write(&mut out, 4).expect("fits");
    assert_eq!(len, IPV6_HEADER_LEN);
    out[IPV6_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);

    let (parsed, payload) = Ipv6Header::parse(&out).expect("parses");
    assert_eq!(parsed, written);
    assert_eq!(payload, &[1, 2, 3, 4]);
}

#[test]
fn header_parse_rejects_bad_version_truncation_and_overrun() {
    let mut out = [0u8; IPV6_HEADER_LEN];
    Ipv6Header::new(SRC, DST, NEXT_HEADER_NO_NEXT)
        .write(&mut out, 0)
        .expect("fits");
    assert!(Ipv6Header::parse(&out).is_some());
    let mut bad = out;
    bad[0] = 0x45;
    assert!(Ipv6Header::parse(&bad).is_none());
    assert!(Ipv6Header::parse(&out[..IPV6_HEADER_LEN - 1]).is_none());
    let mut overrun = out;
    overrun[4..6].copy_from_slice(&1u16.to_be_bytes());
    assert!(Ipv6Header::parse(&overrun).is_none());
}

#[test]
fn header_write_rejects_oversize_fields() {
    let mut out = [0u8; IPV6_HEADER_LEN];
    let header = Ipv6Header {
        flow_label: 0x0010_0000,
        ..Ipv6Header::new(SRC, DST, NEXT_HEADER_NO_NEXT)
    };
    assert!(header.write(&mut out, 0).is_none());
    let header = Ipv6Header::new(SRC, DST, NEXT_HEADER_NO_NEXT);
    assert!(header.write(&mut out, 65_536).is_none());
    assert!(header.write(&mut [0u8; 8], 0).is_none());
}

/// An options-shaped extension header: next, length, then `PadN` fill.
fn padded_ext(next: u8, words_beyond_first: u8) -> Vec<u8> {
    let total = 8 + usize::from(words_beyond_first) * 8;
    let mut header = vec![0u8; total];
    header[0] = next;
    header[1] = words_beyond_first;
    // Fill the options area with one PadN covering it entirely.
    header[2] = 1;
    header[3] = u8::try_from(total - 4).expect("fits");
    header
}

#[test]
fn walk_reaches_upper_layer_directly() {
    let payload = [0xAB; 5];
    match walk(NEXT_HEADER_ICMPV6, &payload, false) {
        Ok(WalkOutcome::Upper {
            protocol,
            payload: got,
            nh_offset,
        }) => {
            assert_eq!(protocol, NEXT_HEADER_ICMPV6);
            assert_eq!(got, &payload);
            // Named by byte 6 of the fixed header.
            assert_eq!(nh_offset, 6);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn walk_traverses_hop_by_hop_and_dest_opts() {
    let mut chain = padded_ext(NEXT_HEADER_DEST_OPTS, 0);
    chain.extend_from_slice(&padded_ext(NEXT_HEADER_ICMPV6, 1));
    chain.extend_from_slice(&[0xEE; 3]);
    match walk(NEXT_HEADER_HOP_BY_HOP, &chain, false) {
        Ok(WalkOutcome::Upper {
            protocol,
            payload,
            nh_offset,
        }) => {
            assert_eq!(protocol, NEXT_HEADER_ICMPV6);
            assert_eq!(payload, &[0xEE; 3]);
            // Named by byte 0 of the second (destination-options)
            // extension header, one 8-byte header past the fixed 40.
            assert_eq!(nh_offset, 48);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn walk_handles_no_next_header() {
    let chain = padded_ext(NEXT_HEADER_NO_NEXT, 0);
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Ok(WalkOutcome::Nothing)
    );
    assert_eq!(
        walk(NEXT_HEADER_NO_NEXT, &[], false),
        Ok(WalkOutcome::Nothing)
    );
}

#[test]
fn walk_stops_at_fragment_header() {
    // Fragment header: next = ICMPv6, offset 1504 bytes (188 units),
    // more set, id 0xDEADBEEF.
    let mut chain = vec![NEXT_HEADER_ICMPV6, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF];
    let offset_flags: u16 = (188 << 3) | 1;
    chain[2..4].copy_from_slice(&offset_flags.to_be_bytes());
    chain.extend_from_slice(&[0x11; 16]);
    match walk(NEXT_HEADER_FRAGMENT, &chain, false) {
        Ok(WalkOutcome::Fragment { info, payload }) => {
            assert_eq!(info.offset, 1504);
            assert!(info.more);
            assert_eq!(info.identification, 0xDEAD_BEEF);
            assert_eq!(info.next_header, NEXT_HEADER_ICMPV6);
            assert_eq!(payload, &[0x11; 16]);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn walk_rejects_hop_by_hop_out_of_position() {
    // Destination options first, then a hop-by-hop: the pointer names
    // the destination-options header's next-header byte (offset 40).
    let mut chain = padded_ext(NEXT_HEADER_HOP_BY_HOP, 0);
    chain.extend_from_slice(&padded_ext(NEXT_HEADER_ICMPV6, 0));
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Err(WalkRejection::ParamProblem {
            code: PARAM_PROBLEM_NEXT_HEADER,
            pointer: 40,
        })
    );
}

#[test]
fn walk_rejects_routing_with_segments_left() {
    let mut chain = padded_ext(NEXT_HEADER_ICMPV6, 0);
    chain[2] = 99; // routing type (unrecognised)
    chain[3] = 1; // segments left
    assert_eq!(
        walk(NEXT_HEADER_ROUTING, &chain, false),
        Err(WalkRejection::ParamProblem {
            code: PARAM_PROBLEM_HEADER_FIELD,
            pointer: 42,
        })
    );
}

#[test]
fn walk_skips_routing_with_no_segments_left() {
    let mut chain = padded_ext(NEXT_HEADER_ICMPV6, 0);
    chain[2] = 99; // routing type
    chain[3] = 0; // segments left
    chain.extend_from_slice(&[0x22; 2]);
    match walk(NEXT_HEADER_ROUTING, &chain, false) {
        Ok(WalkOutcome::Upper {
            protocol,
            payload,
            nh_offset,
        }) => {
            assert_eq!(protocol, NEXT_HEADER_ICMPV6);
            assert_eq!(payload, &[0x22; 2]);
            assert_eq!(nh_offset, 40);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn walk_rejects_truncated_extension_headers() {
    assert_eq!(
        walk(NEXT_HEADER_HOP_BY_HOP, &[], false),
        Err(WalkRejection::Drop)
    );
    // Claims one extra 8-byte word that is not present.
    let chain = [NEXT_HEADER_ICMPV6, 1, 1, 4, 0, 0, 0, 0];
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Err(WalkRejection::Drop)
    );
    // Fragment header shorter than its fixed 8 bytes.
    assert_eq!(
        walk(NEXT_HEADER_FRAGMENT, &[0; 7], false),
        Err(WalkRejection::Drop)
    );
}

#[test]
fn walk_bounds_the_chain_length() {
    // A chain of MAX_EXT_HEADERS + 1 destination-options headers.
    let mut chain = Vec::new();
    for _ in 0..=MAX_EXT_HEADERS {
        chain.extend_from_slice(&padded_ext(NEXT_HEADER_DEST_OPTS, 0));
    }
    chain.extend_from_slice(&padded_ext(NEXT_HEADER_ICMPV6, 0));
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Err(WalkRejection::Drop)
    );
}

/// A destination-options header carrying one option of `option_type`.
fn ext_with_option(next: u8, option_type: u8) -> Vec<u8> {
    let mut header = vec![0u8; 8];
    header[0] = next;
    header[1] = 0;
    header[2] = option_type;
    header[3] = 2; // option data length
    header[6] = 0; // Pad1
    header[7] = 0; // Pad1
    header
}

#[test]
fn walk_applies_option_dispositions() {
    // 00: skip and continue.
    let chain = ext_with_option(NEXT_HEADER_ICMPV6, 0x3F);
    assert!(matches!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Ok(WalkOutcome::Upper { .. })
    ));
    // 01: discard silently.
    let chain = ext_with_option(NEXT_HEADER_ICMPV6, 0x7F);
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Err(WalkRejection::Drop)
    );
    // 10: discard + Parameter Problem, multicast or not. The option
    // sits at packet offset 42 (fixed header + next/len bytes).
    let chain = ext_with_option(NEXT_HEADER_ICMPV6, 0xBF);
    for multicast in [false, true] {
        assert_eq!(
            walk(NEXT_HEADER_DEST_OPTS, &chain, multicast),
            Err(WalkRejection::ParamProblem {
                code: PARAM_PROBLEM_OPTION,
                pointer: 42,
            })
        );
    }
    // 11: Parameter Problem only when the destination is not multicast.
    let chain = ext_with_option(NEXT_HEADER_ICMPV6, 0xFF);
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Err(WalkRejection::ParamProblem {
            code: PARAM_PROBLEM_OPTION,
            pointer: 42,
        })
    );
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, true),
        Err(WalkRejection::Drop)
    );
}

#[test]
fn walk_rejects_option_length_overrun() {
    let mut chain = vec![0u8; 8];
    chain[0] = NEXT_HEADER_ICMPV6;
    chain[2] = 1; // PadN
    chain[3] = 200; // claims far more than the header holds
    assert_eq!(
        walk(NEXT_HEADER_DEST_OPTS, &chain, false),
        Err(WalkRejection::Drop)
    );
}

#[test]
fn hop_by_hop_router_alert_is_one_padded_unit() {
    // RFC 2711: an 8-octet Hop-by-Hop header naming the upper protocol,
    // carrying the Router Alert option (type 5, len 2, value 0 = MLD),
    // padded with a PadN to the 8-octet unit.
    let hbh = hop_by_hop_router_alert(NEXT_HEADER_ICMPV6);
    assert_eq!(hbh.len(), HBH_ROUTER_ALERT_LEN);
    assert_eq!(hbh[0], NEXT_HEADER_ICMPV6, "next header");
    assert_eq!(hbh[1], 0, "one 8-octet unit beyond the first");
    assert_eq!(&hbh[2..6], &[5, 2, 0, 0], "Router Alert option");
    assert_eq!(&hbh[6..8], &[1, 0], "PadN option");
}

// ---- Source fragmentation (RFC 8200 §4.5) ---------------------------

#[test]
fn fragment_pieces_are_contiguous_bounded_and_flagged() {
    let payload_len = 4000;
    let mtu = IPV6_MIN_MTU;
    let pieces = fragment(payload_len, mtu).expect("fragmentable");
    assert!(pieces.len() >= 2, "an oversize datagram is never one piece");
    let mut expected = 0;
    for (i, piece) in pieces.iter().enumerate() {
        assert_eq!(piece.offset, expected, "offset follows the previous end");
        assert_eq!(piece.payload_start, expected);
        let last = i == pieces.len() - 1;
        assert_eq!(piece.more, !last, "only the final piece clears More");
        let len = piece.payload_end - piece.payload_start;
        if !last {
            assert_eq!(len % 8, 0, "a non-final fragment is a multiple of 8");
        }
        assert!(
            IPV6_HEADER_LEN + FRAGMENT_HEADER_LEN + len <= mtu,
            "every fragment fits the link MTU"
        );
        expected = piece.payload_end;
    }
    assert_eq!(expected, payload_len, "pieces cover the whole payload");
}

#[test]
fn fragment_rejects_sub_minimum_mtu() {
    assert!(
        fragment(4000, IPV6_MIN_MTU - 1).is_none(),
        "an MTU below the IPv6 minimum fails closed"
    );
}

#[test]
fn fragment_rejects_a_datagram_beyond_the_offset_field() {
    // The 13-bit offset field (×8) cannot place a fragment past 65528, so
    // a datagram this large cannot be fragmented at all.
    assert!(fragment(100_000, IPV6_MIN_MTU).is_none());
}

#[test]
fn write_fragment_header_rejects_unaligned_or_undersized() {
    let mut out = [0u8; FRAGMENT_HEADER_LEN];
    assert!(
        write_fragment_header(&mut out, NEXT_HEADER_ICMPV6, 7, false, 1).is_none(),
        "an offset that is not a multiple of 8 fails closed"
    );
    assert!(
        write_fragment_header(
            &mut out[..FRAGMENT_HEADER_LEN - 1],
            NEXT_HEADER_ICMPV6,
            0,
            false,
            1
        )
        .is_none(),
        "a buffer too small for the header fails closed"
    );
}

#[test]
fn fragment_then_walk_then_reassemble_round_trips() {
    // Fragment a datagram, serialise each piece's Fragment header, walk it
    // as a receiver would, and reassemble: the original payload returns.
    let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let mtu = IPV6_MIN_MTU;
    let pieces = fragment(payload.len(), mtu).expect("fragmentable");
    assert!(pieces.len() >= 2);
    let identification = 0xDEAD_BEEF;
    let mut reassembler = Reassembler::new(ReassemblyConfig::default());
    let key = FragKey {
        source: IpAddr::V6(SRC),
        destination: IpAddr::V6(DST),
        identification,
        protocol: 0,
    };
    let mut completed = None;
    for piece in &pieces {
        let data = &payload[piece.payload_start..piece.payload_end];
        let mut frag_payload = vec![0u8; FRAGMENT_HEADER_LEN + data.len()];
        write_fragment_header(
            &mut frag_payload,
            NEXT_HEADER_ICMPV6,
            piece.offset,
            piece.more,
            identification,
        )
        .expect("writes");
        frag_payload[FRAGMENT_HEADER_LEN..].copy_from_slice(data);
        let WalkOutcome::Fragment {
            info,
            payload: piece_bytes,
        } = walk(NEXT_HEADER_FRAGMENT, &frag_payload, false).expect("walks")
        else {
            panic!("expected a fragment outcome");
        };
        assert_eq!(info.identification, identification);
        assert_eq!(usize::from(info.offset), piece.offset);
        assert_eq!(info.more, piece.more);
        assert_eq!(info.next_header, NEXT_HEADER_ICMPV6);
        match reassembler.push(
            key,
            usize::from(info.offset),
            info.more,
            piece_bytes,
            Duration64::from_secs(0),
        ) {
            PushOutcome::Complete(bytes) => completed = Some(bytes),
            PushOutcome::Pending => {}
            PushOutcome::Rejected(reason) => panic!("reassembly rejected: {reason:?}"),
        }
    }
    assert_eq!(completed.expect("reassembled"), payload);
}

#[test]
fn fragment_rejects_an_empty_datagram() {
    assert!(
        fragment(0, IPV6_MIN_MTU).is_none(),
        "there is nothing to fragment in a zero-length datagram"
    );
}
