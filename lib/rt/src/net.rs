//! Datagram-socket client wrappers over the `netsock-v1` contract
//! (`plans/NETWORK.md` N4, `tairix_abi::net`).
//!
//! These are the pure-Rust client half of the socket ABI: thin marshalling
//! over the kernel-brokered [`crate::ipc_call`] to the reserved
//! [`NETSTACK_SOCKET_ENDPOINT`] (control plane) and [`crate::ipc_recv`] on
//! the client's own delivery port (receive plane). They add **no**
//! authority — every capability and input check stays kernel- and
//! stack-side ([`CAP_NET`](tairix_abi::CapabilityId::NET) is enforced by
//! the `netstack` dispatcher). A non-Rust program reaches the same
//! contract through the generated C ABI; this is the first-party path.
//!
//! # Receive plane
//!
//! Inbound datagrams — and, for a socket holding group memberships, each
//! change in where they are live — are delivered by the stack as
//! [`SocketDelivery`] frames sent to the async **port** the client bound and
//! named in [`socket`]. The client parks on that port and drains it with
//! [`recv`].
//! A delivery port is an inbox any process may post to, so every receive
//! here authenticates the sender as the stack by its kernel-attested
//! service account ([`from_network_stack`]) and discards anything else
//! (fail closed): a caller is only ever handed the stack's own deliveries.

use tairix_abi::net::{
    decode_bind_reply, decode_send_reply, decode_socket_reply, from_network_stack, ShutdownHow,
    SocketAddr, SocketDelivery, SocketEcho, SocketId, SocketRequest, SocketStreamEvent, SocketType,
    NETSTACK_SOCKET_ENDPOINT, SOCKET_MAX_REPLY,
};
use tairix_abi::net_ipc::{NetAddrFamily, IF_NAME_LEN};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::{Errno, Origin};

use crate::{ipc_call, ipc_recv};

/// Largest fixed control-plane request header (no payload): comfortably
/// covers every [`SocketRequest`] but `Send`, which sizes its own buffer.
const REQUEST_HEADER_MAX: usize = 64;

const _: () = assert!(SocketRequest::HEADER_LEN <= REQUEST_HEADER_MAX);

/// Open a datagram socket of `family`, delivering inbound datagrams to the
/// async port `deliver_port` (an endpoint the caller has already bound).
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::PermissionDenied`]
/// without `CAP_NET`, [`Errno::LimitExceeded`] at the socket-memory budget, or a
/// transport error.
pub fn socket(family: NetAddrFamily, deliver_port: u64) -> Result<SocketId, Errno> {
    let request = SocketRequest::Socket {
        family,
        sock_type: SocketType::Datagram,
        deliver_port,
    };
    let mut buf = [0u8; REQUEST_HEADER_MAX];
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_socket_reply(&reply[..len])
}

/// Open a connection-oriented TCP stream socket of `family`, delivering
/// inbound stream events ([`SocketStreamEvent`]) to the async port
/// `deliver_port` (an endpoint the caller has already bound).
///
/// The socket is created unconnected; call [`connect`] to actively open a
/// connection to a peer. The stack then delivers exactly one
/// [`Connected`](SocketStreamEvent::Connected), zero or more
/// [`Data`](SocketStreamEvent::Data), and exactly one
/// [`Closed`](SocketStreamEvent::Closed) to `deliver_port` over the
/// connection's life (drain them with [`stream_recv`]).
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::PermissionDenied`]
/// without `CAP_NET`, [`Errno::LimitExceeded`] at the socket-memory budget, or a
/// transport error.
pub fn stream_socket(family: NetAddrFamily, deliver_port: u64) -> Result<SocketId, Errno> {
    let request = SocketRequest::Socket {
        family,
        sock_type: SocketType::Stream,
        deliver_port,
    };
    let mut buf = [0u8; REQUEST_HEADER_MAX];
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_socket_reply(&reply[..len])
}

