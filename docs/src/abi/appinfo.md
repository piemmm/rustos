# Application bundles (`AppInfo`, `abi-v1`)

`lib/abi/src/appinfo.rs` (`tairix_abi::appinfo`) is the frozen `abi-v1`
surface for installed application bundles (`AGENTS.md` §16.4, §16.5). It
defines three things and carries the same freeze discipline as the syscall
table (§9): existing fields, offsets, and names never change; new behaviour
ships in `abi-v2`.

## Bundle layout

An installed application is a `/Apps/<Name>.app/` directory whose top-level
entries are drawn from a **closed** set, modelled by `BundleEntry`:

| Entry             | Kind      | Required |
|-------------------|-----------|----------|
| `AppInfo`         | file      | yes      |
| `Run`             | file      | yes      |
| `Code`            | directory | no       |
| `Libraries`       | directory | no       |
| `Resources`       | directory | no       |
| `DefaultSettings` | directory | no       |
| `Help`            | directory | no       |

`validate_bundle_layout(present)` accepts a bundle only if every top-level
name is one of those, no name repeats, and both `AppInfo` and `Run` are
present. Any other entry is a packaging defect (`BundleLayoutError`).

`Help/` is the bundle's internationalised help tree (`plans/APPS.md`): one
structured-Markdown document per command/topic, under one directory per
BCP-47 locale with the mandatory canonical `en-US/` source. It is
the single source the `man` command, each command's short `-h`/`-?` help,
and any graphical help viewer read from; there is no separate long-form
documentation entry.

## `AppInfo` manifest

`AppInfoHeader` is the fixed-size (`WIRE_LEN` = 664), signed prefix of the
manifest. It is `#[repr(C)]`, allocation-free, little-endian, with
`to_le_bytes`/`from_bytes` and a fail-closed decoder. Its declaration order
**is** its wire order, so the in-memory image and the wire image are the same
bytes and the generated C mirror (`include/tairix/tairix_appinfo.h`) is an
honest view of the format rather than a second, differently-ordered spelling
of it; `appinfo_header_repr_c_layout_is_the_wire_layout` pins that field by
field. It carries:

- `magic` (`"RAI1"`), `abi_version`, `flags`.
- The bundle identity: inline `id` / `name` / `version` (length byte plus a
  fixed buffer, validated as non-empty UTF-8 on decode). `id` is additionally
  held to `validate_bundle_id`: dot-separated segments of ASCII lowercase
  letters, digits, `-` and `_`, each non-empty. The grammar is narrow because
  the identifier names a directory in every user's per-app store
  (`plans/APPDATA.md`), so nothing that could be a path traversal (`.`, `..`,
  `/`), a hidden entry, a case-folding collision, or a control character can
  be spelled at all — an app cannot reach outside its own scope by
  construction rather than by a check a caller might forget. `BundleId` is the
  validated inline form the kernel attests on an `Origin`.
- `capability_count` and `mime_count` describing the body.
- `syscall_table_hash` — the syscall interface the bundle was linked against
  (§9 / §19.2).
- `content_hash` — the digest binding the signature to the bundle's contents
  (§16.5).
- `signer_pubkey` and `signature` (Ed25519). The signature covers the whole
  manifest except the `signature` field itself: the `signed_range()` header
  prefix concatenated with the capability/MIME body, so a tampered
  capability request breaks the signature rather than hiding behind a
  header-only signature.
- `publisher_pubkey` and `publisher_cert` — the developer identity, described
  below.

Decoding judges **no** authenticity: neither signature is verified and the
publisher binding is not classified, so a surface that only wants to draw a
bundle's name and icon need not hold a trust anchor to read its manifest.
Authenticity is the [load gate](../lib/appload.md)'s job.

## Publisher identity

The `signer_pubkey` is the key a *build* was signed with, and a release is
free to rotate it. That makes it the wrong thing to own per-app state by: a
re-signed update would orphan the user's settings, secrets, and blobs. So the
manifest also names a **publisher** — the developer's stable identity — and
proves the two belong together:

- `PublisherBinding::SelfPublished` — `publisher_pubkey == signer_pubkey` and
  `publisher_cert` is all zero. Nothing further is checked: the trust root
  that admitted the signer admitted the publisher, because they are one key.
