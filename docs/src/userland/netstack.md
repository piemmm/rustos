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
`net.tcp.syncookies`, `net.ipv6.privacy`, `net.tcp.keepalive`,
`net.tcp.ecn`) after the root unlock and delivers them once over the
`CAP_NET_ADMIN` `ApplyNetworkSettings` admin op (audited, fail-soft-retried;
`plans/NETWORK.md` N9b-2). Until it arrives the stack holds safe defaults
(both families enabled, SYN cookies `auto`, keepalive off, ECN off).

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
  backlog, falling back to cookies only on overflow.
- **`net.tcp.keepalive true`** enables RFC 9293 §3.8.4 keepalive probing
  on every new connection, actively opened and accepted alike (the
  outbound `TcpConfig` and each listener's accepted-connection template):
  an idle connection is probed after the standard idle interval and torn
  down if the peer stops answering. `false` (the default, RFC 1122
  §4.2.3.6) never probes and never drops an idle connection for
  inactivity. Like the SYN-cookie mode it is read at connect/`listen`
  time, so it needs no per-interface re-application.
- **`net.tcp.ecn true`** enables RFC 3168 Explicit Congestion
  Notification on every new connection, actively opened and accepted
  alike (the outbound `TcpConfig` and each listener's accepted-connection
  template): the connection offers ECN in its SYN/SYN-ACK and, once
  negotiated, marks eligible segments ECT(0) and treats a CE mark as a
  congestion signal (a loss-equivalent window reduction with no
  retransmission) instead of forcing a drop. `false` (the default)
  leaves connections Not-ECT. Like keepalive it is read at
  connect/`listen` time, so it needs no per-interface re-application.
  The whole switched-on path is proven live by the
  `netstack_ecn_qemu_aarch64` two-process QEMU vertical
  (`plans/NETWORK.md` N13): a guest whose planted `system.conf` set
  `net.tcp.ecn true` negotiates ECN with an ECN-capable host peer, which
  verifies on the wire that the guest offered ECN, marked its data
  ECT(0), and set CWR after the peer echoed ECE for an injected
  congestion mark.
- **`net.ipv6.privacy true`** forms RFC 8981 temporary (privacy) IPv6
  addresses: alongside the stable SLAAC address of each autonomous
  prefix, the interface adds a short-lived address with a randomised
  interface identifier, regenerated before it deprecates, and prefers it
  as the source for outbound flows (RFC 6724 rule 7). Disabled by
  default; toggling it re-applies to every managed interface (enabling
  forms them promptly, disabling removes them and keeps the stable
  address).

## Per-interface configuration (`network.conf`)

Each managed interface's addressing is declared in one document,
`/System/Settings/Network/network.conf`, whose grammar, closed key
registry, typed values, bounded fail-closed parser, and canonical render
are the one `lib/netconfig` engine (`plans/NETWORK.md` §6.1). As with the
stack-wide policy, `netstack` never reads it: the FS-capable device
manager reads it post-unlock, maps each managed interface that carries a
stable hardware identity — its `match.mac` (MAC) or `match.node` (bus
location) — into a `NetInterfaceConfigMsg`, and delivers it over the
`CAP_NET_ADMIN` admin endpoint. A managed interface carrying neither
selector cannot be bound to hardware by identity and is surfaced loud
(`devmgr` event `13_016`), never silently ignored.

`netstack` locates the interface by its **stable hardware identity** — the
device MAC it holds from the driver's facts, or the hardware location the
device manager recorded at bind time (the register-window base of the
matched node, which a `match.node` selector names) — and renames that
interface to the admin-chosen alias, so an interface first
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

## Link aggregation (bonds)

A **bond** is a virtual interface `netstack` composes over two or more
member NICs (`plans/NETWORK.md` §6.3): the bond owns the addresses,
routes, and neighbour cache, while its members are the physical NICs that
carry its frames but hold **no** addresses of their own (a member refuses
a direct address/route assignment with a typed error — the bond owns
them). Sockets and the routing table see one interface; member fan-out is
internal to the interface table. The device manager derives from
`network.conf` (a) an address-less rename for each member (matched by its
hardware identity, `match.mac` or `match.node`), (b) a `NetBondConfigMsg`
composing the bond over those
members, and (c) the bond's own addressing (matched by the bond alias),
delivering all three over the `CAP_NET_ADMIN` admin endpoint and retrying
`NotFound` until each member has bound — so a bond composes once all its
members are present, in any bind order. The delivery re-attempts the
still-pending per-interface configs after composing the bonds (a bounded
until-stable pass), so the bond's own address lands in the *same* bump the
bond was composed, not a later one — a per-interface config for the bond
alias returns `NotFound` until the bond exists.

Failover is driven by the pure `tairix_net::bond` engine: the bond
inherits its first member's MAC (kept stable for its life, so a peer's
ARP/ND cache survives failover). The live link-down/up report is the sole
source of a failover: a NIC driver senses its link (the virtio-net driver
negotiates `VIRTIO_NET_F_STATUS` and reads the device-config link bit,
updated on the config-change interrupt it already wakes the stack for) and
carries the current `LinkState` on every `netchan` `Service` reply
(`ServiceReport.link`); `service_interface` turns a change into a
`set_member_link` report the service (`run.rs` `on_member_link_change`)
drives. A member that loses its link becomes
ineligible **immediately**, and transmit re-targets a healthy member
within one link-down report; a recovered member (or a declared `primary`)
is readmitted only after one `monitor-interval` up-delay (deliberate
failback, never flapping), driven by the tickless failover monitor folded
into the service's wait-set deadline. On a transmit-path change the bond
re-announces its presence with a gratuitous ARP / unsolicited Neighbour
Advertisement out the newly-selected member so peers relearn the path, and
the change is audited (`BOND_FAILOVER`). When no member is eligible the
bond's aggregate link is down and transmit fails closed. Two transmit
policies form a closed set: `active-backup` (one transmitting member,
ordered failover, an optional reclaiming `primary`) and `balance`
(flow-hashed spread — one flow stays on one member so a TCP stream never
reorders across links). Runtime reload (`configure` + a redelivered
`NetBondConfigMsg`) reconciles mode, primary, monitor interval, and
membership in place; a released member returns to a plain, addressable
interface.

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

