# `rustos-drv-fs-rustfs` — native RustOS filesystem driver

`rustfs` is the **native RustOS filesystem**: a block-backed, copy-on-write
filesystem that stores full POSIX metadata plus an inline access-control
list and an optional capability gate **per inode** (`AGENTS.md` §5.3). It
sits behind any `rustos_abi::driver::block::Block` device and is exposed
through the versioned `rustos_abi::driver::filesystem::FilesystemRead`,
`FilesystemWrite`, `FilesystemSecurity`, and `FilesystemTimestamps` traits.

There is exactly **one** on-disk version. `rustfs` is built up internally
in the stages of `docs/src/filesystem/rustfs-spec.md`, but the driver and its format are a
single shipping thing — not a `v1`/`v2` pair. This crate is the only
implementation.

The frozen `Filesystem` trait carries only `mount`/`unmount` and a
`DriverHandle` — it cannot perform I/O — so each I/O surface is a **new
versioned trait** rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9).

## On-disk format

Fixed-size blocks (the device logical block size, 512–4096 bytes, a power
of two). The device opens at a **superblock ring** of four logical slots,
each a **mirrored pair** of adjacent blocks (eight blocks in all) at the
start of the device; everything else — the transaction root, the
copy-on-write inode-tree nodes, the per-file extent-tree nodes, directory
blocks, and raw file-data blocks — is allocated copy-on-write from the pool
that follows.

Every **metadata** block is self-identifying (`AGENTS.md` §8 block
identity): its first 128 bytes carry a magic, block type, format version,
the volume UUID, an owner object, a generation, its logical and physical
address, and a **keyed authenticator** (HMAC-SHA256 through `lib/crypto`,
`AGENTS.md` §2.12) over identity + payload. Decoding verifies all of that
against the address the reader expected, so a stale, misdirected,
wrong-type, torn, bit-rotted, or wrong-key block is rejected at decode time
and the mount fails closed (`AGENTS.md` §5.4). Raw file-data blocks carry no
header; their tail holds a 28-byte per-block crypto trailer (nonce + AEAD
tag, see *Encryption* below), a 5-byte compression descriptor (see
*Compression* below), and a 40-byte data-integrity trailer (logical content
hash + physical checksum, see *Data integrity* below), so a data block holds
`block_size - 73` bytes of file content.

## Metadata authentication and redundancy (`rustfs-spec.md` §5, §8)

Every metadata block is stored in **two physical copies**: a primary and a
companion mirror at the adjacent block. One read path serves all metadata —
superblock-ring slots, transaction roots, B-tree nodes, and directory blocks
— reading the primary, falling back to the companion when the primary fails
the keyed authenticator, and **repairing** the bad copy from the good one
(`rustfs-spec.md` §8 — try redundant copies, repair bad from good). If
*both* copies fail to authenticate the read fails closed; it never trusts
corrupt bytes and never panics (`AGENTS.md` §5.4 / §2.9). The companion is
always `primary + 1`, so metadata is allocated in adjacent pairs and one
redundancy mechanism covers every metadata block (`AGENTS.md` §2.2). The
metadata-authentication key is the volume's, derived from the per-volume
master key (see *Encryption* below); a volume opened with the wrong key never
recovers it.

## Encryption (`rustfs-spec.md` §5, §7)

