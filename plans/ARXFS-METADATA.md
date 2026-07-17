# ARXFS-METADATA.md — ARXFS extensible file-metadata design brief

This file is an AI-facing design brief for generating the binding ARXFS
extended-metadata specification. It is **not** the final spec. Use it as source
material to produce a new section of `docs/src/filesystem/arxfs-spec.md`
(`§21 Extended file metadata`), the `lib/abi` additions, the VFS/driver trait
extension, the cross-filesystem preservation rules, the preset registry, the
required tests, and the exact `AGENTS.md` / `PLAN.md` amendments. It is binding
under `AGENTS.md`: every rule here is subordinate to the charter; where they
disagree the charter wins (stop and ask, charter §15.7).

The goal has two parts:

1. Give every ARXFS file a general-purpose **extended-attribute** facility — an
   extensible, namespaced `key → value` store per inode — because ARXFS today
   has no generic key=value metadata mechanism (only ext4's driver reads xattr
   blocks, and only to recover POSIX ACLs).
2. Use that facility to **preserve foreign-filesystem metadata** that would
   otherwise be lost when a file is copied onto ARXFS — the Acorn/RISC OS
   (ADFS) filetype (e.g. `&FFF` = Text), and the equivalent Amiga, Atari, and
   classic-Mac metadata — and to round-trip it back out when copying to a
   filesystem that understands it. This is interoperability with foreign
   systems' data (charter §2.13), not TAIRiX self-compatibility.

---

## 1. Why this exists

When a file is copied — by `cp`, by `mv`, or by a desktop drag-and-drop — only
its bytes, POSIX mode, owner, and timestamps survive today. Filesystems such as
Acorn ADFS, AmigaDOS, Atari GEMDOS/TOS, and classic HFS carry *additional*
per-file metadata that has no POSIX equivalent: ADFS load/exec addresses and a
12-bit filetype, Amiga protection bits and a file comment, Mac type/creator
four-character codes and a resource fork, and so on. Copying such a file onto a
filesystem with no place to keep that metadata silently destroys it. When
TAIRiX later gains ADFS/AmigaDOS/etc. read-write support, a copy *off* ARXFS
back onto the native filesystem must be able to restore the original metadata.

ARXFS therefore needs a lossless, general place to keep arbitrary foreign and
TAIRiX-native per-file metadata, plus a normalised vocabulary so the *same*
logical attribute (e.g. "this is a text file") is stored under the *same* key no
matter which tool wrote it.

## 2. Scope

In scope: the on-disk extended-attribute model, the namespaced key grammar, the
ABI/VFS/driver surface to get/set/list/remove attributes, value size and count
limits, capability and security rules, the cross-filesystem preservation
contract (copy, move, archive, snapshot send/receive), the **preset registry**
of well-known foreign-metadata keys (Acorn/ADFS, Amiga, Atari, Mac, and TAIRiX
native), and tests.

Out of scope (named so they are not assumed): implementing the ADFS/Amiga/Atari
filesystem drivers themselves (separate plans); resource-fork *content* storage
policy beyond "it is an extended attribute / named stream"; and any desktop UI
for editing metadata (the WM/file-browser consume the ABI, they do not define
it).

## 3. Non-negotiable invariants

- Extended attributes are **first-class inode metadata**, held to the same COW,
  integrity, redundancy, encryption, and authentication rules as all other
  ARXFS metadata (arxfs-spec §4, §5, §7, §8): self-identifying,
  checksummed/authenticated, two physical copies, encrypted, no plaintext.
- Attributes are **namespaced** and the namespace decides the **capability**
  required to read or write them (§5). There is no "open by default" attribute
  surface (charter §2.7, §5.4).
- Every operation is **capability-checked before state**, validates **every**
  input, and **fails closed** (charter §5.4); decisions on security-relevant
  namespaces are logged with a stable event ID (charter §19.4).
- Attribute values are **opaque byte strings** to ARXFS; ARXFS never
  interprets a value's meaning. Interpretation (e.g. that `acorn.filetype`
  holds a 12-bit hex type) belongs to the preset registry and the converting
  tool, not the filesystem core.
- The facility is **bounded** but the per-inode capacity **scales** (charter
  §24): value size, key length, and per-inode attribute count have fixed
  *security* bounds (charter §24.4 — these are validation bounds on stored
  data, not growable capacities), while the on-disk storage grows by whole COW
  blocks as attributes are added.
