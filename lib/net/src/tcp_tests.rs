//! Unit tests for the TCP segment codec and sequence-space arithmetic.

use super::*;
use crate::addr::{Ipv4Addr, Ipv6Addr};
use alloc::vec;

const V4_SRC: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const V4_DST: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 9);

fn v4_pseudo() -> Pseudo {
    Pseudo::V4 {
        source: V4_SRC,
        destination: V4_DST,
    }
}

fn v6_pseudo() -> Pseudo {
    Pseudo::V6 {
        source: Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1),
        destination: Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 2),
    }
}

fn meta(flags: TcpFlags, options: TcpOptions) -> TcpSegmentMeta {
    TcpSegmentMeta {
        source_port: 0x1234,
        destination_port: 0x5678,
        seq: SeqNumber::new(0x1000_0000),
        ack: SeqNumber::new(0x2000_0000),
        flags,
        window: 0xABCD,
        urgent: 0,
        options,
    }
}

fn round_trip(pseudo: Pseudo, m: &TcpSegmentMeta, payload: &[u8]) -> TcpSegment<'static> {
    // Leak a fixed-capacity buffer so the returned borrow is 'static in
    // these tests; the codec itself never allocates.
    let buf = vec![0u8; MAX_HEADER_LEN + payload.len()];
    let buf = alloc::boxed::Box::leak(buf.into_boxed_slice());
    let n = write(pseudo, m, payload, buf).expect("write within bounds");
    TcpSegment::parse(pseudo, &buf[..n]).expect("a freshly written segment parses")
}

// ----- sequence arithmetic -----

#[test]
fn max_header_len_matches_data_offset() {
    // MAX_HEADER_LEN is a literal; pin it to the data-offset word bound.
    assert_eq!(MAX_HEADER_LEN, usize::from(15u8) * 4);
    assert_eq!(MAX_OPTIONS_LEN, MAX_HEADER_LEN - TCP_HEADER_LEN);
}

#[test]
fn seq_wrapping_add_and_sub() {
    let a = SeqNumber::new(u32::MAX);
    assert_eq!(a.add(1), SeqNumber::new(0));
    assert_eq!(SeqNumber::new(0).sub(1), SeqNumber::new(u32::MAX));
    assert_eq!(a.add(1).distance_from(a), 1);
}

#[test]
fn seq_modular_ordering_holds_across_the_wrap() {
    let base = SeqNumber::new(0xFFFF_FF00);
    let ahead = base.add(0x200); // wraps past zero
    assert!(base.lt(ahead));
    assert!(ahead.gt(base));
    assert!(base.le(ahead));
    assert!(ahead.ge(base));
    assert!(base.le(base));
    assert!(base.ge(base));
    assert!(!base.lt(base));
    assert!(!base.gt(base));
    assert_eq!(ahead.distance_from(base), 0x200);
}

#[test]
fn seq_half_space_is_the_ordering_boundary() {
    let zero = SeqNumber::new(0);
    let just_under_half = SeqNumber::new(0x7FFF_FFFF);
    let half = SeqNumber::new(0x8000_0000);
    // A point just under half ahead is "after" zero...
    assert!(just_under_half.gt(zero));
    // ...but exactly half away flips to "before" (the RFC 1982 ambiguity
    // boundary; TCP never compares points this far apart).
    assert!(half.lt(zero));
}

#[test]
fn seq_in_window_is_half_open() {
    let start = SeqNumber::new(100);
    assert!(start.in_window(start, 10));
    assert!(start.add(9).in_window(start, 10));
    assert!(!start.add(10).in_window(start, 10));
    // An empty window contains nothing, not even its start.
    assert!(!start.in_window(start, 0));
    // Wrapping window.
    let near_top = SeqNumber::new(u32::MAX - 2);
    assert!(SeqNumber::new(1).in_window(near_top, 10));
    assert!(!SeqNumber::new(8).in_window(near_top, 10));
}

// ----- flags -----

