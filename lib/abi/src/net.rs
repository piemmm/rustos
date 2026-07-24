//! The datagram-socket ABI (`plans/NETWORK.md` §2.4, N4).
//!
//! This is the versioned, fail-closed wire contract a program uses to open
//! UDP sockets through the user-space network stack (`userland/net/netstack`).
//! It is deliberately a **pure `lib/abi` definition**: the encoders and
//! decoders here are the one shared source of truth the client wrappers
//! ([`crate`]-consuming `lib/rt`) and the serving stack both build on, so the
//! two can never drift.
//!
//! # Transport shape (the microkernel-honest design)
//!
//! The kernel owns *no* socket object — it holds only endpoint plumbing
//! (`plans/NETWORK.md` §2.2). Sockets are entirely stack/userland state:
//!
//! * **Control plane** — [`SocketRequest`]s (`socket`/`bind`/`connect`/
//!   `send`/`close`, multicast `join`/`leave`) are fixed-header request/reply
//!   calls on the reserved, kernel-brokered [`NETSTACK_SOCKET_ENDPOINT`]. Every
//!   request carries the caller's kernel-attested [`crate::Origin`] (the
//!   dispatcher reads it with `call_peer_origin`, never a claimed field), is
//!   capability-checked (`CAP_NET`, `plans/NETWORK.md` §3) before any state is
//!   touched, validated whole, and refused with one typed [`crate::Errno`].
//!   `CAP_NET` and the serving dispatcher land together in the socket-service
//!   increment (`plans/NETWORK.md` N4b); this module is only the wire contract
//!   they build on.
//! * **Receive plane** — inbound datagrams are *not* a round-trip. When the
//!   stack has a datagram for a socket it [`crate::SyscallNumber::IPC_SEND`]s a
//!   framed [`SocketDatagram`] to the async **port** the client bound and named
//!   in [`SocketRequest::Socket`]. The client parks on that port
//!   ([`crate::WaitSourceKind::Port`]) and drains it with
//!   [`crate::SyscallNumber::IPC_RECV`], authenticating the stack's attested
//!   sender origin on each message exactly as an app authenticates window
//!   events (`plans/APPWIN.md` AW3). There is no kernel `WaitSourceKind::Socket`
//!   — teaching the kernel about an object it does not own would break the
//!   microkernel boundary.
//!
//! Every decode is total and fails closed: an unknown magic, version,
//! operation, family, or socket type, a dirty reserved field, or an
//! over-length payload refuses rather than guessing.

use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::net_ipc::NetAddrFamily;
use crate::reply::{decode_status_reply, encode_status_reply, STATUS_REPLY_LEN};
use crate::Errno;

/// Reserved well-known call-endpoint id of the network stack's **socket**
/// surface (`"NSK1"` little-endian). Distinct from the admin
/// [`crate::net_ipc::NETSTACK_ENDPOINT`] so the data-plane and the
/// configuration surface are separate reserved rendezvous with their own
/// message sizes. Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming it first would
/// receive every process's socket calls.
pub const NETSTACK_SOCKET_ENDPOINT: u64 = u64::from_le_bytes(*b"NSK1\0\0\0\0");

/// Magic number identifying a socket request (`"NSKR"` little-endian).
pub const SOCKET_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"NSKR");

/// Magic number identifying a delivered datagram (`"NSKD"` little-endian).
pub const SOCKET_DATAGRAM_MAGIC: u32 = u32::from_le_bytes(*b"NSKD");

/// Magic number identifying a delivered ICMP echo reply (`"NSKE"`
/// little-endian) — the [`SocketType::IcmpEcho`] analogue of a
/// [`SocketDatagram`].
pub const SOCKET_ECHO_MAGIC: u32 = u32::from_le_bytes(*b"NSKE");

/// Magic number identifying a delivered stream event (`"NSKS"`
/// little-endian) — the connection-oriented (TCP) analogue of a
/// [`SocketDatagram`].
pub const SOCKET_STREAM_MAGIC: u32 = u32::from_le_bytes(*b"NSKS");

/// The `netsock-v1` protocol version.
pub const SOCKET_VERSION_V1: u16 = 1;

/// Largest UDP payload the inline socket transport carries in one call.
///
/// A fixed validation bound, not a capacity: it caps the request and
/// delivery buffers the stack pins per endpoint. Bulk transfer past this
/// bound is the future shared-memory path (`plans/NETWORK.md` §2.4), not a
/// larger inline buffer.
pub const SOCKET_MAX_DATAGRAM: usize = 8192;

/// Highest port in the privileged (well-known) range.
///
/// Binding a *listening* socket to a port at or below this bound requires
/// [`CAP_NET_BIND_PRIVILEGED`](crate::CapabilityId::NET_BIND_PRIVILEGED):
/// the low ports name well-known services, so squatting one lets an
/// unprivileged process impersonate a system service. A `0` (ephemeral)
/// request is never privileged; an outbound (active) socket's local port
/// is unrestricted. Matches the historical Unix 1024 boundary.
pub const SOCKET_PRIVILEGED_PORT_MAX: u16 = 1023;

/// The transport type of a socket.
///
/// Datagram (UDP), stream (TCP), and ICMP-echo sockets are served. The
/// ICMP-echo type takes the reserved raw wire value (`3`, mirroring the
/// POSIX `SOCK_RAW`/`SOCK_DGRAM IPPROTO_ICMP` conventions); opening one
/// requires [`CAP_NET_RAW`](crate::CapabilityId::NET_RAW), and the stack
/// owns the ICMP identifier so a socket only ever receives replies to its
/// own requests. The decoder still fails closed on every value it does not
/// serve, so no unserved type is silently accepted.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SocketType {
    /// A connection-oriented TCP stream socket.
    Stream = 1,
    /// A connectionless UDP datagram socket.
    Datagram = 2,
    /// An ICMP/`ICMPv6` echo socket: it sends echo requests and receives
    /// the matching echo replies (the `ping` path). Capability-gated on
    /// [`CAP_NET_RAW`](crate::CapabilityId::NET_RAW).
    IcmpEcho = 3,
}

impl SocketType {
    /// The wire value for this type.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a type from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value this increment does not serve, so
    /// an unimplemented socket type is refused, never guessed.
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::Stream),
            2 => Ok(Self::Datagram),
            3 => Ok(Self::IcmpEcho),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// A socket address: an IP address and a transport port.
///
/// The address occupies sixteen bytes regardless of family; an IPv4 address
/// uses the first four and the rest must be zero (a dirty tail is wire
/// corruption and is refused).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SocketAddr {
    /// Address family.
    pub family: NetAddrFamily,
    /// The address; IPv4 uses the first four bytes.
    pub addr: [u8; 16],
    /// The transport port, host order.
    pub port: u16,
}

impl SocketAddr {
    /// Encoded size: family (1), reserved (1), port (2), address (16).
    pub const WIRE_LEN: usize = 20;

    fn write(&self, out: &mut [u8]) {
        out[0] = self.family.as_u8();
        put_u16(out, 2, self.port);
        out[4..20].copy_from_slice(&self.addr);
    }

    fn read(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes[1] != 0 {
            return Err(Errno::BadMagic);
        }
        let family = NetAddrFamily::from_u8(bytes[0])?;
        let port = read_u16(bytes, 2);
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&bytes[4..20]);
        if family == NetAddrFamily::V4 && addr[4..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        Ok(Self { family, addr, port })
    }
}