RustFS is **encrypted by default and has no plaintext mode**: there is no
code path that lays out an unencrypted volume.
`RustFs::format(block, inode_hint, &volume_key, &mut entropy)` provisions a
per-volume key hierarchy through `lib/crypto` (`AGENTS.md` §2.12) — a master
key wrapped (AEAD) under a KDF of the caller-supplied volume key and stored
only in wrapped form in every superblock slot's plaintext discovery region,
deriving the metadata-authentication (HMAC), filename (AEAD), and content
(AEAD) keys. The `entropy` argument is the `EntropySource` seam onto the
platform RNG (`lib/rng`'s `CsRng`, `AGENTS.md` §1/§4), injected at the
composition root so the driver never reaches for a global RNG.
`RustFs::open(block, &volume_key)` unwraps the master key; a wrong key never
authenticates the wrapped blob, so the mount is refused with
`PermissionDenied`, fail-closed (`AGENTS.md` §5.4), never a panic (§2.9).
File data and directory-entry names are encrypted at rest with
ChaCha20-Poly1305 (`lib/crypto/src/aead.rs`): each data and directory block
carries a 28-byte nonce+tag trailer, so a bit-flip in encrypted data or a
name is detected on read rather than mis-decrypted (directory blocks are
encrypt-then-MAC; the read path authenticates then decrypts). The master key,
wrapping salt, and wrap nonce are drawn from the injected platform RNG, so the
master key is **independent of the volume key** (and re-wrappable on a key
change) rather than derived from it; only the wrapping key stays a
deterministic KDF of the volume key and the random salt so `open` can
recompute it. The per-volume UUID is likewise random. A failed entropy draw
fails closed (`AGENTS.md` §5.4).

## Data integrity (`rustfs-spec.md` §6, §8)

Every file-data block carries a two-layer **data-integrity field**
(`src/integrity.rs`), distinct from the AEAD tag: a 32-byte **logical content
hash** of the block's plaintext (taken before encryption, recomputed after
decryption) and an 8-byte **physical checksum** over the at-rest bytes
(verified first, before the AEAD). The write path hashes the plaintext,
compresses, encrypts, then checksums; the read path verifies the checksum,
decrypts, decompresses, then verifies the hash. Each layer fails closed to a
`DriverError` and is kept
internally distinct (`integrity::DataFault` — `Physical`/`Aead`/`Logical`),
never a panic (`AGENTS.md` §5.4 / §2.9). Identical plaintext shares one logical
hash — the seam Stage 7 dedupe keys on (`rustfs-spec.md` §9). The hash uses
`lib/crypto`'s audited SHA-256 (`AGENTS.md` §2.12 — never hand-rolled; the
spec names BLAKE3-256 but `lib/crypto` ships only the audited RustCrypto
SHA-256 and a `blake3` crate does not build cleanly on the bare-metal targets,
so §2.12 takes precedence); the physical checksum is a first-party FNV-1a (a
checksum is not a crypto primitive).

Inodes are 256-byte records (inode 1 = root, the four §21 `Time64`
timestamps inline) held in a copy-on-write **inode tree** keyed by inode
number, so metadata scales past any fixed inode count. Each inode names the
root of its own copy-on-write **extent tree** mapping a logical block offset
to a physical run `(start, length)`, so a file can span the whole volume and
a contiguous write stays a single record. Both are the one generic B-tree in
`src/btree.rs` (`AGENTS.md` §2.2). Directories are block-addressed payloads of
64-byte slots reached through the extent map; `.`/`..` are stored on disk and
hidden from `read_dir`.

## Compression (`rustfs-spec.md` §6, §10)

Compression is **mandatory and always on**, with a **first-party** codec — the
`lib/compress` crate, a `no_std`, allocation-free LZ77 ("zstd-fast-style")
codec — and **no external zstd/compression dependency** (`AGENTS.md` §2.12 /
§16.4). On write the order is `compress → encrypt`; a record whose compressed
frame is not smaller than the logical block capacity is stored **raw** (the
§10 adaptive choice). On read the order is `physical checksum → decrypt →
decompress → verify logical hash`. A per-block **compression descriptor** (a
state byte plus the at-rest stored length) records which path the record took;
it sits between the crypto trailer and the logical hash so the physical
checksum covers it. The full content slot is always encrypted, so the crypto
and integrity layers are identical for compressed and raw records and the
logical hash still names the plaintext. Decompression is panic-free: a
malformed frame fails closed to `DeviceFault`, never a panic (`AGENTS.md`
§2.9).

