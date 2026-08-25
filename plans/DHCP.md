# DHCP.md — Dynamic IPv4 address configuration (RFC 2131 / RFC 2132)

Staged build plan for TAIRiX's DHCPv4 client. **Binding under `AGENTS.md`**
(read it first, especially §2, §4, §5, §17, §19, §24, §26); it consumes the
seams `plans/NETWORK.md` fixes and never contradicts it — where the two
touch, NETWORK.md's decisions stand. `abi-v1` is not frozen (PLAN.md
Stage 1): the ABI/config additions here are ordinary pre-release changes
(§2.13).

## 0. Scope and decisions (binding)

- **The DHCPv4 client is a stack-internal address-configuration source, not
  a userland service.** It sits beside RFC 4862 SLAAC in the netstack
  interface engine: an interface that is configured to use DHCPv4 drives the
  client, which yields an address, mask, routers, and lease timers that the
  interface applies exactly as it applies a SLAAC or static address. It is
  **not** a `userland/*` process over the socket ABI — a DHCP client must
  transmit from `0.0.0.0:68` broadcast *before* any address exists, which the
  capability-gated, route-checked UDP socket surface (correctly) refuses
  (`NetworkUnreachable`). Framing DHCP inside the stack, which owns the
  interface's egress, needs no new socket surface and grants no ambient
  authority (§4).
- **One pure engine, host-testable: `lib/net::dhcp`.** All wire parsing,
  message building, and the RFC 2131 client state machine live in the pure,
  `no_std`, `#![forbid(unsafe_code)]`, allocation-bounded `lib/net` crate,
  driven by injected monotonic time and caller-supplied CSPRNG values
  (transaction id, backoff jitter) — the engine never generates randomness
  itself (the `tcp::conn` `iss` precedent). The netstack integration is thin
  glue that frames the engine's output as UDP/IP/Ethernet and feeds replies
  back in. This is the §2.2 one-definition rule: the unit tests, the fuzz
  harness, and the live stack all exercise the *same* engine.
- **The server and every neighbour are hostile (§26.4).** Every server
  message is attacker-controlled: the codec is total, bounded (§24.4 fixed
  validation bounds — capped option-region walk, fixed-capacity router/DNS
  lists), fuzzed (§19.6), and fails closed. A malformed or inconsistent reply
  is dropped whole; nothing partial is applied. Off-path spoofing is bounded
  by the randomised transaction id and by matching the reply's `xid`/`chaddr`
  against the outstanding request (the engine rejects a mismatch).
- **Event-driven, tickless (§2.23).** The client exposes a folded one-shot
  `next_deadline()`; the stack arms a single timer and calls `poll(now)` when
  it fires or a reply arrives. Retransmission uses RFC 2131 §4.1 randomised
  exponential backoff; the BOUND state arms only the T1/T2/expiry deadlines.
  There is no polling loop.
- **Not in this plan:** DHCPv6 (RFC 8415) and stateless DHCPv6/RA option
  provisioning, DHCP INFORM, the DHCP *server*, and DNS/hostname *policy*
  beyond surfacing the options the lease carries. Each is a later increment
  or its own plan (§2.3/§2.4); none is speculated here. RA-driven SLAAC is
  already `plans/NETWORK.md`'s and is untouched.

## 1. Target architecture (binding)

- `lib/net/src/dhcp.rs` — the pure engine:
  - **Codec** — the BOOTP fixed header (RFC 2131 §2, the 236-byte header +
    the 4-byte magic cookie) and the RFC 2132 option TLVs. Parse surfaces a
    `DhcpReply` (message type, `xid`, `yiaddr`, server identifier, subnet
    mask, routers, DNS servers, lease/T1/T2 times) from the recognised
    options and skips unknown ones; RFC 2131 §4.1 option-overload (`file` /
    `sname` carrying options) is honoured. Emit is a single `write_message`
    core over a `MessageSpec` describing the fields, so DISCOVER / REQUEST
    (selecting + renew/rebind forms) / DECLINE / RELEASE share one encoder
    (§2.2). Every decode is total/bounded/fail-closed.
  - **Client state machine** (`DhcpClient`) — RFC 2131 §4.4 Figure 5:
    INIT → SELECTING → REQUESTING → BOUND → RENEWING → REBINDING, plus NAK
    and lease-expiry restart. `poll(now, rng)` advances timers and returns
    the actions the stack must take (send a framed message; apply a lease;
    tear a lease down); `on_reply(now, &DhcpReply)` folds a server message;
    `next_deadline()` is the folded one-shot. Lease timers default to
    T1 = lease/2, T2 = lease·7⁄8 (RFC 2131 §4.4.5), clamped to a consistent
    ordering; an infinite lease (`0xFFFF_FFFF`) arms no renewal.