- Preservation is **lossless or it fails closed**: an exact-preservation copy
  that cannot represent an attribute on the target reports the loss (a typed
  `MetadataNotRepresentable`), it never silently drops, truncates, or guesses
  (mirrors the Time64 truncation rule, charter §21).
- No code duplication: the namespaced-key parser, the preset registry, and the
  conversion helpers live **once** in a shared `lib/*` crate so ARXFS, the
  foreign-FS drivers, `cp`/`mv`, the desktop, and the archive tools all share
  one definition (charter §2.2).
- Production errors are `Result`-based, never panics (charter §2.9).

---

## 4. On-disk and in-memory model

### 4.1 Per-inode attribute set

Each ARXFS inode gains an optional reference to an **attribute set**: an
ordered collection of `(key, value)` pairs. Small sets are stored inline with
the inode (ARXFS already supports inline/packed small-file storage,
arxfs-spec §5); larger sets spill into dedicated COW metadata blocks named
from the inode, exactly as directory blocks and extents are (arxfs-spec §4,
§13). The attribute set is part of the inode's COW image, so setting an
attribute is one atomic transaction and a crash leaves prior-or-new (arxfs-spec
§14).

```text
AttrEntry:
    key       1..=255 bytes, namespaced UTF-8 (see §4.2 grammar)
    flags     bitset (e.g. SYSTEM, NO_BACKUP — usually clear)
    value     0..=VALUE_MAX bytes, opaque
```

Fixed v1 bounds (security/validation bounds, charter §24.4 — not tunable):

```text
KEY_MAX            255 bytes
VALUE_MAX          65536 bytes (64 KiB) for an inline/attribute value
ATTRS_PER_INODE    a fixed cap large enough for all presets plus headroom
TOTAL_ATTR_BYTES   a fixed per-inode cap on summed key+value bytes
```

A value larger than `VALUE_MAX` is **not** an extended attribute; it is a
**named stream** (§4.4) — this is how a Mac resource fork or other large
fork-style data is stored without bloating the inline attribute set.

### 4.2 Key grammar (the namespace)

A key is `namespace "." rest`, byte-for-byte case-sensitive (matching ARXFS
directory-name comparison, arxfs-spec §13), e.g. `acorn.filetype`,
`amiga.protection`, `mac.type`, `user.comment`, `tairix.origin`. Reserved
namespaces and their meaning/capability:

```text
namespace   meaning                                    access capability
---------   ----------------------------------------   --------------------------
user        free-form user metadata                    none beyond file r/w perms
acorn       Acorn / RISC OS (ADFS) preset metadata     none beyond file r/w perms
amiga       AmigaDOS preset metadata                   none beyond file r/w perms
atari       Atari GEMDOS/TOS preset metadata           none beyond file r/w perms
mac         classic Mac OS / HFS preset metadata       none beyond file r/w perms
tairix      TAIRiX-native extended metadata            none beyond file r/w perms
system      security-sensitive (ACL-adjacent) metadata a dedicated capability (§5)
trusted     metadata only privileged services may set  a dedicated capability (§5)
```

The foreign-FS namespaces (`acorn`, `amiga`, `atari`, `mac`) and `user`/`tairix`
are ordinary file metadata: reading/writing them needs only the file's own
read/write permission (the per-inode owner/mode/ACL model, charter §5.3). The
`system` and `trusted` namespaces guard a real security boundary and require a
capability introduced **with** its enforcement point (charter §5.2). An unknown
namespace is rejected at set time (fail closed) — the namespace set is closed
and curated, evolved in place (charter §2.13), not open-ended.

### 4.3 Read/write/list/remove semantics

- `get(node, key) -> Option<value>` — capability-checked by namespace.
- `set(node, key, value)` — validates namespace, key bytes, value size, and the
  per-inode caps; rejects the whole call on any failure; one COW transaction.
- `list(node) -> [key]` — returns only keys whose namespace the caller may read.
- `remove(node, key)` — one COW transaction.
- Atomicity: a `set`/`remove` either fully commits or does not; never partial.
- Reading is filtered by capability: a caller without the `system`-namespace
  capability never even learns a `system.*` key exists (the listing omits it).

### 4.4 Named streams (forks)

