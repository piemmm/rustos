//! IPv6 datagram handling (RFC 8200).
//!
//! [`Ipv6Header`] is the 40-byte fixed header codec. [`walk`] performs
//! the extension-header chain walk a receiving host must make before it
//! reaches the upper-layer protocol: bounded ([`MAX_EXT_HEADERS`]),
//! total, and fail-closed, with the RFC 8200 dispositions for
//! unrecognised headers and options expressed as typed rejections the
//! caller turns into `ICMPv6` Parameter Problem messages (rate-limited,
//! [`crate::icmp`]).
//!
//! A fragment header ends the walk: the remaining bytes belong to the
//! *reassembled* datagram, so the caller feeds them to
//! [`crate::frag::Reassembler`] and walks the reassembled payload again
//! from the fragment header's next-header value.

use crate::addr::Ipv6Addr;

/// Length of the fixed IPv6 header.
pub const IPV6_HEADER_LEN: usize = 40;

/// Smallest MTU every IPv6 link must carry (RFC 8200 §5).
pub const IPV6_MIN_MTU: usize = 1280;

/// Most extension headers accepted in one chain — a fixed validation
/// bound against extension-header flooding, not a growable capacity. A
/// legitimate datagram carries at most one of each of the four headers
/// this host processes; eight leaves honest slack for duplicates the
/// RFCs technically permit (two destination-options headers).
pub const MAX_EXT_HEADERS: usize = 8;

/// Hop-by-Hop Options extension header (RFC 8200 §4.3).
pub const NEXT_HEADER_HOP_BY_HOP: u8 = 0;
/// Routing extension header (RFC 8200 §4.4).
pub const NEXT_HEADER_ROUTING: u8 = 43;
/// Fragment extension header (RFC 8200 §4.5).
pub const NEXT_HEADER_FRAGMENT: u8 = 44;
/// `ICMPv6` (RFC 4443).
pub const NEXT_HEADER_ICMPV6: u8 = 58;
/// "No Next Header" — nothing follows (RFC 8200 §4.7).
pub const NEXT_HEADER_NO_NEXT: u8 = 59;
/// Destination Options extension header (RFC 8200 §4.6).
pub const NEXT_HEADER_DEST_OPTS: u8 = 60;

/// Default hop limit for emitted datagrams.
pub const DEFAULT_HOP_LIMIT: u8 = 64;

/// A parsed or to-be-emitted fixed IPv6 header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv6Header {
    /// Source address.
    pub source: Ipv6Addr,
    /// Destination address.
    pub destination: Ipv6Addr,
    /// The first next-header value (an extension header or the
    /// upper-layer protocol).
    pub next_header: u8,
    /// Remaining hop count.
    pub hop_limit: u8,
    /// Traffic class (DSCP + ECN).
    pub traffic_class: u8,
    /// 20-bit flow label.
    pub flow_label: u32,
}

impl Ipv6Header {
    /// A header with the default hop limit, zero traffic class, and
    /// zero flow label.
    #[must_use]
    pub fn new(source: Ipv6Addr, destination: Ipv6Addr, next_header: u8) -> Self {
        Self {
            source,
            destination,
            next_header,
            hop_limit: DEFAULT_HOP_LIMIT,
            traffic_class: 0,
            flow_label: 0,
        }
    }

    /// Parse a fixed IPv6 header, returning it alongside the payload
    /// the `payload length` field delimits.
    ///
    /// Returns `None` for non-IPv6 versions, a truncated header, or a
    /// payload length beyond `bytes`.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<(Self, &[u8])> {
        let header = bytes.get(..IPV6_HEADER_LEN)?;
        if header[0] >> 4 != 6 {
            return None;
        }
        let payload_length = usize::from(u16::from_be_bytes([header[4], header[5]]));
        let end = IPV6_HEADER_LEN.checked_add(payload_length)?;
        if end > bytes.len() {
            return None;
        }
        let parsed = Self {
            source: address(&header[8..24]),
            destination: address(&header[24..40]),
            next_header: header[6],
            hop_limit: header[7],
            traffic_class: (header[0] << 4) | (header[1] >> 4),
            flow_label: u32::from(header[1] & 0x0F) << 16
                | u32::from(header[2]) << 8
                | u32::from(header[3]),
        };
        Some((parsed, &bytes[IPV6_HEADER_LEN..end]))
    }

    /// Write this header for a datagram carrying `payload_len` bytes.
    ///
    /// Returns `None` when `out` cannot hold the header, when
    /// `payload_len` overflows the 16-bit field, or when
    /// [`Self::flow_label`] does not fit its 20 bits.
    #[must_use]
    pub fn write(&self, out: &mut [u8], payload_len: usize) -> Option<usize> {
        let payload_length = u16::try_from(payload_len).ok()?;
        if self.flow_label > 0x000F_FFFF {
            return None;
        }
        let header = out.get_mut(..IPV6_HEADER_LEN)?;
        header[0] = 0x60 | (self.traffic_class >> 4);
        header[1] = (self.traffic_class << 4) | ((self.flow_label >> 16) & 0x0F) as u8;
        header[2] = ((self.flow_label >> 8) & 0xFF) as u8;
        header[3] = (self.flow_label & 0xFF) as u8;
        header[4..6].copy_from_slice(&payload_length.to_be_bytes());
        header[6] = self.next_header;
        header[7] = self.hop_limit;
        header[8..24].copy_from_slice(&self.source.octets());
        header[24..40].copy_from_slice(&self.destination.octets());
        Some(IPV6_HEADER_LEN)
    }
}

