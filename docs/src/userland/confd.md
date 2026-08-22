# App-data service (`confd`)

`confd` owns every user's per-app data store and is the **only** principal in
the system granted `CAP_APPDATA_ADMIN` — the per-inode capability gate the store
trees carry. It is therefore the only path to an application's stored settings,
and it answers each request against the store it derives from the caller's
kernel-attested identity (`plans/APPDATA.md`).

The installed binary lives at `/System/Services/confd.app/Run`. It is a
**boot-floor** service, named in PID 1's startup configuration rather than
started by `login`: a headless machine needs it exactly as much as a desktop
does, because the shell and every command app reach their own settings through
it.

## Why a service rather than a file mode

All of a user's applications run as that one user. The
[per-inode owner/mode/ACL model](../filesystem/permissions.md) keys on uid, so
it cannot separate two applications of the same account **at all** — whatever
mode bits are chosen, app-from-app isolation inside one account is not
expressible in it. Keying on the identity the kernel attests for the calling
*bundle* is, and that is the whole reason this service exists. It is not a
convenience wrapper over the filesystem.

Before it, every app wrote a file of its own choosing under the launching user's
home with `CAP_FS_ACCESS`, so any app the user launched could read and rewrite
every other app's settings.

## What a caller can ask for, and what it cannot

An `appdata-v1` request (`lib/abi/src/appdata_ipc.rs`) carries a key and a
value. It **never** carries a bundle identifier, a user, or a path: the service
resolves all three from the attested
[`Origin`](../architecture/multitasking.md), so no request shape names a store
and therefore none can reach another application's data. A caller running no
verified bundle — a kernel principal, a boot-floor program with no signed
manifest, a parser-sandbox child — has no store, and is refused and audited.

| Request | Effect |
|---|---|
| `ConfigGet { key }` | the caller's own value, or `NotFound` |
| `ConfigSet { key, value }` | stage a write |
| `ConfigUnset { key }` | stage a removal |
| `ConfigCommit` | publish every staged change atomically |
| `ConfigList { prefix, cursor }` | one bounded page of keys |

A set or an unset *stages* a pending edit against the calling **process
instance** (keyed on the unforgeable `ProcId`, so one instance can never publish
another's half-finished edits). The commit loads the committed document, applies
the edits, and publishes the result as one document replacement. A caller that
never commits changes nothing on the volume, and its own reads see its own
pending edits, so a settings sheet reads back what it just set.

An abandoned session — a caller that stages and exits — is reclaimed by age.
No primitive tells a server that a peer died, and losing an abandoned session's
edits is exactly the contract already stated.

## The tree it serves from

```text
/Users/<u>/Settings/Apps/<bundle-id>/     ← gated: required_cap = CAP_APPDATA_ADMIN
    settings.conf                           the app's own document
    .owner                                  the publisher ownership pin
/System/Settings/<bundle-id>/settings.conf  optional machine-wide policy layer
```

A *per-app* directory cannot be the gate itself: all of a user's applications
may write `Settings/`, so any of them could pre-create a sibling named after
another app's bundle id and have the service walk into it. The gate therefore
sits on the one fixed `Apps` parent, created **with the account** by every home
provisioner from the one shared definition in `tairix_users` — the image
builder, the account-administration path, and the integration fixture — owned by
this service's own account and gated on `CAP_APPDATA_ADMIN`.

Its ancestors carry a **search-only** ACL grant for the service's uid: the least
authority that lets a walk reach the root, and not enough to list a home or open
anything else in it.

Two checks authorise every open, and both are necessary:

- The **home** must be owned by the caller's uid. That makes the resolution a
  real uid→home answer rather than a guess, and it needs no reach into the
  credential database — so this service holds none, and compromising it cannot
  exfiltrate a password record.
- The **gated root** must be owned by the service. The root's parent is writable
  by the account, so an application could otherwise plant a world-traversable
  directory of that name and have the service serve forged settings out of it.
  The capability gate does not catch that: the service holds the capability
  either way.

The gate also guards the root's *name*, not only its content — see
[filesystem permissions](../filesystem/permissions.md) — so no principal that
can write the parent may unlink or rename the root aside and plant an ungated
replacement.

## Ownership is pinned to the publisher

Each store carries an `.owner` record naming the **publisher** — the developer's
stable identity from the signed manifest — rather than the key that signed the
running build. A release re-signed with a fresh build key therefore opens the
same store, while a different developer claiming the same bundle identifier is
refused and audited. The record is fixed-width and self-describing, so a
truncated or zeroed file attests *nothing* rather than reading as some
publisher.

## Read layering

Lowest precedence first:

1. `/System/Settings/<bundle-id>/settings.conf` — the optional machine-wide
   administrator policy. Read-only at runtime, and readable from an app's very
   first launch, before it has ever written anything of its own.
2. The user's own document — overrides only.

A commit writes **only** layer 2, so a user's file never absorbs a policy value
it did not set, and unsetting an override falls back to the policy layer rather
than to nothing. The bundle's own `DefaultSettings/` is a third layer below
both; it is applied by the client library, which is the only principal that can
name its own bundle without a lookup.

## Atomic publish, and what a crash leaves

A publish renders the document, writes it whole to `settings.conf.new`, flushes
it, and renames it over the live document. A crash therefore leaves either the
old document or the new one — never a torn one. A failed rename leaves the old
document live and the temporary behind; the next publish overwrites the
temporary, so a retry converges with no repair pass.

A save never destroys what a human wrote: the document engine
([`tairix-appconf`](../lib/appconf.md)) rewrites the one line it must and leaves
comments, blank lines, key order, and lines it did not understand exactly as it
found them.

## Fail-closed startup

The service binds its endpoint immediately, before any volume is unlocked, and
answers `DeviceOffline` until storage is reachable — a typed refusal, never a
guessed value. If the reserved endpoint cannot be bound it records
`SERVICE_UNAVAILABLE` and exits; PID 1 relaunches it. A squatter that claimed
the rendezvous first could serve forged settings to every application on the
machine, so the bind needs `CAP_IPC_BIND_PRIVILEGED`, which no ordinary
account's ceiling carries.

## Its authority, and what it deliberately lacks

The `confd` service account's ceiling is `CAP_IPC_BIND_PRIVILEGED`,
`CAP_APPDATA_ADMIN`, `CAP_FS_ACCESS`, and `CAP_LOG_EMIT`. It holds no spawn, no
network, no `CAP_FS_CHOWN`, and no users-database authority: it cannot start a
process, reach the credential store, seize a user's file, or hand one away.
Compromising it yields applications' settings and nothing else.

## What it does not isolate

A malicious application running as the user can already delete or rename the
user's own directories — including `~/Settings` — because it shares their uid.
That is the pre-existing consequence of one uid per account, and this service
neither creates nor removes it: a decoy tree is refused by the ownership check,
so the worst outcome is that app data becomes *unavailable* and audited, never
readable or forgeable. Confidentiality and integrity of one app's data against
another's is the boundary this service provides; availability of the user's own
home against their own apps is not a boundary any service can create.