A fork-style payload too large for an attribute value (classic-Mac resource
fork, an icon, a thumbnail) is stored as a **named stream**: a secondary data
stream attached to the inode, stored exactly like file data (COW extents,
checksummed, compressed, encrypted, dedupable, sparse-capable — arxfs-spec §6,
§7, §8, §9, §10, §19). The primary (unnamed) stream is the file's normal
contents; named streams are addressed by a `tairix`/`mac`-namespaced key (e.g.
`mac.resourcefork`). This keeps large forks out of the inline attribute set
while reusing the entire data pipeline (charter §2.2 — no second data path).

---

## 5. Capabilities and security

- Reading/writing `user`, `acorn`, `amiga`, `atari`, `mac`, `tairix` attributes
  requires only the file's existing read/write permission — no new capability
  (charter §5.2: do not add a capability where the per-inode model suffices).
- `system.*` (security-sensitive, ACL-adjacent) and `trusted.*` (privileged
  services only) each require a capability, justified against the §5.2
  minimalism test and introduced with its enforcement point. Reuse an existing
  capability (e.g. an audit/security capability) if it already expresses the
  authority; only add a new `CAP_FS_XATTR_*` if none does.
- Capability checked **before** any attribute state is read or mutated, with the
  kernel-attested caller identity (charter §5.4).
- Every input validated: namespace membership, key bytes (no `/`, no NUL,
  length ≤ `KEY_MAX`, valid UTF-8 per the grammar), value size ≤ `VALUE_MAX`,
  per-inode caps. Fail closed on any violation.
- Attribute keys and values live in the encrypted-metadata domain like filenames
  (arxfs-spec §7); no plaintext attribute leaks on a raw-device read.
- Untrusted source: attributes copied **in** from a foreign filesystem or an
  archive are untrusted input — the importing path validates them against the
  grammar and caps and runs in the normal parser-sandbox discipline where a
  decoder is involved (charter §19.5). A malformed foreign attribute is dropped
  with a typed, logged error, never trusted verbatim.

---

## 6. Cross-filesystem preservation contract

This is the heart of the brief: how metadata survives a copy. The contract is
implemented once in a shared crate (§8) and consumed by `cp`, `mv`, the desktop
file manager, the archive tools, and the snapshot send/receive stream
(plans/ARXFS-SNAPSHOT.md §6.3).

### 6.1 The model: foreign metadata ↔ normalised preset keys

Every foreign filesystem driver exposes its per-file native metadata as a set of
**normalised preset attributes** (the registry, §7) through the filesystem
capability API. A copy is therefore three steps, none of which the copy tool
hard-codes per filesystem:

```text
source driver  -> normalised preset attributes (e.g. acorn.filetype = "fff")
copy engine    -> set those attributes on the destination inode
dest driver    -> store natively if it understands them, else keep as ARXFS
                  extended attributes verbatim (lossless round-trip)
```

- **Foreign → ARXFS**: the source driver yields preset attributes; ARXFS
  stores them verbatim in the matching namespace. Nothing is lost.
- **ARXFS → foreign (same family)**: ARXFS yields the stored preset
  attributes; the destination driver maps them back to its native fields
  (charter §21 checked conversion — exact or `MetadataNotRepresentable`).
- **ARXFS → foreign (different/foreign-incapable, e.g. FAT32)**: attributes
  with no native home are reported as not representable; an exact-preservation
  copy fails closed, a best-effort copy drops them only when the caller
  explicitly requested a documented lossy policy (mirrors charter §21).

### 6.2 Copy semantics (cp / mv / desktop)

- `cp`/`mv` and the desktop copy default to **preserve all representable
  metadata** (the foreign-FS namespaces, `user`, `tairix`), like `cp
  --preserve` does for mode/timestamps. Document the flag surface in
  `userland/shell/` and the desktop.
- `mv` within one filesystem preserves attributes trivially (same inode or a
  metadata-only move); `mv`/`cp` across filesystems goes through §6.1.
- A copy never silently loses metadata: when an attribute cannot be carried, the
  tool reports it (and, where applicable, emits a `stdinfo` `omission` record,
  charter §20.1) rather than dropping it quietly.
- Snapshot send/receive (plans/ARXFS-SNAPSHOT.md §6.3) carries the full
  attribute set so a backup round-trip is lossless.

### 6.3 Archive interop

