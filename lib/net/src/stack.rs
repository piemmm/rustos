//! The dual-stack host engine: one interface's complete IPv4 + IPv6
//! behaviour, composed from this crate's protocol modules.
//!
//! [`Stack`] owns the address engine ([`crate::iface`]), the shared
//! neighbour cache ([`crate::neigh`], ARP and ND as its two
//! providers), the v4/v6 routing tables and default-router list
//! ([`crate::route`]), fragment reassembly ([`crate::frag`]), and the
//! rate-limited ICMP error policy ([`crate::icmp`]). Frames go in
//! through [`Stack::on_frame`], timed work runs in
//! [`Stack::advance`], and the caller re-arms one one-shot timer from
//! [`Stack::next_deadline`] — pure, `now`-driven, no I/O, so the live
//! `netstack` service, the unit tests, and the fuzz harness exercise
//! identical code.
//!
//! # Output model
//!
//! Every entry point returns a [`StackOutput`]: the frames to
//! transmit and the typed events the caller reports on. This is the
//! control plane (ND, ARP, ICMP, echo) — a bounded few small frames
//! per input — so the engine materialises them as owned buffers; the
//! zero-copy bulk data path arrives with the socket layer and the
//! shared-memory frame rings (`plans/NETWORK.md` §2.3), not here.
//!
//! # Security
//!
//! Every frame is attacker-controlled. Each handler validates whole
//! inputs against the underlying codecs' rules and fails closed;
//! state additions are bounded ([`MAX_PENDING_PACKETS`],
//! [`MAX_RA_ROUTES`], [`MAX_REDIRECT_ROUTES`], the neighbour /
//! reassembly / router-list capacities); ICMP errors pass the
//! RFC 4443 §2.4 gate and a token-bucket rate limit; a Redirect is
//! honoured only from the current first hop of the destination it
//! names; multicast echo requests are refused (an amplification
//! vector — deliberate divergence from RFC 4443's MAY).

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, NetOffloads};
use tairix_abi::time::Duration64;

use crate::addr::{
    is_unicast_link_local, solicited_node_multicast, Ecn, IpAddr, Ipv4Addr, Ipv6Addr, ALL_NODES,
    ALL_ROUTERS,
};
use crate::arp::{ArpPacket, OP_REPLY, OP_REQUEST};
use crate::checksum::{ChecksumCheck, ChecksumMode, Pseudo};
use crate::dhcp::{self, Action as DhcpAction, DhcpClient, DhcpReply, Lease, SendAction};
use crate::dhcpv6::{
    self, Action as Dhcp6Action, Dhcp6Client, Dhcp6Reply, Lease6, SendAction as Send6Action,
};
use crate::eth::{
    ipv4_multicast_mac, ipv6_multicast_mac, is_group_mac, write_header, EthernetFrame, BROADCAST,
    ETHERNET_HEADER_LEN, ETHERTYPE_ARP, ETHERTYPE_IPV4, ETHERTYPE_IPV6,
};
use crate::frag::{FragKey, PushOutcome, Reassembler, ReassemblyConfig};
use crate::icmp::{
    error_allowed, ErrorContext, ErrorRateLimiter, IcmpContext, IcmpEcho, IcmpError, IcmpErrorKind,
    IcmpMessage,
};
use crate::iface::{Iface, IfaceAction, IfaceConfig, TempAddrSource};
use crate::igmp::{IgmpMessage, PROTOCOL_IGMP};
use crate::ipv4::{Ipv4Header, IPV4_HEADER_LEN, PROTOCOL_ICMP};
use crate::ipv6::{
    hop_by_hop_router_alert, walk, Ipv6Header, WalkOutcome, WalkRejection, IPV6_HEADER_LEN,
    IPV6_MIN_MTU, NEXT_HEADER_HOP_BY_HOP, NEXT_HEADER_ICMPV6, PARAM_PROBLEM_NEXT_HEADER,
};
use crate::mcast::{Igmp, JoinError, Membership, MembershipReport, Mld, ReportReason};
use crate::mld::{
    self, MldQuery, RecordType, ALL_MLDV2_ROUTERS, TYPE_MLDV2_REPORT, TYPE_MULTICAST_LISTENER_QUERY,
};
use crate::nd::{apply_redirect, NdMessage, ND_HOP_LIMIT};
use crate::neigh::{LookupResult, NeighborAction, NeighborConfig, NeighborTable};
use crate::route::{CandidateAddr, DefaultRouterList, PathMtuCache, Prefix, RoutingTable};
use crate::tcp::{self, TcpSegmentMeta, MAX_HEADER_LEN, PROTOCOL_TCP};
use crate::udp::{self, UdpDatagram, PROTOCOL_UDP};

/// Bound on frames parked awaiting neighbour resolution, in total.
/// RFC 4861 §7.2.2 requires holding at least one packet per pending
/// resolution; the bound caps the whole queue so a burst of sends to
/// dead neighbours cannot grow memory.
pub const MAX_PENDING_PACKETS: usize = 16;

/// Bound on on-link prefix routes installed from Router
/// Advertisements: a hostile router cannot grow the routing table.
pub const MAX_RA_ROUTES: usize = 32;

/// Bound on host routes installed from ND Redirects.
pub const MAX_REDIRECT_ROUTES: usize = 32;

/// ICMP Destination Unreachable code 2: protocol unreachable
/// (RFC 792).
const V4_CODE_PROTOCOL_UNREACHABLE: u8 = 2;

/// The all-systems IPv4 multicast group (`224.0.0.1`): every
/// multicast-capable host joins it, and General IGMP Queries are sent
/// to it. Joined for reception but never reported (RFC 1112).
const ALL_SYSTEMS_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 1);

/// The all-routers IPv4 multicast group (`224.0.0.2`): the destination
/// of an IGMPv2 Leave Group message (RFC 2236 §2.14).
const ALL_ROUTERS_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 2);

/// Hop limit / TTL of an emitted IGMP or MLD membership message: these
/// are single-link protocols and must never be forwarded (RFC 2236
/// §2, RFC 3810 §5).
const MEMBERSHIP_HOP_LIMIT: u8 = 1;

/// Hop limit / TTL of an originated multicast UDP datagram.
///
/// A deliberately conservative link-local default (RFC 1112 §6.1): a
/// datagram addressed to a group stays on the local link and is never
/// forwarded off it. Without a per-socket multicast-scope control (a
/// later increment, not invented here) this closes the door on
/// accidental multicast leakage and amplification off-link — the
/// fail-safe scope, chosen deliberately over a wider one.
const MULTICAST_DATA_HOP_LIMIT: u8 = 1;

/// Default per-interface multicast-group membership bound.
pub const MULTICAST_CAPACITY: usize = 32;

/// How a route entered the table (carried as route metadata).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RouteKind {
    /// Derived from an address assignment (on-link subnet).
    Connected,
    /// Administratively configured.
    Static,
    /// Installed from a Router Advertisement on-link prefix.
    RaOnLink,
    /// Installed from an ND Redirect (host route).
    Redirect,
}

/// Typed events the engine reports alongside its frames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StackEvent {
    /// An echo reply arrived for an echo request this host sent.
    EchoReply {
        /// Address the reply came from.
        source: IpAddr,
        /// Echoed identifier.
        identifier: u16,
        /// Echoed sequence number.
        sequence: u16,
        /// Echoed payload.
        payload: Vec<u8>,
    },
    /// An echo request addressed to this host was answered (the reply
    /// is already queued in the same output). Lets the service layer —
    /// and the QEMU vertical — observe the inbound direction without a
    /// second decode path.
    EchoRequestServed {
        /// Address the request came from.
        source: IpAddr,
        /// The request's identifier.
        identifier: u16,
        /// The request's sequence number.
        sequence: u16,
    },
    /// An IPv6 address completed DAD and is usable.
    AddressPreferred {
        /// The address.
        addr: Ipv6Addr,
    },
    /// An IPv6 address's valid lifetime lapsed.
    AddressInvalidated {
        /// The removed address.
        addr: Ipv6Addr,
    },
    /// DAD found a duplicate for a tentative address.
    DadFailed {
        /// The duplicate address.
        addr: Ipv6Addr,
    },
    /// Neighbour resolution failed; parked packets were dropped.
    NeighborUnreachable {
        /// The unresolvable neighbour.
        ip: IpAddr,
    },
    /// An ICMP/`ICMPv6` error about our traffic arrived.
    IcmpErrorReceived {
        /// Address the error came from.
        source: IpAddr,
        /// The error's kind and fields.
        kind: IcmpErrorKind,
    },
    /// A reassembly held incomplete fragments past its lifetime. The
    /// RFC 4443 §3.2 Time Exceeded is deliberately not emitted: the
    /// reassembler does not retain the first fragment's bytes, and
    /// the error is only permitted when they are available.
    ReassemblyExpired {
        /// Source of the expired datagram's fragments.
        source: IpAddr,
    },
    /// A validated UDP datagram addressed to this host arrived. The
    /// engine surfaces it verbatim; demultiplexing to a bound socket
    /// (and answering an unbound port with an ICMP error) is the
    /// service layer's decision, not the engine's.
    UdpDatagram {
        /// Peer address the datagram came from.
        source: IpAddr,
        /// Local address it was delivered to (an interface address or,
        /// once membership lands, a joined group).
        destination: IpAddr,
        /// Peer source port.
        source_port: u16,
        /// Local destination port.
        destination_port: u16,
        /// The datagram payload.
        payload: Vec<u8>,
    },
    /// A TCP segment addressed to this host arrived and verified its
    /// mandatory pseudo-header checksum. The engine is stateless for TCP
    /// (connection state — the [`crate::tcp::conn::Tcb`] — lives in the
    /// service layer), so it surfaces the raw, checksum-valid segment
    /// bytes with the addressing context and lets the service demultiplex
    /// it to a connection by four-tuple. A segment that fails its
    /// checksum is dropped and never surfaced.
    TcpSegment {
        /// Peer address the segment came from.
        source: IpAddr,
        /// Local address it was delivered to.
        destination: IpAddr,
        /// The IP-layer ECN codepoint the datagram carried (RFC 3168 §5).
        /// The service feeds it to the connection's [`crate::tcp::conn::Tcb`]
        /// so a Congestion-Experienced mark drives the receiver's ECE echo.
        ecn: Ecn,
        /// The checksum-valid TCP segment (header, options, payload).
        segment: Vec<u8>,
    },
    /// The interface's DHCPv4 client committed a lease (RFC 2131): the
    /// engine has applied the address, mask, and default route. The
    /// service layer records the security-relevant configuration change
    /// in the audit log; the addressing itself is already in effect.
    DhcpLeaseAcquired {
        /// The leased interface address.
        address: Ipv4Addr,
        /// The on-link prefix length derived from the lease's subnet mask.
        prefix_len: u8,
        /// The default router the lease carried, if any.
        router: Option<Ipv4Addr>,
    },
    /// The interface's DHCPv4 lease was lost (a server NAK or lease
    /// expiry) and the engine has withdrawn the address and its routes.
    /// The client re-acquires from scratch; the service audits the loss.
    DhcpLeaseLost,
    /// The interface's DHCPv6 client committed a lease (RFC 8415): the
    /// engine has applied the leased IA_NA address as a host `/128`. The
    /// service layer records the security-relevant configuration change in
    /// the audit log; the addressing itself is already in effect.
    Dhcp6LeaseAcquired {
        /// The leased interface address.
        address: Ipv6Addr,
        /// The valid lifetime the lease carried, in seconds.
        valid_lifetime: u32,
    },
    /// The interface's DHCPv6 lease was lost (expiry, a `NoBinding`, or a
    /// changed address on renewal) and the engine has withdrawn the leased
    /// address. The client re-acquires from scratch; the service audits
    /// the loss.
    Dhcp6LeaseLost,
}

impl StackEvent {
    /// Reclaim this event's owned byte buffer (if it carries one) into
    /// `pool` for reuse, so a delivered payload does not free and
    /// reallocate its buffer on the next receive.
    fn recycle_into(self, pool: &mut BufPool) {
        match self {
            StackEvent::EchoReply { payload, .. } | StackEvent::UdpDatagram { payload, .. } => {
                pool.give(payload);
            }
            StackEvent::TcpSegment { segment, .. } => pool.give(segment),
            StackEvent::EchoRequestServed { .. }
            | StackEvent::AddressPreferred { .. }
            | StackEvent::AddressInvalidated { .. }
            | StackEvent::DadFailed { .. }
            | StackEvent::NeighborUnreachable { .. }
            | StackEvent::IcmpErrorReceived { .. }
            | StackEvent::ReassemblyExpired { .. }
            | StackEvent::DhcpLeaseAcquired { .. }
            | StackEvent::DhcpLeaseLost
            | StackEvent::Dhcp6LeaseAcquired { .. }
            | StackEvent::Dhcp6LeaseLost => {}
        }
    }
}

/// Per-frame receive metadata the driver reports alongside a delivered
/// frame: which offloads the device performed on it.
///
/// Fed to [`Stack::on_frame`] so the engine can skip a redundant
/// software checksum fold when the device validated the transport
/// checksum *and* the interface negotiated that offload
/// (`plans/NETWORK.md` §2.3). The [`Default`] (absent metadata) is the
/// canonical software path — a caller that reports nothing loses no
/// safety, only the skip. The offload is never load-bearing for
/// security: every semantic validation still runs (trust is in the
/// device, never the peer).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct RxMeta {
    /// The device verified the frame's transport-layer (TCP/UDP)
    /// checksum, so the stack may skip the software fold.
    pub checksum_validated: bool,
}

impl RxMeta {
    /// Metadata for a frame with no device offloads (software path).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            checksum_validated: false,
        }
    }

    /// Metadata for a frame the device reported checksum-validated.
    #[must_use]
    pub const fn validated() -> Self {
        Self {
            checksum_validated: true,
        }
    }
}

/// Per-frame transmit offload the engine asks the egress device to
/// perform (`plans/NETWORK.md` §2.3) — the transmit counterpart of
/// [`RxMeta`].
///
/// The engine attaches this to a frame it emitted only when the egress
/// interface negotiated the matching [`NetOffloads`] capability; the
/// service layer maps it onto the ring's transport-neutral
/// [`FrameOffload`](tairix_abi::driver::net_ring::FrameOffload) and a
/// device that did not negotiate the offload never sees it. The offload
/// is never load-bearing for correctness: the same frame with
/// [`TxOffload::None`] carries a complete software checksum instead, so a
/// device that ignores the request still transmits a valid frame.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum TxOffload {
    /// No offload: the frame carries a complete software checksum.
    #[default]
    None,
    /// The transport checksum field holds only the folded pseudo-header
    /// sum; the device must fold the frame bytes from `csum_start` to the
    /// end and store the completed checksum at `csum_start + csum_offset`
    /// (virtio `VIRTIO_NET_HDR_F_NEEDS_CSUM`). Both offsets are relative
    /// to the start of the Ethernet frame.
    PartialChecksum {
        /// Byte offset in the frame where the checksummed range starts
        /// (the transport header — Ethernet + IP headers).
        csum_start: u16,
        /// Byte offset, past `csum_start`, of the 16-bit checksum field.
        csum_offset: u16,
    },
    /// The frame is one over-size TCP segment the device must split into
    /// MTU-sized packets on the wire (TCP segmentation offload). The TCP
    /// checksum field holds the pseudo-header partial sum computed with a
    /// zero length ([`ChecksumMode::PartialGso`]); the device replicates
    /// the `hdr_len`-byte header for each `gso_size`-byte payload slice,
    /// advancing the sequence number and completing each segment's
    /// checksum. `csum_start`/`csum_offset` locate the TCP checksum field
    /// as for [`TxOffload::PartialChecksum`]; `ipv6` selects the GSO type.
    TcpSegment {
        /// Byte offset in the frame where the checksummed range starts
        /// (the transport header — Ethernet + IP headers).
        csum_start: u16,
        /// Byte offset, past `csum_start`, of the 16-bit checksum field.
        csum_offset: u16,
        /// Maximum TCP payload bytes per emitted segment.
        gso_size: u16,
        /// Header bytes the device replicates per segment (Ethernet + IP +
        /// TCP header, including options).
        hdr_len: u16,
        /// Whether the segment is IPv6 (`TCPV6`) rather than IPv4
        /// (`TCPV4`).
        ipv6: bool,
    },
}

/// One Ethernet frame the engine emits, with the transmit offload it
/// requests of the egress device.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TxFrame {
    /// The offload the device should perform on the frame.
    pub offload: TxOffload,
    /// The Ethernet frame bytes (header, IP packet, transport payload).
    pub bytes: Vec<u8>,
}

impl TxFrame {
    /// A frame with no transmit offload (a complete software checksum).
    #[must_use]
    pub fn plain(bytes: Vec<u8>) -> Self {
        Self {
            offload: TxOffload::None,
            bytes,
        }
    }
}

/// Frames to transmit and events to report from one engine call.
///
/// The caller owns one [`StackOutput`] and **reuses** it across every
/// engine call: each entry point drains the previous call's frames and
/// events back into the engine's buffer pool before it fills the output
/// again, so the steady-state receive and transmit paths perform **zero**
/// heap allocations (the byte buffers are recycled, not freed and
/// reallocated). This is the allocation-free hot path the network stack's
/// performance budget depends on; the invariant is proven by the
/// `hotpath_allocations` regression test. The contract is simply that the
/// caller consumes `frames`/`events` before the next engine call, which
/// the netstack service loop does by construction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StackOutput {
    /// Ethernet frames to hand to the driver, in order.
    pub frames: Vec<TxFrame>,
    /// Typed facts for the caller.
    pub events: Vec<StackEvent>,
}

impl StackOutput {
    /// Reclaim the previous call's frame and event byte buffers into
    /// `pool` and clear the output, ready to be filled again. Called by
    /// every engine entry point before it emits, so a reused output never
    /// leaks or reallocates its buffers.
    fn recycle_into(&mut self, pool: &mut BufPool) {
        for frame in self.frames.drain(..) {
            pool.give(frame.bytes);
        }
        for event in self.events.drain(..) {
            event.recycle_into(pool);
        }
    }
}

/// A bounded free-list of byte buffers the engine reuses so the hot path
/// allocates nothing in steady state.
///
/// A buffer taken for a transmitted frame or a delivered payload is returned to
/// the pool when the caller's next engine call recycles the output
/// ([`StackOutput::recycle_into`]); a transient buffer (an upper message copied
/// into an IP packet, an IP packet copied into a frame) is returned explicitly
/// the moment its consumer has copied it. The pool is capped so a hostile
/// traffic pattern cannot make it grow without bound (a growable capacity, not
/// an unbounded one); beyond the cap a returned buffer is simply dropped.
#[derive(Debug, Default)]
struct BufPool {
    free: Vec<Vec<u8>>,
}

