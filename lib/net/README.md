# rustos-net

RustOS network protocol engine (`lib/net`). Stability tier: **experimental**.

This crate is the single home of the wire protocols the user-space network
stack speaks (`plans/NETWORK.md`). It is pure and host-testable: no I/O, no
syscalls, no endpoints, no capability checks. The engine transforms
caller-owned byte slices and explicit monotonic time values, so the exact
code the live `netstack` service runs is the code the unit tests, property
tests, and fuzz harnesses (`fuzz_net_eth`, `fuzz_net_addr`) exercise.

## Contents (NETWORK.md increment N1)

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
- `ipv4` — the option-free IPv4 header codec (RFC 791), verifying the
  header checksum on parse (RFC 1122 §3.2.1.2).
- `icmp` — ICMP echo (RFC 792), checksum-verified.
- `neigh` — the provider-agnostic neighbour cache: one bounded RFC 4861
  §7.3.2 state machine (`Incomplete`/`Reachable`/`Stale`/`Delay`/`Probe`)
  that ARP drives today and Neighbour Discovery drives when IPv6 lands.
  Pure and deterministic: methods take `now` explicitly, side effects are
  returned actions, and the caller re-arms its one-shot timer from
  `next_deadline` (event-driven, never polled). Bounded against cache
  poisoning: fixed capacity with LRU eviction of resolved entries only,
  and an unsolicited confirmation never creates state.

Later increments evolve this crate in place with `ipv6`, `icmpv6`/`nd`,
`igmp`/`mld`, `udp`, `tcp`, `route`, and `frag` (`plans/NETWORK.md` §2.1).

## Security

Every decoder parses attacker-controlled bytes and is total (never
panics), bounded (no attacker-sized allocation), and fail-closed (a
malformed input is rejected whole). See `docs/src/lib/net.md` for the
architecture and the seam contract the `netstack` service builds on.
