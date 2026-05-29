# `rustos-net-icmp` — userland ARP / IPv4 / ICMP-echo responder

Stage 4.D deliverable (Item 5). The smallest network service RustOS
ships: it answers ARP requests for one configured IPv4 address and
replies to ICMP echo requests ("ping") aimed at that address. It is the
protocol peer the virtio-net QEMU integration tests exercise.

The crate is `no_std`, allocation-free, and `#![forbid(unsafe_code)]`.
It builds purely on `rustos_abi::driver::net::Net`, so the same logic
runs against a real virtio-net device and against a mock in unit tests.

## Scope

In scope:

- ARP request + reply for IPv4 over Ethernet (RFC 826).
- IPv4 datagrams with option-free 20-byte headers (RFC 791).
- ICMP echo request → echo reply (RFC 792).

Out of scope (deferred to Stage 6): TCP, UDP, IPv6, IP routing,
fragmentation, neighbour caching, and retransmission.

## Design

`Responder` holds the interface's link-layer and IPv4 addresses and is
otherwise stateless:

- `Responder::handle_frame(frame, out)` is a pure function from an
  inbound frame plus a caller-owned scratch buffer to an optional
  outbound frame. Frames not addressed to this host, malformed frames,
  unsupported EtherTypes/protocols, and ICMP messages with a bad
  checksum are dropped silently (`Ok(None)`).
- `Responder::poll(net, rx, tx)` runs one receive/answer/transmit cycle
  over a `Net` driver.
- `Responder::run(net, rx, tx, max_polls)` drives `poll` a bounded
  number of times. The bound keeps the loop finite — there is no
  sleep-until or retry-until loop (`AGENTS.md` §2.1).

The one's-complement Internet checksum (RFC 1071) is implemented once
in `internet_checksum` and shared by the IPv4 and ICMP modules, so the
fold is never duplicated (`AGENTS.md` §2.2).

## Security

The responder performs no privileged operation; it only transforms
bytes. Capability enforcement for `Net::transmit` / `Net::receive`
(`CAP_NET_RAW`) happens at the driver dispatch site (`AGENTS.md` §5.4),
upstream of this crate.

## Test surface

`cargo test -p rustos-net-icmp` covers each protocol module in
isolation (parse round-trips, rejection of truncated / malformed /
wrong-binding inputs, checksum validity) plus end-to-end responder
behaviour over a mock `Net`: ARP and ICMP answers, ignoring frames for
other MACs/IPs, output-too-small handling, the poll/run loop, and
driver-error propagation. 33/33 host-side tests pass.

QEMU `qemu user net` ARP + ICMP round-trips against a live virtio-net
device are added by the virtio-net integration crates
(`.junie/next-session-prompt.md` Item 4).