impl BufPool {
    /// Largest number of recycled buffers held at once. One engine call
    /// emits a small, bounded number of frames plus at most
    /// [`MAX_PENDING_PACKETS`] parked packets, so this comfortably covers
    /// the working set while bounding a hostile pattern's residency.
    const CAP: usize = 512;

    /// A cleared, zero-length buffer — recycled if one is available, else
    /// freshly allocated (the only place the engine allocates a frame or
    /// payload buffer, and only when the pool is cold or momentarily
    /// drained).
    fn take(&mut self) -> Vec<u8> {
        self.free.pop().unwrap_or_default()
    }

    /// A recycled buffer resized to `len` zero bytes.
    fn take_zeroed(&mut self, len: usize) -> Vec<u8> {
        let mut buf = self.take();
        buf.clear();
        buf.resize(len, 0);
        buf
    }

    /// Return `buf` to the pool for reuse (cleared, capacity retained),
    /// unless it never held anything or the pool is at its cap.
    fn give(&mut self, mut buf: Vec<u8>) {
        if buf.capacity() == 0 || self.free.len() >= Self::CAP {
            return;
        }
        buf.clear();
        self.free.push(buf);
    }
}

/// Monotonic counters for observability (`stats:net`, plan §5).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct StackCounters {
    /// Frames handed to [`Stack::on_frame`].
    pub rx_frames: u64,
    /// Bytes handed to [`Stack::on_frame`] (every received frame's whole
    /// Ethernet length, counted before validation — the honest total the
    /// device delivered, dropped frames included).
    pub rx_bytes: u64,
    /// Received frames dropped by validation or lack of a handler. The
    /// engine fails closed identically on a malformed frame and on one it
    /// does not accept, so this single bucket is the honest receive-drop
    /// count: there is deliberately no separate "errors" counter that
    /// would split a distinction the receive path does not draw.
    pub rx_dropped: u64,
    /// Frames emitted for transmission.
    pub tx_frames: u64,
    /// Bytes emitted for transmission (every emitted frame's whole
    /// Ethernet length).
    pub tx_bytes: u64,
    /// ICMP/`ICMPv6` errors emitted.
    pub icmp_errors_sent: u64,
    /// ICMP/`ICMPv6` errors suppressed by the rate limiter.
    pub icmp_errors_suppressed: u64,
    /// Reassemblies expired incomplete.
    pub reassembly_expired: u64,
    /// Packets dropped from the pending-resolution queue.
    pub pending_dropped: u64,
}

/// Typed refusal of a transmit request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SendError {
    /// No route covers the destination.
    NoRoute,
    /// No usable source address for the destination.
    NoSourceAddress,
    /// The destination is not a unicast address.
    NotUnicast,
    /// The packet exceeds the path/link MTU and may not be fragmented.
    TooLarge,
    /// The pending-resolution queue or neighbour table is full.
    ResolutionBusy,
    /// The device link is down.
    LinkDown,
}

/// Typed refusal of engine construction.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StackError {
    /// The device facts failed [`DeviceFacts::validate`].
    BadDeviceFacts,
}

/// Typed refusal of a multicast join/leave request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum McastError {
    /// The address is not a multicast group address.
    NotMulticast,
    /// The bounded membership table is full (fail closed).
    CapacityExhausted,
}

/// Configuration of one [`Stack`].
#[derive(Clone, Copy, Debug)]
pub struct StackConfig {
    /// The device report the engine drives (validated whole at
    /// construction).
    pub facts: DeviceFacts,
    /// Interface address-engine configuration.
    pub iface: IfaceConfig,
    /// Neighbour-cache timing.
    pub neighbor: NeighborConfig,
    /// Neighbour-cache capacity (entries).
    pub neighbor_capacity: usize,
    /// Fragment-reassembly budgets.
    pub reassembly: ReassemblyConfig,
    /// Default-router list capacity (RFC 4861 requires ≥ 2).
    pub router_capacity: usize,
    /// Path-MTU cache capacity (destinations).
    pub pmtu_capacity: usize,
    /// Path-MTU entry lifetime (RFC 8201 §5.3 aging).
    pub pmtu_lifetime: Duration64,
    /// ICMP error token-bucket burst (RFC 4443 §2.4(f)).
    pub error_burst: u32,
    /// ICMP error tokens replenished per second.
    pub error_rate: u32,
    /// First IPv4 identification value; drawn from the platform
    /// CSPRNG by the service so the sequence start is unpredictable
    /// (RFC 6864 §5.1), then incremented per datagram.
    pub ipv4_ident_seed: u16,
    /// Per-interface multicast-group membership bound (per family).
    pub multicast_capacity: usize,
    /// Whether IPv4 is administratively enabled (`net.ipv4.enabled`).
    /// When `false` the interface accepts no IPv4 assignment and
    /// answers no IPv4/ARP — it binds no address and answers nothing.
    pub ipv4_enabled: bool,
}

impl StackConfig {
    /// A configuration with production defaults for `facts`, an
    /// interface identifier, and an identification seed (both drawn
    /// by the caller — the RFC 7217 identifier and CSPRNG seed are
    /// the service layer's job).
    #[must_use]
    pub fn new(facts: DeviceFacts, interface_id: [u8; 8], ipv4_ident_seed: u16) -> Self {
        Self {
            facts,
            iface: IfaceConfig::new(interface_id),
            neighbor: NeighborConfig::default(),
            neighbor_capacity: 64,
            reassembly: ReassemblyConfig::default(),
            router_capacity: 4,
            pmtu_capacity: 32,
            pmtu_lifetime: Duration64::from_secs(600),
            error_burst: 10,
            error_rate: 10,
            ipv4_ident_seed,
            multicast_capacity: MULTICAST_CAPACITY,
            ipv4_enabled: true,
        }
    }
}

/// A packet parked while its next hop resolves (stored without its
/// Ethernet header; the header is prepended once the MAC is known).
#[derive(Clone, Debug)]
struct PendingPacket {
    next_hop: IpAddr,
    ethertype: u16,
    packet: Vec<u8>,
    /// The transmit offload to attach once the frame is emitted (the
    /// checksum-offset fields are relative to the Ethernet frame, so
    /// they are unaffected by prepending the header here).
    offload: TxOffload,
}

/// One interface's DHCPv4 client and the CSPRNG source that feeds it the
/// unpredictable transaction id and backoff jitter RFC 2131 requires.
///
/// Present only while the interface is configured for DHCPv4 (the
/// `<iface>.ipv4.method = dhcp` key). The randomness lives at the service
/// seam — the engine stays pure — exactly as the RFC 8981 temporary-address
/// source does: the service injects a CSPRNG-backed closure through
/// [`Stack::enable_dhcp`], and the engine only ever *calls* it.
struct DhcpDriver {
    client: DhcpClient,
    rng: Box<dyn FnMut() -> u32>,
}

/// One interface's DHCPv6 client (RFC 8415) and its CSPRNG source.
///
/// The IPv6 sibling of [`DhcpDriver`]: present only while the interface is
/// configured for stateful DHCPv6 (the `<iface>.ipv6.method = dhcp` key).
/// The randomness lives at the service seam — the engine stays pure — so
/// the service injects a CSPRNG-backed closure through
/// [`Stack::enable_dhcp6`] and the engine only ever *calls* it (for the
/// 24-bit transaction id and the RFC 8415 §15 retransmission jitter).
struct Dhcp6Driver {
    client: Dhcp6Client,
    rng: Box<dyn FnMut() -> u32>,
}

/// The dual-stack host engine. See the module docs.
///
/// The engine legitimately carries several independent boolean condition
/// flags (link up, and one per negotiated offload); grouping them into a
/// sub-struct purely to satisfy the heuristic would obscure, not clarify.
#[allow(clippy::struct_excessive_bools)]
pub struct Stack {
    mac: MacAddress,
    /// Link MTU (largest IP packet) from the device facts.
    link_mtu: usize,
    /// Effective IPv6 MTU: the link MTU, lowered by a router's RA MTU
    /// option (never below the RFC 8200 §5 floor of 1280).
    mtu_v6: usize,
    link_up: bool,
    /// Hop limit for emitted IPv6 datagrams; adopted from a router's
    /// non-zero `Cur Hop Limit` (RFC 4861 §6.3.4).
    hop_limit: u8,
    iface: Iface,
    neighbors: NeighborTable,
    routes_v4: RoutingTable<Ipv4Addr, RouteKind>,
    routes_v6: RoutingTable<Ipv6Addr, RouteKind>,
    routers: DefaultRouterList,
    pmtu: PathMtuCache,
    reassembler: Reassembler,
    error_limiter: ErrorRateLimiter,
    pending: Vec<PendingPacket>,
    ra_routes: usize,
    redirect_routes: usize,
    next_ident: u16,
    /// IPv6 datagram identification for source fragmentation (RFC 8200
    /// §4.5): a 32-bit counter, distinct from the 16-bit IPv4 one, tying
    /// every fragment of one datagram together.
    next_ipv6_ident: u32,
    /// IPv4 (IGMPv2) group membership.
    membership_v4: Membership<Igmp>,
    /// IPv6 (MLDv2) group membership.
    membership_v6: Membership<Mld>,
    /// Whether the device advertised receive-checksum validation and
    /// the stack opted in: only then may a frame the driver marks
    /// checksum-validated skip the software fold.
    rx_csum_offload: bool,
    /// Whether IPv4 is administratively enabled by policy
    /// (`net.ipv4.enabled`). When `false` no IPv4 address may be
    /// assigned and every inbound ARP/IPv4 frame is dropped.
    ipv4_enabled: bool,
    /// Whether the device advertised transmit TCP-checksum offload and
    /// the stack opted in: only then does the engine emit a TCP segment
    /// with a partial checksum for the device to complete.
    tx_csum_tcp: bool,
    /// Whether the device advertised TCP segmentation offload and the
    /// stack opted in: only then may a connection hand the engine an
    /// over-size super-segment for the device to split.
    tx_segment_tcp: bool,
    counters: StackCounters,
    /// The interface's DHCPv4 client, present only while the interface is
    /// configured for DHCPv4 (`<iface>.ipv4.method = dhcp`). Driven from
    /// [`Stack::advance`], fed replies intercepted in [`Stack::on_ipv4`],
    /// and folded into [`Stack::next_deadline`].
    dhcp: Option<DhcpDriver>,
    /// The interface's DHCPv6 client, present only while the interface is
    /// configured for stateful DHCPv6 (`<iface>.ipv6.method = dhcp`).
    /// Driven from [`Stack::advance`], fed replies intercepted in
    /// [`Stack::on_ipv6`], and folded into [`Stack::next_deadline`].
    dhcp6: Option<Dhcp6Driver>,
    /// Recycled byte buffers backing the allocation-free hot path.
    bufs: BufPool,
}

/// Largest TCP super-segment payload the engine will hand the device for
/// segmentation offload: the transmit ring slot capacity
/// ([`RingGeometry::MAX_SLOT_CAPACITY`](tairix_abi::driver::net_ring::RingGeometry::MAX_SLOT_CAPACITY))
/// minus the Ethernet header, the largest IP header (IPv6, 40 bytes), and
/// the largest TCP header (60 bytes). This bounds the single IP packet the
/// super-segment forms to the 16-bit IP length field for **either** family
/// (the TCB is family-agnostic), so a full-option v6 super-segment still
/// fits one slot and one valid IP packet.
const TSO_MAX_PAYLOAD: usize = {
    let cap = tairix_abi::driver::net_ring::RingGeometry::MAX_SLOT_CAPACITY as usize;
    cap - ETHERNET_HEADER_LEN - IPV6_HEADER_LEN - MAX_HEADER_LEN
};

impl Stack {
    /// Build the engine over a validated device report and begin
    /// interface bring-up at `now` (link-local DAD, then router
    /// solicitation — the frames flow from [`Stack::advance`]).
    ///
    /// # Errors
    ///
    /// [`StackError::BadDeviceFacts`] when `config.facts` fails
    /// validation — a stack is never built over a report it cannot
    /// trust the shape of.
    ///
    /// `temp_source` is the injected CSPRNG seam RFC 8981 temporary
    /// (privacy) addresses draw from; it is consulted only while the
    /// `net.ipv6.privacy` policy is enabled.
    pub fn new(
        config: &StackConfig,
        temp_source: Box<dyn TempAddrSource>,
        now: Duration64,
    ) -> Result<Self, StackError> {
        if config.facts.validate().is_err() {
            return Err(StackError::BadDeviceFacts);
        }
        Ok(Self {
            mac: config.facts.mac,
            link_mtu: config.facts.mtu as usize,
            mtu_v6: config.facts.mtu as usize,
            link_up: config.facts.link == LinkState::Up,
            hop_limit: crate::ipv6::DEFAULT_HOP_LIMIT,
            iface: Iface::new(&config.iface, temp_source, now),
            ipv4_enabled: config.ipv4_enabled,
            neighbors: NeighborTable::new(config.neighbor_capacity, config.neighbor),
            routes_v4: RoutingTable::new(),
            routes_v6: RoutingTable::new(),
            routers: DefaultRouterList::new(config.router_capacity),
            pmtu: PathMtuCache::new(config.pmtu_capacity, config.pmtu_lifetime),
            reassembler: Reassembler::new(config.reassembly),
            error_limiter: ErrorRateLimiter::new(config.error_burst, config.error_rate),
            bufs: BufPool::default(),
            pending: Vec::new(),
            ra_routes: 0,
            redirect_routes: 0,
            next_ident: config.ipv4_ident_seed,
            next_ipv6_ident: u32::from(config.ipv4_ident_seed),
            membership_v4: Membership::new(config.multicast_capacity, mac_seed(config.facts.mac)),
            membership_v6: Membership::new(config.multicast_capacity, mac_seed(config.facts.mac)),
            rx_csum_offload: config
                .facts
                .offloads
                .contains(NetOffloads::RX_CSUM_VALIDATED),
            tx_csum_tcp: config.facts.offloads.contains(NetOffloads::TX_CSUM_TCP),
            tx_segment_tcp: config.facts.offloads.contains(NetOffloads::TX_SEGMENT_TCP),
            counters: StackCounters::default(),
            dhcp: None,
            dhcp6: None,
        })
    }

    /// Record a link-state change reported by the driver.
    pub fn set_link(&mut self, link: LinkState) {
        self.link_up = link == LinkState::Up;
    }

    /// This host's link-layer address.
    #[must_use]
    pub fn mac(&self) -> MacAddress {
        self.mac
    }

    /// Monotonic counters for observability.
    #[must_use]
    pub fn counters(&self) -> StackCounters {
        self.counters
    }

    /// The interface address engine, read-only (address views).
    #[must_use]
    pub fn iface(&self) -> &Iface {
        &self.iface
    }

    /// The recursive DNS servers this interface's DHCP client(s) learned
    /// from their current lease(s), in wire order: the IPv4 lease's servers
    /// (RFC 2132 option 6) first, then the IPv6 lease's (RFC 3646 option
    /// 23).
    ///
    /// This is the pure `lib/net` source the `netstack` service aggregates
    /// with any statically configured servers into an interface's active
    /// resolver set (`plans/DNS.md` DNS2). The set is derived from each
    /// client's *current* lease, so it tracks acquisition and withdrawal
    /// exactly: it is empty when neither family holds a lease (INIT, or a
    /// lost/expired lease returns the client to INIT), and a lease that
    /// carried no servers contributes none. The result is bounded by the
    /// two leases' fixed-capacity option lists, so a hostile server can
    /// never size the allocation.
    #[must_use]
    pub fn dhcp_dns_servers(&self) -> Vec<IpAddr> {
        let mut servers = Vec::new();
        if let Some(lease) = self.dhcp.as_ref().and_then(|d| d.client.lease()) {
            servers.extend(lease.dns_servers.as_slice().iter().copied().map(IpAddr::V4));
        }
        if let Some(lease) = self.dhcp6.as_ref().and_then(|d| d.client.lease()) {
            servers.extend(lease.dns_servers.as_slice().iter().copied().map(IpAddr::V6));
        }
        servers
    }

    /// Configure the static IPv4 assignment: address, subnet, and
    /// optional default gateway. Replaces any previous v4
    /// configuration (connected and gateway routes included).
    ///
    /// # Errors
    ///
    /// Propagates [`crate::iface::AddrError`] refusals; a gateway
    /// outside the connected subnet is refused as
    /// [`crate::iface::AddrError::NotUnicast`] (it could never be
    /// resolved on-link — fail closed rather than install a dead
    /// route).
    pub fn set_ipv4_config(
        &mut self,
        addr: Ipv4Addr,
        prefix_len: u8,
        gateway: Option<Ipv4Addr>,
    ) -> Result<(), crate::iface::AddrError> {
        if !self.ipv4_enabled {
            return Err(crate::iface::AddrError::V4Disabled);
        }
        let connected = Prefix::new(mask_v4(addr, prefix_len), prefix_len)
            .ok_or(crate::iface::AddrError::BadPrefixLen)?;
        if let Some(gw) = gateway {
            if !connected.contains(gw) {
                return Err(crate::iface::AddrError::NotUnicast);
            }
        }
        self.iface.set_ipv4(addr, prefix_len)?;
        self.routes_v4 = RoutingTable::new();
        self.routes_v4.insert(connected, None, RouteKind::Connected);
        // Every multicast-capable host is a member of the all-systems
        // group (RFC 1112): joined for reception, never reported. The
        // time is irrelevant for a non-reported group.
        let _ = self.membership_v4.join(ALL_SYSTEMS_V4, Duration64::ZERO);
        if let Some(gw) = gateway {
            let default = Prefix::new(Ipv4Addr::UNSPECIFIED, 0)
                .ok_or(crate::iface::AddrError::BadPrefixLen)?;
            self.routes_v4.insert(default, Some(gw), RouteKind::Static);
        }
        Ok(())
    }

    /// Assign a static IPv6 address (DAD starts at `now`) and its
    /// connected on-link route.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::iface::AddrError`] refusals.
    pub fn add_ipv6_static(
        &mut self,
        addr: Ipv6Addr,
        prefix_len: u8,
        now: Duration64,
    ) -> Result<(), crate::iface::AddrError> {
        let connected = Prefix::new(mask_v6(addr, prefix_len), prefix_len)
            .ok_or(crate::iface::AddrError::BadPrefixLen)?;
        self.iface.add_ipv6_static(addr, prefix_len, now)?;
        self.routes_v6.insert(connected, None, RouteKind::Static);
        Ok(())
    }

