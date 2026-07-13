//! ICMP and `ICMPv6` (RFC 792, RFC 4443) over one shared machinery.
//!
//! Both families share the wire shape `type | code | checksum | body`;
//! they differ only in their type numbers and in the checksum seed
//! (`ICMPv6` folds the RFC 8200 §8.1 pseudo-header, ICMP does not). That
//! difference lives once, in [`IcmpContext`]; the echo codec
//! ([`IcmpEcho`]) and the error codec ([`IcmpError`]) are written once
//! over it.
//!
//! Error generation is rate-limited by the caller through
//! [`ErrorRateLimiter`] (a token bucket, RFC 4443 §2.4(f)) and gated by
//! [`error_allowed`] — no error about an error, none triggered by
//! multicast except where the RFC allows it — so this host is never an
//! amplification vector.

use crate::addr::Ipv6Addr;
use crate::checksum::{internet_checksum, Checksum};
use crate::ipv6::NEXT_HEADER_ICMPV6;
use rustos_abi::time::{Duration64, NANOS_PER_SEC};

/// Length of the fixed 4-byte ICMP/`ICMPv6` header (type, code, checksum).
pub const ICMP_FIXED_HEADER_LEN: usize = 4;

/// Length of the echo header (fixed header plus identifier/sequence).
pub const ICMP_HEADER_LEN: usize = 8;

/// ICMP type for an echo request.
pub const TYPE_ECHO_REQUEST: u8 = 8;
/// ICMP type for an echo reply.
pub const TYPE_ECHO_REPLY: u8 = 0;
/// ICMP type for destination unreachable.
pub const TYPE_DEST_UNREACHABLE: u8 = 3;
/// ICMP type for time exceeded.
pub const TYPE_TIME_EXCEEDED: u8 = 11;
/// ICMP type for parameter problem.
pub const TYPE_PARAM_PROBLEM: u8 = 12;

/// `ICMPv6` type for destination unreachable.
pub const TYPE_V6_DEST_UNREACHABLE: u8 = 1;
/// `ICMPv6` type for packet too big.
pub const TYPE_V6_PACKET_TOO_BIG: u8 = 2;
/// `ICMPv6` type for time exceeded.
pub const TYPE_V6_TIME_EXCEEDED: u8 = 3;
/// `ICMPv6` type for parameter problem.
pub const TYPE_V6_PARAM_PROBLEM: u8 = 4;
/// `ICMPv6` type for an echo request.
pub const TYPE_V6_ECHO_REQUEST: u8 = 128;
/// `ICMPv6` type for an echo reply.
pub const TYPE_V6_ECHO_REPLY: u8 = 129;

/// The family-specific facts one ICMP message needs: which family's
/// type numbers apply and, for `ICMPv6`, the pseudo-header addresses its
/// checksum folds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IcmpContext {
    /// ICMP for IPv4: no pseudo-header.
    V4,
    /// `ICMPv6`: the checksum folds the RFC 8200 §8.1 pseudo-header.
    V6 {
        /// IPv6 source address of the packet carrying the message.
        source: Ipv6Addr,
        /// IPv6 destination address of the packet carrying the message.
        destination: Ipv6Addr,
    },
}

impl IcmpContext {
    /// Verify the checksum of a whole ICMP message under this context.
    #[must_use]
    fn verify(&self, message: &[u8]) -> bool {
        match self {
            Self::V4 => internet_checksum(message) == 0,
            Self::V6 {
                source,
                destination,
            } => {
                let Ok(len) = u32::try_from(message.len()) else {
                    return false;
                };
                let mut sum = Checksum::ipv6_pseudo(*source, *destination, NEXT_HEADER_ICMPV6, len);
                sum.push(message);
                sum.finish() == 0
            }
        }
    }

    /// Compute the checksum field value for a message whose checksum
    /// bytes are currently zero.
    #[must_use]
    fn seal(&self, message: &[u8]) -> u16 {
        match self {
            Self::V4 => internet_checksum(message),
            Self::V6 {
                source,
                destination,
            } => {
                let len = u32::try_from(message.len()).unwrap_or(u32::MAX);
                let mut sum = Checksum::ipv6_pseudo(*source, *destination, NEXT_HEADER_ICMPV6, len);
                sum.push(message);
                sum.finish()
            }
        }
    }

