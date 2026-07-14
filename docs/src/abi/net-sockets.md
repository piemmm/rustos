# Datagram socket ABI (`netsock-v1`)

The socket ABI is how a program opens UDP sockets through the user-space
network stack (`userland/net/netstack`). It is defined once, as a pure wire
contract, in `lib/abi/src/net.rs` (`rustos_abi::net`), so the client-side
wrappers and the serving stack share a single source of truth and can never
drift.

This page documents the wire contract landed in increment **N4a** of
`plans/NETWORK.md`. The serving dispatcher, the `CAP_NET` capability, the
`lib/rt` client wrappers, and the end-to-end QEMU verticals land in **N4b**;
this page describes the contract they build on.

## The microkernel-honest transport

The kernel owns **no** socket object — it holds only endpoint plumbing
(`plans/NETWORK.md` §2.2). A socket is entirely network-stack/userland state.
Two planes make that work:

- **Control plane.** `socket`, `bind`, `connect`, `send`, `close`, and
  multicast `join`/`leave` are fixed-header request/reply calls
  (`SocketRequest`) on the reserved, kernel-brokered call endpoint
  `NETSTACK_SOCKET_ENDPOINT` (distinct from the admin
  `net_ipc::NETSTACK_ENDPOINT`). Every request carries the caller's
  kernel-attested `Origin` (read with `call_peer_origin`, never a claimed
  field), is capability-checked before any state is touched, validated whole,
  and refused with one typed `Errno`.
- **Receive plane.** Inbound datagrams are **not** a round-trip. When the
  stack has a datagram for a socket it `ipc_send`s a framed `SocketDatagram`
  to the async **port** the client bound and named in `SocketRequest::Socket`.
  The client parks on that port with the existing `WaitSourceKind::Port`
  wait-set member and drains it with `ipc_recv`, authenticating the stack's
  kernel-attested sender origin on each message — exactly the pattern an app
  uses for window events (`plans/APPWIN.md` AW3).

There is deliberately **no** kernel `WaitSourceKind::Socket`. Readiness rides
the generic async-mailbox primitive the kernel already has, so the kernel is
never taught about an object it does not own. This is an in-place evolution of
`plans/NETWORK.md` §2.4, which had anticipated a new wait-set kind.

## Requests (`SocketRequest`)

Each request is one fixed 44-byte header; only `Send` carries a trailing
payload (at most `SOCKET_MAX_DATAGRAM` bytes). Every field a given operation
does not use must be zero — a dirty reserved field is refused as
`Errno::BadMagic`, so no operation can smuggle authority through another's
fields.

| Operation | Fields | Reply |
|---|---|---|
| `Socket` | family, `SocketType`, delivery-port endpoint id | `SocketId` (`encode_socket_reply`) |
| `Bind` | socket, local `SocketAddr` (port `0` ⇒ CSPRNG ephemeral) | bound port (`encode_bind_reply`) |
| `Connect` | socket, peer `SocketAddr` | status |
| `Send` | socket, optional dest (`None` ⇒ connected peer), payload | status |
| `Close` | socket | status |
| `JoinMulticast` / `LeaveMulticast` | socket, group `SocketAddr` (port must be `0`) | status |

`SocketType` serves only `Datagram` in this increment; the reserved stream
(`1`) and raw (`3`) values fail closed at decode. A `SocketAddr` is a family,
a 16-byte address (IPv4 uses the first four; the tail must be zero), and a
host-order port.

## Delivery (`SocketDatagram`)

A `SocketDatagram` is the 36-byte-header-plus-payload frame the stack
`ipc_send`s to a socket's delivery port: the receiving `SocketId`, the peer
`SocketAddr` it came from, and the payload. The client decodes it after
`ipc_recv`.

## Fail-closed decoding

Every decoder (`SocketRequest::from_bytes`, `SocketDatagram::parse`, and the
reply decoders) is total and fails closed: an unknown magic, version,
operation, family, or socket type, a dirty reserved field, a group address
carrying a port, or an over-length payload is refused with a typed `Errno`
rather than guessed. The decoders are exercised by the `lib/abi`
never-panic/round-trip fuzz harness (`tests/fuzz_decode.rs`).

## Error vocabulary

Socket refusals use the shared `Errno` table, including the network codes
`AddressInUse`, `AddressUnavailable`, `NetworkUnreachable`, `NotConnected`,
and `LimitExceeded` (the last is the fail-closed result of a principal
reaching its accounted socket quota).
