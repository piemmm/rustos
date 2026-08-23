# NETWORK.md — Full IPv4 + IPv6 networking: the user-space network stack

This is the staged build plan for TAIRiX's complete dual-stack network
implementation: IPv4 and IPv6 as equals, TCP, UDP, ICMP/ICMPv6, IGMP/MLD,
Neighbour Discovery, multicast, a versioned socket ABI, and negotiated
hardware offloads — all built as a microkernel-style user-space stack above
the existing link-layer driver seam. It is **binding under `AGENTS.md`** —
read `AGENTS.md` (especially §2, §4, §5, §17, §19, §20, §24, §26.4),
`PLAN.md`, `plans/fixdrivers.md` (driver layering), `plans/DEVICES.md`
(hotplug), `plans/ALIAS.md` (the resource-reference and `info:`/`state:`/
`stats:` vocabulary §5 builds on), and `plans/SYSLOG.md` (audit events)
first; every rule in all of them applies here without exception. `abi-v1`
is **not frozen** (PLAN.md Stage 1): the ABI types this plan adds and the
in-place evolutions it makes are ordinary pre-release changes (§2.13).

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
- **Configuration is declarative, observability is typed.** Every managed
  interface — identity binding, addressing, MTU, bonding/failover — is
  described by one fail-closed configuration store
  (`/System/Settings/Network/network.conf`, §6); stack-wide knobs are
  `configure net.*` keys in the `lib/sysconfig` registry (§6.2); and
  every interface's facts, live state, and counters are served through
  the System Information API under the `info:net`/`state:net`/
  `stats:net` resource references (§5). No pseudo-files, no imperative
  boot scripts, no second config parser.
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

Today TAIRiX has a link-layer `Net` trait, one virtio-net driver with no
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

- Typed, versioned, fuzzed request/reply + delivery frames. The datagram
  surface is landed (`SocketRequest`: `socket`/`bind`/`connect`/`send`/
  `close` + multicast `join`/`leave` over v4/v6; `SocketType::Datagram`,
  with stream/raw reserved and fail-closed until their increments);
  `listen`/`accept`/`shutdown`/`getsockopt`/`setsockopt` and shm bulk
  transfer arrive with the TCP and shared-memory increments (no
  speculative surface today).
- **Readiness rides `WaitSourceKind::Port`, not a new kernel kind
  (design decision, evolved in place).** The kernel owns no socket object
  (§2.2), so inbound datagrams are delivered by the stack `ipc_send`ing a
  framed `SocketDatagram` to a per-socket async **port** the client bound
  and named in `socket()`; the client parks on the existing
  `WaitSourceKind::Port` and drains with `ipc_recv`, authenticating the
  stack's attested sender origin (the `plans/APPWIN.md` AW3 window-event
  precedent). A kernel `WaitSourceKind::Socket` is deliberately **not**
  added — teaching ring 0 about a stack-owned object would break the
  microkernel boundary. `recv` is therefore a client-side port drain, not
  a stack round-trip (more efficient, and "inline for small" today; shm
  bulk later).
- Every byte a client sends is untrusted: lengths, addresses, and
  options are validated in the stack before any state change; a
  malformed request poisons nothing and returns one typed error. Each
  operation's unused header fields must be zero, so no request smuggles
  authority through another's fields.
- `lib/rt` grows the thin safe wrappers first-party programs link; the
  §16.4 networking shared-library class (sockets/DNS/HTTP) later fronts
  this same ABI — it is a consumer, not a second path.

## 3. Capabilities (§5.2 discipline)

| Capability | Guards | Introduced with |
|---|---|---|
| `CAP_NET` (live) | originating transport flows + high-port binds — the whole class of ordinary network use | N4b (socket service): `CapabilityId::NET` (36), in `SESSION_BASELINE`, enforced in the `netstack` socket dispatcher |
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

### N1 — `lib/net` foundation: addresses, checksum, Ethernet, ARP/ND-ready neighbour contract `[x]`
- `tairix-net` exists (`no_std`, `#![forbid(unsafe_code)]`, §6 README,
  tier experimental): the dual-stack address vocabulary re-exports the
  `core::net` types and adds RFC 4007 `Ipv6Scope` (fail-closed on
  reserved multicast scopes) + `ScopedIpv6Addr` (zone required exactly
  when scope is non-global); `checksum` is the one RFC 1071 definition
  (one-shot fold + byte-stream incremental `Checksum` accumulator with
  `ipv4_pseudo`/`ipv6_pseudo` seeds); `eth` carries Ethernet II framing
  (`ETHERTYPE_ARP/IPV4/IPV6`); `neigh::NeighborTable` is the bounded,
  provider-agnostic RFC 4861 §7.3.2 state machine (pure `now`-driven
  methods, side effects as returned actions from `advance`, one-shot
  timer via `next_deadline`, LRU-of-resolved eviction that fails closed
  when all entries are mid-resolution, unsolicited confirmations never
  create entries).
- The former `userland/net/icmp` parsers were folded into `lib/net`
  (evolved: `Ipv4Addr` vocabulary; IPv4 parse verifies the header
  checksum per RFC 1122 §3.2.1.2). The `netstack` service subsumed the
  responder and the standalone crate was deleted at N3c.
- Tests landed: parse/emit round-trips and rejection matrices, checksum
  vectors + independent-oracle/split properties, the 14-test neighbour
  state-machine suite, and the `fuzz_net_eth`/`fuzz_net_addr` harnesses
  registered in `cargo xtask fuzz`.
- Docs: `docs/src/lib/net.md`.

### N2 — IPv4 + IPv6 network layer: parse/emit, ICMP+ICMPv6, ND, reassembly, routing `[x]`
- `ipv4` evolved in place: options-tolerant parse (checksum-verified,
  options surfaced opaquely, fragment fields typed with the offset in
  bytes), strict option-free emit, and emit-side fragmentation
  (`fragment`: DF honoured, 68-byte MTU floor, 8-byte-aligned parts).
  `ipv6` carries the fixed-header codec and the bounded
  extension-header `walk` (`MAX_EXT_HEADERS` = 8, a fixed validation
  bound): HBH first-position-only, routing with segments-left ≠ 0
  refused (host, not router), RFC 8200 §4.2 option dispositions as
  typed `WalkRejection` values, and a fragment header ending the walk
  for reassembly-then-rewalk.
- `icmp` is one machinery for both families (`IcmpContext` holds the
  type numbering + pseudo-header difference): `IcmpMessage`,
  `IcmpEcho` (typed `EchoKind`), `IcmpError` (incl. the RFC 1191 v4
  packet-too-big wire form), the RFC 4443 §2.4(e) `error_allowed`
  gate, and the §2.4(f) token-bucket `ErrorRateLimiter`.
- `nd` parses RS/RA/NS/NA/redirect with the RFC 4861 validations
  (hop-limit 255, code 0, non-multicast targets, solicited-NA-to-
  multicast refused, `MAX_ND_OPTIONS` = 16); emits host messages only
  (RS/NS/NA); `apply_*` helpers drive the one N1 `NeighborTable`
  (ARP remains the v4 provider). RA facts are typed data for
  `route::DefaultRouterList` / address configuration.
- `frag::Reassembler` is dual-stack: overlap (incl. exact duplicate) ⇒
  whole-datagram drop (RFC 8900), per-source + global byte budgets
  with oldest-first eviction (offending source first), datagram and
  fragment-count caps, non-final-multiple-of-8 and final-length-
  consistency shape rules, expiry reporting the first-fragment fact
  for the RFC 4443 §3.2 Time Exceeded decision.
- `route`: one generic LPM binary trie (`RoutingTable<A, M>` over the
  `RouteAddr` bit view, O(bits) lookup, pruning + arena reuse under
  churn), on-link = next-hop-free match, the bounded
  `DefaultRouterList` (RFC 4861 §6.3.6 reachable-first + round-robin),
  RFC 6724 `select_source` (rules 1/2/3/6/8; rule 5 is the caller's
  interface pre-filter), and the RFC 8201 `PathMtuCache`
  (reduction-only, 1280 floor, aging, LRU-bounded).
- Tests landed: per-module suites (round-trips, rejection matrices,
  dispositions, budgets, eviction order), property tests (reassembly
  budgets hold after every push; random splits reassemble exactly; LPM
  matches a naive oracle under insert/remove churn), and the
  `fuzz_net_ipv4`/`fuzz_net_ipv6`/`fuzz_net_icmp`/`fuzz_net_nd`
  harnesses registered in `cargo xtask fuzz`. Docs:
  `docs/src/lib/net.md` + `lib/net/README.md` refreshed.

### N3 — the `netstack` service + evolved driver seam: frames flow end to end `[x]`

N3 landed as three tree-green sub-increments (each complete with tests,
docs, and the full gate) because the whole was too large for one change:

#### N3a — interface/address engine + host engine in `lib/net` `[x]`
- Landed: `iface` — the per-interface RFC 4862 address engine (static
  v4/v6 assignment; SLAAC with DAD, RS scheduling per RFC 4861 §6.3.7,
  preferred/valid lifetimes with the §5.5.3(e) two-hour rule; addresses
  formed from an injected 64-bit interface identifier — RFC 7217
  stable-privacy derivation is the service layer's job; capacity-bounded,
  fail-closed) and `stack` — the dual-stack host engine composing eth/
  arp/nd/ipv4/ipv6/icmp/frag/route/neigh/iface into one per-interface
  `Stack`: `receive_frame(now)` → bounded output frames + typed
  `StackEvent`s, `advance`/`next_deadline` event-driven (one-shot timer,
  never polled), ARP/NS answered for owned addresses only, next-hop
  resolution with a bounded pending queue, budgeted reassembly,
  `error_allowed` + rate-limited ICMP errors, bounded RA application
  (SLAAC, default routers, on-link routes, MTU within link floor/
  ceiling, timing adoption), redirect accepted only from the
  destination's current first hop, echo in/out for diagnostics.
- Landed: the driver seam's *facts* half — `Net::mac_address` evolved
  in place into `Net::device_facts` returning a typed, fail-closed-
  validated `DeviceFacts` (MAC, link MTU within the 68..=65535 bounds,
  `LinkState`, the closed `NetOffloads` flag set rejecting reserved
  bits, `rx_queues >= 1`); `virtio_net` serves it (no offloads
  advertised, none negotiated) and every consumer updated in the same
  change. The ring-transport half of the seam is N3b.
- Tests landed: `iface`/`stack` unit + end-to-end suites (two `Stack`s
  wired back-to-back resolve neighbours and ping each other over v4 and
  v6), and the `fuzz_net_stack` harness (random frames into a live
  `Stack`, never panics, outputs bounded) registered in
  `cargo xtask fuzz`. Docs: `docs/src/lib/net.md` + `lib/net/README.md`
  refreshed.

#### N3b — the `netstack` service process `[x]`
- Landed: `userland/net/netstack` (engine library + freestanding `Run`
  binary, the `netstack.app` service bundle, service account uid 14
  with the pinned `NETSTACK_CEILING`): the alias-named interface table
  (one `lib/net` `Stack` per interface), the ring pump
  (`service_interface`: engine output → TX ring → `Net::service` →
  RX frames back through the engine), `next_deadline` arming the wait
  set's one-shot timeout, and the capability-checked, audited
  dispatcher on the reserved `NETSTACK_ENDPOINT` — `CAP_NET_ADMIN`
  (new, id 35, in the administrator ceiling) gates interface
  list/addr add/route add/counters; the whole-state facts/state pages
  are the broker surface gated on `CAP_SYSINFO_INTROSPECT`, narrowed
  per client by `sysinfod` (`CAP_SYSINFO_HW` facts / MAC,
  `CAP_SYSINFO_GLOBAL` state) and resolved by `lib/procinfo` as
  `info:net/<iface>/{mac,mtu,kind}` and
  `state:net/<iface>/{link,address}` (the resolver's first `state:`
  namespace; RFC 5952 v6 rendering).
- Landed: the driver seam's transport half — `Net` frame I/O evolved
  in place into the shared-memory frame-ring transport
  (`tairix_abi::driver::net_ring`: validated `RingGeometry`,
  fail-closed `FrameRing` push/pop/skip, `FrameRings` + `ServiceReport`;
  rings mutated only inside the blocking `service` call, so the call
  boundary is the synchronisation — safe Rust, no shared-memory
  atomics); `virtio_net` serves it (park-once on the host's device
  waiter when nothing moved, lossless staged-RX back-pressure, still
  no offloads); the QEMU netstack verticals drive the same rings.
- Tests landed: netstack loopback end-to-end (a peer-`Stack` fake:
  v4 ARP+echo and v6 DAD+ND+echo through the real pump), the
  audited-refusal dispatch matrix, ring/codec fail-closed suites in
  `lib/abi`, and the gated/audited sysinfod + procinfo query tests.
  Docs: `docs/src/userland/netstack.md`, `docs/src/drivers/network.md`,
  and the driver-trait page refreshed.

#### N3c — QEMU verticals + `userland/net/icmp` deletion `[x]`
- Landed: the `netstack` engine drives a live virtio-net device end to
  end through the ring pump in the QEMU verticals
  `tests/integration/netstack_{pci_x86_64,mmio_riscv64,mmio_aarch64}`.
  A host-side peer (`cargo xtask` `netpeer`, the same `lib/net` `Stack`
  over the QEMU dgram netdev) and the guest resolve each other and ping
  in *and* out over v4 *and* v6 (ARP + Neighbour Solicitation both ways,
  echoes answered both ways); the shared wire topology lives once in
  `tests/integration/netstack_wire` so guest and peer cannot drift.
- Landed: `userland/net/icmp` is **deleted** (crate, workspace member,
  its `fuzz_parse` harness registration, `docs/src/userland/net_icmp.md`,
  SUMMARY entry); the `lib/net` `fuzz_net_*` harnesses carry its parser
  coverage. Nothing links it.

### N4 — UDP + the socket ABI + multicast membership `[x]`
- **Engine UDP layer landed:** `lib/net::udp` is the dual-stack UDP
  codec (RFC 768) — one `write`/`UdpDatagram::parse` core folding the
  family-appropriate pseudo-header checksum, IPv4-optional /
  IPv6-mandatory checksum discipline, total/bounded/fail-closed, with
  the `fuzz_net_udp` harness. `Stack` demuxes received UDP in both
  families to a verbatim `StackEvent::UdpDatagram` (the service decides
  socket delivery / port-unreachable, not the engine) and originates it
  with `Stack::send_datagram` (unicast this increment; the ICMP send
  helpers were generalised to carry the protocol, no duplication).
- **Multicast membership engine + host membership landed:**
  - `lib/net::igmp` (IGMPv2, RFC 2236) and `lib/net::mld` (MLDv2,
    RFC 3810) are the wire codecs — total/bounded/fail-closed, fuzzed
    (`fuzz_net_igmp`, `fuzz_net_mld`); `mld` decodes queries and encodes
    v2 reports only (no report decoder — MLDv2 has no suppression).
  - `lib/net::mcast` is the family-generic host state machine
    `Membership<P>` over the `Igmp`/`Mld` providers (the `neigh`
    one-core/two-providers shape): reference-counted join/leave,
    robustness retransmission, query responses jittered from a
    MAC-seeded non-crypto generator, IGMP-only report suppression,
    all-hosts control groups never reported, bounded + fail-closed at
    capacity.
  - Router Alert on emit: `Ipv4Header::write_with_router_alert`
    (RFC 2113 option, IHL 6) and `ipv6::hop_by_hop_router_alert`
    (RFC 2711 Hop-by-Hop), TTL/hop-limit 1, defined once.
  - `Stack` wiring: `join_multicast`/`leave_multicast`, receive-path
    filtering by membership (v4 + v6), IGMP (proto 2) and MLD
    (`ICMPv6` 130/143) query dispatch, report emission to the group /
    `224.0.0.2` / `ff02::16`, folded `next_deadline`; solicited-node
    multicast for ND is formalised here (auto-joined on
    `AddressPreferred`, left on invalidation) and the all-systems group
    is auto-joined on v4 configuration.
- **N4a — socket ABI wire contract + network errnos `[x]` (landed).**
  `lib/abi/src/net.rs` (`tairix_abi::net`) is the pure, versioned,
  fail-closed wire contract: `SocketRequest`
  (`socket`/`bind`/`connect`/`send`/`close` + multicast `join`/`leave`,
  dgram v4/v6, `SocketType::Datagram` with stream/raw reserved), the
  socket-open/bind reply codecs, and the `SocketDatagram` delivery frame
  the stack sends to a client's async port. `recv` is realised as a
  client-side `WaitSourceKind::Port` drain of that frame, not a stack
  round-trip, and there is **no** kernel `WaitSourceKind::Socket` (see
  §2.4). Reserved socket endpoint `NETSTACK_SOCKET_ENDPOINT`; five
  network `Errno`s added (`AddressInUse`/`AddressUnavailable`/
  `NetworkUnreachable`/`NotConnected`/`LimitExceeded`). Host-tested and
  exercised by the `lib/abi` never-panic/round-trip fuzz harness.
  `CAP_NET` is **not** introduced here — a capability lands only with a
  live enforcement point and holder (§5.2), which is N4b. Docs:
  `docs/src/abi/net-sockets.md`.
- **N4b — the socket service + client + multicast transmit `[x]`.**
  - `CAP_NET` (`CapabilityId::NET` = 36) in `SESSION_BASELINE`, enforced
    in the `netstack` socket dispatcher before any state is touched;
  - `tairix_netstack::SocketService`: the origin (`ProcId`)-keyed socket
    table, CSPRNG-drawn ephemeral ports (kernel `random_get`, injected as
    an entropy closure so the engine stays pure), globally-unique port
    binding, per-principal + global bounded accounting failing closed with
    `LimitExceeded`, and inbound demux from `StackEvent::UdpDatagram` to
    the owning socket's delivery port (peer-filtered, membership-gated) as
    an encoded `SocketDatagram`;
  - UDP unicast **and** multicast datagram transmit —
    `Stack::send_datagram` gained the multicast path (group MAC, link-local
    scope, no route/membership needed), and `Netstack::originate` selects
    egress per link;
  - the `Run` binary binds the second endpoint (`NETSTACK_SOCKET_ENDPOINT`)
    and serves it in the same event-driven wait-set loop;
  - `tairix_rt::net` client wrappers (`socket`/`bind`/`connect`/`send`/
    `recv`/`close`/`join`/`leave`), `recv` returning the sender `Origin`
    for fail-closed authentication; and the `random_get` rt wrapper.
  - Tests: the `SocketService` host suite (cap gate, origin scoping,
    ephemeral/explicit bind + port reuse, quota exhaustion, unicast send,
    peer-filtered + multicast-gated delivery), `lib/net` multicast-transmit
    round-trips (v4+v6), `fuzz_net_sockabi` (the serve path), and the
    `random_get` marshal tests. Docs: `docs/src/lib/net.md` and
    `docs/src/abi/net-sockets.md`.
  - The socket control plane is fully served; a live `send` fails closed
    (`NetworkUnreachable`, empty interface table) until a NIC is bound
    into the running process by N4d. The datagram data path is proven by
    the engine tests over the same `SocketService` + `Stack` the live
    service runs.