/// Byte offset of the embedded [`SocketAddr`] block inside a request header.
const ADDR_OFFSET: usize = 16;
/// Byte offset of the socket-handle field.
const SOCKET_OFFSET: usize = 8;
/// Byte offset of the socket-type field (`Socket` only).
const TYPE_OFFSET: usize = 12;
/// Byte offset of the family field (`Socket` only).
const FAMILY_OFFSET: usize = 13;
/// Byte offset of the ICMP echo sequence field (`SendEcho` only).
const SEQUENCE_OFFSET: usize = 14;
/// Byte offset of the delivery-port field (`Socket` only).
const DELIVER_OFFSET: usize = 36;

/// Wire operation discriminant of [`SocketRequest::Socket`].
const OP_SOCKET: u16 = 1;
/// Wire operation discriminant of [`SocketRequest::Bind`].
const OP_BIND: u16 = 2;
/// Wire operation discriminant of [`SocketRequest::Connect`].
const OP_CONNECT: u16 = 3;
/// Wire operation discriminant of [`SocketRequest::Send`].
const OP_SEND: u16 = 4;
/// Wire operation discriminant of [`SocketRequest::Close`].
const OP_CLOSE: u16 = 5;
/// Wire operation discriminant of [`SocketRequest::JoinMulticast`].
const OP_JOIN: u16 = 6;
/// Wire operation discriminant of [`SocketRequest::LeaveMulticast`].
const OP_LEAVE: u16 = 7;
/// Wire operation discriminant of [`SocketRequest::Listen`].
const OP_LISTEN: u16 = 8;
/// Wire operation discriminant of [`SocketRequest::Accept`].
const OP_ACCEPT: u16 = 9;
/// Wire operation discriminant of [`SocketRequest::SendEcho`].
const OP_SEND_ECHO: u16 = 10;

/// A server-assigned socket handle, scoped to the creating principal.
///
/// The stack keys each socket to the kernel-attested [`crate::Origin`] that
/// created it, so a handle is meaningless — and unusable — to any other
/// principal even if observed.
pub type SocketId = u32;

/// One socket-service control-plane operation.
///
/// Each is one fixed-header frame (plus a trailing payload for
/// [`Send`](SocketRequest::Send)); the service derives the caller's
/// authority from its kernel-attested origin, never from a claimed field.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SocketRequest<'a> {
    /// Open a socket. The stack allocates a handle and records `deliver_port`
    /// as the async port it will send inbound [`SocketDatagram`]s to.
    Socket {
        /// Address family of the socket.
        family: NetAddrFamily,
        /// Transport type (datagram this increment).
        sock_type: SocketType,
        /// The caller's async port endpoint id inbound datagrams are sent to.
        deliver_port: u64,
    },
    /// Bind a socket to a local address and port. A `port` of `0` requests a
    /// CSPRNG-drawn ephemeral port; an unspecified (`0`) address binds to any
    /// local address of the socket's family. The bound port is returned.
    Bind {
        /// The socket handle.
        socket: SocketId,
        /// The requested local address and port.
        local: SocketAddr,
    },
    /// Set a socket's default peer: subsequent [`Send`](Self::Send)s may omit
    /// a destination, and inbound datagrams are filtered to this peer.
    Connect {
        /// The socket handle.
        socket: SocketId,
        /// The peer address and port.
        peer: SocketAddr,
    },
    /// Send one datagram. `dest` is [`None`] to use the connected peer, or a
    /// destination address and port. The payload follows the header.
    Send {
        /// The socket handle.
        socket: SocketId,
        /// The destination, or [`None`] to use the connected peer.
        dest: Option<SocketAddr>,
        /// The datagram payload (at most [`SOCKET_MAX_DATAGRAM`] bytes).
        payload: &'a [u8],
    },
    /// Close a socket and release its state.
    Close {
        /// The socket handle.
        socket: SocketId,
    },
    /// Join a multicast group on the socket. The group `port` field must be
    /// zero (a group is an address, not an address/port pair).
    JoinMulticast {
        /// The socket handle.
        socket: SocketId,
        /// The multicast group address.
        group: SocketAddr,
    },
    /// Leave a multicast group on the socket.
    LeaveMulticast {
        /// The socket handle.
        socket: SocketId,
        /// The multicast group address.
        group: SocketAddr,
    },
    /// Convert a bound stream socket into a passive listener: it accepts
    /// inbound connections on its bound local port instead of originating
    /// one. The socket must be a bound (`Bind`ed) stream socket. Binding a
    /// port at or below [`SOCKET_PRIVILEGED_PORT_MAX`] required
    /// [`CAP_NET_BIND_PRIVILEGED`](crate::CapabilityId::NET_BIND_PRIVILEGED)
    /// at bind time.
    Listen {
        /// The bound stream socket handle to make passive.
        socket: SocketId,
    },
    /// Claim the next established connection queued on a listening socket,
    /// creating a new child stream socket for it. The child delivers its
    /// stream events ([`Connected`](SocketStreamEvent::Connected)/`Data`/
    /// `Closed`) to `deliver_port`. On success the reply carries the new
    /// child [`SocketId`]; when no connection is ready the stack replies
    /// [`Errno::WouldBlock`] (the client waits for the next
    /// [`Accepted`](SocketStreamEvent::Accepted) readiness event).
    Accept {
        /// The listening socket handle.
        socket: SocketId,
        /// The async port the new child socket's stream events go to.
        deliver_port: u64,
    },
    /// Send one ICMP/`ICMPv6` echo request from an [`SocketType::IcmpEcho`]
    /// socket. `dest` is [`None`] to use the connected peer (the port field
    /// of an echo destination is unused — ICMP has no ports — and must be
    /// zero). The stack owns the echo *identifier* (assigned per socket);
    /// the caller chooses the `sequence`. The echo payload follows the
    /// header.
    SendEcho {
        /// The socket handle.
        socket: SocketId,
        /// The destination, or [`None`] to use the connected peer.
        dest: Option<SocketAddr>,
        /// The echo sequence number, echoed back unchanged by the peer.
        sequence: u16,
        /// The echo payload (at most [`SOCKET_MAX_DATAGRAM`] bytes).
        payload: &'a [u8],
    },
}

impl<'a> SocketRequest<'a> {
    /// Byte length of the fixed request header preceding any payload.
    pub const HEADER_LEN: usize = 44;

    /// Largest request the [`NETSTACK_SOCKET_ENDPOINT`] accepts: the header
    /// plus a maximum-size datagram payload.
    pub const MAX_WIRE_LEN: usize = Self::HEADER_LEN + SOCKET_MAX_DATAGRAM;

