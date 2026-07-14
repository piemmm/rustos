//! Datagram-socket client wrappers over the `netsock-v1` contract
//! (`plans/NETWORK.md` N4, `rustos_abi::net`).
//!
//! These are the pure-Rust client half of the socket ABI: thin marshalling
//! over the kernel-brokered [`crate::ipc_call`] to the reserved
//! [`NETSTACK_SOCKET_ENDPOINT`] (control plane) and [`crate::ipc_recv`] on
//! the client's own delivery port (receive plane). They add **no**
//! authority — every capability and input check stays kernel- and
//! stack-side ([`CAP_NET`](rustos_abi::CapabilityId::NET) is enforced by
//! the `netstack` dispatcher). A non-Rust program reaches the same
//! contract through the generated C ABI; this is the first-party path.
//!
//! # Receive plane
//!
//! Inbound datagrams are delivered by the stack as
//! [`SocketDatagram`] frames sent to the
//! async **port** the client bound and named in
//! [`socket`]. The client parks on that port and drains it with [`recv`],
//! which authenticates the stack's kernel-attested sender origin and
//! hands back both the decoded datagram and that origin so the caller can
//! reject a forged sender (fail closed — the delivery port is otherwise an
//! unauthenticated inbox).

use rustos_abi::net::{
    decode_bind_reply, decode_socket_reply, SocketAddr, SocketDatagram, SocketId, SocketRequest,
    SocketType, NETSTACK_SOCKET_ENDPOINT, SOCKET_MAX_REPLY,
};
use rustos_abi::net_ipc::NetAddrFamily;
use rustos_abi::reply::decode_status_reply;
use rustos_abi::{Errno, Origin};

use crate::{ipc_call, ipc_recv};

/// Largest fixed control-plane request header (no payload): comfortably
/// covers every [`SocketRequest`] but `Send`, which sizes its own buffer.
const REQUEST_HEADER_MAX: usize = 64;

/// Open a datagram socket of `family`, delivering inbound datagrams to the
/// async port `deliver_port` (an endpoint the caller has already bound).
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::PermissionDenied`]
/// without `CAP_NET`, [`Errno::LimitExceeded`] at the socket quota, or a
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
/// connected peer (see [`connect`]).
///
/// # Errors
///
/// The typed [`Errno`] the stack returned — [`Errno::NotConnected`] with
/// no `dest` and no connected peer, [`Errno::NetworkUnreachable`],
/// [`Errno::MessageTooLarge`], or a transport error.
pub fn send(socket: SocketId, dest: Option<SocketAddr>, payload: &[u8]) -> Result<(), Errno> {
    let request = SocketRequest::Send {
        socket,
        dest,
        payload,
    };
    let mut buf = alloc::vec![0u8; REQUEST_HEADER_MAX + payload.len()];
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

/// Receive one inbound datagram on the delivery port `deliver_port`,
/// decoding it into `buf` and returning the datagram together with the
/// kernel-attested [`Origin`] of the sender.
///
/// The caller **must** verify the returned origin is the network stack
/// before trusting the datagram: the delivery port is otherwise an
/// unauthenticated inbox any process could post to (fail closed).
///
/// # Errors
///
/// * The raw negative kernel result (as an [`Errno`] via
///   [`Errno::from_syscall`]) if the receive fails.
/// * [`Errno::BadMagic`] / [`Errno::LengthOutOfRange`] / … if the message
///   is not a well-formed [`SocketDatagram`], or the sender origin is
///   malformed.
pub fn recv(deliver_port: u64, buf: &mut [u8]) -> Result<(SocketDatagram<'_>, Origin), Errno> {
    let mut sender = [0u8; rustos_abi::ORIGIN_WIRE_LEN];
    let len = ipc_recv(deliver_port, buf, &mut sender).map_err(Errno::from_syscall)?;
    let origin = Origin::from_bytes(&sender)?;
    let datagram = SocketDatagram::parse(&buf[..len])?;
    Ok((datagram, origin))
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
