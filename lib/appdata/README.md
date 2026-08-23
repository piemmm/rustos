# `tairix-appdata` — the app-data client

Stability tier: **experimental**.

The one way an application reaches its own settings (`plans/APPDATA.md` §3.9).
An app opens its store once, reads every setting out of the document it already
holds, and publishes the changes it made as one atomic commit.

```rust
let mut host = RtHost;
let mut settings = Settings::open(&mut host, "terminal");
let size = settings.u32("font.size")?.unwrap_or(14);
settings.set_u32("font.size", 16)?;
settings.commit()?;
```

## Five scopes

`Settings::open` is the application's **private** scope. `Settings::open_published`
is its **published** one — what it says about itself for other applications to
read through `read_published` — and the two are separate documents with separate
commits, because one atomic publish replaces one document.

`Vault` is the **sealed** scope: the application's secrets, encrypted at rest by
the service under a key derived per (account, application).

```rust
let mut vault = Vault::open(&mut host)?;      // one call, whatever it holds
let saved = vault.get("imap.password");       // local
vault.set("imap.password", typed)?;           // sealed before it returns
```

It differs from the other two in three ways, each the sealed scope's own: no
layer beneath it, because a secret an application did not write is not one it may
be made to believe; no staging and no commit, because the service seals each
write before it replies; and opening can **fail**, because "I could not read
your secrets" is not "you have none" — an application must report a damaged vault
rather than behave as though the user had never saved a password. A write ends by
re-reading, so the handle reflects what the service holds rather than a guess.
The plaintext is wiped when the handle goes out of scope, by the format engine
that owns the document's storage.

`blobs` and `temp` are the **bulk** scopes, and neither is a document: an index,
a cache, a queue, or a staged download is reached as a *descriptor*, so its
bytes never cross the app-data channel.

```rust
let handle = blobs::open(&mut host, "mail.index", BlobMode::ReadWrite)?;
let index = File::from_delegation(handle)?;   // an owned descriptor

let scratch = temp::create(&mut host)?;       // a fresh file, service-named
temp::release(&mut host, &scratch.name)?;     // done with it

let used = bulk_quota(&mut host)?;            // both scopes, one moment
```

They differ in who names the file. A blob is durable and the application names
it. `temp::create` takes no name and nothing *opens* a temporary file, so the
only way to hold one is to have just created it — freshness without coordination
is what a scratch file is for, and an application can never read scratch it did
not write in this process. Their lifetime is the boot. What a delegation conveys
is bounded: only the access asked for, and a byte-extent ceiling the kernel
enforces on a writable one, so direct access is not unbounded access.

## No app spells a path, and none names itself

Nothing here takes a store path or a user, and nothing but `read_published` takes
a bundle **identifier**: the app-data service derives all of those from the
identity the kernel attests for the calling task. So an application cannot reach
outside its own scope by construction rather than by a check some caller might
forget, and this library has no privileged surface to misuse. The one identifier a
caller does name selects a *published* document and nothing else.

The one argument `open` does take is the program's own command **word**, and it
selects nothing but layer 1 below — the app's own shipped defaults. A wrong
value there can only mislead an application about itself. `open_published` and
`Vault::open` take none, because neither of those scopes has a layer beneath it.

## Three layers, and which of them this library owns

Layering is the **private** scope's alone. A read answers from the highest layer
that sets the key:

1. `<Bundle>.app/DefaultSettings/settings.conf` — the defaults the bundle ships.
   **This library's layer**: it needs the *bundle's* path, and nothing attested
   gives the service one, while the app knows its own bundle with no lookup.
   Resolved through the one shared bundle-resolution order (`tairix_cmdres`), so
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
- copy a policy or default value up into the user's file, which is what keeps
  that file holding only what the user actually chose.

A commit ends by re-reading the store, so the handle goes on reflecting what the
service actually holds — which matters after an `unset`, where the effective
value comes back from a layer below rather than from the value removed.

## No service, or no store: read the defaults, refuse the writes

`open` never fails. A store the service cannot serve — early boot before the
encrypted root is unlocked, a crashed service, a caller the kernel admitted from
no signed bundle — leaves the bundle's shipped defaults standing and records the
reason in `store_refusal`. The application therefore starts and behaves, can say
why its settings are the shipped ones, and is never told a change was saved that
was not: the commit reports the same typed error.

A `defaults_refusal` is reported separately, and only for a defaults document
that *exists* and could not be used — a packaging defect worth saying out loud.
A bundle that ships none is the ordinary case and reports nothing.

## Whole documents, never fragments

A read declares the reply capacity the caller has. A document that does not fit
comes back as the byte count it needs and **no body at all**, and the client
asks again at exactly that length. So a caller never parses a truncated prefix
and never assembles a store out of two snapshots — every answer is one
point-in-time view. The retry count is bounded, so a service that keeps asking
for more is given up on rather than chased.

## Layering and testability

The crate is `no_std` (with `alloc`) and the engine performs no I/O: its three
syscalls sit behind the `AppDataHost` seam, so the layered read, the capacity
negotiation, staging, the commit, and the sealed scope are all exercised on the
host against a fake that speaks the *real* `appdata-v1` codec. `RtHost` (feature
`rt`) is the syscall-backed one.

The fake does not *encrypt* the sealed scope — the sealing is the service's,
behind its own tests. What it reproduces is everything a client can observe: one
document, no layers, no staging, a write applied before the reply, and a refusal
that is a refusal rather than an empty vault.

The `key = value` format is not defined here: it has one home, `lib/appconf`,
and this client applies the same validators the service does — so a `set` is
refused where the mistake was made rather than at commit time.