    /// Encode `self` little-endian into `out`, returning the byte length.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the encoding.
    /// * [`Errno::LengthOutOfRange`] — a [`Send`](Self::Send) payload beyond
    ///   [`SOCKET_MAX_DATAGRAM`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let payload = match self {
            Self::Send { payload, .. } | Self::SendEcho { payload, .. } => *payload,
            _ => &[],
        };
        if payload.len() > SOCKET_MAX_DATAGRAM {
            return Err(Errno::LengthOutOfRange);
        }
        let total = Self::HEADER_LEN + payload.len();
        if out.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        for byte in &mut out[..Self::HEADER_LEN] {
            *byte = 0;
        }
        put_u32(out, 0, SOCKET_REQUEST_MAGIC);
        put_u16(out, 4, SOCKET_VERSION_V1);
        match *self {
            Self::Socket {
                family,
                sock_type,
                deliver_port,
            } => {
                put_u16(out, 6, OP_SOCKET);
                out[TYPE_OFFSET] = sock_type.as_u8();
                out[FAMILY_OFFSET] = family.as_u8();
                put_u64(out, DELIVER_OFFSET, deliver_port);
            }
            Self::Bind { socket, local } => {
                put_u16(out, 6, OP_BIND);
                put_u32(out, SOCKET_OFFSET, socket);
                local.write(&mut out[ADDR_OFFSET..ADDR_OFFSET + SocketAddr::WIRE_LEN]);
            }
            Self::Connect { socket, peer } => {
                put_u16(out, 6, OP_CONNECT);
                put_u32(out, SOCKET_OFFSET, socket);
                peer.write(&mut out[ADDR_OFFSET..ADDR_OFFSET + SocketAddr::WIRE_LEN]);
            }
            Self::Send {
                socket,
                dest,
                payload,
            } => {
                put_u16(out, 6, OP_SEND);
                put_u32(out, SOCKET_OFFSET, socket);
                if let Some(dest) = dest {
                    dest.write(&mut out[ADDR_OFFSET..ADDR_OFFSET + SocketAddr::WIRE_LEN]);
                }
                out[Self::HEADER_LEN..total].copy_from_slice(payload);
            }
            Self::Close { socket } => {
                put_u16(out, 6, OP_CLOSE);
                put_u32(out, SOCKET_OFFSET, socket);
            }
            Self::JoinMulticast { socket, group } => {
                put_u16(out, 6, OP_JOIN);
                put_u32(out, SOCKET_OFFSET, socket);
                group.write(&mut out[ADDR_OFFSET..ADDR_OFFSET + SocketAddr::WIRE_LEN]);
            }
            Self::LeaveMulticast { socket, group } => {
                put_u16(out, 6, OP_LEAVE);
                put_u32(out, SOCKET_OFFSET, socket);
                group.write(&mut out[ADDR_OFFSET..ADDR_OFFSET + SocketAddr::WIRE_LEN]);
            }
            Self::Listen { socket } => {
                put_u16(out, 6, OP_LISTEN);
                put_u32(out, SOCKET_OFFSET, socket);
            }
            Self::Accept {
                socket,
                deliver_port,
            } => {
                put_u16(out, 6, OP_ACCEPT);
                put_u32(out, SOCKET_OFFSET, socket);
                put_u64(out, DELIVER_OFFSET, deliver_port);
            }
            Self::SendEcho {
                socket,
                dest,
                sequence,
                payload,
            } => {
                put_u16(out, 6, OP_SEND_ECHO);
                put_u32(out, SOCKET_OFFSET, socket);
                put_u16(out, SEQUENCE_OFFSET, sequence);
                if let Some(dest) = dest {
                    dest.write(&mut out[ADDR_OFFSET..ADDR_OFFSET + SocketAddr::WIRE_LEN]);
                }
                out[Self::HEADER_LEN..total].copy_from_slice(payload);
            }
        }
        Ok(total)
    }

    /// Decode a request from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the header.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved field.
    /// * [`Errno::AbiVersionUnsupported`] — not `netsock-v1`.
    /// * [`Errno::OutOfRange`] — an unknown operation, family, or socket
    ///   type, or a non-zero group port.
    /// * [`Errno::LengthOutOfRange`] — a payload beyond
    ///   [`SOCKET_MAX_DATAGRAM`], or a non-empty payload on a non-`Send`
    ///   operation.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SOCKET_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SOCKET_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        let payload = &bytes[Self::HEADER_LEN..];
        // Only the send operations carry a payload; every other operation is
        // header-only.
        if op != OP_SEND && op != OP_SEND_ECHO && !payload.is_empty() {
            return Err(Errno::LengthOutOfRange);
        }
        if payload.len() > SOCKET_MAX_DATAGRAM {
            return Err(Errno::LengthOutOfRange);
        }
        let socket = read_u32(bytes, SOCKET_OFFSET);
        let addr_block = &bytes[ADDR_OFFSET..ADDR_OFFSET + SocketAddr::WIRE_LEN];
        Self::dispatch(op, bytes, addr_block, socket, payload)
    }

    /// Route a validated request header to its operation decoder.
    fn dispatch(
        op: u16,
        bytes: &'a [u8],
        addr_block: &'a [u8],
        socket: SocketId,
        payload: &'a [u8],
    ) -> Result<Self, Errno> {
        match op {
            OP_SOCKET => Self::decode_socket_op(bytes, addr_block),
            OP_BIND | OP_CONNECT | OP_JOIN | OP_LEAVE => {
                reserved_addr_op(bytes)?;
                let addr = SocketAddr::read(addr_block)?;
                match op {
                    OP_BIND => Ok(Self::Bind {
                        socket,
                        local: addr,
                    }),
                    OP_CONNECT => Ok(Self::Connect { socket, peer: addr }),
                    _ => {
                        // A multicast group is an address, not a port pair.
                        if addr.port != 0 {
                            return Err(Errno::OutOfRange);
                        }
                        if op == OP_JOIN {
                            Ok(Self::JoinMulticast {
                                socket,
                                group: addr,
                            })
                        } else {
                            Ok(Self::LeaveMulticast {
                                socket,
                                group: addr,
                            })
                        }
                    }
                }
            }
            OP_SEND => {
                reserved_addr_op(bytes)?;
                // A family byte of zero (with a zeroed block) means "use the
                // connected peer"; anything else is an explicit destination.
                let dest = if addr_block.iter().all(|&b| b == 0) {
                    None
                } else {
                    Some(SocketAddr::read(addr_block)?)
                };
                Ok(Self::Send {
                    socket,
                    dest,
                    payload,
                })
            }
            OP_CLOSE | OP_LISTEN => {
                // Close and Listen use only the socket handle; every other
                // field (type, family, address, delivery port) is reserved.
                if bytes[TYPE_OFFSET] != 0
                    || bytes[13] != 0
                    || bytes[14] != 0
                    || bytes[15] != 0
                    || addr_block.iter().any(|&b| b != 0)
                    || read_u64(bytes, DELIVER_OFFSET) != 0
                {
                    return Err(Errno::BadMagic);
                }
                if op == OP_CLOSE {
                    Ok(Self::Close { socket })
                } else {
                    Ok(Self::Listen { socket })
                }
            }
            OP_ACCEPT => {
                // Accept carries the listener handle and the child's
                // delivery port; the type/family/address fields are unused.
                if bytes[TYPE_OFFSET] != 0
                    || bytes[13] != 0
                    || bytes[14] != 0
                    || bytes[15] != 0
                    || addr_block.iter().any(|&b| b != 0)
                {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Accept {
                    socket,
                    deliver_port: read_u64(bytes, DELIVER_OFFSET),
                })
            }
            OP_SEND_ECHO => Self::decode_send_echo(bytes, addr_block, socket, payload),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Decode an [`OP_SEND_ECHO`] header: the handle, the sequence (bytes
    /// 14..16), an optional destination, and the payload. The socket-type,
    /// family, and delivery-port fields belong to `Socket` alone and must be
    /// zero; an echo destination carries no port (ICMP has none), so a
    /// non-zero port is wire corruption and fails closed.
    fn decode_send_echo(
        bytes: &[u8],
        addr_block: &[u8],
        socket: SocketId,
        payload: &'a [u8],
    ) -> Result<Self, Errno> {
        if bytes[TYPE_OFFSET] != 0
            || bytes[FAMILY_OFFSET] != 0
            || read_u64(bytes, DELIVER_OFFSET) != 0
        {
            return Err(Errno::BadMagic);
        }
        let sequence = read_u16(bytes, SEQUENCE_OFFSET);
        // A zeroed address block means "use the connected peer"; anything
        // else is an explicit destination.
        let dest = if addr_block.iter().all(|&b| b == 0) {
            None
        } else {
            let addr = SocketAddr::read(addr_block)?;
            if addr.port != 0 {
                return Err(Errno::OutOfRange);
            }
            Some(addr)
        };
        Ok(Self::SendEcho {
            socket,
            dest,
            sequence,
            payload,
        })
    }

    /// Decode an [`OP_SOCKET`] header: the socket type and family live in
    /// dedicated bytes and the delivery port in its field; the handle and
    /// address block must be zero (a dirty one is refused).
    fn decode_socket_op(bytes: &[u8], addr_block: &[u8]) -> Result<Self, Errno> {
        if read_u32(bytes, SOCKET_OFFSET) != 0
            || bytes[14] != 0
            || bytes[15] != 0
            || addr_block.iter().any(|&b| b != 0)
        {
            return Err(Errno::BadMagic);
        }
        let sock_type = SocketType::from_u8(bytes[TYPE_OFFSET])?;
        let family = NetAddrFamily::from_u8(bytes[FAMILY_OFFSET])?;
        Ok(Self::Socket {
            family,
            sock_type,
            deliver_port: read_u64(bytes, DELIVER_OFFSET),
        })
    }
}

/// Refuse an address-bearing operation whose type/family bytes or delivery
/// field carry any non-zero byte (those fields belong to `Socket` alone).
fn reserved_addr_op(bytes: &[u8]) -> Result<(), Errno> {
    if bytes[TYPE_OFFSET] != 0
        || bytes[13] != 0
        || bytes[14] != 0
        || bytes[15] != 0
        || read_u64(bytes, DELIVER_OFFSET) != 0
    {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

/// Byte length of a socket-open reply: the status word then the assigned
/// [`SocketId`].
pub const SOCKET_OPEN_REPLY_LEN: usize = STATUS_REPLY_LEN + 4;

/// Byte length of a bind reply: the status word, the bound port, and a
/// reserved pair.
pub const SOCKET_BIND_REPLY_LEN: usize = STATUS_REPLY_LEN + 4;

/// Largest reply the [`NETSTACK_SOCKET_ENDPOINT`] emits.
pub const SOCKET_MAX_REPLY: usize = SOCKET_OPEN_REPLY_LEN;

/// Encode a socket-open outcome: the status frame, and on success the
/// assigned handle; on refusal the status frame alone.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_socket_reply(
    result: Result<SocketId, Errno>,
    out: &mut [u8],
) -> Result<usize, Errno> {
    match result {
        Ok(id) => {
            if out.len() < SOCKET_OPEN_REPLY_LEN {
                return Err(Errno::BufferTooSmall);
            }
            out[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
            put_u32(out, STATUS_REPLY_LEN, id);
            Ok(SOCKET_OPEN_REPLY_LEN)
        }
        Err(err) => encode_status_only(err, out),
    }
}

/// Decode a socket-open reply.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold the reply.
/// * The decoded [`Errno`] — the service refused the request.
pub fn decode_socket_reply(bytes: &[u8]) -> Result<SocketId, Errno> {
    decode_status_reply(&bytes[..bytes.len().min(STATUS_REPLY_LEN)])?;
    if bytes.len() < SOCKET_OPEN_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    Ok(read_u32(bytes, STATUS_REPLY_LEN))
}

/// Encode a bind outcome: the status frame, and on success the bound port.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_bind_reply(result: Result<u16, Errno>, out: &mut [u8]) -> Result<usize, Errno> {
    match result {
        Ok(port) => {
            if out.len() < SOCKET_BIND_REPLY_LEN {
                return Err(Errno::BufferTooSmall);
            }
            out[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
            put_u16(out, STATUS_REPLY_LEN, port);
            put_u16(out, STATUS_REPLY_LEN + 2, 0);
            Ok(SOCKET_BIND_REPLY_LEN)
        }
        Err(err) => encode_status_only(err, out),
    }
}

/// Decode a bind reply, returning the bound port.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold the reply.
/// * [`Errno::BadMagic`] — a dirty reserved pair.
/// * The decoded [`Errno`] — the service refused the request.
pub fn decode_bind_reply(bytes: &[u8]) -> Result<u16, Errno> {
    decode_status_reply(&bytes[..bytes.len().min(STATUS_REPLY_LEN)])?;
    if bytes.len() < SOCKET_BIND_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    if read_u16(bytes, STATUS_REPLY_LEN + 2) != 0 {
        return Err(Errno::BadMagic);
    }
    Ok(read_u16(bytes, STATUS_REPLY_LEN))
}

/// Write the status-only refusal frame carrying `err`.
fn encode_status_only(err: Errno, out: &mut [u8]) -> Result<usize, Errno> {
    if out.len() < STATUS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    out[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Err(err)));
    Ok(STATUS_REPLY_LEN)
}

/// A datagram the stack delivers to a socket's async port.
///
/// The stack [`crate::SyscallNumber::IPC_SEND`]s this frame to the port the
/// client named in [`SocketRequest::Socket`]; the client authenticates the
/// stack's kernel-attested sender origin, then decodes it. It identifies the
/// receiving socket, the peer it came from, and the payload.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SocketDatagram<'a> {
    /// The socket the datagram was delivered to.
    pub socket: SocketId,
    /// The peer that sent it.
    pub source: SocketAddr,
    /// The datagram payload.
    pub payload: &'a [u8],
}

impl<'a> SocketDatagram<'a> {
    /// Byte length of the fixed delivery header preceding the payload.
    pub const HEADER_LEN: usize = 36;

    /// Largest delivery message: the header plus a maximum-size payload.
    pub const MAX_WIRE_LEN: usize = Self::HEADER_LEN + SOCKET_MAX_DATAGRAM;

    /// Encode `self` little-endian into `out`, returning the byte length.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — the payload exceeds
    ///   [`SOCKET_MAX_DATAGRAM`].
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the message.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if self.payload.len() > SOCKET_MAX_DATAGRAM {
            return Err(Errno::LengthOutOfRange);
        }
        let total = Self::HEADER_LEN + self.payload.len();
        if out.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        for byte in &mut out[..Self::HEADER_LEN] {
            *byte = 0;
        }
        put_u32(out, 0, SOCKET_DATAGRAM_MAGIC);
        put_u16(out, 4, SOCKET_VERSION_V1);
        put_u32(out, 8, self.socket);
        out[12] = self.source.family.as_u8();
        put_u16(out, 14, self.source.port);
        out[16..32].copy_from_slice(&self.source.addr);
        // Payload length fits u32: bounded by SOCKET_MAX_DATAGRAM above.
        put_u32(
            out,
            32,
            u32::try_from(self.payload.len()).map_err(|_| Errno::LengthOutOfRange)?,
        );
        out[Self::HEADER_LEN..total].copy_from_slice(self.payload);
        Ok(total)
    }

    /// Decode a delivery message from `bytes`, failing closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the header or
    ///   the declared payload.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved field.
    /// * [`Errno::AbiVersionUnsupported`] — not `netsock-v1`.
    /// * [`Errno::OutOfRange`] — an unknown family.
    /// * [`Errno::LengthOutOfRange`] — a declared payload beyond
    ///   [`SOCKET_MAX_DATAGRAM`].
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SOCKET_DATAGRAM_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SOCKET_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if read_u16(bytes, 6) != 0 || bytes[13] != 0 {
            return Err(Errno::BadMagic);
        }
        let socket = read_u32(bytes, 8);
        let family = NetAddrFamily::from_u8(bytes[12])?;
        let port = read_u16(bytes, 14);
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&bytes[16..32]);
        if family == NetAddrFamily::V4 && addr[4..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let payload_len = read_u32(bytes, 32) as usize;
        if payload_len > SOCKET_MAX_DATAGRAM {
            return Err(Errno::LengthOutOfRange);
        }
        let payload = bytes
            .get(Self::HEADER_LEN..Self::HEADER_LEN + payload_len)
            .ok_or(Errno::BufferTooSmall)?;
        Ok(Self {
            socket,
            source: SocketAddr { family, addr, port },
            payload,
        })
    }
}

