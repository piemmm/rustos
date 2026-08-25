# tairix-net

TAIRiX network protocol engine (`lib/net`). Stability tier: **experimental**.

This crate is the single home of the wire protocols the user-space network
stack speaks (`plans/NETWORK.md`). It is pure and host-testable: no I/O, no
syscalls, no endpoints, no capability checks. The engine transforms
caller-owned byte slices and explicit monotonic time values, so the exact
code the live `netstack` service runs is the code the unit tests, property
tests, and fuzz harnesses (`fuzz_net_eth`, `fuzz_net_addr`,
`fuzz_net_ipv4`, `fuzz_net_ipv6`, `fuzz_net_icmp`, `fuzz_net_nd`,
`fuzz_net_stack`, `fuzz_net_udp`, `fuzz_net_tcp` (segment codec, the
connection state machine, and the listener + SYN-cookie driver),
`fuzz_net_igmp`, `fuzz_net_mld`, `fuzz_net_dhcp`, `fuzz_net_dhcpv6`,
`fuzz_net_dns`)
exercise.

## Contents

- `addr` — the dual-stack address vocabulary. IPv4 and IPv6 are peers,
  expressed as the `core::net` types (re-exported, never a drifting
  first-party copy), plus what `core::net` does not carry: RFC 4007 scope
  classification (`Ipv6Scope`, fail-closed on reserved multicast scopes)
  and the scope/zone pairing rules (`ScopedIpv6Addr` — a link-local
  address without a zone is unrepresentable). It also carries `Ecn`, the
  shared RFC 3168 §5 ECN codepoint the IPv4 and IPv6 headers both express.
- `checksum` — the one RFC 1071 Internet-checksum definition: a one-shot
  fold plus an incremental accumulator with byte-stream semantics and the
  IPv4 / IPv6 (RFC 8200 §8.1) pseudo-header seeds. Every checksummed
  protocol folds through this module; a second fold is forbidden
  duplication. `Checksum::partial` (the folded, *uncomplemented* sum) plus
  `ChecksumMode::Partial` let a transmit path leave only the pseudo-header
  sum for a device to complete — the transmit-checksum-offload seam
  (`TxOffload`, below).
- `eth` — Ethernet II framing (14-byte header; VLAN/802.3 are
  unrecognised `EtherType`s, dropped by the dispatcher).
- `arp` — ARP for IPv4 over Ethernet (RFC 826), the IPv4 provider of the
  neighbour-cache contract.
- `ipv4` — the IPv4 codec (RFC 791): options-tolerant parse (checksum
  verified per RFC 1122 §3.2.1.2, options surfaced opaquely), strict
  option-free emit, and emit-side fragmentation (`fragment` — DF
  honoured, 68-byte MTU floor, 8-byte-aligned chunks).
- `ipv6` — the IPv6 codec (RFC 8200): the fixed 40-byte header and the
  bounded extension-header chain walk (`walk`) with the RFC 8200
  unrecognised-header/option dispositions as typed rejections; a
  fragment header ends the walk for reassembly.
- `icmp` — ICMP and ICMPv6 over one shared machinery (`IcmpContext`
  carries the family difference): checksum-verified `IcmpMessage`,
  echo (`IcmpEcho`), errors (`IcmpError`, incl. the RFC 1191 v4
  packet-too-big mapping), the RFC 4443 §2.4(e) generation gate
  (`error_allowed`), and the §2.4(f) token-bucket `ErrorRateLimiter`.
- `nd` — Neighbour Discovery (RFC 4861): RS/RA/NS/NA/redirect codecs
  with hop-limit-255/code-0 enforcement and bounded options; host-side
  emit only (RS/NS/NA); apply-helpers that drive the one `neigh` table.
- `frag` — dual-stack fragment reassembly: overlap ⇒ whole-datagram
  drop (RFC 8900), per-source and global byte budgets with oldest-first
  eviction, capped datagram/fragment counts, timeout expiry reporting
  the first-fragment fact for the caller's ICMP Time Exceeded decision.
- `route` — one generic longest-prefix-match trie (`RoutingTable`,
  instantiated for v4/v6, pruning + arena reuse under churn), on-link
  determination, the bounded RFC 4861 `DefaultRouterList`, RFC 6724
  source-address selection (`select_source`), and the RFC 8201
  `PathMtuCache` (reduction-only, 1280 floor, aging).
