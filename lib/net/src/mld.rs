//! Multicast Listener Discovery — MLDv2 (RFC 3810), MLDv1-query aware.
//!
//! MLD is the IPv6 analogue of IGMP, carried as `ICMPv6` (this host folds
//! its checksum through [`crate::icmp::IcmpContext`], so this module works
//! purely on the message *body* — the bytes after the four-byte
//! `ICMPv6` header — and never re-implements checksumming).
//!
//! A host is a *listener*: it decodes Multicast Listener Queries and
//! emits Version 2 Multicast Listener Reports describing the groups it
//! belongs to. There is deliberately no report *decoder* here: unlike
//! IGMPv2, MLDv2 has no report suppression (RFC 3810 §7), so a host never
//! acts on another host's report — adding a parser for one would be
//! surface with no consumer.
//!
//! Every decoder is total, bounded, and fail-closed.

use crate::addr::Ipv6Addr;

/// `ICMPv6` type of a Multicast Listener Query (MLDv1 and MLDv2 share it).
pub const TYPE_MULTICAST_LISTENER_QUERY: u8 = 130;
/// `ICMPv6` type of an MLDv1 Multicast Listener Report.
pub const TYPE_MLDV1_REPORT: u8 = 131;
/// `ICMPv6` type of an MLDv1 Multicast Listener Done.
pub const TYPE_MLDV1_DONE: u8 = 132;
/// `ICMPv6` type of a Version 2 Multicast Listener Report.
pub const TYPE_MLDV2_REPORT: u8 = 143;

/// Length of an MLDv1 query/report/done body (after the `ICMPv6` header):
/// Maximum Response Code (2), Reserved (2), Multicast Address (16).
pub const MLDV1_BODY_LEN: usize = 20;

/// Smallest MLDv2 query body: the MLDv1 fields plus S/QRV (1), QQIC (1),
/// and Number of Sources (2).
pub const MLDV2_QUERY_MIN_BODY_LEN: usize = 24;

/// Length of one MLDv2 Multicast Address Record carrying no sources and
/// no auxiliary data: Record Type (1), Aux Data Len (1), Number of
/// Sources (2), Multicast Address (16).
pub const MCAST_RECORD_LEN: usize = 20;

/// The all-MLDv2-capable-routers link-local group (`ff02::16`,
/// RFC 3810 §5.2.14) — the destination of every Version 2 report.
pub const ALL_MLDV2_ROUTERS: Ipv6Addr = Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 0, 0x16);

/// A Multicast Address Record type this host emits (RFC 3810 §5.2.12).
///
/// This host only ever operates in EXCLUDE `{}` mode (any-source
/// membership), so it emits exactly these three record types; the
/// source-specific types are not produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RecordType {
    /// Current membership: the host is in EXCLUDE mode for the group
    /// (answers a query).
    ModeIsExclude = 2,
    /// State change: the host has left the group (moved to INCLUDE `{}`).
    ChangeToInclude = 3,
    /// State change: the host has joined the group (moved to EXCLUDE `{}`).
    ChangeToExclude = 4,
}

impl RecordType {
    /// The wire value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A decoded Multicast Listener Query, in either version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MldQuery {
    /// Maximum time a listener may delay a response, in milliseconds
    /// (RFC 3810 §5.1.3 — the MLDv2 floating-point code is decoded here).
    pub max_response_millis: u32,
    /// The queried group, or the unspecified address for a General Query.
    pub multicast_address: Ipv6Addr,
    /// True when the query is MLDv2 (carries the S/QRV/QQIC/source
    /// fields); false for a bare MLDv1 query.
    pub is_v2: bool,
}

impl MldQuery {
    /// True for a General Query (multicast address unspecified).
    #[must_use]
    pub fn is_general(&self) -> bool {
        self.multicast_address.is_unspecified()
    }

    /// Decode a Multicast Listener Query from the `ICMPv6` message body
    /// (the bytes after the four-byte header; its checksum is verified
    /// upstream by [`crate::icmp::IcmpMessage::parse`]).
    ///
    /// Returns `None` (fail closed) when `body` is shorter than an MLDv1
    /// query. A body of at least [`MLDV2_QUERY_MIN_BODY_LEN`] is read as
    /// MLDv2 (RFC 3810 §8.1); the source list a longer MLDv2 query may
    /// carry is not needed by an any-source listener and is ignored.
    #[must_use]
    pub fn parse(body: &[u8]) -> Option<Self> {
        let fixed = body.get(..MLDV1_BODY_LEN)?;
        let max_response_code = u16::from_be_bytes([fixed[0], fixed[1]]);
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&fixed[4..20]);
        let multicast_address = Ipv6Addr::from(octets);
        let is_v2 = body.len() >= MLDV2_QUERY_MIN_BODY_LEN;
        let max_response_millis = if is_v2 {
            decode_max_response_code(max_response_code)
        } else {
            u32::from(max_response_code)
        };
        Some(Self {
            max_response_millis,
            multicast_address,
            is_v2,
        })
    }
}

