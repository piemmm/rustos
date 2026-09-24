//! The socket service: the origin-keyed socket table and the
//! capability-checked dispatcher that serves the `netsock-v1` control
//! plane (`plans/NETWORK.md` N4b datagrams, N5c streams).
//!
//! Sockets are entirely stack/userland state — the kernel owns no socket
//! object. This module is the pure engine of that service: it owns the
//! socket table (datagram *and* stream sockets in one id space, exactly as
//! a POSIX file-descriptor table holds every kind), decides port
//! assignment and delivery, drives the [`Netstack`] interface table to
//! originate datagrams and TCP segments, and owns each connection's
//! [`Tcb`]. All I/O (the endpoint recv/reply, the delivery `ipc_send`, the
//! CSPRNG draw) is the thin `Run`-binary glue's job; the engine takes its
//! entropy through an injected closure and returns the frames and
//! deliveries for the glue to move, so it stays host-testable.
//!
//! # Security
//!
//! Every request is capability-checked against the caller's
//! kernel-attested [`tairix_abi::Origin`] **before any state is touched**
//! (`CAP_NET`, fail closed), and every socket is keyed to the creating
//! principal's unforgeable [`ProcId`]: a handle is meaningless — and
//! unusable — to any other principal even if observed. Ports bind
//! globally uniquely (no silent reuse), ephemeral ports are drawn from
//! the kernel CSPRNG, and both the per-principal share and the stack-wide
//! budget are bounded, failing closed with [`Errno::LimitExceeded`] when
//! exhausted. A stream's per-connection send/receive/reassembly buffers
//! are the bounded [`TcpConfig`] capacities, so a hostile peer cannot grow
//! memory.
//!
//! # Budget
//!
//! What bounds the table is **bytes of socket memory**, not a count of
//! sockets. A count cannot bound the resource actually at stake: the same
//! number of sockets is a few kilobytes when they are idle and tens of
//! megabytes when they are fully buffered, so any count is either a
//! refusal while the memory is free or an overrun while it is not. The
//! byte budget carries many idle sockets or few busy ones, according to
//! what the workload really is.
//!
//! The budget is a *capacity*, derived rather than chosen — an eighth of
//! the machine's usable physical RAM, which an administrator may override
//! with `net.sockets.mem` — and each principal may hold a sixteenth share
//! of it, so there is always room for sixteen principals at their full
//! share. Both arrive on the delivered [`NetworkSettings`]: the stack is
//! the network-parsing sandbox and can read neither the machine nor
//! `system.conf` itself.
//!
//! **What a socket is charged is its commitment, not its occupancy.**
//! Charging what a socket holds today would bound nothing: the window
//! ceilings are handed out long before the data that fills them arrives,
//! so a principal could open any number of quiet connections — each
//! already entitled to a quarter of its share — and the stack would have
//! nothing left to refuse when they all filled. Admission therefore
//! *reserves*, and each new connection's send and receive ceilings are
//! sized from what is left of its owner's share **and** of the stack's
//! budget, whichever binds. Both, because a share bounds one principal
//! and sixteen shares come to the budget, but a share does not shrink as
//! others fill. The ceilings handed out consequently cannot sum past
//! either bound and no buffer has to be clawed back later; what stays
//! fixed is the *refusal*, [`Errno::LimitExceeded`] and an audited event.
//!
//! Because every term is a bound rather than a measurement, a socket's
//! charge moves only when its kind or its ceilings do — at open, connect,
//! and `listen` — and each move is applied as a difference, so no decision
//! walks the table.
//!
//! A listener is priced when it starts listening, because only remote
//! peers decide how much it comes to hold: its bounded half-open backlog
//! plus its queue of completed connections at the window its template
//! grants each. The queue's depth is what the budget sets; the backlog is
//! the SYN-flood brake and is *charged* rather than sized, because a
//! defence does not shrink because memory is tight. A share too small to
//! hold it refuses `listen` outright — a stack that can connect but not
//! serve, said plainly rather than by handing back a listener with no
//! brake.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::net::{
    encode_bind_reply, encode_send_reply, encode_socket_reply, ShutdownHow, SocketAddr,
    SocketDatagram, SocketEcho, SocketId, SocketLinkEvent, SocketRequest, SocketStreamEvent,
    SocketType, StreamCloseReason, SOCKET_MAX_DATAGRAM, SOCKET_PRIVILEGED_PORT_MAX,
};
use tairix_abi::net_ipc::{
    address_parts, ip_from_parts, NetAddrFamily, NetSockProto, NetSockState, NetSocketRecord,
    NetStackDefenceCounters, NetworkSettings, CONNECTION_BUFFER_SHARE_DIVISOR, IF_NAME_LEN,
};
use tairix_abi::origin::ProcId;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::{CapabilityId, Duration64, Errno};
use tairix_collections::HashMap;
use tairix_hash::{BuildSipHash13, HashSeed};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use tairix_net::addr::{Ecn, IpAddr, Ipv4Addr};
use tairix_net::checksum::Pseudo;
use tairix_net::stack::StackEvent;
use tairix_net::tcp::conn::{ResetReason, State, Tcb, TcpConfig};
use tairix_net::tcp::listen::{CookieSecret, ListenConfig, Listener, ListenerStats, Peer};
use tairix_net::tcp::{TcpSegment, TcpSegmentMeta};

use crate::events;
use crate::iface::{FrameBatch, Netstack, PublishedLink};
use crate::service::Caller;

/// One outbound TCP segment drained from a connection: its header, its
/// owned payload, the segmentation-offload super-segment size
/// ([`OutSegment::gso_size`](tairix_net::tcp::conn::OutSegment::gso_size)) —
/// `Some(mss)` for an over-size super-segment the device splits, `None`
/// for an ordinary segment — and the IP-layer ECN codepoint
/// ([`OutSegment::ecn`](tairix_net::tcp::conn::OutSegment::ecn)) the
/// engine asks be stamped on the datagram (RFC 3168 §5).
type OutSeg = (TcpSegmentMeta, Vec<u8>, Option<u16>, Ecn);

/// An [`OutSeg`] tagged with the peer address it must be routed to — the
/// shape a listener's `advance` yields (each retransmitted SYN-ACK may be
/// destined for a different peer).
type PeerOutSeg = (IpAddr, TcpSegmentMeta, Vec<u8>, Option<u16>, Ecn);

/// Largest number of multicast groups one socket may join at once.
pub const MAX_GROUPS_PER_SOCKET: usize = 16;

/// First port of the IANA dynamic/ephemeral range (RFC 6335 §6).
const EPHEMERAL_MIN: u16 = 49_152;
/// Last port of the ephemeral range.
const EPHEMERAL_MAX: u16 = 65_535;
/// Bounded number of CSPRNG draws attempted to find a free ephemeral port
/// before failing closed with [`Errno::AddressInUse`].
const EPHEMERAL_TRIES: u32 = 128;

/// Per-socket state of a connectionless datagram socket.
struct DatagramState {
    /// Connected default peer, if [`SocketRequest::Connect`] was called.
    peer: Option<SocketAddr>,
    /// Multicast groups this socket joined (for leave-on-close).
    groups: Vec<[u8; 16]>,
    /// The links this socket has been told its memberships ride, as of each
    /// one's epoch then.
    told: Vec<PublishedLink>,
    /// Whether it is in the service's `behind` list, so it is listed once.
    listed: bool,
}

/// Per-socket state of an ICMP/`ICMPv6` echo socket (the `ping` path).
///
/// The socket's stack-assigned ICMP *identifier* lives in the entry's
/// `local_port` field (unique in its family's port space, so a reply
/// can never be routed to the wrong socket). Only the connected default
/// peer, if any, is transport-specific state.
struct EchoState {
    /// Connected default peer address, if [`SocketRequest::Connect`] was
    /// called (the port is always zero — ICMP has none).
    peer: Option<SocketAddr>,
}

/// Per-socket state of a connection-oriented stream socket, once
/// [`SocketRequest::Connect`] has established a connection. Before that a
/// stream socket carries no connection (`Proto::Stream(None)`).
struct StreamConn {
    /// The connection's transmission control block (the RFC 9293 engine).
    tcb: Tcb,
    /// The fixed peer of the connection.
    peer: SocketAddr,
    /// The egress interface the connection is bound to for its life.
    iface: [u8; IF_NAME_LEN],
    /// Which one-shot client lifecycle events have already been delivered,
    /// so `Connected` and `Closed` are each sent exactly once and no event
    /// follows `Closed`.
    notified: Notified,
    /// Whether the client has issued `close`: the connection is being
    /// torn down in the background and is reaped once fully closed. No
    /// further events are delivered (the client is gone).
    client_closed: bool,
    /// Whether the client has shut the *receive* direction down. Received
    /// bytes are still drained from the connection — so the advertised
    /// window stays open and a still-sending peer is never stalled — but
    /// they are discarded instead of delivered. TCP has no wire signal for
    /// this, so the peer is not told.
    read_shutdown: bool,
    /// Whether this connection has been claimed by the client. A connection
    /// opened actively (via [`SocketRequest::Connect`]) is accepted at
    /// birth; a connection produced passively by a [`Listener`] starts
    /// **unaccepted** and delivers no client events until an
    /// [`SocketRequest::Accept`] claims it (its received bytes buffer in the
    /// bounded [`Tcb`] meanwhile), so the client never sees data for a
    /// connection it has not yet taken.
    accepted: bool,
}

/// The one-shot client-facing lifecycle events already delivered for a
/// connection. The lifecycle is monotonic: `Nothing` → `Connected` →
/// `Closed`, so each event is delivered once and none follows `Closed`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Notified {
    /// No lifecycle event delivered yet.
    Nothing,
    /// The one-shot `Connected` has been delivered.
    Connected,
    /// The one-shot `Closed` has been delivered; no event follows.
    Closed,
}

/// The transport-specific state of one socket.
enum Proto {
    /// A connectionless UDP datagram socket.
    Datagram(DatagramState),
    /// A connection-oriented TCP stream socket; `None` until connected.
    /// The connection (which carries the sizeable [`Tcb`]) is boxed so a
    /// datagram socket's table entry stays small.
    Stream(Option<Box<StreamConn>>),
    /// A passive TCP listener demultiplexing inbound connections on the
    /// socket's bound local port. Each completed handshake becomes a
    /// separate child stream socket in the table (`Proto::Stream`), keyed
    /// to the same principal; the [`Listener`] carries the SYN-flood
    /// defence (bounded half-open backlog, stateless SYN cookies on
    /// overflow) and is boxed so a non-listening entry stays small.
    Listen(Box<Listener>),
    /// An ICMP/`ICMPv6` echo socket (the `ping` path).
    Echo(EchoState),
}

/// One open socket, owned by exactly one principal.
struct SocketEntry {
    /// Server-assigned handle, unique among all live sockets.
    id: SocketId,
    /// The unforgeable process instance that opened it.
    owner: ProcId,
    /// The owning process's pid, kept for the socket-listing diagnostic
    /// so a listing names a human process id rather than the opaque
    /// instance token. Never used for authority (that is `owner`).
    owner_pid: u64,
    /// The client async port inbound datagrams/stream events go to.
    deliver_port: u64,
    /// Address family of the socket.
    family: NetAddrFamily,
    /// Bound local address; unspecified (all-zero) means "any".
    local_addr: [u8; 16],
    /// Bound local port; `0` means unbound.
    local_port: u16,
    /// Bytes this socket is charged against its owner's share and the
    /// stack-wide budget, as of the last time its state changed. Held per
    /// socket so a change costs a difference rather than a re-sum of the
    /// table.
    accounted_bytes: u64,
    /// The transport-specific state.
    proto: Proto,
}

/// What one principal holds: how many sockets, and the bytes they account
/// for.
///
/// The count is the row's own refcount — it is what says when the last of
/// a principal's sockets has gone, exactly as [`PortSlot::holders`] does
/// for a port — while the byte total is what the principal's share is
/// checked against.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct OwnerUsage {
    sockets: u32,
    bytes: u64,
}

/// Bytes one socket costs the table and its indices whatever transport it
/// carries: its table slot, plus one row in each of the four indices.
///
/// Derived from the types rather than estimated, so adding a field to an
/// entry or an index changes the charge with it. This is a *per-socket*
/// attribution, which is what a per-principal share needs; the
/// containers' spare capacity is nobody's socket and is measured against
/// the stack as a whole ([`SocketService::structural_slack`]).
pub(crate) const PER_SOCKET_OVERHEAD: u64 = widen_const(
    size_of::<SocketEntry>()
        + index_row::<SocketId, usize>()
        + index_row::<ConnKey, SocketId>()
        + index_row::<u16, PortSlot>()
        + index_row::<ProcId, OwnerUsage>(),
);

/// Bytes one row of a [`HashMap`] occupies: its `(key, value)` slot and
/// its control byte.
const fn index_row<K, V>() -> usize {
    size_of::<(K, V)>() + 1
}

