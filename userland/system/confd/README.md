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

An `appdata-v1` request carries a key and a value. It never carries a bundle
identifier, a user, or a path: the service resolves all three from the attested
`Origin`, so **no request shape names a store**, and therefore none can reach
another application's data. A caller running no verified bundle — a kernel
principal, a boot-floor program with no signed manifest, a parser-sandbox child
— has no store and is refused, audited.

The five operations are `ConfigGet`, `ConfigSet`, `ConfigUnset`,
`ConfigCommit`, and the paged `ConfigList`. A set or an unset *stages* a pending
edit against the calling process instance; the commit loads the committed
document, applies the edits, and publishes the result as one atomic
replacement. A caller that never commits changes nothing on the volume, and its
own reads see its own pending edits, so a settings sheet reads back what it just
set.

## The tree it serves from

```
/Users/<u>/Settings/Apps/<bundle-id>/     ← gated: required_cap = CAP_APPDATA_ADMIN
    settings.conf                           the app's own document
    .owner                                  the publisher ownership pin
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

## Ownership is pinned to the publisher

Each store carries an `.owner` record naming the **publisher** — the developer's
stable identity from the signed manifest — rather than the key that signed the
running build. A release re-signed with a fresh build key therefore opens the
same store, while a different developer claiming the same bundle identifier is
refused and audited. The record is fixed-width and self-describing, so a
truncated or zeroed file attests *nothing* rather than reading as some
publisher.

## Layering and testability

The crate is `no_std` (with `alloc`) and the dispatcher performs **no I/O**:
every read and write goes through the injected `Storage` seam, so
authorisation, the ownership pin, the layered read, staging, and the atomic
publish are all exercised on the host with no filesystem at all. `src/run.rs`
supplies the real seam over the `fs_*` syscalls and is the only part that is
not host-testable.

The `key = value` document format itself is not defined here: it has one home,
`lib/appconf`, and this service never tokenises a settings line of its own.

There is deliberately **no scope selector** yet, on the wire or inside the
store: the private scope is the only one AD4 serves, and a one-value selector
would be interface built for a stage that has not landed. The public and sealed
scopes introduce it in the same change that gives it a second legal value —
each is a different file in the same directory, reached through the same
ownership check and the same atomic publish.