    /// Install a static IPv6 route (an administrative operation; the
    /// admin surface gates it).
    ///
    /// # Errors
    ///
    /// [`SendError::NoRoute`] for an invalid prefix.
    pub fn add_route_v6(
        &mut self,
        prefix: Ipv6Addr,
        prefix_len: u8,
        next_hop: Option<Ipv6Addr>,
    ) -> Result<(), SendError> {
        let prefix = Prefix::new(prefix, prefix_len).ok_or(SendError::NoRoute)?;
        self.routes_v6.insert(prefix, next_hop, RouteKind::Static);
        Ok(())
    }

    /// Install a static IPv4 route (an administrative operation; the
    /// admin surface gates it).
    ///
    /// # Errors
    ///
    /// [`SendError::NoRoute`] for an invalid prefix.
    pub fn add_route_v4(
        &mut self,
        prefix: Ipv4Addr,
        prefix_len: u8,
        next_hop: Option<Ipv4Addr>,
    ) -> Result<(), SendError> {
        let prefix = Prefix::new(prefix, prefix_len).ok_or(SendError::NoRoute)?;
        self.routes_v4.insert(prefix, next_hop, RouteKind::Static);
        Ok(())
    }

    /// Override the interface's link MTU (the `<iface>.mtu` administrative
    /// key in `network.conf`).
    ///
    /// Replaces the MTU the device reported at bring-up with the operator's
    /// configured value, which governs both IPv4/IPv6 fragmentation and TCP
    /// MSS selection. `mtu` is a valid link MTU: the configuration engine
    /// bounds it at `[1280, 65535]`, so it never falls below the IPv6
    /// minimum and the IPv6 link MTU stays valid. Any learned path MTU is
    /// retained and re-clamped against the new link MTU on the next send,
    /// so lowering the MTU never leaves a stale-too-large path estimate in
    /// use (RFC 8201 §4 — the cache is a floor, the link MTU the ceiling).
    pub fn set_mtu(&mut self, mtu: u16) {
        self.link_mtu = usize::from(mtu);
        self.mtu_v6 = usize::from(mtu);
    }

    /// Whether IPv4 is administratively enabled (`net.ipv4.enabled`).
    #[must_use]
    pub fn ipv4_enabled(&self) -> bool {
        self.ipv4_enabled
    }

    /// Whether IPv6 is administratively enabled (`net.ipv6.enabled`) —
    /// neither policy-disabled nor DAD-disabled.
    #[must_use]
    pub fn ipv6_enabled(&self) -> bool {
        !self.iface.v6_admin_disabled() && !self.iface.v6_disabled()
    }

    /// Administratively enable or disable IPv4 (`net.ipv4.enabled`).
    ///
    /// Disabling drops the static IPv4 assignment and every IPv4 route,
    /// so the interface binds no IPv4 address and answers no IPv4/ARP;
    /// re-enabling permits [`Self::set_ipv4_config`] again. Idempotent;
    /// re-enabling does not restore a previously assigned address (there
    /// is no IPv4 auto-configuration — the admin re-assigns it).
    pub fn set_ipv4_enabled(&mut self, enabled: bool) {
        if self.ipv4_enabled == enabled {
            return;
        }
        self.ipv4_enabled = enabled;
        if !enabled {
            self.iface.clear_ipv4();
            self.routes_v4 = RoutingTable::new();
        }
    }

    /// Administratively enable or disable IPv6 (`net.ipv6.enabled`).
    ///
    /// Delegates the address lifecycle to the interface engine (flush
    /// on disable, re-form the link-local on enable) and additionally
    /// clears the IPv6 routing table and default-router list on disable,
    /// so no stale route can outlive the family. Idempotent.
    pub fn set_ipv6_enabled(&mut self, enabled: bool, now: Duration64) {
        self.iface.set_ipv6_enabled(enabled, now);
        if !enabled {
            self.routes_v6 = RoutingTable::new();
            self.routers.clear();
            self.ra_routes = 0;
            self.redirect_routes = 0;
        }
    }

    /// Enable or disable RFC 8981 temporary (privacy) IPv6 addresses
    /// (`net.ipv6.privacy`). Delegates to the interface engine: enabling
    /// forms a temporary address for every autonomous prefix, disabling
    /// removes them and leaves the stable SLAAC addresses in place. The
    /// resulting DAD/lifecycle frames flow from [`Stack::advance`].
    pub fn set_privacy(&mut self, enabled: bool, now: Duration64) {
        self.iface.set_privacy(enabled, now);
    }

    /// Start the RFC 2131 DHCPv4 client on this interface
    /// (`<iface>.ipv4.method = dhcp`).
    ///
    /// DHCPv4 is an IPv4 addressing method, so this enables the IPv4 family
    /// and starts from a clean slate — any prior static address and its
    /// routes are dropped. The client begins in INIT and broadcasts its
    /// first DISCOVER on the next [`Stack::advance`]; the leased address,
    /// mask, and default route are applied when the exchange completes.
    ///
    /// `rng` is the injected CSPRNG source the client draws its transaction
    /// id and backoff jitter from (the service owns entropy; the engine
    /// stays pure). Re-enabling while already running restarts acquisition,
    /// so the service only calls this when the client is not already active.
    pub fn enable_dhcp(&mut self, rng: Box<dyn FnMut() -> u32>) {
        self.set_ipv4_enabled(true);
        self.iface.clear_ipv4();
        self.routes_v4 = RoutingTable::new();
        self.dhcp = Some(DhcpDriver {
            client: DhcpClient::new(self.mac),
            rng,
        });
    }

    /// Stop the DHCPv4 client and withdraw any lease it applied (the
    /// address and its routes), leaving the IPv4 family enabled but
    /// unaddressed. Idempotent: a no-op when no client is running.
    pub fn disable_dhcp(&mut self) {
        if self.dhcp.take().is_some() {
            self.iface.clear_ipv4();
            self.routes_v4 = RoutingTable::new();
        }
    }

    /// Whether a DHCPv4 client is currently running on this interface.
    #[must_use]
    pub fn dhcp_active(&self) -> bool {
        self.dhcp.is_some()
    }

    /// Start the RFC 8415 stateful DHCPv6 client on this interface
    /// (`<iface>.ipv6.method = dhcp`).
    ///
    /// DHCPv6 rides on the interface's link-local address, so this enables
    /// the IPv6 family (forming the link-local the client sources its
    /// messages from) and starts the client from a clean slate — any prior
    /// DHCPv6-leased address is dropped. The client begins in INIT and
    /// multicasts its first Solicit on the next [`Stack::advance`] once a
    /// usable link-local source exists; the leased address is applied when
    /// the Solicit/Advertise/Request/Reply exchange completes.
    ///
    /// The client's IA identifier is derived from the interface MAC so it
    /// is stable across restarts (RFC 8415 §12.1 — a persistent per-IA
    /// identity). `rng` is the injected CSPRNG source the client draws its
    /// transaction id and retransmission jitter from (the service owns
    /// entropy; the engine stays pure). Re-enabling while already running
    /// restarts acquisition, so the service only calls this when the client
    /// is not already active.
    pub fn enable_dhcp6(&mut self, rng: Box<dyn FnMut() -> u32>, now: Duration64) {
        self.set_ipv6_enabled(true, now);
        self.iface.clear_ipv6_dhcp();
        self.dhcp6 = Some(Dhcp6Driver {
            client: Dhcp6Client::new(self.mac, dhcp6_iaid(self.mac)),
            rng,
        });
    }

    /// Stop the DHCPv6 client and withdraw any lease it applied (the leased
    /// address), leaving the IPv6 family enabled with its link-local and
    /// any SLAAC/static addresses intact. Idempotent: a no-op when no
    /// client is running.
    pub fn disable_dhcp6(&mut self) {
        if self.dhcp6.take().is_some() {
            self.iface.clear_ipv6_dhcp();
        }
    }

    /// Whether a DHCPv6 client is currently running on this interface.
    #[must_use]
    pub fn dhcp6_active(&self) -> bool {
        self.dhcp6.is_some()
    }
}

impl Stack {
    /// Process one received Ethernet frame carrying no device offload
    /// metadata (the canonical software path).
    pub fn on_frame(&mut self, frame_bytes: &[u8], now: Duration64, out: &mut StackOutput) {
        self.on_frame_meta(frame_bytes, RxMeta::none(), now, out);
    }

    /// Process one received Ethernet frame together with the per-frame
    /// offload metadata the driver reported ([`RxMeta`]).
    ///
    /// A transport checksum is verified in software unless the device
    /// validated it *and* the interface negotiated the receive-checksum
    /// offload; every other validation runs regardless
    /// (`plans/NETWORK.md` §2.3).
    pub fn on_frame_meta(
        &mut self,
        frame_bytes: &[u8],
        rx: RxMeta,
        now: Duration64,
        out: &mut StackOutput,
    ) {
        out.recycle_into(&mut self.bufs);
        self.counters.rx_frames += 1;
        self.counters.rx_bytes += frame_bytes.len() as u64;
        let Some(frame) = EthernetFrame::parse(frame_bytes) else {
            self.counters.rx_dropped += 1;
            return;
        };
        if frame.destination != self.mac && !is_group_mac(frame.destination) {
            self.counters.rx_dropped += 1;
            return;
        }
        // The device's checksum assurance is honoured only when the stack
        // opted into the offload; a per-frame claim is otherwise ignored.
        let check = if rx.checksum_validated && self.rx_csum_offload {
            ChecksumCheck::DeviceValidated
        } else {
            ChecksumCheck::Verify
        };
        match frame.ethertype {
            // A disabled family answers nothing: drop its frames before
            // any parsing so no address forms (an inbound RA cannot
            // SLAAC-configure a policy-disabled interface) and nothing
            // is answered. IPv4 without an assignment is already silent,
            // but the explicit gate keeps the two families symmetric.
            ETHERTYPE_ARP | ETHERTYPE_IPV4 if !self.ipv4_enabled => {
                self.counters.rx_dropped += 1;
            }
            ETHERTYPE_IPV6 if self.iface.v6_admin_disabled() => {
                self.counters.rx_dropped += 1;
            }
            ETHERTYPE_ARP => self.on_arp(out, frame.payload, now),
            ETHERTYPE_IPV4 => self.on_ipv4(out, frame.payload, check, now),
            ETHERTYPE_IPV6 => self.on_ipv6(out, frame.payload, check, now),
            _ => self.counters.rx_dropped += 1,
        }
    }

    /// ARP (RFC 826): answer requests for our address; solicited
    /// replies confirm the neighbour cache and release parked frames.
    fn on_arp(&mut self, out: &mut StackOutput, payload: &[u8], now: Duration64) {
        let (Some(arp), Some((our_v4, _))) = (ArpPacket::parse(payload), self.iface.ipv4()) else {
            self.counters.rx_dropped += 1;
            return;
        };
        match arp.operation {
            OP_REQUEST if arp.target_protocol == our_v4 => {
                self.neighbors
                    .learn(IpAddr::V4(arp.sender_protocol), arp.sender_hardware, now);
                let mut buf = [0u8; crate::arp::ARP_PACKET_LEN];
                if arp.reply_from(self.mac).write(&mut buf).is_some() {
                    self.push_frame(out, arp.sender_hardware, ETHERTYPE_ARP, &buf);
                }
                self.drain_pending(out, IpAddr::V4(arp.sender_protocol), now);
            }
            OP_REPLY if arp.target_hardware == self.mac && arp.target_protocol == our_v4 => {
                self.neighbors.confirm(
                    IpAddr::V4(arp.sender_protocol),
                    arp.sender_hardware,
                    true,
                    true,
                    now,
                );
                self.drain_pending(out, IpAddr::V4(arp.sender_protocol), now);
            }
            _ => self.counters.rx_dropped += 1,
        }
    }

    /// IPv4 receive: our unicast address only (hosts do not
    /// forward), fragments through the budgeted reassembler.
    fn on_ipv4(
        &mut self,
        out: &mut StackOutput,
        packet: &[u8],
        check: ChecksumCheck,
        now: Duration64,
    ) {
        let Some((header, _options, payload)) = Ipv4Header::parse(packet) else {
            self.counters.rx_dropped += 1;
            return;
        };
        // A DHCP reply reaches the client before any address is configured
        // — it is broadcast to 255.255.255.255 (or unicast to the leased
        // address during renewal). Intercept it here, ahead of the
        // unicast-address filter that would otherwise drop an
        // address-less receive, whenever a DHCP client is running.
        if self.dhcp.is_some()
            && !header.is_fragment()
            && header.protocol == PROTOCOL_UDP
            && self.consume_dhcp_reply(out, &header, payload, check, now)
        {
            return;
        }
        let Some((our_v4, _)) = self.iface.ipv4() else {
            self.counters.rx_dropped += 1;
            return;
        };
        if header.destination != our_v4 && !self.accepts_v4_multicast(header.destination) {
            self.counters.rx_dropped += 1;
            return;
        }
        if header.is_fragment() {
            let key = FragKey {
                source: IpAddr::V4(header.source),
                destination: IpAddr::V4(header.destination),
                identification: u32::from(header.identification),
                protocol: header.protocol,
            };
            match self.reassembler.push(
                key,
                usize::from(header.fragment_offset),
                header.more_fragments,
                payload,
                now,
            ) {
                PushOutcome::Complete(datagram) => {
                    // The reconstructed datagram exists only here; an
                    // ICMP error about it could not carry the original
                    // packet excerpt, so unknown protocols inside a
                    // reassembly are dropped silently below. A device's
                    // per-frame checksum assurance cannot cover a transport
                    // checksum that spans fragments, so a reassembled
                    // datagram is always software-verified.
                    self.on_ipv4_payload(out, &header, &datagram, None, ChecksumCheck::Verify, now);
                }
                PushOutcome::Pending => {}
                PushOutcome::Rejected(_) => self.counters.rx_dropped += 1,
            }
            return;
        }
        self.on_ipv4_payload(out, &header, payload, Some(packet), check, now);
    }

    /// Dispatch a whole IPv4 payload. `original` carries the intact
    /// packet bytes when available (unfragmented), for ICMP error
    /// excerpts.
    fn on_ipv4_payload(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv4Header,
        payload: &[u8],
        original: Option<&[u8]>,
        check: ChecksumCheck,
        now: Duration64,
    ) {
        if header.protocol == PROTOCOL_ICMP {
            self.on_icmp_v4(out, header, payload, now);
            return;
        }
        if header.protocol == PROTOCOL_IGMP {
            self.on_igmp(payload, now);
            return;
        }
        if header.protocol == PROTOCOL_UDP {
            let pseudo = udp::Pseudo::V4 {
                source: header.source,
                destination: header.destination,
            };
            let Some(datagram) = UdpDatagram::parse_with(pseudo, payload, check) else {
                self.counters.rx_dropped += 1;
                return;
            };
            out.events.push(StackEvent::UdpDatagram {
                source: IpAddr::V4(header.source),
                destination: IpAddr::V4(header.destination),
                source_port: datagram.source_port,
                destination_port: datagram.destination_port,
                payload: self.pooled_copy(datagram.payload),
            });
            return;
        }
        if header.protocol == PROTOCOL_TCP {
            let pseudo = Pseudo::V4 {
                source: header.source,
                destination: header.destination,
            };
            // Verify the mandatory checksum here so a flood of corrupt
            // segments never reaches the connection layer; surface the
            // raw bytes for the service to re-parse against its TCB. When
            // the device validated the checksum and the offload is
            // negotiated, the fold is skipped but every other check runs.
            if tcp::TcpSegment::parse_with(pseudo, payload, check).is_none() {
                self.counters.rx_dropped += 1;
                return;
            }
            out.events.push(StackEvent::TcpSegment {
                source: IpAddr::V4(header.source),
                destination: IpAddr::V4(header.destination),
                ecn: header.ecn,
                segment: payload.to_vec(),
            });
            return;
        }
        // Unknown transport: Destination Unreachable, protocol
        // (RFC 1122 §3.2.2.1), gated and rate-limited.
        let Some(original) = original else {
            self.counters.rx_dropped += 1;
            return;
        };
        self.counters.rx_dropped += 1;
        let context = ErrorContext {
            invoking_is_icmp_error: false,
            dest_is_multicast: false,
            source_is_ambiguous: v4_source_ambiguous(header.source),
            multicast_exception: false,
        };
        self.emit_icmp_error_v4(
            out,
            header.source,
            header.destination,
            IcmpErrorKind::DestinationUnreachable {
                code: V4_CODE_PROTOCOL_UNREACHABLE,
            },
            original,
            context,
            now,
        );
    }

    /// ICMP for IPv4: answer echo requests, surface replies and
    /// errors as events.
    fn on_icmp_v4(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv4Header,
        payload: &[u8],
        now: Duration64,
    ) {
        if let Some(echo) = IcmpEcho::parse(IcmpContext::V4, payload) {
            match echo.kind {
                crate::icmp::EchoKind::Request => {
                    if v4_source_ambiguous(header.source) {
                        self.counters.rx_dropped += 1;
                        return;
                    }
                    let reply = echo.reply();
                    let mut message = vec![0u8; reply.wire_len()];
                    if reply.write(IcmpContext::V4, &mut message).is_none() {
                        self.counters.rx_dropped += 1;
                        return;
                    }
                    self.send_ipv4_packet(
                        out,
                        header.destination,
                        header.source,
                        PROTOCOL_ICMP,
                        &message,
                        now,
                    );
                    out.events.push(StackEvent::EchoRequestServed {
                        source: IpAddr::V4(header.source),
                        identifier: echo.identifier,
                        sequence: echo.sequence,
                    });
                }
                crate::icmp::EchoKind::Reply => out.events.push(StackEvent::EchoReply {
                    source: IpAddr::V4(header.source),
                    identifier: echo.identifier,
                    sequence: echo.sequence,
                    payload: echo.payload.to_vec(),
                }),
            }
            return;
        }
        if let Some(error) = IcmpError::parse(IcmpContext::V4, payload) {
            out.events.push(StackEvent::IcmpErrorReceived {
                source: IpAddr::V4(header.source),
                kind: error.kind,
            });
            return;
        }
        self.counters.rx_dropped += 1;
    }

    /// True when `dest` is a solicited-node group of any of our
    /// addresses (tentative included — DAD listens there).
    fn is_our_solicited_node(&self, dest: Ipv6Addr) -> bool {
        self.iface
            .ipv6_addresses()
            .iter()
            .any(|info| solicited_node_multicast(&info.addr) == dest)
    }

