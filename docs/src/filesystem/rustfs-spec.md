# RustFS specification

Status: implementation spec  
Target: RustOS  
Driver path: `drivers/filesystem/rustfs/`  
Last updated: 2026-06-02

RustFS is the native RustOS filesystem: copy-on-write, encrypted, checksummed,
compressed, deduplicating, SSD-aware, and recoverable. It is optimised for high
I/O throughput, low CPU use, data integrity, and clean fsck/recovery.

This spec is subordinate to `AGENTS.md`; conflicts are resolved in favour of
`AGENTS.md`.

This is the authoritative RustFS implementation specification, delivered
**in stages** (see §18 — Staged delivery & status). It lives in the book so it
ships with the rest of the documentation; the companion user-facing page is
`docs/src/filesystem/rustfs.md` and the live prompt for the next staged
session is `.junie/next-rustfs-prompt.md`. All three are kept in step in the
same change. There is exactly one `rustfs` driver and one on-disk version.

---

## 1. One mandatory profile

RustFS has **one production profile**. All features below are **enabled by
default and not tunable / not tuneable** by mkfs option, mount option, ioctl,
per-volume policy, per-file policy, build feature, environment variable, or
userland configuration.

Forbidden knobs include, but are not limited to:

```text
compression=off
dedupe=off
checksums=off
encryption=off
trim=off
health=off
scrub=off
metadata_copies=1
plaintext
performance_mode_that_weakens_integrity
```

Allowed implementation flexibility:

- test-only fault injection behind test builds;
- internal adaptive choices that preserve semantics, such as storing an
  incompressible record raw;
- future on-disk format versions that change constants globally.

Normal RustOS mount security flags such as `ro`, `noexec`, `nosuid`, and
`nodev` remain valid because they are permission policy, not RustFS feature
configuration.

---

## 2. Mandatory feature table

| Area | Mandatory RustFS v1 behaviour |
|---|---|
| COW | No committed metadata block is overwritten in place. |
| Transactions | Mount highest valid committed root after ordinary power loss. |
| Metadata integrity | Every metadata block is self-identifying and authenticated/checksummed. |
| Data integrity | Every physical record is checksummed; every logical record has a strong hash. |
| Redundancy | Critical metadata has at least two physical copies. |
| Encryption | Every RustFS volume is encrypted. Plaintext RustFS is forbidden. |
| Compression | First-party RustOS zstd-fast-style compression is always active. |
| Deduplication | Exact verified dedupe is always active; it may miss duplicates but may never merge unequal bytes. |
| Shared extents | Reflink/shared immutable chunks are core storage. |
| TRIM | mkfs discards the target range when supported; mounted RustFS trims safely in batches. |
| SMART/NVMe health | Health snapshots are stored and used when exposed by the storage stack. |
| Scrub | Online verification and repair from redundant copies. |
| Check/fsck | Offline structural validation, repair, and index rebuild. |
| Rescue | Damaged-volume root discovery and file extraction. |
| Time | All persistent timestamps use RustOS `Time64`. |
| Security | POSIX bits + ACLs + capability gates on every inode. |

---

## 3. Repository and dependency rules

Primary driver:

```text
drivers/filesystem/rustfs/
```

Expected internal modules:

```text
format, mount, transaction, superblock, trees, inode, extent, chunk,
integrity, crypto, compression, dedupe, trim, health, scrub, check,
rescue, error
```

Shared ABI types belong in `lib/abi`. Cryptographic primitives and key handling
must go through `lib/crypto`. RustFS must not link against kernel internals.

Compression dependency rule:

```text
No external zstd/compression dependency is allowed.
```

RustFS must not use `zstd`, `zstd-safe`, `zstd-sys`, `libzstd`, a vendored C
library, a registry compression crate, or code downloaded from another site.
The zstd-compatible codec subset used by RustFS must be written in the RustOS
workspace in Rust.

Placement:

