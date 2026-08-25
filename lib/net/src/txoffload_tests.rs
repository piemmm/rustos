//! Unit tests for the driver-side TCP segmentation engine.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::checksum::Pseudo;
use crate::tcp::PROTOCOL_TCP;

const SRC_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const DST_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const IDENT: u16 = 0xBEEF;
const SEQ: u32 = 0x1000_0000;

fn src_v6() -> Ipv6Addr {
    Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)
}

fn dst_v6() -> Ipv6Addr {
    Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2)
}

/// The payload byte at stream position `i`, so a segment's bytes identify
/// where in the stream they came from.
fn payload_byte(i: usize) -> u8 {
    u8::try_from(i % 251).unwrap_or(0)
}

/// One super-frame: Ethernet + IP + TCP (with `options_len` option bytes)
/// and `payload_len` payload bytes, with the length-zero pseudo-header
/// partial in the TCP checksum field exactly as the stack leaves it.
fn super_frame(
    ipv6: bool,
    payload_len: usize,
    options_len: usize,
    flags: TcpFlags,
    gso_size: u16,
) -> (Vec<u8>, FrameOffload) {
    let ip_len = if ipv6 {
        IPV6_HEADER_LEN
    } else {
        IPV4_HEADER_LEN
    };
    let tcp_len = TCP_HEADER_LEN + options_len;
    let hdr_len = ETHERNET_HEADER_LEN + ip_len + tcp_len;
    let mut frame = vec![0u8; hdr_len + payload_len];

    frame[0..6].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x02]);
    frame[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x01]);
    let ip = ETHERNET_HEADER_LEN;
    let tcp = ip + ip_len;
    let pseudo = if ipv6 {
        frame[12..14].copy_from_slice(&crate::eth::ETHERTYPE_IPV6.to_be_bytes());
        frame[ip] = 0x60;
        let payload = u16::try_from(tcp_len + payload_len).expect("bounded");
        frame[ip + 4..ip + 6].copy_from_slice(&payload.to_be_bytes());
        frame[ip + 6] = PROTOCOL_TCP;
        frame[ip + 7] = 64;
        frame[ip + 8..ip + 24].copy_from_slice(&src_v6().octets());
        frame[ip + 24..ip + 40].copy_from_slice(&dst_v6().octets());
        Pseudo::V6 {
            source: src_v6(),
            destination: dst_v6(),
        }
    } else {
        frame[12..14].copy_from_slice(&crate::eth::ETHERTYPE_IPV4.to_be_bytes());
        frame[ip] = 0x45;
        let total = u16::try_from(ip_len + tcp_len + payload_len).expect("bounded");
        frame[ip + 2..ip + 4].copy_from_slice(&total.to_be_bytes());
        frame[ip + 4..ip + 6].copy_from_slice(&IDENT.to_be_bytes());
        frame[ip + 6..ip + 8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF
        frame[ip + 8] = 64;
        frame[ip + 9] = PROTOCOL_TCP;
        frame[ip + 12..ip + 16].copy_from_slice(&SRC_V4.octets());
        frame[ip + 16..ip + 20].copy_from_slice(&DST_V4.octets());
        Pseudo::V4 {
            source: SRC_V4,
            destination: DST_V4,
        }
    };

    frame[tcp..tcp + 2].copy_from_slice(&1234u16.to_be_bytes());
    frame[tcp + 2..tcp + 4].copy_from_slice(&80u16.to_be_bytes());
    frame[tcp + 4..tcp + 8].copy_from_slice(&SEQ.to_be_bytes());
    frame[tcp + 12] = u8::try_from(tcp_len / 4).expect("bounded") << 4;
    frame[tcp + 13] = flags.bits();
    frame[tcp + 14..tcp + 16].copy_from_slice(&0xFFFFu16.to_be_bytes());
    // Option bytes: a NOP run, so the header is well-formed at any length.
    for byte in &mut frame[tcp + TCP_HEADER_LEN..tcp + tcp_len] {
        *byte = 1;
    }
    // The GSO partial: the pseudo-header folded with a *zero* upper-layer
    // length, which is what the stack writes for a super-segment.
    let partial = pseudo.seed(PROTOCOL_TCP, 0).partial();
    frame[tcp + CHECKSUM_OFFSET..tcp + CHECKSUM_OFFSET + 2].copy_from_slice(&partial.to_be_bytes());
    for (i, byte) in frame[hdr_len..].iter_mut().enumerate() {
        *byte = payload_byte(i);
    }

    let offload = FrameOffload::TxSegment {
        csum_start: u16::try_from(tcp).expect("bounded"),
        csum_offset: u16::try_from(CHECKSUM_OFFSET).expect("bounded"),
        gso_size,
        hdr_len: u16::try_from(hdr_len).expect("bounded"),
        ipv6,
    };
    (frame, offload)
}

