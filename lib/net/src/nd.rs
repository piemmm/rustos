//! Neighbour Discovery for IPv6 (RFC 4861).
//!
//! The five ND messages — Router Solicitation, Router Advertisement,
//! Neighbour Solicitation, Neighbour Advertisement, and Redirect — are
//! `ICMPv6` messages (types 133–137). [`NdMessage::parse`] decodes a
//! checksum-verified [`crate::icmp::IcmpMessage`] and applies the RFC
//! 4861 validation rules: hop limit 255 (a forwarded ND packet is a
//! spoofing attempt and is dropped), code 0, minimum lengths, and
//! bounded options ([`MAX_ND_OPTIONS`]).
//!
//! The reachability state machine is *not* here: it is the one
//! provider-agnostic [`crate::neigh::NeighborTable`] that ARP also
//! drives. [`apply_neighbor_solicitation`] and
//! [`apply_neighbor_advertisement`] translate validated messages into
//! that table's calls; Router Advertisement facts (default routers,
//! MTU, prefixes) are typed data the caller feeds to
//! [`crate::route::DefaultRouterList`] and its address configuration.

use alloc::vec::Vec;

use tairix_abi::driver::net::MacAddress;
use tairix_abi::time::Duration64;

use crate::addr::{IpAddr, Ipv6Addr};
use crate::neigh::NeighborTable;

/// The hop limit every valid ND packet carries (RFC 4861: a value
/// below 255 proves the packet crossed a router and must be dropped).
pub const ND_HOP_LIMIT: u8 = 255;

/// `ICMPv6` type for Router Solicitation.
pub const TYPE_ROUTER_SOLICITATION: u8 = 133;
/// `ICMPv6` type for Router Advertisement.
pub const TYPE_ROUTER_ADVERTISEMENT: u8 = 134;
/// `ICMPv6` type for Neighbour Solicitation.
pub const TYPE_NEIGHBOR_SOLICITATION: u8 = 135;
/// `ICMPv6` type for Neighbour Advertisement.
pub const TYPE_NEIGHBOR_ADVERTISEMENT: u8 = 136;
/// `ICMPv6` type for Redirect.
pub const TYPE_REDIRECT: u8 = 137;

/// Most options accepted in one ND message — a fixed validation bound
/// against option flooding. A legitimate message carries a handful
/// (link-layer address, MTU, a few prefixes).
pub const MAX_ND_OPTIONS: usize = 16;

/// ND option type: source link-layer address.
const OPTION_SOURCE_LL: u8 = 1;
/// ND option type: target link-layer address.
const OPTION_TARGET_LL: u8 = 2;
/// ND option type: prefix information.
const OPTION_PREFIX_INFO: u8 = 3;
/// ND option type: MTU.
const OPTION_MTU: u8 = 5;

/// A Prefix Information option (RFC 4861 §4.6.2), as advertised by a
/// router for on-link determination and SLAAC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrefixInformation {
    /// The advertised prefix.
    pub prefix: Ipv6Addr,
    /// Leading bits of `prefix` that are valid.
    pub prefix_len: u8,
    /// The prefix can be used for on-link determination.
    pub on_link: bool,
    /// The prefix can be used for stateless address configuration.
    pub autonomous: bool,
    /// Seconds the prefix is valid (`u32::MAX` = infinity).
    pub valid_lifetime: u32,
    /// Seconds addresses from the prefix stay preferred.
    pub preferred_lifetime: u32,
}

/// The typed option set of one ND message: the fields this host acts
/// on, each at most once (a duplicate keeps the first — bounded and
/// deterministic), with unrecognised options skipped per RFC 4861 §9.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Options {
    source_ll: Option<MacAddress>,
    target_ll: Option<MacAddress>,
    mtu: Option<u32>,
    prefixes: Vec<PrefixInformation>,
}

