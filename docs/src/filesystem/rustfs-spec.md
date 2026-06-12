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
| Sparse files | Always active. All-zero logical ranges are stored as metadata-only ZERO/Hole extents, never a physical data record (§19). |
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
logical hash:                SHA-256 through lib/crypto (see note)
metadata authenticator:      lib/crypto keyed hash/MAC (HMAC-SHA256)
physical checksum:           FNV-1a 64-bit (fast, first-party)
critical metadata copies:    2 minimum
root history:                retained for rollback and safe discard
```

A future RustFS format may revise constants globally. A mounted v1 filesystem
must not expose runtime controls for them.

> **Logical-hash primitive (v1).** This spec originally named BLAKE3-256 for
> the logical hash, but `AGENTS.md` §2.12 — to which this spec is subordinate —
> requires cryptographic primitives to come from an audited crate wrapped
> behind `lib/crypto`, never hand-rolled or freshly imported "because it's
> easier". `lib/crypto` ships only the audited RustCrypto SHA-256, and adding a
> `blake3` crate would widen the trusted computing base with a SIMD backend
> that does not build cleanly on the bare-metal kernel targets (the
> freestanding-SIMD problem already pinned around for `chacha20` and
> `curve25519-dalek` in `.cargo/config.toml`). SHA-256 is a 256-bit
> collision-resistant hash that fills the integrity-and-dedupe role
> identically, so RustFS v1 uses it; a future format version may switch to
> BLAKE3 globally once it is available through `lib/crypto`.

---

## 6. Write/read pipeline

Write path:

```text
plaintext logical record
  -> all-zero detection (§19)
  -> if all zero: store a metadata-only ZERO/Hole extent and stop
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

