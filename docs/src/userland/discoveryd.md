# Link-local service discovery (`discoveryd`)

`discoveryd` is the multicast DNS / DNS-SD service, installed at
`/System/Services/discoveryd.app/Run` and started by PID 1 at boot: a segment
takes time to answer, so the service should already be listening when a program
first asks. It requires `network-up`, so it starts once the network stack
answers, and since its sockets live in the stack, a stack relaunch stops it and
starts it again against the new one. The staged design is `plans/ZEROCONF.md`. Programs reach it through
[`lib/discovery`](../lib/discovery.md); the protocol engine is
`tairix_net::mdns` ([`lib/net`](../lib/net.md)); the containment is the
supervised session of [`lib/sandbox`](../security/sandbox.md).

It asks and answers questions for its clients. It publishes nothing of its own
yet.

## The authority split

A multicast DNS datagram is written by whoever shares the segment — a café's
network, a compromised printer. The process that parses it therefore holds
nothing worth taking.

| | Front | Decoder |
|---|---|---|
| Is | the service as started | the same binary, respawned in the kernel's sandbox spawn mode |
| Holds | the two multicast DNS sockets, the discovery endpoint, `CAP_NET`, `CAP_SANDBOX_SPAWN`, `CAP_IPC_BIND_PRIVILEGED`, `CAP_FS_ACCESS` (the grant store, read once at start), `CAP_LOG_EMIT` | two pipe ends; an empty capability record and the sandbox syscall allow-list |
| Parses | nothing a peer sent; only fixed fields from its decoder, each bounds-checked | every datagram, through one engine per interface |
| Has a clock | yes | no: each frame that moves time carries the instant |
| Has randomness | the kernel CSPRNG | none of its own: its cache key and CSPRNG key arrive in its first frame |

The network stack reserves the multicast DNS port and groups to the service's
account (`DISCOVERYD_UID`), so no other process can hear the segment's answers
or send a query that draws unicast replies from every responder on it.

## Clients

A client opens a **session** on the reserved `DISCOVERY_ENDPOINT`, naming a
private delivery port, and starts typed requests in it
(`tairix_abi::discovery_ipc`): browse a service type, resolve an instance, look
up a host's addresses, name a link-local address, or enumerate every type. Each
request is answered continuously until it is stopped. Answers queue in the
session and are taken with a call that never waits; the service rings the port
once when answers are waiting and not again until the client has collected
them all, so a slow reader costs one doorbell however many answers queue. A
doorbell is believed only when its kernel-attested sender is the service's
account.

The service derives every name it asks the segment from the request's fields,
so the type a browse is scoped to is a field it reads, never a name the caller
spelled. Every answer names the interface it was learned on, and nothing merges
two links.

### Admission

Decided from the caller's kernel-attested `Origin`, before any state is
touched:

| Request | Needs |
|---|---|
| any | `CAP_NET` |
| host, reverse | nothing more |
| browse, resolve | a grant for the type, or `CAP_NET_DISCOVER_ALL` |
| enumerate types | `CAP_NET_DISCOVER_ALL` |

Grants live in `/System/Security/Policy/Discovery`. The image builder writes
them from each signed manifest's `browses` list, keyed on the bundle id and
publisher the load gate attests, so a grant is never more than a verified
manifest asked for. The service reads the store once at start and takes it
whole or not at all: a store that does not parse grants nothing, and says why.
A refusal names what the caller lacked and the type it asked for, never whether
another principal holds that type.

### Bounds

Fixed containment bounds, since each is state the service holds for a client:

| Bound | Value |
|---|---|
| Sessions per account | 8 |
| Requests per session | 16 |
| Queued answers per session | 64 KiB; past it the request is told `Lost` once, after everything queued before |
| Sessions in all | 256 |

A session ends when its owner exits: the service watches every principal it
holds a session for through the kernel's peer-exit watch
([`peer_watch`](../architecture/syscalls.md)), so a client that dies takes its
questions with it.

## Questions

Each distinct question — a form and a name — is asked once, however many
requests share it, on every interface whose link is up. A request that joins a
question already asked is brought up to date by a replay from the decoder's
cache rather than from a second copy in the front.

The front keeps keyed fingerprints of what each question holds on each link,
which is all it needs to hold the decoder to its word: an answer is added once,
renewed or retired only while held, and never beyond the most records an honest
engine can hold for one question on one link. An answer that crossed a stop, a
flush, or a link edge in flight is stale and dropped; one no honest decoder
sends condemns it.

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
| front → decoder | `Link` | the instant, an interface, and whether its link came up or went down |
| front → decoder | `Ask` | the instant, a question id, its form, and the name |
| front → decoder | `Stop` | a question id |
| front → decoder | `Replay` | a question id and a token naming the requests owed it |
| decoder → front | `Deadline` | the earliest instant any engine next needs time, or none |
| decoder → front | `Answer` | one edge in one question's answers, already typed |
| decoder → front | `Held` / `Replayed` | a replay's answers, then its end |
| decoder → front | `Transmit` | a datagram an engine built, its interface, and its destination |
| decoder → front | `Linked` | the acknowledgement of one link edge |

