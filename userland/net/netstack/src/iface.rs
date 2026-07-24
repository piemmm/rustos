//! The interface table: one `lib/net` [`Stack`] per managed NIC, plus
//! the frame-ring glue that pumps frames between the engine and a
//! link-layer driver (`plans/NETWORK.md` §2.2).
//!
//! The table is the service's single source of truth for interface
//! identity: an interface is named by its admin-chosen alias
//! (`wan`, `lan0` — never a discovery-order name), carries exactly one
//! protocol engine, and is observed through the typed record types the
//! `netstack-v1` protocol defines. All protocol behaviour lives in the
//! pure engine; this module only owns, names, and feeds it.

use alloc::vec::Vec;

use tairix_abi::driver::net::{DeviceFacts, LinkState, NetOffloads};
use tairix_abi::driver::net_ring::{FrameOffload, FrameRings};
use tairix_abi::net_ipc::{
    validate_if_name, NetAddrFamily, NetAddrState, NetBondConfigMsg, NetBondMemberRecord,
    NetBondMode, NetCounters, NetIfAddr, NetIfKind, NetInterfaceConfigMsg,
    NetInterfaceCountersRecord, NetInterfaceFactsRecord, NetInterfaceRatesRecord,
    NetInterfaceStateRecord, NetIpv4Config, NetIpv6Config, NetworkSettings, IF_NAME_LEN,
    NET_IF_MAX_ADDRS,
};
use tairix_abi::{Duration64, Errno};
use tairix_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::bond::{flow_hash, Bond, BondConfig, BondEvent, BondMode, MemberId};
use tairix_net::iface::{eui64_interface_id, AddrError};
use tairix_net::internet_checksum;
use tairix_net::rate::{RateCounters, RateMeter, RateSelector};
use tairix_net::stack::{
    RxMeta, SendError, Stack, StackConfig, StackEvent, StackOutput, TxFrame, TxOffload,
};
use tairix_net::tcp::TcpSegmentMeta;

use crate::channel::FrameService;

/// The internal role an interface-table entry plays in link aggregation.
///
/// A bond is a virtual interface that owns the addresses, routes, and
/// neighbour cache; its members are the physical NICs that carry its
/// frames but hold no addresses of their own. A plain interface is
/// [`BondRole::None`].
#[derive(Clone, Debug, Default)]
enum BondRole {
    /// Not part of any bond (a plain interface or the loopback).
    #[default]
    None,
    /// A bond virtual interface: the [`Bond`] decision engine and the
    /// declared member aliases (in configured order). The owning
    /// [`Interface`]'s [`Stack`] is the bond's own stack.
    Bond {
        /// The link-aggregation decision engine.
        engine: Bond,
        /// The declared member aliases, in configured order.
        members: Vec<[u8; IF_NAME_LEN]>,
    },
    /// A member NIC enrolled in the named bond. Its own [`Stack`] is
    /// dormant (address-less); received frames fold into the bond's stack
    /// and it refuses direct address assignment.
    Member {
        /// The bond alias this NIC is enrolled in.
        bond: [u8; IF_NAME_LEN],
    },
}

/// Frames produced for one or more interfaces, each batch tagged by the
/// interface alias that emitted it, for the caller to queue onto that
/// interface's TX ring. The one definition every egress helper
/// ([`Netstack::originate`], the multicast join/leave paths) returns.
pub type FrameBatch = Vec<([u8; IF_NAME_LEN], Vec<TxFrame>)>;

/// One managed interface: its admin-chosen alias, link kind, and the
/// per-interface dual-stack protocol engine.
pub struct Interface {
    name: [u8; IF_NAME_LEN],
    kind: NetIfKind,
    facts: DeviceFacts,
    stack: Stack,
    /// The tickless windowed-throughput meter (`stats:net/<iface>/…`).
    rates: RateMeter,
    /// This entry's link-aggregation role (plain, bond, or member).
    role: BondRole,
}

impl Interface {
    /// The interface's admin-chosen alias, NUL-padded.
    #[must_use]
    pub fn name(&self) -> [u8; IF_NAME_LEN] {
        self.name
    }

    /// Borrow the protocol engine (read-only observers).
    #[must_use]
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    /// Borrow the protocol engine mutably (diagnostic senders).
    pub fn stack_mut(&mut self) -> &mut Stack {
        &mut self.stack
    }
}

/// The service's interface table and the engine glue around it.
///
/// Grows on demand — an interface is added per discovered NIC, never
/// from a compile-time ceiling. Reply paging bounds what one IPC
/// answer carries; it never bounds how many interfaces exist.
#[derive(Default)]
pub struct Netstack {
    interfaces: Vec<Interface>,
    /// Stack-wide `net.*` policy (`plans/NETWORK.md` §6.2). Its safe
    /// default (both families on, SYN cookies `auto`) holds until an
    /// FS-capable component delivers the real `system.conf` policy over
    /// the `ApplyNetworkSettings` admin op; the delivered policy governs
    /// both interfaces added afterwards and, by re-application, every
    /// interface already present.
    settings: NetworkSettings,
    /// Reusable RX pump scratch, sized lazily to the widest ring
    /// slot — allocated once, never per pumped frame (hot path).
    scratch: Vec<u8>,
    /// Reusable engine output, passed to every [`Stack`] call so the
    /// pump reuses one set of frame/event buffers across frames instead
    /// of allocating a fresh output per call — the allocation-free hot
    /// path the engine's [`StackOutput`] contract provides.
    out: StackOutput,
}

