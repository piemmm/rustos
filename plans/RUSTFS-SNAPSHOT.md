# RUSTFS-SNAPSHOT.md — RustFS snapshot support design brief

This file is an AI-facing design brief for generating the binding RustFS
snapshot specification. It is **not** the final spec. Use it as source material
to produce the snapshot section of `docs/src/filesystem/rustfs-spec.md` (a new
`§20 Snapshots`), the `docs/src/filesystem/rustfs.md` overview update, the ABI
additions in `lib/abi`, the new `rustfs` tooling, the required tests, and the
exact `AGENTS.md` / `PLAN.md` amendments. It is binding under `AGENTS.md`: every
rule in this brief is subordinate to the charter, and where the charter and this
brief disagree the charter wins (stop and ask, charter §15.7).

Snapshots are designed first as a **correctness/retention feature** and second
as the **foundation of a future backup solution**. Every on-disk and ABI choice
below is made so that a backup service can be built on top without redesigning
snapshots. Keep that consumer in mind throughout: a snapshot must be a stable,
self-consistent, integrity-verified, addressable point-in-time root that a
backup tool can name, diff, stream, and verify.

---

## 1. Purpose and scope

RustFS is already fully copy-on-write with a superblock ring, retained
transaction-root history, reflink/shared immutable chunks, refcounts, and a
reverse-reference tree (rustfs-spec §4, §9, §14). Those are exactly the
primitives a snapshot is built from; this brief specifies the user-visible
snapshot feature on top of them. It does **not** invent a second COW mechanism.

A **snapshot** is a named, read-only, persistent, integrity-verified reference
to a committed RustFS root (a whole-volume point-in-time image of the root tree
and everything reachable from it). It pins every block reachable from that root
against reuse and discard, survives mount/unmount and power loss, and can be
listed, mounted read-only, diffed against another snapshot, streamed, and
deleted under capability control.

In scope: snapshot on-disk model, lifecycle, naming, capabilities, mount/read
semantics, interaction with COW/refcount/dedupe/reflink/TRIM/scrub/check/rescue,
the diff/send/receive primitives a backup solution needs, space accounting,
failure modes, tests, and charter amendments.

Out of scope (named so they are not assumed): writable/cloned snapshots,
cross-volume replication policy, the backup *service* itself (a separate plan),
and any retention *policy engine* (the spec provides mechanism, not schedule).

## 2. Non-negotiable invariants

- A snapshot is **read-only and immutable** once created. There is no writable
  snapshot in v1; a writable clone, if ever wanted, is a separate future
  feature built on reflink, not a mutation of a snapshot.
- A snapshot **never changes the bytes a live file sees**. Creating one is
  metadata-only: it pins an existing committed root; it copies no data.
- A snapshot is a **retained root** for the purposes of the existing safety
  rules: TRIM/discard and block reuse must treat every block reachable from a
  snapshot root as live (rustfs-spec §11, §14). This is the rule already
  alluded to in the spec's TRIM reachability list — this brief makes it real.
- Snapshot metadata is held to the **same integrity, redundancy, encryption,
  and authentication** rules as all other authoritative metadata (rustfs-spec
  §5, §7, §8): self-identifying, checksummed/authenticated, two physical
  copies, encrypted, no plaintext.
- All snapshot operations are **capability-checked and fail closed** (charter
  §5.4): no ambient authority, caller identity is kernel-attested, every input
  validated, security decisions logged with a stable event ID (charter §19.4).
- Snapshot operations are **crash-consistent**: a snapshot is created, renamed,
  or deleted by one atomic COW transaction; a crash mid-operation leaves either
  the prior state or the new state, never a torn snapshot (rustfs-spec §14).
- Snapshots **scale**: the snapshot index is a paged on-disk tree, never an
  in-RAM whole-volume resident structure, so thousands of snapshots on a
  100 TB+ volume cost working-set RAM, not volume-proportional RAM (charter
  §24, §26.6, §26.7).
- Production errors are `Result`-based, never panics (charter §2.9).

---

## 3. On-disk model

Snapshots build on the existing authoritative metadata (rustfs-spec §4). Two
new authoritative structures are added; both are COW, two-copy, authenticated,
and encrypted exactly like every other metadata tree.

### 3.1 The snapshot tree