Archive create/extract (`/System/Libraries/` archive class, charter §16.4) must
carry the normalised preset attributes too, so a file archived on ARXFS and
extracted onto a native ADFS/Amiga volume (or vice versa) keeps its metadata.
The archive decoder is untrusted-input-sandboxed (charter §19.5).

---

## 7. Preset registry (well-known foreign metadata)

A first-party, curated registry maps each foreign filesystem's native per-file
metadata to normalised `namespace.key` attributes with a fixed value encoding.
It lives once in the shared crate (§8) and is the single source of truth for
both the foreign-FS drivers and the conversion tools (charter §2.2). The
registry is closed and curated (evolved in place, charter §2.13); adding an
entry is data, not new code paths.

Value encodings are exact and documented (no guessing, charter §21). Indicative
v1 entries (the spec author finalises exact encodings against each format's
authoritative reference):

### 7.1 Acorn / RISC OS (ADFS, FileCore)

```text
acorn.filetype     12-bit filetype as 3 lowercase hex digits, e.g. "fff" (Text),
                   "ffb" (BASIC), "faf" (HTML); absent if the object is typed by
                   load/exec instead.
acorn.loadaddr     32-bit load address, 8 hex digits (when not a typed file).
acorn.execaddr     32-bit exec address, 8 hex digits.
acorn.attr         FileCore access bits (R/W/L and owner/public), canonical form.
acorn.datestamp    RISC OS 5-byte centisecond timestamp, normalised to Time64
                   on display but stored exactly so it round-trips.
```

Note the RISC OS convention the registry must encode faithfully: a filetyped
object encodes its type and a timestamp *inside* the load/exec words; the
registry stores the decoded `acorn.filetype` **and** preserves the raw
load/exec so a copy back to ADFS is exact.

### 7.2 Amiga (AmigaDOS / FFS)

```text
amiga.protection   HSPARWED protection bits, canonical string/bitset.
amiga.comment      file comment (up to the AmigaDOS limit), opaque bytes.
amiga.filenote     alias accepted on read; canonicalised to amiga.comment.
```

### 7.3 Atari (GEMDOS / TOS)

```text
atari.attributes   GEMDOS attribute bits (read-only, hidden, system, archive).
atari.gemdos_date  GEMDOS/FAT-style date-time, stored exactly, shown as Time64.
```

(Atari TOS uses a FAT-derived on-disk format; the registry keeps Atari-specific
attributes distinct from a generic FAT mapping so intent is not lost.)

### 7.4 Classic Mac OS (HFS/HFS+)

```text
mac.type           4-character type code (OSType), e.g. "TEXT".
mac.creator        4-character creator code, e.g. "ttxt".
mac.finderflags    Finder flags bitset.
mac.resourcefork   the resource fork, stored as a named stream (§4.4).
```

### 7.5 TAIRiX native

```text
tairix.origin      provenance: source filesystem family + volume id, set on
                   import so a later export knows where the metadata came from.
tairix.mime        optional MIME type hint (advisory; never a security decision).
```

The registry also defines, per foreign family, **which native field a TAIRiX
file's attribute maps to on export**, so the round-trip in §6.1 is symmetric and
checked.

---

## 8. ABI, shared crate, tooling, and docs

- Shared crate: add `lib/fsmeta` (name TBD) holding the namespaced-key grammar +
  parser, the `AttrEntry`/`AttrSet` types, the preset registry, and the
  foreign↔normalised conversion helpers — `no_std`, unit-tested, rustdoc on
  every public item, stability tier in its `README.md` (charter §6). One
  definition shared by ARXFS, the foreign-FS drivers, the copy/move tools, the
  desktop, and the archive tools (charter §2.2). Adding the crate updates
  `AGENTS.md` §3 and `PLAN.md` (charter §6).
- ABI/VFS: extend the filesystem driver traits (`lib/abi/src/driver/filesystem.rs`)
  with `get_attr`/`set_attr`/`list_attr`/`remove_attr` and named-stream access,
  under the syscall-table ABI discipline (charter §9): versioned, hashed,
  `#[repr(C)]` where C-visible, frozen on first release. Because `abi-v1` is
  unshipped, extend in place — no `v2` (charter §2.13). The foreign-FS metadata
  query/representability is exposed through the filesystem capability API
  (charter §21 — drivers declare what they can represent).
- Tooling: `cp`/`mv`/desktop preserve-metadata behaviour (§6.2); an attribute
  CLI (e.g. `getattr`/`setattr` in `userland/shell/`, or extend an existing
  tool) backed by the capability-checked ABI — never a privileged bypass.