## Deduplication (`rustfs-spec.md` §9, §6)

Deduplication is **mandatory and exact** and keys on the Stage-5 logical hash.
A physical data record (a **chunk**) may be **shared** by more than one
`(file, logical block)`, but only after its bytes are confirmed
**byte-identical** to the incoming record — a missed duplicate is acceptable,
merging unequal data is corruption (§9). Two copy-on-write trees, both the one
generic `src/btree.rs` (`AGENTS.md` §2.2) and both named by the transaction
root, back it: a **chunk/refcount tree** (physical block → refcount, domain,
logical hash, length) and a **reverse-reference tree** (physical block →
`(inode, logical block)` referrers). An unshared block carries an *implicit*
refcount of one and has no record in either tree; the first share promotes it
to an explicit chunk (refcount 2) and the last drop frees it. Shared chunks
are immutable — overwriting one sharer copies-on-write a fresh record and
leaves the others intact. A **reflink** (`RustFs::reflink`) clones a file by
sharing its chunks until a side is written. Discovery uses an in-memory
**dedupe index** rebuilt from the trees at mount and **never authoritative**:
every candidate is liveness-checked and byte-verified before sharing. Dedupe
is **scoped to the encryption domain** (§7) — the domain is carried in every
chunk record and index key. The write pipeline is `dedupe → compress →
encrypt`, so only unique records are compressed (§10).

## Sparse files (`rustfs-spec.md` §19)

Sparse-file support is **always on and not tunable**: a logical all-zero range
costs metadata only — never a physical data record, a zstd payload, a dedupe
chunk, or an encrypted data blob — so a 10 MiB all-zero file reports a 10 MiB
logical size while mapping **zero** data blocks. A **hole** is an unmapped
logical range, represented *implicitly* as the gap between a file's extent-tree
mappings (the form `.junie/SPARSE.md` §2/§3 permit), so it adds no on-disk field
and is simply the absence of an extent (`AGENTS.md` §2.2). The write path
detects zeros **first** (`store_block`/`is_all_zero`): an all-zero logical record
is caught before the logical hash, dedupe, compression, encryption, or
allocation, drops the block's mapping (making it a hole), and releases any prior
physical block through the normal COW/refcount/free path — a block still held by
a reflink, deduped owner, or recovery root stays live. A zero range is never
deduped or compressed; repeated *non-zero* data follows the normal zstd/RAW path
(no RLE/FILL mode). Reads of a hole synthesise zeroes with no disk I/O,
extending a file leaves a hole, shrinking frees only the real data extents, and
scrub/check/rescue iterate only mapped runs so a hole is never read.

## Online scrub (`rustfs-spec.md` §12)

`RustFs::scrub` is an **online**, resumable verify-and-repair pass over the
mounted volume, **capability-gated** on `CAP_FS_MOUNT` (refused fail-closed and
logged otherwise). It authenticates **both** physical copies of every live
metadata block (superblock slot, transaction root, the inode/extent B-trees,
and the chunk/reverse-reference trees), **repairing** a bad copy from its good
companion and recording a both-copies-bad block as unrepairable; runs every
live file-data block through the integrity pipeline and **classifies** any
fault (`Physical`/`Aead`/`Logical`) without panicking (deep data repair is a
later stage); and **recomputes** the chunk refcounts and reverse-reference sets
from the live extents, **correcting** a divergence toward that truth without
dropping a referrer. A `ScrubBudget::Inodes(n)` call is resumable: it persists
a rebuildable **scrub-progress record** (reached from the transaction root)
holding the cursor and accumulated counts and resumes to the same
`ScrubReport`; a crash mid-scrub still mounts (ordinary recovery never needs
scrub). Scrub returns a structured `ScrubReport` and logs its outcome through
`lib/log` with a stable event ID; a clean scrub changes nothing and is
idempotent.

## Offline check and rescue (`rustfs-spec.md` §12)

