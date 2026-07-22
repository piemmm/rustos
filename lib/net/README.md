# tairix-net

TAIRiX network protocol engine (`lib/net`). Stability tier: **experimental**.

This crate is the single home of the wire protocols the user-space network
stack speaks (`plans/NETWORK.md`). It is pure and host-testable: no I/O, no
syscalls, no endpoints, no capability checks. The engine transforms
caller-owned byte slices and explicit monotonic time values, so the exact
code the live `netstack` service runs is the code the unit tests, property
tests, and fuzz harnesses (`fuzz_net_eth`, `fuzz_net_addr`,
`fuzz_net_ipv4`, `fuzz_net_ipv6`, `fuzz_net_icmp`, `fuzz_net_nd`,
`fuzz_net_stack`, `fuzz_net_udp`, `fuzz_net_tcp`, `fuzz_net_igmp`,
`fuzz_net_mld`) exercise.

## Contents

- `addr` — the dual-stack address vocabulary. IPv4 and IPv6 are peers,
  expressed as the `core::net` types (re-exported, never a drifting
  first-party copy), plus what `core::net` does not carry: RFC 4007 scope
  classification (`Ipv6Scope`, fail-closed on reserved multicast scopes)
  and the scope/zone pairing rules (`ScopedIpv6Addr` — a link-local
  address without a zone is unrepresentable).
- `checksum` — the one RFC 1071 Internet-checksum definition: a one-shot
  fold plus an incremental accumulator with byte-stream semantics and the
  IPv4 / IPv6 (RFC 8200 §8.1) pseudo-header seeds. Every checksummed
  protocol folds through this module; a second fold is forbidden
  duplication.
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
  returns frames plus typed events — ARP/ND answering and resolution
  with a bounded pending queue, echo in/out, budgeted reassembly,
  rate-limited ICMP error generation, RA processing (SLAAC, default
  routers, on-link routes, MTU, timing adoption — all bounded),
  redirect validation against the destination's current first hop,
  UDP demux to typed events, and IGMPv2/MLDv2 multicast membership
  (auto-joining each address's solicited-node group and the all-systems
  group, filtering the receive path by membership, emitting reports
  with a Router Alert; `join_multicast` / `leave_multicast`).
- `udp` — the dual-stack UDP codec (RFC 768): one parse/emit core over
  the family-appropriate pseudo-header checksum, IPv4-optional /
  IPv6-mandatory checksum discipline.
- `igmp`, `mld` — the IPv4 (IGMPv2, RFC 2236) and IPv6 (MLDv2,
  RFC 3810) multicast group-membership message codecs, total and
  fail-closed; `mld` decodes queries and encodes reports only (a host
  never acts on another's report — MLDv2 has no suppression).
- `mcast` — the family-generic host membership state machine
  (`Membership<P>` over the `Igmp` / `Mld` providers, the `neigh`
  "one core, two providers" shape): reference-counted join/leave,
  robustness retransmission, jittered query responses, bounded and
  fail-closed at capacity.
- `tcp` — the TCP segment codec (RFC 9293): the fixed header, the eight
  control flags (`TcpFlags`), the recognised options (MSS, window scale,
  timestamps, SACK-permitted, and up to `MAX_SACK_BLOCKS` SACK blocks),
  and the mandatory pseudo-header checksum (both families; no zero-
  checksum form). `SeqNumber` is the checked modulo-2³² sequence-space
  type — wrapping arithmetic and the RFC 1982 windowed ordering, with no
  total `Ord` so a linear comparison on a cyclic value cannot slip in.
  Total, bounded (fixed option and header ceilings), and fail-closed. The
  connection state machine is a later increment (`plans/NETWORK.md` N5b).

Later increments evolve this crate in place with the TCP connection
state machine (`plans/NETWORK.md` N5b) built on this segment layer.

## Security

Every decoder parses attacker-controlled bytes and is total (never
panics), bounded (fixed validation bounds, no attacker-sized
allocation), and fail-closed (a malformed input is rejected whole). The
stateful engines are bounded and budgeted so a hostile peer can neither
fill nor poison them. See `docs/src/lib/net.md` for the architecture and
the seam contract the `netstack` service builds on.