#[test]
fn flags_named_bits_and_combination() {
    let syn_ack = TcpFlags::SYN | TcpFlags::ACK;
    assert!(syn_ack.syn());
    assert!(syn_ack.ack());
    assert!(!syn_ack.fin());
    assert!(syn_ack.contains(TcpFlags::SYN));
    assert!(!syn_ack.contains(TcpFlags::FIN));
    assert_eq!(syn_ack.bits(), 0x12);
    assert_eq!(TcpFlags::from_bits(0x12), syn_ack);
    assert_eq!(TcpFlags::empty().bits(), 0);
}

// ----- round trips -----

#[test]
fn bare_segment_round_trips_v4_and_v6() {
    for pseudo in [v4_pseudo(), v6_pseudo()] {
        let m = meta(TcpFlags::ACK, TcpOptions::new());
        let seg = round_trip(pseudo, &m, b"payload bytes");
        assert_eq!(seg.source_port, 0x1234);
        assert_eq!(seg.destination_port, 0x5678);
        assert_eq!(seg.seq, SeqNumber::new(0x1000_0000));
        assert_eq!(seg.ack, SeqNumber::new(0x2000_0000));
        assert!(seg.flags.ack());
        assert_eq!(seg.window, 0xABCD);
        assert_eq!(seg.payload, b"payload bytes");
        assert!(seg.options.is_empty());
    }
}

#[test]
fn empty_payload_round_trips() {
    let m = meta(TcpFlags::SYN, TcpOptions::new());
    let seg = round_trip(v4_pseudo(), &m, &[]);
    assert!(seg.payload.is_empty());
    assert!(seg.flags.syn());
}

#[test]
fn syn_options_round_trip() {
    let mut opts = TcpOptions::new();
    opts.mss = Some(1460);
    opts.window_scale = Some(7);
    opts.sack_permitted = true;
    opts.timestamps = Some(Timestamps {
        value: 0xDEAD_BEEF,
        echo: 0,
    });
    let m = meta(TcpFlags::SYN, opts);
    let seg = round_trip(v6_pseudo(), &m, &[]);
    assert_eq!(seg.options.mss, Some(1460));
    assert_eq!(seg.options.window_scale, Some(7));
    assert!(seg.options.sack_permitted);
    assert_eq!(
        seg.options.timestamps,
        Some(Timestamps {
            value: 0xDEAD_BEEF,
            echo: 0,
        })
    );
}

#[test]
fn sack_blocks_round_trip() {
    let mut opts = TcpOptions::new();
    opts.timestamps = Some(Timestamps { value: 1, echo: 2 });
    let blocks = [
        SackBlock {
            left: SeqNumber::new(1000),
            right: SeqNumber::new(1500),
        },
        SackBlock {
            left: SeqNumber::new(2000),
            right: SeqNumber::new(2500),
        },
    ];
    assert!(opts.set_sack(&blocks));
    let m = meta(TcpFlags::ACK, opts);
    let seg = round_trip(v4_pseudo(), &m, b"data");
    assert_eq!(seg.options.sack(), &blocks);
    assert_eq!(
        seg.options.timestamps,
        Some(Timestamps { value: 1, echo: 2 })
    );
    assert_eq!(seg.payload, b"data");
}

#[test]
fn set_sack_rejects_over_the_bound() {
    let mut opts = TcpOptions::new();
    let too_many = [SackBlock {
        left: SeqNumber::new(0),
        right: SeqNumber::new(1),
    }; MAX_SACK_BLOCKS + 1];
    assert!(!opts.set_sack(&too_many));
    assert!(opts.sack().is_empty());
}

// ----- fail-closed parse -----

#[test]
fn truncated_header_is_rejected() {
    assert!(TcpSegment::parse(v4_pseudo(), &[]).is_none());
    assert!(TcpSegment::parse(v4_pseudo(), &[0u8; TCP_HEADER_LEN - 1]).is_none());
}

#[test]
fn bad_data_offset_is_rejected() {
    let m = meta(TcpFlags::ACK, TcpOptions::new());
    let mut buf = vec![0u8; TCP_HEADER_LEN + 4];
    let n = write(v4_pseudo(), &m, b"data", &mut buf).expect("write");
    // Data offset below 5 words.
    let mut small = buf[..n].to_vec();
    small[12] = 0x40; // 4 words
    assert!(TcpSegment::parse(v4_pseudo(), &small).is_none());
    // Data offset claiming a header longer than the segment.
    let mut big = buf[..n].to_vec();
    big[12] = 0xF0; // 15 words = 60 bytes > segment
    assert!(TcpSegment::parse(v4_pseudo(), &big).is_none());
}

