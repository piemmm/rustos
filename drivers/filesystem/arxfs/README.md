# `tairix-drv-fs-arxfs` — native TAIRiX filesystem driver

`arxfs` is the **native TAIRiX filesystem**: a block-backed, copy-on-write
filesystem that stores full POSIX metadata plus an inline access-control
list and an optional capability gate **per inode** (`AGENTS.md` §5.3). It
sits behind any `tairix_abi::driver::block::Block` device and is exposed
through the versioned `tairix_abi::driver::filesystem::FilesystemRead`,
`FilesystemWrite`, `FilesystemSecurity`, and `FilesystemTimestamps` traits.

There is exactly **one** on-disk version. `arxfs` is built up internally
in the stages of `docs/src/filesystem/arxfs-spec.md`, but the driver and its format are a
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
copy-on-write inode-tree nodes, the per-file extent-tree nodes, the
pending-delete set, directory blocks, and raw file-data blocks — is allocated
copy-on-write from the pool that follows.

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
*Compression* below), and a 36-byte data-integrity trailer (a 32-byte logical
content hash and a 4-byte physical checksum, see *Data integrity* below), so a
data block holds `block_size - 69` bytes of file content — 443 on a 512-byte
device, 4027 on a 4096-byte one.

## Metadata authentication and redundancy (`arxfs-spec.md` §5, §8)

Every metadata block is stored in **two physical copies**: a primary and a
companion mirror at the adjacent block. One read path serves all metadata —
superblock-ring slots, transaction roots, B-tree nodes, and directory blocks
— reading the primary, falling back to the companion when the primary fails
the keyed authenticator, and **repairing** the bad copy from the good one
(`arxfs-spec.md` §8 — try redundant copies, repair bad from good). If
*both* copies fail to authenticate the read fails closed; it never trusts
corrupt bytes and never panics (`AGENTS.md` §5.4 / §2.9). The companion is
always `primary + 1`, so metadata is allocated in adjacent pairs and one
redundancy mechanism covers every metadata block (`AGENTS.md` §2.2). The
metadata-authentication key is the volume's, derived from the per-volume
master key (see *Encryption* below); a volume opened with the wrong key never
recovers it.

## Encryption (`arxfs-spec.md` §5, §7)