/// An ICMP/`ICMPv6` echo reply the stack delivers to an
/// [`SocketType::IcmpEcho`] socket's async port.
///
/// The stack [`crate::SyscallNumber::IPC_SEND`]s this frame to the port the
/// client named in [`SocketRequest::Socket`] when a reply arrives whose ICMP
/// identifier matches the socket's stack-assigned identifier (so a socket
/// only ever sees replies to its own requests). It identifies the receiving
/// socket, the source that answered, the echoed sequence number, and the
/// echoed payload. ICMP has no ports, so — unlike a [`SocketDatagram`] — the
/// source carries no port; the wire layout reuses that field for the
/// sequence number.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SocketEcho<'a> {
    /// The socket the reply was delivered to.
    pub socket: SocketId,
    /// The source that sent the reply.
    pub source: SocketAddr,
    /// The echoed sequence number, matching the originating request.
    pub sequence: u16,
    /// The echoed payload.
    pub payload: &'a [u8],
}

impl<'a> SocketEcho<'a> {
    /// Byte length of the fixed delivery header preceding the payload.
    pub const HEADER_LEN: usize = 36;

    /// Largest delivery message: the header plus a maximum-size payload.
    pub const MAX_WIRE_LEN: usize = Self::HEADER_LEN + SOCKET_MAX_DATAGRAM;

