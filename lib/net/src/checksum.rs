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
    pub fn finish(mut self) -> u16 {
        if let Some(high) = self.pending.take() {
            self.sum += u32::from(u16::from_be_bytes([high, 0]));
        }
        let mut sum = self.sum;
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !((sum & 0xFFFF) as u16)
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