impl Netstack {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of managed interfaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.interfaces.len()
    }

    /// Whether no interface is managed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.interfaces.is_empty()
    }

    /// Add a managed interface.
    ///
    /// `interface_id` is the injected 64-bit interface identifier the
    /// SLAAC engine forms addresses from and `ipv4_ident_seed` the
    /// CSPRNG-drawn first IPv4 identification value — both drawn by
    /// the caller (the service layer owns entropy, the engine stays
    /// pure).
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — an invalid alias or device facts the
    ///   engine refuses.
    /// * [`Errno::AlreadyExists`] — the alias is already bound.
    pub fn add_interface(
        &mut self,
        name: [u8; IF_NAME_LEN],
        kind: NetIfKind,
        facts: DeviceFacts,
        interface_id: [u8; 8],
        ipv4_ident_seed: u16,
        now: Duration64,
    ) -> Result<(), Errno> {
        validate_if_name(&name)?;
        if self.find(name).is_some() {
            return Err(Errno::AlreadyExists);
        }
        // A new interface adopts the current stack-wide family policy at
        // construction: a disabled family forms no address (no link-local
        // for v6) and the engine answers nothing for it.
        let mut config = StackConfig::new(facts, interface_id, ipv4_ident_seed);
        config.ipv4_enabled = self.settings.ipv4_enabled;
        config.iface.ipv6_enabled = self.settings.ipv6_enabled;
        let stack = Stack::new(&config, now).map_err(|_| Errno::OutOfRange)?;
        self.interfaces.push(Interface {
            name,
            kind,
            facts,
            stack,
            rates: RateMeter::new(),
            role: BondRole::None,
        });
        Ok(())
    }

    fn find(&self, name: [u8; IF_NAME_LEN]) -> Option<usize> {
        self.interfaces.iter().position(|i| i.name == name)
    }

    /// The current stack-wide `net.*` policy.
    #[must_use]
    pub fn settings(&self) -> NetworkSettings {
        self.settings
    }

    /// Apply a stack-wide `net.*` policy (`ApplyNetworkSettings`).
    ///
    /// Stores the policy so interfaces added later adopt it, and
    /// re-applies the family switches to every interface already managed
    /// — enabling a family re-forms its auto-configured address
    /// (link-local for IPv6), disabling it flushes the family's addresses
    /// and routes so the interface answers nothing. Idempotent, so a
    /// redelivery of the same policy is a no-op. The SYN-cookie mode is
    /// read at `listen` time from [`Self::settings`], so it needs no
    /// per-interface re-application here.
    pub fn apply_settings(&mut self, settings: NetworkSettings, now: Duration64) {
        self.settings = settings;
        for interface in &mut self.interfaces {
            interface.stack.set_ipv4_enabled(settings.ipv4_enabled);
            interface.stack.set_ipv6_enabled(settings.ipv6_enabled, now);
        }
    }

    /// Apply one managed interface's declarative configuration
    /// (`network.conf`, `plans/NETWORK.md` N9b-3-1), delivered by the
    /// device manager over the [`NetInterfaceConfigMsg`] admin message.
    ///
    /// The interface is located by its **stable hardware identity**: when
    /// `msg` carries a MAC selector the interface whose device MAC matches
    /// is found and *renamed* to the admin-chosen alias (netstack is the
    /// only holder of each interface's MAC, from the driver's facts); a
    /// message with no selector matches an interface already bearing the
    /// alias. An interface not yet present is [`Errno::NotFound`] — the
    /// caller retries when the driver binds — and an alias already taken by
    /// a *different* interface is [`Errno::AlreadyExists`].
    ///
    /// The whole message is validated ([`NetInterfaceConfigMsg::validate`])
    /// **before** any state is touched, so a refusal leaves the interface
    /// untouched — the application is atomic per interface. Re-applying the
    /// same configuration is idempotent: a static address already assigned
    /// is a success, not a duplicate error.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — the message failed validation, or the
    ///   engine refused a validated field.
    /// * [`Errno::NotFound`] — no interface matched the selector.
    /// * [`Errno::AlreadyExists`] — the alias is bound to another
    ///   interface.
    pub fn apply_interface_config(
        &mut self,
        msg: &NetInterfaceConfigMsg,
        now: Duration64,
    ) -> Result<(), Errno> {
        // Validate the whole message up front so the mutation below is
        // atomic: after this every engine call can only fail on a resource
        // limit the fresh config never reaches, so a partial apply is not
        // possible (fail closed, leave the interface untouched).
        msg.validate()?;
        let index = match msg.match_mac {
            Some(mac) => self
                .interfaces
                .iter()
                .position(|i| i.facts.mac.as_octets() == &mac)
                .ok_or(Errno::NotFound)?,
            None => self.find(msg.alias).ok_or(Errno::NotFound)?,
        };
        // A bond member owns no addresses: the bond does. Refuse a direct
        // address assignment (static v4/v6 or SLAAC) to an enrolled member
        // (fail closed) — the member is a pure frame conduit. A disabled
        // family is harmless (a member's families are already disabled).
        if matches!(self.interfaces[index].role, BondRole::Member { .. }) {
            let assigns_address = matches!(msg.ipv4, NetIpv4Config::Static { .. })
                || matches!(
                    msg.ipv6,
                    NetIpv6Config::Static { .. } | NetIpv6Config::Slaac
                );
            if assigns_address {
                return Err(Errno::PermissionDenied);
            }
        }
        // Rename a MAC-matched interface to its admin-chosen alias, unless
        // the alias is already taken by a *different* interface (fail
        // closed rather than collide two aliases).
        if self.interfaces[index].name != msg.alias {
            if let Some(other) = self.find(msg.alias) {
                if other != index {
                    return Err(Errno::AlreadyExists);
                }
            }
            self.interfaces[index].name = msg.alias;
        }
        let stack = &mut self.interfaces[index].stack;
        if msg.mtu != 0 {
            stack.set_mtu(msg.mtu);
        }
        match msg.ipv4 {
            NetIpv4Config::Disabled => stack.set_ipv4_enabled(false),
            NetIpv4Config::Static {
                addr,
                prefix,
                gateway,
            } => {
                stack.set_ipv4_enabled(true);
                stack
                    .set_ipv4_config(Ipv4Addr::from(addr), prefix, gateway.map(Ipv4Addr::from))
                    .map_err(|_| Errno::OutOfRange)?;
            }
        }
        match msg.ipv6 {
            NetIpv6Config::Disabled => stack.set_ipv6_enabled(false, now),
            NetIpv6Config::Slaac => stack.set_ipv6_enabled(true, now),
            NetIpv6Config::Static {
                addr,
                prefix,
                gateway,
            } => {
                stack.set_ipv6_enabled(true, now);
                match stack.add_ipv6_static(Ipv6Addr::from(addr), prefix, now) {
                    // A re-applied identical address is not an error: this
                    // apply is idempotent (config-vs-bind ordering aside).
                    Ok(()) | Err(AddrError::Duplicate) => {}
                    Err(_) => return Err(Errno::OutOfRange),
                }
                if let Some(gw) = gateway {
                    stack
                        .add_route_v6(Ipv6Addr::UNSPECIFIED, 0, Some(Ipv6Addr::from(gw)))
                        .map_err(|_| Errno::OutOfRange)?;
                }
            }
        }
        Ok(())
    }

    /// Borrow a managed interface by alias.
    #[must_use]
    pub fn interface(&self, name: [u8; IF_NAME_LEN]) -> Option<&Interface> {
        self.find(name).map(|i| &self.interfaces[i])
    }

    /// Borrow a managed interface mutably by alias.
    pub fn interface_mut(&mut self, name: [u8; IF_NAME_LEN]) -> Option<&mut Interface> {
        self.find(name).map(move |i| &mut self.interfaces[i])
    }

    /// The managed aliases, in table order.
    #[must_use]
    pub fn names(&self) -> Vec<[u8; IF_NAME_LEN]> {
        self.interfaces.iter().map(|i| i.name).collect()
    }

    /// Assign a static address to a named interface.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name`.
    /// * [`Errno::PermissionDenied`] — the interface is an enrolled bond
    ///   member (it owns no addresses; the bond does).
    /// * [`Errno::OutOfRange`] — the engine refused the address
    ///   (bad prefix, gateway off-subnet, table full).
    pub fn addr_add(
        &mut self,
        name: [u8; IF_NAME_LEN],
        family: NetAddrFamily,
        prefix: u8,
        addr: [u8; 16],
        now: Duration64,
    ) -> Result<(), Errno> {
        let index = self.find(name).ok_or(Errno::NotFound)?;
        if matches!(self.interfaces[index].role, BondRole::Member { .. }) {
            return Err(Errno::PermissionDenied);
        }
        let stack = &mut self.interfaces[index].stack;
        match family {
            NetAddrFamily::V4 => stack
                .set_ipv4_config(v4_of(addr), prefix, None)
                .map_err(|_| Errno::OutOfRange),
            NetAddrFamily::V6 => stack
                .add_ipv6_static(Ipv6Addr::from(addr), prefix, now)
                .map_err(|_| Errno::OutOfRange),
        }
    }

    /// Add a route through a named interface.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name`.
    /// * [`Errno::PermissionDenied`] — the interface is an enrolled bond
    ///   member (it owns no routes; the bond does).
    /// * [`Errno::OutOfRange`] — the engine refused the route.
    pub fn route_add(
        &mut self,
        name: [u8; IF_NAME_LEN],
        family: NetAddrFamily,
        prefix: u8,
        dest: [u8; 16],
        next_hop: Option<[u8; 16]>,
    ) -> Result<(), Errno> {
        let index = self.find(name).ok_or(Errno::NotFound)?;
        if matches!(self.interfaces[index].role, BondRole::Member { .. }) {
            return Err(Errno::PermissionDenied);
        }
        let stack = &mut self.interfaces[index].stack;
        match family {
            NetAddrFamily::V4 => stack
                .add_route_v4(v4_of(dest), prefix, next_hop.map(v4_of))
                .map_err(|_| Errno::OutOfRange),
            NetAddrFamily::V6 => stack
                .add_route_v6(Ipv6Addr::from(dest), prefix, next_hop.map(Ipv6Addr::from))
                .map_err(|_| Errno::OutOfRange),
        }
    }

    /// The whole table's live stack counters, one record per interface,
    /// from `offset` in table order.
    #[must_use]
    pub fn counters_records(&self, offset: u32, limit: u16) -> Vec<NetInterfaceCountersRecord> {
        self.interfaces
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| {
                let c = i.stack.counters();
                NetInterfaceCountersRecord {
                    name: i.name,
                    counters: NetCounters {
                        rx_frames: c.rx_frames,
                        rx_bytes: c.rx_bytes,
                        rx_dropped: c.rx_dropped,
                        tx_frames: c.tx_frames,
                        tx_bytes: c.tx_bytes,
                        icmp_errors_sent: c.icmp_errors_sent,
                        icmp_errors_suppressed: c.icmp_errors_suppressed,
                        reassembly_expired: c.reassembly_expired,
                        pending_dropped: c.pending_dropped,
                    },
                }
            })
            .collect()
    }

    /// The whole table's live throughput rates over `window`, one record
    /// per interface, from `offset` in table order.
    ///
    /// Reading also records a fresh counter snapshot, so repeated polling
    /// builds a usable window even on an interface the frame pump is not
    /// otherwise servicing. Each record carries the window that *actually*
    /// elapsed for that interface (a just-created or long-idle interface
    /// reports a shorter — possibly zero — window rather than a fabricated
    /// figure).
    pub fn rates_records(
        &mut self,
        offset: u32,
        limit: u16,
        window: Duration64,
        now: Duration64,
    ) -> Vec<NetInterfaceRatesRecord> {
        self.interfaces
            .iter_mut()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| {
                let current = rate_counters_of(&i.stack);
                i.rates.record(now, current);
                let rx_pps = i.rates.rate(now, current, window, RateSelector::RxPackets);
                let tx_pps = i.rates.rate(now, current, window, RateSelector::TxPackets);
                let rx_bps = i.rates.rate(now, current, window, RateSelector::RxBits);
                let tx_bps = i.rates.rate(now, current, window, RateSelector::TxBits);
                NetInterfaceRatesRecord {
                    name: i.name,
                    // Every selector shares one baseline, so one window.
                    window: rx_pps.window,
                    rx_pps: rx_pps.value,
                    rx_bps: rx_bps.value,
                    tx_pps: tx_pps.value,
                    tx_bps: tx_bps.value,
                }
            })
            .collect()
    }

    /// Originate a UDP datagram from every interface that can carry it,
    /// returning the frames each produced tagged by interface alias so the
    /// caller can queue them onto that interface's TX ring.
    ///
    /// Egress selection is deterministic and per-link: interfaces are
    /// tried in table order. A **unicast** destination is sent out the
    /// first interface whose engine accepts it (one link reaches the
    /// destination); a **multicast** group is a per-link concept, so it is
    /// sent out *every* interface that accepts it. An interface's engine
    /// parks the datagram on neighbour resolution when needed, emitting the
    /// resolution frames now and the datagram once the neighbour answers —
    /// exactly the unicast echo behaviour.
    ///
    /// # Errors
    ///
    /// * [`Errno::MessageTooLarge`] — the datagram cannot fit the path
    ///   (v6) or the length field, on an interface that otherwise matched.
    /// * [`Errno::NetworkUnreachable`] — no interface has a route to the
    ///   destination, a usable source address, or an up link.
    /// * [`Errno::OutOfRange`] — the destination is not a legal datagram
    ///   destination (broadcast / unspecified).
    pub fn originate(
        &mut self,
        dest: IpAddr,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
        now: Duration64,
    ) -> Result<FrameBatch, Errno> {
        let multicast = match dest {
            IpAddr::V4(v4) => v4.is_multicast(),
            IpAddr::V6(v6) => v6.is_multicast(),
        };
        let mut batches: FrameBatch = Vec::new();
        // Remember the most specific refusal so a genuine "too large"
        // (actionable) is surfaced rather than masked as "unreachable".
        let mut deferred: Option<Errno> = None;
        let Self {
            interfaces, out, ..
        } = self;
        for iface in interfaces.iter_mut() {
            match iface
                .stack
                .send_datagram(dest, source_port, destination_port, payload, now, out)
            {
                Ok(()) => {
                    let frames = core::mem::take(&mut out.frames);
                    // A bond tags its egress by the member the flow selects
                    // (so the caller routes it onto a real channel); a plain
                    // interface tags by its own alias. A bond with no
                    // eligible member drops the frames (fail closed).
                    let flow = flow_of(dest, source_port, destination_port);
                    if let Some(tag) = egress_tag(&iface.role, iface.name, flow) {
                        batches.push((tag, frames));
                        if !multicast {
                            // One link carries a unicast datagram; stop.
                            break;
                        }
                    } else {
                        deferred = deferred.or(Some(Errno::NetworkUnreachable));
                    }
                }
                Err(SendError::TooLarge) => deferred = Some(Errno::MessageTooLarge),
                Err(SendError::NotUnicast) => return Err(Errno::OutOfRange),
                // No route / no source / link down on this interface: try
                // the next one, remembering the fail-closed default.
                Err(_) => deferred = deferred.or(Some(Errno::NetworkUnreachable)),
            }
        }
        if batches.is_empty() {
            return Err(deferred.unwrap_or(Errno::NetworkUnreachable));
        }
        Ok(batches)
    }

    /// Originate an ICMP/`ICMPv6` echo request to the unicast `dest`,
    /// returning the frames the first interface that can reach it produced
    /// tagged by its alias. An echo destination is always unicast, so —
    /// unlike [`originate`](Self::originate) — the first accepting interface
    /// carries it and the search stops there.
    ///
    /// # Errors
    ///
    /// * [`Errno::MessageTooLarge`] — the payload cannot fit the path MTU on
    ///   an interface that otherwise matched.
    /// * [`Errno::OutOfRange`] — the destination is not a legal echo target
    ///   (multicast / unspecified).
    /// * [`Errno::NetworkUnreachable`] — no interface has a route to the
    ///   destination, a usable source address, or an up link.
    pub fn originate_echo(
        &mut self,
        dest: IpAddr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        now: Duration64,
    ) -> Result<FrameBatch, Errno> {
        let mut deferred: Option<Errno> = None;
        let Self {
            interfaces, out, ..
        } = self;
        for iface in interfaces.iter_mut() {
            match iface
                .stack
                .send_echo_request(dest, identifier, sequence, payload, now, out)
            {
                Ok(()) => {
                    let frames = core::mem::take(&mut out.frames);
                    let flow = flow_of(dest, identifier, sequence);
                    if let Some(tag) = egress_tag(&iface.role, iface.name, flow) {
                        return Ok(alloc::vec![(tag, frames)]);
                    }
                    deferred = deferred.or(Some(Errno::NetworkUnreachable));
                }
                Err(SendError::TooLarge) => deferred = Some(Errno::MessageTooLarge),
                Err(SendError::NotUnicast) => return Err(Errno::OutOfRange),
                // No route / no source / link down on this interface: try
                // the next, remembering the fail-closed default.
                Err(_) => deferred = deferred.or(Some(Errno::NetworkUnreachable)),
            }
        }
        Err(deferred.unwrap_or(Errno::NetworkUnreachable))
    }

    /// Originate one TCP segment out the interface named `name`.
    ///
    /// A connected stream is bound to one egress interface for its life
    /// (chosen by [`egress_mss_for`](Self::egress_mss_for) at connect), so
    /// every later segment — data, ACKs, retransmits — is sent through this
    /// fixed link.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name` (its link is
    ///   gone; the caller drops the segment and the engine retransmits).
    /// * [`Errno::NetworkUnreachable`] / [`Errno::MessageTooLarge`] — the
    ///   engine refused the segment (no route/source, link down, or a
    ///   segment past the path MTU).
    pub fn send_tcp_on(
        &mut self,
        name: [u8; IF_NAME_LEN],
        dest: IpAddr,
        meta: &TcpSegmentMeta,
        payload: &[u8],
        gso_size: Option<u16>,
        now: Duration64,
    ) -> Result<Vec<TxFrame>, Errno> {
        let index = self.find(name).ok_or(Errno::NotFound)?;
        let Self {
            interfaces, out, ..
        } = self;
        match interfaces[index]
            .stack
            .send_tcp(dest, meta, payload, gso_size, now, out)
        {
            Ok(()) => Ok(core::mem::take(&mut out.frames)),
            Err(SendError::TooLarge) => Err(Errno::MessageTooLarge),
            Err(_) => Err(Errno::NetworkUnreachable),
        }
    }

    /// The largest TCP super-segment payload a connection out the interface
    /// named `name` may batch for segmentation offload, or `0` when the
    /// interface is unknown or its device did not negotiate the offload
    /// (so the connection stays per-MSS). Seeds a new connection's
    /// [`TcpConfig::tso_max_payload`](tairix_net::tcp::conn::TcpConfig::tso_max_payload).
    #[must_use]
    pub fn tso_max_payload_on(&self, name: [u8; IF_NAME_LEN]) -> u16 {
        self.find(name)
            .map_or(0, |index| self.interfaces[index].stack.tso_max_payload())
    }

    /// Choose the egress interface for a new TCP connection to `dest` and
    /// return its alias together with the effective local maximum segment
    /// size ([`Stack::tcp_local_mss`]) for that interface and `dest`'s
    /// family: the first interface that can reach `dest`.
    ///
    /// The connection is bound to this interface for its life, and the MSS
    /// seeds its [`TcpConfig`](tairix_net::tcp::conn::TcpConfig) so every
    /// segment fits the link before it is built (RFC 6691) — an IPv6 link's
    /// 40-byte header is accounted here, not discovered as a dropped
    /// full-size segment.
    ///
    /// # Errors
    ///
    /// [`Errno::NetworkUnreachable`] — no interface has a route, a usable
    /// source address, and an up link to `dest`.
    pub fn egress_mss_for(
        &mut self,
        dest: IpAddr,
        now: Duration64,
    ) -> Result<([u8; IF_NAME_LEN], u16), Errno> {
        for iface in &mut self.interfaces {
            if let Some(mss) = iface.stack.tcp_local_mss(dest, now) {
                return Ok((iface.name, mss));
            }
        }
        Err(Errno::NetworkUnreachable)
    }

    /// Whether any managed interface owns the local address `addr` of
    /// `family` — the check a socket `bind` to a *specific* local address
    /// makes before accepting it (fail closed: an address no interface
    /// holds could never source a datagram).
    #[must_use]
    pub fn has_local_address(&self, family: NetAddrFamily, addr: [u8; 16]) -> bool {
        self.interfaces.iter().any(|iface| match family {
            NetAddrFamily::V4 => iface
                .stack
                .iface()
                .ipv4()
                .is_some_and(|(a, _)| a == v4_of(addr)),
            NetAddrFamily::V6 => iface
                .stack
                .iface()
                .ipv6_addresses()
                .iter()
                .any(|info| info.addr == Ipv6Addr::from(addr)),
        })
    }

    /// Join multicast `group` on every managed interface (multicast
    /// membership is a per-link property), returning the frames each
    /// interface emitted (the IGMP/MLD report) tagged by alias.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `group` is not a multicast group.
    /// * [`Errno::LimitExceeded`] — an interface's bounded membership
    ///   table is full (fail closed).
    pub fn join_multicast_all(
        &mut self,
        group: IpAddr,
        now: Duration64,
    ) -> Result<FrameBatch, Errno> {
        let mut batches = FrameBatch::new();
        let Self {
            interfaces, out, ..
        } = self;
        for iface in interfaces.iter_mut() {
            // A member owns no membership; the bond does. Skip members.
            if matches!(iface.role, BondRole::Member { .. }) {
                continue;
            }
            match iface.stack.join_multicast(group, now) {
                // A fresh join emits a membership report to announce it.
                Ok(true) => {
                    iface.stack.advance(now, out);
                    let frames = core::mem::take(&mut out.frames);
                    if let Some(tag) = egress_tag(&iface.role, iface.name, 0) {
                        batches.push((tag, frames));
                    }
                }
                // Already a member (a prior reference): no new report.
                Ok(false) => {}
                Err(tairix_net::stack::McastError::NotMulticast) => return Err(Errno::OutOfRange),
                Err(tairix_net::stack::McastError::CapacityExhausted) => {
                    return Err(Errno::LimitExceeded)
                }
            }
        }
        Ok(batches)
    }

    /// Leave multicast `group` on every managed interface, returning the
    /// frames each interface emitted (an IGMP Leave when the last
    /// reference dropped) tagged by alias.
    pub fn leave_multicast_all(&mut self, group: IpAddr, now: Duration64) -> FrameBatch {
        let mut batches = FrameBatch::new();
        let Self {
            interfaces, out, ..
        } = self;
        for iface in interfaces.iter_mut() {
            if matches!(iface.role, BondRole::Member { .. }) {
                continue;
            }
            if iface.stack.leave_multicast(group, now) {
                iface.stack.advance(now, out);
                let frames = core::mem::take(&mut out.frames);
                if let Some(tag) = egress_tag(&iface.role, iface.name, 0) {
                    batches.push((tag, frames));
                }
            }
        }
        batches
    }

    /// The whole table's static facts, one record per interface, from
    /// `offset` in table order.
    #[must_use]
    pub fn facts_records(&self, offset: u32, limit: u16) -> Vec<NetInterfaceFactsRecord> {
        self.interfaces
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| NetInterfaceFactsRecord {
                name: i.name,
                kind: i.kind,
                mac: *i.facts.mac.as_octets(),
                mtu: i.facts.mtu,
                offloads: i.facts.offloads.bits(),
                rx_queues: i.facts.rx_queues,
            })
            .collect()
    }

    /// The whole table's live link/address state, one record per
    /// interface, from `offset` in table order.
    #[must_use]
    pub fn state_records(&self, offset: u32, limit: u16) -> Vec<NetInterfaceStateRecord> {
        self.interfaces
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| {
                let mut addrs = [NetInterfaceStateRecord::EMPTY_ADDR; NET_IF_MAX_ADDRS];
                // Bounded by NET_IF_MAX_ADDRS (8), so u8 holds it exactly.
                let mut count: u8 = 0;
                if let Some((addr, prefix)) = i.stack.iface().ipv4() {
                    addrs[usize::from(count)] = NetIfAddr {
                        family: NetAddrFamily::V4,
                        prefix,
                        state: NetAddrState::Preferred,
                        addr: v4_bytes(addr),
                    };
                    count += 1;
                }
                for info in i.stack.iface().ipv6_addresses() {
                    if usize::from(count) == NET_IF_MAX_ADDRS {
                        break;
                    }
                    addrs[usize::from(count)] = NetIfAddr {
                        family: NetAddrFamily::V6,
                        prefix: info.prefix_len,
                        state: if info.tentative {
                            NetAddrState::Tentative
                        } else if info.deprecated {
                            NetAddrState::Deprecated
                        } else {
                            NetAddrState::Preferred
                        },
                        addr: info.addr.octets(),
                    };
                    count += 1;
                }
                // A bond's link is up when any member is eligible; a plain
                // interface's link is its device's reported link.
                let link_up = match &i.role {
                    BondRole::Bond { engine, .. } => engine.is_up(),
                    _ => i.facts.link == LinkState::Up,
                };
                NetInterfaceStateRecord {
                    name: i.name,
                    link_up,
                    addr_count: count,
                    addrs,
                }
            })
            .collect()
    }

    /// Pump one interface's frames through the frame service `fs` once:
    /// queue the engine's due output into the TX ring, doorbell the device,
    /// and feed every delivered frame back through the engine (whose replies
    /// are queued and flushed in the same pass).
    ///
    /// The pump is written once against the [`FrameService`] seam, so it
    /// drives an in-process [`Net`](tairix_abi::driver::net::Net) device
    /// ([`LocalFrameService`](crate::LocalFrameService)) and a cross-process
    /// driver ([`NetChannelClient`](crate::NetChannelClient)) identically:
    /// the service owns the frame region and each doorbell is either a direct
    /// `Net::service` or an `ipc_call` to the driver process. Ring bytes are
    /// never touched across a doorbell, so the call boundary is the whole
    /// synchronisation.
    ///
    /// Returns the typed [`StackEvent`]s the engine reported.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name`.
    /// * [`Errno::DeviceFault`] — the driver failed.
    /// * [`Errno::BadMagic`] — the ring state is corrupt.
    pub fn service_interface<F: FrameService>(
        &mut self,
        name: [u8; IF_NAME_LEN],
        fs: &mut F,
        now: Duration64,
    ) -> Result<Vec<StackEvent>, Errno> {
        let channel_index = self.find(name).ok_or(Errno::NotFound)?;
        // A member NIC has no stack of its own: its frames flow through the
        // bond's stack (the bond owns the addresses/routes). Its replies
        // are queued back onto this member's ring, which is correct for the
        // transmit member — the member a peer reaches the bond on. A plain
        // interface targets its own stack.
        let index = self.stack_target_index(channel_index);
        let geometry = fs.geometry();
        let class = fs.class();
        // Size the reusable scratch to the receive slot capacity once (the
        // scratch only ever holds a received frame; the larger transmit
        // capacity is the driver's staging concern).
        let slot_capacity = geometry.rx_slot_capacity() as usize;
        if self.scratch.len() < slot_capacity {
            self.scratch.resize(slot_capacity, 0);
        }
        // Split borrow: the pump reads `scratch` while it drives one
        // interface's engine. `fs` is a distinct object, borrowed only
        // while a ring view is bound over its region (never across a
        // doorbell).
        let Self {
            interfaces,
            scratch,
            out,
            settings: _,
        } = self;
        let iface = &mut interfaces[index];
        let mut events = Vec::new();

        // Timer-due engine output first (retransmits, DAD probes, RS),
        // queued into the TX ring bound over the service's own region.
        iface.stack.advance(now, out);
        events.append(&mut out.events);
        {
            let mut rings = FrameRings::bind(fs.region_mut(), geometry, class)?;
            queue_frames(&mut rings, &out.frames);
        }
        fs.service()?;

        // Feed delivered frames through the engine; its replies join
        // the TX ring. Bounded by the ring's slot count per pass — a
        // hostile flood cannot pin this loop.
        let mut replied = false;
        {
            let mut rings = FrameRings::bind(fs.region_mut(), geometry, class)?;
            loop {
                let mut offload = FrameOffload::None;
                match rings.rx.pop_with(&mut offload, scratch) {
                    Ok(Some(len)) => {
                        // Resolve the device's per-frame checksum offload:
                        // complete a partial checksum in place, or report a
                        // device-validated one so the engine can skip the
                        // redundant fold. A bogus offset fails closed to the
                        // software path (the engine then drops the frame).
                        let rx = resolve_rx_offload(offload, &mut scratch[..len]);
                        iface.stack.on_frame_meta(&scratch[..len], rx, now, out);
                        events.append(&mut out.events);
                        replied |= !out.frames.is_empty();
                        queue_frames(&mut rings, &out.frames);
                    }
                    Ok(None) => break,
                    // A corrupt slot was consumed; skip it and go on.
                    Err(Errno::LengthOutOfRange) => {}
                    Err(err) => return Err(err),
                }
            }
        }
        if replied {
            fs.service()?;
        }
        // Snapshot the post-pump counters for the throughput meter. Cheap
        // and self-throttling: the meter drops a sample taken within its
        // sampling gap of the last.
        iface.rates.record(now, rate_counters_of(&iface.stack));
        Ok(events)
    }

    /// The earliest engine deadline across every interface, if any —
    /// the one-shot timer the event loop arms. Folds each interface's
    /// protocol-engine deadline and, for a bond, its failover health
    /// monitor's next admission deadline (tickless: `None` when no member
    /// is awaiting readmission).
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        self.interfaces
            .iter()
            .flat_map(|i| {
                let bond = match &i.role {
                    BondRole::Bond { engine, .. } => engine.next_deadline(),
                    _ => None,
                };
                [i.stack.next_deadline(), bond]
            })
            .flatten()
            .min_by_key(|d| (d.secs(), d.subsec_nanos()))
    }
}