/// Send stream bytes on a connected stream `socket`, returning the number
/// of bytes the stack accepted into the connection's send buffer (which
/// may be fewer than offered when the buffer is momentarily full — the
/// caller resends the remainder). A stream `send` never carries a
/// destination (the peer is fixed at [`connect`]).
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::NotConnected`] if the
/// connection is not established (or has closed), or a transport error.
pub fn stream_send(socket: SocketId, payload: &[u8]) -> Result<u32, Errno> {
    let request = SocketRequest::Send {
        socket,
        dest: None,
        interface: None,
        payload,
    };
    let mut buf = payload_buffer(payload)?;
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_send_reply(&reply[..len])
}

/// Receive the next inbound stream event the stack delivered to
/// `deliver_port`, decoding it into `buf`. Anything another sender posted
/// there is discarded unread.
///
/// # Errors
///
/// * The raw negative kernel result (as an [`Errno`] via
///   [`Errno::from_syscall`]) if the receive fails — [`Errno::WouldBlock`]
///   once nothing the stack sent is waiting.
/// * A decode [`Errno`] if the stack's message is not a well-formed
///   [`SocketStreamEvent`].
pub fn stream_recv(deliver_port: u64, buf: &mut [u8]) -> Result<SocketStreamEvent<'_>, Errno> {
    let len = recv_from_stack(deliver_port, buf)?;
    SocketStreamEvent::parse(&buf[..len])
}

/// Make a bound stream `socket` passive (LISTEN): it accepts inbound
/// connections on its bound local port instead of originating one.
///
/// [`bind`] the socket to its local port first; binding a privileged
/// (well-known) port needs `CAP_NET_BIND_PRIVILEGED`. When a connection is
/// ready the stack delivers a [`SocketStreamEvent::Accepted`] readiness
/// event to the socket's delivery port; drain it with [`stream_recv`] and
/// claim the connection with [`accept`].
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::OutOfRange`] if the
/// socket is not an unconnected stream socket, [`Errno::AddressUnavailable`]
/// if it is not bound, or a transport error.
pub fn listen(socket: SocketId) -> Result<(), Errno> {
    status_call(&SocketRequest::Listen { socket })
}

/// Claim the next established connection queued on a listening `socket`,
/// returning a new child stream [`SocketId`] whose stream events
/// ([`Connected`](SocketStreamEvent::Connected)/`Data`/`Closed`) are
/// delivered to `deliver_port` (an endpoint the caller has already bound).
///
/// Call this after a [`SocketStreamEvent::Accepted`] readiness event on the
/// listener's port, and repeat until it returns [`Errno::WouldBlock`] (no
/// more connections are ready).
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::WouldBlock`] when no
/// connection is ready, [`Errno::OutOfRange`] if `socket` is not a
/// listener, [`Errno::LimitExceeded`] at the socket-memory budget, or a transport
/// error.
pub fn accept(socket: SocketId, deliver_port: u64) -> Result<SocketId, Errno> {
    let request = SocketRequest::Accept {
        socket,
        deliver_port,
    };
    let mut buf = [0u8; REQUEST_HEADER_MAX];
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_socket_reply(&reply[..len])
}

/// Bind `socket` to a local address and port; a `port` of `0` requests a
/// CSPRNG-drawn ephemeral port. Returns the bound port.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::AddressInUse`],
/// [`Errno::AddressUnavailable`], [`Errno::NotFound`] (unowned handle), or
/// a transport error.
pub fn bind(socket: SocketId, local: SocketAddr) -> Result<u16, Errno> {
    let request = SocketRequest::Bind { socket, local };
    let mut buf = [0u8; REQUEST_HEADER_MAX];
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_bind_reply(&reply[..len])
}

/// Set `socket`'s default peer: later [`send`]s may omit a destination and
/// inbound datagrams are filtered to this peer.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned.
pub fn connect(socket: SocketId, peer: SocketAddr) -> Result<(), Errno> {
    status_call(&SocketRequest::Connect { socket, peer })
}

