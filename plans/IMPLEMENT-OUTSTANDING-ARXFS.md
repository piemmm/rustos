# IMPLEMENT-OUTSTANDING-ARXFS.md — the ARXFS completion ledger

Status: **planned** — the umbrella that sequences every outstanding ARXFS work
item to closure.
Binding under `AGENTS.md` and listed in its §15.18 jump-sheet.

ARXFS spec stages 1–15 are shipped: an encrypted, checksummed, compressing,
deduplicating, sparse-aware, link-carrying, attribute-carrying copy-on-write
filesystem with online scrub, offline check, rescue, safe discard, and health
tracking (`docs/src/filesystem/arxfs-spec.md` §18). Stages 16–21 are not, and
they are spread across six plans plus two items no plan owned. This file is the
single ordered ledger: what is left, which plan owns it, what blocks what, and
the prompt an implementing session runs.

It **supersedes no plan.** Each item keeps its design in its own plan; this file
holds only the order, the dependencies, and the two items (A0, B1) that had no
owner. When an item lands, its own plan is updated to its done-state summary and
its row here moves to `done` — nothing is appended, here or there.

Read first: `AGENTS.md`, `docs/src/filesystem/arxfs-spec.md` (the binding
spec), and the owning plan of the item you are about to implement.

---

## 1. The ledger

Order is top to bottom. An item may start when every item it depends on is
`done`; items at the same depth with no edge between them may proceed in
parallel across sessions. The table **gains rows**: a defect too large for the
item that found it is inserted as the next item to be taken (§6, the
no-deferral rule), ahead of everything below it.

| # | Item | Owner | Spec stage | Depends on | Status |
|---|---|---|---|---|---|
| A0 | Bounded, resumable tree iteration | §3 here | — | — | **done** |
| A1 | Bounded-stack B-tree mutation path (defect `OPEN-DEFECTS.md` D65) | §7 here | — | A0 | **done** |
| A2 | Bounded whole-volume reconcile and reachability state (defect D-M5) | `ARXFS-MAINTENANCE.md` | 18 | A1 | **done** |
| M0 | Shared background pacer + cross-layer availability query | `ARXFS-MAINTENANCE.md` | 18 | — | **next** |
| M1 | The read-only repair rule (defect D64) | `ARXFS-MAINTENANCE.md` | 18 | — | planned |
| WB0 | Write-amplification measurement harness | `ARXFS-WRITEBACK.md` | 17 | — | planned |
| WB1 | Dirty block set + the commit barrier (defect D63) | `ARXFS-WRITEBACK.md` | 17 | WB0 | planned |
| WB2 | Run coalescer | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB3 | Fold in the allocation map's dirty pages | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB4 | Commit scheduler | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB5 | The bound and memory pressure | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB6 | Hardware acceptance + docs | `ARXFS-WRITEBACK.md` | 17 | WB2–WB5 | planned |
| M2 | Bounded passes: scrub, discard sweep, health (D-M2/3/4) | `ARXFS-MAINTENANCE.md` | 18 | A2, M1 | planned |
| M3 | The maintenance scheduler | `ARXFS-MAINTENANCE.md` | 18 | M0, M2 | planned |
| M4 | The `FilesystemMaintenance` driver-ABI facet | `ARXFS-MAINTENANCE.md` | 18 | M3 | planned |
| M5 | The maintenance runner | `ARXFS-MAINTENANCE.md` | 18 | M4, WB1 | planned |
| M6 | Escalation, reporting, the `arxfs` command app | `ARXFS-MAINTENANCE.md` | 18 | M5 | planned |
| M7 | Maintenance acceptance + docs | `ARXFS-MAINTENANCE.md` | 18 | M6 | planned |
| S1 | Hole-aware seek and punch-hole/zero-range | §4 here + `SPARSE.md` §19 | 13 (completion) | A0 | planned |
| P1 | `cp`/`mv`/file-manager metadata + sparseness preservation | `ARXFS-METADATA.md` §10 | 16 | S1 | planned |
| P2 | The attribute CLI | `ARXFS-METADATA.md` §10 | 16 | — | planned |
| P3 | Named streams (forks above `VALUE_MAX`) | `ARXFS-METADATA.md` §4.4 | 16 | — | planned |
| B1 | The §5 format targets: fs block size, record size, inline small files | §5 here | 19 | WB2, A0 | planned |
| N1 | Snapshots | `ARXFS-SNAPSHOT.md` | 20 | WB1, A0 | planned |
| N2 | Snapshot send/receive carries the attribute set | `ARXFS-METADATA.md` §10 | 16/20 | N1, P3 | planned |
| F1 | FEC and multi-device redundancy (FEC0–FEC20) | `ARXFS-FEC.md` | 21 | WB1, M0 | planned |

