//! The Internet checksum (RFC 1071), defined once.
//!
//! Every protocol that carries the 16-bit one's-complement checksum —
//! IPv4 headers, ICMP, `ICMPv6`, UDP, TCP — folds through this module.
//! There are two entry points:
//!
//! - [`internet_checksum`] — the one-shot fold over a contiguous slice,
//!   for messages whose checksum covers exactly their own bytes (IPv4
//!   headers, ICMP).
//! - [`Checksum`] — the incremental accumulator, for checksums that span
//!   a pseudo-header plus a transport header plus a payload without the
//!   caller assembling them into one buffer. [`Checksum::ipv4_pseudo`]
//!   and [`Checksum::ipv6_pseudo`] seed the accumulator with the RFC 768
//!   / RFC 793 IPv4 pseudo-header or the RFC 8200 §8.1 IPv6
//!   pseudo-header.
//!
//! The accumulator has byte-stream semantics: feeding it the same bytes
//! in any split of [`Checksum::push`] calls yields the same fold as one
//! contiguous [`internet_checksum`] call, including when a push ends on
//! an odd byte (the next push's first byte completes the 16-bit word,
//! exactly as if the buffers were concatenated).

use crate::addr::{Ipv4Addr, Ipv6Addr};

/// Incremental RFC 1071 one's-complement accumulator.
///
/// Construct with [`Checksum::new`] (or a pseudo-header seed), feed bytes
/// with [`Checksum::push`], and take the transmitted checksum value with
/// [`Checksum::finish`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Checksum {
    /// Running 32-bit sum of big-endian 16-bit words; carries are folded
    /// in [`Self::finish`].
    sum: u32,
    /// A trailing byte from an odd-length [`Self::push`], waiting to be
    /// paired with the first byte of the next push (byte-stream
    /// semantics).
    pending: Option<u8>,
}

impl Checksum {
    /// An accumulator over an empty byte stream.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sum: 0,
            pending: None,
        }
    }

    /// An accumulator seeded with the IPv4 pseudo-header (RFC 768 /
    /// RFC 793): source address, destination address, zero + protocol,
    /// and the upper-layer length.
    #[must_use]
    pub fn ipv4_pseudo(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, upper_len: u16) -> Self {
        let mut sum = Self::new();
        sum.push(&src.octets());
        sum.push(&dst.octets());
        sum.push(&[0, protocol]);
        sum.push(&upper_len.to_be_bytes());
        sum
    }

    /// An accumulator seeded with the IPv6 pseudo-header (RFC 8200
    /// §8.1): source address, destination address, the 32-bit
    /// upper-layer length, and three zero octets + next header.
    #[must_use]
    pub fn ipv6_pseudo(src: Ipv6Addr, dst: Ipv6Addr, next_header: u8, upper_len: u32) -> Self {
        let mut sum = Self::new();
        sum.push(&src.octets());
        sum.push(&dst.octets());
        sum.push(&upper_len.to_be_bytes());
        sum.push(&[0, 0, 0, next_header]);
        sum
    }

    /// Fold `bytes` into the running sum.
    pub fn push(&mut self, bytes: &[u8]) {
        let mut bytes = bytes;
        if let Some(high) = self.pending.take() {
            let Some((&low, rest)) = bytes.split_first() else {
                self.pending = Some(high);
                return;
            };
            self.sum += u32::from(u16::from_be_bytes([high, low]));
            bytes = rest;
        }
        let mut words = bytes.chunks_exact(2);
        for word in words.by_ref() {
            self.sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
        }
        if let [odd] = words.remainder() {
            self.pending = Some(*odd);
        }
    }

    /// Complete the fold and return the transmitted checksum value (the
    /// one's complement of the one's-complement sum).
    ///
    /// A trailing odd byte is padded with a zero low byte, per RFC 1071.
    /// Verifying a received message is folding the message *including*
    /// its checksum field and requiring the result to be zero.
    #[must_use]
    pub fn finish(self) -> u16 {
        !self.partial()
    }

    /// Fold the running sum to 16 bits **without** the final one's
    /// complement — the *partial* checksum a transmit-checksum-offload
    /// sender leaves in the checksum field for the device to complete
    /// (virtio's `VIRTIO_NET_HDR_F_NEEDS_CSUM`, Linux `CHECKSUM_PARTIAL`).
    ///
    /// Seeded with a pseudo-header ([`Pseudo::seed`]) and taken *without*
    /// pushing the transport bytes, this is the folded one's-complement
    /// sum of the pseudo-header alone. A device (or the software
    /// completion) then folds the transport bytes from `csum_start` to the
    /// end — which include this value in the checksum field — and
    /// complements the result, reproducing exactly the checksum
    /// [`finish`](Self::finish) would have computed over the whole
    /// datagram (`plans/NETWORK.md` §2.3).
    ///
    /// A trailing odd byte is padded with a zero low byte, per RFC 1071.
    #[must_use]
    pub fn partial(mut self) -> u16 {
        if let Some(high) = self.pending.take() {
            self.sum += u32::from(u16::from_be_bytes([high, 0]));
        }
        let mut sum = self.sum;
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        (sum & 0xFFFF) as u16
    }
}

