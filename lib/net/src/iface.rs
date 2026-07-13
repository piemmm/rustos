//! Interface address configuration engine: static IPv4/IPv6 plus
//! RFC 4862 stateless address autoconfiguration (SLAAC).
//!
//! One [`Iface`] owns a single interface's address state for both
//! families: the IPv4 static assignment, the IPv6 link-local address
//! formed from the injected interface identifier, static IPv6
//! assignments, and the addresses formed from Router Advertisement
//! prefixes. Like every stateful engine in this crate it is pure and
//! `now`-driven: inputs arrive as typed facts (a parsed RA's prefixes,
//! duplicate-address evidence), timed work is performed by
//! [`Iface::advance`], and the caller re-arms its one-shot timer from
//! [`Iface::next_deadline`]. Frame emission is the caller's job — the
//! engine returns typed [`IfaceAction`] send-intents.
//!
//! # Interface identifier
//!
//! The 64-bit interface identifier is injected through
//! [`IfaceConfig::interface_id`], never derived here from the MAC:
//! RFC 8064 recommends stable, opaque identifiers (RFC 7217) over
//! EUI-64, and the RFC 7217 derivation needs a keyed hash and a secret
//! that belong to the service layer (`lib/crypto`), not to a pure
//! protocol engine.
//!
//! # Duplicate address detection
//!
//! Every IPv6 address starts tentative and is confirmed by DAD
//! (RFC 4862 §5.4): [`IfaceConfig::dad_transmits`] Neighbour
//! Solicitations are emitted [`IfaceConfig::retrans_timer`] apart, and
//! evidence of a duplicate ([`Iface::on_dad_evidence`]) invalidates
//! the address. A failed link-local DAD disables IPv6 on the
//! interface entirely (RFC 4862 §5.4.5).
//!
//! # Security
//!
//! Router Advertisements are attacker-observable and, on a hostile
//! link, attacker-controlled. The engine is bounded fail-closed:
//! at most [`MAX_IPV6_ADDRS`] addresses ever exist, a prefix that
//! does not match the RFC 4862 shape rules is ignored whole, and the
//! RFC 4862 §5.5.3(e) two-hour rule prevents a spoofed RA from
//! instantly invalidating an established address.

use alloc::vec::Vec;

use rustos_abi::time::Duration64;

use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::nd::PrefixInformation;
use crate::route::CandidateAddr;

/// Maximum IPv6 addresses on one interface (link-local + static +
/// SLAAC). A fixed validation bound: a hostile router advertising many
/// autonomous prefixes must never grow this table.
pub const MAX_IPV6_ADDRS: usize = 16;

/// RFC 4861 §10 `MAX_RTR_SOLICITATIONS`.
pub const MAX_RTR_SOLICITATIONS: u8 = 3;

/// RFC 4861 §10 `RTR_SOLICITATION_INTERVAL`.
pub const RTR_SOLICITATION_INTERVAL: Duration64 = Duration64::from_secs(4);

/// The RFC 4862 §5.5.3(e) two-hour floor for remaining valid lifetime.
const TWO_HOURS_NANOS: u128 = 7_200 * NANOS_PER_SEC_U128;

/// Nanoseconds per second, widened for deadline arithmetic.
const NANOS_PER_SEC_U128: u128 = 1_000_000_000;

/// "No deadline" sentinel for internal nanosecond deadlines.
const NEVER: u128 = u128::MAX;

/// Static configuration of one interface's address engine.
#[derive(Clone, Copy, Debug)]
pub struct IfaceConfig {
    /// The 64-bit interface identifier used to form the link-local
    /// and SLAAC addresses (see the module docs: injected, RFC 7217
    /// derivation is the service layer's job).
    pub interface_id: [u8; 8],
    /// RFC 4862 `DupAddrDetectTransmits`: Neighbour Solicitations per
    /// DAD run. `0` disables DAD (addresses are immediately
    /// preferred).
    pub dad_transmits: u8,
    /// Spacing between DAD transmissions and the completion wait
    /// after the last one (RFC 4861 `RetransTimer`).
    pub retrans_timer: Duration64,
    /// Delay before the first DAD transmission and the first Router
    /// Solicitation. RFC 4862 §5.4.2 / RFC 4861 §6.3.7 require a
    /// random delay up to `MAX_RTR_SOLICITATION_DELAY` (1 s) to
    /// desynchronise startup floods; the caller injects the drawn
    /// jitter (the engine is deterministic, entropy stays at the
    /// seam).
    pub start_delay: Duration64,
}

