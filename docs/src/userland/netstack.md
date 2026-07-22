# Network-stack service (`netstack`)

`userland/net/netstack` is the user-space process that owns the
network (`plans/NETWORK.md` §2.2): every managed interface, its
addresses and routes, and the frame flow between the pure `lib/net`
protocol engine and the link-layer drivers over the shared-memory
frame-ring transport. It ships as the `netstack.app` service bundle
under `/System/Services/` and runs as the compiled-in `netstack`
service account (uid 14).

It is a **core boot service**: PID 1 `init` launches it from its
compiled-in startup config (`userland/system/init` `DEFAULT_CONFIG`),
after `sysinfod` and before `devmgr`, so the network endpoints are
published and the interface table is ready before `devmgr` binds any
discovered NIC device channel to it (`plans/NETWORK.md` N4d). On the
aarch64 production image the service is spawned from its verified
on-disk bundle; on x86_64/riscv64 it is part of the compiled-in boot
floor (`spawn_layout::SPAWN_PROGRAMS`) until those targets' on-disk
stores land. Until a NIC is bound the interface table is empty, the
one-shot deadline is unarmed, and the loop parks solely on its
endpoints — the service does no work and consumes no CPU.

## Architecture

- **The engine stays pure.** All protocol behaviour lives in
  `lib/net`; the service is thin glue. Each managed interface is one
  `tairix_net::stack::Stack`, named by its admin-chosen alias (`wan`,
  `lan0` — never a discovery-order name).
- **Frames move over rings, not IPC payloads.** The service owns a
  `FrameRings` pair per interface and pumps it through the driver's
  `Net::service` doorbell (`docs/src/drivers/network.md`): engine
  output is queued into the TX ring, the driver call moves frames both
  ways, and every delivered frame runs back through the engine — whose
  replies are queued and flushed in the same pass.
- **Event-driven only.** The `Run` binary binds the reserved
  `NETSTACK_ENDPOINT` (requires `CAP_IPC_BIND_PRIVILEGED`) and parks
  on a wait set; the engines' earliest `next_deadline` arms the
  one-shot wait timeout. There is no polling loop. Each bound NIC's
  notify port joins the wait set, so a received-frame wake pumps just
  that interface.
- **Live NIC binding.** `BindDriver` (below) makes the stack the
  `netchan-v1` *client* of a NIC driver process: it sizes the shared
  frame region from the driver's `DeviceFacts`, `shm_create`s and
  grants it, `port_bind`s a per-interface notify port
  (`net_channel::notify_endpoint_for`), `NetChannelClient::attach`es,
  and derives the interface's IPv6 identity (modified EUI-64 of the
  device MAC, `tairix_net::iface::eui64_interface_id`) and a CSPRNG
  IPv4 identification seed. The one generic
  `Netstack::service_interface` pump drives both an in-process device
  (`LocalFrameService`) and a channel-backed one (`NetChannelClient`)
  identically (`docs/src/drivers/network.md`).

## The `netstack-v1` IPC surface

One fixed-width, fail-closed request frame
(`tairix_abi::net_ipc::NetstackRequest`); every request is
capability-checked against the caller's kernel-attested origin
**before any state is touched**, and every mutation and refusal is a
structured audit record (event range `16000..17000`).

