# ARXFS-FEC.md - Forward Error Correction and Multi-Device Redundancy for ARXFS

Status: **planned. Nothing implemented** — no FEC code, no pool model, no
`lib/fec`, no `arxfs`/`arxfsadmin` command app. This is spec-stage 21
(`docs/src/filesystem/arxfs-spec.md` §15/§18), the last ARXFS stage.

This document predates the block-layer RAID stack that has since shipped
(`lib/raid`, `lib/raidmeta`, `drivers/storage/raid`, `drivers/storage/raid_member`,
`mdadm.app` — `plans/FIX-IO.md` IO6). Section 5.1 states the boundary between
the two and is binding: ARXFS FEC and block-layer RAID are different layers with
different capabilities, they are not alternatives, and neither reimplements the
other's mathematics.

Repository placement: `plans/ARXFS-FEC.md`, binding under `AGENTS.md` and
listed in its section 15.18 jump-sheet.
Primary code area: `drivers/filesystem/arxfs`.
Primary userland area: `userland/apps/`, as self-contained command-app bundles
(`AGENTS.md` sections 16.2/16.5, `plans/APPS.md`): a scriptable CLI command app
and a curses TUI command app (working names `arxfs` and `arxfsadmin`; final
names are fixed at implementation against the repository's command-app
conventions — existing app crates are single-word command names).
Possible shared code area: `lib/fec`, only if at least two production crates need
the same FEC code and `AGENTS.md` section 3 plus `PLAN.md` are updated in the same
change.

This document is written for an AI agent with access to the TAIRiX repository and
bound by `AGENTS.md`. It is not a changelog. It states the current design,
invariants, staged deliverables, tests, documentation requirements, and
acceptance criteria for adding always-on FEC, multi-device redundancy, live pool
changes, failure-safe recovery, and administration tooling to ARXFS.

## 1. Source of truth

The implementing agent must read these files before touching code:

1. `AGENTS.md`.
2. `PLAN.md`.
3. `docs/src/filesystem/arxfs-spec.md`.
4. `docs/src/filesystem/arxfs.md`.
5. `docs/src/filesystem/drives.md` — the binding storage-namespace spec —
   plus `plans/DEVICES.md`, `plans/DRIVES.md`, and `plans/ALIAS.md`.
6. `plans/APPS.md`, for the administration command apps.
7. Any existing ARXFS check, rescue, scrub, formatter, and administration docs.
8. This file.
9. The existing ARXFS code under `drivers/filesystem/arxfs`.
10. The storage stack the implementation builds on: the block-service IPC
    (`lib/abi/src/blkio.rs`), the volume attach/detach ABI
    (`lib/abi/src/volume.rs`), the volume manager (`drivers/storage/volmgr`),
    `lib/partition`, `lib/fsprobe`, the hardware tree
    (`lib/abi/src/hwtree.rs`), and the hotplug, IPC, sysinfo, curses,
    terminal, and standard-I/O code used by the implementation.
11. The block-layer RAID stack this work sits above and must not duplicate:
    `plans/FIX-IO.md` IO6, `docs/src/lib/raid.md`, `lib/raid` (the six
    composition engines, the `gf256` GF(2^8) field, and the P/Q/R syndrome
    code), `lib/raidmeta` (the member superblock and reassembly), the
    `drivers/storage/raid` composer and `drivers/storage/raid_member` agent,
    and the `mdadm` command app. Section 5.1 is the boundary.
12. `plans/ARXFS-WRITEBACK.md`, whose commit barrier every distributed commit
    witness in section 16 depends on.
13. `plans/ARXFS-MAINTENANCE.md`, which owns the shared background-work pacer
    section 21 consumes, the cross-layer stand-down rule, and the `arxfs`
    command app section 27 extends; and
    `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`, the ordered ledger this plan is the
    last item of.

If this document conflicts with `AGENTS.md`, `AGENTS.md` wins. If it conflicts
with the actual ARXFS code or `arxfs-spec.md`, the implementing agent must stop
and surface the mismatch before implementing a guess.

The agent must not assume Linux device paths, `/dev`, `/proc`, or `/sys`. TAIRiX
has none of those top-level interfaces. Disks are reached the TAIRiX way: a
user-space block driver publishes each logical unit as a block-service call
endpoint plus shared data window, granted on the storage-class hardware-tree
node it emits (`lib/abi/src/blkio.rs`, `plans/DEVICES.md`); the volume manager
(`drivers/storage/volmgr`) inherits those grants, probes partitions and
filesystem signatures (`lib/partition`, `lib/fsprobe`), and attaches a
filesystem driver through the audited `volume_attach` syscall under
`CAP_FS_MOUNT` (`lib/abi/src/volume.rs`). Discovered volume roots are published
as durable `id::<volume-id>` roots and aliases per the binding storage-namespace
spec (`docs/src/filesystem/drives.md`); raw device access is the
capability-gated `dev::` resolver, which lands only with its first consumer.
Live system information flows through the System Information API, never a
`/proc`-style tree.

## 2. The state this builds on (verified against the tree)

These were open questions when this plan was written. They are now answered
from the code, so no stage starts by re-deriving them. Where an answer changes,
it is corrected here in the same change (charter §13).

**Format and pipeline.**

- Filesystem block size **is** the device's logical block size (512..4096) —
  `bootstrap` takes it straight from `geometry()`. A metadata block and a data
  record are each exactly one filesystem block, so a 512-byte SD card yields 439
  usable content bytes per record and 384 payload bytes per B-tree node. The
  16 KiB / 128 KiB / 256 KiB record targets in `arxfs-spec.md` §5 are **not**
  implemented and are their own stage (spec stage 19). Any shard geometry this
  plan chooses must be stated in terms of the real record size, not the target.
- Commit model: a four-slot superblock ring of mirrored pairs; a transaction
  root carrying an inline commit record; commit order is copy-on-write blocks,
  root, then slot. `open` re-validates the root before accepting a slot.
- **The barrier is missing today** and is fixed by spec stage 17
  (`plans/ARXFS-WRITEBACK.md`): an ordinary commit issues no `Block::flush()`.
  Every distributed commit witness in section 16 depends on that barrier
  existing, so FEC cannot land before it.
- Authenticator: HMAC-SHA256 through `lib/crypto` over block identity plus
  payload, keyed from the per-volume master key. Data records additionally carry
  a 28-byte ChaCha20-Poly1305 nonce+tag trailer, a 5-byte stored-form
  descriptor, and a 40-byte integrity trailer (SHA-256 logical hash plus CRC-32C
  physical checksum through `lib/crc32c`).
- Metadata redundancy: exactly two copies, the companion at `primary + 1`, one
  shared read path that falls back on an unauthenticated *or unreadable* primary
  and repairs from the good copy. Not triplicated.
- Compression: the first-party `lib/compress` LZ77 codec, no external zstd. A
  whole aligned 16-block cluster compresses into fewer physical blocks; an
  incompressible record is stored raw. Pipeline order is dedupe → compress →
  encrypt.
- Dedupe: an in-memory, non-authoritative index bounded at 100 MiB (20 MiB
  frequently-used / 80 MiB general, LRU), scoped to the encryption domain, every
  candidate liveness-checked and byte-verified before sharing. An unshared block
  has an implicit refcount of one and no tree record.
- Sparse: holes are **implicit** — the absence of an extent — with no
  `ExtentKind` field on disk (`plans/SPARSE.md`, spec §19). Allocated size is
  reported from mapped extents.
- Scrub, check, rescue, trim, and health exist, share one verification core, and
  are capability-gated on `CAP_FS_MOUNT`. **None has a production caller** and
  there is no `arxfs` command app. Both are fixed by spec stage 18
  (`plans/ARXFS-MAINTENANCE.md`), which lands the maintenance scheduler, the
  runner, and the `arxfs` command app — so by the time this plan runs, section
  27's administration surface extends that app rather than being built from
  nothing, and section 21's scheduler joins an existing pacer rather than
  inventing one.

**Storage stack.**

- Devices are reached through the `blkio` block-service endpoint granted on a
  storage-class hardware-tree node; `drivers/storage/volmgr` probes partitions
  and filesystem signatures and attaches a driver through `volume_attach` under
  `CAP_FS_MOUNT`. Leaf drivers: `virtio_blk`, `emmc2` (SD/eMMC, 512-byte
  blocks, 64 KiB multi-block ADMA2 transfers, declares `Removable`),
  `usb_msd`.
- `Block` carries `flush`, `discard_capability`/`discard`, `device_health`, and
  `device_class` (`Rotational`/`SolidState`/`Removable`/`Virtual`). ARXFS
  currently consults none of `device_class`.
- `volmgr` already refuses to attach a filesystem to a device whose first block
  probes as RAID member metadata (`PlanSummary::raid_members`), so a pool member
  cannot be silently mounted as a bare volume.
- ARXFS runs **in-kernel** today (`kernel/tairix-kernel/src/root_mount.rs`,
  `system_mount.rs`, `volume_service.rs`), not as a user-space driver, and
  `tools/mkimage` drives the same driver on the host. A pool model must work in
  both, and the administration service (section 27) is where the user-space
  half lives.
- A whole-disk read cache sits below ARXFS
  (`kernel/tairix-kernel::block_cache::BlockCache`, `plans/SMARTRAM.md`
  SMART11): read-only, write-through, pressure-governed. There is no dirty layer
  below ARXFS and there must not be a second one.

**Bounds.** No dedupe, parser, allocation, or on-disk format bound may be
widened for FEC's convenience; the validation bounds are security bounds
(charter §24.4).

## 3. Terminology

**Pool**
: One mounted ARXFS filesystem backed by one or more block devices.

**Device**
: A block device with a stable ARXFS device UUID recorded in its pool label.
  At runtime it is reached through the `blkio` block-service endpoint granted
  on its hardware-tree node; a transient hardware-tree node id, bus address,
  or user-visible label is not the persistent identity.

**Failure domain**
: The unit whose complete loss ARXFS promises to tolerate. The initial desktop
  implementation uses one physical block device per failure domain. The format
  must allow future controller, enclosure, host, or rack domains without
  changing Reed-Solomon mathematics.

**Shard**
: One fixed-length sealed data image, parity image, or protected replica stored
  at one physical location on one failure domain.

**Protected segment**
: The smallest independently recoverable group of data and redundancy shards.
  A segment uses exactly one closed ARXFS protection profile.

**Protection floor**
: The minimum number of complete failure domains the pool promises can disappear
  while committed data remains recoverable. This is the user-facing redundancy
  contract. It is not a raw `k+m` setting and is never silently lowered.

**Effective protection floor**
: The minimum protection actually provided by every currently committed live
  segment and critical metadata object. During an online upgrade this may be
  lower than the requested target until conversion completes. Administration
  tools must report the effective value, not merely the requested value.

**Placement epoch**
: A monotonic pool generation that identifies the active device set, failure
  domain map, placement policy, and profile-selection policy used for new
  allocations.

**Membership generation**
: A monotonic generation for the committed pool device-membership state.

**Recovery job**
: A persistent, resumable, segment-granular operation such as scrub, repair,
  replacement, evacuation, rebalance, profile conversion, or metadata
  re-replication.

## 4. Goals

Add mandatory protection to ARXFS regular file data so that:

- A single-device filesystem can recover from bad sectors, missing local shards,
  and bit rot through intra-device FEC.
- A multi-device filesystem uses one pool-wide redundancy code across distinct
  failure domains to survive whole-device loss and local media errors.
- Data, parity, authentication, compression, dedupe, sparse files, and COW
  remain one coherent format and transaction model.
- Clean random reads do not read parity.
- A small random overwrite changes one protected segment, not the whole file.
- Device addition, clean removal, replacement, evacuation, rebalancing, and
  protection conversion operate while the filesystem remains mounted.
- Every recovery or reorganisation step after a device failure preserves the
  ability to survive the next failure permitted by the effective protection
  floor.
- Foreground latency and throughput remain first-class goals.
- A safe, obvious, curses-driven administrator and a scriptable companion CLI
  expose the same capability-gated management API.

The default protection policy is:

```text
one active device:
  local RS(8+2) media repair
  whole-device protection floor = 0

two active devices:
  two-way replication
  whole-device protection floor = 1

three or more active devices:
  protection floor = 2 by default
  topology-selected replication or RS(k+2), one shard per failure domain
```

An operator may request a different supported whole-device protection floor
only through the semantic pool-protection operation defined later. Operators do
not choose raw Reed-Solomon dimensions, matrices, shard placement, or per-file
profiles.

## 5. Non-goals

This work must not implement any of the following:

- A mount option or file flag that disables protection.
- Arbitrary user-selected `k`, `m`, generator matrices, stripe width, or shard
  placement.
- Per-file or per-directory redundancy profiles.
- A silent reduction in protection to make an allocation, removal, or rebuild
  succeed.
- A no-FEC compatibility reader for old ARXFS-native data.
- A `v2` beside `v1`, a compatibility shim, a migration-only old-format reader,
  or a feature flag preserving the old ARXFS-native behaviour.
- FEC for foreign filesystems such as ext4 or FAT32.
- Whole-file FEC stripes.
- A permanent dedicated parity disk.
- Stacking intra-device RS coding beneath pool-wide RS coding.
- In-place parity updates that create a parity write hole.
- A destructive `forget missing device` shortcut that can orphan live data.
- A background job that weakens current recoverability while it runs.
- Direct raw-device mutation by the TUI or CLI.
- Hand-rolled cryptographic primitives.
- External FEC crates without explicit owner approval and the full dependency
  audit required by `AGENTS.md`.
- A graphical desktop dependency for administration. The TUI must work on a
  headless image.
- A guaranteed completion-time estimate. Progress and measured throughput may
  be shown; any ETA must be clearly labelled as an estimate.

TAIRiX has not shipped. The ARXFS-native format evolves in place. Obsolete
no-FEC fixtures, code, docs, and generated images must be deleted or regenerated
in the same change that makes them obsolete.

### 5.1 Relationship to the block-layer RAID stack

TAIRiX already ships a complete block-layer RAID stack: `lib/raid` (RAID0/1/5/6/
RAID-TP/RAID10 over a first-party `gf256` GF(2^8) field, with P/Q/R syndromes),
`lib/raidmeta` (member superblock, event counts, reassembly), the
`drivers/storage/raid` composer and `drivers/storage/raid_member` agent, and the
`mdadm` command app (`plans/FIX-IO.md` IO6). ARXFS FEC does **not** replace it
and is not an alternative to it. The two are different layers, and the boundary
below is binding.

**What block-layer RAID can do that ARXFS FEC cannot.** It protects *any*
filesystem — ext4, FAT32, ADFS, a foreign volume, a swap device — because it
knows nothing about content. It composes at attach time, before any filesystem
exists. It is the only redundancy available to a non-ARXFS volume, and it stays
the supported way to give one redundancy.

**What ARXFS FEC can do that block-layer RAID cannot.** Three things, each
structural rather than a matter of effort:

1. **Checksum-directed repair.** A block-layer array that finds two mirror
   copies disagreeing, or a RAID5 stripe whose parity does not match, cannot
   tell *which* copy is right — it has no checksum of its own and no notion of
   what the data should be. ARXFS authenticates every metadata block and
   checksums and hashes every data record, so it knows which reconstruction is
   correct and can prove it before returning a byte.
2. **Rebuild only live data.** A block-layer rebuild must reconstruct the whole
   device because it cannot tell an allocated block from a free one. ARXFS knows
   its allocation map and its reachability, so a rebuild copies only what is
   live — the difference between hours and minutes on a mostly-empty 100 TB
   volume, and a large reduction in the second-failure window.
3. **No parity write hole.** Block-layer RAID5/6 updates parity in place, so a
   power loss between the data and parity write leaves a stripe whose parity is
   wrong and whose loss is silent. ARXFS never overwrites a committed block, so
   a protected segment is written once, whole, and published by the transaction
   — the write hole cannot exist.

**Stacking is forbidden.** ARXFS FEC over a `lib/raid` array is a
double-redundancy configuration that wastes capacity, hides the physical failure
domains from the layer choosing placement, and makes the protection floor
unstateable — the filesystem would believe it has one failure domain where the
array has several, or vice versa. Concretely:

- A pool whose member is a composed RAID array is **refused** at pool create/add
  with that reason, fail-closed. The array is offered as a *single* device, so
  ARXFS would place every shard of a segment inside one apparent failure domain
  and its stated floor would be a lie.
- A single-device ARXFS pool on top of an array is allowed and is the *only*
  legal combination: ARXFS provides intra-device media repair over what the
  array presents, the array provides whole-device redundancy beneath, and
  neither claims the other's floor. Section 4's "one active device: local
  RS(8+2), whole-device protection floor = 0" is exactly the honest statement of
  that case.
- `volmgr` already refuses to attach a filesystem to a raw RAID member, so the
  reverse mistake — ARXFS directly on a member disk of a live array — is already
  closed.

**The mathematics is shared, never reimplemented.** `lib/raid/src/gf256.rs` and
the P/Q/R syndrome code are the existing first-party GF(2^8) and Reed-Solomon
implementation. ARXFS FEC MUST consume that one definition rather than write a
second (charter §2.2, which permits parallel *implementations of a trait* but not
a second copy of the same arithmetic). Since `drivers/*` may not depend on
another driver's crate and `lib/raid` is a `lib/*` crate, the shared home is
either `lib/raid` directly or a `lib/fec` crate the field and codec are hoisted
into and both consumers depend on. That choice is FEC1's first decision and it
updates `AGENTS.md` §3 and `PLAN.md` in the same change; what is *not* open is
whether a second GF(2^8) field may exist. It may not.

**Administration stays separate.** `mdadm` administers arrays; the `arxfs` CLI
and TUI administer pools. Neither grows the other's commands, and neither
mutates a raw device directly (section 27.1).

## 6. Mandatory invariants

These invariants are review blockers if violated.

1. **Authentication before plaintext.** A reconstructed sealed record is
   untrusted until final record authentication succeeds. No failed or unverified
   record is decrypted or decompressed.
2. **One redundancy layer.** A multi-device segment is encoded once across
   failure domains. Do not add a second local RS layer under it.
3. **Failure-domain placement.** In a healthy layout, no two shards of one
   segment occupy the same required failure domain.
4. **Truthful protection.** The committed effective protection floor equals the
   minimum protection of all live data and critical metadata. It is never
   inferred from a desired setting alone.
5. **No silent downgrade.** A transaction that cannot meet the active floor
   fails with a typed error before commit.
6. **COW relocation.** Data repair, device replacement, evacuation, rebalance,
   and profile conversion write a complete verified destination before changing
   the committed descriptor.
7. **Second-failure safety.** With an effective floor of two and one domain
   already missing, every committed intermediate state remains recoverable after
   any one additional active failure domain disappears.
8. **No indispensable device.** Pool import, critical metadata, and committed
   root discovery must not depend on one special disk.
9. **Stable identity.** Device paths, enumeration order, and bus positions are
   never persistent identities. Device UUID plus authenticated membership is.
10. **Current-format mixed placement.** Online changes may leave live segments
    using different closed profiles and placement epochs. This is one current
    format, not backward-compatibility code.
11. **Foreground-first performance.** Background jobs are bounded,
    checkpointed, parallel where safe, and dynamically throttled. They do not
    hold a pool-global lock across data movement.
12. **One management authority.** The CLI and TUI call the same versioned,
    capability-checked administration service. They do not implement placement,
    recovery, or safety policy independently.
13. **Fail closed.** Too many erasures, stale membership, ambiguous device
    identity, insufficient capacity, insufficient metadata witnesses, or failed
    safety proof returns a typed error and preserves the last committed state.

The second-failure guarantee is mathematically impossible after one disk has
failed in a two-device pool. Administration must state this plainly. The
invariant applies to pools whose effective whole-device protection floor is at
least two.

## 7. Mandatory data layering

The regular-data write pipeline is:

```text
logical file bytes
-> sparse-hole detection
-> record/chunk packing
-> dedupe decision over canonical pre-encryption content
-> compression through the existing ARXFS compression layer
-> encryption plus existing per-record authentication/keyed integrity
-> protected-segment assignment
-> FEC or replication over sealed ciphertext data images
-> per-shard keyed integrity for data, parity, and replicas
-> failure-domain-aware COW allocation
-> durable shard writes
-> metadata/root commit witnesses
```

The read pipeline is:

```text
resolve logical extent and protected-segment descriptor
-> select an eligible clean data shard or replica
-> verify shard header and shard keyed integrity
-> if clean, avoid parity reads
-> if missing/corrupt, read enough independently placed surviving shards
-> reconstruct only identified erasures
-> verify reconstructed shard integrity where applicable
-> verify final record authentication
-> decrypt
-> decompress
-> return plaintext
```

The relocation/profile-conversion pipeline is:

```text
read and authenticate source shards
-> reconstruct sealed ciphertext if required
-> if profile unchanged, move sealed shard bytes without decrypt/recompress
-> if profile changes, decode sealed data images and encode target redundancy
-> write and authenticate all destination shards
-> prove destination placement satisfies the active safety invariant
-> COW-commit the new descriptor
-> release old shards only after commit and reference validation
```

## 8. Fault and recovery model

The system must detect and handle:

- A bad sector inside a shard.
- A block-driver read error.
- A missing data, parity, or replica shard.
- A complete missing device.
- A device disappearing during a foreground transaction.
- A device disappearing during add, evacuation, replacement, scrub, rebalance,
  or profile conversion.
- Bit flips detected by per-shard keyed integrity.
- Bad parity discovered by scrub.
- Stale-shard replay.
- Cross-volume, cross-segment, wrong-index, and wrong-device shard substitution.
- A stale device reappearing after replacement.
- A crash at every membership and relocation checkpoint.
- A second device failing while a double-protected pool is degraded or
  rebuilding.

Known-bad shards are decoder erasures. Do not initially add a hot-path
unknown-error decoder that guesses corruption location. If local shard checks
all pass but final record authentication fails, fail closed and report
unrecoverable corruption.

The implementation does not protect against:

- Complete loss of the only device in a single-device pool.
- Loss beyond the effective protection floor.
- A ARXFS bug that creates authenticated but semantically wrong plaintext
  before encryption.
- A malicious writer possessing valid keys and authority.
- Hardware that falsely acknowledges durable writes in a way not detectable by
  the existing flush/FUA contract.
- Simultaneous loss of all metadata witnesses required by the active floor.

## 9. Protection policy and closed profiles

### 9.1 User-facing policy

The user-facing setting is a whole-device failure-domain floor, not copies and
not raw parity dimensions.

Supported semantic values are initially:

```text
0 whole-device failures:
  valid only for a one-device pool; local media FEC remains mandatory

1 whole-device failure:
  valid with at least two active failure domains

2 whole-device failures:
  valid with at least three active failure domains
  default for pools with three or more devices
```

Adding larger floors requires a later approved specification and a current
production need. Do not generalise to arbitrary integers now.

### 9.2 Profile selection

The implementation uses a closed profile set selected by topology and protection
floor. The profile selector is deterministic and format-versioned.

For floor one:

```text
2 domains: Replica2
3 domains: RS(2+1)
4 domains: RS(3+1)
...
9 or more domains: fixed-width RS(8+1) groups over 9 selected domains
```

For floor two:

```text
3 domains: Replica3
4 domains: RS(2+2)
5 domains: RS(3+2)
...
10 or more domains: fixed-width RS(8+2) groups over 10 selected domains
```

For one physical device:

```text
LocalRs8_2 across ten allocator lanes/regions on that device
```

The profile selector may use fewer than all pool devices for a segment when the
pool is wider than the fixed maximum. Segment device subsets must rotate and be
weighted by eligible free capacity so all devices contribute and no permanent
parity devices emerge.

### 9.3 Profile properties

All RS profiles are systematic. Clean reads address an original data shard.
Parity is additional.

The finite field, primitive polynomial, generator matrix construction, profile
IDs, and shard ordering are stable on disk and covered by deterministic known
vectors.