impl IfaceConfig {
    /// A configuration with the RFC-default timers and one DAD
    /// transmit for `interface_id`, with no start jitter (callers
    /// inject their drawn jitter explicitly).
    #[must_use]
    pub fn new(interface_id: [u8; 8]) -> Self {
        Self {
            interface_id,
            dad_transmits: 1,
            retrans_timer: Duration64::from_secs(1),
            start_delay: Duration64::from_secs(0),
        }
    }
}

/// How an IPv6 address came to exist on the interface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AddrOrigin {
    /// The `fe80::/64` address formed from the interface identifier.
    LinkLocal,
    /// Administratively assigned.
    Static,
    /// Formed from a Router Advertisement prefix (RFC 4862).
    Slaac,
}

/// Typed outputs of the engine: send-intents the caller turns into
/// frames, and address lifecycle facts the caller reports on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IfaceAction {
    /// Emit a DAD Neighbour Solicitation for `target` (unspecified
    /// source, destination = solicited-node multicast of `target`).
    SendDadSolicit {
        /// The tentative address being verified.
        target: Ipv6Addr,
    },
    /// Emit a Router Solicitation. `source` is the preferred
    /// link-local address; the caller includes its source link-layer
    /// address option exactly when `source` is `Some`.
    SendRouterSolicitation {
        /// Source address for the solicitation, if one is usable.
        source: Option<Ipv6Addr>,
    },
    /// DAD completed: `addr` is now preferred and usable.
    AddressPreferred {
        /// The address that became preferred.
        addr: Ipv6Addr,
    },
    /// An address's valid lifetime lapsed; it has been removed.
    AddressInvalidated {
        /// The removed address.
        addr: Ipv6Addr,
    },
    /// DAD found a duplicate; the address has been removed. When the
    /// duplicate was the link-local address, IPv6 is disabled on the
    /// interface (RFC 4862 §5.4.5).
    DadFailed {
        /// The duplicate address.
        addr: Ipv6Addr,
    },
}

/// Typed refusal of an address operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AddrError {
    /// The bounded address table is full.
    TableFull,
    /// The address is already present.
    Duplicate,
    /// The address is not a plain unicast address (unspecified,
    /// loopback, or multicast).
    NotUnicast,
    /// The prefix length is out of range for the operation.
    BadPrefixLen,
    /// IPv6 was disabled by a link-local DAD failure.
    V6Disabled,
}

/// Lifecycle state of one IPv6 address (RFC 4862 §5.5.4).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum AddrState {
    /// DAD in progress; the address is not yet usable as a source.
    Tentative {
        /// DAD solicitations sent so far.
        sent: u8,
    },
    /// Usable and preferred for new communication.
    Preferred,
    /// Usable but discouraged (preferred lifetime lapsed).
    Deprecated,
}

/// One IPv6 address record.
#[derive(Copy, Clone, Debug)]
struct V6Addr {
    addr: Ipv6Addr,
    prefix_len: u8,
    origin: AddrOrigin,
    state: AddrState,
    /// Next timed transition for this address: a pending DAD
    /// transmit/completion while tentative, otherwise the earlier of
    /// the lifetime deadlines. [`NEVER`] when nothing is scheduled.
    deadline: u128,
    /// End of the valid lifetime; [`NEVER`] for infinite/static.
    valid_until: u128,
    /// End of the preferred lifetime; [`NEVER`] for infinite/static.
    preferred_until: u128,
}

/// Read-only view of one IPv6 address for observers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ipv6AddrInfo {
    /// The address.
    pub addr: Ipv6Addr,
    /// Prefix length of the subnet the address belongs to.
    pub prefix_len: u8,
    /// How the address came to exist.
    pub origin: AddrOrigin,
    /// DAD has not yet confirmed the address.
    pub tentative: bool,
    /// The preferred lifetime lapsed; usable but discouraged.
    pub deprecated: bool,
}

