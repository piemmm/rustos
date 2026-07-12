# NETWORK.md — Full IPv4 + IPv6 networking: the user-space network stack

This is the staged build plan for RustOS's complete dual-stack network
implementation: IPv4 and IPv6 as equals, TCP, UDP, ICMP/ICMPv6, IGMP/MLD,
Neighbour Discovery, multicast, a versioned socket ABI, and negotiated
hardware offloads — all built as a microkernel-style user-space stack above
the existing link-layer driver seam. It is **binding under `AGENTS.md`** —
read `AGENTS.md` (especially §2, §4, §5, §17, §19, §20, §24, §26.4),
`PLAN.md`, `plans/fixdrivers.md` (driver layering), `plans/DEVICES.md`
(hotplug), and `plans/SYSLOG.md` (audit events) first; every rule in all of
them applies here without exception. `abi-v1` is **not frozen** (PLAN.md
Stage 1): the ABI types this plan adds and the in-place evolutions it makes
are ordinary pre-release changes (§2.13).

## 0. Scope and decisions (binding for this plan)

- **The stack is a user-space service; drivers stay dumb pipes.** The
  protocol stack lives in `userland/net/netstack` — its own process, its own
  address space, never kernel code (§4 microkernel-leaning). A network
  driver (`drivers/network/*`) implements only the link-layer `Net` contract
  plus the offload/queue extensions this plan adds: it moves frames and
  reports device facts; it never parses an IP header, never owns an address,
  never routes. The stack ↔ driver boundary is a versioned `lib/abi` IPC
  transport (the URB-transport precedent, `plans/USB.md`), so any NIC driver
  serves any stack build and neither links the other (§17.4 — drivers
  consume `lib/abi` only).
- **One protocol engine, pure and host-testable: `lib/net`.** All parsing,
  state machines, and policy (Ethernet/ARP/IPv4/IPv6/ICMP/ICMPv6/ND/
  IGMP/MLD/UDP/TCP, routing, fragment reassembly, PMTUD, congestion
  control) live in a `no_std`, `#![forbid(unsafe_code)]`, allocation-bounded
  `lib/net` crate driven entirely through injected seams (time, RNG, frame
  I/O). The `netstack` process is thin glue: it owns the descriptors, the
  IPC endpoints, and the wait/park loop, and calls the engine. This is the
  §2.2 one-definition rule: the QEMU vertical, the fuzz harnesses, and the
  property tests all exercise the *same* engine the live service runs.
  The existing `userland/net/icmp` responder is subsumed and **deleted** in
  the increment that replaces its QEMU coverage (§2.13, §2.14) — its
  bounded ARP/IPv4/ICMP parsers and `internet_checksum` seed `lib/net`,
  evolved in place, not duplicated.
- **IPv6 is a peer of IPv4 from the first increment, not a port.** Every
  layer is written dual-stack: one address vocabulary (`IpAddr` carrying
  v4/v6), one neighbour abstraction (ARP and ND are two providers of one
  neighbour-cache contract), one checksum definition (RFC 1071 fold with
  the v6 pseudo-header variant), one socket ABI. A v4-only code path that
  IPv6 would have to shadow is the §2.2 duplication this plan forbids.
- **Sockets are capabilities, not ambient authority.** A socket is a
  kernel-brokered IPC channel to `netstack`, obtained through the versioned
  socket ABI (`lib/abi/src/net.rs`) and gated per operation: unprivileged
  processes get outbound TCP/UDP flows under `CAP_NET` (new, coarse:
  "originate transport-layer traffic"); binding a listening port below the
  privileged-port bound requires `CAP_NET_BIND_PRIVILEGED` (new); raw
  packet access stays behind the existing `CAP_NET_RAW`. Every new
  capability lands **with** its enforcement point (§5.2) in the
  increment that serves it. Every refusal is a typed error and an
  audited event (§5.4, §19.4).
