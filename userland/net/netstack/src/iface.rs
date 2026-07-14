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

use rustos_abi::driver::net::{DeviceFacts, LinkState};
use rustos_abi::driver::net_ring::FrameRings;
use rustos_abi::net_ipc::{
    validate_if_name, NetAddrFamily, NetAddrState, NetCountersReply, NetIfAddr, NetIfKind,
    NetInterfaceFactsRecord, NetInterfaceStateRecord, IF_NAME_LEN, NET_IF_MAX_ADDRS,
};
use rustos_abi::{Duration64, Errno};
use rustos_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use rustos_net::stack::{SendError, Stack, StackConfig, StackEvent};

use crate::channel::FrameService;

/// Frames produced for one or more interfaces, each batch tagged by the
/// interface alias that emitted it, for the caller to queue onto that
/// interface's TX ring. The one definition every egress helper
/// ([`Netstack::originate`], the multicast join/leave paths) returns.
pub type FrameBatch = Vec<([u8; IF_NAME_LEN], Vec<Vec<u8>>)>;

/// One managed interface: its admin-chosen alias, link kind, and the
/// per-interface dual-stack protocol engine.
pub struct Interface {
    name: [u8; IF_NAME_LEN],
    kind: NetIfKind,
    facts: DeviceFacts,
    stack: Stack,
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

    /// A named interface's monotonic stack counters.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] — no interface bears `name`.
    pub fn counters(&self, name: [u8; IF_NAME_LEN]) -> Result<NetCountersReply, Errno> {
        let index = self.find(name).ok_or(Errno::NotFound)?;
        let c = self.interfaces[index].stack.counters();
        Ok(NetCountersReply {
            rx_frames: c.rx_frames,
            rx_dropped: c.rx_dropped,
            tx_frames: c.tx_frames,
            icmp_errors_sent: c.icmp_errors_sent,
            icmp_errors_suppressed: c.icmp_errors_suppressed,
            reassembly_expired: c.reassembly_expired,
            pending_dropped: c.pending_dropped,
        })
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
                Err(rustos_net::stack::McastError::NotMulticast) => return Err(Errno::OutOfRange),
                Err(rustos_net::stack::McastError::CapacityExhausted) => {
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
    /// drives an in-process [`Net`] device
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
        // Size the reusable scratch to this ring's slot capacity once.
        let slot_capacity = geometry.slot_capacity() as usize;
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
                match rings.rx.pop(scratch) {
                    Ok(Some(len)) => {
                        let out = iface.stack.on_frame(&scratch[..len], now);
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

/// Queue engine output frames, dropping (never wedging on) overflow:
/// the engine's own retransmission machinery recovers a lost frame,
/// and its counters account the drop when the peer never answers.
fn queue_frames(rings: &mut FrameRings<'_>, frames: &[Vec<u8>]) {
    for frame in frames {
        if rings.tx.push(frame).is_err() {
            break;
        }
    }
}

fn v4_of(bytes: [u8; 16]) -> Ipv4Addr {
    Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

fn v4_bytes(addr: Ipv4Addr) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&addr.octets());
    out
}