/// Progress of the RFC 4861 §6.3.7 Router Solicitation schedule.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RsState {
    /// Waiting for the link-local address to become usable.
    NotStarted,
    /// Soliciting: transmissions sent so far and the next due time.
    Soliciting { sent: u8, next: u128 },
    /// Finished: an RA arrived or the transmission budget lapsed.
    Done,
}

/// The per-interface address engine. See the module docs.
#[derive(Debug)]
pub struct Iface {
    interface_id: [u8; 8],
    dad_transmits: u8,
    retrans_timer: u128,
    start_delay: u128,
    v4: Option<(Ipv4Addr, u8)>,
    v6: Vec<V6Addr>,
    v6_disabled: bool,
    rs: RsState,
}

impl Iface {
    /// Create the engine and begin bring-up at `now`: the link-local
    /// address enters DAD (its first solicitation due after
    /// [`IfaceConfig::start_delay`]), and Router Solicitations follow
    /// once it is usable.
    #[must_use]
    pub fn new(config: &IfaceConfig, now: Duration64) -> Self {
        let mut iface = Self {
            interface_id: config.interface_id,
            dad_transmits: config.dad_transmits,
            retrans_timer: nanos(config.retrans_timer),
            start_delay: nanos(config.start_delay),
            v4: None,
            v6: Vec::new(),
            v6_disabled: false,
            rs: RsState::NotStarted,
        };
        let link_local = iface.address_in(LINK_LOCAL_PREFIX);
        let start = nanos(now).saturating_add(iface.start_delay);
        iface.push_v6(link_local, 64, AddrOrigin::LinkLocal, NEVER, NEVER, start);
        iface
    }

    /// The IPv4 static assignment, if configured.
    #[must_use]
    pub fn ipv4(&self) -> Option<(Ipv4Addr, u8)> {
        self.v4
    }

    /// Assign the static IPv4 address `addr/prefix_len`.
    ///
    /// # Errors
    ///
    /// [`AddrError::NotUnicast`] for the unspecified, loopback,
    /// multicast, or limited-broadcast address;
    /// [`AddrError::BadPrefixLen`] for a prefix length over 32.
    pub fn set_ipv4(&mut self, addr: Ipv4Addr, prefix_len: u8) -> Result<(), AddrError> {
        if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() || addr.is_broadcast()
        {
            return Err(AddrError::NotUnicast);
        }
        if prefix_len > 32 {
            return Err(AddrError::BadPrefixLen);
        }
        self.v4 = Some((addr, prefix_len));
        Ok(())
    }

    /// Remove the IPv4 assignment, reporting whether one existed.
    pub fn clear_ipv4(&mut self) -> bool {
        self.v4.take().is_some()
    }

    /// Assign a static IPv6 address; it enters DAD at `now`.
    ///
    /// # Errors
    ///
    /// [`AddrError::V6Disabled`] after a link-local DAD failure,
    /// [`AddrError::NotUnicast`] / [`AddrError::BadPrefixLen`] for a
    /// malformed assignment, [`AddrError::Duplicate`] when already
    /// present, [`AddrError::TableFull`] at the [`MAX_IPV6_ADDRS`]
    /// bound.
    pub fn add_ipv6_static(
        &mut self,
        addr: Ipv6Addr,
        prefix_len: u8,
        now: Duration64,
    ) -> Result<(), AddrError> {
        if self.v6_disabled {
            return Err(AddrError::V6Disabled);
        }
        if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() {
            return Err(AddrError::NotUnicast);
        }
        if prefix_len == 0 || prefix_len > 128 {
            return Err(AddrError::BadPrefixLen);
        }
        if self.find_v6(addr).is_some() {
            return Err(AddrError::Duplicate);
        }
        if self.v6.len() >= MAX_IPV6_ADDRS {
            return Err(AddrError::TableFull);
        }
        self.push_v6(
            addr,
            prefix_len,
            AddrOrigin::Static,
            NEVER,
            NEVER,
            nanos(now),
        );
        Ok(())
    }