    /// Encode `self` little-endian into `out`, returning the byte length.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — the payload exceeds
    ///   [`SOCKET_MAX_DATAGRAM`].
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the message.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if self.payload.len() > SOCKET_MAX_DATAGRAM {
            return Err(Errno::LengthOutOfRange);
        }
        let total = Self::HEADER_LEN + self.payload.len();
        if out.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        for byte in &mut out[..Self::HEADER_LEN] {
            *byte = 0;
        }
        put_u32(out, 0, SOCKET_ECHO_MAGIC);
        put_u16(out, 4, SOCKET_VERSION_V1);
        put_u32(out, 8, self.socket);
        out[12] = self.source.family.as_u8();
        put_u16(out, 14, self.sequence);
        out[16..32].copy_from_slice(&self.source.addr);
        // Payload length fits u32: bounded by SOCKET_MAX_DATAGRAM above.
        put_u32(
            out,
            32,
            u32::try_from(self.payload.len()).map_err(|_| Errno::LengthOutOfRange)?,
        );
        out[Self::HEADER_LEN..total].copy_from_slice(self.payload);
        Ok(total)
    }

    /// Decode a delivery message from `bytes`, failing closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the header or
    ///   the declared payload.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved field.
    /// * [`Errno::AbiVersionUnsupported`] — not `netsock-v1`.
    /// * [`Errno::OutOfRange`] — an unknown family.
    /// * [`Errno::LengthOutOfRange`] — a declared payload beyond
    ///   [`SOCKET_MAX_DATAGRAM`].
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SOCKET_ECHO_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SOCKET_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if read_u16(bytes, 6) != 0 || bytes[13] != 0 {
            return Err(Errno::BadMagic);
        }
        let socket = read_u32(bytes, 8);
        let family = NetAddrFamily::from_u8(bytes[12])?;
        let sequence = read_u16(bytes, 14);
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&bytes[16..32]);
        if family == NetAddrFamily::V4 && addr[4..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let payload_len = read_u32(bytes, 32) as usize;
        if payload_len > SOCKET_MAX_DATAGRAM {
            return Err(Errno::LengthOutOfRange);
        }
        let payload = bytes
            .get(Self::HEADER_LEN..Self::HEADER_LEN + payload_len)
            .ok_or(Errno::BufferTooSmall)?;
        Ok(Self {
            socket,
            source: SocketAddr {
                family,
                addr,
                port: 0,
            },
            sequence,
            payload,
        })
    }
}

/// Byte length of a stream `send` reply: the status word then the count
/// of payload bytes the stack accepted into the connection's send buffer.
///
/// A stream `send` is flow-controlled: the stack may accept fewer bytes
/// than offered when the send buffer is momentarily full, so the reply
/// reports the accepted count (never larger than the offered payload, so
/// it fits [`SOCKET_MAX_DATAGRAM`] and thus a `u32`). A datagram `send`
/// uses the plain status reply — a datagram is all-or-nothing.
pub const SOCKET_SEND_REPLY_LEN: usize = STATUS_REPLY_LEN + 4;

/// Encode a stream-`send` outcome: the status frame, and on success the
/// number of payload bytes accepted.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_send_reply(result: Result<u32, Errno>, out: &mut [u8]) -> Result<usize, Errno> {
    match result {
        Ok(accepted) => {
            if out.len() < SOCKET_SEND_REPLY_LEN {
                return Err(Errno::BufferTooSmall);
            }
            out[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
            put_u32(out, STATUS_REPLY_LEN, accepted);
            Ok(SOCKET_SEND_REPLY_LEN)
        }
        Err(err) => encode_status_only(err, out),
    }
}

/// Decode a stream-`send` reply, returning the accepted byte count.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold the reply.
/// * The decoded [`Errno`] — the service refused the request.
pub fn decode_send_reply(bytes: &[u8]) -> Result<u32, Errno> {
    decode_status_reply(&bytes[..bytes.len().min(STATUS_REPLY_LEN)])?;
    if bytes.len() < SOCKET_SEND_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    Ok(read_u32(bytes, STATUS_REPLY_LEN))
}

/// Why a stream connection ended, delivered in a [`SocketStreamEvent::Closed`].
///
/// The receive half of a stream never fails silently (`AGENTS.md` §2.24):
/// a connection always ends with exactly one `Closed` event stating the
/// reason, so a client `recv` that returns end-of-stream can distinguish an
/// orderly peer close from an abortive reset.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum StreamCloseReason {
    /// The peer closed its send direction (a FIN): an orderly end of
    /// stream. Any data delivered before this event is complete and
    /// correct; `recv` now returns end-of-stream.
    PeerClosed = 1,
    /// The connection was aborted by a RST (RFC 9293): data in flight may
    /// have been lost.
    Reset = 2,
    /// The retransmission budget or user timeout elapsed with data
    /// unacknowledged: the peer became unreachable.
    TimedOut = 3,
    /// A connection-establishment attempt was refused (a RST answered our
    /// SYN).
    Refused = 4,
}

impl StreamCloseReason {
    /// The wire value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a reason from its wire value, failing closed on any
    /// unknown value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — not a defined reason.
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::PeerClosed),
            2 => Ok(Self::Reset),
            3 => Ok(Self::TimedOut),
            4 => Ok(Self::Refused),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// An event the stack delivers to a stream socket's async port.