- If only RustFS uses it: `drivers/filesystem/rustfs/src/compression/`.
- If another crate needs it: add a first-party `lib/compress` crate and update
  `AGENTS.md`, `PLAN.md`, docs, tests, and CI in the same change.

Crypto is the exception from “roll our own”: RustFS uses audited primitives via
`lib/crypto` and must not hand-roll encryption or authentication primitives.

---

## 4. On-disk model

RustFS stores immutable physical records referenced by logical extents.

```text
superblock ring
  -> recent transaction roots
      -> root tree
      -> inode tree
      -> extent tree
      -> chunk/refcount tree
      -> reverse-reference tree
      -> free-space tree
      -> device-health tree
      -> rebuildable secondary indexes
```

Authoritative metadata:

- superblock ring and root history;
- inode tree;
- extent tree;
- chunk table;
- refcount tree;
- reverse-reference tree.

Rebuildable metadata:

- free-space tree;
- dedupe index;
- directory acceleration indexes;
- health summaries;
- scrub progress;
- allocation heat maps.

A corrupt rebuildable tree must not make a valid volume unmountable.
`rustfs check` must rebuild it from authoritative metadata.

---

## 5. Fixed v1 constants

RustFS v1 constants are not user-tunable:

```text
metadata block target:       16 KiB
normal data record target:   128 KiB
large sequential target:     256 KiB
small-file storage:          inline or packed fragments
logical hash:                BLAKE3-256 through lib/crypto
metadata authenticator:      lib/crypto keyed hash/MAC
physical checksum:           fast checksum selected by RustOS storage ABI
critical metadata copies:    2 minimum
root history:                retained for rollback and safe discard
```

A future RustFS format may revise constants globally. A mounted v1 filesystem
must not expose runtime controls for them.

---

## 6. Write/read pipeline

Write path:

```text
plaintext logical record
  -> logical hash
  -> same-encryption-domain dedupe lookup
  -> byte-verify duplicate candidate, or continue as unique
  -> first-party zstd-fast compression attempt
  -> store raw if compression does not win
  -> encrypt stored representation
  -> checksum/authenticate metadata and physical record
  -> write new physical record
  -> commit new COW root
```

Read path:

```text
read physical record
  -> verify physical checksum
  -> decrypt
  -> decompress if compressed
  -> verify logical hash
  -> return plaintext
```

Compression being enabled does not require storing larger compressed output.
Deduplication being enabled does not require exhaustive foreground discovery.
It requires active bounded foreground discovery, background discovery, exact
verification before sharing, and rebuildable dedupe metadata.

---

## 7. Encryption

RustFS volumes are always encrypted. `mkfs.rustfs` must fail if no valid key
source is supplied by the installer, recovery flow, or storage policy service.

Rules:

- no plaintext RustFS format exists;
- data, filenames, directory entries, and sensitive metadata are encrypted;
- only the minimal unlock/discovery header may remain plaintext;
- primitives come from `lib/crypto` only;
- AES-256-XTS is preferred when hardware acceleration is available;
- Adiantum or another approved wide-block mode is selected automatically when
  AES is unsuitable;
- encryption mode selection is automatic and not user-tunable;
- dedupe is allowed only within the same encryption domain;
- keys are never stored unwrapped on disk.

Key hierarchy:

```text
volume wrapping key
  -> domain key
      -> content key
      -> filename key
      -> metadata authentication key
      -> dedupe-domain key
```

Secret-holding allocations inherit RustOS zero-on-free requirements.

---

## 8. Integrity and recovery primitives

Each metadata block stores:

```text
magic, block type, format version, filesystem UUID, owner object,
generation, logical address, physical address, payload length,
authenticator/checksum, payload
```

The authenticator/checksum covers identity, owner, generation, expected address,
and payload. It must detect stale writes, misdirected writes, wrong-type blocks,
torn metadata, and bit rot.

Each data record stores:

```text
chunk id, chunk generation, plaintext logical hash, physical checksum,
compression state, encryption domain, physical location
```