- **Hostile network, hostile neighbours, hostile local callers.** All
  frame input is attacker-controlled (§26.4): every decoder in `lib/net`
  is total, bounded (§24.4 validation bounds), fuzzed (§19.6), and fails
  closed. The `netstack` process *is* the §19.5 minimum-capability
  sandbox for network parsing — it holds only its NIC-channel and
  socket-endpoint capabilities, no filesystem, no spawn. DoS resistance
  is designed in, not patched on: SYN cookies, bounded per-peer and
  global state, RFC 5961 challenge ACKs, randomised ports/ISNs/IDs from
  `lib/rng`'s CSPRNG, and strict reassembly budgets (§1 of the design,
  below).
- **Hardware offload is negotiated, never assumed, never trusted.** The
  driver advertises what the device *verified* it can do (checksum
  offload, TSO/GSO-equivalent segmentation, receive checksum validation,
  multiqueue/RSS); the stack opts in per capability and always retains
  the software path as the one canonical implementation. A device claim
  is trust in the *device*, not the peer: a frame the device marks
  "checksum valid" skips the software fold, but every semantic
  validation (lengths, addresses, state) still runs. No offload is ever
  load-bearing for security.
- **Event-driven throughout (§2.23).** The stack parks on its IPC/IRQ
  wait sources and is woken by frame arrival, socket calls, and one-shot
  timers (retransmit, delayed ACK, reassembly expiry, neighbour-cache
  aging — a timer wheel, §27.2). There is no polling loop anywhere in
  the stack or its drivers.
- **Not in this plan:** DNS resolution, DHCP, address-management
  *policy* services beyond the stack's own RFC 4862 SLAAC and static
  configuration, the HTTP client library (§16.4 networking class — a
  later consumer of the socket ABI), firewalling/NAT policy engines, and
  Wi-Fi/802.11 drivers. Each is a future plan that consumes the seams
  this plan fixes; none is speculated here (§2.3/§2.4).

## 1. The gap this plan closes (and the bar it is held to)

Today RustOS has a link-layer `Net` trait, one virtio-net driver with no
offloads, and a stateless ARP/IPv4/ICMP-echo responder. There is no IP
forwarding table, no transport layer, no IPv6, no socket ABI, and no way
for a program to open a connection. This plan delivers the whole stack —
and is explicitly held to the standard of surviving review by a senior OS
architect:

- **Correctness over shortcuts.** TCP is implemented to the modern RFC
  line (RFC 9293 core; RFC 6298 RTO; RFC 5681/6582 congestion control
  with CUBIC as the default policy behind a pluggable trait; RFC 7323
  window scaling + timestamps; RFC 2018 SACK; RFC 6864 IPv4 ID rules;
  RFC 8200/4443/4861/4862 for IPv6/ICMPv6/ND/SLAAC; RFC 3810/2236 for
  MLDv2/IGMPv2 group membership). Where an RFC's MUST conflicts with a
  charter rule, the charter wins and the divergence is documented (§16.7
  precedent).
- **Security posture, concretely:**
  - SYN floods: listener SYN queues are bounded and overflow moves to
    stateless SYN cookies (RFC 4987) — never unbounded allocation (§26.4).
  - RST/data injection: RFC 5961 in-window checks + challenge ACKs.
  - ISN, ephemeral-port, and IPv4-ID randomness: drawn from the §22
    kernel CSPRNG via `lib/rng`; no predictable sequence anywhere.
  - Reassembly (v4 fragments, v6 fragment header): per-source and global
    byte/entry budgets, oldest-first eviction, RFC 8900-informed refusal
    of pathological overlaps — overlap ≠ merge, overlap = drop whole
    datagram (fail closed).
  - ND/ARP cache poisoning: bounded caches, no unsolicited-entry
    creation, RFC 4861 state machine only; hop-limit-255 enforcement on
    ND; audited anomalies (§19.4).
  - Amplification: rate-limited ICMP/ICMPv6 error generation (token
    bucket per RFC 4443 §2.4(f)), no error-about-error, no
    multicast-triggered unicast errors beyond RFC allowances.
  - Resource exhaustion: every table (sockets, TCB, neighbour, routes,
    reassembly, timers) is a §24.1 discovered/growable capacity with a
    §24.3-enforceable per-principal bound, failing closed with typed
    errors at genuine exhaustion; per-socket buffers are accounted
    against the owning `(uid, …)` (§26.2).
