//! IGMPv2 (RFC 2236) — the IPv4 multicast group-membership protocol.
//!
//! IGMP is carried directly in IPv4 (protocol number 2). A message is a
//! fixed eight bytes: a type, a maximum-response time (meaningful only in
//! a query), the RFC 1071 checksum over the whole message, and the group
//! address. This module is the one definition of that framing — the host
//! membership state machine in [`crate::mcast`] drives it, and the
//! [`crate::stack::Stack`] wires it to the wire.
//!
//! Every decoder is total, bounded, and fail-closed: a truncated message,
//! an unknown type, a trailing-byte mismatch for a fixed-width type, or a
//! checksum that does not verify rejects the whole message (`None`);
//! nothing partial is surfaced.

use crate::addr::Ipv4Addr;
use crate::checksum::internet_checksum;

/// IP protocol number carrying IGMP (RFC 1112).
pub const PROTOCOL_IGMP: u8 = 2;

/// Fixed length of an IGMPv2 message.
pub const IGMP_MESSAGE_LEN: usize = 8;

/// Membership Query message type.
pub const TYPE_MEMBERSHIP_QUERY: u8 = 0x11;
/// Version 1 Membership Report type (accepted from legacy hosts).
pub const TYPE_V1_REPORT: u8 = 0x12;
/// Version 2 Membership Report type.
pub const TYPE_V2_REPORT: u8 = 0x16;
/// Leave Group message type.
pub const TYPE_LEAVE_GROUP: u8 = 0x17;

/// A parsed IGMPv2 message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgmpMessage {
    /// A Membership Query. A `group` of `0.0.0.0` is a General Query for
    /// every group; any other `group` is a Group-Specific Query.
    MembershipQuery {
        /// Maximum time a responding host may wait before reporting, in
        /// units of 1/10 second (RFC 2236 §2.2). A value of zero means
        /// "respond immediately" and is treated as the shortest interval
        /// by the state machine.
        max_resp_deciseconds: u8,
        /// The queried group, or `0.0.0.0` for a General Query.
        group: Ipv4Addr,
    },
    /// A Version 2 Membership Report for `group`.
    V2Report {
        /// The reported group.
        group: Ipv4Addr,
    },
    /// A Version 1 Membership Report for `group` (legacy host).
    V1Report {
        /// The reported group.
        group: Ipv4Addr,
    },
    /// A Leave Group message for `group`, sent to `224.0.0.2`.
    LeaveGroup {
        /// The group being left.
        group: Ipv4Addr,
    },
}