/// One member's live health, for the `state:net/<bond>/…` observability
/// read: the member alias, whether its link is up, and whether it is
/// currently eligible to carry traffic (admitted past the anti-flap
/// up-delay).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BondMemberHealth {
    /// The member interface's admin-chosen alias, NUL-padded.
    pub member: [u8; IF_NAME_LEN],
    /// Whether the member's link is currently up.
    pub link_up: bool,
    /// Whether the member is eligible to carry traffic now.
    pub eligible: bool,
}

// --- Link aggregation (bond) composition --------------------------------
impl Netstack {
    /// Map the pumped channel's interface index onto the stack that
    /// processes its frames: a member's frames flow through its bond's
    /// stack; every other interface uses its own.
    fn stack_target_index(&self, channel_index: usize) -> usize {
        if let BondRole::Member { bond } = self.interfaces[channel_index].role {
            if let Some(bond_index) = self.find(bond) {
                return bond_index;
            }
        }
        channel_index
    }

    /// Map the wire bond mode onto the engine policy.
    fn engine_mode(mode: NetBondMode) -> BondMode {
        match mode {
            NetBondMode::ActiveBackup => BondMode::ActiveBackup,
            NetBondMode::Balance => BondMode::Balance,
        }
    }

    /// Compose (or reconfigure) a bond interface from a
    /// [`NetBondConfigMsg`] (`plans/NETWORK.md` §6.3), delivered by the
    /// device manager over the admin endpoint.
    ///
    /// A bond is a virtual interface that owns the addresses, routes, and
    /// neighbour cache (applied separately by a [`NetInterfaceConfigMsg`]
    /// naming the bond); its members are the physical NICs that carry its
    /// frames but hold no addresses. The bond inherits the first declared
    /// member's device identity (MAC/MTU) — Linux's default bond-MAC
    /// policy — kept stable for the bond's life so a peer's ARP/ND cache
    /// survives failover.
    ///
    /// Every declared member must already be present (its driver bound and
    /// the interface renamed to the member alias); an absent member yields
    /// [`Errno::NotFound`] so the caller retries when the driver binds
    /// (the [`Self::apply_interface_config`] contract). Re-applying is
    /// idempotent and reconciles the running bond (mode, primary, monitor
    /// interval, and membership) in place — the runtime-reload path.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — the message failed validation or the
    ///   engine refused the bond's device facts.
    /// * [`Errno::NotFound`] — a declared member is not present yet.
    /// * [`Errno::AlreadyExists`] — the bond alias names a non-bond
    ///   interface, or a declared member is a bond or already enrolled in
    ///   a *different* bond.
    pub fn apply_bond_config(
        &mut self,
        msg: &NetBondConfigMsg,
        now: Duration64,
    ) -> Result<(), Errno> {
        msg.validate()?;
        // Every declared member must be present and free to enrol. Checked
        // up front so the composition below is atomic (fail closed).
        for member in msg.members() {
            let idx = self.find(*member).ok_or(Errno::NotFound)?;
            if matches!(self.interfaces[idx].kind, NetIfKind::Bond) {
                return Err(Errno::AlreadyExists);
            }
            match self.interfaces[idx].role {
                BondRole::None => {}
                BondRole::Member { bond } if bond == msg.alias => {}
                // Enrolled in another bond, or itself a bond: refuse.
                _ => return Err(Errno::AlreadyExists),
            }
        }
        let mode = Self::engine_mode(msg.mode);
        let primary = msg.primary.map(member_id);

        let bond_index = if let Some(idx) = self.find(msg.alias) {
            if !matches!(self.interfaces[idx].role, BondRole::Bond { .. }) {
                // The alias is taken by a non-bond interface.
                return Err(Errno::AlreadyExists);
            }
            idx
        } else {
            let first = self.find(msg.members()[0]).ok_or(Errno::NotFound)?;
            self.create_bond_interface(msg.alias, mode, msg.monitor_interval, primary, first, now)?;
            self.find(msg.alias).ok_or(Errno::NotFound)?
        };
        self.reconcile_bond(
            bond_index,
            mode,
            msg.monitor_interval,
            primary,
            msg.members(),
            now,
        );
        Ok(())
    }