    /// IPv6 receive: destination-filtered, extension chain walked
    /// under the RFC 8200 dispositions, fragments through the
    /// budgeted reassembler.
    fn on_ipv6(
        &mut self,
        out: &mut StackOutput,
        packet: &[u8],
        check: ChecksumCheck,
        now: Duration64,
    ) {
        let Some((header, payload)) = Ipv6Header::parse(packet) else {
            self.counters.rx_dropped += 1;
            return;
        };
        let dest = header.destination;
        let dest_is_multicast = dest.is_multicast();
        // A DHCPv6 server reply (UDP source 547 → destination 546) is
        // delivered to the interface's link-local address. Intercept it
        // here, ahead of the destination filter — mirroring the DHCPv4
        // pre-filter intercept — whenever a DHCPv6 client is running and
        // the packet is a plain (no extension header) UDP datagram, so the
        // client claims it rather than it surfacing as ordinary traffic.
        if self.dhcp6.is_some()
            && header.next_header == PROTOCOL_UDP
            && self.consume_dhcp6_reply(out, &header, payload, check, now)
        {
            return;
        }
        let for_us = self.iface.is_assigned(dest)
            || self.iface.is_tentative(dest)
            || dest == ALL_NODES
            || self.is_our_solicited_node(dest)
            || self.membership_v6.is_member(dest);
        if !for_us {
            self.counters.rx_dropped += 1;
            return;
        }
        match walk(header.next_header, payload, dest_is_multicast) {
            Err(WalkRejection::Drop) => self.counters.rx_dropped += 1,
            Err(WalkRejection::ParamProblem { code, pointer }) => {
                self.counters.rx_dropped += 1;
                // RFC 8200 mandates this report for the option
                // dispositions that reach here, so the invoking
                // packet is deliberately not classified as a possible
                // ICMP error (its upper layer was never reached).
                let context = ErrorContext {
                    invoking_is_icmp_error: false,
                    dest_is_multicast,
                    source_is_ambiguous: header.source.is_unspecified()
                        || header.source.is_multicast(),
                    multicast_exception: code == crate::ipv6::PARAM_PROBLEM_OPTION,
                };
                self.emit_icmp_error_v6(
                    out,
                    &header,
                    IcmpErrorKind::ParameterProblem { code, pointer },
                    packet,
                    context,
                    now,
                );
            }
            Ok(WalkOutcome::Nothing) => {}
            Ok(WalkOutcome::Fragment { info, payload }) => {
                let key = FragKey {
                    source: IpAddr::V6(header.source),
                    destination: IpAddr::V6(dest),
                    identification: info.identification,
                    protocol: 0,
                };
                match self
                    .reassembler
                    .push(key, usize::from(info.offset), info.more, payload, now)
                {
                    PushOutcome::Complete(datagram) => {
                        // One re-walk over the reassembled payload; a
                        // nested fragment header fails closed.
                        match walk(info.next_header, &datagram, dest_is_multicast) {
                            Ok(WalkOutcome::Upper {
                                protocol,
                                payload,
                                nh_offset: _,
                            }) => {
                                // A reassembled datagram's transport
                                // checksum spans fragments, beyond any
                                // per-frame device assurance: verify it.
                                self.on_ipv6_upper(
                                    out,
                                    &header,
                                    protocol,
                                    payload,
                                    None,
                                    dest_is_multicast,
                                    ChecksumCheck::Verify,
                                    now,
                                );
                            }
                            _ => self.counters.rx_dropped += 1,
                        }
                    }
                    PushOutcome::Pending => {}
                    PushOutcome::Rejected(_) => self.counters.rx_dropped += 1,
                }
            }
            Ok(WalkOutcome::Upper {
                protocol,
                payload,
                nh_offset,
            }) => {
                self.on_ipv6_upper(
                    out,
                    &header,
                    protocol,
                    payload,
                    Some((packet, nh_offset)),
                    dest_is_multicast,
                    check,
                    now,
                );
            }
        }
    }

    /// Dispatch an IPv6 upper-layer payload. `original` carries the
    /// intact packet and the next-header field offset when available
    /// (unfragmented), for the unrecognised-protocol report.
    #[allow(clippy::too_many_arguments)]
    fn on_ipv6_upper(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv6Header,
        protocol: u8,
        payload: &[u8],
        original: Option<(&[u8], u32)>,
        dest_is_multicast: bool,
        check: ChecksumCheck,
        now: Duration64,
    ) {
        if protocol == NEXT_HEADER_ICMPV6 {
            self.on_icmpv6(out, header, payload, dest_is_multicast, now);
            return;
        }
        if protocol == PROTOCOL_UDP {
            let pseudo = udp::Pseudo::V6 {
                source: header.source,
                destination: header.destination,
            };
            let Some(datagram) = UdpDatagram::parse_with(pseudo, payload, check) else {
                self.counters.rx_dropped += 1;
                return;
            };
            out.events.push(StackEvent::UdpDatagram {
                source: IpAddr::V6(header.source),
                destination: IpAddr::V6(header.destination),
                source_port: datagram.source_port,
                destination_port: datagram.destination_port,
                payload: self.pooled_copy(datagram.payload),
            });
            return;
        }
        if protocol == PROTOCOL_TCP {
            let pseudo = Pseudo::V6 {
                source: header.source,
                destination: header.destination,
            };
            if tcp::TcpSegment::parse_with(pseudo, payload, check).is_none() {
                self.counters.rx_dropped += 1;
                return;
            }
            out.events.push(StackEvent::TcpSegment {
                source: IpAddr::V6(header.source),
                destination: IpAddr::V6(header.destination),
                ecn: header.ecn(),
                segment: payload.to_vec(),
            });
            return;
        }
        self.counters.rx_dropped += 1;
        // Unrecognised upper protocol: Parameter Problem code 1
        // pointing at the next-header field (RFC 4443 §3.4 / RFC 8200
        // §4). Without the original bytes (a reassembled datagram)
        // the report cannot carry its excerpt and is not sent.
        let Some((packet, nh_offset)) = original else {
            return;
        };
        let context = ErrorContext {
            invoking_is_icmp_error: false,
            dest_is_multicast,
            source_is_ambiguous: header.source.is_unspecified() || header.source.is_multicast(),
            multicast_exception: false,
        };
        self.emit_icmp_error_v6(
            out,
            header,
            IcmpErrorKind::ParameterProblem {
                code: PARAM_PROBLEM_NEXT_HEADER,
                pointer: nh_offset,
            },
            packet,
            context,
            now,
        );
    }

    /// `ICMPv6`: Neighbour Discovery, echo, and errors.
    fn on_icmpv6(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv6Header,
        payload: &[u8],
        dest_is_multicast: bool,
        now: Duration64,
    ) {
        let context = IcmpContext::V6 {
            source: header.source,
            destination: header.destination,
        };
        let Some(message) = IcmpMessage::parse(context, payload) else {
            self.counters.rx_dropped += 1;
            return;
        };
        match message.message_type {
            crate::nd::TYPE_ROUTER_SOLICITATION
            | crate::nd::TYPE_ROUTER_ADVERTISEMENT
            | crate::nd::TYPE_NEIGHBOR_SOLICITATION
            | crate::nd::TYPE_NEIGHBOR_ADVERTISEMENT
            | crate::nd::TYPE_REDIRECT => {
                let Some(nd) = NdMessage::parse(
                    message.message_type,
                    message.code,
                    header.hop_limit,
                    dest_is_multicast,
                    message.body,
                ) else {
                    self.counters.rx_dropped += 1;
                    return;
                };
                self.on_nd(out, header, &nd, now);
            }
            TYPE_MULTICAST_LISTENER_QUERY => self.on_mld_query(message.body, now),
            // MLDv2 has no report suppression, so a host ignores every
            // report/done it hears (a router's concern, not ours).
            mld::TYPE_MLDV1_REPORT | mld::TYPE_MLDV1_DONE | TYPE_MLDV2_REPORT => {}
            crate::icmp::TYPE_V6_ECHO_REQUEST | crate::icmp::TYPE_V6_ECHO_REPLY => {
                let Some(echo) = IcmpEcho::parse(context, payload) else {
                    self.counters.rx_dropped += 1;
                    return;
                };
                match echo.kind {
                    crate::icmp::EchoKind::Request => {
                        // A multicast-addressed echo request is an
                        // amplification vector: refused (deliberate
                        // divergence from RFC 4443's MAY).
                        if dest_is_multicast
                            || !self.iface.is_assigned(header.destination)
                            || header.source.is_unspecified()
                            || header.source.is_multicast()
                        {
                            self.counters.rx_dropped += 1;
                            return;
                        }
                        let reply = echo.reply();
                        let reply_context = IcmpContext::V6 {
                            source: header.destination,
                            destination: header.source,
                        };
                        let mut message = vec![0u8; reply.wire_len()];
                        if reply.write(reply_context, &mut message).is_none() {
                            self.counters.rx_dropped += 1;
                            return;
                        }
                        self.send_ipv6_packet(
                            out,
                            header.destination,
                            header.source,
                            NEXT_HEADER_ICMPV6,
                            &message,
                            self.hop_limit,
                            now,
                        );
                        out.events.push(StackEvent::EchoRequestServed {
                            source: IpAddr::V6(header.source),
                            identifier: echo.identifier,
                            sequence: echo.sequence,
                        });
                    }
                    crate::icmp::EchoKind::Reply => out.events.push(StackEvent::EchoReply {
                        source: IpAddr::V6(header.source),
                        identifier: echo.identifier,
                        sequence: echo.sequence,
                        payload: echo.payload.to_vec(),
                    }),
                }
            }
            _ => {
                let Some(error) = IcmpError::parse(context, payload) else {
                    self.counters.rx_dropped += 1;
                    return;
                };
                if let IcmpErrorKind::PacketTooBig { mtu } = error.kind {
                    // RFC 8201: the invoking packet's destination is
                    // the path the reported MTU describes.
                    if let Some((invoking, _)) = Ipv6Header::parse(error.invoking) {
                        self.pmtu.packet_too_big(
                            invoking.destination,
                            mtu,
                            self.mtu_v6_wire(),
                            now,
                        );
                    }
                }
                out.events.push(StackEvent::IcmpErrorReceived {
                    source: IpAddr::V6(header.source),
                    kind: error.kind,
                });
            }
        }
    }

    /// Validated Neighbour Discovery dispatch (RFC 4861/4862).
    fn on_nd(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv6Header,
        message: &NdMessage,
        now: Duration64,
    ) {
        match message {
            // Hosts ignore Router Solicitations.
            NdMessage::RouterSolicitation { .. } => self.counters.rx_dropped += 1,
            NdMessage::NeighborSolicitation { target, .. } => {
                if header.source.is_unspecified() {
                    // A DAD probe from another node (RFC 4862 §5.4.3).
                    if let Some(IfaceAction::DadFailed { addr }) =
                        self.iface.on_dad_evidence(*target)
                    {
                        out.events.push(StackEvent::DadFailed { addr });
                        return;
                    }
                    if self.iface.is_assigned(*target) {
                        // Defend the address: advertise to all-nodes
                        // (the prober has no unicast address yet).
                        self.send_neighbor_advertisement(out, *target, ALL_NODES, false, now);
                    }
                    return;
                }
                crate::nd::apply_neighbor_solicitation(
                    message,
                    header.source,
                    &mut self.neighbors,
                    now,
                );
                if self.iface.is_assigned(*target) {
                    self.send_neighbor_advertisement(out, *target, header.source, true, now);
                }
                self.drain_pending(out, IpAddr::V6(header.source), now);
            }
            NdMessage::NeighborAdvertisement { target, .. } => {
                if self.iface.is_tentative(*target) {
                    if let Some(IfaceAction::DadFailed { addr }) =
                        self.iface.on_dad_evidence(*target)
                    {
                        out.events.push(StackEvent::DadFailed { addr });
                    }
                    return;
                }
                crate::nd::apply_neighbor_advertisement(message, &mut self.neighbors, now);
                self.drain_pending(out, IpAddr::V6(*target), now);
            }
            NdMessage::RouterAdvertisement { .. } => {
                self.on_router_advertisement(out, header, message, now);
            }
            NdMessage::Redirect {
                target,
                destination,
                ..
            } => {
                // RFC 4861 §8.1: link-local source, and only from the
                // destination's *current* first hop; the target is a
                // router (link-local) or the destination itself
                // (on-link fact).
                if !is_unicast_link_local(&header.source)
                    || self.next_hop_v6(*destination, now) != Some(header.source)
                    || !(is_unicast_link_local(target) || target == destination)
                {
                    self.counters.rx_dropped += 1;
                    return;
                }
                if self.redirect_routes < MAX_REDIRECT_ROUTES {
                    if let Some(prefix) = Prefix::new(*destination, 128) {
                        let next_hop = (target != destination).then_some(*target);
                        self.routes_v6.insert(prefix, next_hop, RouteKind::Redirect);
                        self.redirect_routes += 1;
                    }
                }
                apply_redirect(message, &mut self.neighbors, now);
            }
        }
    }

    /// Apply a validated Router Advertisement (RFC 4861 §6.3.4):
    /// learn the router, adopt timing parameters, bound the IPv6 MTU,
    /// install bounded on-link routes, and drive SLAAC.
    fn on_router_advertisement(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv6Header,
        message: &NdMessage,
        now: Duration64,
    ) {
        let NdMessage::RouterAdvertisement {
            cur_hop_limit,
            router_lifetime,
            reachable_time,
            retrans_timer,
            source_ll,
            mtu,
            prefixes,
            ..
        } = message
        else {
            return;
        };
        // RFC 4861 §6.1.2: RAs come from link-local sources.
        if !is_unicast_link_local(&header.source) {
            self.counters.rx_dropped += 1;
            return;
        }
        if let Some(ll) = source_ll {
            self.neighbors.learn(IpAddr::V6(header.source), *ll, now);
        }
        self.routers.update(header.source, *router_lifetime, now);
        if *cur_hop_limit != 0 {
            self.hop_limit = *cur_hop_limit;
        }
        self.neighbors.set_timing(
            (*reachable_time != 0).then(|| {
                Duration64::from_nanos(u64::from(*reachable_time).saturating_mul(1_000_000))
            }),
            (*retrans_timer != 0).then(|| {
                Duration64::from_nanos(u64::from(*retrans_timer).saturating_mul(1_000_000))
            }),
        );
        if let Some(ra_mtu) = mtu {
            let ra_mtu = *ra_mtu as usize;
            if ra_mtu >= IPV6_MIN_MTU {
                self.mtu_v6 = ra_mtu.clamp(IPV6_MIN_MTU, self.link_mtu);
            }
        }
        for info in prefixes {
            if !info.on_link
                || info.prefix_len > 128
                || is_unicast_link_local(&info.prefix)
                || self.ra_routes >= MAX_RA_ROUTES
            {
                continue;
            }
            let masked = mask_v6(info.prefix, info.prefix_len);
            if let Some(prefix) = Prefix::new(masked, info.prefix_len) {
                self.routes_v6.insert(prefix, None, RouteKind::RaOnLink);
                self.ra_routes += 1;
            }
        }
        self.iface.on_router_advertisement(prefixes, now);
        self.drain_pending(out, IpAddr::V6(header.source), now);
    }

    /// The effective IPv6 MTU in the `u32` domain the path-MTU cache
    /// speaks. `mtu_v6` never exceeds the link MTU, which
    /// `DeviceFacts::validate` bounds at 65535, so the narrowing is
    /// lossless; a violated invariant saturates rather than truncates.
    fn mtu_v6_wire(&self) -> u32 {
        u32::try_from(self.mtu_v6).unwrap_or(u32::MAX)
    }

    /// A pooled buffer holding a copy of `src`, so a delivered payload is
    /// carried in a recycled buffer rather than a freshly allocated one.
    fn pooled_copy(&mut self, src: &[u8]) -> Vec<u8> {
        let mut buf = self.bufs.take();
        buf.clear();
        buf.extend_from_slice(src);
        buf
    }

    /// Emit one Ethernet frame with no transmit offload (the
    /// control-plane path: ARP, ND, IGMP/MLD, ICMP).
    fn push_frame(
        &mut self,
        out: &mut StackOutput,
        dst: MacAddress,
        ethertype: u16,
        packet: &[u8],
    ) {
        self.push_frame_offloaded(out, dst, ethertype, packet, TxOffload::None);
    }

    /// Emit one Ethernet frame carrying `offload`.
    fn push_frame_offloaded(
        &mut self,
        out: &mut StackOutput,
        dst: MacAddress,
        ethertype: u16,
        packet: &[u8],
        offload: TxOffload,
    ) {
        let mut frame = self.bufs.take_zeroed(ETHERNET_HEADER_LEN + packet.len());
        if write_header(&mut frame, dst, self.mac, ethertype).is_none() {
            self.bufs.give(frame);
            return;
        }
        frame[ETHERNET_HEADER_LEN..].copy_from_slice(packet);
        self.counters.tx_frames += 1;
        self.counters.tx_bytes += frame.len() as u64;
        out.frames.push(TxFrame {
            offload,
            bytes: frame,
        });
    }

    /// Transmit `packet` to `next_hop`, parking it (bounded) while the
    /// neighbour resolves, attaching `offload` to the emitted (or parked)
    /// frame.
    fn resolve_and_send_offloaded(
        &mut self,
        out: &mut StackOutput,
        next_hop: IpAddr,
        ethertype: u16,
        packet: Vec<u8>,
        offload: TxOffload,
        now: Duration64,
    ) {
        match self.neighbors.lookup(next_hop, now) {
            LookupResult::Send(mac) => {
                self.push_frame_offloaded(out, mac, ethertype, &packet, offload);
                self.bufs.give(packet);
            }
            LookupResult::Pending => {
                if self.pending.len() >= MAX_PENDING_PACKETS {
                    self.counters.pending_dropped += 1;
                    self.bufs.give(packet);
                } else {
                    self.pending.push(PendingPacket {
                        next_hop,
                        ethertype,
                        packet,
                        offload,
                    });
                }
                // The new entry's first solicitation is due now.
                let actions = self.neighbors.advance(now);
                self.apply_neighbor_actions(out, actions, now);
            }
            LookupResult::TableFull => {
                self.counters.pending_dropped += 1;
                self.bufs.give(packet);
            }
        }
    }