- `lib/net/tests/fuzz_net_dhcp.rs` — the `fuzz_net_dhcp` harness (registered
  in `tools/xtask`): random and bit-flipped inputs never panic; a built
  message round-trips through the parser.

## 2. Capabilities

No new capability. The client runs inside `netstack`, which already holds
its NIC-channel capability; DHCP transmits are ordinary interface egress.
Enabling DHCP on an interface is an edit to `network.conf`
(`plans/NETWORK.md` §6.1) applied through the existing `CAP_NET_ADMIN`
reload — the same gate every other interface-addressing change uses.

## 3. Staged increments

Status marks as in NETWORK.md: `[ ]` planned, `[~]` in progress, `[x]` done.

### D1 — the pure `lib/net::dhcp` engine `[x]`
The BOOTP/DHCP codec (RFC 2131 header + RFC 2132 options, total/bounded/
fail-closed, option-overload aware) and the RFC 2131 §4.4 client state
machine (all six states + NAK/expiry restart, randomised backoff, injected
time + CSPRNG). Host unit tests cover every state transition, timer
computation (default and option-supplied T1/T2, infinite lease), codec
round-trips, `xid`/`chaddr` mismatch rejection, and fail-closed decode; the
`fuzz_net_dhcp` harness covers the parser. No I/O, no `netstack` change yet —
the engine stands alone and gate-green.

### D2 — netstack interface integration `[x]`
`Stack` drives the `DhcpClient` for an interface whose `network.conf`
selects DHCPv4. Key facts for the next worker:
- **Config:** `NetIpv4Config::Dhcp` (ABI, wire discriminant 2, no address
  fields — decode rejects any set prefix/addr/gw), `Ipv4Method::Dhcp`
  (`lib/netconfig`, spelled `dhcp`, forbids a static `ipv4.address`/
  `ipv4.gateway`), mapped by devmgr `addressing_of`.
- **Engine seam:** `Stack::{enable_dhcp(rng), disable_dhcp, dhcp_active}`.
  DHCP is an IPv4 method: `enable_dhcp` forces IPv4 on and starts clean; the
  client + its injected CSPRNG (`Box<dyn FnMut() -> u32>`) live in
  `Stack.dhcp: Option<DhcpDriver>`. The service injects the rng via
  `Netstack`'s new `dhcp_rng_factory` (a sibling of `temp_factory`);
  `apply_interface_config` enables it idempotently for `Dhcp` and calls
  `disable_dhcp` for `Static`/`Disabled`.
- **Driving:** polled from `Stack::advance`, folded into
  `Stack::next_deadline`. Send actions are framed as UDP(68→67)/IPv4/
  Ethernet — link-layer broadcast to `255.255.255.255` for DISCOVER and
  SELECTING/REBINDING REQUESTs, neighbour-resolved unicast to the server for
  a RENEWING REQUEST. A received reply (UDP 67→68) is intercepted in
  `Stack::on_ipv4` **before** the unicast-address filter (so a broadcast
  reply reaches an address-less client) and never surfaces as an ordinary
  datagram.
- **Lease:** `Configured` applies address + mask prefix + default route
  (a router off the connected subnet is refused by `set_ipv4_config`, so the
  address is applied alone — fail-safe); `Deconfigured` withdraws them. Each
  is a `StackEvent::DhcpLeaseAcquired`/`DhcpLeaseLost` the service audits
  (`netstack` events `DHCP_LEASE_ACQUIRED`=16020 / `DHCP_LEASE_LOST`=16021).
- **Tests:** `lib/net` `stack_tests` (acquire end-to-end, pre-address-filter
  intercept, expiry withdrawal, disable), ABI round-trip + smuggled-field
  rejection, netconfig parse/validate, devmgr mapping, netstack service
  enable/disable. D2 is done and gate-green.

