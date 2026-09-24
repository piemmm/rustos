# `tairix-discovery` — the link-local discovery client

`lib/discovery` is how a program browses for, resolves, and looks up link-local
services. It never speaks multicast DNS itself: the network stack reserves the
port and groups to [`discoveryd`](../userland/discoveryd.md), which asks the
segment for every client and admits each request against the caller's
kernel-attested identity. Stability tier: **experimental**.

## Shape

- **`Session`** — the pure client over an injected `Transport`. `open` names
  the private port the service rings; `start` begins a typed request
  (`Query`: browse, resolve, host, reverse, or every type) and returns its id;
  `collect` takes whatever has arrived, never waiting; `stop` ends one request
  and `close` (or dropping the session) ends them all. Requests are answered
  continuously until stopped.
- **`Held`** — one request's current answers, kept from the entries `collect`
  returns. Each entry says an answer was added, renewed, or retired, that
  everything held on one interface is void, or that updates were lost because
  the session's queue filled; `apply` folds one in and reports what moved, so a
  browse that runs for an hour costs what changes rather than what is held.
- **`RtDiscovery`** (the `program` feature) — the production glue: the endpoint
  call, and a wait on the session's doorbell that the service wakes. A
  doorbell is believed only when its kernel-attested sender is the service's
  account and it names this session.
- **`host_addresses` / `reverse_name`** — one-shot lookups bounded by a
  deadline, which `lib/resolver` routes every `.local` name and link-local
  address to.

## Guarantees

- Every answer names the interface it was learned on; nothing merges two links.
- Every name in an answer was written by an unauthenticated peer: structurally
  valid, never display-safe. A renderer escapes it (`tairix_net::dns::Presentation`).
- Resolution returns an address, never a capability.
- Buffers are committed fallibly at open, so a session that opens never
  allocates on its answer path.

## Errors

`DiscoveryError` separates the service being absent (`Unavailable` — a build
without discovery, or one where the service has not started) from a refusal
(`Refused`, carrying the service's `Errno`), a transport failure, and a reply
that does not decode (`Malformed`).

## Tests

The pure layer is host-tested over a fake transport: the session lifecycle and
its close on drop, an absent service and a refusal, and `Held`'s folding of
every edge, a flush, and a loss. `dns-sd` and `lib/resolver` exercise it end
to end, and the `tairix-test-discovery-qemu-aarch64` vertical runs it against
the live service.