    /// Release parked packets whose next hop just resolved.
    fn drain_pending(&mut self, out: &mut StackOutput, ip: IpAddr, now: Duration64) {
        if !self.pending.iter().any(|p| p.next_hop == ip) {
            return;
        }
        let LookupResult::Send(mac) = self.neighbors.lookup(ip, now) else {
            return;
        };
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].next_hop == ip {
                let parked = self.pending.remove(index);
                self.push_frame_offloaded(
                    out,
                    mac,
                    parked.ethertype,
                    &parked.packet,
                    parked.offload,
                );
                self.bufs.give(parked.packet);
            } else {
                index += 1;
            }
        }
    }

    /// Turn neighbour-cache actions into solicitations and failure
    /// events.
    fn apply_neighbor_actions(
        &mut self,
        out: &mut StackOutput,
        actions: Vec<NeighborAction>,
        now: Duration64,
    ) {
        for action in actions {
            match action {
                NeighborAction::SolicitMulticast { ip } => match ip {
                    IpAddr::V4(target) => self.send_arp_request(out, target, BROADCAST),
                    IpAddr::V6(target) => self.send_neighbor_solicitation(out, target, None, now),
                },
                NeighborAction::SolicitUnicast { ip, mac } => match ip {
                    IpAddr::V4(target) => self.send_arp_request(out, target, mac),
                    IpAddr::V6(target) => {
                        self.send_neighbor_solicitation(out, target, Some(mac), now);
                    }
                },
                NeighborAction::Unreachable { ip } => {
                    let mut index = 0;
                    while index < self.pending.len() {
                        if self.pending[index].next_hop == ip {
                            let parked = self.pending.remove(index);
                            self.bufs.give(parked.packet);
                            self.counters.pending_dropped += 1;
                        } else {
                            index += 1;
                        }
                    }
                    out.events.push(StackEvent::NeighborUnreachable { ip });
                }
            }
        }
    }

    /// Emit an ARP request for `target` to `dest_mac`.
    fn send_arp_request(&mut self, out: &mut StackOutput, target: Ipv4Addr, dest_mac: MacAddress) {
        let Some((our_v4, _)) = self.iface.ipv4() else {
            return;
        };
        let request = ArpPacket {
            operation: OP_REQUEST,
            sender_hardware: self.mac,
            sender_protocol: our_v4,
            target_hardware: MacAddress([0; 6]),
            target_protocol: target,
        };
        let mut buf = [0u8; crate::arp::ARP_PACKET_LEN];
        if request.write(&mut buf).is_some() {
            self.push_frame(out, dest_mac, ETHERTYPE_ARP, &buf);
        }
    }

    /// Build and emit one ND message as an `ICMPv6` frame (hop limit
    /// 255) straight to a known destination MAC.
    fn push_nd(
        &mut self,
        out: &mut StackOutput,
        source: Ipv6Addr,
        dest: Ipv6Addr,
        dest_mac: MacAddress,
        message: &NdMessage,
    ) {
        let mut body = [0u8; 64];
        let Some(body_len) = message.write_body(&mut body) else {
            return;
        };
        let icmp = IcmpMessage {
            message_type: message.message_type(),
            code: 0,
            body: &body[..body_len],
        };
        let mut icmp_bytes = vec![0u8; crate::icmp::ICMP_FIXED_HEADER_LEN + body_len];
        let context = IcmpContext::V6 {
            source,
            destination: dest,
        };
        if icmp.write(context, &mut icmp_bytes).is_none() {
            return;
        }
        let mut header = Ipv6Header::new(source, dest, NEXT_HEADER_ICMPV6);
        header.hop_limit = ND_HOP_LIMIT;
        let Some(packet) = ipv6_packet(&header, &icmp_bytes) else {
            return;
        };
        self.push_frame(out, dest_mac, ETHERTYPE_IPV6, &packet);
    }

    /// Emit a resolution Neighbour Solicitation for `target`:
    /// multicast to its solicited-node group, or unicast when probing
    /// a known MAC.
    fn send_neighbor_solicitation(
        &mut self,
        out: &mut StackOutput,
        target: Ipv6Addr,
        unicast_mac: Option<MacAddress>,
        _now: Duration64,
    ) {
        let Some(source) = self.source_for_v6(target) else {
            return;
        };
        let (dest, dest_mac) = if let Some(mac) = unicast_mac {
            (target, mac)
        } else {
            let group = solicited_node_multicast(&target);
            (group, ipv6_multicast_mac(&group))
        };
        let message = NdMessage::NeighborSolicitation {
            target,
            source_ll: Some(self.mac),
        };
        self.push_nd(out, source, dest, dest_mac, &message);
    }

    /// Emit a DAD Neighbour Solicitation: unspecified source, no
    /// link-layer option (RFC 4862 §5.4.2).
    fn send_dad_solicit(&mut self, out: &mut StackOutput, target: Ipv6Addr) {
        let group = solicited_node_multicast(&target);
        let message = NdMessage::NeighborSolicitation {
            target,
            source_ll: None,
        };
        self.push_nd(
            out,
            Ipv6Addr::from([0u8; 16]),
            group,
            ipv6_multicast_mac(&group),
            &message,
        );
    }

    /// Emit a Router Solicitation to all-routers (RFC 4861 §6.3.7).
    fn send_router_solicitation(&mut self, out: &mut StackOutput, source: Option<Ipv6Addr>) {
        let message = NdMessage::RouterSolicitation {
            source_ll: source.map(|_| self.mac),
        };
        self.push_nd(
            out,
            source.unwrap_or(Ipv6Addr::from([0u8; 16])),
            ALL_ROUTERS,
            ipv6_multicast_mac(&ALL_ROUTERS),
            &message,
        );
    }

    /// Emit a Neighbour Advertisement claiming `target` (an address
    /// this host owns), to `dest`.
    fn send_neighbor_advertisement(
        &mut self,
        out: &mut StackOutput,
        target: Ipv6Addr,
        dest: Ipv6Addr,
        solicited: bool,
        now: Duration64,
    ) {
        let message = NdMessage::NeighborAdvertisement {
            router: false,
            solicited,
            override_flag: true,
            target,
            target_ll: Some(self.mac),
        };
        if dest.is_multicast() {
            self.push_nd(out, target, dest, ipv6_multicast_mac(&dest), &message);
            return;
        }
        // Unicast: the soliciting node's MAC is normally already
        // learned from its solicitation; otherwise resolve.
        if let LookupResult::Send(mac) = self.neighbors.lookup(IpAddr::V6(dest), now) {
            self.push_nd(out, target, dest, mac, &message);
        }
    }

    /// Re-announce every address this interface owns so on-link peers
    /// relearn the path to it — a gratuitous ARP for the IPv4 address and
    /// an unsolicited Neighbour Advertisement (to all-nodes, override set)
    /// for each non-tentative IPv6 address.
    ///
    /// A bond emits this on failover (`plans/NETWORK.md` §6.3): after the
    /// transmit path moves to a new member the bond keeps its MAC, so a
    /// switch must be told the MAC is now reachable on the new port. It is
    /// harmless on a plain interface (a redundant announcement) and does
    /// nothing while the link is down.
    pub fn announce_presence(&mut self, out: &mut StackOutput, now: Duration64) {
        if !self.link_up {
            return;
        }
        if let Some((our_v4, _)) = self.iface.ipv4() {
            // A gratuitous ARP is a broadcast request whose sender and
            // target protocol addresses are both our own address.
            self.send_arp_request(out, our_v4, BROADCAST);
        }
        let announce: Vec<Ipv6Addr> = self
            .iface
            .ipv6_addresses()
            .iter()
            .filter(|info| !info.tentative)
            .map(|info| info.addr)
            .collect();
        for addr in announce {
            self.send_neighbor_advertisement(out, addr, ALL_NODES, false, now);
        }
    }

    /// RFC 6724 source selection over the interface's usable
    /// addresses.
    fn source_for_v6(&self, dest: Ipv6Addr) -> Option<Ipv6Addr> {
        let candidates: Vec<CandidateAddr> = self.iface.candidates();
        crate::route::select_source(&candidates, dest)
    }

    /// The next hop for a unicast IPv6 destination: the destination
    /// itself when on-link (a matching no-next-hop route, or
    /// link-local), a route's gateway, or a reachable-preferred
    /// default router (RFC 4861 §5.2, §6.3.6).
    fn next_hop_v6(&mut self, dest: Ipv6Addr, _now: Duration64) -> Option<Ipv6Addr> {
        if is_unicast_link_local(&dest) {
            return Some(dest);
        }
        if let Some(route) = self.routes_v6.lookup(dest) {
            return Some(route.next_hop.unwrap_or(dest));
        }
        let neighbors = &self.neighbors;
        self.routers.select(|router| {
            matches!(
                neighbors.entry(IpAddr::V6(router)),
                Some((crate::neigh::NeighborState::Reachable, _))
            )
        })
    }

    /// The next hop for a unicast IPv4 destination.
    fn next_hop_v4(&self, dest: Ipv4Addr) -> Option<Ipv4Addr> {
        let route = self.routes_v4.lookup(dest)?;
        Some(route.next_hop.unwrap_or(dest))
    }

    /// Wrap an upper-layer message (ICMP or UDP) for IPv4 and transmit
    /// it with the default TTL, fragmenting when it exceeds the link MTU.
    fn send_ipv4_packet(
        &mut self,
        out: &mut StackOutput,
        source: Ipv4Addr,
        dest: Ipv4Addr,
        protocol: u8,
        upper_message: &[u8],
        now: Duration64,
    ) {
        self.send_ipv4_packet_ttl(
            out,
            source,
            dest,
            protocol,
            upper_message,
            None,
            Ecn::NotEct,
            TxOffload::None,
            now,
        );
    }

    /// [`Self::send_ipv4_packet`], with an optional TTL override.
    ///
    /// A multicast destination maps straight to its group MAC (no
    /// neighbour resolution, RFC 1112 §6.4); a unicast one resolves its
    /// next hop through the neighbour cache. `ttl` of [`None`] keeps the
    /// header's default TTL; multicast datagrams pass an explicit
    /// link-local scope (see [`MULTICAST_DATA_HOP_LIMIT`]).
    #[allow(clippy::too_many_arguments)]
    fn send_ipv4_packet_ttl(
        &mut self,
        out: &mut StackOutput,
        source: Ipv4Addr,
        dest: Ipv4Addr,
        protocol: u8,
        upper_message: &[u8],
        ttl: Option<u8>,
        ecn: Ecn,
        offload: TxOffload,
        now: Duration64,
    ) {
        if !self.link_up {
            return;
        }
        // A multicast group needs no next hop; a unicast destination
        // that cannot be routed is dropped (fail closed).
        let next_hop = if dest.is_multicast() {
            None
        } else {
            match self.next_hop_v4(dest) {
                Some(hop) => Some(hop),
                None => return,
            }
        };
        let mut header = Ipv4Header::new(source, dest, protocol);
        if let Some(ttl) = ttl {
            header.ttl = ttl;
        }
        header.ecn = ecn;
        header.identification = self.next_ident;
        self.next_ident = self.next_ident.wrapping_add(1);
        // A segmentation-offload super-segment is emitted as one packet
        // even though it exceeds the link MTU: the device splits it into
        // MTU-sized packets on the wire, so the IP layer must not fragment
        // it (that would defeat the offload and split the transport
        // header off the payload).
        let is_segmentation = matches!(offload, TxOffload::TcpSegment { .. });
        if is_segmentation || IPV4_HEADER_LEN + upper_message.len() <= self.link_mtu {
            let mut packet = self.bufs.take_zeroed(IPV4_HEADER_LEN + upper_message.len());
            if header.write(&mut packet, upper_message.len()).is_none() {
                self.bufs.give(packet);
                return;
            }
            packet[IPV4_HEADER_LEN..].copy_from_slice(upper_message);
            self.emit_ipv4_frame(out, dest, next_hop, packet, offload, now);
            return;
        }
        // A fragmented datagram cannot carry a single-packet transport
        // checksum offload (only the first fragment holds the transport
        // header): each fragment is emitted with its software checksum.
        let Some(parts) = crate::ipv4::fragment(header, upper_message.len(), self.link_mtu) else {
            return;
        };
        for part in parts {
            let payload = &upper_message[part.payload_start..part.payload_end];
            let mut packet = self.bufs.take_zeroed(IPV4_HEADER_LEN + payload.len());
            if part.header.write(&mut packet, payload.len()).is_none() {
                self.bufs.give(packet);
                continue;
            }
            packet[IPV4_HEADER_LEN..].copy_from_slice(payload);
            self.emit_ipv4_frame(out, dest, next_hop, packet, TxOffload::None, now);
        }
    }

    /// Emit one built IPv4 packet carrying `offload`: a multicast
    /// destination (`next_hop` [`None`]) goes straight to its group MAC;
    /// a unicast one resolves `next_hop` through the neighbour cache.
    fn emit_ipv4_frame(
        &mut self,
        out: &mut StackOutput,
        dest: Ipv4Addr,
        next_hop: Option<Ipv4Addr>,
        packet: Vec<u8>,
        offload: TxOffload,
        now: Duration64,
    ) {
        if let Some(next_hop) = next_hop {
            self.resolve_and_send_offloaded(
                out,
                IpAddr::V4(next_hop),
                ETHERTYPE_IPV4,
                packet,
                offload,
                now,
            );
        } else {
            self.push_frame_offloaded(
                out,
                ipv4_multicast_mac(&dest),
                ETHERTYPE_IPV4,
                &packet,
                offload,
            );
            self.bufs.give(packet);
        }
    }

    /// Wrap an upper-layer message (`ICMPv6` or UDP) and transmit it:
    /// multicast destinations map straight to their group MAC, unicast
    /// ones resolve through the neighbour cache.
    #[allow(clippy::too_many_arguments)]
    fn send_ipv6_packet(
        &mut self,
        out: &mut StackOutput,
        source: Ipv6Addr,
        dest: Ipv6Addr,
        next_header: u8,
        upper_message: &[u8],
        hop_limit: u8,
        now: Duration64,
    ) {
        self.send_ipv6_packet_opt(
            out,
            source,
            dest,
            next_header,
            upper_message,
            hop_limit,
            false,
            Ecn::NotEct,
            TxOffload::None,
            now,
        );
    }

    /// [`Self::send_ipv6_packet`], optionally prepending a Hop-by-Hop
    /// Router Alert header (RFC 2711) and carrying `offload`. MLD
    /// membership reports set `router_alert`; every other emit path
    /// leaves it clear. A Router Alert (a control datagram) never carries
    /// an offload.
    #[allow(clippy::too_many_arguments)]
    fn send_ipv6_packet_opt(
        &mut self,
        out: &mut StackOutput,
        source: Ipv6Addr,
        dest: Ipv6Addr,
        next_header: u8,
        upper_message: &[u8],
        hop_limit: u8,
        router_alert: bool,
        ecn: Ecn,
        offload: TxOffload,
        now: Duration64,
    ) {
        if !self.link_up {
            return;
        }
        // Without a Router Alert the upper message is the IPv6 payload
        // directly (no copy on this hot path). With one, the Hop-by-Hop
        // header — which itself names the upper protocol — is prepended.
        let packet = if router_alert {
            let hbh = hop_by_hop_router_alert(next_header);
            let mut payload = self.bufs.take();
            payload.extend_from_slice(&hbh);
            payload.extend_from_slice(upper_message);
            let mut header = Ipv6Header::new(source, dest, NEXT_HEADER_HOP_BY_HOP);
            header.hop_limit = hop_limit;
            header.set_ecn(ecn);
            let packet = self.pooled_ipv6_packet(&header, &payload);
            self.bufs.give(payload);
            packet
        } else {
            let mut header = Ipv6Header::new(source, dest, next_header);
            header.hop_limit = hop_limit;
            header.set_ecn(ecn);
            self.pooled_ipv6_packet(&header, upper_message)
        };
        let Some(packet) = packet else {
            return;
        };
        self.emit_ipv6_frame(out, dest, packet, offload, now);
    }

    /// Emit one built IPv6 packet: a multicast destination goes straight
    /// to its group MAC (no neighbour resolution, RFC 2464 §7); a unicast
    /// one resolves its next hop through the neighbour cache, parking the
    /// packet until resolution completes. The pooled buffer is always
    /// returned to the pool.
    fn emit_ipv6_frame(
        &mut self,
        out: &mut StackOutput,
        dest: Ipv6Addr,
        packet: Vec<u8>,
        offload: TxOffload,
        now: Duration64,
    ) {
        if dest.is_multicast() {
            self.push_frame_offloaded(
                out,
                ipv6_multicast_mac(&dest),
                ETHERTYPE_IPV6,
                &packet,
                offload,
            );
            self.bufs.give(packet);
            return;
        }
        let Some(next_hop) = self.next_hop_v6(dest, now) else {
            self.bufs.give(packet);
            return;
        };
        self.resolve_and_send_offloaded(
            out,
            IpAddr::V6(next_hop),
            ETHERTYPE_IPV6,
            packet,
            offload,
            now,
        );
    }

    /// Source-fragment an oversize IPv6 datagram (RFC 8200 §4.5) and emit
    /// each fragment.
    ///
    /// Only the source may fragment an IPv6 datagram, so a host that
    /// originates one larger than `mtu` must split it here. The whole
    /// upper-layer message (`upper_message`) is the fragmentable part —
    /// its transport checksum, already computed over the entire message,
    /// travels in the first fragment. Every fragment of the datagram
    /// shares one identification. Returns `false` (having emitted nothing)
    /// when the datagram cannot be fragmented onto `mtu` (fail closed);
    /// the caller then reports [`SendError::TooLarge`].
    #[allow(clippy::too_many_arguments)]
    fn send_ipv6_fragmented(
        &mut self,
        out: &mut StackOutput,
        source: Ipv6Addr,
        dest: Ipv6Addr,
        next_header: u8,
        upper_message: &[u8],
        hop_limit: u8,
        mtu: usize,
        now: Duration64,
    ) -> bool {
        if !self.link_up {
            return false;
        }
        let Some(pieces) = crate::ipv6::fragment(upper_message.len(), mtu) else {
            return false;
        };
        let identification = self.next_ipv6_ident;
        self.next_ipv6_ident = self.next_ipv6_ident.wrapping_add(1);
        for piece in pieces {
            let data = &upper_message[piece.payload_start..piece.payload_end];
            let Some(packet) = self.pooled_ipv6_fragment(
                source,
                dest,
                hop_limit,
                next_header,
                identification,
                &piece,
                data,
            ) else {
                continue;
            };
            self.emit_ipv6_frame(out, dest, packet, TxOffload::None, now);
        }
        true
    }

    /// Assemble one IPv6 fragment — fixed header (next-header Fragment) +
    /// Fragment extension header + this piece's payload — into a pooled
    /// buffer. Returns the buffer to the pool and `None` if the headers
    /// will not serialise.
    #[allow(clippy::too_many_arguments)]
    fn pooled_ipv6_fragment(
        &mut self,
        source: Ipv6Addr,
        dest: Ipv6Addr,
        hop_limit: u8,
        upper_header: u8,
        identification: u32,
        piece: &crate::ipv6::FragmentPiece,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        let payload_len = crate::ipv6::FRAGMENT_HEADER_LEN + data.len();
        let mut header = Ipv6Header::new(source, dest, crate::ipv6::NEXT_HEADER_FRAGMENT);
        header.hop_limit = hop_limit;
        let mut buf = self.bufs.take_zeroed(IPV6_HEADER_LEN + payload_len);
        let ok = header.write(&mut buf, payload_len).is_some()
            && crate::ipv6::write_fragment_header(
                &mut buf[IPV6_HEADER_LEN..],
                upper_header,
                piece.offset,
                piece.more,
                identification,
            )
            .is_some();
        if !ok {
            self.bufs.give(buf);
            return None;
        }
        buf[IPV6_HEADER_LEN + crate::ipv6::FRAGMENT_HEADER_LEN..].copy_from_slice(data);
        Some(buf)
    }

    /// Assemble a fixed IPv6 header and payload into a **pooled** buffer
    /// (the allocation-free transmit path). Returns the buffer to the
    /// pool and `None` if the header will not serialise.
    fn pooled_ipv6_packet(&mut self, header: &Ipv6Header, payload: &[u8]) -> Option<Vec<u8>> {
        let mut buf = self.bufs.take_zeroed(IPV6_HEADER_LEN + payload.len());
        if write_ipv6_into(&mut buf, header, payload).is_none() {
            self.bufs.give(buf);
            return None;
        }
        Some(buf)
    }

    /// Gate, rate-limit, and emit an ICMP error about a v4 packet.
    #[allow(clippy::too_many_arguments)]
    fn emit_icmp_error_v4(
        &mut self,
        out: &mut StackOutput,
        invoking_source: Ipv4Addr,
        our_addr: Ipv4Addr,
        kind: IcmpErrorKind,
        invoking_packet: &[u8],
        context: ErrorContext,
        now: Duration64,
    ) {
        if !error_allowed(context) {
            return;
        }
        if !self.error_limiter.allow(now) {
            self.counters.icmp_errors_suppressed += 1;
            return;
        }
        let error = IcmpError::about(kind, invoking_packet, false);
        let mut message = vec![0u8; error.wire_len()];
        if error.write(IcmpContext::V4, &mut message).is_none() {
            return;
        }
        self.counters.icmp_errors_sent += 1;
        self.send_ipv4_packet(out, our_addr, invoking_source, PROTOCOL_ICMP, &message, now);
    }

    /// Gate, rate-limit, and emit an `ICMPv6` error about a packet.
    fn emit_icmp_error_v6(
        &mut self,
        out: &mut StackOutput,
        invoking: &Ipv6Header,
        kind: IcmpErrorKind,
        invoking_packet: &[u8],
        context: ErrorContext,
        now: Duration64,
    ) {
        if !error_allowed(context) {
            return;
        }
        if !self.error_limiter.allow(now) {
            self.counters.icmp_errors_suppressed += 1;
            return;
        }
        // Reply from the invoked address when it is ours; otherwise
        // select a source for the reporter.
        let source = if !invoking.destination.is_multicast()
            && self.iface.is_assigned(invoking.destination)
        {
            invoking.destination
        } else {
            match self.source_for_v6(invoking.source) {
                Some(source) => source,
                None => return,
            }
        };
        let error = IcmpError::about(kind, invoking_packet, true);
        let context = IcmpContext::V6 {
            source,
            destination: invoking.source,
        };
        let mut message = vec![0u8; error.wire_len()];
        if error.write(context, &mut message).is_none() {
            return;
        }
        self.counters.icmp_errors_sent += 1;
        self.send_ipv6_packet(
            out,
            source,
            invoking.source,
            NEXT_HEADER_ICMPV6,
            &message,
            self.hop_limit,
            now,
        );
    }

    /// Originate an ICMP echo request to `dest`.
    ///
    /// # Errors
    ///
    /// Typed [`SendError`] refusals: link down, non-unicast
    /// destination, no source address / v4 configuration, no route,
    /// or a payload too large to fit or fragment onto the path (an
    /// oversize IPv6 request is source-fragmented, RFC 8200 §4.5).
    pub fn send_echo_request(
        &mut self,
        dest: IpAddr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        now: Duration64,
        out: &mut StackOutput,
    ) -> Result<(), SendError> {
        out.recycle_into(&mut self.bufs);
        if !self.link_up {
            return Err(SendError::LinkDown);
        }
        match dest {
            IpAddr::V4(dest) => {
                if dest.is_multicast() || dest.is_broadcast() || dest.is_unspecified() {
                    return Err(SendError::NotUnicast);
                }
                let Some((source, _)) = self.iface.ipv4() else {
                    return Err(SendError::NoSourceAddress);
                };
                if self.next_hop_v4(dest).is_none() {
                    return Err(SendError::NoRoute);
                }
                let echo = IcmpEcho {
                    kind: crate::icmp::EchoKind::Request,
                    identifier,
                    sequence,
                    payload,
                };
                let mut message = self.bufs.take_zeroed(echo.wire_len());
                echo.write(IcmpContext::V4, &mut message)
                    .ok_or(SendError::TooLarge)?;
                self.send_ipv4_packet(out, source, dest, PROTOCOL_ICMP, &message, now);
                self.bufs.give(message);
            }
            IpAddr::V6(dest) => {
                if dest.is_multicast() || dest.is_unspecified() {
                    return Err(SendError::NotUnicast);
                }
                let source = self.source_for_v6(dest).ok_or(SendError::NoSourceAddress)?;
                if self.next_hop_v6(dest, now).is_none() {
                    return Err(SendError::NoRoute);
                }
                let path_mtu = self.pmtu.mtu(dest, self.mtu_v6_wire(), now) as usize;
                let echo = IcmpEcho {
                    kind: crate::icmp::EchoKind::Request,
                    identifier,
                    sequence,
                    payload,
                };
                let context = IcmpContext::V6 {
                    source,
                    destination: dest,
                };
                // The `ICMPv6` checksum spans the whole message and is
                // computed here, before any fragmentation, so it travels
                // (correctly) in the first fragment.
                let mut message = self.bufs.take_zeroed(echo.wire_len());
                if echo.write(context, &mut message).is_none() {
                    self.bufs.give(message);
                    return Err(SendError::TooLarge);
                }
                let sent = if IPV6_HEADER_LEN + message.len() <= path_mtu {
                    self.send_ipv6_packet(
                        out,
                        source,
                        dest,
                        NEXT_HEADER_ICMPV6,
                        &message,
                        self.hop_limit,
                        now,
                    );
                    true
                } else {
                    self.send_ipv6_fragmented(
                        out,
                        source,
                        dest,
                        NEXT_HEADER_ICMPV6,
                        &message,
                        self.hop_limit,
                        path_mtu,
                        now,
                    )
                };
                self.bufs.give(message);
                if !sent {
                    return Err(SendError::TooLarge);
                }
            }
        }
        Ok(())
    }

    /// Originate a UDP datagram from `source_port` to `dest`:`destination_port`.
    ///
    /// A unicast destination resolves its next hop through the neighbour
    /// cache; a multicast group is transmitted straight to its group MAC
    /// with a link-local scope (TTL / hop-limit 1) and needs no route or
    /// membership (a host may send to a group it has not joined,
    /// RFC 1112 §6.2). The limited broadcast (`255.255.255.255`) and the
    /// unspecified address are refused as [`SendError::NotUnicast`] (fail
    /// closed): neither is a meaningful datagram destination here. An
    /// oversize datagram is fragmented on emit for either family — IPv4
    /// (RFC 791) and IPv6 source fragmentation (RFC 8200 §4.5) alike —
    /// against the path MTU (unicast) or link MTU (multicast); it is
    /// refused as [`SendError::TooLarge`] only when it cannot be
    /// fragmented at all (a datagram beyond the fragmentation limits).
    ///
    /// # Errors
    ///
    /// Typed [`SendError`] refusals: link down, broadcast/unspecified
    /// destination, no usable source address / v4 configuration, no route
    /// (unicast only), or a datagram too large to fragment onto the link
    /// or to fit the datagram-length field.
    pub fn send_datagram(
        &mut self,
        dest: IpAddr,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
        now: Duration64,
        out: &mut StackOutput,
    ) -> Result<(), SendError> {
        out.recycle_into(&mut self.bufs);
        if !self.link_up {
            return Err(SendError::LinkDown);
        }
        match dest {
            IpAddr::V4(dest) => {
                if dest.is_broadcast() || dest.is_unspecified() {
                    return Err(SendError::NotUnicast);
                }
                let Some((source, _)) = self.iface.ipv4() else {
                    return Err(SendError::NoSourceAddress);
                };
                if !dest.is_multicast() && self.next_hop_v4(dest).is_none() {
                    return Err(SendError::NoRoute);
                }
                let mut message = self.bufs.take_zeroed(udp::UDP_HEADER_LEN + payload.len());
                udp::write(
                    udp::Pseudo::V4 {
                        source,
                        destination: dest,
                    },
                    source_port,
                    destination_port,
                    payload,
                    &mut message,
                )
                .map_err(|_| SendError::TooLarge)?;
                let ttl = dest.is_multicast().then_some(MULTICAST_DATA_HOP_LIMIT);
                self.send_ipv4_packet_ttl(
                    out,
                    source,
                    dest,
                    PROTOCOL_UDP,
                    &message,
                    ttl,
                    Ecn::NotEct,
                    TxOffload::None,
                    now,
                );
                self.bufs.give(message);
            }
            IpAddr::V6(dest) => {
                self.send_datagram_v6(out, dest, source_port, destination_port, payload, now)?;
            }
        }
        Ok(())
    }

    /// The IPv6 UDP origination path: resolve the source and next hop,
    /// fold the pseudo-header checksum, and emit the datagram whole when it
    /// fits the path MTU or source-fragmented (RFC 8200 §4.5) when it does
    /// not. A multicast group has no path-MTU state, so it is bounded by
    /// (and fragments against) the link MTU.
    fn send_datagram_v6(
        &mut self,
        out: &mut StackOutput,
        dest: Ipv6Addr,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
        now: Duration64,
    ) -> Result<(), SendError> {
        if dest.is_unspecified() {
            return Err(SendError::NotUnicast);
        }
        let source = self.source_for_v6(dest).ok_or(SendError::NoSourceAddress)?;
        if !dest.is_multicast() && self.next_hop_v6(dest, now).is_none() {
            return Err(SendError::NoRoute);
        }
        let total = udp::UDP_HEADER_LEN + payload.len();
        let path_mtu = if dest.is_multicast() {
            self.mtu_v6_wire() as usize
        } else {
            self.pmtu.mtu(dest, self.mtu_v6_wire(), now) as usize
        };
        let mut message = self.bufs.take_zeroed(total);
        if udp::write(
            udp::Pseudo::V6 {
                source,
                destination: dest,
            },
            source_port,
            destination_port,
            payload,
            &mut message,
        )
        .is_err()
        {
            self.bufs.give(message);
            return Err(SendError::TooLarge);
        }
        let hop_limit = if dest.is_multicast() {
            MULTICAST_DATA_HOP_LIMIT
        } else {
            self.hop_limit
        };
        let sent = if IPV6_HEADER_LEN + total <= path_mtu {
            self.send_ipv6_packet(out, source, dest, PROTOCOL_UDP, &message, hop_limit, now);
            true
        } else {
            self.send_ipv6_fragmented(
                out,
                source,
                dest,
                PROTOCOL_UDP,
                &message,
                hop_limit,
                path_mtu,
                now,
            )
        };
        self.bufs.give(message);
        if sent {
            Ok(())
        } else {
            Err(SendError::TooLarge)
        }
    }

    /// The effective TCP maximum segment size (data bytes) for a
    /// reachable unicast `dest` over this interface: the family's path
    /// MTU minus the IP and fixed TCP headers (RFC 6691).
    ///
    /// This is the value the stack both advertises in its SYN (the most it
    /// is willing to receive over this link) and clamps its send
    /// segmentation to (the most it can put on the wire), so a full-size
    /// segment plus its headers and options never exceeds the link — the
    /// difference between the IPv4 (20-byte) and IPv6 (40-byte) headers is
    /// accounted here rather than discovered as a dropped segment.
    ///
    /// Returns `None` when `dest` is not a reachable unicast destination
    /// (link down, no route, no usable source address, or a non-unicast
    /// address), so the caller fails closed rather than opening a
    /// connection this interface cannot carry.
    #[must_use]
    pub fn tcp_local_mss(&mut self, dest: IpAddr, now: Duration64) -> Option<u16> {
        if !self.link_up {
            return None;
        }
        let (mtu, ip_header) = match dest {
            IpAddr::V4(d) => {
                if d.is_broadcast() || d.is_unspecified() || d.is_multicast() {
                    return None;
                }
                self.iface.ipv4()?;
                self.next_hop_v4(d)?;
                (self.link_mtu, IPV4_HEADER_LEN)
            }
            IpAddr::V6(d) => {
                if d.is_unspecified() || d.is_multicast() {
                    return None;
                }
                self.source_for_v6(d)?;
                self.next_hop_v6(d, now)?;
                (
                    self.pmtu.mtu(d, self.mtu_v6_wire(), now) as usize,
                    IPV6_HEADER_LEN,
                )
            }
        };
        let payload = mtu
            .saturating_sub(ip_header)
            .saturating_sub(tcp::TCP_HEADER_LEN);
        // Clamped into `1..=u16::MAX`, so the conversion is always exact.
        Some(u16::try_from(payload.clamp(1, usize::from(u16::MAX))).unwrap_or(u16::MAX))
    }

    /// The largest TCP payload a connection out this interface may batch
    /// into one segmentation-offload super-segment, or `0` when the device
    /// did not negotiate segmentation offload (so the connection stays
    /// per-MSS). The service seeds a new connection's
    /// [`TcpConfig::tso_max_payload`](crate::tcp::conn::TcpConfig::tso_max_payload)
    /// from this.
    #[must_use]
    pub fn tso_max_payload(&self) -> u16 {
        if self.tx_segment_tcp {
            // `TSO_MAX_PAYLOAD` is a compile-time-bounded value well within
            // `u16` (≈ 65 435); the clamp is defensive only.
            u16::try_from(TSO_MAX_PAYLOAD).unwrap_or(u16::MAX)
        } else {
            0
        }
    }

    /// The transmit checksum offload for a TCP segment over an IP header
    /// of `ip_header_len` bytes: [`TxOffload::PartialChecksum`] when the
    /// device negotiated TCP transmit-checksum offload and the segment is
    /// a `single_frame` (unfragmented) datagram, else [`TxOffload::None`].
    ///
    /// The checksum offsets are relative to the Ethernet frame the engine
    /// emits: the checksummed range starts at the transport header
    /// (Ethernet + IP headers) and the checksum field sits at
    /// [`tcp::CHECKSUM_OFFSET`] within it.
    fn tcp_tx_offload(&self, ip_header_len: usize, single_frame: bool) -> TxOffload {
        if !(self.tx_csum_tcp && single_frame) {
            return TxOffload::None;
        }
        // Ethernet (14) + IP header (20/40) + the TCP checksum field
        // offset all fit a `u16` far below the 1500-byte MTU.
        let csum_start = u16::try_from(ETHERNET_HEADER_LEN + ip_header_len).unwrap_or(u16::MAX);
        let csum_offset = u16::try_from(tcp::CHECKSUM_OFFSET).unwrap_or(u16::MAX);
        TxOffload::PartialChecksum {
            csum_start,
            csum_offset,
        }
    }

    /// The transmit segmentation offload for an over-size TCP segment
    /// over an IP header of `ip_header_len` bytes carrying a TCP header of
    /// `tcp_header_len` bytes (including options): a
    /// [`TxOffload::TcpSegment`] the device splits into `gso_size`-byte
    /// segments. The checksum offsets and `hdr_len` are relative to the
    /// Ethernet frame the engine emits (`csum_start` at the transport
    /// header, `hdr_len` covering Ethernet + IP + TCP headers). Only ever
    /// called with the device's negotiated segmentation offload.
    fn tcp_segment_offload(
        ip_header_len: usize,
        tcp_header_len: usize,
        gso_size: u16,
        ipv6: bool,
    ) -> TxOffload {
        // Ethernet (14) + IP header (20/40) + TCP header (20..=60) all fit
        // a `u16` far below the 65 535-byte segmentation ceiling.
        let csum_start = u16::try_from(ETHERNET_HEADER_LEN + ip_header_len).unwrap_or(u16::MAX);
        let csum_offset = u16::try_from(tcp::CHECKSUM_OFFSET).unwrap_or(u16::MAX);
        let hdr_len =
            u16::try_from(ETHERNET_HEADER_LEN + ip_header_len + tcp_header_len).unwrap_or(u16::MAX);
        TxOffload::TcpSegment {
            csum_start,
            csum_offset,
            gso_size,
            hdr_len,
            ipv6,
        }
    }

    /// Originate one TCP segment to `dest`, folding the mandatory
    /// pseudo-header checksum over the source address this interface
    /// selects for `dest`.
    ///
    /// The connection state machine ([`crate::tcp::conn::Tcb`]) lives in
    /// the service layer and produces the header [`TcpSegmentMeta`] and
    /// payload; this engine method owns the *addressing*: it selects the
    /// source, resolves the next hop, serialises the segment with the
    /// pseudo-header checksum, and IP-wraps it (protocol 6). TCP is always
    /// unicast, so a multicast, broadcast, or unspecified destination is
    /// refused as [`SendError::NotUnicast`] (fail closed). Over IPv6 a
    /// segment past the path MTU is refused as [`SendError::TooLarge`];
    /// over IPv4 the segment (already MSS-bounded by the TCB) is emitted,
    /// fragmenting only in the pathological case a peer's MSS exceeds the
    /// path.
    ///
    /// # Errors
    ///
    /// Typed [`SendError`] refusals: link down, a non-unicast
    /// destination, no usable source address / v4 configuration, no
    /// route, or a segment that cannot fit the path MTU (v6) or the
    /// segment-length field.
    ///
    /// When `gso_size` is `Some(mss)` the `payload` is an over-size TCP
    /// *super-segment* the egress device will split into `mss`-byte
    /// segments (TCP segmentation offload): the engine emits it as one
    /// IP packet with a [`TxOffload::TcpSegment`] descriptor — it never
    /// fragments the packet and does not refuse it for exceeding the path
    /// MTU, because the device produces MTU-sized packets on the wire.
    /// The caller only ever passes `Some` when the interface negotiated
    /// segmentation offload and bounded `payload` so the single IP packet
    /// stays within the 16-bit length field. `None` is the ordinary
    /// per-segment path.
    #[allow(clippy::too_many_arguments)]
    pub fn send_tcp(
        &mut self,
        dest: IpAddr,
        meta: &TcpSegmentMeta,
        payload: &[u8],
        gso_size: Option<u16>,
        ecn: Ecn,
        now: Duration64,
        out: &mut StackOutput,
    ) -> Result<(), SendError> {
        out.recycle_into(&mut self.bufs);
        if !self.link_up {
            return Err(SendError::LinkDown);
        }
        let tcp_header_len = tcp::TCP_HEADER_LEN + meta.options.wire_len();
        match dest {
            IpAddr::V4(dest) => {
                if dest.is_broadcast() || dest.is_unspecified() || dest.is_multicast() {
                    return Err(SendError::NotUnicast);
                }
                let Some((source, _)) = self.iface.ipv4() else {
                    return Err(SendError::NoSourceAddress);
                };
                if self.next_hop_v4(dest).is_none() {
                    return Err(SendError::NoRoute);
                }
                let offload = if let Some(mss) = gso_size {
                    // Segmentation offload: one over-MTU packet the device
                    // splits (emitted unfragmented below).
                    Self::tcp_segment_offload(IPV4_HEADER_LEN, tcp_header_len, mss, false)
                } else {
                    // Offload the checksum only when the device negotiated
                    // it *and* the segment fits one frame: a fragmented
                    // datagram (only the first fragment carries the
                    // transport header) must keep its software checksum.
                    let seg_len = tcp_header_len + payload.len();
                    let single_frame = IPV4_HEADER_LEN + seg_len <= self.link_mtu;
                    self.tcp_tx_offload(IPV4_HEADER_LEN, single_frame)
                };
                let mut segment = self.bufs.take_zeroed(MAX_HEADER_LEN + payload.len());
                let n = tcp::write_with_checksum(
                    Pseudo::V4 {
                        source,
                        destination: dest,
                    },
                    meta,
                    payload,
                    &mut segment,
                    checksum_mode(offload),
                )
                .map_err(|_| SendError::TooLarge)?;
                self.send_ipv4_packet_ttl(
                    out,
                    source,
                    dest,
                    PROTOCOL_TCP,
                    &segment[..n],
                    None,
                    ecn,
                    offload,
                    now,
                );
                self.bufs.give(segment);
            }
            IpAddr::V6(dest) => {
                if dest.is_unspecified() || dest.is_multicast() {
                    return Err(SendError::NotUnicast);
                }
                let source = self.source_for_v6(dest).ok_or(SendError::NoSourceAddress)?;
                if self.next_hop_v6(dest, now).is_none() {
                    return Err(SendError::NoRoute);
                }
                let offload = if let Some(mss) = gso_size {
                    Self::tcp_segment_offload(IPV6_HEADER_LEN, tcp_header_len, mss, true)
                } else {
                    // A TCP segment is never IP-fragmented — it is sized
                    // to the path MSS, and an over-MTU segment is refused
                    // below — so a negotiated offload always applies.
                    self.tcp_tx_offload(IPV6_HEADER_LEN, true)
                };
                let mut segment = self.bufs.take_zeroed(MAX_HEADER_LEN + payload.len());
                let n = tcp::write_with_checksum(
                    Pseudo::V6 {
                        source,
                        destination: dest,
                    },
                    meta,
                    payload,
                    &mut segment,
                    checksum_mode(offload),
                )
                .map_err(|_| SendError::TooLarge)?;
                // A super-segment is legitimately larger than the path MTU
                // (the device segments it); only the ordinary per-segment
                // path refuses an over-MTU segment.
                if gso_size.is_none()
                    && IPV6_HEADER_LEN + n > self.pmtu.mtu(dest, self.mtu_v6_wire(), now) as usize
                {
                    return Err(SendError::TooLarge);
                }
                self.send_ipv6_packet_opt(
                    out,
                    source,
                    dest,
                    PROTOCOL_TCP,
                    &segment[..n],
                    self.hop_limit,
                    false,
                    ecn,
                    offload,
                    now,
                );
                self.bufs.give(segment);
            }
        }
        Ok(())
    }

    /// Perform every timed transition due at `now`: interface
    /// bring-up (DAD, router solicitation), address lifetimes,
    /// neighbour-cache aging, default-router and path-MTU expiry, and
    /// reassembly timeouts.
    pub fn advance(&mut self, now: Duration64, out: &mut StackOutput) {
        out.recycle_into(&mut self.bufs);
        for action in self.iface.advance(now) {
            match action {
                IfaceAction::SendDadSolicit { target } => self.send_dad_solicit(out, target),
                IfaceAction::SendRouterSolicitation { source } => {
                    self.send_router_solicitation(out, source);
                }
                IfaceAction::AddressPreferred { addr } => {
                    // A now-usable address announces membership in its
                    // solicited-node group so routers deliver its
                    // Neighbour Solicitations (RFC 4862 §5.4.2).
                    let _ = self
                        .membership_v6
                        .join(solicited_node_multicast(&addr), now);
                    out.events.push(StackEvent::AddressPreferred { addr });
                }
                IfaceAction::AddressInvalidated { addr } => {
                    self.membership_v6
                        .leave(solicited_node_multicast(&addr), now);
                    out.events.push(StackEvent::AddressInvalidated { addr });
                }
                IfaceAction::DadFailed { addr } => {
                    self.membership_v6
                        .leave(solicited_node_multicast(&addr), now);
                    // A DHCPv6-leased address that fails DAD is in use by
                    // another host: Decline it to the server and
                    // re-acquire (RFC 8415 §18.2.10.1) rather than keep an
                    // unusable binding.
                    self.decline_dhcp6_if_leased(out, addr, now);
                    out.events.push(StackEvent::DadFailed { addr });
                }
            }
        }
        let actions = self.neighbors.advance(now);
        self.apply_neighbor_actions(out, actions, now);
        self.routers.advance(now);
        self.pmtu.advance(now);
        for expired in self.reassembler.advance(now) {
            self.counters.reassembly_expired += 1;
            out.events.push(StackEvent::ReassemblyExpired {
                source: expired.key.source,
            });
        }
        for report in self.membership_v4.advance(now) {
            self.emit_igmp_report(out, report, now);
        }
        for report in self.membership_v6.advance(now) {
            self.emit_mld_report(out, report, now);
        }
        // Drive the DHCPv4 client's timed work (INIT DISCOVER, retransmit
        // backoff, T1/T2/expiry) and carry out the actions it returns
        // (send a framed message, apply or withdraw a lease).
        self.drive_dhcp(out, now);
        // Drive the DHCPv6 client's timed work (INIT Solicit, retransmit
        // backoff, T1/T2/expiry) the same way.
        self.drive_dhcp6(out, now);
    }

    /// When the earliest timed transition across every component is
    /// due, for the caller's one-shot timer.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        [
            self.iface.next_deadline(),
            self.neighbors.next_deadline(),
            self.routers.next_deadline(),
            self.pmtu.next_deadline(),
            self.reassembler.next_deadline(),
            self.membership_v4.next_deadline(),
            self.membership_v6.next_deadline(),
            self.dhcp.as_ref().and_then(|d| d.client.next_deadline()),
            self.dhcp6.as_ref().and_then(|d| d.client.next_deadline()),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|deadline| (deadline.secs(), deadline.subsec_nanos()))
    }
}