    /// True when this context is the `ICMPv6` family.
    #[must_use]
    pub fn is_v6(&self) -> bool {
        matches!(self, Self::V6 { .. })
    }
}

/// A checksum-verified ICMP/`ICMPv6` message split into its fixed header
/// and body — the one entry point every typed decoder (echo, error,
/// [`crate::nd`]) builds on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IcmpMessage<'a> {
    /// Message type (family-specific numbering).
    pub message_type: u8,
    /// Message code.
    pub code: u8,
    /// Message body after the 4-byte fixed header.
    pub body: &'a [u8],
}

impl<'a> IcmpMessage<'a> {
    /// Split and checksum-verify a raw ICMP/`ICMPv6` message.
    ///
    /// Returns `None` for a truncated message or a failed checksum.
    #[must_use]
    pub fn parse(context: IcmpContext, bytes: &'a [u8]) -> Option<Self> {
        let header = bytes.get(..ICMP_FIXED_HEADER_LEN)?;
        if !context.verify(bytes) {
            return None;
        }
        Some(Self {
            message_type: header[0],
            code: header[1],
            body: &bytes[ICMP_FIXED_HEADER_LEN..],
        })
    }

    /// Serialise this message into `out`, filling in the checksum.
    ///
    /// Returns `None` when `out` cannot hold the message.
    #[must_use]
    pub fn write(&self, context: IcmpContext, out: &mut [u8]) -> Option<usize> {
        let total = ICMP_FIXED_HEADER_LEN.checked_add(self.body.len())?;
        let message = out.get_mut(..total)?;
        message[0] = self.message_type;
        message[1] = self.code;
        message[2..4].copy_from_slice(&0u16.to_be_bytes());
        message[ICMP_FIXED_HEADER_LEN..].copy_from_slice(self.body);
        let checksum = context.seal(message);
        message[2..4].copy_from_slice(&checksum.to_be_bytes());
        Some(total)
    }
}

/// Whether an echo message is a request or a reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EchoKind {
    /// An echo request to be answered.
    Request,
    /// An echo reply answering an earlier request.
    Reply,
}

impl EchoKind {
    /// The wire type number for this kind under `context`'s family.
    #[must_use]
    fn wire_type(self, context: IcmpContext) -> u8 {
        match (self, context.is_v6()) {
            (Self::Request, false) => TYPE_ECHO_REQUEST,
            (Self::Reply, false) => TYPE_ECHO_REPLY,
            (Self::Request, true) => TYPE_V6_ECHO_REQUEST,
            (Self::Reply, true) => TYPE_V6_ECHO_REPLY,
        }
    }

    /// The kind a wire type number names under `context`'s family.
    #[must_use]
    fn of_wire_type(message_type: u8, context: IcmpContext) -> Option<Self> {
        match (message_type, context.is_v6()) {
            (TYPE_ECHO_REQUEST, false) | (TYPE_V6_ECHO_REQUEST, true) => Some(Self::Request),
            (TYPE_ECHO_REPLY, false) | (TYPE_V6_ECHO_REPLY, true) => Some(Self::Reply),
            _ => None,
        }
    }
}

/// A parsed ICMP/`ICMPv6` echo message borrowing its payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IcmpEcho<'a> {
    /// Request or reply.
    pub kind: EchoKind,
    /// Echo identifier, echoed back unchanged.
    pub identifier: u16,
    /// Echo sequence number, echoed back unchanged.
    pub sequence: u16,
    /// Opaque echo payload, echoed back unchanged.
    pub payload: &'a [u8],
}

impl<'a> IcmpEcho<'a> {
    /// Parse an echo message, verifying its checksum under `context`.
    ///
    /// Returns `None` for a truncated message, a non-echo type for the
    /// context's family, a non-zero code, or a failed checksum.
    #[must_use]
    pub fn parse(context: IcmpContext, bytes: &'a [u8]) -> Option<Self> {
        let message = IcmpMessage::parse(context, bytes)?;
        let kind = EchoKind::of_wire_type(message.message_type, context)?;
        if message.code != 0 {
            return None;
        }
        let body = message.body.get(..4)?;
        Some(Self {
            kind,
            identifier: u16::from_be_bytes([body[0], body[1]]),
            sequence: u16::from_be_bytes([body[2], body[3]]),
            payload: &message.body[4..],
        })
    }