#[test]
fn corrupt_payload_fails_checksum() {
    let m = meta(TcpFlags::ACK, TcpOptions::new());
    let mut buf = vec![0u8; TCP_HEADER_LEN + 8];
    let n = write(v6_pseudo(), &m, &[7u8; 8], &mut buf).expect("write");
    buf[n - 1] ^= 0x01;
    assert!(TcpSegment::parse(v6_pseudo(), &buf[..n]).is_none());
}

#[test]
fn wrong_pseudo_header_fails_checksum() {
    let m = meta(TcpFlags::ACK, TcpOptions::new());
    let mut buf = vec![0u8; TCP_HEADER_LEN + 2];
    let n = write(v4_pseudo(), &m, &[1, 2], &mut buf).expect("write");
    let other = Pseudo::V4 {
        source: V4_SRC,
        destination: Ipv4Addr::new(203, 0, 113, 1),
    };
    assert!(TcpSegment::parse(other, &buf[..n]).is_none());
}

#[test]
fn malformed_option_length_is_rejected() {
    // A hand-built segment whose MSS option claims length 3 (not 4).
    let mut seg = vec![0u8; 24];
    seg[12] = 0x60; // 6 words = 24 bytes header
    seg[13] = TcpFlags::SYN.bits();
    seg[20] = OPT_MSS_KIND;
    seg[21] = 3; // wrong length
    seg[22] = 0x05;
    seg[23] = 0xB4;
    // Fill in a correct checksum so only the option check can reject it.
    fix_checksum(v4_pseudo(), &mut seg);
    assert!(TcpSegment::parse(v4_pseudo(), &seg).is_none());
}

#[test]
fn option_overrunning_the_region_is_rejected() {
    let mut seg = vec![0u8; 24];
    seg[12] = 0x60;
    seg[13] = TcpFlags::ACK.bits();
    seg[20] = OPT_MSS_KIND;
    seg[21] = 10; // claims 10 bytes but only 4 remain in the region
    fix_checksum(v4_pseudo(), &mut seg);
    assert!(TcpSegment::parse(v4_pseudo(), &seg).is_none());
}

#[test]
fn too_many_sack_blocks_is_rejected() {
    // Build a header advertising a SACK option with 5 blocks (2 + 40).
    let header_words = 5 + (2 + 8 * (MAX_SACK_BLOCKS + 1)).div_ceil(4);
    let header_len = header_words * 4;
    let mut seg = vec![0u8; header_len];
    seg[12] = u8::try_from(header_words).unwrap() << 4;
    seg[13] = TcpFlags::ACK.bits();
    seg[20] = OPT_SACK_KIND;
    seg[21] = u8::try_from(2 + 8 * (MAX_SACK_BLOCKS + 1)).unwrap();
    fix_checksum(v4_pseudo(), &mut seg);
    assert!(TcpSegment::parse(v4_pseudo(), &seg).is_none());
}

#[test]
fn nop_and_eol_padding_is_tolerated() {
    // Header of 6 words: NOP, NOP, EOL, then padding — no real option.
    let mut seg = vec![0u8; 24];
    seg[12] = 0x60;
    seg[13] = TcpFlags::ACK.bits();
    seg[20] = OPT_NOP_KIND;
    seg[21] = OPT_NOP_KIND;
    seg[22] = OPT_EOL_KIND;
    fix_checksum(v4_pseudo(), &mut seg);
    let parsed = TcpSegment::parse(v4_pseudo(), &seg).expect("parses");
    assert!(parsed.options.is_empty());
}

#[test]
fn write_into_short_buffer_fails_closed() {
    let m = meta(TcpFlags::ACK, TcpOptions::new());
    let mut tiny = [0u8; TCP_HEADER_LEN - 1];
    assert_eq!(
        write(v4_pseudo(), &m, &[], &mut tiny),
        Err(WriteError::BufferTooSmall)
    );
}