ARXFS is **encrypted by default and has no plaintext mode**: there is no
code path that lays out an unencrypted volume.
`ARXFS::format(block, inode_hint, &volume_key, &mut entropy)` provisions a
per-volume key hierarchy through `lib/crypto` (`AGENTS.md` §2.12) — a master
key wrapped (AEAD) under a KDF of the caller-supplied volume key and stored
only in wrapped form in every superblock slot's plaintext discovery region,
deriving the metadata-authentication (HMAC), filename (AEAD), and content
(AEAD) keys. The `entropy` argument is the `EntropySource` seam onto the
platform RNG (`lib/rng`'s `CsRng`, `AGENTS.md` §1/§4), injected at the
composition root so the driver never reaches for a global RNG.
`ARXFS::open(block, &volume_key)` unwraps the master key; a wrong key never
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

## Data integrity (`arxfs-spec.md` §6, §8)

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
hash — the seam Stage 7 dedupe keys on (`arxfs-spec.md` §9). The hash uses
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
fixed-width **263-byte slots** (an 8-byte header — 4-byte inode number, 4-byte
name length — plus a maximum-length 255-byte name) reached through the extent
map; `.`/`..` are stored on disk and hidden from `read_dir`.

Every tree is read through one bounded, resumable walk (`TreeWalk`): a step
descends one root-to-leaf path and yields that leaf's records into the walk's
own block-sized buffer, so a stat, truncate, delete, scrub step, or mount-time
free-space rebuild holds a node's worth of bytes whatever the tree's size, and
allocates nothing per record. The position is a key, so a caller may mutate the
tree between steps and a long pass may stop, persist it, and resume with the
sequence an uninterrupted walk would have given. Callers needing every *node*
(the free-space rebuild, freeing a whole tree) take them from the walk's path
as it moves (`NodeTrail`) rather than from a collected list. A tree whose shape
is impossible — a level that does not decrease, an entry count wider than its
block, keys that do not ascend in a leaf — is a fail-closed device fault, never
a read past a buffer or an endless descent.

Mutations are bounded the same way. `btree_insert` and `btree_remove` descend
once recording the path, edit the leaf in place, and walk back up rewriting each
ancestor, working in the node buffers of one scratch (`TreeEdit`) the mount
lends them: the node being rewritten, plus the adjacent pair a split, borrow, or
merge moves entries between. Nothing recurses, so the stack a mutation needs is
a few hundred bytes whatever the tree's depth — measured, against the 32 KiB
thread stack the kernel hosts the driver on — and nothing is decoded per record,
so an edit allocates a bounded handful. Each level re-entered on the way up is
validated as the descent validates it, so the write path refuses an impossible
tree rather than running off the stack.

## Serving reads: one device request per contiguous run

An extent maps a **contiguous** physical run, so a read spanning one asks the
device **once for the whole run** instead of once per block, over a 64 KiB run
window (`read_block_run`, `RunStage`) — the round-trips a reading task parks
across scale with the runs it spans, not the blocks inside them. Reading a
1 MiB file costs 35 device requests against 783 block-at-a-time, and the
extent tree is descended once per run. The checks are unchanged and still
per block: each staged block passes its own physical checksum, AEAD, and
content-slot hash keyed by its own address (`verify_data_block`), so a
misdirected or rotted block *inside* a run fails the read closed. The staging
is one bounded allocation per read, wiped on drop, and falls back to a single
block when memory is too tight to reserve it (`AGENTS.md` §4, §26.3). A
compressed cluster's stored run is fetched in one request too.

## Names (`arxfs-spec.md` §13)

Directory-entry names match ext4's rules so a name valid on one is valid on the
other: **1..=255 bytes**, with `/` and NUL the only forbidden bytes (every
other byte, including arbitrary UTF-8, is stored verbatim) and `.`/`..`
reserved. Names are compared **byte-for-byte**, so they are **case-sensitive**
(`File`, `file`, and `FILE` are three distinct entries); ARXFS does no
case-folding or normalisation. A directory grows by whole copy-on-write blocks
as entries are added (a 512-byte block holds one slot, a 4096-byte block holds
fourteen), so `.`/`..` span as many blocks as the block size requires.

## Online resize (`arxfs-spec.md` §13)

A volume's committed block count is pinned in the superblock and may be smaller
than its backing device (e.g. after an admin enlarges the partition, logical
volume, or virtual disk); a volume mounts at its committed size and leaves any
surplus device tail unused. `ARXFS::grow` extends a *mounted* volume to fill an
enlarged device, online and in place: it re-reads the device geometry, folds the
new (free) tail blocks into the free pool, and commits a new superblock
recording the larger size in one atomic transaction — no data moves, the space
is usable immediately without a remount, and a crash before the commit point
leaves the previous committed size selected on the next mount. A device that has
shrunk below the committed size is rejected (online shrink is not offered).

## Compression (`arxfs-spec.md` §6, §10)

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

## Deduplication (`arxfs-spec.md` §9, §6)

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
leaves the others intact. A **reflink** (`ARXFS::reflink`) clones a file by
sharing its chunks until a side is written. Discovery uses an in-memory
**dedupe index** rebuilt from the trees at mount and **never authoritative**:
every candidate is liveness-checked and byte-verified before sharing. Dedupe
is **scoped to the encryption domain** (§7) — the domain is carried in every
chunk record and index key. The write pipeline is `dedupe → compress →
encrypt`, so only unique records are compressed (§10).

## Sparse files (`arxfs-spec.md` §19)

Sparse-file support is **always on and not tunable**: a logical all-zero range
costs metadata only — never a physical data record, a zstd payload, a dedupe
chunk, or an encrypted data blob — so a 10 MiB all-zero file reports a 10 MiB
logical size while mapping **zero** data blocks. A **hole** is an unmapped
logical range, represented *implicitly* as the gap between a file's extent-tree
mappings (the form `plans/SPARSE.md` §2/§3 permit), so it adds no on-disk field
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

## Symbolic links (`arxfs-spec.md` §20)

A link is an inode of on-disk kind `3` (beside `1` directory, `2` regular file)
whose **stored target is its node data**, so it reuses the whole existing
pipeline — extents, AEAD, logical hash, physical checksum, dedupe — with no
second storage path (`AGENTS.md` §2.2). Three consequences are deliberate
rather than inherited: the compressor is never reached (a target is at most
`FS_SYMLINK_MAX` = 4096 bytes, under one 16-block cluster at every supported
block size, and a single-block record is always stored raw); dedupe *does*
apply, since excluding one object kind would be a `dedupe=off` knob the
mandatory profile forbids; and a link's blocks are **data**, so allocation
accounting, freeing, scrub, and the free-space rebuild treat them as the
single-copy records they are rather than as a directory's mirrored pairs.

`Inode::kind` is an `InodeKind` enum, not a directory/not-directory boolean, so
every site must say what it means for a link: `read_at`, `write_at`,
`truncate`, and `reflink` refuse one fail-closed (a reflink most sharply — it
clones data blocks into a fresh *regular file*, which would silently turn a
link into a file holding the target's text), `create` refuses the kind, and
`rescue` counts and skips a link rather than emitting its target through a
byte-oriented sink. `create_link` and `read_link` are the only ways in and out;
`read_link` returns the target verbatim and refuses an undersized buffer rather
than truncating a path.

A volume that holds a link declares `INCOMPAT_SYMLINKS` in the superblock's
plaintext **incompatible-feature word**, so a reader that does not know the
kind refuses the volume at mount with that reason instead of reading a link
inode as corrupt. The bit is set by the *first* link, in that transaction (and
rolled back with it), so a link-free volume stays readable by a build without
the feature — which a format-version bump could not have preserved. `check`
widens a word that understates the volume.

## Online scrub (`arxfs-spec.md` §12)

`ARXFS::scrub` is an **online**, resumable verify-and-repair pass over the
mounted volume, **capability-gated** on `CAP_FS_MOUNT` (refused fail-closed and
logged otherwise). It authenticates **both** physical copies of every live
metadata block (superblock slot, transaction root, the inode/extent B-trees,
and the chunk/reverse-reference trees), **repairing** a bad copy from its good
companion and recording a both-copies-bad block as unrepairable; runs every
live file-data block through the integrity pipeline and **classifies** any
fault (`Physical`/`Aead`/`Logical`) without panicking (deep data repair is a
later stage); and **recomputes** the chunk refcounts and reverse-reference sets
from the live extents, **correcting** a divergence toward that truth without
dropping a referrer. The recompute holds nothing proportional to the volume:
the referrer list is complete by construction, so each stored referrer is
verified with one bounded lookup, and the one irreducibly global question —
whether a block with no chunk record is claimed by exactly one extent — is
answered by streaming every claim through a **transient on-disk claim array**
(`src/scratch.rs`) at four bits per block, released before the pass returns.
Where the volume can spare no run for it, `ScrubReport::claims_counted` says
so and no correction is made from a partial truth. A `ScrubBudget::Inodes(n)`
call is resumable: it persists
a rebuildable **scrub-progress record** (reached from the transaction root)
holding the cursor and accumulated counts and resumes to the same
`ScrubReport`; a crash mid-scrub still mounts (ordinary recovery never needs
scrub). Scrub returns a structured `ScrubReport` and logs its outcome through
`lib/log` with a stable event ID; a clean scrub changes nothing and is
idempotent.

**A read-only handle verifies and reports without writing anything**
(`arxfs-spec.md` §12): no copy-repair, no refcount correction, no cursor, no
cleared progress record, no transaction — and no trim, which is refused
outright. That is the state a volume is held in when its medium must not be
touched, so a well-meant repair there is itself the damage. The rule lives in
**one** place (`ARXFS::repair_meta_copy`, the sole mirror copy-repair site, so
it cannot be honoured at three sites and forgotten at a fourth), and it costs no
finding: a mirror the pass may not rewrite is counted as
`ScrubReport::metadata_damaged` rather than a repair that did not happen, and
`ScrubReport::pass` distinguishes a bounded pass that kept its cursor
(`PassVerdict::Paused`) from one that kept none (`Stopped`, with its own audit
event) — repeating the latter never reaches past its own budget.

## Offline check and rescue (`arxfs-spec.md` §12)

Scrub is the online verifier; `check` and `rescue` are the offline recovery
operations it does not attempt, reusing the same seams rather than duplicating
them (`AGENTS.md` §2.2).

`ARXFS::check` is the **offline superset** of scrub, run on a mounted handle
and **capability-gated** on `CAP_FS_MOUNT`. It rebuilds the rebuildable derived
state first — the free-space bitmap (§4) and the dedupe index (§9) — from the
authoritative trees (the same `rebuild_free_space` walk `open` uses), so a
corrupt derivation can never keep a sound volume unmountable; reuses the scrub
verification core to verify/repair metadata copies, classify data faults, and
reconcile refcounts; validates the directory tree (an entry to a missing inode
is a *dangling* finding, reported not auto-deleted); detects and **reclaims
orphaned inodes**; and reconciles every inode's stored name count. The last
three derive one value per inode — reachable, still owed an expansion, how
many names — so each lives in a transient on-disk scratch array over the inode
space, and `CheckReport::structure` reports `NotWalked` rather than a soundness
nothing established when no run can be placed. It returns a structured
`CheckReport`, is idempotent, and commits only when it actually repaired
something.

`ARXFS::rescue` recovers files from a volume too damaged to mount. It is an
associated function (it takes the block device), **read-only** on the device
(the repair-on-read writes are suppressed), and capability-gated. It recovers
the keys from a surviving superblock discovery header, **scans** every block
for a self-identifying transaction root whose commit record validates
(`TxnRoot::decode_any`), picks the highest-generation root, maps its
inode/extent metadata to files, and **extracts** the readable file data —
running every block through the Stage 5/6 integrity pipeline and emitting only
blocks that pass to a caller-supplied `RescueSink` (a failing block is skipped,
never handed back). It returns a structured `RescueReport`.

## TRIM / discard (`arxfs-spec.md` §11, §15.10)

`arxfs` returns freed space to the device **safely**: discard may never destroy
data reachable from any retained root, snapshot, reflink, deduped extent, or
recovery root (§11), and there is no `nodiscard` / `trim=off` mode. The `Block`
ABI gains a versioned discard surface — `discard_capability()` (support,
granularity, per-request cap) and `discard(lba, blocks)` — and a device without
discard support is *recorded, not failed*. Freed runs enter a transient,
in-memory **pending-discard queue** as a committed transaction reclaims them
(`finish_txn`), reusing the deferred-free machinery rather than a second
free-tracking mechanism (`AGENTS.md` §2.2); the queue holds coalesced runs, so
one large free is one entry. `ARXFS::trim`, **capability-gated** on
`CAP_FS_MOUNT`, splits each queued run against the live map and discards only
the parts **still free** at trim time (a reallocated or still-shared block —
refcount ≥ 1 — is marked used by the free-space rebuild and is skipped, never
discarded), aligns each part **inward** to the device granularity, and
rate-limits to `TRIM_BATCH_RANGES` runs per call (the remainder stays queued).
It never assumes a discarded block reads back as zero. The queue is rebuildable
transient state (§4): a crash mid-trim drops it, the volume remounts cleanly,
and no live data is lost. `trim` returns a structured `TrimReport` and logs its
outcome with a stable event ID. `ARXFS::format` issues a full-range discard on
a discard-capable device before laying down the encrypted structures.

## Device health and health-triggered scrub (`arxfs-spec.md` §11, §15.11)

`arxfs` tracks the volume's health to decide *when* a scrub is worth running,
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

`ARXFS::health`, **capability-gated** on `CAP_FS_MOUNT`, reads the current
telemetry, classifies the volume against the documented
`HealthThresholds::DEFAULT` (`Healthy` / `Degraded` / `Failing`, the worse of the
device and filesystem signals — no magic numbers, `AGENTS.md` §2.1), and — when
the device's unsafe-shutdown or media-error counters have risen since the
baseline — **triggers a scrub** through the Stage-8 `scrub` machinery (never a
parallel verifier, §2.2), folding its findings into the counters. It stores the
current telemetry as the new baseline and returns a structured `HealthReport`,
logging its outcome with stable event IDs in the `arxfs` `12000..13000` range.
A read-only handle stores no baseline and returns its reading anyway; a mirror
the triggered scrub found damaged but could not rewrite classifies the volume
exactly as a repaired one would, because the copy went bad either way.

## Crash consistency (copy-on-write + superblock ring)

Every operation is a transaction. A block reachable from the last
committed transaction root is **never overwritten in place**: modified
metadata and data are written copy-on-write to freshly allocated blocks,
and superseded blocks are deferred-freed (reusable only after the
transaction commits) as coalesced `(start, length)` **runs**, so releasing a
file costs one entry per extent it maps rather than one per block
(`arxfs-spec.md` §4). A file's extent count is itself unbounded, so **freeing
spans transactions**: an unlink stops on an extent boundary once the
transaction has reached the write-back ceiling and publishes before taking
another step, and the **pending-delete set** the transaction root names — a
tree of the inode numbers whose last name has gone and whose blocks are not all
freed — is what makes that resumable. The name's removal and the set entry are
published together, so a crash mid-delete leaves an inode the set names and the
next writable mount finishes it before serving; an ordinary delete is still one
transaction, because the operation that detaches the name takes the first step
itself. A `truncate` needs no entry, freeing downward and publishing the shorter
size at each boundary, so an interrupted one is a shorter file and never one of
its original length with holes where its data was (`arxfs-spec.md` §14). The
commit order (`docs/src/filesystem/arxfs-spec.md` §14,
§22) is: stage the copy-on-write blocks and the new transaction root carrying its
inline commit record, drain them all to the device, issue one `Block::flush()`
barrier, then publish the next superblock-ring slot pointing at that root.
`ARXFS::open` scans the ring and selects the highest-generation slot whose root
and commit record validate — so a crash leaves the mount on a whole transaction
boundary, never a torn one.

The barrier is what makes that order real on a device with a volatile write
cache: when a commit returns, the only authoritative blocks the device may
still be holding are the slot's two copies, so it cannot make the slot durable
while a tree node beneath its root is not. Those copies go out companion-first, so the *primary*
— the copy a mount prefers — is the last write of the commit and a half-written
pair publishes nothing; anything failing before it rolls the transaction back,
while a failure *of* those writes leaves publication unknown and the handle
reserves both candidate roots and forces itself read-only rather than freeing a
root the device may have published. An explicit `fs_sync` drains rebuildable map
pages and issues one further barrier before returning.

A transaction **spans operations**: it stays open and the next operation joins
it, so a burst of small writes costs one transaction root, one ring slot, one
barrier, and one write of each metadata block they all rewrite — the same
64 KiB in sixteen calls puts exactly the blocks and bytes on the device that one
call does. It closes on an explicit `fs_sync`, on the dirty-age window expiring,
on an operation that needs the committed state to be the whole truth (`trim`,
`grow`, `scrub`, `check`, `health`, or one that widens the incompatible-feature
word), or on the volume being handed on. The window is one policy over the
device class the block seam reports — 30 s removable, 15 s rotational, 5 s
solid-state and paravirtual, widest where a command is dearest.

Between operations nothing in the driver runs, so the **host** enforces that
window: the driver names each transaction's deadline through the
`FilesystemWrite::set_writeback_host` seam as the transaction opens and names
its absence as it closes, and the kernel's write-back flusher parks until the
soonest deadline any mounted volume published, then calls the ordinary
`fs_sync` on each volume that is due. Nothing polls, and a machine with no
dirty volume arms nothing. The clock and the timer arrive together
(`ARXFS::with_writeback_host`), so a handle can never defer against a window
nothing will fire; a handle given neither publishes at every operation rather
than deferring durability it can neither measure nor have published for it.

A **failed operation** is undone alone: everything it changed in the staged set
and the private-block bookkeeping is recorded as it changed and replayed
backwards, so the operations that already joined the transaction — and were
reported successful — stand. It costs the operation's own size and never the
volume's, which is what keeps an *ordinary* refusal cheap: a create over a name
already taken, or a remove of one that is not there, changes nothing and must
not be able to provoke a walk of every tree on the volume, however full it is.

A **failed commit** is the wider undo: it abandons the whole transaction back to
the last published root, and a handle that had reported operations into it
forces itself read-only rather than serving writes it can no longer honour. A
device fault that leaves the map's image genuinely ambiguous does the same and
demands the rebuild; nothing else does.

The blocks and allocation-map pages use the one dirty layer beneath the
driver's device-write seam (`wcache`): a physical-block-keyed set of sealed
blocks with pre- and post-barrier phases that replaces on
rewrite, so the repeated copy-on-writes of one B-tree node cost one device write
rather than one each — 746 device writes down to 158 for a 64 KiB write on a
512-byte volume. It is read-through, so a read-after-write inside the
transaction sees the staged bytes and it drops a block the transaction frees
again unwritten. A resident map page moves into this set before I/O instead of
remaining as a second page-sized copy; transient verification scratch and an
idempotent mirror repair still write directly because neither participates in
publication.

The drain then hands the device **runs rather than blocks**. Data blocks are
allocated consecutively and mirrored metadata blocks are adjacent pairs, so the
set's ascending order gathers into contiguous runs and each run is one
`write_blocks`, bounded by the same 64 KiB transfer window the read path gathers
to (`RUN_BYTES`, one definition for both directions). Those same 158 blocks cost
**five** commands, and an empty-file create after a clean mkfs costs four
commands for fifteen blocks: the payload bytes are unchanged, but the commands
carrying them — and the completion wait each costs a per-command device like an
SD card — collapse. A
run stops at the first address the set does not hold, so it can never name a
block outside the transaction or reach past the end of the device, and the
gather buffer is one fallible reservation sized to the transaction's longest
run, wiped on drop, with a machine too short of memory to hold it writing block
by block rather than failing the commit.

A staged block is **pinned** memory, not reclaimable cache: it exists nowhere
else, so it can only be written out, never dropped. The set is therefore not
admitted through the reclaim classification gate — that gate's contract is
droppability — and nothing may shrink it behind the driver's back. What bounds
it is a byte ceiling derived from the RAM the host discovered
(`ARXFS::with_writeback_bound`). Reaching the ceiling publishes the
transaction, so a writer that outruns the device waits for real I/O instead of
growing. The ceiling counts the transaction's run bookkeeping with its staged
blocks, because a free holds almost all of its memory in the runs and dirties a
spine's worth of blocks whatever the file's extent count: over the blocks alone
it would not bound a delete at all. Measured, deleting a maximally fragmented
file holds 88 600 bytes at 1 200 extents and 110 368 at 4 800, where freeing it
inside one transaction holds 448 448 and grows with the file
(`tests/bounded_iteration.rs`).

The ceiling is the **machine's**, and the mounted volumes share it
(`tairix_reclaim::PinnedShare`, one instance the host installs on every
volume's pinned ledger): a volume may hold an equal share of it, capped further
by what the volumes already holding leave and by the machine-wide reserve floor
every consumer obeys. A figure derived per volume is a multiple of the machine
as soon as the machine has several volumes, and these are the bytes nothing can
reclaim. A volume holding nothing counts for nothing, so a machine whose other
volumes are empty leaves the whole ceiling to the one that is writing, and one
volume is bounded exactly as it would be alone. Measured, four 100 TiB volumes writing at
once peak together at 4 195 744 bytes against a 4 194 304-byte machine ceiling,
where a per-volume ceiling let the same four reach 8 670 016
(`tests/write_amplification.rs`).

Rising memory pressure lowers that ceiling and halves the dirty-age window band
by band, down to one coalesced device transfer (`RUN_BYTES`) and no further:
the answer to a tightening machine is always to publish sooner, never to hold
more and never to drop. Below that floor the drain could not form a full run, so
a machine whose ceiling cannot reach it refuses the mount rather than accepting
it and leaving it to commit after almost every record.

Forward progress does not depend on the ceiling. The only operation whose staged
bytes scale with a caller's argument is a file write, and it is the one the bound
cuts short: it stores at least one record whatever the ceiling, then stops on a
record boundary and reports the count, exactly as `write(2)` may. An unlink or a
truncate scales with the volume's own state instead, and yields to the same
ceiling from the other side: it frees at least one extent, then stops on an
extent boundary and the operation publishes before the next step.
`FilesystemWrite::write_all` is where that loop lives once for every caller that
needs the whole value stored, and a caller with an indivisible value (a symlink
target, bounded by the ABI) asks for the whole of it so the bound bites at the
operation's end instead. The pinned bytes are published through the reclaim
model's pinned ledger, so a volume's unwritten data is a row of its own in the
System Information cache-ledger export — kept out of the per-class reclaim
totals, because memory that can only be written out is not headroom.

Free space lives in an **on-disk paged allocation map** (`allocmap`,
`allocator`): a contiguous region of a header block, a summary recording each
bitmap page's free count, and one bit per device block, each block sealed with
the ordinary keyed header under `BlockType::AllocMap`. Because free space is
rebuildable rather than authoritative, the region is *not* copy-on-written but
updated in place under a clean/dirty generation stamp. The first mutation after
a clean sync stages the invalid stamp with the commit's authoritative phase, so
its existing barrier makes invalidation durable before any page write. Pages
stay in the bounded cache between commits, then move into the same dirty set;
`fs_sync` drains them in bounded runs, barriers once, and writes the clean
stamp. If staging, a page write, or a barrier fails — under a sync or under an
eviction — the cache and staged pages are discarded as untrusted and the next
allocator operation rebuilds from the committed trees; nothing short of a device
fault does that. A mount adopts the map only when it authenticates at the
address the committed root names and is stamped clean at that generation;
otherwise it rebuilds by walking the trees from the selected root. Mounting a
synced volume therefore costs a handful of block reads rather than a walk of
every inode and extent on it, and the resident cost is a bounded cache of
`MAX_CACHED_PAGES` pages however large the volume.

A **read-only handle holds no allocator at all** — the field is `None` — so it
cannot allocate, free, dedupe, or trim by construction, builds no allocation
state, and reports the committed free count straight from the transaction root.
It also writes nothing on the *verifying* paths, which do not allocate: the one
copy-repair site and both the scrub cursor and the health baseline are gated on
the same flag, so a scrub or health pass on such a handle verifies and reports
without touching the medium. Block allocation draws data upward and metadata
downward from the pool with a small metadata reserve, so a delete can
copy-on-write itself even on a full volume. No `unwrap`/`expect`/`panic!` and no
`unsafe`.

> **Staged build.** A volume is a mountable copy-on-write
> filesystem with B-tree metadata, a `lib/crypto` keyed-MAC authenticator in
> two physical copies, **at-rest encryption** under a per-volume key
> hierarchy, and a per-data-record **integrity field** (logical content hash
> + physical checksum) verified on every read (Stage 5), and **first-party
> compression** of every data record before encryption with a raw-store
> fallback (Stage 6), and **deduplication** with a chunk/refcount tree, a
> reverse-reference tree, reflinks, and a bounded byte-verified dedupe
> cache (Stage 7), and a resumable, capability-gated **online scrub** that
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
> earlier stage (Stage 12), plus stages 13–15 in the
> [specification](../../../docs/src/filesystem/arxfs-spec.md). Stages 16–21
> remain sequenced by `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`.

## Security

`arxfs` **stores** each inode's owner, mode, ACL, and capability gate. It
reports the record through `FilesystemSecurity` (`security(node)`) and
accepts an updated one through `ARXFS::set_security`, but makes **no**
permission decision itself: the VFS is the policy point (`AGENTS.md` §5.4).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_FS_MOUNT` to run `ARXFS::scrub` (online verify-and-repair),
  `ARXFS::check` (offline structural validation/rebuild), `ARXFS::rescue`
  (damaged-volume extraction), `ARXFS::trim` (TRIM/discard), and
  `ARXFS::health` (device-health pass + health-triggered scrub); without it
  each fails closed with `PermissionDenied`.
- The read/write methods are reached only through the `DriverHandle` the
  host minted at load time, and the VFS only delegates a write to a
  non-`READ_ONLY` mount. The driver runs in user space; it does not
  request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p tairix-drv-fs-arxfs` runs the block-header decode-rejection
tests (wrong magic / type / address / UUID / flipped checksum) and host
tests over an in-memory device: format/open (and unformatted-device
rejection), nested create/lookup/listing, read/write across block
boundaries, extent-backed large files across a remount, inode-tree
split/borrow/merge across many inodes, a many-extent file that splits its
extent tree, a contiguous write collapsing to one extent, the free-space
rebuild matching the authoritative live set, `truncate` prefix survival,
`remove` reclaiming space after `NoSpace`, the fail-closed
extremes (`AlreadyExists`/`DirectoryNotEmpty`/`LengthOutOfRange`/
`NotFound`), the per-inode security
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
free-space rebuild agreeing, the dedupe cache warming from writes rather than a
mount-time walk, dedupe staying within the encryption domain, and integrity +
compression holding on a
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

Allocation-map tests refuse the first page write and retain arbitrary map-page
subsets across a failed sync barrier, both with and without the publishing slot.
Every case rebuilds the exact map from the selected transaction root, while a
clean sync remains directly adoptable. Same-handle recovery then exercises
check, write, and grow before remounting, so no entry point can consult poisoned
derived state.

The Stage-12 suites are the adversarial superset of all the above, reusing the
same seams (`AGENTS.md` §2.2): the crash-replay sweep is **generalised to every
commit step across every representative transaction** (create, write, truncate,
remove, reflink, scrub, check, trim, health) — each faulted at every
write-budget cut-off, the re-opened volume always mounting on a whole
transaction boundary with the effect fully present or fully absent and the
witness file never lost. That sweep runs in **both commit shapes** from one
body: a transaction per operation, and a *batch* of three operations sharing
one, where every write count leaves either all of the batch or none (run
unbatched, that assertion fails, so the sweep discriminates the shape rather
than merely passing under it). Two further cases hold the batched commit: a
power loss straight after the commit that publishes a batch, over every
combination of the slot pair a volatile device cache may keep — the primary
copy selects the batch and when it lands every operation in it is *readable*,
because every block its root names crossed the one barrier first — and a batch
the ceiling forces out mid-way, after which a crash keeps no more than the
caller was told was written. Alongside them is a **corruption-injection suite**
that wounds each
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

The **sparse-file tests** (`arxfs-spec.md` §19, `plans/SPARSE.md` §17) cover
all ten mandatory cases: a 10 MiB all-zero file with a 10 MiB logical size and
zero mapped data blocks (also the encrypted-volume no-plaintext case, surviving
a remount), a non-zero write splitting an extent map around a hole (ordered,
non-overlapping), overwriting data with zeroes turning the block into a hole
while a reflink keeps the old data, `truncate` up making a hole and down freeing
only the real data, a reflink preserving holes with no zero-range chunk, scrub +
check validating sparse metadata with no physical read for a hole, and an
all-zero record bypassing compression while a non-zero constant still
compresses.

The **write-amplification baseline** (`write_amplification`,
`arxfs-spec.md` §22, `plans/ARXFS-WRITEBACK.md` §1) prices the write path
instead of describing it: an in-RAM device records every command the driver
issues it, in order — each write's start block and run length, and each cache
barrier — and the harness asserts exactly what a single-call 64 KiB write, the
same bytes in sixteen calls, a 34-byte append, and an empty-file create each
cost at both block sizes (commands, blocks, blocks superseded, bytes,
amplification). It also holds the write-back cache's contract: a transaction
writes each authoritative block once, every ordinary commit and sync issues one
barrier, map pages share the same bounded run drain, and the drain issues one
request per physical run rather than one per block. A second baseline prices the
same workloads *batched*, where the calls of one window join a single
transaction: sixteen calls then cost the same blocks and the same bytes as one
call, behind one barrier, with nothing superseded. The same workloads on a
100 TiB volume produce an identical command stream.

The same harness holds the **bound**: a payload far wider than the ceiling a
small machine derives is written whole through repeated short counts, the peak
bytes the set pinned stay inside that ceiling plus the one record the write was
in the middle of, the transaction is written out more than once, every forced
commit carries its own barrier, and the bytes read back exactly. A critical
band pins less and publishes more often than an unpressured one; the smallest
machine a volume may be mounted on — one transfer window per volume — still
completes the same write, and does so with a hundred tebibytes attached at both
block sizes. A read-only mount pins nothing at all.

It also holds the **combined floor**: four 100 TiB volumes mounted on one
machine, each advancing a slice in turn so all of them are holding staged bytes
while the others decide what they may stage. The bytes pinned across every
volume at once stay inside the machine's one derived ceiling, each volume's
payload reads back byte-exact, and the bound bites on every volume. The
machine-wide peak is taken as the shared total moves rather than summed from
per-volume peaks the volumes never reached together, and the case is run both
on a machine whose ceiling divides into shares above one device transfer and on
one whose shares fall to the forward-progress floor.

The **pending-delete tests** hold the freeing-across-transactions contract: a
delete of a file with more extents than one transaction may hold publishes the
inode in the set and finishes it in further transactions, an interrupted one is
completed by the next writable mount and returns exactly the blocks an
uninterrupted one does, a read-only mount leaves the set and the device
untouched, a stale handle cannot hard-link a node the set names, an interrupted
truncate is only ever a prefix of the file that was there, and `check` reclaims
an orphan by the same route. The bounded-iteration harness prices it: deleting
four times the extents holds the same peak footprint, where doing it inside one
transaction grows with the file.

The 1 GiB filesystem soak (`cargo xtask fssoak --target arxfs`) drives the
shared cross-filesystem exerciser, and `cargo xtask fuzz` harnesses fuzz the
mount / metadata-decode path (`fuzz_mount`, which since Stage 7 also decodes
the chunk/refcount and reverse-reference records via the dedupe-index rebuild,
and since Stage 8 also drives the scrub-progress record decode by running a
bounded scrub on every successful mount, and since Stage 9 also runs the
offline `check` on every successful mount and feeds every image to
`ARXFS::rescue`, driving the transaction-root scan and extraction decode
paths, and since Stage 11 reports SMART-style telemetry and runs `health` on
every successful mount, driving the health-baseline record decode path, and
since Stage 12 also walking every reachable directory on every successful
mount, driving the encrypted directory-block decode path)
and the first-party compression decoder (`fuzz_compress`, in `lib/compress`)
(`AGENTS.md` §19.6).

## End-to-end QEMU vertical

`tests/integration/arxfs_virtio_blk_pci_x86_64` mounts a planted arxfs
volume over a real (emulated) virtio-blk-pci device under QEMU and
round-trips a read and a write (`cargo xtask test --qemu`). The backing
image comes from the `tests/integration/arxfs_image` fixture, which the
real arxfs driver itself authors, so the fixture and the driver share one
source of truth for the on-disk format (`AGENTS.md` §2.2).

## Public surface

`register` is the driver entry point. `ARXFS` itself is exported so the driver
host can construct an instance (`ARXFS::format` / `ARXFS::open`) and reach into
it through the filesystem traits and the `set_security` accessor, and
`plant_nested_file` authors a file at a nested path for the image builders and
store fixtures. The rest is the vocabulary those calls need: the report and
option types the capability-gated operations return or take, the volume-key and
entropy seam, and the behavioural constants a caller or an acceptance test holds
the driver to — `RUN_BYTES` (the device transfer window both I/O directions
gather to), `TRIM_BATCH_RANGES`, `MAX_CLUSTER_PLAINTEXT`, and
`VOLUME_KEY_LEN`.
