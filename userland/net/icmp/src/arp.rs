//! Address Resolution Protocol for IPv4 over Ethernet (RFC 826).
//!
//! Only the IPv4-over-Ethernet binding is handled: 6-byte hardware
//! addresses, 4-byte protocol addresses, request and reply opcodes.
//! Other hardware or protocol types are rejected by [`ArpPacket::parse`]
//! so the responder never answers a binding it does not understand.

use rustos_abi::driver::net::{MacAddress, MAC_ADDRESS_LEN};

use crate::Ipv4Address;

/// Wire length of an IPv4-over-Ethernet ARP packet.
pub const ARP_PACKET_LEN: usize = 28;

/// `htype` for Ethernet hardware addresses.
const HTYPE_ETHERNET: u16 = 1;

/// `ptype` for IPv4 protocol addresses (matches the IPv4 `EtherType`).
const PTYPE_IPV4: u16 = 0x0800;

/// Hardware-address length for Ethernet (equals [`MAC_ADDRESS_LEN`]).
const HARDWARE_LEN: u8 = 6;

/// Protocol-address length for IPv4.
const PROTOCOL_LEN: u8 = 4;

/// ARP request opcode.
pub const OP_REQUEST: u16 = 1;

/// ARP reply opcode.
pub const OP_REPLY: u16 = 2;

/// A parsed IPv4-over-Ethernet ARP packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpPacket {
    /// Operation: [`OP_REQUEST`] or [`OP_REPLY`].
    pub operation: u16,
    /// Sender hardware address.
    pub sender_hardware: MacAddress,
    /// Sender protocol (IPv4) address.
    pub sender_protocol: Ipv4Address,
    /// Target hardware address.
    pub target_hardware: MacAddress,
    /// Target protocol (IPv4) address.
    pub target_protocol: Ipv4Address,
}

impl ArpPacket {
    /// Parse an IPv4-over-Ethernet ARP packet from `bytes`.
    ///
    /// Returns `None` if the packet is truncated or describes a
    /// hardware/protocol binding other than IPv4-over-Ethernet.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let body = bytes.get(..ARP_PACKET_LEN)?;
        let htype = u16::from_be_bytes([body[0], body[1]]);
        let ptype = u16::from_be_bytes([body[2], body[3]]);
        let hlen = body[4];
        let plen = body[5];
        if htype != HTYPE_ETHERNET
            || ptype != PTYPE_IPV4
            || hlen != HARDWARE_LEN
            || plen != PROTOCOL_LEN
        {
            return None;
        }
        let operation = u16::from_be_bytes([body[6], body[7]]);
        Some(Self {
            operation,
            sender_hardware: mac(&body[8..14]),
            sender_protocol: ipv4(&body[14..18]),
            target_hardware: mac(&body[18..24]),
            target_protocol: ipv4(&body[24..28]),
        })
    }

    /// Build the reply that answers this request, claiming `local_mac`
    /// for the requested protocol address.
    ///
    /// The caller is responsible for only invoking this when
    /// [`Self::operation`] is [`OP_REQUEST`] and [`Self::target_protocol`]
    /// is owned by this host.
    #[must_use]
    pub fn reply_from(&self, local_mac: MacAddress) -> Self {
        Self {
            operation: OP_REPLY,
            sender_hardware: local_mac,
            sender_protocol: self.target_protocol,
            target_hardware: self.sender_hardware,
            target_protocol: self.sender_protocol,
        }
    }

    /// Serialise this packet into `out`, returning its length.
    ///
    /// Returns `None` when `out` cannot hold [`ARP_PACKET_LEN`] bytes.
    #[must_use]
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        let body = out.get_mut(..ARP_PACKET_LEN)?;
        body[0..2].copy_from_slice(&HTYPE_ETHERNET.to_be_bytes());
        body[2..4].copy_from_slice(&PTYPE_IPV4.to_be_bytes());
        body[4] = HARDWARE_LEN;
        body[5] = PROTOCOL_LEN;
        body[6..8].copy_from_slice(&self.operation.to_be_bytes());
        body[8..14].copy_from_slice(self.sender_hardware.as_octets());
        body[14..18].copy_from_slice(self.sender_protocol.as_octets());
        body[18..24].copy_from_slice(self.target_hardware.as_octets());
        body[24..28].copy_from_slice(self.target_protocol.as_octets());
        Some(ARP_PACKET_LEN)
    }
}

fn mac(bytes: &[u8]) -> MacAddress {
    let mut octets = [0u8; MAC_ADDRESS_LEN];
    octets.copy_from_slice(bytes);
    MacAddress(octets)
}

fn ipv4(bytes: &[u8]) -> Ipv4Address {
    let mut octets = [0u8; 4];
    octets.copy_from_slice(bytes);
    Ipv4Address(octets)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUESTER_MAC: MacAddress = MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]);
    const LOCAL_MAC: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    const REQUESTER_IP: Ipv4Address = Ipv4Address([10, 0, 2, 2]);
    const LOCAL_IP: Ipv4Address = Ipv4Address([10, 0, 2, 15]);

    fn request_bytes() -> [u8; ARP_PACKET_LEN] {
        let mut out = [0u8; ARP_PACKET_LEN];
        let req = ArpPacket {
            operation: OP_REQUEST,
            sender_hardware: REQUESTER_MAC,
            sender_protocol: REQUESTER_IP,
            target_hardware: MacAddress([0; MAC_ADDRESS_LEN]),
            target_protocol: LOCAL_IP,
        };
        req.write(&mut out).expect("fits");
        out
    }

    #[test]
    fn parse_round_trips_a_request() {
        let parsed = ArpPacket::parse(&request_bytes()).expect("parses");
        assert_eq!(parsed.operation, OP_REQUEST);
        assert_eq!(parsed.sender_hardware, REQUESTER_MAC);
        assert_eq!(parsed.sender_protocol, REQUESTER_IP);
        assert_eq!(parsed.target_protocol, LOCAL_IP);
    }

    #[test]
    fn parse_rejects_truncated() {
        assert!(ArpPacket::parse(&[0u8; ARP_PACKET_LEN - 1]).is_none());
    }

    #[test]
    fn parse_rejects_non_ethernet_ipv4_binding() {
        let mut bytes = request_bytes();
        bytes[1] = 9; // htype != Ethernet
        assert!(ArpPacket::parse(&bytes).is_none());

        let mut bytes = request_bytes();
        bytes[5] = 16; // plen != 4
        assert!(ArpPacket::parse(&bytes).is_none());
    }

    #[test]
    fn reply_swaps_and_claims_local_mac() {
        let request = ArpPacket::parse(&request_bytes()).expect("parses");
        let reply = request.reply_from(LOCAL_MAC);
        assert_eq!(reply.operation, OP_REPLY);
        assert_eq!(reply.sender_hardware, LOCAL_MAC);
        assert_eq!(reply.sender_protocol, LOCAL_IP);
        assert_eq!(reply.target_hardware, REQUESTER_MAC);
        assert_eq!(reply.target_protocol, REQUESTER_IP);
    }

    #[test]
    fn write_rejects_short_buffer() {
        let reply = ArpPacket::parse(&request_bytes())
            .expect("parses")
            .reply_from(LOCAL_MAC);
        let mut out = [0u8; ARP_PACKET_LEN - 1];
        assert!(reply.write(&mut out).is_none());
    }
}
