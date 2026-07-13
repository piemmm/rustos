# `rustos-net`

`lib/net` is the RustOS network protocol engine: the single, pure,
host-testable definition of the wire protocols the user-space network
stack speaks. The staged build plan is `plans/NETWORK.md`; this page
describes what exists today (increment N1) and the contract the rest of
the stack builds on.

## Design

The engine is `no_std`, `#![forbid(unsafe_code)]`, and deliberately free
of I/O: it never names a syscall, an IPC endpoint, or a device. Callers
own the frame buffers, and time enters as explicit monotonic
`Duration64` values. That makes the engine deterministic and replayable
— given the same inputs and time steps its outputs are byte-identical —
which is what lets the unit tests, the fuzz harnesses (`fuzz_net_eth`,
`fuzz_net_addr`, registered with `cargo xtask fuzz`), and the live
`netstack` service all exercise the *same* code.

Every decoder parses attacker-controlled bytes and is total (never
panics for any input), bounded (no attacker-sized allocation), and
fail-closed (a malformed input is rejected whole, never partially
applied).

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
`ipv6_pseudo` (RFC 8200 §8.1) for the transport checksums that span a
pseudo-header. Every checksummed protocol folds through this module.

### `eth`, `arp`, `ipv4`, `icmp` — the wire codecs

Ethernet II framing; ARP for IPv4-over-Ethernet (RFC 826); the
option-free IPv4 header codec (RFC 791), which verifies the received
header checksum (RFC 1122 §3.2.1.2); and ICMP echo (RFC 792). Each
codec's parse rejects truncated, malformed, or checksum-invalid input by
returning `None`, and an accepted decode round-trips exactly through its
matching encoder (a fuzzed invariant). The `userland/net/icmp` responder
composes these re-exported codecs; it carries no parser of its own.

### `neigh` — the provider-agnostic neighbour cache

One bounded RFC 4861 §7.3.2 state machine
(`Incomplete`/`Reachable`/`Stale`/`Delay`/`Probe`) that ARP drives for
IPv4 today and Neighbour Discovery drives when IPv6 lands — one table,
two providers, so the families cannot drift.

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

## What lands next

Later increments of `plans/NETWORK.md` evolve this crate in place:
`ipv6` + extension-header handling, `icmpv6`/`nd`, fragment reassembly
(`frag`), routing (`route`), `igmp`/`mld`, `udp`, and `tcp`. None of
that surface exists yet; it is added with its callers, tests, and fuzz
harnesses per increment.
