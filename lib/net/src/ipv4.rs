//! IPv4 datagram handling (RFC 791).
//!
//! The parse side is options-tolerant: a header with `IHL > 5` is
//! accepted, its checksum verified over the full header, and the options
//! bytes surfaced (opaque, never interpreted — this host neither sets
//! nor honours IPv4 options). The emit side is strict: only option-free
//! 20-byte headers are written. Fragmentation on emit is [`fragment`];
//! reassembly of received fragments lives in [`crate::frag`].

use crate::addr::{Ecn, Ipv4Addr};
use crate::checksum::internet_checksum;

/// Length of an option-free IPv4 header.
pub const IPV4_HEADER_LEN: usize = 20;

/// Largest header the 4-bit IHL field can describe (15 words).
pub const IPV4_MAX_HEADER_LEN: usize = 60;

/// IP protocol number for ICMP.
pub const PROTOCOL_ICMP: u8 = 1;

/// Smallest MTU every IPv4 link must carry (RFC 791 §3.2), and thus the
/// smallest MTU [`fragment`] accepts.
pub const IPV4_MIN_MTU: usize = 68;

/// Default time-to-live for emitted datagrams.
pub const DEFAULT_TTL: u8 = 64;

/// Don't-Fragment flag in the flags/fragment-offset field.
const FLAG_DONT_FRAGMENT: u16 = 0x4000;

/// More-Fragments flag in the flags/fragment-offset field.
const FLAG_MORE_FRAGMENTS: u16 = 0x2000;

/// Reserved ("evil") bit — must be zero (RFC 791).
const FLAG_RESERVED: u16 = 0x8000;

/// Mask of the 13-bit fragment offset (in 8-byte units).
const OFFSET_MASK: u16 = 0x1FFF;

/// A parsed or to-be-emitted IPv4 header.
///
/// The fragment offset is carried in bytes (always a multiple of 8 —
/// the wire field counts 8-byte units).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4Header {
    /// Source address.
    pub source: Ipv4Addr,
    /// Destination address.
    pub destination: Ipv4Addr,
    /// Upper-layer protocol number.
    pub protocol: u8,
    /// The ECN codepoint carried in the low two bits of the TOS byte
    /// (RFC 3168 §5). The DSCP (high six bits) is not used and is written
    /// as zero; a parsed header reports only the ECN field.
    pub ecn: Ecn,
    /// Datagram identification, shared by all fragments of one datagram.
    pub identification: u16,
    /// Remaining hop count.
    pub ttl: u8,
    /// Don't-Fragment flag.
    pub dont_fragment: bool,
    /// More-Fragments flag.
    pub more_fragments: bool,
    /// Fragment offset in bytes (a multiple of 8).
    pub fragment_offset: u16,
}

impl Ipv4Header {
    /// A whole (unfragmented) datagram header with the default TTL and
    /// Don't-Fragment clear, ready for [`Self::write`] or [`fragment`].
    #[must_use]
    pub fn new(source: Ipv4Addr, destination: Ipv4Addr, protocol: u8) -> Self {
        Self {
            source,
            destination,
            protocol,
            ecn: Ecn::NotEct,
            identification: 0,
            ttl: DEFAULT_TTL,
            dont_fragment: false,
            more_fragments: false,
            fragment_offset: 0,
        }
    }

    /// True when this header describes a fragment of a larger datagram
    /// (either a non-final piece or a non-zero offset).
    #[must_use]
    pub fn is_fragment(&self) -> bool {
        self.more_fragments || self.fragment_offset != 0
    }

    /// Parse an IPv4 header, tolerating options, returning the header,
    /// the opaque options bytes, and the payload the `total length`
    /// field delimits.
    ///
    /// Returns `None` for non-IPv4 versions, an IHL below 5 words or
    /// beyond `bytes`, a `total length` shorter than the header or
    /// longer than `bytes`, a set reserved flag bit, or a header whose
    /// checksum does not verify (RFC 1122 §3.2.1.2 — a corrupted header
    /// is discarded, never acted on).
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<(Self, &[u8], &[u8])> {
        let first = *bytes.first()?;
        if first >> 4 != 4 {
            return None;
        }
        let header_len = usize::from(first & 0x0F) * 4;
        if header_len < IPV4_HEADER_LEN {
            return None;
        }
        let header = bytes.get(..header_len)?;
        if internet_checksum(header) != 0 {
            return None;
        }
        let total_length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        if total_length < header_len || total_length > bytes.len() {
            return None;
        }
        let flags_offset = u16::from_be_bytes([header[6], header[7]]);
        if flags_offset & FLAG_RESERVED != 0 {
            return None;
        }
        let parsed = Self {
            source: address(&header[12..16]),
            destination: address(&header[16..20]),
            protocol: header[9],
            ecn: Ecn::from_bits(header[1]),
            identification: u16::from_be_bytes([header[4], header[5]]),
            ttl: header[8],
            dont_fragment: flags_offset & FLAG_DONT_FRAGMENT != 0,
            more_fragments: flags_offset & FLAG_MORE_FRAGMENTS != 0,
            fragment_offset: (flags_offset & OFFSET_MASK) * 8,
        };
        Some((
            parsed,
            &bytes[IPV4_HEADER_LEN..header_len],
            &bytes[header_len..total_length],
        ))
    }