Scrub is the online verifier; `check` and `rescue` are the offline recovery
operations it does not attempt, reusing the same seams rather than duplicating
them (`AGENTS.md` §2.2).

`RustFs::check` is the **offline superset** of scrub, run on a mounted handle
and **capability-gated** on `CAP_FS_MOUNT`. It rebuilds the rebuildable derived
state first — the free-space bitmap (§4) and the dedupe index (§9) — from the
authoritative trees (the same `rebuild_free_space` walk `open` uses), so a
corrupt derivation can never keep a sound volume unmountable; reuses the scrub
verification core to verify/repair metadata copies, classify data faults, and
reconcile refcounts; validates the directory tree (an entry to a missing inode
is a *dangling* finding, reported not auto-deleted); and detects and
**reclaims orphaned inodes**. It returns a structured `CheckReport`, is
idempotent, and commits only when it actually repaired something.

`RustFs::rescue` recovers files from a volume too damaged to mount. It is an
associated function (it takes the block device), **read-only** on the device
(the repair-on-read writes are suppressed), and capability-gated. It recovers
the keys from a surviving superblock discovery header, **scans** every block
for a self-identifying transaction root whose commit record validates
(`TxnRoot::decode_any`), picks the highest-generation root, maps its
inode/extent metadata to files, and **extracts** the readable file data —
running every block through the Stage 5/6 integrity pipeline and emitting only
blocks that pass to a caller-supplied `RescueSink` (a failing block is skipped,
never handed back). It returns a structured `RescueReport`.

## TRIM / discard (`rustfs-spec.md` §11, §15.10)

`rustfs` returns freed space to the device **safely**: discard may never destroy
data reachable from any retained root, snapshot, reflink, deduped extent, or
recovery root (§11), and there is no `nodiscard` / `trim=off` mode. The `Block`
ABI gains a versioned discard surface — `discard_capability()` (support,
granularity, per-request cap) and `discard(lba, blocks)` — and a device without
discard support is *recorded, not failed*. Freed blocks enter a transient,
in-memory **pending-discard queue** as a committed transaction reclaims them
(`finish_txn`), reusing the deferred-free machinery rather than a second
free-tracking mechanism (`AGENTS.md` §2.2). `RustFs::trim`, **capability-gated**
on `CAP_FS_MOUNT`, discards a queued block only if it is **still free** at trim
time (a reallocated or still-shared block — refcount ≥ 1 — is marked used by the
free-space rebuild and is skipped, never discarded), coalesces still-free blocks
into contiguous runs aligned **inward** to the device granularity, and
rate-limits to `TRIM_BATCH_RANGES` runs per call (the remainder stays queued).
It never assumes a discarded block reads back as zero. The queue is rebuildable
transient state (§4): a crash mid-trim drops it, the volume remounts cleanly,
and no live data is lost. `trim` returns a structured `TrimReport` and logs its
outcome with a stable event ID. `RustFs::format` issues a full-range discard on
a discard-capable device before laying down the encrypted structures.

## Device health and health-triggered scrub (`rustfs-spec.md` §11, §15.11)

`rustfs` tracks the volume's health to decide *when* a scrub is worth running,
reusing the earlier stages' seams (`AGENTS.md` §2.2). The `Block` ABI gains a
versioned `device_health() -> DeviceHealth` surface (`Available(HealthSnapshot)`
of SMART/NVMe-style counters, or `Unavailable` — *recorded, not failed*, default
`Unavailable`). A self-identifying `BlockType::HealthBaseline` block reached from
the transaction root (like the Stage-8 scrub-progress record) **persists** the
last clean device snapshot plus the volume's accumulated filesystem-observed
fault counters — metadata copy-repairs/unrepairable (Stage-3 seam) and per-class
data faults (Stage-5 seam); both are persisted because a repaired transient
fault leaves no trace in the live trees (§4). `format` stores the initial
baseline, and a crash mid-update leaves the previous committed baseline selected
(§14).