**Blocked, and honestly so — not in this ledger's scope.** Two items in
`ARXFS-METADATA.md` §10 cannot be completed by ARXFS work and stay open with
their reason recorded there, not here:

- **Per-family foreign-driver wiring** (`amiga.*`, `atari.*`, `mac.*` presets).
  ADFS answers `acorn.*` already; the AmigaDOS, Atari GEMDOS, and classic-Mac
  drivers do not exist in the tree, so those registry entries have no producer.
  Each lands with its driver, not here.
- **Archive round-trip of preset attributes.** There is no archive tool in the
  tree, so there is nothing to teach. It lands with the archive tool.

## 2. Why this order

- **A0 first, because it is the floor everything else is tested against.**
  Every plan's scalability acceptance is the combined floor — a ~1 GiB machine
  serving several 100 TB volumes at once. A tree walk that materialises a whole
  tree into a `Vec` before its caller reads the first entry cannot pass that on
  any stage, and it makes a "bounded" maintenance chunk unbounded in memory
  before it does any work. Fixing it once, first, was also the difference
  between one change and six that each work around it.
- **A1 and A2 came next, because A0's work surfaced them (§7) and the
  no-deferral rule puts a found defect ahead of everything planned.** Both are
  closed. A1 was a reachable kernel-stack overflow on the write path: the
  mutation path is iterative like the read paths A0 converted, where it had
  recursed at 8 KiB per tree level against a 32 KiB stack. A2 finished the
  boundedness A0 was taken for: the iteration under `scrub` and `check` was
  bounded, their own whole-volume accumulators were not, and M2 could not have
  claimed a bounded pass over them.
- **The barrier before every background writer and every durable-root
  consumer.** WB1 closes the commit-barrier defect (`OPEN-DEFECTS.md` D63): a
  superblock slot published with no barrier before it can name a root whose
  interior nodes never reached media, losing the whole volume to one power cut.
  Snapshots exist to give exactly the guarantee that defect breaks, FEC's commit
  witnesses depend on it, and the maintenance runner would multiply its exposure
  across every background pass — so all three sequence behind WB1.
- **The rest of write-back before the format targets.** A wider record on an
  uncoalesced write path multiplies the per-record device command count instead
  of reducing it, so B1 follows WB2.
- **M0 and M1 can go first, and should.** M0 is a hoist plus one
  default-provided query with two producers; M1 is a security fix — a read-only
  mount currently writes to its device through scrub's unguarded copy-repair
  (`plans/OPEN-DEFECTS.md` D64).
  Neither needs the barrier, and M1 should not wait behind six write-back
  stages.
- **M2 before M3, because a scheduler over broken operations is worse than
  none.** M2 makes each operation a bounded, resumable, lossless pass (over the
  reconcile state A2 bounded first). Until it lands, `scrub` is one
  uninterruptible call over a whole volume, `trim`
  silently forfeits most of the space it is asked to discard, and `health`
  scrubs inline — so a runner driving them would *look* correct while doing the
  wrong thing quietly. That is the one ordering in this ledger where getting it
  backwards would be actively worse than leaving the work undone.
- **P2 and P3 are independent.** The attribute CLI and named streams touch
  neither the write path nor the maintenance path.