    /// Write an option-free header for a datagram carrying `payload_len`
    /// bytes, filling in the length and header checksum.
    ///
    /// Returns `None` when `out` cannot hold the header, when the
    /// resulting total length would overflow the 16-bit field, or when
    /// [`Self::fragment_offset`] is not a multiple of 8 (an aligned
    /// 16-bit offset always fits the 13-bit unit field).
    #[must_use]
    pub fn write(&self, out: &mut [u8], payload_len: usize) -> Option<usize> {
        self.write_with_options(&[], out, payload_len)
    }

    /// Write a header carrying the Router Alert option (RFC 2113): a
    /// four-byte option flagging routers to examine the datagram, as
    /// IGMP membership messages require (RFC 2236 §2). The header grows
    /// to 24 bytes (`IHL = 6`).
    ///
    /// Returns `None` under the same conditions as [`Self::write`].
    #[must_use]
    pub fn write_with_router_alert(&self, out: &mut [u8], payload_len: usize) -> Option<usize> {
        self.write_with_options(&ROUTER_ALERT_OPTION, out, payload_len)
    }

    /// Write a header carrying `options` (a whole number of 32-bit
    /// words, at most the 40 the IHL field allows), filling in the
    /// length and checksum. The one emit core [`Self::write`] and
    /// [`Self::write_with_router_alert`] share.
    fn write_with_options(
        &self,
        options: &[u8],
        out: &mut [u8],
        payload_len: usize,
    ) -> Option<usize> {
        if options.len() % 4 != 0 || options.len() > IPV4_MAX_HEADER_LEN - IPV4_HEADER_LEN {
            return None;
        }
        let header_len = IPV4_HEADER_LEN + options.len();
        let total_length = u16::try_from(header_len.checked_add(payload_len)?).ok()?;
        if self.fragment_offset % 8 != 0 {
            return None;
        }
        let mut flags_offset = self.fragment_offset / 8;
        if self.dont_fragment {
            flags_offset |= FLAG_DONT_FRAGMENT;
        }
        if self.more_fragments {
            flags_offset |= FLAG_MORE_FRAGMENTS;
        }
        let header = out.get_mut(..header_len)?;
        // IHL counts 32-bit words; header_len is always a multiple of 4.
        header[0] = 0x40 | u8::try_from(header_len / 4).ok()?;
        // DSCP (high six bits) unused; ECN in the low two bits (RFC 3168).
        header[1] = self.ecn.bits();
        header[2..4].copy_from_slice(&total_length.to_be_bytes());
        header[4..6].copy_from_slice(&self.identification.to_be_bytes());
        header[6..8].copy_from_slice(&flags_offset.to_be_bytes());
        header[8] = self.ttl;
        header[9] = self.protocol;
        header[10..12].copy_from_slice(&0u16.to_be_bytes());
        header[12..16].copy_from_slice(&self.source.octets());
        header[16..20].copy_from_slice(&self.destination.octets());
        header[IPV4_HEADER_LEN..header_len].copy_from_slice(options);
        let checksum = internet_checksum(header);
        header[10..12].copy_from_slice(&checksum.to_be_bytes());
        Some(header_len)
    }
}

/// The IPv4 Router Alert option (RFC 2113): option type 148, length 4,
/// value 0 ("router shall examine this packet").
const ROUTER_ALERT_OPTION: [u8; 4] = [0x94, 0x04, 0x00, 0x00];

/// One piece of a fragmented emit: the payload byte range to carry and
/// the header it travels under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FragmentPart {
    /// Header for this fragment (offset and More-Fragments filled in).
    pub header: Ipv4Header,
    /// Start of this fragment's payload slice within the datagram.
    pub payload_start: usize,
    /// End (exclusive) of this fragment's payload slice.
    pub payload_end: usize,
}

/// Plan the fragmentation of a `payload_len`-byte datagram under
/// `header` onto a link with the given `mtu`.
///
/// Returns the per-fragment headers and payload ranges, in order. A
/// datagram that fits emits one whole part. Fails closed (`None`) when
/// `mtu` is below [`IPV4_MIN_MTU`], when the header has Don't-Fragment
/// set and the datagram does not fit, when the header already describes
/// a fragment, or when the datagram exceeds the 16-bit total length.
#[must_use]
pub fn fragment(
    header: Ipv4Header,
    payload_len: usize,
    mtu: usize,
) -> Option<alloc::vec::Vec<FragmentPart>> {
    use alloc::vec::Vec;
    if header.is_fragment() {
        return None;
    }
    u16::try_from(IPV4_HEADER_LEN.checked_add(payload_len)?).ok()?;
    if IPV4_HEADER_LEN + payload_len <= mtu {
        return Some(alloc::vec![FragmentPart {
            header,
            payload_start: 0,
            payload_end: payload_len,
        }]);
    }
    if header.dont_fragment || mtu < IPV4_MIN_MTU {
        return None;
    }
    // Per-fragment payload: the MTU minus the header, rounded down to a
    // multiple of 8 so every non-final offset is representable.
    let chunk = (mtu - IPV4_HEADER_LEN) & !7;
    let mut parts = Vec::new();
    let mut start = 0usize;
    while start < payload_len {
        let end = core::cmp::min(start + chunk, payload_len);
        let mut part_header = header;
        part_header.fragment_offset = u16::try_from(start).ok()?;
        part_header.more_fragments = end < payload_len;
        parts.push(FragmentPart {
            header: part_header,
            payload_start: start,
            payload_end: end,
        });
        start = end;
    }
    Some(parts)
}

fn address(bytes: &[u8]) -> Ipv4Addr {
    let mut octets = [0u8; 4];
    octets.copy_from_slice(bytes);
    Ipv4Addr::from(octets)
}

#[cfg(test)]
#[path = "ipv4_tests.rs"]
mod tests;