/// Fragment-header facts recorded by [`walk`] (RFC 8200 §4.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FragmentInfo {
    /// Offset of this piece within the reassembled payload, in bytes
    /// (a multiple of 8 — the wire field counts 8-byte units).
    pub offset: u16,
    /// More-Fragments flag.
    pub more: bool,
    /// Identification shared by all fragments of one datagram.
    pub identification: u32,
    /// The header type that follows the reassembled payload's start.
    pub next_header: u8,
}

/// Where a completed [`walk`] arrived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkOutcome<'a> {
    /// The chain reached an upper-layer protocol.
    Upper {
        /// The upper-layer protocol number.
        protocol: u8,
        /// Its payload.
        payload: &'a [u8],
        /// Byte offset, within the whole IPv6 packet, of the
        /// next-header field that named this protocol — the pointer an
        /// RFC 4443 §3.4 code-1 Parameter Problem carries when the
        /// protocol is unrecognised.
        nh_offset: u32,
    },
    /// The chain reached a fragment header: `payload` is this piece of
    /// the fragmented datagram, to be reassembled before any further
    /// headers are interpreted.
    Fragment {
        /// The fragment-header facts, keyed for the reassembler.
        info: FragmentInfo,
        /// This fragment's piece of the reassembled payload.
        payload: &'a [u8],
    },
    /// The chain ended with "No Next Header": nothing to deliver.
    Nothing,
}

/// Why [`walk`] refused a chain, and what the host must do about it
/// (RFC 8200 §4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkRejection {
    /// Discard silently (malformed lengths, over-long chains, an
    /// option whose disposition is "discard silently").
    Drop,
    /// Discard and send an `ICMPv6` Parameter Problem with this code and
    /// pointer (an offset from the start of the fixed IPv6 header),
    /// subject to the caller's rate limiter and multicast rules.
    ParamProblem {
        /// `ICMPv6` Parameter Problem code (RFC 4443 §3.4 / RFC 8200 §4.2).
        code: u8,
        /// Byte offset of the offending field from the start of the
        /// IPv6 header.
        pointer: u32,
    },
}

/// `ICMPv6` Parameter Problem code: unrecognised Next Header (RFC 4443).
pub const PARAM_PROBLEM_NEXT_HEADER: u8 = 1;
/// `ICMPv6` Parameter Problem code: unrecognised IPv6 option (RFC 4443).
pub const PARAM_PROBLEM_OPTION: u8 = 2;
/// `ICMPv6` Parameter Problem code: erroneous header field (RFC 4443).
pub const PARAM_PROBLEM_HEADER_FIELD: u8 = 0;

