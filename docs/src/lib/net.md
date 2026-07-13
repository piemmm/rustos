# `rustos-net`

`lib/net` is the RustOS network protocol engine: the single, pure,
host-testable definition of the wire protocols the user-space network
stack speaks. The staged build plan is `plans/NETWORK.md`; this page
describes what exists today (increments N1–N3a: the link and network
layers plus the interface/address and host engines) and the contract
the rest of the stack builds on.

## Design

The engine is `no_std`, `#![forbid(unsafe_code)]`, and deliberately free
of I/O: it never names a syscall, an IPC endpoint, or a device. Callers
own the frame buffers, and time enters as explicit monotonic
`Duration64` values. That makes the engine deterministic and replayable
— given the same inputs and time steps its outputs are byte-identical —
which is what lets the unit tests, the property tests, the fuzz
harnesses (`fuzz_net_eth`, `fuzz_net_addr`, `fuzz_net_ipv4`,
`fuzz_net_ipv6`, `fuzz_net_icmp`, `fuzz_net_nd`, `fuzz_net_stack`,
registered with `cargo xtask fuzz`), and the live `netstack` service
all exercise the *same* code.

Every decoder parses attacker-controlled bytes and is total (never
panics for any input), bounded (fixed validation bounds, no
attacker-sized allocation), and fail-closed (a malformed input is
rejected whole, never partially applied). Stateful engines (the
neighbour cache, the reassembler, the default-router list, the path-MTU
cache, the error rate limiter) all share one shape: methods take `now`,
side effects come back as returned values, expiry runs in `advance`,
and the caller re-arms a one-shot timer from `next_deadline` —
event-driven, never polled.

## Modules

### `addr` — the dual-stack address vocabulary

IPv4 and IPv6 are peers throughout the engine, expressed as the
`core::net` address types (`IpAddr`, `Ipv4Addr`, `Ipv6Addr`) re-exported
rather than shadowed by a first-party copy that would drift. On top of
them:

- `Ipv6Scope` — RFC 4007 scope classification. Multicast scopes follow
  the `scop` field; reserved/unassigned values classify to `None` so a
  caller drops rather than guesses. Link-local unicast (`fe80::/10`) and
  the loopback address are link-local scope; ULA is global *scope* per
  RFC 4193.
- `ScopedIpv6Addr` — an address paired with the zone (interface) index
  its scope requires. A non-global-scope address without a zone — the
  "link-local, but on which interface?" ambiguity — is unrepresentable:
  the constructor refuses it with a typed error.

### `checksum` — the Internet checksum, defined once

The RFC 1071 one's-complement fold: `internet_checksum` for contiguous
messages, and the incremental `Checksum` accumulator (byte-stream
semantics across arbitrary `push` splits) seeded by `ipv4_pseudo` /
`ipv6_pseudo` (RFC 8200 §8.1) for the checksums that span a
pseudo-header. Every checksummed protocol folds through this module.

### `eth`, `arp` — the link layer

Ethernet II framing, and ARP for IPv4-over-Ethernet (RFC 826) — the
IPv4 provider of the neighbour-cache contract. Each codec's parse
rejects truncated or malformed input by returning `None`, and an
accepted decode round-trips exactly through its matching encoder (a
fuzzed invariant).

### `ipv4` — options-tolerant parse, strict emit, fragmentation

`Ipv4Header::parse` accepts headers with options (`IHL > 5`), verifies
the header checksum over the full header (RFC 1122 §3.2.1.2), and
returns the header, the opaque options bytes (never interpreted — this
host neither sets nor honours IPv4 options), and the
total-length-delimited payload. The emit side writes only strict
option-free headers. `fragment` plans emit-side fragmentation: DF is
honoured, the MTU floor is RFC 791's 68 bytes, chunks are 8-byte
aligned, and the plan provably covers the payload exactly once.

### `ipv6` — the fixed header and the extension-header walk

