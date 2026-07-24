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

Two fixed-width, fail-closed framed messages arrive on the admin
endpoint: the 64-byte `tairix_abi::net_ipc::NetstackRequest` (below) and
the wider per-interface `NetInterfaceConfigMsg` (its own `"NIC1"` magic;
see *Per-interface configuration*). The service transport tells them
apart by magic and decodes the interface-config message before the
request enum, the `BindDriver`-interception precedent. Every request is
capability-checked against the caller's kernel-attested origin **before
any state is touched**, and every mutation and refusal is a structured
audit record (event range `16000..17000`).

| Operation | Gate | Reply |
|---|---|---|
| `InterfaceList` | `CAP_NET_ADMIN` | paged interface aliases |
| `AddrAdd` / `RouteAdd` | `CAP_NET_ADMIN` | status frame |
| `InterfaceFacts` / `InterfaceState` / `InterfaceCounters` | `CAP_SYSINFO_INTROSPECT` | paged facts / link+address / monotonic-counter records |
| `InterfaceRates` | `CAP_SYSINFO_INTROSPECT` | paged windowed throughput-rate records (carries the caller's averaging window) |
| `Sockets` | `CAP_SYSINFO_INTROSPECT` | paged open-socket records (protocol, state, local/peer address, owning pid, queue depths) — the `ss`/`netstat` socket table |
| `BindDriver` | `CAP_NET_ADMIN` | status frame (the device manager hands the stack a discovered NIC driver's device-channel endpoint under a `netN` alias) |
| `ApplyNetworkSettings` | `CAP_NET_ADMIN` | status frame (the stack-wide `net.*` policy — IPv4/IPv6 family enable and the TCP SYN-cookie mode) |

The facts/state reads are the *broker* surface: `netstack` answers
whole-system interface state only to the System Information service,
exactly as the kernel's introspection primitive does, and all
per-client narrowing lives in that audited broker (`sysinfod` gates
facts on `CAP_SYSINFO_HW` — the record carries the MAC, stable
hardware identity — and state on `CAP_SYSINFO_GLOBAL`).

## Stack-wide network policy (`net.*`)

`netstack` is the network-parsing sandbox and holds no filesystem
capability (`plans/NETWORK.md` §0), so it never reads `system.conf`
itself. The FS-capable device manager reads the `net.*` registry keys
(`lib/sysconfig`, §6.2 — `net.ipv4.enabled`, `net.ipv6.enabled`,
`net.tcp.syncookies`) after the root unlock and delivers them once over
the `CAP_NET_ADMIN` `ApplyNetworkSettings` admin op (audited,
fail-soft-retried; `plans/NETWORK.md` N9b-2). Until it arrives the stack
holds safe defaults (both families enabled, SYN cookies `auto`).

The policy is enforced, not advisory:

- **A disabled family binds no address and answers nothing.** With
  `net.ipv6.enabled false` an interface forms no link-local, accepts no
  inbound IPv6 (an inbound Router Advertisement cannot SLAAC-configure
  it), and a socket `open` for the family is refused up front
  (`Errno::NotSupported`, audited) rather than handed a dead handle;
  `net.ipv4.enabled false` is the symmetric IPv4 case. Applying the
  policy reconfigures every interface already managed as well as those
  bound later, so delivery order does not matter (idempotent).
- **`net.tcp.syncookies always`** sets each new listener's
  `max_half_open = 0`, so it holds no half-open state and answers every
  SYN with a stateless RFC 4987 cookie; `auto` keeps the bounded default
  backlog, falling back to cookies only on overflow. (`net.ipv6.privacy`
  has no enforcement consumer yet and is deliberately not delivered.)

## Per-interface configuration (`network.conf`)

Each managed interface's addressing is declared in one document,
`/System/Settings/Network/network.conf`, whose grammar, closed key
registry, typed values, bounded fail-closed parser, and canonical render
are the one `lib/netconfig` engine (`plans/NETWORK.md` §6.1). As with the
stack-wide policy, `netstack` never reads it: the FS-capable device
manager reads it post-unlock, maps each **managed, non-bond** interface
that carries a stable `match.mac` identity into a `NetInterfaceConfigMsg`,
and delivers it over the `CAP_NET_ADMIN` admin endpoint. A managed
non-bond interface with no `match.mac` cannot be bound to hardware by
identity (`match.node` binding is a later increment) and is surfaced loud
(`devmgr` event `13_016`), never silently ignored; bond interfaces and
their members are omitted (bonding is a later increment).

`netstack` locates the interface by its **stable MAC** — it is the only
component holding each interface's MAC, from the driver's facts — and
renames that interface to the admin-chosen alias, so an interface first
brought up under a derived `netN` alias becomes `wan`/`lan0` once its
configuration is applied. The apply is **atomic per interface**: the whole
message is validated (unicast addresses, an on-subnet IPv4 gateway, a
`≥ 1280` MTU override) before any state is touched, so a refusal leaves
the interface untouched, and it is idempotent (re-applying the same
configuration is a success, not a duplicate). Because interfaces bind
asynchronously as their drivers come up, delivery of an interface not yet
present returns `NotFound` and is retried silently on the next
hardware-tree generation bump; a successful apply is recorded so it is not
re-pushed (`devmgr` events `13_014`/`13_015`). The image ships an **empty**
`network.conf` ("no managed interfaces beyond loopback"); the installer,
or `configure`, writes the operator's interfaces through the same engine.

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

`info:net/<iface>/{mac,mtu,kind}`,
`state:net/<iface>/{link,address}`,
`stats:net/<iface>/{rx,tx}.{packets,bytes,dropped}`, and the windowed
throughput rates `stats:net/<iface>/{rx,tx}.{pps,bps}?window=…` resolve
through `lib/procinfo`'s userspace resolver onto the `NET_INTERFACE_FACTS`
/ `NET_INTERFACE_STATE` / `NET_INTERFACE_COUNTERS` / `NET_INTERFACE_RATES`
sysinfo queries — never a `/proc` shape, never text scraping
(`plans/NETWORK.md` §5). A rate is the average over the window that
*actually* elapsed (an interface with too little history reports a
shorter, possibly zero, window rather than a fabricated figure); the meter
is tickless — it snapshots counters opportunistically as the service
wakes, never on a periodic timer. Addresses
render canonically (dotted-quad v4; RFC 5952 v6) with their SLAAC/DAD
state annotated. The per-interface counters are monotonic since boot; a
denial-of-service in progress is visible through the stack-wide
aggregates `stats:net/stack/{icmp-errors,icmp-suppressed,reassembly-evicted}`,
summed across every managed interface. The counter reads are gated on
`CAP_SYSINFO_GLOBAL` (system-wide network metrics), like the address
state; the engine keeps one honest `dropped` bucket per direction rather
than a fabricated errors/dropped split.

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
