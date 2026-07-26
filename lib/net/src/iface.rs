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

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::time::Duration64;

use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::nd::PrefixInformation;
use crate::route::CandidateAddr;
use crate::timeutil::{from_nanos, nanos, NEVER};

/// Maximum IPv6 addresses on one interface (link-local + static +
/// SLAAC). A fixed validation bound: a hostile router advertising many
/// autonomous prefixes must never grow this table.
pub const MAX_IPV6_ADDRS: usize = 16;

/// RFC 4861 §10 `MAX_RTR_SOLICITATIONS`.
pub const MAX_RTR_SOLICITATIONS: u8 = 3;

/// RFC 8981 §3.8 `TEMP_PREFERRED_LIFETIME` default (1 day): the ceiling
/// preferred lifetime of a temporary (privacy) address before a
/// successor is generated.
pub const TEMP_PREFERRED_LIFETIME: Duration64 = Duration64::from_secs(86_400);

/// RFC 8981 §3.8 `TEMP_VALID_LIFETIME` default (2 days): the ceiling
/// valid lifetime of a temporary address.
pub const TEMP_VALID_LIFETIME: Duration64 = Duration64::from_secs(172_800);

/// RFC 8981 §3.8 `TEMP_IDGEN_RETRIES` default: how many times a fresh
/// randomised interface identifier is tried against DAD for one prefix
/// before the engine stops forming temporary addresses there.
pub const TEMP_IDGEN_RETRIES: u8 = 3;

/// The numerator/denominator of RFC 8981 §3.8 `MAX_DESYNC_FACTOR`
/// (`0.4 * TEMP_PREFERRED_LIFETIME`), expressed as an exact fraction so
/// the computation stays integer-only.
const MAX_DESYNC_NUM: u128 = 2;
const MAX_DESYNC_DEN: u128 = 5;

/// RFC 4861 §10 `RTR_SOLICITATION_INTERVAL`.
pub const RTR_SOLICITATION_INTERVAL: Duration64 = Duration64::from_secs(4);

/// The RFC 4862 §5.5.3(e) two-hour floor for remaining valid lifetime.
const TWO_HOURS_NANOS: u128 = 7_200 * NANOS_PER_SEC_U128;

/// Nanoseconds per second, widened for deadline arithmetic.
const NANOS_PER_SEC_U128: u128 = 1_000_000_000;

/// The injected source of unpredictable material RFC 8981 temporary
/// (privacy) addresses require.
///
/// Entropy lives at this seam so the [`Iface`] engine stays pure and
/// `now`-driven (NETWORK.md §0 "injected seams: time, RNG, frame
/// I/O"): the engine calls it only while forming a temporary interface
/// identifier or its desync jitter, and a deterministic implementation
/// makes the engine fully reproducible in tests. The service layer
/// backs it with the kernel CSPRNG.
pub trait TempAddrSource: core::fmt::Debug {
    /// Fill `out` with unpredictable bytes.
    ///
    /// Used for the temporary interface identifier — which **must** be
    /// unpredictable to off-path observers, the entire point of a
    /// privacy address — and for the `DESYNC_FACTOR` jitter. The engine
    /// discards and re-draws a reserved or colliding identifier, so an
    /// implementation need not filter; it must, however, draw from a
    /// cryptographically strong generator.
    fn fill_random(&mut self, out: &mut [u8]);
}

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
    /// Whether IPv6 is administratively enabled on this interface. When
    /// `false` the engine forms no link-local address at bring-up, so
    /// the interface neither solicits routers nor accepts any IPv6
    /// traffic (the `net.ipv6.enabled` policy). Enable it later with
    /// [`Iface::set_ipv6_enabled`].
    pub ipv6_enabled: bool,
    /// Whether the interface forms RFC 8981 temporary (privacy) IPv6
    /// addresses in addition to the stable SLAAC address of each
    /// autonomous prefix (the `net.ipv6.privacy` policy). Disabled by
    /// default: the stable address is always present, and privacy
    /// addresses are opt-in. Toggle it later with
    /// [`Iface::set_privacy`].
    pub privacy: bool,
    /// RFC 8981 `TEMP_PREFERRED_LIFETIME`: the ceiling preferred
    /// lifetime of a temporary address before a successor is generated
    /// (defaults to [`TEMP_PREFERRED_LIFETIME`]). Capped further by
    /// each prefix's advertised preferred lifetime and shortened by a
    /// random `DESYNC_FACTOR`.
    pub temp_preferred_lifetime: Duration64,
    /// RFC 8981 `TEMP_VALID_LIFETIME`: the ceiling valid lifetime of a
    /// temporary address (defaults to [`TEMP_VALID_LIFETIME`]), capped
    /// further by each prefix's advertised valid lifetime.
    pub temp_valid_lifetime: Duration64,
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
            ipv6_enabled: true,
            privacy: false,
            temp_preferred_lifetime: TEMP_PREFERRED_LIFETIME,
            temp_valid_lifetime: TEMP_VALID_LIFETIME,
        }
    }
}