/// Parse the options area of an ND message.
///
/// Fails closed on a zero-length option, a length overrun, or more
/// than [`MAX_ND_OPTIONS`] options. Unrecognised option types are
/// skipped (RFC 4861 §9); recognised options with malformed bodies are
/// a reject, not a skip.
fn parse_options(mut bytes: &[u8]) -> Option<Options> {
    let mut options = Options::default();
    let mut count = 0usize;
    while !bytes.is_empty() {
        count += 1;
        if count > MAX_ND_OPTIONS {
            return None;
        }
        let length = usize::from(*bytes.get(1)?) * 8;
        if length == 0 {
            return None;
        }
        let option = bytes.get(..length)?;
        match option[0] {
            OPTION_SOURCE_LL | OPTION_TARGET_LL => {
                if length != 8 {
                    return None;
                }
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&option[2..8]);
                let slot = if option[0] == OPTION_SOURCE_LL {
                    &mut options.source_ll
                } else {
                    &mut options.target_ll
                };
                if slot.is_none() {
                    *slot = Some(MacAddress(mac));
                }
            }
            OPTION_MTU => {
                if length != 8 {
                    return None;
                }
                let mtu = u32::from_be_bytes([option[4], option[5], option[6], option[7]]);
                if options.mtu.is_none() {
                    options.mtu = Some(mtu);
                }
            }
            OPTION_PREFIX_INFO => {
                if length != 32 {
                    return None;
                }
                let prefix_len = option[2];
                if prefix_len > 128 {
                    return None;
                }
                let mut prefix = [0u8; 16];
                prefix.copy_from_slice(&option[16..32]);
                options.prefixes.push(PrefixInformation {
                    prefix: Ipv6Addr::from(prefix),
                    prefix_len,
                    on_link: option[3] & 0x80 != 0,
                    autonomous: option[3] & 0x40 != 0,
                    valid_lifetime: u32::from_be_bytes([
                        option[4], option[5], option[6], option[7],
                    ]),
                    preferred_lifetime: u32::from_be_bytes([
                        option[8], option[9], option[10], option[11],
                    ]),
                });
            }
            _ => {}
        }
        bytes = &bytes[length..];
    }
    Some(options)
}

/// A validated Neighbour Discovery message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NdMessage {
    /// A host asking routers to advertise (RFC 4861 §4.1).
    RouterSolicitation {
        /// The sender's link-layer address, when supplied.
        source_ll: Option<MacAddress>,
    },
    /// A router describing itself and its prefixes (RFC 4861 §4.2).
    RouterAdvertisement {
        /// Default hop limit hosts should use (0 = unspecified).
        cur_hop_limit: u8,
        /// Addresses are available via `DHCPv6` ("managed").
        managed: bool,
        /// Other configuration is available via `DHCPv6`.
        other: bool,
        /// Seconds the sender serves as a default router (0 = it does
        /// not).
        router_lifetime: u16,
        /// Milliseconds an entry stays reachable after confirmation
        /// (0 = unspecified).
        reachable_time: u32,
        /// Milliseconds between retransmitted solicitations
        /// (0 = unspecified).
        retrans_timer: u32,
        /// The router's link-layer address, when supplied.
        source_ll: Option<MacAddress>,
        /// Advertised link MTU, when supplied.
        mtu: Option<u32>,
        /// Advertised prefixes (bounded by [`MAX_ND_OPTIONS`]).
        prefixes: Vec<PrefixInformation>,
    },
    /// Address resolution or reachability probe (RFC 4861 §4.3).
    NeighborSolicitation {
        /// The address whose link-layer address is sought.
        target: Ipv6Addr,
        /// The solicitor's link-layer address, when supplied.
        source_ll: Option<MacAddress>,
    },
    /// The answer to a solicitation, or an unsolicited update
    /// (RFC 4861 §4.4).
    NeighborAdvertisement {
        /// The sender is a router.
        router: bool,
        /// Sent in response to a solicitation.
        solicited: bool,
        /// The carried link-layer address overrides a cached one.
        override_flag: bool,
        /// The address the advertisement is about.
        target: Ipv6Addr,
        /// The target's link-layer address, when supplied.
        target_ll: Option<MacAddress>,
    },
    /// A router pointing the host at a better first hop (RFC 4861 §4.5).
    Redirect {
        /// The better first-hop address for `destination`.
        target: Ipv6Addr,
        /// The destination the redirect is about.
        destination: Ipv6Addr,
        /// The target's link-layer address, when supplied.
        target_ll: Option<MacAddress>,
    },
}

