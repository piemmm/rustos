# `tairix-confd` — the app-data service

Stability tier: **experimental**.

`confd` owns every user's per-app data store and is the **only** principal in
the system granted `CAP_APPDATA_ADMIN` — the per-inode capability gate the store
trees carry. So it is the only path to an application's stored settings, and it
answers each request against the store it derives from the caller's
kernel-attested identity (`plans/APPDATA.md`).

The installed binary lives at `/System/Services/confd.app/Run`. It is a
**boot-floor** service: a headless machine needs it exactly as much as a desktop
does, because the shell and every command app reach their own settings through
it, so PID 1 launches it from its startup configuration rather than `login`
starting it for a graphical session.

## Why a service and not a file mode

All of a user's applications run as that one user. The per-inode
owner/mode/ACL model keys on uid, so it cannot separate two applications of the
same account at all — app-from-app isolation *inside* one account is not
expressible in it, whatever mode bits are chosen. Keying on the identity the
kernel attests for the calling **bundle** is, and that is the whole reason this
service exists. It is not a convenience wrapper over the filesystem.

## What a caller can ask for, and what it cannot

A configuration `appdata-v1` request carries a scope, a key, and a value. It
never carries a bundle identifier, a user, or a path: the service resolves all
three from the attested `Origin`, so **no configuration request shape names a
store**, and therefore none can reach another application's data. A caller
running no verified bundle — a kernel principal, a boot-floor program with no
signed manifest, a parser-sandbox child — has no store and is refused, audited.

The operations are `ConfigRead`, `ConfigSet`, `ConfigUnset`, and `ConfigCommit`
on the caller's own configuration scopes; `PublicRead` on another application's
**published** document; and `VaultRead`, `VaultSet`, and `VaultUnset` on the
caller's **sealed** one.

`PublicRead` is the one shape that names an application, and it is a distinct
operation precisely so that it carries no scope field: a request that names
another app is public by construction. The three vault operations carry no scope
field either, and have no foreign counterpart at all — so a configuration frame
cannot name a secret, a vault frame cannot name a configuration document, and no
frame reaches another application's secrets. None of that is a check; there is no
request shape to refuse.

`ConfigRead` answers with the caller's **whole** merged document — the
machine-wide policy layer, the app's own settings over it, the caller's own
staged edits over those — as canonical `key = value` text the client parses with
the one format engine. So an application's start-up costs one call, one store
read, and one parse however many settings it goes on to consult; a per-key read
would have cost a file read and a parse *each*. The request declares the reply
buffer the caller has, and a document that does not fit comes back as the byte
count it needs with no body at all — so a caller never parses a prefix, and
never assembles a store out of two different snapshots.

A configuration set or unset *stages* a pending edit against the calling process
instance, in the scope it named; the commit loads that scope's committed
document, applies the edits for it, and publishes the result as one atomic
replacement. A caller that never commits changes nothing on the volume, and its
own reads see its own pending edits, so a settings sheet reads back what it just
set.

A **sealed** write is not staged and there is no `VaultCommit`: the service
opens the sealed document, applies the one change, re-seals it, and publishes it
before it replies. Plaintext secret material therefore exists here for the span
of one request rather than for a staging session's lifetime, and because
requests are served one at a time the whole read-modify-seal-publish is atomic —
so two processes of one application sealing different secrets cannot lose each
other's. A vault that cannot be opened is a typed, audited refusal rather than an
empty document: "your secrets are damaged" and "you have no secrets" must not
look alike.

## The tree it serves from

```
/Users/<u>/Settings/Apps/                 ← gated: required_cap = CAP_APPDATA_ADMIN
    .vault-master                           the account's app-data master secret
    <bundle-id>/
        settings.conf                       the app's own private document
        public.conf                         what the app publishes; any app may read
        secret.vault                        the app's secrets, sealed
        .owner                              the publisher ownership pin
/System/Settings/<bundle-id>/settings.conf  optional machine-wide policy layer
```

`Settings/Apps` is created **with the account** by every home provisioner (the
image builder, the account-administration path, and the integration fixture,
all reading the one shared definition in `tairix_users`), owned by this
service's own account and gated on `CAP_APPDATA_ADMIN`. Its ancestors carry a
search-only ACL grant for this service's uid — the least authority that lets a
walk reach the root, and not enough to list a home or open anything in it.

Two checks authorise every open, and both are necessary:

- The **home** must be owned by the caller's uid. That is what makes the
  resolution a real uid→home answer rather than a guess, and it needs no reach
  into the credential database — so this service holds none, and a compromise of
  it cannot exfiltrate a password record.
- The **gated root** must be owned by this service. The root's parent is
  writable by the account, so an application could otherwise plant a
  world-traversable directory of that name and have this service serve forged
  settings out of it. The capability gate does not catch that: this service
  holds the capability either way.

## The sealed scope

`secret.vault` holds the application's secrets, encrypted with
ChaCha20-Poly1305 under a key derived per (account, application) from the
account's `.vault-master` secret, the application's publisher, and its bundle
identifier — every primitive through `lib/crypto`, and no cryptography of its
own beyond one domain-separating context label. Binding the **publisher** means a
release re-signed with a fresh build key still opens the vault it wrote, while a
developer squatting the identifier derives a different key.

The master secret is stored as drawn, in the gated root: this service owns it,
mode `0700`, gated on the capability, on a volume ARXFS has no plaintext mode
for. It is deliberately not wrapped under a second key, because at this stage
there is no second secret to wrap it with and a publicly derivable wrapping key
would be theatre. The record is versioned, so a login-passphrase or TPM
protector reshapes it in place. A record that attests nothing is refused and
**never replaced** — the same secret is every application's key material in that
account, so drawing a fresh one would strand every existing vault while looking
like a clean start. Nothing is cached: it is read afresh per operation, used, and
wiped.

## Ownership is pinned to the publisher

Each store carries an `.owner` record naming the **publisher** — the developer's
stable identity from the signed manifest — rather than the key that signed the
running build. A release re-signed with a fresh build key therefore opens the
same store, while a different developer claiming the same bundle identifier is
refused and audited. The record is fixed-width and self-describing, so a
truncated or zeroed file attests *nothing* rather than reading as some
publisher.

## Layering and testability

The crate is `no_std` (with `alloc`), performs **no I/O**, and draws no
randomness of its own: every read and write goes through the injected `Storage`
seam and every draw through the injected `Entropy` seam, so authorisation, the
ownership pin, the layered read, staging, the atomic publish, and the sealed
scope's whole key hierarchy are exercised on the host with no filesystem and no
generator at all. `src/run.rs` supplies the real seams over the `fs_*` and
`random_get` syscalls and is the only part that is not host-testable.

The `key = value` document format itself is not defined here: it has one home,
`lib/appconf`, and this service never tokenises a settings line of its own. Nor
is the wipe: a sealed document is one of those documents, and the engine wipes
every line it discards, so no discard path here has to remember to.
