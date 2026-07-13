//! The dual-stack address vocabulary.
//!
//! IPv4 and IPv6 are peers throughout `lib/net`, expressed in one
//! vocabulary: the [`core::net`] address types ([`IpAddr`], [`Ipv4Addr`],
//! [`Ipv6Addr`]), re-exported here so every layer of the stack — and every
//! consumer of the engine — names the same types the language already
//! defines, rather than a parallel first-party copy that would drift.
//!
//! What `core::net` deliberately does not carry, this module adds:
//!
//! - [`Ipv6Scope`] — RFC 4007 scope classification, needed to decide
//!   whether an address is meaningful beyond one link and whether it
//!   requires a zone to disambiguate.
//! - [`ScopedIpv6Addr`] — an IPv6 address paired with the zone (interface)
//!   index its scope requires. A link-local address without a zone is
//!   ambiguous on a multi-interface host, so the constructor makes that
//!   state unrepresentable instead of every consumer re-checking it.

use core::num::NonZeroU32;

pub use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// An IPv6 scope per RFC 4007 §5 / RFC 4291 §2.7.
///
/// Discriminants are the multicast `scop` field values, so the derived
/// ordering is the RFC 4007 "covers" ordering: a smaller scope is
/// contained in every larger one.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Ipv6Scope {
    /// Spans a single interface only (`scop` 1); loopback traffic.
    InterfaceLocal = 0x1,
    /// Spans a single link (`scop` 2); `fe80::/10` unicast and the
    /// loopback address (RFC 4007 §4 treats the loopback interface as
    /// its own link).
    LinkLocal = 0x2,
    /// The smallest administratively configured scope (`scop` 4).
    AdminLocal = 0x4,
    /// Spans a single site (`scop` 5).
    SiteLocal = 0x5,
    /// Spans several sites of one organisation (`scop` 8).
    OrganizationLocal = 0x8,
    /// Unbounded (`scop` 0xE); all ordinary unicast addresses.
    Global = 0xE,
}

impl Ipv6Scope {
    /// Classify `addr`.
    ///
    /// Returns `None` for the addresses that have no scope to act on —
    /// the unspecified address and multicast addresses whose `scop`
    /// field is reserved or unassigned — so a caller fails closed
    /// (drops) instead of guessing.
    ///
    /// Unicast rules: `fe80::/10` and the loopback address are
    /// link-local (RFC 4007 §4); everything else — including ULA
    /// `fc00::/7`, whose *scope* is global per RFC 4193 — is
    /// [`Ipv6Scope::Global`]. The deprecated site-local `fec0::/10`
    /// prefix (RFC 3879) still classifies as [`Ipv6Scope::SiteLocal`],
    /// because that is what its bits say on the wire.
    #[must_use]
    pub fn of(addr: &Ipv6Addr) -> Option<Self> {
        if addr.is_unspecified() {
            return None;
        }
        if addr.is_multicast() {
            return match addr.octets()[1] & 0x0F {
                0x1 => Some(Self::InterfaceLocal),
                0x2 => Some(Self::LinkLocal),
                0x4 => Some(Self::AdminLocal),
                0x5 => Some(Self::SiteLocal),
                0x8 => Some(Self::OrganizationLocal),
                0xE => Some(Self::Global),
                _ => None,
            };
        }
        if addr.is_loopback() || is_unicast_link_local(addr) {
            return Some(Self::LinkLocal);
        }
        if addr.segments()[0] & 0xFFC0 == 0xFEC0 {
            return Some(Self::SiteLocal);
        }
        Some(Self::Global)
    }
}

/// True for a unicast link-local address (`fe80::/10`, RFC 4291 §2.5.6).
#[must_use]
pub fn is_unicast_link_local(addr: &Ipv6Addr) -> bool {
    addr.segments()[0] & 0xFFC0 == 0xFE80
}

/// Why an address/zone pairing was refused by [`ScopedIpv6Addr::new`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ScopeError {
    /// The address has no scope to pair a zone with (unspecified, or a
    /// multicast address with a reserved `scop` value).
    Unscoped,
    /// The address's scope is not global, so a zone index is required
    /// to make it unambiguous, and none was supplied (RFC 4007 §6).
    ZoneRequired,
    /// The address is global scope; a zone index is meaningless and a
    /// supplied one is refused rather than silently ignored.
    ZoneForbidden,
}

/// An IPv6 address paired with the zone (interface) index its scope
/// requires, per RFC 4007 §6.
///
/// A non-global-scope address (link-local unicast, non-global multicast,
/// loopback) is meaningful only relative to a zone, so the constructor
/// requires one; a global-scope address needs none and refuses one. This
/// removes the "link-local address, but on which interface?" ambiguity
/// from every consumer.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ScopedIpv6Addr {
    addr: Ipv6Addr,
    zone: Option<NonZeroU32>,
}

