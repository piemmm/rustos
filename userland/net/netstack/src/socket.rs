//! The datagram-socket service: the origin-keyed socket table and the
//! capability-checked dispatcher that serves the `netsock-v1` control
//! plane (`plans/NETWORK.md` N4b).
//!
//! Sockets are entirely stack/userland state — the kernel owns no socket
//! object. This module is the pure engine of that service: it owns the
//! socket table, decides port assignment and delivery, and drives the
//! [`Netstack`] interface table to originate datagrams. All I/O (the
//! endpoint recv/reply, the delivery `ipc_send`, the CSPRNG draw) is the
//! thin `Run`-binary glue's job; the engine takes its entropy through an
//! injected closure and returns the frames and deliveries for the glue to
//! move, so it stays host-testable.
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
//!
//! # Lifecycle
//!
//! A socket is released by an explicit [`SocketRequest::Close`]. Reclaim
//! on process exit rides on the process-exit notification the service
//! consumes once the NIC-autobind data path is wired (a later increment);
//! until then the bounded, fail-closed global table is the backstop, so a
//! principal that never closes cannot exhaust memory — it exhausts only
//! its own quota.

use alloc::vec::Vec;

use tairix_abi::net::{
    encode_bind_reply, encode_socket_reply, SocketAddr, SocketDatagram, SocketId, SocketRequest,
    SocketType,
};
use tairix_abi::net_ipc::NetAddrFamily;
use tairix_abi::origin::ProcId;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::{CapabilityId, Duration64, Errno};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use tairix_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::stack::StackEvent;

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

/// One open datagram socket, owned by exactly one principal.
struct SocketEntry {
    /// Server-assigned handle, unique among all live sockets.
    id: SocketId,
    /// The unforgeable process instance that opened it.
    owner: ProcId,
    /// The client async port inbound datagrams are delivered to.
    deliver_port: u64,
    /// Address family of the socket.
    family: NetAddrFamily,
    /// Bound local address; unspecified (all-zero) means "any".
    local_addr: [u8; 16],
    /// Bound local port; `0` means unbound.
    local_port: u16,
    /// Connected default peer, if [`SocketRequest::Connect`] was called.
    peer: Option<SocketAddr>,
    /// Multicast groups this socket joined (for leave-on-close).
    groups: Vec<[u8; 16]>,
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

/// One inbound datagram to deliver to a socket's client: the async port to
/// `ipc_send` it to, and the encoded [`SocketDatagram`] payload.
#[derive(Debug, PartialEq, Eq)]
pub struct Delivery {
    /// The client async port the datagram is sent to.
    pub deliver_port: u64,
    /// The encoded [`SocketDatagram`] frame.
    pub datagram: Vec<u8>,
}

/// The socket table and its dispatcher.
#[derive(Default)]
pub struct SocketService {
    sockets: Vec<SocketEntry>,
    /// Rolling handle allocator; the next candidate id, advanced past any
    /// live collision so a delivered datagram can never alias a reused id.
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
    /// Decodes the [`SocketRequest`] from `request`, enforces `CAP_NET`
    /// against the caller's attested origin **before any state is
    /// touched**, applies it against the socket table and `interfaces`,
    /// writes the encoded reply into `response`, and returns the reply
    /// length plus any frames to transmit. `entropy` yields CSPRNG words
    /// for ephemeral-port selection (injected so the engine stays pure).
    ///
    /// Fails closed: a malformed frame, a missing capability, an unknown
    /// or unowned handle, a full quota, or a refused send each return a
    /// typed [`Errno`] and leave `response` unspecified — the transport
    /// loop frames the error as a status reply.
    ///
    /// # Errors
    ///
    /// * [`Errno::PermissionDenied`] — the caller lacks `CAP_NET`.
    /// * [`Errno::LimitExceeded`] — a bounded per-principal or global
    ///   table is full.
    /// * [`Errno::NotFound`] — the request named a handle the caller does
    ///   not own (existence is not leaked across principals).
    /// * [`Errno::AddressInUse`] / [`Errno::AddressUnavailable`] /
    ///   [`Errno::NetworkUnreachable`] / [`Errno::NotConnected`] — a bind,
    ///   address, or send refusal.
    /// * A frame-decode [`Errno`] — the request failed to decode.
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