// --- DHCPv4 client driving (RFC 2131) -----------------------------------
impl Stack {
    /// Poll the DHCPv4 client for the timed work due at `now` (the INIT
    /// DISCOVER, retransmission backoff, and the T1/T2/expiry transitions)
    /// and carry out the actions it returns. A no-op when no client runs.
    fn drive_dhcp(&mut self, out: &mut StackOutput, now: Duration64) {
        let actions = match self.dhcp.as_mut() {
            Some(driver) => driver.client.poll(now, &mut *driver.rng),
            None => return,
        };
        self.apply_dhcp_actions(out, actions, now);
    }

    /// Fold a received DHCP server message (UDP source 67 → destination 68)
    /// into the client, returning `true` when the datagram was a DHCP reply
    /// this client claimed — parsed and applied, or dropped as a spoof —
    /// so the caller stops treating it as ordinary IPv4 traffic. A UDP
    /// datagram that is not DHCP (or does not parse) returns `false` and
    /// takes the normal receive path.
    fn consume_dhcp_reply(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv4Header,
        payload: &[u8],
        check: ChecksumCheck,
        now: Duration64,
    ) -> bool {
        let pseudo = udp::Pseudo::V4 {
            source: header.source,
            destination: header.destination,
        };
        let Some(datagram) = UdpDatagram::parse_with(pseudo, payload, check) else {
            return false;
        };
        if datagram.destination_port != dhcp::CLIENT_PORT
            || datagram.source_port != dhcp::SERVER_PORT
        {
            return false;
        }
        // A DHCP-ported datagram belongs to this client. Match it against
        // the outstanding transaction id and this interface's hardware
        // address (the parser rejects any other), fold it, and never
        // surface it as ordinary traffic. A reply that does not parse (a
        // spoof, a truncation, a foreign transaction) is dropped.
        let (xid, chaddr) = match self.dhcp.as_ref() {
            Some(driver) => (
                driver.client.transaction_id(),
                driver.client.hardware_addr(),
            ),
            None => return false,
        };
        let Some(reply) = DhcpReply::parse(datagram.payload, xid, chaddr) else {
            self.counters.rx_dropped += 1;
            return true;
        };
        let actions = match self.dhcp.as_mut() {
            Some(driver) => driver.client.on_reply(now, &reply),
            None => return true,
        };
        self.apply_dhcp_actions(out, actions, now);
        true
    }

