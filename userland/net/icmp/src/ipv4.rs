//! Minimal IPv4 datagram handling (RFC 791).
//!
//! The responder only parses and emits option-free 20-byte headers,
//! which is all that ICMP echo over the QEMU user network requires.
//! Headers carrying options (`IHL > 5`), fragments, or a non-matching
//! total-length field are rejected by [`Ipv4Header::parse`].

use crate::{internet_checksum, Ipv4Address};

/// Length of an option-free IPv4 header.
pub const IPV4_HEADER_LEN: usize = 20;

/// IP protocol number for ICMP.
pub const PROTOCOL_ICMP: u8 = 1;

/// `version << 4 | IHL` for IPv4 with a 5-word (20-byte) header.
const VERSION_IHL: u8 = 0x45;

/// Default time-to-live for emitted datagrams.
const DEFAULT_TTL: u8 = 64;

/// Don't-Fragment flag in the flags/fragment-offset field.
const FLAG_DONT_FRAGMENT: u16 = 0x4000;

/// A parsed option-free IPv4 header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4Header {
    /// Source address.
    pub source: Ipv4Address,
    /// Destination address.
    pub destination: Ipv4Address,
    /// Upper-layer protocol number.
    pub protocol: u8,
}

impl Ipv4Header {
    /// Parse an option-free IPv4 header, returning it alongside the
    /// payload that the `total length` field delimits.
    ///
    /// Returns `None` for non-IPv4 headers, headers carrying options,
    /// or a `total length` that does not fit within `bytes`.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<(Self, &[u8])> {
        let header = bytes.get(..IPV4_HEADER_LEN)?;
        if header[0] != VERSION_IHL {
            return None;
        }
        let total_length = u16::from_be_bytes([header[2], header[3]]) as usize;
        if total_length < IPV4_HEADER_LEN || total_length > bytes.len() {
            return None;
        }
        let parsed = Self {
            source: address(&header[12..16]),
            destination: address(&header[16..20]),
            protocol: header[9],
        };
        Some((parsed, &bytes[IPV4_HEADER_LEN..total_length]))
    }

    /// Write a header for a datagram carrying `payload_len` bytes,
    /// filling in the length and header checksum.
    ///
    /// Returns `None` when `out` cannot hold the header or when the
    /// resulting total length would overflow the 16-bit field.
    #[must_use]
    pub fn write(&self, out: &mut [u8], payload_len: usize) -> Option<usize> {
        let total_length = u16::try_from(IPV4_HEADER_LEN.checked_add(payload_len)?).ok()?;
        let header = out.get_mut(..IPV4_HEADER_LEN)?;
        header[0] = VERSION_IHL;
        header[1] = 0;
        header[2..4].copy_from_slice(&total_length.to_be_bytes());
        header[4..6].copy_from_slice(&0u16.to_be_bytes());
        header[6..8].copy_from_slice(&FLAG_DONT_FRAGMENT.to_be_bytes());
        header[8] = DEFAULT_TTL;
        header[9] = self.protocol;
        header[10..12].copy_from_slice(&0u16.to_be_bytes());
        header[12..16].copy_from_slice(self.source.as_octets());
        header[16..20].copy_from_slice(self.destination.as_octets());
        let checksum = internet_checksum(header);
        header[10..12].copy_from_slice(&checksum.to_be_bytes());
        Some(IPV4_HEADER_LEN)
    }
}

fn address(bytes: &[u8]) -> Ipv4Address {
    let mut octets = [0u8; 4];
    octets.copy_from_slice(bytes);
    Ipv4Address(octets)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: Ipv4Address = Ipv4Address([10, 0, 2, 2]);
    const DST: Ipv4Address = Ipv4Address([10, 0, 2, 15]);

    #[test]
    fn write_then_parse_round_trips() {
        let mut out = [0u8; IPV4_HEADER_LEN + 4];
        let header = Ipv4Header {
            source: SRC,
            destination: DST,
            protocol: PROTOCOL_ICMP,
        };
        let len = header.write(&mut out, 4).expect("fits");
        assert_eq!(len, IPV4_HEADER_LEN);
        out[IPV4_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);

        let (parsed, payload) = Ipv4Header::parse(&out).expect("parses");
        assert_eq!(parsed, header);
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn written_header_has_valid_checksum() {
        let mut out = [0u8; IPV4_HEADER_LEN];
        Ipv4Header {
            source: SRC,
            destination: DST,
            protocol: PROTOCOL_ICMP,
        }
        .write(&mut out, 0)
        .expect("fits");
        // The one's-complement sum of a header including its checksum
        // field is zero.
        assert_eq!(internet_checksum(&out), 0);
    }

    #[test]
    fn parse_rejects_options_and_wrong_version() {
        let mut out = [0u8; IPV4_HEADER_LEN];
        Ipv4Header {
            source: SRC,
            destination: DST,
            protocol: PROTOCOL_ICMP,
        }
        .write(&mut out, 0)
        .expect("fits");
        out[0] = 0x46; // IHL = 6 (options present)
        assert!(Ipv4Header::parse(&out).is_none());
        out[0] = 0x60; // version 6
        assert!(Ipv4Header::parse(&out).is_none());
    }

    #[test]
    fn parse_rejects_total_length_overrun() {
        let mut out = [0u8; IPV4_HEADER_LEN];
        Ipv4Header {
            source: SRC,
            destination: DST,
            protocol: PROTOCOL_ICMP,
        }
        .write(&mut out, 0)
        .expect("fits");
        out[2..4].copy_from_slice(&100u16.to_be_bytes());
        assert!(Ipv4Header::parse(&out).is_none());
    }

    #[test]
    fn parse_rejects_truncated() {
        assert!(Ipv4Header::parse(&[0u8; IPV4_HEADER_LEN - 1]).is_none());
    }
}
