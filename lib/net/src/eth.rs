//! Ethernet II framing.
//!
//! One fixed 14-byte header: destination, source, `EtherType`. That is
//! the whole contract between the stack and an Ethernet link — VLAN
//! tags and 802.3 length framing are not spoken by the stack and a
//! frame carrying them is simply an unrecognised `EtherType`, dropped
//! by the dispatcher (fail closed) rather than mis-parsed.

use tairix_abi::driver::net::{MacAddress, MAC_ADDRESS_LEN};

/// Length of the fixed Ethernet II header (no VLAN tag).
pub const ETHERNET_HEADER_LEN: usize = 2 * MAC_ADDRESS_LEN + 2;

/// `EtherType` identifying an ARP payload (RFC 826).
pub const ETHERTYPE_ARP: u16 = 0x0806;

/// `EtherType` identifying an IPv4 payload (RFC 894).
pub const ETHERTYPE_IPV4: u16 = 0x0800;

/// `EtherType` identifying an IPv6 payload (RFC 2464).
pub const ETHERTYPE_IPV6: u16 = 0x86DD;

/// The all-ones link-layer broadcast address.
pub const BROADCAST: MacAddress = MacAddress([0xFF; MAC_ADDRESS_LEN]);

/// A parsed view over the header of an Ethernet II frame.
///
/// The [`Self::payload`] slice borrows the input frame; no copy is
/// made.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EthernetFrame<'a> {
    /// Destination link-layer address.
    pub destination: MacAddress,
    /// Source link-layer address.
    pub source: MacAddress,
    /// `EtherType` of [`Self::payload`].
    pub ethertype: u16,
    /// Frame payload following the 14-byte header.
    pub payload: &'a [u8],
}

impl<'a> EthernetFrame<'a> {
    /// Parse the header of `bytes`, borrowing its payload.
    ///
    /// Returns `None` when `bytes` is shorter than a bare Ethernet
    /// header, in which case the caller drops the frame.
    #[must_use]
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let header = bytes.get(..ETHERNET_HEADER_LEN)?;
        let mut destination = [0u8; MAC_ADDRESS_LEN];
        let mut source = [0u8; MAC_ADDRESS_LEN];
        destination.copy_from_slice(&header[..MAC_ADDRESS_LEN]);
        source.copy_from_slice(&header[MAC_ADDRESS_LEN..2 * MAC_ADDRESS_LEN]);
        let ethertype = u16::from_be_bytes([header[12], header[13]]);
        Some(Self {
            destination: MacAddress(destination),
            source: MacAddress(source),
            ethertype,
            payload: &bytes[ETHERNET_HEADER_LEN..],
        })
    }

    /// True when the frame's destination is this host's address or
    /// the broadcast address.
    #[must_use]
    pub fn addressed_to(&self, local: MacAddress) -> bool {
        self.destination == local || self.destination == BROADCAST
    }
}

/// The Ethernet multicast address an IPv6 group maps to
/// (`33:33` + the group's last four octets, RFC 2464 §7).
#[must_use]
pub fn ipv6_multicast_mac(group: &crate::addr::Ipv6Addr) -> MacAddress {
    let octets = group.octets();
    MacAddress([0x33, 0x33, octets[12], octets[13], octets[14], octets[15]])
}

/// The Ethernet multicast address an IPv4 group maps to
/// (`01:00:5e` + the low 23 bits of the group, RFC 1112 §6.4).
#[must_use]
pub fn ipv4_multicast_mac(group: &crate::addr::Ipv4Addr) -> MacAddress {
    let o = group.octets();
    MacAddress([0x01, 0x00, 0x5E, o[1] & 0x7F, o[2], o[3]])
}

/// True when `mac` is a group (multicast or broadcast) address: the
/// I/G bit of the first octet is set (IEEE 802).
#[must_use]
pub fn is_group_mac(mac: MacAddress) -> bool {
    mac.as_octets()[0] & 0x01 != 0
}

/// Write an Ethernet II header into `out`, returning its length.
///
/// Returns `None` when `out` cannot hold the 14-byte header.
#[must_use]
pub fn write_header(
    out: &mut [u8],
    destination: MacAddress,
    source: MacAddress,
    ethertype: u16,
) -> Option<usize> {
    let header = out.get_mut(..ETHERNET_HEADER_LEN)?;
    header[..MAC_ADDRESS_LEN].copy_from_slice(destination.as_octets());
    header[MAC_ADDRESS_LEN..2 * MAC_ADDRESS_LEN].copy_from_slice(source.as_octets());
    header[12..14].copy_from_slice(&ethertype.to_be_bytes());
    Some(ETHERNET_HEADER_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DST: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    const SRC: MacAddress = MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]);

    fn sample() -> [u8; 18] {
        [
            0x52, 0x54, 0x00, 0x12, 0x34, 0x56, // dst
            0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01, // src
            0x08, 0x00, // IPv4
            0xDE, 0xAD, 0xBE, 0xEF, // payload
        ]
    }

    #[test]
    fn parse_extracts_fields_and_payload() {
        let bytes = sample();
        let frame = EthernetFrame::parse(&bytes).expect("parses");
        assert_eq!(frame.destination, DST);
        assert_eq!(frame.source, SRC);
        assert_eq!(frame.ethertype, ETHERTYPE_IPV4);
        assert_eq!(frame.payload, &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn parse_rejects_runt() {
        assert!(EthernetFrame::parse(&[0u8; ETHERNET_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn empty_payload_is_allowed() {
        let bytes = sample();
        let frame = EthernetFrame::parse(&bytes[..ETHERNET_HEADER_LEN]).expect("parses");
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn addressed_to_matches_unicast_and_broadcast() {
        let bytes = sample();
        let frame = EthernetFrame::parse(&bytes).expect("parses");
        assert!(frame.addressed_to(DST));
        assert!(!frame.addressed_to(SRC));

        let mut bcast = sample();
        bcast[..MAC_ADDRESS_LEN].copy_from_slice(BROADCAST.as_octets());
        let frame = EthernetFrame::parse(&bcast).expect("parses");
        assert!(frame.addressed_to(DST));
    }

    #[test]
    fn write_header_round_trips() {
        let mut out = [0u8; ETHERNET_HEADER_LEN];
        let len = write_header(&mut out, DST, SRC, ETHERTYPE_ARP).expect("fits");
        assert_eq!(len, ETHERNET_HEADER_LEN);
        let frame = EthernetFrame::parse(&out).expect("parses");
        assert_eq!(frame.destination, DST);
        assert_eq!(frame.source, SRC);
        assert_eq!(frame.ethertype, ETHERTYPE_ARP);
    }

    #[test]
    fn write_header_rejects_short_buffer() {
        let mut out = [0u8; ETHERNET_HEADER_LEN - 1];
        assert!(write_header(&mut out, DST, SRC, ETHERTYPE_ARP).is_none());
    }
}