The decoder acknowledges every link edge. The front believes an answer about a
link, and sends a datagram built for it, only once every edge it told the
decoder has been acknowledged, so nothing learned before a link's last edge
reaches a client after it.

A decoder that receives anything before its configuration, a second
configuration, or a frame it cannot decode ends its session: the front never
sends one, so guessing would only hide a bug.

## The front's rules

- **Only on-link senders are relayed.** The stack stamps every delivery with
  whether its sender is on the arrival interface's link. The front drops
  anything else unread, because reflected mDNS is an amplifier.
- **Relay admission is budgeted per sender, then shared.** A sender gets a
  burst of 64 at 32 per second; all senders together get 2048 at 1024 per
  second, across 32 tracked senders.
- **What it sends is bounded too.** A datagram goes to the group, within the
  interface's budget of a burst of 32 at 8 per second, or straight back to a
  peer the front relayed from on that interface within the last second, once
  per datagram relayed from it and at most four owed at once. It names its
  egress interface, so a message built from one link's state never leaves by
  another.
- **Back-pressure costs nothing per datagram.** The decoder's queue holds
  64 KiB. A datagram it has no room for is held, and the front stops draining
  its delivery port until the queue drains.
- **Time is paced by the front.** It ticks the decoder when the reported
  instant comes, never before the decoder has reported since the last tick, and
  never closer together than 10 ms. After every wake, the next wake it asks for
  is later than the instant it just acted on, so its loop parks and never
  spins.
- **Containment is total.** A decoder is reaped and logged, and everything
  learned from it dropped — every client holding an answer is told the answers
  are void — if it crashes, breaks the framing, ends its stream, or sends a
  frame no decoder sends. A replacement starts after the supervisor's paced
  delay (100 ms, doubling to 30 s), is keyed afresh, and is told the links and
  asked the questions again.
- **Without entropy, no decoder.** If the random source refuses the keys a
  decoder needs, the service logs why and exits.

## The reactor

One wait-set holds the delivery port (registered only while the front takes
datagrams, and drained at most one mailbox's worth per wake), the decoder's two
pipe ends, the discovery endpoint, the thread's peer-exit feed, and room on any
client port a doorbell is owed to, with a timeout for the one instant the front
must next act by. A dead wait-set ends the service rather than degrading into a
poll.

## Audit records

The service owns the stable `tairix_log::EventId` range **`25_000` …
`25_999`**. A decoder's crash is `lib/sandbox`'s own `6000`, and a refused
decoder launch is its `6001`.

| Id | Name | Level | Meaning |
|---|---|---|---|
| `25_001` | `SERVICE_STARTED` | Info | The front holds its multicast DNS sockets; carries each address family's outcome — joined, or the step the stack refused and why. |
| `25_002` | `DECODER_STARTED` | Info | A decoder started and was keyed; carries its generation. |
| `25_003` | `SERVICE_UNAVAILABLE` | Error | The service cannot serve and is exiting; carries the reason, and each family's outcome when no socket could be opened. |
| `25_004` | `REQUEST_DENIED` | Warn | A client request was refused for want of authority; carries what was lacking, the caller's uid, and the type asked for. |
| `25_005` | `GRANTS_REFUSED` | Warn | The grant store could not be opened or read, or was refused whole, so no application may browse; carries the reason. |

## Tests

The front, the decoder, the channel, the question table, the sessions, and the
query planner are host-tested, the front over in-process workers — a recording
one, a doomed one, one that lies, and the real decoder end to end. They cover
relay admission, the held datagram, the tick floor, crash replacement under
fresh keys, condemnation, admission and refusal, grants, replays to late
joiners, link flaps in flight, bounded queues and the `Lost` notice, peer exit,
and every transmit rule.

`fuzz_discoveryd` is enrolled in `cargo xtask fuzz`: both codecs canonical under
random frames and single-bit mutations, the decoder total over hostile and
well-formed datagrams with time running backwards, and the front against a
decoder saying anything at all while it holds its no-spin bound, configures each
decoder first and once, spaces its ticks, contains the decoder, and relays to
its replacement.

The live vertical `tairix-test-discovery-qemu-aarch64` boots the production
aarch64 image with a host-side multicast DNS responder on the wire and runs
`dns-sd` browse, resolve, and host lookups through the whole path; the peer
requires that the guest asked the wire for every record they need, and the
script that the guest printed each of the peer's answers.