On verification failure, RustFS must try redundant copies, repair bad copies
from good copies, return an error if no valid copy exists, and record affected
inode/range details for health, scrub, and check.

---

## 9. Deduplication

Deduplication is mandatory and exact.

Rules:

- key is `BLAKE3-256(plaintext logical record) + length`;
- dedupe index is rebuildable and never authoritative;
- candidate matches are byte-verified before sharing;
- cross-domain dedupe is forbidden;
- shared chunks are immutable and refcounted;
- overwriting shared data creates a new physical record.

Missing a duplicate is acceptable. Merging unequal data is corruption.

---

## 10. Compression

Compression is mandatory and uses the first-party RustOS zstd codec.

Rules:

- dedupe before compression;
- compression before encryption;
- compress only unique records;
- store raw when compression does not reduce size enough;
- bound memory before allocation;
- malformed compressed data returns an error, never panic;
- decompression failure is data corruption;
- background recompression may exist but is not user-controllable.

The v1 target is a low-CPU zstd-fast-style profile, not maximum ratio.

The first-party codec must include corpus tests, known-answer tests, malformed
input tests, and fuzz targets.

---

## 11. TRIM/discard and drive health

mkfs flow:

```text
open target exclusively
read discard and health capabilities
record initial health snapshot when available
issue full-range discard when supported
create encrypted RustFS structures
flush
store health baseline
```

Mounted trim rules:

- freed ranges enter a pending-discard queue;
- discard is issued only after the range is unreachable from every retained
  root, snapshot, reflink, deduped extent, and recovery root;
- discard is batched, aligned to device granularity, and rate-limited;
- RustFS must not assume discarded blocks read back as zero;
- devices without discard support are recorded, not failed.

There is no `nodiscard` or `trim=off` mode.

Health fields, when exposed by storage drivers:

```text
model, serial, firmware, power-on hours, unsafe shutdowns, temperature,
wear/percentage used, available spare, media/data integrity errors,
error-log entries, ATA reallocated/pending/uncorrectable sectors,
interface CRC errors
```

Health behaviour:

- mkfs stores a baseline;
- mount compares current health with the last clean state;
- unsafe-shutdown deltas schedule metadata scrub;
- media-error deltas schedule deep scrub;
- critical multi-device health avoids new allocations to failing devices;
- critical single-device health raises warnings and may force read-only mount.

If health data is unavailable, store `HealthUnavailable`; the health subsystem
remains enabled.

---

## 12. Scrub, check, and rescue

```text
rustfs scrub   online verification and repair
rustfs check   offline structural validation, repair, and index rebuild
rustfs rescue  damaged-volume root discovery and file extraction
```

`rustfs scrub` verifies metadata, physical checksums, logical hashes, refcounts,
and shared chunks. It is resumable and safe to interrupt.

`rustfs check` validates and repairs superblocks, root history, tree structure,
inodes, extents, refcounts, reverse refs, directories, ACL/capability metadata,
free-space by rebuild, dedupe index by rebuild, and orphaned inodes.

`rustfs rescue` scans for self-identifying metadata blocks, lists valid roots,
maps physical LBAs to files when possible, and extracts readable files without
requiring a fully mountable filesystem.

---

## 13. Permissions and namespace

Each inode stores:

```text
owner uid, group gid, POSIX mode bits, ACL, optional capability requirement,
created Time64, modified Time64, accessed Time64, changed Time64
```

RustFS must enforce:

- no ambient root authority;
- capability checks before state access;
- no `/proc`, `/sys`, or legacy top-level RustOS layout assumptions;
- RustOS-forbidden top-level names rejected on the system volume;
- fail-closed behaviour.

---

## 14. Crash consistency

Commit order:

```text
allocate new records
write data records
write metadata leaves
write metadata internal blocks
write new root
write commit record
flush
publish superblock slot
flush
```

After power loss, mount selects the highest valid committed root. A partial
transaction is ignored. Full `rustfs check` is not required for ordinary crash
recovery.