impl IgmpMessage {
    /// Parse and verify an IGMP message.
    ///
    /// Returns `None` (fail closed) for a message shorter than
    /// [`IGMP_MESSAGE_LEN`], an unknown type, a fixed-width type
    /// (report/leave) carrying trailing bytes, or a checksum that does
    /// not verify. A Membership Query longer than eight bytes (an
    /// IGMPv3 query) is accepted and interpreted through its v2 fields,
    /// as a v2 host must (RFC 3376 §7.3.2).
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let header = bytes.get(..IGMP_MESSAGE_LEN)?;
        let message_type = header[0];
        let is_query = message_type == TYPE_MEMBERSHIP_QUERY;
        // A fixed-width type must be exactly eight bytes; a query may be
        // longer (IGMPv3). The checksum always covers the whole message.
        if !is_query && bytes.len() != IGMP_MESSAGE_LEN {
            return None;
        }
        if internet_checksum(bytes) != 0 {
            return None;
        }
        let group = Ipv4Addr::new(header[4], header[5], header[6], header[7]);
        match message_type {
            TYPE_MEMBERSHIP_QUERY => Some(Self::MembershipQuery {
                max_resp_deciseconds: header[1],
                group,
            }),
            TYPE_V2_REPORT => Some(Self::V2Report { group }),
            TYPE_V1_REPORT => Some(Self::V1Report { group }),
            TYPE_LEAVE_GROUP => Some(Self::LeaveGroup { group }),
            _ => None,
        }
    }

    /// The group this message concerns (`0.0.0.0` for a General Query).
    #[must_use]
    pub fn group(&self) -> Ipv4Addr {
        match self {
            Self::MembershipQuery { group, .. }
            | Self::V2Report { group }
            | Self::V1Report { group }
            | Self::LeaveGroup { group } => *group,
        }
    }

    /// Encode `self` into an eight-byte buffer with the checksum filled
    /// in.
    ///
    /// Returns `None` when `out` cannot hold [`IGMP_MESSAGE_LEN`] bytes.
    #[must_use]
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        let buf = out.get_mut(..IGMP_MESSAGE_LEN)?;
        buf.fill(0);
        let (message_type, max_resp, group) = match *self {
            Self::MembershipQuery {
                max_resp_deciseconds,
                group,
            } => (TYPE_MEMBERSHIP_QUERY, max_resp_deciseconds, group),
            Self::V2Report { group } => (TYPE_V2_REPORT, 0, group),
            Self::V1Report { group } => (TYPE_V1_REPORT, 0, group),
            Self::LeaveGroup { group } => (TYPE_LEAVE_GROUP, 0, group),
        };
        buf[0] = message_type;
        buf[1] = max_resp;
        buf[4..8].copy_from_slice(&group.octets());
        let checksum = internet_checksum(buf);
        buf[2..4].copy_from_slice(&checksum.to_be_bytes());
        Some(IGMP_MESSAGE_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn encode(message: IgmpMessage) -> [u8; IGMP_MESSAGE_LEN] {
        let mut buf = [0u8; IGMP_MESSAGE_LEN];
        message.write(&mut buf).expect("write");
        buf
    }

    #[test]
    fn round_trips_every_message() {
        let group = Ipv4Addr::new(224, 1, 2, 3);
        for message in [
            IgmpMessage::MembershipQuery {
                max_resp_deciseconds: 100,
                group: Ipv4Addr::UNSPECIFIED,
            },
            IgmpMessage::MembershipQuery {
                max_resp_deciseconds: 30,
                group,
            },
            IgmpMessage::V2Report { group },
            IgmpMessage::V1Report { group },
            IgmpMessage::LeaveGroup { group },
        ] {
            let bytes = encode(message);
            assert_eq!(IgmpMessage::parse(&bytes), Some(message));
        }
    }

    #[test]
    fn emitted_checksum_verifies() {
        let bytes = encode(IgmpMessage::V2Report {
            group: Ipv4Addr::new(239, 0, 0, 1),
        });
        assert_eq!(internet_checksum(&bytes), 0);
    }

    #[test]
    fn corrupt_checksum_is_rejected() {
        let mut bytes = encode(IgmpMessage::LeaveGroup {
            group: Ipv4Addr::new(224, 0, 0, 22),
        });
        bytes[5] ^= 0x01;
        assert!(IgmpMessage::parse(&bytes).is_none());
    }

    #[test]
    fn truncated_message_is_rejected() {
        assert!(IgmpMessage::parse(&[0x16, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(IgmpMessage::parse(&[]).is_none());
    }

    #[test]
    fn unknown_type_is_rejected() {
        let mut bytes = encode(IgmpMessage::V2Report {
            group: Ipv4Addr::new(224, 1, 1, 1),
        });
        bytes[0] = 0x99;
        // Re-seal the checksum so only the type is wrong.
        bytes[2..4].copy_from_slice(&[0, 0]);
        let checksum = internet_checksum(&bytes);
        bytes[2..4].copy_from_slice(&checksum.to_be_bytes());
        assert!(IgmpMessage::parse(&bytes).is_none());
    }

    #[test]
    fn fixed_width_type_rejects_trailing_bytes() {
        let mut bytes: Vec<u8> = encode(IgmpMessage::V2Report {
            group: Ipv4Addr::new(224, 1, 1, 1),
        })
        .to_vec();
        bytes.push(0);
        assert!(IgmpMessage::parse(&bytes).is_none());
    }

    #[test]
    fn igmpv3_query_is_read_through_v2_fields() {
        // An IGMPv3 General Query: type 0x11, longer than eight bytes.
        let mut bytes = [0u8; 12];
        bytes[0] = TYPE_MEMBERSHIP_QUERY;
        bytes[1] = 50;
        let checksum = internet_checksum(&bytes);
        bytes[2..4].copy_from_slice(&checksum.to_be_bytes());
        assert_eq!(
            IgmpMessage::parse(&bytes),
            Some(IgmpMessage::MembershipQuery {
                max_resp_deciseconds: 50,
                group: Ipv4Addr::UNSPECIFIED,
            })
        );
    }

    #[test]
    fn general_query_group_is_unspecified() {
        let query = IgmpMessage::MembershipQuery {
            max_resp_deciseconds: 100,
            group: Ipv4Addr::UNSPECIFIED,
        };
        assert!(query.group().is_unspecified());
    }
}