- **FEC last, and largest.** Twenty-one stages, its own implementation prompt
  (`ARXFS-FEC.md` §30), and a hard dependency on both the barrier and the shared
  pacer M0 hoists — a third background scheduler pacing to a third notion of
  "busy" is the defect that plan and `ARXFS-MAINTENANCE.md` §6 both forbid.

## 3. A0 — bounded, resumable tree iteration — done

`TreeWalk` (`drivers/filesystem/arxfs/src/btree.rs`) is the driver's only way
to read more than one record from a tree. A step descends one root-to-leaf path
and yields that leaf's records from the walk's own block-sized buffer, so an
operation's resident bytes are set by the block size and never by the tree, and
nothing is allocated per record. The position is a single key, so a caller may
mutate the tree between steps — truncation frees run by run, seeking straight to
the run covering the cut — and a bounded pass may stop, persist that key, and
resume with the sequence an uninterrupted walk would have yielded. `NodeTrail`
turns the same walk into the node enumeration the free-space rebuild and
whole-tree freeing need, reporting each node as the walk enters it and again
when it leaves the last key beneath it; freeing on the second is what stops a
later step descending through a block already back in the allocator. The
collecting forms (`btree_collect_entries`, `btree_collect_nodes`,
`btree_collect_tree`) are deleted, and `scrub`'s verifying node walk is a
bounded frame stack rather than recursion, so no read path recurses per tree
level.

One descent now serves lookups, floor queries, and the walk, and it validates
what every reader then trusts: a level that decreases by one per step (so a
child pointer leading back to an ancestor is refused, where the old descent
looped or recursed forever), an entry count that fits the block (the old one
indexed the buffer straight from the on-disk count), and keys that ascend
within a leaf. Each is a fail-closed device fault; a walk never ends early and
quietly, because a caller freeing or accounting for every record would then
miss some.

*Measured:* a stat over a file with 800 extents holds 512 bytes and allocates
twice — the same as one with 200 (`tests/bounded_iteration.rs`). The
allocation-map rebuild over sixteen times the records stays inside a
geometry-fixed budget rather than scaling with them.

## 4. S1 — hole-aware seek and punch-hole/zero-range

**Owned jointly:** `plans/SPARSE.md` §19 states the ARXFS behaviour; the ABI
addition is stated here because that plan correctly declined to invent it.

`SPARSE.md` §19 records both items as blocked on "an interface that does not
exist yet". That is accurate but not a reason to stop: the interface is
TAIRiX's own, `abi-v1` is unfrozen, and there is a real consumer — a copy that
preserves sparseness (§15 of that plan requires it, and P1 is the change that
does it). So the interface is added here, in place, with its first consumer.

**Deliverable.**

- Two filesystem operations on the existing versioned filesystem driver traits
  and their syscalls, extended in place with no `v2`: a hole-aware seek
  (`SEEK_DATA` / `SEEK_HOLE`-equivalent) and a punch-hole / zero-range. Both
  capability-checked as the ordinary write/read authority on the node — no new
  capability: punching a hole is writing zeroes, and asking where the holes are
  is reading metadata the caller may already read.
- ARXFS answers the seek from the extent tree it already walks (an unmapped
  range *is* a hole), through the A0 cursor rather than by collecting the map.
  It implements the punch by dropping the covering mappings and releasing the
  replaced data extents through the copy-on-write refcount/free path — the same
  code an all-zero write already runs, so it is a bound and a range walk, not a
  new pipeline.
- Every other filesystem driver answers honestly: a driver with no hole concept
  reports the whole file as data and refuses the punch as unsupported, never
  fabricating either.

**Acceptance.** The mandatory behaviours `SPARSE.md` §15 makes conditional
become unconditional and tested: a seek over a mixed file reports every hole and
every data run exactly, at the boundaries, including a hole at offset zero and a
file that is entirely a hole; a punch over a data range makes it read as zero,
frees the replaced extents only when the copy-on-write rules allow, leaves a
snapshot's view unchanged, and normalises adjacent holes; a punch that spans
end-of-file, a zero-length punch, and a punch on a read-only mount each fail or
no-op correctly. `SPARSE.md` §19 is replaced by its done-state summary.