impl SocketEntry {
    /// Derive this socket's read-only [`NetSocketRecord`] for the listing
    /// query. A datagram socket reports `UNCONN` until `connect` sets a
    /// default peer; a stream socket reports its RFC 9293 state and its
    /// connection's peer and queue depths; a listener reports `LISTEN`.
    fn to_record(&self) -> NetSocketRecord {
        let (proto, state, peer_addr, peer_port, recv_q, send_q) = match &self.proto {
            Proto::Datagram(datagram) => match datagram.peer {
                Some(peer) => (
                    NetSockProto::Udp,
                    NetSockState::Established,
                    peer.addr,
                    peer.port,
                    0,
                    0,
                ),
                None => (
                    NetSockProto::Udp,
                    NetSockState::Unconnected,
                    [0u8; 16],
                    0,
                    0,
                    0,
                ),
            },
            Proto::Stream(None) => (NetSockProto::Tcp, NetSockState::Closed, [0u8; 16], 0, 0, 0),
            Proto::Stream(Some(conn)) => (
                NetSockProto::Tcp,
                map_tcp_state(conn.tcb.state()),
                conn.peer.addr,
                conn.peer.port,
                conn.tcb.recv_len() as u64,
                conn.tcb.send_queued() as u64,
            ),
            Proto::Listen(_) => (NetSockProto::Tcp, NetSockState::Listen, [0u8; 16], 0, 0, 0),
            Proto::Echo(echo) => {
                let proto = match self.family {
                    NetAddrFamily::V4 => NetSockProto::Icmp,
                    NetAddrFamily::V6 => NetSockProto::Icmpv6,
                };
                match echo.peer {
                    Some(peer) => (proto, NetSockState::Established, peer.addr, 0, 0, 0),
                    None => (proto, NetSockState::Unconnected, [0u8; 16], 0, 0, 0),
                }
            }
        };
        NetSocketRecord {
            proto,
            state,
            family: self.family,
            local_addr: self.local_addr,
            local_port: self.local_port,
            peer_addr,
            peer_port,
            owner: self.owner_pid,
            recv_q,
            send_q,
        }
    }
}

/// Map an RFC 9293 [`State`] onto the ABI [`NetSockState`] (a 1:1
/// vocabulary; the ABI adds only the UDP `Unconnected` value the state
/// machine has no analogue for).
fn map_tcp_state(state: State) -> NetSockState {
    match state {
        State::Closed => NetSockState::Closed,
        State::Listen => NetSockState::Listen,
        State::SynSent => NetSockState::SynSent,
        State::SynReceived => NetSockState::SynReceived,
        State::Established => NetSockState::Established,
        State::FinWait1 => NetSockState::FinWait1,
        State::FinWait2 => NetSockState::FinWait2,
        State::CloseWait => NetSockState::CloseWait,
        State::Closing => NetSockState::Closing,
        State::LastAck => NetSockState::LastAck,
        State::TimeWait => NetSockState::TimeWait,
    }
}

/// The outcome of serving one control-plane request: the encoded reply
/// (already written into the caller's `response` buffer) and any frames to
/// transmit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SocketReply {
    /// Number of reply bytes written into `response`.
    pub len: usize,
    /// Frames to queue onto each named interface's TX ring (empty for a
    /// request that transmits nothing).
    pub tx: FrameBatch,
    /// Stream events to deliver to clients' async ports as a *result of
    /// serving the request itself* (the buffered `Connected`/`Data` a
    /// newly [`Accept`](SocketRequest::Accept)ed connection already holds).
    /// Empty for every other operation; the glue `ipc_send`s each.
    pub deliveries: Vec<Delivery>,
    /// A principal this request gave its first socket. The glue watches it
    /// for exit, so the sockets it leaves behind are reclaimed rather than
    /// held — ports and all — for the rest of the boot.
    pub watch: Option<ProcId>,
}

/// One message to deliver to a socket's client: the async port to
/// `ipc_send` it to, and the encoded [`SocketDatagram`] or
/// [`SocketStreamEvent`] payload.
#[derive(Debug, PartialEq, Eq)]
pub struct Delivery {
    /// The client async port the message is sent to.
    pub deliver_port: u64,
    /// The encoded delivery frame.
    pub datagram: Vec<u8>,
}

/// Frames to transmit and messages to deliver from driving a connection —
/// the outcome of an inbound TCP segment or a stream timer tick.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StreamIo {
    /// Frames to queue onto each named interface's TX ring.
    pub tx: FrameBatch,
    /// Stream events to deliver to clients' async ports.
    pub deliveries: Vec<Delivery>,
    /// Set on the pass where a listener's half-open backlog first
    /// overflowed and it fell back to stateless SYN cookies — the
    /// SYN-flood brake engaging, which the service audits once per
    /// listener. Reported rather than logged here so the engine keeps
    /// returning facts and the caller that holds the audit sink decides,
    /// and set only on the transition so an actual flood cannot turn the
    /// audit log into its own amplifier.
    pub cookies_engaged: bool,
}

/// The four-tuple that identifies an established stream, and the key its
/// demux index is looked up by.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
struct ConnKey {
    family: NetAddrFamily,
    local_port: u16,
    peer_addr: [u8; 16],
    peer_port: u16,
}

/// What holds one local port.
///
/// `holders` counts every socket carrying the port — the one that bound it
/// and any stream children that inherited it from a listener — so the
/// globally-unique-port rule reads the same as a scan of the whole table
/// would. `demux` names the socket inbound traffic for the port goes to,
/// which is never an established stream: those are reached by their full
/// four-tuple instead, so a listener and its children do not contend.
#[derive(Copy, Clone, Default)]
struct PortSlot {
    holders: u32,
    demux: Option<SocketId>,
}

/// A local port in one family's port space. A socket has one family and never
/// receives the other's traffic, so the two spaces are disjoint and a service
/// holds a port in both.
type PortKey = (NetAddrFamily, u16);

/// The socket table and its dispatcher.
///
/// # Lookup
///
/// The table is indexed, not scanned. Its capacity scales with the
/// machine, so every lookup on a path a remote peer can drive has to be
/// independent of how many sockets exist: a scan would let one principal's
/// sockets slow every other principal's traffic, which is a denial of
/// service rather than merely slow. The indices are keyed with the
/// process's `SipHash` key, because a peer chooses the address and port half
/// of a connection key and an unkeyed hash would be collision-floodable —
/// the same O(n) defect by another route.
pub struct SocketService {
    sockets: Vec<SocketEntry>,
    /// Handle to its position in `sockets` — the only index holding a
    /// position, so a `swap_remove` repairs one row rather than many.
    by_id: HashMap<SocketId, usize, BuildSipHash13>,
    /// Established stream four-tuple to its handle.
    by_conn: HashMap<ConnKey, SocketId, BuildSipHash13>,
    /// Local port to what holds it.
    by_port: HashMap<PortKey, PortSlot, BuildSipHash13>,
    /// What each owning principal holds, for the per-principal share.
    owned: HashMap<ProcId, OwnerUsage, BuildSipHash13>,
    /// Bytes every live socket accounts for, summed. The stack-wide budget
    /// is checked against this plus the containers' own spare capacity.
    bytes_total: u64,
    /// Rolling handle allocator; the next candidate id, advanced past any
    /// live collision so a delivered message can never alias a reused id.
    next_id: SocketId,
    /// Local ports assigned since the table was created — a witness that
    /// the set of bound ports may have changed, so the broadcast-consumer
    /// ports are republished without any operation having to say so.
    port_assignments: u64,
    /// Member sockets whose told links may lag the published ones.
    behind: Vec<SocketId>,
    /// The link epoch every member socket was last marked behind for.
    links_seen: u64,
    /// Connection-defence totals of listeners that have since closed,
    /// folded in as each one is dropped. Without this the stack-wide
    /// counters would fall when a listener closes, so a flood that ended
    /// with the listening socket closing would vanish from the count
    /// instead of staying visible.
    retired_defence: ListenerStats,
}

impl SocketService {
    /// An empty socket table whose indices hash under `hash_key`.
    ///
    /// The key is the process's published one. It is an argument rather
    /// than read here so the table cannot silently fall back to an
    /// unkeyed hash: a peer picks the address and port half of every
    /// connection key, so the caller states what it is hashing under.
    #[must_use]
    pub fn new(hash_key: HashSeed) -> Self {
        let hasher = BuildSipHash13::with_seed(hash_key);
        Self {
            sockets: Vec::new(),
            by_id: HashMap::with_hasher(hasher),
            by_conn: HashMap::with_hasher(hasher),
            by_port: HashMap::with_hasher(hasher),
            owned: HashMap::with_hasher(hasher),
            bytes_total: 0,
            next_id: 0,
            port_assignments: 0,
            behind: Vec::new(),
            links_seen: 0,
            retired_defence: ListenerStats::default(),
        }
    }

