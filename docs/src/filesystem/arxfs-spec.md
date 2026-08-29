# ARXFS specification

Status: implementation spec  
Target: TAIRiX  
Driver path: `drivers/filesystem/arxfs/`

ARXFS is the native TAIRiX filesystem: copy-on-write, encrypted, checksummed,
compressed, deduplicating, SSD-aware, and recoverable. It is optimised for high
I/O throughput, low CPU use, data integrity, and clean fsck/recovery.

This spec is subordinate to `AGENTS.md`; conflicts are resolved in favour of
`AGENTS.md`.

This is the authoritative ARXFS implementation specification, delivered
**in stages** (see §18 — Staged delivery & status). It lives in the book so it
ships with the rest of the documentation; the companion user-facing page is
`docs/src/filesystem/arxfs.md`, and the staged plans this spec is delivered
against are `plans/SPARSE.md`, `plans/ARXFS-METADATA.md`,
`plans/ARXFS-WRITEBACK.md`, `plans/ARXFS-SNAPSHOT.md`, and
`plans/ARXFS-FEC.md`. All of them are kept in step in the same change. There is
exactly one `arxfs` driver and one on-disk version.

---

## 1. One mandatory profile

ARXFS has **one production profile**. All features below are **enabled by
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

Normal TAIRiX mount security flags such as `ro`, `noexec`, `nosuid`, and
`nodev` remain valid because they are permission policy, not ARXFS feature
configuration.

---

## 2. Mandatory feature table

| Area | Mandatory ARXFS v1 behaviour |
|---|---|
| COW | No committed metadata block is overwritten in place. |
| Transactions | Mount highest valid committed root after ordinary power loss. |
| Metadata integrity | Every metadata block is self-identifying and authenticated/checksummed. |
| Data integrity | Every physical record is checksummed; every logical record has a strong hash. |
| Redundancy | Critical metadata has at least two physical copies. |
| Encryption | Every ARXFS volume is encrypted. Plaintext ARXFS is forbidden. |
| Compression | First-party TAIRiX zstd-fast-style compression is always active. |
| Deduplication | Exact verified dedupe is always active; it may miss duplicates but may never merge unequal bytes. |
| Shared extents | Reflink/shared immutable chunks are core storage. |
| Sparse files | Always active. All-zero logical ranges are stored as metadata-only ZERO/Hole extents, never a physical data record (§19). |
| TRIM | mkfs discards the target range when supported; mounted ARXFS trims safely in batches. |
| SMART/NVMe health | Health snapshots are stored and used when exposed by the storage stack. |
| Scrub | Online verification and repair from redundant copies. |
| Check/fsck | Offline structural validation, repair, and index rebuild. |
| Rescue | Damaged-volume root discovery and file extraction. |
| Time | All persistent timestamps use TAIRiX `Time64`. |
| Security | POSIX bits + ACLs + capability gates on every inode. |
| Extended metadata | Every inode has a namespaced extended-attribute store (encrypted, mirrored, COW); foreign per-file metadata (Acorn/Amiga/Atari/Mac) is preserved across copies (§21). |
| Symbolic links | A first-class inode kind whose target is stored as node data; a volume holding one declares the `symlinks` incompatible feature so a reader without it refuses the volume rather than misreading it (§20). |
| Hard links | An inode may be named by several directory entries; `nlink` counts them and its storage is freed only at zero. A volume holding one declares the `hardlinks` incompatible feature, because a reader without it would free an inode a surviving name still reaches (§20.5). |
| Feature declaration | The superblock carries an incompatible-feature word; a volume declaring a bit the reader does not implement is refused at mount, never mounted and misread (§4). |
| Durability ordering | A commit makes every block it publishes durable before the superblock slot that publishes them, through one mandatory barrier. A commit that cannot barrier does not publish (§22). |

---

## 3. Repository and dependency rules

Primary driver:

```text
drivers/filesystem/arxfs/
```

Internal modules (`src/`):

```text
lib        the volume handle, inode/extent model, and the POSIX surface
superblock the ring slot format          header    block identity + authenticator
transaction the root and commit record   btree     the one generic COW node
allocator  the two-cursor allocator      allocmap  the on-disk paged free map
cluster    compressed cluster extents    xform     the ClusterCache seam
integrity  logical hash + checksum       crypto    the per-volume key hierarchy
dedupe     chunk/refcount + reverse refs discard   the pending-discard queue
scrub      online verify and repair      check     offline check
health     device-health baselines       unlock    passphrase-derived key recovery
```

There is one module per concern and no module without a consumer. `mount`,
`format`, and `rescue` are operations on the volume handle in `lib`, not
separate modules; `compression` is the shared `lib/compress` crate.

Shared ABI types belong in `lib/abi`. Cryptographic primitives and key handling
must go through `lib/crypto`. ARXFS must not link against kernel internals.

Compression dependency rule:

```text
No external zstd/compression dependency is allowed.
```

ARXFS must not use `zstd`, `zstd-safe`, `zstd-sys`, `libzstd`, a vendored C
library, a registry compression crate, or code downloaded from another site.
The zstd-compatible codec subset used by ARXFS must be written in the TAIRiX
workspace in Rust.

Placement:

- If only ARXFS uses it: `drivers/filesystem/arxfs/src/compression/`.
- If another crate needs it: add a first-party `lib/compress` crate and update
  `AGENTS.md`, `PLAN.md`, docs, tests, and CI in the same change.

Crypto is the exception from “roll our own”: ARXFS uses audited primitives via
`lib/crypto` and must not hand-roll encryption or authentication primitives.

---

## 4. On-disk model

ARXFS stores immutable physical records referenced by logical extents.

```text
superblock ring
  -> recent transaction roots
      -> root tree
      -> inode tree
      -> extent tree
      -> chunk/refcount tree
      -> reverse-reference tree
      -> allocation map (in place, not a COW child)
      -> device-health tree
      -> rebuildable secondary indexes
```

The superblock also carries an **incompatible-feature word**: a bitmap of
on-disk structures a reader must understand to mount the volume at all. It is
plaintext (a reader must consult it before it can unwrap a key) and is covered
by the block's keyed authenticator, so a bit that survives the check is one the
volume's own writer set. A volume declaring a bit outside the reader's
supported set is **refused with that reason**, never mounted and misread. A bit
is set the first time the volume uses the structure it names, so a volume that
has never used one stays readable by a build that does not know it.

Authoritative metadata:

- superblock ring and root history;
- inode tree;
- extent tree;
- chunk table;
- refcount tree;
- reverse-reference tree.

Rebuildable metadata:

- allocation map;
- dedupe index;
- directory acceleration indexes;
- health summaries;
- scrub progress;
- a verification pass's transient scratch arrays (§12);
- allocation heat maps.

A corrupt rebuildable structure must not make a valid volume unmountable.
`arxfs check` must rebuild it from authoritative metadata.

### Tree iteration and mutation (resident footprint)

Every metadata tree is read through a **bounded, resumable walk**: one step
descends one root-to-leaf path and yields that leaf's records, so the bytes an
operation holds are set by the block size, never by the tree. The walk's
position is a single key, so a caller may mutate the tree between steps and may
persist where it stopped and resume later — a resumed walk yields exactly what
an uninterrupted one would. Callers that must touch every *node* (the
allocation-map rebuild, freeing a whole tree) take them from the walk's own
path as it moves, so no operation ever materialises a tree's records or node
list. A **directory** is read the same way, through a cursor holding one
directory block, so path resolution, a listing, and the structural check each
cost the block size and not the directory.

A **mutation is bounded in the same way, and in the stack as well as the heap.**
An insert or a remove descends once recording the path, edits the leaf in place,
and walks back up rewriting each ancestor in turn — nothing recurses, so no cost
scales with the tree's depth. The node buffers an edit needs (the node being
rewritten, plus the adjacent pair a split, borrow, or merge moves entries
between) are a fixed handful the mount lends it, so a steady-state edit
allocates nothing and decodes nothing per record. Every level a mutation
re-enters on the way up is validated exactly as the descent validates it.

A read or a mutation refuses a tree whose shape is impossible (a level that does
not decrease on the way down or increase by one on the way back up, an entry
count wider than its block, keys that do not ascend within a leaf) rather than
reading past a buffer, descending forever, or running off the stack; and a walk
never ends early and silently, because a caller freeing or accounting for every
record would then miss some.

### On-disk allocation map (mount-time footprint)

Free space is tracked by a contiguous **on-disk paged allocation map**, not
rebuilt in RAM at every mount. On a fresh volume the region sits immediately
above the superblock ring, at block `RING_BLOCKS`:

```text
[ header block | summary blocks | bitmap pages ]
```

- the bitmap pages hold **one bit per device block**;
- the summary holds **one `u16` free-block count per bitmap page**, so
  allocation skips a wholly-full page without reading it, and a page the
  summary reports wholly free is *synthesised as zeroes* rather than read —
  which is also why laying the map out on a huge volume writes only the
  summary, never a bitmap page per terabyte;
- every region block is sealed with the ordinary keyed block header under
  `BlockType::AllocMap`, **one copy, deliberately not mirrored**: free space
  is rebuildable, so a page that fails to authenticate makes the mount
  rebuild rather than repair it.

Because the region holds rebuildable, non-authoritative state, it is **not
copy-on-written**; it is updated **in place**. That sidesteps the
self-allocation problem an authoritative copy-on-written free-space tree
would have — allocating space to record where space is free.

**Crash safety.** The region header carries a clean/dirty stamp naming the
transaction generation the map reflects. The first mutation after a clean sync
stages an invalid stamp with the transaction's authoritative dirty blocks; the
commit barrier makes it durable before any map page may reach the device.
Resident pages remain in the bounded map cache between commits. An explicit
sync (`fs_sync`) moves them into the shared dirty set, writes them in bounded
runs, forces the device cache once, then stamps the region clean at the
committed generation. A cache eviction uses the same set and may write only
after the invalid stamp is durable. A failed stage, page write, or barrier —
whether under a sync or under an eviction — discards the untrusted cache and
staging; the next allocator operation rebuilds from the committed trees before
touching state. That is the *only* path to a rebuild: a transaction refused for
an ordinary reason undoes its own marks instead, at its own cost (§22).
Ordinary commits therefore leave the on-disk map dirty while it stays exact
in RAM. A mount adopts the map only when it authenticates at the address the
committed transaction root names (`TxnRoot::alloc_map_start`), its coverage
matches the committed volume size (`TxnRoot::alloc_map_covered`), and its
stamp is clean at the root's generation; anything else — after an ordinary
crash, most mounts — rebuilds the map from the authoritative trees and
rewrites it. A crash between syncs therefore costs one rebuild, never a
correctness problem. `mkfs` leaves the map stamped clean, so a freshly built
image mounts fast.

**Resident cost.** The region is read through a bounded LRU cache of at most
**64 pages per region** (`MAX_CACHED_PAGES`, shared with the
reconcile scratch arrays of §12),
volume-independent: a cache miss costs one block read, never a failure, so
several 100 TB+ volumes mount together on a 1 GiB machine. A changed page
moves out of that cache when it enters the shared dirty set, so
staging never retains a second page-sized copy; its drain window is bounded
below the cache footprint. Other write-path-only structures — the map's
allocation/metadata cursors, the
per-transaction bookkeeping (blocks a not-yet-committed transaction has
allocated or released), the pending-discard queue (capped at
`MAX_PENDING_DISCARD`, a dropped entry merely stays un-discarded until a
future free, trim pass, or rebuild requeues it), and the dedupe index (§9) —
are grouped into one `Allocator` held as `Option<Allocator>` on the mounted
handle, bounded the same way and never sized to the device.

**Read-only mounts build nothing.** A read-only handle holds `None`: it
cannot allocate, free, dedupe, or trim by construction, and it reads no
allocation-map block, builds no cache, and walks no tree — mounting a
read-only volume such as `/System` costs a handful of block reads (the
superblock ring and the committed root), not a walk of its contents.
`statfs` on a read-only mount reports the free-block count committed in the
transaction root (`TxnRoot::free_count`) directly, with no map read or
rebuild involved.

`grow` widens the map in place when the region length is unchanged;
otherwise it relays the region (preferring the freshly added tail, else the
first contiguous free run) and rebuilds it from the authoritative trees.
`check` always rebuilds the map from the authoritative trees; a clean check
therefore leaves every committed structure and the map byte-identical, apart
from the transient scratch run its reconcile borrows from free space and
releases (§12).

---

## 5. Fixed v1 constants

ARXFS constants are not user-tunable. These are the values the driver actually
uses:

```text
filesystem block size:       the device's logical block size, 512..4096
metadata block:              one filesystem block (128-byte identity header)
data record:                 one filesystem block, block_size - 69 usable
                             (28-byte crypto trailer, 5-byte stored-form
                             descriptor, 36-byte integrity trailer)
compression cluster:         16 filesystem blocks, compressed as one frame
                             into fewer physical blocks
small-file storage:          one data record; no inline or packed form
logical hash:                SHA-256 through lib/crypto (see note)
metadata authenticator:      lib/crypto keyed hash/MAC (HMAC-SHA256)
physical checksum:           CRC-32C (Castagnoli, fast, first-party via lib/crc32c)
critical metadata copies:    2 minimum, companion at primary + 1
root history:                retained for rollback and safe discard
symlink target maximum:      FS_SYMLINK_MAX (4096 bytes, the ABI path bound)
```

**Targets not yet met (stage 19).** A wider metadata block (16 KiB) and data
record (128 KiB normal, 256 KiB large-sequential) than one device block, and an
inline or packed small-file form, are design targets this driver does not
implement: it uses one device block for both. They matter most where the device
block is smallest — a 512-byte SD card leaves 443 usable content bytes per data
record and 384 payload bytes per B-tree node, so trees are far deeper and
extent records far more numerous than a 4 KiB volume's. Reaching the targets
requires a filesystem block size decoupled from the device's logical block
size, which is an on-disk change and its own stage. Anything implemented
against this section must read the values above, not the targets.

A future ARXFS format may revise constants globally. A mounted v1 filesystem
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
> identically, so ARXFS v1 uses it; a future format version may switch to
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

> **Cluster granularity (implementation).** The compression step operates on
> whole aligned **clusters** of 16 logical blocks, never on a single block: a
> compressed frame stored inside one fixed 1:1 block can free nothing (its
> padding is encrypted, so not even a lower layer could reclaim it), so a
> single-block record is always stored raw and only a whole-cluster write is
> compressed — into strictly fewer contiguous physical blocks, recorded as one
> compressed extent (§10). Zero detection still runs first: an all-zero
> cluster becomes holes, not a compressed extent.

---

## 7. Encryption

ARXFS volumes are always encrypted. `mkfs.arxfs` must fail if no valid key
source is supplied by the installer, recovery flow, or storage policy service.

Rules:

- no plaintext ARXFS format exists;
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

Secret-holding allocations inherit TAIRiX zero-on-free requirements.

> **Implementation.** `ARXFS::format` takes an `EntropySource` seam onto the
> platform RNG and draws the master key, wrapping salt, wrap nonce, and UUID
> from it (`drivers/filesystem/arxfs/src/crypto.rs`). The driver never reaches
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
(`drivers/filesystem/arxfs/src/unlock.rs`, `UnlockDescriptor`): the analogue of
a LUKS header, laid down where the bootstrap can read it *before* anything is
decrypted (on a Pi SD image, a file on the FAT boot partition). The descriptor
is not secret — the salt only makes precomputation per-volume and the count
makes each guess expensive; the passphrase is never stored. A wrong passphrase
derives the wrong key, which `ARXFS::open` rejects through the wrapped
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

On verification failure, ARXFS must try redundant copies, repair bad copies
from good copies, return an error if no valid copy exists, and record affected
inode/range details for health, scrub, and check.

