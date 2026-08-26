//! The receive pre-filter: decide whether a frame can possibly have a local
//! consumer, before the network stack is woken for it
//! (`plans/NETWORK.md` N17).
//!
//! On a busy segment nearly all broadcast and multicast traffic is addressed
//! to other hosts, or to groups this host never joined — ARP asking after a
//! neighbour, mDNS, LLMNR, SSDP, `NetBIOS` and the rest of a LAN's
//! background noise. Measured on an idle Raspberry Pi 4: of 7994 frames the
//! stack was woken for, it discarded 7917. Each of those cost a frame copy
//! into the shared ring, a wake of the stack process, and a full protocol
//! parse, to be dropped. The driver evaluates this classifier on its harvest
//! path instead, before the copy, and a harvest that admits nothing sends no
//! notify at all — so the stack is never woken for them.
//!
//! # What it matches on
//!
//! It **mirrors the stack's own destination-acceptance rule**, over the
//! slow-changing L3 state that rule is made of: the interface's addresses,
//! its subnet broadcast, and the groups it has joined. That is the whole
//! input — the stack gates a group or broadcast destination on *membership*,
//! never on a listening port, so no per-socket state is needed and nothing
//! here can fall behind a socket opening.
//!
//! One IPv4 carve-out, and it is the engine's own: a DHCPv4 reply arrives
//! broadcast before any address exists, so broadcast UDP to
//! [`dhcp::CLIENT_PORT`](crate::dhcp::CLIENT_PORT) is admitted. Matching the
//! port rather than tracking "a client is running" keeps this stateless — a
//! running-client flag would be true for the whole lease and so admit every
//! broadcast datagram on a DHCP-configured interface, which is the traffic
//! this sheds.
//!
//! # Its bias is to admit
//!
//! It is never load-bearing for security. Every admitted frame is still
//! fully validated by the stack, and the driver process already owns the
//! device and could drop whatever it liked, so refusing here grants nothing.
//! So anything the classifier cannot parse with confidence is **admitted**,
//! and a policy that could not name every local address widens to admit all
//! unicast: dropping is the optimisation, delivering is the safe default.

use tairix_abi::driver::net_channel::RxFilterPolicy;
use tairix_abi::driver::net_ring::RxAdmit;

use crate::addr::{solicited_node_multicast, Ipv4Addr, Ipv6Addr, ALL_NODES};
use crate::arp::{self, ArpPacket};
use crate::eth::{EthernetFrame, ETHERTYPE_ARP, ETHERTYPE_IPV4, ETHERTYPE_IPV6};
use crate::ipv4::Ipv4Header;
use crate::ipv6;
use crate::udp::{PROTOCOL_UDP, UDP_HEADER_LEN};

/// Classifies a received frame against one interface's local addresses.
///
/// Built from the [`RxFilterPolicy`] the stack published; it holds no state
/// of its own, so a driver rebuilds it whenever the policy changes and never
/// has to reconcile two views.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RxClassifier {
    policy: RxFilterPolicy,
}

impl RxClassifier {
    /// Build a classifier for `policy`.
    #[must_use]
    pub const fn new(policy: RxFilterPolicy) -> Self {
        Self { policy }
    }

    /// The policy this classifier evaluates.
    #[must_use]
    pub const fn policy(&self) -> &RxFilterPolicy {
        &self.policy
    }

    /// Whether the policy can decide an IPv4 unicast destination at all.
    ///
    /// It must name every address the interface has **and** have at least one
    /// to compare against. A family with no address can only admit: there is
    /// nothing a destination could match, so filtering it would drop
    /// everything — including the DHCP offer that is about to give the
    /// interface its first address. "Names them all" is not the same as
    /// "can decide", and conflating the two wedges address configuration.
    fn decides_v4(&self) -> bool {
        self.policy.is_exhaustive() && !self.policy.v4_addresses().is_empty()
    }

    /// The IPv6 counterpart of [`Self::decides_v4`].
    fn decides_v6(&self) -> bool {
        self.policy.is_exhaustive() && !self.policy.v6_addresses().is_empty()
    }