Discard may never destroy data reachable from retained roots.

---

## 15. Implementation order

1. On-disk headers, superblock ring, transaction roots.
2. COW metadata trees, inode tree, extent tree, free-space rebuild.
3. Metadata authentication/checksums and duplicated critical metadata.
4. Encrypted volume creation, key hierarchy, filename/data encryption.
5. Data records with physical checksum and logical hash.
6. First-party RustOS zstd codec and RustFS compression integration.
7. Chunk table, refcounts, reverse refs, reflinks, dedupe index.
8. Online scrub.
9. Offline check and rescue.
10. TRIM/discard queues and mkfs-time discard.
11. Device health baselines and health-triggered scrub.
12. Fuzz, proptest, crash-replay, and corruption-injection suites.

Do not implement dedupe before COW, checksums, refcounts, and check/rebuild
logic are solid.

---

## 16. Required tests

Minimum acceptance tests:

- crash replay at every commit step;
- metadata bit flip detection and repair from duplicate copy;
- data bit flip detection;
- wrong key refuses mount;
- plaintext RustFS cannot be created;
- first-party zstd round-trip, corpus, malformed-input, and fuzz tests;
- dedupe never merges unequal data;
- dedupe index rebuild works;
- free-space rebuild matches authoritative extents;
- TRIM never touches retained-root/snapshot/reflink/dedupe-reachable ranges;
- mkfs issues discard on discard-capable mock devices;
- SMART/NVMe health deltas trigger required scrub/check actions;
- Time64 persists dates before 1970, after 2038, and far-future values;
- fuzz targets for mount, metadata decode, directory decode, compression decode,
  check, and rescue.

The implementing agent must run the full RustOS CI/test requirements from
`AGENTS.md` before reporting completion. 
---

## 17. Definition of done

RustFS is not complete until:

- every mandatory feature is present and non-tunable;
- RustFS has no external zstd/compression dependency;
- crypto use is through `lib/crypto`;
- production errors are `Result`-based, not panics;
- ABI changes are versioned and documented;
- docs under `docs/src/filesystem/` are updated;
- `cargo fmt --all`, `cargo xtask ci`, and required fuzz/proptest runs pass for
  the whole RustOS workspace;

---

## 18. Staged delivery & status

RustFS v1 is large and lands **one stage per session**, bottom-up, exactly
in the §15 order. Each session:

1. implements the next unfinished stage **completely** — code, rustdoc, the
   §16 tests for that stage, and the `docs/src/filesystem/rustfs.md`
   update — behind the existing frozen `Filesystem*` traits so the VFS is
   never broken (`AGENTS.md` §2.4 / §2.5);
2. runs the **whole-project** definition of done (`AGENTS.md` §7) and fixes
   every failure before reporting complete;
3. rewrites `.junie/next-rustfs-prompt.md` for the following stage and ticks
   the status legend below.

There is exactly one `rustfs` driver and one on-disk version: the
copy-on-write driver in `drivers/filesystem/rustfs/` fully replaced the
earlier journaled implementation (no `v1` folder, no parallel version).
Each later stage grows that single driver and must not regress it.

Status legend: `✓` done · `*` in progress · `!` blocked · (blank) not started.

| Stage | §15 step | Status |
|---|---|---|
| 1 | On-disk headers, superblock ring, transaction roots. | ✓ |
| 2 | COW metadata trees, inode tree, extent tree, free-space rebuild. | ✓ |
| 3 | Metadata authentication/checksums and duplicated critical metadata. | ✓ |
| 4 | Encrypted volume creation, key hierarchy, filename/data encryption. | ✓ |
| 5 | Data records with physical checksum and logical hash. | |
| 6 | First-party RustOS zstd codec and RustFS compression integration. | |
| 7 | Chunk table, refcounts, reverse refs, reflinks, dedupe index. | |
| 8 | Online scrub. | |
| 9 | Offline check and rescue. | |
| 10 | TRIM/discard queues and mkfs-time discard. | |
| 11 | Device health baselines and health-triggered scrub. | |
| 12 | Fuzz, proptest, crash-replay, and corruption-injection suites. | |