/// Collect every segment a super-frame produces, each with the transport
/// checksum *completed* the way a device's engine completes it: fold the
/// bytes from `csum_start` to the end (the partial in the field included)
/// and store the complement.
fn segments(frame: &[u8], offload: FrameOffload) -> Vec<Vec<u8>> {
    let mut segmenter = TcpSegmenter::new(frame, offload).expect("a well-formed super-frame");
    let csum_start = match segmenter.checksum_offload() {
        FrameOffload::TxChecksum { csum_start, .. } => usize::from(csum_start),
        other => panic!("expected a partial-checksum offload, got {other:?}"),
    };
    let mut out = vec![0u8; segmenter.max_segment_len()];
    let mut collected = Vec::new();
    while let Some(len) = segmenter
        .next_segment(&mut out)
        .expect("buffer is large enough")
    {
        let mut segment = out[..len].to_vec();
        let complete = internet_checksum(&segment[csum_start..]);
        segment[csum_start + CHECKSUM_OFFSET..csum_start + CHECKSUM_OFFSET + 2]
            .copy_from_slice(&complete.to_be_bytes());
        collected.push(segment);
    }
    collected
}

/// Independently verify one segment: the IPv4 header checksum folds to
/// zero, and the transport checksum folds to zero against a pseudo-header
/// carrying this segment's own length.
fn assert_segment_valid(segment: &[u8], ipv6: bool) {
    let ip = ETHERNET_HEADER_LEN;
    let (pseudo, ip_len) = if ipv6 {
        (
            Pseudo::V6 {
                source: src_v6(),
                destination: dst_v6(),
            },
            IPV6_HEADER_LEN,
        )
    } else {
        let ihl = usize::from(segment[ip] & 0x0F) * 4;
        assert_eq!(
            internet_checksum(&segment[ip..ip + ihl]),
            0,
            "the IPv4 header checksum must verify"
        );
        (
            Pseudo::V4 {
                source: SRC_V4,
                destination: DST_V4,
            },
            ihl,
        )
    };
    let tcp = ip + ip_len;
    let tcp_len = u16::try_from(segment.len() - tcp).expect("bounded");
    let mut sum = pseudo.seed(PROTOCOL_TCP, tcp_len);
    sum.push(&segment[tcp..]);
    assert_eq!(sum.finish(), 0, "the TCP checksum must verify");
}

/// The declared length field of a segment's IP header.
fn ip_length_field(segment: &[u8], ipv6: bool) -> usize {
    let ip = ETHERNET_HEADER_LEN;
    let off = if ipv6 {
        IPV6_PAYLOAD_LENGTH
    } else {
        IPV4_TOTAL_LENGTH
    };
    usize::from(u16::from_be_bytes([
        segment[ip + off],
        segment[ip + off + 1],
    ]))
}

fn seq_of(segment: &[u8], ipv6: bool) -> u32 {
    let tcp = ETHERNET_HEADER_LEN
        + if ipv6 {
            IPV6_HEADER_LEN
        } else {
            IPV4_HEADER_LEN
        };
    u32::from_be_bytes([
        segment[tcp + 4],
        segment[tcp + 5],
        segment[tcp + 6],
        segment[tcp + 7],
    ])
}

fn flags_of(segment: &[u8], ipv6: bool) -> TcpFlags {
    let tcp = ETHERNET_HEADER_LEN
        + if ipv6 {
            IPV6_HEADER_LEN
        } else {
            IPV4_HEADER_LEN
        };
    TcpFlags::from_bits(segment[tcp + TCP_FLAGS])
}

// -- Segmentation --------------------------------------------------------