## 5. B1 — the §5 format targets

**Owned here** because spec stage 19 has no plan and `ARXFS-WRITEBACK.md` §10
records the finding rather than the work.

The spec's §5 constants are targets the implementation does not meet: a
metadata block and a data record are each **one device block**, because
`bootstrap` takes the filesystem block size straight from the device geometry.
A 512-byte SD card therefore gives ARXFS 512-byte blocks — 443 usable content
bytes per block after the per-block trailers, 384 bytes of B-tree node payload —
so trees are far deeper and extent records roughly eight times as numerous as on
a 4 KiB volume. Inline/packed small-file storage does not exist. This compounds
with every write-amplification cause WB0–WB6 addresses.

**Deliverable.** The filesystem block size is decoupled from the device's
logical block size: a volume is formatted at a filesystem block size derived
from the device (never below its logical size, and wider on flash), reads and
writes translate to device blocks through the one existing device seam, and the
§5 metadata-block and data-record targets and inline small-file storage are
implemented against it. It is an on-disk format change, made **in place** —
there is no shipped release, so there is no compatibility reader, no `v2`
alongside a `v1`, and no migration path; the single living definition is the
only definition.

**Constraint A1 measured.** The driver stages metadata through *on-stack*
block-sized buffers at some twenty sites (`write_file`, `store_cluster`,
`map_write`, `commit`, …), and one `write_at` already spends ~11.6 KiB of the
32 KiB kernel thread stack on them. Widening the filesystem block size
multiplies every one of those, so decoupling it means moving that staging off
the stack in the same change — not raising `MAX_BLOCK_SIZE` and hoping.

**Acceptance.** A 512-byte device formats and mounts with a wider filesystem
block size and the same correctness suite green; the §5 constants in the spec
become statements of fact rather than targets, with the §18 stage-19 row ticked;
a small file stores inline with no data extent; the WB0 harness shows the
per-record device command count falling rather than rising; the crash-replay,
corruption-injection, and fuzz suites pass on the new geometry.

If the record/inline design turns out to need decisions this section does not
fix, **stop and ask** — do not guess an on-disk layout.

## 6. What every session does

1. **Read** `AGENTS.md`, the spec, and the owning plan of the item — plus, for
   the write-back and maintenance items, `plans/OPEN-DEFECTS.md` D63 and
   `plans/FIX-IO.md` for the storage-fault model the pacer and the
   availability query sit in.
2. **Take the next item whose dependencies are all `done`**, from this ledger.
   One item per session. Do not start two, and do not start half of one.
3. **Implement it completely** — code, rustdoc, that item's tests, the
   `docs/src/` page, and the spec section — behind the existing versioned
   driver traits so the VFS is never broken.
4. **Fix every defect the work surfaces or you notice** — see the no-deferral
   rule below. There is no version of "done" that leaves one unfixed.
5. **Run the whole-project gate once**, over the entire workspace, and quote its
   real output.
6. **Update** the owning plan's status, this ledger's row, the spec's §18 stage
   row, and `PLAN.md` where it names the stage — replacing prose with the
   done-state summary, never appending a log.
7. **Report** the acceptance-gate verdict, including what you did not do.

### The no-deferral rule

**Every defect found gets fixed. Size is not an exit; recording is not a
fix.** This binds every item in this ledger and every plan it names.

- **Default: fix it in the item you are working on**, with a regression test
  that fails before and passes after. This covers a defect in the code the item
  touches, in the driver around it, in a `lib/*` crate it consumes, in the
  kernel path that calls it — anywhere the work leads.
- **A defect too large for the current item becomes the immediate next item.**
  Finish the current item's own scope completely, write the defect up as a
  numbered entry in the owning plan's defect section, and **insert it into this
  ledger as the next item to be taken** — ahead of everything else, blocking
  every other row until it is fixed. Sacrificing a whole stage to one defect is
  the correct outcome, not a failure of planning. What is *never* correct is
  carrying it forward as a note while other items proceed past it.