`Ipv6Header` is the 40-byte RFC 8200 codec. `walk` traverses the
extension-header chain a receiving host must process — Hop-by-Hop (first
position only), Destination Options, Routing (segments-left ≠ 0 is
refused with a Parameter Problem: this host never forwards), and
Fragment — bounded by the fixed `MAX_EXT_HEADERS` validation cap.
Unrecognised options take their RFC 8200 §4.2 dispositions (skip /
drop / drop + Parameter Problem, multicast-suppressed for `11`-typed
options), expressed as typed `WalkRejection` values the caller turns
into rate-limited ICMPv6 errors. A fragment header ends the walk: the
caller reassembles first and walks the reassembled payload again.

### `icmp` — ICMP and ICMPv6 over one machinery

Both families share the `type | code | checksum | body` shape; the
family difference (type numbers, the ICMPv6 pseudo-header checksum)
lives once in `IcmpContext`. On top: `IcmpMessage` (the checksum-
verified split every typed decoder builds on), `IcmpEcho`
(request/reply as a typed `EchoKind` — one codec for ping in both
families), and `IcmpError` (destination unreachable, packet too big —
mapped to the RFC 1191 "fragmentation needed" wire form for v4 — time
exceeded, and parameter problem, with the invoking-packet excerpt
bounded per family). Error *generation* is defended: `error_allowed`
enforces the RFC 4443 §2.4(e) rules (no error about an error, none to
an ambiguous source, none for multicast except the sanctioned
exceptions) and `ErrorRateLimiter` is the RFC 4443 §2.4(f) token
bucket, so this host is never an amplification vector.

### `nd` — Neighbour Discovery

The five RFC 4861 messages (RS/RA/NS/NA/Redirect) as `NdMessage`, with
the validation rules applied at parse: hop limit 255 (a forwarded ND
packet is a spoofing attempt), code 0, minimum lengths, non-multicast
targets, solicited-NA-to-multicast refused, and options bounded by
`MAX_ND_OPTIONS` (unknown option types skipped per RFC 4861 §9;
malformed recognised options reject the message whole). The host emits
RS/NS/NA; Router Advertisements and Redirects are router output and
refuse to encode. The reachability state machine is *not* here — it is
the one `neigh::NeighborTable` — and `apply_neighbor_solicitation` /
`apply_neighbor_advertisement` / `apply_redirect` translate validated
messages into that table's calls. RA facts (router lifetime, MTU,
prefixes) are typed data the caller feeds to `route::DefaultRouterList`
and address configuration.

### `frag` — dual-stack fragment reassembly

