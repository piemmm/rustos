//! The receive pre-filter: decide whether a frame can possibly have a local
//! consumer, before the network stack is woken for it
//! (`plans/NETWORK.md` N17).
//!
//! On a busy segment most broadcast traffic is addressed to other hosts —
//! ARP asking after a neighbour, `NetBIOS` and SSDP announcements, and the
//! rest of the background noise of a LAN. Every such frame otherwise costs a
//! wake of the stack process, a full protocol parse, and a drop. The driver
//! evaluates this classifier on its harvest path instead and hands over only
//! what could matter, so an idle machine on a noisy network costs
//! approximately nothing.
//!
//! # What it matches on, and what it deliberately does not
//!
//! Only **slow-changing L3 address state**, published by the stack when an
//! interface's addresses change. It knows nothing about listening ports or
//! group memberships, and that is the point: per-socket state could fall
//! behind a socket opening and drop a frame someone wanted, for a share of
//! the noise that does not justify the risk. Multicast is admitted
//! wholesale for the same reason — the device's own group filter already
//! sheds unjoined groups where it has one, and mirroring IGMP/MLD
//! membership here would be more state to keep in step for less benefit.
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

use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::arp::{self, ArpPacket};
use crate::eth::{EthernetFrame, ETHERTYPE_ARP, ETHERTYPE_IPV4, ETHERTYPE_IPV6};
use crate::{ipv4, ipv6};

/// The IPv4 limited-broadcast address, which every host accepts.
const IPV4_BROADCAST: Ipv4Addr = Ipv4Addr::BROADCAST;

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

    /// Whether an IPv4 destination is one this interface answers for.
    fn admits_v4(&self, destination: Ipv4Addr) -> bool {
        if destination == IPV4_BROADCAST || destination.is_multicast() {
            return true;
        }
        if !self.decides_v4() {
            return true;
        }
        let octets = destination.octets();
        self.policy.v4_addresses().contains(&octets)
            || self.policy.v4_broadcasts().contains(&octets)
    }

    /// Whether an IPv6 destination is one this interface answers for.
    fn admits_v6(&self, destination: Ipv6Addr) -> bool {
        if destination.is_multicast() {
            return true;
        }
        if !self.decides_v6() {
            return true;
        }
        self.policy.v6_addresses().contains(&destination.octets())
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
            ETHERTYPE_IPV4 => ipv4::peek_destination(parsed.payload)
                .is_none_or(|destination| self.admits_v4(destination)),
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