    /// Build the echo reply that answers this request, preserving its
    /// identifier, sequence, and payload.
    #[must_use]
    pub fn reply(&self) -> Self {
        Self {
            kind: EchoKind::Reply,
            ..*self
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
    pub fn write(&self, context: IcmpContext, out: &mut [u8]) -> Option<usize> {
        let total = self.wire_len();
        let message = out.get_mut(..total)?;
        message[0] = self.kind.wire_type(context);
        message[1] = 0;
        message[2..4].copy_from_slice(&0u16.to_be_bytes());
        message[4..6].copy_from_slice(&self.identifier.to_be_bytes());
        message[6..8].copy_from_slice(&self.sequence.to_be_bytes());
        message[ICMP_HEADER_LEN..].copy_from_slice(self.payload);
        let checksum = context.seal(message);
        message[2..4].copy_from_slice(&checksum.to_be_bytes());
        Some(total)
    }
}

/// Longest invoking-packet excerpt an ICMP (v4) error carries: the RFC
/// 1122 §3.2.2 "as much as possible without exceeding 576 bytes" bound
/// minus the IPv4 and ICMP headers.
pub const MAX_ERROR_EXCERPT_V4: usize = 576 - crate::ipv4::IPV4_HEADER_LEN - ICMP_HEADER_LEN;

/// Longest invoking-packet excerpt an `ICMPv6` error carries: the RFC
/// 4443 §2.4(c) minimum-MTU bound minus the IPv6 and `ICMPv6` headers.
pub const MAX_ERROR_EXCERPT_V6: usize =
    crate::ipv6::IPV6_MIN_MTU - crate::ipv6::IPV6_HEADER_LEN - ICMP_HEADER_LEN;

/// The error conditions this host reports, in one family-neutral
/// vocabulary (RFC 792 / RFC 4443).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IcmpErrorKind {
    /// Destination unreachable, with the family-specific code.
    DestinationUnreachable {
        /// Family-specific code (e.g. port unreachable: 3 for v4, 4
        /// for v6).
        code: u8,
    },
    /// The packet exceeded the next-hop MTU. On the wire this is
    /// `ICMPv6` Packet Too Big, or ICMP Destination Unreachable code 4
    /// ("fragmentation needed and DF set") with the RFC 1191 next-hop
    /// MTU field.
    PacketTooBig {
        /// The next-hop MTU the sender must not exceed.
        mtu: u32,
    },
    /// Hop limit / TTL exhausted, or fragment reassembly timed out.
    TimeExceeded {
        /// Code: 0 = hop limit in transit, 1 = reassembly timeout.
        code: u8,
    },
    /// A header field or option was unprocessable.
    ParameterProblem {
        /// Family-specific code.
        code: u8,
        /// Byte offset of the offending field within the invoking
        /// packet (v4 narrows this to its one-byte pointer field).
        pointer: u32,
    },
}

/// ICMP Destination Unreachable code 4: fragmentation needed and DF
/// set (RFC 792 / RFC 1191).
const V4_CODE_FRAG_NEEDED: u8 = 4;

/// An ICMP/`ICMPv6` error message about an invoking packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IcmpError<'a> {
    /// What went wrong.
    pub kind: IcmpErrorKind,
    /// The leading excerpt of the invoking packet (starting at its IP
    /// header), already bounded to the family's excerpt limit.
    pub invoking: &'a [u8],
}

impl<'a> IcmpError<'a> {
    /// Build an error about `invoking_packet` (the packet from its IP
    /// header), truncating the excerpt to the family bound.
    #[must_use]
    pub fn about(kind: IcmpErrorKind, invoking_packet: &'a [u8], v6: bool) -> Self {
        let bound = if v6 {
            MAX_ERROR_EXCERPT_V6
        } else {
            MAX_ERROR_EXCERPT_V4
        };
        Self {
            kind,
            invoking: &invoking_packet[..core::cmp::min(invoking_packet.len(), bound)],
        }
    }

