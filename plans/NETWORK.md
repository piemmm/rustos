# NETWORK.md — Full IPv4 + IPv6 networking: the user-space network stack

This is the staged build plan for RustOS's complete dual-stack network
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
- `rustos-net` exists (`no_std`, `#![forbid(unsafe_code)]`, §6 README,
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
  (`rustos_abi::driver::net_ring`: validated `RingGeometry`,
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

### N4 — UDP + the socket ABI + multicast membership `[~]`
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
  `lib/abi/src/net.rs` (`rustos_abi::net`) is the pure, versioned,
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
  - `rustos_netstack::SocketService`: the origin (`ProcId`)-keyed socket
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
  - `rustos_rt::net` client wrappers (`socket`/`bind`/`connect`/`send`/
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
(`rustos_abi::driver::net_channel`, `netchan-v1`) is the versioned,
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
  parks on the device IRQ; the stack parks on the notify port. The
  single-address-space in-process caller parks with
  `VirtioNet::wait_for_device_event` between doorbells.
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
  emitted `netchan` node (`compatible = "rustos,netchan"`), reads its endpoint
  resource, and hands it to `netstack` `BindDriver` under a derived `netN`
  alias — each endpoint bound exactly once across generation bumps, fail-soft
  retry if the stack is not yet up.
- **The driver's §18.3 bind table** `rustos_drv_network_virtio_net::BIND_KEYS`
  (`HwMatchKey::virtio(1)`, exact-match `BIND_PRIORITY`): the discovery
  identity `devmgr`/the signed-manifest bind table is authored from, so a
  discovered virtio-net node resolves to this driver. Without it the driver
  process was undiscoverable; it lives in the `drivers/network/virtio_net`
  `lib` (the `virtio_input`/`virtio_blk` `BIND_KEYS` precedent) and survives
  the N4e scaffold removal (only `register` goes).
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
  `EchoRequestServed` event. `netpeer` gained a `PeerCampaign::V6LinkLocalOnly`
  mode targeting the guest's EUI-64 link-local from the pinned
  `rustos_test_netstack_wire::GUEST_MAC`; the QEMU `NetDevice` gained a
  `mac` field threaded through the shared `net_device_arg` on all three arches.

**Follow-up increments (not this change).**
- **N4e-riscv64 / N4e-x86_64**: build the driver-autoload-into-user-process
  production vertical on those arches (pioneering their autoload QEMU path),
  then the same two-process netstack vertical. Scope reality: riscv64 and
  x86_64 currently carry *none* of the autoload subsystem (no root mount, no
  unlock kthread, no `devmgr`, no `driver_spawn_loader`) — pioneering it is
  the `plans/WIRING.md` / `plans/ARCHSUPPORT.md` parity port, not a small
  extension. Foundation in place: the synthetic node-id bases the probes emit
  from now live in one shared, disjoint-by-construction, compile-time-guarded
  map (`kernel/rustos-kernel/src/hwtree_node_ids.rs`) — a new arch's NIC-probe
  region is claimed as the next index there, never a fresh literal, so the
  base-collision class that bit N4e-β cannot recur. Second foundation in
  place: the pure virtio-MMIO discovery observers
  (`observe_virtio_mmio_{block,input,network}_devices` + the shared
  interrupt-class core) now live in the arch-neutral
  `kernel/rustos-kernel/src/hwdiscovery` module, split out of `root_storage`
  (which retains only the drvhost-linked root-block *catalogue resolution*).
  Because `hwdiscovery` injects the enumerated bus through the frozen
  `lib/abi` seams and links no `driver_catalog` / `drvhost`, a riscv64 /
  x86_64 boot path reuses the *same* observers without pulling the
  driver-signing trust anchor onto those arches — so the arch discovery
  wiring is a thin caller (an injected `FdtDiscovery` + a per-slot arch IRQ
  resolver), not a copy of the walk (§2.2 / §2.21).
  Third foundation in place: the riscv64 boot path now **seeds the hardware
  tree**. `boot_riscv64::try_boot` runs the port's `FdtDiscovery` into the
  shared `kernel/rustos-kernel/src/boot_hwtree.rs` `CollectingHwNodeSink`
  (the one growable boot-tree sink, extracted from the aarch64 boot path so
  neither copies it, §2.2) and publishes it to `HW_TREE`, so the
  `hw_tree_read` / `hw_tree_wait` syscalls expose the riscv64 platform
  (root/memory/timer) inventory to user space. This is pure device-tree
  normalisation — no MMIO — so it is safe before the bootstrap-floor bus
  bring-up. Fourth foundation in place: **the riscv64 bootstrap-floor
  virtio-MMIO `DeviceID` probe is now wired.** `boot_riscv64::seed_hardware_tree`
  builds the MMIO bus from the discovered device tree
  (`rustos_drv_bus_mmio::virtio_mmio_bus_from_dtb`, mapped by the Sv39 identity
  window `boot` enables) and calls the *same* arch-neutral
  `hwdiscovery::observe_virtio_mmio_{block,input,network}_devices` observers
  aarch64 uses (§2.2 / §2.21), so the served tree now carries the probed,
  autoloadable Block/Input/Network nodes. The interrupt-driven input/network
  nodes carry their discovered PLIC line, resolved by the arch port's pure
  `rustos_arch_riscv64::fdt::plic_device_source` (reads a `virtio,mmio` node's
  single `interrupts` cell — the QEMU `virt` PLIC is `#interrupt-cells = <1>`
  — and bounds it against the discovered `riscv,ndev`; a discovered value,
  never a board constant, host-tested). Fifth foundation in place: **the
  in-kernel bootstrap-floor driver catalogue is now per-architecture.**
  `driver_catalog::IN_KERNEL_DRIVERS`/`IN_KERNEL_DRIVER_COUNT`/`EMMC2_PATH`
  and `build.rs`'s signed-manifest set are gated on `kernel_isa`: the floor is
  virtio-blk on every target, and the BCM2711 EMMC2 SD-host driver is floor
  **only on aarch64** (`rustos-drv-storage-emmc2` is now an aarch64-only
  runtime dependency). This fixed a live §2.20 defect — the Pi-only EMMC2
  driver was compiled into the x86_64 image (and would have been in riscv64's)
  — and lets the riscv64 autoload tranche join the drvhost-gated catalogue
  with a virtio-blk-only floor, never dragging a foreign-silicon driver into
  its image (host tests + all-three-arch builds green). What remains for the
  arch's autoload tranche, in order: (1) **extract the arch-neutral
  autoload/unlock orchestration** out of `aarch64/root_unlock.rs::finish_unlock`
  into a shared module both arches call over injected console/IRQ/block-unlock/
  `DriverProcessSpawn` seams (§2.21/§2.2), rewiring aarch64 in the same change;
  (2) the riscv64 root mount + unlock kthread + `devmgr` + `driver_spawn_loader`
  parity port over that shared orchestration (adding the `rustos-drvhost`
  runtime dep the signed *load* gate needs — only the pure `hwdiscovery`
  observers stay drvhost-free); then (3) the two-process netstack vertical
  (all QEMU-validated on CI hardware — the live-boot TCG verticals are too slow
  to confirm on a developer machine).
- **§18.5 scaffold removal** (once all three arches are two-process): delete the
  `register` shell in `drivers/network/virtio_net` (keeping `BIND_KEYS` +
  `VirtioNet`), `FixedSpawner`/`netstack_ping` in the support crate, and
  `VirtioNet::wait_for_device_event` (its only consumer).

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

### N9 — interface configuration, bonding, failover `[ ]`
- `lib/netconfig` (grammar, closed registry, fail-closed parse,
  canonical render; §6 README + stability tier; fuzz
  `fuzz_netconfig`); `/System/Settings/Network/network.conf` laid out
  by `tools/mkimage`/installer; `configure` gains the `net.*`
  stack-wide keys in `lib/sysconfig`'s registry (§6.2).
- `netstack`: config load/reload/apply (atomic per interface, audited),
  bond interface kind with `active-backup` + `balance` (§6.3), member
  health monitor, gratuitous ARP/NA on failover.
- `info:`/`state:`/`stats:` members for bonds and the remaining §5
  counter/rate queries; `SysinfoQueryId` additions; `lib/procinfo`
  resolver mapping.
- Tests: parser round-trip/adversarial corpora, bond failover QEMU
  vertical (kill a member mid-TCP-transfer, assert the flow survives
  within the monitor budget), config-reject-leaves-state tests,
  audited-refusal tests.
- Docs: `docs/src/userland/networking.md` configuration chapter;
  `userland/apps/configure` Help/ + README for `net.*`.

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
  plus the stack-wide defence counters (SYN-cookie activations,
  reassembly evictions, rate-limited ICMP suppressions) under
  `stats:net/stack/…` so a DoS in progress is *visible*.

Mechanically: `netstack` answers typed `sysinfo` queries (new
`SysinfoQueryId` members added under the §16.6 ABI discipline, versioned +
hashed); `lib/procinfo`'s userspace `info:`/`stats:` resolver maps the
parsed `ResourceRef` onto those query ids exactly as it does for the
existing namespaces (§2.2 — one resolver, no second path). Unprivileged
callers may read their own sockets' counters; interface-wide and
stack-wide queries declare `CAP_SYSINFO_GLOBAL` (and `CAP_SYSINFO_HW`
where hardware identity is exposed). Landed in N3 (interface facts/state,
with the admin surface), completed in N8 (counters, rates, `ss`-class
tooling reads the same queries), and extended by N9 for bond members
(§6.3).

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
  - addressing: `<iface>.ipv4.method` (`static` | `disabled`),
    `<iface>.ipv4.address`/`gateway`, `<iface>.ipv6.method` (`slaac` |
    `static` | `disabled`), `<iface>.ipv6.address`/`gateway`,
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
