# APPDATA.md — Per-app settings, secrets, blobs and temporary files, gated by attested app identity

Binding under `AGENTS.md`. Read `AGENTS.md` and `PLAN.md` first; every rule in
both applies here without exception.

This plan replaces the current "every app invents its own file under
`Settings/`" arrangement with **one** OS-provided app-data service: a per-app
private configuration store keyed on the app's signed bundle identifier, an
opt-in public scope, a sealed secret scope, a descriptor-backed blob store for
bulk data, and per-app temporary files — all reachable only through an API
that gates on the **kernel-attested identity of the calling app**, not on
anything the caller says about itself.

It exists as a plan because it adds a new `lib/abi` service ABI, a new system
service, a new capability, a signed-manifest field, and an amendment to the
authoritative filesystem layout — each of which `AGENTS.md` §3/§5.2/§9/§16.4
requires be proposed and approved in a plan file **before** any of it is built.

---

## 1. The defect

### 1.1 There is no isolation between apps' settings

Every app today writes a file of its own choosing under the launching user's
home, under its own authority, with `CAP_FS_ACCESS`:

| Consumer | Path it invents |
|---|---|
| `userland/apps/terminal` | `<home>/Settings/Terminal/terminal.conf` — **migrated** (AD5) |
| `userland/apps/fstree` | `<home>/Settings/fstree/config` (`settings.rs`) |
| `userland/gui/session` (writes) / `userland/apps/wallpaper` (reads) | `<home>/Settings/Pinboard/pinboard.conf` — **two programs, one hand-rolled path** (§1.5) |
| `userland/apps/applib` | `<home>/Settings/ProgramLibrary/library.conf` |
| `userland/system/init` | `<home>/Settings/Services/` |

Because the whole tree is owned by the user and every app runs as that user
with `CAP_FS_ACCESS`, **any** app the user launches can read and rewrite
**every** other app's settings. A malicious or merely buggy app can read the
terminal's profile, rewrite the service enrolment store, or corrupt the
pinboard. The filesystem permission model (§5.3) cannot fix this: it keys on
uid, and all of a user's apps share one uid. **App-from-app isolation within a
single user is not expressible with per-inode owner/mode/ACL at all** — which
is precisely why a gating service is required rather than a tighter mode bit.

### 1.2 The naming is ad-hoc and collides

The directory component is whatever the app picked — `Terminal`, `fstree`,
`Pinboard`, `ProgramLibrary` — inconsistently cased, unnamespaced, and
first-come-first-served. Two independent developers who both ship a `Notes`
app collide. The charter's own spelling (§16.5, "copied into the user's
`/Users/<u>/Settings/<Name>/`") keys on the bundle's display *name*, which is
neither unique nor stable.

Meanwhile the signed manifest **already** carries a globally unique,
developer-namespaced identifier — `id = "os.tairix.terminal"`,
`AppInfoHeader::id` (`lib/abi/src/appinfo.rs:658`) — and nothing uses it to
key storage.

### 1.3 The format engine is copied per app

Three separate hand-rolled `key=value` parsers exist (`lib/sysconfig`,
`userland/apps/fstree/src/settings.rs`, `lib/wallpaper`), each with its own
tolerance rules, its own bounds, and its own idea of what a malformed line
means. §2.2 (define shared data once) and §2.3 forbid the fourth copy.

None of them preserves comments or unrecognised lines on rewrite, so an app
that saves its settings silently destroys whatever the user hand-edited in.

### 1.4 There is no attested app identity to gate on

The kernel-attested `Origin` (`lib/abi/src/origin.rs:262`) carries
`trust_domain, uid, gid, pid, proc_id, capabilities, console` — **no app
identity**. A service receiving a request over IPC can attest *which user* and
*which process instance* is calling, but not *which application*. So no
service can implement "only `os.tairix.terminal` may touch
`os.tairix.terminal`'s settings" today, however it is written.

The information exists and is already verified: the kernel runs the signed
load gate at spawn (`kernel/core/src/appspawn.rs`, `lib/appload`), which
checks the Ed25519 manifest signature and the bundle content hash, and it
already retains other attested facts about the admitted program on the
process's capability record (`spawn_path`, `credential`, `name` —
`kernel/core/src/syscalls.rs:10202`). The verified bundle identity is simply
dropped on the floor instead of being retained beside them.

### 1.5 Isolation with no sanctioned sharing channel re-creates §1.1

Two of a user's applications sometimes have to agree on a value — a terminal's
font and an app that embeds one, a chooser and the surface it configures. The
tree already has one: the desktop session owns and writes
`~/Settings/Pinboard/pinboard.conf`, and the wallpaper chooser — a separate
bundle — opens that path directly under `CAP_FS_ACCESS` to show the user what is
currently applied. (Its *write* already goes through the session's own endpoint,
so only the read is unmediated — which is exactly the half a published scope
replaces.)

A store that *only* isolates therefore does not remove §1.1; it relocates it.
An application that must publish a value to another would be left with the one
route it already has — an invented path written under `CAP_FS_ACCESS`, readable
and writable by every other app of that user — which is precisely the
arrangement this plan exists to delete. Isolation is only complete when the
store also provides the sanctioned channel, so that "reach another app's data"
has exactly one shape, that shape is opt-in by the *publishing* app, and it
cannot be widened into the private scope.

### 1.6 There is nowhere to put secrets, bulk data, or temporary files

- **Secrets.** An email client has nowhere to keep a password. `lib/crypto`
  has the primitives (ChaCha20-Poly1305 `seal`/`open`, PBKDF2, and
  `derive_key` — single-block HKDF-Expand, RFC 5869) and ARXFS has a per-volume
  key hierarchy, but there is no per-app, per-user sealed store — **closed
  (AD7)**.
- **Bulk data.** A config file is the wrong shape for a mail index or a
  thumbnail cache, and the IPC payload ceiling is 1 MiB
  (`IPC_MESSAGE_MAX_PAYLOAD_LEN`), so no store that proxies bytes through
  messages can serve large data at all, let alone with random access.
- **Temporary files.** There is no `/tmp` and the OS never creates one (§16.1,
  correctly). Apps therefore have no sanctioned scratch space, so scratch
  files land in the user's real directories with no isolation and no reaping.

---

## 2. Scope and decisions (binding for this plan)

- **Isolation is enforced by a service, not by file modes.** Per-inode
  owner/mode/ACL keys on uid and cannot separate two apps of the same user
  (§1.1). One system service (`confd`) owns the store tree and is the only
  principal that can reach it; every app reaches its own data only through
  the service's ABI. This is the justification for the service existing at
  all — it is not a convenience wrapper over the VFS.
- **The store is keyed on the signed bundle id, never on a display name.**
  `os.tairix.terminal`, `org.pty.widgets`. The id comes from the
  kernel-attested manifest, so it is unforgeable and unique; the display name
  is neither.
- **An app never names its own store.** The daemon derives it from the attested
  `Origin`. A request carries a bundle id **only** to name a *foreign* app's
  published scope, and that request is a **distinct operation with no scope
  field** — so a frame that names another app cannot ask for its private
  document at all. There is no request shape by which an app can claim to be
  another app.
- **The sealed scope is not a configuration scope, and its writes are not
  staged.** Secrets are reached by operations carrying no scope field and with
  no foreign counterpart, so "a configuration frame naming a secret" and "any
  frame naming another app's secrets" are unrepresentable rather than refused.
  A sealed write is applied before the reply — no staging, no commit — which
  keeps plaintext in the daemon for one request instead of a session's lifetime
  *and* makes the read-modify-seal-publish atomic, because the daemon serves one
  request at a time. A stage-then-commit pair would let two instances of one app
  lose each other's secrets. See §3.6, §3.7.