        // Capability check before any state is read or mutated.
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
                self.connect(entropy, owner, socket, peer, response)
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

    /// Open a socket: account it against the caller's quota and the global
    /// table, then record its delivery port.
    fn open(
        &mut self,
        audit: &dyn Sink,
        owner: ProcId,
        family: NetAddrFamily,
        sock_type: SocketType,
        deliver_port: u64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        // N4 serves datagram sockets only; the decoder already rejects
        // other types, but re-check so an ABI change cannot silently
        // admit an unserved type.
        let SocketType::Datagram = sock_type;
        // A zero delivery port could never receive a datagram.
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
        let id = self.alloc_id();
        self.sockets.push(SocketEntry {
            id,
            owner,
            deliver_port,
            family,
            local_addr: [0u8; 16],
            local_port: 0,
            peer: None,
            groups: Vec::new(),
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
    /// port when the request asks for `0`.
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
        // A specified local address must be owned by some interface.
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

    /// Set a socket's default peer, implicitly binding an ephemeral local
    /// port when the socket is still unbound (POSIX `connect` semantics).
    fn connect(
        &mut self,
        entropy: &mut dyn FnMut() -> u32,
        owner: ProcId,
        socket: SocketId,
        peer: SocketAddr,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        if peer.family != self.sockets[index].family {
            return Err(Errno::OutOfRange);
        }
        if self.sockets[index].local_port == 0 {
            let port = self.assign_port(entropy, 0)?;
            self.sockets[index].local_port = port;
        }
        self.sockets[index].peer = Some(peer);
        status_reply(response)
    }

    /// Send one datagram from a socket, implicitly binding an ephemeral
    /// local port on first send.
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
        let family = self.sockets[index].family;
        let Some(target) = dest.or(self.sockets[index].peer) else {
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

    /// Close a socket, leaving every multicast group it still holds.
    fn close(
        &mut self,
        interfaces: &mut Netstack,
        owner: ProcId,
        socket: SocketId,
        now: Duration64,
        response: &mut [u8],
    ) -> Result<SocketReply, Errno> {
        let index = self.owned_index(owner, socket)?;
        let entry = self.sockets.swap_remove(index);
        let mut tx = FrameBatch::new();
        for group in &entry.groups {
            let ip = family_addr_to_ip(entry.family, *group);
            tx.extend(interfaces.leave_multicast_all(ip, now));
        }
        let len = status_reply(response)?.len;
        Ok(SocketReply { len, tx })
    }

    /// Join a multicast group on the socket, refcounted per membership.
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
        if group.family != self.sockets[index].family || !is_multicast_addr(group) {
            return Err(Errno::OutOfRange);
        }
        if self.sockets[index].groups.contains(&group.addr) {
            // Idempotent per socket: already a member.
            return status_reply(response);
        }
        if self.sockets[index].groups.len() >= MAX_GROUPS_PER_SOCKET {
            return Err(Errno::LimitExceeded);
        }
        let tx = interfaces.join_multicast_all(ip_of(group), now)?;
        self.sockets[index].groups.push(group.addr);
        let len = status_reply(response)?.len;
        Ok(SocketReply { len, tx })
    }

    /// Leave a multicast group the socket had joined.
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
        let Some(pos) = self.sockets[index]
            .groups
            .iter()
            .position(|g| *g == group.addr)
        else {
            // Leaving a group never joined is a no-op success.
            return status_reply(response);
        };
        self.sockets[index].groups.swap_remove(pos);
        let tx = interfaces.leave_multicast_all(ip_of(group), now);
        let len = status_reply(response)?.len;
        Ok(SocketReply { len, tx })
    }

    /// Route one engine [`StackEvent`] to the sockets that should receive
    /// it, returning an encoded [`SocketDatagram`] delivery per matching
    /// socket. A non-datagram event yields nothing.
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
            if entry.family != dest_family || entry.local_port != *destination_port {
                continue;
            }
            // Destination match: a joined group for multicast, else the
            // bound local address (or any).
            let dest_ok = if dest_multicast {
                entry.groups.contains(&dest_bytes)
            } else {
                entry.local_addr == [0u8; 16] || entry.local_addr == dest_bytes
            };
            if !dest_ok {
                continue;
            }
            // Connected sockets only receive from their peer.
            if let Some(peer) = entry.peer {
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