The zero-detection step runs before compression, dedupe, encryption, and
physical allocation: an all-zero logical record never reaches those stages
and never consumes a physical block (§19).

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
- keys are never stored unwrapped on disk;
- fresh per-volume key material (the master key, the wrapping salt, and the
  wrap nonce) and the per-volume UUID are drawn from the platform RNG
  (`lib/rng`'s cryptographically secure `CsRng`, `AGENTS.md` §1/§4), not
  derived from the volume key, so the master key is independent of — and
  re-wrappable without — the volume key; a failed entropy draw fails closed
  (§5.4) and no volume is laid out with predictable key material.

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

> **Implementation.** `RustFs::format` takes an `EntropySource` seam onto the
> platform RNG and draws the master key, wrapping salt, wrap nonce, and UUID
> from it (`drivers/filesystem/rustfs/src/crypto.rs`). The driver never reaches
> for a global RNG; the concrete `CsRng` is injected at the composition root,
> mirroring the seam `kernel/mem`'s encrypted swap uses, so the driver stays
> architecture-neutral (§17.2). Only the wrapping key remains a deterministic
> KDF of the volume key and the random salt, because `open` must recompute it
> to unseal the master key on mount.

### Passphrase-derived volume key

The `VolumeKey` above is 256 bits of high-entropy material no human can type,
so it is never the thing an operator supplies at boot. The standard LUKS-style
indirection sits *above* the volume: an operator supplies a **passphrase**, and
the `VolumeKey` is derived from it with PBKDF2-HMAC-SHA256 (`lib/crypto`, the
same audited KDF that protects `/System/Security/Users`) over a per-volume
random salt and a tunable iteration count. Both public parameters travel beside
the volume in a small plaintext **unlock descriptor**
(`drivers/filesystem/rustfs/src/unlock.rs`, `UnlockDescriptor`): the analogue of
a LUKS header, laid down where the bootstrap can read it *before* anything is
decrypted (on a Pi SD image, a file on the FAT boot partition). The descriptor
is not secret — the salt only makes precomputation per-volume and the count
makes each guess expensive; the passphrase is never stored. A wrong passphrase
derives the wrong key, which `RustFs::open` rejects through the wrapped
master-key AEAD authentication (`PermissionDenied`), so a guess costs a full
mount attempt and there is no separate passphrase oracle (§5.4). The iteration
count is bounded (`UNLOCK_MIN_ITERATIONS..=UNLOCK_MAX_ITERATIONS`) and a
descriptor outside the range, with a wrong magic/KDF id, or a non-zero reserved
byte is refused fail-closed (§2.9 / §5.4.3).

This `VolumeKey`-from-passphrase layer is authored by the §11 installer flow
(it sets the real passphrase when it provisions the user's encrypted root) and,
for development, by the debug Pi image (a known passphrase, never shipped, like
the debug `root` account). A **shippable installer image must not** bake in a
known passphrase: the installed root is (re-)provisioned at install time under
the operator's chosen passphrase. The descriptor format and derivation are the
landed primitive; wiring the boot path to read the descriptor, prompt for the
passphrase on the console, mount the root, and serve
`/System/Security/Users` to login is staged (`plans/PI.md` P11).

> **Hardware-backed key storage (future, §19.9).** Typing a passphrase at every
> boot is the baseline available on any board. A platform with a hardware root
> of trust — a TPM with measured boot / sealed storage, an Arm `TrustZone`
> secure world, an Apple-style Secure Enclave, or the UEFI Secure Boot + TPM
> chain Windows `BitLocker` uses — should instead **seal** the `VolumeKey` (or
> the passphrase-derived wrapping key) to the platform's measured state and
> release it automatically when the boot chain is unmodified, falling back to
> the passphrase on a recovery path. That hand-off is a future *source* of the
> `VolumeKey` slotting in beside the passphrase path; it changes nothing about
> the on-disk volume. Physical attacks (cold-boot, decap) stay out of the
> charter threat model (§19.9): sealing bounds the remote/offline attacker, not
> one with the silicon in a lab.

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

> **Stage 5 implementation.** Of the data-record fields above, the **plaintext
> logical hash** (SHA-256 of the block's plaintext content) and the
> **physical checksum** (FNV-1a over the at-rest block) land in Stage 5,
> stored in a fixed 40-byte trailer appended to every file-data block after
> the Stage-4 crypto trailer (`drivers/filesystem/rustfs/src/integrity.rs`).
> The read path verifies the physical checksum first (media corruption is
> caught before the AEAD), authenticates-and-decrypts, then verifies the
> logical hash over the recovered plaintext; each layer fails closed and is
> kept internally distinct (`integrity::DataFault`). `physical location` is the
> extent map (Stage 2). The **compression state** field lands in Stage 6 as a
> per-block descriptor (a state byte plus the at-rest stored length) placed
> between the crypto trailer and the logical hash, so the physical checksum
> covers it (`drivers/filesystem/rustfs/src/integrity.rs`). `chunk id`, `chunk
> generation`, and `encryption domain` arrive with the chunk/refcount table and
> dedupe (Stage 7); until then a data record is named by its `(file, logical
> block)` extent and the trailer above.

---

## 9. Deduplication

Deduplication is mandatory and exact.

Rules:

- key is `BLAKE3-256(plaintext logical record) + length`;
- dedupe index is rebuildable and never authoritative;
- the dedupe index is a **bounded in-memory cache**, not an unbounded map: its
  resident RAM is capped (a missed duplicate is acceptable, so a full index
  evicts rather than grows);
- candidate matches are byte-verified before sharing;
- cross-domain dedupe is forbidden;
- shared chunks are immutable and refcounted;
- overwriting shared data creates a new physical record.

Missing a duplicate is acceptable. Merging unequal data is corruption.

> **Bounded dedupe-index RAM (implementation).** Because *missing a duplicate
> is acceptable*, the rebuildable dedupe index
> (`drivers/filesystem/rustfs/src/dedupe.rs`, `DedupeIndex`) is a fixed-budget
> cache rather than a map that grows with the volume. Its resident RAM is
> capped at **100 MiB**, split into a **20 MiB "frequently used" hot tier**
> (candidates promoted on a dedupe hit) and an **80 MiB general tier** (freshly
> written candidates). Each tier is a least-recently-used cache: once full it
> evicts its least-recently-used candidate (the hot tier demotes its eviction
> back into the general tier) instead of growing, so the index never exceeds
> its budget regardless of how much unique data the volume holds. Eviction only
> forgoes a future dedupe opportunity — it never affects correctness, since the
> chunk/refcount and reverse-reference trees remain authoritative and the index
> is rebuilt from them at mount. The per-entry footprint is deliberately
> over-estimated when deriving the per-tier entry caps, so the byte budgets are
> a hard ceiling, not an approximation.

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

> **Stage 6 implementation.** The first-party codec is the `lib/compress`
> crate (`AGENTS.md` §16.4 lists compression as a curated shared-library
> class): a `no_std`, allocation-free LZ77 codec with a `"RLZ1"` frame and
> LZ4-style token sequences — no external zstd/compression dependency (§3,
> `AGENTS.md` §2.12). RustFS compresses each file-data record before
> encrypting it (`compress → encrypt`) and stores the record raw when the
> compressed frame is not smaller than the logical block capacity. The read
> path decompresses after decrypting and before verifying the logical hash, so
> the hash still names the plaintext. *Dedupe before compression* and
> *compress only unique records* are satisfied trivially while dedupe is
> pending (Stage 7): every record is unique today, so every record is
> compressed; the order is preserved (`dedupe → compress → encrypt`) for when
> the dedupe stage lands.

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

### Directory-entry names

RustFS directory-entry names match ext4's rules so a name valid on one is
valid on the other:

- a name is **1..=255 bytes** long (the ext4 maximum);
- the only forbidden bytes are the path separator `/` and NUL — every other
  byte is stored verbatim, so arbitrary UTF-8 and high bytes are allowed;
- `.` and `..` are reserved for the VFS and are not creatable as names;
- names are compared **byte-for-byte**, so they are **case-sensitive**
  (`File`, `file`, and `FILE` are three distinct entries); RustFS performs no
  case-folding or Unicode normalisation — that policy, if any, belongs to the
  VFS.

A directory block is an array of **fixed-width 263-byte slots** (an 8-byte
header — 4-byte inode number, 4-byte name length — plus room for a
maximum-length 255-byte name). A fixed slot keeps directory lookup, insertion,
and removal O(1) per slot with no in-block compaction. The slot count per block
follows the block size (1 on a 512-byte block, 14 on a 4096-byte block); a
directory grows by whole copy-on-write blocks as entries are added, so `.` and
`..` span as many blocks as the block size requires.

### Online resize (grow)

A volume's committed block count is pinned in the superblock and may be
**smaller** than its backing device — for example after an administrator
enlarges the underlying partition, logical volume, or virtual disk. Mount
operates at the committed size and leaves any surplus device tail unused.

`RustFs::grow` extends a *mounted* volume to fill an enlarged device, online
and in place: it re-reads the device geometry, folds the newly available tail
blocks (which start free) into the free pool, and commits a new superblock
recording the larger size in a single atomic transaction. No existing data
moves, the new space is usable immediately without a remount, and a crash
before the commit point leaves the previous (smaller) committed size selected
on the next mount (§14). A device that has *shrunk* below the committed size is
rejected: online shrink would require relocating live tail blocks first and is
not offered.

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
| 5 | Data records with physical checksum and logical hash. | ✓ |
| 6 | First-party RustOS zstd codec and RustFS compression integration. | ✓ |
| 7 | Chunk table, refcounts, reverse refs, reflinks, dedupe index. | ✓ |
| 8 | Online scrub. | ✓ |
| 9 | Offline check and rescue. | ✓ |
| 10 | TRIM/discard queues and mkfs-time discard. | ✓ |
| 11 | Device health baselines and health-triggered scrub. | ✓ |
| 12 | Fuzz, proptest, crash-replay, and corruption-injection suites. | ✓ |

**RustFS v1 is complete: every stage above is shipped (§17 definition of
done).**

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
all pass. Stage 4 first derived the master key, wrapping salt, and UUID
deterministically from the volume key and geometry because the platform RNG
had not yet landed; once `lib/rng` shipped, `format` was given an
`EntropySource` seam and now **draws** the master key, salt, wrap nonce, and
UUID from the injected `CsRng` (§7) — the master key is independent of the
volume key and a failed draw fails closed, with the wrapping key the only
value still a deterministic KDF of the volume key so `open` can recompute it.

Stage 5 added the §6/§8 **data-integrity layer** to every file-data block: a
40-byte trailer after the Stage-4 crypto trailer holding a **logical content
hash** (SHA-256 of the plaintext, through `lib/crypto`, §2.12 — see the §5
logical-hash note on the SHA-256-vs-BLAKE3 choice) and a fast **physical
checksum** (first-party FNV-1a over the at-rest block,
`src/integrity.rs`). The write path hashes the plaintext, encrypts, then
checksums the at-rest block; the read path verifies the physical checksum
first (so media corruption is caught cheaply before the AEAD),
authenticates-and-decrypts, then verifies the logical hash over the recovered
plaintext. Each layer fails closed to a `DriverError` (never a panic, §5.4 /
§2.9) and is kept internally distinct (`integrity::DataFault` —
`Physical`/`Aead`/`Logical`), the seam Stage 8 scrub / Stage 11 health will
record against. The §16 acceptance tests for this stage — each of the three
layers detecting its own corruption class and failing closed, identical
plaintext sharing one logical hash while different plaintext differs (the
Stage 7 dedupe seam) even though the two blocks encrypt to distinct
ciphertext, and the integrity field surviving a remount and a copy-on-write
rewrite — all pass, alongside the crash-replay sweep, the 1 GiB `fssoak`, the
posix suite, and the QEMU vertical.

Stage 6 made compression **mandatory and always on** (§1, §10) with a
**first-party RustOS codec — no external zstd/compression dependency** (§3,
`AGENTS.md` §2.12 / §16.4). The codec landed as the new `lib/compress` crate:
a low-CPU, byte-oriented LZ77 ("zstd-fast-style") codec with a `"RLZ1"` frame
header, a greedy hash-table match finder, and LZ4-style literal/match token
sequences. It is `no_std`, allocates nothing (it works through caller-provided
slices), and is panic-free — `compress`/`decompress` are `Result`-based, the
declared output length is bounds-checked against the destination before any
byte is produced, and malformed compressed input returns an error, never a
panic (§10, §2.9). RustFS wires it into the §6 data-record pipeline: on write
the plaintext is hashed (the logical hash still names the plaintext, before
compression), then `compress → encrypt`, storing the record **raw** when the
compressed frame is not smaller than the logical capacity (the §1/§10 allowed
adaptive choice); on read `physical checksum → decrypt → decompress → verify
logical hash`. The §8 data-record **compression state** field is a per-block
descriptor (a state byte plus the at-rest stored length) placed between the
crypto trailer and the logical hash, so the fast physical checksum still
covers it; `data_capacity()` shrank by that descriptor accordingly. The full
content slot is always encrypted, so the Stage-4 crypto and Stage-5 integrity
layers are identical for compressed and raw records, and the logical hash
(the Stage 7 dedupe seam) is unchanged. The §16 acceptance tests for this
stage — codec round-trip / corpus / known-answer / malformed-input, an
incompressible record stored raw and round-tripping, a compressible file
shrinking its at-rest footprint yet reading back byte-identical across a
remount and a COW rewrite, integrity still catching a physical and a logical
corruption on a compressed block, and a new `fuzz_compress` decode harness
wired into `cargo xtask ci` and the soak — all pass, alongside the
crash-replay sweep, the 1 GiB `fssoak`, the posix suite, and the QEMU
vertical.

Stage 7 added **deduplication**, the chunk/refcount machinery the §4 model
names (§9, §6). A data record ("chunk") may be **shared** by more than one
`(file, logical block)`; sharing is **exact and verified** — a candidate is
taken only after its bytes are confirmed byte-identical to the incoming
record, so a missed duplicate is acceptable but unequal data is never merged
(§9). Two new copy-on-write trees reuse the one generic `src/btree.rs`
(`AGENTS.md` §2.2) and are named by the transaction root: a **chunk/refcount
tree** (keyed by a chunk's physical block → referrer count, encryption
domain, logical hash, length) and a **reverse-reference tree** (the same key →
the `(inode, logical block)` referrers, for scrub/check/health and safe
discard). To keep ordinary writes cheap, an unshared block carries an
*implicit* reference count of one and has **no** record in either tree; the
first share promotes it to an explicit chunk (refcount 2) and the last drop
frees it (`src/lib.rs` `store_block` / `release_block_ref` / `share_block_ref`).
Shared chunks are immutable: overwriting one sharer copies-on-write a fresh
record and drops the old refcount, leaving the other sharer intact. A
**reflink** (`RustFs::reflink`) is a copy-on-write clone that shares every
block with its source until a side is written. Discovery is driven by an
in-memory **dedupe index** (`(domain, length, logical hash) → candidate`)
that is **rebuilt from the chunk + reverse-reference trees at mount and is
never authoritative** (§9): every candidate is liveness-checked (its recorded
referrer's extent map still points at it) and byte-verified before sharing,
so a stale entry can never merge wrong data. The index is a **bounded cache**
(100 MiB, split 20 MiB frequently-used / 80 MiB general; see §9), evicting its
least-recently-used candidates rather than growing with the volume. Dedupe is
**scoped to the encryption domain** (§7) — the domain is carried in every chunk record and
index key — and the pipeline order is **`dedupe → compress → encrypt`** so
only unique records are compressed (§10). The §16 acceptance tests for this
stage — identical content sharing one chunk (refcount 2) while distinct
content does not, byte-verify-before-share refusing an injected colliding
entry, COW-on-overwrite leaving the other sharer intact, a reflink sharing
until written, refcount-to-zero freeing the chunk with the free-space rebuild
agreeing, the dedupe index rebuilding at mount and yielding the same sharing,
dedupe staying within the domain, and integrity + compression holding on a
shared chunk across a remount and a COW rewrite — all pass, alongside the
crash-replay sweep, the 1 GiB `fssoak` (its fill now uses distinct per-file
content so dedupe does not mask exhaustion), the posix suite, the QEMU
vertical, and the `fuzz_mount` harness extended to decode the chunk and
reverse-reference records.

Stage 8 added **online scrub** (§12), a resumable, interrupt-safe
verify-and-repair pass that walks the live volume while it stays mounted and
leans on the redundancy and integrity seams the earlier stages built
(structure rebuild is the later `check`, not scrub). `RustFs::scrub` is an
inherent driver operation (not a widening of a frozen `Filesystem*` trait,
`AGENTS.md` §2.4), **capability-gated** on `CAP_FS_MOUNT` (refused
fail-closed and logged otherwise, §5.4). It (1) authenticates **both** physical
copies of every live metadata block — the committed superblock slot, the
transaction root, the inode and per-file extent B-trees, and the chunk and
reverse-reference trees — **repairing** a bad copy from its good companion
(the Stage 3 seam) and recording a both-copies-bad block as unrepairable
(never a panic, §5.4 / §2.9); (2) runs every live file-data block through the
Stage 5/6 pipeline and **classifies** any failure by its `integrity::DataFault`
(`Physical`/`Aead`/`Logical`), recording it (deep data repair is a later
stage); and (3) **recomputes** the chunk refcounts and reverse-reference sets
from the live inode/extent trees and reconciles them with the on-disk trees
(§9), correcting a divergence toward the extent-derived truth without dropping
a referrer. Scrub is **resumable**: a `ScrubBudget::Inodes(n)` call persists a
rebuildable **scrub-progress record** (a `BlockType::ScrubProgress` block
reached from the transaction root, holding the resume cursor and accumulated
counts, §4) and resumes to the identical accumulated `ScrubReport`; a crash
mid-scrub leaves a mountable volume (ordinary recovery never needs scrub, §14)
and a corrupt progress record simply restarts the scrub. Scrub **reports,
never silently mutates** — it returns a structured `ScrubReport` and logs its
outcome through `lib/log` with a stable event ID (§5.4 / §19.4), and a clean
scrub changes nothing and is idempotent. The §16 acceptance tests for this
stage — clean/idempotent scrub, single-copy metadata repair from the
companion, data `Physical`/`Logical` fault classification, refcount and
reverse-reference divergence detection and correction, resumability matching an
uninterrupted pass plus a crash-mid-scrub remount, a shared chunk accounted
once within its encryption domain, the capability gate, and integrity +
compression + dedupe invariants surviving a scrub/remount/COW rewrite — all
pass, alongside the crash-replay sweep, the 1 GiB `fssoak`, the posix suite,
the QEMU vertical, and the `fuzz_mount` harness extended to drive the
scrub-progress decode path.

Stage 9 added **offline check and rescue** (§12), the recovery operations
scrub deliberately does not attempt. Both reuse the seams the earlier stages
built rather than re-implementing them (`AGENTS.md` §2.2) — the §8 block
identity + companion mirror, the `DataFault` classes, the chunk/reverse-ref
trees, and the free-space / dedupe-index rebuilds. `RustFs::check` is the
**offline superset** of the online scrub, run on a mounted handle and
**capability-gated** on `CAP_FS_MOUNT`: it rebuilds the rebuildable derived
state first — the free-space bitmap (§4) and the dedupe index (§9) — from the
authoritative trees (sharing the one `rebuild_free_space` walk `open` uses), so
a corrupt derivation can never keep a sound volume unmountable; reuses the
scrub verification core (`verify_everything`) to verify/repair metadata copies,
classify data faults, and reconcile refcounts; validates the directory tree
(an entry to a missing inode is a *dangling* finding, reported not auto-
deleted); and detects and **reclaims orphaned inodes**. It returns a structured
`CheckReport` (the embedded scrub `verification`, directories checked, dangling
entries, orphans found/reclaimed, derived-state rebuilt, and the count of
findings it could not safely fix), is idempotent, and commits only when it
actually repaired something. `RustFs::rescue` extracts data from a volume too
damaged to mount: it is **read-only** on the device (the repair-on-read writes
are suppressed) and capability-gated, recovers the keys from a surviving
superblock discovery header, **scans** every block for a self-identifying
transaction root whose commit record validates (`TxnRoot::decode_any`, needing
no externally-supplied generation), picks the highest-generation root, maps its
inode/extent metadata to files, and **extracts** the readable file data,
running every block through the Stage 5/6 integrity pipeline and emitting only
blocks that pass to a caller-supplied `RescueSink` (a failing block is skipped,
never handed back). It returns a structured `RescueReport`. The §16 acceptance
tests for this stage — a clean check sound and rebuilding nothing (idempotent),
check rebuilding a corrupt free-space/dedupe derivation with the volume staying
mountable, check reclaiming an orphan and correcting a refcount divergence
while reporting an unrepairable data fault, the check + rescue capability
gates, rescue discovering a root and extracting files from a wounded
superblock ring (read-only and repeatable), and rescue never emitting a block
that fails integrity — all pass, alongside the crash-replay sweep, the 1 GiB
`fssoak`, the posix suite, the QEMU vertical, and the `fuzz_mount` harness
extended to drive the offline `check` and the `rescue` root-scan / extraction
decode paths.

Stage 10 added **TRIM/discard** (§11, §15.10), returning freed space to the
device **safely** and reusing the deferred-free machinery rather than a second
free-tracking mechanism (`AGENTS.md` §2.2). The `Block` ABI gained a versioned
discard surface (`discard_capability` / `discard`, an `abi-v1` extension, not a
widening of the frozen read/write methods, §2.4 / §9); a device without discard
support is *recorded, not failed*. Freed blocks enter a transient, in-memory
**pending-discard queue** as a committed transaction reclaims them
(`finish_txn`). `RustFs::trim`, **capability-gated** on `CAP_FS_MOUNT`
(fail-closed and logged otherwise, §5.4), discards a queued block **only if it
is still free** at trim time — the mount-time free-space rebuild marks every
block reachable from the committed root (every reflink target and every deduped
chunk at refcount ≥ 1 included) as used, so a freed-then-reallocated or
still-shared block is skipped and never discarded; this is the §11 hard
constraint that discard may never destroy data reachable from any retained root,
snapshot, reflink, deduped extent, or recovery root. Still-free blocks are
coalesced into contiguous runs, each aligned **inward** to the device's discard
granularity (the unaligned edges requeued), and at most `TRIM_BATCH_RANGES` runs
issue per call (the remainder stays queued). RustFS never assumes a discarded
block reads back as zero, and there is **no** `nodiscard` / `trim=off` mode. The
queue is rebuildable transient state (§4): never persisted, so a crash mid-trim
drops it, the volume remounts cleanly, and no live data is lost. `trim` returns
a structured `TrimReport` and logs its outcome with a stable event ID in the
`rustfs` `12000..13000` range; `format` issues a full-range discard on a
discard-capable device before laying down the encrypted structures, recorded-
not-failed without support. The §16 acceptance tests for this stage — the
capability gate, an unsupported device draining the queue recorded-not-failed,
contiguous free blocks coalescing into one granularity-aligned range, inward
alignment requeuing the edges, per-request-cap splitting, batch rate-limiting
draining over passes, a reallocated and a still-dedupe-shared block never being
discarded, the transient queue dropping across a crash with no live data lost,
and mkfs full-range discard (recorded-not-failed without support) — all pass,
alongside the crash-replay sweep, the 1 GiB `fssoak`, the posix suite, the QEMU
vertical, and the `fuzz_mount` harness (the discard queue is pure in-memory
transient state and adds no on-disk decode path).

Stage 11 added **device-health baselines and health-triggered scrub** (§11,
§15.11), giving RustFS a notion of the volume's health so it can decide *when* a
scrub is worth running, reusing the earlier stages' seams rather than a second
integrity or scrub path (`AGENTS.md` §2.2). The `Block` ABI gained a versioned
`device_health()` surface returning `DeviceHealth::Available(HealthSnapshot)` —
the SMART/NVMe-style counters the §11 health-field list enumerates — or
`DeviceHealth::Unavailable` (a device without telemetry is *recorded, not
failed*; the default implementation reports `Unavailable`). A self-identifying
`BlockType::HealthBaseline` block reached from the transaction root (like the
Stage-8 scrub-progress record) **persists** the last clean device snapshot the
next pass compares against plus the volume's accumulated filesystem-observed
fault counters — metadata copy-repairs/unrepairable (the Stage-3 companion seam)
and per-class `DataFault`s (the Stage-5 seam); both are persisted, not
rebuildable, because a repaired transient fault leaves no trace in the live
trees (§4). `format` stores the initial baseline at mkfs time, and a crash
mid-update leaves the previous committed baseline (or none) selected and never
blocks a mount (§14). `RustFs::health` is an inherent driver operation
**capability-gated** on `CAP_FS_MOUNT` (refused fail-closed and logged
otherwise, §5.4): it reads the current telemetry, classifies the volume against
the documented `HealthThresholds::DEFAULT` (`Healthy` / `Degraded` / `Failing`,
taking the worse of the device and filesystem signals — no magic numbers, §2.1)
and — when the device's unsafe-shutdown counter (a metadata scrub) or
media-error counter (a deep scrub) has risen since the baseline — **triggers a
scrub** through the Stage-8 machinery (its gate, budget, and resumable core,
never a parallel verifier), folding its findings into the counters. It then
stores the current telemetry as the new baseline, returns a structured
`HealthReport`, and logs its classification (and any triggered scrub) with
stable event IDs in the `rustfs` `12000..13000` range. The §16 acceptance tests
for this stage — the capability gate, a no-telemetry device still classifying
and persisting a baseline that survives a remount, the classification crossing
healthy → degraded → failing as the device media-error count climbs, an
unsafe-shutdown delta triggering a Stage-8 scrub (and the advanced baseline
triggering no further scrub), and the persisted baseline surviving a crash at
every write count during its update with no live data lost — all pass, alongside
the crash-replay sweep, the 1 GiB `fssoak`, the posix suite, the QEMU vertical,
and the `fuzz_mount` harness extended to drive the health-baseline decode path.

Stage 12 added the **fuzz, crash-replay, and corruption-injection suites**
(§15.12, §16), the adversarial superset that hardens everything the earlier
stages built without adding a new on-disk feature and without a second
integrity, scrub, or decode path (`AGENTS.md` §2.2). The §16 "fuzz targets for
mount, metadata decode, directory decode, compression decode, check, and
rescue" are all present: the `fuzz_mount` harness (`tests/fuzz_mount.rs`) drives
the mount / metadata / scrub-progress / health-baseline / check / rescue decode
paths and now also the **directory-block decode** path (`read_dir`/`lookup`
decrypt and parse the encrypted dirent payload that the mount-time free-space
walk never reads), while the `rustos-compress` `fuzz_compress` harness covers
compression decode; every harness keeps the single invariant "returns a
`Result`, never panics, fails closed" and is wired into `cargo xtask fuzz` /
`--quick` / `ci` / the nightly `--soak`. The crash-replay sweep is generalised
to **every commit step across every representative transaction** — create,
write, truncate, remove, reflink, scrub, check, trim, and health — asserting
that for each write-budget cut-off the re-opened volume always mounts on a whole
transaction boundary, the committed state is fully present or fully absent
(never torn), and the witness file is never lost (§14). The
**corruption-injection suite** systematically wounds each on-disk structure
class — superblock-ring slot, transaction root, the inode / extent / chunk /
reverse-reference B-trees, directory block, the scrub-progress and
health-baseline records, and each data-integrity layer — in **one** copy and in
**both** copies, asserting the documented seam behaviour: a single bad copy is
always repaired from the §8 companion mirror (volume mounts, scrub reports
nothing unrepairable, check is sound, data intact); both copies of mount-
critical metadata never tear (the mount fails closed or recovers an earlier
whole, consistent committed root via the superblock ring, §14); a both-copies-
bad directory still mounts but reads fail closed and scrub records it
unrepairable; the transient scrub-progress/health-baseline records recover
gracefully (scrub restarts, health re-derives); and an unmirrored data block's
fault is detected, classified by its `DataFault` layer, and surfaced as a fail-
closed `DeviceFault`, never silently repaired. These reuse the existing
`MemBlock` write-budget + fault-injection helpers, the `DataFault` classes, and
the `verify_everything` scrub/check core (`AGENTS.md` §2.2). With Stage 12
shipped, **RustFS v1 is complete** (§17).

---

## 19. Sparse files (ZERO/Hole extents)

Sparse-file support is mandatory, always enabled, and not tunable (the
authoritative appendix is `.junie/SPARSE.md`). RustFS stores a logical
all-zero range as metadata only — never a physical data record, a zstd
payload, a dedupe chunk, or an encrypted data blob.

**Representation.** A hole is an *unmapped* logical range. RustFS represents
holes **implicitly** as gaps between the per-file extent-tree mappings (§4,
§6) — the form `.junie/SPARSE.md` §2/§3 permit alongside an explicit ZERO
extent. An extent run always names physical data; a logical block with no
covering run is a hole. The extent map stays sorted by logical offset with no
overlapping runs (the B-tree invariant, normalised by `extent_assign` /
`extent_remove`). No new on-disk field, extent kind, or format-version bump is
introduced: a hole is the *absence* of an extent, so there is nothing extra to
checksum, encrypt, compress, dedupe, scrub, relocate, or trim.

**Write pipeline.** `store_block` scans the full logical record for all-zero
content (`is_all_zero`, a cheap bounded first-party scan, §16/§17 of the
appendix) **before** the logical hash, dedupe lookup, compression, encryption,
or physical allocation. An all-zero record drops the `(inode, block)` mapping
(making the block a hole) and releases any prior physical block through the
normal COW/refcount/free path — a block still referenced by a reflink, a
deduped owner, or a retained recovery root stays live (§9, §14). A zero range
is never entered in the dedupe index and never passed to the compressor.
Repeated *non-zero* data (e.g. `0xFF`) is not special-cased; it follows the
normal zstd/RAW path (§10). No RLE/FILL storage mode exists.

**Read, extension, truncation.** A read of a hole synthesises zero bytes with
no physical I/O (`read_file`). Extending a file (a larger `truncate`, or a
write past EOF) leaves the new range a hole. Shrinking frees the data extents
beyond the new EOF through the normal path; removed holes need no physical
free. A partial write that makes the whole resulting logical record zero
becomes a hole (the read-modify-write reconstructs the full record, so
`store_block` sees the zeros).

**Interaction with the other layers.** Scrub, check, and rescue iterate the
extent runs only, so a hole is never read, never faulted on, and needs no
data-block recovery; check validates that the extent map is ordered and
non-overlapping (§12). Space accounting separates logical size (includes
holes) from allocated data (excludes holes, bar metadata overhead): a 10 MiB
all-zero file reports a 10 MiB logical size and zero mapped data blocks. Because
every volume is encrypted, a hole also leaves no plaintext data payload for the
zero range; only the surrounding metadata is protected, as for any inode.