### N4c — the cross-process NIC device-channel handoff contract `[x]`

The stack and a NIC driver run as **separate processes** (the true
microkernel shape, §2/§4). `lib/abi::driver::net_channel`
(`tairix_abi::driver::net_channel`, `netchan-v1`) is the versioned,
pure, fail-closed IPC control-plane contract that establishes and drives
the [`net_ring`] frame region across that boundary:

- The **driver** owns the device (MMIO/DMA/IRQ) and serves a call
  endpoint; the **stack** is the client that owns the frame-ring region.
  This is the display D7a `shm_grant` pattern with the roles inverted and
  the data flowing both ways: the stack sizes a `RingGeometry` from the
  device MTU, `shm_create`s the region, `shm_grant`s it to the driver's
  endpoint (recipient resolved kernel-side from the endpoint — never a
  recyclable PID), and forwards the unforgeable handle in `Attach`; the
  driver `shm_map`s exactly that region (owner-checked — no ambient
  authority).
- Operations (`NetChannelRequest`): `Facts` (get `DeviceFacts` to size
  geometry), `Attach { geometry, region_grant, class, notify_port }`,
  `Service` (the doorbell — the driver services the mapped rings once and
  replies a `ServiceReport`), `Detach`. Between doorbells the driver
  parks on its device IRQ and wakes the stack with a `NetChannelNotify`
  `ipc_send` to `notify_port` when receive frames arrive; the stack,
  parked on that port in its wait set, issues the next `Service`. Neither
  side ever busy-polls (§2.23).
- Wire codecs for `DeviceFacts` (the `Facts` reply) and `ServiceReport`
  (the `Service` reply) were added beside their types (evolved in place,
  §2.13). Every decode is total, validates whole (magic, version,
  reserved-must-be-zero, geometry/class bounds, `DeviceFacts::validate`),
  and fails closed with one typed `Errno`.
- Tests: host round-trip + fail-closed suites for every request, the
  notify frame, and both reply frames; the shared `lib/abi` `fuzz_decode`
  never-panic harness gained a `net_channel` arm. No new capability and
  no new syscall — it is built entirely on the existing `shm_create`/
  `shm_grant`/`shm_map`, endpoint, and `irq_wait` primitives. Docs:
  `docs/src/drivers/network.md`.

### N4d — live driver-process wiring `[x]`

The N4c device-channel contract is wired end to end so a real NIC's frames
reach the running `netstack` process. Done:

- **The driver process** `drivers/network/virtio_net_driver` (freestanding
  `Run` binary + inert host stub): `RtDriverHost::from_grants_query` +
  `sole_register_window` + `MmioTransport::new` + `VirtioNet::open` bring-up,
  claims the first free reserved device-channel endpoint bound
  restricted-sender `{CAP_NET_RAW}`, emits the `netchan` hardware-tree node
  carrying that endpoint, and runs a wait-set loop over `{call endpoint,
  device IRQ}` driving the pure `NetChannelServer`. On an RX IRQ it
  `ack_interrupt`s and `ipc_send`s one `NetChannelNotify` to the stack's
  notify port; never busy-polls. Depends only on `lib/*` (the `virtio_kbd` ↔
  `lib/virtio_input` layering precedent — the `Net` device logic lives in
  `lib/virtio_net`, not `drivers/*`).
- **`VirtioNet::service` is a non-blocking doorbell**: it drains what is ready
  and returns (no internal RX park), so a cross-process `Service` never blocks
  the reply. Parking belongs to each process's own wait loop — the driver
  parks on the device IRQ; the stack parks on the notify port.
- **netstack `run.rs` live glue**: `RtNetChannelTransport` (an `ipc_call`
  `NetChannelTransport`), a bounded channel-client table, and the
  `BindDriver` admin op — capability-checked (`CAP_NET_ADMIN`), audited,
  fail-closed — provisioning a channel (query facts → size ring geometry from
  `DeviceFacts::mtu` → `shm_create` + `shm_grant` → `port_bind` a
  non-reserved per-`(pid, slot)` notify port `net_channel::notify_endpoint_for`
  → `NetChannelClient::attach` → derive the IPv6 iid `eui64_interface_id` +
  a CSPRNG IPv4 ident seed → `add_interface`). The wait loop pumps a channel
  on its notify wake, all channels on the engine deadline lapse, and the
  socket-TX batch after a `send`/`join`/`leave`/`close` (`queue_tx` then the
  one `service_interface` pump), delivering received datagrams to their
  sockets' async ports. The engine pump is the single generic
  `Netstack::service_interface<F: FrameService>` — one definition for both
  the in-process `LocalFrameService` and the remote `NetChannelClient`.
- **`devmgr` autobind** (`netbind` module, host-tested): recognises the
  emitted `netchan` node (`compatible = "tairix,netchan"`), reads its endpoint
  resource, and hands it to `netstack` `BindDriver` under a derived `netN`
  alias — each endpoint bound exactly once across generation bumps, fail-soft
  retry if the stack is not yet up.
- **The driver's §18.3 bind table** `tairix_drv_network_virtio_net::BIND_KEYS`
  (`HwMatchKey::virtio(1)`, exact-match `BIND_PRIORITY`): the discovery
  identity `devmgr`/the signed-manifest bind table is authored from, so a
  discovered virtio-net node resolves to this driver. Without it the driver
  process was undiscoverable; it lives in the `drivers/network/virtio_net`
  `lib` (the `virtio_input`/`virtio_blk` `BIND_KEYS` precedent) and outlived
  the §18.5 scaffold removal, in which the single-process in-kernel `register`
  shell was deleted (this crate is now a pure bind-table + engine re-export).
- **Capabilities**: `netstack` += `CAP_SHM` (owns/grants the frame region),
  `devmgr` += `CAP_NET_ADMIN` (calls `BindDriver`) — in both the account
  ceilings and the manifest requests (effective = ceiling ∩ manifest); the
  stack still never holds `CAP_NET_ADMIN`, it *enforces* it. No new syscall.

### N4e-α — netstack is a launched core boot service `[x]`

The prerequisite the two-process vertical builds on: `netstack` is now a
real, launched core service on **every** Tier-1 arch, coming up owning an
empty interface table and ready to be handed NIC device channels.