    /// Create the bond virtual interface, its own [`Stack`] built from the
    /// first member's device identity (MAC/MTU) with **no** offloads (the
    /// bond negotiates none of its own — a member's offloads are the
    /// member's), and an empty [`Bond`] engine seeded with the policy.
    fn create_bond_interface(
        &mut self,
        alias: [u8; IF_NAME_LEN],
        mode: BondMode,
        monitor_interval: Duration64,
        primary: Option<MemberId>,
        first_member_index: usize,
        now: Duration64,
    ) -> Result<(), Errno> {
        let mut facts = self.interfaces[first_member_index].facts;
        // The virtual bond negotiates no offloads of its own; frames it
        // hands a member are plain (the member checksums/segments in its
        // own right where its config negotiated it).
        facts.offloads = NetOffloads::empty();
        let interface_id = eui64_interface_id(*facts.mac.as_octets());
        let octets = *facts.mac.as_octets();
        let ipv4_ident_seed = u16::from_le_bytes([octets[4], octets[5]]);
        let mut config = StackConfig::new(facts, interface_id, ipv4_ident_seed);
        config.ipv4_enabled = self.settings.ipv4_enabled;
        config.iface.ipv6_enabled = self.settings.ipv6_enabled;
        let mut stack = Stack::new(&config, now).map_err(|_| Errno::OutOfRange)?;
        // The bond has no admitted member yet, so its aggregate link is
        // down until the failover monitor admits one.
        stack.set_link(LinkState::Down);
        let engine = Bond::new(&BondConfig {
            mode,
            monitor_interval,
            primary,
        });
        self.interfaces.push(Interface {
            name: alias,
            kind: NetIfKind::Bond,
            facts,
            stack,
            rates: RateMeter::new(),
            role: BondRole::Bond {
                engine,
                members: Vec::new(),
            },
        });
        Ok(())
    }