    /// Carry out each action the client produced: frame and transmit a
    /// message, apply a newly committed lease, or withdraw a lost one.
    fn apply_dhcp_actions(
        &mut self,
        out: &mut StackOutput,
        actions: Vec<DhcpAction>,
        now: Duration64,
    ) {
        for action in actions {
            match action {
                DhcpAction::Send(send) => self.send_dhcp(out, &send, now),
                DhcpAction::Configured(lease) => self.apply_dhcp_lease(out, lease),
                DhcpAction::Deconfigured => self.withdraw_dhcp_lease(out),
            }
        }
    }

    /// Frame one client→server DHCP message as UDP(68→67)/IPv4/Ethernet on
    /// this link and queue it.
    ///
    /// A broadcast message (DISCOVER, a SELECTING or REBINDING REQUEST) is
    /// sent from the client's current address (`0.0.0.0` before a lease) to
    /// the limited broadcast `255.255.255.255` at the link-layer broadcast
    /// MAC — no route or neighbour resolution, since the client may have
    /// neither yet. A RENEWING unicast to the leasing server is sent from
    /// the leased address and resolves the server's MAC through the
    /// neighbour cache (the lease installed its connected route).
    fn send_dhcp(&mut self, out: &mut StackOutput, action: &SendAction, now: Duration64) {
        if !self.link_up {
            return;
        }
        let mut message = [0u8; dhcp::MAX_MESSAGE_LEN];
        let Ok(len) = dhcp::write_message(&action.spec, &mut message) else {
            return;
        };
        let (source, dest) = match action.destination {
            dhcp::Destination::Broadcast => (action.spec.client_addr, Ipv4Addr::BROADCAST),
            dhcp::Destination::Server(server) => (action.spec.client_addr, server),
        };
        // The UDP datagram: client port 68 → server port 67.
        let mut datagram = self.bufs.take_zeroed(udp::UDP_HEADER_LEN + len);
        if udp::write(
            udp::Pseudo::V4 {
                source,
                destination: dest,
            },
            dhcp::CLIENT_PORT,
            dhcp::SERVER_PORT,
            &message[..len],
            &mut datagram,
        )
        .is_err()
        {
            self.bufs.give(datagram);
            return;
        }
        // Wrap it in an IPv4 packet.
        let mut ipv4 = Ipv4Header::new(source, dest, PROTOCOL_UDP);
        ipv4.identification = self.next_ident;
        self.next_ident = self.next_ident.wrapping_add(1);
        let mut packet = self.bufs.take_zeroed(IPV4_HEADER_LEN + datagram.len());
        if ipv4.write(&mut packet, datagram.len()).is_none() {
            self.bufs.give(packet);
            self.bufs.give(datagram);
            return;
        }
        packet[IPV4_HEADER_LEN..].copy_from_slice(&datagram);
        self.bufs.give(datagram);
        match action.destination {
            dhcp::Destination::Broadcast => {
                self.push_frame(out, BROADCAST, ETHERTYPE_IPV4, &packet);
                self.bufs.give(packet);
            }
            dhcp::Destination::Server(server) => {
                self.resolve_and_send_offloaded(
                    out,
                    IpAddr::V4(server),
                    ETHERTYPE_IPV4,
                    packet,
                    TxOffload::None,
                    now,
                );
            }
        }
    }

    /// Apply a committed DHCP lease: the leased address, the subnet mask's
    /// prefix, and (when the server named an on-link router) the default
    /// route. A router the server placed off the connected subnet is
    /// refused by [`Stack::set_ipv4_config`]; the address is then applied
    /// alone rather than left unconfigured (fail safe, never a partial
    /// state). A `DhcpLeaseAcquired` event lets the service audit the
    /// change.
    fn apply_dhcp_lease(&mut self, out: &mut StackOutput, lease: Lease) {
        // A lease with no (or a non-contiguous) mask falls back to a host
        // /32: the address is usable even when the server omitted a mask.
        let prefix_len = lease
            .subnet_mask
            .and_then(prefix_len_from_mask)
            .filter(|&bits| bits >= 1)
            .unwrap_or(32);
        let (applied, router) = match self.set_ipv4_config(lease.addr, prefix_len, lease.router) {
            Ok(()) => (true, lease.router),
            Err(_) => (
                self.set_ipv4_config(lease.addr, prefix_len, None).is_ok(),
                None,
            ),
        };
        if applied {
            out.events.push(StackEvent::DhcpLeaseAcquired {
                address: lease.addr,
                prefix_len,
                router,
            });
        }
    }

    /// Withdraw a DHCP lease that was lost (a NAK or expiry): drop the
    /// leased address and every IPv4 route, leaving the family enabled so
    /// the client re-acquires. A `DhcpLeaseLost` event lets the service
    /// audit the loss.
    fn withdraw_dhcp_lease(&mut self, out: &mut StackOutput) {
        self.iface.clear_ipv4();
        self.routes_v4 = RoutingTable::new();
        out.events.push(StackEvent::DhcpLeaseLost);
    }
}

// --- DHCPv6 client driving (RFC 8415) -----------------------------------
impl Stack {
    /// Poll the DHCPv6 client for the timed work due at `now` (the INIT
    /// Solicit, retransmission backoff, and the T1/T2/expiry transitions)
    /// and carry out the actions it returns. A no-op when no client runs.
    fn drive_dhcp6(&mut self, out: &mut StackOutput, now: Duration64) {
        let actions = match self.dhcp6.as_mut() {
            Some(driver) => driver.client.poll(now, &mut *driver.rng),
            None => return,
        };
        self.apply_dhcp6_actions(out, actions, now);
    }

    /// Fold a received DHCPv6 server message (UDP source 547 → destination
    /// 546) into the client, returning `true` when the datagram was a
    /// DHCPv6 reply this client claimed — parsed and applied, or dropped as
    /// a spoof — so the caller stops treating it as ordinary IPv6 traffic.
    /// A UDP datagram that is not DHCPv6 (or does not parse) returns
    /// `false` and takes the normal receive path.
    fn consume_dhcp6_reply(
        &mut self,
        out: &mut StackOutput,
        header: &Ipv6Header,
        payload: &[u8],
        check: ChecksumCheck,
        now: Duration64,
    ) -> bool {
        let pseudo = udp::Pseudo::V6 {
            source: header.source,
            destination: header.destination,
        };
        let Some(datagram) = UdpDatagram::parse_with(pseudo, payload, check) else {
            return false;
        };
        if datagram.destination_port != dhcpv6::CLIENT_PORT
            || datagram.source_port != dhcpv6::SERVER_PORT
        {
            return false;
        }
        // A DHCPv6-ported datagram belongs to this client. Match it against
        // the outstanding transaction id and this client's DUID (the parser
        // rejects any other), fold it, and never surface it as ordinary
        // traffic. A reply that does not parse (a spoof, a truncation, a
        // foreign transaction) is dropped.
        let (xid, duid) = match self.dhcp6.as_ref() {
            Some(driver) => (driver.client.transaction_id(), driver.client.client_duid()),
            None => return false,
        };
        let Some(reply) = Dhcp6Reply::parse(datagram.payload, xid, &duid) else {
            self.counters.rx_dropped += 1;
            return true;
        };
        let actions = match self.dhcp6.as_mut() {
            Some(driver) => driver.client.on_reply(now, &reply, &mut *driver.rng),
            None => return true,
        };
        self.apply_dhcp6_actions(out, actions, now);
        true
    }

    /// Carry out each action the client produced: frame and transmit a
    /// message, apply a newly committed lease, or withdraw a lost one.
    fn apply_dhcp6_actions(
        &mut self,
        out: &mut StackOutput,
        actions: Vec<Dhcp6Action>,
        now: Duration64,
    ) {
        for action in actions {
            match action {
                Dhcp6Action::Send(send) => self.send_dhcp6(out, &send, now),
                Dhcp6Action::Configured(lease) => self.apply_dhcp6_lease(out, lease, now),
                Dhcp6Action::Deconfigured => self.withdraw_dhcp6_lease(out),
            }
        }
    }

    /// Frame one client→server DHCPv6 message as UDP(546→547)/IPv6/Ethernet
    /// and queue it.
    ///
    /// Every DHCPv6 client message is sent from the interface's link-local
    /// address to the `All_DHCP_Relay_Agents_and_Servers` link-scoped
    /// multicast (`ff02::1:2`, RFC 8415 §16) at hop limit 1 — the multicast
    /// MAC is derived directly (no neighbour resolution). Until the
    /// link-local address has completed DAD there is no usable source, so
    /// the send is skipped and the client's retransmission timer re-attempts
    /// it (fail safe, never a spoofable unspecified source).
    fn send_dhcp6(&mut self, out: &mut StackOutput, action: &Send6Action, now: Duration64) {
        if !self.link_up {
            return;
        }
        let Some(source) = self.iface.link_local() else {
            return;
        };
        let mut message = [0u8; dhcpv6::MAX_MESSAGE_LEN];
        let Ok(len) = dhcpv6::write_message(&action.spec, &mut message) else {
            return;
        };
        // The UDP datagram: client port 546 → server port 547.
        let dest = dhcpv6::ALL_SERVERS_MULTICAST;
        let mut datagram = self.bufs.take_zeroed(udp::UDP_HEADER_LEN + len);
        if udp::write(
            udp::Pseudo::V6 {
                source,
                destination: dest,
            },
            dhcpv6::CLIENT_PORT,
            dhcpv6::SERVER_PORT,
            &message[..len],
            &mut datagram,
        )
        .is_err()
        {
            self.bufs.give(datagram);
            return;
        }
        self.send_ipv6_packet(
            out,
            source,
            dest,
            PROTOCOL_UDP,
            &datagram,
            MULTICAST_DATA_HOP_LIMIT,
            now,
        );
        self.bufs.give(datagram);
    }