- **Performance posture (§2.16):** zero-copy where the seams allow it
  (shared-memory frame rings between driver and stack, the D7c
  `shm_grant` pattern), no per-packet allocation on the hot path,
  amortised timer wheel, checksum folded once, offloads used when
  negotiated, multiqueue-ready receive path. Budgets are stated per
  increment and measured, not guessed.

## 2. Target architecture (binding)

Three independent layers, one-way edges only (§17.4):

```
apps / services (userland/*)          — socket ABI clients (lib/abi::net)
        │  versioned socket IPC (kernel-brokered endpoints, per-op caps)
        ▼
userland/net/netstack                 — the stack process: owns sockets,
        │                                addresses, routes; drives lib/net
        │  frame-ring IPC + offload negotiation (the lib/abi NIC seam)
        ▼
drivers/network/<nic>                 — link-layer only: frames in/out,
                                         device facts, offload execution
```

### 2.1 `lib/net` — the protocol engine (pure, `no_std`)

- One crate, module-per-protocol, no I/O: `eth`, `arp`, `ipv4`, `ipv6`,
  `icmp` (v4+v6 in one module family over shared machinery), `nd`,
  `igmp`, `mld`, `udp`, `tcp`, `route`, `frag`, `neigh`, `checksum`.
- Everything is driven through injected traits: `Clock` (monotonic
  `Duration64` time), `Entropy` (CSPRNG draws), and the caller-owned
  frame buffers. The engine never names a syscall, an endpoint, or a
  device — that is `netstack`'s glue.
- Deterministic and replayable: given the same inputs, time steps, and
  entropy, the engine's outputs are byte-identical. This is what makes
  the TCP state machine property-testable and the fuzz corpus
  meaningful.
- All §24.4 validation bounds (max options length, max SACK blocks, max
  extension-header chain length and count, max fragments per datagram,
  max ND options) are fixed security bounds defined once in the engine.

### 2.2 `userland/net/netstack` — the stack service

- Owns: interface table (one entry per bound NIC channel), address
  configuration (static + SLAAC), neighbour caches, routing table
  (longest-prefix match over one v4/v6-generic trie), socket table,
  timer wheel, and the per-principal accounting the §24.3 limits
  enforce.
- Serves: the socket ABI on a reserved, kernel-brokered endpoint (the
  `DISPLAY_ENDPOINT` precedent); every request carries the
  kernel-attested caller identity, is capability-checked before state
  is touched, validated whole, and refused typed (§5.4).
- Parks on: a wait set of {socket endpoint, per-NIC frame ring, timer}.
  Wake sources are the existing kernel wait primitives; no polling.
- Crash containment: `netstack` dying resets network state but never
  the system; sockets surface typed disconnection to holders; `init`
  restarts the service; the restart is audited (§19.4). No kernel state
  is left dangling because the kernel holds only endpoint plumbing,
  never protocol state.

### 2.3 The driver seam — `lib/abi` netlink transport + offloads

- The `Net` trait is evolved **in place** (§2.13) into the channel form
  the stack consumes: shared-memory RX/TX frame rings (zero-copy,
  grant-bounded, the D7c pattern), doorbell/wake integration with
  `irq_wait`, and a typed `DeviceFacts` report — MAC, MTU, link state,
  and the offload capability set.
- Offload vocabulary (closed, versioned): `TX_CSUM_IPV4`, `TX_CSUM_TCP`,
  `TX_CSUM_UDP` (v4+v6), `RX_CSUM_VALIDATED`, `TX_SEGMENT_TCP`
  (TSO-equivalent: stack hands one over-size TCP payload + template
  header, device segments), `RX_MULTIQUEUE(n)`. A driver advertises only
  what it implements and tests; the stack enables per-flag; the software
  path remains canonical and is the conformance oracle for the offload
  path (same-bytes tests).
