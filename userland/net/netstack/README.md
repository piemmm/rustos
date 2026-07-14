# rustos-netstack

The RustOS network-stack service (`plans/NETWORK.md` §2.2, N3b): the
user-space process that owns every managed network interface and is the
thin, audited glue around the pure `lib/net` protocol engine.

Stability tier: **experimental** — the `netstack-v1` IPC surface and the
engine API evolve in place while `abi-v1` is unfrozen.

## What lives here

* `src/iface.rs` — the interface table: one `rustos_net::stack::Stack`
  per managed interface, named by its admin-chosen alias; address/route
  mutation, counters, the typed facts/state records, the frame-ring pump
  (`service_interface`) between the engine and a `Net` driver, and the
  earliest one-shot deadline the event loop arms.
* `src/service.rs` — the admin request dispatcher: decodes one
  fixed-width `netstack-v1` frame, enforces `CAP_NET_ADMIN` (admin
  surface) or `CAP_SYSINFO_INTROSPECT` (the System Information broker's
  whole-state reads) against the caller's kernel-attested origin
  **before touching state**, applies it, audits it.
* `src/socket.rs` — the datagram-socket service (`plans/NETWORK.md`
  N4b): the origin (`ProcId`)-keyed socket table and the `netsock-v1`
  dispatcher, gating every call on `CAP_NET` before any state is
  touched. CSPRNG ephemeral ports (injected entropy), globally-unique
  port binding, per-principal + global bounded accounting failing
  closed with `LimitExceeded`, and inbound `UdpDatagram` demux to each
  socket's delivery port.
* `src/run.rs` — the freestanding `Run` binary: binds the reserved
  admin `NETSTACK_ENDPOINT` **and** socket `NETSTACK_SOCKET_ENDPOINT`
  (both need `CAP_IPC_BIND_PRIVILEGED`), parks on a wait set with the
  engines' one-shot deadline as timeout, and serves both. NIC
  frame-ring channels join the wait set as network drivers are bound to
  the service (NIC autobind is a later increment), so a live socket
  send fails closed until a NIC is bound; the datagram data path is
  proven by the engine tests.
* `src/events.rs` — the reserved `16000..17000` audit event range.

## Capabilities

Requested by the bundle manifest: `CAP_NET_RAW` (the NIC frame rings),
`CAP_IPC_BIND_PRIVILEGED` (the reserved endpoints), `CAP_LOG_EMIT`
(audit records). The service *enforces* `CAP_NET_ADMIN` (admin surface)
and `CAP_NET` (socket surface) against its callers; it holds neither.

## Testing

Host tests drive the engine end-to-end over a loopback fake whose
"device" is a full peer `Stack` (v4 ARP + echo and v6 DAD + ND + echo
round-trips through the real ring pump) and exercise the dispatcher's
capability-refusal/audit matrix. `cargo test -p rustos-netstack`.