///
/// A connection-oriented socket has no per-message peer (the peer is fixed
/// at [`SocketRequest::Connect`]), so unlike a [`SocketDatagram`] a stream
/// event carries no source address — only the socket it concerns and, for
/// received data, the stream bytes. The stack `ipc_send`s these frames to
/// the port the client named in [`SocketRequest::Socket`]; the client
/// authenticates the stack's kernel-attested sender origin, then decodes.
///
/// The three events form the client-visible connection lifecycle:
/// [`Connected`](Self::Connected) once (the handshake completed),
/// [`Data`](Self::Data) zero or more times (received in order), and
/// [`Closed`](Self::Closed) exactly once at the end (stating why).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SocketStreamEvent<'a> {
    /// The three-way handshake completed; the socket is established and
    /// may send and receive.
    Connected {
        /// The socket the event concerns.
        socket: SocketId,
    },
    /// Received stream bytes, delivered in sequence order.
    Data {
        /// The socket the bytes belong to.
        socket: SocketId,
        /// The received payload (at most [`SOCKET_MAX_DATAGRAM`] bytes;
        /// the stack fragments a larger receive across several events).
        payload: &'a [u8],
    },
    /// The connection ended; no further events follow for this socket.
    Closed {
        /// The socket that closed.
        socket: SocketId,
        /// Why it closed.
        reason: StreamCloseReason,
    },
    /// A listening socket has at least one established connection ready to
    /// [`Accept`](SocketRequest::Accept). Delivered to the *listener's*
    /// delivery port (edge-triggered, one per newly ready connection); the
    /// client responds by calling [`Accept`](SocketRequest::Accept) on
    /// `socket` until it replies [`Errno::WouldBlock`].
    Accepted {
        /// The listening socket a connection is ready on.
        socket: SocketId,
    },
}

/// Wire event discriminant of [`SocketStreamEvent::Connected`].
const STREAM_EV_CONNECTED: u16 = 1;
/// Wire event discriminant of [`SocketStreamEvent::Data`].
const STREAM_EV_DATA: u16 = 2;
/// Wire event discriminant of [`SocketStreamEvent::Closed`].
const STREAM_EV_CLOSED: u16 = 3;
/// Wire event discriminant of [`SocketStreamEvent::Accepted`].
const STREAM_EV_ACCEPTED: u16 = 4;

impl<'a> SocketStreamEvent<'a> {
    /// Byte length of the fixed event header preceding any payload.
    pub const HEADER_LEN: usize = 20;

    /// Largest delivery message: the header plus a maximum-size data
    /// payload.
    pub const MAX_WIRE_LEN: usize = Self::HEADER_LEN + SOCKET_MAX_DATAGRAM;