- **A vault that cannot be opened is never an empty vault.** A damaged record, a
  failed authentication, and absent key material are each a distinct audited
  refusal. An application deciding whether to prompt for a password must be able
  to tell "damaged" from "none saved"; conflating them is how a store silently
  loses a user's secrets.
- **The published scope is one document, with no layer beneath it.** Not a
  simplification: `confd` cannot name a bundle's own directory (§3.4), so a
  bundle-shipped published document could never be read on the foreign path, and
  a layer that worked only for the publishing app would mean two apps
  disagreeing about what a third publishes. A machine-wide layer is excluded for
  a second reason — a reader must be able to attribute a published value to the
  *application*, and an administrator must not be able to make an app appear to
  say something it never said. Layering is the private scope's alone.
- **A foreign read is never an oracle.** An app that publishes nothing, one that
  has never run for the account, and one whose store cannot be attested all
  answer the same empty document, audited. Only the caller's *own* refusals — no
  home, a root the service does not own, an unreachable volume — are reported as
  themselves, because only those are worth a retry. So a reader learns what an
  app chose to publish and nothing else.
- **Ownership is pinned to the developer, not to the build key.** The stable
  developer identity is a **publisher key** declared in the signed manifest; a
  per-build **signing key** is certified by it. An app update signed with a new
  signing key keeps its settings; a different developer claiming the same
  bundle id is refused. See §3.2.
- **Format is `key = value` lines, not JSON.** The requirement is that a human
  can edit the file and that bad formatting is survivable. JSON fails the
  second outright — one trailing comma loses the document — and the house
  already has three `key=value` parsers to consolidate (§1.3). Structure comes
  from dotted keys (`effects.blur`), not from nesting syntax. See §3.3.
- **A rewrite never destroys what a human wrote.** Comments, blank lines, key
  order, and lines the parser did not understand survive a `set` and a save;
  only the touched key's line changes. This is a hard requirement of the
  format engine, not a nicety.
- **The daemon is on the control path, never the bulk data path.** Blob and
  temp access is granted as a **descriptor** (`fd_grant`/`fd_redeem`), so the
  app then reads, writes, seeks, truncates, and `file_map`s directly against
  the kernel VFS at full speed. Proxying bytes through IPC is excluded by the
  1 MiB message ceiling and would be slow besides.
- **Exactly one new capability.** `CAP_APPDATA_ADMIN`, held only by `confd`.
  It is tested against §5.2's three conditions in §3.5.1. `confd`'s whole
  ceiling is that plus `CAP_IPC_BIND_PRIVILEGED`, `CAP_FS_ACCESS`, and
  `CAP_LOG_EMIT`: no spawn, no network, no `CAP_FS_CHOWN`, and — deliberately —
  no `CAP_USERS_READ`, so a compromised store service cannot reach the
  credential database (§3.4).
- **Fail closed, degrade to defaults.** No `confd` (early boot, crashed
  service, a port with no storage) means reads answer from the bundle's
  shipped defaults and writes fail with a typed error — never a guess, never a
  silent partial write.
- **No compat shim (§2.13, §23.3).** Each migrated consumer's hand-rolled path
  is **deleted** in the stage that migrates it. There is no dual-read, no
  fallback to the old location, and no importer left behind. `abi-v1` is not
  frozen (§9), so the manifest and `Origin` changes are made in place.
- **Out of scope, deliberately.** Publisher-key *rotation* (as opposed to
  signing-key rotation) is an audited administrative re-pin, not a manifest
  lineage format (§3.2). Cross-user sharing, sync/replication, and a settings
  GUI are not in this plan. A per-descriptor byte ceiling is not in this plan
  and its absence is recorded as a known limit (§3.8). Sealed *bulk* data is not
  in this plan either: a secret crosses the wire as a setting's value and is
  bounded by it (§3.7).

---

## 3. Design

### 3.1 Shape

```
        app process                         confd (CAP_APPDATA_ADMIN)
   ┌──────────────────────┐            ┌───────────────────────────────┐
   │ lib/appdata (client) │──ipc_call──▶│ attest Origin  ──▶ app id     │
   │  Settings / Vault    │◀──reply────│ pin check      ──▶ publisher   │
   │  Blobs / Temp        │            │ lib/appconf    ──▶ merge/parse │
   │                      │            │ lib/crypto     ──▶ seal/open  │
   └──────────┬───────────┘            └───────────┬───────────────────┘
              │                                    │ owns, exclusively
              │  fd_redeem (blobs, temp)           ▼
              └───────────────▶ direct fs_read / fs_write / file_map
                                on /Users/<u>/{Settings,Library}/<id>/
```

### 3.2 Attested app identity (the foundation)

**Manifest.** Two fields are added to `AppInfoHeader` (`lib/abi/src/appinfo.rs`):

- `publisher_pubkey: [u8; 32]` — the developer's stable Ed25519 identity.
- `publisher_cert: [u8; 64]` — proof that the publisher delegated to this
  build's signing key.

The rule, `AppInfoHeader::publisher_binding()`, enforced in `lib/appload`'s
authenticity step as a fourth check beside the existing signature and
content-hash checks:

- Self-published: `publisher_pubkey == signer_pubkey` and `publisher_cert` is
  all zero. No second signature is checked — the trust root that admitted the
  signer admitted the publisher, because they are one key.
- Delegated: `publisher_cert` must verify, against `publisher_pubkey`, as the
  publisher's signature over `AppInfoHeader::publisher_cert_message()`.
- Anything else — a claim with no certificate to justify it, a certificate
  with nothing to delegate — refuses the bundle.

The certificate's message is fixed-width and domain-labelled:
`"tairix-publisher-cert/v1" ‖ 0x00 ‖ id_len ‖ id[64] ‖ signer_pubkey`. Naming
the *identifier* as well as the key is what stops a certificate issued for one
of a publisher's bundles being lifted into another; the label stops a
signature the publisher key made for any other purpose being replayed as a
delegation. The one definition is shared by the signing tool and the load
gate, so the two can never sign and verify different messages.

Both fields are inside the region the existing manifest signature already
covers, so neither can be swapped behind a valid signature.

The **publisher id** (`PublisherId`) is
`SHA-256("tairix-publisher-id/v1" ‖ 0x00 ‖ publisher_pubkey)` — the stable,
32-byte developer identity the store keys ownership on. It is a digest rather
than the key itself for two reasons: it stays one fixed width whatever the
publisher key's algorithm (the on-disk pin and the vault KDF context are
formats, and should not be spelled in terms of one signature scheme), and it
is a value no consumer can mistake for something it may verify a signature
with. `PublisherId::NONE` (all zero) is the sentinel for a principal with no
attested publisher; no real identity can collide with it.

`lib/abi` has no dependencies and so no hash, which fixes where the pieces
live: `lib/abi` defines the two labelled preimages and the shape rule,
`lib/appload` completes the derivation and the certificate check. Neither goes
behind the loader's `Verifier` seam, because neither carries host policy —
that seam exists for the *trust root* of the manifest signer, which genuinely
differs between the kernel's embedded anchor and a user-space installer.
Putting a derivation that keys on-disk state behind a per-host trait would
invite two hosts to key the same developer's data differently.

Decoding a manifest deliberately judges **none** of this, exactly as it
verifies no signature: a surface that only draws a bundle's name and icon
(the file manager, the taskbar, the program library) must not need a trust
anchor to read a manifest. Authenticity is the gate's.

This is what makes "the signing may change for a new app version" work: the
per-build signing key is free to rotate on every release, because the store
pins the *publisher*, not the signer.

