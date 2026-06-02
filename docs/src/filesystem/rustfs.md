# rustfs driver

`rustfs` (`drivers/filesystem/rustfs`, crate `rustos-drv-fs-rustfs`) is the
**native RustOS filesystem**: a block-backed, copy-on-write filesystem that
stores full POSIX metadata plus an inline access-control list and an
optional capability gate **per inode** (`AGENTS.md` §5.3). There is exactly
one on-disk version — `rustfs` is built up internally in the stages of
its [specification](./rustfs-spec.md), but the driver and its format are a
single shipping thing, not a `v1`/`v2` pair. It
sits behind any `rustos_abi::driver::block::Block` device and is exposed
through the versioned `FilesystemRead` and `FilesystemWrite` traits — never
by widening the frozen mount/unmount `Filesystem` trait (`AGENTS.md` §2.4 /
§9).

The driver **stores** each inode's owner, mode, ACL, and capability gate
but makes **no** permission decision itself: the VFS is the policy point
(`AGENTS.md` §5.4). The stored record is read back through the versioned
`FilesystemSecurity` trait (`security(node) -> NodeSecurity`) and written
through `RustFs::set_security`. Because `rustfs` implements
`FilesystemSecurity`, the kernel host delegates to it through the VFS's
`*_via_secured` operations, which judge each node against its **own**
stored §5.3 record (`Metadata::from_node_security`) rather than a uniform
mount-point template — so an owner-only or capability-gated file is
enforced as stored. See [Driver delegation](./overview.md) and the
[driver-trait reference](../abi/driver_traits.md).

## On-disk layout

A volume is a sequence of fixed-size blocks (the device's logical block
size, between 512 and 4096 bytes, a power of two). The device opens at a
**superblock ring** of four logical slots, each a **mirrored pair** of
adjacent blocks (eight blocks in all); everything else is allocated
copy-on-write from the pool that follows. `RustFs::open` re-derives and
validates the geometry from the selected superblock slot.

| Region          | Contents                                                  |
| --------------- | --------------------------------------------------------- |
| Superblock ring | Blocks 0–7: four slots, each a mirrored pair of blocks,   |
|                 | each pointing at a committed root.                        |
| Pool            | Everything else, allocated copy-on-write: the transaction |
|                 | root, the inode-tree nodes, the per-file extent-tree      |
|                 | nodes, directory blocks, and raw file-data blocks.        |