/// Derive the modified EUI-64 interface identifier from a 48-bit Ethernet
/// MAC address (RFC 4291 Appendix A / RFC 2464 §4).
///
/// The 24-bit OUI and 24-bit NIC-specific parts are split by the fixed
/// `FF:FE` fill, and the universal/local bit (bit 1 of the first octet) is
/// inverted, yielding the 64-bit identifier SLAAC and the link-local
/// address are formed from. It is the deterministic default an Ethernet
/// interface uses when no RFC 7217 stable-privacy secret is configured
/// (that derivation is a separate, keyed concern the service layer owns).
#[must_use]
pub fn eui64_interface_id(mac: [u8; 6]) -> [u8; 8] {
    [
        mac[0] ^ 0x02,
        mac[1],
        mac[2],
        0xFF,
        0xFE,
        mac[3],
        mac[4],
        mac[5],
    ]
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
    /// An RFC 8981 temporary (privacy) address: a stable SLAAC
    /// prefix combined with a randomised interface identifier, formed
    /// only when the `net.ipv6.privacy` policy is enabled and
    /// regenerated periodically so a host is not tracked by a stable
    /// address across sessions.
    Temporary,
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
    /// IPv4 is administratively disabled by policy (`net.ipv4.enabled
    /// false`), so no IPv4 address may be assigned.
    V4Disabled,
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
    /// For an [`AddrOrigin::Temporary`] address: when its successor is
    /// generated (`preferred_until - REGEN_ADVANCE`), so a fresh
    /// privacy address is preferred before this one deprecates.
    /// [`NEVER`] for every other origin.
    regen_at: u128,
    /// For a temporary address: whether its successor has already been
    /// generated (or the attempt was made). Prevents re-firing at
    /// `regen_at` — without it the past `regen_at` deadline would spin
    /// the caller's timer.
    regen_done: bool,
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

/// Per-prefix RFC 8981 §3.4 duplicate-IID retry guard. Counts
/// consecutive DAD failures of temporary addresses on one prefix so
/// the engine stops forming them there after [`TEMP_IDGEN_RETRIES`]
/// (a badly misconfigured or hostile link must not spin re-drawing
/// identifiers forever).
#[derive(Copy, Clone, Debug)]
struct TempGuard {
    prefix: [u8; 8],
    failures: u8,
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
    /// IPv6 is administratively disabled by policy (`net.ipv6.enabled
    /// false`). Distinct from `v6_disabled`, which is the RFC 4862
    /// §5.4.5 link-local-DAD-failure disable; this one is operator
    /// policy and is reversible through [`Iface::set_ipv6_enabled`].
    v6_admin_disabled: bool,
    rs: RsState,
    /// Whether RFC 8981 temporary (privacy) addresses are formed
    /// (`net.ipv6.privacy`). See [`Iface::set_privacy`].
    privacy: bool,
    /// RFC 8981 `TEMP_PREFERRED_LIFETIME` in nanoseconds.
    temp_preferred: u128,
    /// RFC 8981 `TEMP_VALID_LIFETIME` in nanoseconds.
    temp_valid: u128,
    /// RFC 8981 `MAX_DESYNC_FACTOR` in nanoseconds (`0.4 *
    /// TEMP_PREFERRED_LIFETIME`); the desync jitter is drawn below it.
    max_desync: u128,
    /// RFC 8981 `REGEN_ADVANCE` in nanoseconds.
    regen_advance: u128,
    /// The injected CSPRNG seam for temporary identifiers/jitter.
    temp_source: Box<dyn TempAddrSource>,
    /// Per-prefix duplicate-IID retry guards (bounded by the address
    /// table size).
    temp_guards: Vec<TempGuard>,
    /// When a one-off temporary-address maintenance pass is owed
    /// regardless of any address deadline — set to `now` when privacy
    /// is enabled at runtime so temporary addresses form promptly for
    /// prefixes already configured. [`NEVER`] otherwise; cleared by
    /// the next maintenance pass.
    temp_maintenance_at: u128,
}

impl Iface {
    /// Create the engine and begin bring-up at `now`: the link-local
    /// address enters DAD (its first solicitation due after
    /// [`IfaceConfig::start_delay`]), and Router Solicitations follow
    /// once it is usable.
    /// `temp_source` is the injected CSPRNG seam RFC 8981 temporary
    /// addresses draw their randomised identifiers and desync jitter
    /// from; it is consulted only while [`IfaceConfig::privacy`] is on.
    #[must_use]
    pub fn new(
        config: &IfaceConfig,
        temp_source: Box<dyn TempAddrSource>,
        now: Duration64,
    ) -> Self {
        let temp_preferred = nanos(config.temp_preferred_lifetime);
        let retrans = nanos(config.retrans_timer);
        // RFC 8981 §3.8: REGEN_ADVANCE = 2s + (TEMP_IDGEN_RETRIES *
        // DupAddrDetectTransmits * RetransTimer), so the successor's DAD
        // always completes before the current address deprecates.
        let regen_advance = nanos(Duration64::from_secs(2)).saturating_add(
            u128::from(TEMP_IDGEN_RETRIES)
                .saturating_mul(u128::from(config.dad_transmits))
                .saturating_mul(retrans),
        );
        let mut iface = Self {
            interface_id: config.interface_id,
            dad_transmits: config.dad_transmits,
            retrans_timer: retrans,
            start_delay: nanos(config.start_delay),
            v4: None,
            v6: Vec::new(),
            v6_disabled: false,
            v6_admin_disabled: !config.ipv6_enabled,
            rs: RsState::NotStarted,
            privacy: config.privacy,
            temp_preferred,
            temp_valid: nanos(config.temp_valid_lifetime),
            max_desync: temp_preferred / MAX_DESYNC_DEN * MAX_DESYNC_NUM,
            regen_advance,
            temp_source,
            temp_guards: Vec::new(),
            temp_maintenance_at: NEVER,
        };
        if !iface.v6_admin_disabled {
            iface.start_link_local(now);
        }
        iface
    }

    /// Form the link-local address and begin its DAD/RS bring-up at
    /// `now`. Called at construction and when IPv6 is re-enabled; a
    /// no-op if a link-local record already exists.
    fn start_link_local(&mut self, now: Duration64) {
        let link_local = self.address_in(LINK_LOCAL_PREFIX);
        if self.find_v6(link_local).is_some() {
            return;
        }
        let start = nanos(now).saturating_add(self.start_delay);
        self.push_v6(
            link_local,
            64,
            AddrOrigin::LinkLocal,
            NEVER,
            NEVER,
            NEVER,
            start,
        );
    }

    /// Administratively enable or disable IPv6 on this interface
    /// (`net.ipv6.enabled`).
    ///
    /// Disabling flushes every IPv6 address (link-local, static, and
    /// SLAAC), halts Router Solicitation, and makes the interface
    /// refuse new IPv6 assignment and drop all inbound IPv6 — it binds
    /// no address and answers nothing. Enabling re-forms the link-local
    /// address and restarts bring-up at `now`. Idempotent.
    ///
    /// A DAD-failure disable ([`Iface::v6_disabled`]) is a separate,
    /// stronger condition: re-enabling does not re-form a link-local
    /// that a duplicate on the link already claimed.
    pub fn set_ipv6_enabled(&mut self, enabled: bool, now: Duration64) {
        if enabled {
            if self.v6_admin_disabled {
                self.v6_admin_disabled = false;
                if !self.v6_disabled {
                    self.start_link_local(now);
                }
            }
        } else if !self.v6_admin_disabled {
            self.v6_admin_disabled = true;
            self.v6.clear();
            self.temp_guards.clear();
            self.temp_maintenance_at = NEVER;
            self.rs = RsState::NotStarted;
        }
    }

    /// Enable or disable RFC 8981 temporary (privacy) addresses at
    /// runtime (`net.ipv6.privacy`). Idempotent.
    ///
    /// Enabling schedules an immediate maintenance pass at `now`, so a
    /// temporary address forms for every autonomous prefix already
    /// configured (a fresh one is also formed as each future Router
    /// Advertisement adds a prefix). Disabling removes every temporary
    /// address and clears the per-prefix retry guards; the stable
    /// SLAAC address of each prefix is untouched.
    pub fn set_privacy(&mut self, enabled: bool, now: Duration64) {
        if self.privacy == enabled {
            return;
        }
        self.privacy = enabled;
        if enabled {
            self.temp_maintenance_at = nanos(now);
        } else {
            self.v6
                .retain(|entry| entry.origin != AddrOrigin::Temporary);
            self.temp_guards.clear();
            self.temp_maintenance_at = NEVER;
        }
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
        if self.v6_disabled || self.v6_admin_disabled {
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
                NEVER,
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
        let prefix = prefix_bits(target);
        self.v6.swap_remove(index);
        if origin == AddrOrigin::LinkLocal {
            self.v6_disabled = true;
            self.v6.clear();
            self.temp_guards.clear();
            self.rs = RsState::Done;
        } else if origin == AddrOrigin::Temporary {
            // RFC 8981 §3.4: a duplicate temporary identifier is
            // retried with a fresh one up to TEMP_IDGEN_RETRIES times,
            // after which the engine stops forming them for the prefix.
            self.bump_temp_guard(prefix);
        }
        Some(IfaceAction::DadFailed { addr: target })
    }

    /// RFC 8981 temporary-address maintenance, run at the tail of
    /// [`Self::advance`]: form a temporary (privacy) address for every
    /// stable SLAAC prefix that lacks a fresh one, and regenerate one
    /// whose preferred lifetime is within `REGEN_ADVANCE` of expiry so
    /// a fresh randomised address is always preferred before the
    /// current one deprecates. Drives each new address's first DAD
    /// solicitation, appending its send-intent to `actions`. A no-op
    /// unless privacy is enabled and IPv6 is up.
    fn maintain_temp_addresses(&mut self, now: u128, actions: &mut Vec<IfaceAction>) {
        // Consume the one-off runtime-enable trigger unconditionally,
        // so a disabled or torn-down interface never re-fires it.
        self.temp_maintenance_at = NEVER;
        if !self.privacy || self.v6_disabled || self.v6_admin_disabled {
            return;
        }
        // Snapshot each stable SLAAC prefix and the lifetimes a
        // temporary address inherits (capped) from it. Collected first
        // so the address table is not borrowed while it is mutated.
        let mut prefixes: Vec<([u8; 8], u128, u128)> = Vec::new();
        for entry in &self.v6 {
            if entry.origin != AddrOrigin::Slaac {
                continue;
            }
            let prefix = prefix_bits(entry.addr);
            if !prefixes.iter().any(|(bits, _, _)| *bits == prefix) {
                prefixes.push((prefix, entry.valid_until, entry.preferred_until));
            }
        }
        for (prefix, stable_valid, stable_preferred) in prefixes {
            if self.temp_guard_disabled(prefix) || self.has_fresh_temp(prefix, now) {
                continue;
            }
            // The outgoing temporary address (if any) has reached its
            // regeneration point: stop its `regen_at` from re-firing.
            self.mark_temp_regen_done(prefix);
            if self.v6.len() >= MAX_IPV6_ADDRS {
                continue;
            }
            if let Some(action) = self.generate_temp(prefix, stable_valid, stable_preferred, now) {
                actions.push(action);
            }
        }
        self.prune_temp_guards();
    }

    /// Whether `prefix` already has a temporary address that need not
    /// be (re)generated: a tentative one, or a preferred one whose
    /// successor is not yet due.
    fn has_fresh_temp(&self, prefix: [u8; 8], now: u128) -> bool {
        self.v6.iter().any(|entry| {
            entry.origin == AddrOrigin::Temporary
                && prefix_bits(entry.addr) == prefix
                && match entry.state {
                    AddrState::Tentative { .. } => true,
                    AddrState::Preferred => !entry.regen_done && now < entry.regen_at,
                    AddrState::Deprecated => false,
                }
        })
    }

    /// Mark every preferred temporary address of `prefix` as having had
    /// its successor generated, so its (now-past) `regen_at` deadline
    /// stops scheduling maintenance.
    fn mark_temp_regen_done(&mut self, prefix: [u8; 8]) {
        for entry in &mut self.v6 {
            if entry.origin == AddrOrigin::Temporary
                && prefix_bits(entry.addr) == prefix
                && entry.state == AddrState::Preferred
                && !entry.regen_done
            {
                entry.regen_done = true;
                entry.rearm();
            }
        }
    }

    /// Form one temporary address for `prefix`, its lifetimes capped by
    /// the stable prefix's advertised `stable_valid`/`stable_preferred`
    /// and shortened by a random `DESYNC_FACTOR`, and start its DAD.
    /// Returns the first send-intent, or `None` when no usable
    /// identifier could be drawn or the prefix's remaining preferred
    /// lifetime is too short to be worth a privacy address.
    fn generate_temp(
        &mut self,
        prefix: [u8; 8],
        stable_valid: u128,
        stable_preferred: u128,
        now: u128,
    ) -> Option<IfaceAction> {
        // RFC 8981 §3.4: preferred = min(prefix preferred,
        // TEMP_PREFERRED_LIFETIME - DESYNC_FACTOR); valid =
        // min(prefix valid, TEMP_VALID_LIFETIME).
        let desync = self.draw_desync();
        let mut preferred_until = now.saturating_add(self.temp_preferred.saturating_sub(desync));
        if stable_preferred != NEVER {
            preferred_until = preferred_until.min(stable_preferred);
        }
        let mut valid_until = now.saturating_add(self.temp_valid);
        if stable_valid != NEVER {
            valid_until = valid_until.min(stable_valid);
        }
        preferred_until = preferred_until.min(valid_until);
        // Not worth forming (and would churn) if the successor would be
        // due at or before birth: require a preferred span beyond
        // REGEN_ADVANCE.
        if preferred_until <= now.saturating_add(self.regen_advance) {
            return None;
        }
        let addr = self.draw_temp_iid(prefix)?;
        let regen_at = preferred_until.saturating_sub(self.regen_advance);
        self.push_v6(
            addr,
            64,
            AddrOrigin::Temporary,
            valid_until,
            preferred_until,
            regen_at,
            now,
        );
        let index = self.v6.len() - 1;
        if self.dad_transmits == 0 {
            // DAD disabled: push_v6 made it immediately preferred.
            return Some(IfaceAction::AddressPreferred { addr });
        }
        // Drive the first DAD solicitation now (mirrors the tentative
        // handling in `advance`, so no tick is wasted).
        self.v6[index].state = AddrState::Tentative { sent: 1 };
        self.v6[index].deadline = now.saturating_add(self.retrans_timer);
        Some(IfaceAction::SendDadSolicit { target: addr })
    }

    /// Draw a fresh, unpredictable, non-reserved temporary interface
    /// identifier for `prefix` and return the resulting address, or
    /// `None` if every bounded attempt was reserved or already present.
    fn draw_temp_iid(&mut self, prefix: [u8; 8]) -> Option<Ipv6Addr> {
        // A handful of draws is ample: a CSPRNG collision with a
        // reserved value or an existing address is astronomically
        // unlikely, and the bound keeps this total.
        const DRAW_ATTEMPTS: u8 = 4;
        for _ in 0..DRAW_ATTEMPTS {
            let mut iid = [0u8; 8];
            self.temp_source.fill_random(&mut iid);
            if is_reserved_iid(iid) || iid == self.interface_id {
                continue;
            }
            let addr = address_with_iid(prefix, iid);
            if self.find_v6(addr).is_none() {
                return Some(addr);
            }
        }
        None
    }

    /// Draw the RFC 8981 §3.8 `DESYNC_FACTOR`: a value in
    /// `0..MAX_DESYNC_FACTOR` nanoseconds, used to shorten a temporary
    /// address's preferred lifetime so peers do not regenerate in
    /// lock-step.
    fn draw_desync(&mut self) -> u128 {
        if self.max_desync == 0 {
            return 0;
        }
        let mut bytes = [0u8; 8];
        self.temp_source.fill_random(&mut bytes);
        u128::from(u64::from_le_bytes(bytes)) % self.max_desync
    }

    /// Whether the duplicate-IID retry budget for `prefix` is spent.
    fn temp_guard_disabled(&self, prefix: [u8; 8]) -> bool {
        self.temp_guards
            .iter()
            .any(|guard| guard.prefix == prefix && guard.failures >= TEMP_IDGEN_RETRIES)
    }

    /// Record one duplicate-IID DAD failure for `prefix`.
    fn bump_temp_guard(&mut self, prefix: [u8; 8]) {
        if let Some(guard) = self.temp_guards.iter_mut().find(|g| g.prefix == prefix) {
            guard.failures = guard.failures.saturating_add(1);
        } else if self.temp_guards.len() < MAX_IPV6_ADDRS {
            self.temp_guards.push(TempGuard {
                prefix,
                failures: 1,
            });
        }
    }

    /// Clear the retry budget for `prefix` after a temporary address
    /// there survives DAD.
    fn reset_temp_guard(&mut self, prefix: [u8; 8]) {
        self.temp_guards.retain(|guard| guard.prefix != prefix);
    }

    /// Drop retry guards for prefixes whose stable SLAAC address is
    /// gone, so a re-advertised prefix starts with a fresh budget.
    fn prune_temp_guards(&mut self) {
        let v6 = &self.v6;
        self.temp_guards.retain(|guard| {
            v6.iter()
                .any(|e| e.origin == AddrOrigin::Slaac && prefix_bits(e.addr) == guard.prefix)
        });
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
                        let origin = entry.origin;
                        let addr = entry.addr;
                        actions.push(IfaceAction::AddressPreferred { addr });
                        if origin == AddrOrigin::LinkLocal {
                            link_local_ready = true;
                        }
                        entry.rearm();
                        // A temporary address that survives DAD clears
                        // its prefix's duplicate-IID retry count.
                        if origin == AddrOrigin::Temporary {
                            self.reset_temp_guard(prefix_bits(addr));
                        }
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
        self.maintain_temp_addresses(now, &mut actions);
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
            .chain(core::iter::once(self.temp_maintenance_at))
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

    /// Whether IPv6 is administratively disabled by policy
    /// (`net.ipv6.enabled false`). The stack drops all inbound IPv6
    /// while this holds, so the interface answers nothing.
    #[must_use]
    pub fn v6_admin_disabled(&self) -> bool {
        self.v6_admin_disabled
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
                    temporary: entry.origin == AddrOrigin::Temporary,
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

    /// Form this interface's address inside the /64 `prefix` bits
    /// using the stable interface identifier.
    fn address_in(&self, prefix: [u8; 8]) -> Ipv6Addr {
        address_with_iid(prefix, self.interface_id)
    }

    fn find_v6(&self, addr: Ipv6Addr) -> Option<usize> {
        self.v6.iter().position(|entry| entry.addr == addr)
    }

    /// Insert a new record entering DAD, with its first solicitation
    /// due at `start` (immediately preferred when DAD is disabled).
    // Each argument is an independent field of the record being
    // inserted (address, prefix length, origin, the two lifetime
    // deadlines, the regeneration time, and the start time); bundling
    // them into a throwaway struct would only obscure the four call
    // sites, so the list is deliberately flat.
    #[allow(clippy::too_many_arguments)]
    fn push_v6(
        &mut self,
        addr: Ipv6Addr,
        prefix_len: u8,
        origin: AddrOrigin,
        valid_until: u128,
        preferred_until: u128,
        regen_at: u128,
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
            regen_at,
            regen_done: false,
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

    /// Re-arm [`Self::deadline`] to the earliest pending transition of
    /// a usable address. For a still-to-be-regenerated temporary
    /// address that includes its `regen_at` successor-generation time,
    /// so [`Iface::advance`] runs then; once regeneration is done the
    /// deadline falls back to the lifetime transitions.
    fn rearm(&mut self) {
        self.deadline = match self.state {
            AddrState::Tentative { .. } => self.deadline,
            AddrState::Preferred => {
                let mut deadline = self.preferred_until.min(self.valid_until);
                if self.origin == AddrOrigin::Temporary && !self.regen_done {
                    deadline = deadline.min(self.regen_at);
                }
                deadline
            }
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

/// Combine the leading 64 `prefix` bits with a 64-bit interface
/// identifier into a full address.
fn address_with_iid(prefix: [u8; 8], iid: [u8; 8]) -> Ipv6Addr {
    let mut octets = [0u8; 16];
    octets[..8].copy_from_slice(&prefix);
    octets[8..].copy_from_slice(&iid);
    Ipv6Addr::from(octets)
}

/// Whether a 64-bit interface identifier is reserved and so must not be
/// used for a temporary address (RFC 8981 §3.3.2, RFC 5453).
///
/// Rejects the Subnet-Router Anycast identifier (all zeros), the
/// RFC 2526 Reserved Subnet Anycast range (`fdff:ffff:ffff:ff80` …
/// `…ffff`), and the IANA Ethernet-block identifiers
/// (`0200:5eff:fe00:0000` … `0200:5eff:feff:ffff`) that a modified
/// EUI-64 address would use. A CSPRNG draw hits one of these only
/// astronomically rarely; rejecting keeps a temporary address from
/// masquerading as a reserved or vendor-derived one.
fn is_reserved_iid(iid: [u8; 8]) -> bool {
    if iid == [0; 8] {
        return true;
    }
    let reserved_anycast_prefix = [0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    if iid[..7] == reserved_anycast_prefix && iid[7] >= 0x80 {
        return true;
    }
    iid[..5] == [0x02, 0x00, 0x5E, 0xFF, 0xFE]
}

/// Deadline for a RA lifetime in seconds; `u32::MAX` means no expiry
/// (RFC 4861 §4.6.2).
fn lifetime_deadline(now: u128, lifetime_secs: u32) -> u128 {
    if lifetime_secs == u32::MAX {
        return NEVER;
    }
    now.saturating_add(u128::from(lifetime_secs) * NANOS_PER_SEC_U128)
}

#[cfg(test)]
#[path = "iface_tests.rs"]
mod tests;
