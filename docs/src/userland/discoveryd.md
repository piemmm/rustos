# Link-local service discovery (`discoveryd`)

`discoveryd` is the multicast DNS / DNS-SD service, installed at
`/System/Services/discoveryd.app/Run`. The staged design is
`plans/ZEROCONF.md`. The protocol engine it runs is `tairix_net::mdns`
([`lib/net`](../lib/net.md)), and the containment it runs under is the
supervised session of [`lib/sandbox`](../security/sandbox.md).

Today it listens: it learns what its segments announce into one cache per
interface. Nothing publishes or asks through it yet, so it sends nothing.
PID 1 does not enrol it, so it is installed but not started until a client
of it exists.

## The authority split

A multicast DNS datagram is written by whoever shares the segment — a
café's network, a compromised printer. The process that parses it therefore
holds nothing worth taking.

| | Front | Decoder |
|---|---|---|
| Is | the service as started | the same binary, respawned in the kernel's sandbox spawn mode |
| Holds | the two multicast DNS sockets, `CAP_NET`, `CAP_SANDBOX_SPAWN`, `CAP_LOG_EMIT` | two pipe ends; an empty capability record and the sandbox syscall allow-list |
| Parses | nothing a peer sent; only a flag and an integer from its decoder, field by field | every datagram, through one engine per interface |
| Has a clock | yes | no: each frame that moves time carries the instant |
| Has randomness | the kernel CSPRNG | none of its own: its cache key and CSPRNG key arrive in its first frame |

A decoder's engines are keyed by what the front drew for that decoder, so a
replacement never shares keys with the decoder it replaces. One engine per
interface, created by that interface's first datagram, means a record learned
on one link can never answer for another.

## The channel

Every frame is one fixed layout behind a one-byte tag, and both directions
refuse a frame whose tag, length, or any field is not exactly what an honest
peer sends. The encodings are canonical: a frame that decodes re-encodes to
exactly its bytes.

| Direction | Frame | Carries |
|---|---|---|
| front → decoder | `Configure` | the cache key and CSPRNG key; the first frame, sent once |
| front → decoder | `Datagram` | the instant, the arrival interface, the sender's address and port, the payload unread |
| front → decoder | `Tick` | the instant |
| decoder → front | `Deadline` | the earliest instant any engine next needs time, or none |

A decoder that receives anything before its configuration, a second
configuration, or a frame it cannot decode ends its session: the front never
sends one, so guessing would only hide a bug.

## The front's rules

- **Only on-link senders are relayed.** The stack stamps every delivery with
  whether its sender is on the arrival interface's link. The front drops
  anything else unread, because reflected mDNS is an amplifier.
- **Relay admission is budgeted per sender, then shared.** A sender gets a
  burst of 64 at 32 per second. All senders together get 2048 at 1024 per
  second, across 32 tracked senders. A sender over its own budget is refused
  before the shared one is charged, so one flooder cannot starve the rest,
  and the shared budget bounds a sender rotating its address.
- **Back-pressure costs nothing per datagram.** The decoder's queue holds
  64 KiB. A datagram it has no room for is held, and the front stops
  draining its delivery port until the queue drains. The stack's bounded
  mailbox then fills and drops.
- **Time is paced by the front.** It ticks the decoder when the reported
  instant comes, never before the decoder has reported since the last tick,
  and never closer together than 10 ms. A tick the queue has no room for
  waits on the queue draining. After every wake, the next wake the front
  asks for is later than the instant it just acted on, so its loop parks and
  never spins, whatever the decoder reports.
- **Containment is total.** A decoder is reaped and logged, and everything
  learned from it dropped, if it crashes, breaks the framing, ends its
  stream, or sends a frame no decoder sends. A replacement starts after the
  supervisor's paced delay (100 ms, doubling to 30 s), so a datagram crafted
  to kill the decoder costs a spawn per backoff step, never one per datagram.
- **Without entropy, no decoder.** If the random source refuses the keys a
  decoder needs, the service logs why and exits rather than run a decoder
  under predictable keys.

## The reactor

One wait-set holds three things:

- the delivery port, registered only while the front will take datagrams,
  and drained at most one mailbox's worth (64) per wake, so a segment that
  refills it as fast as it drains cannot keep the loop from its decoder or
  its timer;
- the decoder's two pipe ends, registered exactly as its session wants
  them, and moved to each generation's new pipes;
- a timeout for the one instant the front must next act by.

A dead wait-set ends the service rather than degrading into a poll.

## Audit records

The service owns the stable `tairix_log::EventId` range **`25_000` …
`25_999`**. A decoder's crash is `lib/sandbox`'s own `6000`, and a refused
decoder launch is its `6001`.

| Id | Name | Level | Meaning |
|---|---|---|---|
| `25_001` | `SERVICE_STARTED` | Info | The front holds its multicast DNS sockets; carries which address families joined their group. |
| `25_002` | `DECODER_STARTED` | Info | A decoder started and was keyed; carries its generation. |
| `25_003` | `SERVICE_UNAVAILABLE` | Error | The service cannot serve and is exiting; carries the reason. |

## Tests

The front and the decoder are host-tested over in-process workers: a
recording one, a doomed one, one that lies, and the real decoder end to end.
The tests cover:

- relay of on-link datagrams only;
- per-sender budgets;
- the held datagram and the stopped drain;
- the tick floor, and a tick owed to a full queue;
- crash replacement under fresh keys;
- condemnation of an unbelievable frame;
- refusal to start without entropy.

`fuzz_discoveryd` is enrolled in `cargo xtask fuzz`. It runs every
structural case on every iteration, with drawn content inside each:

- both codecs canonical under random frames and single-bit mutations;
- the decoder total over hostile and well-formed datagrams, with time
  running backwards;
- the front against a decoder saying anything at all. That covers
  believable deadlines past and future, an unknown tag, an oversize frame,
  framed and raw noise, and an ended stream, plus a queue filled to the
  byte while a tick falls due. In every case the front must hold its no-spin
  bound, configure each decoder first and once, space its ticks, contain the
  decoder, and relay to its replacement.
