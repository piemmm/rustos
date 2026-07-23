# Socket ABI (`netsock-v1`)

The socket ABI is how a program opens UDP **datagram** and TCP **stream**
sockets through the user-space network stack (`userland/net/netstack`). It is
defined once, as a pure wire contract, in `lib/abi/src/net.rs`
(`tairix_abi::net`), so the client-side wrappers and the serving stack share a
single source of truth and can never drift.

The wire contract landed in increment **N4a** of `plans/NETWORK.md`; the
serving dispatcher, the `CAP_NET` capability, and the `lib/rt` client
wrappers landed in **N4b**. The only remaining N4b piece is the live
*data path* through the running service, which waits on NIC autobind (a
driver bound into the `netstack` process) — a later increment. Until then
the socket control plane is fully served, and the datagram data path is
exercised end to end by the `lib/net` engine tests and the netstack
service tests, which drive the same `SocketService` engine over the same
`lib/net` `Stack` the live service runs.

## The serving side (`CAP_NET`, N4b)

The `netstack` service binds a second reserved endpoint,
`NETSTACK_SOCKET_ENDPOINT`, alongside its admin endpoint and serves it from
the same event-driven wait-set loop. Each socket request is dispatched by
`tairix_netstack::SocketService::serve`, which:

- **checks `CAP_NET` before any state is touched** — the coarse "originate
  transport traffic" capability (`CapabilityId::NET`), granted to ordinary
  interactive accounts in the session baseline and enforced against the
  caller's kernel-attested `Origin`; a caller without it is refused
  `PermissionDenied` and the denial is audited;
- keys every socket to the creating principal's unforgeable `ProcId`, so a
  handle is meaningless — and reported absent (`NotFound`) — to any other
  principal even if observed;
- binds ports **globally uniquely** (no silent reuse); a `port` of `0`
  draws a CSPRNG ephemeral port from the kernel random subsystem;
- bounds the socket table per principal and globally, failing closed with
  `LimitExceeded` at capacity; and
- demultiplexes each inbound `StackEvent::UdpDatagram` to the owning
  socket, honouring a connected socket's peer filter and multicast
  membership, and delivers it as a `SocketDatagram` to that socket's
  delivery port.

Multicast **transmit** rides the same path: a datagram addressed to a group
is sent straight to the group MAC with a link-local scope (TTL/hop-limit 1),
needing no route and no membership (a host may send to a group it has not
joined).

## The client side (`tairix_rt::net`)

A first-party Rust program links the thin client wrappers in
`tairix_rt::net` — `socket`, `bind`, `connect`, `send`, `recv`, `close`,
`join_multicast`, `leave_multicast` — which marshal over `ipc_call` to the
socket endpoint (control plane) and `ipc_recv` on the client's own delivery
port (receive plane). `recv` returns both the decoded datagram and the
kernel-attested sender `Origin` so the caller can reject a forged sender:
the delivery port is otherwise an unauthenticated inbox (fail closed). The
wrappers add no authority — every capability and input check stays kernel-
and stack-side.

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
| `Listen` | socket (a bound stream socket) | status |
| `Accept` | listening socket, child delivery-port endpoint id | child `SocketId` (`encode_socket_reply`), or `WouldBlock` |

`SocketType` serves `Datagram` (`2`) and `Stream` (`1`); the reserved raw
(`3`) value fails closed at decode. A `SocketAddr` is a family, a 16-byte
address (IPv4 uses the first four; the tail must be zero), and a host-order
port. For a stream socket `Send` carries no destination (`dest` must be
`None` — the peer is fixed at `Connect`) and its reply is the accepted byte
count (`encode_send_reply`), since a stream `send` is flow-controlled and may
accept fewer bytes than offered; a datagram `Send` is all-or-nothing and
replies bare status.

## Stream sockets (TCP, N5c)

A stream socket is opened with `SocketType::Stream`, then actively opened to
a peer with `Connect` (which returns immediately — the three-way handshake
runs asynchronously in the stack, driving the pure RFC 9293
`tairix_net::tcp::conn::Tcb` the stack owns per connection). The connection's
CSPRNG initial sequence number and its egress interface are chosen by the
stack at `Connect`. `Send` enqueues bytes onto the connection's bounded send
buffer; `Close` begins an orderly teardown (FIN) and the connection is reaped
in the background once fully closed.