- `PublisherBinding::Delegated` — `publisher_cert` must verify, against
  `publisher_pubkey`, as the publisher's Ed25519 signature over
  `publisher_cert_message()`.

Those two forms are exhaustive; `publisher_binding()` refuses every other
combination (a claim with no certificate to justify it, a certificate with
nothing to delegate). Both fields sit inside the region the manifest
signature covers, so neither can be swapped behind a valid signature.

`publisher_cert_message()` is the fixed-size, domain-labelled message a
delegation signs: the `PUBLISHER_CERT_CONTEXT` label, the bundle identifier at
full inline width with its length byte, and the signing key. Naming the
identifier as well as the key is what stops a certificate issued for one of a
publisher's bundles being lifted into another; the label stops a signature the
publisher key made for anything else being replayed as a delegation.

`PublisherId` is the opaque 32-byte identity per-app state is owned by:
`SHA-256(publisher_id_preimage())`, the `PUBLISHER_ID_CONTEXT` label followed
by the publisher key. It is an *identity*, deliberately not a key — one fixed
width whatever the publisher key's algorithm, and nothing a consumer could
mistake for something it may verify a signature with. `lib/abi` carries no
hash implementation, so it defines the preimage and the load gate completes
the derivation; `PublisherId::NONE` (all zero) is the sentinel for a principal
with no attested publisher. `plans/APPDATA.md` describes the per-app store
that pins it.

The variable body that follows the header is the requested capability-id
list (`capability_count` little-endian `u16`s, decoded by the shared
`decode_capability_ids`) immediately followed by the MIME-type table
(`mime_count` fixed-stride entries, read by `mime_type_at`). `body_len`
gives the exact body size for a given count pair.

## Bundle content digest

`digest_bundle_contents(files, update)` is the **one** definition of what
`content_hash` is computed over, shared by the build-time bundle composer
and every `BundleStore::content_hash` implementation so the two can never
drift. The digest covers every file in the bundle except `AppInfo` itself
(the manifest cannot cover its own bytes). The framing is injective — the
`BUNDLE_CONTENT_DIGEST_MAGIC` domain prefix, a little-endian `u32` file
count, then per file its length-framed path and length-framed bytes — and
deterministic: callers pass `BundleFileDigest` rows sorted by path in
strictly ascending byte order. A path that is empty, names `AppInfo`,
escapes the bundle (absolute, `.`/`..`/empty component), carries a NUL, or
arrives unsorted/duplicated fails closed. The caller supplies the hash
primitive (SHA-256 from `lib/crypto` in production) through the `update`
closure, so `lib/abi` carries no cryptographic dependency.

## Dynamic-loader policy

`resolve_library(reference, bundle_libraries_dir)` is the §16.4 policy: a
shared-library reference resolves only against the requesting bundle's own
`Libraries/` directory or `SYSTEM_LIBRARIES_DIR` (`/System/Libraries`). A
reference with a `..` component, an empty reference, or one that points
anywhere else is refused (`LibraryError`). The bundle directory is tried
first, so a bundle's private copy shadows the system one.

## The system command, application, and service stores

`SYSTEM_COMMAND_STORE` (`/System/Commands`), `SYSTEM_APPLICATION_STORE`
(`/System/Applications`), `SYSTEM_SERVICE_STORE` (`/System/Services`), and
`BUNDLE_SUFFIX` (`.app`) are the one definition of where the OS-provided
programs live and how a bundle directory is named (`AGENTS.md` §16.2,
`plans/APPS.md` §8). Which store a bundle lands in is decided solely by
its `AppInfo.toml` `kind` declaration (`ProgramKind::Command`,
`ProgramKind::Application`, or `ProgramKind::Service`) — there is no
separate list anywhere of which programs are which. A service is an app:
it ships as the same self-contained `<name>.app` bundle as a command app,
just in the service store. The kernel's embedded-program registry
registers every command app as a command-named store bundle
(`/System/Commands/<command>.app/Run`) and every service as
`/System/Services/<name>.app/Run` (the paths PID 1 `init`'s startup config
names), and the shell's command resolution builds the same spelling from
these constants — `/System/Commands` is an always-first, non-overridable
prefix, followed by `/System/Applications`, then the user's own
`Commands`/`Applications` stores, then the user's `PATH`, so `PATH` can
never shadow a system command.

The user-space service that applies all of this to a real bundle is
[`appmgr`](../userland/appmgr.md).