Replica profiles are explicit profiles, not special-case undocumented mirrors.
They use the same descriptor, generation, keyed-integrity, COW, placement, and
repair invariants as RS profiles.

### 9.4 Protection changes

A mounted pool may change its semantic floor online.

- Raising the requested floor makes new writes use the target profiles, then
  converts existing segments. The effective floor is not raised until every
  live segment and critical metadata object satisfies it.
- Lowering the floor requires a separate explicit operation and clear
  confirmation. The effective floor is lowered before any lower-protection
  segment is committed, so status remains truthful.
- A disk-removal command must not implicitly lower the floor. For example,
  shrinking a three-device floor-two pool to two devices requires a deliberate
  floor change to one first.
- When the lower floor is invalid until the device count changes, such as a
  two-device floor-one pool becoming a one-device floor-zero pool, one atomic
  remove-and-downgrade plan is permitted only if the downgrade is displayed,
  reasoned about, and confirmed as a distinct high-severity consequence.
- Raw `k`, `m`, matrix, and per-file controls remain unavailable.

## 10. Protected-segment model

A protected segment is the smallest independently recoverable unit.

For an RS profile it contains:

```text
k data shard slots, where 2 <= k <= 8
m parity shard slots, where m is 1 or 2
one closed profile id
one fixed fec_shard_len
one placement epoch
one segment id
```

For a replica profile it contains one sealed data record and two or three
independently placed authenticated copies.

Each RS data slot stores exactly one sealed ARXFS data record or an explicit
unused tail slot. Every shard in the segment has the same bounded
`fec_shard_len`. The actual sealed length is recorded separately. Bytes after
the sealed record are authenticated zero padding and are not decrypted or
returned.

Tail segments are encoded with explicit authenticated zero-padded unused data
slots. Do not add a second tail algorithm.

Sparse holes have no data shard, no replica, and no parity. A hole is represented
only by existing sparse metadata.

A segment must not combine shards requiring different encryption keys,
protection floors, membership generations, or placement epochs in a way that
makes independent recovery ambiguous.

## 11. On-disk metadata

The exact Rust names must follow the existing code style. The format requires
the following concepts without duplicating equivalent existing fields.

### 11.1 Pool label

Every member device stores an authenticated label ring in multiple device-local
regions. A label identifies:

```text
ARXFS magic and current native format
pool UUID
device UUID
membership generation
placement epoch
device state
failure-domain identity and type
logical and physical sector constraints
usable device range
latest witnessed pool-root generations
protection-floor policy
label sequence and keyed-integrity tag
```

Labels are discovery records, not sole authority for data placement. A stale
label must never override a newer committed pool configuration.

### 11.2 Pool configuration

The committed pool configuration identifies:

```text
pool UUID
membership generation
placement epoch
active, joining, evacuating, missing, quarantined, and retired device records
stable device UUID to failure-domain mapping
requested and effective protection floors
profile-selection policy id
critical-metadata replica policy
background-job checkpoints
allocator/recovery workspace accounting
```

Membership changes are COW transactions. There is no mutable single superblock
whose loss prevents import.

### 11.3 Protected-segment descriptor

A descriptor identifies:

```text
pool UUID
format and transaction/root generation
profile id
segment id
placement epoch
logical owner or allocation-group context
fec_shard_len
k and m implied and validated by the profile
actual sealed length for every data slot
slot state: live, unused tail, or an equivalent existing current state
for each shard or replica:
  role and index
  device UUID
  failure-domain id
  physical extent
  shard generation
```

A descriptor must not trust a device's current path or enumeration index.

### 11.4 Shard header

Every data, parity, and replica shard carries a compact self-identifying header
protected by keyed integrity. It includes enough data to reject stale,
cross-segment, cross-volume, wrong-device, and wrong-index replay:

```text
block type
ARXFS format generation
pool UUID
membership generation or equivalent binding
transaction/root generation
placement epoch
profile id
segment id
shard role and index
device UUID expected by the committed descriptor
fec_shard_len
actual sealed length where applicable
keyed-integrity tag or reference to its existing storage location
```

A header is untrusted until its keyed-integrity check succeeds. A bad header or
tag makes that shard an erasure.

### 11.5 Commit witness

A root or membership transaction is not acknowledged until its new data,
redundancy, and metadata writes are durable and the commit record has been
written to at least:

```text
active whole-device protection floor + 1
```

distinct eligible failure domains, or every device when the topology has fewer.
For a floor-two pool this means at least three authenticated commit witnesses.
For a two-device floor-one pool it means two.

A commit witness binds:

```text
pool UUID
membership generation
placement epoch
transaction/root generation
root identity and digest
write-set or equivalent transaction digest
requested/effective protection state
authenticated commit marker
```

After any number of device losses up to the acknowledged floor, at least one
witness for an acknowledged commit remains. During an unacknowledged crash,
mount may recover the old or new complete state, never a mixed state.

### 11.6 Persistent recovery-job record

Every long-running job has bounded persistent state:

```text
job UUID
job kind
pool UUID
source and target membership generations
source and target placement epochs
source and target protection/profile policy
state: planned, running, paused, blocked, cancelling, complete, failed
last committed segment cursor or bounded work queue root
verified bytes/segments completed
remaining live bytes/segments as a measured snapshot
blocking condition and stable diagnostic code
creation and update Time64 values
```

A job record is control metadata and receives the same critical metadata
protection. It must not contain secrets or unbounded user strings.

### 11.7 Final record authentication

The existing per-record authentication remains the final authority. Its
associated data must bind the record strongly enough to reject stale replay. If
current associated data is insufficient, update it in place and update every
caller and fixture in the same change.

It should bind at least:

```text
pool/volume identity
inode or extent identity
logical offset or record id
transaction/root generation where existing semantics require it
compressed length
sealed length
encryption nonce
compression mode
protected-segment id
data-shard index or replica identity
profile id
placement epoch where needed to prevent replay
```

Avoid duplicating fields already transitively authenticated, but preserve the
security invariant: a valid sealed record from one logical location must not
authenticate as another.

## 12. Encryption and integrity

FEC and replication operate on sealed ciphertext, never plaintext.

Per-shard keyed integrity identifies bad shards before decode. Final record
authentication detects any reconstruction or substitution error before decrypt.
If local shard verification and final authentication disagree, final
authentication wins and the operation fails closed.

No cryptographic primitive may be implemented in FEC, placement, pool, job, CLI,
or TUI code. Use the existing ARXFS integrity abstraction and `lib/crypto`.

Secret keys and plaintext must never appear in parity, pool labels, job records,
logs, panic messages, CLI output, TUI screens, or test names. Temporary buffers
follow existing zeroisation rules.

A reappeared stale device is quarantined. Its apparently valid shards are not
silently introduced into the active pool. Any recovery use must be explicitly
validated against current authenticated descriptors and generations.

## 13. Compression, sparse files, and dedupe

### 13.1 Sparse files

Hole detection occurs before dedupe, compression, encryption, and protection.
Holes allocate no data, replica, or parity bytes. Reading a hole follows the
existing zero-fill path.

### 13.2 Compression

Compression occurs before encryption and protection. Parity is computed over
the padded sealed compressed record image. Encrypted or parity bytes are never
fed to a second compression pass.

Random writes retain bounded independent compression-record granularity. This
work must not create a whole-file or excessively large mutable compression unit.

If a changed compressed record has a different sealed length, parity delta uses
the complete old and new fixed-length padded shard images.

### 13.3 Dedupe

Dedupe decisions remain before encryption details that would make equal content
non-equivalent, following the existing ARXFS security model exactly.

Deduped records are immutable COW objects. An overwrite creates a new sealed
record and updates its protected segment. It never mutates a shared shard.

A dedupe hit references one existing protected record and its complete recovery
context. Reference counts must keep every shard, parity image, replica, and
descriptor required to recover that record alive.

Relocation of a deduped object happens once regardless of reference count.
Profile conversion must update the shared physical object atomically before
references can observe the new descriptor.

If existing dedupe cannot safely share one record within a multi-record
segment, choose the simpler safe policy: dedupe only at a protection-context
granularity the current reference model can pin correctly. Do not implement
unsafe partial sharing.

## 14. Read behaviour

### 14.1 Clean random read

A clean read of one logical record should perform:

```text
one eligible data-shard or replica read
required metadata reads, normally cached
zero parity reads
```

In a multi-device pool, the read selector may choose among equivalent replicas
or cached/reconstructed copies using measured queue depth and latency, while
preserving stable integrity checks.

### 14.2 Recovery read

If the addressed shard is missing or fails local integrity, read enough
independently placed peers from the same segment to reconstruct it.

For systematic `RS(k+m)`, one missing data shard usually requires `k` valid
surviving shards in total. Two missing shards under an `m=2` profile require a
valid set of at least `k` survivors.

All peer I/O should be issued in parallel where the block API permits. Once `k`
valid shards are available, speculative extra reads may be cancelled only
through a safe existing cancellation primitive; otherwise let them complete
without blocking the returned verified read.

### 14.3 Read repair

After successful reconstruction and final authentication:

- A read-only mount returns verified data but persists nothing.
- A writable mount may enqueue a COW read-repair job.
- A degraded pool must run the same safety evaluator as any other relocation.
- A repair never overwrites a suspect shard in place before the new committed
  descriptor exists.

### 14.4 Too many failures

If fewer than `k` valid RS shards or no valid replica remain, return a typed
unrecoverable-corruption error and no plaintext. Do not guess, zero-fill, or
substitute stale data.

## 15. Random and sequential writes

### 15.1 Full-shard random overwrite

A full data-shard overwrite affects one protected segment.

For `RS(k+m)`:

```text
payload writes = one new data shard + m new parity shards
```

For `ReplicaN`:

```text
payload writes = N new replicas
```

This is before normal COW metadata. All independent device writes should be
issued concurrently.

For local or pool `RS(8+2)`, a one-record overwrite writes one data shard and
two parity shards. It must not rewrite a 1000-block file or unrelated segment
parity.

### 15.2 Parity delta

The preferred random-update method is linear parity delta:

```text
delta = new_padded_data_shard - old_padded_data_shard
new_parity_i = old_parity_i + coefficient_i * delta
```

The implementation uses the selected finite-field representation correctly.
Plain XOR may appear only as explanatory documentation, not as an RS
implementation shortcut.

### 15.3 Partial logical write

A partial write follows:

1. Read and verify the old sealed record.
2. Authenticate before decrypting.
3. Decrypt and decompress.
4. Apply the logical patch.
5. Recompress through the existing compressor.
6. Reseal through existing encryption/authentication.
7. Compute target replicas or parity delta over old/new padded sealed images.
8. COW-write new payload shards, metadata, and commit witnesses.

A failed old authentication aborts the write. Unverified plaintext is never used
as rewrite input.

### 15.4 Multiple writes in one segment

The writeback path may coalesce multiple dirty data slots and calculate parity
once. This is one optimisation over the same tested encoder, not a separate
correctness implementation.

### 15.5 Sequential write

Sequential writes fill data slots, calculate parity once per full segment, and
issue shard writes in parallel. Data and parity roles rotate across devices; no
device becomes a permanent parity bottleneck.

## 16. COW, distributed commit, and crash consistency

There is no parity write hole and no membership write hole.

A transaction changing a protected segment writes new physical versions of:

```text
changed data shards or replicas
all affected parity shards
segment descriptor
allocation and reference metadata
required critical metadata copies
transaction root and commit witnesses
```

The old root and old segment remain valid until commit. Before commit, recovery
selects the old complete state. After a recoverable new commit witness exists,
recovery may select the new complete state. It never treats mixed old/new data
and parity as committed truth.

Foreground transactions pin a placement epoch. A membership transition must
fence new placement without holding a long pool-global lock:

- New reservations stop using an evacuating or failed device after the epoch
  transition commits.
- Reservations made under the old epoch either finish before the fence or fail
  with a typed topology-change result through the existing transaction conflict
  mechanism.
- There is no sleep-loop or retry-until-success workaround.

`NoSpace`, device loss, flush failure, or safety-proof failure leaves the old
state valid and frees uncommitted destinations through existing COW recovery.