    /// Remove an IPv6 address, reporting whether it existed. The
    /// link-local address cannot be removed this way.
    pub fn remove_ipv6(&mut self, addr: Ipv6Addr) -> bool {
        let Some(index) = self.find_v6(addr) else {
            return false;
        };
        if self.v6[index].origin == AddrOrigin::LinkLocal {
            return false;
        }
        self.v6.swap_remove(index);
        true
    }

    /// Apply the autonomous/on-link address facts of a validated
    /// Router Advertisement's prefix options (RFC 4862 §5.5.3). The
    /// caller has already checked the RA's source and hop limit; the
    /// engine enforces the per-prefix shape rules and ignores
    /// non-conforming prefixes whole. Receiving any RA also ends the
    /// Router Solicitation schedule (RFC 4861 §6.3.7).
    pub fn on_router_advertisement(&mut self, prefixes: &[PrefixInformation], now: Duration64) {
        self.rs = RsState::Done;
        if self.v6_disabled {
            return;
        }
        let now = nanos(now);
        for info in prefixes {
            if !info.autonomous {
                continue;
            }
            // RFC 4862 §5.5.3: the prefix must leave exactly the
            // 64-bit interface identifier, must not be link-local,
            // and preferred must not exceed valid.
            if info.prefix_len != 64
                || crate::addr::is_unicast_link_local(&info.prefix)
                || info.preferred_lifetime > info.valid_lifetime
            {
                continue;
            }
            let addr = self.address_in(prefix_bits(info.prefix));
            let valid_until = lifetime_deadline(now, info.valid_lifetime);
            let preferred_until = lifetime_deadline(now, info.preferred_lifetime);
            if let Some(index) = self.find_v6(addr) {
                let entry = &mut self.v6[index];
                if entry.origin != AddrOrigin::Slaac {
                    continue;
                }
                // §5.5.3(e): always adopt the preferred lifetime; cap
                // a shrinking valid lifetime at two hours remaining so
                // one spoofed RA cannot expire an established address.
                entry.preferred_until = preferred_until;
                let two_hours = now.saturating_add(TWO_HOURS_NANOS);
                if valid_until > two_hours || valid_until > entry.valid_until {
                    entry.valid_until = valid_until;
                } else if entry.valid_until > two_hours {
                    entry.valid_until = two_hours;
                }
                entry.refresh_lifetime_state(now);
                entry.rearm();
                continue;
            }
            // §5.5.3(d): only create an address for a non-zero valid
            // lifetime, within the bounded table.
            if info.valid_lifetime == 0 || self.v6.len() >= MAX_IPV6_ADDRS {
                continue;
            }
            self.push_v6(
                addr,
                64,
                AddrOrigin::Slaac,
                valid_until,
                preferred_until,
                now,
            );
        }
    }

    /// Record duplicate-address evidence for `target` (an NS from the
    /// unspecified source, or any NA, naming one of our tentative
    /// addresses — RFC 4862 §5.4.3/§5.4.4).
    ///
    /// Returns the resulting [`IfaceAction::DadFailed`] when `target`
    /// was tentative here; a failed link-local additionally disables
    /// IPv6 and drops every other address.
    pub fn on_dad_evidence(&mut self, target: Ipv6Addr) -> Option<IfaceAction> {
        let index = self.find_v6(target)?;
        if !matches!(self.v6[index].state, AddrState::Tentative { .. }) {
            return None;
        }
        let origin = self.v6[index].origin;
        self.v6.swap_remove(index);
        if origin == AddrOrigin::LinkLocal {
            self.v6_disabled = true;
            self.v6.clear();
            self.rs = RsState::Done;
        }
        Some(IfaceAction::DadFailed { addr: target })
    }

