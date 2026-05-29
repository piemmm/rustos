//! ICMP echo request/reply handling (RFC 792).
//!
//! Only the echo service (types 8 and 0) is implemented; every other
//! ICMP type is rejected by [`IcmpEcho::parse`] so the responder
//! answers pings and nothing else.

use crate::internet_checksum;

/// Length of the fixed ICMP echo header preceding the payload.
pub const ICMP_HEADER_LEN: usize = 8;

/// ICMP type for an echo request.
pub const TYPE_ECHO_REQUEST: u8 = 8;

/// ICMP type for an echo reply.
pub const TYPE_ECHO_REPLY: u8 = 0;

/// A parsed ICMP echo message borrowing its payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IcmpEcho<'a> {
    /// Message type ([`TYPE_ECHO_REQUEST`] or [`TYPE_ECHO_REPLY`]).
    pub message_type: u8,
    /// Echo identifier, echoed back unchanged.
    pub identifier: u16,
    /// Echo sequence number, echoed back unchanged.
    pub sequence: u16,
    /// Opaque echo payload, echoed back unchanged.
    pub payload: &'a [u8],
}

impl<'a> IcmpEcho<'a> {
    /// Parse an ICMP echo message, verifying its checksum.
    ///
    /// Returns `None` for a truncated message, a non-echo type, a
    /// non-zero code, or a failed checksum.
    #[must_use]
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let header = bytes.get(..ICMP_HEADER_LEN)?;
        let message_type = header[0];
        let code = header[1];
        if (message_type != TYPE_ECHO_REQUEST && message_type != TYPE_ECHO_REPLY) || code != 0 {
            return None;
        }
        if internet_checksum(bytes) != 0 {
            return None;
        }
        Some(Self {
            message_type,
            identifier: u16::from_be_bytes([header[4], header[5]]),
            sequence: u16::from_be_bytes([header[6], header[7]]),
            payload: &bytes[ICMP_HEADER_LEN..],
        })
    }

    /// Build the echo reply that answers this request, preserving its
    /// identifier, sequence, and payload.
    #[must_use]
    pub fn reply(&self) -> Self {
        Self {
            message_type: TYPE_ECHO_REPLY,
            identifier: self.identifier,
            sequence: self.sequence,
            payload: self.payload,
        }
    }

    /// Total wire length of this message (header plus payload).
    #[must_use]
    pub fn wire_len(&self) -> usize {
        ICMP_HEADER_LEN + self.payload.len()
    }

    /// Serialise this message into `out`, filling in the checksum.
    ///
    /// Returns `None` when `out` cannot hold the header and payload.
    #[must_use]
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        let total = self.wire_len();
        let message = out.get_mut(..total)?;
        message[0] = self.message_type;
        message[1] = 0;
        message[2..4].copy_from_slice(&0u16.to_be_bytes());
        message[4..6].copy_from_slice(&self.identifier.to_be_bytes());
        message[6..8].copy_from_slice(&self.sequence.to_be_bytes());
        message[ICMP_HEADER_LEN..total].copy_from_slice(self.payload);
        let checksum = internet_checksum(message);
        message[2..4].copy_from_slice(&checksum.to_be_bytes());
        Some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_bytes(payload: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        let echo = IcmpEcho {
            message_type: TYPE_ECHO_REQUEST,
            identifier: 0x1234,
            sequence: 0x0001,
            payload,
        };
        let len = echo.write(&mut out).expect("fits");
        let mut sized = [0u8; 64];
        sized[..len].copy_from_slice(&out[..len]);
        sized
    }

    #[test]
    fn parse_round_trips_and_verifies_checksum() {
        let payload = [0xAB, 0xCD, 0xEF];
        let bytes = request_bytes(&payload);
        let echo = IcmpEcho::parse(&bytes[..ICMP_HEADER_LEN + payload.len()]).expect("parses");
        assert_eq!(echo.message_type, TYPE_ECHO_REQUEST);
        assert_eq!(echo.identifier, 0x1234);
        assert_eq!(echo.sequence, 0x0001);
        assert_eq!(echo.payload, &payload);
    }

    #[test]
    fn parse_rejects_bad_checksum() {
        let mut bytes = request_bytes(&[1, 2, 3]);
        bytes[2] ^= 0xFF; // corrupt the checksum field
        assert!(IcmpEcho::parse(&bytes[..ICMP_HEADER_LEN + 3]).is_none());
    }

    #[test]
    fn parse_rejects_non_echo_type() {
        let mut bytes = request_bytes(&[1, 2, 3]);
        bytes[0] = 3; // destination unreachable
                      // Recompute checksum so only the type check can reject it.
        bytes[2..4].copy_from_slice(&0u16.to_be_bytes());
        let csum = internet_checksum(&bytes[..ICMP_HEADER_LEN + 3]);
        bytes[2..4].copy_from_slice(&csum.to_be_bytes());
        assert!(IcmpEcho::parse(&bytes[..ICMP_HEADER_LEN + 3]).is_none());
    }

    #[test]
    fn parse_rejects_truncated() {
        assert!(IcmpEcho::parse(&[0u8; ICMP_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn reply_preserves_identity_and_round_trips() {
        let payload = [9, 8, 7, 6, 5];
        let bytes = request_bytes(&payload);
        let request = IcmpEcho::parse(&bytes[..ICMP_HEADER_LEN + payload.len()]).expect("parses");
        let reply = request.reply();
        assert_eq!(reply.message_type, TYPE_ECHO_REPLY);
        assert_eq!(reply.identifier, request.identifier);
        assert_eq!(reply.sequence, request.sequence);

        let mut out = [0u8; 64];
        let len = reply.write(&mut out).expect("fits");
        let parsed = IcmpEcho::parse(&out[..len]).expect("reply parses");
        assert_eq!(parsed.message_type, TYPE_ECHO_REPLY);
        assert_eq!(parsed.payload, &payload);
    }

    #[test]
    fn write_rejects_short_buffer() {
        let echo = IcmpEcho {
            message_type: TYPE_ECHO_REPLY,
            identifier: 1,
            sequence: 1,
            payload: &[0; 8],
        };
        let mut out = [0u8; ICMP_HEADER_LEN + 7];
        assert!(echo.write(&mut out).is_none());
    }
}
