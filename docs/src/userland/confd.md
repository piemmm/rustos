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

An own-store `appdata-v1` request (`lib/abi/src/appdata_ipc.rs`) carries a
scope, a key, and a value. It **never** carries a bundle identifier, a user, or
a path: the service resolves all three from the attested
[`Origin`](../architecture/multitasking.md), so no request shape names a store
and therefore none can reach another application's private data. A caller
running no verified bundle — a kernel principal, a boot-floor program with no
signed manifest, a parser-sandbox child — has no store, and is refused and
audited whichever operation it sent.

| Request | Effect |
|---|---|
| `ConfigRead { scope, capacity }` | the caller's whole merged document for one of its own scopes, or the length it needs |
| `ConfigSet { scope, key, value }` | stage a write |
| `ConfigUnset { scope, key }` | stage a removal |
| `ConfigCommit { scope }` | publish that scope's staged changes atomically |
| `PublicRead { bundle_id, capacity }` | another application's **published** document |
| `VaultRead { capacity }` | the caller's whole **sealed** document |
| `VaultSet { key, value }` | seal one secret, immediately |
| `VaultUnset { key }` | remove one secret, immediately |

A read answers with the **whole** document, not one key: for the private scope,
the policy layer, the app's own settings over it, and the caller's own staged
edits over those, rendered as canonical `key = value` text the client parses
with the one format engine (`lib/appconf`). An application's start-up therefore
costs one call, one store read, and one parse however many settings it goes on
to consult — where a per-key read would have cost the service a file read and a
parse *each*, so a thirteen-setting profile cost thirteen of both.

The request declares the reply buffer the caller has. A document that does not
fit comes back as the byte count it needs and **no body at all**, so a caller
never parses a truncated prefix, and never assembles a store out of two
different snapshots: every answer is one point-in-time view. A client sizes a
small buffer for the store it expects and pays a second call only for a store
larger than that.