    /// Whether an IPv4 packet is one this interface answers for.
    ///
    /// Mirrors the stack's rule — our own address, or a joined group — with
    /// the engine's own DHCP carve-out: its client claims a broadcast reply
    /// ahead of the address filter, before any address exists to match.
    fn admits_v4(&self, header: &Ipv4Header, payload: &[u8]) -> bool {
        if !self.decides_v4() {
            return true;
        }
        let octets = header.destination.octets();
        if self.policy.v4_addresses().contains(&octets) {
            return true;
        }
        if header.destination.is_multicast() {
            return self.policy.v4_groups().contains(&octets);
        }
        if header.destination == Ipv4Addr::BROADCAST
            || self.policy.v4_broadcasts().contains(&octets)
        {
            return is_dhcp_client_datagram(header, payload);
        }
        false
    }

    /// Whether an IPv6 destination is one this interface answers for.
    ///
    /// The all-nodes and solicited-node groups are derived rather than
    /// carried: both follow from addresses the policy already names, so
    /// there is no second set to keep in step.
    fn admits_v6(&self, destination: Ipv6Addr) -> bool {
        if !self.decides_v6() {
            return true;
        }
        if self.policy.v6_addresses().contains(&destination.octets()) {
            return true;
        }
        if !destination.is_multicast() {
            return false;
        }
        destination == ALL_NODES
            || self.policy.v6_groups().contains(&destination.octets())
            || self
                .policy
                .v6_addresses()
                .iter()
                .any(|octets| solicited_node_multicast(&Ipv6Addr::from(*octets)) == destination)
    }

    /// Whether an ARP payload concerns this interface.
    ///
    /// A *reply* is always admitted: it may answer a request this host sent,
    /// and the stack's neighbour cache is the only thing that can tell. A
    /// *request* is admitted only when its target protocol address is one of
    /// ours, which is what sheds the dominant broadcast load on a LAN.
    fn admits_arp(&self, payload: &[u8]) -> bool {
        let Some(packet) = ArpPacket::parse(payload) else {
            // Unparsable: admit and let the stack refuse it, rather than
            // making a drop decision on bytes we did not understand.
            return true;
        };
        if packet.operation != arp::OP_REQUEST {
            return true;
        }
        if !self.decides_v4() {
            return true;
        }
        self.policy
            .v4_addresses()
            .contains(&packet.target_protocol.octets())
    }
}

/// Whether an IPv4 broadcast datagram is one the stack's DHCPv4 client
/// claims: a plain (unfragmented) UDP datagram to the client port.
///
/// A fragment is admitted without inspection — the port lives in the first
/// one, and reassembly is the stack's job, so guessing here could shed a
/// reply.
fn is_dhcp_client_datagram(header: &Ipv4Header, payload: &[u8]) -> bool {
    if header.is_fragment() {
        return true;
    }
    if header.protocol != PROTOCOL_UDP {
        return false;
    }
    let Some(udp) = payload.get(..UDP_HEADER_LEN) else {
        return true;
    };
    u16::from_be_bytes([udp[2], udp[3]]) == crate::dhcp::CLIENT_PORT
}

impl RxAdmit for RxClassifier {
    fn admit(&self, frame: &[u8]) -> bool {
        let Some(parsed) = EthernetFrame::parse(frame) else {
            // Too short to be a frame at all. The stack drops it too, but
            // admitting keeps the one place that decides *why* a frame is
            // invalid in the stack.
            return true;
        };
        match parsed.ethertype {
            ETHERTYPE_ARP => self.admits_arp(parsed.payload),
            // One parse yields the destination, the protocol and the
            // payload, so the DHCP check below needs no second decoder; a
            // header this refuses is admitted for the stack to diagnose.
            ETHERTYPE_IPV4 => Ipv4Header::parse(parsed.payload)
                .is_none_or(|(header, _options, payload)| self.admits_v4(&header, payload)),
            ETHERTYPE_IPV6 => ipv6::peek_destination(parsed.payload)
                .is_none_or(|destination| self.admits_v6(destination)),
            // The stack speaks no other ethertype, so nothing local can
            // consume one: spanning tree, LLDP, 802.1X, Wake-on-LAN and the
            // rest are shed here rather than parsed and dropped. This is a
            // *positive* identification, not a parse the filter is unsure
            // of — which is why it refuses where a malformed frame is
            // admitted.
            _ => false,
        }
    }
}

#[cfg(test)]
#[path = "rxfilter_tests.rs"]
mod tests;