Stage 1 landed as the copy-on-write `rustfs` driver: self-identifying
block headers (§8), the four-slot superblock ring and transaction root +
inline commit record (§4 / §14), and a copy-on-write inode map backing the
full POSIX read/write/security/timestamp surface. It replaced the old
journaled driver outright and passes its unit tests, the 1 GiB `fssoak`,
the posix suite, the rustfs-over-virtio-blk QEMU vertical, and the
`fuzz_mount` metadata-decode harness.

Stage 2 replaced the flat Stage-1 structures with the copy-on-write B-trees
this spec fixes: a single generic node implementation (`src/btree.rs`) backs
both the **inode tree** (keyed by inode number, superseding the two-level
inode map and its format-time `inode_count` cap) and a per-file **extent
tree** (logical block → `(physical run, length)`, superseding the
12-direct + single-indirect map so a file may span the whole volume). The
transaction root now names the inode-tree root and the next inode number;
the mount-time free-space rebuild walks those trees, and a two-cursor
allocator keeps sequential data contiguous so large writes collapse to one
extent.

Stage 3 replaced the fast physical checksum in the block header with a
**keyed authenticator** (HMAC-SHA256 through `lib/crypto`, §5/§8) covering
identity + payload, and gave every metadata block **two physical copies** (a
primary and a companion mirror at `primary + 1`, §5 — critical metadata
copies: 2 minimum). One read path serves all metadata — superblock-ring
slots, transaction roots, B-tree nodes, and directory blocks: it reads the
primary, falls back to the companion when the primary fails to authenticate,
and repairs the bad copy from the good one; both copies bad fails closed,
never a panic (§8, `AGENTS.md` §5.4 / §2.9). Metadata is allocated in
adjacent pairs, intra-transaction frees are reclaimed immediately so the
mirroring does not inflate a transaction's peak footprint, and the §16
acceptance tests for this stage — metadata bit-flip detection and repair from
the duplicate copy, wrong-key rejection, both-copies-bad fail-closed, crash
replay, and the extended `fuzz_mount` authenticated-header / duplicated-copy
sweeps — all pass. The authenticator key was a placeholder derived from the
volume UUID in Stage 3; Stage 4 replaced it with the real per-volume key
hierarchy.

Stage 4 made RustFS **encrypted at rest with no plaintext mode** (§5, §7).
`format` now takes a caller-supplied volume key and provisions a per-volume
key hierarchy through `lib/crypto` (`AGENTS.md` §2.12): a master key, wrapped
(AEAD) under a KDF of the volume key and stored only in wrapped form in every
superblock slot's plaintext discovery region, derives the
metadata-authentication key (the Stage-3 `MacKey` seam, no longer a UUID
placeholder), the filename key, and the content key. `open` recovers the keys
by unwrapping the master key; a wrong key never authenticates the wrapped
blob, so the mount is refused with `PermissionDenied`, fail-closed (§5.4),
never a panic (§2.9). File data and directory-entry names are encrypted with
`lib/crypto`'s ChaCha20-Poly1305 AEAD — each data and directory block carries
a 28-byte nonce+tag trailer, so a bit-flip in encrypted data or a name is
detected on read rather than mis-decrypted; directory blocks are
encrypt-then-MAC and the read path authenticates then decrypts. There is no
code path that lays out an unencrypted volume. The §16 acceptance tests for
this stage — wrong key refuses mount, plaintext cannot be created (the
filename and content are absent from the raw bytes), filename + data round
trip across a remount, an encrypted-data bit-flip is detected, crash replay
at every commit step, and the extended `fuzz_mount` encrypted-open sweep —
all pass. Stages 5–12 remain; each implementing session ticks this table and
`PLAN.md`.