### D3 — the live two-process QEMU vertical, all three Tier-1 arches `[x]`
`tests/integration/netstack_dhcp_qemu_{aarch64,x86_64,riscv64}` each boot the
production pipeline for their arch against the `dhcp-net-root` disk (the
signed virtio-net driver bundle plus a planted `network.conf` binding the NIC
by `match.node`, selecting `ipv4.method dhcp`, and disabling IPv6) with the
harness-side DHCP-server peer (`NetPeerMode::V4DhcpEcho`) on the QEMU `dgram`
netdev. `devmgr` autoloads the driver and delivers the config; `netstack`
drives its DHCP client, which DISCOVERs, accepts the peer's OFFER of
`wire::DHCP_LEASED_V4`, REQUESTs it, and applies the ACK — the interface's
only address (it forms none itself). The peer then pings the guest at that
leased address and the guest answers. PASS keys on three log witnesses
(`devmgr` `NETSTACK_BOUND`, `netstack` `DHCP_LEASE_ACQUIRED`, `netstack`
`INBOUND_ECHO_SERVED`) plus the peer's own verdict (it offered, acked, and
got the echo reply at the leased address), so a broken lease cannot pass on
an address the guest formed itself.

Key facts for the next worker:
- The three arches differ **only** in the bus the NIC lives on (aarch64/
  riscv64 virtio-MMIO, x86_64 virtio-PCI) and hence the `match.node` bus
  location the planted config names — the aarch64/x86_64/riscv64
  `DHCP_NETWORK_CONF_*`, planted per-arch by `dhcp_net_store_files`. The
  driver set, DHCP-server peer, disk builder, and the three witnesses are
  the one shared definition every arch reuses (§2.2); the per-arch crate is
  the thin boot-glue-plus-witness bin its static sibling already establishes.
- The host DHCP **server** is test-only (the plan ships no server), so it
  lives beside its one consumer in `tools/xtask`'s `netpeer::dhcp_server`,
  encoding/decoding the *same* wire layout the client codec now exposes
  publicly (`tairix_net::dhcp`'s `opt`, header offsets, `MAGIC_COOKIE`); a
  round-trip unit test parses every reply it builds back through the real
  `DhcpReply::parse`, so the two cannot drift.
- The shared wire constants (`DHCP_SERVER_V4`, `DHCP_LEASED_V4`,
  `DHCP_SUBNET_MASK`, `DHCP_LEASE_SECS`, `DHCP_NETWORK_CONF_{AARCH64,X86_64,
  RISCV64}`) live in `tests/integration/netstack_wire`, cross-checked against
  the real `lib/netconfig` parser.

### D4 — DHCPv6 (RFC 8415), stateful IA_NA address configuration `[x]`
Stateful DHCPv6 as an IPv6 peer of D1–D3, reusing the interface-engine
integration shape. DHCPv6 is a distinct protocol (UDP 546↔547, all-DHCP-
relay-agents-and-servers multicast `ff02::1:2`, DUID-keyed leases, IA_NA/
IAADDR bindings, four-message Solicit/Advertise/Request/Reply plus Renew/
Rebind/Release/Decline, RFC 8415 §15 RT/IRT/MRT/MRC/MRD retransmission),
so it is its own pure engine (`lib/net::dhcpv6`) beside `lib/net::dhcp`,
never a `cfg`-forked sharing of the v4 one (§2.2 carve-out for parallel
implementations of the same role).

#### D4a — the pure `lib/net::dhcpv6` engine `[x]`
The DHCPv6 wire codec (the 4-byte message header — msg-type + 3-byte
transaction id — and the RFC 8415 §21 option TLVs: Client/Server
Identifier DUID, IA_NA with its encapsulated IAADDR, Option Request,
Elapsed Time, Status Code) and the RFC 8415 §18.2.1 client state machine
(Solicit → Request → Bound → Renew → Rebind, plus Release/Decline and
lease-expiry / Reply-Status restart). Pure, `no_std`,
`#![forbid(unsafe_code)]`, allocation-bounded: a fixed-capacity IAADDR /
DNS list, a capped option-region walk, total/bounded/fail-closed decode.
Injected monotonic time and caller-supplied CSPRNG values (the 24-bit
transaction id, the RFC 8415 §15 randomised-RT jitter); the engine never
generates randomness itself (the `dhcp`/`tcp::conn` precedent). Host unit
tests cover every state transition, the RT/MRC/MRD retransmission and
lease-timer computation (default and IA-supplied T1/T2, infinite lease),
codec round-trips, transaction-id / DUID mismatch rejection, and
fail-closed decode; the `fuzz_net_dhcpv6` harness covers the parser and
the state machine. No I/O, no `netstack` change yet — the engine stands
alone and gate-green.

#### D4b — netstack interface integration `[x]`
`Stack` drives the `Dhcp6Client` for an interface whose `network.conf`
selects DHCPv6 as an IPv6 method. Key facts for the next worker:
- **Config:** `NetIpv6Config::Dhcp` (ABI, wire discriminant 3, no address
  fields — decode rejects any set prefix/addr/gw), `Ipv6Method::Dhcp`
  (`lib/netconfig`, spelled `dhcp`, forbids a static `ipv6.address`/
  `ipv6.gateway`), mapped by devmgr `addressing_of`.
- **Engine seam:** `Stack::{enable_dhcp6(rng, now), disable_dhcp6,
  dhcp6_active}`. `enable_dhcp6` turns IPv6 on (so the link-local the client
  sources from forms) and starts clean; the client + its injected CSPRNG
  (`Box<dyn FnMut() -> u32>`) live in `Stack.dhcp6: Option<Dhcp6Driver>`.
  The client's IA identifier is derived from the interface MAC (stable, no
  persisted state). The service injects the rng via the existing
  `dhcp_rng_factory` (shared with DHCPv4; both engines take
  `&mut dyn FnMut() -> u32`); `apply_interface_config` enables it
  idempotently for `Dhcp` and calls `disable_dhcp6` for
  `Static`/`Slaac`/`Disabled`.