/// Send one datagram from `socket`. `dest` is [`None`] to use the
/// connected peer (see [`connect`]); `interface` pins the one logical
/// interface it leaves by, or is [`None`] for the stack's own choice.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::NotConnected`] with
/// no `dest` and no connected peer, [`Errno::NetworkUnreachable`] (on a
/// named interface: that one cannot carry it), [`Errno::MessageTooLarge`],
/// [`Errno::OutOfMemory`] when the request cannot be built, or a transport
/// error.
pub fn send(
    socket: SocketId,
    dest: Option<SocketAddr>,
    interface: Option<[u8; IF_NAME_LEN]>,
    payload: &[u8],
) -> Result<(), Errno> {
    let request = SocketRequest::Send {
        socket,
        dest,
        interface,
        payload,
    };
    let mut buf = payload_buffer(payload)?;
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_status_reply(&reply[..len])
}

/// Close `socket`, releasing its handle and leaving any joined groups.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned.
pub fn close(socket: SocketId) -> Result<(), Errno> {
    status_call(&SocketRequest::Close { socket })
}

/// Half-close one or both directions of a connected stream socket, keeping
/// the handle open (POSIX `shutdown`).
///
/// [`ShutdownHow::Write`] sends a FIN after the buffered data and leaves the
/// socket readable, so a client signals end-of-request and still reads the
/// response; [`close`] is what releases the handle. Repeating a direction
/// already shut down succeeds.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::NotConnected`] for an
/// unconnected or listening socket, [`Errno::OutOfRange`] for a datagram or
/// echo socket (only TCP has a FIN).
pub fn shutdown(socket: SocketId, how: ShutdownHow) -> Result<(), Errno> {
    status_call(&SocketRequest::Shutdown { socket, how })
}

/// Open an ICMP/`ICMPv6` echo socket of `family` (the `ping` path),
/// delivering inbound echo replies ([`SocketEcho`]) to the async port
/// `deliver_port` (an endpoint the caller has already bound).
///
/// Opening one requires [`CAP_NET_RAW`](tairix_abi::CapabilityId::NET_RAW),
/// enforced by the stack; the stack owns the ICMP identifier, so a socket
/// only ever receives replies to its own requests.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::PermissionDenied`]
/// without `CAP_NET`/`CAP_NET_RAW`, [`Errno::LimitExceeded`] at the socket
/// quota, or a transport error.
pub fn icmp_echo_socket(family: NetAddrFamily, deliver_port: u64) -> Result<SocketId, Errno> {
    let request = SocketRequest::Socket {
        family,
        sock_type: SocketType::IcmpEcho,
        deliver_port,
    };
    let mut buf = [0u8; REQUEST_HEADER_MAX];
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_socket_reply(&reply[..len])
}

/// Send one ICMP/`ICMPv6` echo request from an echo `socket`. `dest` is
/// [`None`] to use the connected peer (see [`connect`]); its port field is
/// unused (ICMP has none) and must be zero. The caller chooses the
/// `sequence`; the stack owns the identifier.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::NotConnected`] with
/// no `dest` and no connected peer, [`Errno::NetworkUnreachable`],
/// [`Errno::MessageTooLarge`], or a transport error.
pub fn send_echo(
    socket: SocketId,
    dest: Option<SocketAddr>,
    sequence: u16,
    payload: &[u8],
) -> Result<(), Errno> {
    let request = SocketRequest::SendEcho {
        socket,
        dest,
        sequence,
        payload,
    };
    let mut buf = payload_buffer(payload)?;
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(&request, &mut buf, &mut reply)?;
    decode_status_reply(&reply[..len])
}

/// Join multicast `group` on `socket`, so it receives the group's traffic.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::OutOfRange`] for a
/// non-multicast group, [`Errno::LimitExceeded`] at the group quota.
pub fn join_multicast(socket: SocketId, group: SocketAddr) -> Result<(), Errno> {
    status_call(&SocketRequest::JoinMulticast { socket, group })
}