`RustFs::health`, **capability-gated** on `CAP_FS_MOUNT`, reads the current
telemetry, classifies the volume against the documented
`HealthThresholds::DEFAULT` (`Healthy` / `Degraded` / `Failing`, the worse of the
device and filesystem signals — no magic numbers, `AGENTS.md` §2.1), and — when
the device's unsafe-shutdown or media-error counters have risen since the
baseline — **triggers a scrub** through the Stage-8 `scrub` machinery (never a
parallel verifier, §2.2), folding its findings into the counters. It stores the
current telemetry as the new baseline and returns a structured `HealthReport`,
logging its outcome with stable event IDs in the `rustfs` `12000..13000` range.

## Crash consistency (copy-on-write + superblock ring)

Every operation is a transaction. A block reachable from the last
committed transaction root is **never overwritten in place**: modified
metadata and data are written copy-on-write to freshly allocated blocks,
and superseded blocks are deferred-freed (reusable only after the
transaction commits). The commit order (`docs/src/filesystem/rustfs-spec.md` §14) is: write
the copy-on-write blocks, write the new transaction root carrying its
inline commit record, then publish the next superblock-ring slot pointing
at it. `RustFs::open` scans the ring and selects the highest-generation
slot whose root and commit record validate — so a crash leaves the mount
on a whole transaction boundary, never a torn one.

The free-block bitmap is rebuilt in memory at mount by walking the trees
from the selected root — every inode-tree node, then each inode's extent-tree
nodes and the runs they map — so the authoritative free set is always derived
from live metadata (the crate uses `alloc` for these). Block allocation draws
data upward and metadata downward from the pool with a small metadata reserve,
so a delete can copy-on-write itself even on a full volume. No
`unwrap`/`expect`/`panic!` and no `unsafe`.

> **Staged build.** A volume is a complete, mountable copy-on-write
> filesystem with B-tree metadata, a `lib/crypto` keyed-MAC authenticator in
> two physical copies, **at-rest encryption** under a per-volume key
> hierarchy, and a per-data-record **integrity field** (logical content hash
> + physical checksum) verified on every read (Stage 5), and **first-party
> compression** of every data record before encryption with a raw-store
> fallback (Stage 6), and **deduplication** with a chunk/refcount tree, a
> reverse-reference tree, reflinks, and a rebuildable byte-verified dedupe
> index (Stage 7), and a resumable, capability-gated **online scrub** that
> verifies and repairs metadata, classifies data faults, and reconciles
> refcounts (Stage 8), and **offline `check` and `rescue`** — `check` the
> mounted-handle structural validator that rebuilds the derived state,
> reconciles refcounts, validates directories, and reclaims orphaned inodes,
> and `rescue` the read-only damaged-volume root scanner and integrity-gated
> file extractor (Stage 9), and **safe TRIM/discard** — a block-device discard
> capability, a rebuildable pending-discard queue that discards only
> still-unreachable ranges (batched, granularity-aligned, rate-limited), and
> mkfs-time full-range discard (Stage 10), and **device-health baselines and
> health-triggered scrub** — a block-device health surface, a persisted
> self-identifying baseline of the last clean device snapshot plus accumulated
> filesystem-observed fault counters, a structured `HealthReport` classified
> against documented thresholds, and a scrub triggered through the Stage-8
> machinery when a device-health delta crosses a threshold (Stage 11), and the
> **fuzz / crash-replay / corruption-injection suites** that harden every
> earlier stage (Stage 12). **RustFS v1 is complete** — see the
> [specification](../../../docs/src/filesystem/rustfs-spec.md).

## Security