- **The write-up is the schedule, not the resolution.** An entry in a defect
  section is a statement that the very next piece of work is fixing it. A
  ledger whose next item is not the known defect is a ledger that has deferred
  one, which this rule forbids.
- **No exits.** "Unrelated", "pre-existing", "out of scope", "not this plan's
  area", "the gate didn't catch it", "too large", "we'll get to it", and
  "recorded in the plan" are none of them exits. A defect owned by a different
  plan is still fixed: the item is inserted here, cross-referenced to that
  plan, and still blocks this ledger until it is closed.
- **The one thing that is not deferral: asking.** If fixing it properly
  conflicts with something else — another rule, an in-flight design, two
  requirements that contradict — stop and put the decision to the User. That is
  a decision point, and the work waits on an answer rather than moving on. It
  is not permission to pick a lesser fix, and it is not permission to proceed
  with other items in the meantime.
- **Consequence.** No item is done while it knows about an unfixed defect, and
  ARXFS is not done while any plan's defect section is non-empty. "All items
  planned → done" and "every defect section empty" are the same finish line.

## 7. A1 and A2 — the two defects A0 surfaced

A0's own scope is closed. Reading the code it converted surfaced two defects too
large to fold into it, so under §6 they took precedence over every row below
them. Both are done and neither blocks the ledger any longer.

### A1 — bounded-stack B-tree mutation path — done

`btree_insert` and `btree_remove` (`drivers/filesystem/arxfs/src/btree.rs`) are
iterative. Each descends once through the one shared descent, recording the path,
edits the leaf in place in a node buffer, and walks back up rewriting each
ancestor — re-deriving the child index at each level from the same `child_index`
the descent used, so the two cannot disagree, and taking the child's minimum key
from the step that just wrote it rather than reading the node back. Every level
re-entered on the way up is validated at `child_level + 1`, so the write path
refuses a cyclic or over-deep tree exactly as the read descent does.

The node buffers live in one `TreeEdit` scratch the mount lends per mutation:
the node being rewritten, plus the adjacent pair (`lo`, `hi`) a split, borrow,
or merge moves entries between. Staging the pair in key order makes a borrow one
direction of a single entry move and a merge always a fold of the higher node
into the lower, whichever side underflowed. Each buffer carries one entry of
slack past the block, so an insert lays a full node out and then splits it —
no second layout path for the overflowing case. Entries are edited in place
(`node_insert_entry` / `node_remove_entry` / `node_move_entry` / `node_append`),
so the per-record `Vec` decode `btree_load_entries` performed at every level is
gone with the function. Every entry edit takes a checked region, and one place
(`btree_write_node`) zeroes the payload past the count and refuses a count the
block cannot hold before a node reaches the media.

*Measured (release, x86_64).* One `write_at` on a fragmented file: **48 097**
stack bytes over a three-level extent tree and **34 633** over a single leaf —
both past the 32 KiB thread stack the kernel hosts the driver on — become
**11 633** and **11 609**, the difference no longer scaling with depth. What
remains is the driver's own on-stack block staging down the write path, not the
tree edit, whose frames measure 904 (insert) and 968 (remove) bytes.
Removing one extent from an 800-extent tree allocated **596** times and now
allocates **118**. Device reads are unchanged (58 per insert) and one lower per
remove, the root re-read the collapse no longer needs.

Two defects the work surfaced were fixed with it, each with a regression test
that fails before and passes after: a merge of two empty siblings indexed into
an empty entry list and **panicked** on the write path of a corrupt volume (now
a fail-closed device fault); and `btree_insert` copied a caller's value with
`copy_from_slice`, so a record of the wrong width would have panicked rather
than been refused. The four byte-identical copies of the little-endian field
accessors across `lib.rs`, `btree.rs`, `header.rs`, and `superblock.rs` are one
definition.

