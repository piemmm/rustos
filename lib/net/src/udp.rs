//! UDP (RFC 768) over IPv4 and IPv6.
//!
//! UDP is a thin framing over the network layer: an eight-byte header of
//! source port, destination port, length, and checksum, followed by the
//! payload. This module is the one dual-stack definition of that framing —
//! parse and emit share a single core that folds the RFC 1071 checksum over
//! the family-appropriate pseudo-header ([`crate::checksum`]), so IPv4 and
//! IPv6 are not two shadowed code paths.
//!
//! The checksum discipline differs by family, deliberately:
//!
//! * **IPv4** — the checksum is optional on the wire (RFC 768). A received
//!   datagram carrying a zero checksum field is accepted unverified; an
//!   emitted datagram always carries a computed checksum (a computed value of
//!   zero is transmitted as `0xFFFF`, the one's-complement equivalent, so it
//!   is never mistaken for "no checksum").
//! * **IPv6** — the checksum is mandatory (RFC 8200 §8.1). A received
//!   datagram with a zero checksum field is rejected, and an emitted datagram
//!   always carries a non-zero checksum (`0xFFFF` substituted as above).
//!
//! Every decoder is total, bounded, and fail-closed: a malformed length,
//! truncation, or a checksum mismatch rejects the whole datagram (`None`);
//! nothing partial is surfaced.

use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::checksum::Checksum;

/// Length of the fixed UDP header (source/dest port, length, checksum).
pub const UDP_HEADER_LEN: usize = 8;

/// IP protocol number (IPv4) and next-header value (IPv6) for UDP.
pub const PROTOCOL_UDP: u8 = 17;

/// Largest value the 16-bit UDP `length` field can carry.
const UDP_LENGTH_MAX: usize = u16::MAX as usize;

/// The pseudo-header a UDP checksum folds over, one variant per family.
///
/// Carrying the family this way keeps the [`UdpDatagram::parse`] and
/// [`write()`] entry points single, dual-stack definitions rather than a v4
/// path a v6 path would shadow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pseudo {
    /// The IPv4 pseudo-header source and destination.
    V4 {
        /// Source address.
        source: Ipv4Addr,
        /// Destination address.
        destination: Ipv4Addr,
    },
    /// The IPv6 pseudo-header source and destination.
    V6 {
        /// Source address.
        source: Ipv6Addr,
        /// Destination address.
        destination: Ipv6Addr,
    },
}

impl Pseudo {
    /// A checksum accumulator seeded with this pseudo-header for a UDP
    /// datagram of `udp_len` bytes (header + payload).
    fn seed(self, udp_len: u16) -> Checksum {
        match self {
            Self::V4 {
                source,
                destination,
            } => Checksum::ipv4_pseudo(source, destination, PROTOCOL_UDP, udp_len),
            Self::V6 {
                source,
                destination,
            } => Checksum::ipv6_pseudo(source, destination, PROTOCOL_UDP, u32::from(udp_len)),
        }
    }

    /// True for the IPv6 pseudo-header, whose checksum is mandatory.
    const fn is_v6(self) -> bool {
        matches!(self, Self::V6 { .. })
    }
}

/// A parsed UDP datagram: the two ports and the payload borrowed from the
/// input buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UdpDatagram<'a> {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub destination_port: u16,
    /// The datagram payload (the bytes the `length` field delimits).
    pub payload: &'a [u8],
}

impl<'a> UdpDatagram<'a> {
    /// Parse and verify a UDP datagram carried in `bytes` under the
    /// `pseudo` addressing context.
    ///
    /// Returns `None` (fail closed) for a truncated header, a `length`
    /// field below the header size or beyond `bytes`, a mandatory-checksum
    /// IPv6 datagram with a zero checksum field, or any datagram whose
    /// checksum does not verify. The payload is `bytes[8..length]`, so
    /// trailing bytes the network layer may have included past the UDP
    /// `length` are ignored.
    #[must_use]
    pub fn parse(pseudo: Pseudo, bytes: &'a [u8]) -> Option<Self> {
        let header = bytes.get(..UDP_HEADER_LEN)?;
        let source_port = u16::from_be_bytes([header[0], header[1]]);
        let destination_port = u16::from_be_bytes([header[2], header[3]]);
        let length_field = u16::from_be_bytes([header[4], header[5]]);
        let length = usize::from(length_field);
        let checksum = u16::from_be_bytes([header[6], header[7]]);
        if length < UDP_HEADER_LEN || length > bytes.len() {
            return None;
        }
        let datagram = &bytes[..length];
        // IPv4 permits an all-zero checksum meaning "not computed"; IPv6
        // requires a checksum, so a zero field there is a malformed datagram.
        if checksum == 0 {
            if pseudo.is_v6() {
                return None;
            }
        } else {
            let mut sum = pseudo.seed(length_field);
            sum.push(datagram);
            if sum.finish() != 0 {
                return None;
            }
        }
        Some(Self {
            source_port,
            destination_port,
            payload: &bytes[UDP_HEADER_LEN..length],
        })
    }
}

