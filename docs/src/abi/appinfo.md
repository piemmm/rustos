# Application bundles (`AppInfo`, `abi-v1`)

`lib/abi/src/appinfo.rs` (`rustos_abi::appinfo`) is the frozen `abi-v1`
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
BCP-47 locale plus the mandatory `default/` (en-US) canonical source. It is
the single source the `man` command, each command's short `-h`/`-?` help,
and any graphical help viewer read from; there is no separate long-form
documentation entry.

## `AppInfo` manifest

`AppInfoHeader` is the fixed-size (`WIRE_LEN` = 340), signed prefix of the
manifest. It is `#[repr(C)]`, allocation-free, little-endian, with
`to_le_bytes`/`from_bytes` and a fail-closed decoder. It carries:

- `magic` (`"RAI1"`), `abi_version`, `flags`.
- The bundle identity: inline `id` / `name` / `version` (length byte plus a
  fixed buffer, validated as non-empty UTF-8 on decode).
- `capability_count` and `mime_count` describing the body.
- `syscall_table_hash` — the syscall interface the bundle was linked against
  (§9 / §19.2).
- `content_hash` — the digest binding the signature to the bundle's contents
  (§16.5).
- `signer_pubkey` and `signature` (Ed25519). `signed_range()` is the byte
  range the signature covers (everything except `signature`).

The variable body that follows the header is the requested capability-id
list (`capability_count` little-endian `u16`s, decoded by the shared
`decode_capability_ids`) immediately followed by the MIME-type table
(`mime_count` fixed-stride entries, read by `mime_type_at`). `body_len`
gives the exact body size for a given count pair.

## Dynamic-loader policy

`resolve_library(reference, bundle_libraries_dir)` is the §16.4 policy: a
shared-library reference resolves only against the requesting bundle's own
`Libraries/` directory or `SYSTEM_LIBRARIES_DIR` (`/System/Libraries`). A
reference with a `..` component, an empty reference, or one that points
anywhere else is refused (`LibraryError`). The bundle directory is tried
first, so a bundle's private copy shadows the system one.

The user-space service that applies all of this to a real bundle is
[`appmgr`](../userland/appmgr.md).