## 17. Physical placement

### 17.1 Single-device lanes

On one device, local `RS(8+2)` uses deterministic placement lanes or allocation
regions so likely localised media damage does not erase all ten shards.

```text
lane 0..7: data shard index 0..7 across many segments
lane 8..9: parity shard index 0..1 across many segments
```

Lanes remain sequentially allocatable and batchable. Do not randomly scatter
every small write.

### 17.2 Multi-device placement

A healthy multi-device segment places at most one shard on each required failure
domain. Physical devices are the initial domain type.

The allocator selects devices using:

- Current membership and placement epoch.
- Profile width.
- Failure-domain uniqueness.
- Eligible free capacity after reserved metadata/workspace.
- Device health and write eligibility.
- Queue depth and balanced long-term utilisation.
- Sector/alignment constraints.

Parity roles rotate. Weighted selection must avoid making the smallest device a
hidden permanent bottleneck where a different valid subset can be used.

### 17.3 Temporary degraded placement

A recovery destination may temporarily place more than one segment shard on one
physical domain only if the safety evaluator proves that every additional
failure permitted by the effective floor still leaves enough valid shards. The
descriptor and administration tools must label such placement as degraded.

This is an emergency/recovery state, not normal allocation policy. Rebalance
must restore distinct-domain placement when capacity becomes available.

### 17.4 Larger failure domains

The format stores a domain type and stable id separately from device UUID. A
future controller/enclosure-aware allocator can map multiple devices to one
domain. The initial implementation must not claim controller/enclosure
redundancy unless it can discover and enforce it.

## 18. Critical metadata and pool import

Regular file data uses the closed replication/RS profiles. Critical metadata
uses authenticated full copies because it is small, latency-sensitive, and
required to locate everything else.

Critical metadata includes at least:

- Pool labels and membership configuration.
- Transaction root rings and commit witnesses.
- Allocation and extent roots.
- Inode and dedupe roots.
- Protected-segment descriptors.
- Key-wrapper and encryption-policy metadata.
- Persistent recovery-job roots.

The number of healthy metadata copies is at least the effective whole-device
floor plus one on distinct eligible domains. A floor-two pool therefore uses at
least three copies. A two-device floor-one pool uses both devices. A one-device
pool keeps existing local mirrored/triplicated metadata in separate regions.

Pool import:

1. Obtains candidate block devices through the existing storage-discovery
   path: the storage-class hardware-tree nodes and `blkio` block-service
   endpoints the volume manager consumes.
2. Reads bounded label rings and groups devices by authenticated pool UUID.
3. Validates device UUID, membership generation, sector constraints, and label
   integrity.
4. Quarantines stale, duplicate, foreign, or ambiguous identities.
5. Selects the highest internally valid committed root for which surviving
   witnesses and referenced metadata satisfy the active failure model.
6. Verifies that every required critical root is recoverable.
7. Mounts read-write only if the resulting state meets write-safety rules;
   otherwise mounts read-only when existing ARXFS policy allows and reports the
   exact reason.

No device is a mandatory "first disk". Device enumeration order cannot change
the selected committed root.

## 19. Online membership and topology state machine

### 19.1 Device states

The implementation uses a closed, validated state machine equivalent to:

```text
Unclaimed -> Joining -> Active -> Evacuating -> Retired
                         |  |
                         |  +-> Missing
                         |       |
                         |       +-> Replacing -> Retired
                         |
                         +-> Quarantined, only through explicit validation paths
```

Exact type names may differ to fit existing code. Illegal transitions are
unrepresentable or rejected before state mutation.

A stale reappeared device becomes `Quarantined`, not automatically `Active`.

### 19.2 Live add

Adding a device while mounted performs:

1. Resolve the device through the storage-discovery path: its hardware-tree
   node and `blkio` block-service endpoint, as consumed by the volume
   manager. There is no `/dev`; a Linux-style device path is never accepted.
2. Verify the device is unclaimed, writable, not mounted elsewhere, and meets
   sector/flush/alignment requirements.
3. Detect existing foreign or ARXFS signatures. Destructive initialisation
   requires explicit confirmation identifying model, serial where available,
   capacity, and stable id.
4. Allocate a new device UUID and write authenticated `Joining` labels.
5. COW-commit pool membership with the device present but not yet used for live
   allocations.
6. Establish required critical metadata copies and verify them.
7. Validate that the resulting active topology supports the pool's semantic
   protection floor. A one-device floor-zero pool cannot activate a second
   data-bearing device without an explicitly confirmed transition to at least
   floor one. The add plan may combine those operations, but it must present the
   protection change as a separate visible consequence rather than a silent side
   effect.
8. Commit a new placement epoch with the device `Active`.
9. Let new writes use the new epoch immediately under the target protection.
10. Start a throttled rebalance/profile-conversion job when beneficial.

When a one-device pool adds its second device, the requested target becomes
floor one only after explicit confirmation. The effective floor remains zero
until every old local-only segment and critical metadata object has been
converted. Adding a third device may offer a floor-two upgrade, but does not
silently perform one.

A crash at any step yields either the old pool plus an unclaimed/joining device,
or the new pool with a fully recognised member and truthful requested/effective
protection. It never creates an active unlabelled allocation target.

### 19.3 Live clean remove

Removing a healthy device while mounted performs:

1. Preflight target topology, target profile set, protected capacity, metadata
   copies, COW workspace, and effective protection floor.
2. Refuse if the target topology cannot satisfy the active floor. If the
   requested operation necessarily lowers protection, require the separately
   displayed and confirmed semantic floor transition first. The only combined
   exception is an atomic two-device floor-one to one-device floor-zero removal,
   because floor zero is not valid while both devices remain active.
3. Commit the device as `Evacuating`; new allocations stop using it.
4. Move or re-encode every live regular-data segment referencing it.
5. Move all critical metadata copies and commit witnesses needed after removal.
6. Verify zero live descriptors, references, job roots, or allocation roots
   require it.
7. Commit the target membership and placement epoch.
8. Mark the old device `Retired` and clear or invalidate its membership labels
   only after the pool no longer depends on it.

If the device disappears during evacuation, the job converts to failed-device
recovery using the last committed descriptors. Already relocated segments stay
valid. Unrelocated segments use their old redundancy.

A clean removal may be cancelled before the final retirement commit. Data moved
so far remains valid in its new placement; cancellation does not need to move it
back.

### 19.4 Live replacement

Replacing a failed or failing device:

1. Adds and validates the replacement as a distinct device/failure domain.
2. Associates it with the missing device in a persistent replacement job.
3. Reconstructs or relocates live segments one at a time through COW.
4. Restores critical metadata witnesses early.
5. Commits each new descriptor only after the second-failure safety proof.
6. Retires the old device UUID only after no live object depends on it.

A replacement may be larger. A smaller replacement is allowed only when the
preflight proves all live data, target protection, metadata, and working space
fit. The tools must warn that nominal raw size is not the same as protected
usable capacity.

### 19.5 Rebalance and profile conversion

Adding/removing devices or changing the semantic floor may create mixed closed
profiles and placement epochs. That is valid current state.

Rebalance:

- Moves sealed ciphertext directly when the profile is unchanged.
- Reconstructs sealed data images and computes new redundancy when the profile
  changes.
- Does not decrypt/recompress merely to move data.
- Preserves dedupe sharing and reference counts.
- Processes segment-granular COW units.
- Checkpoints progress persistently.
- Can pause and resume across reboot.
- Restores distinct failure-domain placement after emergency degraded placement.

The allocator uses the latest committed placement epoch for new data while old
segments remain readable through their descriptors.

### 19.6 Live protection-floor change

`survive one device` and `survive two devices` are semantic pool operations.
They are never side effects of another command.

Raising the floor:

- Validates enough distinct domains and capacity.
- Creates a target placement epoch.
- Uses target protection for new writes.
- Converts existing data and metadata online.
- Raises the effective floor only in the final commit after a complete audit.

Lowering the floor:

- Requires an explicit high-severity confirmation.
- Reports the exact failures no longer tolerated and expected capacity change.
- Lowers the effective floor truthfully before lower-protection segments appear.
- Converts/rebalances online without disabling encryption, integrity,
  compression, sparse files, or FEC/replication.

## 20. Second-failure-safe repair and reorganisation

### 20.1 Safety predicate

Before committing a relocated/reconstructed segment, ARXFS evaluates the
candidate destination against the current missing-domain set.

For every additional failure-domain set allowed by the effective protection
floor, the candidate must leave:

```text
at least k valid independently addressable shards for RS profiles
at least one valid copy for replica profiles
sufficient critical metadata and commit witnesses to import the resulting root
```

For the required floor-two case with one domain already missing, exhaustively
check every one additional active failure domain. Device counts are bounded by
format policy, so an exact check is preferred over an approximation.

### 20.2 Transition rule

For each segment:

1. Verify the old descriptor and all source shards used.
2. Compute a complete candidate destination.
3. Run the safety predicate before issuing writes.
4. Write and verify all destination shards.
5. Re-run the predicate against the latest membership generation before commit.
6. COW-commit the new descriptor and witnesses.
7. Free old extents only after the new root is durable.

The old layout remains the authority until step 6. A second failure before step
6 is handled using the old descriptor. A second failure after step 6 is handled
using the new descriptor.

### 20.3 Failure during a job

When another device disappears:

- Stop allocating new work under the stale membership generation.
- Preserve completed segment commits.
- Leave uncommitted destination writes unreachable.
- Recompute pool health, effective floor, and every active job plan.
- Resume only if the new plan passes the safety predicate.
- Otherwise mark the job `Blocked`, keep the pool in its safest readable state,
  and tell the administrator exactly which device/capacity action is required.

There is no destructive "continue anyway" mode.

### 20.4 Capacity and spare domains

A full rebuild may require a replacement device, an already active spare failure
domain, or sufficient safe distributed free capacity. ARXFS must not pretend
that free bytes automatically create a new failure domain.

The allocator reserves bounded COW/recovery workspace so one segment transition,
critical metadata repair, and transaction commit cannot be starved by ordinary
allocation. It need not reserve an entire device's capacity by default, because
that would impose severe hidden capacity loss. Instead:

- Ordinary allocation stops before consuming mandatory COW/recovery workspace.
- A full-device rebuild preflight calculates destination capacity explicitly.
- If safe capacity is insufficient, the pool remains degraded but does not
  weaken its existing ability to survive the next permitted failure.
- The admin tools state whether a replacement device is required.

### 20.5 Writes while degraded

A degraded pool remains writable only when every new regular-data segment,
critical metadata update, and commit witness can meet the current effective
protection floor on the surviving/added domains.

Examples:

- A floor-two six-device pool with one missing device may write new `RS(3+2)`
  segments across the five survivors while old `RS(4+2)` segments remain
  recoverable with one missing shard.
- A floor-two four-device pool with one missing device may use `Replica3` for
  new data while old `RS(2+2)` segments remain degraded-but-protected.
- A floor-two three-device pool with one missing device cannot place new
  floor-two data. It enters `ReadOnlySafetyStop` for ordinary mutation until a
  replacement restores a third domain or an explicitly confirmed protection
  downgrade completes.

ARXFS must not keep accepting writes by silently using a lower profile.

### 20.6 Two-device limitation

After one device fails in a two-device mirror, no design can survive loss of the
remaining device. ARXFS can still protect against local corruption on the
survivor through authenticated copies already available only if one remains;
it cannot claim a second whole-device tolerance. The TUI and CLI must display
this as a critical limitation, not a generic yellow warning.

## 21. Background jobs and performance scheduling

### 21.1 Job types

The persistent job engine supports the current required set only:

```text
Scrub
ReadRepair
MetadataRepair
ReplaceDevice
EvacuateDevice
Rebalance
ConvertProtection
RestoreFailureDomainPlacement
```

Do not create a generic plugin framework. Add another job kind only with a
current production operation, tests, and docs.

### 21.2 Foreground priority

Foreground filesystem I/O, synchronous recovery reads, and transaction commits
must not wait behind an unbounded background queue.

The scheduler must:

- Use bounded per-device and pool-wide in-flight work.
- Issue independent shard I/O concurrently.
- Yield based on actual queue pressure and measured foreground latency, not a
  fixed sleep loop.
- Pace against the foreground through the **one shared background-work pacer and
  class-keyed budget** every storage layer uses (`plans/ARXFS-MAINTENANCE.md`
  section 5), never a third copy of the duty arithmetic. Three schedulers pacing
  to three notions of "busy" cannot compose: their shares sum on one device and
  a filesystem-level verification can spend the bandwidth a rebuild needs. The
  cross-layer rule — restoring redundancy outranks verifying it — is the same
  rule stated there, applied inside ARXFS once ARXFS owns the redundancy.
- Avoid holding filesystem-global locks while reading or writing segment data.
- Release segment-level state between committed work units.
- Prioritise urgent metadata repair and failed-device reconstruction above
  cosmetic rebalancing, while still giving foreground I/O bounded service.
- Prevent indefinite starvation of any active recovery job.

### 21.3 Default job policy

The default policy is safe and automatic:

- Read repair is small and urgent.
- Critical metadata re-replication starts immediately after a loss.
- Failed-device reconstruction runs in `Balanced` mode by default.
- Scrub runs at low priority unless corruption is already known.
- Rebalance and non-urgent profile conversion use idle capacity.
- An administrator may select only closed priorities such as `Urgent`,
  `Balanced`, and `Idle`; raw queue depths and arbitrary scheduler constants are
  not public API.

`Urgent` does not bypass integrity, COW, capability, or safety checks.

### 21.4 Checkpointing and idempotence

Every job commits bounded segment-granular progress. Reboot, crash, pause, or a
new device failure resumes from authenticated metadata rather than rescanning
completed work blindly.

A work item must be idempotent:

- If its destination descriptor already committed, mark it complete.
- If only destination blocks exist, they are unreachable COW garbage and are
  reclaimed through normal recovery.
- If the source generation changed due to a foreground write, discard the stale
  plan and re-evaluate that segment.

There is no unbounded retry loop. Repeated conflicts become a visible blocked or
backoff state under an event/timer-driven policy.

### 21.5 Efficient rebuild and rebalance

- Scan allocated live segment descriptors, not every raw sector.
- Rebuild a deduped physical object once.
- Group work by source/destination device and physical locality.
- Batch full-stripe encoding where available.
- Reuse verified shard buffers within a bounded work item.
- Avoid plaintext, decompression, and recompression when moving sealed data.
- Use all eligible devices in parallel without saturating one parity target.
- Keep per-device statistics so the scheduler can avoid a slow or error-prone
  device without silently changing protection.

### 21.6 Pause, resume, cancel

- Pause occurs at a committed work boundary.
- Resume revalidates membership generation and the safety plan.
- Scrub, rebalance, and protection upgrades can be cancelled safely at segment
  boundaries.
- Evacuation can be cancelled before retirement; already moved segments stay.
- A replacement cannot be "cancelled" by forgetting the missing device. It may
  be paused or redirected to another replacement.
- A protection downgrade that has committed the lower effective floor cannot be
  described as cancelled back to the higher floor; restoring it is a new
  protection-upgrade operation.

## 22. Scrub, check, rescue, and repair

Online scrub verifies:

```text
pool labels and membership generations
critical metadata copies and commit witnesses
protected-segment descriptors
shard headers and keyed-integrity tags
failure-domain placement invariants
RS parity or replica consistency
final record authentication under existing ARXFS policy
dedupe reference safety
persistent job metadata
requested versus effective protection reporting
```

For a recoverable segment, scrub COW-repairs bad or missing shards and verifies
final authentication before success. In a multi-device pool it uses the same
second-failure safety evaluator as rebuild and evacuation.

Offline check/rescue uses the same decoder, profile table, descriptor parser,
pool import logic, and safety predicates as the mounted implementation. Do not
write a second FEC or pool-layout implementation.

Offline rescue may offer a read-only extraction path when the pool cannot be
safely mounted read-write. It must never fabricate data or silently choose a
stale ambiguous membership.

## 23. Capacity accounting

ARXFS exposes internal and administrator-visible accounting for:

```text
logical_user_bytes
sparse_hole_bytes
unique_pre_protection_bytes
deduped_saved_bytes
compressed_saved_bytes
data_or_replica_bytes
fec_parity_bytes
fec_padding_bytes
critical_metadata_bytes
device_label_bytes
cow_recovery_workspace_bytes
unusable_due_to_topology_bytes
unbalanced_free_bytes
evacuation_required_bytes
physical_allocated_bytes
physical_free_bytes
```

Do not add a public ABI without current callers. The administration service and
both tools are current callers and justify a narrow versioned status response.

For RS:

```text
protected multiplier = (k + m) / k
raw efficiency       = k / (k + m)
```

For replication:

```text
Replica2 efficiency = 1 / 2
Replica3 efficiency = 1 / 3
```

Capacity reporting for mixed profiles must sum actual allocated bytes rather
than pretending the whole pool has one ratio.

The pool must distinguish:

- Raw unclaimed device capacity.
- Eligible protected capacity.
- Space unavailable because the target profile cannot place enough distinct
  shards.
- Space retained for COW/recovery workspace.
- Space needed to complete a requested removal, replacement, rebalance, or
  protection conversion.

For single-device documentation:

```text
Local RS(8+2) overhead over protected unique bytes = 25%
Local RS(8+2) raw efficiency                       = 80%
```

A marketed 1 TB disk is about 931 GiB before filesystem overhead. With mostly
incompressible data and local RS(8+2), expected logical capacity remains roughly
720-740 GiB after metadata/auth overhead. A typical game-heavy desktop with
modest compression/dedupe/sparse savings may see roughly 750-820 GiB logical.
These are documentation estimates, not acceptance numbers.

Multi-device documentation must show worked examples, including at least:

```text
2 x 1 TB, Replica2: roughly one device worth of protected raw payload
3 x 1 TB, Replica3: roughly one device worth, survives two device losses
4 x 1 TB, RS(2+2): roughly two devices worth, survives any two device losses
6 x 1 TB, RS(4+2): roughly four devices worth, survives any two device losses
10 x 1 TB, RS(8+2): roughly eight devices worth, survives any two device losses
```

All examples must then subtract real metadata, padding, COW workspace, and
unusable topology space and account for actual compression/dedupe separately.

## 24. Performance requirements

Security and correctness are the floor. Within that floor, unnecessary latency,
copying, allocation, locking, or serial I/O is a defect.

### 24.1 Architectural I/O budgets

The implementation must satisfy these measurable I/O-count invariants:

```text
clean random read:
  one data shard or one replica
  zero parity reads

RS random overwrite:
  one new data shard + m new parity shards
  no unrelated data-shard reads when parity delta inputs are cached/available
  no unrelated segment writes

full RS segment write:
  k data writes + m parity writes
  one parity computation per full segment

profile-preserving relocation:
  sealed ciphertext copied without decrypt/decompress/recompress

rebuild:
  scans allocated live segments, not unused raw address space
```

### 24.2 Parallelism

- Independent shard reads and writes are submitted concurrently.
- Encoding can overlap with device I/O through bounded buffers.
- Device queues are independent; one slow device does not hold a global lock.
- Full-stripe sequential throughput should scale with data-shard devices until
  CPU, compression, crypto, or the slowest required device becomes the measured
  bottleneck.
- There is no dedicated parity-device hot spot.

### 24.3 Foreground latency under background work

The benchmark harness must establish idle foreground baselines and measure the
same workloads while each job type runs.

On deterministic host/mock devices, the default `Balanced` policy must keep:

```text
foreground p95 latency <= 1.20 x the same idle workload
foreground p99 latency <= 1.35 x the same idle workload
```

unless urgent synchronous recovery is required for the requested data itself.
Real NVMe, SATA SSD, HDD, and SD/eMMC measurements are reported separately and
must not be hidden by averaging.

If the initial repository benchmark environment cannot model latency credibly,
retain the exact I/O-count and bounded-queue acceptance rules, add a deterministic
queue-delay fixture in the same stage, and do not substitute unsupported claims.

### 24.4 Clean-path CPU target

On the host benchmark with data resident in the mock block cache, clean random
read CPU time should regress by no more than 10% relative to the same ARXFS
record pipeline before the FEC read-path change. Any larger regression requires
profiling evidence and a fix before final acceptance.

### 24.5 Random-write target

Parity delta must avoid reading all peer data shards for a one-shard overwrite.
The benchmark report separates:

- Cached old data/parity.
- Uncached old data/parity.
- Full-stripe sequential writes.
- Partial compressed-record writes.
- Replica profiles.

### 24.6 Recovery throughput

Rebuild and rebalance report:

- Verified logical and physical bytes per second.
- Device read/write utilisation.
- CPU time in integrity, RS math, crypto, and compression.
- Foreground latency impact.
- Work skipped due to dedupe sharing.
- Work blocked by capacity, device errors, or safety proof.

### 24.7 SIMD

A portable safe Rust implementation is mandatory. SIMD may follow only after the
portable path is correct and measured. Any `unsafe` is encapsulated, documented
with `// SAFETY:`, tested against the portable path, and kept within the target
configuration rules of `AGENTS.md`.

## 25. Reed-Solomon and replication implementation

Use a systematic Reed-Solomon code over a fixed finite field with deterministic
profile tables and known-answer vectors.

**The field and the codec are the existing first-party ones.**
`lib/raid/src/gf256.rs` and the P/Q/R syndrome code already implement GF(2^8)
and Reed-Solomon in this workspace. A second GF(2^8) field or a second RS
codec is forbidden (section 5.1). FEC1 decides the shared home — depend on
`lib/raid` or hoist the field and codec into a `lib/fec` both consumers use —
and updates `AGENTS.md` section 3 and `PLAN.md` in the same change. The
*profile tables*, the protected-segment model, and the placement evaluator are
this plan's own; the arithmetic beneath them is not.

The internal API must support the closed profile set, conceptually:

```text
encode(profile, data[0..k]) -> parity[0..m]
reconstruct(profile, shards, erasures) -> recovered shards
update_parity_delta(profile, data_index, old_data, new_data, old_parity)
verify_parity(profile, data, parity)
```

Replica profiles use the same protected-segment abstraction but do not route
through fake RS math unless `RS(1+m)` is deliberately chosen and proven simpler.
Do not duplicate integrity, placement, or COW logic for mirrors.

Illegal states are rejected:

- Unknown profile id.
- Zero data shards.
- Unsupported `k` or `m`.
- Out-of-range shard index.
- Too many erasures.
- Wrong shard length.
- Duplicate failure-domain placement in a healthy target.
- Missing explicit tail slots.
- Allocation failure.

The math code is `no_std` compatible unless its actual production crate already
justifies otherwise. Production code contains no `unwrap()`, `expect()`, or
`panic!()`.

The placement safety evaluator is separate from the RS matrix code. It reasons
about device/failure-domain sets and profile recoverability; it does not know
plaintext or crypto keys.

## 26. Errors, health states, and logging

Use existing ARXFS error types where semantically exact. Add a variant only
when a current caller distinguishes it.

Required internal conditions include:

```text
bad shard header or integrity tag
block read/write/flush error
unknown or invalid profile
stale shard, label, membership, or placement epoch
duplicate or ambiguous device identity
wrong device for committed shard
too many erasures
RS reconstruction or parity verification failure
final record authentication failure
corrupt segment descriptor
insufficient distinct failure domains
insufficient protected capacity
insufficient COW/recovery workspace
insufficient metadata witnesses
protection floor would be violated
second-failure safety proof failed
device already claimed or mounted
device signature requires destructive confirmation
device too small or incompatible
membership changed during operation
job blocked, paused, cancelled, or stale
NoSpace during repair/evacuation/rebalance
```

Pool health is a closed state model equivalent to:

```text
Healthy
Rebalancing
DegradedButProtected
DegradedAtLimit
ReadOnlySafetyStop
Unrecoverable
```

Do not derive health only from device count; inspect actual segment and metadata
protection.