`rustfs` **stores** each inode's owner, mode, ACL, and capability gate. It
reports the record through `FilesystemSecurity` (`security(node)`) and
accepts an updated one through `RustFs::set_security`, but makes **no**
permission decision itself: the VFS is the policy point (`AGENTS.md` §5.4).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_FS_MOUNT` to run `RustFs::scrub` (online verify-and-repair),
  `RustFs::check` (offline structural validation/rebuild), `RustFs::rescue`
  (damaged-volume extraction), `RustFs::trim` (TRIM/discard), and
  `RustFs::health` (device-health pass + health-triggered scrub); without it
  each fails closed with `PermissionDenied`.
- The read/write methods are reached only through the `DriverHandle` the
  host minted at load time, and the VFS only delegates a write to a
  non-`READ_ONLY` mount. The driver runs in user space; it does not
  request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p rustos-drv-fs-rustfs` runs the block-header decode-rejection
tests (wrong magic / type / address / UUID / flipped checksum) and host
tests over an in-memory device: format/open (and unformatted-device
rejection), nested create/lookup/listing, read/write across block
boundaries, extent-backed large files across a remount, inode-tree
split/borrow/merge across many inodes, a many-extent file that splits its
extent tree, a contiguous write collapsing to one extent, the free-space
rebuild matching the authoritative live set, `truncate` prefix survival,
`remove` reclaiming space after `NoSpace`, the fail-closed
extremes (`Busy`/`LengthOutOfRange`/`NotFound`), the per-inode security
record and four §21 timestamps (incl. pre-1970 / far-future) round-tripping
across a remount, superblock-ring generation selection, the Stage-4
encryption acceptance tests (wrong key refuses the mount while the right key
mounts, a filename and content are absent from the raw on-disk bytes,
filename + data round-trip through encryption across a remount, and a
bit-flip in an encrypted data block is detected), the Stage-5 data-integrity
tests (each of the three layers — physical checksum, AEAD, logical hash —
detecting its own corruption class and failing closed, identical plaintext
sharing one logical hash while different plaintext differs, and integrity
surviving a remount and a copy-on-write rewrite), the Stage-6 compression
tests (an incompressible record stored raw and round-tripping, a compressible
file shrinking its at-rest footprint yet reading back byte-identical across a
remount and a COW rewrite, and integrity still catching a physical and a
logical corruption on a compressed block), the Stage-7 dedupe tests (identical
content sharing one physical chunk at refcount 2 while distinct content does
not, byte-verify-before-share refusing an injected colliding index entry,
overwriting one sharer copying-on-write while the other stays intact, a
reflink sharing until written, refcount-to-zero freeing the chunk with the
free-space rebuild agreeing, the dedupe index rebuilding at mount, dedupe
staying within the encryption domain, and integrity + compression holding on a
shared chunk), the Stage-8 online-scrub tests (a clean/idempotent scrub
changing nothing, single-copy metadata repair from the companion, data
`Physical`/`Logical` fault classification, refcount and reverse-reference
divergence detection and correction, resumability matching an uninterrupted
pass plus a crash-mid-scrub remount, a shared chunk accounted once within its
encryption domain, the `CAP_FS_MOUNT` gate, and integrity + compression +
dedupe invariants surviving a scrub/remount/COW rewrite), the Stage-9 offline
check/rescue tests (a clean check sound and rebuilding nothing/idempotent,
check rebuilding a corrupt free-space and dedupe-index derivation with the
volume staying mountable, check reclaiming an orphan and correcting a refcount
divergence while reporting an unrepairable data fault, the check + rescue
capability gates, rescue discovering a root and extracting files from a wounded
superblock ring read-only/repeatably, and rescue never emitting a block that
fails integrity), the Stage-10 TRIM/discard tests (the `CAP_FS_MOUNT` gate, an
unsupported device draining the queue recorded-not-failed, contiguous free
blocks coalescing into one granularity-aligned range, inward alignment
requeuing the unaligned edges, per-request-cap splitting, batch rate-limiting
that drains over passes, a reallocated and a still-dedupe-shared block never
being discarded, the transient queue dropping across a crash with no live data
lost, and mkfs full-range discard recorded-not-failed without support), the
Stage-11 device-health tests (the `CAP_FS_MOUNT` gate, a no-telemetry device
still classifying and persisting a baseline that survives a remount, the
classification crossing healthy → degraded → failing as the device media-error
count climbs, an unsafe-shutdown delta triggering a Stage-8 scrub with the
advanced baseline triggering no further scrub, and the persisted baseline
surviving a crash at every write count during its update with no live data
lost), and a
**crash-replay sweep** that faults the device after every write count
during a committing transaction and asserts the re-opened volume always
mounts with the in-flight write either fully applied or fully absent —
never torn.