#[test]
fn an_ipv4_super_frame_splits_into_mss_sized_segments() {
    let flags = TcpFlags::from_bits(TcpFlags::ACK.bits() | TcpFlags::PSH.bits());
    let (frame, offload) = super_frame(false, 3_500, 0, flags, 1_460);
    let out = segments(&frame, offload);
    assert_eq!(
        out.len(),
        3,
        "3500 bytes at an MSS of 1460 is three segments"
    );

    let header = ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + TCP_HEADER_LEN;
    let expected = [1_460usize, 1_460, 580];
    let mut stream = 0usize;
    for (index, segment) in out.iter().enumerate() {
        let payload = expected[index];
        assert_eq!(segment.len(), header + payload);
        assert_eq!(
            ip_length_field(segment, false),
            IPV4_HEADER_LEN + TCP_HEADER_LEN + payload
        );
        assert_eq!(
            seq_of(segment, false),
            SEQ.wrapping_add(u32::try_from(stream).expect("bounded"))
        );
        assert_segment_valid(segment, false);
        for (i, &byte) in segment[header..].iter().enumerate() {
            assert_eq!(byte, payload_byte(stream + i), "payload runs in order");
        }
        stream += payload;
    }
    assert_eq!(stream, 3_500, "every payload byte is emitted exactly once");
}

#[test]
fn an_ipv6_super_frame_splits_and_carries_the_payload_length() {
    let (frame, offload) = super_frame(true, 3_000, 0, TcpFlags::ACK, 1_440);
    let out = segments(&frame, offload);
    assert_eq!(out.len(), 3);
    let expected = [1_440usize, 1_440, 120];
    for (index, segment) in out.iter().enumerate() {
        assert_eq!(
            ip_length_field(segment, true),
            TCP_HEADER_LEN + expected[index],
            "the IPv6 payload length excludes the fixed header"
        );
        assert_segment_valid(segment, true);
    }
}

#[test]
fn tcp_options_are_replicated_in_every_segment() {
    let (frame, offload) = super_frame(false, 2_000, 12, TcpFlags::ACK, 1_448);
    let out = segments(&frame, offload);
    assert_eq!(out.len(), 2);
    let tcp = ETHERNET_HEADER_LEN + IPV4_HEADER_LEN;
    for segment in &out {
        assert_eq!(
            usize::from(segment[tcp + TCP_DATA_OFFSET] >> 4) * 4,
            TCP_HEADER_LEN + 12,
            "the data offset still names the option-bearing header"
        );
        assert_eq!(
            &segment[tcp + TCP_HEADER_LEN..tcp + TCP_HEADER_LEN + 12],
            &[1u8; 12]
        );
        assert_segment_valid(segment, false);
    }
}

#[test]
fn fin_and_push_ride_only_the_last_segment() {
    let flags = TcpFlags::from_bits(
        TcpFlags::ACK.bits() | TcpFlags::PSH.bits() | TcpFlags::FIN.bits() | TcpFlags::CWR.bits(),
    );
    let (frame, offload) = super_frame(false, 3_000, 0, flags, 1_000);
    let out = segments(&frame, offload);
    assert_eq!(out.len(), 3);
    for (index, segment) in out.iter().enumerate() {
        let seen = flags_of(segment, false);
        assert!(seen.contains(TcpFlags::ACK), "ACK rides every segment");
        let last = index + 1 == out.len();
        assert_eq!(seen.contains(TcpFlags::FIN), last, "segment {index}");
        assert_eq!(seen.contains(TcpFlags::PSH), last, "segment {index}");
        assert_eq!(
            seen.contains(TcpFlags::CWR),
            index == 0,
            "CWR signals once, on the first segment"
        );
    }
}

#[test]
fn each_segment_takes_its_own_ipv4_identification() {
    let (frame, offload) = super_frame(false, 3_000, 0, TcpFlags::ACK, 1_000);
    let out = segments(&frame, offload);
    let ip = ETHERNET_HEADER_LEN;
    let idents: Vec<u16> = out
        .iter()
        .map(|s| u16::from_be_bytes([s[ip + IPV4_IDENTIFICATION], s[ip + IPV4_IDENTIFICATION + 1]]))
        .collect();
    assert_eq!(idents, alloc::vec![IDENT, IDENT + 1, IDENT + 2]);
}

#[test]
fn a_payload_that_already_fits_is_one_segment() {
    let (frame, offload) = super_frame(false, 100, 0, TcpFlags::ACK, 1_460);
    let out = segments(&frame, offload);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), frame.len());
    assert_segment_valid(&out[0], false);
}

