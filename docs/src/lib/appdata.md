# `tairix-appdata` — the app-data client

`lib/appdata` (`tairix-appdata`) is the one way an application reaches its own
settings (`plans/APPDATA.md` §3.9). An app opens its store once, reads every
setting out of the document it already holds, and publishes the changes it made
as one atomic commit.

```rust
let mut host = RtHost;
let mut settings = Settings::open(&mut host, "terminal");
let size = settings.u32("font.size")?.unwrap_or(14);
settings.set_u32("font.size", 16)?;
settings.commit()?;
```

## No app spells a path, and none names itself

Nothing here takes a store path, a user, or a bundle **identifier**: the
[app-data service](../userland/confd.md) derives all three from the identity the
kernel attests for the calling task. So an application cannot reach outside its
own scope by construction rather than by a check some caller might forget, and
this library has no privileged surface to misuse.

The one argument `open` does take is the program's own command **word**, and it
selects nothing but layer 1 below — the app's own shipped defaults. A wrong
value there can only mislead an application about itself.

## Three layers, and which of them this library owns

A read answers from the highest layer that sets the key:

1. `<Bundle>.app/DefaultSettings/settings.conf` — the defaults the bundle ships.
   **This library's layer.** It needs the *bundle's* path, and nothing attested
   gives the service one — `argv[0]` is caller data, and scanning the program
   stores for an identifier is a lookup that can disagree with what is actually
   running — while the app knows its own bundle with no lookup at all. It is
   resolved through the one shared bundle-resolution order (`tairix_cmdres`), so
   an app's defaults and its `man` page can never come from different bundles.
2. `/System/Settings/<bundle-id>/settings.conf` — optional machine-wide
   administrator policy. The service's layer.
3. The user's own document — overrides only. The service's layer.

Layers 2 and 3 arrive already merged, as one document, in one call.

## One call to open; reads are local; writes are published once

`open` does the one round trip. Every read after it is a lookup in memory, so an
application that consults forty settings issues no further calls — where a
per-key protocol would have cost the service a file read and a parse *each*.

Every `set` is memory too, until `commit` stages the keys that changed and
publishes them as one atomic document replacement. A handle that is never
committed changes nothing on the volume. Two things a `set` deliberately does
**not** do:

- stage a value the effective layers already carry, so an app that saves a
  setting it did not change does not rewrite the user's document; and
- copy a policy or default value up into the user's own file, which is what
  keeps that file holding only what the user actually chose.

A commit ends by re-reading the store, so the handle goes on reflecting what the
service actually holds — which matters after an `unset`, where the effective
value comes back from a layer below rather than from the value removed. A
standalone `reload` is a fresh view, not a merge: it discards unpublished edits,
which is the contract a handle that is simply never committed already has.

## No service, or no store: read the defaults, refuse the writes

`open` never fails. A store the service cannot serve — early boot before the
encrypted root is unlocked, a crashed service, a caller the kernel admitted from
no signed bundle — leaves the bundle's shipped defaults standing and records the
reason in `store_refusal`. The application therefore starts and behaves, can say
why its settings are the shipped ones, and is never told a change was saved that
was not: the commit reports the same typed error, and the edits stay staged for a
retry.

`defaults_refusal` is reported separately, and only for a defaults document that
*exists* and could not be used — a packaging defect worth saying out loud. A
bundle that ships none is the ordinary case and reports nothing.

## Whole documents, never fragments

A read declares the reply capacity the caller has. A document that does not fit
comes back as the byte count it needs and **no body at all**, and the client asks
again at exactly that length. So a caller never parses a truncated prefix and
never assembles a store out of two snapshots — every answer is one point-in-time
view. The retry count is bounded, so a service that keeps asking for more is
given up on rather than chased.

## Layering and testability

The crate is `no_std` (with `alloc`) and the engine performs no I/O: its three
syscalls sit behind the `AppDataHost` seam, so the layered read, the capacity
negotiation, staging, and the commit are all exercised on the host against a fake
that speaks the *real* `appdata-v1` codec rather than a mock of it. `RtHost`
(feature `rt`) is the syscall-backed one.

The `key = value` format is not defined here: it has one home,
[`tairix-appconf`](./appconf.md), and this client applies the same validators the
service does — so a `set` outside the grammar is refused where the mistake was
made rather than at commit time.