    /// Reconcile a bond's engine policy and membership to the declared set
    /// (the create path starts from an empty membership; the reload path
    /// diffs the running one). Enrols newly-declared members (disabling
    /// their own stacks so they hold no addresses) and releases members no
    /// longer declared (restoring them to plain interfaces).
    fn reconcile_bond(
        &mut self,
        bond_index: usize,
        mode: BondMode,
        monitor_interval: Duration64,
        primary: Option<MemberId>,
        declared: &[[u8; IF_NAME_LEN]],
        now: Duration64,
    ) {
        // Snapshot the currently-enrolled members to diff against.
        let current: Vec<[u8; IF_NAME_LEN]> = match &self.interfaces[bond_index].role {
            BondRole::Bond { members, .. } => members.clone(),
            _ => return,
        };
        let bond_alias = self.interfaces[bond_index].name;

        // Release members no longer declared: leave the engine and become
        // plain interfaces again (re-adopting the stack-wide family policy).
        for member in &current {
            if !declared.contains(member) {
                if let BondRole::Bond { engine, .. } = &mut self.interfaces[bond_index].role {
                    let _ = engine.remove_member(member_id(*member));
                }
                if let Some(idx) = self.find(*member) {
                    self.interfaces[idx].role = BondRole::None;
                    self.reenable_member_stack(idx, now);
                }
            }
        }

        // Reassert the engine policy (idempotent).
        if let BondRole::Bond { engine, .. } = &mut self.interfaces[bond_index].role {
            engine.set_mode(mode);
            engine.set_monitor_interval(monitor_interval);
            engine.set_primary(primary);
        }

        // Enrol newly-declared members: disable the member's own stack (it
        // owns no addresses) and add it to the engine with its link state.
        for member in declared {
            if !current.contains(member) {
                let Some(idx) = self.find(*member) else {
                    continue;
                };
                self.disable_member_stack(idx, now);
                self.interfaces[idx].role = BondRole::Member { bond: bond_alias };
                let link = self.interfaces[idx].facts.link;
                if let BondRole::Bond { engine, .. } = &mut self.interfaces[bond_index].role {
                    if engine.add_member(member_id(*member)).is_ok() {
                        let _ = engine.set_member_link(member_id(*member), link, now);
                    }
                }
            }
        }

        // Record the declared membership (in configured order) and sync the
        // bond stack's link to the engine's aggregate up-state.
        if let BondRole::Bond { members, .. } = &mut self.interfaces[bond_index].role {
            *members = declared.to_vec();
        }
        self.sync_bond_link(bond_index);
    }