/// Leave multicast `group` previously joined on `socket`.
///
/// # Errors
///
/// The typed [`Errno`] the stack returned.
pub fn leave_multicast(socket: SocketId, group: SocketAddr) -> Result<(), Errno> {
    status_call(&SocketRequest::LeaveMulticast { socket, group })
}

/// Receive the next delivery the stack made to `deliver_port` — a datagram,
/// or a change in where the socket's memberships are live — decoding it into
/// `buf`. Anything another sender posted there is discarded unread.
///
/// # Errors
///
/// * The raw negative kernel result (as an [`Errno`] via
///   [`Errno::from_syscall`]) if the receive fails — [`Errno::WouldBlock`]
///   once nothing the stack sent is waiting.
/// * [`Errno::BadMagic`] / [`Errno::LengthOutOfRange`] / … if the stack's
///   message is not a well-formed [`SocketDelivery`].
pub fn recv(deliver_port: u64, buf: &mut [u8]) -> Result<SocketDelivery<'_>, Errno> {
    let len = recv_from_stack(deliver_port, buf)?;
    SocketDelivery::parse(&buf[..len])
}

/// Receive the next inbound ICMP echo reply the stack delivered to
/// `deliver_port`, decoding it into `buf`. Anything another sender posted
/// there is discarded unread.
///
/// # Errors
///
/// * The raw negative kernel result (as an [`Errno`] via
///   [`Errno::from_syscall`]) if the receive fails — [`Errno::WouldBlock`]
///   once nothing the stack sent is waiting.
/// * [`Errno::BadMagic`] / [`Errno::LengthOutOfRange`] / … if the stack's
///   message is not a well-formed [`SocketEcho`].
pub fn recv_echo(deliver_port: u64, buf: &mut [u8]) -> Result<SocketEcho<'_>, Errno> {
    let len = recv_from_stack(deliver_port, buf)?;
    SocketEcho::parse(&buf[..len])
}

/// A zeroed request buffer for a header and `payload`, refused as
/// [`Errno::OutOfMemory`] rather than aborting when the heap cannot supply it.
fn payload_buffer(payload: &[u8]) -> Result<alloc::vec::Vec<u8>, Errno> {
    tairix_util::fallible::filled(REQUEST_HEADER_MAX + payload.len(), 0u8).ok_or(Errno::OutOfMemory)
}

/// Dequeue messages from `port` into `buf` until one the network stack
/// sent, returning its length.
///
/// Bounded by the port's own mailbox: each pass dequeues a message, so a
/// flood of forged posts costs one pass each and ends at the empty mailbox's
/// [`Errno::WouldBlock`].
fn recv_from_stack(port: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    loop {
        let mut sender = [0u8; tairix_abi::ORIGIN_WIRE_LEN];
        let len = ipc_recv(port, buf, &mut sender).map_err(Errno::from_syscall)?;
        if from_network_stack(&Origin::from_bytes(&sender)?) {
            return Ok(len);
        }
    }
}

/// Encode `request` into `buf`, call the socket endpoint, and return the
/// reply length written into `reply`.
fn call(request: &SocketRequest<'_>, buf: &mut [u8], reply: &mut [u8]) -> Result<usize, Errno> {
    let len = request.encode(buf)?;
    ipc_call(NETSTACK_SOCKET_ENDPOINT, &buf[..len], reply).map_err(Errno::from_syscall)
}

/// A control call whose success reply is the bare status frame.
fn status_call(request: &SocketRequest<'_>) -> Result<(), Errno> {
    let mut buf = [0u8; REQUEST_HEADER_MAX];
    let mut reply = [0u8; SOCKET_MAX_REPLY];
    let len = call(request, &mut buf, &mut reply)?;
    decode_status_reply(&reply[..len])
}
