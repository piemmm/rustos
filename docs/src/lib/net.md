# `tairix-net`

`lib/net` is the TAIRiX network protocol engine: the single, pure,
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
`fuzz_net_udp`, `fuzz_net_tcp`, `fuzz_net_igmp`, `fuzz_net_mld`,
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
caller reassembles first and walks the reassembled payload again. On the
emit side, `fragment` plans RFC 8200 §4.5 source fragmentation (the only
entity that may fragment an IPv6 datagram is its source — routers never
do): given the fragmentable payload and the path MTU it returns pieces
that are contiguous, 8-byte-aligned (bar the last), and provably cover
the payload exactly once, and `write_fragment_header` serialises each
piece's 8-byte Fragment extension header. It fails closed below the
1280-byte floor and beyond the 13-bit offset field.

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

IPv6 can be administratively disabled per interface (`net.ipv6.enabled`,
distinct from the RFC 4862 §5.4.5 link-local-DAD-failure disable): a
disabled interface forms no link-local at bring-up, refuses static
assignment, and `set_ipv6_enabled` toggles it at runtime — flushing every
IPv6 address and halting Router Solicitation on disable, re-forming the
link-local on enable. `Stack` mirrors it for IPv4 (`ipv4_enabled` /
`set_ipv4_enabled`, dropping the static assignment and routes on disable)
and drops all inbound frames of a disabled family before parsing (so an
inbound RA cannot SLAAC-configure a disabled interface), so the family
binds no address and answers nothing. This is the enforcement the
`netstack` service applies from the `net.*` policy.

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

The engine is stateless for the transport protocols: a validated inbound
UDP datagram surfaces as `StackEvent::UdpDatagram` and a checksum-verified
inbound TCP segment as `StackEvent::TcpSegment` (the raw segment bytes plus
its addressing context), leaving demultiplexing to a socket — and, for TCP,
the per-connection `tcp::conn::Tcb` — to the `netstack` service. Origination
is the mirror: `Stack::send_datagram` (above) and `Stack::send_tcp` select
the source address, resolve the next hop, fold the mandatory pseudo-header
checksum (via `udp::write` / `tcp::write`), and IP-wrap the message. TCP is
always unicast, so `send_tcp` refuses a multicast/broadcast/unspecified
destination (`SendError::NotUnicast`).

### `udp` — the dual-stack UDP codec

One parse/emit core (RFC 768) folding the family-appropriate
pseudo-header checksum through `checksum`, so IPv4 and IPv6 are not two
shadowed paths. The checksum discipline differs by family deliberately:
IPv4 accepts a zero (uncomputed) checksum but always emits one; IPv6
requires it (RFC 8200 §8.1). Emit substitutes `0xFFFF` for a computed
zero so it is never read as "no checksum". Every decode is total,
bounded, and fail-closed. `Stack::send_datagram` originates a datagram
to a **unicast** peer (resolving the next hop, parking on ARP/ND) or a
**multicast** group (straight to the group MAC with a link-local scope —
TTL/hop-limit 1 — needing no route and no membership); an oversize
datagram is fragmented on emit for either family — IPv4 (RFC 791) and
IPv6 source fragmentation (RFC 8200 §4.5) alike, against the path MTU
(unicast) or link MTU (multicast) — and refused as `SendError::TooLarge`
only when it cannot be fragmented at all. The limited broadcast and the
unspecified address are refused (`SendError::NotUnicast`).

### `igmp`, `mld` — multicast group-membership message codecs

The IPv4 (IGMPv2, RFC 2236) and IPv6 (MLDv2, RFC 3810) membership
message framings. `igmp` is the eight-byte query/report/leave message
carried in IP protocol 2, checksum-sealed and total (an over-length
IGMPv3 query is read through its v2 fields, as a v2 host must). `mld`
decodes Multicast Listener Queries (both MLDv1 and MLDv2 lengths, with
the RFC 3810 §5.1.3 floating Maximum Response Code) and encodes Version
2 Multicast Listener Reports; it carries no report *decoder*, because
MLDv2 has no report suppression and a host never acts on another's
report.

