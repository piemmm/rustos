# Userland networking responder (`userland/net/icmp`)

`rustos-net-icmp` is the smallest network service RustOS ships. It
answers ARP requests for one configured IPv4 address and replies to
ICMP echo requests ("ping") aimed at that address. It is the protocol
peer the virtio-net QEMU integration tests exercise (`PLAN.md`
Stage 4.D, Item 5).

The crate is `no_std`, allocation-free, and `#![forbid(unsafe_code)]`.
It depends only on `rustos_abi::driver::net::Net`, so identical logic
runs against a real virtio-net device and against a mock in tests.

## Protocol surface

| Layer    | Handled                                   | RFC  |
|----------|-------------------------------------------|------|
| Ethernet | parse + emit Ethernet II headers          | 894  |
| ARP      | request + reply, IPv4-over-Ethernet only  | 826  |
| IPv4     | option-free 20-byte headers               | 791  |
| ICMP     | echo request → echo reply                 | 792  |

Anything else — TCP, UDP, IPv6, routing, fragmentation, neighbour
caching, retransmission — is explicitly out of scope and deferred to
Stage 6.

## Components

- `ethernet`, `arp`, `ipv4`, `icmp` — each parses and serialises one
  protocol layer with bounds-checked, panic-free accessors. Each
  rejects truncated or malformed inputs by returning `None`.
- `internet_checksum` — the one's-complement Internet checksum
  (RFC 1071), written once and shared by the IPv4 and ICMP layers so
  the fold is never duplicated (`AGENTS.md` §2.2).
- `Responder` — binds an interface's MAC and IPv4 address. It is
  stateless beyond those two values.

### `Responder` API

- `handle_frame(frame, out) -> Result<Option<usize>, NetServiceError>`
  is a pure function: it parses `frame`, and on a request that is
  addressed to this host and well-formed, writes the reply into `out`
  and returns its length. Frames for other hosts, malformed frames,
  unsupported EtherTypes/protocols, and ICMP messages whose checksum
  fails verification are dropped silently (`Ok(None)`). A reply that
  does not fit in `out` returns `NetServiceError::OutputTooSmall`.
- `poll(net, rx, tx)` runs one receive/answer/transmit cycle over a
  `Net` driver and reports whether a frame was processed.
- `run(net, rx, tx, max_polls)` drives `poll` a bounded number of
  times. The bound is mandatory: there is no sleep-until or
  retry-until loop (`AGENTS.md` §2.1); a long-running service supplies
  its own budget and re-enters between blocking driver waits.

## Security

The responder performs no privileged operation; it only transforms
bytes. Capability enforcement for `Net::transmit` / `Net::receive`
(`CAP_NET_RAW`) happens at the driver dispatch site (`AGENTS.md` §5.4),
upstream of this crate. A reply is emitted only for a request that is
correctly addressed, well-formed, and (for ICMP) checksum-valid;
everything else is dropped rather than answered, so the service cannot
be coaxed into reflecting arbitrary traffic.

## Tests

`cargo test -p rustos-net-icmp` runs 33 host-side tests: per-layer
parse/serialise round-trips and rejection paths, checksum validity
(including the RFC 1071 worked example), and end-to-end responder
behaviour over a mock `Net` (ARP and ICMP answers, ignoring frames for
other MACs/IPs, output-too-small handling, the poll/run loop, and
driver-error propagation).