/// Walk the extension-header chain of `payload` (the bytes after the
/// fixed header), starting from the fixed header's next-header value.
///
/// `dest_is_multicast` selects the RFC 8200 §4.2 `11`-prefixed option
/// disposition (a multicast destination suppresses the Parameter
/// Problem). Hop-by-Hop is honoured only as the first header; routing
/// headers with segments left are refused (this host never forwards);
/// a fragment header ends the walk (see the module docs).
///
/// # Errors
///
/// [`WalkRejection`] carries the RFC 8200 disposition: silent discard,
/// or discard plus a Parameter Problem the caller emits.
pub fn walk(
    first_header: u8,
    payload: &[u8],
    dest_is_multicast: bool,
) -> Result<WalkOutcome<'_>, WalkRejection> {
    let mut next = first_header;
    let mut rest = payload;
    // Offset of `rest` within the whole IPv6 packet, for pointers.
    let mut at = IPV6_HEADER_LEN;
    // Offset of the next-header field that named the current header:
    // byte 6 of the fixed header, then byte 0 of each extension header.
    let mut named_at = 6usize;
    for step in 0..=MAX_EXT_HEADERS {
        match next {
            NEXT_HEADER_HOP_BY_HOP => {
                // Only valid immediately after the fixed header
                // (RFC 8200 §4: any other position is an unrecognised
                // next header pointed at the field that named it).
                if step != 0 {
                    return Err(WalkRejection::ParamProblem {
                        code: PARAM_PROBLEM_NEXT_HEADER,
                        pointer: u32::try_from(named_at).unwrap_or(u32::MAX),
                    });
                }
                let (header_next, body, advanced) = options_header(rest)?;
                options(body, at + 2, dest_is_multicast)?;
                next = header_next;
                named_at = at;
                at += advanced;
                rest = &rest[advanced..];
            }
            NEXT_HEADER_DEST_OPTS => {
                let (header_next, body, advanced) = options_header(rest)?;
                options(body, at + 2, dest_is_multicast)?;
                next = header_next;
                named_at = at;
                at += advanced;
                rest = &rest[advanced..];
            }
            NEXT_HEADER_ROUTING => {
                let (header_next, body, advanced) = options_header(rest)?;
                // body[0] = routing type, body[1] = segments left.
                let segments_left = *body.get(1).ok_or(WalkRejection::Drop)?;
                if segments_left != 0 {
                    // This host does not forward and recognises no
                    // routing type, so any segments left are an error
                    // pointed at the routing-type field (RFC 8200 §4.4).
                    return Err(WalkRejection::ParamProblem {
                        code: PARAM_PROBLEM_HEADER_FIELD,
                        pointer: u32::try_from(at + 2).unwrap_or(u32::MAX),
                    });
                }
                next = header_next;
                named_at = at;
                at += advanced;
                rest = &rest[advanced..];
            }
            NEXT_HEADER_FRAGMENT => {
                let header = rest.get(..8).ok_or(WalkRejection::Drop)?;
                let offset_flags = u16::from_be_bytes([header[2], header[3]]);
                let info = FragmentInfo {
                    offset: (offset_flags >> 3) * 8,
                    more: offset_flags & 0x0001 != 0,
                    identification: u32::from_be_bytes([
                        header[4], header[5], header[6], header[7],
                    ]),
                    next_header: header[0],
                };
                return Ok(WalkOutcome::Fragment {
                    info,
                    payload: &rest[8..],
                });
            }
            NEXT_HEADER_NO_NEXT => return Ok(WalkOutcome::Nothing),
            protocol => {
                return Ok(WalkOutcome::Upper {
                    protocol,
                    payload: rest,
                    nh_offset: u32::try_from(named_at).unwrap_or(u32::MAX),
                })
            }
        }
    }
    // Chain longer than the fixed bound: fail closed, silently.
    Err(WalkRejection::Drop)
}

/// Split one options-shaped extension header (Hop-by-Hop, Destination
/// Options, Routing — all `next | len | body`): returns its next-header
/// value, its body after the two fixed bytes, and its total length.
fn options_header(rest: &[u8]) -> Result<(u8, &[u8], usize), WalkRejection> {
    let total = 8usize
        .checked_add(usize::from(*rest.get(1).ok_or(WalkRejection::Drop)?) * 8)
        .ok_or(WalkRejection::Drop)?;
    let header = rest.get(..total).ok_or(WalkRejection::Drop)?;
    Ok((header[0], &header[2..], total))
}

/// Validate an options area (RFC 8200 §4.2), applying the unrecognised-
/// option dispositions. `base` is the byte offset of `body` within the
/// IPv6 packet, for Parameter Problem pointers.
fn options(body: &[u8], base: usize, dest_is_multicast: bool) -> Result<(), WalkRejection> {
    let mut i = 0usize;
    while i < body.len() {
        let option_type = body[i];
        if option_type == 0 {
            // Pad1: a lone byte.
            i += 1;
            continue;
        }
        let len = usize::from(*body.get(i + 1).ok_or(WalkRejection::Drop)?);
        let end = i.checked_add(2 + len).ok_or(WalkRejection::Drop)?;
        if end > body.len() {
            return Err(WalkRejection::Drop);
        }
        if option_type != 1 {
            // Not PadN: an option this host does not recognise (it
            // implements none). Disposition is the type's top two bits.
            let pointer = u32::try_from(base + i).unwrap_or(u32::MAX);
            match option_type >> 6 {
                0b00 => {}
                0b01 => return Err(WalkRejection::Drop),
                0b10 => {
                    return Err(WalkRejection::ParamProblem {
                        code: PARAM_PROBLEM_OPTION,
                        pointer,
                    })
                }
                _ => {
                    if dest_is_multicast {
                        return Err(WalkRejection::Drop);
                    }
                    return Err(WalkRejection::ParamProblem {
                        code: PARAM_PROBLEM_OPTION,
                        pointer,
                    });
                }
            }
        }
        i = end;
    }
    Ok(())
}

fn address(bytes: &[u8]) -> Ipv6Addr {
    let mut octets = [0u8; 16];
    octets.copy_from_slice(bytes);
    Ipv6Addr::from(octets)
}

#[cfg(test)]
#[path = "ipv6_tests.rs"]
mod tests;