*Left for its owner, not deferred.* The whole `write_at` chain still spends
~11.6 KiB of stack in a chain of block-sized staging buffers
(`write_file`, `store_cluster`, `map_write`, `commit` — 4–8 KiB each). It is
constant, within the 32 KiB budget, and guarded by a test; but it is sized by
`MAX_BLOCK_SIZE`, so item **B1**, which widens the filesystem block size, must
move that staging off the stack as part of decoupling it — recorded in §5.

### A2 — bounded whole-volume reconcile and reachability state — done

**Owned by `ARXFS-MAINTENANCE.md`** (defect D-M5 there).

A0 made the *iteration* under `scrub` and `check` bounded; A2 moved the truth
they derive out of RAM. Where a pass must decide something about every block or
every inode, it streams that decision through a **transient on-disk scratch
array** (`drivers/filesystem/arxfs/src/scratch.rs`): a flat array of
fixed-width elements in a contiguous run of the volume's own free space, paged
through the same bounded 64-page cache the allocation map uses
(`src/pagecache.rs`, hoisted so there is one such cache, not two), sealed per
page, zero-filled at allocation so no read can fall back on unauthenticated
bytes, and released before the pass returns. It is scratch, not metadata: a
crash leaves stale bytes in blocks the next mount finds free, and free space is
derived from the trees, so nothing leaks.

The refcount reconcile (`src/reconcile.rs`) splits the question in two. Two
thirds of it needs no accumulation at all, because the write path keeps the
referrer list **complete** — sharing past `REVERSE_REF_CAP` declines to dedupe
— so a lawful record satisfies `refcount == referrers.len()` with every referrer
named, and each stored referrer is verified against the extent it claims to come
from with one bounded lookup. The remaining question is irreducibly global:
whether a block with no chunk record is claimed by exactly one extent, since the
only record of a claim is the extent that makes it and the extent trees are
ordered by `(inode, logical block)`. That is what the claim array counts, at
**four bits per block** — exact over every lawful refcount, with a distinct
saturated state above them — which makes "the refcount says two but three
extents claim it", the divergence that frees live data, detectable rather than
suspected.

Where the run does not fit whole the pass covers the volume in **windows**, one
extra extent-metadata walk each, bounded at eight and never taking more than an
eighth of free space. Where no run can be placed at all — a read-only handle, a
nearly-full or badly fragmented volume — the bounded half still runs and the
report says claims were not counted (`ScrubReport::claims_counted`,
`CheckReport::structure`); **no correction is made from a partial truth**,
because a refcount lowered on a guess frees a block a live extent still maps.

`check`'s three per-inode accumulators are the same shape over the inode space:
a reachability bit, an expansion frontier (each directory enters it once, which
bounds the queue and terminates a directory cycle), and a name count.
`check::dir_entries` is gone; one directory cursor holding a single block
(`DirScan`) serves the structural check, path resolution, `readdir`, and the
empty-directory test, replacing four copies of the same slot scan and two
on-stack block buffers.

*Measured (`tests/bounded_iteration.rs`).* A scrub over sixteen times the
records holds **3 412** bytes against **3 028** — flat. A check adds the
free-space rebuild, whose footprint is the allocation map's bounded page cache
filling up, and stays inside a 128 KiB budget that the deleted per-block
accumulator blows at 819 KiB.

*Fixed with it, each with a regression test.* A chunk record that no extent
claims, or one keyed past the last block of the device, was invisible to the
extent-driven recompute and held its block allocated for the life of the volume;
both are now removed on exact knowledge. The allocation map and the
scrub-progress record both stamped the owner sentinel `u64::MAX - 3`, declared
as separate constants in separate files — every reserved owner now derives from
one enum, so a repeat is a compile error. `map_find_free_run` tested one block
at a time, so placing a large run on a fragmented volume scanned the whole map
block by block and was slowest exactly when the answer was "no such run"; it is
page-oriented, skipping a wholly-used or wholly-free page without reading it.