A new authoritative **snapshot tree** is named from the transaction root
alongside the inode tree, extent tree, chunk/refcount tree, and reverse-ref
tree:

```text
superblock ring
  -> recent transaction roots
      -> root tree
      -> inode tree
      -> extent tree
      -> chunk/refcount tree
      -> reverse-reference tree
      -> free-space tree
      -> snapshot tree        (new — authoritative)
      -> device-health tree
      -> rebuildable secondary indexes
```

The snapshot tree is a COW B-tree (reusing the single generic node
implementation in `drivers/filesystem/rustfs/src/btree.rs`, charter §2.2 — no
second tree implementation) keyed by a stable 64-bit **snapshot id**
(monotonic, never reused within a volume). Each leaf is a `SnapshotRecord`:

```text
SnapshotRecord:
    snapshot_id            u64        stable, monotonic, never reused
    root_pointer           on-disk address of the pinned committed root
    root_generation        u64        the transaction generation it pins
    name                   1..=255 bytes (same name rules as dir entries, §13)
    created                Time64
    parent_snapshot_id     u64        SNAPSHOT_NONE (0) or the base it descends from
    root_digest            32-byte logical digest over the pinned root metadata
    flags                  bitset (e.g. AUTOMATIC, HELD, BACKUP_SOURCE)
    metadata-integrity/authentication fields inherited from RustFS metadata
```

`root_pointer` + `root_generation` together name an immutable committed root.
`root_digest` lets a backup/verify tool name a snapshot by content, not just by
id, and lets `rustfs scrub`/`check` detect a snapshot root that no longer
authenticates. `name` is held in the encrypted-metadata domain like filenames.