- **Driving:** polled from `Stack::advance`, folded into
  `Stack::next_deadline`. Every send is framed UDP(546→547)/IPv6/Ethernet
  from the link-local to `ff02::1:2` at hop limit 1 (the multicast MAC is
  derived directly). The send is skipped — retried by the client's timer —
  until the link-local completes DAD, so no message is ever sourced from
  the unspecified address. A received reply (UDP 547→546) is intercepted in
  `Stack::on_ipv6` **before** the destination filter and never surfaces as
  an ordinary datagram.
- **Lease:** `Configured` assigns the leased IA_NA address as a host `/128`
  under a new `AddrOrigin::Dhcp` (DHCPv6 grants no on-link prefix — on-link
  reachability comes from RAs); the engine owns the lease lifetime so the
  interface holds it with no expiry of its own. If the address fails DAD it
  is Declined to the server and re-acquired (RFC 8415 §18.2.10.1).
  `Deconfigured` (expiry, `NoBinding`, or a changed address on renewal)
  withdraws it via `clear_ipv6_dhcp`, leaving the link-local and any
  SLAAC/static addresses intact. Each is a
  `StackEvent::Dhcp6Lease{Acquired,Lost}` the service audits (`netstack`
  events `DHCP6_LEASE_ACQUIRED`=16022 / `DHCP6_LEASE_LOST`=16023).
- **Tests:** `lib/net` `stack_tests` (acquire end-to-end, pre-filter
  intercept, expiry withdrawal, disable), ABI round-trip + smuggled-field
  rejection, netconfig parse/validate, devmgr mapping, netstack service
  enable/disable. D4b is done and gate-green.

#### D4c — the live two-process QEMU vertical, all three Tier-1 arches `[x]`
`tests/integration/netstack_dhcp6_qemu_{aarch64,x86_64,riscv64}` each boot the
production pipeline for their arch against the `dhcp6-net-root` disk (the
signed virtio-net driver bundle plus a planted `network.conf` binding the NIC
by `match.node`, selecting `ipv6.method dhcp`, and disabling IPv4) with the
harness-side DHCPv6-server peer (`NetPeerMode::V6Dhcp6Echo`) on the QEMU
`dgram` netdev. `devmgr` autoloads the driver and delivers the config;
`netstack` drives its DHCPv6 client, which Solicits, accepts the peer's
Advertise of `wire::DHCP6_LEASED_V6`, Requests it, and applies the Reply — the
interface's only global address (it forms none itself). PASS keys on three log
witnesses (`devmgr` `NETSTACK_BOUND`, `netstack` `DHCP6_LEASE_ACQUIRED`,
`netstack` `INBOUND_ECHO_SERVED`) plus the peer's own verdict (it advertised,
replied, and got the echo reply at the leased address), so a broken lease
cannot pass on an address the guest formed itself.

Key facts for the next worker:
- The three arches differ **only** in the bus the NIC lives on (aarch64/
  riscv64 virtio-MMIO, x86_64 virtio-PCI) and hence the `match.node` bus
  location the planted config names — the per-arch `DHCP6_NETWORK_CONF_*`,
  planted by `dhcp6_net_store_files`. The driver set, DHCPv6-server peer, disk
  builder, and the three witnesses are the one shared definition every arch
  reuses (§2.2).
- **Reachability is the one genuinely new thing over D3.** DHCPv6 conveys no
  on-link prefix (RFC 8415 leaves that to RAs), so the leased `/128` is not
  reachable by itself. The host peer therefore *also acts as the on-link
  router*: it periodically emits a hand-built Router Advertisement naming the
  shared `/64` on-link and **non-autonomous** (the guest installs the on-link
  route + default router but forms no SLAAC address, keeping the DHCPv6 lease
  its only global address), and gives itself `DHCP6_SERVER_V6` in that `/64`.
  Only then can the peer↔guest echo round-trip complete.