    /// Disable a member interface's own protocol stack so it forms and
    /// holds no addresses — a member is a pure frame conduit for its bond.
    fn disable_member_stack(&mut self, index: usize, now: Duration64) {
        let stack = &mut self.interfaces[index].stack;
        stack.set_ipv4_enabled(false);
        stack.set_ipv6_enabled(false, now);
    }

    /// Restore a released member to a plain interface: re-adopt the
    /// stack-wide family policy so it can once again form its own
    /// addresses.
    fn reenable_member_stack(&mut self, index: usize, now: Duration64) {
        let settings = self.settings;
        let stack = &mut self.interfaces[index].stack;
        stack.set_ipv4_enabled(settings.ipv4_enabled);
        stack.set_ipv6_enabled(settings.ipv6_enabled, now);
    }

    /// Report a member NIC's link-state change to its bond and act on the
    /// resulting transmit-path events, returning any gratuitous
    /// ARP/unsolicited-NA frames tagged by the newly-selected member (for
    /// the caller to transmit). A link report for a NIC that is not an
    /// enrolled member is ignored (no change).
    pub fn set_member_link(
        &mut self,
        member: [u8; IF_NAME_LEN],
        link: LinkState,
        now: Duration64,
    ) -> FrameBatch {
        let Some(member_index) = self.find(member) else {
            return FrameBatch::new();
        };
        let BondRole::Member { bond } = self.interfaces[member_index].role else {
            return FrameBatch::new();
        };
        // Track the member's own link for observability.
        self.interfaces[member_index].facts.link = link;
        let Some(bond_index) = self.find(bond) else {
            return FrameBatch::new();
        };
        let events = match &mut self.interfaces[bond_index].role {
            BondRole::Bond { engine, .. } => engine.set_member_link(member_id(member), link, now),
            _ => Vec::new(),
        };
        self.apply_bond_events(bond_index, &events, now)
    }