#[test]
fn emitted_checksum_verifies_including_zero_result() {
    // Sweep payloads and confirm each written segment verifies. TCP has
    // no 0-checksum substitution, so a computed zero is transmitted as-is
    // and must still verify.
    for seed in 0u16..3000 {
        let m = meta(TcpFlags::ACK, TcpOptions::new());
        let payload = seed.to_be_bytes();
        let mut buf = [0u8; TCP_HEADER_LEN + 2];
        let n = write(v4_pseudo(), &m, &payload, &mut buf).expect("write");
        assert!(TcpSegment::parse(v4_pseudo(), &buf[..n]).is_some());
    }
}

// Test-only option-kind aliases, so the hand-built adversarial segments
// read clearly without exposing the private constants.
const OPT_EOL_KIND: u8 = 0;
const OPT_NOP_KIND: u8 = 1;
const OPT_MSS_KIND: u8 = 2;
const OPT_SACK_KIND: u8 = 5;

/// Lay a correct checksum into a hand-built segment's checksum field.
fn fix_checksum(pseudo: Pseudo, seg: &mut [u8]) {
    seg[16] = 0;
    seg[17] = 0;
    let len = u16::try_from(seg.len()).expect("test segment fits u16");
    let mut sum = match pseudo {
        Pseudo::V4 {
            source,
            destination,
        } => crate::checksum::Checksum::ipv4_pseudo(source, destination, PROTOCOL_TCP, len),
        Pseudo::V6 {
            source,
            destination,
        } => crate::checksum::Checksum::ipv6_pseudo(
            source,
            destination,
            PROTOCOL_TCP,
            u32::from(len),
        ),
    };
    sum.push(seg);
    let checksum = sum.finish();
    seg[16..18].copy_from_slice(&checksum.to_be_bytes());
}

/// Transmit-checksum offload conformance: a segment written with only the
/// partial (pseudo-header) checksum, then completed as the device would
/// (fold the transport bytes from `csum_start` to the end and store the
/// result at the checksum field), is byte-for-byte identical to the
/// segment the full software checksum produces. This is the invariant the
/// TX-checksum offload rests on — the device saves the fold, never changes
/// the wire result.
#[test]
fn tx_partial_checksum_completed_matches_the_software_full_checksum() {
    let opts = {
        let mut o = TcpOptions::new();
        o.mss = Some(1460);
        o.timestamps = Some(Timestamps {
            value: 0x0102_0304,
            echo: 0x0506_0708,
        });
        o
    };
    for pseudo in [v4_pseudo(), v6_pseudo()] {
        for payload in [&b""[..], &b"transmit-offload conformance payload"[..]] {
            let mut full = vec![0u8; MAX_HEADER_LEN + payload.len()];
            let n_full = super::write_with_checksum(
                pseudo,
                &meta(TcpFlags::ACK, opts),
                payload,
                &mut full,
                crate::checksum::ChecksumMode::Full,
            )
            .expect("full write");

            let mut partial = vec![0u8; MAX_HEADER_LEN + payload.len()];
            let n_partial = super::write_with_checksum(
                pseudo,
                &meta(TcpFlags::ACK, opts),
                payload,
                &mut partial,
                crate::checksum::ChecksumMode::Partial,
            )
            .expect("partial write");
            assert_eq!(n_full, n_partial);

            // The device completes the fold: over the whole segment
            // (csum_start = 0 here, the segment being the transport
            // region) it computes `internet_checksum` and stores it at the
            // checksum field (offset 16), exactly as the netstack RX
            // completion does.
            let completed = crate::internet_checksum(&partial[..n_partial]);
            partial[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 2].copy_from_slice(&completed.to_be_bytes());

            assert_eq!(
                &full[..n_full],
                &partial[..n_partial],
                "partial+completed must equal the full-checksum segment"
            );
            // And the completed segment verifies against the pseudo-header.
            assert!(TcpSegment::parse(pseudo, &partial[..n_partial]).is_some());
        }
    }
}
