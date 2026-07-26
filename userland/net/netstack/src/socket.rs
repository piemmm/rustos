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
//! the kernel CSPRNG, and both the per-principal and global socket tables
//! are bounded, failing closed with [`Errno::LimitExceeded`] at capacity.
//! A stream's per-connection send/receive/reassembly buffers are the
//! bounded [`TcpConfig`] capacities, so a hostile peer cannot grow memory.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::net::{
    encode_bind_reply, encode_send_reply, encode_socket_reply, SocketAddr, SocketDatagram,
    SocketEcho, SocketId, SocketRequest, SocketStreamEvent, SocketType, StreamCloseReason,
    SOCKET_MAX_DATAGRAM, SOCKET_PRIVILEGED_PORT_MAX,
};
use tairix_abi::net_ipc::{
    NetAddrFamily, NetSockProto, NetSockState, NetSocketRecord, NetworkSettings, IF_NAME_LEN,
};
use tairix_abi::origin::ProcId;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::{CapabilityId, Duration64, Errno};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use tairix_net::addr::{Ecn, IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::checksum::Pseudo;
use tairix_net::stack::StackEvent;
use tairix_net::tcp::conn::{ResetReason, State, Tcb, TcpConfig};
use tairix_net::tcp::listen::{CookieSecret, ListenConfig, Listener, Peer};
use tairix_net::tcp::{TcpSegment, TcpSegmentMeta};

use crate::events;
use crate::iface::{FrameBatch, Netstack};
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

/// Largest number of sockets a single principal may hold at once.
///
/// A per-principal fail-closed bound so one principal cannot exhaust the
/// table and starve others. It is a security bound (a denial-of-service
/// ceiling), not a scaling capacity, so it stays fixed rather than growing
/// with the machine.
pub const MAX_SOCKETS_PER_PRINCIPAL: usize = 64;

/// Largest number of sockets the service holds in total, across every
/// principal — the global fail-closed backstop.
pub const MAX_SOCKETS_TOTAL: usize = 1024;

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
}

/// Per-socket state of an ICMP/`ICMPv6` echo socket (the `ping` path).
///
/// The socket's stack-assigned ICMP *identifier* lives in the entry's
/// `local_port` field (globally unique across every socket, so a reply
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
    /// The transport-specific state.
    proto: Proto,
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
}

/// The socket table and its dispatcher.
#[derive(Default)]
pub struct SocketService {
    sockets: Vec<SocketEntry>,
    /// Rolling handle allocator; the next candidate id, advanced past any
    /// live collision so a delivered message can never alias a reused id.
    next_id: SocketId,
}

