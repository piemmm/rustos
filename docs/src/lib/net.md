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

### `dhcp` — the DHCPv4 client engine

The pure DHCPv4 client (RFC 2131 / RFC 2132, `plans/DHCP.md`), driven the
way SLAAC is: an interface that is configured for DHCP obtains its address,
mask, routers, and lease timers from this engine rather than from a
userland socket client. A DHCP client must transmit from `0.0.0.0:68`
broadcast *before* any address exists, which the capability-gated,
route-checked socket surface correctly refuses; framing DHCP inside the
stack (which owns the interface's egress) needs no new socket surface and
grants no ambient authority.

The codec is the BOOTP fixed header plus the RFC 2132 option TLVs.
`DhcpReply::parse` surfaces only the fields a client acts on — message
type, `yiaddr`, server identifier, subnet mask, routers, DNS servers, and
the lease/T1/T2 times — and is total, bounded (a fixed option-region walk
and fixed-capacity `MAX_ADDRESSES` address lists — never an attacker-sized
allocation), and fail-closed: it rejects a wrong `op`/`htype`/`hlen`, a
missing or corrupt magic cookie, a `xid`/`chaddr` that does not match the
client's outstanding request (bounding off-path spoofing), or an options
field with no message type, and it honours RFC 2131 §4.1 option overload
(options carried in the `file`/`sname` fields). A single `write_message`
encoder over a `MessageSpec` produces every client message — DISCOVER,
the SELECTING and renew/rebind forms of REQUEST, DECLINE, and RELEASE —
so there is one wire definition, not five.

`DhcpClient` is the RFC 2131 §4.4 state machine, pure and event-driven like
`neigh`/`mcast`: INIT → SELECTING → REQUESTING → BOUND → RENEWING →
REBINDING, with NAK and lease-expiry restart. `poll(now, rng)` advances
retransmissions and the T1/T2/expiry transitions; `on_reply(now, reply)`
folds a server message; both return the `Action`s the interface layer
performs (send a framed message to a broadcast or unicast destination,
apply a `Lease`, or withdraw one on `Deconfigured`). Retransmission uses
RFC 2131 §4.1 randomised exponential backoff (4 s doubling to 64 s, ±1 s
jitter); the renewal timers default to T1 = lease/2 and T2 = lease·7⁄8
(RFC 2131 §4.4.5), honouring server-supplied option 58/59 values only when
they are internally consistent, and an infinite lease (`0xFFFF_FFFF`) arms
no renewal. The transaction id and the backoff jitter are caller-supplied
CSPRNG draws (the `tcp::conn` `iss` precedent), so the engine stays
deterministic and replayable and never generates randomness itself.
`next_deadline` is a folded one-shot, so a bound interface with a permanent
lease costs no timer wakeups.

`Stack` drives this client when an interface is configured for DHCPv4
(`Stack::enable_dhcp`, selected by the `<iface>.ipv4.method = dhcp` key,
`plans/DHCP.md` D2): it polls the client from `Stack::advance`, folds the
client's deadline into `Stack::next_deadline`, and frames each send action
as a UDP(68→67)/IPv4/Ethernet datagram on the owning link — a link-layer
broadcast to `255.255.255.255` for DISCOVER and the SELECTING/REBINDING
REQUESTs (no route or ARP, since the client may have neither yet), or a
neighbour-resolved unicast to the leasing server for a RENEWING REQUEST.
A received DHCP reply (UDP source 67 → destination 68) is intercepted in
`Stack::on_ipv4` *before* the unicast-address filter — so a broadcast reply
reaches the client while it still has no address — and never surfaces as an
ordinary datagram. On `Configured` the stack applies the leased address,
the subnet mask's prefix, and the default route through the leased router
(fail-safe: a router the server placed off the connected subnet is refused
by `set_ipv4_config`, and the address is applied alone); on `Deconfigured`
it withdraws the address and its routes. Each lease change is surfaced as a
`StackEvent::DhcpLeaseAcquired`/`DhcpLeaseLost` the service audits.

### `dhcpv6` — the DHCPv6 client engine

The pure stateful DHCPv6 client (RFC 8415, `plans/DHCP.md` D4a), a sibling
of `dhcp` — not a `cfg`-fork of it, because DHCPv6 is a distinct protocol
(UDP 546↔547, the `ff02::1:2` all-servers multicast, DUID-keyed leases,
IA_NA/IAADDR address bindings, a four-message Solicit/Advertise/Request/
Reply exchange). Like the DHCPv4 client it lives inside the stack rather
than as a userland socket client and grants no ambient authority: every
client message is framed as UDP(546→547)/IPv6/Ethernet to the all-servers
multicast, which a client can send before it has any global address.

The codec is the four-octet message header (type + 24-bit transaction id)
plus the RFC 8415 §21 option TLVs. `Dhcp6Reply::parse` walks the top-level
options and the options nested inside an IA_NA, surfacing only the fields a
client acts on — the Server Identifier DUID, the IA_NA's IAID / T1 / T2, the
leased IA Addresses with their preferred/valid lifetimes, the top-level and
IA-level Status Codes, and the DNS servers (RFC 3646). It is total, bounded
(a fixed option-region walk, fixed-capacity `MAX_ADDRESSES` lists, a DUID
capped at the RFC 8415 §11 128-octet maximum — never an attacker-sized
allocation), and fail-closed: it rejects a truncated header, a message type
that is not a server response, a transaction id that does not match, and —
bounding off-path spoofing — a missing or mismatched echoed Client
Identifier or an absent Server Identifier. A single `write_message` encoder
over a `MessageSpec` produces every client message (Solicit, Request, Renew,
Rebind, Release, Decline), so there is one wire definition, not six. The
client forms its own DUID-LL (`Duid::ll_ethernet`) from the interface MAC —
stable, needing no persisted timestamp.

`Dhcp6Client` is the RFC 8415 §18.2 state machine, pure and event-driven
like `neigh`/`mcast`: Init → Soliciting → Requesting → Bound → Renewing →
Rebinding, plus the Releasing and Declining teardown paths and lease-expiry
/ NoBinding restart. `poll(now, rng)` advances retransmissions and the
T1/T2/valid-lifetime transitions; `on_reply(now, reply, rng)` folds a server
message; both return the `Action`s the interface layer performs (send a
message to the all-servers multicast, apply a `Lease6`, or withdraw one on
`Deconfigured`). Retransmission uses the RFC 8415 §15 randomised RT algorithm
with the §7.6 per-message IRT/MRT/MRC parameters (a ±0.1 jitter, doubling up
to MRT, bounded by MRC for Request/Release/Decline and by T2 / the valid
lifetime for Renew / Rebind); the renewal timers honour server-supplied T1/T2
and otherwise default to T1 = ½·preferred and T2 = ⅘·preferred (clamped so
renew never lands after rebind), and an infinite lifetime (`0xFFFF_FFFF`)
arms no renewal. The transaction id and the RT jitter are caller-supplied
CSPRNG draws (the `tcp::conn` `iss` precedent), so the engine stays
deterministic and never generates randomness itself. `next_deadline` is a
folded one-shot, so a bound interface with a permanent lease — or an idle
client after a completed Release — costs no timer wakeups.

`Stack` drives this client when an interface is configured for DHCPv6
(`Stack::enable_dhcp6`, selected by the `<iface>.ipv6.method = dhcp` key,
`plans/DHCP.md` D4b): enabling it turns IPv6 on so the link-local the client
sources its messages from forms, polls the client from `Stack::advance`,
folds the client's deadline into `Stack::next_deadline`, and frames each
send action as a UDP(546→547)/IPv6/Ethernet datagram from the interface's
link-local to the `ff02::1:2` all-servers multicast at hop limit 1 (the
multicast MAC is derived directly, no neighbour resolution — and the send is
skipped, to be retried by the client's own timer, until the link-local has
completed DAD, never sourced from the unspecified address). A received
DHCPv6 reply (UDP source 547 → destination 546) is intercepted in
`Stack::on_ipv6` *before* the destination filter and never surfaces as an
ordinary datagram. On `Configured` the stack assigns the leased IA_NA
address as a host `/128` (DHCPv6 grants no on-link prefix — on-link
reachability comes from Router Advertisements), and if that address later
fails DAD it is Declined to the server and re-acquired (RFC 8415
§18.2.10.1); on `Deconfigured` (expiry, `NoBinding`, or a changed address on
renewal) it withdraws the leased address, leaving the link-local and any
SLAAC/static addresses intact. Each lease change is surfaced as a
`StackEvent::Dhcp6LeaseAcquired`/`Dhcp6LeaseLost` the service audits. The
engine remains host-tested and fuzzed (`fuzz_net_dhcpv6`).

### `dns` — the DNS stub resolver engine

The pure stub resolver (RFC 1035 / RFC 5452, `plans/DNS.md` DNS1): a client
that sends a recursion-desired query to a configured recursive server and
interprets the answer (RFC 1034 §5.3.1). It is a sibling of `dhcp` — pure,
`no_std`, allocation-bounded, driven by injected monotonic time and
caller-supplied CSPRNG values — not a protocol baked into a socket.

The wire vocabulary is `Name`: a domain name in its canonical wire encoding
(length-prefixed labels ended by the root label), ASCII-case-folded so two
names compare equal exactly when they are equal under RFC 4343.
`Name::encode` parses a dotted host name with the label rules (non-empty,
≤ 63 octets, printable-ASCII — a control byte, space, or non-ASCII byte is
rejected rather than encoded, since a resolver queries host names), bounded
by the 255-octet ceiling. The internal reader expands RFC 1035 §4.1.4
compression pointers, but every followed pointer must target an offset
*strictly before* the pointer itself, so the walk decreases monotonically
and a crafted pointer loop can never hang the parser; a reserved label type
or an over-length expansion fails closed.

`write_query` emits one standard query (the 12-byte header with the RD bit,
one question, no other records). `DnsResponse::parse` validates a response
datagram against the outstanding `QuerySpec` and is total, bounded, and
fail-closed: it is accepted only when its transaction id equals the query's
CSPRNG-random id *and* its single echoed question matches the queried name
(case-insensitively), type, and class — the RFC 5452 §9 acceptance test
that, with the random id, bounds off-path spoofing (source-port
randomisation, the other RFC 5452 defence, is the socket layer's job in
DNS2, not the engine's). Any structural error rejects the whole message.
Answer records are followed through a CNAME chain from the queried name to
collect matching-type (A / AAAA) addresses, capped at `MAX_ADDRESSES`; a
record of the wrong class, a CNAME target this stub does not itself pursue,
or a record whose RDATA length does not match its type is skipped rather
than trusted. The surfaced `min_ttl` is the minimum TTL across the records
used, so a caching caller knows how long the answer holds.

`DnsResolver` is the retry/failover state machine, event-driven exactly like
`DhcpClient`: `poll(now, rng)` starts the query and advances its
retransmission and failover timers, `on_response(now, bytes, rng)` folds a
datagram, and both return the `Action` the caller performs (`Send { query,
server }` or `Finished(Resolution)`); `next_deadline` is the folded tickless
one-shot. It tries each configured recursive server in turn with randomised
exponential-backoff retransmission (a fresh random id per server, the same
id across a server's retransmissions), and concludes as `Success` (an
address of the queried type), `NoData` (NoError with no such record),
`NonExistent` (NXDOMAIN, RFC 8020), or `Timeout` (no server answered within
the budget); a `ServFail`/`Refused`/truncated answer fails the current
server over to the next. A datagram that does not match the outstanding
query is ignored and the resolver keeps waiting.

The `DnsTransport` trait and the `resolve(name, record_type, servers,
transport, rng)` function are the one shared driver that runs that engine
over a real datagram socket (`plans/DNS.md` DNS2). `DnsTransport` is the
object-safe seam to the outside world — `now` (the monotonic clock the
resolver's deadlines read against), `send(server, query)`, and
`wait(deadline, buf)` returning `Wait::Datagram(len)` or `Wait::TimedOut` —
each failing closed with a typed `Errno`. `resolve` is the single "send /
wait / fold / retransmit / fail over" loop: it performs only the I/O the
engine's `Action`s ask for (the engine still owns the timers and failover),
drops unmatched or fail-closed transport-errored datagrams without inventing
an answer, and bounds every reception to `MAX_MESSAGE_LEN` (512, the classic
RFC 1035 UDP ceiling — a larger answer sets TC, which the engine treats as a
per-server soft failure). The live `netsock-v1` socket client, the QEMU
vertical, and the unit tests all drive this *same* loop (no second copy of
the orchestration). The engine and driver are host-tested and fuzzed
(`fuzz_net_dns`).

`Stack::dhcp_dns_servers()` is the pure source that feeds a real resolver
its servers (`plans/DNS.md` DNS2): it surfaces the recursive DNS servers an
interface's DHCP clients learned from their current leases — the IPv4
lease's option-6 servers first, then the IPv6 lease's option-23 servers —
derived from each client's *live* lease, so the set tracks acquisition and
withdrawal exactly (empty before a lease, empty once one is lost, never a
stale copy) and is bounded by the leases' fixed-capacity option lists. The
`netstack` service aggregates it across every managed interface (with any
statically configured servers) into the host's active resolver set,
deduplicated and bounded by `tairix_abi::net_ipc::MAX_RESOLVER_SERVERS`, and
serves it as the `ResolverServers` broker read. The System Information API
surfaces that same set as the ungated `NET_RESOLVER_SERVERS` query — the
resolv.conf-analogue `state:net/resolver/servers` reading — so a resolver
client and an operator see one source of truth (`plans/DNS.md` DNS2).

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

### `rxfilter` — the receive pre-filter

The classifier a NIC driver applies on its harvest path to decide, *before*
the network stack is woken, whether a frame could have a local consumer
(`plans/NETWORK.md` N17d). On a busy segment most broadcast traffic is
addressed to other hosts, and each such frame otherwise costs a stack wake,
a full protocol parse and a drop.

`RxClassifier` evaluates the `RxFilterPolicy` the stack published: an
ethertype allow-list (IPv4/IPv6/ARP), an ARP target-address match, and an
IPv4/IPv6 destination match against the interface's own addresses, its
subnet's directed broadcast, the limited broadcast, and any multicast. It
reads a destination through `ipv4::peek_destination` /
`ipv6::peek_destination` rather than a full parse, because folding the
header checksum twice per frame is exactly the wasted work a filter exists
to avoid.

It matches on **slow-changing L3 address state only** — no listening ports
and no group memberships. Per-socket state could fall behind a socket
opening and drop a frame someone wanted, for a share of the noise that does
not justify the risk; multicast is admitted wholesale because a device's own
group filter already sheds unjoined groups where it has one.

**Its bias is to admit.** It is never load-bearing for security: the stack
still validates every admitted frame, and a driver process already owns its
device and could drop whatever it liked, so refusing here grants nothing.
Anything it cannot parse with confidence is admitted, and a policy that
could not name every local address (`is_exhaustive` false) widens to admit
all unicast. `Stack::rx_filter_policy` assembles the policy beside the
addresses it describes, so the classifier and what it evaluates are never
derived twice.

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

`close` is the RFC 9293 §3.10.4 CLOSE, so it is a *half*-close: the FIN is
queued behind the buffered data and the connection keeps receiving through
FIN-WAIT-2 until the peer closes too, with `send_closed` reporting that the
send direction has ended. That is the guarantee the socket-level
`SocketRequest::Shutdown` (`plans/NETWORK.md` N15) is built on; `abort` is
the RFC 9293 ABORT that resets both directions at once.

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
induce an ACK storm), delayed ACKs, the RFC 9293 user timeout, and
RFC 9293 §3.8.4 keepalive probing. Keepalive is off by default (RFC 1122
§4.2.3.6); once enabled through `TcpConfig`, an *idle* established
connection — one with no unacknowledged or queued data, since data in
flight is already proven live by the retransmission timer — is probed
after the idle interval with a zero-length `snd_nxt - 1` ACK the peer is
obliged to acknowledge, and is aborted (with a RST) after a bounded number
of unanswered probes; any inbound segment or fresh data send re-arms the
idle timer. Every
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

The connection feeds the policy four signals: `on_ack` (new data
acknowledged — grow), `on_loss` (loss detected by duplicate/selective
ACKs — multiplicative decrease, applied once per loss window through the
RFC 6582 `recover` high-water mark so a burst cannot halve the window
repeatedly), `on_rto` (a timeout — collapse to one segment and restart
slow start), and `on_ecn` (an explicit congestion mark, RFC 3168 §6.1.2 —
a multiplicative decrease with no retransmission, since nothing was lost).
Both policies implement RFC 8511 Alternative Backoff with ECN (ABE): in
congestion avoidance an ECN mark from a shallow AQM signals *incipient*
congestion, not an overflowed buffer, so it backs off with a larger
(gentler) multiplicative-decrease factor than a loss — `beta_ecn = 0.8`
vs `beta_loss = 0.5` for NewReno, `0.85` vs `0.7` for CUBIC — which drains
the bottleneck without needlessly under-filling the path. ABE applies only
in congestion avoidance (`cwnd > ssthresh`); in slow start an ECN mark
keeps the standard loss reduction (RFC 8511 §3.1). The trait default is the
RFC 3168 baseline (react exactly as to a loss) so a minimal policy is
always correct; the shipped policies override it for ABE through the one
reduction path each already uses for loss. During recovery the send rate is
governed by the RFC 6675 `pipe` estimate against `cwnd`, not by window
inflation.

### RFC 3168 Explicit Congestion Notification

ECN lets a congested router *mark* an ECN-capable packet instead of
dropping it, so the sender slows down without a loss. It spans the IP and
TCP layers and is one shared codepoint vocabulary (`addr::Ecn`) both IP
families express — the IPv4 TOS byte's low two bits and the IPv6 Traffic
Class's low two bits — so the connection engine reasons about ECN without
knowing which family carried a packet. It is negotiated in the handshake
and off by default (`TcpConfig::enable_ecn`); a peer that did not opt in is
never sent ECN-capable packets, so enabling it can only add ECN where both
ends agree (RFC 3168 §6.1.1):

- **Negotiation.** An active open sends an ECN-setup SYN (both ECE and CWR
  set); a passive open that also enables ECN answers with a SYN-ACK
  carrying ECE alone. The connection is ECN-capable only when this exact
  exchange completes, and falls back to plain TCP otherwise.
- **Marking.** An ECN-capable connection stamps ECT(0) on its fresh data
  segments (never on control segments, retransmissions, or window probes,
  RFC 3168 §5.2/§6.1.6); `send_tcp` writes the codepoint into the IP
  header, and the receive path surfaces it on `StackEvent::TcpSegment`.
- **Receiver echo (§6.1.3).** On a Congestion-Experienced (CE) datagram
  the receiver sets ECE on every ACK until the peer answers with CWR.
- **Sender response (§6.1.2, RFC 8511 ABE).** An ECE-marked ACK triggers a
  window reduction (`cc.on_ecn`) — but no retransmission — at most once per
  window of data, and the next fresh data segment carries CWR to tell the
  peer the reduction happened. In congestion avoidance the reduction uses
  the RFC 8511 gentler `beta_ecn` factor (0.8 NewReno / 0.85 CUBIC) rather
  than the loss factor; in slow start it is identical to a loss.

The engine, its full data-path threading, the stack-wide `net.tcp.ecn`
operator toggle (delivered to `netstack` by `devmgr`, off by default), and a
live two-process QEMU vertical (`netstack_ecn_qemu_aarch64`) that asserts the
negotiation → ECT(0) → CE → ECE → CWR exchange on the wire are all complete.
The host peer verifies, over the real device, that a guest whose
`system.conf` set `net.tcp.ecn true` offers ECN in its SYN, marks its data
ECT(0), and sets CWR after the peer echoes ECE for an injected congestion
mark.

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