- `drivers/network/virtio_net` is the first server: it negotiates the
  matching `VIRTIO_NET_F_*` features and maps them onto the closed
  vocabulary; a feature the device offers but the vocabulary does not
  name is left un-negotiated (no speculative surface, §2.4).

### 2.4 The socket ABI — `lib/abi/src/net.rs`

- Typed, versioned, fuzzed request/reply + event frames: `socket`
  (domain v4/v6 × type stream/dgram/raw), `bind`, `listen`, `accept`,
  `connect`, `send`/`recv` (shm-backed for bulk, inline for small),
  `shutdown`, `close`, `getsockopt`/`setsockopt` over a **closed**
  option registry, multicast join/leave, and non-blocking readiness
  integration with the existing wait-set ABI (a `WaitSourceKind::Socket`
  member, added in place like `Stream` was).
- Every byte a client sends is untrusted: lengths, addresses, and
  options are validated in the stack before any state change; a
  malformed request poisons nothing and returns one typed error.
- `lib/rt` grows the thin safe wrappers first-party programs link; the
  §16.4 networking shared-library class (sockets/DNS/HTTP) later fronts
  this same ABI — it is a consumer, not a second path.

## 3. Capabilities (§5.2 discipline)

| Capability | Guards | Introduced with |
|---|---|---|
| `CAP_NET` (new) | originating transport flows + high-port binds — the whole class of ordinary network use | N4 (socket ABI), enforcement in `netstack` dispatch |
| `CAP_NET_BIND_PRIVILEGED` (new) | binding listeners below port 1024, v4 and v6 alike | N6 (TCP listen), enforcement at `bind`/`listen` |
| `CAP_NET_RAW` (exists) | raw frame/packet sockets and the NIC frame rings | already live; `netstack` is its principal holder |
| `CAP_NET_ADMIN` (new) | interface/address/route mutation, offload toggling, stack-wide counters reset | N3 (interface bring-up), enforcement in the admin surface |

Four entries total; each guards a class, has a live holder and
enforcement point in its introducing increment, and no existing
capability expresses it (the §5.2 three-part test). Per-instance
questions (which port, which interface) are parameters checked inside
the gated operation, never new capabilities.

## 4. Staged increments

Each increment is one deliverable change: code + tests + docs + the §7
gate, and each leaves the tree fully working. Status marks: `[ ]`
planned, `[~]` in progress, `[x]` done.

### N1 — `lib/net` foundation: addresses, checksum, Ethernet, ARP/ND-ready neighbour contract `[ ]`
- Crate skeleton (`rustos-net`, `no_std`, `#![forbid(unsafe_code)]`,
  §6 README with stability tier), the dual-stack address vocabulary
  (`IpAddr`/`Ipv4Addr`/`Ipv6Addr`, scope/zone handling for v6
  link-local), the one checksum definition (RFC 1071 + v6
  pseudo-header), Ethernet framing, and the neighbour-cache contract
  (`NeighborTable`: bounded, state-machine per RFC 4861 §7.3.2 shape,
  provider-agnostic so ARP and ND both drive it).
- The existing `userland/net/icmp` parsers migrate in (evolved, not
  copied); `userland/net/icmp` keeps compiling against `lib/net`
  re-exports until N3 deletes it.
- Tests: exhaustive parse/emit round-trips, truncation/mutation
  matrices, checksum vectors (RFC examples + property tests), fuzz
  harnesses `fuzz_net_eth`/`fuzz_net_addr` registered in `cargo xtask
  fuzz`.
- Docs: `docs/src/lib/net.md` (architecture + seam contract).

### N2 — IPv4 + IPv6 network layer: parse/emit, ICMP+ICMPv6, ND, reassembly, routing `[ ]`
- IPv4 (options-tolerant parse, strict emit), IPv6 (extension-header
  chain walk with §24.4 count/length bounds; unrecognised
  headers/options handled per RFC 8200 dispositions), ICMP + ICMPv6
  (echo, errors, rate-limited generation), ND (RS/RA/NS/NA/redirect
  parse + the RFC 4861 state machine over the N1 neighbour contract,
  hop-limit-255 enforced), ARP as the v4 neighbour provider.