/// One-shot RFC 1071 Internet checksum of `data`.
///
/// Returns the value transmitted in a checksum field (the complement of
/// the fold). Folding a message together with its correct checksum field
/// therefore returns `0`.
#[must_use]
pub fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum = Checksum::new();
    sum.push(data);
    sum.finish()
}

/// The addressing context a transport checksum folds over — the source
/// and destination addresses of one family, from which the family's
/// pseudo-header seed is derived.
///
/// This is the one definition of the transport pseudo-header context:
/// UDP, TCP, and any other protocol whose checksum spans a pseudo-header
/// name the same `Pseudo` and pass their own protocol number to
/// [`Pseudo::seed`], rather than each carrying a private copy of the
/// v4/v6 split.
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
    /// A [`Checksum`] accumulator seeded with this pseudo-header for an
    /// upper-layer datagram of `upper_len` bytes (transport header plus
    /// payload) carrying IP protocol / next-header `protocol`.
    ///
    /// `upper_len` is a 16-bit value for both families: the IPv6
    /// pseudo-header's 32-bit upper-layer length field is written from
    /// the widened value, and TAIRiX does not emit jumbograms, so no
    /// datagram this engine builds exceeds the 16-bit range.
    #[must_use]
    pub fn seed(self, protocol: u8, upper_len: u16) -> Checksum {
        match self {
            Self::V4 {
                source,
                destination,
            } => Checksum::ipv4_pseudo(source, destination, protocol, upper_len),
            Self::V6 {
                source,
                destination,
            } => Checksum::ipv6_pseudo(source, destination, protocol, u32::from(upper_len)),
        }
    }

    /// True for the IPv6 pseudo-header, whose transport checksum is
    /// mandatory (RFC 8200 §8.1).
    #[must_use]
    pub const fn is_v6(self) -> bool {
        matches!(self, Self::V6 { .. })
    }
}

/// Whether a transport-layer parse must fold and verify the checksum,
/// or may trust that a network device already validated it.
///
/// The offloaded value ([`ChecksumCheck::DeviceValidated`]) is passed
/// only when the driver reported the frame as receive-checksum-validated
/// *and* the interface negotiated that offload (`plans/NETWORK.md`
/// §2.3). It suppresses **only** the redundant one's-complement fold;
/// every other validation the parser performs — header lengths, the
/// mandatory-checksum rule for IPv6, the pseudo-header length bound —
/// still runs. The offload is never load-bearing for security: trust is
/// in the device that carried the frame, never in the peer that sent it,
/// and a device that lies can at worst let a corrupt frame reach the
/// same semantic checks a software-folded frame faces.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum ChecksumCheck {
    /// Fold and verify the checksum in software — the canonical path.
    #[default]
    Verify,
    /// The device validated the transport checksum; skip the fold.
    DeviceValidated,
}

impl ChecksumCheck {
    /// Whether the software fold must run.
    #[must_use]
    pub const fn must_verify(self) -> bool {
        matches!(self, ChecksumCheck::Verify)
    }
}