A name→id secondary index is **rebuildable** (lives under "rebuildable
secondary indexes", rustfs-spec §4) so lookup-by-name is O(log n) without
making name uniqueness an authoritative invariant of the leaf. Name uniqueness
within a volume is enforced at create/rename time, not by the on-disk leaf
ordering.

### 3.2 Snapshot roots are first-class retained roots

The superblock already retains recent transaction roots for rollback and safe
discard (rustfs-spec §5, §14). A snapshot **promotes** one of those committed
roots to a *named, indefinitely retained* root: the ordinary root-history
window may scroll past it, but a snapshot keeps its pinned root (and everything
reachable from it) alive until the snapshot is deleted. The set of live roots
is therefore: the current root, the rolling recent-history window, and every
snapshot root. The reachability union of all of these is what TRIM/refcount/GC
must treat as live.

### 3.3 Refcounting and reachability

RustFS already refcounts shared chunks and keeps a reverse-reference tree
(rustfs-spec §4, §9). Snapshots **must not** require bumping a per-block
refcount at creation time — that would make snapshot creation O(volume),
violating the metadata-only and scalability invariants (§2, charter §26.6).
Instead:

- A block is **live** iff it is reachable from *any* live root (current,
  retained-history, or snapshot). Reachability — not a single global
  refcount — is the authority for liveness, exactly as COW already requires.
- Block reuse and TRIM (rustfs-spec §11) compute "unreachable from every
  retained root, snapshot, reflink, deduped extent, and recovery root" using
  the live-root set above. Snapshot creation is therefore O(1) metadata: write
  one `SnapshotRecord` naming an already-committed root and commit.
- Deletion of a snapshot removes its `SnapshotRecord`; blocks reachable only
  from that (now removed) root become eligible for the normal COW/refcount/free
  and pending-discard pipeline (rustfs-spec §11). Freeing is incremental and
  interruptible, never a foreground O(volume) stall (charter §26.6), and never
  busy-spins (charter §2.23).

The spec author must state precisely how the existing refcount/reverse-ref
machinery and the snapshot-root set combine so that the "unreachable from every
retained root **and snapshot**" rule the TRIM section already names is
implemented exactly once (charter §2.2), not duplicated between TRIM and
snapshot code.

---

## 4. Lifecycle and operations

All operations are one atomic COW transaction each (rustfs-spec §14).

- **Create**: snapshot the *current committed* root (or an explicitly named
  retained root, for "snapshot what mount selected after a crash"). Allocates a
  new `snapshot_id`, writes one `SnapshotRecord`, commits. Metadata-only, O(1).
  A snapshot of an in-flight uncommitted state is impossible by construction —
  only committed roots are snapshottable, which is what makes a snapshot
  crash-consistent and a sound backup source.
- **List**: enumerate the snapshot tree (paged, capability-checked). Returns
  id, name, created, parent, root_digest, flags, and accounting (§7).
- **Rename**: change `name` in place (one COW transaction), enforcing
  uniqueness against the rebuildable name index.
- **Mount read-only**: expose a snapshot root as a read-only view through the
  VFS. The driver serves reads from the pinned root tree; writes are refused
  (`DriverError::PermissionDenied`/read-only). A snapshot mount is just the
  normal read path pointed at a retained root — no new read code path.
- **Delete**: remove the `SnapshotRecord`; schedule now-unreachable blocks for
  the incremental free/discard pipeline. A `HELD` snapshot (e.g. an in-progress
  backup source, §6) refuses deletion until released, fail-closed.
- **Hold/release**: a backup or replication consumer marks a snapshot `HELD` so
  a concurrent retention sweep cannot delete the root it is streaming. Holds are
  capability-gated and logged.
- **Rollback (whole-volume)**: optional v1 feature — publish a snapshot root as
  the new current root in one transaction (the inverse of create). Stated here
  so its safety rules (it does not destroy other snapshots; the prior current
  root becomes eligible for GC unless itself snapshotted) are designed, even if
  staged later. Per-file restore is a backup-tool concern built on read+copy,
  not a filesystem primitive.

Determinism and no retry-until-it-works (charter §2.1): a failed operation
returns a typed error and leaves the committed state untouched.

---

## 5. Capabilities and security

New capabilities, each introduced **with** its enforcement point (charter §5.2 —
no speculative capability):

- `CAP_FS_SNAPSHOT_CREATE` — create/rename/hold snapshots of a mounted volume.
- `CAP_FS_SNAPSHOT_DELETE` — delete snapshots and release held roots.
- `CAP_FS_SNAPSHOT_READ` — list snapshots and mount one read-only.

The spec author must justify each against the charter §5.2 minimalism test
before adding it; if an existing capability (e.g. `CAP_FS_MOUNT`) already
expresses the authority at the right granularity, reuse it and drop the new
one. Whichever survive, the rules are fixed:

- Capability checked **before** any snapshot state is read or mutated, using the
  kernel-attested caller identity (charter §5.4).
- Every input validated: name length/bytes (rustfs-spec §13 name rules),
  snapshot id existence, root_generation validity, no overlap with reserved
  ids. Reject the whole request on any failure; never partially apply.
- Fail closed: an unknown id, a missing capability, a not-yet-authenticated
  snapshot root, or an `Err` denies and never widens authority.
- Every create/delete/rollback/hold decision emits a stable event ID on the
  hash-chained audit log (charter §19.4).
- Snapshot names and contents stay in the encrypted-metadata domain; reading a
  snapshot requires the same volume key as the live volume (no plaintext
  snapshot bypass, rustfs-spec §7).

---

## 6. Backup-oriented primitives (the reason snapshots exist)

A future backup solution is the primary consumer. The snapshot spec must
provide the mechanism a backup tool needs, **without** building the backup
service itself. Design these now so snapshots do not need redesigning later:

### 6.1 Stable content addressing

Every snapshot exposes its `root_digest` (§3.1) and its per-file logical hashes
(RustFS already stores a strong logical hash per record, rustfs-spec §2, §5).
A backup tool can therefore deduplicate and verify across runs by content hash,
and detect bit-rot by comparing a streamed object's hash to the stored one.

### 6.2 Snapshot diff (the incremental-backup primitive)

Provide a `snapshot_diff(base_id, target_id)` operation that returns the set of
changes between two snapshots of the same volume **efficiently**, by walking the
two COW root trees and pruning identical subtrees: where two B-tree nodes share
the same on-disk address (unchanged by COW), the entire subtree is unchanged and
skipped. Cost scales with the *delta*, not volume size (charter §26.6). Output is
a typed stream of change records:

```text
DiffRecord:
    kind        Added | Removed | Modified | MetadataOnly
    node        inode id + path-or-handle
    extents     for Modified: the changed logical ranges (so backup copies
                only changed bytes, leveraging reflink/COW sharing)
    metadata    changed inode metadata + extended-attribute deltas (see
                plans/RUSTFS-METADATA.md — backups must carry metadata too)
```

This is what turns "full snapshot" into "incremental backup": a backup run
streams only `snapshot_diff(last_backed_up, new)`.

### 6.3 Send / receive stream

Provide a `snapshot_send(id [, base_id])` that serialises a snapshot (or the
diff from `base_id`) into a self-describing, integrity-checked byte stream, and
a `snapshot_receive` that reconstructs it into a target RustFS volume,
recreating reflink/COW sharing rather than inflating shared extents. The stream:

- is versioned and hashed like any ABI (charter §9), framed, and
  self-authenticating (each object carries its logical hash);
- carries extended-attribute / preset metadata (plans/RUSTFS-METADATA.md) so a
  backup round-trip never loses ADFS/Amiga/Atari/Mac metadata;
- never embeds the volume key; encryption of the stream at rest is the backup
  tool's policy via `lib/crypto`, but the stream format must make whole-stream
  authentication possible;
- is produced by reading a `HELD` snapshot so the source cannot be GC'd
  mid-stream (§4 hold/release).

### 6.4 Crash- and failure-resilient streaming

A backup may run for hours over a 100 TB+ volume (charter §26.6); `send` must be
resumable/restartable, make bounded forward progress, surface I/O errors from a
failing disk as typed errors (charter §26.5), and never busy-spin (charter
§2.23). A snapshot held by an interrupted backup is released on a documented
timeout/owner-death path so a crashed backup cannot pin a root forever.

---

## 7. Space accounting

Report, per snapshot and per volume (through the System Information API, charter
§16.6 — never a `/proc` scrape):

- `referenced` — logical bytes reachable from the snapshot root;
- `exclusive` — bytes reachable *only* from this snapshot (what deleting it
  would actually free), computed from reachability, not a stored counter;
- `shared` — bytes shared with the live volume or other snapshots.

`exclusive` is the number a retention/backup UI needs ("how much do I get back
if I delete this?"). It is derived on demand by a bounded, paged walk, never
held resident for the whole volume (charter §26.6).

---

## 8. Interaction with existing subsystems

- **COW / transactions** (rustfs-spec §14): every snapshot op is one atomic
  transaction; crash leaves prior-or-new, never torn.
- **Reflink / dedupe / refcounts** (rustfs-spec §9): snapshots share the same
  immutable chunks; the reachability rule (§3.3) is the single liveness
  authority, not a duplicated counter.
- **Sparse** (plans/SPARSE.md §6.1, §15): snapshots preserve ZERO/Hole extents
  exactly; a snapshot of a sparse file allocates no data and copies no zeroes.
  (SPARSE.md already names "snapshot" as a retained root — this brief defines
  the feature it referenced.)
- **TRIM/discard** (rustfs-spec §11): the pending-discard queue gates on the
  full live-root set including snapshots; this brief supplies the snapshot half
  of the reachability check the TRIM section already wrote down.
- **Scrub** (rustfs-spec §12): scrub verifies snapshot roots and the snapshot
  tree like any metadata; a snapshot root that fails to authenticate is a
  reported integrity event (charter §26.5).
- **Check** (rustfs-spec §12): offline check validates the snapshot tree,
  rebuilds the name index, detects dangling `root_pointer`s, and reconciles the
  snapshot-root set with reachability; a corrupt rebuildable name index must
  never make a volume unmountable (rustfs-spec §4).
- **Rescue** (rustfs-spec §12): rescue lists valid snapshot roots among the
  self-identifying metadata it discovers, so a damaged volume's snapshots are
  recoverable extraction targets.
- **Online grow** (rustfs-spec §13 resize): unaffected; snapshots pin roots, not
  device geometry.

---

## 9. ABI, tooling, and docs

- ABI: add `SnapshotId`, `SnapshotRecord`/`SnapshotInfo`, `DiffRecord`, and the
  send/receive stream framing types to `lib/abi` (a new
  `lib/abi/src/driver/snapshot.rs` or an extension of the filesystem driver
  surface), under the same discipline as the syscall table (charter §9):
  versioned, hashed, `#[repr(C)]` where C-visible, frozen on first release.
  Because `abi-v1` is not yet shipped, extend the existing `Filesystem*` traits
  in place rather than adding a `v2` (charter §2.13).
- Tooling: extend the `rustfs` command (rustfs-spec §12) with
  `rustfs snapshot create|list|rename|delete|hold|mount|diff|send|receive`,
  backed by the capability-checked ABI — never a privileged bypass (charter
  §16.6). The CLI binds to standard streams only (charter §20).
- Docs: add `§20 Snapshots` to `docs/src/filesystem/rustfs-spec.md`, a row to
  the §2 mandatory feature table, a stage to the §18 staged-delivery plan, and
  update `docs/src/filesystem/rustfs.md`. Rustdoc on every new public item
  (charter §2.8, §13).

---

## 10. Required tests

The snapshot implementation is incomplete unless these pass (charter §7, §16,
§23.4 — every fix carries a regression test):

1. create snapshot is metadata-only: no data blocks allocated, live file bytes
   unchanged, O(1)-ish regardless of volume contents;
2. snapshot pins data: overwrite/delete a live file, snapshot still reads the
   old bytes; the old chunks are not freed or trimmed while the snapshot exists;
3. delete snapshot frees exactly the exclusive blocks, frees nothing shared with
   the live volume or another snapshot, and the free/discard is incremental;
4. TRIM never touches a range reachable from any snapshot (extends the existing
   rustfs-spec §16 TRIM-reachability test to live snapshots);
5. crash replay at every snapshot create/rename/delete/rollback commit step
   leaves prior-or-new, never a torn snapshot;
6. read-only mount of a snapshot serves correct historical bytes and refuses
   writes (fail closed);
7. wrong key cannot read a snapshot; no plaintext snapshot name or data exists;
8. capability gate: each op refused without its capability, allowed with it,
   and the decision logged with a stable event ID;
9. `snapshot_diff` returns exactly the changed set and prunes unchanged
   subtrees (cost scales with the delta, asserted on a large mostly-unchanged
   volume);
10. `send`/`receive` round-trips a snapshot and an incremental diff, preserves
    reflink/COW sharing (received volume does not inflate shared extents),
    preserves extended-attribute/preset metadata (RUSTFS-METADATA.md), and
    authenticates the stream; a corrupted stream is rejected;
11. scrub/check validate snapshot roots and the snapshot tree; check rebuilds
    the name index and reconciles dangling roots; rescue lists snapshot roots;
12. scalability floor (charter §26.7): thousands of snapshots on a small-RAM
    machine with a large emulated volume — bounded resident metadata, no panic,
    no busy-spin, fail-closed on exhaustion;
13. Time64 snapshot `created` persists pre-1970 / post-2038 / far-future values;
14. fuzz targets for snapshot-record decode, snapshot-tree decode, and the
    send/receive stream parser (the stream is untrusted input — sandboxed and
    fuzzed, charter §19.5, §19.6).

---

## 11. AGENTS.md and PLAN.md amendments to call out

The generated spec must explicitly identify these charter touch-points:

- **`AGENTS.md` §3** — the `drivers/filesystem/rustfs/` module list (rustfs-spec
  §3) gains a `snapshot` module; note it in the layout if the charter enumerates
  RustFS internals there.
- **`AGENTS.md` §5.2** — adding `CAP_FS_SNAPSHOT_*` requires the capability-
  minimalism justification; record each surviving capability and its
  enforcement point. Update the §5.2 example set only if the charter lists
  capabilities by name.
- **`AGENTS.md` §16.6 / §18.1** — snapshot listing and space accounting are
  exposed through the System Information API, never a `/proc`/`/sys` view.
- **`AGENTS.md` §21** — snapshot `created` and all stream timestamps are
  `Time64`.
- **`PLAN.md`** — add a RustFS snapshot stage (after the v1 stages, rustfs-spec
  §18) and, if a capability or ABI type is added, a one-line "Charter
  Amendments" rationale (charter §13).
- **`docs/src/filesystem/rustfs-spec.md`** — new §20, §2 feature-table row, §18
  stage; the existing §11/§16 "snapshot" references stop being forward
  allusions and cite §20.

This brief, like the rest of `plans/`, states the plan and the design, not a
build log (charter §13): when the work lands, replace the planned/in-progress
prose with the done-state summary rather than appending a changelog.