    /// Number of open sockets across all principals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sockets.len()
    }

    /// The stack-wide TCP connection-defence totals (`stats:net/stack/…`):
    /// every live listener's counters plus those of the listeners that have
    /// closed, so each figure is monotonic over the life of the boot and a
    /// flood stays visible after its target socket goes away.
    #[must_use]
    pub fn defence_counters(&self) -> NetStackDefenceCounters {
        let total = self
            .sockets
            .iter()
            .filter_map(|entry| match &entry.proto {
                Proto::Listen(listener) => Some(listener.stats()),
                _ => None,
            })
            .fold(self.retired_defence, fold_defence);
        NetStackDefenceCounters {
            half_open_started: total.half_open_started,
            syn_cookies_sent: total.cookies_sent,
            syn_cookies_accepted: total.cookies_accepted,
            syn_cookies_rejected: total.cookies_rejected,
            accepted: total.accepted,
            accept_overflow: total.accept_overflow,
            half_open_expired: total.half_open_expired,
            resets_sent: total.resets_sent,
        }
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
    }

    /// Snapshot the open sockets as wire records for the `ss`/`netstat`
    /// listing query, starting at `offset` and returning at most `limit`.
    ///
    /// A read-only diagnostic: it derives each socket's protocol, state,
    /// local/peer addresses, owning pid, and queue depths without touching
    /// any connection. Serving it is gated on `CAP_SYSINFO_GLOBAL` at the
    /// sysinfo broker; the table order is stable within a page.
    #[must_use]
    pub fn socket_records(&self, offset: u32, limit: u16) -> Vec<NetSocketRecord> {
        self.sockets
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(SocketEntry::to_record)
            .collect()
    }

    /// Serve one `netsock-v1` control-plane request on behalf of `caller`.
    ///
    /// Enforces `CAP_NET` against the caller's attested origin **before
    /// any state is touched**, decodes the [`SocketRequest`], routes it by
    /// the target socket's transport, writes the encoded reply into
    /// `response`, and returns the reply length plus any frames to
    /// transmit. Fails closed with a typed [`Errno`] on any malformed
    /// frame, missing capability, unowned handle, full quota, or refused
    /// operation.
    ///
    /// # Errors
    ///
    /// See the per-operation helpers; every refusal is typed and audited.
    #[allow(clippy::too_many_arguments)]
    pub fn serve(
        &mut self,
        interfaces: &mut Netstack,
        caller: &Caller,
        audit: &dyn Sink,
        entropy: &mut dyn FnMut() -> u32,
        request: &[u8],
        response: &mut [u8],
        now: Duration64,
    ) -> Result<SocketReply, Errno> {
        let decoded = match SocketRequest::from_bytes(request) {
            Ok(decoded) => decoded,
            Err(err) => {
                emit(
                    audit,
                    Level::Warn,
                    events::SOCKET_MALFORMED,
                    "socket request rejected: frame decode failed",
                    &[],
                );
                return Err(err);
            }
        };
        if !caller.capabilities().holds(CapabilityId::NET) {
            emit(
                audit,
                Level::Warn,
                events::SOCKET_DENIED,
                "socket request denied: caller lacks CAP_NET",
                &[op_field(&decoded)],
            );
            return Err(Errno::PermissionDenied);
        }
        let owner = caller.origin().proc_id();
        // Two structural witnesses of a change to the broadcast-consumer
        // ports, so no operation has to remember to announce one: every port
        // is assigned through `assign_port`, and every socket removal
        // shortens the table. A fresh socket lengthens it with no port yet,
        // which the set comparison below then finds unchanged.
        let assignments = self.port_assignments;
        let population = self.sockets.len();
        let held_any = self.owned.get(&owner).is_some();
        let mut result = self.dispatch(
            interfaces, caller, audit, entropy, decoded, owner, response, now,
        );
        if let Ok(reply) = &mut result {
            if !held_any && self.owned.get(&owner).is_some() {
                reply.watch = Some(owner);
            }
        }
        if self.port_assignments != assignments || self.sockets.len() != population {
            interfaces.publish_datagram_ports(broadcast_consumer_ports(&self.sockets));
        }
        #[cfg(test)]
        self.assert_indices_agree();
        result
    }

    /// The one dispatch arm per socket operation.
    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &mut self,
        interfaces: &mut Netstack,
        caller: &Caller,
        audit: &dyn Sink,
        entropy: &mut dyn FnMut() -> u32,
        decoded: SocketRequest<'_>,
        owner: ProcId,
        response: &mut [u8],
        now: Duration64,
    ) -> Result<SocketReply, Errno> {
        if claims_multicast_dns(&decoded) && !is_discovery(caller) {
            emit(
                audit,
                Level::Warn,
                events::SOCKET_DENIED,
                "socket request denied: the multicast DNS port and groups are reserved to the discovery service",
                &[op_field(&decoded)],
            );
            return Err(Errno::PermissionDenied);
        }
        match decoded {
            SocketRequest::Socket {
                family,
                sock_type,
                deliver_port,
            } => self.open(
                interfaces,
                caller,
                audit,
                family,
                sock_type,
                deliver_port,
                response,
            ),
            SocketRequest::Bind { socket, local } => {
                // Claiming a specific privileged (well-known) local port is
                // a further gate beyond CAP_NET: an unprivileged process
                // must not squat a low port and impersonate a system
                // service. A `0` (ephemeral) request is never privileged.
                if local.port != 0
                    && local.port <= SOCKET_PRIVILEGED_PORT_MAX
                    && !caller
                        .capabilities()
                        .holds(CapabilityId::NET_BIND_PRIVILEGED)
                {
                    emit(
                        audit,
                        Level::Warn,
                        events::SOCKET_DENIED,
                        "socket bind denied: privileged port needs CAP_NET_BIND_PRIVILEGED",
                        &[op_field(&decoded)],
                    );
                    return Err(Errno::PermissionDenied);
                }
                self.bind(interfaces, entropy, owner, socket, local, response)
            }
            SocketRequest::Connect { socket, peer } => {
                self.connect(interfaces, entropy, owner, socket, peer, now, response)
            }
            SocketRequest::Send {
                socket,
                dest,
                interface,
                payload,
            } => self.send(
                interfaces, entropy, audit, owner, socket, dest, interface, payload, now, response,
            ),
            SocketRequest::Close { socket } => self.close(interfaces, owner, socket, now, response),
            SocketRequest::Shutdown { socket, how } => {
                self.shutdown(interfaces, owner, socket, how, now, response)
            }
            SocketRequest::JoinMulticast { socket, group } => {
                self.join(interfaces, owner, socket, group, now, response)
            }
            SocketRequest::LeaveMulticast { socket, group } => {
                self.leave(interfaces, owner, socket, group, now, response)
            }
            SocketRequest::Listen { socket } => {
                self.listen(interfaces, audit, owner, socket, response)
            }
            SocketRequest::Accept {
                socket,
                deliver_port,
            } => self.accept_socket(
                interfaces,
                audit,
                owner,
                socket,
                deliver_port,
                now,
                response,
            ),
            SocketRequest::SendEcho {
                socket,
                dest,
                sequence,
                payload,
            } => self.send_echo(
                interfaces, entropy, audit, owner, socket, dest, sequence, payload, now, response,
            ),
        }
    }

    /// Open a socket of the requested transport, accounting it against the
    /// caller's quota and the global table. An ICMP-echo (raw) socket is a
    /// further gate beyond `CAP_NET`: forging or observing ICMP is raw-frame
    /// authority, so opening one requires `CAP_NET_RAW` (fail closed, audited).
    #[allow(clippy::too_many_arguments)]
    fn open(
        &mut self,
        interfaces: &Netstack,
        caller: &Caller,
        audit: &dyn Sink,
        family: NetAddrFamily,
        sock_type: SocketType,
        deliver_port: u64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        // A family the stack-wide policy has disabled (`net.ipv4.enabled`
        // / `net.ipv6.enabled`) binds no address and answers nothing, so
        // a socket for it can never carry traffic: refuse the open up
        // front (fail closed, audited) rather than hand back a dead
        // handle.
        let settings = interfaces.settings();
        let family_enabled = match family {
            NetAddrFamily::V4 => settings.ipv4_enabled,
            NetAddrFamily::V6 => settings.ipv6_enabled,
        };
        if !family_enabled {
            return refuse(
                audit,
                "socket open denied: address family administratively disabled",
                Errno::NotSupported,
            );
        }
        if sock_type == SocketType::IcmpEcho && !caller.capabilities().holds(CapabilityId::NET_RAW)
        {
            return refuse(
                audit,
                "socket open denied: ICMP echo socket needs CAP_NET_RAW",
                Errno::PermissionDenied,
            );
        }
        let owner = caller.origin().proc_id();
        let owner_pid = caller.origin().pid();
        if deliver_port == 0 {
            return refuse(
                audit,
                "socket open refused: zero delivery port",
                Errno::OutOfRange,
            );
        }
        if self.budget_exhausted(&settings, owner) {
            return refuse(
                audit,
                "socket open refused: socket memory budget exhausted",
                Errno::LimitExceeded,
            );
        }
        let proto = match sock_type {
            SocketType::IcmpEcho => Proto::Echo(EchoState { peer: None }),
            SocketType::Datagram => Proto::Datagram(DatagramState {
                peer: None,
                groups: Vec::new(),
                told: Vec::new(),
                listed: false,
            }),
            SocketType::Stream => Proto::Stream(None),
        };
        let id = self.alloc_id();
        self.insert_entry(SocketEntry {
            id,
            owner,
            owner_pid,
            deliver_port,
            family,
            local_addr: [0u8; 16],
            local_port: 0,
            accounted_bytes: 0,
            proto,
        })?;
        emit(
            audit,
            Level::Info,
            events::SOCKET_OPENED,
            "socket opened",
            &[],
        );
        let len = encode_socket_reply(Ok(id), response)?;
        Ok(SocketReply {
            len,
            tx: Vec::new(),
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Bind a socket to a local address and port, drawing an ephemeral
    /// port when the request asks for `0`. Applies to both transports.
    fn bind(
        &mut self,
        interfaces: &Netstack,
        entropy: &mut dyn FnMut() -> u32,
        owner: ProcId,
        socket: SocketId,
        local: SocketAddr,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        if local.family != self.sockets[index].family {
            return Err(Errno::OutOfRange);
        }
        // A socket already carrying a local port is not re-bound. Moving
        // one would strand the port it holds and, for a connected socket,
        // silently cut its inbound traffic adrift from the four-tuple it
        // is reached by.
        if self.sockets[index].local_port != 0 {
            return Err(Errno::AlreadyExists);
        }
        if local.addr != [0u8; 16] && !interfaces.has_local_address(local.family, local.addr) {
            return Err(Errno::AddressUnavailable);
        }
        self.reserve_index_rows()?;
        let port = self.assign_port(entropy, local.family, local.port)?;
        self.unindex_entry(index);
        let entry = &mut self.sockets[index];
        entry.local_addr = local.addr;
        entry.local_port = port;
        self.index_entry(index);
        let len = encode_bind_reply(Ok(port), response)?;
        Ok(SocketReply {
            len,
            tx: Vec::new(),
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Set a datagram socket's default peer, or actively open a stream
    /// connection to `peer`.
    #[allow(clippy::too_many_arguments)]
    fn connect(
        &mut self,
        interfaces: &mut Netstack,
        entropy: &mut dyn FnMut() -> u32,
        owner: ProcId,
        socket: SocketId,
        peer: SocketAddr,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        if peer.family != self.sockets[index].family {
            return Err(Errno::OutOfRange);
        }
        match self.sockets[index].proto {
            Proto::Datagram(_) => {
                self.ensure_local_port(entropy, index)?;
                if let Proto::Datagram(dg) = &mut self.sockets[index].proto {
                    dg.peer = Some(peer);
                }
                status_reply(response)
            }
            Proto::Stream(Some(_)) => Err(Errno::AlreadyExists),
            // A passive listener cannot actively open a connection: it is
            // in the wrong state for a connect, not a duplicate of one.
            Proto::Listen(_) => Err(Errno::OutOfRange),
            Proto::Stream(None) => {
                self.connect_stream(interfaces, entropy, index, peer, now, response)
            }
            Proto::Echo(_) => {
                // An echo socket's default peer is an address only; ICMP has
                // no port, so a non-zero port is a malformed connect.
                if peer.port != 0 {
                    return Err(Errno::OutOfRange);
                }
                // Assign the stack-owned ICMP identifier now (its lifetime
                // is the socket's), so it is stable across every send.
                self.ensure_local_port(entropy, index)?;
                if let Proto::Echo(echo) = &mut self.sockets[index].proto {
                    echo.peer = Some(peer);
                }
                status_reply(response)
            }
        }
    }

    /// Send a datagram, or enqueue bytes onto a stream's send buffer.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        interfaces: &mut Netstack,
        entropy: &mut dyn FnMut() -> u32,
        audit: &dyn Sink,
        owner: ProcId,
        socket: SocketId,
        dest: Option<SocketAddr>,
        interface: Option<[u8; IF_NAME_LEN]>,
        payload: &[u8],
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        match self.sockets[index].proto {
            Proto::Datagram(_) => self.send_datagram(
                interfaces, entropy, audit, index, dest, interface, payload, now, response,
            ),
            Proto::Stream(_) => {
                // A connected stream has no per-datagram destination, and
                // is bound to its egress for life.
                if dest.is_some() || interface.is_some() {
                    return Err(Errno::OutOfRange);
                }
                self.send_stream(interfaces, index, payload, now, response)
            }
            // A listening socket is passive: it originates no data. The
            // client sends on the accepted child sockets instead.
            Proto::Listen(_) => Err(Errno::NotConnected),
            // An echo socket sends only through the dedicated `SendEcho`
            // operation (which carries the sequence number); a plain
            // datagram `send` on it is malformed.
            Proto::Echo(_) => Err(Errno::OutOfRange),
        }
    }

    /// Close a socket. A datagram socket is released at once (leaving its
    /// groups); a connected stream begins an orderly teardown (FIN) and is
    /// reaped in the background once fully closed.
    fn close(
        &mut self,
        interfaces: &mut Netstack,
        owner: ProcId,
        socket: SocketId,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        let mut tx = FrameBatch::new();
        if matches!(self.sockets[index].proto, Proto::Stream(Some(_))) {
            if let Proto::Stream(Some(conn)) = &mut self.sockets[index].proto {
                conn.client_closed = true;
                let _ = conn.tcb.close(now);
            }
            // The connection lingers until teardown completes (the FIN is
            // retransmitted and TIME-WAIT observed in the background); it
            // is reaped once fully closed.
            tx = self.pump_stream(interfaces, index, now);
            self.reap_if_done(index);
        } else if matches!(self.sockets[index].proto, Proto::Listen(_)) {
            // Closing a listener drops it and abandons any connection it had
            // completed but the client never accepted (an unclaimed child on
            // the same port): the client is walking away from the port, so
            // those connections have no owner to serve them.
            let family = self.sockets[index].family;
            let port = self.sockets[index].local_port;
            let listener_owner = self.sockets[index].owner;
            // Keep its defence totals: the stack-wide counters must not
            // fall when a listener goes away.
            if let Proto::Listen(listener) = &self.sockets[index].proto {
                self.retired_defence = fold_defence(self.retired_defence, listener.stats());
            }
            self.remove_at(index);
            // Highest position first, so each `swap_remove` only ever moves
            // an entry from beyond the one being dropped and no unvisited
            // match can be carried over a position already passed. A scan
            // is right here where it is wrong on the demux path above: this
            // is the owner closing its own listener, not something a remote
            // peer can drive.
            let abandoned: Vec<usize> = self
                .sockets
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.owner == listener_owner
                        && e.family == family
                        && e.local_port == port
                        && matches!(&e.proto, Proto::Stream(Some(c)) if !c.accepted)
                })
                .map(|(i, _)| i)
                .collect();
            for position in abandoned.into_iter().rev() {
                self.remove_at(position);
            }
        } else {
            let family = self.sockets[index].family;
            let groups = match &mut self.sockets[index].proto {
                Proto::Datagram(dg) => core::mem::take(&mut dg.groups),
                _ => Vec::new(),
            };
            for group in &groups {
                let ip = ip_from_parts(family, *group);
                tx.extend(interfaces.leave_multicast_all(ip, now));
            }
            self.remove_at(index);
        }
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Half-close one or both directions of a connected stream socket,
    /// keeping the handle (POSIX `shutdown`).
    ///
    /// Shutting the write direction down queues a FIN behind the buffered
    /// data and flushes it, but — unlike [`close`](Self::close) — leaves the
    /// socket alive and delivering, so the client reads the peer's response
    /// and its eventual end-of-stream. Repeating a direction already shut
    /// down succeeds, as POSIX requires.
    fn shutdown(
        &mut self,
        interfaces: &mut Netstack,
        owner: ProcId,
        socket: SocketId,
        how: ShutdownHow,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        match &mut self.sockets[index].proto {
            Proto::Stream(Some(conn)) => {
                if how.closes_read() {
                    conn.read_shutdown = true;
                }
                // A FIN already queued means the write side is closed: say so
                // rather than letting the engine refuse a repeat.
                if how.closes_write() && !conn.tcb.send_closed() {
                    conn.tcb.close(now).map_err(|_| Errno::NotConnected)?;
                }
            }
            // A listener has no connection to half-close, and an unconnected
            // stream socket has none yet.
            Proto::Stream(None) | Proto::Listen(_) => return Err(Errno::NotConnected),
            // A half-close is a FIN, which only TCP has.
            Proto::Datagram(_) | Proto::Echo(_) => return Err(Errno::OutOfRange),
        }
        let tx = if how.closes_write() {
            self.pump_stream(interfaces, index, now)
        } else {
            FrameBatch::new()
        };
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Make a bound stream socket passive (LISTEN): it accepts inbound
    /// connections on its bound local port instead of originating one.
    ///
    /// The socket must be a bound, not-yet-connected stream socket. The
    /// privileged-port check was applied at [`bind`](Self::bind) time.
    fn listen(
        &mut self,
        interfaces: &Netstack,
        audit: &dyn Sink,
        owner: ProcId,
        socket: SocketId,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        // Only an unconnected stream socket can be made to listen.
        if !matches!(self.sockets[index].proto, Proto::Stream(None)) {
            return refuse(
                audit,
                "socket listen refused: not an unconnected stream socket",
                Errno::OutOfRange,
            );
        }
        if self.sockets[index].local_port == 0 {
            return refuse(
                audit,
                "socket listen refused: socket not bound to a local port",
                Errno::AddressUnavailable,
            );
        }
        let local_port = self.sockets[index].local_port;
        // A listener holds completed connections that only remote peers
        // decide the arrival of, so both the window each is given and how
        // many may wait at once come out of the owner's remaining share. A
        // share with no room for a workable connection is a listener that
        // could not serve one.
        let settings = interfaces.settings();
        // The queue takes a slice of the share rather than all that is
        // left: its slots are memory reserved for connections nobody has
        // accepted yet, and reserving the lot would leave nothing for the
        // ones the client does accept.
        let allowance =
            self.remaining_allowance(&settings, owner) / CONNECTION_BUFFER_SHARE_DIVISOR;
        let Some(config) = listen_config(settings, allowance) else {
            return refuse(
                audit,
                "socket listen refused: socket memory budget exhausted",
                Errno::LimitExceeded,
            );
        };
        self.unindex_entry(index);
        self.sockets[index].proto = Proto::Listen(Box::new(Listener::new(local_port, config)));
        self.index_entry(index);
        self.reaccount(index);
        emit(
            audit,
            Level::Info,
            events::SOCKET_LISTENING,
            "socket listening",
            &[],
        );
        status_reply(response)
    }

    /// Claim the next established connection queued on a listening socket:
    /// find the oldest connection the listener has completed but the client
    /// has not yet taken (an unaccepted child stream socket on the same
    /// port), rebind it to the caller-supplied delivery port, mark it
    /// claimed, and hand back its new [`SocketId`]. Any bytes the peer
    /// already sent are delivered on this reply.
    ///
    /// Replies [`Errno::WouldBlock`] when no connection is ready (the client
    /// waits for the next [`Accepted`](SocketStreamEvent::Accepted) event).
    #[allow(clippy::too_many_arguments)]
    fn accept_socket(
        &mut self,
        interfaces: &mut Netstack,
        audit: &dyn Sink,
        owner: ProcId,
        socket: SocketId,
        deliver_port: u64,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        if deliver_port == 0 {
            return refuse(
                audit,
                "socket accept refused: zero delivery port",
                Errno::OutOfRange,
            );
        }
        let lindex = self.owned_index(owner, socket)?;
        if !matches!(self.sockets[lindex].proto, Proto::Listen(_)) {
            return refuse(
                audit,
                "socket accept refused: not a listening socket",
                Errno::OutOfRange,
            );
        }
        let family = self.sockets[lindex].family;
        let local_port = self.sockets[lindex].local_port;
        // The oldest unaccepted child of this listener owned by the caller.
        let Some(child) = self.sockets.iter().position(|e| {
            e.owner == owner
                && e.family == family
                && e.local_port == local_port
                && matches!(&e.proto, Proto::Stream(Some(c)) if !c.accepted)
        }) else {
            // Nothing ready — a non-error "try again" the client waits on.
            return Err(Errno::WouldBlock);
        };
        let child_id = self.sockets[child].id;
        self.sockets[child].deliver_port = deliver_port;
        if let Proto::Stream(Some(conn)) = &mut self.sockets[child].proto {
            conn.accepted = true;
        }
        emit(
            audit,
            Level::Info,
            events::SOCKET_ACCEPTED,
            "socket connection accepted",
            &[],
        );
        // Deliver whatever the connection already holds (the one-shot
        // Connected, any buffered received bytes, a close it already saw)
        // now that it has an owner and a delivery port.
        let deliveries = self.collect_stream_events(child);
        // Draining the receive buffer may have opened the window; pump any
        // resulting ACK. `reap_if_done` never fires here (the client just
        // took it and has not closed).
        let tx = self.pump_stream(interfaces, child, now);
        let len = encode_socket_reply(Ok(child_id), response)?;
        Ok(SocketReply {
            len,
            tx,
            deliveries,
            watch: None,
        })
    }
}

impl SocketService {
    /// Send one datagram from a datagram socket, implicitly binding an
    /// ephemeral local port on first send.
    #[allow(clippy::too_many_arguments)]
    fn send_datagram(
        &mut self,
        interfaces: &mut Netstack,
        entropy: &mut dyn FnMut() -> u32,
        audit: &dyn Sink,
        index: usize,
        dest: Option<SocketAddr>,
        interface: Option<[u8; IF_NAME_LEN]>,
        payload: &[u8],
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let family = self.sockets[index].family;
        let peer = match &self.sockets[index].proto {
            Proto::Datagram(dg) => dg.peer,
            Proto::Stream(_) | Proto::Listen(_) | Proto::Echo(_) => None,
        };
        let Some(target) = dest.or(peer) else {
            return refuse(
                audit,
                "socket send refused: not connected",
                Errno::NotConnected,
            );
        };
        if target.family != family {
            return Err(Errno::OutOfRange);
        }
        let source_port = self.ensure_local_port(entropy, index)?;
        match interfaces.originate(
            ip_of(target),
            source_port,
            target.port,
            payload,
            interface,
            now,
        ) {
            Ok(tx) => {
                let len = status_reply(response)?.len;
                Ok(SocketReply {
                    len,
                    tx,
                    deliveries: Vec::new(),
                    watch: None,
                })
            }
            Err(err) => refuse(audit, "socket send refused", err),
        }
    }

    /// Send one ICMP/`ICMPv6` echo request from an echo socket, assigning
    /// the socket's stack-owned ICMP identifier on first use. The caller
    /// chooses the `sequence`; the identifier is never caller-controlled, so
    /// a socket only ever receives replies to its own requests.
    #[allow(clippy::too_many_arguments)]
    fn send_echo(
        &mut self,
        interfaces: &mut Netstack,
        entropy: &mut dyn FnMut() -> u32,
        audit: &dyn Sink,
        owner: ProcId,
        socket: SocketId,
        dest: Option<SocketAddr>,
        sequence: u16,
        payload: &[u8],
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        let family = self.sockets[index].family;
        let peer = match &self.sockets[index].proto {
            Proto::Echo(echo) => echo.peer,
            // A non-echo socket cannot originate an echo request.
            Proto::Datagram(_) | Proto::Stream(_) | Proto::Listen(_) => {
                return Err(Errno::OutOfRange)
            }
        };
        let Some(target) = dest.or(peer) else {
            return refuse(
                audit,
                "socket echo refused: not connected",
                Errno::NotConnected,
            );
        };
        if target.family != family {
            return Err(Errno::OutOfRange);
        }
        // The identifier is the socket's globally-unique local id; assign
        // it on first send so replies demux to exactly this socket.
        let identifier = self.ensure_local_port(entropy, index)?;
        match interfaces.originate_echo(ip_of(target), identifier, sequence, payload, now) {
            Ok(tx) => {
                let len = status_reply(response)?.len;
                Ok(SocketReply {
                    len,
                    tx,
                    deliveries: Vec::new(),
                    watch: None,
                })
            }
            Err(err) => refuse(audit, "socket echo refused", err),
        }
    }

    /// Actively open a stream connection: draw a CSPRNG ISN, build the
    /// [`Tcb`], choose the egress interface by originating the SYN, and
    /// record the connection. The socket stays unconnected (and the client
    /// may retry) if no interface can reach the peer.
    fn connect_stream(
        &mut self,
        interfaces: &mut Netstack,
        entropy: &mut dyn FnMut() -> u32,
        index: usize,
        peer: SocketAddr,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let local_port = self.ensure_local_port(entropy, index)?;
        self.reserve_index_rows()?;
        let dest = ip_of(peer);
        // Bind the egress interface and learn its effective MSS for this
        // family *before* building the TCB, so the SYN advertises — and the
        // connection segments to — a size that fits the link (RFC 6691).
        // The stack stays unconnected (the client may retry) when no
        // interface can reach the peer.
        let (iface, local_mss) = interfaces.egress_mss_for(dest, now)?;
        // Size the connection's buffers out of what is left of the owner's
        // share, so the ceilings handed out cannot sum past it. Too little
        // left to carry data is the point at which the stack refuses,
        // rather than admitting a connection it would cripple.
        let owner = self.sockets[index].owner;
        let remaining = self.remaining_allowance(&interfaces.settings(), owner);
        let Some(buffer) = NetworkSettings::connection_buffer_bytes(remaining) else {
            return Err(Errno::LimitExceeded);
        };
        // The ISN is a CSPRNG draw (the engine makes no randomness).
        let iss = entropy();
        let config = TcpConfig {
            local_mss,
            send_buffer: buffer,
            receive_buffer: buffer,
            // Enable segmentation offload when the egress device negotiated
            // it (0 keeps the connection per-MSS).
            tso_max_payload: interfaces.tso_max_payload_on(iface),
            // Probe an idle peer only when the stack-wide `net.tcp.keepalive`
            // policy is enabled (off by default, RFC 1122 §4.2.3.6).
            enable_keepalive: interfaces.settings().tcp_keepalive,
            // Negotiate RFC 3168 ECN only when the stack-wide `net.tcp.ecn`
            // policy is enabled (off by default; connections stay Not-ECT).
            enable_ecn: interfaces.settings().tcp_ecn,
            ..TcpConfig::default()
        };
        let tcb = Tcb::connect(config, local_port, peer.port, iss, now);
        // The socket becomes reachable by its four-tuple here, so it is
        // re-indexed rather than merely mutated.
        self.unindex_entry(index);
        self.sockets[index].proto = Proto::Stream(Some(Box::new(StreamConn {
            tcb,
            peer,
            iface,
            notified: Notified::Nothing,
            client_closed: false,
            read_shutdown: false,
            // An actively-opened connection is the client's from birth.
            accepted: true,
        })));
        self.index_entry(index);
        self.reaccount(index);
        // The SYN leaves through the one drain-and-route path every other
        // stream segment takes.
        let tx = self.pump_stream(interfaces, index, now);
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Enqueue bytes onto a stream's send buffer (accepting as many as the
    /// bounded buffer holds) and pump the resulting segments.
    fn send_stream(
        &mut self,
        interfaces: &mut Netstack,
        index: usize,
        payload: &[u8],
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let accepted = match &mut self.sockets[index].proto {
            Proto::Stream(Some(conn)) => match conn.tcb.send(payload) {
                Ok(n) => n,
                // The connection has closed or reset: no more may be sent.
                Err(_) => return Err(Errno::NotConnected),
            },
            _ => return Err(Errno::NotConnected),
        };
        let tx = self.pump_stream(interfaces, index, now);
        let accepted = u32::try_from(accepted).unwrap_or(u32::MAX);
        let len = encode_send_reply(Ok(accepted), response)?;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Drain a connected stream's outbound segments through its bound
    /// interface, returning the frames tagged by that interface's alias.
    fn pump_stream(
        &mut self,
        interfaces: &mut Netstack,
        index: usize,
        now: Duration64,
    ) -> FrameBatch {
        let (iface, dest) = match &self.sockets[index].proto {
            Proto::Stream(Some(conn)) => (conn.iface, ip_of(conn.peer)),
            _ => return FrameBatch::new(),
        };
        let mut segs = Vec::new();
        if let Proto::Stream(Some(conn)) = &mut self.sockets[index].proto {
            segs = drain_segments(&mut conn.tcb, now);
        }
        let mut frames = Vec::new();
        for (meta, payload, gso_size, ecn) in &segs {
            if let Ok(more) =
                interfaces.send_tcp_on(iface, dest, meta, payload, *gso_size, *ecn, now)
            {
                frames.extend(more);
            }
        }
        if frames.is_empty() {
            FrameBatch::new()
        } else {
            alloc::vec![(iface, frames)]
        }
    }

    /// Feed one inbound TCP segment (already checksum-verified by the
    /// engine) to the socket its four-tuple names, driving the resulting
    /// egress segments and client-visible stream events.
    ///
    /// The segment is routed to, in order: an established connection
    /// (active or an accepted child) matching the full four-tuple; else a
    /// passive listener on the destination port, which demultiplexes it
    /// (SYN handshake, SYN-cookie validation, or RST). `secret`
    /// authenticates SYN cookies. A segment matching neither is dropped.
    /// It never panics and returns bounded output.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn on_tcp_segment(
        &mut self,
        interfaces: &mut Netstack,
        source: IpAddr,
        destination: IpAddr,
        ecn: Ecn,
        segment: &[u8],
        now: Duration64,
        secret: &dyn CookieSecret,
    ) -> StreamIo {
        let pseudo = pseudo_for(source, destination);
        let Some(seg) = TcpSegment::parse(pseudo, segment) else {
            return StreamIo::default();
        };
        let (fam, src_bytes) = address_parts(source);
        let (dst_port, src_port) = (seg.destination_port, seg.source_port);
        // 1. An established connection (active open, or a child accepted off
        //    a listener) claims the segment by its full four-tuple.
        let key = ConnKey {
            family: fam,
            local_port: dst_port,
            peer_addr: src_bytes,
            peer_port: src_port,
        };
        if let Some(index) = self
            .by_conn
            .get(&key)
            .and_then(|id| self.by_id.get(id))
            .copied()
        {
            if let Proto::Stream(Some(conn)) = &mut self.sockets[index].proto {
                conn.tcb.on_segment(&seg, ecn, now);
            }
            let tx = self.pump_stream(interfaces, index, now);
            let deliveries = self.collect_stream_events(index);
            self.reap_if_done(index);
            #[cfg(test)]
            self.assert_indices_agree();
            return StreamIo {
                tx,
                deliveries,
                cookies_engaged: false,
            };
        }
        // 2. A passive listener on the destination port demultiplexes it.
        if let Some(lindex) = self.demux_index(fam, dst_port) {
            let entry = &self.sockets[lindex];
            if entry.family == fam && matches!(&entry.proto, Proto::Listen(_)) {
                let io =
                    self.drive_listener(interfaces, lindex, source, destination, &seg, now, secret);
                #[cfg(test)]
                self.assert_indices_agree();
                return io;
            }
        }
        StreamIo::default()
    }

    /// Feed one inbound segment to the listener at `lindex`, route the
    /// segments it emits (SYN-ACK, cookie SYN-ACK, RST) back to the peer,
    /// and drain any newly completed connection into a pending child
    /// socket. `local` is the local address the segment arrived on.
    #[allow(clippy::too_many_arguments)]
    fn drive_listener(
        &mut self,
        interfaces: &mut Netstack,
        lindex: usize,
        source: IpAddr,
        destination: IpAddr,
        seg: &TcpSegment<'_>,
        now: Duration64,
        secret: &dyn CookieSecret,
    ) -> StreamIo {
        let peer = Peer {
            addr: source,
            port: seg.source_port,
        };
        let mut emitted: Vec<OutSeg> = Vec::new();
        let mut cookies_engaged = false;
        if let Proto::Listen(listener) = &mut self.sockets[lindex].proto {
            let before = listener.stats().cookies_sent;
            listener.on_segment(destination, peer, seg, now, secret, |_peer, out| {
                emitted.push((out.meta, out.payload.to_vec(), out.gso_size, out.ecn));
                true
            });
            cookies_engaged = before == 0 && listener.stats().cookies_sent > 0;
        }
        let mut io = StreamIo {
            cookies_engaged,
            ..StreamIo::default()
        };
        io.tx
            .extend(route_segments_to(interfaces, source, &emitted, now));
        io.deliveries
            .extend(self.drain_listener_accepts(interfaces, lindex, now));
        io
    }

    /// Advance a listener's timers (retransmit owed SYN-ACKs, expire stale
    /// half-open handshakes), routing each retransmitted segment back to
    /// its peer.
    fn advance_listener(
        &mut self,
        interfaces: &mut Netstack,
        lindex: usize,
        now: Duration64,
    ) -> FrameBatch {
        let mut emitted: Vec<PeerOutSeg> = Vec::new();
        if let Proto::Listen(listener) = &mut self.sockets[lindex].proto {
            listener.advance(now, |peer, out| {
                emitted.push((
                    peer.addr,
                    out.meta,
                    out.payload.to_vec(),
                    out.gso_size,
                    out.ecn,
                ));
                true
            });
        }
        let mut tx = FrameBatch::new();
        for (peer_ip, meta, payload, gso_size, ecn) in &emitted {
            if let Ok((iface, _mss)) = interfaces.egress_mss_for(*peer_ip, now) {
                if let Ok(frames) =
                    interfaces.send_tcp_on(iface, *peer_ip, meta, payload, *gso_size, *ecn, now)
                {
                    if !frames.is_empty() {
                        tx.push((iface, frames));
                    }
                }
            }
        }
        tx
    }

    /// Drain every connection the listener at `lindex` has completed into a
    /// new **pending** child stream socket (owned by the same principal,
    /// on the same local port), and deliver one
    /// [`Accepted`](SocketStreamEvent::Accepted) readiness event per child
    /// to the listener's port. Child creation is bounded by the socket
    /// quota: at the ceiling the completed connections stay queued in the
    /// listener (which itself RSTs further completions once its bounded
    /// accept queue fills) — fail closed, never an unbounded table.
    fn drain_listener_accepts(
        &mut self,
        interfaces: &mut Netstack,
        lindex: usize,
        now: Duration64,
    ) -> Vec<Delivery> {
        let owner = self.sockets[lindex].owner;
        let owner_pid = self.sockets[lindex].owner_pid;
        let deliver_port = self.sockets[lindex].deliver_port;
        let family = self.sockets[lindex].family;
        let local_port = self.sockets[lindex].local_port;
        let listener_id = self.sockets[lindex].id;
        let settings = interfaces.settings();
        let mut out = Vec::new();
        loop {
            if self.budget_exhausted(&settings, owner) {
                break;
            }
            // Both budget questions are asked before the connection leaves
            // the listener: a child taken and then refused would be a
            // connection dropped, where one left queued is a connection the
            // peer is still holding open.
            let remaining = self.remaining_allowance(&settings, owner);
            let Some(buffer) = NetworkSettings::connection_buffer_bytes(remaining) else {
                break;
            };
            let conn = match &mut self.sockets[lindex].proto {
                Proto::Listen(listener) => listener.accept(),
                _ => break,
            };
            let Some(conn) = conn else {
                break;
            };
            // Bind the child to the interface that reaches its peer; a
            // connection with no route home is dropped (it times out
            // remotely) rather than parked forever.
            let Ok((iface, _mss)) = interfaces.egress_mss_for(conn.peer.addr, now) else {
                continue;
            };
            let (pfam, paddr) = address_parts(conn.peer.addr);
            let peer = SocketAddr {
                family: pfam,
                addr: paddr,
                port: conn.peer.port,
            };
            let id = self.alloc_id();
            let child = self.insert_entry(SocketEntry {
                id,
                owner,
                owner_pid,
                // Inherited until `Accept` rebinds it to the client's port.
                deliver_port,
                family,
                local_addr: [0u8; 16],
                local_port,
                accounted_bytes: 0,
                proto: Proto::Stream(Some(Box::new(StreamConn {
                    tcb: {
                        // A listener template does not know the egress link,
                        // so segmentation offload is enabled here, once the
                        // accepted child is bound to the interface that
                        // reaches its peer.
                        let mut tcb = conn.tcb;
                        tcb.set_tso_max_payload(interfaces.tso_max_payload_on(iface));
                        // Re-ceiling against the share as it stands now: the
                        // listener's template was sized when it began
                        // listening, and its earlier children have since
                        // taken part of the share.
                        tcb.set_buffer_limits(buffer, buffer);
                        tcb
                    },
                    peer,
                    iface,
                    notified: Notified::Nothing,
                    client_closed: false,
                    read_shutdown: false,
                    // Passive: the client must claim it with `Accept`.
                    accepted: false,
                }))),
            });
            // A child the table cannot index is a connection nothing could
            // reach, so draining stops and the rest stay queued in the
            // listener, exactly as they do at the capacity bound.
            if child.is_err() {
                break;
            }
            push_stream_event(
                &mut out,
                deliver_port,
                &SocketStreamEvent::Accepted {
                    socket: listener_id,
                },
            );
        }
        out
    }

    /// Drive every connected stream's timers at `now` (retransmit, delayed
    /// ACK, persist, user timeout, TIME-WAIT), returning the egress frames
    /// and client events. Fully-closed client-closed connections are
    /// reaped.
    #[must_use]
    pub fn advance_streams(&mut self, interfaces: &mut Netstack, now: Duration64) -> StreamIo {
        let mut io = StreamIo::default();
        let mut i = 0;
        while i < self.sockets.len() {
            match &self.sockets[i].proto {
                Proto::Stream(Some(_)) => {
                    if let Proto::Stream(Some(conn)) = &mut self.sockets[i].proto {
                        conn.tcb.advance(now);
                    }
                    io.tx.extend(self.pump_stream(interfaces, i, now));
                    io.deliveries.extend(self.collect_stream_events(i));
                    if self.reap_if_done(i) {
                        // A reap swap-removed this slot; re-examine it.
                        continue;
                    }
                }
                Proto::Listen(_) => {
                    io.tx.extend(self.advance_listener(interfaces, i, now));
                    io.deliveries
                        .extend(self.drain_listener_accepts(interfaces, i, now));
                }
                // A datagram socket, an echo socket, and an unconnected
                // stream socket have no timers to advance.
                Proto::Datagram(_) | Proto::Stream(None) | Proto::Echo(_) => {}
            }
            i += 1;
        }
        #[cfg(test)]
        self.assert_indices_agree();
        io
    }

    /// The earliest deadline across every connected stream, folded into
    /// the service's wait-set timeout beside the per-interface deadlines.
    #[must_use]
    pub fn stream_next_deadline(&self) -> Option<Duration64> {
        self.sockets
            .iter()
            .filter_map(|e| match &e.proto {
                Proto::Stream(Some(c)) => c.tcb.next_deadline(),
                Proto::Listen(l) => l.next_deadline(),
                _ => None,
            })
            .min_by_key(|d| (d.secs(), d.subsec_nanos()))
    }

    /// Collect the client-visible events a connection now owes: the
    /// one-shot `Connected`, any received stream bytes in order, and the
    /// one-shot `Closed` (once, stating why). No event follows `Closed`.
    fn collect_stream_events(&mut self, index: usize) -> Vec<Delivery> {
        let deliver_port = self.sockets[index].deliver_port;
        let id = self.sockets[index].id;
        let mut out = Vec::new();
        let Proto::Stream(Some(conn)) = &mut self.sockets[index].proto else {
            return out;
        };
        // A connection produced by a listener but not yet claimed with
        // `Accept` has no owner to hear its events: hold them (its received
        // bytes buffer in the bounded TCB) until it is accepted, so the
        // client never sees data for a connection it has not taken.
        if !conn.accepted {
            return out;
        }
        if conn.notified == Notified::Closed {
            return out;
        }
        if conn.tcb.is_established() && conn.notified == Notified::Nothing {
            conn.notified = Notified::Connected;
            push_stream_event(
                &mut out,
                deliver_port,
                &SocketStreamEvent::Connected { socket: id },
            );
        }
        // Deliver every in-order received byte before any close, in
        // bounded chunks. Draining the receive buffer keeps the advertised
        // window open (the client's port queue is the app buffer) — so a
        // read-shutdown socket keeps draining too, discarding what it reads
        // instead of stalling a peer that is still sending.
        loop {
            let mut buf = [0u8; SOCKET_MAX_DATAGRAM];
            let n = conn.tcb.recv(&mut buf);
            if n == 0 {
                break;
            }
            if conn.read_shutdown {
                continue;
            }
            push_stream_event(
                &mut out,
                deliver_port,
                &SocketStreamEvent::Data {
                    socket: id,
                    payload: &buf[..n],
                },
            );
        }
        let reason = if let Some(r) = conn.tcb.reset_reason() {
            Some(map_reset(r))
        } else if peer_closed(conn.tcb.state()) {
            Some(StreamCloseReason::PeerClosed)
        } else {
            None
        };
        if let Some(reason) = reason {
            conn.notified = Notified::Closed;
            push_stream_event(
                &mut out,
                deliver_port,
                &SocketStreamEvent::Closed { socket: id, reason },
            );
        }
        out
    }

    /// Reap a client-closed stream once its teardown has fully completed
    /// (RFC 9293 CLOSED). Returns whether the slot was removed.
    fn reap_if_done(&mut self, index: usize) -> bool {
        let done = matches!(&self.sockets[index].proto,
            Proto::Stream(Some(c)) if c.client_closed && matches!(c.tcb.state(), State::Closed));
        if done {
            self.remove_at(index);
        }
        done
    }

    /// Join a multicast group on a datagram socket, refcounted per
    /// membership.
    fn join(
        &mut self,
        interfaces: &mut Netstack,
        owner: ProcId,
        socket: SocketId,
        group: SocketAddr,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        // Multicast is a datagram-only concept.
        let Proto::Datagram(_) = &self.sockets[index].proto else {
            return Err(Errno::OutOfRange);
        };
        if group.family != self.sockets[index].family || !is_multicast_addr(group) {
            return Err(Errno::OutOfRange);
        }
        if let Proto::Datagram(dg) = &self.sockets[index].proto {
            if dg.groups.contains(&group.addr) {
                return status_reply(response);
            }
            if dg.groups.len() >= MAX_GROUPS_PER_SOCKET {
                return Err(Errno::LimitExceeded);
            }
        }
        self.behind.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
        let tx = interfaces.join_multicast_all(ip_of(group), now)?;
        // A first membership is owed every link it already rides.
        let mut enlist = false;
        if let Proto::Datagram(dg) = &mut self.sockets[index].proto {
            enlist = dg.groups.is_empty() && !dg.listed;
            dg.listed |= enlist;
            dg.groups.push(group.addr);
        }
        if enlist {
            self.behind.push(socket);
        }
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Leave a multicast group a datagram socket had joined.
    fn leave(
        &mut self,
        interfaces: &mut Netstack,
        owner: ProcId,
        socket: SocketId,
        group: SocketAddr,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        let Proto::Datagram(_) = &self.sockets[index].proto else {
            return Err(Errno::OutOfRange);
        };
        let removed = if let Proto::Datagram(dg) = &mut self.sockets[index].proto {
            if let Some(pos) = dg.groups.iter().position(|g| *g == group.addr) {
                dg.groups.swap_remove(pos);
                true
            } else {
                false
            }
        } else {
            false
        };
        if !removed {
            // Leaving a group never joined is a no-op success.
            return status_reply(response);
        }
        let tx = interfaces.leave_multicast_all(ip_of(group), now);
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
            watch: None,
        })
    }

    /// Tell each member socket what moved on the links its memberships
    /// ride since it was last told, through `send`, and return the delivery
    /// ports that had no room: each is owed the rest, which the next call
    /// delivers once it drains.
    ///
    /// A socket is told the difference between its view and `links`: a link
    /// that came up, one that went down or left, and one whose epoch moved
    /// while its state did not, which is a flap told as down then up so the
    /// reader discards what it learned before. Only a socket marked behind is
    /// visited, and every member is marked when `epoch` moves.
    pub fn tell_links(
        &mut self,
        links: &[PublishedLink],
        epoch: u64,
        send: &mut Post<'_>,
    ) -> Vec<u64> {
        if epoch != self.links_seen && self.mark_members_behind() {
            self.links_seen = epoch;
        }
        let mut blocked: Vec<u64> = Vec::new();
        let mut position = 0;
        while position < self.behind.len() {
            let socket = self.behind[position];
            let Some(&index) = self.by_id.get(&socket) else {
                self.behind.swap_remove(position);
                continue;
            };
            let entry = &mut self.sockets[index];
            let port = entry.deliver_port;
            if blocked.contains(&port) {
                position += 1;
                continue;
            }
            match tell_one(entry.id, entry.family, port, &mut entry.proto, links, send) {
                Told::Caught => {
                    self.behind.swap_remove(position);
                }
                Told::Blocked => {
                    if blocked.try_reserve(1).is_ok() {
                        blocked.push(port);
                    }
                    position += 1;
                }
                Told::Short => position += 1,
            }
        }
        blocked
    }

    /// Mark every socket holding a membership behind, reporting whether all
    /// of them could be.
    fn mark_members_behind(&mut self) -> bool {
        let Self {
            sockets, behind, ..
        } = self;
        for entry in sockets.iter_mut() {
            let Proto::Datagram(dg) = &mut entry.proto else {
                continue;
            };
            if dg.groups.is_empty() || dg.listed {
                continue;
            }
            if behind.try_reserve(1).is_err() {
                return false;
            }
            dg.listed = true;
            behind.push(entry.id);
        }
        true
    }

    /// Release everything `owner` held, because it has exited: its groups
    /// are left, its connections aborted, its listeners and their unclaimed
    /// connections dropped, and every port freed. Returns the frames that
    /// takes to say so on the wire.
    ///
    /// A scan of the table, as the listener close is: an exit is driven by
    /// the owner, never by a remote peer, and a principal the table holds no
    /// row for costs one lookup.
    pub fn reclaim_owner(
        &mut self,
        interfaces: &mut Netstack,
        owner: ProcId,
        now: Duration64,
    ) -> FrameBatch {
        let mut tx = FrameBatch::new();
        if self.owned.get(&owner).is_none() {
            return tx;
        }
        // Highest position first, so each removal only moves an entry from
        // beyond the ones still to visit.
        let mut position = self.sockets.len();
        while position > 0 {
            position -= 1;
            if self
                .sockets
                .get(position)
                .is_none_or(|entry| entry.owner != owner)
            {
                continue;
            }
            tx.extend(self.abandon_at(interfaces, position, now));
        }
        interfaces.publish_datagram_ports(broadcast_consumer_ports(&self.sockets));
        #[cfg(test)]
        self.assert_indices_agree();
        tx
    }

    /// Drop the socket at `position` whose owner is gone: nothing remains to
    /// be told, so a stream is reset rather than closed gracefully.
    fn abandon_at(
        &mut self,
        interfaces: &mut Netstack,
        position: usize,
        now: Duration64,
    ) -> FrameBatch {
        let family = self.sockets[position].family;
        let mut tx = FrameBatch::new();
        match &mut self.sockets[position].proto {
            Proto::Datagram(dg) => {
                for group in core::mem::take(&mut dg.groups) {
                    tx.extend(interfaces.leave_multicast_all(ip_from_parts(family, group), now));
                }
            }
            Proto::Stream(Some(conn)) => {
                conn.client_closed = true;
                conn.tcb.abort(now);
                tx = self.pump_stream(interfaces, position, now);
            }
            Proto::Listen(listener) => {
                self.retired_defence = fold_defence(self.retired_defence, listener.stats());
            }
            Proto::Stream(None) | Proto::Echo(_) => {}
        }
        self.remove_at(position);
        tx
    }

    /// Route one engine receive [`StackEvent`] the logical interface
    /// `arrival` raised to the sockets that should receive it, returning an
    /// encoded delivery per matching socket: a [`SocketDatagram`] for a
    /// [`StackEvent::UdpDatagram`], or a [`SocketEcho`] for a
    /// [`StackEvent::EchoReply`]. Any other event yields nothing.
    #[must_use]
    pub fn deliver(&self, event: &StackEvent, arrival: [u8; IF_NAME_LEN]) -> Vec<Delivery> {
        match event {
            StackEvent::UdpDatagram { .. } => self.deliver_datagram(event, arrival),
            StackEvent::EchoReply { .. } => self.deliver_echo(event),
            _ => Vec::new(),
        }
    }

    /// Route one [`StackEvent::EchoReply`] to the echo socket whose
    /// stack-assigned identifier matches the reply's, filtered to the
    /// connected peer when one is set.
    fn deliver_echo(&self, event: &StackEvent) -> Vec<Delivery> {
        let StackEvent::EchoReply {
            source,
            identifier,
            sequence,
            payload,
        } = event
        else {
            return Vec::new();
        };
        let (src_family, src_bytes) = address_parts(*source);
        let mut out = Vec::new();
        // The identifier lives in `local_port` and is unique in its family,
        // so at most one socket matches — a reply never crosses sockets.
        if let Some(entry) = self
            .demux_index(src_family, *identifier)
            .map(|i| &self.sockets[i])
        {
            let Proto::Echo(echo) = &entry.proto else {
                return out;
            };
            if entry.family != src_family {
                return out;
            }
            if let Some(peer) = echo.peer {
                if peer.family != src_family || peer.addr != src_bytes {
                    return out;
                }
            }
            let echo_msg = SocketEcho {
                socket: entry.id,
                source: SocketAddr {
                    family: src_family,
                    addr: src_bytes,
                    port: 0,
                },
                sequence: *sequence,
                payload,
            };
            let mut buf = alloc::vec![0u8; SocketEcho::HEADER_LEN + payload.len()];
            if let Ok(len) = echo_msg.encode(&mut buf) {
                buf.truncate(len);
                out.push(Delivery {
                    deliver_port: entry.deliver_port,
                    datagram: buf,
                });
            }
        }
        out
    }

    /// Route one [`StackEvent::UdpDatagram`] to the datagram sockets that
    /// should receive it, returning an encoded [`SocketDatagram`] delivery
    /// per matching socket.
    fn deliver_datagram(&self, event: &StackEvent, arrival: [u8; IF_NAME_LEN]) -> Vec<Delivery> {
        let StackEvent::UdpDatagram {
            source,
            destination,
            source_port,
            destination_port,
            source_on_link,
            payload,
        } = event
        else {
            return Vec::new();
        };
        let (dest_family, dest_bytes) = address_parts(*destination);
        let (src_family, src_bytes) = address_parts(*source);
        let dest_multicast = is_multicast_ip(*destination);
        let mut out = Vec::new();
        // A port is bound by at most one socket of a family, so the
        // destination port names the one candidate rather than a scan.
        if let Some(entry) = self
            .demux_index(dest_family, *destination_port)
            .map(|index| &self.sockets[index])
        {
            let Proto::Datagram(dg) = &entry.proto else {
                return out;
            };
            if entry.family != dest_family {
                return out;
            }
            let dest_ok = if dest_multicast {
                dg.groups.contains(&dest_bytes)
            } else {
                entry.local_addr == [0u8; 16] || entry.local_addr == dest_bytes
            };
            if !dest_ok {
                return out;
            }
            if let Some(peer) = dg.peer {
                if peer.family != src_family || peer.addr != src_bytes || peer.port != *source_port
                {
                    return out;
                }
            }
            let datagram = SocketDatagram {
                socket: entry.id,
                interface: arrival,
                source: SocketAddr {
                    family: src_family,
                    addr: src_bytes,
                    port: *source_port,
                },
                source_on_link: *source_on_link,
                payload,
            };
            let mut buf = alloc::vec![0u8; SocketDatagram::HEADER_LEN + payload.len()];
            if let Ok(len) = datagram.encode(&mut buf) {
                buf.truncate(len);
                out.push(Delivery {
                    deliver_port: entry.deliver_port,
                    datagram: buf,
                });
            }
        }
        out
    }

    /// Whether a further socket for `owner` would take that principal past
    /// its share, or the stack past the delivered budget.
    ///
    /// The share is what stops one principal taking the stack's memory;
    /// the budget is what stops every principal together taking it. A
    /// socket costs at least its structural overhead however idle it is,
    /// so that is the figure which must still fit.
    fn budget_exhausted(&self, settings: &NetworkSettings, owner: ProcId) -> bool {
        if self.owner_bytes(owner).saturating_add(PER_SOCKET_OVERHEAD)
            > settings.socket_bytes_per_principal()
        {
            return true;
        }
        self.committed_bytes().saturating_add(PER_SOCKET_OVERHEAD) > settings.socket_budget_bytes
    }

    /// Bytes of socket state `owner` currently holds.
    fn owner_bytes(&self, owner: ProcId) -> u64 {
        self.owned.get(&owner).map_or(0, |usage| usage.bytes)
    }

    /// Bytes a new commitment of `owner`'s may take: whichever of the
    /// principal's remaining share and the stack's remaining budget is
    /// smaller.
    ///
    /// Both, because neither alone bounds the stack. A share bounds one
    /// principal, and sixteen of them come to the budget — but a
    /// principal's share does not shrink as *others* fill, so enough
    /// principals each taking their own share would together pass the
    /// budget. Sizing every ceiling against what the stack has left as
    /// well is what makes the reservation hold in aggregate.
    fn remaining_allowance(&self, settings: &NetworkSettings, owner: ProcId) -> u64 {
        let share = settings
            .socket_bytes_per_principal()
            .saturating_sub(self.owner_bytes(owner));
        share.min(
            settings
                .socket_budget_bytes
                .saturating_sub(self.committed_bytes()),
        )
    }

    /// Bytes of socket state the stack holds across every principal: what
    /// each socket is committed to, plus the spare capacity its table and
    /// indices carry.
    ///
    /// The figure `net.sockets.mem` bounds. Constant time.
    #[must_use]
    pub fn committed_bytes(&self) -> u64 {
        self.bytes_total.saturating_add(self.structural_slack())
    }

    /// Bytes the table and its indices hold beyond the rows they are
    /// charged for: a container's spare slots and its growth headroom.
    ///
    /// Charged to the stack rather than to a socket, because no socket
    /// asked for it. Constant time — each figure is a container's own — so
    /// the budget stays honest about what is really allocated without any
    /// decision walking the table.
    fn structural_slack(&self) -> u64 {
        let allocated = widen(self.sockets.capacity())
            .saturating_mul(widen(size_of::<SocketEntry>()))
            .saturating_add(widen(self.by_id.allocated_bytes()))
            .saturating_add(widen(self.by_conn.allocated_bytes()))
            .saturating_add(widen(self.by_port.allocated_bytes()))
            .saturating_add(widen(self.owned.allocated_bytes()));
        allocated.saturating_sub(widen(self.sockets.len()).saturating_mul(PER_SOCKET_OVERHEAD))
    }

    /// Bytes the socket at `index` is charged: its structural share, plus
    /// what its transport is committed to.
    ///
    /// The **commitment**, never the occupancy. Charging what a socket
    /// holds today would admit any number of quiet connections and then
    /// leave the stack unable to refuse the data that filled them — the
    /// ceilings were handed out long before, so there would be nothing
    /// left to refuse. Charging what each may grow to makes admission a
    /// reservation, which is what lets the ceilings be sized from the share
    /// and never need clawing back.
    ///
    /// Every term is therefore a bound rather than a measurement, so a
    /// socket's charge moves only when its kind or its ceilings do.
    fn committed_of(&self, index: usize) -> u64 {
        let transport = match &self.sockets[index].proto {
            Proto::Datagram(_) => MAX_GROUPS_PER_SOCKET * size_of::<[u8; 16]>(),
            // An echo socket and an unconnected stream socket hold nothing
            // beyond their table slot.
            Proto::Echo(_) | Proto::Stream(None) => 0,
            // The boxed allocation, then the connection's own buffers,
            // which `committed_bytes` deliberately excludes it from.
            Proto::Stream(Some(conn)) => size_of::<StreamConn>() + conn.tcb.committed_bytes(),
            Proto::Listen(listener) => size_of::<Listener>() + listener.committed_bytes(),
        };
        PER_SOCKET_OVERHEAD.saturating_add(widen(transport))
    }

    /// Bring the socket at `index`'s charge up to date, applying the
    /// difference to its owner's share and to the stack total.
    ///
    /// A difference, never a re-sum: a recount would walk the table, which
    /// is exactly the cost the indices exist to avoid. Called at the three
    /// points a commitment can move — a socket appearing, connecting, or
    /// beginning to listen; the test-build invariant check recomputes every
    /// figure from scratch, so a site that forgets has nowhere to hide.
    fn reaccount(&mut self, index: usize) {
        let now = self.committed_of(index);
        let was = self.sockets[index].accounted_bytes;
        if now == was {
            return;
        }
        self.sockets[index].accounted_bytes = now;
        let owner = self.sockets[index].owner;
        self.bytes_total = self.bytes_total.saturating_sub(was).saturating_add(now);
        if let Some(usage) = self.owned.get_mut(&owner) {
            usage.bytes = usage.bytes.saturating_sub(was).saturating_add(now);
        }
    }

    /// The table index of the socket `owner` owns bearing `id`, or
    /// [`Errno::NotFound`] — a handle another principal owns is reported as
    /// absent, never distinguished (existence is not leaked).
    fn owned_index(&self, owner: ProcId, id: SocketId) -> Result<usize, Errno> {
        let index = *self.by_id.get(&id).ok_or(Errno::NotFound)?;
        if self.sockets[index].owner == owner {
            Ok(index)
        } else {
            Err(Errno::NotFound)
        }
    }

    /// The connection key of the entry at `index`, when it is an
    /// established stream (the only kind reached by four-tuple).
    fn conn_key_at(&self, index: usize) -> Option<ConnKey> {
        let entry = &self.sockets[index];
        let Proto::Stream(Some(conn)) = &entry.proto else {
            return None;
        };
        Some(ConnKey {
            family: entry.family,
            local_port: entry.local_port,
            peer_addr: conn.peer.addr,
            peer_port: conn.peer.port,
        })
    }

    /// Add every index row the entry at `index` implies, from its current
    /// state. Paired with [`Self::unindex_entry`]: a field the indices are
    /// keyed on is changed between the two, never under them.
    ///
    /// An index row is dropped on an allocation failure rather than the
    /// operation failing: the table is still correct, only slower to
    /// search, and refusing a socket the caller is entitled to because a
    /// cache could not grow would be the worse answer.
    fn index_entry(&mut self, index: usize) {
        let id = self.sockets[index].id;
        let owner = self.sockets[index].owner;
        let port = self.sockets[index].local_port;
        let accounted = self.sockets[index].accounted_bytes;
        let conn = self.conn_key_at(index);
        let _ = self.by_id.try_insert(id, index);
        // The socket's charge travels with its owner row, so the bracket a
        // key change is made inside puts back exactly what it took even
        // when this is the principal's only socket and the row went with it.
        if let Some(usage) = self.owned.get_mut(&owner) {
            usage.sockets = usage.sockets.saturating_add(1);
            usage.bytes = usage.bytes.saturating_add(accounted);
        } else {
            let _ = self.owned.try_insert(
                owner,
                OwnerUsage {
                    sockets: 1,
                    bytes: accounted,
                },
            );
        }
        if port != 0 {
            let key = (self.sockets[index].family, port);
            let mut slot = self.by_port.get(&key).copied().unwrap_or_default();
            slot.holders = slot.holders.saturating_add(1);
            if conn.is_none() {
                slot.demux = Some(id);
            }
            let _ = self.by_port.try_insert(key, slot);
        }
        if let Some(key) = conn {
            let _ = self.by_conn.try_insert(key, id);
        }
    }

    /// Remove every index row the entry at `index` implies.
    fn unindex_entry(&mut self, index: usize) {
        let id = self.sockets[index].id;
        let owner = self.sockets[index].owner;
        let port = self.sockets[index].local_port;
        let accounted = self.sockets[index].accounted_bytes;
        let conn = self.conn_key_at(index);
        self.by_id.remove(&id);
        if let Some(usage) = self.owned.get_mut(&owner) {
            usage.sockets = usage.sockets.saturating_sub(1);
            usage.bytes = usage.bytes.saturating_sub(accounted);
            if usage.sockets == 0 {
                self.owned.remove(&owner);
            }
        }
        if port != 0 {
            let key = (self.sockets[index].family, port);
            if let Some(mut slot) = self.by_port.get(&key).copied() {
                slot.holders = slot.holders.saturating_sub(1);
                if slot.demux == Some(id) {
                    slot.demux = None;
                }
                if slot.holders == 0 {
                    self.by_port.remove(&key);
                } else {
                    let _ = self.by_port.try_insert(key, slot);
                }
            }
        }
        if let Some(key) = conn {
            // Only if it is still this socket's row: a stale key can never
            // evict a live socket's.
            if self.by_conn.get(&key) == Some(&id) {
                self.by_conn.remove(&key);
            }
        }
    }

    /// Whether any live socket holds local `port` — the port index as the
    /// tests see it, so a release can be asserted rather than inferred.
    #[cfg(test)]
    pub(crate) fn port_is_held(&self, port: u16) -> bool {
        [NetAddrFamily::V4, NetAddrFamily::V6]
            .into_iter()
            .any(|family| self.port_in_use(family, port))
    }

    /// How many member sockets are listed behind, so the tests can see each
    /// is listed once.
    #[cfg(test)]
    pub(crate) fn behind_len(&self) -> usize {
        self.behind.len()
    }

    /// Bytes charged to `owner`, so the tests can assert the share is
    /// respected rather than infer it from a refusal.
    #[cfg(test)]
    pub(crate) fn charged_to(&self, owner: ProcId) -> u64 {
        self.owner_bytes(owner)
    }

    /// The send ceiling the connection behind `id` was given, so the tests
    /// can assert what the budget handed out rather than infer it.
    #[cfg(test)]
    pub(crate) fn send_window_of(&self, id: SocketId) -> Option<usize> {
        let index = *self.by_id.get(&id)?;
        match &self.sockets[index].proto {
            Proto::Stream(Some(conn)) => Some(conn.tcb.send_buffer_limit()),
            _ => None,
        }
    }

    /// Assert every index row, and every byte charged, agrees with the
    /// table it describes.
    ///
    /// Both the indices and the byte accounting are maintained
    /// incrementally, so an update missed beside a mutation is the one
    /// failure this structure has. Rebuilding what the table implies and
    /// comparing leaves nowhere for such a miss to hide; the whole suite
    /// drives it, because every served request, inbound segment, and timer
    /// pass ends here in the test build.
    #[cfg(test)]
    fn assert_indices_agree(&self) {
        use alloc::collections::BTreeMap;

        assert_eq!(self.by_id.len(), self.sockets.len(), "by_id row count");
        let mut conns = 0usize;
        let mut ports: BTreeMap<PortKey, PortSlot> = BTreeMap::new();
        let mut owners: BTreeMap<ProcId, OwnerUsage> = BTreeMap::new();
        let mut total = 0u64;
        for (index, entry) in self.sockets.iter().enumerate() {
            assert_eq!(self.by_id.get(&entry.id), Some(&index), "by_id row");
            let charge = self.committed_of(index);
            assert_eq!(
                entry.accounted_bytes, charge,
                "socket {} charge is stale",
                entry.id
            );
            total = total.saturating_add(charge);
            let usage = owners.entry(entry.owner).or_default();
            usage.sockets += 1;
            usage.bytes += charge;
            let conn = self.conn_key_at(index);
            if let Some(key) = conn {
                assert_eq!(self.by_conn.get(&key), Some(&entry.id), "by_conn row");
                conns += 1;
            }
            if entry.local_port != 0 {
                let slot = ports.entry((entry.family, entry.local_port)).or_default();
                slot.holders += 1;
                if conn.is_none() {
                    assert!(slot.demux.is_none(), "two binders on one port");
                    slot.demux = Some(entry.id);
                }
            }
        }
        assert_eq!(self.by_conn.len(), conns, "by_conn row count");
        assert_eq!(self.by_port.len(), ports.len(), "by_port row count");
        for (key, want) in ports {
            let got = self.by_port.get(&key).expect("indexed port");
            assert_eq!(got.holders, want.holders, "port {key:?} holders");
            assert_eq!(got.demux, want.demux, "port {key:?} demux");
        }
        assert_eq!(self.owned.len(), owners.len(), "owned row count");
        for (who, want) in owners {
            assert_eq!(self.owned.get(&who), Some(&want), "owner usage");
        }
        assert_eq!(self.bytes_total, total, "stack byte total");
    }

    /// Push `entry` onto the table and index it, returning its position.
    ///
    /// Every index row is reserved before the table gains the socket. A
    /// row that could not be added afterwards would leave a socket that
    /// nothing can address, close, or demultiplex to while it still
    /// counts against its owner's share — so the open fails closed
    /// instead, which is the answer an exhausted heap should give.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when the table or an index cannot grow.
    fn insert_entry(&mut self, entry: SocketEntry) -> Result<usize, Errno> {
        self.reserve_index_rows()?;
        self.sockets
            .try_reserve(1)
            .map_err(|_| Errno::OutOfMemory)?;
        self.sockets.push(entry);
        let index = self.sockets.len() - 1;
        self.index_entry(index);
        self.reaccount(index);
        Ok(index)
    }

    /// Reserve room for one further row in each index.
    ///
    /// Removal never shrinks a table, so a bracketed
    /// [`unindex_entry`](Self::unindex_entry) →  mutate →
    /// [`index_entry`](Self::index_entry) can always put back what it
    /// took; only a mutation that adds a *new* kind of row (a first port,
    /// a first connection) can need to grow, and it reserves here first.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when an index cannot grow.
    fn reserve_index_rows(&mut self) -> Result<(), Errno> {
        self.by_id.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
        self.owned.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
        self.by_port
            .try_reserve(1)
            .map_err(|_| Errno::OutOfMemory)?;
        self.by_conn.try_reserve(1).map_err(|_| Errno::OutOfMemory)
    }

    /// Drop the socket at `index`, keeping every index row in step.
    ///
    /// `swap_remove` moves the last entry into the hole, so exactly one
    /// other socket changes position and only its `by_id` row is repaired
    /// — every other index is keyed to a handle, not a position.
    fn remove_at(&mut self, index: usize) {
        self.unindex_entry(index);
        self.bytes_total = self
            .bytes_total
            .saturating_sub(self.sockets[index].accounted_bytes);
        self.sockets.swap_remove(index);
        if index < self.sockets.len() {
            let moved = self.sockets[index].id;
            let _ = self.by_id.try_insert(moved, index);
        }
    }

    /// Give this socket an ephemeral local port if it has none, keeping
    /// the port index in step, and return the port it now holds.
    ///
    /// Idempotent: a socket that already bound one keeps it. Datagram,
    /// echo, and actively-opening stream sockets all reach a port this
    /// way on their first use, so the drawing and the indexing live here
    /// rather than once per transport.
    fn ensure_local_port(
        &mut self,
        entropy: &mut dyn FnMut() -> u32,
        index: usize,
    ) -> Result<u16, Errno> {
        if self.sockets[index].local_port == 0 {
            self.reserve_index_rows()?;
            let port = self.assign_port(entropy, self.sockets[index].family, 0)?;
            self.unindex_entry(index);
            self.sockets[index].local_port = port;
            self.index_entry(index);
        }
        Ok(self.sockets[index].local_port)
    }

    /// Assign a local port: the requested port if free, or a CSPRNG-drawn
    /// ephemeral one when `requested` is `0`. A port is unique in its
    /// family's space across every socket (no silent reuse); fail closed
    /// with [`Errno::AddressInUse`].
    fn assign_port(
        &mut self,
        entropy: &mut dyn FnMut() -> u32,
        family: NetAddrFamily,
        requested: u16,
    ) -> Result<u16, Errno> {
        self.port_assignments = self.port_assignments.wrapping_add(1);
        if requested != 0 {
            if self.port_in_use(family, requested) {
                return Err(Errno::AddressInUse);
            }
            return Ok(requested);
        }
        let span = u32::from(EPHEMERAL_MAX - EPHEMERAL_MIN) + 1;
        for _ in 0..EPHEMERAL_TRIES {
            // `entropy() % span` is < span <= 16384, so the u16 cast never
            // truncates a meaningful bit.
            #[allow(clippy::cast_possible_truncation)]
            let candidate = EPHEMERAL_MIN + (entropy() % span) as u16;
            if !self.port_in_use(family, candidate) {
                return Ok(candidate);
            }
        }
        Err(Errno::AddressInUse)
    }

    /// The table position of the socket inbound traffic for local `port`
    /// goes to, if any. Never an established stream: those are reached by
    /// their four-tuple, so a listener and its accepted children on one
    /// port do not contend for this row.
    fn demux_index(&self, family: NetAddrFamily, port: u16) -> Option<usize> {
        let id = self.by_port.get(&(family, port))?.demux?;
        self.by_id.get(&id).copied()
    }

    /// Whether any live socket already holds local `port`.
    fn port_in_use(&self, family: NetAddrFamily, port: u16) -> bool {
        self.by_port
            .get(&(family, port))
            .is_some_and(|slot| slot.holders > 0)
    }

    /// Allocate a socket handle not currently held by any live socket.
    fn alloc_id(&mut self) -> SocketId {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            let id = self.next_id;
            if id != 0 && !self.by_id.contains_key(&id) {
                return id;
            }
        }
    }
}

/// Drain every segment a connection's TCB wants transmitted into owned
/// `(header, payload)` pairs, so the engine's `send_tcp` can be called
/// without holding a borrow of the socket table across it.
fn drain_segments(tcb: &mut Tcb, now: Duration64) -> Vec<OutSeg> {
    let mut segs = Vec::new();
    tcb.poll_transmit(now, |out| {
        segs.push((out.meta, out.payload.to_vec(), out.gso_size, out.ecn));
        true
    });
    segs
}

/// Route a batch of listener-emitted `(header, payload)` segments to a
/// single peer, choosing the egress interface by the route to that peer,
/// and return the produced frames tagged by that interface's alias. An
/// empty batch, or a peer with no route home, yields nothing (the listener
/// is passive; a lost SYN-ACK is retransmitted by `advance`).
fn route_segments_to(
    interfaces: &mut Netstack,
    peer_ip: IpAddr,
    segs: &[OutSeg],
    now: Duration64,
) -> FrameBatch {
    if segs.is_empty() {
        return FrameBatch::new();
    }
    let Ok((iface, _mss)) = interfaces.egress_mss_for(peer_ip, now) else {
        return FrameBatch::new();
    };
    let mut frames = Vec::new();
    for (meta, payload, gso_size, ecn) in segs {
        if let Ok(more) =
            interfaces.send_tcp_on(iface, peer_ip, meta, payload, *gso_size, *ecn, now)
        {
            frames.extend(more);
        }
    }
    if frames.is_empty() {
        FrameBatch::new()
    } else {
        alloc::vec![(iface, frames)]
    }
}

/// The local ports a broadcast IPv4 datagram could be delivered to: every
/// bound IPv4 datagram socket's port.
///
/// Deliberately not narrowed by the socket's bound local address. A socket
/// bound to a specific unicast address cannot in fact match a broadcast
/// destination, so including its port costs at most one wasted parse; but a
/// socket bound to the broadcast address itself *can*, and excluding that
/// would lose datagrams. The safe direction is to admit.
fn broadcast_consumer_ports(sockets: &[SocketEntry]) -> impl Iterator<Item = u16> + '_ {
    sockets.iter().filter_map(|entry| {
        let bound = entry.local_port != 0;
        let datagram = matches!(entry.proto, Proto::Datagram(_));
        (bound && datagram && entry.family == NetAddrFamily::V4).then_some(entry.local_port)
    })
}

/// Encode a stream event and push it as a delivery to `deliver_port`. A
/// bounded, valid event always encodes; an encode failure is dropped
/// rather than delivering a malformed frame (fail closed).
fn push_stream_event(out: &mut Vec<Delivery>, deliver_port: u64, event: &SocketStreamEvent<'_>) {
    let mut buf = alloc::vec![0u8; SocketStreamEvent::MAX_WIRE_LEN];
    if let Ok(len) = event.encode(&mut buf) {
        buf.truncate(len);
        out.push(Delivery {
            deliver_port,
            datagram: buf,
        });
    }
}

/// The pseudo-header context for a segment received from `source` to our
/// `destination`.
fn pseudo_for(source: IpAddr, destination: IpAddr) -> Pseudo {
    match (source, destination) {
        (IpAddr::V4(s), IpAddr::V4(d)) => Pseudo::V4 {
            source: s,
            destination: d,
        },
        (IpAddr::V6(s), IpAddr::V6(d)) => Pseudo::V6 {
            source: s,
            destination: d,
        },
        // Mixed families never arrive together from one IP packet; fold a
        // v4 context (the checksum will simply not verify).
        _ => Pseudo::V4 {
            source: Ipv4Addr::UNSPECIFIED,
            destination: Ipv4Addr::UNSPECIFIED,
        },
    }
}

/// Map a connection's abort reason onto the client-visible close reason.
fn map_reset(reason: ResetReason) -> StreamCloseReason {
    match reason {
        ResetReason::ConnectionRefused => StreamCloseReason::Refused,
        ResetReason::TimedOut => StreamCloseReason::TimedOut,
        ResetReason::ConnectionReset | ResetReason::Aborted => StreamCloseReason::Reset,
    }
}

/// Whether the peer has closed its send direction (a FIN was received and
/// every byte before it delivered): the client's `recv` now sees
/// end-of-stream.
fn peer_closed(state: State) -> bool {
    matches!(
        state,
        State::CloseWait | State::Closing | State::LastAck | State::TimeWait | State::Closed
    )
}

/// Write the success status frame into `response`.
/// The link event a datagram socket holding memberships is owed for one
/// edge, or `None` for a socket that holds none.
/// Posts one message to a delivery port.
pub type Post<'a> = dyn FnMut(u64, &[u8]) -> Result<(), Errno> + 'a;

/// How far one socket's view was brought to the published links.
enum Told {
    /// It holds exactly what is published.
    Caught,
    /// Its port had no room; the rest waits for it to drain.
    Blocked,
    /// Its view could not grow for want of memory; the next call resumes.
    Short,
}

/// Bring one socket's told view to `links`, one event at a time: a link's
/// view changes only once its event has landed, so an interrupted pass
/// resumes exactly where it stopped. A port that is gone takes nothing
/// further and reads as caught up, as a datagram to it would be dropped.
fn tell_one(
    socket: SocketId,
    family: NetAddrFamily,
    port: u64,
    proto: &mut Proto,
    links: &[PublishedLink],
    send: &mut Post<'_>,
) -> Told {
    let Proto::Datagram(dg) = proto else {
        return Told::Caught;
    };
    if dg.groups.is_empty() {
        dg.told.clear();
        dg.listed = false;
        return Told::Caught;
    }
    let mut landed = |interface, up| {
        let Ok(frame) = (SocketLinkEvent {
            socket,
            interface,
            up,
        })
        .encode() else {
            return true;
        };
        !matches!(send(port, &frame), Err(Errno::WouldBlock))
    };
    let mut at = 0;
    while at < dg.told.len() {
        let told = dg.told[at];
        if links
            .iter()
            .any(|link| link.family == family && link.name == told.name)
        {
            at += 1;
            continue;
        }
        if told.up && !landed(told.name, false) {
            return Told::Blocked;
        }
        dg.told.swap_remove(at);
    }
    for link in links.iter().filter(|link| link.family == family) {
        let Some(at) = told_index(&dg.told, link.name) else {
            if dg.told.try_reserve(1).is_err() {
                return Told::Short;
            }
            if link.up && !landed(link.name, true) {
                return Told::Blocked;
            }
            dg.told.push(*link);
            continue;
        };
        if dg.told[at].epoch == link.epoch {
            continue;
        }
        // Up then up again under a new epoch is a flap: said as down first,
        // so the reader drops what it learned before it.
        if dg.told[at].up {
            if !landed(link.name, false) {
                return Told::Blocked;
            }
            dg.told[at].up = false;
        }
        if link.up && !landed(link.name, true) {
            return Told::Blocked;
        }
        dg.told[at] = *link;
    }
    dg.listed = false;
    Told::Caught
}

fn told_index(told: &[PublishedLink], name: [u8; IF_NAME_LEN]) -> Option<usize> {
    told.iter().position(|link| link.name == name)
}

/// The multicast DNS port every mDNS message is sent from and to.
const MDNS_PORT: u16 = tairix_net::mdns::PORT;

/// Whether `request` binds the multicast DNS port — in any transport, since
/// they share one port space — or addresses a multicast DNS group. Both are
/// the discovery service's alone: a query sent to the group from any other
/// port is answered unicast by every responder on the segment, and a second
/// listener on the port would hear the segment's answers without its checks.
fn claims_multicast_dns(request: &SocketRequest<'_>) -> bool {
    match *request {
        SocketRequest::Bind { local, .. } => local.port == MDNS_PORT,
        SocketRequest::Connect { peer, .. } => is_multicast_dns(peer),
        SocketRequest::Send { dest, .. } => dest.is_some_and(is_multicast_dns),
        _ => false,
    }
}

/// Whether `peer` is a multicast DNS group on the mDNS port.
fn is_multicast_dns(peer: SocketAddr) -> bool {
    peer.port == MDNS_PORT
        && match ip_of(peer) {
            IpAddr::V4(group) => group == tairix_net::mdns::GROUP_V4,
            IpAddr::V6(group) => group == tairix_net::mdns::GROUP_V6,
        }
}

/// Whether the caller is the discovery service's account, the one principal
/// the multicast DNS port and groups are reserved to.
fn is_discovery(caller: &Caller) -> bool {
    tairix_abi::discovery_ipc::from_discovery_service(caller.origin())
}

fn status_reply(response: &mut [u8]) -> Result<SocketReply, Errno> {
    if response.len() < STATUS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    response[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
    Ok(SocketReply {
        len: STATUS_REPLY_LEN,
        tx: Vec::new(),
        deliveries: Vec::new(),
        watch: None,
    })
}

/// Build a new listener's [`ListenConfig`] from the stack-wide policy.
///
/// `net.tcp.syncookies always` sets `max_half_open = 0`, so the listener
/// holds no half-open state and answers every SYN with a stateless RFC
/// 4987 cookie (the unconditional-defence mode); `auto` keeps the bounded
/// default backlog, falling back to cookies only once it overflows.
/// `net.tcp.keepalive` sets the accepted-connection template's
/// `enable_keepalive`, so an inbound connection is probed on an idle link
/// exactly as an outbound one is. `net.tcp.ecn` likewise sets the
/// template's `enable_ecn`, so an accepted connection negotiates RFC 3168
/// ECN exactly as an outbound one does.
pub(crate) fn listen_config(settings: NetworkSettings, allowance: u64) -> Option<ListenConfig> {
    let default = ListenConfig::default();
    // Each connection the listener completes is given a window out of the
    // allowance, exactly as an actively-opened one is.
    let buffer = NetworkSettings::connection_buffer_bytes(allowance)?;
    let mut config = ListenConfig {
        // The half-open backlog is the SYN-flood brake, charged to the
        // budget but never sized by it: a defence does not shrink because
        // memory is tight. `syncookies_always` is the one policy that sets
        // it aside, in favour of answering statelessly.
        max_half_open: if settings.syncookies_always {
            0
        } else {
            default.max_half_open
        },
        template: TcpConfig {
            send_buffer: buffer,
            receive_buffer: buffer,
            enable_keepalive: settings.tcp_keepalive,
            enable_ecn: settings.tcp_ecn,
            ..TcpConfig::default()
        },
        ..default
    };
    // How many completed connections may wait at once is the one part of a
    // listener only remote peers decide, so it is what the allowance sets.
    // `lib/net` owns the arithmetic, because it knows what its own backlog
    // and queue entries cost.
    if !config.fit_within(usize::try_from(allowance).unwrap_or(usize::MAX)) {
        return None;
    }
    Some(config)
}

/// Widen a byte count to the budget's width, saturating rather than
/// wrapping where a 32-bit target's `usize` is the narrower of the two.
fn widen(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// [`widen`] for a `const` context, where `TryFrom` is not available.
const fn widen_const(bytes: usize) -> u64 {
    // `usize` is never wider than `u64` on any target TAIRiX builds for, so
    // this is a widening on every one of them.
    bytes as u64
}

/// Audit an after-capability refusal and return it as the typed error.
fn refuse(audit: &dyn Sink, message: &str, err: Errno) -> Result<SocketReply, Errno> {
    emit(audit, Level::Warn, events::SOCKET_REFUSED, message, &[]);
    Err(err)
}

/// The IP address a [`SocketAddr`] denotes.
fn ip_of(addr: SocketAddr) -> IpAddr {
    ip_from_parts(addr.family, addr.addr)
}

/// Whether a [`SocketAddr`] names a multicast group.
fn is_multicast_addr(addr: SocketAddr) -> bool {
    is_multicast_ip(ip_of(addr))
}

/// Whether an [`IpAddr`] is a multicast address.
fn is_multicast_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_multicast(),
        IpAddr::V6(a) => a.is_multicast(),
    }
}

/// Add every counter of `b` into `a`, saturating rather than wrapping so a
/// long-lived stack cannot roll a defence total back to zero.
fn fold_defence(a: ListenerStats, b: ListenerStats) -> ListenerStats {
    ListenerStats {
        half_open_started: a.half_open_started.saturating_add(b.half_open_started),
        cookies_sent: a.cookies_sent.saturating_add(b.cookies_sent),
        cookies_accepted: a.cookies_accepted.saturating_add(b.cookies_accepted),
        cookies_rejected: a.cookies_rejected.saturating_add(b.cookies_rejected),
        accepted: a.accepted.saturating_add(b.accepted),
        accept_overflow: a.accept_overflow.saturating_add(b.accept_overflow),
        half_open_expired: a.half_open_expired.saturating_add(b.half_open_expired),
        resets_sent: a.resets_sent.saturating_add(b.resets_sent),
    }
}

/// The operation name an audit record carries.
fn op_field(request: &SocketRequest<'_>) -> Field<'static> {
    let op = match request {
        SocketRequest::Socket { .. } => "socket",
        SocketRequest::Bind { .. } => "bind",
        SocketRequest::Connect { .. } => "connect",
        SocketRequest::Send { .. } => "send",
        SocketRequest::Close { .. } => "close",
        SocketRequest::Shutdown { .. } => "shutdown",
        SocketRequest::JoinMulticast { .. } => "join",
        SocketRequest::LeaveMulticast { .. } => "leave",
        SocketRequest::Listen { .. } => "listen",
        SocketRequest::Accept { .. } => "accept",
        SocketRequest::SendEcho { .. } => "send_echo",
    };
    Field {
        key: "op",
        value: FieldValue::Str(op),
    }
}

/// Emit one structured audit record.
fn emit(audit: &dyn Sink, level: Level, id: EventId, message: &str, fields: &[Field<'_>]) {
    log(
        audit,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}