/// Decode an MLDv2 Maximum Response Code into milliseconds (RFC 3810
/// §5.1.3): linear below `0x8000`, floating-point at or above it.
#[must_use]
fn decode_max_response_code(code: u16) -> u32 {
    if code < 0x8000 {
        return u32::from(code);
    }
    let mant = u32::from(code & 0x0FFF);
    let exp = u32::from((code >> 12) & 0x0007);
    (mant | 0x1000) << (exp + 3)
}

/// Write the body of a Version 2 Multicast Listener Report describing the
/// `(record type, group)` pairs in `records`, each with no sources and
/// no auxiliary data. This is the body after the four-byte `ICMPv6`
/// header: Reserved (2), Number of Records (2), then the records.
///
/// Returns the number of bytes written.
///
/// # Errors
///
/// Returns `None` when `out` cannot hold the report or when `records`
/// exceeds the 16-bit record-count field.
#[must_use]
pub fn write_v2_report(records: &[(RecordType, Ipv6Addr)], out: &mut [u8]) -> Option<usize> {
    let count = u16::try_from(records.len()).ok()?;
    let total = 4usize.checked_add(records.len().checked_mul(MCAST_RECORD_LEN)?)?;
    let buf = out.get_mut(..total)?;
    buf.fill(0);
    buf[2..4].copy_from_slice(&count.to_be_bytes());
    let mut cursor = 4;
    for (record_type, group) in records {
        buf[cursor] = record_type.as_u8();
        // Aux Data Len (cursor+1) and Number of Sources (cursor+2..4)
        // stay zero: no sources, no auxiliary data.
        buf[cursor + 4..cursor + 20].copy_from_slice(&group.octets());
        cursor += MCAST_RECORD_LEN;
    }
    Some(total)
}

/// Byte length of a Version 2 report body carrying `record_count`
/// records.
#[must_use]
pub const fn v2_report_len(record_count: usize) -> usize {
    4 + record_count * MCAST_RECORD_LEN
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn group() -> Ipv6Addr {
        Ipv6Addr::new(0xFF15, 0, 0, 0, 0, 0, 0, 0x1234)
    }

    fn mldv1_query_body(max_resp: u16, addr: Ipv6Addr) -> [u8; MLDV1_BODY_LEN] {
        let mut body = [0u8; MLDV1_BODY_LEN];
        body[0..2].copy_from_slice(&max_resp.to_be_bytes());
        body[4..20].copy_from_slice(&addr.octets());
        body
    }

    #[test]
    fn parses_mldv1_general_query() {
        let body = mldv1_query_body(10000, Ipv6Addr::UNSPECIFIED);
        let query = MldQuery::parse(&body).expect("parse");
        assert!(query.is_general());
        assert!(!query.is_v2);
        assert_eq!(query.max_response_millis, 10000);
    }

    #[test]
    fn parses_mldv2_group_specific_query() {
        let mut body = vec![0u8; MLDV2_QUERY_MIN_BODY_LEN];
        body[0..2].copy_from_slice(&500u16.to_be_bytes());
        body[4..20].copy_from_slice(&group().octets());
        let query = MldQuery::parse(&body).expect("parse");
        assert!(!query.is_general());
        assert!(query.is_v2);
        assert_eq!(query.multicast_address, group());
        assert_eq!(query.max_response_millis, 500);
    }

    #[test]
    fn mldv2_floating_max_response_code() {
        // code >= 0x8000: mant=(code&0xFFF)|0x1000, exp=(code>>12)&7,
        // value=(mant)<<(exp+3). For 0x8000: mant=0x1000, exp=0 => 0x8000.
        assert_eq!(decode_max_response_code(0x8000), 0x1000 << 3);
        assert_eq!(decode_max_response_code(0x0FFF), 0x0FFF);
    }

    #[test]
    fn truncated_query_is_rejected() {
        assert!(MldQuery::parse(&[0u8; MLDV1_BODY_LEN - 1]).is_none());
        assert!(MldQuery::parse(&[]).is_none());
    }

    #[test]
    fn writes_report_with_records() {
        let records = [
            (RecordType::ChangeToExclude, group()),
            (RecordType::ModeIsExclude, ALL_MLDV2_ROUTERS),
        ];
        let mut out = vec![0u8; v2_report_len(records.len())];
        let len = write_v2_report(&records, &mut out).expect("write");
        assert_eq!(len, out.len());
        // Reserved(2)=0, count=2.
        assert_eq!(u16::from_be_bytes([out[2], out[3]]), 2);
        // First record: type 4, aux len 0, sources 0, then the group.
        assert_eq!(out[4], RecordType::ChangeToExclude.as_u8());
        assert_eq!(out[5], 0);
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 0);
        assert_eq!(&out[8..24], &group().octets());
    }

    #[test]
    fn write_into_short_buffer_fails_closed() {
        let records = [(RecordType::ChangeToInclude, group())];
        let mut out = vec![0u8; v2_report_len(1) - 1];
        assert!(write_v2_report(&records, &mut out).is_none());
    }

    #[test]
    fn empty_report_is_just_the_header() {
        let mut out = vec![0u8; v2_report_len(0)];
        let len = write_v2_report(&[], &mut out).expect("write");
        assert_eq!(len, 4);
        assert_eq!(u16::from_be_bytes([out[2], out[3]]), 0);
    }
}
