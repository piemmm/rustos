# `tairix-discoveryd` — link-local service discovery

Stability tier: **experimental**.

`discoveryd` is the multicast DNS / DNS-SD service (`plans/ZEROCONF.md`),
installed at `/System/Services/discoveryd.app/Run`. It learns the names and
services its segments announce. Nothing publishes or asks through it yet, so
it transmits nothing, and nothing enrols it: it is installed but not started.

## Two processes, one binary

* **The front** holds the two multicast DNS sockets (IPv4 and IPv6, port
  5353, each joined to its group) and every authority the service has:
  `CAP_NET`, `CAP_SANDBOX_SPAWN`, `CAP_LOG_EMIT`. It never parses a byte a
  peer sent.
* **The decoder** is this same binary, respawned through the kernel's sandbox
  spawn mode: capability-empty, holding two pipe ends and nothing else. It
  runs one `tairix-net` mDNS engine per interface, so a record learned on one
  link never answers for another.

Everything the decoder needs from the outside arrives on its pipe. The keys
its caches are indexed under and its CSPRNG is seeded from are drawn by the
front for each decoder. Time arrives as the instant carried by every frame
that moves it. The decoder answers only with the one instant its engines
next need time, a flag and an integer that the front checks field by field.

## What the front does

* **Relay.** A datagram is relayed only if the stack found its sender
  on-link and the sender is within its relay budget. A sender over its own
  budget is refused before the budget all senders share is charged. The
  payload crosses unread.
* **Back-pressure.** A datagram the decoder's queue has no room for is held,
  and the front stops draining its delivery port until the queue drains. The
  stack's mailbox then fills and drops, which costs the front nothing.
* **Time.** It ticks the decoder when the reported instant comes, but never
  before the decoder has reported since the last tick, and never closer
  together than 10 ms. A tick the queue has no room for waits for the queue
  to drain, not for a timer. The loop parks on one wait-set and never polls.
* **Containment.** A decoder is reaped and logged if it crashes, breaks the
  framing, ends its stream, or sends a frame no decoder sends. Everything
  learned from it is dropped. After the supervisor's paced delay, a
  replacement starts under fresh keys.

## Events

`discoveryd` owns ids `25_000..26_000`: service started (`25_001`), decoder
started with its generation (`25_002`), and service unavailable with its
reason (`25_003`). A decoder's failure is `lib/sandbox`'s own `6000`.

## Tests

The front and the decoder are host-tested over in-process workers: a
recording one, a doomed one, one that lies, and the real decoder end to end.
`fuzz_discoveryd` (enrolled in `cargo xtask fuzz`) holds:

* both frame codecs canonical;
* the decoder total over hostile datagrams;
* the front whole against a decoder saying anything at all.
