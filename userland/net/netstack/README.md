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
* `src/service.rs` — the request dispatcher: decodes one fixed-width
  `netstack-v1` frame, enforces `CAP_NET_ADMIN` (admin surface) or
  `CAP_SYSINFO_INTROSPECT` (the System Information broker's whole-state
  reads) against the caller's kernel-attested origin **before touching
  state**, applies it, audits it.
* `src/run.rs` — the freestanding `Run` binary: binds the reserved
  `NETSTACK_ENDPOINT` (needs `CAP_IPC_BIND_PRIVILEGED`), parks on a
  wait set with the engines' one-shot deadline as timeout, and serves.
  NIC frame-ring channels join the wait set as network drivers are
  bound to the service; the QEMU vertical wiring is N3c.
* `src/events.rs` — the reserved `16000..17000` audit event range.

## Capabilities

Requested by the bundle manifest: `CAP_NET_RAW` (the NIC frame rings),
`CAP_IPC_BIND_PRIVILEGED` (the reserved endpoint), `CAP_LOG_EMIT`
(audit records). The service *enforces* `CAP_NET_ADMIN` against its
callers; it never holds it.

## Testing

Host tests drive the engine end-to-end over a loopback fake whose
"device" is a full peer `Stack` (v4 ARP + echo and v6 DAD + ND + echo
round-trips through the real ring pump) and exercise the dispatcher's
capability-refusal/audit matrix. `cargo test -p rustos-netstack`.