    /// Perform every timed transition due at `now`: DAD transmits and
    /// completions, Router Solicitations, and lifetime expiry.
    pub fn advance(&mut self, now: Duration64) -> Vec<IfaceAction> {
        let now = nanos(now);
        let mut actions = Vec::new();
        let mut link_local_ready = false;
        let mut index = 0;
        while index < self.v6.len() {
            let entry = &mut self.v6[index];
            if entry.deadline > now {
                index += 1;
                continue;
            }
            match entry.state {
                AddrState::Tentative { sent } => {
                    if sent < self.dad_transmits {
                        entry.state = AddrState::Tentative { sent: sent + 1 };
                        entry.deadline = now.saturating_add(self.retrans_timer);
                        actions.push(IfaceAction::SendDadSolicit { target: entry.addr });
                        index += 1;
                    } else {
                        // The last solicitation went unanswered for a
                        // full retransmission interval: unique.
                        entry.state = AddrState::Preferred;
                        entry.refresh_lifetime_state(now);
                        actions.push(IfaceAction::AddressPreferred { addr: entry.addr });
                        if entry.origin == AddrOrigin::LinkLocal {
                            link_local_ready = true;
                        }
                        entry.rearm();
                        index += 1;
                    }
                }
                AddrState::Preferred | AddrState::Deprecated => {
                    if entry.valid_until <= now {
                        let addr = entry.addr;
                        self.v6.swap_remove(index);
                        actions.push(IfaceAction::AddressInvalidated { addr });
                    } else {
                        entry.refresh_lifetime_state(now);
                        entry.rearm();
                        index += 1;
                    }
                }
            }
        }
        if link_local_ready && self.rs == RsState::NotStarted {
            self.rs = RsState::Soliciting {
                sent: 0,
                next: now.saturating_add(self.start_delay),
            };
        }
        if let RsState::Soliciting { sent, next } = self.rs {
            if next <= now {
                if sent < MAX_RTR_SOLICITATIONS {
                    self.rs = RsState::Soliciting {
                        sent: sent + 1,
                        next: now.saturating_add(nanos(RTR_SOLICITATION_INTERVAL)),
                    };
                    actions.push(IfaceAction::SendRouterSolicitation {
                        source: self.link_local(),
                    });
                } else {
                    self.rs = RsState::Done;
                }
            }
        }
        actions
    }

    /// When the earliest timed transition is due, for the caller's
    /// one-shot timer. `None` when nothing is pending.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let addr_deadlines = self.v6.iter().map(|entry| entry.deadline);
        let rs_deadline = match self.rs {
            RsState::Soliciting { next, .. } => next,
            RsState::NotStarted | RsState::Done => NEVER,
        };
        let earliest = addr_deadlines
            .chain(core::iter::once(rs_deadline))
            .filter(|&deadline| deadline != NEVER)
            .min()?;
        Some(from_nanos(earliest))
    }

    /// The preferred link-local address, once DAD confirmed it.
    #[must_use]
    pub fn link_local(&self) -> Option<Ipv6Addr> {
        self.v6.iter().find_map(|entry| {
            (entry.origin == AddrOrigin::LinkLocal && entry.state == AddrState::Preferred)
                .then_some(entry.addr)
        })
    }

    /// Whether IPv6 was disabled by a link-local DAD failure.
    #[must_use]
    pub fn v6_disabled(&self) -> bool {
        self.v6_disabled
    }

    /// Source-selection candidates: every usable (non-tentative)
    /// IPv6 address, with its deprecation fact (RFC 6724 rule 3).
    #[must_use]
    pub fn candidates(&self) -> Vec<CandidateAddr> {
        self.v6
            .iter()
            .filter_map(|entry| match entry.state {
                AddrState::Tentative { .. } => None,
                AddrState::Preferred | AddrState::Deprecated => Some(CandidateAddr {
                    addr: entry.addr,
                    deprecated: entry.state == AddrState::Deprecated,
                    prefix_len: entry.prefix_len,
                }),
            })
            .collect()
    }

    /// Read-only views of every IPv6 address, tentative included.
    #[must_use]
    pub fn ipv6_addresses(&self) -> Vec<Ipv6AddrInfo> {
        self.v6
            .iter()
            .map(|entry| Ipv6AddrInfo {
                addr: entry.addr,
                prefix_len: entry.prefix_len,
                origin: entry.origin,
                tentative: matches!(entry.state, AddrState::Tentative { .. }),
                deprecated: entry.state == AddrState::Deprecated,
            })
            .collect()
    }

    /// Whether `addr` is assigned here and usable (non-tentative).
    #[must_use]
    pub fn is_assigned(&self, addr: Ipv6Addr) -> bool {
        self.find_v6(addr)
            .is_some_and(|index| !matches!(self.v6[index].state, AddrState::Tentative { .. }))
    }

    /// Whether `addr` is assigned here and still tentative.
    #[must_use]
    pub fn is_tentative(&self, addr: Ipv6Addr) -> bool {
        self.find_v6(addr)
            .is_some_and(|index| matches!(self.v6[index].state, AddrState::Tentative { .. }))
    }

    /// Form this interface's address inside the /64 `prefix` bits.
    fn address_in(&self, prefix: [u8; 8]) -> Ipv6Addr {
        let mut octets = [0u8; 16];
        octets[..8].copy_from_slice(&prefix);
        octets[8..].copy_from_slice(&self.interface_id);
        Ipv6Addr::from(octets)
    }

    fn find_v6(&self, addr: Ipv6Addr) -> Option<usize> {
        self.v6.iter().position(|entry| entry.addr == addr)
    }

    /// Insert a new record entering DAD, with its first solicitation
    /// due at `start` (immediately preferred when DAD is disabled).
    fn push_v6(
        &mut self,
        addr: Ipv6Addr,
        prefix_len: u8,
        origin: AddrOrigin,
        valid_until: u128,
        preferred_until: u128,
        start: u128,
    ) {
        let mut entry = V6Addr {
            addr,
            prefix_len,
            origin,
            state: AddrState::Tentative { sent: 0 },
            deadline: start,
            valid_until,
            preferred_until,
        };
        if self.dad_transmits == 0 {
            entry.state = if preferred_until <= start && preferred_until != NEVER {
                AddrState::Deprecated
            } else {
                AddrState::Preferred
            };
            entry.rearm();
            if entry.origin == AddrOrigin::LinkLocal && self.rs == RsState::NotStarted {
                self.rs = RsState::Soliciting {
                    sent: 0,
                    next: start,
                };
            }
        }
        self.v6.push(entry);
    }
}