/// How a transport serialiser fills its checksum field on transmit.
///
/// [`Full`](ChecksumMode::Full) is the canonical software path: the
/// serialiser folds the pseudo-header, transport header, and payload and
/// stores the complete one's-complement checksum. [`Partial`](Self::Partial) is used
/// only when the egress interface negotiated transmit-checksum offload
/// (`plans/NETWORK.md` §2.3): the serialiser stores the folded
/// pseudo-header sum alone ([`Checksum::partial`]) and the device
/// completes the fold over the transport bytes. The two produce an
/// identical on-wire checksum once the device (or the software
/// completion) finishes the fold — the offload is never load-bearing for
/// correctness, only a work saving.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum ChecksumMode {
    /// Compute and store the complete transport checksum in software.
    #[default]
    Full,
    /// Store only the pseudo-header partial sum for the device to
    /// complete.
    Partial,
    /// Store the pseudo-header partial sum computed with a **zero**
    /// upper-layer length, for a TCP-segmentation-offload super-segment
    /// (`plans/NETWORK.md` §2.3): the device splits the payload into
    /// MTU-sized segments of differing lengths and adds each segment's
    /// own length to the sum before folding, so the length must not be
    /// pre-folded into the partial checksum (Linux `CHECKSUM_PARTIAL` for
    /// GSO seeds the pseudo-header with length 0 identically).
    PartialGso,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Independent oracle: the textbook fold, written differently from
    /// the incremental accumulator.
    fn oracle(data: &[u8]) -> u16 {
        let mut sum: u64 = 0;
        for pair in data.chunks(2) {
            let word = if pair.len() == 2 {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], 0])
            };
            sum += u64::from(word);
        }
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !((sum & 0xFFFF) as u16)
    }

    #[test]
    fn rfc1071_worked_example() {
        // RFC 1071 §3 example bytes 00 01 f2 03 f4 f5 f6 f7: the
        // one's-complement sum is 0xddf2, so the checksum is !0xddf2.
        let data = [0x00, 0x01, 0xF2, 0x03, 0xF4, 0xF5, 0xF6, 0xF7];
        assert_eq!(internet_checksum(&data), !0xDDF2);
    }

    #[test]
    fn matches_oracle_on_structured_inputs() {
        let mut data = Vec::new();
        for len in 0..64u8 {
            data.clear();
            for i in 0..len {
                data.push(i.wrapping_mul(37).wrapping_add(len));
            }
            assert_eq!(internet_checksum(&data), oracle(&data), "len {len}");
        }
    }

    #[test]
    fn verifying_a_correct_message_folds_to_zero() {
        let mut message = [0x45u8, 0x00, 0x00, 0x1C, 0x12, 0x34, 0x00, 0x00, 0x40, 0x01];
        let checksum = internet_checksum(&message);
        // Lay the checksum into a two-byte field appended to the message.
        let mut with_field = Vec::from(message.as_slice());
        with_field.extend_from_slice(&checksum.to_be_bytes());
        assert_eq!(internet_checksum(&with_field), 0);
        // Any single-bit corruption must be detected.
        message[3] ^= 0x01;
        let mut corrupt = Vec::from(message.as_slice());
        corrupt.extend_from_slice(&checksum.to_be_bytes());
        assert_ne!(internet_checksum(&corrupt), 0);
    }

    #[test]
    fn incremental_split_is_byte_stream_equivalent() {
        let data: Vec<u8> = (0..31u8).map(|i| i.wrapping_mul(211)).collect();
        let expected = internet_checksum(&data);
        for split_a in 0..=data.len() {
            for split_b in split_a..=data.len() {
                let mut sum = Checksum::new();
                sum.push(&data[..split_a]);
                sum.push(&data[split_a..split_b]);
                sum.push(&data[split_b..]);
                assert_eq!(sum.finish(), expected, "splits {split_a}/{split_b}");
            }
        }
    }

    #[test]
    fn ipv4_pseudo_header_equals_contiguous_fold() {
        let src = Ipv4Addr::new(192, 0, 2, 1);
        let dst = Ipv4Addr::new(198, 51, 100, 7);
        let payload = [0xDE, 0xAD, 0xBE, 0xEF, 0x01];
        let upper_len = u16::try_from(payload.len()).expect("fits");
        let mut sum = Checksum::ipv4_pseudo(src, dst, 17, upper_len);
        sum.push(&payload);

        let mut contiguous = Vec::new();
        contiguous.extend_from_slice(&src.octets());
        contiguous.extend_from_slice(&dst.octets());
        contiguous.extend_from_slice(&[0, 17]);
        contiguous.extend_from_slice(&upper_len.to_be_bytes());
        contiguous.extend_from_slice(&payload);
        assert_eq!(sum.finish(), internet_checksum(&contiguous));
    }

    #[test]
    fn ipv6_pseudo_header_equals_contiguous_fold() {
        let src = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1);
        let dst = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 2);
        let payload = [0x00, 0x2A, 0xFF];
        let upper_len = u32::try_from(payload.len()).expect("fits");
        let mut sum = Checksum::ipv6_pseudo(src, dst, 58, upper_len);
        sum.push(&payload);

        let mut contiguous = Vec::new();
        contiguous.extend_from_slice(&src.octets());
        contiguous.extend_from_slice(&dst.octets());
        contiguous.extend_from_slice(&upper_len.to_be_bytes());
        contiguous.extend_from_slice(&[0, 0, 0, 58]);
        contiguous.extend_from_slice(&payload);
        assert_eq!(sum.finish(), internet_checksum(&contiguous));
    }

    #[test]
    fn empty_input_checksum_is_all_ones_complemented() {
        assert_eq!(internet_checksum(&[]), 0xFFFF);
    }
}