One bounded `Reassembler` for both families (`FragKey` carries the
family-specific identity). Security posture per RFC 8900: overlapping
fragments — including exact duplicates — drop the whole datagram;
buffered bytes are budgeted per source and globally with oldest-first
eviction (the offending source's own datagrams first); datagram count
and fragments-per-datagram are capped; non-final fragments must be
8-byte multiples; contradictory final lengths drop the datagram.
Expiry (`advance`) reports whether the zero-offset fragment had
arrived, so the caller sends ICMP Time Exceeded only where RFC 4443
§3.2 permits. Property tests assert the budgets hold after every push
and that random splits reassemble exactly.

### `route` — longest-prefix match, routers, source selection, PMTU

- `RoutingTable<A, M>` — one generic binary trie instantiated for
  `Ipv4Addr` and `Ipv6Addr` through the `RouteAddr` bit view: `O(BITS)`
  lookup regardless of route count, node pruning + free-list reuse so
  route churn never grows the arena, and a property test against a
  naive oracle. A route's `next_hop: None` is on-link determination;
  `Prefix::new` refuses set host bits.
- `DefaultRouterList` — the bounded RFC 4861 default-router list:
  lifetimes from Router Advertisements, expiry via `advance`, selection
  preferring reachable routers then rotating round-robin (§6.3.6), and
  fail-closed refusal of new routers beyond capacity (RA floods).
- `select_source` — RFC 6724 source-address selection over the caller's
  candidate set (rules 1, 2, 3, 6 with the default policy-table labels,
  and 8; rule 5 is the caller's interface pre-filter).
- `PathMtuCache` — the bounded RFC 8201 per-destination path MTU:
  Packet Too Big reports only ever *reduce* the estimate, never below
  the 1280-byte floor; entries age back to the link MTU; LRU-bounded.

### `neigh` — the provider-agnostic neighbour cache

One bounded RFC 4861 §7.3.2 state machine
(`Incomplete`/`Reachable`/`Stale`/`Delay`/`Probe`) that ARP drives for
IPv4 and Neighbour Discovery drives for IPv6 — one table, two
providers, so the families cannot drift.

The table is pure: every method takes `now`, and side effects are
returned `NeighborAction` values (solicit multicast, probe unicast,
report unreachable) the caller performs. The caller owns the timer —
`next_deadline()` reports the earliest pending transition and
`advance(now)` performs everything that is due, so the service parks on
a one-shot timer and is woken by events, never polling.

Defences against cache poisoning and exhaustion are structural:

- Fixed capacity chosen at construction; when full, insertion evicts the
  least-recently-used *resolved* entry, and if every entry is
  mid-resolution the insert is refused (fail closed) so attacker-driven
  churn cannot evict live resolution state.
- A confirmation for an address with no entry is ignored: an unsolicited
  reply never creates cache state.
- Non-override confirmations carrying a different link-layer address
  only degrade a `Reachable` entry to `Stale` (RFC 4861 §7.2.5); the
  cached address is kept.

### `iface` — per-interface address configuration

The RFC 4862 address lifecycle for one interface, pure and
deterministic like everything else here: static IPv4/IPv6 assignment
plus SLAAC. Each IPv6 address walks tentative → preferred → deprecated
→ invalid; duplicate address detection sends the configured number of
neighbour solicitations and a defending NA or observed duplicate marks
the address `Duplicate` (never silently reused). Router solicitations
are scheduled with the RFC 4861 §6.3.7 initial cadence and stop on a
valid RA. Prefix-information options form addresses from an injected
64-bit interface identifier (the stable-privacy RFC 7217 derivation is
the service layer's job — the engine never hashes secrets), and
lifetime updates apply the RFC 4862 §5.5.3(e) two-hour rule so an
unauthenticated RA cannot instantly invalidate an address. Address
count is capacity-bounded and fail-closed.

### `stack` — the dual-stack host engine

`Stack` composes the modules above into one per-interface host engine
the `netstack` service drives: `receive_frame` takes an
attacker-controlled frame and explicit `now`, and returns bounded
output frames plus typed `StackEvent`s; `advance`/`next_deadline` keep
it event-driven (the caller parks on a one-shot timer, never polls).
It answers ARP requests and neighbour solicitations for owned
addresses only, resolves next hops through the one `neigh` table with
a bounded pending-transmit queue (fail-closed when full), reassembles
through the budgeted `frag` engine, generates ICMP errors only through
`error_allowed` + the rate limiter, applies Router Advertisements
(SLAAC via `iface`, default routers, on-link prefixes, MTU within the
link floor/ceiling, timing parameters — each bounded), and accepts a
Redirect only from the destination's current first hop. Echo requests
in either family are answered (reported as
`StackEvent::EchoRequestServed`, so the service layer observes the
inbound direction without a second decode path); `send_echo_request`
and `StackEvent::EchoReply` support the diagnostic path.

## What lands next

Later increments of `plans/NETWORK.md` evolve this crate in place:
`igmp`/`mld`, `udp`, and `tcp`, alongside the `netstack` service
(N3b/N3c) that wires these engines to real interfaces and deletes the
interim `userland/net/icmp` responder. None of that surface exists
yet; it is added with its callers, tests, and fuzz harnesses per
increment.