**The first-party build delegates.** `SYSTEM_APP_PUBLISHER_SEED` is a pinned
seed distinct from `SYSTEM_APP_SIGNING_SEED`, and every bundle the image build
composes is *delegated*, not self-published. This is deliberate on three
counts: the two keys answer different questions (may this run here? whose app
is this?) and only one of them is pinned by the kernel's embedded anchor, so
only one of them costs a reflash to rotate; a first-party signing-key rotation
must not silently re-key every user's store, and that property is only real if
it is exercised; and it puts the certificate path — the one a third-party
release uses — on the path every boot takes, rather than leaving it a form
only tests reach.

Nothing is added to `AppInfo.toml`. There is exactly one first-party
publisher, the composer derives it from the one pinned seed, and a key
repeated verbatim across seventy manifest sources is precisely the duplication
the charter forbids. A third-party bundle names its own publisher through its
own signing tooling; a manifest *source* key with one legal value would be
bloat.

**The identifier is a name in a directory, so its grammar is a security
rule.** `validate_bundle_id` is the one definition: dot-separated segments of
ASCII lowercase letters, digits, `-` and `_`, each non-empty. It is applied
when a manifest decodes and again wherever an identifier crosses a trust
boundary. Nothing that could be a traversal (`.`, `..`, `/`), a hidden entry,
a case-folding collision on a case-insensitive volume, or a control character
can be spelled at all — so "an app cannot reach outside its own scope" holds
by construction rather than by a check some caller might forget.

**Retention.** `LoadedApp` carries a `BundleIdentity` (identifier, name,
version, publisher). The deferred-load path turns the verified identifier and
publisher into an `AppIdentity` and snapshots it onto the child's
kernel-attested capability record (`kernel/sec/src/captable.rs`, beside
`spawn_path`/`credential`/`name`) — from kernel-verified state at the call
site, never from caller-supplied bytes. It travels beside the manifest
capability request as one `VerifiedProgram`, because both come from the same
verified manifest and there must be no path that installs one without the
other.

**Exposure.** `Origin` (`lib/abi/src/origin.rs`) gains
`app: Option<AppIdentity>`, filled by `TaskCapabilities::attest_origin()`.
Every existing consumer of `call_peer_origin` and `self_origin` therefore gets
app identity for free, with no new syscall.

The two halves are **one value, whole or absent** — not two independently
empty fields. A half-filled identity (an identifier with no publisher, or the
reverse) is not representable in memory and is a refusal on the wire, so a
store can never be keyed on something nothing owns.

A principal with no bundle identity — a kernel thread, a boot principal, a
parser-sandbox child — carries no `AppIdentity` and gets **no** store: the
daemon refuses it. A sandbox child additionally has every capability stripped
structurally and cannot reach the endpoint at all; its *attribution* survives
the strip, because an audit consumer still has to be able to say what it did.

**Known consequence: a boot-floor program has no identity.** The embedded
program registry carries an `rxe` and a capability request, not a signed
manifest, so a program the kernel launches from it cannot be attributed to a
publisher and gets no store. That is honest rather than convenient: there is
nothing to attest. On the aarch64 port only PID 1 is embedded and every other
program is a store bundle, so the consequence is confined to the ports whose
storage floor has not landed yet (`plans/ARCHSUPPORT.md`); it is not worked
around here, because the fix is those ports' storage floors, not a fabricated
identity.