impl SocketService {
    /// An empty socket table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of open sockets across all principals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sockets.len()
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
                payload,
            } => self.send(
                interfaces, entropy, audit, owner, socket, dest, payload, now, response,
            ),
            SocketRequest::Close { socket } => self.close(interfaces, owner, socket, now, response),
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
        if self.sockets.len() >= MAX_SOCKETS_TOTAL
            || self.count_owned(owner) >= MAX_SOCKETS_PER_PRINCIPAL
        {
            return refuse(
                audit,
                "socket open refused: socket quota exhausted",
                Errno::LimitExceeded,
            );
        }
        let proto = match sock_type {
            SocketType::IcmpEcho => Proto::Echo(EchoState { peer: None }),
            SocketType::Datagram => Proto::Datagram(DatagramState {
                peer: None,
                groups: Vec::new(),
            }),
            SocketType::Stream => Proto::Stream(None),
        };
        let id = self.alloc_id();
        self.sockets.push(SocketEntry {
            id,
            owner,
            owner_pid,
            deliver_port,
            family,
            local_addr: [0u8; 16],
            local_port: 0,
            proto,
        });
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
        if local.addr != [0u8; 16] && !interfaces.has_local_address(local.family, local.addr) {
            return Err(Errno::AddressUnavailable);
        }
        let port = self.assign_port(entropy, local.port)?;
        let entry = &mut self.sockets[index];
        entry.local_addr = local.addr;
        entry.local_port = port;
        let len = encode_bind_reply(Ok(port), response)?;
        Ok(SocketReply {
            len,
            tx: Vec::new(),
            deliveries: Vec::new(),
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
                if self.sockets[index].local_port == 0 {
                    let port = self.assign_port(entropy, 0)?;
                    self.sockets[index].local_port = port;
                }
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
                if self.sockets[index].local_port == 0 {
                    let ident = self.assign_port(entropy, 0)?;
                    self.sockets[index].local_port = ident;
                }
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
        payload: &[u8],
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        match self.sockets[index].proto {
            Proto::Datagram(_) => self.send_datagram(
                interfaces, entropy, audit, index, dest, payload, now, response,
            ),
            Proto::Stream(_) => {
                // A connected stream has no per-datagram destination.
                if dest.is_some() {
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
            self.sockets.swap_remove(index);
            self.sockets.retain(|e| {
                !(e.owner == listener_owner
                    && e.family == family
                    && e.local_port == port
                    && matches!(&e.proto, Proto::Stream(Some(c)) if !c.accepted))
            });
        } else {
            let family = self.sockets[index].family;
            let groups = match &mut self.sockets[index].proto {
                Proto::Datagram(dg) => core::mem::take(&mut dg.groups),
                _ => Vec::new(),
            };
            for group in &groups {
                let ip = family_addr_to_ip(family, *group);
                tx.extend(interfaces.leave_multicast_all(ip, now));
            }
            self.sockets.swap_remove(index);
        }
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
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
        self.sockets[index].proto = Proto::Listen(Box::new(Listener::new(
            local_port,
            listen_config(interfaces.settings()),
        )));
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
        if self.sockets[index].local_port == 0 {
            let port = self.assign_port(entropy, 0)?;
            self.sockets[index].local_port = port;
        }
        let source_port = self.sockets[index].local_port;
        match interfaces.originate(ip_of(target), source_port, target.port, payload, now) {
            Ok(tx) => {
                let len = status_reply(response)?.len;
                Ok(SocketReply {
                    len,
                    tx,
                    deliveries: Vec::new(),
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
        if self.sockets[index].local_port == 0 {
            let ident = self.assign_port(entropy, 0)?;
            self.sockets[index].local_port = ident;
        }
        let identifier = self.sockets[index].local_port;
        match interfaces.originate_echo(ip_of(target), identifier, sequence, payload, now) {
            Ok(tx) => {
                let len = status_reply(response)?.len;
                Ok(SocketReply {
                    len,
                    tx,
                    deliveries: Vec::new(),
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
        if self.sockets[index].local_port == 0 {
            let port = self.assign_port(entropy, 0)?;
            self.sockets[index].local_port = port;
        }
        let local_port = self.sockets[index].local_port;
        let dest = ip_of(peer);
        // Bind the egress interface and learn its effective MSS for this
        // family *before* building the TCB, so the SYN advertises — and the
        // connection segments to — a size that fits the link (RFC 6691).
        // The stack stays unconnected (the client may retry) when no
        // interface can reach the peer.
        let (iface, local_mss) = interfaces.egress_mss_for(dest, now)?;
        // The ISN is a CSPRNG draw (the engine makes no randomness).
        let iss = entropy();
        let config = TcpConfig {
            local_mss,
            // Enable segmentation offload when the egress device negotiated
            // it (0 keeps the connection per-MSS).
            tso_max_payload: interfaces.tso_max_payload_on(iface),
            // Probe an idle peer only when the stack-wide `net.tcp.keepalive`
            // policy is enabled (off by default, RFC 1122 §4.2.3.6).
            enable_keepalive: interfaces.settings().tcp_keepalive,
            ..TcpConfig::default()
        };
        let mut tcb = Tcb::connect(config, local_port, peer.port, iss, now);
        let segs = drain_segments(&mut tcb, now);
        let mut frames = Vec::new();
        for (meta, payload, gso_size, ecn) in &segs {
            if let Ok(more) =
                interfaces.send_tcp_on(iface, dest, meta, payload, *gso_size, *ecn, now)
            {
                frames.extend(more);
            }
        }
        self.sockets[index].proto = Proto::Stream(Some(Box::new(StreamConn {
            tcb,
            peer,
            iface,
            notified: Notified::Nothing,
            client_closed: false,
            // An actively-opened connection is the client's from birth.
            accepted: true,
        })));
        let tx = if frames.is_empty() {
            FrameBatch::new()
        } else {
            alloc::vec![(iface, frames)]
        };
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
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
        let (fam, src_bytes) = parts_of(source);
        let (dst_port, src_port) = (seg.destination_port, seg.source_port);
        // 1. An established connection (active open, or a child accepted off
        //    a listener) claims the segment by its full four-tuple.
        if let Some(index) = self.sockets.iter().position(|e| {
            e.family == fam
                && e.local_port == dst_port
                && matches!(&e.proto, Proto::Stream(Some(c))
                    if c.peer.port == src_port && c.peer.addr == src_bytes)
        }) {
            if let Proto::Stream(Some(conn)) = &mut self.sockets[index].proto {
                conn.tcb.on_segment(&seg, ecn, now);
            }
            let tx = self.pump_stream(interfaces, index, now);
            let deliveries = self.collect_stream_events(index);
            self.reap_if_done(index);
            return StreamIo { tx, deliveries };
        }
        // 2. A passive listener on the destination port demultiplexes it.
        if let Some(lindex) = self.sockets.iter().position(|e| {
            e.family == fam && e.local_port == dst_port && matches!(&e.proto, Proto::Listen(_))
        }) {
            return self.drive_listener(interfaces, lindex, source, destination, &seg, now, secret);
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
        if let Proto::Listen(listener) = &mut self.sockets[lindex].proto {
            listener.on_segment(destination, peer, seg, now, secret, |_peer, out| {
                emitted.push((out.meta, out.payload.to_vec(), out.gso_size, out.ecn));
                true
            });
        }
        let mut io = StreamIo::default();
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
        let mut out = Vec::new();
        loop {
            if self.sockets.len() >= MAX_SOCKETS_TOTAL
                || self.count_owned(owner) >= MAX_SOCKETS_PER_PRINCIPAL
            {
                break;
            }
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
            let (pfam, paddr) = parts_of(conn.peer.addr);
            let peer = SocketAddr {
                family: pfam,
                addr: paddr,
                port: conn.peer.port,
            };
            let id = self.alloc_id();
            self.sockets.push(SocketEntry {
                id,
                owner,
                owner_pid,
                // Inherited until `Accept` rebinds it to the client's port.
                deliver_port,
                family,
                local_addr: [0u8; 16],
                local_port,
                proto: Proto::Stream(Some(Box::new(StreamConn {
                    tcb: {
                        // A listener template does not know the egress link,
                        // so segmentation offload is enabled here, once the
                        // accepted child is bound to the interface that
                        // reaches its peer.
                        let mut tcb = conn.tcb;
                        tcb.set_tso_max_payload(interfaces.tso_max_payload_on(iface));
                        tcb
                    },
                    peer,
                    iface,
                    notified: Notified::Nothing,
                    client_closed: false,
                    // Passive: the client must claim it with `Accept`.
                    accepted: false,
                }))),
            });
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
        // window open (§2.16 — the client's port queue is the app buffer).
        loop {
            let mut buf = [0u8; SOCKET_MAX_DATAGRAM];
            let n = conn.tcb.recv(&mut buf);
            if n == 0 {
                break;
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
            self.sockets.swap_remove(index);
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
        let tx = interfaces.join_multicast_all(ip_of(group), now)?;
        if let Proto::Datagram(dg) = &mut self.sockets[index].proto {
            dg.groups.push(group.addr);
        }
        let len = status_reply(response)?.len;
        Ok(SocketReply {
            len,
            tx,
            deliveries: Vec::new(),
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
        })
    }

    /// Route one engine receive [`StackEvent`] to the sockets that should
    /// receive it, returning an encoded delivery per matching socket: a
    /// [`SocketDatagram`] for a [`StackEvent::UdpDatagram`], or a
    /// [`SocketEcho`] for a [`StackEvent::EchoReply`]. Any other event
    /// yields nothing.
    #[must_use]
    pub fn deliver(&self, event: &StackEvent) -> Vec<Delivery> {
        match event {
            StackEvent::UdpDatagram { .. } => self.deliver_datagram(event),
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
        let (src_family, src_bytes) = parts_of(*source);
        let mut out = Vec::new();
        for entry in &self.sockets {
            let Proto::Echo(echo) = &entry.proto else {
                continue;
            };
            // The identifier lives in `local_port` and is globally unique,
            // so at most one socket matches — a reply never crosses sockets.
            if entry.family != src_family || entry.local_port != *identifier {
                continue;
            }
            if let Some(peer) = echo.peer {
                if peer.family != src_family || peer.addr != src_bytes {
                    continue;
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
    fn deliver_datagram(&self, event: &StackEvent) -> Vec<Delivery> {
        let StackEvent::UdpDatagram {
            source,
            destination,
            source_port,
            destination_port,
            payload,
        } = event
        else {
            return Vec::new();
        };
        let (dest_family, dest_bytes) = parts_of(*destination);
        let (src_family, src_bytes) = parts_of(*source);
        let dest_multicast = is_multicast_ip(*destination);
        let mut out = Vec::new();
        for entry in &self.sockets {
            let Proto::Datagram(dg) = &entry.proto else {
                continue;
            };
            if entry.family != dest_family || entry.local_port != *destination_port {
                continue;
            }
            let dest_ok = if dest_multicast {
                dg.groups.contains(&dest_bytes)
            } else {
                entry.local_addr == [0u8; 16] || entry.local_addr == dest_bytes
            };
            if !dest_ok {
                continue;
            }
            if let Some(peer) = dg.peer {
                if peer.family != src_family || peer.addr != src_bytes || peer.port != *source_port
                {
                    continue;
                }
            }
            let datagram = SocketDatagram {
                socket: entry.id,
                source: SocketAddr {
                    family: src_family,
                    addr: src_bytes,
                    port: *source_port,
                },
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

    /// Number of sockets owned by `owner`.
    fn count_owned(&self, owner: ProcId) -> usize {
        self.sockets.iter().filter(|s| s.owner == owner).count()
    }

    /// The table index of the socket `owner` owns bearing `id`, or
    /// [`Errno::NotFound`] — a handle another principal owns is reported as
    /// absent, never distinguished (existence is not leaked).
    fn owned_index(&self, owner: ProcId, id: SocketId) -> Result<usize, Errno> {
        self.sockets
            .iter()
            .position(|s| s.owner == owner && s.id == id)
            .ok_or(Errno::NotFound)
    }

    /// Assign a local port: the requested port if free, or a CSPRNG-drawn
    /// ephemeral one when `requested` is `0`. Ports are globally unique
    /// across all sockets (no silent reuse); fail closed with
    /// [`Errno::AddressInUse`].
    fn assign_port(&self, entropy: &mut dyn FnMut() -> u32, requested: u16) -> Result<u16, Errno> {
        if requested != 0 {
            if self.port_in_use(requested) {
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
            if !self.port_in_use(candidate) {
                return Ok(candidate);
            }
        }
        Err(Errno::AddressInUse)
    }

    /// Whether any live socket already holds local `port`.
    fn port_in_use(&self, port: u16) -> bool {
        self.sockets.iter().any(|s| s.local_port == port)
    }

    /// Allocate a socket handle not currently held by any live socket.
    fn alloc_id(&mut self) -> SocketId {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            let id = self.next_id;
            if id != 0 && !self.sockets.iter().any(|s| s.id == id) {
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
fn status_reply(response: &mut [u8]) -> Result<SocketReply, Errno> {
    if response.len() < STATUS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    response[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
    Ok(SocketReply {
        len: STATUS_REPLY_LEN,
        tx: Vec::new(),
        deliveries: Vec::new(),
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
/// exactly as an outbound one is.
pub(crate) fn listen_config(settings: NetworkSettings) -> ListenConfig {
    ListenConfig {
        max_half_open: if settings.syncookies_always {
            0
        } else {
            ListenConfig::default().max_half_open
        },
        template: TcpConfig {
            enable_keepalive: settings.tcp_keepalive,
            ..TcpConfig::default()
        },
        ..ListenConfig::default()
    }
}

/// Audit an after-capability refusal and return it as the typed error.
fn refuse(audit: &dyn Sink, message: &str, err: Errno) -> Result<SocketReply, Errno> {
    emit(audit, Level::Warn, events::SOCKET_REFUSED, message, &[]);
    Err(err)
}

/// The IP address a [`SocketAddr`] denotes.
fn ip_of(addr: SocketAddr) -> IpAddr {
    family_addr_to_ip(addr.family, addr.addr)
}

/// Build an [`IpAddr`] from a family and its 16-byte address block.
fn family_addr_to_ip(family: NetAddrFamily, addr: [u8; 16]) -> IpAddr {
    match family {
        NetAddrFamily::V4 => IpAddr::V4(Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3])),
        NetAddrFamily::V6 => IpAddr::V6(Ipv6Addr::from(addr)),
    }
}

/// The family and 16-byte block of an [`IpAddr`].
fn parts_of(ip: IpAddr) -> (NetAddrFamily, [u8; 16]) {
    match ip {
        IpAddr::V4(a) => {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&a.octets());
            (NetAddrFamily::V4, bytes)
        }
        IpAddr::V6(a) => (NetAddrFamily::V6, a.octets()),
    }
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

/// The operation name an audit record carries.
fn op_field(request: &SocketRequest<'_>) -> Field<'static> {
    let op = match request {
        SocketRequest::Socket { .. } => "socket",
        SocketRequest::Bind { .. } => "bind",
        SocketRequest::Connect { .. } => "connect",
        SocketRequest::Send { .. } => "send",
        SocketRequest::Close { .. } => "close",
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