Security-relevant and topology-changing decisions are logged through `lib/log`
with stable event IDs. Include pool UUID, device UUID, job UUID, generations,
and stable diagnostic codes where safe. Never log keys, plaintext, raw corrupted
payloads, capability tokens, passwords, or unrestricted user paths.

## 27. Administration architecture

### 27.1 Single authoritative service

The mounted ARXFS implementation exposes or is fronted by one versioned
capability-checked administration IPC service. Repository inspection decides the
correct existing service host; do not create a second filesystem authority.

The service owns:

- Pool status and capacity accounting.
- Device discovery/claim preflight through the volume manager's
  storage-discovery path (hardware-tree nodes and their `blkio` endpoints).
- Membership and protection changes.
- Job planning, start, pause, resume, cancel, and status.
- Stable warnings and audit events.
- Safety proofs and final command execution.

The TUI and CLI only collect intent, display plans/status, and invoke this API.
They never calculate authoritative placement or modify raw blocks.

### 27.2 Capabilities

Reuse existing capabilities when their semantics exactly match (`CAP_FS_MOUNT`
already gates volume attach/detach, and the `CAP_SYSINFO_*` family covers
observation-only queries). Otherwise add a minimal versioned capability surface
with current callers in the same change — never a capability minted ahead of
the service that holds and enforces it. The required semantic split is:

```text
read pool status and job progress
administer pool membership/protection/jobs
claim or initialise an unclaimed block device through the volume-manager path
audit-log access, only when displaying audit details
```

A read-only status capability must not permit mutation. Every IPC request
identifies the kernel-supplied caller, checks capability before state access,
validates all fields, logs security-relevant decisions, and fails closed.

### 27.3 Source layout and code reuse

The tools are OS command apps: self-contained `.app` bundles in the system app
store, discovered from disk, each with its own signed `AppInfo`, `Run` binary,
and on-disk `Help/` tree (`AGENTS.md` sections 16.2/16.5, `plans/APPS.md`).
Help is authored in each bundle's `Help/<locale>/` documents and served through
`lib/help` — never hardcoded into a binary. Command-app crates under
`userland/apps/` are single-word command names (`mount`, `df`, `sysmon`, …);
follow that convention. Working names, fixed at implementation:

```text
userland/apps/arxfs/      - scriptable CLI command app
userland/apps/arxfsadmin/ - curses TUI command app
```

Shared client/model/error-rendering logic is written once: two app crates
sharing it means a `lib/*` crate, updating `AGENTS.md` section 3 and `PLAN.md`
in the same change. ABI request/response types live in the existing appropriate
`lib/abi` module. Generic terminal rendering uses existing `lib/curses`,
`lib/termcap`, and `lib/vt`; do not implement another terminal stack.

### 27.4 CLI command surface

The final names must be concise and tested. The initial required shape is:

```text
arxfs pool status [POOL] [--json]
arxfs pool capacity [POOL] [--json]
arxfs pool protection show [POOL]
arxfs pool protection set [POOL] --survive-devices <0|1|2>

arxfs device list [POOL] [--available] [--json]
arxfs device add POOL --device <stable-device-id>
arxfs device remove POOL --device <device-uuid>
arxfs device replace POOL --missing <device-uuid> --with <stable-device-id>

arxfs scrub start POOL
arxfs job list [POOL] [--json]
arxfs job show POOL <job-uuid> [--json]
arxfs job pause POOL <job-uuid>
arxfs job resume POOL <job-uuid>
arxfs job cancel POOL <job-uuid>
arxfs explain <stable-diagnostic-code>
```

Do not use `/dev/...` examples; there is no `/dev`. Pool selectors use
authenticated pool UUIDs or the storage-namespace spellings the binding spec
defines (`id::<volume-id>`, an unambiguous alias), resolved by the service.
Device selectors use pool-member device UUIDs or, for an unclaimed candidate,
the stable identity the storage-discovery path reports (its hardware-tree
node/`blkio` endpoint via the volume manager). Unambiguous human labels may be
accepted and are resolved server-side, failing closed on ambiguity.

Every mutating command performs server-side preflight and displays:

- Current and target topology.
- Current, requested, and target effective protection.
- Whether another whole-device failure can be survived now and during the job.
- Bytes to move/re-encode.
- Required free/protected workspace.
- Expected capacity change.
- Devices and failure domains touched.
- Foreground-performance impact and selected job priority.
- Any irreversible step.

Interactive execution requires explicit confirmation. Non-interactive execution
requires an explicit `--yes` or equivalent existing convention and still runs
the same preflight. A `--dry-run` or plan-only mode must produce no mutation.

`--json` output has a versioned schema, stable enum/string codes, byte counts as
integers, and no ANSI escapes. Human text is not a parsing API.

### 27.5 Standard streams and structured warnings

Follow the repository's standard-I/O contract:

- Requested tabular or JSON data goes to stdout.
- Errors go to stderr.
- Structured warnings, omissions, consequences, and safe next actions use the
  existing `stdinfo` channel (fd 3) with the closed canonical record kinds
  (`omission`, `summary`, `schema`, `suggestion`, `context`) and stable codes
  where required.
- Progress is rate-limited and does not spam `stdinfo`.
- Secrets, raw capability tokens, and untrusted instructions never appear.

Stable warnings include at least:

```text
pool.cannot_survive_another_device_failure
pool.protection_floor_not_met
pool.mixed_placement_epochs
pool.temporary_same_domain_shards
pool.insufficient_evacuation_capacity
pool.replacement_required
pool.device_identity_ambiguous
pool.device_contains_existing_signature
pool.device_smaller_than_source
pool.job_blocked_by_new_failure
pool.background_io_affecting_foreground
pool.protection_downgrade
```

### 27.6 Curses TUI

`arxfsadmin` is keyboard-complete, obvious without a manual, and usable on
truecolour, 256-colour, 16-colour, monochrome, and `TERM=dumb` fallbacks through
`lib/curses`/`termcap`.

Required views:

1. **Overview** - pool health, effective protection, capacity, whether another
   device can fail, active warnings, and current foreground/background load.
2. **Devices** - stable identity, model/serial when available, size, failure
   domain, state, error counters, allocation, and role distribution.
3. **Protection** - requested/effective floor, profile distribution, mixed
   epochs, metadata copy health, and plain-language explanation.
4. **Jobs** - progress, verified bytes, measured throughput, priority, blocking
   reason, pause/resume/cancel actions.
5. **Capacity** - logical, compressed, deduped, sparse, parity/replica,
   metadata, workspace, topology-unusable, and evacuation estimates.
6. **Events** - recent relevant stable event codes through permitted audit/status
   APIs, never a raw unrestricted log reader.
7. **Help** - context-sensitive keys and explanations for every warning/action.

The TUI must not rely on colour alone. Use words, icons from the terminal
vocabulary where safe, and accessible focus/order. Mouse support is optional;
every action works by keyboard.

Destructive or protection-lowering actions use a two-step confirmation view that
shows the exact pool/device identity and consequences. The final confirmation
requires an explicit action distinct from ordinary navigation.

The dashboard must make these answers immediately visible:

```text
Is my data currently protected?
How many whole devices may fail now?
Can another device fail while this job runs?
Which device needs attention?
Is the filesystem still writable?
How much protected capacity remains?
What is ARXFS doing, and can I safely pause it?
```

### 27.7 Admin testing requirements

The CLI and TUI are tested against an injected fake administration service and
real integration pools.

Tests include:

- Capability denial before state access.
- Invalid/ambiguous device selector rejection.
- Dry-run performs no mutation.
- Confirmation required for destructive initialisation and protection lowering.
- JSON schema/golden tests.
- No ANSI escapes in JSON.
- Stable exit codes and diagnostic codes.
- TUI navigation and action tests on monochrome and reduced terminal
  capabilities.
- Resize, narrow-screen, and long-label truncation without panic.
- Keyboard-only operation.
- New failure arriving while a confirmation dialog is open causes plan
  invalidation and re-preflight, not execution of a stale plan.

## 28. Implementation stages

Each stage is intended to be a manageable AI task. Each stage lands as one
complete, reviewable change with production use, tests, rustdoc, user/developer
docs, and the full workspace validation gate required by `AGENTS.md`.

A stage must not add unused public surface, stubs, `todo!()`, ignored tests,
compatibility shims, duplicate algorithms, or hand-edited generated files. If a
stage would otherwise leave dead code, combine it with the next stage.

Every stage begins with repository-verified assumptions and ends with an
adversarial self-review under `AGENTS.md` section 23.

### FEC0 - Source-of-truth and plan integration

**Blocked until spec stage 17 lands** (`plans/ARXFS-WRITEBACK.md`): every
distributed commit witness in section 16 requires the commit barrier that stage
adds, and section 2 records that an ordinary ARXFS commit issues none today.

Deliverables:

- Update `PLAN.md`'s and the spec's "one mandatory profile" wording so it
  means all safety features remain mandatory while ARXFS may select from a
  closed, topology-derived profile set. Do not add raw user-selected `k+m`
  options.
- Add the high-level single- and multi-device model, protection-floor
  terminology, and mandatory invariants to `docs/src/filesystem/arxfs-spec.md`
  as its next free section, and tick the section 18 stage-21 row.
- Re-confirm section 2 against the tree and correct it where the tree has moved.
  Section 2 is the record of that check; it is not re-derived per stage.
- State the section 5.1 boundary in the spec too, so a reader of the spec alone
  cannot mistake ARXFS FEC for a replacement for `lib/raid`.
- Record unresolved source conflicts explicitly and stop rather than guessing.

Tests:

- Documentation build, link checks, stale-symbol checks, and the full workspace
  gate despite this being a planning/docs stage.

Acceptance:

- No historical landing log in `PLAN.md`.
- No generated file edited by hand.
- No implementation begins until source-of-truth conflicts are resolved.

### FEC1 - Closed profile engine and FEC mathematics

**First decision of this stage:** the shared home for the existing GF(2^8) field
and Reed-Solomon codec (section 5.1, section 25) — depend on `lib/raid` or hoist
into `lib/fec`. A second copy of the arithmetic is forbidden either way.

Deliverables:

- Implement the first-party systematic RS engine required for `k=2..8` and
  `m=1..2` using one stable field/matrix construction.
- Implement parity encode, reconstruction, verification, and parity delta.
- Define and validate the closed profile table, including local `RS(8+2)` and
  replica-profile descriptors.
- Keep code private to ARXFS unless a second production crate requires the math
  in the same change. If creating `lib/fec`, update `AGENTS.md`, `PLAN.md`,
  workspace membership, README stability tier, rustdoc, docs, and tests.
- Wire the engine to a current ARXFS formatter or segment-construction call site
  so it is not speculative dead code.

Tests:

- Known-answer vectors for every supported RS dimension.
- Encode determinism.
- Every one-shard erasure for every profile.
- Every two-shard erasure for every `m=2` profile.
- Refusal beyond `m` erasures.
- Every data index parity-delta equivalence to full recompute.
- Wrong length, unknown profile, duplicate index, and invalid tail rejection.
- Deterministic property tests over random shard images.
- OOM/failure paths return errors without panic where host-testable.

Docs:

- Field, matrix, shard ordering, and profile table in `arxfs-spec.md`.

### FEC2 - Single-device on-disk segment and shard format

Deliverables:

- Extend the current ARXFS format in place with protected-segment descriptors,
  local `RS(8+2)` shard headers, profile ids, generations, and keyed integrity.
- Bind segment/shard identity to final record authentication or prove equivalent
  existing binding.
- Preserve existing local metadata mirroring/triplication.
- Delete/regenerate obsolete no-FEC fixtures.

Tests:

- Descriptor/header encode/decode.
- Fail-closed malformed profile, index, role, length, generation, and padding.
- Stale, cross-segment, cross-volume, and wrong-index replay rejection.
- Mount rejects malformed protected metadata without panic.
- Fuzz the new decoders.

Docs:

- Current on-disk format diagrams and field bounds.

### FEC3 - Single-device format and sequential write path

Deliverables:

- Fresh format creates local FEC-enabled ARXFS only.
- Sequential writes pack sealed records into `LocalRs8_2` segments.
- Tail segments use explicit authenticated zero-padded slots.
- Parity writes are COW and durable before root commit.
- Sparse holes allocate no protected payload.

Tests:

- Record counts 1, 7, 8, 9, 16, and 17.
- Correct parity and segment counts.
- Sparse file allocation and readback.
- Crash before/after commit.
- `NoSpace` during data/parity write preserves old contents.

Docs:

- User docs state local FEC is mandatory and automatic.

### FEC4 - Single-device clean and recovery reads

Deliverables:

- Clean reads use one data shard and no parity.
- Bad/missing shards become erasures.
- Recovery reconstructs sealed ciphertext, verifies final auth, then decrypts
  and decompresses.
- Read-only mounts recover only in memory.

Tests:

- Mock block device proves zero parity reads on a clean random read.
- Bit flip, full-sector zero, read error, one data plus one parity loss, and two
  data losses recover.
- Three losses fail closed.
- Final auth failure returns no plaintext.
- Corrupt parity is detected and not trusted.

Docs:

- Troubleshooting and read-repair behaviour.

### FEC5 - Single-device random-write parity delta

Deliverables:

- Full-shard overwrite writes one data plus two parity shards, segment-local.
- Partial writes follow authenticate/decrypt/decompress/patch/recompress/reseal.
- Changed compressed length remains local through fixed padded images.
- Dedupe overwrite remains immutable/COW.
- Dirty writes in one segment may coalesce through the same parity engine.

Tests:

- One-block overwrite in a 1000-block file touches one segment.
- Exact payload I/O count through mock storage.
- Partial write with same and changed compressed length.
- Dedupe reference remains unchanged after sibling overwrite.
- Crash at every random-write commit point returns old or new state.
- Full parity recompute matches committed delta parity.

Docs:

- Developer random-write cost and locality note.

### FEC6 - Single-device placement lanes

Deliverables:

- Integrate deterministic shard-index lanes/regions into the existing allocator.
- Preserve sequential batching.
- Use the same policy for tail segments and repairs.
- Document fallback when device topology exposes no erase-region information.

Tests:

- Mock allocator verifies expected separation and deterministic fallback.
- Lane `NoSpace` aborts the transaction cleanly.
- Repair follows lane policy.
- Sequential benchmark records batching.

Docs:

- Allocator/placement section.

### FEC7 - Single-device scrub, check, rescue, and repair

Deliverables:

- Online scrub verifies descriptors, shard tags, parity, final auth, and dedupe
  safety.
- Scrub COW-repairs up to two erasures.
- Offline check/rescue reuses the same parser/decoder/verification code.
- Writable read repair persists; read-only does not.

Tests:

- Bad data, bad parity, two erasures, and unrecoverable three-erasure cases.
- Offline damaged-image recovery.
- Read-only versus writable repair.
- Corruption-injection corpus covering headers, descriptors, generations,
  parity, data, and final auth.

Docs:

- Operator scrub/check/rescue messages.

### FEC8 - Single-device capacity accounting and diagnostics

Deliverables:

- Account logical, sparse, compressed, deduped, parity, padding, metadata,
  workspace, and physical bytes.
- Extend existing ARXFS diagnostics without unnecessary ABI.
- Add measured capacity fixtures and documentation estimates.

Tests:

- Incompressible data shows approximately 25% parity over protected unique
  bytes plus measured metadata.
- Sparse holes add zero parity.
- Dedupe hits share protection.
- Compression savings occur before parity.
- Tail padding is separately accounted.

Docs:

- 1 TB desktop/gaming explanation with clear estimate caveats.

### FEC9 - Single-device performance and acceptance baseline

Deliverables:

- Bench clean reads, recovery reads, sequential writes, random full-record and
  partial writes, scrub, and rescue.
- Add FEC metadata/math fuzz harnesses and regression corpus.
- Establish the baseline used by later multi-device stages.

Tests:

- All existing ARXFS POSIX, fssoak, QEMU, crash, and fuzz suites remain green.
- Exact I/O-count invariants pass.
- Clean-path CPU target is measured.
- Full workspace validation gate passes.

Docs:

- Baseline tables in developer docs, not marketing claims.

### FEC10 - Pool model, profile selector, and safety evaluator

Dependencies: FEC1-FEC9.

Deliverables:

- Add pool UUID, device UUID, failure-domain, membership generation, placement
  epoch, requested/effective floor, and closed device-state types.
- Implement deterministic topology-to-profile selection for floors one and two.
- Implement the exact second-failure safety predicate.
- Wire the selector/evaluator into the production multi-device format planner.
- Keep physical-device domains as the only claimed initial domain type while
  retaining format fields for future domain hierarchy.

Tests:

- Topologies from 1 through the maximum supported device count.
- Every legal/illegal floor/topology combination.
- Every single device loss for floor one.
- Every pair of device losses for floor two.
- Duplicate-domain placement rejection.
- Temporary same-domain recovery placement accepted only when exact proof passes.
- Stateful property model for membership/profile/effective-floor truthfulness.

Docs:

- Protection floor and profile-selection table.

### FEC11 - Multi-device labels, import, metadata copies, and commit witnesses

Dependencies: FEC10 and existing multi-block-device test seam.

Deliverables:

- Authenticated per-device label rings.
- COW pool membership/configuration metadata.
- Critical metadata copies on `floor + 1` distinct domains.
- Distributed commit witnesses and highest-valid-root import.
- Stale/duplicate/reappeared device quarantine.
- Multi-device formatter and read-only import path over injected block devices.

Tests:

- Device enumeration order independence.
- Loss of any allowed label/metadata devices still imports acknowledged root.
- Crash during every label/membership/witness write returns old or new state.
- Partially written unacknowledged root never creates mixed state.
- Stale higher/lower generation labels are handled fail-closed.
- Duplicate device UUID and cloned-device image rejection.
- No special first disk.
- Fuzz label/config/root decoders.

Docs:

- Pool label, import, metadata witness, and cloning warnings.

### FEC12 - Multi-device placement and mounted data path

Dependencies: FEC11.

Deliverables:

- Create and mount healthy multi-device pools.
- Place one shard per physical failure domain for healthy layouts.
- Implement `Replica2`, `Replica3`, and required `RS(k+1)`/`RS(k+2)` data paths.
- Rotate data/parity roles and weight placement by eligible capacity.
- Parallelise independent shard I/O.
- Use pool-wide coding only; no local FEC beneath pool segments.

Tests:

- Two-, three-, four-, six-, ten-, and twelve-device mock pools.
- Clean reads issue one device read.
- Random writes issue exact one-plus-parity or replica counts concurrently.
- Every allowed device-loss set recovers.
- Sector corruption plus device loss consumes the correct erasure budget.
- Role distribution remains balanced.
- Mixed-size capacity accounting is truthful.
- QEMU multi-virtio-blk mounted read/write vertical.

Docs:

- Multi-device creation, topology examples, and no-stacked-FEC rationale.

### FEC13 - Persistent job engine and foreground-aware scheduler

Dependencies: FEC12, and the shared pacer and class-keyed background budget
`plans/ARXFS-MAINTENANCE.md` M0 hoists (section 21.2 - this engine consumes
them, it does not restate the duty arithmetic).

Deliverables:

- Persistent bounded job records and segment-granular checkpointing.
- Closed job kinds and priority modes.
- Foreground-aware per-device/pool scheduling without sleep loops.
- Pause/resume/cancel rules.
- Use the engine for scrub and a production rebalance/no-op balancing consumer so
  no surface is unused.

Tests:

- Crash/reboot resumes at the last committed work unit.
- Source generation conflict re-evaluates one segment without unbounded retry.
- Pause at boundary, resume after membership revalidation.
- Cancel semantics for scrub/rebalance.
- Queue-delay fixture proves p95/p99 policy targets.
- No starvation under foreground plus multiple jobs.
- Bounded memory/in-flight I/O under large pools.

Docs:

- Job lifecycle and performance policy.

### FEC14 - Live device add and hotplug integration

Dependencies: FEC13 and the repository's runtime block-device discovery path.

Deliverables:

- Add-device preflight through the stable identity the volume manager's
  storage-discovery path reports for the candidate device.
- Destructive-signature confirmation contract.
- `Joining` labels, metadata-copy establishment, `Active` epoch commit.
- Explicit combined add-and-floor-one plan when activating a second device for a
  former one-device floor-zero pool; no unsupported multi-device floor-zero
  state.
- Immediate use for new writes after activation under the target protection.
- Truthful effective floor until all old local-only segments are converted.
- Automatic throttled rebalance/profile conversion when beneficial.
- Crash-safe handling of device disappearance during join.

Tests:

- Add to one-, two-, three-, and wide pools while foreground I/O continues.
- One-to-two add requires explicit floor-one confirmation, uses target profiles
  for new writes, and reports effective floor zero until conversion completes.
- Adding a third device does not silently raise floor one to floor two.
- Foreign signature refusal without confirmation.
- Already claimed/mounted device refusal.
- Duplicate identity rejection.
- Crash at every join transition.
- New failure during add re-preflights and either completes safely or blocks.
- New allocations use new epoch; old segments remain readable.
- QEMU hot-add where supported, injected hotplug elsewhere.

Docs:

- Live add workflow and warnings.

### FEC15 - Live clean remove and evacuation

Dependencies: FEC14.

Deliverables:

- Whole-pool removal preflight for target floor, capacity, metadata, and
  workspace.
- `Evacuating` epoch fence.
- Online movement/re-encoding of all regular data and critical metadata.
- Zero-reference proof and final retirement commit.
- Safe cancellation before retirement.
- Conversion to failed-device recovery if the source disappears mid-evacuation.

Tests:

- Remove from four-, six-, ten-, and wider healthy pools with floor preserved.
- Refuse three-to-two removal while floor remains two.
- Two-to-one removal requires an explicitly confirmed atomic floor-one to
  floor-zero transition and converts all remaining data to local RS(8+2).
- Capacity shortfall refusal before mutation.
- Foreground write races update source generation safely.
- Crash at every evacuation/retirement checkpoint.
- Device failure at every evacuation percentage.
- Cancel leaves a valid mixed-epoch pool.
- Retired device is not required for import.

Docs:

- Live removal, cancellation, and target-capacity explanation.

### FEC16 - Failed-device replacement and second-failure campaign

Dependencies: FEC15.

Deliverables:

- Persistent replacement relation between missing and new device UUIDs.
- Early critical-metadata re-replication.
- Segment-by-segment COW reconstruction with pre/post safety proof.
- Safe use of replacement, spare domain, or proven distributed capacity.
- Quarantine of a stale old device that reappears.
- Exact blocked state when no safe destination exists.

Tests:

- Fail one device, begin rebuild, then fail every possible second device at
  every meaningful checkpoint.
- A degraded floor-two six-device pool writes new `RS(3+2)` segments safely.
- A degraded floor-two four-device pool writes new `Replica3` segments safely.
- A degraded floor-two three-device pool refuses ordinary writes with
  `ReadOnlySafetyStop` until topology or protection changes.
- Crash before destination write, after write, before descriptor commit, and
  after descriptor commit.
- Larger and safely smaller replacement.
- Insufficient capacity blocks without weakening existing recoverability.
- Replacement device fails during rebuild.
- Old device reappears before and after replacement completion.
- Import after any allowed failure set selects a recoverable root.
- QEMU unplug/replug multi-device vertical.

Docs:

- Degraded states, replacement workflow, and two-device impossibility warning.

### FEC17 - Online protection changes and full rebalance

Dependencies: FEC16.

Deliverables:

- Live semantic floor raise/lower operations.
- Requested versus effective floor state.
- Online conversion among the closed replica and RS profiles.
- Restoration of distinct-domain placement after emergency recovery.
- Capacity and performance preflight.
- Explicit, separate high-severity acknowledgement for a downgrade.

Tests:

- Two-to-three devices then floor one-to-two upgrade.
- Three-device floor-two to floor-one downgrade followed by live removal to two.
- Effective floor remains old during upgrade until every object is compliant.
- Effective floor lowers before first lower-protection segment commits.
- Crash and second failure during every conversion direction.
- Dedupe object converts once.
- Sealed data moves without plaintext path when profile unchanged.
- Mixed profiles import, read, scrub, and resume conversion.