### `mcast` — the host membership engine

One family-generic join/leave/query state machine (`Membership<P>`)
driven by two `McastProtocol` providers (`Igmp`, `Mld`) — the same
"one core, two providers" shape as `neigh`. It reference-counts joins,
retransmits unsolicited state-change reports `ROBUSTNESS` times, answers
queries after a jittered delay (seeded from the interface MAC so hosts
desynchronise — non-security jitter, not a CSPRNG draw), suppresses a
pending response on hearing another host's report where the protocol
does (IGMPv2, not MLDv2), and never reports the all-hosts control
groups. The table is bounded and joins fail closed at capacity. It
emits family-neutral `MembershipReport`s; the `stack` maps each to the
family's wire message and sends it with a Router Alert (IPv4 option /
IPv6 Hop-by-Hop, TTL/hop-limit 1). The `stack` joins each address's
solicited-node group (formalising ND's listening) and the all-systems
group, and filters the receive path by membership; `join_multicast` /
`leave_multicast` expose explicit application membership.

### `rate` — the tickless windowed throughput meter

`RateMeter` turns an interface's monotonic byte/packet counters into the
live rates (`stats:net/<iface>/rx.pps`, `tx.bps`, …) an operator reads to
see a link's load or a flood in progress. It is pure and integer-only,
like everything else here: it holds no clock and does no I/O — the caller
feeds explicit monotonic time and the live counters. It is **tickless by
construction**: it keeps a small bounded ring of coalesced counter
snapshots that the service records opportunistically whenever it wakes for
other work, so a quiet interface costs nothing and no timer is armed
merely to measure a rate. A read computes the average from the live
counters and the retained snapshot nearest the requested window, and
reports the window that *actually* elapsed — never inventing coverage the
history does not have (a just-created or long-idle interface reports a
shorter, possibly zero, window). Bit rates multiply bytes by eight and
the arithmetic widens through `u128` so it never overflows or wraps; a
counter that appears to move backwards saturates its delta to zero. The
ring depth and sampling gap are a fixed measurement *resolution*, not a
per-device capacity. `netstack` owns one meter per interface and answers
the `NET_INTERFACE_RATES` broker read from it.

### `bond` — the link-aggregation decision core

`Bond` is the pure decision core for link aggregation (`plans/NETWORK.md`
§6.3): a bond is a virtual interface `netstack` composes over two or more
member NICs, so any driver serving the frame-ring seam participates with
zero driver changes — aggregation is a stack construct, not a device
feature. Like `neigh` and `mcast`, it is pure and event-driven: it owns no
addresses, no routes, and no I/O; the caller feeds member link-state
reports and explicit `now`, and it answers *which member should carry a
transmit* and *when a peer must relearn the bond's location*.

Member health is link-state driven with a deliberate anti-flap discipline.
A member that loses its link becomes ineligible **immediately**, so the
transmit path fails over within one link-down report — never a polling
delay. A member that regains its link is admitted only after it has been
continuously up for one `monitor_interval` (the equivalent of a bonding
driver's up-delay); this is the "failback is deliberate, never flapping"
rule — a recovered `primary` reclaims the path one interval after it comes
back, not the instant a flapping link reports up. The monitor is tickless:
`next_deadline` arms a one-shot only while a member is up but not yet
admitted, and is unarmed once the set is stable.

Two transmit policies form a closed set (LACP is a future in-place
extension, not speculated here). `active-backup` keeps one transmitting
member at a time with ordered failover to the next eligible member; a
declared `primary` makes it a deliberate failover interface. `balance`
spreads transmits across the eligible members by a family-agnostic
`flow_hash` over the 4-tuple, so one flow stays on one member (a TCP stream
never reorders across links) while that member stays eligible. Every
mutation returns the `BondEvent`s the composing interface acts on:
`PathChanged` (emit a gratuitous ARP / unsolicited NA so peers relearn the
path, and audit the change) and `WentDown` (the bond lost its last eligible
member; transmit now fails closed). The member set is bounded by
`MAX_BOND_MEMBERS`, and `transmit_member` returns `None` — fail closed —
whenever no member is eligible.

### `tcp` — the TCP segment codec and sequence arithmetic

The RFC 9293 wire layer: the fixed 20-byte header, the eight control
flags (`TcpFlags`, with named `SYN`/`ACK`/… bits, `contains`, and a
`|` combinator), and the recognised options — MSS, window scale,
timestamps (RFC 7323), SACK-permitted, and up to `MAX_SACK_BLOCKS`
selective-acknowledgement blocks (RFC 2018). `TcpSegment::parse`
verifies the mandatory pseudo-header checksum in both families (TCP has
no zero-checksum form, unlike UDP over IPv4), rejects a data offset
outside `5..=15` words, a header longer than the segment, or any
malformed option, and is total and bounded (a SACK count over the fixed
bound is refused rather than allocated for). `write` serialises a
`TcpSegmentMeta` with canonical, NOP-aligned option ordering and pads to
a 32-bit boundary.

`SeqNumber` is the checked modulo-2³² sequence-space type every window
and acknowledgement comparison uses: wrapping `add`/`sub`, unsigned
`distance_from`, the RFC 1982 windowed ordering (`lt`/`le`/`gt`/`ge`,
computed from the unsigned gap so no `u32`→`i32` reinterpretation is
needed), and `in_window` for the RFC 9293 §3.4 acceptance test. It
deliberately has **no** `Ord`/`PartialOrd`, so a linear comparison on a
cyclic value cannot compile.

### `tcp::conn` — the connection state machine

The RFC 9293 transmission control block (`Tcb`), built on the segment
codec and `SeqNumber`. It is pure and event-driven exactly as `neigh`
and `mcast` are: the caller feeds parsed inbound segments and explicit
`now`, drives the application side (`connect`/`listen`/`send`/`recv`/
`close`/`abort`), drains outbound segments through a `poll_transmit`
`emit` closure, fires timers with `advance`, and re-arms one one-shot
timer from `next_deadline` (never a poll loop). The initial sequence
number is a **caller-supplied CSPRNG draw** (§22): the engine generates
no randomness itself, so it is deterministic and replayable — the
property tests and the `fuzz_net_tcp` state-machine driver exercise the
exact code the live service runs.

It implements the full state machine — active, passive, and simultaneous
open; the complete teardown lattice through TIME-WAIT (2·MSL) — plus the
send and receive windows over `SeqNumber`, RFC 7323 window scaling and
timestamps with PAWS, RFC 2018 SACK generation from a bounded
out-of-order reassembly set, RFC 6298 retransmission (SRTT/RTTVAR/RTO)
with Karn's algorithm, RFC 6675 SACK-based selective loss recovery (a
bounded scoreboard drives `IsLost`/`SetPipe`/`NextSeg`, replacing
go-back-N when the peer negotiated SACK; go-back-N remains the fallback
after a retransmission timeout and when SACK is absent), fast retransmit on
three duplicate ACKs, zero-window persist probing, RFC 5961 in-window RST/SYN
handling with rate-limited challenge ACKs (so a hostile peer cannot
induce an ACK storm), delayed ACKs, and the RFC 9293 user timeout. Every
buffer and the reassembly set are capacity-bounded and fail closed;
addresses never enter the TCB (the caller folds the pseudo-header
checksum through `tcp::write`), so it is address-family agnostic. The
send path is bounded by both the peer's advertised window and the
congestion window (`tcp::cc`, below). The demultiplexing server-side
listener sits one level above it (`tcp::listen`, below). The connection engine is
driven end to end through the `Stack` demux/originate paths above by the
`netstack` `SocketService` stream sockets (N5c): the service owns one `Tcb`
per connection and turns `Connect`/`Send`/`Close` and inbound
`StackEvent::TcpSegment`s into segment egress and client-visible
`SocketStreamEvent`s.

### `tcp::cc` — pluggable congestion control

Congestion control is a policy, the same shape as the pluggable kernel
scheduler: the connection owns the sequence space and loss detection and
consults a `CongestionControl` object for the one value it does not own,
the congestion window `cwnd` (in bytes). `plan_segment` bounds every send
by `min(snd_wnd, cwnd)` — the peer's flow-control window *and* the
congestion window (RFC 5681 §4.1). Adding an algorithm is implementing the
trait and adding a `CongestionAlgorithm` variant; nothing else changes.

Two policies ship, held to one shared conformance suite (RFC 6928 initial
window, slow-start vs. congestion-avoidance growth rates, multiplicative
decrease on loss, collapse to one segment on timeout, monotonic growth
under a pure ACK stream):

- **CUBIC** (RFC 9438), the default: after a congestion event the window
  follows a cubic curve of the time since that event — concave as it
  approaches the pre-loss peak, convex as it probes beyond it — with a
  Reno-friendly floor so it is never *slower* than NewReno on a short-RTT
  path. The cubic term and its `K` are computed in exact integer
  fixed-point over an integer cube root, so the crate needs no floating
  point or libm (the charter's roll-your-own rule).
- **NewReno** (RFC 6582 / RFC 5681): classic AIMD — slow-start doubling
  below `ssthresh`, one-MSS-per-RTT additive increase above it, halve on
  loss.

The connection feeds the policy three signals: `on_ack` (new data
acknowledged — grow), `on_loss` (loss detected by duplicate/selective
ACKs — multiplicative decrease, applied once per loss window through the
RFC 6582 `recover` high-water mark so a burst cannot halve the window
repeatedly), and `on_rto` (a timeout — collapse to one segment and restart
slow start). During recovery the send rate is governed by the RFC 6675
`pipe` estimate against `cwnd`, not by window inflation.

### RFC 6675 SACK-based loss recovery

When the peer negotiated SACK, the connection retransmits selectively
rather than by go-back-N. A bounded scoreboard records the SACKed send
ranges (coalesced, capped, and clamped to the outstanding window, so a
reordering or hostile peer can never grow the state or inject ranges
outside the data in flight). From it the engine computes RFC 6675's three
functions: `IsLost` (a byte is lost once at least three discontiguous SACK
ranges — or more than `2·SMSS` bytes — lie above it), `SetPipe` (the
in-flight estimate that bounds transmission against `cwnd`), and `NextSeg`
(the next segment to send: a lost hole to retransmit, then fresh data,
then a single rescue retransmission per episode). A retransmission timeout
clears the scoreboard and falls back to go-back-N (RFC 6675 §5.1); future
selective acknowledgements rebuild it.

### `tcp::listen` — the demultiplexing listener and SYN-flood defence

`Tcb` models one connection; `Listener` models the *server* side of
connection establishment for one local port and sits above it. It
demultiplexes inbound segments by peer identity (the peer address/port the
TCB itself does not carry), holds a bounded backlog of half-open
(SYN-RECEIVED) handshakes with a timeout, and moves each completed
handshake onto a bounded accept queue that `accept` drains. Both queues are
fixed capacity: a completed handshake that finds the accept queue full is
refused with a RST rather than growing it, and a half-open connection whose
client never returns its ACK is expired by `advance` (which also retransmits
owed SYN-ACKs; `next_deadline` folds the one-shot timer). It is pure and
event-driven like the rest of the crate.

When the half-open backlog is full — exactly the SYN-flood condition — the
listener stops allocating state and answers further SYNs with **stateless
SYN cookies** (RFC 4987). The server ISN it returns is a keyed MAC over the
connection 4-tuple and a slowly-rotating counter, with a 3-bit MSS index and
the counter tick packed into the top bits, so the whole handshake can be
reconstructed from the client's returning ACK holding *no* per-connection
memory between the SYN and the ACK — a flood of spoofed SYNs therefore costs
nothing. The documented trade-off is option loss: a cookie carries only the
MSS, so a connection accepted via a cookie negotiates no window scaling,
SACK, or timestamps (cookies are the overflow path, not the default — while
the backlog has room a full-state half-open with options is kept instead).
The keyed MAC is an injected `CookieSecret` seam: the engine hand-rolls no
cryptography (the charter's rule), so the live `netstack` service backs it
with `lib/crypto` over a per-boot secret while the tests inject a
deterministic MAC. A returning ACK whose cookie was not minted by this
secret — or minted under an expired counter — is refused with a RST and
reconstructs nothing (fail closed). The `fuzz_net_tcp` listener driver
floods a bounded listener with hostile SYN/ACK/RST traffic and asserts no
panic, bounded half-open and accept queues, and that every emitted segment
parses.

## Hardware offload: the software path is the oracle

A network device may verify a receive frame's transport checksum for the
stack. The stack opts into that offload per interface and honours it per
frame, but the software fold stays the canonical implementation and the
conformance oracle — an offload is never load-bearing for security (trust
is in the *device* that carried the frame, never in the peer that sent
it).

The engine takes the device's per-frame report through
`Stack::on_frame_meta(frame, RxMeta, now)` (`on_frame` is the
no-offload wrapper). When `RxMeta::validated()` is passed **and** the
interface negotiated `NetOffloads::RX_CSUM_VALIDATED`, the UDP/TCP
decoders skip only the one's-complement *fold* (`ChecksumCheck` threaded
through `UdpDatagram::parse_with` / `TcpSegment::parse_with`); every other
validation — header lengths, the IPv6 mandatory-checksum rule, the
pseudo-header length bound, all protocol-state checks — runs exactly as on
the software path. A reassembled datagram is always software-verified,
because a per-frame device assurance cannot cover a transport checksum
spanning fragments. A `Validated` claim on an interface that did *not*
negotiate the offload is ignored (fold in software). The
`rx_checksum_offload_matches_the_software_path_byte_for_byte` test asserts
the offloaded output equals the software oracle byte-for-byte; the
transport carrying the per-frame descriptor and the driver→stack wiring
live in `tairix_abi::driver::net_ring::FrameOffload`, `drivers/network`,
and `netstack` respectively (`plans/NETWORK.md` N7a).

### Transmit checksum offload (TCP)

The symmetric transmit path lets a device that negotiated
`NetOffloads::TX_CSUM_TCP` finish a TCP segment's checksum. When the egress
interface advertised the offload and the segment is a single unfragmented
frame, `send_tcp` writes it through `ChecksumMode::Partial`: the checksum
field holds only the folded pseudo-header sum (`Checksum::partial`, the
uncomplemented fold Linux's `CHECKSUM_PARTIAL` leaves), and the frame
carries a `TxOffload::PartialChecksum { csum_start, csum_offset }`
descriptor whose offsets address the transport checksum within the
Ethernet frame (14 + IPv4 20 or IPv6 40, then the TCP header's 16-byte
checksum-field offset). The device folds the transport bytes and completes
the field — the fold the stack would otherwise have run. It is never
load-bearing: a fragmented datagram (only the first fragment carries the
transport header), an interface that did not negotiate the offload, or a
device that ignores the request all keep the complete software checksum
(`ChecksumMode::Full`, `TxOffload::None`).
`tx_partial_checksum_completed_matches_the_software_full_checksum` (codec
level) and `tcp_v4_tx_checksum_offload_matches_the_software_path` (engine
level) assert the partial-plus-completion result equals the software
full-checksum frame byte-for-byte; the ring descriptor
(`FrameOffload::TxChecksum`) and the `virtio_net` header mapping carry it
to the device (`plans/NETWORK.md` N7b-1).

UDP transmit-checksum offload is deliberately **not** done: UDP's
zero-checksum-transmitted-as-`0xFFFF` rule (RFC 768) is not expressed by
the virtio protocol-agnostic partial-checksum contract, which would let a
device emit an illegal zero checksum on an IPv6 datagram and silently
disable protection on the rare IPv4 datagram that folds to zero, so UDP
stays on the software path (`plans/NETWORK.md` N7b-2).

### Transmit segmentation offload (TSO)

A device that negotiated `NetOffloads::TX_SEGMENT_TCP` splits one over-size
TCP *super-segment* into MTU-sized packets on the wire. A connection whose
egress interface advertised it (`Stack::tso_max_payload` seeds
`TcpConfig::tso_max_payload`) batches fresh, never-retransmitted data at
the send frontier into a single segment up to that bound; retransmissions
and SACK recovery always stay per-MSS, so a lost super-segment recovers as
ordinary segments. `send_tcp` emits it as one IP packet — never
IP-fragmented, never MTU-refused — carrying `TxOffload::TcpSegment {
csum_start, csum_offset, gso_size, hdr_len, ipv6 }` and a **length-0**
pseudo-header partial checksum (`ChecksumMode::PartialGso`, matching
Linux's `CHECKSUM_PARTIAL` for GSO), so the device adds each split
segment's own length before folding. `TSO_MAX_PAYLOAD` bounds the one IP
packet to the 16-bit length field for either family.
`tcp_v4_tx_segmentation_offload_matches_the_software_path` splits the
super-segment as the device must and asserts it reproduces the per-MSS
software segments TCP-byte-for-byte. The ring descriptor
(`FrameOffload::TxSegment`) and the `virtio_net` GSO header carry it to the
device (`plans/NETWORK.md` N7b-2).

## Performance budget: the allocation-free data-plane hot path

The engine's receive and transmit fast paths allocate **nothing** on the
heap in steady state — the charter's first-class performance rule for a hot
path (§2.16), not an afterthought. This is a *budget*, enforced as a
regression: a per-packet allocation reintroduced on the send or receive
path fails the build.

The mechanism is a reused output. Every engine entry point
(`on_frame` / `on_frame_meta`, `send_datagram`, `send_echo_request`,
`send_tcp`, `advance`) takes a caller-owned `&mut StackOutput` scratch
rather than returning a freshly allocated one. On entry it recycles the
previous call's frame and payload byte buffers into an internal bounded
pool (`StackOutput::recycle_into`), then draws every buffer it needs — the
Ethernet frame it emits, the IP packet and upper-layer message it builds,
and the payload it delivers on a `StackEvent` — from that pool, returning
each transient buffer the moment its consumer has copied it. Once the pool
and the output vectors are warm, a receive or a transmit reuses that memory
and touches the allocator zero times. The `netstack` service holds one such
`StackOutput` and reuses it across every frame, so the property carries into
the live service, not just the engine in isolation.

`tests/hotpath_allocations.rs` proves it with a counting global allocator:
it warms two back-to-back stacks (resolving ARP so transmits no longer
park), then drives 512 UDP transmit + receive rounds through reused outputs
and asserts the allocation count over that window is exactly zero. The
budget covers the *data plane* — `send_datagram` / `send_tcp` transmit and
`on_frame` receive. Infrequent control-plane emissions (ARP/ND, ICMP
errors, IGMP/MLD reports) and the timer sweep `advance` (which gathers
transient action vectors from its sub-components) are outside this
steady-state budget by design: they are rare, not per-packet, and are not
where throughput lives.

Per-packet cost otherwise is linear in the frame length (one bounded copy
per protocol layer, no per-call temporaries and no growth once warm) and
independent of the number of connections, routes, or neighbours the stack
holds (those are indexed structures, not scanned per packet). End-to-end
throughput and latency over a real device are exercised by the guest-driven
QEMU verticals (`netstack_stream_qemu_*`, `netstack_ping_qemu_*`), the
realistic measurement environment; the allocation-free invariant above is
the portion of the budget that is deterministic and therefore machine-
independent, so it — not a machine-specific packets-per-second figure — is
the enforced regression guard.

## What lands next

The remaining `plans/NETWORK.md` increment evolves this crate in place:
multiqueue receive (N7c-2, deferred until a device presents more than one
receive queue). It is added with its callers, tests, and fuzz harnesses.