#[test]
fn a_payload_that_divides_exactly_emits_no_empty_tail() {
    let (frame, offload) = super_frame(false, 2_000, 0, TcpFlags::ACK, 1_000);
    let out = segments(&frame, offload);
    assert_eq!(out.len(), 2, "an exact multiple must not add a third");
    let header = ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + TCP_HEADER_LEN;
    assert!(out.iter().all(|s| s.len() == header + 1_000));
}

#[test]
fn a_payload_free_super_frame_emits_exactly_one_segment() {
    let (frame, offload) = super_frame(false, 0, 0, TcpFlags::ACK, 1_460);
    let mut segmenter = TcpSegmenter::new(&frame, offload).expect("valid");
    assert_eq!(segmenter.segment_count(), 1);
    let mut out = vec![0u8; segmenter.max_segment_len()];
    assert_eq!(
        segmenter.next_segment(&mut out).expect("ok"),
        Some(frame.len())
    );
    assert_eq!(
        segmenter.next_segment(&mut out).expect("ok"),
        None,
        "a payload-free frame must terminate, not repeat"
    );
}

#[test]
fn the_segment_count_matches_what_is_emitted() {
    for payload in [1usize, 999, 1_000, 1_001, 65_000] {
        let (frame, offload) = super_frame(false, payload, 0, TcpFlags::ACK, 1_000);
        let predicted = TcpSegmenter::new(&frame, offload)
            .expect("valid")
            .segment_count();
        assert_eq!(
            predicted,
            segments(&frame, offload).len(),
            "payload {payload}"
        );
    }
}

// -- Fail-closed validation ----------------------------------------------

#[test]
fn a_non_segment_offload_is_refused() {
    let (frame, _) = super_frame(false, 100, 0, TcpFlags::ACK, 1_460);
    for offload in [
        FrameOffload::None,
        FrameOffload::Validated,
        FrameOffload::TxChecksum {
            csum_start: 34,
            csum_offset: 16,
        },
    ] {
        assert_eq!(
            TcpSegmenter::new(&frame, offload).err(),
            Some(GsoError::NotSegmented)
        );
    }
}

#[test]
fn a_malformed_descriptor_is_refused_whole() {
    let (frame, base) = super_frame(false, 2_000, 0, TcpFlags::ACK, 1_000);
    let FrameOffload::TxSegment {
        csum_start,
        csum_offset,
        gso_size,
        hdr_len,
        ipv6,
    } = base
    else {
        panic!("built a segment offload")
    };
    let segment = |csum_start, csum_offset, gso_size, hdr_len, ipv6| FrameOffload::TxSegment {
        csum_start,
        csum_offset,
        gso_size,
        hdr_len,
        ipv6,
    };
    let over_long = u16::try_from(frame.len() + 1).expect("bounded");
    let bad = [
        // A zero segment size would never terminate.
        segment(csum_start, csum_offset, 0, hdr_len, ipv6),
        // A header longer than the frame.
        segment(csum_start, csum_offset, gso_size, over_long, ipv6),
        // A transport header shorter than TCP's fixed 20 octets.
        segment(csum_start, csum_offset, gso_size, csum_start + 8, ipv6),
        // A checksum field somewhere other than TCP's.
        segment(csum_start, 6, gso_size, hdr_len, ipv6),
        // A transport start inside the Ethernet header.
        segment(4, csum_offset, gso_size, hdr_len, ipv6),
        // An IP header length that disagrees with the frame's IHL nibble.
        segment(csum_start + 4, csum_offset, gso_size, hdr_len + 4, ipv6),
        // The wrong family for the frame's version nibble.
        segment(csum_start, csum_offset, gso_size, hdr_len, !ipv6),
    ];
    for offload in bad {
        assert_eq!(
            TcpSegmenter::new(&frame, offload).err(),
            Some(GsoError::Malformed),
            "offload {offload:?} must be refused"
        );
    }
}

#[test]
fn a_tcp_data_offset_that_disagrees_with_the_header_is_refused() {
    let (mut frame, offload) = super_frame(false, 2_000, 0, TcpFlags::ACK, 1_000);
    let tcp = ETHERNET_HEADER_LEN + IPV4_HEADER_LEN;
    frame[tcp + TCP_DATA_OFFSET] = 8 << 4; // claims 32 octets of TCP header
    assert_eq!(
        TcpSegmenter::new(&frame, offload).err(),
        Some(GsoError::Malformed)
    );
}

