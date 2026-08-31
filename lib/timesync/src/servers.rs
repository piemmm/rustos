//! Which servers to ask: the three-tier source policy.
//!
//! A machine can learn of a time server three ways, and they are not equal.
//! An operator who wrote one down meant it; a DHCP server offering one is
//! advising the machines on its own network, which is better information than
//! a guess; and with neither, a general-purpose OS that never asks anybody is
//! a machine with no clock. So the tiers are ordered, and the first non-empty
//! one wins outright — tiers are never merged, because a merged list would
//! quietly keep querying the public pool on a network that named its own
//! server.
//!
//! # Politeness of the fallback
//!
//! The fallback names the public NTP pool. RFC 8633 §3.1 asks a vendor
//! shipping a fleet to obtain its own pool vendor zone rather than point every
//! device at the generic names, and TAIRiX has no such zone yet, so the
//! generic names are what it can honestly use. What makes that acceptable is
//! the politeness policy the engine already enforces on every tier — a hard
//! minimum poll interval, one request in flight per server, bounded
//! exponential backoff with CSPRNG jitter, a randomised initial delay, and
//! obedience to a Kiss-o'-Death — plus the pool's own DNS rotation, which
//! hands each lookup a different member. Registering a vendor zone changes
//! only these four names.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::MAX_TIME_SERVERS;
use tairix_net::addr::IpAddr;

/// The built-in servers a machine falls back on when nothing else named one:
/// the public NTP pool.
///
/// The four numbered names rather than the bare `pool.ntp.org`, because that
/// is what the pool documents — each name resolves into a different slice of
/// the rotation, so a client gets four distinct servers to rotate over
/// instead of four lookups of one.
pub const FALLBACK_TIME_SERVERS: [&str; 4] = [
    "0.pool.ntp.org",
    "1.pool.ntp.org",
    "2.pool.ntp.org",
    "3.pool.ntp.org",
];

const _: () = assert!(FALLBACK_TIME_SERVERS.len() <= MAX_TIME_SERVERS);

/// Where the servers in use came from.
///
/// Ordered worst to best, so a caller compares tiers rather than
/// re-deriving the precedence: a set may be replaced by one of a strictly
/// greater source, never by an equal or lesser one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ServerSource {
    /// The built-in [`FALLBACK_TIME_SERVERS`]: nobody named a server.
    Fallback,
    /// Supplied by the network (DHCPv4 option 42 / DHCPv6 option 56).
    Network,
    /// Named by the operator or the installer in `time.servers`.
    Configured,
}

impl ServerSource {
    /// The stable audit spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Network => "network",
            Self::Configured => "configured",
        }
    }
}

/// One server to query: how it is spelled, and its address when the source
/// already supplied one.
///
/// A network-supplied server arrives *as* an address, so it carries one and
/// needs no name resolution — which is what lets a machine with no DNS at all
/// still keep time from its own network. The name is then the address's text,
/// held for the audit trail and never parsed back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeServer {
    /// The server's spelling: a host name, or an address literal.
    pub name: String,
    /// The address, when the source supplied one rather than a name.
    pub address: Option<IpAddr>,
}

impl TimeServer {
    /// A server named by text, to be resolved when first queried.
    #[must_use]
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            address: None,
        }
    }

    /// A server the network supplied as an address.
    #[must_use]
    pub fn learned(address: IpAddr) -> Self {
        Self {
            name: address.to_string(),
            address: Some(address),
        }
    }
}

/// The servers in use and the tier they came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSelection {
    /// The tier the servers came from.
    pub source: ServerSource,
    /// The servers, in query order, at most [`MAX_TIME_SERVERS`] of them.
    pub servers: Vec<TimeServer>,
}