- `neigh` — the provider-agnostic neighbour cache: one bounded RFC 4861
  §7.3.2 state machine (`Incomplete`/`Reachable`/`Stale`/`Delay`/`Probe`)
  that ARP drives for IPv4 and ND drives for IPv6. Pure and
  deterministic: methods take `now` explicitly, side effects are
  returned actions, and the caller re-arms its one-shot timer from
  `next_deadline` (event-driven, never polled). Bounded against cache
  poisoning: fixed capacity with LRU eviction of resolved entries only,
  and an unsolicited confirmation never creates state.
- `iface` — the per-interface address engine: static IPv4/IPv6
  assignment and RFC 4862 SLAAC (duplicate address detection, router
  solicitation scheduling, preferred/valid lifetimes with the
  §5.5.3(e) two-hour rule) over an injected 64-bit interface
  identifier (RFC 7217 derivation is the service layer's job).
- `stack` — the dual-stack host engine composing everything above:
  one `Stack` per interface takes frames and explicit `now` values and
  fills a caller-owned, **reused** `StackOutput` with frames plus typed
  events. Reusing that output across calls makes the receive and transmit
  data paths allocation-free in steady state — the engine recycles the
  previous call's frame and payload buffers through a bounded pool rather
  than freeing and reallocating them (a §2.16 budget the
  `hotpath_allocations` test enforces; `docs/src/lib/net.md`). The engine
  performs ARP/ND answering and resolution
  with a bounded pending queue, echo in/out, budgeted reassembly,
  rate-limited ICMP error generation, RA processing (SLAAC, default
  routers, on-link routes, MTU, timing adoption — all bounded),
  redirect validation against the destination's current first hop,
  UDP demux to typed events, and IGMPv2/MLDv2 multicast membership
  (auto-joining each address's solicited-node group and the all-systems
  group, filtering the receive path by membership, emitting reports
  with a Router Alert; `join_multicast` / `leave_multicast`). Emitted
  frames carry a per-frame `TxOffload`: `send_tcp` attaches
  `TxOffload::PartialChecksum` (and writes only the partial checksum) when
  the interface negotiated `NetOffloads::TX_CSUM_TCP` and the segment is a
  single unfragmented frame, so a device can complete the fold; and, when
  the interface negotiated `NetOffloads::TX_SEGMENT_TCP`, `send_tcp`
  attaches `TxOffload::TcpSegment` to one over-size TCP super-segment (a
  length-0-pseudo partial checksum, `ChecksumMode::PartialGso`) the device
  splits into MTU-sized packets (TSO); every other frame keeps its full
  software checksum (`TxOffload::None`). `Stack::tso_max_payload` reports
  the connection's super-segment bound. `send_tcp` also stamps the RFC 3168
  ECN codepoint the connection chose into the IPv4 TOS / IPv6 Traffic Class,
  and the receive path surfaces the codepoint on `StackEvent::TcpSegment`.
- `udp` — the dual-stack UDP codec (RFC 768): one parse/emit core over
  the family-appropriate pseudo-header checksum, IPv4-optional /
  IPv6-mandatory checksum discipline.
- `dhcp` — the pure DHCPv4 client (RFC 2131 / RFC 2132, `plans/DHCP.md`):
  the BOOTP/DHCP wire codec (`DhcpReply::parse` — total, bounded, fail
  closed, `xid`/`chaddr`-matched, option-overload aware; the single
  `write_message` encoder shared by DISCOVER / REQUEST / DECLINE / RELEASE)
  and the RFC 2131 §4.4 client state machine (`DhcpClient`): INIT →
  SELECTING → REQUESTING → BOUND → RENEWING → REBINDING with NAK / lease-
  expiry restart, RFC 2131 §4.1 randomised exponential backoff, and RFC
  2131 §4.4.5 T1/T2 renewal timers. Pure and event-driven like `neigh` /
  `mcast` (`poll`/`on_reply` take `now`, emit `Action`s, and re-arm from a
  tickless `next_deadline`); the transaction id and backoff jitter are
  caller-supplied CSPRNG draws (the `tcp::conn` `iss` precedent), so an
  interface obtains an address the way SLAAC does, not over a socket.
  `Stack` drives this client when an interface selects DHCPv4
  (`enable_dhcp`, the `<iface>.ipv4.method = dhcp` key): it polls the client
  from `advance`, folds its deadline into `next_deadline`, frames each send
  as a UDP(68→67)/IPv4/Ethernet broadcast (or a neighbour-resolved unicast
  to the leasing server for a renewal), and intercepts a received reply
  (UDP 67→68) in `on_ipv4` **before** the unicast-address filter — so a
  broadcast reply reaches the client with no address yet — applying the
  leased address/mask/route on `Configured` and withdrawing them on
  `Deconfigured`, each surfaced as a `StackEvent::DhcpLease*` the service
  audits (`plans/DHCP.md` D2).
- `dhcpv6` — the pure stateful DHCPv6 client (RFC 8415, `plans/DHCP.md`
  D4a), a sibling of `dhcp`, not a `cfg`-fork of it (DHCPv6 is a distinct
  protocol: UDP 546↔547, the `ff02::1:2` all-servers multicast, DUID-keyed
  leases, IA_NA/IAADDR bindings, a Solicit/Advertise/Request/Reply
  exchange). The message + option wire codec (`Dhcp6Reply::parse` — total,
  bounded, fail closed, transaction-id + echoed-Client-ID matched, walking
  the options nested in an IA_NA; the single `write_message` encoder shared
  by Solicit / Request / Renew / Rebind / Release / Decline) and the RFC
  8415 §18.2 client state machine (`Dhcp6Client`): Init → Soliciting →
  Requesting → Bound → Renewing → Rebinding with Release/Decline teardown
  and lease-expiry / NoBinding restart, the RFC 8415 §15 randomised RT
  retransmission (§7.6 IRT/MRT/MRC), and server-or-default T1/T2 renewal
  timers. Pure and event-driven like `dhcp` (`poll`/`on_reply` take `now`,
  emit `Action`s, re-arm from a tickless `next_deadline`); the transaction
  id and RT jitter are caller-supplied CSPRNG draws (the `tcp::conn` `iss`
  precedent). The client forms its own DUID-LL from the interface MAC.
  `Stack` drives this client when an interface selects DHCPv6
  (`enable_dhcp6`, the `<iface>.ipv6.method = dhcp` key): it enables IPv6 so
  the link-local it sources from forms, polls the client from `advance`,
  folds its deadline into `next_deadline`, frames each send as a
  UDP(546→547)/IPv6/Ethernet datagram to the `ff02::1:2` all-servers
  multicast (skipped until the link-local passes DAD, never an unspecified
  source), and intercepts a received reply (UDP 547→546) in `on_ipv6`
  **before** the destination filter. On `Configured` it assigns the leased
  IA_NA address as a host `/128` (on-link reachability comes from RAs) — and
  Declines + re-acquires it if it fails DAD (RFC 8415 §18.2.10.1); on
  `Deconfigured` it withdraws it, each surfaced as a
  `StackEvent::Dhcp6Lease*` the service audits (`plans/DHCP.md` D4b). The
  engine is host-tested and fuzzed.
- `dns` — the pure DNS stub resolver (RFC 1035 / RFC 5452, `plans/DNS.md`
  DNS1), a sibling of `dhcp`, not a protocol baked into a socket. The
  message codec: `Name` (a bounded, case-folded canonical wire encoding —
  `Name::encode` parses a dotted host name with the label/length rules, and
  the internal reader expands RFC 1035 §4.1.4 compression pointers with a
  strictly-backwards follow rule so a crafted pointer loop cannot hang the
  parser), `write_query` (one recursion-desired standard query), and
  `DnsResponse::parse` — total, bounded, fail closed: a response is accepted
  only when its id matches the outstanding query's CSPRNG-random id and its
  echoed question matches the queried name (case-insensitively), type, and
  class (the RFC 5452 §9 spoofing bound), and any CNAME chain in the answer
  is followed to the queried type, capped at `MAX_ADDRESSES`. The
  `DnsResolver` state machine sends a query to each configured recursive
  server in turn with randomised exponential-backoff retransmission and
  deterministic failover, finishing as Success / NoData / NonExistent
  (NXDOMAIN) / Timeout. Pure and event-driven like `dhcp` (`poll` /
  `on_response` take `now`, emit `Action`s, re-arm from a tickless
  `next_deadline`); the query id and retransmit jitter are caller-supplied
  CSPRNG draws (the `tcp::conn` `iss` precedent). The `DnsTransport` trait
  and `resolve(name, record_type, servers, transport, rng)` function are the
  one shared driver (`plans/DNS.md` DNS2) that runs the engine over a real
  datagram socket: `DnsTransport` is the object-safe I/O seam (`now`,
  `send(server, query)`, and `wait(deadline, buf) -> Wait::{Datagram,
  TimedOut}`, each fail-closed with a typed `Errno`) and `resolve` is the
  single send/wait/fold/retransmit/failover loop bounded by
  `MAX_MESSAGE_LEN` (512), which the socket client, the QEMU vertical, and
  the tests all share (no second orchestration). Host-tested and fuzzed.
  `Stack::dhcp_dns_servers()` surfaces the recursive DNS servers an
  interface's DHCP clients learned from their current leases (the IPv4
  lease's option-6 servers, then the IPv6 lease's option-23 servers),
  derived from each client's live lease so it tracks acquisition and
  withdrawal — the pure source the `netstack` service aggregates across every
  interface (with any static configuration) into the host's active resolver
  set, deduplicated and bounded by `MAX_RESOLVER_SERVERS`, served as the
  `ResolverServers` broker read and surfaced by the ungated
  `NET_RESOLVER_SERVERS` System Information query
  (`state:net/resolver/servers`, the resolv.conf analogue) — one source of
  truth for a resolver client and an operator alike (`plans/DNS.md` DNS2).