impl ScopedIpv6Addr {
    /// Pair `addr` with `zone`, enforcing the scope/zone rules.
    ///
    /// # Errors
    ///
    /// * [`ScopeError::Unscoped`] — `addr` has no scope (unspecified, or
    ///   reserved multicast `scop`).
    /// * [`ScopeError::ZoneRequired`] — `addr` is non-global scope and
    ///   `zone` is `None`.
    /// * [`ScopeError::ZoneForbidden`] — `addr` is global scope and
    ///   `zone` is `Some`.
    pub fn new(addr: Ipv6Addr, zone: Option<NonZeroU32>) -> Result<Self, ScopeError> {
        let scope = Ipv6Scope::of(&addr).ok_or(ScopeError::Unscoped)?;
        match (scope == Ipv6Scope::Global, zone) {
            (true, Some(_)) => Err(ScopeError::ZoneForbidden),
            (false, None) => Err(ScopeError::ZoneRequired),
            _ => Ok(Self { addr, zone }),
        }
    }

    /// The address.
    #[must_use]
    pub const fn addr(&self) -> Ipv6Addr {
        self.addr
    }

    /// The zone index, present exactly when the scope is not global.
    #[must_use]
    pub const fn zone(&self) -> Option<NonZeroU32> {
        self.zone
    }

    /// The address's scope.
    ///
    /// Infallible: the constructor already refused unscoped addresses.
    #[must_use]
    pub fn scope(&self) -> Ipv6Scope {
        match Ipv6Scope::of(&self.addr) {
            Some(scope) => scope,
            // The constructor refused every unscoped address, so this
            // arm is unreachable; fail closed to the narrowest scope
            // rather than panic.
            None => Ipv6Scope::InterfaceLocal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(index: u32) -> Option<NonZeroU32> {
        NonZeroU32::new(index)
    }

    #[test]
    fn unicast_scope_classification() {
        let link_local = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1);
        assert_eq!(Ipv6Scope::of(&link_local), Some(Ipv6Scope::LinkLocal));
        assert!(is_unicast_link_local(&link_local));

        let global = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1);
        assert_eq!(Ipv6Scope::of(&global), Some(Ipv6Scope::Global));
        assert!(!is_unicast_link_local(&global));

        // ULA has global *scope* (RFC 4193 §3.3).
        let ula = Ipv6Addr::new(0xFD00, 0, 0, 0, 0, 0, 0, 1);
        assert_eq!(Ipv6Scope::of(&ula), Some(Ipv6Scope::Global));

        // Deprecated site-local bits still classify as what they say.
        let site = Ipv6Addr::new(0xFEC0, 0, 0, 0, 0, 0, 0, 1);
        assert_eq!(Ipv6Scope::of(&site), Some(Ipv6Scope::SiteLocal));

        assert_eq!(
            Ipv6Scope::of(&Ipv6Addr::LOCALHOST),
            Some(Ipv6Scope::LinkLocal)
        );
        assert_eq!(Ipv6Scope::of(&Ipv6Addr::UNSPECIFIED), None);
    }

    #[test]
    fn multicast_scope_follows_scop_field() {
        let cases = [
            (0x1, Some(Ipv6Scope::InterfaceLocal)),
            (0x2, Some(Ipv6Scope::LinkLocal)),
            (0x4, Some(Ipv6Scope::AdminLocal)),
            (0x5, Some(Ipv6Scope::SiteLocal)),
            (0x8, Some(Ipv6Scope::OrganizationLocal)),
            (0xE, Some(Ipv6Scope::Global)),
            (0x0, None),
            (0x3, None),
            (0xF, None),
        ];
        for (scop, expected) in cases {
            let addr = Ipv6Addr::new(0xFF00 | scop, 0, 0, 0, 0, 0, 0, 1);
            assert_eq!(Ipv6Scope::of(&addr), expected, "scop {scop:#x}");
        }
    }

    #[test]
    fn scope_ordering_is_rfc4007_covering_order() {
        assert!(Ipv6Scope::InterfaceLocal < Ipv6Scope::LinkLocal);
        assert!(Ipv6Scope::LinkLocal < Ipv6Scope::AdminLocal);
        assert!(Ipv6Scope::AdminLocal < Ipv6Scope::SiteLocal);
        assert!(Ipv6Scope::SiteLocal < Ipv6Scope::OrganizationLocal);
        assert!(Ipv6Scope::OrganizationLocal < Ipv6Scope::Global);
    }

    #[test]
    fn scoped_address_zone_rules() {
        let link_local = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1);
        let global = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1);

        let scoped = ScopedIpv6Addr::new(link_local, zone(3)).expect("zoned link-local");
        assert_eq!(scoped.addr(), link_local);
        assert_eq!(scoped.zone(), zone(3));
        assert_eq!(scoped.scope(), Ipv6Scope::LinkLocal);

        assert_eq!(
            ScopedIpv6Addr::new(link_local, None),
            Err(ScopeError::ZoneRequired)
        );
        assert_eq!(
            ScopedIpv6Addr::new(global, zone(3)),
            Err(ScopeError::ZoneForbidden)
        );

        let scoped = ScopedIpv6Addr::new(global, None).expect("global needs no zone");
        assert_eq!(scoped.zone(), None);
        assert_eq!(scoped.scope(), Ipv6Scope::Global);

        assert_eq!(
            ScopedIpv6Addr::new(Ipv6Addr::UNSPECIFIED, None),
            Err(ScopeError::Unscoped)
        );
        // Reserved multicast scop is unscoped, zone or not.
        let reserved = Ipv6Addr::new(0xFF03, 0, 0, 0, 0, 0, 0, 1);
        assert_eq!(
            ScopedIpv6Addr::new(reserved, zone(1)),
            Err(ScopeError::Unscoped)
        );
    }
}