#[test]
fn a_short_destination_buffer_is_refused_without_advancing() {
    let (frame, offload) = super_frame(false, 2_000, 0, TcpFlags::ACK, 1_000);
    let mut segmenter = TcpSegmenter::new(&frame, offload).expect("valid");
    let mut small = vec![0u8; 32];
    assert_eq!(
        segmenter.next_segment(&mut small).err(),
        Some(GsoError::BufferTooSmall)
    );
    // Nothing was consumed, so a large enough buffer still gets segment one.
    let mut out = vec![0u8; segmenter.max_segment_len()];
    let len = segmenter.next_segment(&mut out).expect("ok").expect("some");
    assert_eq!(seq_of(&out[..len], false), SEQ);
}

// -- Resuming a part-emitted split ---------------------------------------

#[test]
fn resuming_at_a_boundary_continues_the_same_split() {
    let (frame, offload) = super_frame(false, 3_500, 0, TcpFlags::ACK, 1_000);
    let whole = segments(&frame, offload);

    // Stop after two segments, then pick the rest up from the boundary.
    let mut first = TcpSegmenter::new(&frame, offload).expect("valid");
    let mut out = vec![0u8; first.max_segment_len()];
    for _ in 0..2 {
        first.next_segment(&mut out).expect("ok").expect("some");
    }
    let resume_at = first.emitted();
    assert_eq!(resume_at, 2_000);

    let mut rest = TcpSegmenter::resume(&frame, offload, resume_at).expect("valid");
    let csum_start = ETHERNET_HEADER_LEN + IPV4_HEADER_LEN;
    let mut index = 2;
    while let Some(len) = rest.next_segment(&mut out).expect("ok") {
        let mut segment = out[..len].to_vec();
        let complete = internet_checksum(&segment[csum_start..]);
        segment[csum_start + CHECKSUM_OFFSET..csum_start + CHECKSUM_OFFSET + 2]
            .copy_from_slice(&complete.to_be_bytes());
        assert_eq!(segment, whole[index], "resumed segment {index} must match");
        index += 1;
    }
    assert_eq!(index, whole.len(), "every remaining segment is emitted");
}

#[test]
fn resuming_at_the_end_emits_nothing_more() {
    let (frame, offload) = super_frame(false, 2_000, 0, TcpFlags::ACK, 1_000);
    let mut done = TcpSegmenter::resume(&frame, offload, 2_000).expect("valid");
    let mut out = vec![0u8; done.max_segment_len()];
    assert_eq!(done.next_segment(&mut out).expect("ok"), None);
}

#[test]
fn a_resume_point_that_is_not_a_boundary_is_refused() {
    let (frame, offload) = super_frame(false, 2_000, 0, TcpFlags::ACK, 1_000);
    for emitted in [1usize, 999, 1_001, 2_001] {
        assert_eq!(
            TcpSegmenter::resume(&frame, offload, emitted).err(),
            Some(GsoError::Malformed),
            "resume at {emitted}"
        );
    }
}

// -- The transport a transmit-checksum engine must be told ----------------

#[test]
fn the_transport_protocol_is_read_from_either_family() {
    let (v4, _) = super_frame(false, 8, 0, TcpFlags::ACK, 1_460);
    assert_eq!(transport_protocol(&v4), Some(PROTOCOL_TCP));
    let (v6, _) = super_frame(true, 8, 0, TcpFlags::ACK, 1_460);
    assert_eq!(transport_protocol(&v6), Some(PROTOCOL_TCP));
}

#[test]
fn a_udp_frame_reports_udp_so_the_rfc_768_rule_is_applied() {
    let (mut frame, _) = super_frame(false, 8, 0, TcpFlags::ACK, 1_460);
    frame[ETHERNET_HEADER_LEN + 9] = crate::udp::PROTOCOL_UDP;
    assert_eq!(transport_protocol(&frame), Some(crate::udp::PROTOCOL_UDP));
}

#[test]
fn a_non_ip_or_short_frame_reports_nothing() {
    let (mut frame, _) = super_frame(false, 8, 0, TcpFlags::ACK, 1_460);
    frame[12..14].copy_from_slice(&crate::eth::ETHERTYPE_ARP.to_be_bytes());
    assert_eq!(transport_protocol(&frame), None);
    assert_eq!(transport_protocol(&frame[..10]), None);
    assert_eq!(transport_protocol(&[]), None);
}