    /// Parse an error message, verifying its checksum under `context`.
    ///
    /// Returns `None` for a truncated message, a non-error type for the
    /// context's family, or a failed checksum. A v4 Parameter Problem
    /// carries its pointer in one byte; a v4 Packet Too Big arrives as
    /// Destination Unreachable code 4 with the RFC 1191 MTU field.
    #[must_use]
    pub fn parse(context: IcmpContext, bytes: &'a [u8]) -> Option<Self> {
        let message = IcmpMessage::parse(context, bytes)?;
        let body = message.body.get(..4)?;
        let rest = &message.body[4..];
        let kind = if context.is_v6() {
            match message.message_type {
                TYPE_V6_DEST_UNREACHABLE => {
                    IcmpErrorKind::DestinationUnreachable { code: message.code }
                }
                TYPE_V6_PACKET_TOO_BIG => IcmpErrorKind::PacketTooBig {
                    mtu: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
                },
                TYPE_V6_TIME_EXCEEDED => IcmpErrorKind::TimeExceeded { code: message.code },
                TYPE_V6_PARAM_PROBLEM => IcmpErrorKind::ParameterProblem {
                    code: message.code,
                    pointer: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
                },
                _ => return None,
            }
        } else {
            match message.message_type {
                TYPE_DEST_UNREACHABLE if message.code == V4_CODE_FRAG_NEEDED => {
                    IcmpErrorKind::PacketTooBig {
                        mtu: u32::from(u16::from_be_bytes([body[2], body[3]])),
                    }
                }
                TYPE_DEST_UNREACHABLE => {
                    IcmpErrorKind::DestinationUnreachable { code: message.code }
                }
                TYPE_TIME_EXCEEDED => IcmpErrorKind::TimeExceeded { code: message.code },
                TYPE_PARAM_PROBLEM => IcmpErrorKind::ParameterProblem {
                    code: message.code,
                    pointer: u32::from(body[0]),
                },
                _ => return None,
            }
        };
        Some(Self {
            kind,
            invoking: rest,
        })
    }

    /// Total wire length of this message.
    #[must_use]
    pub fn wire_len(&self) -> usize {
        ICMP_HEADER_LEN + self.invoking.len()
    }

    /// Serialise this message into `out`, filling in the checksum.
    ///
    /// Returns `None` when `out` cannot hold the message, when the
    /// excerpt exceeds the family bound, or when the kind is not
    /// representable in the context's family (a v4 Parameter Problem
    /// pointer beyond one byte, a v4 MTU beyond 16 bits).
    #[must_use]
    pub fn write(&self, context: IcmpContext, out: &mut [u8]) -> Option<usize> {
        let v6 = context.is_v6();
        let bound = if v6 {
            MAX_ERROR_EXCERPT_V6
        } else {
            MAX_ERROR_EXCERPT_V4
        };
        if self.invoking.len() > bound {
            return None;
        }
        let (message_type, code, second_word) = if v6 {
            match self.kind {
                IcmpErrorKind::DestinationUnreachable { code } => {
                    (TYPE_V6_DEST_UNREACHABLE, code, 0u32)
                }
                IcmpErrorKind::PacketTooBig { mtu } => (TYPE_V6_PACKET_TOO_BIG, 0, mtu),
                IcmpErrorKind::TimeExceeded { code } => (TYPE_V6_TIME_EXCEEDED, code, 0),
                IcmpErrorKind::ParameterProblem { code, pointer } => {
                    (TYPE_V6_PARAM_PROBLEM, code, pointer)
                }
            }
        } else {
            match self.kind {
                IcmpErrorKind::DestinationUnreachable { code } => {
                    (TYPE_DEST_UNREACHABLE, code, 0u32)
                }
                IcmpErrorKind::PacketTooBig { mtu } => {
                    let mtu = u16::try_from(mtu).ok()?;
                    (TYPE_DEST_UNREACHABLE, V4_CODE_FRAG_NEEDED, u32::from(mtu))
                }
                IcmpErrorKind::TimeExceeded { code } => (TYPE_TIME_EXCEEDED, code, 0),
                IcmpErrorKind::ParameterProblem { code, pointer } => {
                    let pointer = u8::try_from(pointer).ok()?;
                    (TYPE_PARAM_PROBLEM, code, u32::from(pointer) << 24)
                }
            }
        };
        let total = self.wire_len();
        let message = out.get_mut(..total)?;
        message[0] = message_type;
        message[1] = code;
        message[2..4].copy_from_slice(&0u16.to_be_bytes());
        message[4..8].copy_from_slice(&second_word.to_be_bytes());
        message[ICMP_HEADER_LEN..].copy_from_slice(self.invoking);
        let checksum = context.seal(message);
        message[2..4].copy_from_slice(&checksum.to_be_bytes());
        Some(total)
    }
}