- Fragment reassembly (v4 + v6) with per-source and global budgets,
  oldest-first eviction, overlap ⇒ whole-datagram drop; fragmentation
  on emit (v4) and PMTUD plumbing (v6 never fragments in flight).
- Routing: one generic longest-prefix-match table (v4/v6 instantiations
  of one trie), on-link determination, default routers from RA, source
  address selection (RFC 6724).
- Tests: RFC vectors, adversarial fragment/extension-header corpora,
  property tests (reassembly never exceeds budget; LPM matches a naive
  oracle), fuzz `fuzz_net_ipv4`/`fuzz_net_ipv6`/`fuzz_net_icmp`/
  `fuzz_net_nd`.

### N3 — the `netstack` service + evolved driver seam: frames flow end to end `[ ]`
- `userland/net/netstack` process: interface/address/route state, the
  event loop (wait set over NIC rings + timer), SLAAC + static v4/v6
  address configuration, the timer wheel, `CAP_NET_ADMIN` admin surface
  (typed IPC: interface list/addr add/route add/counters).
- The driver seam evolves in place: `Net` becomes the ring-transport +
  `DeviceFacts` contract; `virtio_net` serves it (still no offloads);
  the kernel provides only endpoint plumbing + `irq_wait` wake.
- `userland/net/icmp` is **deleted**; its QEMU ARP/ping coverage is
  re-landed as the `netstack` vertical (ping in/out over v4 *and* v6:
  answers echo, resolves neighbours both ways).
- Tests: engine-level end-to-end over a loopback fake, QEMU vertical
  (`tests/integration/netstack_*`), audited-refusal tests for the admin
  surface.
- Docs: `docs/src/userland/netstack.md`; driver README updates.

### N4 — UDP + the socket ABI + multicast membership `[ ]`
- UDP over both families; the socket ABI (`lib/abi/src/net.rs`) with
  `socket`/`bind`/`connect`/`send`/`recv`/`close`, `CAP_NET`
  introduced + enforced, ephemeral ports CSPRNG-randomised, per-socket
  and per-principal buffer accounting (§24.3), `WaitSourceKind::Socket`
  readiness.
- Multicast: IGMPv2 + MLDv2 host-side membership, socket join/leave,
  reception filtering; solicited-node multicast for ND (already needed
  by N2) formalised here.
- Tests: two-process QEMU vertical (UDP echo v4+v6, multicast join +
  receive), fuzz `fuzz_net_udp`/`fuzz_net_sockabi`, limit-exhaustion
  tests failing closed.
- Docs: `docs/src/abi/net-sockets.md`.

### N5 — TCP core: the RFC 9293 state machine, retransmission, flow control `[ ]`
- Connection establishment/teardown (full state machine, simultaneous
  open/close), sequence-space arithmetic as a checked type, send/recv
  windows, RFC 6298 RTO with Karn's algorithm, fast retransmit,
  zero-window probing, RFC 7323 window scaling + timestamps (PAWS),
  CSPRNG ISNs, RFC 5961 challenge ACKs, user timeout.
- Deterministic-engine property tests (the state machine never
  regresses sequence invariants; every segment corpus replayable),
  fuzz `fuzz_net_tcp` (segment decoder + state-machine driver), QEMU
  vertical: client connect to a host peer, bulk transfer both
  directions with loss injection.

### N6 — TCP listeners, SYN-flood defence, congestion control, SACK `[ ]`
- `listen`/`accept` with bounded accept + SYN queues; overflow ⇒
  stateless SYN cookies (RFC 4987) with the documented option-loss
  trade-off; `CAP_NET_BIND_PRIVILEGED` introduced + enforced.
