# Userland networking service (`userland/net/icmp`)

`rustos-net-icmp` is the smallest network service RustOS ships. It has
two halves. `Responder` answers ARP requests for one configured IPv4
address and replies to ICMP echo requests ("ping") aimed at that
address. `Client` is the initiating counterpart: it resolves a peer's
link-layer address via ARP and pings it. It is the protocol peer the
virtio-net QEMU integration tests exercise (`PLAN.md` Stage 4.D,
Item 5): the test bin uses `Client` to resolve and ping the QEMU
user-network gateway over the live virtio-net device.

The crate is `no_std`, allocation-free, and `#![forbid(unsafe_code)]`.
It composes the shared protocol engine (`lib/net`, re-exported) over
`rustos_abi::driver::net::Net`, so identical logic runs against a real
virtio-net device and against a mock in tests.

## Protocol surface

| Layer    | Handled                                   | RFC  |
|----------|-------------------------------------------|------|
| Ethernet | parse + emit Ethernet II headers          | 894  |
| ARP      | request + reply, IPv4-over-Ethernet only  | 826  |
| IPv4     | option-free 20-byte headers               | 791  |
| ICMP     | echo request → echo reply                 | 792  |

Anything else — TCP, UDP, IPv6, routing, fragmentation, neighbour
caching, retransmission — is explicitly out of scope: the `netstack`
service (`plans/NETWORK.md`) delivers those and replaces this
responder outright in its N3 increment.

## Components

- The wire codecs (`eth`, `arp`, `ipv4`, `icmp`) and the Internet
  checksum live in the shared protocol engine ([`lib/net`](../lib/net.md))
  and are re-exported here (`ethernet` is `rustos_net::eth`); this crate
  contains no parser of its own, so the definitions cannot drift
  (`AGENTS.md` §2.2).
- `write_arp_frame` / `write_icmp_frame` — Ethernet+ARP and
  Ethernet+IPv4+ICMP framing, written once and shared by `Responder`
  (replies) and `Client` (requests) so the framing is never duplicated
  (`AGENTS.md` §2.2).
- `Responder` — binds an interface's MAC and IPv4 address. It is
  stateless beyond those two values.
- `Client` — binds the same two values and initiates exchanges. It is
  likewise stateless: no neighbour cache, no retransmission (the
  `netstack` service layers `rustos_net::neigh` and retries on top).

### `Responder` API

- `handle_frame(frame, out) -> Result<Option<usize>, NetServiceError>`
  is a pure function: it parses `frame`, and on a request that is
  addressed to this host and well-formed, writes the reply into `out`
  and returns its length. Frames for other hosts, malformed frames,
  unsupported EtherTypes/protocols, and ICMP messages whose checksum
  fails verification are dropped silently (`Ok(None)`). A reply that
  does not fit in `out` returns `NetServiceError::OutputTooSmall`.
- `poll(net, rings, rx, tx)` runs one service/answer cycle over a
  `Net` driver's frame rings and reports whether a frame was
  processed.
- `run(net, rings, rx, tx, max_polls)` drives `poll` a bounded number
  of times. The bound is mandatory: there is no sleep-until or
  retry-until loop (`AGENTS.md` §2.1); each poll's device wait is
  event-driven inside `Net::service`, so the budget bounds churn, not
  a spin.

### `Client` API

- `write_arp_request(target, out)` / `parse_arp_reply(frame, target)`
  serialise a broadcast ARP request resolving `target` and recognise
  the matching unicast reply, returning the resolved MAC.
- `write_echo_request(peer_mac, dest, id, seq, payload, out)` /
  `is_echo_reply(frame, dest, id, seq)` serialise an ICMP echo request
  to an already-resolved peer and recognise the matching checksum-valid
  echo reply.
- `resolve(net, rings, target, rx, tx, max_polls)` queues one ARP
  request and services the rings a bounded number of times looking for
  the reply, returning `Ok(Some(mac))` once resolved or `Ok(None)`
  within the budget.
- `ping(net, rings, peer_mac, dest, id, seq, payload, rx, tx,
  max_polls)` queues one ICMP echo request and services the rings for
  the matching reply, returning `Ok(true)` once confirmed. Both loops
  are bounded for the same reason as `Responder::run`.

## Security

The responder performs no privileged operation; it only transforms
bytes. Capability enforcement for `Net::service`
(`CAP_NET_RAW`) happens at the driver dispatch site (`AGENTS.md` §5.4),
upstream of this crate. A reply is emitted only for a request that is
correctly addressed, well-formed, and (for ICMP) checksum-valid;
everything else is dropped rather than answered, so the service cannot
be coaxed into reflecting arbitrary traffic.

## Tests

`cargo test -p rustos-net-icmp` runs the end-to-end responder
behaviour over a mock `Net` (ARP and ICMP answers, ignoring frames for
other MACs/IPs, output-too-small handling, the poll/run loop, and
driver-error propagation), `Client` behaviour over the same mock
(ARP resolve emitting a well-formed request, ICMP ping confirming the
reply, mismatched-sequence and wrong-target rejection, output-too-small
handling, and driver-error propagation), and the composed-path fuzz
harness (`fuzz_parse`, registered with `cargo xtask fuzz`). The
per-layer parse/serialise and checksum tests live with the codecs in
`lib/net` (`fuzz_net_eth`, `fuzz_net_addr`).