impl NdMessage {
    /// Decode and validate an ND message from a checksum-verified
    /// `ICMPv6` message (RFC 4861 §6.1, §7.1, §8.1).
    ///
    /// `hop_limit` is the carrying packet's hop limit and must be
    /// [`ND_HOP_LIMIT`]; `dest_is_multicast` rejects a solicited
    /// Neighbour Advertisement sent to a multicast destination.
    /// Returns `None` for a non-ND type, a non-zero code, a violated
    /// validation rule, or malformed options.
    #[must_use]
    pub fn parse(
        message_type: u8,
        code: u8,
        hop_limit: u8,
        dest_is_multicast: bool,
        body: &[u8],
    ) -> Option<Self> {
        if hop_limit != ND_HOP_LIMIT || code != 0 {
            return None;
        }
        match message_type {
            TYPE_ROUTER_SOLICITATION => {
                let rest = body.get(4..)?;
                let options = parse_options(rest)?;
                Some(Self::RouterSolicitation {
                    source_ll: options.source_ll,
                })
            }
            TYPE_ROUTER_ADVERTISEMENT => {
                let fixed = body.get(..12)?;
                let options = parse_options(&body[12..])?;
                Some(Self::RouterAdvertisement {
                    cur_hop_limit: fixed[0],
                    managed: fixed[1] & 0x80 != 0,
                    other: fixed[1] & 0x40 != 0,
                    router_lifetime: u16::from_be_bytes([fixed[2], fixed[3]]),
                    reachable_time: u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]),
                    retrans_timer: u32::from_be_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]),
                    source_ll: options.source_ll,
                    mtu: options.mtu,
                    prefixes: options.prefixes,
                })
            }
            TYPE_NEIGHBOR_SOLICITATION => {
                let fixed = body.get(..20)?;
                let target = address(&fixed[4..20]);
                if target.is_multicast() {
                    return None;
                }
                let options = parse_options(&body[20..])?;
                Some(Self::NeighborSolicitation {
                    target,
                    source_ll: options.source_ll,
                })
            }
            TYPE_NEIGHBOR_ADVERTISEMENT => {
                let fixed = body.get(..20)?;
                let solicited = fixed[0] & 0x40 != 0;
                if solicited && dest_is_multicast {
                    return None;
                }
                let target = address(&fixed[4..20]);
                if target.is_multicast() {
                    return None;
                }
                let options = parse_options(&body[20..])?;
                Some(Self::NeighborAdvertisement {
                    router: fixed[0] & 0x80 != 0,
                    solicited,
                    override_flag: fixed[0] & 0x20 != 0,
                    target,
                    target_ll: options.target_ll,
                })
            }
            TYPE_REDIRECT => {
                let fixed = body.get(..36)?;
                let options = parse_options(&body[36..])?;
                Some(Self::Redirect {
                    target: address(&fixed[4..20]),
                    destination: address(&fixed[20..36]),
                    target_ll: options.target_ll,
                })
            }
            _ => None,
        }
    }

    /// The `ICMPv6` type number of this message.
    #[must_use]
    pub fn message_type(&self) -> u8 {
        match self {
            Self::RouterSolicitation { .. } => TYPE_ROUTER_SOLICITATION,
            Self::RouterAdvertisement { .. } => TYPE_ROUTER_ADVERTISEMENT,
            Self::NeighborSolicitation { .. } => TYPE_NEIGHBOR_SOLICITATION,
            Self::NeighborAdvertisement { .. } => TYPE_NEIGHBOR_ADVERTISEMENT,
            Self::Redirect { .. } => TYPE_REDIRECT,
        }
    }

    /// Serialise the message body (the bytes after the 4-byte `ICMPv6`
    /// header) into `out`, returning its length.
    ///
    /// This host emits what a host emits: Router Solicitations,
    /// Neighbour Solicitations, and Neighbour Advertisements. Router
    /// Advertisements and Redirects are router output; writing one
    /// returns `None` (TAIRiX is a host, not a router — see
    /// `plans/NETWORK.md` §9).
    #[must_use]
    pub fn write_body(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Self::RouterSolicitation { source_ll } => {
                let total = 4 + ll_option_len(source_ll.as_ref());
                let body = out.get_mut(..total)?;
                body[..4].fill(0);
                write_ll_option(&mut body[4..], OPTION_SOURCE_LL, source_ll.as_ref());
                Some(total)
            }
            Self::NeighborSolicitation { target, source_ll } => {
                let total = 20 + ll_option_len(source_ll.as_ref());
                let body = out.get_mut(..total)?;
                body[..4].fill(0);
                body[4..20].copy_from_slice(&target.octets());
                write_ll_option(&mut body[20..], OPTION_SOURCE_LL, source_ll.as_ref());
                Some(total)
            }
            Self::NeighborAdvertisement {
                router,
                solicited,
                override_flag,
                target,
                target_ll,
            } => {
                let total = 20 + ll_option_len(target_ll.as_ref());
                let body = out.get_mut(..total)?;
                body[..4].fill(0);
                body[0] = (u8::from(*router) << 7)
                    | (u8::from(*solicited) << 6)
                    | (u8::from(*override_flag) << 5);
                body[4..20].copy_from_slice(&target.octets());
                write_ll_option(&mut body[20..], OPTION_TARGET_LL, target_ll.as_ref());
                Some(total)
            }
            Self::RouterAdvertisement { .. } | Self::Redirect { .. } => None,
        }
    }
}