- Docs: new `§21 Extended file metadata` in `docs/src/filesystem/arxfs-spec.md`,
  a row in the §2 mandatory feature table, a stage in §18, plus a registry
  reference page under `docs/src/filesystem/`. Update
  `docs/src/filesystem/arxfs.md` and the ext4/fat32 driver docs to state their
  representability limits.

---

## 9. Required tests

Incomplete unless these pass (charter §7, §16, §23.4):

1. set/get/list/remove round-trips for every namespace; case-sensitive keys are
   distinct; unknown namespace rejected; oversize key/value/count rejected;
2. attribute changes are one COW transaction — crash replay leaves prior-or-new;
3. attributes are encrypted at rest (no plaintext key/value on a raw-device
   read) and authenticated (a flipped attribute block is detected and repaired
   from the duplicate copy, like other metadata, arxfs-spec §16);
4. capability gate: `system`/`trusted` namespaces refused without the
   capability, allowed with it, listing omits unreadable namespaces, decisions
   logged with a stable event ID;
5. preset round-trip per family: a synthetic ADFS file with `&FFF` filetype
   imported to ARXFS stores `acorn.filetype = "fff"` plus exact load/exec, and
   exporting back to a mock ADFS driver reproduces the original native fields
   byte-for-byte; same for Amiga protection/comment, Atari attributes/date, and
   Mac type/creator/finderflags/resource-fork;
6. lossless or fail-closed: copying a ARXFS file carrying `mac.type` to a mock
   FAT32 target reports `MetadataNotRepresentable` under exact-preservation and
   drops only under an explicit documented lossy policy (charter §21);
7. `cp`/`mv`/desktop preserve representable metadata by default and emit a
   `stdinfo` omission record when dropping (charter §20.1);
8. named stream (resource fork) stores/reads through the full data pipeline
   (checksum, compression, encryption, sparse) and is preserved across copy and
   snapshot send/receive;
9. archive create/extract round-trips preset attributes onto a foreign target;
10. scalability/bounds: per-inode caps enforced (fail closed at the limit, not a
    panic), attribute storage grows by COW blocks, large attribute sets on a
    small-RAM machine stay bounded (charter §24, §26);
11. fuzz targets for the key-grammar parser, the attribute-block decoder, and
    each foreign-metadata import decoder (untrusted input, sandboxed — charter
    §19.5, §19.6);
12. Time64-bearing preset values (acorn datestamp, atari/gemdos date) round-trip
    pre-1970 / post-2038 / far-future and fail closed when the foreign format
    cannot represent the instant (charter §21).

---

## 10. AGENTS.md and PLAN.md amendments to call out

The generated spec must explicitly identify these charter touch-points:

- **`AGENTS.md` §3** — add the shared `lib/fsmeta` crate and note the ARXFS
  `attr`/metadata module if the charter enumerates ARXFS internals.
- **`AGENTS.md` §5.2 / §5.3** — `user`/foreign/`tairix` attributes use the
  existing per-inode permission model (no new capability); `system`/`trusted`
  namespaces add a justified capability with its enforcement point. Record the
  decision.
- **`AGENTS.md` §16.4** — extended metadata and named streams interact with the
  curated shared-library set (archive, image-decoding for thumbnails); confirm
  no new library *class* is needed, or add one with a §16.4 + `PLAN.md` update.
- **`AGENTS.md` §21** — preset timestamp values (acorn/atari) are converted to
  and from foreign formats with checked Time64 conversions; no silent
  truncation.
- **`AGENTS.md` §2.13** — foreign-metadata preservation is *interoperability
  with foreign systems' data*, explicitly allowed and distinct from TAIRiX
  self-compatibility; state this so it is not mistaken for a compat shim.
- **`PLAN.md`** — add a ARXFS extended-metadata stage (alongside/after the
  snapshot stage) and a one-line "Charter Amendments" rationale for any new
  crate or capability (charter §13).
- **`docs/src/filesystem/arxfs-spec.md`** — new §21, §2 feature-table row, §18
  stage, and the preset-registry reference page.

This brief, like the rest of `plans/`, states the plan and the design, not a
build log (charter §13): when the work lands, replace the planned/in-progress
prose with a done-state summary rather than appending a changelog.