`confd` itself is on the embedded floor of those ports (a registry row beside
`fontd`'s), so the *service* runs identically everywhere and one boot sequence
covers every target. Its callers there are embedded programs with no signed
manifest, so it correctly serves them nothing — the consequence above, observed
rather than papered over. Leaving `confd` off that floor would instead have
made PID 1's startup config fail a spawn on those ports, which is a worse
answer: a service that is absent for a reason no log explains.

**Known consequence, stated plainly.** A shell script has no identity of its
own; it runs under the identity of the shell that interprets it, and shares
the shell's store. Scripts are not a security boundary here and this plan does
not pretend otherwise.

**Ownership pin (TOFU).** Each app's store root holds an `.owner` record —
readable only by `confd` — carrying the publisher id that created it. On every
open the attested publisher id is compared against the pin: equal proceeds,
different refuses with a typed error and an audited log record. A first write
creates the pin. Publisher-key rotation is an audited administrative re-pin
through the daemon's control surface, not a manifest field (§2).

### 3.3 The configuration format (`lib/appconf`)

One new `no_std` crate, no I/O, host-testable, fuzzed. Grammar:

```
# The user's own comment survives a save.
scheme      = dark
font.size   = 14
effects.blur = 0.5
recent.0    = /Users/ada/Documents/notes.txt
greeting    = "  leading space, a # sign, and \n an escape "
```

- One `key = value` per line. Whitespace around `=` and at line ends is
  ignored. `#` starts a comment to end of line; inside a quoted value it is
  literal.
- Keys are dot-separated lowercase segments (`[a-z0-9][a-z0-9._-]*`), bounded
  in length and depth. Dotted keys give structure without nesting syntax a
  hand-editor can get wrong.
- Values are bare (trimmed) or `"quoted"` with `\\ \" \n \t` escapes, for
  values that need leading/trailing space, a `#`, or a newline.
- **Tolerance.** A line that is not a valid `key = value` is *retained
  verbatim*, reported as unparsed, and never aborts the read. A duplicate key:
  last wins on read, collapsed on rewrite.
- **Preservation.** The engine models a document as an ordered line list, not
  a map, so `set` rewrites one line and leaves comments, ordering, and
  unparsed lines untouched (§2).
- Typed accessors (`bool`, `u32`, `i64`, permille) so each app validates once
  at the edge rather than re-parsing strings. A read distinguishes three
  answers — absent, valid, and *present but not that type* — so an app can
  report a broken value instead of silently substituting a default. An
  enumerated or bounded-string setting needs no accessor of its own: `get`
  hands back the already-bounded text and the app's own `from_key` closes the
  set, which is one function rather than a wrapper per shape.
- **Fractions are permille integers, not decimals.** A permille round-trips
  through text exactly, needs no float parser in a `no_std` build, and is
  already how the shipped effect strengths are expressed.
- **Every bound is a fixed security bound, not a derived capacity.** This
  corrects an error in an earlier draft of this plan, which cited §24.1: that
  section governs resource *capacities*, and §24.4 names "config length caps"
  explicitly as validation bounds on untrusted input that must stay fixed and
  fail closed. Deriving them from discovered RAM would mean a bigger machine
  accepts a bigger hostile document, which is a security regression, not
  flexibility. The document-level bounds (`MAX_DOCUMENT_LEN`, `MAX_LINES`,
  `MAX_SETTINGS`) refuse a document whole; the per-line bounds
  (`MAX_KEY_LEN`, `MAX_KEY_DEPTH`, `MAX_VALUE_LEN`) make an over-long line
  *unparsed* rather than a setting that means something else.

`lib/sysconfig`'s closed-registry system store keeps its own stricter
fail-the-whole-document semantics and is **not** folded in — that is a
deliberate difference, recorded here so the two are not later "unified" by
mistake. It also keeps its own comment tokenisation
(`tairix_util::conf::strip_comment`, which cuts at the first `#`
unconditionally): here a `#` inside a quoted value is a literal, so the
tokenisation has to know about quoting and therefore lives with the grammar
that has quotes. Two grammars, each with one implementation — not one grammar
with two.

### 3.4 Layout and layering

Within the fixed home shape (§16.3 — apps may not invent siblings):

```
/Users/<u>/Settings/Apps/               ← configuration (gated tree)
    .vault-master                         the account's app-data master secret
    <bundle-id>/
        settings.conf                     private scope
        public.conf                       public scope (foreign apps may read)
        secret.vault                      sealed scope (AEAD)
        .owner                            publisher pin
/Users/<u>/Library/Apps/<bundle-id>/    ← bulk + volatile (gated tree)
    Blobs/<name>                          app blob store
    Cache/                                evictable
    Temp/                                 reaped at session end and at boot
```

The master-secret record sits in the gated root beside the per-app directories,
because it is the *account's* key material and every app's vault key derives from
it (§3.7). Its leading dot cannot be a bundle id — the identifier grammar forbids
one — so no app's store can be named that. It is under `Settings/` rather than
`Library/` because `Library/` holds the evictable and boot-reaped scopes, and key
material may be in neither.

**The gate sits on the fixed `Apps` parent, not on the per-app directory.**
This corrects an earlier draft of this plan, which put the store directly under
`Settings/<bundle-id>` and gated "both roots". Two things were wrong with it.

- A per-app directory *cannot* be the gate. Every app of a user may write
  `Settings/`, so any of them could pre-create a sibling named after another
  app's identifier — with whatever mode it liked — and have `confd` walk into
  it and serve forged settings. The gate must sit on a parent no app can
  create.
- Gating `Settings/` and `Library/` themselves is not available: `Library/`
  already holds the user's own `Trash/` (`lib/browse`), which the file manager
  must reach under the user's own identity.

So one fixed name, `Apps`, in each of the two trees. It is provisioned **with
the account** by all three home provisioners (`tools/mkimage`,
`kernel/tairix-kernel::user_admin_backing`, and the integration fixture) from
one shared definition in `lib/users/src/policy.rs`, and carries:

- owner = the `confd` service account, mode `0700`, and
- `required_cap = CAP_APPDATA_ADMIN` (§5.3's existing mechanism).

Both halves do work. The capability refuses every principal but `confd` —
including the owning user, whose shell is one more unattested principal. The
*ownership* is what makes a planted decoy harmless: `confd` verifies the root's
owner on every open, and a directory an app created is unreadable to `confd`
rather than trusted, because an app cannot chown to a service uid without
`CAP_FS_CHOWN`. A capability check alone would not catch that — `confd` holds
the capability either way.

The data stays in the user's home, so per-user backup and the volume's at-rest
encryption still apply, while `confd` is the only reachable path to it.

**Reaching the root needs a transit grant.** `confd` runs as its own service
uid, and `/Users/<u>` and `/Users/<u>/Settings` are mode `0700` owned by the
user, so `confd` cannot traverse them at all. Each carries one ACL entry
granting **search only** (`0b001`) to `confd`'s uid — the least authority that
lets a walk reach the root. It cannot list a home and cannot open any child
whose own record refuses it, so `confd` still reaches nothing but the root it
owns.

**Resolving which home.** `confd` maps the attested uid to a home by listing
`/Users` and matching the owning uid, **not** by reading the credential
database. That keeps `CAP_USERS_READ` out of its ceiling — `users_db_read`
hands over the whole `users-v1` text including PBKDF2 records, so a compromised
`confd` could otherwise exfiltrate password material for offline cracking. It
is also the more honest answer: the store must be beside the account's actual
files, and §16.3 fixes `/Users` as the only place user-owned files live. A uid
with no owned directory there gets no store, audited.

The scan is paid **once per account**, not once per read: a settings read must
not carry a directory listing that grows with the number of accounts. What is
remembered is only the resolved *path* — the ownership that authorises it is
re-checked on every use, so reassigning a home (the one act that could
invalidate a resolution, and it needs `CAP_FS_CHOWN`) cannot make a stale entry
serve the wrong account's store. Only successful resolutions are remembered, so
an account created while the service is running still resolves.

**Read layering** is the *private* scope's (§2: the published scope is one
document), lowest to highest precedence:

1. `<Bundle>.app/DefaultSettings/` — the defaults the bundle ships (§16.5).
2. `/System/Settings/<bundle-id>/` — optional machine-wide administrator
   policy.
3. The user's own file — overrides only.

Writes always land in layer 3, so a user's file never absorbs a policy value it
did not set and an unset falls back to layer 2 rather than to nothing. Defaults
are read as a **fallback layer**, not copied on first launch as §16.5 said: an
app update then ships new defaults that take effect immediately, and the user's
file holds only what the user actually changed. This is a charter amendment
(§4).

**Layers 2–3 are the daemon's; layer 1 is the client's.** The daemon reads
layer 2 and 3 from fixed paths it composes itself. Layer 1 needs the *bundle's*
path, and nothing attested gives `confd` one: `argv[0]` is caller data, and
scanning the program stores for an identifier would be a lookup that can
disagree with what is actually running. The client, by contrast, is the app —
it can read its own bundle with no lookup at all, and a wrong layer-1 value
could only ever mislead the app about itself, so no boundary is crossed. Layer 1
therefore lands in `lib/appdata` (AD5) rather than in `confd` (AD4), which is
also what makes the "no `confd`, degrade to shipped defaults" path (§2) one code
path instead of two.

### 3.5 A prerequisite the gate did not have: the capability guards the *name*

A `required_cap` on an inode governed *access to* the inode but not removal of
the name that holds it. `unlink`, `rmdir`, and `rename` authorise the **holding
directory's** write permission, and a same-parent rename never authorises the
moved node at all (POSIX only requires that when the parents differ). So any
principal that could write the parent could rename a gated directory aside and
create an ungated one of the same name — defeating the gate without ever
opening it, and leaving the gate's own service walking into the replacement.

That is a defect in the §5.3 model itself, not something this plan introduced:
it applies to every `required_cap` inode in the system. It is fixed here rather
than worked around, because a gate this plan depends on being unbypassable must
actually be unbypassable (§2.17).

The rule added: **a mutation that removes, replaces, or re-points a name whose
occupant carries a `required_cap` requires that capability** — `unlink`,
`rmdir`, and `rename` at either end, since replacing a name destroys its
occupant. It lands in both policy paths (`kernel/core/src/fs/delegate.rs` for
driver-backed mounts, `kernel/core/src/fs/vfs.rs` for the in-RAM tree) and is a
§5.3 charter amendment (§4). A hard link needs no rule of its own: a second
name shares the one inode and therefore the one security record.

**What this still does not isolate, stated plainly.** An app running as the user
can rename `~/Settings` itself aside and create a fresh one — `Settings` is the
user's own directory and carries no gate. `confd`'s ownership check then refuses
the decoy, so the outcome is that app data becomes *unavailable* and audited,
never readable or forgeable. Availability of a user's own home against their own
apps is not a boundary any service can create (the same app could delete
`~/Documents`); confidentiality and integrity of one app's data against
another's is, and that holds.

### 3.5.1 `CAP_APPDATA_ADMIN` against §5.2's three tests

1. **Guards a class, not one object.** It gates the entire app-data tree of
   every user — every app's configuration, secrets, blobs, and temporary
   files — not one file or one method.
2. **Live holder and live enforcement point in the same change.** Introduced in
   the stage that ships `confd` (its sole holder) and the per-inode gate plus
   the endpoint bind that enforce it. Nothing is defined "for later".
3. **No existing capability expresses it.** `CAP_FS_ACCESS` is held by nearly
   every app in the system, so gating on it would grant every app exactly the
   reach this plan removes.

### 3.6 The ABI (`lib/abi/src/appdata_ipc.rs`, `APPDATA_ENDPOINT`)

A reserved well-known endpoint (added to `is_reserved_endpoint`, and
deliberately **not** seat-scoped — app data is not a property of a seat and a
headless machine serves it identically), bound by `confd` under
`CAP_IPC_BIND_PRIVILEGED` so a squatter cannot impersonate the store.
Fail-closed requests in the house style: unknown magic, version, opcode, or a
field the operation does not use left non-zero refuses rather than guessing.

| Request | Stage | Effect |
|---|---|---|
| `ConfigRead { scope, capacity }` | AD4, scoped AD6 | the caller's **whole** merged document for one own scope, or the length it needs |
| `ConfigSet { scope, key, value }` | AD4, scoped AD6 | stage a write |
| `ConfigUnset { scope, key }` | AD4, scoped AD6 | stage a removal |
| `ConfigCommit { scope }` | AD4, scoped AD6 | publish that scope's staged writes atomically |
| `PublicRead { bundle_id, capacity }` | AD6 | a **foreign** app's published document |
| `VaultRead { capacity }` | AD7 | the caller's whole **sealed** document |
| `VaultSet { key, value }` / `VaultUnset { key }` | AD7 | seal or remove one secret, **immediately** |
| `BlobOpen { name, mode }` | AD8 | descriptor grant handle for a blob |
| `BlobDelete { name }` / `BlobList { cursor }` | AD8 | manage own blobs |
| `TempCreate {}` | AD9 | descriptor grant handle for a fresh temp file |
| `QuotaGet {}` | AD8 | own usage and ceiling |

**A read answers with the whole document, not one key.** This corrects an
earlier draft of this plan, which specified `ConfigGet { key }` and a paged
`ConfigList`. Both were the wrong shape:

- A per-key read costs the daemon a **file read and a parse per key**. An app
  with a dozen settings paid a dozen of each at start-up, which is exactly the
  premature pessimisation §2.16 forbids — and the client already links the
  format engine, so it can parse once and answer every `get` locally.
- `ConfigList` then has nothing to do: a client holding the document can
  enumerate it without a call, so the paged listing, its cursor, its
  fixed-width key record, and the `validate_key_prefix` grammar it needed were
  all surface with no consumer. All four are gone.

The reply is a whole document **or nothing**: the request declares the caller's
reply-buffer capacity, and a document that does not fit comes back as the byte
count it needs (`ConfigDocument::NeedsCapacity`) with no body. A partly
transferred document is therefore not representable, so no caller can parse a
fragment as a store or splice two snapshots together.

What the served document is: the policy layer, the app's own settings over it,
and the caller's own staged edits over those, rendered *canonically* — one line
per setting, no comments, no duplicates. That normalisation applies only to the
view; the app's own file keeps its comments, its ordering, and the lines the
grammar refused, because a save must never destroy what a human wrote.

**The record is variable-width.** Also a correction: the draft specified
fixed-width requests, and a padded frame would put a kilobyte of zeroes on the
hot settings path for a request that is sixteen bytes. It is length-prefixed
instead, bounded by `APPDATA_MAX_REQUEST`, exactly as the `users_admin` codec
is.

**The `scope` field arrived with the second scope, not before.** AD4 had exactly
one scope, so a `ConfigScope` enum with a single variant would have been
interface built for a stage that had not landed — §2.3/§2.4 forbid it, and
§2.13 blesses changing the wire in place when the second scope arrives. AD6 is
that arrival: `ConfigScope { Private, Public }` is on every own-store request,
neither variant is zero (so an all-zero frame cannot decode as a scoped
operation), and the daemon's store surface takes the scope through one
scope→file-name mapping rather than composing a name per call site.

**A foreign read is a distinct operation, not a scope.** An earlier draft of
this plan listed `PublicSet` beside `PublicRead` as though both were opcodes.
They are not, and neither half of that was right:

- Setting one's own published scope is `ConfigSet { scope: Public }`. A second
  opcode would duplicate `ConfigSet` byte for byte to select a file — the scope
  selector *is* the mechanism, and the draft's `PublicSet` named the capability
  rather than a frame.
- Reading a *foreign* one is its own opcode precisely so that it carries **no
  scope field**. A shared `ConfigRead { scope, bundle_id }` would have made
  "another app's private document" a representable request refused by a check;
  as two operations it is not representable at all. The private scope is
  unreachable across applications by construction, which is the stronger
  property and costs one discriminant.

**Both bounds on a foreign read are the ones already there.** The identifier is
bounded and validated by `validate_bundle_id` at decode — the same grammar the
manifest and the attested identity use, applied because the value becomes a path
component — and the document is bounded by `APPDATA_DOCUMENT_MAX` and answered
under the same whole-or-nothing capacity negotiation as an own read. No
operation carries an identifier *and* a setting, so `APPDATA_MAX_REQUEST` is the
wider of the two shapes rather than their sum.

The ABI is structural only. It bounds the record's shape, its lengths, and its
text encoding; the **grammar** of a key and a value has one home,
`lib/appconf`, and the daemon applies it through that engine's own validators —
the `users_admin` precedent, where the semantic field rules live in `lib/users`.
The three byte bounds are the one exception, and they are not duplicated: the
key, the value, and a whole document all cross the wire as fields, so they are
`abi-v1` widths *and* the grammar's own bounds; `lib/abi` may have no
dependencies, so the single definition lives in `appdata_ipc` and `lib/appconf`
imports it.

**Writes are staged per calling process instance and per scope.** Keyed on the
attested `ProcId` — unforgeable and never reused — so two processes of one app
cannot publish each other's half-finished edits, and on the scope, so a commit
publishes the one document a rename can replace. A commit that claimed to
publish both scopes at once would be claiming an atomicity no filesystem offers;
an app with a settings sheet open while it publishes about itself therefore has
two independent pieces of unpublished work, and the pending-edit bound is per
scope so one cannot deny the other. A session holds only its *pending edits*,
not a whole document, so it is small; the document is loaded at commit. `ConfigRead` applies the caller's own pending edits over the merged
view, so a settings sheet reads back what it just set while every other
principal still sees the committed value. A commit renders, writes a sibling
temporary, flushes, and renames over the live document, so a crash leaves either
the old document or the new one.

No primitive tells a server that a peer died, so an abandoned session is
reclaimed by **age** (`STAGING_IDLE_NS`). Losing an abandoned session's edits is
exactly the contract already stated: a caller that never commits changes
nothing.

Every entry point follows §5.4: attest the caller, check authority before
touching state, validate every input, log the security-relevant decisions, fail
closed.

**The sealed scope is three opcodes, not a third `ConfigScope`.** It could have
been a scope: the machinery would mostly have worked. It is not, for the same
reason `PublicRead` is not a scope — the stronger property costs one discriminant
each way. With no scope field on a vault frame and no scope value naming a vault,
"a configuration request that reaches a secret" and "a vault request that reaches
a configuration document" are both *unrepresentable*, in either direction, rather
than refused by a check. And there is no foreign vault opcode at all, so "one app
reads another's secrets" is not a frame that exists.

It also keeps two vocabularies apart that will diverge. The sealed scope has
refusals the configuration scopes cannot have (a record that is not a vault, an
authentication that failed, key material that attests nothing) and, once a
passphrase protector lands, a *locked* state the configuration scopes will not
share. A shared `ConfigRead` would have had to answer for both.

**A sealed write is immediate, and there is no `VaultCommit`.** This is the one
place the sealed scope deliberately departs from the configuration scopes' shape.
The daemon opens the sealed document, applies the one change, re-seals, and
publishes before it replies. Two reasons, and the second is the decisive one:

- Plaintext secret material then exists in the daemon for the span of one request
  rather than for the life of a staging session that `STAGING_IDLE_NS` reclaims.
- The daemon serves requests one at a time, so the whole read-modify-seal-publish
  is **atomic**. A stage-then-commit pair is not: two processes of one app that
  each stage a different secret and then commit would each publish a document
  built from what *it* read, and the later commit would silently drop the
  other's secret. Immediate writes make that race unrepresentable, which is
  strictly *more* consistent than staging rather than a concession.

A caller that seals three secrets pays three round trips, which is the right
price for an operation an application performs when a human types a password.

Secrets are never enumerable or readable across apps — there is no foreign
equivalent of `PublicRead` for the sealed scope, and no scope field to mis-set.

### 3.7 Secrets (`secret.vault`)

ChaCha20-Poly1305 (`lib/crypto::seal`/`open`) over the same `lib/appconf`
document, so a secret is a `key = value` pair like any other.

Key hierarchy, mirroring the shape ARXFS already uses for volume keys:

```
per-account master secret         (32 random bytes, drawn once per account)
  └─ derive_key(master, "tairix-appdata-secret/v1" ‖ 0x00 ‖ publisher_id ‖ bundle_id)
       └─ per-(user, app) AEAD key
```

(`lib/crypto::derive_key` is the audited single-block HKDF-Expand the ARXFS key
hierarchy already derives its subkeys through — the same primitive, a distinct
context string, so no new cryptography is introduced (§2.12). Every field before
the identifier is fixed width and the label is fixed, so no two
(publisher, identifier) pairs can produce one context.)

Binding the derivation to the **publisher id** means a rotated signing key
still opens the vault, while a different developer squatting the bundle id
derives a different key and cannot read it even if the pin check were somehow
bypassed. Defence in depth, not a substitute for §3.2's pin.

**What the sealing buys, given that the gate already exists.** The key is bound
to the *application*, so the compromise that matters — a defect that lets one
app's request reach another's store directory — yields ciphertext it has no key
for. And the AEAD authenticates the record, so a damaged or altered sealed
document is *refused* rather than parsed: a vault that cannot be opened must
never read as "this application has no secrets", because an application deciding
whether to prompt for a password would take the wrong branch.

**What protects the master secret, and why there is no protector seam.** This
corrects an earlier draft of this plan, which specified a caller-supplied
"key-protector seam" with a volume-backed stage-1 protector. The seam was
interface built for a stage that has not landed, by the same rule §3.6 applies
to `ConfigScope`: AD7 has exactly one protector, so a protector enum with one
variant — and a salt, nonce, and tag with nothing to put in them — is forbidden
(§2.3/§2.4). Worse, it would have been dishonest: the stage-1 protector holds no
secret of its own, so the "wrap" could only have used a key derivable by anyone
who could read the record, which is theatre rather than defence.

What actually protects it is stated plainly instead. The master secret is stored
as drawn, in the gated store root (`.vault-master`): owned by `confd`, mode
`0700`, gated on `CAP_APPDATA_ADMIN`, on a volume ARXFS has no plaintext mode
for. So secrets are exactly as safe at rest as the volume, they survive an
administrative password reset, and an application — including the account's own
shell — cannot reach the record at all. The record carries a version and binds
the account's uid; the stage that brings the first protector with a real second
secret (a login passphrase, so secrets are locked while logged out; TPM sealing,
`plans/TPM.md`) reshapes it in place to carry a keyslot, which is exactly the
in-place evolution §2.13 blesses.

**A record that attests nothing is never replaced.** The same master secret is
every application's key material in that account, so drawing a fresh one to get
past a damaged or absent record would strand every existing vault while looking
like a clean start. It refuses, audited, and an operator restores it. For the
same reason the record is written through the atomic replacement every other
document uses: a torn write would produce exactly the record that can never be
replaced.

**No master secret is cached.** It is read afresh for each sealed operation,
used, and wiped. A vault write is a rare, human-driven act, and holding an
account's key material in the service's heap for the life of the machine would
buy a file read it does not need.

**Plaintext lifetime.** A sealed write is not staged (§3.6), so plaintext secret
material exists in the daemon for the span of one request. The transient buffers
that carry it are wiped: `lib/appconf` wipes every line a document discards and
every line of a document that goes out of scope (which is what makes the format
engine, not its callers, responsible for it), the rendered text is sealed in
place so the plaintext allocation *becomes* the ciphertext, and the service
binary wipes the request and reply frames it reuses across callers.

**Known limit, stated honestly.** A secret crosses the wire as a setting's
value, so it is bounded by `APPDATA_VALUE_MAX` (1 KiB). That covers a password,
a token, or a symmetric key, and not a certificate chain or a large private key.
Sealed *bulk* data is not in this plan: AD8's blobs are descriptor-backed and
unsealed, and giving them a sealed mode is a separate decision with its own
nonce discipline, not a widening of this bound.

### 3.8 Blobs and temporary files

`BlobOpen` and `TempCreate` return a **grant handle** minted by `fd_grant`,
scoped to the calling task and useless to a bystander. The app `fd_redeem`s it
and then uses `fs_read`, `fs_write`, `fs_truncate`, and `file_map` directly.
That gives streaming, random access, and memory mapping at kernel VFS speed,
with `confd` making the policy decision once at open and never touching a byte
of payload.

Temp files live in `Library/<bundle-id>/Temp/`, are reaped at session end and
at boot, and are gated identically — an app's scratch data is no more visible
to another app than its settings are. There is still no `/tmp` and nothing here
creates one (§16.1).

**Quotas.** Per-app blob count and byte ceilings, and a per-user total, all
derived from the resource profile rather than fixed constants (§24.1), checked
at open-time admission.

**Known limit, stated honestly.** Once an app holds a writable descriptor it
can grow that file until the *user's* filesystem quota stops it; the per-app
byte ceiling is therefore an admission check, not a hard per-descriptor cap. A
true per-descriptor byte ceiling would need a VFS change, which is out of
scope for this plan and is recorded here rather than papered over.

### 3.9 The client API (`lib/appdata`)

```rust
let mut s = Settings::open(&mut host, OWN_WORD);   // one call: the whole document
let size = s.u32("font.size")?.unwrap_or(14);     // local, no call
s.set_u32("font.size", 16)?;                      // stages, locally
s.commit()?;                                      // publishes, atomically

let mut mine = Settings::open_published(&mut host);   // own published scope
mine.set("font.family", "berkeley")?; mine.commit()?;
let theirs = read_published(&mut host, "os.tairix.terminal")?;  // foreign snapshot

let mut v = Vault::open(&mut host)?;         // sealed scope; may fail
v.set("imap.password", secret)?;            // sealed before it returns

let blob = Blobs::open()?.open("index", BlobMode::ReadWrite)?;  // real fd
let tmp  = Temp::create()?;                                     // real fd
```

**A foreign read hands back a `Document`, not a handle.** An earlier sketch in
this plan wrote it as `Settings::open_public(id)`, which implied a handle over
another app's store — and a handle is a thing a later call site can commit
through. A foreign read is one read with nothing to hold open, so it answers the
format engine's own document value: read-only in the only sense that matters
(nothing can publish through it), and carrying the engine's typed accessors, so
the published scope needs no accessor of its own. `open_published` takes no
command word, because that scope has no bundle-shipped layer to resolve one for
(§3.4).

`open` costs one `ConfigRead`; every `get` after it is a lookup in the document
the client already holds and parsed, so an app's start-up cost does not grow
with the number of settings it consults. The client also owns the
bundle-defaults layer (§3.4), so a `get` that the store does not answer falls
back to the app's own shipped default without a call either.

No app ever spells a store path, and no app names its own bundle id. Both come
from the attested identity, so an app cannot reach outside its own scope by
construction rather than by check. The one argument `open` takes is the
program's own **command word**, which selects nothing but layer 1 — resolved
through the shared `lib/cmdres` order, the same one the shell launches through
and `man` reads a bundle's help from, so an app's shipped defaults and its help
can never come from different bundles.

**A `set` whose value the layers already answer with stages nothing.** That one
rule is what makes "the user's file holds only what the user actually changed"
structural rather than each app's discipline: a value already coming from the
policy or defaults layer is never copied up, and an app that saves a setting it
did not change does not rewrite the user's document at all. Its converse is
that *removing* an opinion is an explicit `unset`, and that the effective value
it uncovers is the **service's** answer to give — so a commit ends by re-reading
the store rather than letting the client guess what a lower layer says.

**Writes are staged locally and pushed at commit.** A `set` is memory; the
`ConfigSet` frames go over the wire only when `commit` is called, so a caller
that stages ten keys and never commits costs the service nothing and holds no
staging session open. That also shrinks the abandoned-session window §3.6
reclaims by age to the span of one save.

**`Vault` is the one handle that can fail to open, and the one with no commit.**
`Settings::open` degrades to the shipped defaults and records why, because an
application must still run; `Vault::open` returns the refusal, because "I could
not read your secrets" is not "you have none" and an application that concluded
the second would prompt the user to re-enter a password it already holds. And
there is nothing to commit: the service seals each `set`/`unset` before it
replies, so a `Vault` holds no unsaved work at all. A value the vault already
holds, and a removal of a key it does not, each cost no call.

A sealed write **ends by re-reading**, exactly as a commit does, and for the
same reason: once a write has landed the service is the only thing that knows
what the sealed document says, and applying the change to the client's own copy
instead would be a guess — one that is wrong the moment another instance of the
application has sealed something of its own. A write that lands and is then
followed by a failed re-read reports the re-read's error and keeps the handle's
previous view; retrying the write is harmless, because sealing a value the vault
already holds costs nothing.

### 3.10 What the first migration settled

Migrating a real consumer with a settings sheet behind it (`terminal`) decided
three things the API sketch had left open, and each is now the rule for every
migration after it.

**"Restore defaults" is an `unset`, not a write.** The sheet used to reset its
in-memory profile to the app's compiled defaults and save that, which froze
today's values into the user's document: a machine-wide policy, or a later
version's better default, would then never apply again. It cannot be fixed
inside the sheet either — "the defaults" means the layers *beneath* the user's
own document, and only the store knows what they say. So the sheet reports the
request, the program removes the user's opinions, re-reads the profile that then
applies, and hands it back for the sheet to show. The widget states the intent;
the store answers it.

**An app keeps its own closed registry over the store's open namespace.** The
store's keys are an open namespace the app owns, but an app reads a *fixed* set
of them, so the migrated profile keeps its enumerated key type and its typed
field bridges: a value the registry refuses leaves that one field at its
documented default and is *named* to the caller, which reports it. One broken
setting costs only itself, and a key the registry does not know is one the app
leaves alone rather than destroying on the next save.

**A bundle ships no defaults document unless its defaults are genuinely data.**
`terminal`'s defaults are its code's — `Profile::default` is the last-resort
answer when nothing else applies — so shipping a `DefaultSettings/settings.conf`
that restated them would be exactly the duplication §2.2 forbids, with the
divergence risk that goes with it. Layer 1 is therefore implemented and tested
but deliberately unused by this consumer: it exists for a bundle whose defaults
*are* data (an integrator's themed build, a keymap, a shipped catalogue), which
is what §16.5 declares the entry for. Stating that here is what stops a later
migration adding a defaults file "for symmetry".

### 3.11 What the second scope settled

**One pin, two scopes.** The `.owner` record attests who owns the *data*, and
both documents are the same app's, so there is one pin and not one per scope. A
squatting publisher is therefore refused before it can put anything in front of
readers of the real app's published document — the pin does double duty as an
integrity guarantee for the sharing channel, not only for the private one.

**A publish is per document, and the temporary name is derived from it.** The
sibling a publish writes before renaming is the live name plus `.new`, computed
rather than declared per scope: it is then always a sibling in the app's own
store directory, two scopes can never contend for it, and the rename can never
cross a volume. Two constants would have been two things to keep in step.

**A foreign read sees committed data only.** A published value is what every
*other* app sees, so it is the committed document; a publisher's staging is its
own business, and a reader must not act on a value that may never be published.
The publisher's own `ConfigRead { Public }` still shows what it staged, so a
"publish this" surface reads back what it just set.

**No value stands for "another app's store".** The foreign path is a free
function returning a document, not a type holding a directory: a value standing
for someone else's store is a value a later call site could publish through.
There is nothing to hold, so there is nothing to misuse.

**The scope is not yet consumed by a first-party app, and that is stated rather
than papered over.** AD6 lands a platform facility: `abi-v1` surface an
application — including one not in this tree — integrates through, exactly as
AD4 landed `confd` before AD5's first client existed. What holds it honest in
the meantime is that every property above is tested through the *real* codec, on
both sides (the daemon's dispatcher over an in-memory volume, and the client
over the shared fake), rather than through a mock. The first in-tree consumer is AD10's
`pinboard` document, which the wallpaper chooser reads today by opening the
session's file directly (§1.5) — the concrete instance of the defect this scope
closes. It is left to that stage rather than pulled in here because it also has
to replace `lib/wallpaper`'s own `key = value` engine (§1.3) and settle whether
the desktop session runs under an attested app identity at all; a migration with
those two questions open is not a smaller change than the mechanism.

### 3.12 What the sealed scope settled

**The wipe belongs to the format engine, not to its callers.** The sealed scope
is a `lib/appconf` document, so a line the engine discards — an overwritten
setting, a collapsed duplicate, an `unset` removal, every line of a document
going out of scope — may hold a secret. Wiping in `Line`'s `Drop` rather than at
each call site is what makes it hold for *every* discard path, including ones a
later change adds: a caller cannot forget. The cost is a memset of bytes that
were about to be freed anyway, and it removes the alternative — a `wipe()` method
every consumer must remember to call, which is the shape that eventually gets
missed. `Document` also implements no `Debug`, so it cannot reach a log by
construction.

**No protector abstraction, and the reason is the same one AD6 gave for
`ConfigScope`.** AD7 has exactly one protector, so a protector enum with one
variant and a keyslot with nothing to put in it would be interface built for a
stage that has not landed. The honest alternative is to say plainly what protects
the master secret today (§3.7) and to leave the record versioned so the stage
that brings a real second secret reshapes it in place.

**A refusal is never an absence, and this is where that rule earns its keep.**
The published scope deliberately answers an empty document for a target-side
defect (§3.6) — there, indistinguishability is the *point*, because a reader must
learn nothing but what an app published. The sealed scope is the caller's own
data, so the opposite holds: every defect is reported as itself, precisely
because the caller can act on it and because an application that reads a damaged
vault as empty will overwrite or re-prompt. Two scopes, opposite rules, one
reason: report what the caller can act on, and nothing else.

**The service is now the principal that holds every account's secrets, and that
is stated rather than glossed.** Compromising `confd` yields applications'
settings *and* their secrets. No arrangement in which one service can both seal
and open vaults avoids that, and the alternative — each app holding its own key —
is exactly the "every app invents its own storage" arrangement this plan deletes,
with no place to put the key. What the sealing does buy is stated in §3.7: the
key is per-application, so a defect that lets one app's request reach another's
store directory yields ciphertext, and a damaged record is refused rather than
believed. What `confd` still cannot reach is unchanged: no credential record, no
other user's files, no process.

---

## 4. Stages

Each stage ships code **plus** tests **plus** docs, leaves the whole-project
gate green (§7), and contains no stubs (§15.1).

| # | Stage | Content |
|---|---|---|
| **AD1** ✅ | Manifest publisher identity | `publisher_pubkey`/`publisher_cert` in `AppInfoHeader`; `PublisherBinding`, the two labelled preimages, and `PublisherId`; the fourth authenticity check and `PublisherId` derivation in `lib/appload`; `LoadedApp::publisher`; `APP_PUBLISHER_INVALID`; the pinned `SYSTEM_APP_PUBLISHER_SEED` and the composer's `PublisherSource`. Regenerate the C view (`cargo xtask c-header --write`). |
| **AD2** ✅ | Attested app identity | `validate_bundle_id` + `BundleId`; `AppIdentity` (whole-or-absent); `TaskCapabilities::with_app_identity`/`app_identity`; `VerifiedProgram` threading it beside the manifest request; `Origin::with_app`/`app` and its wire tail; `self_origin`/`call_peer_origin` expose it. Tests prove a caller cannot forge or inflate it, that an identity-less principal reads absent, and that a malformed or half-filled wire tail is refused. |
| **AD3** ✅ | `lib/appconf` | The format engine: parse, typed accessors, comment/unparsed-line-preserving rewrite, fixed fail-closed bounds, fuzz harness (§19.6) holding the parse/render fixed point and the one-key-per-write property. No I/O; entirely host-testable. |
| **AD4** ✅ | `confd` + private scope | The service bundle and its `confd` account/ceiling, `APPDATA_ENDPOINT` + `lib/abi/src/appdata_ipc.rs`, `CAP_APPDATA_ADMIN`, the gated `Settings/Apps` + `Library/Apps` roots with their `required_cap` inodes and search-only transit grants across all three home provisioners, the `.owner` pin, the whole-document merged read, per-process staging and atomic commit, and the §3.5 name-gate fix the gate turned out to need. |
| **AD5** ✅ | `lib/appdata` + first migration | The client: one call to open, local reads, staged writes published as one commit, the **bundle-defaults fallback layer** (§3.4), the degrade path with a typed reason, and the shared fake service every consumer's own tests drive it over. `terminal` migrated: its hand-rolled `key value` parser, renderer, document bound, error types, and `~/Settings/Terminal/terminal.conf` path **deleted**, its registry re-keyed on dotted keys, and its *Restore defaults* turned from "write today's values" into an `unset` the store answers (§3.10). |
| **AD6** ✅ | Public scope | `public.conf`; `ConfigScope { Private, Public }` on every own-store request and the reshaped `appdata-v1` header that carries it; `PublicRead` as a **separate opcode with no scope field**, so another app's private document is not a representable request; per-scope staging, per-scope commit and per-scope pending-edit bound; the one scope→file-name mapping and the derived `.new` sibling; the published scope's deliberate absence of any layer beneath it; the foreign read's audited empty answer for a target-side defect and its typed refusal for the caller's own; `Settings::open_published` and `read_published` in the client, and the fake service extended to both scopes and to foreign apps. |
| **AD7** ✅ | Secrets | `secret.vault` and the per-account `.vault-master` record; the AEAD hierarchy (`derive_key` under one labelled context, ChaCha20-Poly1305 per document, a fresh nonce per seal) and the injected `Entropy` seam; `VaultRead`/`VaultSet`/`VaultUnset` as opcodes with **no scope field and no foreign counterpart**, and writes applied before the reply rather than staged; a damaged record, a failed authentication, and absent key material as three distinct audited refusals that are never an empty vault; a malformed master record refused and **never replaced**; the wipe moved into `lib/appconf`'s own `Line::drop`; `Vault` in the client, the fake service extended to the scope, and a fuzz harness over both records. **No protector abstraction** — §3.7 records why. |
| **AD8** | Blobs | `BlobOpen`/`BlobList`/`BlobDelete`, `fd_grant` handoff, quotas, `file_map` random access. |
| **AD9** | Temp | `TempCreate`, session-end and boot reaping, quota accounting. |
| **AD10** | Remaining migrations | Migrate `fstree`, `lib/wallpaper`/session pinboard, `applib` program library; delete every hand-rolled path. The `docs/src/` page and every §5 amendment but the manifest-field record land with the stage that needs them, and AD4 landed most of them; AD10 lands only what its own migrations drive. |

AD1 and AD2 are the foundation — nothing else can be built first, and they are
split because AD1 is tooling-and-format while AD2 is kernel-and-ABI, with
different review surfaces.

---

## 5. Charter amendments this plan drives

Landed as the stages that need them land, never pre-written (§3, §15.2). A
✅ names the stage that landed it.

- **§16.3** ✅ (AD4) — name `Settings/Apps/<bundle-id>/` and
  `Library/Apps/<bundle-id>/` as the app-data roots, keyed on the signed bundle
  id, reachable only through the app-data service, and say why the per-app
  directory cannot itself be the gate.
- **§16.3** ✅ (AD6) — record the two configuration scopes: a private document
  no other principal may read, and a published one any app may read through a
  request shape that cannot name the private scope, with the reason the second
  exists (§1.5).
- **§16.3** ✅ (AD7) — record the sealed scope: the app's secrets, encrypted at
  rest under a key derived per (account, app), reached by operations that carry
  no scope field and have no foreign counterpart, and with no layer beneath it.
- **§16.5** ✅ (AD4) — three changes to the bundle contract:
  `DefaultSettings/` becomes a read-only **fallback layer** rather than a
  first-launch copy (its *implementation* is AD5's); the "apps may write
  `Settings/<Name>/`" sentence becomes "apps never write their store directly"
  (§3.4); and the `AppInfo` declaration list records the two publisher fields.
  AD1 shipped those fields and this list is where §9 defers the per-field record
  to, so the charter was inaccurate until it landed.
- **§5.3** ✅ (AD4) — a capability gate guards an inode's *name* as well as its
  content (§3.5).
- **§5.2** ✅ (AD4) — add `CAP_APPDATA_ADMIN` to the capability list.
- **§3** ✅ (AD5) — the repository map gains `lib/appdata/`, the app-data
  client.
- **§15.18** ✅ (AD3, AD6) — a jump-sheet row: *"App settings, secrets, blobs
  and temporary files: the per-app store keyed on bundle id, the publisher pin,
  the `key = value` format engine, the published scope one app reads another's
  values through, the sealed scope, descriptor-backed blobs" →
  `plans/APPDATA.md`*.
- **`README.md`** ✅ (AD4, AD6, AD7) — the security/attack-vector matrix gains the
  app-from-app isolation row and the name-gate row (§13), (AD6) the row for
  cross-app sharing confined to the published scope: an app reaching another
  app's *private* settings through the sharing channel, or using it to probe
  which applications an account has run, and (AD7) the row for the sealed store:
  an app reading another app's saved passwords or tokens, and a damaged or forged
  vault being read as "no secrets saved".

---

## 6. Cross-references

`plans/APPS.md` (bundle layout, `DefaultSettings/`), `plans/CAPABILITY_USE.md`
(manifest ∩ user-grant ceilings), `plans/USERS.md` (the `confd` service
identity), `plans/NEW-SERVICEMANAGER.md` (enrolment, restart policy, readiness),
`plans/TPM.md` (a later key protector for §3.7), `plans/SPAWN.md` (the attested
record AD2 extends), `docs/src/security/sandbox.md` (why a sandbox child has no
store), `docs/src/filesystem/drives.md` (path/alias grammar).