/// Wire length of an optional link-layer address option.
fn ll_option_len(mac: Option<&MacAddress>) -> usize {
    if mac.is_some() {
        8
    } else {
        0
    }
}

/// Append a link-layer address option when one is supplied. The caller
/// sized `out` via [`ll_option_len`].
fn write_ll_option(out: &mut [u8], option_type: u8, mac: Option<&MacAddress>) {
    if let Some(mac) = mac {
        out[0] = option_type;
        out[1] = 1;
        out[2..8].copy_from_slice(&mac.0);
    }
}

/// Record what a validated Neighbour Solicitation teaches: the
/// sender's binding, learned as `Stale` (RFC 4861 §7.2.3).
///
/// A solicitation from the unspecified address (duplicate address
/// detection) carries no binding and never creates cache state.
pub fn apply_neighbor_solicitation(
    message: &NdMessage,
    source: Ipv6Addr,
    table: &mut NeighborTable,
) {
    if let NdMessage::NeighborSolicitation {
        source_ll: Some(mac),
        ..
    } = message
    {
        if !source.is_unspecified() {
            table.learn(IpAddr::V6(source), *mac);
        }
    }
}

/// Record what a validated Neighbour Advertisement confirms
/// (RFC 4861 §7.2.5): a reachability confirmation for the target,
/// honouring the `solicited` and `override` flags.
///
/// An advertisement without a target link-layer option confirms
/// nothing here (for an `Incomplete` entry it carries no address to
/// resolve with; the table ignores confirmations for absent entries).
pub fn apply_neighbor_advertisement(
    message: &NdMessage,
    table: &mut NeighborTable,
    now: Duration64,
) {
    if let NdMessage::NeighborAdvertisement {
        solicited,
        override_flag,
        target,
        target_ll: Some(mac),
        ..
    } = message
    {
        table.confirm(IpAddr::V6(*target), *mac, *solicited, *override_flag, now);
    }
}

/// Record what a validated Redirect teaches about the new first hop
/// (RFC 4861 §8.3): when the redirect carries the target's link-layer
/// address, it is learned like a solicitation's source binding. The
/// route change itself is the caller's decision (its destination
/// cache), made only for redirects from the current first-hop router.
pub fn apply_redirect(message: &NdMessage, table: &mut NeighborTable) {
    if let NdMessage::Redirect {
        target,
        target_ll: Some(mac),
        ..
    } = message
    {
        table.learn(IpAddr::V6(*target), *mac);
    }
}

fn address(bytes: &[u8]) -> Ipv6Addr {
    let mut octets = [0u8; 16];
    octets.copy_from_slice(bytes);
    Ipv6Addr::from(octets)
}

#[cfg(test)]
#[path = "nd_tests.rs"]
mod tests;