    /// Advance every bond's failover health monitor (admitting members
    /// past their anti-flap up-delay), returning any gratuitous
    /// announcements the resulting path changes require, tagged by the
    /// member each must go out. Folded into the service's timer sweep.
    pub fn advance_bonds(&mut self, now: Duration64) -> FrameBatch {
        let mut batch = FrameBatch::new();
        let bonds: Vec<usize> = (0..self.interfaces.len())
            .filter(|&i| matches!(self.interfaces[i].role, BondRole::Bond { .. }))
            .collect();
        for bond_index in bonds {
            let events = match &mut self.interfaces[bond_index].role {
                BondRole::Bond { engine, .. } => engine.advance(now),
                _ => Vec::new(),
            };
            batch.append(&mut self.apply_bond_events(bond_index, &events, now));
        }
        batch
    }

    /// Sync a bond's own stack link to the engine's aggregate up-state.
    fn sync_bond_link(&mut self, bond_index: usize) {
        let up = match &self.interfaces[bond_index].role {
            BondRole::Bond { engine, .. } => engine.is_up(),
            _ => return,
        };
        self.interfaces[bond_index].stack.set_link(if up {
            LinkState::Up
        } else {
            LinkState::Down
        });
    }

    /// Act on a bond's transmit-path events: keep the bond stack's link in
    /// sync, and on a [`BondEvent::PathChanged`] re-announce the bond's
    /// presence (gratuitous ARP / unsolicited NA) so peers relearn the
    /// path, returning those frames tagged by the newly-selected member.
    fn apply_bond_events(
        &mut self,
        bond_index: usize,
        events: &[BondEvent],
        now: Duration64,
    ) -> FrameBatch {
        let mut batch = FrameBatch::new();
        self.sync_bond_link(bond_index);
        let path_changed = events.iter().any(|e| matches!(e, BondEvent::PathChanged));
        if !path_changed {
            return batch;
        }
        // Emit the presence announcement on the member the flow-agnostic
        // selection now points at (the active member in active-backup).
        let member = match &self.interfaces[bond_index].role {
            BondRole::Bond { engine, .. } => engine.transmit_member(0),
            _ => None,
        };
        let Some(member) = member else {
            return batch;
        };
        let Self {
            interfaces, out, ..
        } = self;
        interfaces[bond_index].stack.announce_presence(out, now);
        let frames = core::mem::take(&mut out.frames);
        if !frames.is_empty() {
            batch.push((member, frames));
        }
        batch
    }

    /// Resolve the physical channel alias a logical interface transmits on
    /// for the given flow: a bond selects its member (active member in
    /// active-backup; flow-hashed in balance), a plain interface is itself,
    /// and a member — never addressed directly — resolves to nothing.
    /// `None` when the interface is unknown or the bond has no eligible
    /// member (fail closed).
    #[must_use]
    pub fn egress_member(&self, name: [u8; IF_NAME_LEN], flow: u32) -> Option<[u8; IF_NAME_LEN]> {
        let index = self.find(name)?;
        egress_tag(&self.interfaces[index].role, name, flow)
    }