/// Errors from [`write()`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteError {
    /// `out` is smaller than the eight-byte header plus the payload.
    BufferTooSmall,
    /// The header plus payload exceeds the 16-bit UDP `length` field.
    TooLarge,
}

/// Write a UDP datagram — header then `payload` — into `out`, computing the
/// checksum over `pseudo`. Returns the number of bytes written
/// ([`UDP_HEADER_LEN`] + `payload.len()`).
///
/// A computed checksum of zero is transmitted as `0xFFFF` so it is never
/// read as "no checksum" (RFC 768 for IPv4, mandatory for IPv6).
///
/// # Errors
///
/// * [`WriteError::TooLarge`] — the datagram exceeds the `length` field.
/// * [`WriteError::BufferTooSmall`] — `out` cannot hold the datagram.
pub fn write(
    pseudo: Pseudo,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Result<usize, WriteError> {
    let total = UDP_HEADER_LEN
        .checked_add(payload.len())
        .filter(|&total| total <= UDP_LENGTH_MAX)
        .ok_or(WriteError::TooLarge)?;
    let out = out.get_mut(..total).ok_or(WriteError::BufferTooSmall)?;
    // `total <= UDP_LENGTH_MAX` (checked above) so this conversion holds.
    let length = u16::try_from(total).map_err(|_| WriteError::TooLarge)?;
    out[0..2].copy_from_slice(&source_port.to_be_bytes());
    out[2..4].copy_from_slice(&destination_port.to_be_bytes());
    out[4..6].copy_from_slice(&length.to_be_bytes());
    out[6..8].copy_from_slice(&[0, 0]);
    out[UDP_HEADER_LEN..].copy_from_slice(payload);
    let mut sum = pseudo.seed(length);
    sum.push(out);
    let checksum = match sum.finish() {
        0 => 0xFFFF,
        value => value,
    };
    out[6..8].copy_from_slice(&checksum.to_be_bytes());
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const V4_SRC: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const V4_DST: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 9);

    fn v6_src() -> Ipv6Addr {
        Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1)
    }

    fn v6_dst() -> Ipv6Addr {
        Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 2)
    }

    fn v4_pseudo() -> Pseudo {
        Pseudo::V4 {
            source: V4_SRC,
            destination: V4_DST,
        }
    }

    fn v6_pseudo() -> Pseudo {
        Pseudo::V6 {
            source: v6_src(),
            destination: v6_dst(),
        }
    }

    #[test]
    fn write_then_parse_round_trips_v4() {
        let payload = b"hello udp";
        let mut buf = vec![0u8; UDP_HEADER_LEN + payload.len()];
        let n = write(v4_pseudo(), 4000, 53, payload, &mut buf).expect("write");
        assert_eq!(n, buf.len());
        let dg = UdpDatagram::parse(v4_pseudo(), &buf).expect("parse");
        assert_eq!(dg.source_port, 4000);
        assert_eq!(dg.destination_port, 53);
        assert_eq!(dg.payload, payload);
    }

    #[test]
    fn write_then_parse_round_trips_v6() {
        let payload = b"hello udp6";
        let mut buf = vec![0u8; UDP_HEADER_LEN + payload.len()];
        write(v6_pseudo(), 9, 123, payload, &mut buf).expect("write");
        let dg = UdpDatagram::parse(v6_pseudo(), &buf).expect("parse");
        assert_eq!(dg.source_port, 9);
        assert_eq!(dg.destination_port, 123);
        assert_eq!(dg.payload, payload);
    }

    #[test]
    fn empty_payload_round_trips() {
        let mut buf = vec![0u8; UDP_HEADER_LEN];
        write(v4_pseudo(), 1, 2, &[], &mut buf).expect("write");
        let dg = UdpDatagram::parse(v4_pseudo(), &buf).expect("parse");
        assert!(dg.payload.is_empty());
    }

    #[test]
    fn emitted_checksum_is_never_zero() {
        // Search for a payload whose folded checksum would be zero and
        // confirm it is transmitted as 0xFFFF instead of 0x0000.
        for seed in 0u16..2000 {
            let payload = seed.to_be_bytes();
            let mut buf = [0u8; UDP_HEADER_LEN + 2];
            write(v4_pseudo(), 0x1234, 0x5678, &payload, &mut buf).expect("write");
            let checksum = u16::from_be_bytes([buf[6], buf[7]]);
            assert_ne!(checksum, 0, "an emitted UDP checksum is never zero");
            // Whatever it is, it must verify.
            assert!(UdpDatagram::parse(v4_pseudo(), &buf).is_some());
        }
    }

    #[test]
    fn v4_zero_checksum_is_accepted_unverified() {
        let mut buf = vec![0u8; UDP_HEADER_LEN + 3];
        write(v4_pseudo(), 10, 20, &[1, 2, 3], &mut buf).expect("write");
        // Blank the checksum field: IPv4 reads this as "not computed".
        buf[6] = 0;
        buf[7] = 0;
        let dg = UdpDatagram::parse(v4_pseudo(), &buf).expect("v4 accepts zero checksum");
        assert_eq!(dg.payload, &[1, 2, 3]);
    }

    #[test]
    fn v6_zero_checksum_is_rejected() {
        let mut buf = vec![0u8; UDP_HEADER_LEN + 3];
        write(v6_pseudo(), 10, 20, &[1, 2, 3], &mut buf).expect("write");
        buf[6] = 0;
        buf[7] = 0;
        assert!(UdpDatagram::parse(v6_pseudo(), &buf).is_none());
    }

    #[test]
    fn corrupt_payload_fails_checksum() {
        let mut buf = vec![0u8; UDP_HEADER_LEN + 4];
        write(v6_pseudo(), 1, 2, &[9, 9, 9, 9], &mut buf).expect("write");
        buf[UDP_HEADER_LEN] ^= 0x01;
        assert!(UdpDatagram::parse(v6_pseudo(), &buf).is_none());
    }

    #[test]
    fn wrong_pseudo_header_fails_checksum() {
        // A datagram checksummed for one address pair must not verify under
        // another — this is what binds the checksum to the IP header.
        let mut buf = vec![0u8; UDP_HEADER_LEN + 2];
        write(v4_pseudo(), 1, 2, &[7, 7], &mut buf).expect("write");
        let other = Pseudo::V4 {
            source: V4_SRC,
            destination: Ipv4Addr::new(203, 0, 113, 1),
        };
        assert!(UdpDatagram::parse(other, &buf).is_none());
    }

    #[test]
    fn truncated_header_is_rejected() {
        assert!(UdpDatagram::parse(v4_pseudo(), &[0, 1, 2, 3, 4]).is_none());
        assert!(UdpDatagram::parse(v4_pseudo(), &[]).is_none());
    }

    #[test]
    fn length_field_out_of_range_is_rejected() {
        let mut buf = vec![0u8; UDP_HEADER_LEN + 4];
        write(v4_pseudo(), 1, 2, &[0, 0, 0, 0], &mut buf).expect("write");
        // Length claiming more than the buffer holds.
        buf[4] = 0xFF;
        buf[5] = 0xFF;
        assert!(UdpDatagram::parse(v4_pseudo(), &buf).is_none());
        // Length below the header size.
        buf[4] = 0;
        buf[5] = 4;
        assert!(UdpDatagram::parse(v4_pseudo(), &buf).is_none());
    }

    #[test]
    fn trailing_bytes_past_length_are_ignored() {
        let payload = [1u8, 2, 3];
        let mut buf = vec![0u8; UDP_HEADER_LEN + payload.len()];
        write(v4_pseudo(), 1, 2, &payload, &mut buf).expect("write");
        // Append network-layer padding beyond the UDP length.
        buf.extend_from_slice(&[0xAA, 0xBB]);
        let dg = UdpDatagram::parse(v4_pseudo(), &buf).expect("parse");
        assert_eq!(dg.payload, &payload);
    }

    #[test]
    fn write_into_short_buffer_fails_closed() {
        let mut tiny = [0u8; UDP_HEADER_LEN - 1];
        assert_eq!(
            write(v4_pseudo(), 1, 2, &[], &mut tiny),
            Err(WriteError::BufferTooSmall)
        );
    }

    #[test]
    fn write_rejects_oversize_datagram() {
        let payload = vec![0u8; UDP_LENGTH_MAX];
        let mut out = vec![0u8; UDP_LENGTH_MAX + UDP_HEADER_LEN];
        assert_eq!(
            write(v4_pseudo(), 1, 2, &payload, &mut out),
            Err(WriteError::TooLarge)
        );
    }
}