impl V6Addr {
    /// Apply the preferred-lifetime transition due at `now` to a
    /// usable address.
    fn refresh_lifetime_state(&mut self, now: u128) {
        if matches!(self.state, AddrState::Tentative { .. }) {
            return;
        }
        self.state = if self.preferred_until <= now {
            AddrState::Deprecated
        } else {
            AddrState::Preferred
        };
    }

    /// Re-arm [`Self::deadline`] to the earlier pending lifetime
    /// transition of a usable address.
    fn rearm(&mut self) {
        self.deadline = match self.state {
            AddrState::Tentative { .. } => self.deadline,
            AddrState::Preferred => self.preferred_until.min(self.valid_until),
            AddrState::Deprecated => self.valid_until,
        };
    }
}

/// The `fe80::/64` link-local prefix bits.
const LINK_LOCAL_PREFIX: [u8; 8] = [0xFE, 0x80, 0, 0, 0, 0, 0, 0];

/// The leading 64 prefix bits of `prefix`.
fn prefix_bits(prefix: Ipv6Addr) -> [u8; 8] {
    let mut bits = [0u8; 8];
    bits.copy_from_slice(&prefix.octets()[..8]);
    bits
}

/// Deadline for a RA lifetime in seconds; `u32::MAX` means no expiry
/// (RFC 4861 §4.6.2).
fn lifetime_deadline(now: u128, lifetime_secs: u32) -> u128 {
    if lifetime_secs == u32::MAX {
        return NEVER;
    }
    now.saturating_add(u128::from(lifetime_secs) * NANOS_PER_SEC_U128)
}

/// Widen a [`Duration64`] to nanoseconds for deadline arithmetic.
fn nanos(d: Duration64) -> u128 {
    let secs = u128::try_from(d.secs()).unwrap_or(0);
    secs * NANOS_PER_SEC_U128 + u128::from(d.subsec_nanos())
}

/// Narrow an internal nanosecond deadline back to a [`Duration64`].
fn from_nanos(deadline: u128) -> Duration64 {
    Duration64::from_nanos(u64::try_from(deadline).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[path = "iface_tests.rs"]
mod tests;