    /// Encode `self` little-endian into `out`, returning the byte length.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — a [`Data`](Self::Data) payload
    ///   exceeds [`SOCKET_MAX_DATAGRAM`].
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the message.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let payload: &[u8] = match self {
            Self::Data { payload, .. } => payload,
            _ => &[],
        };
        if payload.len() > SOCKET_MAX_DATAGRAM {
            return Err(Errno::LengthOutOfRange);
        }
        let total = Self::HEADER_LEN + payload.len();
        if out.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        for byte in &mut out[..Self::HEADER_LEN] {
            *byte = 0;
        }
        put_u32(out, 0, SOCKET_STREAM_MAGIC);
        put_u16(out, 4, SOCKET_VERSION_V1);
        match *self {
            Self::Connected { socket } => {
                put_u16(out, 6, STREAM_EV_CONNECTED);
                put_u32(out, 8, socket);
            }
            Self::Data { socket, payload } => {
                put_u16(out, 6, STREAM_EV_DATA);
                put_u32(out, 8, socket);
                // Payload length fits u32: bounded by SOCKET_MAX_DATAGRAM.
                put_u32(
                    out,
                    16,
                    u32::try_from(payload.len()).map_err(|_| Errno::LengthOutOfRange)?,
                );
                out[Self::HEADER_LEN..total].copy_from_slice(payload);
            }
            Self::Closed { socket, reason } => {
                put_u16(out, 6, STREAM_EV_CLOSED);
                put_u32(out, 8, socket);
                out[12] = reason.as_u8();
            }
            Self::Accepted { socket } => {
                put_u16(out, 6, STREAM_EV_ACCEPTED);
                put_u32(out, 8, socket);
            }
        }
        Ok(total)
    }

    /// Decode a stream event from `bytes`, failing closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the header or
    ///   the declared payload.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved field.
    /// * [`Errno::AbiVersionUnsupported`] — not `netsock-v1`.
    /// * [`Errno::OutOfRange`] — an unknown event kind or close reason.
    /// * [`Errno::LengthOutOfRange`] — a declared payload beyond
    ///   [`SOCKET_MAX_DATAGRAM`].
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SOCKET_STREAM_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SOCKET_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let kind = read_u16(bytes, 6);
        let socket = read_u32(bytes, 8);
        match kind {
            STREAM_EV_CONNECTED => {
                if bytes[12] != 0 || bytes[13..Self::HEADER_LEN].iter().any(|&b| b != 0) {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Connected { socket })
            }
            STREAM_EV_DATA => {
                if bytes[12] != 0 || bytes[13] != 0 || bytes[14] != 0 || bytes[15] != 0 {
                    return Err(Errno::BadMagic);
                }
                let payload_len = read_u32(bytes, 16) as usize;
                if payload_len > SOCKET_MAX_DATAGRAM {
                    return Err(Errno::LengthOutOfRange);
                }
                let payload = bytes
                    .get(Self::HEADER_LEN..Self::HEADER_LEN + payload_len)
                    .ok_or(Errno::BufferTooSmall)?;
                Ok(Self::Data { socket, payload })
            }
            STREAM_EV_CLOSED => {
                let reason = StreamCloseReason::from_u8(bytes[12])?;
                if bytes[13..Self::HEADER_LEN].iter().any(|&b| b != 0) {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Closed { socket, reason })
            }
            STREAM_EV_ACCEPTED => {
                if bytes[12] != 0 || bytes[13..Self::HEADER_LEN].iter().any(|&b| b != 0) {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Accepted { socket })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[a, b, c, d]);
        SocketAddr {
            family: NetAddrFamily::V4,
            addr,
            port,
        }
    }

    fn v6(port: u16) -> SocketAddr {
        SocketAddr {
            family: NetAddrFamily::V6,
            addr: [0x20; 16],
            port,
        }
    }

    fn round_trip(request: SocketRequest<'_>) {
        let mut buf = [0u8; SocketRequest::MAX_WIRE_LEN];
        let n = request.encode(&mut buf).expect("request encodes");
        assert_eq!(SocketRequest::from_bytes(&buf[..n]), Ok(request));
    }

    #[test]
    fn magics_are_the_ascii_tags() {
        assert_eq!(SOCKET_REQUEST_MAGIC, u32::from_le_bytes(*b"NSKR"));
        assert_eq!(SOCKET_DATAGRAM_MAGIC, u32::from_le_bytes(*b"NSKD"));
        assert_eq!(SOCKET_ECHO_MAGIC, u32::from_le_bytes(*b"NSKE"));
        assert_eq!(
            NETSTACK_SOCKET_ENDPOINT,
            u64::from_le_bytes(*b"NSK1\0\0\0\0")
        );
    }

    #[test]
    fn socket_type_round_trips_and_fails_closed() {
        assert_eq!(SocketType::from_u8(1), Ok(SocketType::Stream));
        assert_eq!(SocketType::from_u8(2), Ok(SocketType::Datagram));
        assert_eq!(SocketType::from_u8(3), Ok(SocketType::IcmpEcho));
        // Everything else is refused.
        assert_eq!(SocketType::from_u8(0), Err(Errno::OutOfRange));
        assert_eq!(SocketType::from_u8(4), Err(Errno::OutOfRange));
    }

    #[test]
    fn stream_socket_open_round_trips() {
        round_trip(SocketRequest::Socket {
            family: NetAddrFamily::V4,
            sock_type: SocketType::Stream,
            deliver_port: 0x99,
        });
    }

    #[test]
    fn send_reply_round_trips_ok_and_error() {
        let mut out = [0u8; SOCKET_SEND_REPLY_LEN];
        let n = encode_send_reply(Ok(4096), &mut out).expect("encode");
        assert_eq!(decode_send_reply(&out[..n]), Ok(4096));
        let n = encode_send_reply(Err(Errno::NotConnected), &mut out).expect("encode");
        assert_eq!(decode_send_reply(&out[..n]), Err(Errno::NotConnected));
    }

    #[test]
    fn stream_close_reason_round_trips_and_fails_closed() {
        for reason in [
            StreamCloseReason::PeerClosed,
            StreamCloseReason::Reset,
            StreamCloseReason::TimedOut,
            StreamCloseReason::Refused,
        ] {
            assert_eq!(StreamCloseReason::from_u8(reason.as_u8()), Ok(reason));
        }
        assert_eq!(StreamCloseReason::from_u8(0), Err(Errno::OutOfRange));
        assert_eq!(StreamCloseReason::from_u8(5), Err(Errno::OutOfRange));
    }

    #[test]
    fn stream_event_round_trips_and_fails_closed() {
        let events = [
            SocketStreamEvent::Connected { socket: 7 },
            SocketStreamEvent::Data {
                socket: 7,
                payload: b"stream bytes",
            },
            SocketStreamEvent::Closed {
                socket: 7,
                reason: StreamCloseReason::PeerClosed,
            },
            SocketStreamEvent::Accepted { socket: 42 },
        ];
        for event in events {
            let mut out = [0u8; SocketStreamEvent::MAX_WIRE_LEN];
            let n = event.encode(&mut out).expect("encode");
            assert_eq!(SocketStreamEvent::parse(&out[..n]), Ok(event));
        }
        // A Data event truncated past its declared payload fails closed.
        let data = SocketStreamEvent::Data {
            socket: 1,
            payload: b"abcd",
        };
        let mut out = [0u8; SocketStreamEvent::MAX_WIRE_LEN];
        let n = data.encode(&mut out).expect("encode");
        assert_eq!(
            SocketStreamEvent::parse(&out[..n - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = out;
        bad_magic[0] ^= 0xFF;
        assert_eq!(
            SocketStreamEvent::parse(&bad_magic[..n]),
            Err(Errno::BadMagic)
        );
        // An unknown event kind fails closed.
        let mut bad_kind = out;
        bad_kind[6] = 9;
        assert_eq!(
            SocketStreamEvent::parse(&bad_kind[..n]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn empty_stream_data_event_round_trips() {
        let event = SocketStreamEvent::Data {
            socket: 3,
            payload: &[],
        };
        let mut out = [0u8; SocketStreamEvent::HEADER_LEN];
        let n = event.encode(&mut out).expect("encode");
        assert_eq!(n, SocketStreamEvent::HEADER_LEN);
        assert_eq!(SocketStreamEvent::parse(&out[..n]), Ok(event));
    }

    #[test]
    fn every_request_round_trips() {
        round_trip(SocketRequest::Socket {
            family: NetAddrFamily::V4,
            sock_type: SocketType::Datagram,
            deliver_port: 0xDEAD_BEEF_1234,
        });
        round_trip(SocketRequest::Socket {
            family: NetAddrFamily::V6,
            sock_type: SocketType::Datagram,
            deliver_port: 7,
        });
        round_trip(SocketRequest::Bind {
            socket: 5,
            local: v4(0, 0, 0, 0, 0),
        });
        round_trip(SocketRequest::Bind {
            socket: 9,
            local: v6(6000),
        });
        round_trip(SocketRequest::Connect {
            socket: 1,
            peer: v4(10, 0, 2, 2, 53),
        });
        round_trip(SocketRequest::Send {
            socket: 2,
            dest: Some(v4(10, 0, 2, 2, 53)),
            payload: b"hello",
        });
        round_trip(SocketRequest::Send {
            socket: 2,
            dest: None,
            payload: b"connected",
        });
        round_trip(SocketRequest::Send {
            socket: 2,
            dest: Some(v6(123)),
            payload: &[],
        });
        round_trip(SocketRequest::Close { socket: 3 });
        round_trip(SocketRequest::JoinMulticast {
            socket: 4,
            group: v4(224, 0, 0, 251, 0),
        });
        round_trip(SocketRequest::LeaveMulticast {
            socket: 4,
            group: v6(0),
        });
        round_trip(SocketRequest::Listen { socket: 8 });
        round_trip(SocketRequest::Accept {
            socket: 8,
            deliver_port: 0x1234_5678_9ABC,
        });
        round_trip(SocketRequest::Accept {
            socket: 8,
            deliver_port: 0,
        });
        round_trip(SocketRequest::Socket {
            family: NetAddrFamily::V6,
            sock_type: SocketType::IcmpEcho,
            deliver_port: 0x50,
        });
        round_trip(SocketRequest::SendEcho {
            socket: 6,
            dest: Some(v4(10, 0, 2, 2, 0)),
            sequence: 42,
            payload: b"ping payload",
        });
        round_trip(SocketRequest::SendEcho {
            socket: 6,
            dest: None,
            sequence: 65_535,
            payload: &[],
        });
        round_trip(SocketRequest::SendEcho {
            socket: 7,
            dest: Some(SocketAddr {
                family: NetAddrFamily::V6,
                addr: [0xFE; 16],
                port: 0,
            }),
            sequence: 1,
            payload: b"v6 echo",
        });
    }

    #[test]
    fn send_echo_with_a_dest_port_is_refused() {
        // ICMP has no ports; an echo destination carrying one is corruption.
        let request = SocketRequest::SendEcho {
            socket: 6,
            dest: Some(v4(10, 0, 2, 2, 0)),
            sequence: 3,
            payload: b"x",
        };
        let mut buf = [0u8; SocketRequest::HEADER_LEN + 1];
        let n = request.encode(&mut buf).expect("encode");
        // Smuggle a non-zero port into the address block (offset +2..+4).
        let mut dirty = buf;
        dirty[ADDR_OFFSET + 2] = 5;
        assert_eq!(
            SocketRequest::from_bytes(&dirty[..n]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn send_echo_rejects_socket_only_fields() {
        let request = SocketRequest::SendEcho {
            socket: 6,
            dest: None,
            sequence: 9,
            payload: b"x",
        };
        let mut buf = [0u8; SocketRequest::HEADER_LEN + 1];
        let n = request.encode(&mut buf).expect("encode");
        // A stray family byte belongs only to Socket.
        let mut dirty = buf;
        dirty[FAMILY_OFFSET] = 2;
        assert_eq!(SocketRequest::from_bytes(&dirty[..n]), Err(Errno::BadMagic));
    }

    #[test]
    fn listen_rejects_a_dirty_reserved_field() {
        let mut buf = [0u8; SocketRequest::HEADER_LEN];
        let n = SocketRequest::Listen { socket: 8 }
            .encode(&mut buf)
            .expect("encode");
        // A Listen frame with a non-zero delivery field is corruption.
        let mut dirty = buf;
        dirty[36] = 1;
        assert_eq!(SocketRequest::from_bytes(&dirty[..n]), Err(Errno::BadMagic));
    }

    #[test]
    fn accept_rejects_a_dirty_address_field() {
        let mut buf = [0u8; SocketRequest::HEADER_LEN];
        let n = SocketRequest::Accept {
            socket: 8,
            deliver_port: 5,
        }
        .encode(&mut buf)
        .expect("encode");
        // Accept must not carry an address; a dirty address block is refused.
        let mut dirty = buf;
        dirty[ADDR_OFFSET] = 1;
        assert_eq!(SocketRequest::from_bytes(&dirty[..n]), Err(Errno::BadMagic));
    }

    #[test]
    fn decode_fails_closed_on_framing() {
        let good = SocketRequest::Close { socket: 1 };
        let mut buf = [0u8; SocketRequest::MAX_WIRE_LEN];
        let n = good.encode(&mut buf).expect("encode");
        assert_eq!(
            SocketRequest::from_bytes(&buf[..SocketRequest::HEADER_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = buf;
        bad_magic[0] ^= 0xFF;
        assert_eq!(
            SocketRequest::from_bytes(&bad_magic[..n]),
            Err(Errno::BadMagic)
        );
        let mut bad_version = buf;
        bad_version[4] = 9;
        assert_eq!(
            SocketRequest::from_bytes(&bad_version[..n]),
            Err(Errno::AbiVersionUnsupported)
        );
        let mut bad_op = buf;
        bad_op[6] = 99;
        assert_eq!(
            SocketRequest::from_bytes(&bad_op[..n]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn non_send_payload_is_refused() {
        let mut buf = [0u8; SocketRequest::HEADER_LEN + 1];
        let close = SocketRequest::Close { socket: 1 };
        close.encode(&mut buf).expect("encode");
        // Append a payload byte to a Close frame: only Send carries payload.
        assert_eq!(
            SocketRequest::from_bytes(&buf),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn oversize_send_is_refused() {
        let payload = vec![0u8; SOCKET_MAX_DATAGRAM + 1];
        let request = SocketRequest::Send {
            socket: 1,
            dest: Some(v4(1, 1, 1, 1, 1)),
            payload: &payload,
        };
        let mut buf = vec![0u8; SocketRequest::MAX_WIRE_LEN + 64];
        assert_eq!(request.encode(&mut buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn socket_op_rejects_dirty_reserved_fields() {
        let request = SocketRequest::Socket {
            family: NetAddrFamily::V4,
            sock_type: SocketType::Datagram,
            deliver_port: 1,
        };
        let mut buf = [0u8; SocketRequest::HEADER_LEN];
        request.encode(&mut buf).expect("encode");
        // Smuggle a byte into the address block, which Socket leaves zero.
        let mut dirty = buf;
        dirty[ADDR_OFFSET] = 4;
        assert_eq!(SocketRequest::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn addr_op_rejects_socket_only_fields() {
        let request = SocketRequest::Connect {
            socket: 1,
            peer: v4(10, 0, 0, 1, 80),
        };
        let mut buf = [0u8; SocketRequest::HEADER_LEN];
        request.encode(&mut buf).expect("encode");
        // A stray delivery port belongs only to Socket.
        let mut dirty = buf;
        dirty[DELIVER_OFFSET] = 1;
        assert_eq!(SocketRequest::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn multicast_group_with_a_port_is_refused() {
        let request = SocketRequest::JoinMulticast {
            socket: 1,
            group: v4(239, 1, 2, 3, 5000),
        };
        let mut buf = [0u8; SocketRequest::HEADER_LEN];
        request.encode(&mut buf).expect("encode");
        assert_eq!(SocketRequest::from_bytes(&buf), Err(Errno::OutOfRange));
    }

    #[test]
    fn v4_address_with_a_dirty_tail_is_refused() {
        let request = SocketRequest::Connect {
            socket: 1,
            peer: v4(10, 0, 0, 1, 80),
        };
        let mut buf = [0u8; SocketRequest::HEADER_LEN];
        request.encode(&mut buf).expect("encode");
        // Byte 4 of a V4 address must be zero.
        let mut dirty = buf;
        dirty[ADDR_OFFSET + 4 + 4] = 1;
        assert_eq!(SocketRequest::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn socket_reply_round_trips_ok_and_error() {
        let mut out = [0u8; SOCKET_MAX_REPLY];
        let n = encode_socket_reply(Ok(0x1234_5678), &mut out).expect("encode");
        assert_eq!(decode_socket_reply(&out[..n]), Ok(0x1234_5678));
        let n = encode_socket_reply(Err(Errno::PermissionDenied), &mut out).expect("encode");
        assert_eq!(decode_socket_reply(&out[..n]), Err(Errno::PermissionDenied));
    }

    #[test]
    fn bind_reply_round_trips_and_rejects_dirty_reserved() {
        let mut out = [0u8; SOCKET_BIND_REPLY_LEN];
        let n = encode_bind_reply(Ok(49152), &mut out).expect("encode");
        assert_eq!(decode_bind_reply(&out[..n]), Ok(49152));
        let mut dirty = out;
        dirty[STATUS_REPLY_LEN + 2] = 1;
        assert_eq!(decode_bind_reply(&dirty), Err(Errno::BadMagic));
        let n = encode_bind_reply(Err(Errno::AddressInUse), &mut out).expect("encode");
        assert_eq!(decode_bind_reply(&out[..n]), Err(Errno::AddressInUse));
    }

    #[test]
    fn datagram_round_trips_and_fails_closed() {
        let dg = SocketDatagram {
            socket: 7,
            source: v4(198, 51, 100, 9, 4000),
            payload: b"payload bytes",
        };
        let mut out = [0u8; SocketDatagram::MAX_WIRE_LEN];
        let n = dg.encode(&mut out).expect("encode");
        assert_eq!(SocketDatagram::parse(&out[..n]), Ok(dg));
        // Truncation past the declared payload fails closed.
        assert_eq!(
            SocketDatagram::parse(&out[..n - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = out;
        bad_magic[0] ^= 0xFF;
        assert_eq!(SocketDatagram::parse(&bad_magic[..n]), Err(Errno::BadMagic));
    }

    #[test]
    fn empty_datagram_round_trips() {
        let dg = SocketDatagram {
            socket: 1,
            source: v6(9),
            payload: &[],
        };
        let mut out = [0u8; SocketDatagram::HEADER_LEN];
        let n = dg.encode(&mut out).expect("encode");
        assert_eq!(n, SocketDatagram::HEADER_LEN);
        assert_eq!(SocketDatagram::parse(&out[..n]), Ok(dg));
    }

    #[test]
    fn echo_reply_round_trips_and_fails_closed() {
        // A v6 echo reply carries no port; its sequence rides the port slot.
        let echo = SocketEcho {
            socket: 7,
            source: SocketAddr {
                family: NetAddrFamily::V6,
                addr: [0x20; 16],
                port: 0,
            },
            sequence: 4321,
            payload: b"echoed payload",
        };
        let mut out = [0u8; SocketEcho::MAX_WIRE_LEN];
        let n = echo.encode(&mut out).expect("encode");
        assert_eq!(SocketEcho::parse(&out[..n]), Ok(echo));
        // Truncation past the declared payload fails closed.
        assert_eq!(SocketEcho::parse(&out[..n - 1]), Err(Errno::BufferTooSmall));
        let mut bad_magic = out;
        bad_magic[0] ^= 0xFF;
        assert_eq!(SocketEcho::parse(&bad_magic[..n]), Err(Errno::BadMagic));
    }

    #[test]
    fn empty_echo_reply_round_trips() {
        let echo = SocketEcho {
            socket: 3,
            source: v4(198, 51, 100, 1, 0),
            sequence: 1,
            payload: &[],
        };
        let mut out = [0u8; SocketEcho::HEADER_LEN];
        let n = echo.encode(&mut out).expect("encode");
        assert_eq!(n, SocketEcho::HEADER_LEN);
        assert_eq!(SocketEcho::parse(&out[..n]), Ok(echo));
    }
}