- `igmp`, `mld` — the IPv4 (IGMPv2, RFC 2236) and IPv6 (MLDv2,
  RFC 3810) multicast group-membership message codecs, total and
  fail-closed; `mld` decodes queries and encodes reports only (a host
  never acts on another's report — MLDv2 has no suppression).
- `mcast` — the family-generic host membership state machine
  (`Membership<P>` over the `Igmp` / `Mld` providers, the `neigh`
  "one core, two providers" shape): reference-counted join/leave,
  robustness retransmission, jittered query responses, bounded and
  fail-closed at capacity.
- `bond` — the pure link-aggregation decision core (`plans/NETWORK.md`
  §6.3): a family-agnostic bond state machine over member NICs, the
  `neigh`/`mcast` "one pure core, injected time" shape. Member health is
  link-state driven — a member fails out **immediately** on link-down and
  is readmitted only after one `monitor_interval` up-delay (deliberate
  failback, never flapping). `active-backup` runs one transmitting member
  with ordered failover (a declared `primary` reclaims the path); `balance`
  spreads flows across the eligible members by a `flow_hash` over the
  4-tuple, so a flow never reorders across links. Every mutation returns
  the `BondEvent`s the composing interface acts on (`PathChanged` ⇒
  gratuitous ARP / unsolicited NA + audit; `WentDown` ⇒ transmit fails
  closed); the member set is bounded and `transmit_member` fails closed to
  `None` when no member is eligible. The monitor is tickless
  (`next_deadline` arms only while a member awaits admission).
- `tcp` — the TCP segment codec (RFC 9293): the fixed header, the eight
  control flags (`TcpFlags`), the recognised options (MSS, window scale,
  timestamps, SACK-permitted, and up to `MAX_SACK_BLOCKS` SACK blocks),
  and the mandatory pseudo-header checksum (both families; no zero-
  checksum form). `SeqNumber` is the checked modulo-2³² sequence-space
  type — wrapping arithmetic and the RFC 1982 windowed ordering, with no
  total `Ord` so a linear comparison on a cyclic value cannot slip in.
  Total, bounded (fixed option and header ceilings), and fail-closed.
- `tcp::conn` — the RFC 9293 connection state machine (`Tcb`) built on the
  segment codec: pure and event-driven like `neigh`/`mcast` (methods take
  `now`, output is drained through an `emit` closure, timers re-arm from
  `next_deadline`). It carries active/passive/simultaneous open, teardown
  (incl. TIME-WAIT), the send/receive windows with RFC 7323 window scaling
  and timestamps (PAWS), RFC 2018 SACK generation, RFC 6675 SACK-based
  selective loss recovery (a bounded scoreboard drives IsLost/SetPipe/
  NextSeg, replacing go-back-N when the peer negotiated SACK; go-back-N is
  the fallback after an RTO and when SACK is absent), RFC 6298 retransmission
  with Karn's algorithm, fast retransmit on triple
  duplicate ACKs, zero-window persist probing, RFC 5961 in-window RST/SYN
  handling with rate-limited challenge ACKs, delayed ACKs, the RFC 9293
  user timeout, and RFC 9293 §3.8.4 keepalive probing of an idle connection
  (off by default per RFC 1122 §4.2.3.6; when enabled, an idle connection is
  probed with a zero-length `snd_nxt - 1` ACK and torn down after a bounded
  number of unanswered probes), and RFC 3168 Explicit Congestion Notification
  (off by default; when both ends negotiate it in the handshake, fresh data is
  marked ECT(0), a received CE mark is echoed with ECE until the peer answers
  with CWR, and an ECE-marked ACK reduces the window once per window and sets
  CWR on the next fresh data). The initial sequence number is a caller-supplied CSPRNG
  draw (§22) so the engine stays deterministic and replayable; every buffer
  and the reassembly set are bounded (fail closed, never attacker-sized).
  The send path is bounded by both the peer's advertised window and the
  congestion window from `tcp::cc`.
- `tcp::cc` — pluggable congestion control, the scheduler-policy precedent
  applied to TCP: a `CongestionControl` trait the connection consults for
  its send window, with RFC 9438 CUBIC (the default) and RFC 6582 NewReno
  siblings and a shared conformance suite both must pass. Windows are byte
  counts; the arithmetic (including CUBIC's cube root) is exact integer
  fixed-point, so the crate needs no floating point or libm. Loss (three
  duplicate ACKs) applies the multiplicative decrease once per window
  (RFC 6582 recover) and a timeout collapses to one segment; growth is slow
  start below `ssthresh` and the policy's increase above it. `on_ecn` (RFC 3168
  §6.1.2) reduces the window for an explicit congestion mark with no
  retransmission; both policies implement RFC 8511 Alternative Backoff with
  ECN (ABE), backing off in congestion avoidance with a larger `beta_ecn`
  (0.8 NewReno, 0.85 CUBIC) than on loss and keeping the loss reduction in
  slow start, each through its one reduction path (no second code path).
- `tcp::listen` — the demultiplexing server-side listener (`Listener`) that
  sits above `tcp::conn`. It demultiplexes inbound segments by peer, holds a
  bounded backlog of half-open (SYN-RECEIVED) handshakes with a timeout, and
  moves completed connections onto a bounded accept queue (`accept`). When the
  backlog is full — the SYN-flood condition — it answers further SYNs with
  **stateless RFC 4987 SYN cookies** instead of allocating state: the server
  ISN is a keyed MAC over the connection 4-tuple and a rotating counter, so
  the handshake is reconstructed from the client's returning ACK holding no
  per-connection memory (at the documented cost of the connection's window
  scale / SACK / timestamps options). The keyed MAC is an injected
  `CookieSecret` seam (the engine hand-rolls no crypto; `netstack` backs it
  with `lib/crypto`). Both queues are fixed capacity and fail closed: an
  exhausted accept queue refuses (RST) rather than growing, and a hostile
  ACK bearing an unminted cookie is refused with a RST. Pure and
  event-driven like the rest of the crate (`advance`/`next_deadline` drive
  half-open retransmit + expiry).

The remaining work evolves this crate in place: wiring `bond` into the
`netstack` interface table (composing members, gratuitous ARP/NA on
failover, config reload — `plans/NETWORK.md` N9b-3-2) and multiqueue
receive (N7c-2, deferred until a device presents more than one receive
queue). UDP transmit-checksum offload is deliberately not done — the
virtio partial-checksum contract cannot honour UDP's `0x0000`→`0xFFFF`
rule, so UDP stays on the software path (N7b-2).

## Security

Every decoder parses attacker-controlled bytes and is total (never
panics), bounded (fixed validation bounds, no attacker-sized
allocation), and fail-closed (a malformed input is rejected whole). The
stateful engines are bounded and budgeted so a hostile peer can neither
fill nor poison them. See `docs/src/lib/net.md` for the architecture and
the seam contract the `netstack` service builds on.

## `rxfilter` — the receive pre-filter

The classifier a NIC driver applies on its harvest path so a frame with no
possible local consumer never wakes the stack (`plans/NETWORK.md` N17d). It
matches on slow-changing L3 address state only — no ports, no group
memberships — and its bias is to **admit**: it is never load-bearing for
security, so anything it cannot parse confidently, and any policy that could
not name every local address, widens rather than drops.
