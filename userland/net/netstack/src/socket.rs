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
    SocketId, SocketRequest, SocketStreamEvent, SocketType, StreamCloseReason, SOCKET_MAX_DATAGRAM,
};
use tairix_abi::net_ipc::{NetAddrFamily, IF_NAME_LEN};
use tairix_abi::origin::ProcId;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::{CapabilityId, Duration64, Errno};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use tairix_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::checksum::Pseudo;
use tairix_net::stack::StackEvent;
use tairix_net::tcp::conn::{ResetReason, State, Tcb, TcpConfig};
use tairix_net::tcp::{TcpSegment, TcpSegmentMeta};

use crate::events;
use crate::iface::{FrameBatch, Netstack};
use crate::service::Caller;

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
    /// Whether the client has been told the handshake completed.
    connected_notified: bool,
    /// Whether the client has been told the connection ended.
    closed_notified: bool,
    /// Whether the client has issued `close`: the connection is being
    /// torn down in the background and is reaped once fully closed. No
    /// further events are delivered (the client is gone).
    client_closed: bool,
}

/// The transport-specific state of one socket.
enum Proto {
    /// A connectionless UDP datagram socket.
    Datagram(DatagramState),
    /// A connection-oriented TCP stream socket; `None` until connected.
    /// The connection (which carries the sizeable [`Tcb`]) is boxed so a
    /// datagram socket's table entry stays small.
    Stream(Option<Box<StreamConn>>),
}

/// One open socket, owned by exactly one principal.
struct SocketEntry {
    /// Server-assigned handle, unique among all live sockets.
    id: SocketId,
    /// The unforgeable process instance that opened it.
    owner: ProcId,
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
            } => self.open(audit, owner, family, sock_type, deliver_port, response),
            SocketRequest::Bind { socket, local } => {
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
        }
    }

    /// Open a socket of the requested transport, accounting it against the
    /// caller's quota and the global table.
    fn open(
        &mut self,
        audit: &dyn Sink,
        owner: ProcId,
        family: NetAddrFamily,
        sock_type: SocketType,
        deliver_port: u64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
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
            Proto::Stream(None) => {
                self.connect_stream(interfaces, entropy, index, peer, now, response)
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
        } else {
            let family = self.sockets[index].family;
            let groups = match &mut self.sockets[index].proto {
                Proto::Datagram(dg) => core::mem::take(&mut dg.groups),
                Proto::Stream(_) => Vec::new(),
            };
            for group in &groups {
                let ip = family_addr_to_ip(family, *group);
                tx.extend(interfaces.leave_multicast_all(ip, now));
            }
            self.sockets.swap_remove(index);
        }
        let len = status_reply(response)?.len;
        Ok(SocketReply { len, tx })
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
            Proto::Stream(_) => None,
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
                Ok(SocketReply { len, tx })
            }
            Err(err) => refuse(audit, "socket send refused", err),
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
        // The ISN is a CSPRNG draw (the engine makes no randomness).
        let iss = entropy();
        let mut tcb = Tcb::connect(TcpConfig::default(), local_port, peer.port, iss, now);
        let segs = drain_segments(&mut tcb, now);
        let dest = ip_of(peer);
        let Some(((meta0, payload0), rest)) = segs.split_first() else {
            return Err(Errno::NetworkUnreachable);
        };
        let (iface, mut frames) = interfaces.choose_tcp_egress(dest, meta0, payload0, now)?;
        for (meta, payload) in rest {
            if let Ok(more) = interfaces.send_tcp_on(iface, dest, meta, payload, now) {
                frames.extend(more);
            }
        }
        self.sockets[index].proto = Proto::Stream(Some(Box::new(StreamConn {
            tcb,
            peer,
            iface,
            connected_notified: false,
            closed_notified: false,
            client_closed: false,
        })));
        let tx = if frames.is_empty() {
            FrameBatch::new()
        } else {
            alloc::vec![(iface, frames)]
        };
        let len = status_reply(response)?.len;
        Ok(SocketReply { len, tx })
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
        Ok(SocketReply { len, tx })
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
        for (meta, payload) in &segs {
            if let Ok(more) = interfaces.send_tcp_on(iface, dest, meta, payload, now) {
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
    /// engine) to the connection its four-tuple names, driving the
    /// resulting egress segments and client-visible stream events.
    ///
    /// A segment matching no connected stream is dropped (this increment
    /// serves only active opens; passive listeners and stateless RSTs are
    /// N6). It never panics and returns bounded output.
    #[must_use]
    pub fn on_tcp_segment(
        &mut self,
        interfaces: &mut Netstack,
        source: IpAddr,
        destination: IpAddr,
        segment: &[u8],
        now: Duration64,
    ) -> StreamIo {
        let pseudo = pseudo_for(source, destination);
        let Some(seg) = TcpSegment::parse(pseudo, segment) else {
            return StreamIo::default();
        };
        let (fam, src_bytes) = parts_of(source);
        let (dst_port, src_port) = (seg.destination_port, seg.source_port);
        let Some(index) = self.sockets.iter().position(|e| {
            e.family == fam
                && e.local_port == dst_port
                && matches!(&e.proto, Proto::Stream(Some(c))
                    if c.peer.port == src_port && c.peer.addr == src_bytes)
        }) else {
            return StreamIo::default();
        };
        if let Proto::Stream(Some(conn)) = &mut self.sockets[index].proto {
            conn.tcb.on_segment(&seg, now);
        }
        let tx = self.pump_stream(interfaces, index, now);
        let deliveries = self.collect_stream_events(index);
        self.reap_if_done(index);
        StreamIo { tx, deliveries }
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
            if matches!(self.sockets[i].proto, Proto::Stream(Some(_))) {
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
        if conn.closed_notified {
            return out;
        }
        if conn.tcb.is_established() && !conn.connected_notified {
            conn.connected_notified = true;
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
            conn.closed_notified = true;
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
        Ok(SocketReply { len, tx })
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
        Ok(SocketReply { len, tx })
    }

    /// Route one engine [`StackEvent::UdpDatagram`] to the datagram
    /// sockets that should receive it, returning an encoded
    /// [`SocketDatagram`] delivery per matching socket. A non-datagram
    /// event yields nothing.
    #[must_use]
    pub fn deliver(&self, event: &StackEvent) -> Vec<Delivery> {
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
fn drain_segments(tcb: &mut Tcb, now: Duration64) -> Vec<(TcpSegmentMeta, Vec<u8>)> {
    let mut segs = Vec::new();
    tcb.poll_transmit(now, |out| {
        segs.push((out.meta, out.payload.to_vec()));
        true
    });
    segs
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
    })
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
