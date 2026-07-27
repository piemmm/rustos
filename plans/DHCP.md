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

### D3 — the live two-process QEMU vertical `[x]`
`tests/integration/netstack_dhcp_qemu_aarch64` boots the production aarch64
pipeline against the `dhcp-net-root` disk (the signed virtio-net driver
bundle plus a planted `network.conf` binding the NIC by `match.node`,
selecting `ipv4.method dhcp`, and disabling IPv6) with the harness-side
DHCP-server peer (`NetPeerMode::V4DhcpEcho`) on the QEMU `dgram` netdev.
`devmgr` autoloads the driver and delivers the config; `netstack` drives its
DHCP client, which DISCOVERs, accepts the peer's OFFER of
`wire::DHCP_LEASED_V4`, REQUESTs it, and applies the ACK — the interface's
only address (it forms none itself). The peer then pings the guest at that
leased address and the guest answers. PASS keys on three log witnesses
(`devmgr` `NETSTACK_BOUND`, `netstack` `DHCP_LEASE_ACQUIRED`, `netstack`
`INBOUND_ECHO_SERVED`) plus the peer's own verdict (it offered, acked, and
got the echo reply at the leased address), so a broken lease cannot pass on
an address the guest formed itself.

Key facts for the next worker:
- The host DHCP **server** is test-only (the plan ships no server), so it
  lives beside its one consumer in `tools/xtask`'s `netpeer::dhcp_server`,
  encoding/decoding the *same* wire layout the client codec now exposes
  publicly (`tairix_net::dhcp`'s `opt`, header offsets, `MAGIC_COOKIE`); a
  round-trip unit test parses every reply it builds back through the real
  `DhcpReply::parse`, so the two cannot drift.
- The shared wire constants (`DHCP_SERVER_V4`, `DHCP_LEASED_V4`,
  `DHCP_SUBNET_MASK`, `DHCP_LEASE_SECS`, `DHCP_NETWORK_CONF_AARCH64`) live in
  `tests/integration/netstack_wire`, cross-checked against the real
  `lib/netconfig` parser.
- Only the aarch64 vertical is enrolled; the x86_64 (virtio-PCI) and riscv64
  (virtio-MMIO) siblings are the natural next increment, mirroring how the
  static vertical staged its arches.

### D4 — DHCPv6 (RFC 8415) `[ ]` (future, own sub-plan when scheduled)
Stateful DHCPv6 as an IPv6 peer of D1–D3, reusing the interface-engine
integration shape. Not started; not speculated here.

## 4. Tests, docs, and gate (binding)

Every increment lands its unit/fuzz/QEMU tests, updates its rustdoc +
`docs/src/lib/net.md` + this plan's status marks in the same change (§13,
status only — no landing narrative), and ends with the full §2.15 gate
(`cargo fmt --all`, `cargo xtask ci` once, `cargo xtask fuzz --secs 5`,
`tools/ci/soak.sh both --secs 20`), quoted in the completion report.
