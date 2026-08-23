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

## Four scopes

`Settings::open` is the application's **private** scope: the user's settings for
it, which no other principal can read. `Settings::open_published` is its
**published** scope — what it says about itself for other applications to read —
and `read_published` reads another application's:

```rust
let mut mine = Settings::open_published(&mut host);
mine.set("font.family", "berkeley")?;
mine.commit()?;                                     // now readable by others

let theirs = read_published(&mut host, "os.tairix.terminal")?;
let family = theirs.get("font.family");
```

The two are separate documents with separate commits, because one atomic publish
replaces one document. A foreign read answers a plain `tairix-appconf`
`Document` — a snapshot, with the format engine's own typed accessors, and no
handle that could publish through it.

`Vault` is the **sealed** scope: the application's secrets, encrypted at rest by
the [service](../userland/confd.md) under a key derived per (account,
application).

```rust
let mut vault = Vault::open(&mut host)?;      // one call, whatever it holds
let saved = vault.get("imap.password");       // local
vault.set("imap.password", typed)?;           // sealed before it returns
vault.unset("smtp.password")?;
```

It differs from the other two scopes in three ways, and each is the sealed
scope's own:

- **No layer beneath it.** A secret an application did not write is not one it
  may be made to believe, so there are no bundle-shipped defaults and no
  machine-wide policy. An `unset` leaves the key absent rather than uncovering
  something else.
- **No staging and no commit.** The service seals and publishes each write
  before it replies, so nothing is ever left unsaved — and because the service
  serves requests one at a time, two instances of one application sealing
  different secrets cannot lose each other's. A write ends by re-reading, so the
  handle reflects what the service holds rather than what the client guessed;
  that is the same rule `commit` follows.
- **Opening can fail.** `Settings::open` degrades to the shipped defaults;
  `Vault::open` returns the refusal, because "I could not read your secrets" is
  not "you have none". An application must report a damaged vault rather than
  behave as though the user had never saved a password.

The handle holds the opened plaintext, so it wipes it when it goes out of scope
— and so does the format engine, for every line a document discards.

`blobs` is the **bulk** scope, and it is not a document at all. An index, a
cache, or a queue is reached as a *descriptor*, so its bytes never cross the
app-data channel:

```rust
let handle = blobs::open(&mut host, "mail.index", BlobMode::ReadWrite)?;
let index = File::from_delegation(handle)?;   // an owned descriptor
index.write_at(0, record)?;                   // straight to the kernel VFS

let held = blobs::list(&mut host)?;           // one call, whole listing
let quota = blobs::quota(&mut host)?;         // usage against both ceilings
blobs::remove(&mut host, "mail.index")?;
```

`open` answers the grant *handle* rather than an owned descriptor, because
installing one is a syscall and this crate is I/O-free by design so the whole
client stays host-testable; `File::from_delegation` is the owned redemption and
closes the descriptor on every path out. Redeeming it needs no filesystem
capability of the application's own: the delegation is exercised under the
service's captured authority.

`BlobMode` decides both what the delegation conveys and whether an absent blob
is created — `Read` refuses one the application does not hold, `ReadWrite`
creates it — so "create but do not write" is not a request that exists. What the
delegation conveys is *bounded*: a writable one carries a byte-extent ceiling
the kernel enforces on every write and truncate through the descriptor, so
direct access is not unbounded access. `quota` reports usage against that
ceiling and against the blob-count one, so an application that reaches either
can say "this cache is full" in its own terms rather than surfacing an errno.

`list` costs exactly one call, always: the widest listing that can exist is a
few kilobytes, so it asks for that outright rather than negotiating a capacity
the way a document read must.

## No app spells a path, and none names itself

Nothing here takes a store path or a user, and nothing but `read_published`
takes a bundle **identifier**: the [app-data service](../userland/confd.md)
derives all of those from the identity the kernel attests for the calling task.
So an application cannot reach outside its own scope by construction rather than
by a check some caller might forget, and this library has no privileged surface
to misuse. The one identifier a caller does name selects a *published* document
and nothing else — there is no request shape that reaches another application's
private settings.

The one argument `open` does take is the program's own command **word**, and it
selects nothing but layer 1 below — the app's own shipped defaults. A wrong
value there can only mislead an application about itself.
`open_published`, `Vault::open`, and the `blobs` calls take none: none of those
scopes has a layer beneath it, and a blob is not a document.

## Three layers, and which of them this library owns

Layering is the **private** scope's. The published scope is exactly one
document: the service cannot name a bundle's own directory, so a shipped
published document could never be read by anyone else, and what an application
publishes is therefore exactly what it wrote.

A private read answers from the highest layer that sets the key:

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

## Reading what another application publishes

`read_published` is the one call that names another application. It answers the
publisher's **committed** document — never its unsaved edits — and answers the
**empty document** for an application that publishes nothing, has never run for
this account, or whose store the service cannot attest. Those cases are
deliberately indistinguishable, so a caller learns what an application chose to
publish and nothing more.

An unreachable service or volume is reported as itself rather than as an empty
document, because only that is worth a retry. An identifier outside the
bundle-identifier grammar is refused by the wire codec, so a frame naming one
never reaches the service.

## No service, or no store: read the defaults, refuse the writes

`open` never fails, and neither does `open_published`. A store the service
cannot serve — early boot before the encrypted root is unlocked, a crashed
service, a caller the kernel admitted from no signed bundle — leaves the
bundle's shipped defaults standing (the published scope simply empty) and
records the reason in `store_refusal`. The application therefore starts and behaves, can say
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
negotiation, staging, the commit, and the sealed scope are all exercised on the
host against a fake that speaks the *real* `appdata-v1` codec rather than a mock
of it. `RtHost` (feature `rt`) is the syscall-backed one.

The fake does not *encrypt* the sealed scope — the sealing is the service's,
behind its own tests, and a fake that reimplemented it would be a second opinion
about a key hierarchy. What it reproduces is everything a client can observe: one
document, no layers, no staging, a write applied before the reply, and a refusal
that is a refusal rather than an empty vault.

The `key = value` format is not defined here: it has one home,
[`tairix-appconf`](./appconf.md), and this client applies the same validators the
service does — so a `set` outside the grammar is refused where the mistake was
made rather than at commit time.