- The host DHCPv6 **server** and the RA are test-only (the plan ships no
  server and the `lib/net` engine is a host that refuses to emit an RA), so
  both live beside their one consumer in `tools/xtask`'s `netpeer::dhcp6_server`,
  encoding/decoding the *same* wire layout the client codec exposes publicly
  (`tairix_net::dhcpv6`'s `opt`/`status`/`Duid`/`MessageType`, and
  `tairix_net::nd` for the RA). Round-trip unit tests parse every server reply
  back through the real `Dhcp6Reply::parse` and the RA back through
  `NdMessage::parse`, so the two sides cannot drift.
- The shared wire constants (`DHCP6_SERVER_V6`, `DHCP6_LEASED_V6`,
  `DHCP6_PREFIX`, `DHCP6_PREFIX_LEN`, `DHCP6_LEASE_SECS`,
  `DHCP6_NETWORK_CONF_{AARCH64,X86_64,RISCV64}`) live in
  `tests/integration/netstack_wire`, cross-checked against the real
  `lib/netconfig` parser.

### D5 — DHCPv4 reaches real hardware: the shipped image default `[x]`

D1–D4c proved the engines and the three QEMU verticals, but every shipped
image still shipped the canonical **empty** `network.conf` ("no managed
interfaces beyond loopback"), so a booted machine ran the DHCP client on
nothing. With the Pi 4B's on-board NIC now driven (`plans/NETWORK.md` N14),
the flashable Raspberry Pi image ships an addressing default that actually
exercises it.

Key facts for the next worker:
- **The document** is composed by `tools/xtask`
  (`image_drivers::genet_network_conf`) and binds one `ethernet` interface by
  `match.node` — the GENET register aperture taken from the driver's own
  `tairix_drv_network_genet::GENET_REGS_CPU_BASE`, so the planted default and
  the location `devmgr` resolves from the discovered node cannot drift. It
  selects `ipv4.method dhcp` plus `ipv6.method slaac`.
- **Binding by alias is not an option**, by design: `devmgr`'s
  `interface_configs_from_config` refuses an `ethernet` interface carrying
  neither `match.mac` nor `match.node` rather than guessing, and
  `plans/NETWORK.md` §6.1 binds an alias to hardware "by stable identity,
  never discovery order". A board-neutral default is therefore impossible; the
  default is a property of the *image*, which is why the composed document is
  a `build_rpi_image` argument rather than a hard-coded empty one. The
  low-level `tools/mkimage` CLI, which plants no NIC driver, still ships the
  empty document.
- **It is planted on the read-only `/System` volume**, at the volume-relative
  path `SystemConfigFile::volume_path` names — the one location `devmgr`'s
  pre-unlock store read resolves. Planting it on the writable root through
  the `/System/Settings` view path instead is the defect that kept DHCP from
  ever starting on a flashed board: at runtime that path is the writable
  sub-mount backed by the encrypted root, which no bootstrap client can
  reach, so the shipped default was read by nothing and the machine ran with
  no managed interface. Reader and writer now derive the path from the one
  ABI definition, and `tools/mkimage` proves the plant lands there.
- **It is validated at build time through the one engine that reads it**:
  `mkimage` parses the document with `tairix_netconfig` and re-renders it, so
  an image can never ship an addressing default its own stack would reject
  (`MkimageError::NetworkConfig`); `tools/xtask` additionally asserts the
  parsed result is exactly one DHCPv4+SLAAC interface bound to the GENET
  aperture with no static address.
- **Acceptance is on metal** (`plans/PI.md`): a flashed Pi 4B must log
  `devmgr` `NETSTACK_BOUND`, `netstack` `DRIVER_BOUND` and
  `DHCP_LEASE_ACQUIRED`, and answer a ping at its leased address. QEMU models
  no GENET, so there is no emulated form of this vertical; the lease machinery
  itself stays covered by D3's three virtio verticals.

## 4. Tests, docs, and gate (binding)

Every increment lands its unit/fuzz/QEMU tests, updates its rustdoc +
`docs/src/lib/net.md` + this plan's status marks in the same change (§13,
status only — no landing narrative), and ends with the full §2.15 gate
(`cargo fmt --all`, `cargo xtask ci` once, `cargo xtask fuzz --secs 5`,
`tools/ci/soak.sh both --secs 20`), quoted in the completion report.