- `init`'s `DEFAULT_CONFIG` gains `service /System/Services/netstack.app/Run
  netstack`, launched after `sysinfod` and before `devmgr` (so it is ready
  when `devmgr` binds discovered NIC channels to it); `MAX_SERVICES` (4)
  fits exactly. Global to every production-boot image.
- netstack is spawnable on every arch: on aarch64 it is spawned from its
  verified on-disk `netstack.app` bundle (auto-discovered and planted by
  `image_apps`); on x86_64/riscv64 it is a compiled-in boot-floor row
  (`spawn_paths::NETSTACK_PATH`, `program_manifests::NETSTACK_MANIFEST` =
  {`CAP_NET_RAW`, `CAP_SHM`, `CAP_IPC_BIND_PRIVILEGED`, `CAP_LOG_EMIT`},
  `spawn_layout::SPAWN_PROGRAMS`, built by the kernel `build.rs`) until
  those targets' on-disk stores land. The effective set is
  `NETSTACK_MANIFEST ∩ NETSTACK_CEILING` (the same four); the stack still
  never holds `CAP_NET_ADMIN` — it *enforces* it.
- Until a NIC is bound the interface table is empty, the deadline is
  unarmed, and the loop parks solely on its endpoints — no work, no CPU.

### N4e-β — two-process QEMU vertical (aarch64 first) `[x]`

The aarch64 two-process production-boot vertical
(`tests/integration/netstack_autoload_qemu_aarch64`) proves the whole
network stack ↔ driver path across **two user processes** on a live guest.
It boots the production `boot_aarch64::boot` pipeline against the shared
`AutoloadRootDisk` fixture — now planting the signed `virtio_net_driver`
bundle at `Drivers/network/virtio_net/Run` beside the input + display
bundles — as a `ramfb` display world with a virtio-net-device-mmio attached
and the `netpeer` host link-peer (v6-link-local-only campaign) on the QEMU
dgram netdev. `devmgr` autoloads the driver into its own process, the driver
publishes its `netchan` node, `devmgr` calls `netstack` `BindDriver`,
netstack provisions the channel and auto-configures the interface's EUI-64
IPv6 link-local (no IPv4), and answers the peer's link-local echo. Guest
PASS keys on three witnesses observed on the kernel **log** sink (all
userland `log_emit` records): `devmgr` `NETSTACK_BOUND`, `netstack`
`DRIVER_BOUND`, and `netstack` `INBOUND_ECHO_SERVED` (the last gating exit so
a frame has provably crossed the boundary and been answered); the harness
also requires the peer's own v6 echo verdict, so neither side passes alone.

Load-bearing facts this milestone established (each a general production
improvement, not test scaffolding):
- **aarch64 bootstrap-floor discovery now enumerates virtio-net nodes.**
  `root_storage::observe_virtio_mmio_input_devices` and the new
  `observe_virtio_mmio_network_devices` share one
  `observe_virtio_mmio_interrupt_devices(device_id, class, node_base_id)`
  core (§2.2); `boot_aarch64` calls the network probe beside the input one.
  `VIRTIO_NET_DEVICE_ID` moved to `lib/virtio_net` (re-exported by the driver
  crate) so the kernel names it without a kernel→driver dependency (§17.4).
- **An endpoint's owner may advertise the endpoint it created.**
  `call_create` mints the per-endpoint owner grant for *any* restricted-sender
  endpoint (binding one already requires `CAP_IPC_BIND_PRIVILEGED`), not only
  `CAP_IPC_ENDPOINT`-restricted ones — so the `netchan`-owning driver's
  `hw_emit_node` coverage check passes for the `CAP_NET_RAW`-restricted
  endpoint. `ipc_call` still couples the grant to the *call* only for
  `CAP_IPC_ENDPOINT` endpoints, so no sender-authorisation changed.
- **The netchan shm region tolerates page-rounding.** The kernel maps whole
  pages, so the driver's `attach` accepts `mapped_len >= geometry.region_len()`
  (binding the ring view over the first `region_len` bytes, unmapping the full
  `mapped_len`) instead of demanding exact equality.
- netstack's `DRIVER_BIND_FAILED` audit now carries the failing step's errno
  (fail-loud); the "inbound echo served" audit is `INBOUND_ECHO_SERVED`
  (`EventId(16_012)`), emitted from `run.rs::deliver` on the engine's
  `EchoRequestServed` event. The host `netpeer` targets the guest's EUI-64
  link-local from the pinned `tairix_test_netstack_wire::GUEST_MAC`; the QEMU
  `NetDevice` gained a `mac` field threaded through the shared
  `net_device_arg` on all three arches.

**Follow-up increments.**
- **N4e-x86_64 `[x]` DONE** — x86_64 is now fully two-process over
  virtio-**PCI**, so all three Tier-1 targets match. Both live verticals pass a
  real guest boot: `autoload_input_qemu_x86_64` (autoloaded user-space
  `virtio_kbd` delivering an injected keypress, PASS on
  `AuditEvent::InputDelivered kind=key`) and `netstack_autoload_qemu_x86_64`
  (the two-process netstack path, PASS on the three log witnesses + the peer
  echo verdict). The live exercise surfaced + fixed the one gap: **the x86_64
  enumerator now routes each interrupt-driven virtio-PCI function's MSI-X**
  (`boot_x86_64::probe_virtio_pci` MSI-allocates a kernel vector, programs the
  function's MSI-X table entry 0 via `MsixBus::route_msix` through a
  `CAP_MMIO_MAP` `KernelMmioMapper`, and grants the driver the routed MSI line
  instead of the legacy INTx GSI — the user-space driver only `enable_msix(0)`
  + `irq_bind`s the line, never touching PCI config or the MSI-X BAR, so the
  kernel owns interrupt routing like Linux). To make `msi::allocate` available
  at probe time, `seed_hardware_tree` was reordered to run after
  `discover_and_program_io_apics` (`install_msi_lines`);
  `root_unlock_admission_qemu_x86_64` re-confirmed no regression. The QEMU
  harness (`tools/qemu/src/x86_64.rs`) gained `virtio-keyboard-pci` /
  `virtio-mouse-pci` attachment (the PCI form of the aarch64
  `virtio-keyboard-device`). Foundations that were in place: the synthetic node-id bases the probes emit
  from now live in one shared, disjoint-by-construction, compile-time-guarded
  map (`kernel/tairix-kernel/src/hwtree_node_ids.rs`) — a new arch's NIC-probe
  region is claimed as the next index there, never a fresh literal, so the
  base-collision class that bit N4e-β cannot recur. Second foundation in
  place: the pure virtio-MMIO discovery observers
  (`observe_virtio_mmio_{block,input,network}_devices` + the shared
  interrupt-class core) now live in the arch-neutral
  `kernel/tairix-kernel/src/hwdiscovery` module, split out of `root_storage`
  (which retains only the drvhost-linked root-block *catalogue resolution*).
  Because `hwdiscovery` injects the enumerated bus through the frozen
  `lib/abi` seams and links no `driver_catalog` / `drvhost`, a riscv64 /
  x86_64 boot path reuses the *same* observers without pulling the
  driver-signing trust anchor onto those arches — so the arch discovery
  wiring is a thin caller (an injected `FdtDiscovery` + a per-slot arch IRQ
  resolver), not a copy of the walk (§2.2 / §2.21).
  Third foundation in place: the riscv64 boot path now **seeds the hardware
  tree**. `boot_riscv64::try_boot` runs the port's `FdtDiscovery` into the
  shared `kernel/tairix-kernel/src/boot_hwtree.rs` `CollectingHwNodeSink`
  (the one growable boot-tree sink, extracted from the aarch64 boot path so
  neither copies it, §2.2) and publishes it to `HW_TREE`, so the
  `hw_tree_read` / `hw_tree_wait` syscalls expose the riscv64 platform
  (root/memory/timer) inventory to user space. This is pure device-tree
  normalisation — no MMIO — so it is safe before the bootstrap-floor bus
  bring-up. Fourth foundation in place: **the riscv64 bootstrap-floor
  virtio-MMIO `DeviceID` probe is now wired.** `boot_riscv64::seed_hardware_tree`
  builds the MMIO bus from the discovered device tree
  (`tairix_drv_bus_mmio::virtio_mmio_bus_from_dtb`, mapped by the Sv39 identity
  window `boot` enables) and calls the *same* arch-neutral
  `hwdiscovery::observe_virtio_mmio_{block,input,network}_devices` observers
  aarch64 uses (§2.2 / §2.21), so the served tree now carries the probed,
  autoloadable Block/Input/Network nodes. The interrupt-driven input/network
  nodes carry their discovered PLIC line, resolved by the arch port's pure
  `tairix_arch_riscv64::fdt::plic_device_source` (reads a `virtio,mmio` node's
  single `interrupts` cell — the QEMU `virt` PLIC is `#interrupt-cells = <1>`
  — and bounds it against the discovered `riscv,ndev`; a discovered value,
  never a board constant, host-tested). Fifth foundation in place: **the
  in-kernel bootstrap-floor driver catalogue is now per-architecture.**
  `driver_catalog::IN_KERNEL_DRIVERS`/`IN_KERNEL_DRIVER_COUNT`/`EMMC2_PATH`
  and `build.rs`'s signed-manifest set are gated on `kernel_isa`: the floor is
  virtio-blk on every target, and the BCM2711 EMMC2 SD-host driver is floor
  **only on aarch64** (`tairix-drv-storage-emmc2` is now an aarch64-only
  runtime dependency). This fixed a live §2.20 defect — the Pi-only EMMC2
  driver was compiled into the x86_64 image (and would have been in riscv64's)
  — and lets the riscv64 autoload tranche join the drvhost-gated catalogue
  with a virtio-blk-only floor, never dragging a foreign-silicon driver into
  its image (host tests + all-three-arch builds green). Sixth foundation in
  place: the arch-neutral autoload/unlock orchestration
  (`unlock_orchestrate::finish_unlock`) is extracted and both arches call it
  over injected console/spawn seams. **The riscv64 root-mount + unlock-kthread
  + `devmgr` + `driver_spawn_loader` parity port is now landed and
  host-gate-green**: the
  drvhost/devmgr runtime deps + the 11 module gates, the PLIC external-IRQ
  dispatch (the `PlicIrqController` bridge promoted to the production crate
  root `riscv64_plic_irq.rs` with a `rearm`→`unmask` override + host tests, and
  `riscv64/irq.rs` doing `record_plic`/`install_dispatch`/claim→fire→complete
  over `fdt::plic_base`+`plic_ndev`), `riscv64/root_unlock.rs` (virtio-blk
  bring-up over the PLIC path → `finish_unlock`, SBI console fail-closed on
  input), and the `try_boot` BootInfo hooks + init-spawn call. Seventh
  foundation in place: the image pipeline is now **per-arch**
  (`tairix_itest_harness::pie::PieArch` threaded through `image_apps` +
  `image_drivers`, per-arch memoised; `qemu_tests::stores_for` composes only
  the `/System` stores an enrolment plants, for its own target arch), so a
  non-aarch64 autoload vertical plants that arch's rxe. **The first riscv64
  boot vertical, `autoload_input_qemu_riscv64`, is authored + enrolled** (the
  `virt`-board virtio-mmio input-autoload analogue of the aarch64 vertical,
  reduced to the input path: no display world, and the encrypted-root unlock
  fails closed by design because the SBI console has no interactive input this
  slice). Authoring it surfaced and fixed a **pre-existing riscv64 defect**:
  `RiscvBinArch` never overrode `KernelArch::irq_routing`, so the kernel-core
  `IrqTable` was sized `max_line = 0` and *every* device-source `bind` failed
  closed — root-unlock, root-mount, and all interrupt-driven bring-up were
  broken on a real boot. The fix (`riscv64/irq.rs::plic_routing` +
  `ensure_controller`, the one place the PLIC controller is built; wired into a
  `RiscvBinArch::irq_routing` that returns `max_line = riscv,ndev` + the shared
  controller) is confirmed on a real guest boot. Bringing the injected
  key-delivery PASS home surfaced and fixed a **second** riscv64 defect: the
  PLIC `IrqController` bridge's `rearm` forwarded to `unmask` (priority only),
  never setting the source's per-context *enable* bit. In-kernel paths call
  `arm` (which enables) explicitly, but the user-space driver path only ever
  re-arms through the bridge — so an autoloaded driver's line was enabled-never
  and its interrupt never reached S-mode. `PlicIrqController::rearm` now calls
  the idempotent `arm` (enable + threshold + priority), matching the aarch64
  GIC bridge's "route + enable on the driver's behalf" `rearm` contract.
  `autoload_input_qemu_riscv64` now **passes end to end** (production boot →
  `/System` mount → autoload → user-space driver bring-up → injected key →
  `InputDelivered kind=key`) in a few seconds on TCG, budgeted at 60 s like the
  other boot-then-fixed-work verticals. **The riscv64 two-process netstack
  vertical (`tests/integration/netstack_autoload_qemu_riscv64`) is now landed
  and passes a real guest boot**: the `virt`-board virtio-mmio / PLIC analogue
  of the aarch64 N4e-β vertical, reduced to the headless boot world (no display;
  the SBI/NULL-console encrypted-root unlock fails closed by design, but the
  `/System` store binds independently of the passphrase and the virtio-net
  driver still autoloads). It boots the production `boot_riscv64::boot`
  pipeline against the shared per-arch `AutoloadRootDisk` (carrying the
  riscv64-cross-compiled signed virtio-net bundle) with a virtio-net-device-mmio
  attached (MAC pinned to `tairix_test_netstack_wire::GUEST_MAC`) and the
  `netpeer` host peer in its v6-link-local-only campaign; `devmgr` autoloads the
  driver into its own process, netstack binds the `netchan` channel and
  auto-configures the EUI-64 link-local, and answers the peer's echo. PASS keys
  on the same three log witnesses (`NETSTACK_BOUND`, `DRIVER_BOUND`,
  `INBOUND_ECHO_SERVED`) plus the peer's own v6 echo verdict, at the 240 s
  budget its aarch64 sibling uses. x86_64 then repeated the whole sequence over
  its virtio-**PCI** bus (its `irq_routing` / IO-APIC path already existed); the
  x86_64 foundations that led there were: `boot_x86_64::bring_up_bsp`
  **seeds the hardware tree**. It runs the already-built-but-unconsumed port
  discovery seam `tairix_arch_x86_64::platform::AcpiDiscovery` (root, every
  enabled Local APIC as a `Cpu` node, and each I/O APIC as an
  `InterruptController` node with its MMIO window) over the already-located,
  validated MADT bytes into the shared `boot_hwtree::CollectingHwNodeSink`
  (§2.2 — no per-arch collect copy) and publishes it to the authoritative
  `HW_TREE`, so `hw_tree_read` / `hw_tree_wait` expose the real x86_64 platform
  inventory to user space instead of an empty tree — the exact sibling of the
  riscv64/aarch64 pure device-tree seed. It is pure ACPI byte-slice
  normalisation (no MMIO register access), so it is safe before any
  bootstrap-floor bus bring-up, and fails closed (a malformed table seeds
  whatever was collected rather than failing the boot). **Second x86_64
  foundation landed: the two-process virtio-PCI driver *contract*** (the
  `abi-v1`-unfrozen multi-window grant work), host-gate-green. The kernel PCI
  probe resolves a virtio function's four config windows to CPU-physical
  `(base,len)` **without mapping** (`VirtioPciBus::virtio_window_region`, the
  resolve-only sibling of `map_virtio_window`) and emits them as four
  role-tagged MMIO grants (`lib/abi` `virtio_pci_window_resource` /
  `HwResource::mmio_tagged`, role in the tag + `notify_off_multiplier` on the
  notify window's aux) + `dma(0,0)` + the discovered IRQ line, on a node keyed
  by the *shared* `HwMatchKey::virtio(type)` (PCI device id `0x1040+type`
  translated back to the virtio type, so **one** signed bundle binds on both
  buses). `hwdiscovery::observe_virtio_pci_network_devices` is the ECAM
  analogue of the MMIO probe (node-id `region(4)`); `boot_x86_64::seed_hardware_tree`
  builds the `tairix_pci::mechanism_ecam` bus over the identity-mapped ECAM
  window (`acpi::locate_mcfg`+`mcfg_first_ecam`, over one signature-parameterised
  (X|R)SDT walk shared with `locate_madt`) and runs it, resolving each
  function's IRQ from its PCI Interrupt-Line register. The `virtio_net_driver`
  process is grant-shape-keyed (four role-tagged windows ⇒ `PciTransport` +
  `enable_msix`, one window ⇒ `MmioTransport`) over one shared generic serve
  path. **Third x86_64 foundation landed: the virtio-input-PCI probe + the
  `virtio_kbd` driver's PCI path** (the input sibling of the net foundation,
  host-gate-green): `hwdiscovery::observe_virtio_pci_input_devices` (over the
  same shared `observe_virtio_pci_devices` core, node-id `region(6)`) emits
  each modern virtio-input PCI function as an `Input` node keyed by the shared
  `HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID)` carrying the four role-tagged
  windows + `dma(0,0)` + the discovered PCI Interrupt-Line GSI;
  `boot_x86_64::probe_virtio_pci` runs it beside the net + block probes over
  either config-access mechanism; and `drivers/input/virtio_kbd` gained the
  same grant-shape-keyed transport (`virtio_pci_windows` ⇒ `PciTransport` +
  `enable_msix`, else `sole_register_window` ⇒ `MmioTransport`) over one
  generic `run<T: Transport>` bring-up/pump, so one signed input bundle binds
  on both buses. Both verticals (`autoload_input_qemu_x86_64` and the
  two-process `netstack_autoload_qemu_x86_64`) now live-exercise these
  contracts and pass a real guest boot (see the DONE summary above), which is
  where the kernel-routed MSI-X fix landed.
- **§18.5 scaffold removal `[x]` DONE** — with all three arches two-process,
  the single-process in-kernel netstack path is gone: the `register` shell in
  `drivers/network/virtio_net` was deleted (the crate is now a pure
  `BIND_KEYS` + `VirtioNet`/`VIRTIO_NET_DEVICE_ID` re-export), the
  `netstack_ping` tail and its net-only helpers were removed from the shared
  `virtio_qemu_support` crate (its four net deps too), and
  `VirtioNet::wait_for_device_event` (that tail's only consumer) was deleted.
  The three single-process verticals (`netstack_mmio_aarch64`,
  `netstack_mmio_riscv64`, `netstack_pci_x86_64`) were **removed**, not kept
  beside the two-process `netstack_autoload_qemu_*` replacements (§2.14). The
  now-unused v4/guest-side wire topology and the host `netpeer`'s dual-stack
  `PeerCampaign` were removed with them; `netpeer` and `tairix_test_netstack_wire`
  are v6-link-local-only, matching the surviving verticals. (`FixedSpawner`
  stays: it still spawns the in-kernel-floor block driver and the input driver
  for their live single-process verticals — it was never net-only.)

### N5 — TCP core: the RFC 9293 state machine, retransmission, flow control `[x]`

Split into three tree-green sub-increments (the N3 precedent), because
the whole is too large for one change and each leaves the tree working.

#### N5a — the TCP segment codec + sequence-space arithmetic `[x]`
- `lib/net::tcp` is the pure, dual-stack RFC 9293 wire layer: the fixed
  20-byte header, the eight control flags (`TcpFlags` — named `SYN`/`ACK`/…
  bits, `contains`, `|`), and the recognised options — MSS, window scale
  (RFC 7323, raw value surfaced; the §2.3 clamp is state-machine policy),
  timestamps, SACK-permitted, and up to `MAX_SACK_BLOCKS` (= 4, a fixed
  bound) SACK blocks (RFC 2018). `TcpSegment::parse` verifies the
  mandatory pseudo-header checksum in both families (no zero-checksum
  form, unlike UDP-over-IPv4), rejects a data offset outside `5..=15`
  words / a header longer than the segment / any malformed or overrunning
  option / a too-large SACK count, and is total + bounded + fail-closed.
  `write` serialises a `TcpSegmentMeta` with canonical NOP-aligned option
  ordering, padding to a 32-bit boundary, failing closed on an
  over-40-byte options region.
- `SeqNumber` is the checked modulo-2³² sequence type (RFC 1982 / RFC 9293
  §3.4): wrapping `add`/`sub`, `distance_from`, the windowed
  `lt`/`le`/`gt`/`ge` (computed from the unsigned gap, no signed cast),
  and `in_window`. It has **no** `Ord`/`PartialOrd` so a linear comparison
  on a cyclic value cannot compile — the type the N5b window/ACK
  arithmetic is built on.
- The v4/v6 pseudo-header context was hoisted from `udp` into the shared
  `checksum::Pseudo` (protocol-neutral `seed(protocol, upper_len)`), so
  UDP and TCP fold one definition (§2.2); `udp` re-exports it.
- Tests: the `tcp` unit suite (sequence arithmetic across the wrap,
  option round-trips incl. SACK + timestamps, and the fail-closed matrix
  — truncation, bad data offset, malformed/overrunning options, too-many
  SACK blocks, checksum + wrong-pseudo-header rejection) and the
  `fuzz_net_tcp` harness (parse never-panics + write→parse round-trip),
  registered in `cargo xtask fuzz`. Docs: `docs/src/lib/net.md`,
  `lib/net/README.md`.

#### N5b — the TCP connection state machine `[x]`
- `lib/net::tcp::conn` is the pure, event-driven RFC 9293 transmission
  control block (`Tcb`), built on the N5a segment codec + `SeqNumber`.
  It carries the full state machine (active/passive/simultaneous open,
  the complete teardown lattice through TIME-WAIT), send/receive windows
  over `SeqNumber`, RFC 7323 window scaling + timestamps with PAWS,
  RFC 2018 SACK generation from a bounded out-of-order reassembly set,
  RFC 6298 retransmission (SRTT/RTTVAR/RTO) with Karn's algorithm and
  go-back-N recovery, fast retransmit on three duplicate ACKs,
  zero-window persist probing, RFC 5961 in-window RST/SYN handling with
  rate-limited challenge ACKs, delayed ACKs, and the RFC 9293 user
  timeout. It is deterministic like `neigh`/`mcast`: the ISN is a
  caller-supplied CSPRNG draw, `now` is explicit, output is drained via a
  `poll_transmit(emit)` closure, and one timer re-arms from
  `next_deadline`. Addresses never enter the TCB (the caller folds the
  pseudo-header checksum through `tcp::write`), so it is family-agnostic;
  every buffer + the reassembly set are capacity-bounded and fail closed.
- Design decisions carried forward: congestion control is deliberately
  **not** here — the send path is flow-control-bounded only, and a
  pluggable `CongestionControl` policy plus listeners/accept-queue and
  SYN cookies are N6. Retransmission recovery is go-back-N (reset
  `snd_nxt` to `snd_una`); fast retransmit re-sends the oldest segment
  without a cwnd change (N6 adds RFC 6675 loss recovery).
- Tests (`tcp::conn::tests`): handshake, bidirectional data, orderly and
  simultaneous close, out-of-order reassembly with SACK advertisement,
  RTO retransmit, peer RST, sequence-space wrap at 2³²−1, connect
  timeout, and a 16-seed byte-exact bulk-transfer-under-loss/reorder
  property test. `fuzz_net_tcp` gained the `state_machine_driver` arm
  (two live TCBs + a hostile parseable-segment injector; asserts no panic
  and that every emitted segment re-parses).

#### N5c — the stream socket surface + QEMU vertical `[x]`
- **The software path is landed and host-gate-green.** `SocketType::Stream`
  is wired through the whole stack:
  - `lib/net::stack`: the engine gained a `StackEvent::TcpSegment`
    receive-demux (checksum-verified, v4 + v6) and a `Stack::send_tcp`
    origination path (source selection + pseudo-header checksum via
    `tcp::write` + IP-wrap; unicast-only, fail-closed) — the engine stays
    stateless for TCP, mirroring UDP. Covered by two back-to-back-`Stack`
    engine tests (handshake + bidirectional data, and an 8 KB bulk transfer).
  - `lib/abi::net`: `SocketType::Stream`, the accepted-byte-count `Send`
    reply (`encode_send_reply`), and the `SocketStreamEvent` delivery frame
    (`Connected`/`Data`/`Closed` + `StreamCloseReason`) — the
    connection-oriented analogue of `SocketDatagram`. Host-tested and fuzzed
    (`fuzz_decode` gained the stream-event round-trip arm).
  - `netstack`: `SocketService` is now one origin-keyed table over datagram
    **and** stream sockets (`Proto::{Datagram,Stream}`), owning one `Tcb` per
    connection. `Connect` actively opens (CSPRNG ISN, egress interface chosen
    once); inbound `StackEvent::TcpSegment`s drive the `Tcb`; the service
    turns the results into segment egress and client `SocketStreamEvent`s;
    stream timers fold into `stream_next_deadline`/`advance_streams`. `run.rs`
    drives the stream pump in the event loop. A netstack stream connect+echo
    test exercises the real pump against a passive-peer echo server.
  - `lib/rt::net`: `stream_socket`/`stream_send`/`stream_recv` client
    wrappers (`connect`/`close` shared with datagrams).
  `listen`/`accept` remain N6; `shutdown` is N15.
- **The live two-process QEMU vertical passes a real guest boot.**
  `tests/integration/netstack_stream_qemu_aarch64` boots the production
  aarch64 pipeline with the encrypted-root disk carrying the standard store
  bundles **plus** the signed virtio-net driver bundle and the test-only
  `tcpecho` client fixture (`FsDisk::StreamRootDisk` — net driver only, no
  display/input, so login is serial-scripted over the UART console). `devmgr`
  autoloads the NIC driver into its own process and `netstack` binds it; the
  runner unlocks the root, logs in `root`/`root`, and types `tcpecho`. The
  client (`tests/integration/tcpecho_program`) opens a `SocketType::Stream`
  socket, connects to the harness-side passive TCP echo peer
  (`NetPeerMode::V6TcpEcho`), streams a fixed deterministic 32 KiB run, and
  verifies the peer echoes every byte back in order. The peer injects bounded
  frame loss so a pass proves RFC 9293 retransmission carried the stream
  across the two-process boundary. PASS keys on the client's audited `exit`
  (`comm=tcpecho`) then the shell's exit (typed after the `TCPECHO PASS`
  marker), and the harness additionally requires the echo peer to report the
  whole transfer received and echoed — neither side passes alone.
- **The wire reuses the proven IPv6 link-local addressing**, not a new v4
  path: the guest already auto-configures its EUI-64 link-local and the peer
  already reaches it (the N4e ICMP vertical proves both directions), and TCP
  is family-agnostic, so no admin v4-address-assignment machinery was invented
  for the stream test — the deterministic transfer byte generator + port live
  in the shared `tairix-test-netstack-wire` topology so client and echo server
  cannot drift.
- **Send segment sizing is path- and option-aware (RFC 6691).** A connection
  clamps its send segment size to `min(peer advertised MSS, local path MSS)`,
  where the local path MSS is the egress link MTU minus the family's IP header
  and the fixed TCP header — computed by `Stack::tcp_local_mss` and seeded into
  the connection's `TcpConfig.local_mss` by `netstack` at connect
  (`Netstack::egress_mss_for`), so the SYN advertises and the sender segments to
  a size the link can carry. The per-segment payload is further reduced by the
  wire length of the TCP options it carries (`TcpOptions::wire_len`; timestamps
  = 12 B), so header + options + payload never exceeds the MTU. Without this an
  IPv6 connection built full IPv4-MSS (1460 B) segments that overflowed the
  1500 B MTU once the 40 B IPv6 header was added; `send_tcp` refused each as
  `TooLarge` and the sender silently dropped every full-size segment, emitting
  only each burst's short trailing segment — the bulk transfer then stalled to
  the RFC 9293 user timeout. Regression: `stack_tests`
  `tcp_bulk_transfer_over_ipv6_respects_the_link_mtu` drives a multi-segment
  transfer over a real v6 link and asserts every byte arrives in order.

### N6 — TCP listeners, SYN-flood defence, congestion control, SACK `[x]`

#### N6a — pluggable congestion control `[x]`
- `lib/net::tcp::cc` is the pluggable congestion-control policy layer, the
  §17.1 scheduler-policy precedent applied to TCP: a `CongestionControl`
  trait the connection consults for its send window, RFC 9438 **CUBIC**
  (the default) and RFC 6582 **NewReno** siblings, and a shared conformance
  suite both must pass (RFC 6928 initial window, slow-start vs.
  congestion-avoidance growth, multiplicative decrease on loss, one-segment
  collapse on timeout, monotonic growth under a pure ACK stream). Windows
  are byte counts; all arithmetic — including CUBIC's `K` and window target
  over an integer cube root (`icbrt`) — is exact integer fixed-point, so the
  `no_std` crate needs no floating point or libm (§2.12).
- `TcpConfig.congestion` selects the algorithm; `Tcb` stores the boxed
  policy, bounds every send by `min(snd_wnd, cwnd)` in `plan_segment`, and
  feeds it `on_ack`/`on_loss`/`on_rto`. Fast retransmit (three duplicate
  ACKs) drives the decrease **once per loss window** through the RFC 6582
  `recover` high-water mark; an RTO collapses to one segment and restarts
  slow start. Retransmission stays the existing go-back-N.
- Covered by the `tcp::cc` conformance + unit suite (both policies, the
  cube root, CUBIC's Reno-friendliness and convex overshoot) and the
  `tcp::conn` end-to-end tests (initial-window bound, cwnd opening on ACKs,
  a full bulk transfer under each policy); the existing lossy/reordering
  property test now also exercises cwnd + go-back-N together. Docs in
  `lib/net` lib.rs, `lib/net/README.md`, `docs/src/lib/net.md`.

#### N6b-1 — RFC 6675 SACK-based selective loss recovery `[x]`
- `lib/net::tcp::conn` now recovers loss selectively when the peer
  negotiated SACK, replacing go-back-N. A bounded `Scoreboard` records the
  peer's SACKed send ranges — coalesced, capped at `MAX_SACK_RANGES`, and
  clamped to the outstanding window `(snd_una, snd_max]`, so a reordering
  or hostile peer can neither grow the state nor inject ranges outside the
  data actually in flight (fail closed; the board holds only sequence
  extents, never payload).
- From the board the engine computes RFC 6675's three functions:
  `is_lost` (a byte is lost once ≥ `DUP_THRESH` discontiguous SACK ranges,
  or > `(DUP_THRESH−1)·SMSS` SACKed bytes, lie above it — constant across a
  hole because a hole contains no SACK edges), `set_pipe` (the in-flight
  estimate bounding transmission against `cwnd`), and the `NextSeg` walk in
  `plan_sack` (rule 1: the lowest lost hole above `HighRxt`; rule 2: fresh
  data; rule 3: one rescue retransmission per episode). `Plan::Retransmit`
  carries an explicit `seq` and never advances the send frontier.
- Recovery entry is RFC 6675 §5 (`DUP_THRESH` duplicate ACKs **or**
  `is_lost(snd_una)`); the one multiplicative decrease per episode uses the
  RFC 6582 `recover` high-water mark. A retransmission timeout clears the
  board and falls back to go-back-N (§5.1); `HighRxt` initialises to
  `snd_una − 1` so the first hole byte is eligible. SACK stays negotiated
  via the existing SYN option; go-back-N remains the fallback when the peer
  did not permit SACK.
- Covered by `tcp::conn::tests`: scoreboard unit tests (lost-by-count,
  lost-by-volume, coalescing, out-of-window/hostile-block rejection,
  bounded-under-fragmentation), and end-to-end recovery tests
  (selective retransmit of a single lost segment with no RTO and no
  go-back-N amplification; two-hole recovery with no RTO). The
  `fuzz_net_tcp` state-machine driver gained hostile SACK-bearing ACK
  injection at the sender. Docs: `lib/net` lib.rs, `README.md`,
  `docs/src/lib/net.md`.

#### N6b-2 — listeners, SYN-flood defence `[x]`

##### N6b-2-α — the pure `lib/net` listener + SYN-cookie engine `[x]`
- `lib/net::tcp::listen` is the demultiplexing server-side `Listener`
  above `tcp::conn`: it demuxes inbound segments by `Peer`, holds a
  bounded backlog of half-open (SYN-RECEIVED) handshakes with a timeout,
  and moves completed connections onto a bounded accept queue (`accept`).
  Both queues are fixed capacity and fail closed — an accept-queue-full
  handshake is refused with a RST, a stale half-open is expired by
  `advance` (which also retransmits owed SYN-ACKs; `next_deadline` folds
  the one-shot timer).
- Overflow of the half-open backlog ⇒ **stateless RFC 4987 SYN cookies**:
  the server ISN is a keyed MAC over the connection 4-tuple and a rotating
  counter (5-bit tick + 3-bit MSS index + 24-bit MAC), so the handshake is
  reconstructed from the client's returning ACK holding no per-connection
  memory. The documented trade-off is option loss (a cookie carries only
  the MSS; a cookie-accepted connection negotiates no window scale/SACK/
  timestamps). The keyed MAC is an injected `CookieSecret` seam — the
  engine hand-rolls no crypto (§2.12); `netstack` backs it with
  `lib/crypto`. Reconstruction replays the existing state machine (build
  `Tcb::listen` with options disabled, synthesize the SYN, commit the
  SYN-ACK, feed the ACK), so no new `Tcb` surface was added.
- Covered by `tcp::listen::tests` (handshake→accept, SYN-flood→bounded
  cookies, cookie round-trip, tampered + stale cookie → RST, accept-queue
  exhaustion fail-closed, half-open expiry, peer-RST reap, data-only drop,
  IPv6 handshake) plus the `fuzz_net_tcp` listener driver (hostile
  SYN/ACK/RST flood asserting no panic + bounded queues + every emitted
  segment parses). Docs: `lib/net` lib.rs, `README.md`, `docs/src/lib/net.md`.

##### N6b-2-β-1 — the socket surface + capability + cookie secret `[x]`
- `tcp::listen` is exposed through `netstack`: the socket ABI
  (`lib/abi/src/net.rs`) carries `SocketRequest::Listen` (op 8) and
  `SocketRequest::Accept { deliver_port }` (op 9) and the
  `SocketStreamEvent::Accepted` readiness event; `SocketService` holds a
  `Proto::Listen(Box<Listener>)` per listening socket and drives it from
  `on_tcp_segment`/`advance_streams`/`stream_next_deadline`. Each completed
  handshake is drained into a **pending** child stream socket keyed to the
  owner on the listening port (its received bytes buffer in the bounded TCB;
  no client events until claimed); `Accepted` is delivered to the listener's
  port and `Accept` claims the oldest pending child, rebinds its delivery
  port, returns the child `SocketId`, and flushes its buffered
  Connected/Data — or replies `WouldBlock` when none is ready. Child
  creation is socket-quota-bounded (fail closed). `lib/rt::net` gains
  `listen`/`accept`.
- `CAP_NET_BIND_PRIVILEGED` (id 38) landed **with** its enforcement point:
  binding a local port at or below `SOCKET_PRIVILEGED_PORT_MAX` (1023)
  requires it (checked at `Bind`, Unix `CAP_NET_BIND_SERVICE` model),
  audited and fail-closed (§5.2). The frozen-id/name tests extend to `38`.
- The `CookieSecret` seam is backed by `netstack`'s `CryptoCookieSecret`:
  HMAC-SHA256 (`lib/crypto`, §2.12) over a per-boot key drawn from the
  platform CSPRNG at service start and never persisted; threaded into
  `on_tcp_segment` through the `run.rs` pump.
- Covered by the socket-service unit suite (privileged-bind
  allow/deny/ephemeral, listen state/errors, accept-`WouldBlock`, accept on
  a non-listener), the `CryptoCookieSecret` unit tests, and the ABI
  round-trip/fail-closed tests for the new ops and event. Docs:
  `docs/src/abi/net-sockets.md`.

##### N6b-2-β-2 — the live two-process QEMU listener vertical `[x]`
- `netstack_listener_qemu_aarch64` is the role-swapped mirror of the N5c
  stream vertical: a guest **server** command app (`tcpserve_program`) binds
  the well-known (**privileged**) `wire::GUEST_TCP_PORT`, `listen`s, `accept`s
  the host client's connection over the shared v6 link-local wire, echoes every
  received byte back, and verifies the received run against the shared
  deterministic stream; the host **client** peer (`netpeer::run_tcp_connect_peer`,
  `Tcb::connect`) streams `STREAM_TRANSFER_BYTES`, verifies the echo
  byte-for-byte, and injects bounded inbound loss so RFC 9293 retransmission is
  exercised both ways. PASS keys on the server's audited `exit`
  (`comm=tcpserve`, a verified exchange — a failed one parks forever) then the
  shell's scripted `exit` after the `TCPSERVE PASS` marker, **and** the client
  peer reporting the whole transfer echoed+verified, so neither side passes
  alone.
- The **privileged** listener path is exercised end to end: the guest server's
  manifest requests `CAP_NET_BIND_PRIVILEGED`, `CAP_NET_BIND_PRIVILEGED` is now
  in the administrator ceiling (`tairix_users::ADMINISTRATIVE_SET`) so the
  seeded root account grants it, and the netstack `Bind` gate enforces it
  against the kernel-attested origin.
- Shared byte gen/verify (`fill_chunk`/`verify_chunk`) was hoisted into
  `netstack_wire` as the one definition both fixtures and both host peers use
  (§2.2); the three fixture-store helpers in `image_apps` were collapsed onto
  one `fixture_store_files` (§2.2). Wire const `GUEST_TCP_PORT` (privileged,
  distinct from `PEER_TCP_PORT`) added.
- The standalone connection-exhaustion/SYN-flood *vertical* is N16b; this
  vertical proves the ordinary accept-and-serve path live.

### N7 — hardware offloads + performance hardening `[x]`

#### N7a — receive-checksum offload, negotiated end to end `[x]`
- The frame-ring transport carries a per-slot offload descriptor
  (`tairix_abi::driver::net_ring::FrameOffload` — `None` / `Validated` /
  `NeedsChecksum{csum_start,csum_offset}`) in a 5-byte fail-closed meta
  prefix, read/written by `push_with`/`pop_with` (`push`/`pop` stay the
  meta-`None` path). An unknown tag decodes to `None`, so a corrupt meta
  byte can only *lose* an offload, never fabricate one.
- `virtio_net` negotiates `VIRTIO_NET_F_GUEST_CSUM` when the device
  offers it and reports `NetOffloads::RX_CSUM_VALIDATED`; it tags each
  received frame from the device's `virtio_net_hdr` flags
  (`DATA_VALID`→`Validated`, `NEEDS_CSUM`→`NeedsChecksum` carrying the
  device's `csum_start`/`csum_offset`). The driver does no checksum
  arithmetic (it never links the stack), so the kernel — which links the
  driver crate for the device id — stays free of `lib/net`.
- `netstack` resolves the tag: it *completes* a `NeedsChecksum` frame in
  place through the one `internet_checksum` and then software-re-verifies
  it, and it passes a `Validated` frame's assurance to the engine as
  `RxMeta`. `lib/net`'s `Stack::on_frame_meta` skips the transport
  checksum *fold* (via `ChecksumCheck`/`UdpDatagram::parse_with`/
  `TcpSegment::parse_with`) only when the device validated it **and** the
  interface negotiated the offload; every semantic check still runs, a
  reassembled datagram is always software-verified, and the offload is
  never load-bearing for security.
- Same-bytes conformance: `rx_checksum_offload_matches_the_software_path_byte_for_byte`
  asserts the offloaded output equals the software oracle byte-for-byte;
  further tests prove the skip delivers a device-validated frame, a
  `Validated` claim is ignored without a negotiated offload, and a
  `NeedsChecksum` completion reproduces the transport checksum. Ring
  meta round-trip + unknown-tag-fail-closed, driver RX-tag, and the
  netstack completion/fail-closed paths are all unit-tested.

#### N7b-1 — transmit-side TCP checksum offload `[x]`
- The stack emits a TCP segment carrying only the partial (pseudo-header)
  checksum plus a per-frame TX offload descriptor when the egress
  interface negotiated `NetOffloads::TX_CSUM_TCP` and the segment is a
  single unfragmented frame. `checksum::Checksum::partial` is the folded
  uncomplemented pseudo sum (Linux `CHECKSUM_PARTIAL`); `ChecksumMode`
  threads Full/Partial through `tcp::write_with_checksum`; `TxOffload`
  (`None`/`PartialChecksum{csum_start,csum_offset}`) rides on
  `StackOutput.frames` (now `Vec<TxFrame>`), threaded through
  `push_frame`/`resolve_and_send`/`emit_ipv4_frame`/`send_ip*_packet*` and
  the `PendingPacket` ARP/ND-park queue. Offsets address the transport
  checksum within the Ethernet frame (14 + IPv4 20 / IPv6 40, TCP field
  +16).
- Ring: `tairix_abi::driver::net_ring::FrameOffload::TxChecksum` (tag 3,
  reusing the 5-byte meta prefix). netstack `queue_frames` maps
  `TxOffload`→`FrameOffload` and `push_with`s it; the RX resolver treats a
  `TxChecksum` tag as the software path (fail closed).
- `virtio_net` negotiates `VIRTIO_NET_F_CSUM` (feature bit 0) and reports
  `TX_CSUM_TCP`; `tx_one` reads the offload (`pop_with`) and builds the
  `virtio_net_hdr` with `VIRTIO_NET_HDR_F_NEEDS_CSUM` + the offsets so the
  device completes the fold. No checksum arithmetic in the driver.
- Same-bytes conformance: `tcp::tests::tx_partial_checksum_completed_matches_the_software_full_checksum`
  (codec) and `stack::tests::tcp_v4_tx_checksum_offload_matches_the_software_path`
  (engine) assert partial + device-completion == the full software
  checksum byte-for-byte; driver TX-header tests + the ABI round-trip
  cover the wiring. The path is guest-driven, so the existing TCP QEMU
  verticals exercise it once `virtio_net` advertises `VIRTIO_NET_F_CSUM`
  (QEMU recomputes the checksum on loopback).

#### N7b-2 — TCP segmentation offload (TSO) `[x]`
- The frame ring now carries **per-direction** capacities: `RingGeometry`
  holds a receive and a transmit slot capacity, sized by the one shared
  `RingGeometry::for_device(facts, slots)` (RX = MTU + Ethernet header; TX
  = `MAX_SLOT_CAPACITY`, a 64 KiB-class super-frame, when the device
  negotiated `TX_SEGMENT_TCP`, else the same as RX). The transmit ring
  never enlarges the receive ring. `AttachParams` carries both capacities
  and the driver validates the offered geometry against its own
  `for_device` minima.
- The ring meta prefix grew to 9 bytes to carry `gso_size` + `hdr_len`
  beside the checksum offsets; `FrameOffload::TxSegment { csum_start,
  csum_offset, gso_size, hdr_len, ipv6 }` (v4/v6 in the tag, 4/5) is the
  segmentation descriptor, fail-closed-decoded like the rest.
- `Tcb` emits one over-size **super-segment** only for fresh,
  never-retransmitted data at the send frontier (`snd_nxt == snd_max`, not
  in SACK recovery), bounded by `TcpConfig.tso_max_payload`; retransmits
  and SACK recovery always stay per-MSS, so a lost super-segment recovers
  as ordinary segments. `OutSegment.gso_size` carries the per-segment MSS.
- `Stack::send_tcp(dest, meta, payload, gso_size, now)` emits the
  super-segment as one IP packet — never IP-fragmented, never MTU-refused
  — tagged `TxOffload::TcpSegment`. The TCP checksum is the **length-0**
  pseudo-header partial (`ChecksumMode::PartialGso`; Linux
  `CHECKSUM_PARTIAL` for GSO), so the device adds each split segment's own
  length. `TSO_MAX_PAYLOAD` bounds the one IP packet to the 16-bit length
  field for either family.
- `virtio_net` negotiates `VIRTIO_NET_F_HOST_TSO4` **and** `TSO6` (both,
  since the offload is family-neutral) on top of `VIRTIO_NET_F_CSUM`,
  reports `TX_SEGMENT_TCP`, sizes its transmit staging to the GSO cap, and
  builds the GSO `virtio_net_hdr` (`gso_type` `TCPV4`/`TCPV6`, `hdr_len`,
  `gso_size`, `NEEDS_CSUM` + offsets). No checksum/segmentation arithmetic
  in the driver.
- netstack seeds `TcpConfig.tso_max_payload` from the egress interface's
  `Stack::tso_max_payload()` on connect and (via `Tcb::set_tso_max_payload`)
  on each accepted child once it is bound to its interface.
- Same-bytes conformance:
  `stack::tests::tcp_v4_tx_segmentation_offload_matches_the_software_path`
  splits the super-segment as the device must and asserts it reproduces
  the per-MSS software segments TCP-byte-for-byte (and that the field
  holds the length-0 partial). Ring meta round-trip + per-direction
  geometry, driver TSO-negotiation/GSO-header, and the netstack offload
  map are unit-tested. The path is guest-driven, so the existing TCP QEMU
  verticals exercise it when a backend offers `HOST_TSO*`; the `dgram`
  test backend does not advertise it, so the software path runs and the
  verticals pass unchanged (the N7a/N7b-1 precedent — host same-bytes
  conformance is the authoritative proof).
- **UDP transmit-checksum offload (`TX_CSUM_UDP`) stays on the software
  path — a settled decision, not deferred work.** virtio's
  protocol-agnostic partial-checksum contract cannot honour RFC 768's
  `0x0000`→`0xFFFF` substitution, which would put an illegal zero checksum
  on an IPv6 UDP datagram and silently disable protection on the rare IPv4
  datagram that folds to zero. `virtio_net` therefore never advertises
  `TX_CSUM_UDP`; the rationale is documented in `device_facts`.

#### N7c-1 — mergeable receive buffers (`MRG_RXBUF`) `[x]`
- `lib/virtio_net` negotiates `VIRTIO_NET_F_MRG_RXBUF` (feature bit 15)
  when the device offers it. The `virtio_net_hdr` becomes the 12-byte
  `virtio_net_hdr_mrg_rxbuf` on **both** rings (a transitional device
  sizes the header uniformly once mergeable is on); the header length is
  one runtime value used by every transmit and receive chain.
- The driver posts a **pool** of single-descriptor receive buffers
  (`RX_POOL` = the RX virtqueue size), not a single outstanding buffer,
  so a burst the device delivers back to back is captured before the
  stack next services the ring instead of being dropped past one buffer.
  This is the concrete N7c receive-side win and applies whether or not
  mergeable is negotiated.
- A frame the device merged across several buffers is reassembled in
  order, reading the buffer count from the first buffer's `num_buffers`:
  a ≤MTU frame arrives in one buffer (`num_buffers` == 1) and is
  delivered straight from it (one copy, into the RX ring); a merged frame
  is assembled through a reassembly buffer bounded to one link frame.
  Reassembly is total and fail-closed — a zero / out-of-range
  `num_buffers`, a completion naming no posted buffer, a runt shorter
  than the header, or an over-link-frame merge drops the frame (never a
  fabricated one, never an out-of-bounds access) and harvesting
  continues. Back-pressure holds a reassembled frame in `rx_pending` when
  the RX ring is full and retries it before the next completion, so
  nothing the device handed over is dropped and RX-ring order is kept.
  The negotiated features are one `features: u64` field behind
  `guest_csum`/`host_csum`/`host_tso`/`mergeable` accessors (no bag of
  bools).
- Host-tested: negotiation on/off, single-buffer over the 12-byte header,
  in-order three-buffer reassembly, the three fail-closed drops, and the
  pool capturing an `RX_POOL`-frame burst in one service; the existing 29
  driver tests still pass over the rewritten single-descriptor receive
  path. Docs: `lib/virtio_net` rustdoc + README, `docs/src/drivers/
  network.md`, README support-matrix networking paragraph.
- Guest-driven, so the live QEMU verticals exercise it once QEMU's
  virtio-net offers `MRG_RXBUF` (its default); the host reassembly tests
  are the authoritative multi-buffer proof (the N7a/N7b precedent).

#### N7c-2 — multiqueue receive (`VIRTIO_NET_F_MQ` / RSS) `[x]`
- **The transport carries one receive ring per device receive queue.**
  `RingGeometry` gained `rx_queues` (bounded by `MAX_RX_QUEUES` = 8, a
  pinned-memory resource bound), so a region is `rx_queues` receive rings
  followed by one transmit ring; `FrameRings` exposes `rx_queues()` /
  `rx_ring(i)` accessors (no-alloc fixed array, fail-closed on a bad
  index) in place of the old single `rx` field. `for_device` derives the
  count from `DeviceFacts.rx_queues` clamped to the ceiling; the
  `net_channel` Attach wire carries it. Transmit stays a single queue —
  the stack serialises its own egress, so a second transmit ring would be
  pinned memory without a consumer (§2.4).
- **`lib/virtio_net` enables and services N receive queues.** It
  negotiates `VIRTIO_NET_F_MQ` + `VIRTIO_NET_F_CTRL_VQ` when both are
  offered and the device advertises more than one pair, reads
  `max_virtqueue_pairs` from device config, brings up one receive + one
  transmit virtqueue per enabled pair (the idle transmit queues are held
  alive to satisfy the virtio "configure before enable" rule), sets up the
  control virtqueue, and issues `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET` after
  `DRIVER_OK`. Each receive queue is an `RxQueue` owning its own buffer
  pool + reassembly buffer + back-pressure `pending`; `service` harvests
  every queue into its matching shared receive ring, so a busy link's
  receive work is spread rather than serialised. `device_facts.rx_queues`
  reports the enabled count. The single-queue path (one `RxQueue` at index
  0) is unchanged behaviourally.
- **Host-tested end to end.** `multiqueue_enables_queues_and_steers_receive_per_queue`
  drives a real two-pair mock device: the driver negotiates MQ, completes
  the control-queue pair-count handshake (via a new QEMU-accurate
  `MockTransport::set_synchronous_notify` seam — a notify processes the
  queue inline, as a real vmexit does), reports `rx_queues == 2`, and each
  queue's frame lands in its own ring (queue 0 → ring 0, queue 1 → ring
  1). The ABI multi-ring geometry/accessor + fail-closed index checks are
  unit-tested in `net_ring`. The existing 39 driver tests and 336 netstack
  tests pass unchanged over the multi-RX API.
- **No live QEMU vertical is possible with the current harness — a
  backend limitation, not deferred work.** The net verticals use
  `-netdev dgram` (a unix-socket backend); QEMU multiqueue needs a netdev
  with `queues=N`, which `dgram` rejects, and `virtio-net-device,mq=on`
  over a one-queue dgram peer advertises `max_virtqueue_pairs = 1`, so a
  dgram-backed guest correctly stays single-queue. Only a `tap` netdev
  offers `queues=N` (host root/networking the sandbox lacks). Per the
  N7a/N7b/N7c-1 precedent, the host tests are the authoritative proof and
  the live verticals exercise the wiring only when the backend offers the
  feature; the multiqueue-capable driver runs safely single-queue on the
  existing dgram verticals. A `tap`-backed multiqueue vertical is the
  future consumer if the harness ever gains privileged networking.

#### N7c-3 — measured budgets + per-arch offload matrix `[x]`
- The engine's data-plane hot path is **allocation-free in steady state**,
  and that budget is enforced as a regression, not merely documented. Every
  `Stack` entry point takes a caller-owned, reused `&mut StackOutput`
  scratch; on entry it recycles the previous call's frame and payload byte
  buffers into a bounded internal pool (`BufPool`, capped so a hostile
  pattern cannot grow it) and draws every frame / IP-packet / upper-message
  / delivered-payload buffer from that pool, returning each transient buffer
  the moment its consumer copied it. Once warm, `on_frame`/`send_datagram`/
  `send_tcp` touch the allocator zero times. `netstack` holds one reusable
  `StackOutput` and copies frames into the ring by borrow, so the property
  carries into the live service.
- The budget is proven by `lib/net/tests/hotpath_allocations.rs`: a counting
  global allocator, two warmed back-to-back stacks (ARP resolved), and 512
  UDP transmit+receive rounds asserting **0** allocations. A per-packet
  allocation reintroduced on the send or receive path fails the build (the
  §2.16 regression guard).
- Scope of the enforced budget is the data plane (`send_datagram`/`send_tcp`
  transmit, `on_frame` receive); infrequent control-plane emissions (ARP/ND,
  ICMP errors, IGMP/MLD reports) and the timer sweep `advance` are outside
  it by design (rare, not per-packet). End-to-end throughput/latency over a
  real device are exercised by the guest-driven QEMU verticals
  (`netstack_stream_qemu_*`, `netstack_ping_qemu_*`) — the realistic
  measurement environment — rather than a machine-specific packets-per-second
  figure, which would be non-deterministic and useless as a regression gate;
  the allocation budget is the deterministic, machine-independent guard.
- Docs: `docs/src/lib/net.md` ("Performance budget"), `docs/src/drivers/
  network.md` ("Per-architecture offload state"), `lib/net/README.md`. The
  `README.md` support matrix carries the per-arch offload row (`Network
  offloads (RX/TX csum, TSO, mergeable RX)` — `✓ virtio` on x86_64/aarch64/
  riscv64, `—` on wasm32; the offloads are the arch-neutral driver's, so the
  only per-arch difference is the discovery bus).

### N8 — `ping`/`ss`-class command apps + observability `[x]`

#### N8a — per-interface counters through the System Information API `[x]`
- `lib/net::StackCounters` carries honest byte accounting (`rx_bytes`,
  `tx_bytes`) beside the frame/drop/ICMP/reassembly counters; bytes are
  counted at the single receive entry and the single transmit funnel.
  A pre-existing IGMP/MLD transmit double-count (both the membership
  emit *and* the shared frame funnel incremented `tx_frames`) is fixed —
  the funnel is now the one counting point — with a regression test.
- Counters reach user space exactly like interface state: the new paged
  `NetstackRequest::InterfaceCounters` op (gated `CAP_SYSINFO_INTROSPECT`
  at the broker, replacing the old admin-only per-iface `Counters` op)
  returns name-keyed `NetInterfaceCountersRecord`s; `sysinfod` forwards
  it through `SysinfoQueryId::NET_INTERFACE_COUNTERS` (id 21, gated
  `CAP_SYSINFO_GLOBAL`, audited); `lib/procinfo` resolves
  `stats:net/<iface>/{rx,tx}.{packets,bytes,dropped}` and the stack-wide
  defence aggregates `stats:net/stack/{icmp-errors,icmp-suppressed,
  reassembly-evicted}` (summed across interfaces). Every layer is
  fail-closed and host-tested; the engine keeps one honest per-direction
  `dropped` bucket rather than a fabricated errors/dropped split.

#### N8b-1 — windowed throughput rate queries `[x]`
- The live `stats:net/<iface>/{rx,tx}.{pps,bps}?window=…` rates exposed
  through the System Information API (§16.6) — never a `/proc` shape. The
  pure tickless `tairix_net::rate::RateMeter` (a bounded ring of coalesced
  counter snapshots, integer-only, no periodic timer) averages each rate
  over the window that *actually* elapsed; `netstack` owns one per
  interface and answers the new `NET_INTERFACE_RATES` broker query
  (`CAP_SYSINFO_GLOBAL`, audited), which `lib/procinfo` resolves. The
  `?window=` decoration (mandatory, fail-closed, `500ms`/`1s`/`2m`) is the
  one `stats:` query that carries one; `MetricKind::Rate` + the
  `packets/s`/`bits/s` units land with it. Host-tested at every layer; no
  new QEMU vertical (guest-driven, the N8a precedent). Docs: `sysinfo.md`,
  `netstack.md`, `lib/net.md`.

#### N8b-2a — the `ss` socket-listing tool + posture docs `[x]`
- The system-wide open-socket table is exposed through the System
  Information API: `NetSocketRecord` + `NetstackRequest::Sockets` +
  `SysinfoQueryId::NET_SOCKETS` (id 23, `CAP_SYSINFO_GLOBAL`, audited),
  netstack `SocketService::socket_records` (each entry carries its
  `owner_pid`) served by the `serve_read` broker helper, forwarded by
  `sysinfod`, and paged by the shared `tairix_procinfo::for_each_net_socket`
  (one query client, never a second). `NET_SOCKETS` is a privileged,
  system-wide diagnostic: netstack's sysinfo caller is `sysinfod`, so the
  original principal is not visible to netstack — per-caller "own sockets"
  is a *different* future mechanism, not a relaxation of this query.
- `userland/apps/ss` renders it in the iproute2 shape
  (`-t/-u/-a/-l/-n/-p/-4/-6/-H`, columns `Netid State Recv-Q Send-Q Local
  Peer [Process]`, addresses via `core::net`), notes hidden listeners on
  fd 3 (`net.listening_omitted`), and fails loud (never a partial table)
  when the capability is refused. 13-locale `Help/` tree, README, host
  tests; auto-discovered.
- Docs: `docs/src/userland/networking.md`, `docs/src/security/network.md`
  (threat model ↔ defence table + the §19.4 network event-id registry,
  ids 16_001–16_014), plus the `abi/sysinfo.md` / `userland/netstack.md`
  tables. Host-tested at every layer; no new QEMU vertical (the query is
  the N8a-precedent guest-driven path; the app is host-tested + planted).

#### N8b-2b — the `ping` command app `[x]`

##### N8b-2b-α — the ICMP-echo socket path + the `ping` app `[x]`
- **The ICMP-echo socket path is landed end to end** (host-tested).
  `SocketType::IcmpEcho` (wire value `3`) is served: `lib/abi/src/net.rs`
  adds the `SendEcho` request op (caller-chosen sequence; the stack owns the
  identifier) and the `SocketEcho` delivery frame (magic `"NSKE"`, the echo
  analogue of `SocketDatagram`); `NetSockProto` gains `Icmp`/`Icmpv6` so the
  socket lists in `ss`. The netstack `SocketService` gates the open on
  `CAP_NET_RAW` (audited, fail-closed), assigns each socket a globally-unique
  identifier, originates via the new `Netstack::originate_echo`, and
  demultiplexes `StackEvent::EchoReply` to the owning socket (identifier +
  connected-peer filtered). `lib/rt::net` gains `icmp_echo_socket` /
  `send_echo` / `recv_echo`. The `lib/net::icmp` echo codec and
  `Stack::send_echo_request` / `StackEvent::EchoReply` already existed and are
  reused unchanged.
- **`ping` (v4+v6, iputils-familiar §16.7)** lands as `userland/apps/ping`: a
  pure host-testable engine (`command`/`error`/`io`/`net` seam/`client`) over
  an injected `PingIo` clock+echo-socket+park seam, the freestanding `Run`
  binary driving it over `tairix_rt::net` + `clock_get`/`waitset`, a
  13-locale `Help/` tree (help-lint clean), and a README. Options
  `-c/-i/-s/-W/-w/-4/-6/-n/-q` + `-?/--help`; the target is a literal IP
  (no DNS in this plan — a hostname is a loud usage error). The reply line
  omits `ttl=` (not exposed by the echo socket — a documented divergence).
  Registered in the kernel `program_manifests` pin and the harness discovery
  test. Covered by unit tests at every layer (ABI round-trip/fail-closed,
  netstack open-gate/send/deliver, engine loss/quiet/verify).

##### N8b-2b-β — the live two-process QEMU vertical `[x]`
- `tairix-test-netstack-ping-qemu-aarch64` boots the production aarch64
  pipeline over the net-only-driver encrypted-root disk carrying the
  **system command store** (so the real `ping` bundle is present — no test
  fixture) plus the signed virtio-net driver (`FsDisk::PingRootDisk`), with a
  virtio-net device and the harness-side **passive ICMP echo responder**
  (`NetPeerMode::V6PingResponder` → `netpeer::run_ping_responder`: no
  campaign; `Stack::on_frame` answers ND + echo requests; verdict = ≥1
  `EchoRequestServed`). The serial script unlocks, logs in `root`/`root`, and
  runs `ping -c 3 fe80::2` (the peer's `link_local(PEER_IID)`, pinned by a
  host test). The guest bin arms on the tool's audited `exit` (`comm=ping`)
  and PASSes on the shell's next `exit`, typed only after an `icmp_seq=` reply
  line — a token `ping` prints only on a genuinely received reply — so an
  unanswered run times out fail-loud; the responder must also report a served
  request, so neither side passes alone.
- **`CAP_NET_RAW` joins the administrator ceiling** (`tairix_users::
  ADMINISTRATIVE_SET`): the `ping` manifest requests it for its ICMP-echo
  socket (netstack gates the open on it), and reaching below the transport
  layer is an administrative act — the Unix `CAP_NET_RAW`/setuid-`ping` model,
  the `CAP_NET_BIND_PRIVILEGED` precedent. The pinned administrator-ceiling
  count is 21→22. Ordinary transport use stays baseline `CAP_NET`.

### N9 — interface configuration, bonding, failover `[x]`

#### N9a — the `lib/netconfig` store engine `[x]`
- `lib/netconfig` is the one definition of the
  `/System/Settings/Network/network.conf` document: the per-interface
  `<iface>.<key>` grammar, the closed `IfaceKey` registry (`kind`,
  `match.mac`/`match.node`, `ipv4.*`/`ipv6.*`, `mtu`, `bond.*`), the typed
  value vocabulary (`IfaceKind`/`Ipv4Method`/`Ipv6Method`/`BondMode`/
  `MacAddr`/`Ipv4Cidr`/`Ipv6Cidr`), the bounded fail-closed parser
  (`MAX_CONFIG_LEN`/`MAX_INTERFACES`/`MAX_BOND_MEMBERS`; line-numbered
  `ParseError`), and the canonical render. Interfaces are stable admin
  aliases bound to hardware by identity; every field is `Option`-per-key so
  render emits only set keys and round-trips exactly.
- Whole-document `validate` enforces the bond invariants a per-line parse
  cannot: `bond.*` only on a `kind bond`, ≥2 members, `primary`∈members,
  every member a declared address-free `ethernet` enrolled in one bond,
  and static-method↔address agreement — all fail-closed
  (`ConfigError::InconsistentInterface`).
- `no_std`+`alloc`, no I/O, no authority; host-unit-tested (36) + fuzzed
  (`fuzz_netconfig`, registered in `cargo xtask fuzz`). README (experimental
  tier) + `docs/src/lib/netconfig.md`.

#### N9b-1 — the `configure net.*` stack-wide keys in `lib/sysconfig` `[x]`
- `configure` gains the `net.*` stack-wide keys in `lib/sysconfig`'s closed
  registry (§6.2): `net.ipv4.enabled`, `net.ipv6.enabled`,
  `net.ipv6.privacy` (typed `NetToggle`, `true`/`false`), and
  `net.tcp.syncookies` (typed `SynCookies`, `auto`/`always`, deliberately
  no `off`). A hand-written `SystemConfig::Default` sets the two family
  switches on, IPv6 privacy off, and cookies `auto`.
- Pure engine surface only (the N9a `lib/netconfig` precedent): the
  `configure` command lists/shows/sets/persists them unchanged, all 13
  Help locales + `docs/src/lib/sysconfig.md` + `configure/README.md`
  document them, and the sysconfig/configure host suites cover parse,
  defaults, closed-value rejection, and round-trip. `netstack` delivery +
  enforcement is N9b-2.

#### N9b-2 — deliver + enforce the `net.*` settings in `netstack` `[x]`
- **ABI** (`lib/abi/src/net_ipc.rs`): `NetworkSettings { ipv4_enabled,
  ipv6_enabled, syncookies_always, ipv6_privacy, tcp_keepalive, tcp_ecn }`
  (a `Default` matching the sysconfig defaults; `tcp_keepalive` added by
  N12, `tcp_ecn` by N13) and `NetstackRequest::ApplyNetworkSettings`
  (op 10),
  fail-closed encode/decode (booleans are exactly 0/1) + round-trip tests.
  `net.ipv6.privacy` maps to `ipv6_privacy` and is enforced by the RFC 8981
  temporary-address engine (see N10).
- **Engine** (`lib/net`): admin family-enable is engine state, not just
  service policy. `IfaceConfig.ipv6_enabled` skips link-local formation;
  `Iface::set_ipv6_enabled` flushes/reforms; `Stack.ipv4_enabled` +
  `set_ipv4_enabled`/`set_ipv6_enabled` gate `set_ipv4_config`/
  `add_ipv6_static`, clear routes on disable, and drop all inbound frames
  of a disabled family before parsing (an RA can't SLAAC a disabled
  interface). Host-tested (iface + stack suites).
- **Service** (`userland/net/netstack`): `Netstack` stores the settings,
  threads them into `add_interface`, and `apply_settings` re-applies to
  every existing interface (idempotent, order-independent). `service.rs`
  dispatches `ApplyNetworkSettings` under `CAP_NET_ADMIN` (audited,
  `NETWORK_SETTINGS_APPLIED` = 16_015); socket `open` refuses a disabled
  family (`Errno::NotSupported`, audited); `listen` maps
  `syncookies_always` → `ListenConfig.max_half_open = 0`, else the bounded
  default.
- **Delivery**: `devmgr` (FS-capable, already drives the stack's admin
  endpoint) reads `system.conf` post-unlock and pushes the settings once
  over `ApplyNetworkSettings`, fail-soft-retried on each hardware-tree
  generation bump (`netcfg` module: `NetworkConfigSource` seam +
  `settings_from_config` mapping + `NetConfigState`; events 13_012/13_013).
  `devmgr` gains `CAP_FS_ACCESS` (manifest + `DEVMGR_CEILING`) for that one
  read; `netstack` stays filesystem-free (§0).
- Deferred to N9b-3: runtime reload (the store is static this increment) and
  a live QEMU vertical (the enforcement is host-tested; a knob whose effect
  needs on-wire proof gets one when it lands).

#### N9b-3-1 — per-interface `network.conf` applied (static addressing) `[x]`
Per-interface static addressing is delivered and enforced end to end
(bonding excluded; that is N9b-3-2). The interface is bound to hardware by
its **stable MAC** (approach A: `netstack` holds each interface's MAC, so
it matches config by `match.mac` and renames the interface to the admin
alias).
- **ABI** (`lib/abi/net_ipc`): `NetInterfaceConfigMsg` — a **separate**
  framed message (own `"NIC1"` magic, 96-byte wire, versioned) carrying
  `alias`, an optional `match.mac`, typed `NetIpv4Config`/`NetIpv6Config`
  (`disabled`/`static`{addr,prefix,gw?} and, for v6, `slaac`), and an MTU
  override, with `to_le_bytes`/`from_bytes`/`validate` (unicast addresses,
  on-subnet v4 gateway, `≥1280` MTU) — round-trip + fail-closed tested.
  `NETSTACK_MAX_REQUEST` is now `max(NetstackRequest::WIRE_LEN,
  NetInterfaceConfigMsg::WIRE_LEN)`.
- **netstack**: `Netstack::apply_interface_config` (match by MAC→rename
  else by alias; validate-then-mutate so a refusal is atomic; static v6
  `Duplicate`→idempotent success); `run.rs` intercepts the framed message
  like `BindDriver` and gates/audits it via `serve_interface_config`
  (`CAP_NET_ADMIN`, event `16_016`). Host-tested (mac-match+rename,
  apply-by-alias, static v4/v6, disabled, mtu, atomic-on-refusal,
  idempotency, NotFound, alias-clash).
- **devmgr** (`netcfg`): `interface_configs_from_config` maps
  `network.conf` → `InterfaceConfigPlan` (bonds/members/loopback omitted; a
  managed non-bond iface without `match.mac` is rejected loud, event
  `13_016`); `deliver_interface_configs` pushes each per interface,
  retrying silently on `NotFound` (interface not bound yet) each hw-tree
  generation bump and recording each success (events `13_014`/`13_015`).
  Reuses `CAP_FS_ACCESS`; `netstack` stays filesystem-free.
- **`tools/mkimage`**: the writable root ships an **empty** `network.conf`
  ("no managed interfaces beyond loopback") via the `lib/netconfig` default
  render; the installer/`configure` write the operator's interfaces.
- **Docs**: `docs/src/userland/{netstack,networking}.md`.
- **Deferred to N9b-3-2**: bonding, `match.node` binding, runtime reload,
  bond `info:`/`state:`/`stats:`, and the live QEMU vertical (this
  increment is host-tested; the guest path is guest-driven and exercised
  once an image carries a configured static interface).

#### N9b-3-2-α — the pure `lib/net::bond` engine `[x]`
The link-aggregation decision core (`lib/net::bond`), landed ahead of its
netstack wiring (the N9a/N6b-2-α "engine before wiring" precedent). It is a
pure, family-agnostic, `neigh`/`mcast`-shaped state machine over member
NICs, driven by injected time and link reports; it owns no addresses,
routes, or I/O.
- **Modes** (closed set, §6.3): `active-backup` (one transmitting member,
  ordered failover, a declared `primary` reclaims the path) and `balance`
  (flow-hashed spread over the eligible members via a family-agnostic
  `flow_hash` — one flow stays on one member).
- **Health**: link-state driven with an anti-flap discipline — a member
  fails out **immediately** on link-down (fast failover), and is readmitted
  only after one `monitor_interval` up-delay (deliberate failback, never
  flapping). The monitor is tickless (`next_deadline` arms a one-shot only
  while a member awaits admission).
- **Events**: each mutation returns `BondEvent`s the composing interface
  acts on — `PathChanged` (emit gratuitous ARP / unsolicited NA + audit)
  and `WentDown` (transmit fails closed). `transmit_member` returns `None`
  when no member is eligible; the member set is bounded by
  `MAX_BOND_MEMBERS`. Runtime `set_mode`/`set_primary`/`add_member`/
  `remove_member` recompute through the one selection point.
- Covered by 15 `bond` host tests (admission/up-delay, immediate failover,
  deliberate + no-op failback, balance stickiness + spread, fail-closed
  transmit, runtime reconfig, flow-hash determinism). Not a byte decoder,
  so no new fuzz harness (§19.6). Docs: `lib/net` lib.rs, `README.md`,
  `docs/src/lib/net.md`.

#### N9b-3-2-β-1 — bond wiring, failover, reload (host-gate-green) `[x]`
The bond is composed end to end and host-tested; the live path is wired
but its QEMU verticals are β-2.
- **ABI** (`lib/abi/net_ipc`): `NetIfKind::Bond`; the self-identifying
  `NetBondConfigMsg` ("NBC1") + `NetBondMode`; `NET_BOND_MAX_MEMBERS` is
  the one member-count bound `lib/net::bond` and `lib/netconfig` both alias
  (§2.2). Round-trip/validate/fail-closed tested; `NETSTACK_MAX_REQUEST`
  widened. `Stack::announce_presence` (gratuitous ARP + unsolicited NA)
  added to `lib/net`.
- **netstack** (`iface.rs`/`run.rs`): a bond is an `Interface` owning one
  `Stack` (single MAC from the first member, no offloads); members are
  address-less conduits (`BondRole`) whose RX folds into the bond's stack
  and which refuse direct addressing (`addr_add`/`route_add`/
  `apply_interface_config` → `PermissionDenied`). `apply_bond_config`
  composes/reconciles (mode/primary/monitor/membership — the runtime
  reload path); `set_member_link`/`advance_bonds` drive `lib/net::bond`,
  sync the bond's aggregate link, and emit a gratuitous announcement on
  `PathChanged` tagged by the newly-selected member; egress is routed to
  the flow-selected member (`egress_member`), fail-closed when no member is
  eligible. Failover is audited (`BOND_CONFIG_APPLIED`/`BOND_CONFIG_REFUSED`
  16_017/16_018, `BOND_FAILOVER` 16_019). Bond `active-member`/per-member
  health are exposed (`bond_active_member`/`bond_member_health`). 9 host
  tests (compose/up-delay/defer-until-present/facts+member-address-refusal/
  immediate-failover+announce/last-member-down/deliberate-failback/reload-
  primary+membership/alias-shadow).
- **devmgr** (`netcfg`): the N9b-3-1 omission is removed —
  `interface_configs_from_config` now emits an address-less member rename
  per member, a `NetBondConfigMsg` per bond, and the bond's own
  (alias-matched) addressing; `deliver_interface_configs` delivers bonds,
  retrying `NotFound` until every member has bound. `NetstackBind` gains
  `apply_bond_config`. Host-tested.
- Docs: `docs/src/userland/netstack.md` (bond section), `security/network.md`
  (threat row + event registry 16_016..16_019).

#### N9b-3-2-β-2-i — bond observability query `[x]`
Bond members and live failover state are observable end to end through
the one System Information API path (never a `/proc` shape), host-tested
at every layer.
- **ABI** (`lib/abi`): `SysinfoQueryId::NET_BOND_MEMBERS` (id 24,
  `CAP_SYSINFO_GLOBAL`, audited) + registry row/frozen-id test; the paged
  `NetstackRequest::BondMembers{offset,limit}` op (11); and the
  `NetBondMemberRecord` (bond alias + member alias + `active`/`link_up`/
  `eligible` flags, WIRE_LEN 40) with round-trip/fail-closed tests. C
  header regenerated (`cargo xtask c-header --write`).
- **netstack**: `Netstack::bond_member_records(offset,limit)` flattens
  every bond's members (interface-table then configured-member order) over
  the ready `bond_active_member`/`bond_member_health` accessors; `serve`
  gates `BondMembers` on `CAP_SYSINFO_INTROSPECT` and routes it through
  `serve_read`. Host test (producer + broker-gate + failover-moves-active).
- **sysinfod**: `SysinfoSource::net_bond_members` + `net_bond_members_list`
  dispatch + `NetstackBondMembersPage` forwarder + gated/audited test.
- **`lib/procinfo`**: `net_bond_members_for` pages the query and filters by
  bond; `info:net/<bond>/members`, `state:net/<bond>/active-member`, and
  `state:net/<bond>/member-health` resolve through it (all
  `CAP_SYSINFO_GLOBAL`, fail-closed on a non-bond alias). Tests at every
  layer. Docs: `abi/sysinfo.md`, `userland/{netstack,networking}.md`,
  `security/network.md`.

#### N9b-3-2-β-2-ii-a — `match.node` interface binding (host-gate-green) `[x]`
An interface (plain or bond member) can be bound to hardware by its stable
**bus location** — `<iface>.match.node`, the register-window base of its
hardware-tree node — as an alternative to `match.mac`, independent of MAC
and discovery order. The identity is resolved where it is known, symmetric
with the MAC path, so `netstack` stays free of node ids and the binding is
order-independent (retried like `match.mac` until the interface appears).
- **`lib/netconfig`**: `match_node` is a typed `Option<u64>` parsed from a
  mandatory-`0x` hex, non-zero (a hardware *location*; `0` is the "no
  location" sentinel), round-tripping to canonical lowercase hex.
  `validate_interface` forbids it on a non-ethernet kind and forbids it
  together with `match.mac` (one identity, never two). The dead
  `MAX_NODE_MATCH_LEN` was removed.
- **ABI** (`lib/abi/net_ipc`): `NetstackRequest::BindDriver` gains
  `node_location: u64` (the location devmgr resolved for the bound NIC);
  `NetInterfaceConfigMsg` gains an optional `match_node: u64` selector
  (present-flag + u64 in the reserved tail). Encode/decode/validate +
  round-trip/fail-closed tests extended (dirty node tail/padding, bad
  present-flag, full u64 width). Not in the C-ABI surface.
- **devmgr**: `netbind::netchan_node_location` resolves a `netchan`'s
  **parent** NIC node's lowest register-window base and threads it into
  `bind_driver`; `netcfg::interface_configs_from_config` accepts a MAC *or*
  node identity (emitting `match_node`, no longer rejecting a node-bound
  interface). Host tests at both layers.
- **netstack**: `Interface` records the `node_location` from `BindDriver`;
  `apply_interface_config` locates the interface MAC → node-location →
  alias (a node selector never matches an unresolved `0` location). Host
  test added.
- Docs: `lib/netconfig.md`, `userland/netstack.md`, code rustdoc; `devmgr`
  event `13_016` doc broadened.

#### N9b-3-2-β-2-ii-b-static — the live static-addressing vertical (gate-green) `[x]`
`tairix-test-netstack-static-qemu-aarch64` boots the production aarch64
pipeline against a `static-net-root` disk (net-only driver bundle + a planted
`/System/Settings/Network/network.conf` binding the NIC to the `wan` alias by
`match.node` = the QEMU-virt virtio-net register base `0x0a00_3c00`, static
IPv6 `fd00::2/64`). `devmgr` autoloads the NIC, reads the config, binds `wan`
by bus location, and `netstack` assigns the static address — all pre-unlock,
headless. The host `V6StaticEcho` peer addresses the guest's *static* address
(never the link-local it always forms), so a `match.node` mis-bind fails loud;
PASS keys on `NETSTACK_BOUND` + `INTERFACE_CONFIG_APPLIED` + `INBOUND_ECHO_SERVED`.
Bringing it up surfaced and fixed **two latent defects the config path had never
exercised live**:
- **Live config delivery was broken.** `devmgr` read `network.conf` via
  `fs_open`, which returns `NotImplemented` before the root unlock (the VFS is
  not mounted) and is never retried afterward. Fixed by serving the closed
  set of `/System/Settings/` config files (`SystemConfigFile{System,Network}`)
  over the always-mounted read-only `/System` **store endpoint**
  (`StoreRequest::ReadConfig` in `lib/abi/driver_store`, served by the kernel
  `driver_store_server` via `SystemFileService::read_system_config` confined to
  `SYSTEM_VOLUME_SETTINGS_PATH`, read by `devmgr`'s config sources). `devmgr`
  also logs the resolved NIC bus location on `NETSTACK_BOUND` (`node` field).
- **Rename detached the interface pump.** `apply_interface_config` renamed the
  bound interface (`net0`→`wan`) but the driver `Channel` still held the old
  name, so `service_interface` could no longer find it and the interface went
  dark (no DAD, no RX, no replies). Fixed: the engine reports the rename
  (`IfaceRename`) and the service retargets the channel; netstack also flushes
  each channel after an admin mutation so a freshly-assigned address's DAD/MLD
  reaches the wire immediately. Regression tests at every layer.

`tairix-test-netstack-static-qemu-x86-64` is the virtio-**PCI** sibling: the
same production x86_64 pipeline, `static-net-root` disk, and `V6StaticEcho`
peer, differing only in the planted `network.conf`'s `match.node` value —
`STATIC_NETWORK_CONF_X86_64` names the NIC's lowest config-window BAR base
(`0xfe00_4000`, the deterministic address the kernel PCI enumerator assigns)
rather than the aarch64 mmio slot. `static_net_store_files` plants the arch's
conf by `PieArch`; everything else (runner, peer, fixture, sinks) is reused
unchanged. No production code changed.

`tairix-test-netstack-static-qemu-riscv64` is the riscv64 virtio-**MMIO**
sibling (the third and last Tier-1 target), over the QEMU `virt` board's PLIC
device-IRQ path: the same production `boot_riscv64::boot` pipeline,
`static-net-root` disk, and `V6StaticEcho` peer, differing only in
`STATIC_NETWORK_CONF_RISCV64`'s `match.node` value — the NIC's virtio-mmio
transport slot base `0x1000_7000` (slot 6 of the eight `0x1000_1000`-based
transports; QEMU fills them top-down and the root disk takes the top slot).
`static_net_store_files` plants it by `PieArch`; runner, peer, fixture, and
sinks are reused unchanged. No production code changed — all riscv64 autoload
foundations were already in place.

#### N9b-3-2-β-2-ii-b-bond — the live bond-failover vertical (gate-green) `[x]`
`tairix-test-netstack-bond-qemu-aarch64` boots the production aarch64 pipeline
against a `bond-net-root` disk: the net-only signed driver bundle **plus** a
planted `network.conf` binding two NICs by `match.mac` as the members of one
active-backup bond (`wan`, primary `m0`) carrying a static IPv6 address
(`fd00::2/64`). Two `virtio-net-device`s are attached (`net0`=`m0`/`GUEST_MAC`,
`net1`=`m1`/`GUEST_MAC_2`); `devmgr` autoloads the NIC driver into a process per
NIC and `netstack` composes the bond. The harness-side **bond peer** serves
*both* wires (replies on the arrival wire, campaigns on both), so it follows the
active member across the failover without tracking it. Once the guest serves its
first inbound echo, the runner drops the primary member's carrier over the QEMU
monitor (`set_link net0 off`); the driver's virtio config-change interrupt makes
`netstack` fail the bond over to the backup member, and the guest keeps
answering over the second wire. PASS keys on `BOND_CONFIG_APPLIED`,
`BOND_FAILOVER`, **and** an `INBOUND_ECHO_SERVED` observed *after* the failover
(the ordering makes a pre-failover echo insufficient), plus the peer's own
reply verdict — so neither side passes without the flow surviving the drop.

Bringing it up required two supporting pieces, both first-class:
- **A live link-status path (the sole live source of a bond failover).** The
  bond monitor is link-state driven, but the stack had no live link-down
  source. Now `lib/virtio_net` negotiates `VIRTIO_NET_F_STATUS`, reads the
  device-config `status` link bit (updated on the config-change interrupt the
  driver process already wakes the stack for), and stamps it into
  `DeviceFacts::link` and a new `ServiceReport.link` carried on every
  `netchan` `Service` reply. `Netstack::service_interface` returns a
  `link_change`; `run.rs` turns it into `on_member_link_change` →
  `set_member_link` → `BOND_FAILOVER`, transmitting the gratuitous
  announcement out the newly-selected member. Fully host-tested at each layer.
- **A config-delivery fix.** `devmgr`'s `deliver_interface_configs` applied a
  bond interface's own addressing before the bond was composed (a `NotFound`
  never retried in the same bump), so `fd00::2` was never assigned. It now
  re-attempts pending per-interface configs after composing the bonds
  (bounded until-stable), so the bond's address lands in the same bump.

Harness additions (all reusable): `tools/qemu` gained a generic marker-gated
`MonitorCommand` injection (used for `set_link`); a second attached NIC needs
no new builder (two `with_virtio_net_dgram_mac` calls); the bond peer
(`NetPeer::spawn_bond`/`run_bond_peer`) serves two wires from one engine.

`tairix-test-netstack-bond-qemu-x86-64` is the virtio-**PCI** sibling: the same
production x86_64 pipeline, `bond-net-root` disk, `Bond` peer, and mid-flow
`set_link net0 off` failover trigger. The bond binds its members by
`match.mac`, so `BOND_NETWORK_CONF` is arch-neutral and reused verbatim; the
two members and the failover simply live on the PCI bus. No production code
changed — the two dual-NIC verticals differ only in the emitted device model
(`virtio-net-pci` vs `virtio-net-device`), which the shared `Spec` builder
already selects per arch.

`tairix-test-netstack-bond-qemu-riscv64` is the riscv64 virtio-**MMIO** sibling
(the third and last Tier-1 target), over the QEMU `virt` board's PLIC
device-IRQ path: the same production `boot_riscv64::boot` pipeline,
`bond-net-root` disk, `Bond` peer, and mid-flow `set_link net0 off` failover
trigger. The bond binds by `match.mac`, so `BOND_NETWORK_CONF` is arch-neutral
and reused verbatim; the two NICs and the failover live on the board's
virtio-mmio transports. No production code changed. With it all three Tier-1
targets carry both declarative-config verticals.

#### N10 — RFC 8981 IPv6 temporary (privacy) addresses `[x]`
The `net.ipv6.privacy` policy is enforced end to end: in addition to the
stable SLAAC address of each autonomous prefix, the interface forms a
short-lived temporary address with a randomised interface identifier,
regenerated before it deprecates, so a host is not tracked by a stable
address across sessions.
- **Engine** (`lib/net/src/iface.rs`): `AddrOrigin::Temporary`, per-prefix
  temporary-address maintenance driven from `Iface::advance` — form one per
  stable SLAAC prefix, cap its preferred/valid lifetimes by the prefix's own
  and a random DESYNC_FACTOR, regenerate `REGEN_ADVANCE` before the preferred
  lifetime expires (so a fresh one is preferred before the old deprecates),
  and bound duplicate-IID DAD failures to `TEMP_IDGEN_RETRIES` per prefix.
  Reserved identifiers (RFC 5453) are rejected and re-drawn. Entropy is the
  injected `TempAddrSource` seam (the engine stays pure/`now`-driven); the
  temporary lifetimes are `IfaceConfig` knobs (RFC 8981 defaults). RFC 6724
  rule 7 (`route::CandidateAddr.temporary`) prefers a temporary source for
  outbound flows. `Iface::set_privacy`/`Stack::set_privacy` toggle it at
  runtime (enable forms them promptly; disable removes them, keeps the stable
  address). Host-tested (formation, distinct/non-reserved IID, reserved skip,
  regeneration overlap, retry cap, runtime toggle, source preference) and
  fuzzed against hostile RAs (`fuzz_net_stack` runs with privacy on).
- **ABI/config/service**: `NetworkSettings.ipv6_privacy` (N9b-2), mapped from
  `net.ipv6.privacy` by `devmgr::netcfg::settings_from_config` and applied by
  `Netstack::apply_settings` → `Stack::set_privacy`. The `Run` glue injects a
  per-interface CSPRNG-backed `TempAddrSource` (kernel `random_get`); host
  tests inject a deterministic one.

#### N11 — RFC 8200 §4.5 IPv6 source fragmentation `[x]`
An IPv6 datagram the host originates larger than the path MTU is
source-fragmented — the only entity that may fragment an IPv6 datagram is
its source, since routers never do — closing the dual-stack parity gap
where IPv4 fragmented on emit but IPv6 refused with `SendError::TooLarge`.
- **Engine** (`lib/net/src/ipv6.rs`): pure `fragment(payload_len, mtu)`
  planner (mirroring `ipv4::fragment`) returning contiguous, 8-byte-aligned
  (bar the last) `FragmentPiece`s that cover the payload exactly once, plus
  `write_fragment_header` for the 8-byte Fragment extension header
  (`FRAGMENT_HEADER_LEN`). Fails closed below the 1280-byte floor and beyond
  the 13-bit offset field (`MAX_FRAGMENT_OFFSET`). Host-tested (sizing,
  fail-closed cases, fragment→walk→`Reassembler` round-trip) and fuzzed
  (`fuzz_net_ipv6`).
- **Stack** (`lib/net/src/stack.rs`): a 32-bit `next_ipv6_ident`, an
  `emit_ipv6_frame` refactor (multicast MAC vs. next-hop resolution shared
  by the whole and fragmented paths, §2.2), and `send_ipv6_fragmented`. The
  UDP and ICMPv6-echo v6 send paths compute the transport checksum over the
  whole message first (so the first fragment carries it), then fragment
  against the path MTU (unicast) / link MTU (multicast); `TooLarge` now
  means only "cannot be fragmented at all". Host-tested: oversize v6 UDP and
  echo round-trip end to end, and an oversize multicast datagram emits well-
  formed fragments that a member reassembles. TCP is unaffected (segments
  are MSS-sized, never IP-fragmented).

#### N12 — RFC 9293 §3.8.4 TCP keepalive `[x]`
An established but idle TCP connection can be probed for peer liveness and
torn down when the peer becomes unreachable, so a long-lived idle
connection does not linger forever against a silently-dead peer.
- **Engine** (`lib/net/src/tcp_conn.rs`, host-gate-green): `TcpConfig`
  gains `enable_keepalive` (off by default per RFC 1122 §4.2.3.6),
  `keepalive_idle`, `keepalive_interval`, and `keepalive_probes` (defaults
  2 h / 75 s / 9, the BSD/Linux values). The `Tcb` carries a
  `keepalive_deadline`/`keepalive_unacked`/`keepalive_pending` triple and a
  `Plan::Keepalive` probe: an idle established connection — one with no
  unacknowledged or queued data, since in-flight data is already proven live
  by the retransmission timer — emits after the idle interval a zero-length
  ACK carrying `snd_nxt - 1`, which a compliant peer must acknowledge
  (RFC 1122 §4.2.3.6). Each unanswered probe spaces the next by
  `keepalive_interval`; after `keepalive_probes` unanswered probes the
  connection aborts with a RST and `ResetReason::TimedOut`, exactly as the
  user timeout does. Any inbound segment or fresh data send re-arms the idle
  timer; `next_deadline` folds the keepalive deadline so the service arms one
  one-shot timer (never a poll loop, §2.23). Host-tested (probe-on-idle +
  reply-resets, abort after the probe budget, disabled-by-default, and
  data-send defers the probe).
- **Stack-wide policy** (gate-green): keepalive is wired end to end through
  the existing config chain — a `net.tcp.keepalive` `NetToggle` key in the
  `lib/sysconfig` `net.*` registry (§6.2, off by default), the
  `tcp_keepalive` boolean on `NetworkSettings` (`lib/abi/src/net_ipc.rs`,
  wire byte 12, fail-closed encode/decode + round-trip tests), the
  `devmgr::netcfg::settings_from_config` mapping, and `netstack` seeding
  `enable_keepalive` on both connection paths — `socket.rs::connect_stream`
  (outbound `TcpConfig`) and `listen_config` (the listener's
  accepted-connection `template`) — from the delivered settings. Read at
  connect/`listen` time like the SYN-cookie mode, so it needs no
  per-interface re-application. The engine's `keepalive_idle`/`interval`/
  `probes` keep their RFC-default timings; the stack-wide switch toggles
  keepalive on or off (the config store models closed-set values only, so
  numeric per-timing knobs are not a `system.conf` key). Host-tested in
  `netstack` (`listen_config` template) and `lib/sysconfig`/`devmgr`
  (registry + mapping).

#### N13 — RFC 3168 Explicit Congestion Notification `[x]`

The pure `lib/net` ECN engine, its full data-path threading, the
stack-wide operator toggle, **and a live two-process QEMU vertical
asserting the ECN handshake on the wire** are all landed and gate-green.

- **Shared codepoint** (`addr::Ecn`, done): the RFC 3168 §5 two-bit
  codepoint (`NotEct`/`Ect1`/`Ect0`/`Ce`), one definition both IP families
  express — the IPv4 `Ipv4Header.ecn` field (low two bits of the TOS byte,
  DSCP written zero) and the IPv6 `Ipv6Header::ecn()`/`set_ecn()` accessors
  (low two bits of Traffic Class, DSCP preserved). Round-trip host-tested.
- **Congestion response** (`tcp::cc`, done): a `CongestionControl::on_ecn`
  signal, applied at most once per window, whose trait default is the
  RFC 3168 baseline (defer to `on_loss` — the same multiplicative decrease
  with no retransmission) so a minimal policy is always correct. CUBIC and
  NewReno both implement **RFC 8511 Alternative Backoff with ECN (ABE)**:
  in congestion avoidance an ECN mark backs off with a larger `beta_ecn`
  (0.8 NewReno vs 0.5 loss; 0.85 CUBIC vs 0.7 loss), so a shallow-AQM mark
  drains the bottleneck without needlessly under-filling the path; in slow
  start the standard loss reduction stands (RFC 8511 §3.1). Each policy
  reuses its one loss-reduction path parameterised by the backoff factor
  (no second code path), host-tested for the gentler-than-loss CA response,
  the slow-start fallback, and the exact per-policy `beta_ecn`.
- **Connection engine** (`tcp::conn`, done): `TcpConfig::enable_ecn` (off
  by default). Negotiation (ECN-setup SYN with ECE+CWR; SYN-ACK with ECE
  alone; `ecn_ok` set only on the exact exchange, falling back otherwise),
  receiver CE→ECE echo until the peer's CWR (§6.1.3), sender ECE→once-per-
  window `on_ecn` reduction + CWR on the next fresh data (§6.1.2), and
  ECT(0) marking of fresh data only (never control, retransmissions, or
  window probes, §5.2/§6.1.6). `OutSegment.ecn` carries the codepoint to
  the framer; `on_segment` takes the received codepoint.
- **Stack + service** (done): `Stack::send_tcp` stamps the codepoint into
  the IP header and `StackEvent::TcpSegment` surfaces the received one; the
  `netstack` `SocketService`/`Listener` thread it end to end. Host-tested
  in `tcp_conn_tests` (negotiation, CE echo, ECE reduction + CWR, once-per-
  window, fallback), `stack_tests` (ECT(0) on emit, CE surfaced on
  receive), and `ipv4_tests`/`ipv6_tests`/`addr` (codepoint round trips).
- **Stack-wide policy** (done): the `net.tcp.ecn` operator toggle is wired
  end to end exactly like `net.tcp.keepalive` (N12) — a `net.tcp.ecn`
  `NetToggle` key in the `lib/sysconfig` `net.*` registry (§6.2, off by
  default), the `tcp_ecn` boolean on `NetworkSettings`
  (`lib/abi/src/net_ipc.rs`, wire byte 13, reserved tail from byte 14,
  fail-closed encode/decode + round-trip/dirty-tail tests), the
  `devmgr::netcfg::settings_from_config` mapping, and `netstack` seeding
  `enable_ecn` on both connection paths — `socket.rs::connect_stream`
  (outbound `TcpConfig`) and `listen_config` (the listener's
  accepted-connection `template`) — from the delivered settings. Read at
  connect/`listen` time like keepalive, so no per-interface re-application.
  All 13 `configure` Help locales + README + docs/src updated.
- **Live vertical** (done): `tests/integration/netstack_ecn_qemu_aarch64`
  boots the production aarch64 pipeline against the stream vertical's
  encrypted-root disk plus a planted `system.conf`
  (`tairix_test_netstack_wire::ECN_SYSTEM_CONF`, `net.tcp.ecn true`), so
  `devmgr` delivers `tcp_ecn = true` and the guest `tcpecho` client
  negotiates ECN. The host peer (`netpeer::run_tcp_echo_ecn_peer`,
  `NetPeerMode::V6TcpEchoEcn`) is ECN-capable and verifies on the live
  wire that the guest's SYN offered ECN (ECE+CWR), its data carried
  ECT(0), and — after the peer echoed ECE for a bounded injected CE — the
  guest reduced its window and set CWR on a subsequent segment; its
  verdict requires all three plus the full echoed transfer, so a stack
  that ignored the toggle fails the run loud. The wire `system.conf` is
  cross-checked against the real `lib/sysconfig` engine
  (`ecn_system_conf_enables_only_ecn`), and the ECN store/disk/peer reuse
  the stream vertical's one builder and delivery path (no duplication).

#### N14 — the Broadcom GENET v5 link driver (Raspberry Pi 4B) `[x]`

The first NIC driver that is not virtio: the Pi 4B's on-board gigabit
Ethernet (`brcm,bcm2711-genet-v5`, a UniMAC with an embedded MDIO master
driving an external BCM54213PE RGMII PHY). It serves the same N4c
device-channel seam, so the whole stack — sockets, TCP, DNS, DHCP — reaches
real hardware with no change above the driver boundary. Before this the
flashable Pi image shipped no NIC at all.

##### N14a — the `netchan-v1` driver side hoisted to `lib/netchan` `[x]`
`NetChannelServer` lived in `lib/virtio_net` and the ~250 lines of
device-agnostic process glue around it in `virtio_net_driver/src/main.rs`; a
Broadcom MAC driver can link neither. Both moved verbatim into the new
`lib/netchan` crate, which every NIC driver process now shares (§2.2):
- `NetChannelServer<N: Net>` — the pure, host-tested per-request handler.
- `serve(net, irq_handle)` — the freestanding loop: claim a reserved endpoint
  bound restricted-sender `{CAP_NET_RAW}`, publish the `netchan` node, park on
  `{call endpoint, device IRQ}`, `shm_map` on attach. Gated
  `cfg(target_os = "none")`, so the host build carries only the pure half and
  nothing pulls `tairix-rt` into a kernel graph.
- `netchan::exit` — the reserved fail-closed exit codes (80–83) both NIC
  driver binaries report, one definition.
- `NETCHAN_NODE_COMPATIBLE` moved into `lib/abi::driver::net_channel` beside
  the endpoint block, replacing a `devmgr` `const` plus a bare literal in the
  driver; `devmgr` and both drivers now name the one definition.
`virtio_net_driver` is now bring-up plus one `netchan::serve` call, and its
behaviour is unchanged — proven by the existing `netstack_autoload_qemu_*`,
`netstack_dhcp_qemu_*` and `netstack_dhcp6_qemu_*` verticals staying green.

##### N14b — the `drivers/network/genet` device engine `[x]`
One crate, `drivers/bus/pcie_brcm`-shaped: a host-testable `lib` target plus
the `Run` binary, co-located because a NIC sits above the §18.6 bootstrap
floor and so has no charter-legal non-driver consumer for a `lib/*`
device-support crate (§2.22). Key facts for the next worker:
- **Register map** (`regs.rs`): `SYS` / `EXT` / `INTRL2_0` / `RBUF` / `UMAC`
  blocks, the receive descriptor RAM at `0x2000` and transmit at `0x4000`,
  and the per-ring / per-block register offsets derived from them
  (`base + 256·12 + ring·0x40`; ring 16 is the default queue). Descriptors are
  **on-chip**, inside the register aperture, so the only DMA is frame buffers.
- **Bring-up** refuses anything but the GENET v5 core revision
  (`SYS_REV_CTRL`), masks all of `INTRL2_0` *before* programming, resets the
  RBUF and UniMAC under local loopback, programs the MAC and frame limit,
  builds both rings over one 256 KiB carve (64 × 2 KiB per direction),
  negotiates the PHY, then enables TX/RX and unmasks exactly
  `{RXDMA done, TXDMA done, link up, link down}`.
- **PHY** (`mdio.rs`): the UniMAC MDIO master plus generic clause-22
  autonegotiation (BMCR/BMSR/ANAR/ANLPAR + the 1000BASE-T pair) — no vendor
  register. Every handshake is bounded by wall clock and fails closed. A link
  event re-resolves and re-programs the link on the next service doorbell, so
  a cable change needs no driver restart.
- **`Net::service` is non-blocking**: reclaim completed transmits, drain the
  ring into free slots, harvest deliveries. `rx_ring_full` back-pressures with
  the frame left in its descriptor; error/fragmented/malformed descriptors are
  dropped but still freed, and a queued frame wider than a device buffer is
  released explicitly, so nothing can wedge either ring. Receive descriptors
  are armed once and never rewritten — the consumer index alone hands a slot
  back.
- **The controller's own reports are validated**, not trusted: a transmit
  consumer index past what was queued, or a receive producer index past the
  ring's capacity, is refused with `DeviceFault` rather than acted on.
- **Offloads: none advertised.** The GENET has checksum and segmentation
  engines, but §0 binds a driver to advertise only what it *verified*, and
  QEMU models no GENET. The software path stays canonical, so the NIC is
  complete without them; advertising them is a later increment gated on metal.
- **The MAC address is not in the controller.** The Pi's factory address is
  published by firmware through the device tree's
  `mac-address` / `local-mac-address` binding, so a new
  `HwResourceKind::LinkAddress` (`lib/abi`) carries it in the node's grant set
  — the one carrier that reaches a driver *process* (`resource_grants`
  delivers resources, not node snapshots). It is a discovered *fact*, not a
  handle: `HwResourceKind::required_capability` is now `Option<CapabilityId>`
  and reports `None` for it (capability ids start at 1, so the stored `0`
  names none). A node publishing no address fails bring-up closed rather than
  inventing one. `RtDriverHost::link_address` surfaces it; `lspci` renders it.
- **Coverage**: 26 host tests over a register-level controller model — the
  revision gate, the bring-up write sequence *and its ordering*, MDIO framing
  and its fail-closed timeout, link resolution at every rate plus link-down
  and re-plug, transmit encoding with producer/consumer accounting and ring
  wrap, receive delivery past the 2-byte pad, every drop class, back-pressure
  without loss, and sensitive-class scrubbing in both directions.

##### N14c — discovery, the signed bundle, and the shipped image `[x]`
- **A latent defect fixed**: `push_irq_resources`
  (`kernel/arch/aarch64/src/platform.rs`) emitted the *raw* device-tree
  `interrupts` cell as the IRQ resource instead of the GICv2 INTID
  (`gic_intid_from_cells`: SPI + 32, PPI + 16). No driver read it before — the
  virtio-MMIO nodes get their line from `hwdiscovery`'s `slot_irq`, EMMC2 from
  `root_unlock::emmc2_spi`, the VL805 from a driver-minted resource — but
  GENET parks on it, so it would have bound INTID 157 instead of 189 and never
  woken. Now mapped through the one shared decoder; an unrepresentable
  specifier is dropped, never guessed.
- **The DMA constraint** is read from the GENET's **parent bus** `dma-ranges`
  (`/scb`, `0..0xfc00_0000` on the Pi 4), because Devicetree Spec v0.4 §2.3.9
  puts the property on the bus, not on the mastering device — a discovered
  value, never a board constant. `BusLevel` gained `dma_ranges` and
  `dma_ranges_aperture` was split into a byte-slice decoder plus its node
  wrapper (one decode, no behaviour change for the PCIe caller).
- **The link address** is read from the standard ethernet-controller binding
  for *any* node that carries it (`mac-address` first, then
  `local-mac-address`), never a board special case; an all-zero or
  wrong-length property is ignored.
- **Fixture**: `raspi_like_arm` grew the `/scb` bus and the
  `ethernet@7d580000` node (both SPIs, the board MAC, the `mdio@e14` child),
  so discovery is proven to emit a Network node at CPU-physical `0xfd58_0000`
  with INTIDs 189/190, the SCB aperture, and the MAC.
- **Bundle**: `GENET_STORE_PATH` (`Drivers/network/genet/Run`) and
  `build_genet_bundle`, requesting exactly the virtio-net set
  `{MMIO_MAP, MEM_DMA, IRQ_BIND, SHM, IPC_ENDPOINT, IPC_BIND_PRIVILEGED,
  HW_EMIT, LOG_EMIT}`; planted by `build_image_driver_bundles`, so the
  flashable Pi image autoloads the NIC. The store-scan test proves a GENET
  node resolves to it and to nothing else.
- **No QEMU vertical is possible**: QEMU models no GENET and its `raspi*`
  machines hand the kernel no device tree. The register-level suite is the
  coverage; the live path is an on-metal acceptance item (`plans/PI.md`), as
  for the Pi's EMMC2, PCIe and HVS drivers.

#### N15 — the socket half-close (`shutdown`) `[x]`

The POSIX `shutdown` surface N5c named and N6 did not land: a client could
only `Close` a stream, which releases the socket, so it had no way to signal
end-of-request and still read the response. Key facts for the next worker:
- **The engine already had it.** `Tcb::close` *is* the RFC 9293 §3.10.4
  CLOSE — it queues a FIN and the TCB keeps receiving through FIN-WAIT-2 —
  and `Tcb::send` already fails closed once a FIN is queued. So this is a
  socket-surface increment with no protocol change. What was missing was a
  test of the guarantee it rests on: `tcp::conn::tests`
  `half_close_keeps_receiving` now drives a FIN-WAIT-2 peer receiving data.
- **ABI:** `ShutdownHow` (`Read`=1/`Write`=2/`Both`=3, fail-closed
  `from_u8`) and `SocketRequest::Shutdown { socket, how }` (op 11). `how`
  occupies header byte 12 as `SHUTDOWN_HOW_OFFSET` — the byte `Socket`
  spends on its type, since the two ops are disjoint and each requires it
  zeroed in the other. Reserved-field rejection matches `Close`/`Listen`.
  Covered by the existing round-trip fuzz arm and `fuzz_net_sockabi`; no new
  harness. The complete POSIX direction set is deliberate (§27): a
  write-only half-close is not `shutdown`.
- **Service:** `StreamConn.read_shutdown` plus a `shutdown` handler.
  `Write` calls `tcb.close` and pumps so the FIN leaves at once, but does
  **not** set `client_closed`, so the socket keeps delivering and is not
  reaped — the client still owns the handle and must `close` it.
  `peer_closed` already excludes FIN-WAIT-1/2, so `Closed` still arrives
  only on the peer's FIN. `Read` makes `collect_stream_events` keep draining
  the receive buffer but discard it, so the advertised window stays open and
  a still-sending peer is never stalled. Idempotence comes from the new
  `Tcb::send_closed` accessor rather than a second socket-level flag that
  could drift from the engine.
- **Errors:** `NotConnected` for an unconnected or listening socket,
  `OutOfRange` for a datagram or echo socket (only TCP has a FIN). No new
  capability or audit event — `shutdown` is not a privilege operation — and
  no `ss` change, since `map_tcp_state` already reports FIN-WAIT-1/2.
- **Tests:** `lib/rt::net::shutdown` is the client wrapper; the netstack
  suite covers the FIN flush with the socket still alive, the peer's reply
  arriving after the client's FIN, send-after-shutdown, the idempotent
  repeat, read-discard with a drained receive queue, `Both`, and the four
  refusal cases. Docs: `docs/src/abi/net-sockets.md`.

#### N16 — connection-exhaustion defence: observability, then the live vertical `[~]`

N6b-2-β-2 deferred the standalone SYN-flood vertical "to N7+", and N7 is
done. Auditing it first turned up a prerequisite: the defence counters the
vertical needs as its witness were tracked but unreadable.

##### N16a — the stack-wide TCP defence counters `[x]`

`tcp::listen::ListenerStats` had **no consumer anywhere in the tree**, so
§5's promise that SYN-cookie activations are visible under
`stats:net/stack/…` was unmet — `lib/procinfo` even pinned the absence with
a test asserting `stats:net/stack/syn-cookies` resolved to
`UnknownSelector`. Key facts for the next worker:
- **A single record, not a page.** The counters belong to the socket table
  as a whole, so they have no per-interface home: `NetstackRequest::StackDefence`
  (op 13, no argument) answers one `NetStackDefenceCounters` (8 × `u64`),
  reached through `SysinfoQueryId::NET_STACK_DEFENCE` (**id 34**,
  `CAP_SYSINFO_GLOBAL`, audited). Note `SYSINFO_QUERIES` is indexed *by id* —
  a spec appended in the wrong position makes `spec_for` return `None` and
  every call fail `NotImplemented`; the entry must sit at index == id.
- **Monotonic across a listener close.** `SocketService.retired_defence`
  folds a listener's totals in as it is dropped, so `defence_counters()` is
  live-plus-retired. Summing only live listeners would let a flood that
  ended with its target socket closing vanish from the count — the
  regression test is `defence_counters_survive_a_listener_close`.
- **Two sources behind one selector prefix.** `net_stack_metric` routes the
  packet-path leaves (`icmp-errors`, `icmp-suppressed`,
  `reassembly-evicted`) to the summed per-interface counters as before, and
  the eight connection-defence leaves (`syn-cookies`,
  `syn-cookies-{accepted,rejected}`, `syn-backlog-{started,expired}`,
  `accepts`, `accept-overflow`, `tcp-resets`) to the new single-record
  query. Summing *those* per interface would multiply one figure by the
  interface count.
- **`ss -s`/`--summary`** prints the totals instead of the socket table
  (the iproute2 switch), a refused query being fatal there as for the
  listing. Its `OPTIONS` row landed in all thirteen help locales, which
  `help-lint` enforces for cross-locale switch-key parity.
- Docs: `docs/src/userland/{networking,netstack}.md`, the `ss` help, and
  §5's counter list above.

##### N16b — the live connection-exhaustion vertical `[~]`

`tests/integration/netstack_synflood_qemu_aarch64` is the N6b-2-β-2 listener
vertical run against a *hostile* peer. Key facts for the next worker:
- **No new guest fixture.** It reuses that vertical's disk and `tcpserve`
  server unchanged (`FsDisk::ListenRootDisk`): what is under test is the
  stack's behaviour under connection exhaustion, not a new guest program.
  The peer is the whole difference (`NetPeerMode::V6TcpFlood`,
  `netpeer::run_tcp_flood_peer`): it sends `max_half_open + 1` SYNs from
  distinct source ports and **never answers** the SYN-ACKs, so each occupies
  a backlog slot until the guest expires it; then it opens one real
  connection, whose SYN meets a full backlog and can be admitted only
  through a stateless cookie, and verifies the guest echoes the whole
  transfer over it. The flood's SYNs are hand-built through
  `Stack::send_tcp` rather than driven by `Tcb`s — the point is precisely
  that they are never completed, so there is no connection state to keep.
  The count comes from `ListenConfig::default().max_half_open`, not a
  restated literal.
- **The cookie path had to become observable to be provable.** A run where
  the flood never landed would otherwise look identical to a pass, since a
  cookie SYN-ACK is indistinguishable on the wire from a backlog one. So
  `netstack` now audits the transition — `SYN_COOKIES_ENGAGED` (=16024),
  emitted **once per listener** so a flood cannot amplify itself through the
  log — reported out of `StreamIo::cookies_engaged` rather than logged in the
  engine, keeping the service returning facts and the caller holding the
  sink. The vertical's serial script requires that message *before* it will
  await the fixture's PASS marker (an expect-only step that types nothing).
  Note a userland service's `log_emit` reaches the **diagnostic** sink, never
  the kernel audit sink an in-kernel witness bin watches — which is why this
  is a serial-transcript gate and not a sink counter, as for the DHCP
  verticals' netstack witnesses.
- **Three witnesses, none sufficient alone:** the cookie-brake message, the
  fixture's audited `exit` (a verified exchange — a shortfall parks forever),
  and the peer's verdict, which requires *both* the whole flood sent and the
  whole transfer echoed back verified.
- **Scope.** This proves backlog overflow → cookies engaged → a
  cookie-reconstructed connection serving data. Accept-queue exhaustion stays
  covered by the `tcp::listen` unit tests and the `fuzz_net_tcp` listener
  driver; a second live vertical for it would be scope creep. No loss is
  injected here — retransmission has its own vertical (N6b-2-β-2).
- **Why this is `[~]` and not `[x]`: the live run is unvalidated on this
  development machine, and the cause is not in this increment.** The vertical
  is written, registered, and host-green (the audit-once-per-listener
  mechanism is covered by the `cookies_engaged_is_reported_once_per_listener`
  netstack test), but `cargo xtask test --qemu` reports it *passing* in ~10 s
  — far too fast for unlock + login + a 257-SYN flood + a 32 KiB transfer,
  which the sibling verticals budget 300 s for. The **same** is true of the
  pre-existing `netstack_listener_qemu_aarch64` on a **clean tree with this
  increment stashed** (12.5 s, exit 0), so it is an environment or
  harness-level condition that predates this work, not a regression from it:
  a QEMU run that ends early is being scored `Outcome::Pass`, and on a pass
  the runner persists no serial transcript to contradict it. Two *earlier*
  iterations of this vertical did boot for real (604 s and 342 s, with full
  transcripts) and each failed loud with a genuine diagnosis — a peer that
  never serviced inbound frames so ND never completed, then a flood that ran
  before the guest was listening, both fixed — so the choreography has been
  exercised; what is missing is a trustworthy final green. **Next worker:
  establish why a short QEMU exit scores as a pass (start at `Outcome`
  classification in `tools/qemu`) before trusting any vertical's verdict on
  this machine, then re-run this one and mark it `[x]`.**

## 5. Observability: `info:` / `state:` / `stats:` for every interface

Every network interface `netstack` manages is a first-class resource,
addressable through the closed `lib/resref` namespaces (`net:` is already
registered) and observable through the System Information API (§16.6) —
never a `/proc` shape, never text scraping. The selector vocabulary follows
`plans/ALIAS.md` §6 exactly; this plan adds the network-specific members:

- **`net:<iface>`** — the interface itself (the device reference the admin
  surface and `configure` name). Interface names are stable, admin-chosen
  aliases (`wan`, `lan0`), never discovery-order names (ALIAS §6 rule).
- **`info:net/<iface>/…`** — static facts: `driver`, `mac`, `mtu`,
  `capabilities` (the negotiated offload set), `kind`
  (`ethernet`/`bond`/`loopback`/…), and for a bond `members`. MAC
  addresses are sensitive (ALIAS §6.2): `info:net` queries sit behind
  `CAP_SYSINFO_HW`-class policy review, not open by default.
- **`state:net/<iface>/…`** — current mutable state: `link` (up/down,
  negotiated speed/duplex), `address` (the bound v4/v6 address set +
  SLAAC state), `routes`, and for a bond `active-member` and per-member
  health. `state:` reads are capability-checked; state **changes** go
  through the typed `CAP_NET_ADMIN` admin IPC only (ALIAS §6.4 — never a
  writable pseudo-file).
- **`stats:net/<iface>/…`** — live metrics with the ALIAS §6.3 metadata
  (kind/unit/source/time/window/reset_behavior): `rx.bytes`, `rx.packets`,
  `tx.bytes`, `tx.packets`, `rx.errors`, `tx.errors`, `rx.dropped`,
  `tx.dropped`, and windowed rates (`rx.pps?window=1s`, `tx.bps?…`);
  plus the stack-wide defence counters under `stats:net/stack/…` so a DoS
  in progress is *visible* — the packet-path aggregates (reassembly
  evictions, rate-limited ICMP suppressions), summed across interfaces,
  and the TCP connection-defence totals (SYN-cookie activations, backlog
  starts/expiries, accepts, accept-queue overflow, resets), read from the
  stack's one socket table. Landed as N8a and N16a respectively.

Mechanically: `netstack` answers typed `sysinfo` queries (new
`SysinfoQueryId` members added under the §16.6 ABI discipline, versioned +
hashed); `lib/procinfo`'s userspace `info:`/`stats:` resolver maps the
parsed `ResourceRef` onto those query ids exactly as it does for the
existing namespaces (§2.2 — one resolver, no second path). Unprivileged
callers may read their own sockets' counters; interface-wide and
stack-wide queries declare `CAP_SYSINFO_GLOBAL` (and `CAP_SYSINFO_HW`
where hardware identity is exposed). Landed in N3 (interface facts/state,
with the admin surface) and N8a (per-interface + packet-path counters via
`NET_INTERFACE_COUNTERS` → `stats:net/…`); the windowed rates and the
`ss`-class tooling that reads the same queries are N8b, N9 extends it for
bond members (§6.3), and the TCP connection-defence totals are N16a via
`NET_STACK_DEFENCE`.

## 6. Configuration: `/System/Settings/Network`, `configure net.*`, bonding and failover

Interface configuration is deliberately boring: one declarative,
fail-closed configuration store plus one `sysctl`-shaped command surface.
No hidden state, no per-driver config files, no imperative boot scripts.

### 6.1 The configuration store — `/System/Settings/Network/network.conf`

- One document, engine-parsed like `system.conf`: the grammar, closed key
  registry, typed value sets, bounded fail-closed parser, and canonical
  render live in **`lib/netconfig`** (a `lib/sysconfig`-shaped sibling
  engine, `no_std` + `alloc`, host-unit-tested), shared by the writer
  (`configure`, the installer) and the one reader (`netstack` at start
  and on typed reload) so producer and consumer can never diverge (§2.2).
- The file fully describes every managed interface. Key shape is
  `<iface>.<key>` under a closed per-interface registry:
  - identity/binding: `<iface>.match.mac` / `<iface>.match.node` (bind an
    alias to hardware by stable identity, never discovery order),
    `<iface>.kind` (`ethernet` | `bond` | `loopback`; future link kinds
    extend the closed set in place, §2.13);
  - addressing: `<iface>.ipv4.method` (`static` | `dhcp` | `disabled`;
    `dhcp` leases the address over RFC 2131 DHCPv4, `plans/DHCP.md`),
    `<iface>.ipv4.address`/`gateway`, `<iface>.ipv6.method` (`slaac` |
    `static` | `dhcp` | `disabled`; `dhcp` leases an address over RFC 8415
    stateful DHCPv6, `plans/DHCP.md`), `<iface>.ipv6.address`/`gateway`,
    `<iface>.mtu`;
  - bonding (see 6.3): `<iface>.bond.members`, `<iface>.bond.mode`
    (`active-backup` | `balance`), `<iface>.bond.monitor-interval`,
    `<iface>.bond.primary`.
- Parse failures reject the whole document with a typed, line-numbered
  error and leave the running configuration untouched (fail closed,
  §5.4); a malformed file never yields a half-configured stack. An
  absent file means "no managed interfaces beyond loopback", not an
  error.
- Writes go through the secured VFS under the caller's kernel-attested
  identity; the per-inode policy on `/System/Settings` decides who may
  write (the `configure` precedent — no new capability for the file
  itself). Applying it to the live stack is the `CAP_NET_ADMIN` admin
  IPC: `netstack` re-reads, diffs, and applies atomically per interface,
  auditing each change (§19.4).

### 6.2 `configure net.*` — stack-wide knobs

Stack-wide IP configuration joins the existing `configure` command's
closed key registry (`lib/sysconfig`, `system.conf`) under a `net.*`
tree, exactly like `os.*`:

- `net.ipv4.enabled` (`true`|`false`), `net.ipv6.enabled`
  (`true`|`false`) — a disabled family binds no addresses, answers no
  packets, and refuses family-specific socket creation with a typed
  error (fail closed, not silent drop).
- `net.ipv6.privacy` (`true`|`false`) — RFC 8981 temporary addresses.
- `net.tcp.syncookies` (`auto`|`always`) — `auto` (bounded queue,
  cookies on overflow) is the default; there is deliberately **no**
  `off`: an unbounded or undefended SYN queue is a §2.17 regression,
  not a configuration.
- `net.tcp.keepalive` (`true`|`false`) — RFC 9293 §3.8.4 TCP keepalive
  probing on idle connections; off by default (RFC 1122 §4.2.3.6). When
  on, both actively-opened and accepted connections probe an idle peer
  and are torn down if it stops answering (N12).
- `net.tcp.ecn` (`true`|`false`) — RFC 3168 Explicit Congestion
  Notification negotiation; off by default (connections are Not-ECT).
  When on, both actively-opened and accepted connections offer ECN in
  the handshake and, once negotiated, mark eligible segments ECT(0) and
  react to a CE mark as a congestion signal instead of forcing a drop
  (N13).
- Per-interface settings live in `network.conf` (6.1), never in
  `system.conf`; `configure net.<key> <value>` edits the stack-wide
  registry, and `configure` grows no interface sub-grammar — interface
  changes are edits to `network.conf` plus the typed admin reload.
  Both stores surface as `state:net/…` reads (§5).

### 6.3 Bonding and failover

Link aggregation is a stack construct, not a driver feature: a **bond**
is a virtual interface `netstack` composes over two or more member NICs,
so any driver serving the frame-ring seam participates with zero driver
changes (§17.4 — the seam is the contract).

- A bond owns the addresses, neighbour caches, and routes; members carry
  no addresses of their own and refuse direct address assignment while
  enrolled (typed error). Sockets and the routing table see one
  interface; member fan-out is internal to the interface table.
- **Modes (closed set):** `active-backup` — one transmitting member,
  ordered failover to the next healthy member (the failover-interface
  requirement is this mode with a declared `primary`); `balance` —
  flow-hashed transmit spread (one flow stays on one member, so TCP
  never reorders across links). LACP/802.3ad is a future in-place
  extension of the mode set, not speculated here (§2.4).
- **Health and failover:** member health is link-state driven (the
  `DeviceFacts` link report over the ring seam) plus the configured
  `monitor-interval` one-shot probe timer (§2.23 — timer-armed, never
  polled). Failover re-targets transmit within one monitor interval,
  emits gratuitous ARP / unsolicited NA so peers re-learn the path, and
  is audited (§19.4). Failback to a recovered `primary` is deliberate
  (configured), never flapping.
- Bond state is fully observable: `info:net/<bond>/members`,
  `state:net/<bond>/active-member`, per-member `state:`/`stats:` remain
  addressable (§5), so a dead member is a visible, audited fact.
- Landed as **N9** (§4) — after TCP and offloads, because failover
  correctness is asserted *through* live TCP flows in the vertical.

## 7. Why this survives senior review

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

## 8. Tests, docs, and gate (binding)

- Every increment lands its unit/property/fuzz/QEMU tests in the same
  change (§7); every fuzz harness registers with `cargo xtask fuzz`;
  adversarial corpora enter the regression corpus (§19.6).
- Every increment updates its rustdoc + `docs/src/` pages and this
  plan's status marks in the same change (§13) — status only, no
  landing narrative.
- Every increment ends with the full §2.15 gate: `cargo fmt --all`,
  `cargo xtask ci` (once), `cargo xtask fuzz --secs 5`, and
  `tools/ci/soak.sh both --secs 20`, quoted in the completion report.

## 9. What this plan explicitly does *not* do

- No DNS resolver, DHCP client/server, NTP, or HTTP library — future
  consumers of the socket ABI, each its own plan. (DHCP is now
  `plans/DHCP.md`; the DNS stub resolver is now `plans/DNS.md`, whose DNS1
  pure `lib/net::dns` engine has landed.)
- No firewall/NAT/forwarding policy engine (the routing table forwards
  nothing between interfaces in this plan; TAIRiX is a host, not a
  router, until a dedicated plan says otherwise).
- No TLS (already curated under `lib/crypto`/§16.4; it fronts sockets,
  it is not part of the stack).
- No Wi-Fi/802.11, no non-Ethernet link layers — new drivers serve the
  same seam later, as the Pi 4B's GENET MAC already does (N14).
- No kernel-resident fast path: if profiling ever motivates one, that
  is a design conflict to raise (§15.7), not a quiet migration.