A bond's live topology and failover state resolve the same way:
`info:net/<bond>/members` (the member aliases in configured order),
`state:net/<bond>/active-member` (the currently-transmitting member in
active-backup, `none` in balance mode or while the bond is down), and
`state:net/<bond>/member-health` (each member `up`/`down` with its
`eligible`/`active` flags) resolve through the `NET_BOND_MEMBERS` sysinfo
query, gated `CAP_SYSINFO_GLOBAL` and audited like the interface state
(link aggregation is system-wide topology and its live failover state,
`plans/NETWORK.md` §5, §6.3). A non-bond alias fails closed.

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

The `netstack_autoload_qemu_{aarch64,riscv64,x86_64}` QEMU verticals
(`plans/NETWORK.md` N4e-β / N4e-riscv64 / A4) prove the service live in the
**two-process** production boot on all three Tier-1 targets: the autoloaded
virtio-net driver runs in its own process, `devmgr` calls `BindDriver`, and
`netstack` auto-configures the interface's EUI-64 IPv6 link-local (no IPv4)
and answers a host peer's link-local echo — witnessed by `devmgr`'s
`NETSTACK_BOUND`, the stack's `DRIVER_BOUND`, and the stack's
`INBOUND_ECHO_SERVED` audit records (a provisioning failure now also reports
its errno through `DRIVER_BIND_FAILED`, fail-loud). aarch64 and riscv64 drive
the headless `virt`-board virtio-mmio / GIC-or-PLIC path; x86_64 drives
virtio-PCI with kernel-routed MSI-X.

The `netstack_static_qemu_{aarch64,riscv64,x86_64}` and
`netstack_bond_qemu_{aarch64,riscv64,x86_64}` verticals prove the declarative
`network.conf` path live on all three Tier-1 targets. The static vertical binds
one NIC to an admin alias by `match.node` (its bus location — the virtio-mmio
transport slot base on aarch64/riscv64, the config-window BAR base on x86_64)
and assigns it a static IPv6 address. The bond vertical composes an
active-backup bond over **two** NICs (bound by `match.mac`, so its config is
arch-neutral) with a static address, then drops the primary member's carrier
mid-flow over the QEMU monitor (`set_link net0 off`): the driver's
`VIRTIO_NET_F_STATUS` config-change interrupt reports the link down and the
bond fails over to the surviving member, witnessed by `BOND_CONFIG_APPLIED`,
`BOND_FAILOVER`, and a post-failover `INBOUND_ECHO_SERVED` (the ordering makes
a pre-failover echo insufficient) — the end-to-end proof of the live
link-status → failover path.

The `netstack_dhcp_qemu_{aarch64,riscv64,x86_64}` verticals prove the RFC 2131
dynamic-addressing path live on all three Tier-1 targets (`plans/DHCP.md` D3).
The planted `network.conf` binds the NIC to the `wan` alias by `match.node`
(its per-bus location, as the static vertical does) but selects `ipv4.method
dhcp` and disables IPv6, so the interface forms **no** address of its own: the
stack must drive its DHCP client to lease one from the harness-side DHCP-server
peer (`NetPeerMode::V4DhcpEcho`), which OFFERs/ACKs a lease and then pings the
guest at the leased address. Witnessed by `devmgr`'s `NETSTACK_BOUND`, the
stack's `DHCP_LEASE_ACQUIRED`, and a post-lease `INBOUND_ECHO_SERVED`, plus the
peer's own offered/acked/echo verdict — so a broken lease cannot pass on an
address the guest formed itself (it forms none). The three arches differ only
in the NIC's bus (aarch64/riscv64 virtio-mmio, x86_64 virtio-PCI) and hence the
`match.node` the planted config names.