| Operation | Gate | Reply |
|---|---|---|
| `InterfaceList` | `CAP_NET_ADMIN` | paged interface aliases |
| `AddrAdd` / `RouteAdd` | `CAP_NET_ADMIN` | status frame |
| `Counters` | `CAP_NET_ADMIN` | the interface's monotonic stack counters |
| `InterfaceFacts` / `InterfaceState` | `CAP_SYSINFO_INTROSPECT` | paged facts / link+address records |
| `BindDriver` | `CAP_NET_ADMIN` | status frame (the device manager hands the stack a discovered NIC driver's device-channel endpoint under a `netN` alias) |

The facts/state reads are the *broker* surface: `netstack` answers
whole-system interface state only to the System Information service,
exactly as the kernel's introspection primitive does, and all
per-client narrowing lives in that audited broker (`sysinfod` gates
facts on `CAP_SYSINFO_HW` — the record carries the MAC, stable
hardware identity — and state on `CAP_SYSINFO_GLOBAL`).

## Sockets (the data plane)

Alongside the admin endpoint the service binds a second reserved
rendezvous, `NETSTACK_SOCKET_ENDPOINT`, and serves the `netsock-v1`
contract (`docs/src/abi/net-sockets.md`) from the same event-driven loop.
`tairix_netstack::SocketService` is the origin-keyed socket table — one id
space for **datagram** (UDP) and **stream** (TCP) sockets alike, as a POSIX
fd table holds every kind — gating every call on `CAP_NET` before any state
is touched. A datagram socket demultiplexes each inbound
`StackEvent::UdpDatagram` to its bound socket and delivers a
`SocketDatagram`. A stream socket owns one pure `tairix_net::tcp::conn::Tcb`
per connection: `Connect` actively opens it (CSPRNG ISN, egress interface
chosen once), inbound `StackEvent::TcpSegment`s drive it, and the service
turns the results into segment egress (through `Stack::send_tcp`) and
client-visible `SocketStreamEvent`s (`Connected`/`Data`/`Closed`). Stream
timers (retransmit, delayed ACK, persist, user timeout, TIME-WAIT) fold
into the wait-set deadline and run on the timer wake, so the stream path is
event-driven with no polling.

## Observability

`info:net/<iface>/{mac,mtu,kind}` and
`state:net/<iface>/{link,address}` resolve through `lib/procinfo`'s
userspace resolver onto the `NET_INTERFACE_FACTS` /
`NET_INTERFACE_STATE` sysinfo queries — never a `/proc` shape, never
text scraping (`plans/NETWORK.md` §5). Addresses render canonically
(dotted-quad v4; RFC 5952 v6) with their SLAAC/DAD state annotated.

## Capabilities

The bundle requests `CAP_NET_RAW` (the NIC frame rings, and to call a
driver's restricted-sender device channel), `CAP_SHM` (create and
grant the shared frame-ring region each channel client owns),
`CAP_IPC_BIND_PRIVILEGED` (the reserved endpoint), and `CAP_LOG_EMIT`
(audit records); the service account's ceiling
(`tairix_users::NETSTACK_CEILING`) carries exactly those. The service
*enforces* `CAP_NET_ADMIN` against its callers and never holds it;
the administrator account ceiling — and the device manager, which
makes the `BindDriver` call — carries it.

## Crash containment

`netstack` dying resets network state but never the system: the
kernel holds only endpoint plumbing, never protocol state, and PID 1
supervises and relaunches the service.

## Tests

`cargo test -p tairix-netstack` drives the engine end-to-end over a
loopback fake whose "device" is a full peer `Stack` (v4 ARP + echo
and v6 DAD + ND + echo round-trips through the real ring pump), a TCP
stream connect-and-echo through the real socket-service pump against a
passive-peer echo server, and the dispatcher's capability-refusal/audit
matrix.

The `netstack_autoload_qemu_aarch64` and `netstack_autoload_qemu_riscv64`
QEMU verticals (`plans/NETWORK.md` N4e-β / N4e-riscv64) prove the service
live in the **two-process** production boot on both arches: the autoloaded
virtio-net driver runs in its own process, `devmgr` calls `BindDriver`, and
`netstack` auto-configures the interface's EUI-64 IPv6 link-local (no IPv4)
and answers a host peer's link-local echo — witnessed by `devmgr`'s
`NETSTACK_BOUND`, the stack's `DRIVER_BOUND`, and the stack's
`INBOUND_ECHO_SERVED` audit records (a provisioning failure now also reports
its errno through `DRIVER_BIND_FAILED`, fail-loud). The riscv64 vertical is
the headless `virt`-board virtio-mmio / PLIC analogue; x86_64 (over virtio-PCI)
is the remaining follow-up.