A set or an unset *stages* a pending edit against the calling **process
instance** (keyed on the unforgeable `ProcId`, so one instance can never publish
another's half-finished edits) in the scope it named. The commit loads that
scope's committed document, applies the pending edits for it, and publishes the
result as one document replacement. A caller that never commits changes nothing
on the volume, and its own reads see its own pending edits, so a settings sheet
reads back what it just set.

Staging and committing are **per scope**, and the pending-edit bound is too:
one rename replaces one name, so a commit that claimed to publish two documents
at once would be claiming an atomicity no filesystem offers. An application
editing its settings and publishing about itself at the same time therefore has
two independent pieces of unpublished work, and neither commit carries the
other's.

An abandoned session — a caller that stages and exits — is reclaimed by age.
No primitive tells a server that a peer died, and losing an abandoned session's
edits is exactly the contract already stated.

## Three scopes: the app's own settings, what it publishes, and its secrets

`settings.conf` is the **private** scope — the user's settings for that
application, which no other principal can read. `public.conf` is the
**published** scope: what the application says about itself for other
applications to read.

The published scope is what keeps the isolation complete rather than merely
strict. Two applications that must share a value have exactly one sanctioned
way to do it; without one, the only remaining route would be an invented path
under `CAP_FS_ACCESS`, which is precisely the un-isolated arrangement this
service exists to remove.

Reading it is the one request that names an application, and it is a **distinct
operation** rather than a scope on a shared one, so a request that names another
app cannot ask for the private scope — there is no field to set. Three things
answer identically, with the empty document: an application that publishes
nothing, one that has never run for this account, and one whose store the
service cannot attest (a malformed ownership pin, an out-of-bounds document —
both audited, naming the *target*). So a reader learns what an application chose
to publish and nothing else, and cannot use the endpoint to probe which
applications an account has ever run. The caller's own refusals — no home, a
root the service does not own, an unreachable volume — are reported as
themselves, because only those are worth a retry.

A foreign read answers the **committed** document, never the publisher's staged
edits: a published value is what every other application sees, and a reader
must not act on one that may never be published.

## The sealed scope

`secret.vault` is the application's **secrets** — a password, a token, a key —
encrypted at rest with ChaCha20-Poly1305 under a key the service derives per
(account, application):

```text
per-account master secret        (32 random bytes, drawn once per account)
  └─ derive_key(master, "tairix-appdata-secret/v1" ‖ 0x00 ‖ publisher ‖ bundle-id)
       └─ per-(account, application) AEAD key
```

Every primitive comes from [`lib/crypto`](../lib/crypto.md): the single-block
HKDF-Expand the ARXFS key hierarchy already derives its own subkeys through, and
the same AEAD encrypted swap uses. The sealed scope introduces no cryptography —
only a domain-separating context label of its own.

Binding the derivation to the **publisher** means a release re-signed with a
fresh build key still opens the vault it wrote, while a different developer
squatting the same bundle identifier derives a different key and cannot read it
even if the ownership pin were somehow bypassed. That is defence in depth behind
the pin, not a substitute for it.

It is reached by three operations that carry **no scope field** and have **no
foreign counterpart**, so a configuration frame cannot name a secret, a vault
frame cannot name a configuration document, and no frame at all reaches another
application's secrets. None of that is a check the service performs; there is no
request shape to refuse.

A sealed write is **immediate**. There is no `VaultCommit` and nothing is
staged: the service opens the sealed document, applies the one change, re-seals
it, and publishes it before it replies. Two reasons, both the sealed scope's
own. Plaintext secret material exists in the service for the span of one request
rather than for the life of a staging session; and because the service serves
requests one at a time, the whole read-modify-seal-publish is atomic — so two
processes of one application sealing different secrets cannot lose each other's,
which a stage-then-commit pair would allow.

A vault that cannot be opened is **refused**, never answered as an empty one: a
damaged record, a failed authentication, or missing key material are each a
distinct audited refusal, because "your secrets are damaged" and "you have no
secrets" must not look alike to an application deciding whether to prompt.

The scope has no layer beneath it — no bundle-shipped defaults, no machine-wide
policy — for a reason of its own beyond the published scope's: a secret an
application did not write is not one it may be made to believe, and a layer
would let an administrator or a package plant one.

### What protects the master secret

The master secret is stored as drawn, in the gated store root: owned by this
service, mode `0700`, gated on `CAP_APPDATA_ADMIN`. At rest it is protected by
the encrypted root volume, which ARXFS has no plaintext mode for. So secrets are
exactly as safe at rest as the volume, they survive an administrative password
reset, and an application — including the account's own shell — cannot reach the
record at all.

It is deliberately **not** wrapped under a second key today, and there is
deliberately no protector abstraction: at this stage there is no second secret
to wrap it with, so a wrap would have to use a key derivable by anyone who could
read the record, which is theatre rather than defence. A login-passphrase
protector (secrets locked while logged out) and TPM sealing
(`plans/TPM.md`) each bring a real second secret; the record carries a version,
and the stage that brings the first of them reshapes it in place to carry a
keyslot.

A record that attests nothing is **never replaced**. The same master secret is
every application's key material in that account, so drawing a fresh one to get
past a damaged record would strand every existing vault while looking like a
clean start. The refusal is audited and an operator restores the record.

The service caches no master secret: it is read afresh for each sealed
operation, used, and wiped. A vault write is a rare, human-driven act, and
holding an account's key material in the service's heap for the life of the
machine would buy a file read it does not need.

## The tree it serves from

```text
/Users/<u>/Settings/Apps/                 ← gated: required_cap = CAP_APPDATA_ADMIN
    .vault-master                           the account's app-data master secret
    <bundle-id>/
        settings.conf                       the app's own private document
        public.conf                         what the app publishes; any app may read
        secret.vault                        the app's secrets, sealed
        .owner                              the publisher ownership pin
/System/Settings/<bundle-id>/settings.conf  optional machine-wide policy layer
```

The master-secret record sits in the gated root beside the per-app directories,
because it is the *account's* key material and every application's vault key is
derived from it. Its leading dot cannot be a bundle identifier — the identifier
grammar forbids one — so no application's store can ever be named that. It is
under `Settings/` rather than `Library/` because `Library/` holds the evictable
and boot-reaped scopes, and key material may be in neither.

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

One ownership pin governs all three scopes: it records who owns the *data*, and
every document is the same application's. A different developer claiming the
bundle identifier is therefore refused before it can put anything in front of
readers of the real application's published document — and before it reaches a
key derivation at all.

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

Layering is the **private** scope's alone. The published scope is exactly one
document, and that is structural rather than a simplification: the service
cannot name a bundle's own directory — nothing attested gives it one — so a
bundle-shipped published document could never be read on the foreign path, and a
layer that worked only for the publishing application would mean two
applications disagreeing about what a third publishes. A machine-wide layer is
excluded for a second reason: a reader must be able to attribute a published
value to the application, and an administrator must not be able to make an
application appear to say something it never said.

For the private scope, lowest precedence first:

1. `/System/Settings/<bundle-id>/settings.conf` — the optional machine-wide
   administrator policy. Read-only at runtime, and readable from an app's very
   first launch, before it has ever written anything of its own.
2. The user's own document — overrides only.

A commit writes **only** layer 2, so a user's file never absorbs a policy value
it did not set, and unsetting an override falls back to the policy layer rather
than to nothing. The bundle's own `DefaultSettings/` is a third layer below
both; it is applied by the client library, which is the only principal that can
name its own bundle without a lookup.

The document a read is *served* is canonical — one line per setting, no
comments, no duplicates — because it is two layers made one and the caller
parses it rather than editing it. The app's own file on the volume keeps its
comments, its ordering, and its hand-edits untouched; only this view of it is
normalised.

## Atomic publish, and what a crash leaves

A publish renders the document, writes it whole to a sibling temporary — the
live name plus `.new`, so each scope has its own and no two can contend —
flushes it, and renames it over the live document. A crash therefore leaves
either the old document or the new one — never a torn one. A failed rename
leaves the old document live and the temporary behind; the next publish
overwrites the temporary, so a retry converges with no repair pass.

The same replacement serves the sealed document and the account's master-secret
record, which is what stops a torn write from producing a record that attests
nothing — and because such a record is never replaced, a torn write would
otherwise strand the account's vaults for good.

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
Drawing randomness needs no capability at all, so the sealed scope adds nothing
to the ceiling.

Compromising it yields applications' settings **and their secrets** — it is the
principal that holds every account's master secret, and no arrangement in which
one service can seal and open vaults avoids that. What it does *not* yield is
anything outside the app-data tree: not a credential record, not another user's
files, not a process.

## What it does not isolate

A malicious application running as the user can already delete or rename the
user's own directories — including `~/Settings` — because it shares their uid.
That is the pre-existing consequence of one uid per account, and this service
neither creates nor removes it: a decoy tree is refused by the ownership check,
so the worst outcome is that app data becomes *unavailable* and audited, never
readable or forgeable. Confidentiality and integrity of one app's data against
another's is the boundary this service provides; availability of the user's own
home against their own apps is not a boundary any service can create.
