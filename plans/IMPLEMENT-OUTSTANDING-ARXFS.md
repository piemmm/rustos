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
| A0 | Bounded, resumable tree iteration | §3 here | — | — | planned |
| M0 | Shared background pacer + cross-layer availability query | `ARXFS-MAINTENANCE.md` | 18 | — | planned |
| M1 | The read-only repair rule (defect D64) | `ARXFS-MAINTENANCE.md` | 18 | — | planned |
| WB0 | Write-amplification measurement harness | `ARXFS-WRITEBACK.md` | 17 | — | planned |
| WB1 | Dirty block set + the commit barrier (defect D63) | `ARXFS-WRITEBACK.md` | 17 | WB0 | planned |
| WB2 | Run coalescer | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB3 | Fold in the allocation map's dirty pages | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB4 | Commit scheduler | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB5 | The bound and memory pressure | `ARXFS-WRITEBACK.md` | 17 | WB1 | planned |
| WB6 | Hardware acceptance + docs | `ARXFS-WRITEBACK.md` | 17 | WB2–WB5 | planned |
| M2 | Bounded passes: scrub, discard sweep, health (D-M2/3/4) | `ARXFS-MAINTENANCE.md` | 18 | A0, M1 | planned |
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
  before it does any work. Fixing it once, first, is also the difference between
  one change and six that each work around it.
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
  none.** M2 makes each operation a bounded, resumable, lossless pass. Until it
  lands, `scrub` is one uninterruptible call over a whole volume, `trim`
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

## 3. A0 — bounded, resumable tree iteration

**Owned here** because no plan owned it and four do not work without it.

`ARXFS::btree_collect_entries` walks a whole tree and returns every leaf entry
in one `Vec`. Its callers are not incidental:

- `scrub_run` collects **every inode in the volume** before its budget applies,
  then `scrub_inode` collects every extent of one inode;
- `check` does the same over the inode tree five times, plus per-inode extents;
- `lib.rs`'s truncate, reflink, and read-span paths collect a file's whole
  extent map to walk part of it.

On the combined floor that is a device-proportional allocation on paths that
must be working-set-bounded, and it is the reason a scrub cannot honestly claim
a bounded chunk. It is a live scalability defect independent of the maintenance
work, not a refactor for tidiness.

**Deliverable.** A resumable cursor is the tree's *primary* iteration
primitive: seek to a key, yield entries in order under a caller-supplied bound,
return the key to resume from. It is allocation-free per step (the caller owns
one node-sized buffer), it is the same single generic B-tree node
implementation — there is no second tree — and every call site above is
converted to it. The collecting form is **deleted**, not kept beside it: a
superseded helper left in place is dead code that the next caller will reach
for.

**Acceptance.** Every converted path is bounded in resident bytes by its
caller's buffer, asserted against an allocation counter, not by inspection; a
walk interrupted and resumed yields exactly the same sequence as an
uninterrupted one; the tree suite and the existing scrub/check/truncate/reflink
tests pass unchanged; a large-volume floor test shows resident bytes independent
of volume size.

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

## 7. The implementation prompt

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

## 8. Non-goals

- **Not a redesign.** Every item's design lives in its owning plan; this file
  changes none of it.
- **Not a schedule.** It is a dependency order, not dates or estimates.
- **Not a place for status prose.** One row per item; the detail belongs to the
  owning plan.
- **Not a licence to batch.** One item per session, gated. Two half-landed
  items cannot be reviewed and cannot be reverted independently.