*Deliberately not done here.* The reconcile still runs to completion inside the
call that reaches it. Making it stop and resume mid-pass is the budget rework
M2 owns (`ARXFS-MAINTENANCE.md` §12: the budget must count verified blocks and
the cursor must be `(inode, logical offset)`), and doing it now would collide
with that design. The scratch machinery A2 lands is what M2 needs to do it.

## 8. The implementation prompt

Paste this, naming the item:

```text
Implement item <ID> from `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`.

Read `AGENTS.md`, `docs/src/filesystem/arxfs-spec.md`,
`docs/src/filesystem/arxfs.md`, `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`, and
that item's owning plan in full before writing code. For a write-back item also
read `plans/OPEN-DEFECTS.md` D63; for a maintenance item also read
`plans/FIX-IO.md` and `lib/raid/src/maintenance.rs`; for a snapshot or FEC item
also read `plans/ARXFS-WRITEBACK.md`. Then read the ARXFS driver code the item
touches, and state the assumptions you verified from the repository.

Confirm every dependency of the item is done before starting. If it is not,
say so and stop.

Implement the item completely, as a production change:
- no stubs, no `todo!()`, no `#[ignore]`, no dead or commented-out code, no
  `#[allow]` without a justification, no "for now";
- no compatibility shim, no `v2` beside a `v1`, no migration path, no feature
  flag preserving old behaviour, no mount option or tunable — ARXFS has one
  mandatory profile and `abi-v1` is not frozen, so wrong things are changed in
  place with every caller updated in the same change;
- no second implementation of anything the tree already has: one B-tree, one
  seal/integrity path, one dirty layer, one background pacer, one liveness
  authority, one match/discovery path;
- capability-checked before state, every input validated, fails closed, no
  ambient authority, security decisions on the hash-chained log with a stable
  event ID;
- allocation failure and every `Result` handled as a value, never
  `unwrap`/`expect`/`panic!` on a production path;
- event-driven, never a busy-poll, a yield loop, or a fixed-frequency tick;
- every capacity derived from discovered hardware or grown on demand, and every
  long operation bounded, resumable, cancellable, and correct on a ~1 GiB
  machine serving several 100 TB volumes at once;
- comments are terse *why* only, and never cite a charter section number.

Write that item's tests as part of the change, including its scalability floor
case.

Every defect the work surfaces or you notice gets **fixed**, each with a
regression test that fails before and passes after — in the code the item
touches or anywhere the work leads. Size is not an exit and recording is not a
fix: a defect too large for this item is written up in the owning plan's defect
section and inserted into the ledger as the **next item to be taken**, ahead of
everything else, and you say so. "Unrelated", "pre-existing", "out of scope",
"another plan owns it", and "too large" are not exits. Stop and ask only when
fixing it properly *conflicts* with something else and the User must decide —
that is a decision point, not a deferral, and no other item proceeds meanwhile.

Update the owning plan's status, this ledger's row (plus a new row for any
defect that became the next item), the spec's §18 stage row and the section the
item specifies, `docs/src/filesystem/arxfs.md`, the driver `README.md`, and
`PLAN.md` where it names the stage — replacing superseded prose with the
done-state summary, never appending a changelog.

Finish by running the whole-project gate from the repository root, once:
`cargo fmt --all`, `cargo fmt --all --check`, `cargo xtask ci`,
`cargo xtask fuzz --secs 5`, and `tools/ci/soak.sh both --secs 20`. Quote the
actual output. Do not commit or push. Report the acceptance-gate verdict and
state explicitly anything incomplete, blocked, or deferred.
```

For FEC (item F1) use the stage prompt in `plans/ARXFS-FEC.md` §30 instead of
the body above; it carries that plan's mandatory design constraints. The reading
list, the ledger check, and the closing gate still apply.

## 9. Non-goals

- **Not a redesign.** Every item's design lives in its owning plan; this file
  changes none of it.
- **Not a schedule.** It is a dependency order, not dates or estimates.
- **Not a place for status prose.** One row per item; the detail belongs to the
  owning plan.
- **Not a licence to batch.** One item per session, gated. Two half-landed
  items cannot be reviewed and cannot be reverted independently.