    /// Apply a committed DHCPv6 lease: assign the leased IA_NA address as a
    /// host `/128` (DHCPv6 grants no on-link prefix — on-link reachability
    /// comes from Router Advertisements). A re-applied identical address (a
    /// renewal) is idempotent. A `Dhcp6LeaseAcquired` event lets the
    /// service audit the change. When the interface refuses the address
    /// (family disabled, table full) nothing is applied and no event is
    /// emitted (fail safe, never a partial state).
    fn apply_dhcp6_lease(&mut self, out: &mut StackOutput, lease: Lease6, now: Duration64) {
        match self.iface.add_ipv6_dhcp(lease.addr, now) {
            // A fresh assignment, or a renewal of the same address, is a
            // usable lease: audit it either way.
            Ok(()) | Err(crate::iface::AddrError::Duplicate) => {
                out.events.push(StackEvent::Dhcp6LeaseAcquired {
                    address: lease.addr,
                    valid_lifetime: lease.valid_lifetime,
                });
            }
            Err(_) => {}
        }
    }

    /// Withdraw a DHCPv6 lease that was lost (expiry, `NoBinding`, or a
    /// changed address on renewal): drop the leased address, leaving the
    /// IPv6 family enabled (link-local and any SLAAC/static addresses
    /// intact) so the client re-acquires. A `Dhcp6LeaseLost` event lets the
    /// service audit the loss.
    fn withdraw_dhcp6_lease(&mut self, out: &mut StackOutput) {
        self.iface.clear_ipv6_dhcp();
        out.events.push(StackEvent::Dhcp6LeaseLost);
    }

    /// When a DHCPv6-leased address fails DAD, Decline it to the server and
    /// re-acquire (RFC 8415 §18.2.10.1). A no-op unless a DHCPv6 client is
    /// running and `addr` is exactly the address it leased — an unrelated
    /// DAD failure (a static or SLAAC address) never touches the client.
    fn decline_dhcp6_if_leased(&mut self, out: &mut StackOutput, addr: Ipv6Addr, now: Duration64) {
        let is_leased = self
            .dhcp6
            .as_ref()
            .and_then(|d| d.client.lease())
            .is_some_and(|lease| lease.addr == addr);
        if !is_leased {
            return;
        }
        // Build the Decline (it captures the declined address) before
        // withdrawing the local binding.
        let action = match self.dhcp6.as_mut() {
            Some(driver) => driver.client.decline(now, &mut *driver.rng),
            None => return,
        };
        self.withdraw_dhcp6_lease(out);
        if let Some(action) = action {
            self.apply_dhcp6_actions(out, vec![action], now);
        }
    }
}

impl Stack {
    /// Join a multicast `group` at time `now`, so the host both receives
    /// its traffic and (for a reportable group) announces membership
    /// (IGMPv2 / MLDv2). Idempotent per reference: joining twice requires
    /// two leaves.
    ///
    /// # Errors
    ///
    /// * [`McastError::NotMulticast`] — `group` is not a multicast group.
    /// * [`McastError::CapacityExhausted`] — the bounded membership table
    ///   is full (fail closed).
    pub fn join_multicast(&mut self, group: IpAddr, now: Duration64) -> Result<bool, McastError> {
        match group {
            IpAddr::V4(g) if g.is_multicast() => self
                .membership_v4
                .join(g, now)
                .map_err(|JoinError::CapacityExhausted| McastError::CapacityExhausted),
            IpAddr::V6(g) if g.is_multicast() => self
                .membership_v6
                .join(g, now)
                .map_err(|JoinError::CapacityExhausted| McastError::CapacityExhausted),
            _ => Err(McastError::NotMulticast),
        }
    }

    /// Leave a multicast `group` at time `now`. Returns `true` when the
    /// last reference was dropped (the host has left the group).
    pub fn leave_multicast(&mut self, group: IpAddr, now: Duration64) -> bool {
        match group {
            IpAddr::V4(g) => self.membership_v4.leave(g, now),
            IpAddr::V6(g) => self.membership_v6.leave(g, now),
        }
    }

    /// True when a received IPv4 multicast destination is a group this
    /// host is a member of.
    fn accepts_v4_multicast(&self, dest: Ipv4Addr) -> bool {
        self.membership_v4.is_member(dest)
    }

    /// A counter that changes whenever [`Self::multicast_macs`] would yield
    /// a different set.
    ///
    /// Folded from the three things the set is derived from, so the frame
    /// pump can decide whether to reprogram a NIC's hardware group filter by
    /// comparing one integer per pass instead of rebuilding and diffing the
    /// set.
    #[must_use]
    pub fn multicast_revision(&self) -> u64 {
        let mut acc: u64 = 0xCBF2_9CE4_8422_2325;
        for value in [
            self.membership_v4.revision(),
            self.membership_v6.revision(),
            self.iface.multicast_revision(),
            u64::from(self.ipv4_enabled),
        ] {
            acc = (acc ^ value).wrapping_mul(0x0100_0000_01B3);
        }
        acc
    }

    /// Write the link-layer group addresses this interface needs its device
    /// to admit into `out`, replacing its contents, and report how many.
    ///
    /// Exactly the groups the receive path accepts: every joined IPv4 group,
    /// the IPv6 all-nodes group, the solicited-node group of every IPv6
    /// address (tentative included — DAD listens there, so an address whose
    /// solicited-node group were missing would pass DAD against a duplicate
    /// that could not answer), and every joined IPv6 group. Two IPv6 groups
    /// can share one link-layer address, so the result is deduplicated.
    ///
    /// A device that filters groups in hardware admits this set and nothing
    /// else; a device reporting
    /// [`McastFilter::Unfiltered`](tairix_abi::driver::net::McastFilter::Unfiltered)
    /// needs none of it.
    pub fn multicast_macs(&self, out: &mut Vec<MacAddress>) {
        out.clear();
        let push = |mac: MacAddress, out: &mut Vec<MacAddress>| {
            if !out.contains(&mac) {
                out.push(mac);
            }
        };
        if self.ipv4_enabled {
            for group in self.membership_v4.groups() {
                push(ipv4_multicast_mac(&group), out);
            }
        }
        if self.ipv6_enabled() {
            push(ipv6_multicast_mac(&ALL_NODES), out);
            for info in self.iface.ipv6_addresses() {
                push(
                    ipv6_multicast_mac(&solicited_node_multicast(&info.addr)),
                    out,
                );
            }
            for group in self.membership_v6.groups() {
                push(ipv6_multicast_mac(&group), out);
            }
        }
    }

    /// IGMP receive (RFC 2236): a query schedules our responses; another
    /// host's report suppresses ours; a leave is a router's concern.
    fn on_igmp(&mut self, payload: &[u8], now: Duration64) {
        let Some(message) = IgmpMessage::parse(payload) else {
            self.counters.rx_dropped += 1;
            return;
        };
        match message {
            IgmpMessage::MembershipQuery {
                max_resp_deciseconds,
                group,
            } => {
                let target = if group.is_unspecified() {
                    None
                } else {
                    Some(group)
                };
                let max = Duration64::from_nanos(u64::from(max_resp_deciseconds) * 100_000_000);
                self.membership_v4.on_query(target, max, now);
            }
            IgmpMessage::V2Report { group } | IgmpMessage::V1Report { group } => {
                self.membership_v4.on_report_seen(group);
            }
            // A host neither acts on nor forwards another node's Leave.
            IgmpMessage::LeaveGroup { .. } => {}
        }
    }

    /// MLD receive (RFC 3810): a query schedules our responses. MLDv2
    /// has no report suppression, so another host's report is ignored.
    fn on_mld_query(&mut self, body: &[u8], now: Duration64) {
        let Some(query) = MldQuery::parse(body) else {
            self.counters.rx_dropped += 1;
            return;
        };
        let target = if query.is_general() {
            None
        } else {
            Some(query.multicast_address)
        };
        let max = Duration64::from_nanos(u64::from(query.max_response_millis) * 1_000_000);
        self.membership_v6.on_query(target, max, now);
    }

    /// Turn one IPv4 membership report into an IGMP message and send it
    /// (a report to the group, a leave to the all-routers group).
    fn emit_igmp_report(
        &mut self,
        out: &mut StackOutput,
        report: MembershipReport<Ipv4Addr>,
        now: Duration64,
    ) {
        let _ = now;
        let Some((our_v4, _)) = self.iface.ipv4() else {
            return;
        };
        let (message, dest) = match report.reason {
            ReportReason::JoinGroup | ReportReason::QueryResponse => (
                IgmpMessage::V2Report {
                    group: report.group,
                },
                report.group,
            ),
            ReportReason::LeaveGroup => (
                IgmpMessage::LeaveGroup {
                    group: report.group,
                },
                ALL_ROUTERS_V4,
            ),
        };
        let mut body = [0u8; crate::igmp::IGMP_MESSAGE_LEN];
        if message.write(&mut body).is_none() {
            return;
        }
        self.send_igmp(out, our_v4, dest, &body);
    }

    /// Emit an IGMP message to `dest`: TTL 1, Router Alert, straight to
    /// the destination's multicast MAC (no neighbour resolution).
    fn send_igmp(&mut self, out: &mut StackOutput, source: Ipv4Addr, dest: Ipv4Addr, body: &[u8]) {
        if !self.link_up {
            return;
        }
        let mut header = Ipv4Header::new(source, dest, PROTOCOL_IGMP);
        header.ttl = MEMBERSHIP_HOP_LIMIT;
        header.identification = self.next_ident;
        self.next_ident = self.next_ident.wrapping_add(1);
        // The Router Alert option makes the header 24 bytes (IHL = 6).
        let header_len = IPV4_HEADER_LEN + 4;
        let mut packet = vec![0u8; header_len + body.len()];
        if header
            .write_with_router_alert(&mut packet, body.len())
            .is_none()
        {
            return;
        }
        packet[header_len..].copy_from_slice(body);
        self.push_frame(out, ipv4_multicast_mac(&dest), ETHERTYPE_IPV4, &packet);
    }

    /// Turn one IPv6 membership report into an MLDv2 report and send it
    /// to the all-MLDv2-routers group with a Hop-by-Hop Router Alert.
    fn emit_mld_report(
        &mut self,
        out: &mut StackOutput,
        report: MembershipReport<Ipv6Addr>,
        now: Duration64,
    ) {
        // A link-local source is required for MLD (RFC 3810 §5.2.13);
        // `source_for_v6` selects one for the link-local report group.
        let Some(source) = self.source_for_v6(ALL_MLDV2_ROUTERS) else {
            return;
        };
        let record_type = match report.reason {
            ReportReason::JoinGroup => RecordType::ChangeToExclude,
            ReportReason::LeaveGroup => RecordType::ChangeToInclude,
            ReportReason::QueryResponse => RecordType::ModeIsExclude,
        };
        let records = [(record_type, report.group)];
        let mut body = vec![0u8; mld::v2_report_len(records.len())];
        if mld::write_v2_report(&records, &mut body).is_none() {
            return;
        }
        let icmp = IcmpMessage {
            message_type: TYPE_MLDV2_REPORT,
            code: 0,
            body: &body,
        };
        let context = IcmpContext::V6 {
            source,
            destination: ALL_MLDV2_ROUTERS,
        };
        let mut message = vec![0u8; crate::icmp::ICMP_FIXED_HEADER_LEN + body.len()];
        if icmp.write(context, &mut message).is_none() {
            return;
        }
        self.send_ipv6_packet_opt(
            out,
            source,
            ALL_MLDV2_ROUTERS,
            NEXT_HEADER_ICMPV6,
            &message,
            MEMBERSHIP_HOP_LIMIT,
            true,
            Ecn::NotEct,
            TxOffload::None,
            now,
        );
    }
}

/// The transport [`ChecksumMode`] a transmit `offload` implies: a
/// [`TxOffload::PartialChecksum`] frame is serialised with only the
/// pseudo-header partial sum for the device to complete; every other
/// frame carries a complete software checksum.
fn checksum_mode(offload: TxOffload) -> ChecksumMode {
    match offload {
        TxOffload::None => ChecksumMode::Full,
        TxOffload::PartialChecksum { .. } => ChecksumMode::Partial,
        TxOffload::TcpSegment { .. } => ChecksumMode::PartialGso,
    }
}

/// Zero the host bits of `addr` for a `prefix_len`-bit prefix.
fn mask_v4(addr: Ipv4Addr, prefix_len: u8) -> Ipv4Addr {
    if prefix_len == 0 || prefix_len > 32 {
        return Ipv4Addr::UNSPECIFIED;
    }
    let bits = u32::from_be_bytes(addr.octets());
    Ipv4Addr::from((bits & (u32::MAX << (32 - u32::from(prefix_len)))).to_be_bytes())
}

/// The prefix length a DHCP subnet mask (option 1) encodes, or `None`
/// when the mask is not a contiguous run of leading ones (a hole makes it
/// no valid prefix — fail closed rather than guess a length).
fn prefix_len_from_mask(mask: Ipv4Addr) -> Option<u8> {
    let bits = u32::from_be_bytes(mask.octets());
    let ones = bits.leading_ones();
    if bits.count_ones() != ones {
        return None;
    }
    // `ones` is at most 32, so this conversion never truncates.
    u8::try_from(ones).ok()
}

/// Zero the host bits of `addr` for a `prefix_len`-bit prefix.
fn mask_v6(addr: Ipv6Addr, prefix_len: u8) -> Ipv6Addr {
    if prefix_len == 0 || prefix_len > 128 {
        return Ipv6Addr::from([0u8; 16]);
    }
    let bits = u128::from_be_bytes(addr.octets());
    Ipv6Addr::from((bits & (u128::MAX << (128 - u32::from(prefix_len)))).to_be_bytes())
}

/// Write a fixed IPv6 header and payload into `buf` (sized to exactly
/// `IPV6_HEADER_LEN + payload.len()`). The one definition of the IPv6
/// header+payload assembly, shared by the pooled transmit path
/// ([`Stack::pooled_ipv6_packet`]) and the allocating [`ipv6_packet`].
fn write_ipv6_into(buf: &mut [u8], header: &Ipv6Header, payload: &[u8]) -> Option<()> {
    header.write(buf, payload.len())?;
    buf[IPV6_HEADER_LEN..].copy_from_slice(payload);
    Some(())
}

/// Assemble a fixed IPv6 header and payload into one freshly allocated
/// packet buffer (the control-plane path and the test helper; the
/// data-plane transmit path uses [`Stack::pooled_ipv6_packet`]).
fn ipv6_packet(header: &Ipv6Header, payload: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![0u8; IPV6_HEADER_LEN + payload.len()];
    write_ipv6_into(&mut out, header, payload)?;
    Some(out)
}

/// True when a v4 source cannot identify a single sender (RFC 1122
/// §3.2.2: unspecified, multicast, or limited broadcast).
fn v4_source_ambiguous(source: Ipv4Addr) -> bool {
    source.is_unspecified() || source.is_multicast() || source.is_broadcast()
}

/// Derive a stable DHCPv6 IA identifier (IAID) from the interface MAC.
///
/// RFC 8415 §12.1 wants the IAID to persist across restarts of the client
/// on the same interface; deriving it from the low four octets of the
/// stable hardware address gives that persistence without any stored
/// state (the MAC is the same identity SLAAC and the DUID-LL already key
/// on). The value is opaque — it only has to be stable and per-interface.
fn dhcp6_iaid(mac: MacAddress) -> u32 {
    let octets = mac.as_octets();
    u32::from_be_bytes([octets[2], octets[3], octets[4], octets[5]])
}

/// Seed the membership report-jitter generator from the interface MAC,
/// so two hosts on a link pick different report delays (see
/// [`crate::mcast`]).
fn mac_seed(mac: MacAddress) -> u64 {
    let o = mac.as_octets();
    u64::from_be_bytes([0, 0, o[0], o[1], o[2], o[3], o[4], o[5]])
}

/// Test-only ergonomic wrappers that drive the reusable-`StackOutput`
/// engine entry points with a throwaway output and return it owned, so
/// the large unit-test suite reads without per-call scaffolding. Not
/// compiled into any shipping artefact; the production callers use the
/// allocation-free `&mut StackOutput` entry points directly.
#[cfg(test)]
impl Stack {
    pub(crate) fn on_frame_collect(&mut self, frame_bytes: &[u8], now: Duration64) -> StackOutput {
        let mut out = StackOutput::default();
        self.on_frame(frame_bytes, now, &mut out);
        out
    }

    pub(crate) fn on_frame_meta_collect(
        &mut self,
        frame_bytes: &[u8],
        rx: RxMeta,
        now: Duration64,
    ) -> StackOutput {
        let mut out = StackOutput::default();
        self.on_frame_meta(frame_bytes, rx, now, &mut out);
        out
    }

    pub(crate) fn advance_collect(&mut self, now: Duration64) -> StackOutput {
        let mut out = StackOutput::default();
        self.advance(now, &mut out);
        out
    }

    pub(crate) fn send_datagram_collect(
        &mut self,
        dest: IpAddr,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
        now: Duration64,
    ) -> Result<StackOutput, SendError> {
        let mut out = StackOutput::default();
        self.send_datagram(dest, source_port, destination_port, payload, now, &mut out)?;
        Ok(out)
    }

    pub(crate) fn send_echo_request_collect(
        &mut self,
        dest: IpAddr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        now: Duration64,
    ) -> Result<StackOutput, SendError> {
        let mut out = StackOutput::default();
        self.send_echo_request(dest, identifier, sequence, payload, now, &mut out)?;
        Ok(out)
    }

    pub(crate) fn send_tcp_collect(
        &mut self,
        dest: IpAddr,
        meta: &TcpSegmentMeta,
        payload: &[u8],
        gso_size: Option<u16>,
        now: Duration64,
    ) -> Result<StackOutput, SendError> {
        self.send_tcp_ecn_collect(dest, meta, payload, gso_size, Ecn::NotEct, now)
    }

    pub(crate) fn send_tcp_ecn_collect(
        &mut self,
        dest: IpAddr,
        meta: &TcpSegmentMeta,
        payload: &[u8],
        gso_size: Option<u16>,
        ecn: Ecn,
        now: Duration64,
    ) -> Result<StackOutput, SendError> {
        let mut out = StackOutput::default();
        self.send_tcp(dest, meta, payload, gso_size, ecn, now, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
#[path = "stack_tests.rs"]
mod tests;