The Stage-12 suites are the adversarial superset of all the above, reusing the
same seams (`AGENTS.md` §2.2): the crash-replay sweep is **generalised to every
commit step across every representative transaction** (create, write, truncate,
remove, reflink, scrub, check, trim, health) — each faulted at every
write-budget cut-off, the re-opened volume always mounting on a whole
transaction boundary with the effect fully present or fully absent and the
witness file never lost; and a **corruption-injection suite** that wounds each
on-disk structure class (superblock slot, transaction root, the inode / extent
/ chunk / reverse-reference B-trees, a directory block, the scrub-progress and
health-baseline records, and each data-integrity layer) in **one** and in
**both** copies, asserting a single bad copy is always repaired from the
companion mirror, both copies of mount-critical metadata never tear (fail
closed or recover an earlier consistent root via the ring), a both-copies-bad
directory still mounts but reads fail closed and scrub records it unrepairable,
the transient records recover gracefully, and an unmirrored data block's fault
is classified by its `DataFault` layer and surfaced fail-closed, never silently
repaired.

The **sparse-file tests** (`rustfs-spec.md` §19, `.junie/SPARSE.md` §17) cover
all ten mandatory cases: a 10 MiB all-zero file with a 10 MiB logical size and
zero mapped data blocks (also the encrypted-volume no-plaintext case, surviving
a remount), a non-zero write splitting an extent map around a hole (ordered,
non-overlapping), overwriting data with zeroes turning the block into a hole
while a reflink keeps the old data, `truncate` up making a hole and down freeing
only the real data, a reflink preserving holes with no zero-range chunk, scrub +
check validating sparse metadata with no physical read for a hole, and an
all-zero record bypassing compression while a non-zero constant still
compresses.

The 1 GiB filesystem soak (`cargo xtask fssoak --target rustfs`) drives the
shared cross-filesystem exerciser, and `cargo xtask fuzz` harnesses fuzz the
mount / metadata-decode path (`fuzz_mount`, which since Stage 7 also decodes
the chunk/refcount and reverse-reference records via the dedupe-index rebuild,
and since Stage 8 also drives the scrub-progress record decode by running a
bounded scrub on every successful mount, and since Stage 9 also runs the
offline `check` on every successful mount and feeds every image to
`RustFs::rescue`, driving the transaction-root scan and extraction decode
paths, and since Stage 11 reports SMART-style telemetry and runs `health` on
every successful mount, driving the health-baseline record decode path, and
since Stage 12 also walking every reachable directory on every successful
mount, driving the encrypted directory-block decode path)
and the first-party compression decoder (`fuzz_compress`, in `lib/compress`)
(`AGENTS.md` §19.6).

## End-to-end QEMU vertical

`tests/integration/rustfs_virtio_blk_pci_x86_64` mounts a planted rustfs
volume over a real (emulated) virtio-blk-pci device under QEMU and
round-trips a read and a write (`cargo xtask test --qemu`). The backing
image comes from the `tests/integration/rustfs_image` fixture, which the
real rustfs driver itself authors, so the fixture and the driver share one
source of truth for the on-disk format (`AGENTS.md` §2.2).

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `RustFs`
type is exported so the driver host can construct an instance with
`RustFs::format` / `RustFs::open`; the host reaches into it through the
filesystem traits and the `set_security` accessor.
