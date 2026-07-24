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

use tairix_abi::driver::net::{DeviceFacts, LinkState};
use tairix_abi::driver::net_ring::{FrameOffload, FrameRings};
use tairix_abi::net_ipc::{
    validate_if_name, NetAddrFamily, NetAddrState, NetCounters, NetIfAddr, NetIfKind,
    NetInterfaceCountersRecord, NetInterfaceFactsRecord, NetInterfaceRatesRecord,
    NetInterfaceStateRecord, IF_NAME_LEN, NET_IF_MAX_ADDRS,
};
use tairix_abi::{Duration64, Errno};
use tairix_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::internet_checksum;
use tairix_net::rate::{RateCounters, RateMeter, RateSelector};
use tairix_net::stack::{RxMeta, SendError, Stack, StackConfig, StackEvent, TxFrame, TxOffload};
use tairix_net::tcp::TcpSegmentMeta;

use crate::channel::FrameService;

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
    /// Reusable RX pump scratch, sized lazily to the widest ring
    /// slot — allocated once, never per pumped frame (hot path).
    scratch: Vec<u8>,
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
        let config = StackConfig::new(facts, interface_id, ipv4_ident_seed);
        let stack = Stack::new(&config, now).map_err(|_| Errno::OutOfRange)?;
        self.interfaces.push(Interface {
            name,
            kind,
            facts,
            stack,
            rates: RateMeter::new(),
        });
        Ok(())
    }

    fn find(&self, name: [u8; IF_NAME_LEN]) -> Option<usize> {
        self.interfaces.iter().position(|i| i.name == name)
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
        for iface in &mut self.interfaces {
            match iface
                .stack
                .send_datagram(dest, source_port, destination_port, payload, now)
            {
                Ok(out) => {
                    batches.push((iface.name, out.frames));
                    if !multicast {
                        // One link carries a unicast datagram; stop.
                        break;
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
        for iface in &mut self.interfaces {
            match iface
                .stack
                .send_echo_request(dest, identifier, sequence, payload, now)
            {
                Ok(out) => return Ok(alloc::vec![(iface.name, out.frames)]),
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
        match self.interfaces[index]
            .stack
            .send_tcp(dest, meta, payload, gso_size, now)
        {
            Ok(out) => Ok(out.frames),
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
        for iface in &mut self.interfaces {
            match iface.stack.join_multicast(group, now) {
                // A fresh join emits a membership report to announce it.
                Ok(true) => batches.push((iface.name, iface.stack.advance(now).frames)),
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
        for iface in &mut self.interfaces {
            if iface.stack.leave_multicast(group, now) {
                batches.push((iface.name, iface.stack.advance(now).frames));
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
                NetInterfaceStateRecord {
                    name: i.name,
                    link_up: i.facts.link == LinkState::Up,
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
        let index = self.find(name).ok_or(Errno::NotFound)?;
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
        } = self;
        let iface = &mut interfaces[index];
        let mut events = Vec::new();

        // Timer-due engine output first (retransmits, DAD probes, RS),
        // queued into the TX ring bound over the service's own region.
        let out = iface.stack.advance(now);
        events.extend(out.events);
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
                        let out = iface.stack.on_frame_meta(&scratch[..len], rx, now);
                        events.extend(out.events);
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
    /// the one-shot timer the event loop arms.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        self.interfaces
            .iter()
            .filter_map(|i| i.stack.next_deadline())
            .min_by_key(|d| (d.secs(), d.subsec_nanos()))
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