    /// The live per-member health of the bond named `bond`
    /// (`state:net/<bond>/…`), in configured order, or `None` if `bond` is
    /// not a bond interface.
    #[must_use]
    pub fn bond_member_health(&self, bond: [u8; IF_NAME_LEN]) -> Option<Vec<BondMemberHealth>> {
        let index = self.find(bond)?;
        match &self.interfaces[index].role {
            BondRole::Bond { engine, members } => Some(
                members
                    .iter()
                    .map(|member| BondMemberHealth {
                        member: *member,
                        link_up: engine
                            .is_member_link_up(member_id(*member))
                            .unwrap_or(false),
                        eligible: engine
                            .is_member_eligible(member_id(*member))
                            .unwrap_or(false),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The bond's currently-active member (`state:net/<bond>/active-member`)
    /// in active-backup, or `None` in balance mode / when the bond is down /
    /// when `bond` is not a bond.
    #[must_use]
    pub fn bond_active_member(&self, bond: [u8; IF_NAME_LEN]) -> Option<[u8; IF_NAME_LEN]> {
        let index = self.find(bond)?;
        match &self.interfaces[index].role {
            BondRole::Bond { engine, .. } => engine.active_member(),
            _ => None,
        }
    }

    /// Every bond's members and their live health, one record per (bond,
    /// member) pair, flattened in interface-table order then configured
    /// member order, from the `offset`th pair and at most `limit` records
    /// (the [`NetstackRequest::BondMembers`](tairix_abi::net_ipc::NetstackRequest::BondMembers)
    /// page backing `info:net/<bond>/members`,
    /// `state:net/<bond>/active-member`, and per-member health).
    ///
    /// Only bond interfaces contribute; a plain interface or an enrolled
    /// member emits nothing. Each record marks whether the member is the
    /// bond's currently-active transmitting member (active-backup only) and
    /// carries its link/eligibility health from the engine.
    #[must_use]
    pub fn bond_member_records(&self, offset: u32, limit: u16) -> Vec<NetBondMemberRecord> {
        let mut records = Vec::new();
        let mut index: u32 = 0;
        for iface in &self.interfaces {
            let BondRole::Bond { engine, members } = &iface.role else {
                continue;
            };
            let active = engine.active_member();
            for member in members {
                if index < offset {
                    index += 1;
                    continue;
                }
                if records.len() >= limit as usize {
                    return records;
                }
                records.push(NetBondMemberRecord {
                    bond: iface.name,
                    member: *member,
                    active: active == Some(*member),
                    link_up: engine
                        .is_member_link_up(member_id(*member))
                        .unwrap_or(false),
                    eligible: engine
                        .is_member_eligible(member_id(*member))
                        .unwrap_or(false),
                });
                index += 1;
            }
        }
        records
    }
}
/// Map the engine's monotonic stack counters onto the four accumulators
/// the [`RateMeter`] takes throughput rates over.
fn rate_counters_of(stack: &Stack) -> RateCounters {
    let c = stack.counters();
    RateCounters {
        rx_packets: c.rx_frames,
        rx_bytes: c.rx_bytes,
        tx_packets: c.tx_frames,
        tx_bytes: c.tx_bytes,
    }
}

/// Resolve a received frame's per-frame offload metadata into the
/// [`RxMeta`] the engine consumes, completing a device-partial checksum
/// in place when required.
///
/// * [`FrameOffload::Validated`] — the device verified the transport
///   checksum; the engine may skip the software fold.
/// * [`FrameOffload::NeedsChecksum`] — the device delivered only the
///   partial (pseudo-header) sum; [`complete_partial_checksum`] finishes
///   it in place and the engine re-verifies it in software (belt and
///   braces: a mis-completed frame is dropped, never wrongly accepted).
/// * [`FrameOffload::None`], [`FrameOffload::TxChecksum`] /
///   [`FrameOffload::TxSegment`] (transmit-only descriptors a device must
///   never deliver on receive), or a bogus partial — the software path.
fn resolve_rx_offload(offload: FrameOffload, frame: &mut [u8]) -> RxMeta {
    match offload {
        FrameOffload::Validated => RxMeta::validated(),
        FrameOffload::NeedsChecksum {
            csum_start,
            csum_offset,
        } => {
            complete_partial_checksum(frame, usize::from(csum_start), usize::from(csum_offset));
            // Re-verify our completion in software rather than trust it.
            RxMeta::none()
        }
        // A transmit offload (checksum or segmentation) has no meaning on a
        // received frame; fall back to the software path (fail closed).
        FrameOffload::None | FrameOffload::TxChecksum { .. } | FrameOffload::TxSegment { .. } => {
            RxMeta::none()
        }
    }
}

/// Complete a device-partial transport checksum: fold the frame from
/// `csum_start` to the end (the field there holds the pseudo-header
/// partial sum) and store the result at `csum_start + csum_offset`.
///
/// Fails closed on out-of-range offsets — the frame is left with its
/// partial checksum, which the engine's software fold then rejects.
/// Returns whether the completion was applied.
fn complete_partial_checksum(frame: &mut [u8], csum_start: usize, csum_offset: usize) -> bool {
    let Some(field) = csum_start.checked_add(csum_offset) else {
        return false;
    };
    let Some(field_end) = field.checked_add(2) else {
        return false;
    };
    if csum_start >= frame.len() || field_end > frame.len() {
        return false;
    }
    let sum = internet_checksum(&frame[csum_start..]);
    frame[field..field_end].copy_from_slice(&sum.to_be_bytes());
    true
}

/// Map the engine's per-frame [`TxOffload`] onto the ring's
/// transport-neutral [`FrameOffload`], so a frame the engine asked the
/// device to checksum carries that request across the ring to the driver
/// (`plans/NETWORK.md` §2.3). A device that did not negotiate the offload
/// never receives one: the engine only attaches [`TxOffload::PartialChecksum`]
/// when the interface's `NetOffloads` advertised it.
pub(crate) fn frame_offload(offload: TxOffload) -> FrameOffload {
    match offload {
        TxOffload::None => FrameOffload::None,
        TxOffload::PartialChecksum {
            csum_start,
            csum_offset,
        } => FrameOffload::TxChecksum {
            csum_start,
            csum_offset,
        },
        TxOffload::TcpSegment {
            csum_start,
            csum_offset,
            gso_size,
            hdr_len,
            ipv6,
        } => FrameOffload::TxSegment {
            csum_start,
            csum_offset,
            gso_size,
            hdr_len,
            ipv6,
        },
    }
}

/// Queue engine output frames, dropping (never wedging on) overflow:
/// the engine's own retransmission machinery recovers a lost frame,
/// and its counters account the drop when the peer never answers. Each
/// frame carries its transmit offload across the ring to the driver.
fn queue_frames(rings: &mut FrameRings<'_>, frames: &[TxFrame]) {
    for frame in frames {
        if rings
            .tx
            .push_with(frame_offload(frame.offload), &frame.bytes)
            .is_err()
        {
            break;
        }
    }
}

/// Pre-queue an outbound frame batch onto a frame service's TX ring.
///
/// A socket `send`, multicast `join`/`leave`, or `close` returns the frames
/// the engine produced for a named interface (a [`FrameBatch`] entry); the
/// service layer stages them here so the next
/// [`service_interface`](Netstack::service_interface) doorbell transmits
/// them alongside the engine's timer-due output — one pump, one doorbell.
/// Overflow is dropped (the engine retransmits); the ring is never
/// wedged.
///
/// # Errors
///
/// [`Errno::BadMagic`] if the service's region does not hold a valid ring
/// header for its declared geometry.
pub fn queue_tx<F: FrameService>(fs: &mut F, frames: &[TxFrame]) -> Result<(), Errno> {
    let geometry = fs.geometry();
    let class = fs.class();
    let mut rings = FrameRings::bind(fs.region_mut(), geometry, class)?;
    queue_frames(&mut rings, frames);
    Ok(())
}

fn v4_of(bytes: [u8; 16]) -> Ipv4Addr {
    Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

/// An interface alias as a bond [`MemberId`]. The two are the same
/// fixed-width name; this names the intent (a member is keyed in the bond
/// engine by its interface alias).
fn member_id(name: [u8; IF_NAME_LEN]) -> MemberId {
    name
}

/// Resolve the physical channel alias a logical interface transmits on for
/// the given flow: a bond selects its member (the active member in
/// active-backup, flow-hashed in balance), a plain interface is itself,
/// and a member — never addressed directly — resolves to nothing. `None`
/// when a bond has no eligible member (fail closed).
fn egress_tag(role: &BondRole, name: [u8; IF_NAME_LEN], flow: u32) -> Option<[u8; IF_NAME_LEN]> {
    match role {
        BondRole::Bond { engine, .. } => engine.transmit_member(flow),
        BondRole::Member { .. } => None,
        BondRole::None => Some(name),
    }
}

/// A deterministic transmit flow hash over a destination and its transport
/// ports, for bond balance-mode member selection (`plans/NETWORK.md`
/// §6.3). The source address is left empty — a destination plus ports keys
/// a flow to one member for its life, and active-backup ignores the hash.
fn flow_of(dest: IpAddr, port_a: u16, port_b: u16) -> u32 {
    match dest {
        IpAddr::V4(v4) => flow_hash(&[], &v4.octets(), port_a, port_b),
        IpAddr::V6(v6) => flow_hash(&[], &v6.octets(), port_a, port_b),
    }
}

fn v4_bytes(addr: Ipv4Addr) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&addr.octets());
    out
}

#[cfg(test)]
mod offload_tests {
    use super::{complete_partial_checksum, resolve_rx_offload};
    use alloc::vec;
    use tairix_abi::driver::net_ring::FrameOffload;
    use tairix_net::addr::Ipv4Addr;
    use tairix_net::stack::RxMeta;
    use tairix_net::udp::{self, Pseudo, UdpDatagram, PROTOCOL_UDP};

    const SRC: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const DST: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 9);

    /// A UDP datagram whose checksum field holds only the pseudo-header
    /// partial sum — exactly what a `NEEDS_CSUM` device delivers — paired
    /// with the correct checksum the completion must reproduce.
    fn partial_udp_datagram() -> (vec::Vec<u8>, [u8; 2]) {
        let pseudo = Pseudo::V4 {
            source: SRC,
            destination: DST,
        };
        let payload = b"partial-checksum";
        let mut dg = vec![0u8; udp::UDP_HEADER_LEN + payload.len()];
        udp::write(pseudo, 4000, 53, payload, &mut dg).expect("write");
        let correct = [dg[6], dg[7]];
        // A CHECKSUM_PARTIAL sender seeds the field with the raw
        // ones-complement sum of the pseudo-header.
        let udp_len = u16::try_from(dg.len()).expect("fits");
        let partial = !pseudo.seed(PROTOCOL_UDP, udp_len).finish();
        dg[6..8].copy_from_slice(&partial.to_be_bytes());
        (dg, correct)
    }

    #[test]
    fn complete_partial_checksum_reproduces_the_transport_checksum() {
        let (mut dg, correct) = partial_udp_datagram();
        assert!(complete_partial_checksum(&mut dg, 0, 6));
        assert_eq!([dg[6], dg[7]], correct);
        // The completed datagram now verifies against the pseudo-header.
        assert!(UdpDatagram::parse(
            Pseudo::V4 {
                source: SRC,
                destination: DST,
            },
            &dg,
        )
        .is_some());
    }

    #[test]
    fn complete_partial_checksum_fails_closed_on_out_of_range_offsets() {
        let mut frame = [0u8; 16];
        let before = frame;
        // csum_start at/after the end.
        assert!(!complete_partial_checksum(&mut frame, 16, 0));
        // The checksum field would run past the end.
        assert!(!complete_partial_checksum(&mut frame, 15, 4));
        // A bogus partial leaves the frame untouched (fail closed).
        assert_eq!(frame, before);
    }

    #[test]
    fn resolve_rx_offload_maps_each_tag() {
        let mut frame = [0u8; 32];
        assert_eq!(
            resolve_rx_offload(FrameOffload::None, &mut frame),
            RxMeta::none()
        );
        assert_eq!(
            resolve_rx_offload(FrameOffload::Validated, &mut frame),
            RxMeta::validated()
        );
        // A NeedsChecksum completes the fold in place, then reports the
        // software path so the engine re-verifies our completion.
        let (mut dg, correct) = partial_udp_datagram();
        let rx = resolve_rx_offload(
            FrameOffload::NeedsChecksum {
                csum_start: 0,
                csum_offset: 6,
            },
            &mut dg,
        );
        assert_eq!(rx, RxMeta::none());
        assert_eq!([dg[6], dg[7]], correct);
    }
}