Docs:

- Protection semantics versus raw copies/parity.

### FEC18 - Versioned administration service and the `arxfs` CLI

Dependencies: FEC14-FEC17.

Deliverables:

- Narrow versioned `lib/abi` request/response types used by current clients.
- Capability-checked ARXFS administration service methods.
- Pool status, capacity, device, protection, and job operations.
- Stable plan/warning/diagnostic records.
- The `arxfs` CLI command app with human, JSON, dry-run, confirmation,
  exit-code, stdout, stderr, and `stdinfo` behaviour from section 27.
- No direct raw-device access in the CLI.

Tests:

- ABI encode/decode, hash/drift checks, bounds, and fuzzing.
- Capability denial before state access.
- Every CLI command happy path and refusal path.
- Golden JSON schema and stable codes.
- Interactive/non-interactive confirmation.
- Plan invalidation on topology change.
- No ANSI in JSON and no secrets in any stream.
- End-to-end CLI over a live QEMU multi-device pool.

Docs:

- `docs/src/filesystem/arxfs-admin.md` and utilities documentation.

### FEC19 - `arxfsadmin` curses TUI

Dependencies: FEC18 and existing `lib/curses` stack.

Deliverables:

- Overview, Devices, Protection, Jobs, Capacity, Events, and Help views.
- Keyboard-complete navigation and actions.
- Context-sensitive explanations and stable warning codes.
- Two-step destructive/protection-lowering confirmations.
- Monochrome and reduced-capability rendering.
- Client/model/error logic shared with the `arxfs` CLI through their common
  crate; no duplicate pool policy.

Tests:

- Injected fake-service screen-model tests.
- Every action and confirmation path.
- Narrow, wide, resized, monochrome, and `TERM=dumb` cases.
- Long labels, large device counts, and large byte values.
- Keyboard-only accessibility.
- Stale plan invalidation while dialog is open.
- Headless image build includes and runs the TUI without `userland/gui`.

Docs:

- Key map, screen descriptions, warning glossary, and recovery walkthroughs.

### FEC20 - Final multi-device fault, performance, fuzz, and acceptance gate

Dependencies: FEC0-FEC19.

Deliverables:

- Multi-device crash/failure soak with deterministic seeds.
- Fault injection for device loss, sector errors, stale devices, metadata-copy
  loss, witness loss, and job interruption.
- Performance report for single device; 2, 3, 4, 6, 10, and 12 devices; clean
  and damaged reads; random and sequential writes; rebuild; evacuation;
  rebalance; protection conversion; CLI/TUI status overhead.
- Fuzz harnesses for every new public ABI decoder and untrusted persistent
  parser.
- Final docs and concise `PLAN.md` status update.

Tests and gate:

- `cargo fmt --all` and `cargo fmt --all --check`.
- Full `cargo xtask ci` over the entire workspace.
- `cargo xtask fuzz --secs 5`.
- Every additional command in `.github/workflows/ci.yml`.
- Existing ARXFS POSIX, fssoak, QEMU, crash-replay, corruption-injection, and
  fuzz suites.
- Multi-device QEMU verticals with device removal during active rebuild.
- Final adversarial `AGENTS.md` section 23 self-review.

Acceptance:

- Actual command output is quoted in the completion report.
- Any discovered failure or defect is fixed with a regression test in the same
  change, or the change is not complete.

## 29. Required test matrix summary

The completed work includes at least the following coverage.

```text
FEC/profile math:
  known vectors for every profile
  all one-erasure cases
  all two-erasure cases for floor-two profiles
  refusal beyond protection
  parity-delta equivalence
  deterministic property tests

persistent format:
  labels, pool config, descriptors, shard headers, witnesses, job records
  malformed and over-bound rejection
  stale/cross-pool/cross-segment/wrong-device replay rejection
  fuzzing for every decoder

single-device:
  sequential/tail writes
  sparse and dedupe
  clean read no parity
  random-write locality
  crash and NoSpace
  scrub/check/rescue

pool import/metadata:
  no special first disk
  enumeration-order independence
  allowed label/metadata device loss
  acknowledged root survives floor failures
  unacknowledged crash returns old or new complete state
  duplicate/cloned/stale device quarantine

placement/data path:
  replica and every RS profile
  one shard per healthy failure domain
  balanced data/parity roles
  exact clean-read and random-write I/O counts
  device loss plus local sector corruption
  mixed-size and wide-pool placement

live add/remove:
  hot-add while busy
  destructive preflight
  crash in every state transition
  remove with profile conversion
  insufficient capacity refusal
  cancel evacuation
  source failure during evacuation

replacement and second failure:
  one failed device plus every possible second failure
  second failure before/after each segment commit
  replacement failure
  stale old device reappearance
  blocked-no-capacity state preserves safety

protection conversion/rebalance:
  requested versus effective floor
  raise and lower
  mixed profiles and epochs
  dedupe relocation once
  no plaintext path for profile-preserving move
  crash/failure resume

job engine/performance:
  bounded queues and memory
  foreground p95/p99 policy
  no starvation
  persistent resume
  pause/cancel semantics
  live-segment scan rather than raw-disk scan

administration:
  capability gates
  plan/dry-run/confirmation
  stable JSON, codes, exit statuses, and streams
  no /dev assumptions
  TUI keyboard/monochrome/resize/accessibility
  stale plan invalidation

end to end:
  QEMU multi-virtio create, mount, add, remove, fail, replace, rebuild
  fail another device during rebuild
  reboot during every long-running operation
  full workspace gate
```

## 30. AI implementation prompt

An implementation agent may use this prompt:

```text
You are implementing the next stage of `plans/ARXFS-FEC.md` for TAIRiX.
Before coding, read `AGENTS.md`, `PLAN.md`,
`docs/src/filesystem/arxfs-spec.md`, `docs/src/filesystem/arxfs.md`,
`docs/src/filesystem/drives.md`, `plans/DEVICES.md`, this plan, and all
relevant ARXFS, block-service (`lib/abi` blkio/volume), volmgr, IPC, sysinfo,
and curses code. There is no /dev, /proc, or /sys: block devices are reached
through blkio block-service endpoints on storage-class hardware-tree nodes,
claimed and attached by the volume manager under CAP_FS_MOUNT, and volume
roots are published as id:: roots and aliases.
State the assumptions verified from the repository, including the current
compression layer, dedupe limit, record size, integrity/AAD fields, metadata
replication, block flush/hotplug/device-identity semantics, and existing admin
capabilities/API seams.

Implement only a stage that can land as a complete, tested, documented
production change. Do not add stubs, todo!(), ignored tests, dead code,
compatibility readers, no-FEC flags, arbitrary k+m settings, duplicate math,
hand-edited generated files, direct /dev assumptions, or external FEC crates.
If a public interface is needed, add only the narrow versioned interface used
by current callers in the same change. If creating lib/fec, update AGENTS.md
section 3, PLAN.md, workspace membership, README stability tier, rustdoc, docs,
and tests in that change.

Mandatory design:
- One device uses local systematic RS(8+2) over sealed ciphertext.
- Multi-device pools use one redundancy layer across distinct physical failure
  domains: Replica2/Replica3 or topology-selected systematic RS(k+1)/RS(k+2),
  from a closed table.
- The user-facing contract is survive N whole-device failure domains, not raw
  copies or parity dimensions.
- Compression, sparse handling, and dedupe remain before encryption/FEC.
- Final record authentication succeeds before any decrypt/decompress/return.
- Clean reads do not read parity.
- Random writes are segment-local and use parity delta.
- Metadata has floor+1 authenticated copies on distinct domains.
- Every add/remove/replace/rebalance/protection change is online, COW,
  checkpointed, and generation-fenced.
- With effective floor two and one device already missing, every committed
  repair/reorganisation state must survive any one further active-device loss.
- If a safe destination cannot be proven, block the job without weakening the
  pool.
- CLI and curses TUI use one capability-checked administration service and
  never write raw devices themselves.
- Performance is measured: parallel shard I/O, bounded background queues,
  foreground latency protection, and no decrypt/recompress for a
  profile-preserving relocation.

Finish by running the full workspace gate from the repository root:
`cargo fmt --all`, `cargo fmt --all --check`, `cargo xtask ci`,
`cargo xtask fuzz --secs 5`, and anything else `.github/workflows/ci.yml`
runs. Quote actual output and state the AGENTS.md section 23 verdict. Any defect
discovered is fixed with a regression test before reporting completion.
```

## 31. Acceptance checklist

This specification is complete only when all applicable statements are true.

### Data protection

- ARXFS formats only protected native volumes.
- One-device pools use local RS(8+2).
- Multi-device pools use one pool-wide redundancy layer.
- No healthy segment has two shards in one required failure domain.
- Replica and RS profiles are closed, deterministic, and authenticated.
- FEC/replication operates over sealed ciphertext.
- Final record authentication precedes plaintext use.
- Sparse holes allocate no protected payload.
- Compression and dedupe precede protection and remain COW-correct.
- Clean random reads read no parity.
- Random writes are segment-local and parity delta is implemented.
- COW prevents parity, membership, and relocation write holes.

### Pool and metadata

- Device identity is UUID-based and independent of enumeration/path.
- No device is indispensable for import.
- Critical metadata has at least effective floor plus one copies on distinct
  domains.
- Acknowledged roots retain a witness after every allowed failure set.
- Stale, cloned, duplicate, and reappeared devices are quarantined fail-closed.
- Requested and effective protection are reported separately and truthfully.
- Mixed profiles/placement epochs remain one current supported format.

### Live operation

- Devices can be added while mounted.
- Healthy devices can be evacuated and removed while mounted when target safety
  and capacity permit.
- Failed devices can be replaced while mounted.
- Rebalance and profile conversion are online, persistent, resumable, and
  segment-granular.
- A disk removal never silently lowers protection.
- A protection downgrade is separate, explicit, and highly visible.
- A topology change invalidates stale plans before execution.
- Crash at every state transition recovers a complete old or new state.

### Second-failure safety

- A floor-two pool with one missing domain remains recoverable after any one
  additional domain failure at every committed repair/reorganisation step.
- Old descriptors remain authoritative until complete verified destinations
  commit.
- New failures cause jobs to stop and re-plan against the new membership.
- Insufficient safe capacity blocks work without weakening existing protection.
- The unavoidable two-device limitation is displayed plainly.

### Performance

- Independent shard I/O is parallel.
- There is no dedicated parity disk.
- Sequential parity is calculated once per full segment.
- Rebuild scans live allocated segments, not raw unused space.
- Profile-preserving moves do not decrypt, decompress, or recompress.
- Background work is bounded, checkpointed, foreground-aware, and starvation
  free.
- Clean-path CPU, exact I/O counts, foreground latency, rebuild throughput, and
  topology conversions are benchmarked and documented.

### Administration

- One versioned capability-checked service owns pool administration.
- The `arxfs` CLI and `arxfsadmin` TUI use that service and share
  client/model logic.
- The CLI has human and versioned JSON output, dry-run, explicit confirmation,
  stable codes, and correct stdout/stderr/stdinfo use.
- The TUI uses the existing curses stack, has no GUI dependency, is
  keyboard-complete, works in monochrome, and does not rely on colour alone.
- Both tools immediately show current protection, whether another disk may fail,
  writeability, capacity, jobs, blocking reasons, and safe next actions.
- Neither tool accepts Linux `/dev` paths; selectors are the storage-namespace
  and stable-identity forms (pool/device UUIDs, `id::` roots, unambiguous
  aliases).
- Destructive actions and protection lowering are never one-key accidents.

### Quality gate

- All public items have rustdoc.
- Relevant `docs/src/` pages and `PLAN.md` are current and concise.
- Obsolete fixtures/code/docs are deleted or regenerated.
- All new parsers and ABI decoders are fuzzed.
- Every fixed defect has a regression test.
- The entire workspace validation gate is green and its actual output is quoted
  in the completion report.