/// Choose the servers to query: `configured` if it names any, else `learned`
/// if the network supplied any, else the built-in fallback.
///
/// The result is never empty — the fallback has no empty case — so a machine
/// always has somewhere to ask. Every tier is truncated to
/// [`MAX_TIME_SERVERS`], the bound the engine's own server array and the
/// configuration store share, so a server can never sit silently past the
/// engine's reach.
#[must_use]
pub fn select_servers(configured: &[String], learned: &[IpAddr]) -> ServerSelection {
    if !configured.is_empty() {
        return ServerSelection {
            source: ServerSource::Configured,
            servers: configured
                .iter()
                .take(MAX_TIME_SERVERS)
                .map(|name| TimeServer::named(name))
                .collect(),
        };
    }
    if !learned.is_empty() {
        return ServerSelection {
            source: ServerSource::Network,
            servers: learned
                .iter()
                .take(MAX_TIME_SERVERS)
                .copied()
                .map(TimeServer::learned)
                .collect(),
        };
    }
    ServerSelection {
        source: ServerSource::Fallback,
        servers: FALLBACK_TIME_SERVERS
            .iter()
            .copied()
            .map(TimeServer::named)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{Ipv4Addr, Ipv6Addr};

    fn names(selection: &ServerSelection) -> Vec<&str> {
        selection
            .servers
            .iter()
            .map(|server| server.name.as_str())
            .collect()
    }

    #[test]
    fn nothing_named_falls_back_to_the_pool() {
        let selection = select_servers(&[], &[]);
        assert_eq!(selection.source, ServerSource::Fallback);
        assert_eq!(names(&selection), FALLBACK_TIME_SERVERS.to_vec());
        assert!(
            selection.servers.iter().all(|s| s.address.is_none()),
            "a pool name is resolved, never assumed to be an address"
        );
    }

    #[test]
    fn the_network_supplied_servers_beat_the_fallback() {
        let learned = [IpAddr::V4(Ipv4Addr::new(192, 168, 66, 1))];
        let selection = select_servers(&[], &learned);
        assert_eq!(selection.source, ServerSource::Network);
        assert_eq!(names(&selection), ["192.168.66.1"]);
        assert_eq!(
            selection.servers[0].address,
            Some(learned[0]),
            "a learned server carries its address, so it needs no resolver"
        );
    }

    #[test]
    fn a_configured_server_beats_what_the_network_offered() {
        let configured = ["ntp.example.invalid".to_string()];
        let learned = [IpAddr::V4(Ipv4Addr::new(192, 168, 66, 1))];
        let selection = select_servers(&configured, &learned);
        assert_eq!(selection.source, ServerSource::Configured);
        assert_eq!(
            names(&selection),
            ["ntp.example.invalid"],
            "the tiers are never merged: an operator's choice stands alone"
        );
    }

    #[test]
    fn a_learned_ipv6_server_renders_in_canonical_form() {
        let learned = [IpAddr::V6(Ipv6Addr::new(0xFD00, 0, 0, 0, 0, 0, 0, 1))];
        let selection = select_servers(&[], &learned);
        assert_eq!(names(&selection), ["fd00::1"]);
    }

    #[test]
    fn every_tier_is_bounded_by_the_engines_own_server_array() {
        let configured: Vec<String> = (0..MAX_TIME_SERVERS + 3)
            .map(|index| alloc::format!("ntp{index}.example.invalid"))
            .collect();
        assert_eq!(
            select_servers(&configured, &[]).servers.len(),
            MAX_TIME_SERVERS
        );
        let learned: Vec<IpAddr> = (0..MAX_TIME_SERVERS + 3)
            .map(|index| {
                let last = u8::try_from(index).expect("small index");
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
            })
            .collect();
        assert_eq!(
            select_servers(&[], &learned).servers.len(),
            MAX_TIME_SERVERS
        );
    }

    #[test]
    fn the_tiers_are_ordered_so_a_caller_never_re_derives_the_precedence() {
        assert!(ServerSource::Configured > ServerSource::Network);
        assert!(ServerSource::Network > ServerSource::Fallback);
    }
}