### Passive open — `Listen` / `Accept` (N6b-2)

A server binds the socket to its local port, then `Listen` makes it passive:
the stack drives a demultiplexing `tairix_net::tcp::listen::Listener` on that
port, with the SYN-flood defence (a bounded half-open backlog and stateless
RFC 4987 SYN cookies on overflow, keyed by an HMAC-SHA256 per-boot secret
drawn from the platform RNG — `lib/crypto`, never hand-rolled). Each completed
handshake becomes a **pending** child stream socket keyed to the same
principal; the stack delivers an `Accepted` readiness event to the listener's
delivery port. The server responds with `Accept`, which claims the oldest
pending connection, binds it to a caller-supplied delivery port, hands back a
new child `SocketId`, and delivers any bytes the peer already sent. `Accept`
with no connection ready replies the retryable `WouldBlock`. Until a
connection is accepted its received bytes buffer in the bounded TCB, so the
server never sees data for a connection it has not taken.

Binding a **privileged** (well-known) local port — at or below
`SOCKET_PRIVILEGED_PORT_MAX` (1023) — requires the
`CapabilityId::NET_BIND_PRIVILEGED` capability (id 38), a further gate beyond
`CAP_NET`: an unprivileged process cannot squat a low port and impersonate a
system service. An ephemeral (`0`) bind is never privileged. The check is at
`Bind` time, matching the Unix `CAP_NET_BIND_SERVICE` model, and a refusal is
audited and fails closed. Running a privileged network service is an
administrative act, so `CAP_NET_BIND_PRIVILEGED` is part of the administrator
account ceiling (`tairix_users::ADMINISTRATIVE_SET`); a program still receives
it only if its own signed manifest requests it, intersected with that ceiling.
The whole privileged listener path — bind, listen, accept, echo under injected
loss — is proven live by the `netstack_listener_qemu_aarch64` two-process QEMU
vertical (`plans/NETWORK.md` N6b-2-β-2): a guest `tcpserve` server against a
host client peer.

The stack reports the connection lifecycle to the client's delivery port as
`SocketStreamEvent` frames (magic `"NSKS"`), the connection-oriented analogue
of `SocketDatagram` — carrying no per-message peer (the peer is fixed):

- `Connected` — exactly once, when the handshake completes;
- `Data` — zero or more times, the received stream bytes in order (a receive
  larger than `SOCKET_MAX_DATAGRAM` is fragmented across several events);
- `Closed` — exactly once at the end, carrying a `StreamCloseReason`
  (`PeerClosed` orderly EOF, `Reset`, `TimedOut`, or `Refused`), so a
  `recv` reaching end-of-stream can tell an orderly close from an abortive
  one (the receive half never ends silently). No event follows `Closed`;
- `Accepted` — delivered to a *listening* socket's port, edge-triggered, one
  per newly ready connection: the client responds by calling `Accept` on the
  listener until it replies `WouldBlock`.

The client links `tairix_rt::net::stream_socket` / `connect` / `stream_send`
/ `listen` / `accept` / `close` and drains events with `stream_recv` (which,
like `recv`, returns the kernel-attested sender `Origin` for fail-closed
authentication).

## Delivery (`SocketDatagram`)

A `SocketDatagram` is the 36-byte-header-plus-payload frame the stack
`ipc_send`s to a datagram socket's delivery port: the receiving `SocketId`,
the peer `SocketAddr` it came from, and the payload. The client decodes it
after `ipc_recv`.

## Fail-closed decoding

Every decoder (`SocketRequest::from_bytes`, `SocketDatagram::parse`,
`SocketStreamEvent::parse`, and the reply decoders) is total and fails
closed: an unknown magic, version, operation, family, socket type, or
stream-event kind, a dirty reserved field, a group address carrying a port,
or an over-length payload is refused with a typed `Errno` rather than
guessed. The decoders are exercised by the `lib/abi` never-panic/round-trip
fuzz harness (`tests/fuzz_decode.rs`).

## Error vocabulary

Socket refusals use the shared `Errno` table, including the network codes
`AddressInUse`, `AddressUnavailable`, `NetworkUnreachable`, `NotConnected`,
and `LimitExceeded` (the last is the fail-closed result of a principal
reaching its accounted socket quota).