> **Stage 5 implementation.** Of the data-record fields above, the **plaintext
> logical hash** (SHA-256 of the block's plaintext content) and the
> **physical checksum** (CRC-32C over the at-rest block, through the shared
> `lib/crc32c` — a first-party error-detecting checksum with a portable
> baseline and a hardware `crc32c*` / SSE4.2 path selected once at boot and
> self-verified against the baseline) land in Stage 5, stored in a fixed
> 36-byte trailer (32-byte SHA-256 logical hash + 4-byte CRC-32C) appended to
> every file-data block after the Stage-4 crypto trailer
> (`drivers/filesystem/arxfs/src/integrity.rs`).
> The read path verifies the physical checksum first (media corruption is
> caught before the AEAD), authenticates-and-decrypts, then verifies the
> logical hash over the recovered plaintext; each layer fails closed and is
> kept internally distinct (`integrity::DataFault`). `physical location` is the
> extent map (Stage 2). The **compression state** field is the per-block
> **stored-form descriptor** (a state byte plus a `u32`) placed between the
> crypto trailer and the logical hash, so the physical checksum covers it
> (`drivers/filesystem/arxfs/src/integrity.rs`): a block is a raw
> single-block record, the head of a compressed cluster (carrying the whole
> frame length), or a numbered continuation of one — so a misdirected or
> reordered stored block fails closed on read. The logical hash names the
> decrypted content slot: the plaintext for a raw record (the dedupe seam), or
> the block's slice of the compressed frame for a cluster block, whose
> end-to-end plaintext integrity then rests on the AEAD plus exact-size
> decompression. `chunk id`, `chunk
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
> (`drivers/filesystem/arxfs/src/dedupe.rs`, `DedupeIndex`) is a fixed-budget
> cache rather than a map that grows with the volume. Its resident RAM is
> capped at **100 MiB**, split into a **20 MiB "frequently used" hot tier**
> (candidates promoted on a dedupe hit) and an **80 MiB general tier** (freshly
> written candidates). Each tier is a least-recently-used cache: once full it
> evicts its least-recently-used candidate (the hot tier demotes its eviction
> back into the general tier) instead of growing, so the index never exceeds
> its budget regardless of how much unique data the volume holds. Eviction only
> forgoes a future dedupe opportunity — it never affects correctness, since the
> chunk/refcount and reverse-reference trees remain authoritative. The index is
> deliberately **not pre-seeded at mount**: walking the chunk tree would cost a
> read per chunk on a volume of any size — unbounded on a 100 TB one — to fill
> a cache that evicts all but its last few thousand entries anyway. It instead
> **warms from the writes that can use it**, so a duplicate written in an
> earlier mount session may go unfound until the cache warms again; that is a
> missed dedupe opportunity, never a correctness risk. The per-entry footprint
> is deliberately over-estimated when deriving the per-tier entry caps, so the
> byte budgets are a hard ceiling, not an approximation.

---

## 10. Compression

Compression is mandatory and uses the first-party TAIRiX zstd codec.

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

> **Implementation — cluster-aligned compressed extents.** The first-party
> codec is the `lib/compress` crate (`AGENTS.md` §16.4 lists compression as a
> curated shared-library class): a `no_std`, allocation-free LZ77 codec with a
> `"RLZ1"` frame and LZ4-style token sequences — no external zstd/compression
> dependency (§3, `AGENTS.md` §2.12). Compression operates on whole aligned
> **clusters** of 16 logical blocks
> (`drivers/filesystem/arxfs/src/cluster.rs`): a write covering a full
> cluster compresses its plaintext as one frame and, when that frees at least
> one block and a contiguous free run is available, stores it in
> `ceil(frame / capacity) < 16` physical blocks recorded as a single
> **compressed extent** `(phys, logical_len = 16, phys_len, compressed)` in
> the extent tree (on-disk format version 2). Every stored block is sealed
> exactly like a raw record (AEAD, stored-form descriptor, slot hash, physical
> checksum), so `compress → encrypt` holds and the freed blocks are real free
> space — `allocated` reports the stored size. Reading any byte decompresses
> at most one bounded cluster, so random access stays one extent-tree descent
> regardless of file size. A single-block record is always stored **raw** (a
> compressed frame inside one fixed block frees nothing and would burn CPU on
> the hot path for zero benefit); an all-zero cluster becomes holes (§19);
> incompressible, unaligned, or sub-cluster writes fall back to the per-block
> path. A partial overwrite or mid-cluster truncate first **decomposes** the
> cluster back to per-block records (bounded work); a reflink shares a
> compressed cluster whole, refcounted by its first physical block (§9).
> *Dedupe before compression* and *compress only unique records* hold:
> per-block dedupe runs on the per-block path, cluster blocks never enter the
> dedupe index, and a missed cross-form duplicate is an allowed missed
> opportunity (§9). Files smaller than one cluster and small streaming
> appends therefore store raw in v1; the optional background recompression is
> the staged answer if profiling ever justifies it.

---

## 11. TRIM/discard and drive health

mkfs flow:

```text
open target exclusively
read discard and health capabilities
record initial health snapshot when available
issue full-range discard when supported
create encrypted ARXFS structures
flush
store health baseline
```

Mounted trim rules:

- freed ranges enter a pending-discard queue;
- discard is issued only after the range is unreachable from every retained
  root, snapshot, reflink, deduped extent, and recovery root;
- discard is batched, aligned to device granularity, and rate-limited;
- ARXFS must not assume discarded blocks read back as zero;
- devices without discard support are recorded, not failed;
- a read-only handle is refused before anything is touched (§12).

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
- critical single-device health raises warnings and may force read-only mount;
- a read-only handle returns its reading and stores no baseline (§12).

If health data is unavailable, store `HealthUnavailable`; the health subsystem
remains enabled.

---

## 12. Scrub, check, and rescue

```text
arxfs scrub   online verification and repair
arxfs check   offline structural validation, repair, and index rebuild
arxfs rescue  damaged-volume root discovery and file extraction
```

`arxfs scrub` verifies metadata, physical checksums, logical hashes, refcounts,
and shared chunks. It is resumable and safe to interrupt.

`arxfs check` validates and repairs superblocks, root history, tree structure,
inodes, extents, refcounts, reverse refs, directories, ACL/capability metadata,
free-space by rebuild, dedupe index by rebuild, and orphaned inodes.

`arxfs rescue` scans for self-identifying metadata blocks, lists valid roots,
maps physical LBAs to files when possible, and extracts readable files without
requiring a fully mountable filesystem.

### Read-only means read-only: a verifying pass writes nothing

A read-only handle **verifies and reports; it never writes**. That is the state
a volume is held in when its medium must not be touched at all — a re-inserted
volume whose non-mutation could not be proven is mounted read-only *with its
uncommitted write set still held* so that an operator, not the filesystem,
decides what happens to it. A well-meant repair there mutates exactly the
bytes that decision is about.

So on a read-only handle a scrub performs no metadata copy-repair, corrects no
refcount divergence, persists no cursor, clears no progress record, and
publishes no transaction; a health pass stores no baseline; and both a discard
and a `check` are refused before anything is touched (a discard is destructive
and irreversible, so a medium whose state is in doubt never receives one, and
`check`'s first act is a rebuild it has no allocator for). None of that costs a
finding:

- The copy-repair lives in **one** place, so the rule is stated once and cannot
  be honoured at some repair sites and forgotten at another. A mirror the pass
  may not rewrite is reported as **damaged** — the good copy served the read
  and the mirror is still degraded — never as a repair that did not happen, and
  it classifies the volume exactly as a repaired copy would: a copy that went
  bad is the same medium signal either way.
- A refcount divergence is reported and left alone, as it is for any pass
  without an exact claim count.
- A bounded pass that kept no cursor says so distinctly from one that will be
  resumed (`PassVerdict::Stopped` against `Paused`, with its own audit event),
  because repeating the first never reaches past its own budget: a caller that
  could not tell them apart could not tell a volume being progressively
  verified from one being re-verified from the start forever.
- A health pass returns the reading it took. Only the durable baseline is
  skipped, so the next pass measures its delta from the same stored one.

### Derived truth lives in transient on-disk scratch, never in RAM

A pass that must decide something about *every* block or *every* inode — how
many extents claim a physical block, which inodes the directory tree reaches,
how many names each inode has — holds one small value per index. In RAM that is
proportional to the volume, so on the machine §26.7 of `AGENTS.md` requires a
100 TB volume to be served from it cannot exist at all. Each such value
therefore lives in a **transient scratch array**: a flat array of fixed-width
elements in a contiguous run of the volume's own free space, paged through the
same bounded 64-page cache the allocation map uses, released before the pass
returns. A pass's resident cost is a fixed handful of blocks whatever the
volume's size, and a scrub or a check over sixteen times the records measures
the same footprint.

- The array is **scratch, not metadata**: nothing outside the pass that
  allocated it reads it, and a crash simply leaves stale bytes in blocks the
  next mount finds free — free space is derived from the authoritative trees,
  so an interrupted pass leaks nothing. Like the allocation map it is
  single-copy and updated in place, never copy-on-written.
- Every page is nonetheless **sealed** with the ordinary keyed block header,
  and the pass **writes every page before it reads any**, so a page that fails
  to authenticate at its own address under its array's owner is a device fault.
  The array's contents drive corrections to authoritative refcounts, so
  "unauthenticated bytes read as zero" would be a fail-open path.
- Where the run does not fit whole, the pass covers the index space in
  **windows**, one extra metadata walk each, up to a bounded window count. It
  never takes more than an eighth of free space, so a verification pass cannot
  be the reason a write fails for want of space.
- Where no run can be placed at all — a read-only handle, a nearly-full or
  badly fragmented volume — the pass runs the half that needs no array and
  **reports what it did not verify** (`ScrubReport::claims_counted`,
  `CheckReport::structure`). It makes no correction from a partial truth: a
  refcount lowered on a guess frees a block a live extent still maps.

**What the reconcile actually checks.** The write path keeps the
reverse-reference list *complete* — sharing that would exceed the cap declines
to dedupe instead (§9) — so a lawful record satisfies
`refcount == referrers.len()` with every referrer named. Verifying each stored
referrer against the extent it claims to come from is therefore one bounded
lookup per referrer with no accumulated state. Only one question is
irreducibly global: whether a block with no chunk record is claimed by exactly
one extent, because the sole record of a claim is the extent that makes it and
the extent trees are ordered by `(inode, logical block)`. That is what the
claim array counts, at four bits per block — exact over every lawful refcount,
with a distinct saturated state above them — which is what makes "the refcount
says two but three extents claim it", the divergence that frees live data,
detectable rather than merely suspected.

A clean pass leaves every committed structure and the allocation map
byte-identical; the scratch run it borrowed and handed back holds whatever it
wrote.

---

## 13. Permissions and namespace

Each inode stores:

```text
kind (directory / regular file / symbolic link),
owner uid, group gid, POSIX mode bits, ACL, optional capability requirement,
created Time64, modified Time64, changed Time64
```

A **symbolic link**'s mode is the conventional `lrwxrwxrwx`, and it gates
nothing: resolution authorises every directory it traverses and then the
target, exactly as it would for a path the caller typed, so following a link
grants no authority the caller did not already have (§20). A link is stamped
with its creator's ownership like any other node.

ARXFS tracks **no access time (atime)**: updating a stamp on every read
would defeat the copy-on-write model (a pure read would write metadata),
so the format keeps only `created`/`modified`/`changed`. The 12-byte
atime slot in the on-disk inode record is reserved (written zero, ignored
on read) so the surrounding field offsets stay fixed, and the node
reports `accessed = Time64::UNIX_EPOCH` — the honest "no stamp" value.
The three tracked stamps are surfaced through `NodeInfo::times` (read in
the same structural read as kind/size), not a separate timestamp trait.

ARXFS must enforce:

- no ambient root authority;
- capability checks before state access;
- no `/proc`, `/sys`, or legacy top-level TAIRiX layout assumptions;
- TAIRiX-forbidden top-level names rejected on the system volume;
- fail-closed behaviour.

### Directory-entry names

ARXFS directory-entry names match ext4's rules so a name valid on one is
valid on the other:

- a name is **1..=255 bytes** long (the ext4 maximum);
- the only forbidden bytes are the path separator `/` and NUL — every other
  byte is stored verbatim, so arbitrary UTF-8 and high bytes are allowed;
- `.` and `..` are reserved for the VFS and are not creatable as names;
- names are compared **byte-for-byte**, so they are **case-sensitive**
  (`File`, `file`, and `FILE` are three distinct entries); ARXFS performs no
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

`ARXFS::grow` extends a *mounted* volume to fill an enlarged device, online
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
stage allocation-map invalidation when needed
flush authoritative blocks and map invalidation
publish superblock slot (companion, then primary)

explicit fs_sync:
write allocation-map pages
flush slot and map pages
write clean map stamp
```

After power loss, mount selects the highest valid committed root. A partial
transaction is ignored. Full `arxfs check` is not required for ordinary crash
recovery.

**Durability vs. consistency.** Crash *consistency* depends on the mandatory
pre-slot barrier: it makes every block the new root transitively names durable
before either slot copy can publish that root. A barrier failure publishes
nothing. The slot itself may remain volatile when an ordinary operation
returns, so *durability* — the caller's write surviving power loss on demand —
is delivered by `fs_sync`. Its one `Block::flush` makes the slot and allocation
map pages stable through the block driver (virtio-blk `VIRTIO_BLK_T_FLUSH`,
SCSI `SYNCHRONIZE CACHE`) and fails closed if the device cannot confirm them.
The clean map stamp follows; losing only that stamp costs a rebuild.

Discard may never destroy data reachable from retained roots.

---

## 15. Implementation order

The dependency order every stage lands in. §18 carries the same numbering with
each stage's status and owning plan; the finer-grained ordered ledger of the
remaining work — including the two prerequisites no stage names — is
`plans/IMPLEMENT-OUTSTANDING-ARXFS.md`.

1. On-disk headers, superblock ring, transaction roots.
2. COW metadata trees, inode tree, extent tree, free-space rebuild.
3. Metadata authentication/checksums and duplicated critical metadata.
4. Encrypted volume creation, key hierarchy, filename/data encryption.
5. Data records with physical checksum and logical hash.
6. First-party TAIRiX compression codec and its ARXFS integration.
7. Chunk table, refcounts, reverse refs, reflinks, dedupe index.
8. Online scrub.
9. Offline check and rescue.
10. TRIM/discard queues and mkfs-time discard.
11. Device health baselines and health-triggered scrub.
12. Fuzz, proptest, crash-replay, and corruption-injection suites.
13. Sparse files: metadata-only all-zero ranges (§19).
14. Extended file metadata: the attribute store and its ABI (§21).
15. Links, symbolic and hard, and the incompatible-feature word (§20).
16. Extended-metadata preservation: copy/move/archive tooling, named streams,
    per-family foreign-filesystem wiring.
17. Write-back cache, commit batching, and the commit barrier (§22).
18. Autonomous maintenance: the scheduler and runner that drive scrub,
    trim, and health, plus the `arxfs` command app (§24).
19. The §5 constant targets: wider metadata and data records, a filesystem
    block size decoupled from the device's, small-file inline/packed storage.
20. Snapshots: the snapshot tree, lifecycle, diff, and send/receive (§23).
21. FEC and multi-device redundancy.

Do not implement dedupe before COW, checksums, refcounts, and check/rebuild
logic are solid. Do not batch commits (17) before the barrier they amortise
exists, and do not widen the §5 record targets (19) before the write path
coalesces (17) — a wider record over a single-block write path multiplies the
per-record command count rather than reducing it. Do not drive maintenance in
the background (18), publish a snapshot (20), or write a FEC commit witness
(21) before the commit barrier (17): each depends on a published root whose
subtree has reached media.

---

## 16. Required tests

Minimum acceptance tests:

- crash replay at every commit step;
- metadata bit flip detection and repair from duplicate copy;
- data bit flip detection;
- wrong key refuses mount;
- plaintext ARXFS cannot be created;
- first-party zstd round-trip, corpus, malformed-input, and fuzz tests;
- dedupe never merges unequal data;
- dedupe index rebuild works;
- free-space rebuild matches authoritative extents;
- allocation-map invalidation is durable before any page write, and every
  partial page subset after a failed sync rebuilds exactly from either selected
  transaction root;
- TRIM never touches retained-root/snapshot/reflink/dedupe-reachable ranges;
- mkfs issues discard on discard-capable mock devices;
- SMART/NVMe health deltas trigger required scrub/check actions;
- Time64 persists dates before 1970, after 2038, and far-future values;
- fuzz targets for mount, metadata decode, directory decode, symbolic-link
  decode, compression decode, check, and rescue;
- a symbolic link's target round-trips verbatim across a remount, including a
  maximum-length target spanning several blocks;
- a link is never byte-readable, byte-writable, truncatable, or reflinkable,
  and `create` refuses the link kind;
- a link's blocks are accounted as data: the free-space rebuild agrees with the
  live allocator, scrub verifies them through the data pipeline, and removing
  the link returns them;
- a volume declares the symlink feature only once it holds a link, a rolled
  back creation leaves it undeclared, and `check` widens a declared set that
  understates the volume;
- a volume declaring an unsupported feature is refused at mount with that
  reason;
- a metadata copy that cannot be *read* is recovered from its companion, just
  as one that fails to authenticate is;
- an inode may be named by several directory entries: `nlink` counts them, the
  last unlink frees the storage and no earlier one does, a rename that replaces
  a destination decrements the replaced inode, and the `hardlinks` feature is
  declared on first use and rolled back with a failed creation;
- extended attributes round-trip across every namespace and a remount, keys are
  case-sensitive, an unknown namespace and an oversize key/value/count are
  refused, a set that overflows its block fails closed leaving the prior set
  intact, keys and values are encrypted at rest, a read-only mount refuses
  mutation, a set is one transaction under crash replay, removing a file frees
  its attribute block, and a reflink's attributes are independent;
- a sparse file allocates no data payload for a zero range, a non-zero write
  splits the surrounding holes, an all-zero overwrite frees the old data only
  when copy-on-write rules allow, truncate up creates a hole and truncate down
  frees only real extents, a reflink preserves holes without creating dedupe
  chunks, and scrub and check validate a hole's metadata without reading a
  backing block;
- the write path's device-command cost — commands, blocks, superseded rewrites,
  run lengths, and barriers — is asserted against a recorded baseline for a
  single-call write, the same bytes in many calls, a small append, and a
  metadata-only create, at both block sizes, and is identical on a 100 TiB
  volume; a second baseline prices the same workloads batched, where the calls
  of one window join a single transaction, and requires many calls to cost the
  blocks, bytes, and single barrier one call costs, with nothing superseded;
- a transaction spanning operations survives a failure inside one of them: an
  operation refused before it mutates, and one that runs out of space after it
  has allocated, each leave every operation already reported into the batch
  readable byte for byte and the volume sound; an unsynced batch is lost whole
  and leaves the prior published state sound; each barrier-requiring operation
  publishes the batch before it runs; and a commit failure that would lose an
  operation already reported freezes the handle read-only.

Stage 17 (§22) adds, and stage 20 (§23) will add, the remaining tests their
owning plans enumerate.

The implementing agent must run the full TAIRiX CI/test requirements from
`AGENTS.md` before reporting completion.
---

## 17. Definition of done

ARXFS is not complete until:

- every mandatory feature is present and non-tunable;
- ARXFS has no external zstd/compression dependency;
- crypto use is through `lib/crypto`;
- production errors are `Result`-based, not panics;
- ABI changes are versioned and documented;
- docs under `docs/src/filesystem/` are updated;
- `cargo fmt --all`, `cargo xtask ci`, and required fuzz/proptest runs pass for
  the whole TAIRiX workspace;

---

## 18. Staged delivery & status

ARXFS lands **one stage per session**, bottom-up. Each session implements the
next unfinished stage completely — code, rustdoc, the §16 tests for that stage,
and the `docs/src/filesystem/arxfs.md` update — behind the existing versioned
`Filesystem*` traits so the VFS is never broken; runs the whole-project
definition of done (`AGENTS.md` §7) and fixes every failure before reporting;
and records the next stage in the owning `plans/ARXFS-*.md` and the legend
below.

There is exactly one `arxfs` driver and one on-disk version. The copy-on-write
driver in `drivers/filesystem/arxfs/` fully replaced the earlier journaled
implementation — no `v1` folder, no parallel version. Each later stage grows
that single driver and must not regress it.

Status legend: `✓` done · `*` in progress · `!` blocked · (blank) not started.

| Stage | §15 step | Owning plan | Status |
|---|---|---|---|
| 1 | Headers, superblock ring, transaction roots. | — | ✓ |
| 2 | COW trees, inode tree, extent tree, free-space rebuild. | — | ✓ |
| 3 | Metadata authentication and duplicated critical metadata. | — | ✓ |
| 4 | Encrypted volume creation, keys, filename/data encryption. | — | ✓ |
| 5 | Data records with physical checksum and logical hash. | — | ✓ |
| 6 | First-party compression codec and its integration. | — | ✓ |
| 7 | Chunks, refcounts, reverse refs, reflinks, dedupe index. | — | ✓ |
| 8 | Online scrub. | — | ✓ |
| 9 | Offline check and rescue. | — | ✓ |
| 10 | TRIM/discard queues and mkfs-time discard. | — | ✓ |
| 11 | Device health baselines and health-triggered scrub. | — | ✓ |
| 12 | Fuzz, proptest, crash-replay, corruption-injection suites. | — | ✓ |
| 13 | Sparse files. | `plans/SPARSE.md` | ✓ |
| 14 | Extended file metadata. | `plans/ARXFS-METADATA.md` | ✓ |
| 15 | Links and the incompatible-feature word. | — | ✓ |
| 16 | Extended-metadata preservation. | `plans/ARXFS-METADATA.md` §10 | |
| 17 | Write-back cache, batching, commit barrier. | `plans/ARXFS-WRITEBACK.md` | * |
| 18 | Autonomous maintenance and the `arxfs` command app. | `plans/ARXFS-MAINTENANCE.md` | * |
| 19 | The §5 constant targets. | `plans/IMPLEMENT-OUTSTANDING-ARXFS.md` §5 | |
| 20 | Snapshots. | `plans/ARXFS-SNAPSHOT.md` | |
| 21 | FEC and multi-device redundancy. | `plans/ARXFS-FEC.md` | |

**Stages 1–15 are shipped**, so ARXFS is a complete, encrypted, checksummed,
compressing, deduplicating, sparse-aware, link-carrying, attribute-carrying
copy-on-write filesystem with online scrub, offline check, rescue, safe
discard, and health tracking. Stages 16–21 are the named remaining work.

### What the shipped stages guarantee

**Format and transactions (1, 2).** Every metadata block is self-identifying:
its header carries a magic, block type, format version, volume UUID, owner
object, generation, and its own logical and physical address, so a stale,
misdirected, wrong-type, or torn block is rejected at decode rather than
trusted. A four-slot superblock ring of mirrored pairs opens the volume; a
transaction root carries an inline commit record, so a half-written root is
rejected and the ring falls back. One generic B-tree node implementation
(`src/btree.rs`) backs every tree — inode, per-file extent, chunk/refcount,
reverse-reference — so there is no second tree, and one bounded resumable walk
(§4) reads them all, so no operation's resident bytes scale with a tree. A
free-space rebuild walks those trees whenever the on-disk allocation map cannot
be adopted, taking each node from the walk's own path, and a two-cursor
allocator keeps sequential data contiguous.

**Redundancy and authentication (3).** Every metadata block is authenticated
with a keyed HMAC-SHA256 over identity plus payload and stored in **two**
physical copies, the companion at `primary + 1`. One read path serves every
metadata class: read the primary, fall back to the companion when the primary
fails *or cannot be read*, and repair the bad copy from the good one. Both
copies bad fails closed, never a panic.

**Encryption (4).** There is no code path that lays out an unencrypted volume.
`format` provisions a per-volume key hierarchy through `lib/crypto`: a master
key drawn from the injected platform RNG, wrapped under a KDF of the
caller-supplied volume key and stored only in wrapped form, deriving the
metadata-authentication, filename, and content keys. A wrong key never
authenticates the wrapped blob, so the mount is refused with
`PermissionDenied`. File data and directory-entry names are ChaCha20-Poly1305
sealed per block, so a bit-flip is detected rather than mis-decrypted.

**Data integrity (5).** Every data block carries a logical content hash
(SHA-256 of the plaintext) and a fast physical checksum (CRC-32C through
`lib/crc32c`). Write hashes the plaintext, seals, then checksums the at-rest
block; read verifies the checksum first — media corruption is caught before the
AEAD — then authenticates and decrypts, then verifies the logical hash. The
three layers stay distinct (`integrity::DataFault`: `Physical`/`Aead`/`Logical`)
because scrub and health classify against them.

**Compression (6).** Always on, through the first-party `lib/compress` codec —
no external zstd dependency, `no_std`, allocation-free, panic-free. A whole
aligned cluster of compressible plaintext is stored as one frame in *fewer*
physical blocks, so the saving is real free space rather than slack inside a
1:1 block; an incompressible record is stored raw. The logical hash names the
plaintext, before compression, so it remains the dedupe key.

**Sharing (7).** A data record may be shared by several `(file, logical
block)` pairs. Sharing is exact and verified: a candidate is taken only after
its bytes are confirmed byte-identical, so a missed duplicate is acceptable and
unequal data is never merged. An unshared block carries an implicit refcount of
one and no tree record at all; the first share promotes it, the last drop frees
it. Discovery rides an in-memory dedupe index that warms from writes and is
**never** authoritative — every candidate is liveness-checked and byte-verified
— bounded as a cache rather than growing with the volume, and scoped to the
encryption domain. `reflink` shares every block until a side is written.

**Verification and recovery (8, 9).** `scrub` is a resumable, capability-gated,
online verify-and-repair pass: it authenticates both copies of every live
metadata block and repairs from the companion, runs every live data block
through the integrity pipeline and classifies each failure, and recomputes
refcounts and reverse-references from the live trees to reconcile divergence
toward extent-derived truth. It reports and logs; it never silently mutates,
and its progress record makes a resumed pass identical to an uninterrupted one.
`check` is the offline superset: it rebuilds the derived state first (the
allocation map and the dedupe index) so a corrupt derivation can never keep a
sound volume unmountable, reuses scrub's verification core, validates the
directory tree, and reclaims orphaned inodes. `rescue` extracts from a volume
too damaged to mount: read-only on the device, it scans for a self-identifying
transaction root whose commit record validates, picks the highest generation,
and emits only file data that passes the integrity pipeline.

**Discard and health (10, 11).** Freed blocks enter a transient in-memory
pending-discard queue as a transaction reclaims them. `trim` discards a queued
block only if it is *still free* at trim time, so a reallocated or still-shared
block is never discarded; runs are coalesced and aligned inward to the device's
discard granularity, rate-limited per call, and the queue is never persisted,
so a crash mid-trim costs nothing. A device without discard support is
recorded, not failed. `health` reads device telemetry, classifies the volume
against documented thresholds taking the worse of the device and
filesystem-observed signals, triggers a scrub when the unsafe-shutdown or
media-error counters have risen since the persisted baseline, and stores the
new baseline.

**Adversarial suites (12).** Fuzz harnesses cover mount, metadata decode,
directory decode, compression decode, check, and rescue, each holding the one
invariant "returns a `Result`, never panics, fails closed". The crash-replay
sweep cuts every representative transaction at every write budget and asserts
the re-opened volume always mounts on a whole transaction boundary. The
corruption-injection suite wounds each on-disk structure class in one copy and
in both, asserting the documented seam behaviour for each.

**Sparse files (13).** A hole is the *absence* of an extent, so there is no new
on-disk field and nothing extra to checksum, encrypt, compress, dedupe, scrub,
or trim. Detection happens before the logical hash, dedupe, compression, and
allocation, so a zero range never reaches the compressor or the dedupe index.
Full behaviour: §19.

**Extended metadata (14).** Every inode may carry a namespaced attribute set in
one self-identifying `BlockType::Attr` block off its `attr_root`, encrypted and
mirrored exactly like a directory block. The key grammar, bounds, and the
foreign-metadata preset registry live once in `lib/fsmeta`. Full behaviour:
§21; the preservation tooling is stage 16.

**Links (15).** A symbolic link is its own inode kind whose target is stored as
node data, reusing the whole data pipeline; the driver's kind is an enum, not a
directory/not-directory boolean, which is what forced every call site to say
what it means for a link. A hard link is a second directory entry for an
existing inode, with `nlink` counting names and storage freed only at zero.
Each declares its own incompatible-feature bit on first use, so a volume
without one stays readable by a build without the feature, and `check` widens a
word that understates the volume. Full behaviour: §20.


## 19. Sparse files (ZERO/Hole extents)

Sparse-file support is mandatory, always enabled, and not tunable (the
authoritative appendix is `plans/SPARSE.md`). ARXFS stores a logical
all-zero range as metadata only — never a physical data record, a zstd
payload, a dedupe chunk, or an encrypted data blob.

**Representation.** A hole is an *unmapped* logical range. ARXFS represents
holes **implicitly** as gaps between the per-file extent-tree mappings (§4,
§6) — the form `plans/SPARSE.md` §2/§3 permit alongside an explicit ZERO
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

---

## 20. Links

ARXFS carries both kinds of link: a **symbolic** one is its own object kind
(§20.1–§20.4), and a **hard** one is a second directory entry for an inode
that already has a name (§20.5). They are unrelated mechanisms — one stores a
path, the other stores nothing at all — and each declares its own
incompatible feature bit.

Symbolic links are a first-class ARXFS object kind. A link is an inode of
on-disk kind `3` (beside `1` for a directory and `2` for a regular file)
whose **stored target is its node data**. `Inode::decode` rejects any other
kind value, so an undefined kind is corruption rather than something coerced
onto the nearest match.

The kind is an enum in the driver, not a directory/not-directory boolean.
That is deliberate: the code it replaced asked "is this a directory?" and
treated everything else as a regular file, which would have made a link a
readable, writable, reflinkable file at half a dozen call sites the compiler
could not have flagged.

### 20.1 The target is node data

A link's target goes through the ordinary file write path, so it is one
`(inode, logical block)` extent map like any other content and there is no
second storage path to checksum, encrypt, scrub, relocate, or repair. What
that inherits, deliberately:

- **Checksummed and authenticated.** The target passes the §5/§8 data
  pipeline — physical checksum, AEAD, logical hash — so a corrupt target is
  *detected* rather than resolved to some other path. This is the point of
  reusing the data path rather than packing the target into the inode record.
- **Never compressed in practice.** Compression operates on whole aligned
  16-block clusters (§10) and a single-block record is always stored raw. A
  target is at most `FS_SYMLINK_MAX` (4096) bytes, which is under one cluster
  at every supported block size, so a link never reaches the compressor. Path
  resolution reads a link per hop, and that path stays free of codec work by
  construction rather than by a special case.
- **Dedupe-eligible, on purpose.** Two links with the same target share a
  chunk exactly as two files with the same bytes do — byte-verified before
  sharing, refcounted, copied on write (§9). Excluding links would be a
  `dedupe=off` knob for one object kind, which §1 forbids; and many desktop
  shortcuts to one bundle is precisely the case sharing was built for.
- **Sparse-safe by construction.** An all-zero target is unrepresentable: the
  VFS grammar rejects an empty target and rejects NUL inside a component, so
  the §19 hole path is never reached for a link.

A link's blocks are therefore **data**, not a directory's mirrored metadata
pairs. Allocation accounting, freeing, scrub, and the free-space rebuild all
key on "is this node's content mirrored metadata?", which only a directory
answers yes to, so a link's blocks are marked, verified, and returned as the
single-copy data records they are.

### 20.2 A link is not a byte stream

The driver refuses, fail-closed, every operation that would treat a link's
data as file content: `read_at`, `write_at`, `truncate`, and `reflink`. A
reflink is the sharpest case — it clones data blocks into a fresh *regular
file*, so cloning a link would silently produce a file holding the target's
text instead of a second link. `create` refuses the link kind outright,
because it carries no target to store; links are created only by
`create_link`, which does.

The target is read with `read_link` and nowhere else. It comes back exactly as
stored — unresolved, unnormalised, with no terminator — because resolution is
the VFS's decision, made component by component under the caller's attested
identity. A buffer too small for the target is refused, never handed a
truncated path.

`rescue` (§12) **counts and skips** links rather than extracting them: its
sink carries file bytes, so emitting a target through it would recreate the
link on the destination as a regular file holding the target's text. An honest
omission in the report beats a silent change of kind.

### 20.3 Declaring the feature

A volume that holds a link declares `INCOMPAT_SYMLINKS` in the superblock's
incompatible-feature word (§4). A reader that does not know kind `3` would
otherwise read a link inode as structurally invalid — reporting a sound volume
as corrupt, or, in a reader less careful than this one, misreading it. The bit
makes the refusal happen once at mount, with its reason, instead of at some
arbitrary later inode read.

The bit is set by the **first link the volume gets**, in the same transaction
that mints it, and a rolled back creation takes it back with the rest of the
transaction's state. It is deliberately not set at format time: a volume that
has never held a link is not gratuitously made unmountable by a build that
lacks the feature. Bumping the format version instead would have had exactly
that effect on every existing volume, which is why the feature word exists.

`check` (§12) validates the declaration against reality and **widens** a word
that understates the volume. Both the inode tree and the superblock are sealed
under the same key, so a volume can only reach that state through a driver
defect — which is what an offline structural check is for. Widening fails safe
in one direction only: a reader without the feature then refuses the volume
rather than reading a link as a corrupt inode.

### 20.4 What ARXFS does not decide

ARXFS stores the bytes; it never resolves them. The hop bound, the physical
`..` handling, the refusal to escape the volume, the per-component permission
checks, and the `NO_FOLLOW` posture are all VFS policy and live in
`plans/SYMLINKS.md`. The driver's whole contribution is a durable, integrity-
checked place to keep a path.

### 20.5 Hard links: the `nlink` lifecycle

An inode may be named by more than one directory entry. `Inode::nlink` counts
them, and the rule is uniform across kinds because `.` and `..` are real
entries here: **an inode's count is how many directory entries name it**. A
file or link with two names counts two; an empty directory counts two (its
parent's entry and its own `.`) and gains one per child directory (each
child's `..`).

The count is the whole lifecycle. `link` raises it and writes the new entry
in one transaction; `unlink` — and a rename that replaces a destination —
lowers it and frees the inode's blocks and its inode slot **only at zero**.
Freeing because *a* name went would destroy data the remaining names still
reach, which is why the operation is one shared `drop_name` rather than a
free open-coded at each site. A node's own contents do not change when a name
is added or dropped, so only `changed` (ctime) moves, never `modified`.

`u32` names per inode is a bound the format fixes, not a capacity to grow: a
create that would overflow it fails closed with `TooManyLinks` rather than
wrapping a count whose zero would free a live inode.

A **directory** never gets a second name. The VFS refuses one before
delegating, and the driver refuses one too, because it owns the invariant
that its own tree stays a tree — which is what makes the VFS's physical `..`
resolution well-defined.

Because the count is per *inode*, everything that walks the inode tree rather
than the name space is already correct for a multiply-named node and visits
it once: the allocation-map rebuild marks its blocks once (so a remount's
rebuilt free count matches the live one), `scrub` verifies them once, and
`rescue` extracts them once. Only `check`, which walks names, has extra work:
it counts the entries that name each inode and rewrites any stored count that
disagrees — the hard-link analogue of widening an understated feature word,
and the repair for a drift that would otherwise leak storage or free live
data.

A volume holding one declares `INCOMPAT_HARDLINKS` (§4), set by the first
hard link in the transaction that adds it and rolled back with it. The safety
argument is **stronger** than §20.3's: a reader that does not know kind `3`
merely misreads a link inode, but a reader that does not know about a second
name would run an unlink that frees the inode outright and destroy data the
other name still reaches — silent corruption rather than a misread.

---

## 21. Extended file metadata

ARXFS gives every inode a general-purpose, namespaced `key → value`
extended-attribute store, and uses it to preserve foreign-filesystem per-file
metadata (Acorn/RISC OS, Amiga, Atari, classic Mac) that has no POSIX
equivalent and would otherwise be destroyed by a copy. Preserving another
system's metadata is interoperability with foreign data, not TAIRiX
self-compatibility, and is explicitly permitted (`AGENTS.md` §2.13).

The key grammar, the bounded attribute model, the on-disk encoding, and the
foreign-metadata conversions live **once** in the shared `lib/fsmeta` crate, so
ARXFS, the foreign-filesystem drivers, and the copy/move/archive tooling share
one definition (`AGENTS.md` §2.2). ARXFS never interprets a value's meaning;
it stores opaque bytes.

### 21.1 The attribute set

Each inode gains an optional `attr_root`: the physical block of its
extended-attribute set, `0` when it carries none. The set is one
self-identifying, mirrored, copy-on-write metadata block
(`BlockType::Attr`) reached from the inode, held to the same COW, integrity,
redundancy, encryption, and authentication rules as every other ARXFS
metadata block (§4, §5, §7, §8): it is authenticated by the keyed block
header, stored in two physical copies (the §8 companion mirror,
repaired-on-read), and its keys and values are **encrypted at rest** under the
metadata (filename) key before authentication (encrypt-then-MAC), exactly like
a directory block's entry names — so no plaintext attribute leaks on a
raw-device read. Setting an attribute is one atomic transaction; a crash leaves
the prior-or-new set, never a torn one (§14).

An attribute is `(key, flags, value)`. Keys are namespaced,
byte-for-byte case-sensitive (matching directory-name comparison, §13), and
drawn from a **closed, curated** namespace set:

| namespace | meaning | access |
|---|---|---|
| `user` | free-form user metadata | file read/write permission |
| `acorn` | Acorn / RISC OS (ADFS) preset metadata | file read/write permission |
| `amiga` | AmigaDOS preset metadata | file read/write permission |
| `atari` | Atari GEMDOS/TOS preset metadata | file read/write permission |
| `mac` | classic Mac OS / HFS preset metadata | file read/write permission |
| `tairix` | TAIRiX-native extended metadata | file read/write permission |
| `system` | security-sensitive, ACL-adjacent metadata | privileged (VFS capability gate) |
| `trusted` | metadata only privileged services may set | privileged (VFS capability gate) |

The `user`, foreign, and `tairix` namespaces are ordinary file metadata: they
need only the file's own read/write permission (the per-inode owner/mode/ACL
model, `AGENTS.md` §5.3), no new capability. The `system` and `trusted`
namespaces guard a real security boundary; the VFS gates them with a capability
introduced **with** its enforcement point, never ahead of it (`AGENTS.md`
§5.2). An unknown namespace is rejected at set time (fail closed); the set is
evolved in place, never opened up (`AGENTS.md` §2.13).

### 21.2 Fixed bounds (validation, not capacity)

These are fixed *security* validation bounds on untrusted stored data, not
growable capacities (`AGENTS.md` §24.4):

```text
KEY_MAX            255 bytes
VALUE_MAX          3072 bytes (inline attribute value)
ATTRS_PER_INODE    32
TOTAL_ATTR_BYTES   3072 (summed key + value bytes)
```

`VALUE_MAX` is sized so a full attribute set — every key, value, and the
self-identifying framing — serialises into a single metadata block on a
4 KiB-block volume. A value larger than `VALUE_MAX` is **not** an extended
attribute; it is a *named stream* (a fork — e.g. a classic-Mac resource fork),
stored through the file-data pipeline (COW extents, checksummed, compressed,
encrypted, dedupable, sparse-capable) under a `mac`/`tairix`-namespaced key, so
large forks stay out of the inline set and reuse the whole data path with no
second data path (§6–§10, §19). *(The named-stream content path is staged
future work; see §18.)* On a smaller-block volume a set that does not fit one
metadata block fails closed with `NoSpace` rather than spanning blocks.

### 21.3 Operations and the driver ABI

The driver implements the versioned `abi-v1` `FilesystemAttrs` trait
(`lib/abi/src/driver/filesystem.rs`), a separate trait alongside
`FilesystemRead`/`FilesystemWrite`/`FilesystemSecurity`
(new behaviour ships as a new trait, never a widening of a shipped one):

- `get_attr(node, key, value_out) -> Option<len>` — reads a value into a
  caller buffer; a value that does not fit is `BufferTooSmall`, never a
  truncation.
- `set_attr(node, key, value)` — inserts or replaces, one COW transaction.
- `list_attr(node, index, key_out) -> Option<len>` — yields keys in stable
  on-disk order.
- `remove_attr(node, key)` — one COW transaction; a missing key is `NotFound`.

Every operation validates the key against the shared grammar and the bounds and
**fails closed** before touching state; the driver makes no permission
decision (the VFS authorises against the model first, gating the privileged
namespaces with a capability). A reflink copies the source's attributes into
the destination's **own** attribute block, so freeing one inode never frees the
other's; removing an inode frees its attribute block exactly once. Every
timestamp a preset carries is a `Time64`, converted to and from the foreign
format through a **checked** conversion — an instant the foreign format cannot
represent fails closed with a typed error, never silently truncated
(`AGENTS.md` §21).

### 21.4 Cross-filesystem preservation and the preset registry

A foreign-filesystem driver exposes its native per-file metadata as normalised
preset attributes; a copy engine sets those on the destination inode; the
destination stores them natively if it understands them, else keeps them
verbatim as ARXFS extended attributes (a lossless round-trip). An
exact-preservation copy to a target that cannot represent an attribute reports
`MetadataNotRepresentable` and fails closed; a best-effort copy drops it only
under an explicit, documented lossy policy. The `lib/fsmeta` preset registry is
the single source of truth for the value encodings (see the registry reference
page). Indicative v1 entries: `acorn.filetype`/`loadaddr`/`execaddr`/
`datestamp`, `amiga.protection`/`comment`, `atari.attributes`/`gemdos_date`,
`mac.type`/`creator`/`finderflags`/`resourcefork`, `tairix.origin`/`mime`.
*(The `cp`/`mv`/desktop/archive tooling and the per-family driver wiring are
staged future work; see §18.)*

---

## 22. Write-back cache, commit batching, and the commit barrier

*Stage 17 — in progress. The dirty block set, commit barrier, run coalescer,
allocation-map integration, and the commit scheduler are implemented; the
RAM-derived bound is not, and no host yet installs the monotonic clock the
dirty-age window is measured against, so a live volume still publishes at every
operation. The staged design is `plans/ARXFS-WRITEBACK.md`.*

**Commit ordering.** A commit drains every authoritative dirty block *except*
the superblock slot, issues one `Block::flush()` barrier, then writes the slot.
One barrier is sufficient and mandatory: the transaction root is just another
block that must be durable before the slot that publishes it, so a per-step
barrier costs a full cache flush per step and buys nothing. An explicit
`fs_sync` issues one further barrier to make the slot and rebuildable map pages
durable before it returns. A commit that cannot barrier does not publish.

The one addition to that count is the map's clean-to-dirty transition below,
which needs its invalidation durable before any page write: it is paid once per
sync period, by whichever of the commit or the sync first has a map page to
write, and never per write. A pass that publishes nothing — a clean `check` or
`scrub` — writes nothing and so barriers not at all.

This closes the only ordering hole the pre-stage-17 driver had: it wrote the
copy-on-write blocks, the root, and the slot with no barrier at all, so a device
with a volatile write cache could make the slot durable while an interior tree
node beneath its root was not. `open` re-validates the root before accepting a
slot, so a lost *root* was always survivable; a lost interior node beneath a
durable root was not.

**The commit point is one block write.** The slot's two mirror copies are
written companion-first, so the *primary* — the copy a mount reads and prefers
— is the last write of the commit and a half-written pair publishes nothing.
Everything before that write is rolled back on failure. A failure *of* the slot
writes leaves publication genuinely unknown, because the device may have taken
one copy: the handle then reserves the union of both candidate roots — the
deferred frees the commit had already applied to the map are reserved back, its
own blocks stay reserved — and forces itself read-only rather than guessing, so
whichever of the two roots the device holds stays intact for the next mount.

**Two undo scopes, and a rollback costs its own scope, never the volume.** A
transaction spans operations, so a failed operation and a failed commit undo
different amounts.

An **operation** that fails is undone alone, leaving the operations that already
joined the transaction and were reported successful exactly as they were. Every
change it made to the transaction's staged set and private-block bookkeeping is
recorded as it is made — the previous version of each block it staged over or
discarded, the blocks it claimed, the private blocks it released, and the frees
it deferred — and replayed backwards. This is what lets a transaction rewrite the
same metadata block across many operations, which is the whole of the batching
win, without a failure in the last one destroying the first one's work. The
recorded set is the operation's own working set: an operation refused for an
ordinary reason — a name already taken, a name not found — changes nothing, and a
caller who may create a name must not be able to make each refusal walk every
tree on the volume (§26.2, §26.6). Both map marks are no-ops where the change
they undo never reached the map, so the free count is exact either way.

A failed **commit**, and a device fault that leaves the map's image genuinely
ambiguous, abandon the whole transaction back to the last published root. That
loses the work of every operation already reported successful into it, so a
handle that made such a report forces itself read-only rather than serving
writes it can no longer honour; a transaction carrying only the failing
operation's own work leaves the handle writable, exactly as an unbatched commit
failure always did. A map image discarded this way is re-derived from the
committed trees.

**The allocation map's clean stamp is invalidated durably.** The map is adopted
only when it is stamped clean at the generation the mount selected. Its
invalidation is staged in the dirty set's pre-barrier phase, so the ordinary
commit barrier makes it durable before the first map page can land. Removing
the marker is unsafe for an in-place map: a volatile cache could retain a page
carrying committed frees while losing that commit's slot, leaving the old clean
generation to reallocate a block still live in the selected root. If the map's
bounded cache must evict before commit, it first confirms the invalid stamp;
it never trades this ordering for a lower barrier count.

**What is cached.** One physical-block-keyed dirty set holds sealed blocks in
two ordering phases. Authoritative copy-on-write blocks drain before the commit
barrier; rebuildable allocation-map pages drain only after their invalid stamp
is durable, normally at `fs_sync`. Rewriting an address replaces its staged
bytes, reads overlay either phase, and adjacent blocks coalesce into a bounded
multi-block request. Mirroring remains two adjacent entries. Resident map pages
move into the set rather than being copied, and an eviction drains through that
same path. Transient scratch arrays and idempotent repairs of already-committed
mirrors still write directly because neither participates in publication.

**Batching.** A transaction stays open and the next operation joins it, closing
on the first of: an explicit `fs_sync`; the dirty byte ceiling (back-pressure —
the writer waits for I/O, the set never grows past it); the dirty-age window
expiring; an operation that needs a barrier for its own correctness (`trim`,
`grow`, `scrub`, `check`, `health`, or widening the incompatible-feature word);
or the volume being handed on. `close()` does not sync — POSIX semantics, and
the write-then-close workloads are exactly the ones batching exists for. The
offline `rescue` opens its own read-only handle, so it has no transaction to
close.

The **window** is one policy function over the device class the block seam
reports: 30 s removable, 15 s rotational, 5 s solid-state and paravirtual. It
buys device commands with recency — operations inside it fold into one commit,
one transaction root, one superblock slot, one barrier, and one write of each
metadata block they all rewrite — so it is widest where a command is dearest and
smallest where one is already cheap. Nothing tunes it per volume.

Ageing a transaction needs a **monotonic clock**, which only the host can
supply. A handle given one batches; a handle without one has no window to
measure and publishes at every operation, so a host that cannot say how much
time has passed never defers durability.

Measured, on the §16 device-command ledger: the same 64 KiB written in sixteen
4 KiB calls costs, joined, the same blocks and the same bytes as one call — 159
blocks and 7 commands against 159 and 6 on a 512-byte volume, 26 and 7 against
26 and 6 at 4096 — where per operation it cost 64 commands, 335 blocks, and 24
of them superseded. Nothing in a joined transaction is written twice.

**What is not traded.** Consistency. Nothing is published until the slot, so
every crash still leaves the prior committed state or the new one, never a torn
one (§14). What batching trades is how *recent* the surviving state is, bounded
by the deadline. There is no mount option and no knob (§1): the behaviour is
derived from the device, not configured.

**Measured, not asserted.** What a write costs a device is a number, so it is
recorded and machine-checked rather than described: one in-RAM device logs every
command the driver issues it, in order, and the write-amplification and
command-count figures the stages are judged on — including the run-length
histogram the coalescer produces and the barrier count this section makes
mandatory — are asserted against that recording. The cost is identical on a
100 TiB volume, so it is a property of the write path rather than of the device
it was measured on. The figures live in `plans/ARXFS-WRITEBACK.md` §1.

**Bounds.** The set holds one transaction's authoritative write set, or a map
page transiently moved from the bounded page cache; a rollback discards it, and
a read-only handle can never stage a block. The drain adds one gather buffer, reserved
fallibly for the transaction's longest physical run and never past the transfer
window, so a machine too short of memory to hold it writes block by block
instead of failing the commit. The ceiling that forces a commit before it
grows further is derived from discovered RAM, never a hand-picked constant, with
a floor of one transaction's own working set — below that floor a transaction
could not complete, so the floor is a correctness property. A dirty block is
pinned memory, not reclaimable cache: it cannot be dropped, only written, so
pressure shortens the deadline and lowers the ceiling toward the floor rather
than evicting. The bytes are charged to the reclaim ledger as pinned so they
stay visible in the accounting.

---

## 23. Snapshots

*Stage 20 — planned, nothing implemented. The design brief is
`plans/ARXFS-SNAPSHOT.md`, which this section will be generated from.*

A snapshot is a named, read-only, integrity-verified reference to a committed
transaction root, pinning everything reachable from it against reuse and
discard. The primitives it is built from already exist — the superblock ring's
retained root history, reflink/shared immutable chunks, refcounts, and the
reverse-reference tree (§4, §9, §14) — so this is a user-visible feature over
them, not a second copy-on-write mechanism.

The forward references already in this spec resolve here: the §11 TRIM
reachability rule ("unreachable from every retained root, snapshot, reflink,
deduped extent, and recovery root") and the §16 test of the same name both name
the snapshot half of the liveness check, which lands with this stage.

---

## 24. Autonomous maintenance

*Stage 18 — in progress. The design brief is `plans/ARXFS-MAINTENANCE.md`,
which this section will be generated from. Landed so far, from that plan's
staging: the shared background pacer and the cross-layer availability query
(M0), and the read-only rule §12 states (M1).*

`scrub`, `trim`, and `health` (§11, §12) are implemented and capability-gated
but have **no production caller**, so on a live system discard never issues,
verification never runs, and the health baseline never advances past mkfs. This
stage supplies the thing that drives them: a pure, event-timed **maintenance
scheduler** per mounted volume and one **maintenance runner** beside the mount
that performs one bounded chunk per turn and parks on the soonest deadline.

The shape is fixed by four rules. Maintenance is *paced* — it takes a share of
the device derived from the device's own class and yields to the foreground
workload. It is *bounded and resumable* — every action is one chunk with a
persisted resume point, so a 100 TB volume is maintained on a 1 GiB machine.
It is *subordinate across layers* — restoring redundancy beneath the filesystem
outranks verifying above it, so background work stands down while the backing is
degraded, recovering, or unavailable. And it is *event-driven* — a trigger wakes
it and a single one-shot deadline is its fallback; there is no periodic tick and
no polling loop.

`check`, `rescue`, and `grow` are never background actions: the first two are
offline supersets and the third is an operator's instruction. They, and the
health this stage accumulates, are reached through the `arxfs` command app that
lands with it. Damage a mounted volume cannot repair itself sets a sticky
check-requested mark that survives a remount and is reported rather than
silently acted on.