- Congestion control behind a pluggable `CongestionControl` trait
  (§17.1 scheduler-policy precedent): CUBIC (RFC 9438) default,
  NewReno (RFC 6582) sibling — conformance suite both must pass; RFC
  2018 SACK + RFC 6675 loss recovery.
- Adversarial tests: SYN flood soak (bounded memory asserted), RST/data
  injection corpus (RFC 5961 behaviour), connection-exhaustion
  fail-closed, cookie round-trip property tests.

### N7 — hardware offloads + performance hardening `[ ]`
- The offload vocabulary negotiated end to end: `virtio_net` maps
  `VIRTIO_NET_F_CSUM`/`GUEST_CSUM`/`HOST_TSO*`/`MRG_RXBUF`; stack uses
  TX checksum offload, RX checksum-validated skip, and TCP segmentation
  offload; multiqueue receive plumbed where the device offers it.
- Same-bytes conformance: every offloaded path is asserted equal to the
  software path (the oracle); offload never bypasses semantic
  validation.
- Measured budgets recorded in the docs (loopback + QEMU virtio
  throughput/latency, allocation counts on the hot path = 0);
  regressions are §2.16 defects.
- `README.md` support matrix rows updated (per-arch offload state).

### N8 — `ping`/`ss`-class command apps + observability `[ ]`
- System command apps (`ping` — v4+v6, coreutils/iputils-familiar
  surface per §16.7; a socket/interface inspection tool following `ss`
  conventions) as `.app` bundles with Help/ trees; stack counters and
  socket tables exposed through the System Information API (§16.6)
  behind `CAP_SYSINFO_GLOBAL` — never a `/proc` shape.
- Docs: user-facing `docs/src/userland/networking.md`; the security
  posture page `docs/src/security/network.md` (threat model ↔ defence
  table, the §19.4 event-id registry for network events).

## 5. Why this survives senior review

- **One engine, replayable and fuzzed**, exercised identically by unit
  tests, property tests, fuzzers, and the live service — no "tested
  code" vs "shipped code" divergence.
- **Every DoS class named above has a designed, tested defence** with a
  budget, an eviction policy, and a fail-closed exhaustion path — not a
  TODO.
- **Strict layering**: drivers never see protocols, the kernel never
  sees protocols, apps never see frames; each seam is a versioned,
  fuzzed `lib/abi` contract, so replacing a NIC driver, the congestion
  policy, or the whole stack build touches exactly one layer.
- **Dual-stack by construction**: IPv6 is not a second implementation
  to drift; it is the same tables, sockets, and machinery under one
  address vocabulary.
- **No shortcuts inherited**: the interim icmp responder is deleted,
  not wrapped; the `Net` trait is evolved in place, not shimmed
  (§2.13, §2.14).

## 6. Tests, docs, and gate (binding)

- Every increment lands its unit/property/fuzz/QEMU tests in the same
  change (§7); every fuzz harness registers with `cargo xtask fuzz`;
  adversarial corpora enter the regression corpus (§19.6).
- Every increment updates its rustdoc + `docs/src/` pages and this
  plan's status marks in the same change (§13) — status only, no
  landing narrative.
- Every increment ends with the full §2.15 gate: `cargo fmt --all`,
  `cargo xtask ci` (once), `cargo xtask fuzz --secs 5`, and
  `tools/ci/soak.sh both --secs 20`, quoted in the completion report.

## 7. What this plan explicitly does *not* do

- No DNS resolver, DHCP client/server, NTP, or HTTP library — future
  consumers of the socket ABI, each its own plan.
- No firewall/NAT/forwarding policy engine (the routing table forwards
  nothing between interfaces in this plan; RustOS is a host, not a
  router, until a dedicated plan says otherwise).
- No TLS (already curated under `lib/crypto`/§16.4; it fronts sockets,
  it is not part of the stack).
- No Wi-Fi/802.11, no non-Ethernet link layers — new drivers serve the
  same seam later.
- No kernel-resident fast path: if profiling ever motivates one, that
  is a design conflict to raise (§15.7), not a quiet migration.