Every **metadata** block is self-identifying (`AGENTS.md` §8 block
identity): its first 128 bytes carry a magic, block type, format version,
the volume UUID, an owner object, a generation, its logical and physical
address, and a **keyed authenticator** — an HMAC-SHA256 tag computed
through `lib/crypto` (`AGENTS.md` §2.12) over identity + payload. Decoding
verifies all of that against the address the reader *expected*, so a stale,
misdirected, wrong-type, torn, bit-rotted, or wrong-key block is rejected at
decode time and the mount fails closed (`AGENTS.md` §5.4). Raw file-data
blocks carry no header; their tail holds a 28-byte per-block crypto trailer
(a 12-byte nonce and a 16-byte AEAD tag, see [Encryption](#encryption)) and a
40-byte **data-integrity trailer** (a 32-byte logical content hash and an
8-byte physical checksum, see [Data integrity](#data-integrity)), so a data
block holds `block_size - 68` bytes of file content.

Inodes are 256-byte records held in a **copy-on-write inode tree** keyed by
inode number (see the next section); inode 1 is the root directory. Each
inode names the root of its own **extent tree**, which maps a file's logical
block offset to a physical run `(start, length)` — so a file can span the
whole volume and a large contiguous write collapses to a single extent
record. Directories are block-addressed payloads of 64-byte slots (`inode`,
`name_len`, name) reached through the same extent map; the entry names are
encrypted at rest and the block reserves the same 28-byte crypto trailer at
its tail (see [Encryption](#encryption)). `.` and `..` are stored on disk and
hidden from `read_dir`. The inode record also stores the four §21
timestamps. A volume written by a different format version is refused rather
than misread.

> **Stage 5 of the [specification](./rustfs-spec.md).** The volume is a
> complete, mountable copy-on-write filesystem whose metadata scales through
> B-trees, is **authenticated** with a `lib/crypto` keyed MAC stored in **two
> physical copies** repaired from each other, is **encrypted at rest** under a
> real per-volume key hierarchy (see [Encryption](#encryption)), and now carries
> a per-data-record **integrity field** — a logical content hash plus a fast
> physical checksum, verified on every read (see [Data integrity](#data-integrity)).
> RustFS has no plaintext layout. Compression and dedupe are later stages.
> The free-block bitmap is rebuilt in memory at mount by walking the trees
> from the selected root; it is not stored on disk.

## Metadata authentication and redundancy (`rustfs-spec.md` §5, §8)

Each metadata block is sealed with a **keyed authenticator** (HMAC-SHA256
through `lib/crypto`, `AGENTS.md` §2.12 — crypto is the standing "don't roll
your own" exception) covering the block's identity *and* its payload, so the
tag detects not only a flipped payload byte but a stale, misdirected,
wrong-type, torn, or wrong-key block. The metadata-authentication key is the
volume's, derived from the per-volume master key (see
[Encryption](#encryption)); a volume opened with the wrong key never recovers
it and the mount is refused, fail-closed.

Every metadata block is stored in **two physical copies** — a primary and a
companion mirror at the adjacent block (`companion = primary + 1`), so
metadata is allocated in adjacent pairs. One read path serves all metadata
— superblock-ring slots, transaction roots, B-tree nodes, and directory
blocks: it reads the primary, and when the primary fails to authenticate it
falls back to the companion and **repairs** the primary from the good copy
(`rustfs-spec.md` §8 — try redundant copies, repair bad from good). If both
copies fail to authenticate the read fails closed; it never trusts corrupt
bytes and never panics (`AGENTS.md` §5.4 / §2.9). A directory's content
blocks are themselves metadata, so they too are mirrored pairs; a regular
file's data blocks are single-copy and carry no header. Because every
metadata block obeys the one `primary + 1` rule, there is a single
redundancy mechanism rather than one per structure (`AGENTS.md` §2.2).

## Encryption

RustFS is **encrypted by default and has no plaintext mode**
(`rustfs-spec.md` §5, §7): there is no code path that lays out an unencrypted
volume. Every volume is created with a caller-supplied **volume key** (the
installer's, recovery flow's, or storage policy service's key material):
`RustFs::format(block, inode_hint, &volume_key)` provisions the per-volume
key hierarchy and `RustFs::open(block, &volume_key)` recovers it.

The key hierarchy is grown through `lib/crypto` only (`AGENTS.md` §2.12 —
crypto is the standing "don't roll your own" exception):

```text
volume key (caller-supplied)
  -> wrapping key  (KDF)  ── unwraps ──> master key (on disk, AEAD-wrapped)
                                            -> metadata-authentication key (HMAC-SHA256)
                                            -> filename key (AEAD)
                                            -> content  key (AEAD)
```

The master key is **never stored unwrapped**: only its AEAD-sealed form lives
on disk, in the plaintext discovery region of every superblock-ring slot (the
minimal unlock header the spec permits). `open` derives the wrapping key from
the supplied volume key, unseals the master key, and derives the working
keys. A **wrong key** never authenticates the wrapped blob, so the mount is
refused with `PermissionDenied`, fail-closed (`AGENTS.md` §5.4), never a panic
(§2.9).

- **File data** is encrypted per block under the content key with
  ChaCha20-Poly1305 (`lib/crypto/src/aead.rs`); the block's 28-byte trailer
  holds the nonce and tag, so a bit-flip in encrypted data is **detected** by
  the authenticator on read rather than silently mis-decrypted.
- **Directory-entry names** are encrypted under the filename key the same
  way; the directory block is then sealed with the metadata authenticator
  (encrypt-then-MAC), and the read path authenticates then decrypts.
- **Metadata** (superblock, transaction roots, B-tree nodes) stays
  authenticated-only — its confidentiality is not in this stage's scope,
  though a directory block's *names* are encrypted.

The KDF is HMAC-SHA256 used as a single-block HKDF-Expand
(`lib/crypto/src/kdf.rs`), and the AEAD nonce for a data or directory block
is derived from its `(physical address, generation)` and stored in the
trailer, so copy-on-write never reuses a `(key, nonce)` pair. This driver has
no entropy source of its own, so the master key and its salt are derived
deterministically from the volume key and UUID and wrapped on disk; sourcing
the master key from the platform RNG is a later refinement (as the random
per-volume UUID is).

## Data integrity

Every file-data block carries a two-layer **data-integrity field**
(`rustfs-spec.md` §6, §8), stored in a 40-byte trailer that follows the crypto
trailer (`src/integrity.rs`). It complements — and is distinct from — the
Stage-4 AEAD tag: the AEAD proves *authenticity* of the ciphertext, while this
field gives a cheap media-corruption check plus a content-addressable name for
the plaintext.

- **Logical content hash** (32 bytes). The hash of the block's *plaintext*
  content, taken before encryption on write and recomputed after decryption on
  read. Identical content hashes identically — the seam Stage 7 deduplication
  keys on (`rustfs-spec.md` §9) — and a single changed plaintext byte changes
  it, catching a corruption that survived decryption.
- **Physical checksum** (8 bytes). A fast, non-cryptographic checksum over the
  block's at-rest bytes (ciphertext + crypto trailer + compression descriptor +
  logical hash). It is verified **first** on read, so media or transport bit
  rot is caught cheaply before the AEAD runs.

The write path is the spec's: take the logical hash of the plaintext, compress
then encrypt the content (see *Compression* below), then checksum the at-rest
block. The read path reverses it: verify the physical checksum,
decrypt-and-authenticate, decompress, then verify the logical hash.
Each layer fails closed to a `DriverError` (never a panic, `AGENTS.md` §5.4 /
§2.9) and is kept internally distinct (`integrity::DataFault` —
`Physical`/`Aead`/`Logical`) so a media fault is not confused with a tamper or
a plaintext mismatch, the seam Stage 8 scrub and Stage 11 health will record
against.

The logical hash is computed through `lib/crypto`'s audited SHA-256
(`AGENTS.md` §2.12 — never hand-rolled). The specification's fixed-v1 constant
names BLAKE3-256; `lib/crypto` exposes only the audited RustCrypto SHA-256, and
importing a `blake3` crate would widen the trusted computing base with a SIMD
backend that does not build cleanly on the bare-metal kernel targets (the same
freestanding-SIMD problem already pinned around for `chacha20` and
`curve25519-dalek`). SHA-256 is a 256-bit collision-resistant hash that fills
the integrity-and-dedupe role identically, so RustFS v1 uses it; `AGENTS.md`
§2.12 (use the audited `lib/crypto` hash, do not hand-roll or import an unvetted
one) takes precedence over the spec's named primitive. The physical checksum is
a first-party FNV-1a — a checksum is not a cryptographic primitive, so §2.12
does not bar rolling it, and the block's keyed authenticity still rests on the
AEAD and the metadata MAC.

## Compression

Compression is **mandatory and always on** (`rustfs-spec.md` §1, §10): every
file-data record is compressed before it is encrypted. The codec is
**first-party** — the `lib/compress` crate, a `no_std`, allocation-free LZ77
("zstd-fast-style") codec — and RustFS takes **no external zstd/compression
dependency** (`AGENTS.md` §2.12 / §16.4; `rustfs-spec.md` §3). It is a
low-CPU profile (a greedy hash-table match finder, LZ4-style literal/match
tokens, no entropy stage), not a maximum-ratio one.

On the §6 write path the order is `dedupe → compress → encrypt` (see
*Deduplication* below — only **unique** records are compressed). The plaintext
logical hash is taken first (it always names the plaintext and is the dedupe
key), then, if the record is not shared with an existing chunk, it is
compressed; when the compressed frame is
**not smaller** than the logical block capacity the record is stored **raw**
(the §10 adaptive choice — incompressible data is never inflated). On the read
path the order is `physical checksum → decrypt → decompress → verify logical
hash`; a record stored raw skips the decompress step.

Which path a record took is recorded in a per-block **compression descriptor**
(the §8 data-record *compression state* field, `src/integrity.rs`): one state
byte plus the little-endian `u32` length of the at-rest stored representation.
It sits between the crypto trailer and the logical hash, so the fast physical
checksum covers it and a corrupted descriptor is caught before the AEAD runs.
`data_capacity()` reserves it alongside the crypto and integrity trailers.

The whole fixed-size content slot is always encrypted regardless of whether
the record compressed, so the Stage-4 crypto and Stage-5 integrity layers are
**identical** for compressed and raw records — a compressed record simply
stores fewer at-rest bytes inside the same slot, while a logical block still
maps exactly one file block. Decompression is panic-free: a malformed or
truncated compressed frame returns an error (surfaced as the fail-closed
`DriverError::DeviceFault`), never a panic (`rustfs-spec.md` §10, `AGENTS.md`
§2.9).

## Deduplication

Deduplication is **mandatory and exact** (`rustfs-spec.md` §1, §9). A physical
data record — a **chunk** — may be **shared** by more than one `(file, logical
block)`, and it keys on the Stage-5 **logical hash** (the SHA-256 of the
plaintext). Sharing is **exact and verified**: a candidate is taken only after
its stored bytes are confirmed **byte-identical** to the incoming record, so a
missed duplicate is acceptable but unequal data is never merged (§9 — merging
unequal data is corruption).

Two copy-on-write trees back it, both the **same** generic `src/btree.rs`
(`AGENTS.md` §2.2 — no second B-tree), and both named by the transaction root
alongside the inode-tree root:

- **Chunk/refcount tree.** Keyed by a chunk's physical block; the value is the
  referrer count, the encryption domain, the plaintext logical hash, and the
  logical length. It is authoritative for safe freeing.
- **Reverse-reference tree.** Keyed by the same physical block; the value is the
  capped list of `(inode, logical block)` referrers, needed by scrub / check /
  health and by safe discard.

To keep ordinary writes cheap, an **unshared** block carries an *implicit*
reference count of one and has **no** record in either tree. The first time a
block is shared it is promoted to an explicit chunk (refcount 2, both referrers
recorded); further shares bump the count and append a referrer; dropping a
reference decrements it, and dropping the last reference frees the physical
block. A chunk that falls back to a single referrer returns to the implicit
state (its records are removed) and keeps its block. Shared chunks are
**immutable**: overwriting one sharer copies-on-write a fresh record for the
writer and drops the old refcount, leaving every other sharer's data intact.

Discovery uses an in-memory **dedupe index** — `(domain, length, logical hash)
→ candidate` — that is **rebuilt from the chunk and reverse-reference trees at
mount and is never authoritative** (§9). Before sharing, a candidate is
**liveness-checked** (its recorded referrer's extent map must still point at
it) and then **byte-verified**; a candidate that fails either check is a stale
index entry and is dropped, never shared. This is what lets the fast in-memory
index be approximate without ever risking a wrong merge.

A **reflink** (`RustFs::reflink`) is a copy-on-write clone of a file that
shares every data block with its source until a side is written, when only the
written blocks diverge. It is an inherent driver operation, not a widening of a
frozen `Filesystem*` ABI trait (`AGENTS.md` §2.4).

Dedupe is **scoped to the encryption domain** (`rustfs-spec.md` §7): the domain
(derived from the volume's master key) is carried in every chunk record and in
the index key, so dedupe can never cross a domain. With a single volume key
today there is exactly one domain, but the keying already enforces the rule for
when multiple domains arrive.

## Online scrub (`rustfs-spec.md` §12)

`RustFs::scrub` is an **online** verify-and-repair pass: it walks the live
volume while it stays mounted, leaning on the redundancy and integrity seams
the earlier stages already built rather than rebuilding structure offline
(that is the later `check`). It is an inherent driver operation, not a
widening of a frozen `Filesystem*` ABI trait (`AGENTS.md` §2.4), and is
**capability-gated** on `CAP_FS_MOUNT` — without it scrub fails closed with
`PermissionDenied` and logs the refusal (`AGENTS.md` §5.4).

What scrub verifies, and what it repairs versus records:

- **Metadata (verify + repair).** Every live metadata block — the committed
  superblock slot, the transaction root, the inode and per-file extent
  B-trees, and the chunk and reverse-reference trees — is authenticated in
  **both** physical copies. A copy that fails the keyed authenticator is
  **repaired from its good companion** (the same redundancy seam `open` uses),
  and the repair is counted. A block whose **both** copies fail is recorded as
  an unrepairable finding — fail-closed, never a panic (`AGENTS.md` §5.4 /
  §2.9).
- **Data (verify + record).** Every live file-data block is run through the
  integrity read pipeline and any failure is classified by its layer —
  `Physical` (fast checksum), `Aead` (tag), or `Logical` (plaintext hash) — and
  **recorded**. Deep repair / reconstruction of data is a later stage; scrub
  records honestly rather than pretending to fix what it cannot.
- **Refcounts + reverse references (verify + repair).** The chunk refcounts and
  reverse-reference sets are **recomputed from the live inode/extent trees**
  and compared with the on-disk chunk and reverse-reference trees
  (`rustfs-spec.md` §9). A divergence is a finding; scrub corrects it toward the
  extent-derived truth without dropping a referrer (a wrong refcount is reset, a
  bogus referrer struck out, a stale shared record removed). A genuinely shared
  block missing its chunk record is recorded but not fabricated (that
  reconstruction belongs to the offline `check`).

**Resumable + interrupt-safe.** Scrub takes a `ScrubBudget`:
`ScrubBudget::Unlimited` verifies the whole volume in one call, while
`ScrubBudget::Inodes(n)` verifies a bounded number of inodes, then persists a
**scrub-progress record** — a `BlockType::ScrubProgress` block reached from the
transaction root, holding the resume cursor and the accumulated counts — and
returns so the caller can resume later. The accumulated `ScrubReport` of a
completed scrub is identical whether it ran in one call or many. The progress
record is **rebuildable** metadata (`rustfs-spec.md` §4): a crash mid-scrub
leaves a fully mountable volume and ordinary crash recovery never needs scrub
(§14); a corrupt progress record simply restarts the scrub rather than failing
the mount. The cursor is cleared when the pass completes.

**Report, never silent mutation.** Scrub returns a structured `ScrubReport`
(blocks checked, faults per class, repairs made, divergences corrected,
unrepairable findings) and logs its closing outcome through `lib/log` with a
stable event ID in the `rustfs` `12000` range (`AGENTS.md` §5.4 / §19.4). A
clean scrub of a clean volume changes nothing on disk and is idempotent —
metadata copy-repairs are direct block writes, and a transaction is committed
only when scrub actually corrected something or persisted a cursor.

## Offline check and rescue (`rustfs-spec.md` §12)

Scrub is the *online* verifier; `check` and `rescue` are the *offline*
recovery operations it deliberately does not attempt. Both reuse the seams the
earlier stages built rather than re-implementing them (`AGENTS.md` §2.2): the
§8 block identity + companion mirror, the `DataFault` classes, the
chunk/reverse-reference trees, and the free-space / dedupe-index rebuilds.

**`RustFs::check` — offline structural validation, repair, and index
rebuild.** `check` runs on a **mounted handle** (a volume that opens is the
input) and is the **superset** of the online scrub's checks plus structural
rebuild. It is **capability-gated** on `CAP_FS_MOUNT` (fail-closed and logged
otherwise) and:

- **rebuilds the rebuildable derived state first** — the free-space bitmap (§4)
  and the in-memory dedupe index (§9) — from the authoritative trees, so a
  corrupt derivation can **never** keep a sound volume unmountable. This shares
  the one `rebuild_free_space` walk `open` uses;
- **verifies and repairs** metadata copies, classifies data-integrity faults,
  and reconciles refcounts / reverse references against the live extents, by
  reusing the online scrub's verification core (`verify_everything`);
- **validates the directory tree** by walking it from the root: an entry
  pointing at a missing inode is a *dangling* finding (reported, not
  auto-deleted — removing a live name is not a safe automatic repair); and
- **detects and reclaims orphaned inodes** — live inodes the directory tree no
  longer reaches — freeing their data blocks (releasing any shared-chunk
  references) and their inode slot.

`check` returns a structured `CheckReport` (the embedded scrub `verification`,
directories checked, dangling entries, orphans found/reclaimed, whether the
derived state was rebuilt, and the count of findings it could **not** safely
repair) and logs its outcome with a stable `rustfs` `12000`-range event ID. A
clean check changes nothing on disk and is idempotent; it commits only when it
actually corrected or reclaimed something.

**`RustFs::rescue` — damaged-volume root discovery and file extraction.**
`rescue` does **not** require a mountable filesystem. It is an associated
function (it takes the block device, not a mounted handle), capability-gated on
`CAP_FS_MOUNT`, and **read-only** on the damaged volume — the repair-on-read
paths are suppressed for its duration, so it never writes to the device. It:

1. recovers the volume keys from a surviving superblock **discovery header**
   (the wrapped master key, plaintext at rest), so a wounded superblock ring
   does not stop key recovery;
2. **scans** every physical block for a self-identifying §8 transaction root
   whose inline commit record validates (`TxnRoot::decode_any`, which needs no
   externally-supplied generation), and picks the **highest-generation** valid
   root — so it recovers a usable root even when the ring no longer names one;
3. **maps** the inode/extent metadata that root names to files; and
4. **extracts** each file's readable data, running every recovered block
   through the Stage 5/6 integrity pipeline and emitting only blocks that pass
   to a caller-supplied `RescueSink` — a block that fails integrity is skipped
   and counted, **never handed back** (§6).

`rescue` returns a structured `RescueReport` (roots found, the chosen
generation, files mapped, blocks extracted, blocks skipped, unreadable inodes)
and logs its outcome with a stable event ID. Because the driver owns no
destination filesystem, extraction streams recovered plaintext blocks to the
`RescueSink` the caller provides (a recovery host writes them to a safe
volume).

## Timestamps (§21)

Every inode stores four 64-bit-native `Time64` timestamps —
`created`, `modified`, `accessed`, and `changed` — so absolute time is
never a seconds-only scalar and the full pre-1970 / post-2038 range
round-trips without truncation (`AGENTS.md` §21). They are surfaced
through the versioned `FilesystemTimestamps` trait
(`times(node) -> NodeTimes`), a separate `abi-v1` extension alongside
`FilesystemSecurity` — never a widening of `FilesystemRead` /
`FilesystemWrite` (`AGENTS.md` §2.4 / §9).

The driver stamps them from a clock seam installed with
`RustFs::with_clock(clock: fn() -> Time64)`; without it every stamp is
the Unix epoch, so a board with no wall clock yet keeps deterministic,
in-range timestamps rather than panicking or inventing a time
(`AGENTS.md` §2.9). The stamping follows the POSIX model:

- **create** sets all four to the creation instant and bumps the parent
  directory's `modified`/`changed`;
- **write** advances `modified`/`accessed`/`changed`;
- **truncate** advances `modified`/`changed`;
- **set_security** advances only `changed` (a metadata change);
- **remove** bumps the parent directory's `modified`/`changed`.

`created` is set once and never changed. Installing a different clock
never rewrites timestamps already on disk.

## Copy-on-write metadata trees

Both scalable metadata structures are the **same** generic copy-on-write
B-tree (`src/btree.rs`), keyed by `u64` (`AGENTS.md` §2.2 — one
implementation, not two). Each tree node is one self-identifying metadata
block (`BlockType::Btree`); a leaf holds `(key, value)` records in key order
and an internal node holds `(separator, child)` records, where the separator
is the smallest key in the child.

- **Inode tree.** Keyed by inode number, value the 256-byte inode record. It
  supersedes Stage 1's two-level inode map and removes the format-time
  `inode_count` cap — the tree grows as inodes are created. The transaction
  root names the tree's root block and the next inode number to hand out.
- **Extent tree.** One per file, keyed by logical block offset, value a
  `(physical start, run length)` extent. It supersedes the 12-direct +
  single-indirect map; a lookup is a floor query that finds the run covering
  an offset, and a sequential write merges into the adjacent run so the map
  stays compact.

Mutations copy-on-write the touched node to a fresh (or transaction-private)
block and bubble the change up to a new root; nodes split on overflow and
borrow-or-merge on underflow, all `Result`-based and panic-free with no
`unsafe`. Block allocation draws file **data** upward from the low end of the
pool and **metadata** downward from the high end, with a small metadata
reserve so a delete can always copy-on-write itself and commit even on an
otherwise-full volume.

The mount-time **free-space rebuild** walks these trees from the selected
root — every inode-tree node, then each inode's extent-tree nodes and the
physical runs they map — to reconstruct the in-memory free-block bitmap, so
the authoritative free set is always derived from live metadata rather than a
stored bitmap.

## Copy-on-write and the superblock ring

`rustfs` keeps metadata and data consistent across a crash without
`fsck` (`AGENTS.md` §2.5). Every operation is a transaction, and a block
reachable from the last committed transaction root is **never overwritten
in place**:

- **Copy-on-write everywhere.** A modified metadata or data block is
  written to a freshly allocated block; the block that referenced it is
  itself copy-on-written to point at the new location, up to the inode
  map. Blocks superseded by the transaction are *deferred-freed* — marked
  reusable only after the transaction commits — so the previous committed
  tree stays wholly intact until the new one is durable.
- **Commit order (`docs/src/filesystem/rustfs-spec.md` §14).** Write the copy-on-write
  blocks, write the new transaction root carrying its inline commit
  record, then publish the next superblock-ring slot (round-robin)
  pointing at that root. `open` scans the ring and selects the
  highest-generation slot whose root *and* commit record validate. A
  crash before the slot is published leaves the previous committed root
  selected; a crash mid-publish overwrites only the oldest ring slot, so
  the most recent committed root always survives — the mount lands on a
  whole transaction boundary, never a torn one.

## Operations

`FilesystemRead` provides `root`/`node_info`/`lookup`/`read_at`/`read_dir`;
`FilesystemWrite` provides `create`/`write_at`/`truncate`/`remove`/`flush`,
addressing a target as a `(dir, name)` pair. `write_at` extends files
(zero-filling sparse gaps), `truncate` shrinks (freeing the tail and
copy-on-write zeroing the partial last block) or grows, and `remove`
refuses a non-empty directory with `Busy`. A `NodeId` is the inode index;
node identity is stable across a remount. The driver additionally exposes
`RustFs::reflink(dir, src, dst)` — a copy-on-write clone that shares the
source's data chunks until a side is written (see *Deduplication*) — as an
inherent operation, not a widening of a frozen ABI trait (`AGENTS.md` §2.4).

## End-to-end QEMU vertical

`tests/integration/rustfs_virtio_blk_pci_x86_64` exercises the driver
against a **real (emulated) virtio-blk-pci device** under QEMU. It boots
the production kernel pipeline, brings the block device online through
the same shared bring-up the virtio-blk and FAT32 verticals use, then
mounts a planted rustfs volume through `RustFs::open` (with the fixture's
shared volume key), verifies the
planted file reads back its known contents, and creates + writes + reads
back a fresh file before signalling success.

The on-disk image is built by the shared `rustos-test-rustfs-image`
fixture (a 1 MiB, 512-byte-block, 64-inode volume). Unlike the
hand-encoded FAT32 fixture, the rustfs image is authored by the **real
rustfs driver itself** — the fixture formats an in-memory volume through
`RustFs::format` and plants the file through the driver's own write path
— so the fixture and the driver can never disagree about the on-disk
format (`AGENTS.md` §2.2). The host harness (`cargo xtask test --qemu`)
plants exactly that image on the backing disk, and the freestanding guest
tail names the same planted and to-be-written files through the fixture's
constants. The device tail (`rustfs_round_trip`) is generic over the
virtio transport, so a riscv64 MMIO sibling runs identical code.

## Capabilities

Loading requires `CAP_DRV_LOAD` at `register` time. The driver runs in
user space; it does not request `CAP_DRV_KERNEL`. The read/write methods
are reached only through the `DriverHandle` the host minted at load time,
and the VFS only delegates a write to a non-`READ_ONLY` mount.

## Test surface

`cargo test -p rustos-drv-fs-rustfs` formats an in-memory volume and
exercises: the self-identifying block header rejecting a wrong magic,
wrong type, wrong expected address, foreign UUID, a flipped payload byte,
and a wrong authenticator key; a metadata bit-flip being **detected and
repaired** from the companion mirror, a one-copy superblock corruption still
mounting via the mirror, and both copies corrupt failing closed;
`format`/`open` round-trip and rejection of an unformatted device;
create/lookup/listing across nested directories; read/write with
block-boundary straddling; extent-backed large files across a remount;
inode-tree growth and shrink (split, borrow, and merge) across many inodes;
a file with many non-contiguous extents that splits its extent tree; a large
contiguous write collapsing to a single extent; the mount-time free-space
rebuild matching the authoritative live set; `truncate` keeping the surviving
prefix; `remove` reclaiming space so a full volume can allocate again; the
fail-closed extremes
(`Busy`/`LengthOutOfRange`/`NotFound`); the Stage-4 encryption acceptance
tests — a **wrong key refusing the mount** (`PermissionDenied`, never a
panic) while the right key still mounts, a distinctive filename and file
content being **absent from the raw on-disk bytes** (no plaintext at rest),
a filename and file data **round-tripping through encryption across a
remount**, and a **bit-flip in an encrypted data block being detected** on
read; the Stage-5 data-integrity acceptance tests — a data block's three
integrity layers (physical checksum, AEAD, logical hash) each detecting
**its own** class of corruption and all failing closed, identical plaintext
sharing **one logical hash** while different plaintext differs (the dedupe
seam) — identical content now also sharing one physical chunk (refcount 2)
while distinct content does not — and the
integrity field surviving a remount and a copy-on-write rewrite; the Stage-6
compression acceptance tests — an **incompressible record stored raw** and
reading back byte-identical, a **compressible file shrinking its at-rest
footprint** yet reading back byte-identical across a remount and a COW
rewrite, and the integrity layers still catching a physical and a logical
corruption on a **compressed** block; the Stage-7 dedupe acceptance tests —
two files with identical content **sharing one physical chunk** (refcount 2)
while distinct content does not, **byte-verify-before-share** refusing an
injected colliding index entry, overwriting one sharer **copying-on-write** a
fresh chunk and leaving the other intact, a **reflink** sharing chunks until a
side is written, **refcount-to-zero freeing** the chunk with the mount-time
free-space rebuild agreeing, the **dedupe index rebuilding** from the chunk
tree at mount and yielding the same sharing, dedupe staying **within the
encryption domain**, and integrity + compression still holding on a **shared**
chunk across a remount and a COW rewrite; the Stage-8 online-scrub acceptance
tests — a **clean scrub** of a populated volume reporting zero faults and
changing nothing (idempotent), scrub **detecting and repairing** a single-copy
metadata corruption from its companion and reporting the repair, scrub
**detecting and classifying** an injected data-block `Physical` and `Logical`
fault without panicking, scrub **detecting and correcting** an injected
refcount and a reverse-reference divergence against the on-disk chunk trees,
scrub being **resumable** (a budgeted one-inode-per-call pass reaching the same
result as one uninterrupted pass and clearing its cursor on completion) with a
**crash mid-scrub still mounting** and resuming, a **shared chunk accounted
once** with the dedupe domain preserved, the scrub being **capability-gated**
on `CAP_FS_MOUNT` (refused and logged otherwise), and integrity + compression +
dedupe invariants still holding across a scrub, a remount, and a COW rewrite;
the Stage-9 offline check/rescue acceptance tests — a **clean check** reporting
a sound structure and rebuilding nothing (idempotent, changing nothing on
disk), check **rebuilding** a deliberately corrupted free-space bitmap and
dedupe-index derivation from the authoritative trees with the volume staying
mountable, check **reclaiming an orphaned inode** and **correcting a refcount
divergence** while **reporting** an unrepairable data fault it cannot safely
fix, check being **capability-gated**, `rescue` **discovering a valid root and
extracting** files from a volume whose superblock ring is wounded (and being
read-only/repeatable), and `rescue` **never emitting a block that fails** the
Stage 5/6 integrity pipeline while still recovering the good blocks; the
per-inode security record and
the four §21 `Time64` timestamps (incl. pre-1970 and far-future)
round-tripping across a remount; superblock-ring selection of the
highest committed generation; and a **crash-replay sweep** that faults the
device after every possible write count during a single committing
transaction and asserts the re-opened volume always mounts, the
pre-existing file is always intact, and the in-flight write is either
fully applied or fully absent — never torn.

The mount / metadata-decode path additionally has a `cargo xtask fuzz`
harness (`fuzz_mount`, `AGENTS.md` §19.6): a per-byte flip sweep over a
valid image (which also drives the authenticate-then-fall-back-to-mirror
path), a duplicated-copy sweep that corrupts *both* copies of each block
pair, and a fixed-seed PRNG all drive `RustFs::open` over arbitrary bytes,
asserting it never panics and fails closed. Since Stage 7 the fuzz image is
populated with duplicate-content files and a reflink, so the sweep also drives
the **chunk/refcount** and **reverse-reference** record decode paths that
mount rebuilds the dedupe index from. Since Stage 8 the base image is left with
a **paused scrub**, and each successful mount additionally runs a bounded
`scrub`, so the sweep also drives the **scrub-progress** record decode path
(`load_scrub_progress`), asserting it too never panics and fails closed. Since
Stage 9 each successful mount additionally runs the offline `check`, and every
image (mountable or not) is also fed to `RustFs::rescue`, so the sweep drives
the **transaction-root scan** (`TxnRoot::decode_any`) and the rescue extraction
pipeline over arbitrary bytes, asserting both never panic and fail closed. The
first-party compression codec
has its own `cargo xtask fuzz` harness (`fuzz_compress`, in `lib/compress`):
the spec's required "compression decode" target (`rustfs-spec.md` §10,
`AGENTS.md` §19.6), it round-trips structured inputs and feeds corrupted
frames and pure noise to `rustos_compress::decompress`, asserting it never
panics and fails closed.

The `pjdfstest`-equivalent POSIX suite remains tracked in
`.junie/next-session-prompt.md`.