/// Facts about a received packet that decide whether an ICMP error may
/// be generated about it (RFC 4443 §2.4(e), RFC 1122 §3.2.2).
///
/// Four independent yes/no facts about one packet, not a state machine
/// — the `struct_excessive_bools` lint is off target here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorContext {
    /// The invoking packet was itself an ICMP/`ICMPv6` error (or a
    /// message the receiver could not classify — fail closed).
    pub invoking_is_icmp_error: bool,
    /// The invoking packet's destination was a multicast (or link-layer
    /// broadcast) address.
    pub dest_is_multicast: bool,
    /// The invoking packet's source does not identify a single node
    /// (unspecified, multicast, or a known broadcast address).
    pub source_is_ambiguous: bool,
    /// The error is one of the multicast exceptions: Packet Too Big, or
    /// Parameter Problem code 2 about an unrecognised option whose
    /// disposition demanded the report (RFC 4443 §2.4(e.2)).
    pub multicast_exception: bool,
}

/// Whether an ICMP/`ICMPv6` error may be generated about a packet.
///
/// Never about an error, never to an ambiguous source, and never about
/// a multicast-addressed packet except for the RFC's two exceptions.
#[must_use]
pub fn error_allowed(context: ErrorContext) -> bool {
    if context.invoking_is_icmp_error || context.source_is_ambiguous {
        return false;
    }
    if context.dest_is_multicast && !context.multicast_exception {
        return false;
    }
    true
}

/// Token-bucket limiter for ICMP/`ICMPv6` error generation (RFC 4443
/// §2.4(f)): at most `burst` errors at once, refilled at `per_second`.
///
/// Pure and `now`-driven like every stateful engine in this crate: the
/// caller asks [`Self::allow`] before emitting each error and drops the
/// error (silently — suppression is the defence) when refused.
#[derive(Clone, Debug)]
pub struct ErrorRateLimiter {
    /// Tokens scaled by [`NANOS_PER_SEC`], so refill needs no division.
    tokens: u64,
    /// Bucket capacity in scaled tokens.
    capacity: u64,
    /// Refill rate in scaled tokens per second (= errors per second).
    per_second: u64,
    /// Monotonic nanoseconds of the last refill.
    last: u128,
}

/// One scaled token: the cost of one error message.
const TOKEN: u64 = NANOS_PER_SEC as u64;

impl ErrorRateLimiter {
    /// A limiter allowing bursts of `burst` errors, refilled at
    /// `per_second` errors per second. Zero values fail closed: a zero
    /// burst or rate allows nothing.
    #[must_use]
    pub fn new(burst: u32, per_second: u32) -> Self {
        Self {
            tokens: u64::from(burst).saturating_mul(TOKEN),
            capacity: u64::from(burst).saturating_mul(TOKEN),
            per_second: u64::from(per_second),
            last: 0,
        }
    }

    /// Take one error's worth of budget at time `now`; `false` means
    /// the error must be suppressed.
    pub fn allow(&mut self, now: Duration64) -> bool {
        let now = duration_nanos(now);
        let elapsed = now.saturating_sub(self.last);
        self.last = now;
        let refill = u64::try_from(elapsed)
            .unwrap_or(u64::MAX)
            .saturating_mul(self.per_second);
        self.tokens = core::cmp::min(self.capacity, self.tokens.saturating_add(refill));
        if self.tokens >= TOKEN {
            self.tokens -= TOKEN;
            true
        } else {
            false
        }
    }
}

/// Nanoseconds of a non-negative monotonic duration (negative inputs
/// saturate to zero rather than wrapping).
fn duration_nanos(d: Duration64) -> u128 {
    let secs = u128::try_from(d.secs()).unwrap_or(0);
    secs * u128::from(NANOS_PER_SEC) + u128::from(d.subsec_nanos())
}

#[cfg(test)]
#[path = "icmp_tests.rs"]
mod tests;
