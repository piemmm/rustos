# ARXFS-WRITEBACK.md — ARXFS write-back cache, commit batching, and the commit barrier

Status: **WB0–WB6 done**, save one on-metal measurement (measurement, the
dirty set and commit barrier, run coalescing, allocation-map integration, the
commit scheduler, the host's write-back expiry timer, the RAM-derived bound
with its back-pressure, and the acceptance suite). The only work left in this
plan is the on-metal Pi 4 SD throughput figure, whose procedure is the
checklist in §8's WB6 entry; nothing in the tree waits on it.
Binding under `AGENTS.md` and listed in its §15.18 jump-sheet.
Primary code area: `drivers/filesystem/arxfs/`.
Companion spec section: `docs/src/filesystem/arxfs-spec.md` §22.

ARXFS issued one single-block device write per copy-on-write block, one
transaction per VFS operation, and **no durability barrier at all**. That made
writes both slow — measured 5.6×–17.8× byte amplification and, on a 512-byte SD
card, one device command per 512 bytes — and unsafe on any device with a
volatile write cache. This plan fixes both with one mechanism: a
transaction-scoped dirty block set, a run coalescer, a commit scheduler, and the
single barrier that becomes affordable once commits are batched. The dirty set,
barrier, coalescer, allocation-map integration, scheduler, the kernel timer that
fires the window on an idle volume, and the RAM-derived bound are all present,
so batching is live on a mounted volume and bounded in content, in time, and in
memory — and bounded across a *machine's* volumes rather than per volume, which
the acceptance suite (WB6) found it was not. The on-metal throughput figure
remains.

It adds **no** capability, **no** ABI surface, **no** second data path, and no
mount option: the behaviour is the one production profile
(`docs/src/filesystem/arxfs-spec.md` §1), tuned from the device class the block
seam already reports.

---

## 1. The measured problem

Measured, and **machine-checked** by the WB0 harness
(`drivers/filesystem/arxfs/tests/write_amplification.rs`): a device recording
every command it is issued, incompressible payload, one file per case, one
freshly formatted volume per case. "Superseded" is the block writes a later
write in the same window replaced — bytes sealed and sent whose only surviving
version is the last one.

| Workload | fs block size | device commands | blocks sent | superseded | bytes to device | byte amplification |
|---|---|---|---|---|---|---|
| 64 KiB in one `write_at` | 512 | 5 | 158 | 0 | 79 KiB | 1.23× |
| 64 KiB as 16 × 4 KiB `write_at` | 512 | 64 | 335 | 24 | 167.5 KiB | 2.61× |
| 64 KiB in one `write_at` | 4096 | 5 | 25 | 0 | 100 KiB | 1.56× |
| 64 KiB as 16 × 4 KiB `write_at` | 4096 | 64 | 160 | 24 | 640 KiB | 10.00× |
| 34-byte append to an existing file | 512 | 4 | 11 | 0 | 5.5 KiB | 165.6× |
| create one empty file after a clean mkfs | 512 | 4 | 15 | 0 | 7.5 KiB | — |

Commands are fewer than blocks because the drain gathers adjacent staged blocks
into runs, so the exact run-length histogram is part of the baseline too. Every
commit issues exactly one barrier, save the map's once-per-sync-period
clean-to-dirty transition. The figures are properties of the write path, not of
the device measured on: the harness reproduces each of them on a 100 TiB volume,
thirteen million times the size, to the command — and it holds the same
independence for the operation *after a refused one*, which a rollback that
discarded the map instead made scale with the volume's metadata.

The same harness prices a **batched** window, where the calls inside it join one
transaction. The fixture is published before the window is armed and handed on
at its end, so a row here costs one command more than its per-operation
counterpart above — the map invalidation the published fixture makes the window
pay — and the comparison between the two rows of a pair is exact:

| Workload | fs block size | device commands | blocks sent | superseded | bytes to device | byte amplification |
|---|---|---|---|---|---|---|
| 64 KiB in one `write_at` | 512 | 6 | 159 | 0 | 79.5 KiB | 1.24× |
| 64 KiB as 16 × 4 KiB `write_at` | 512 | 7 | 159 | 0 | 79.5 KiB | 1.24× |
| 64 KiB in one `write_at` | 4096 | 6 | 26 | 0 | 104 KiB | 1.62× |
| 64 KiB as 16 × 4 KiB `write_at` | 4096 | 7 | 26 | 0 | 104 KiB | 1.62× |

Sixteen calls put exactly the blocks and bytes on the device that one call does,
behind one barrier, with nothing superseded. The single extra command is the
metadata run splitting in two, because the chunked path takes its tree blocks in
a different order.

Amplification is structural, not granular. It had four separate causes, one
stage each so each win is separately measurable. All four are closed: C1 and C4
by WB1, C3 by WB2, C2 by WB4.

**C1 — intra-transaction rewrite churn. Closed.** Every `store_block` ended in
`extent_assign`, a B-tree insert that copy-on-wrote the covering leaf and wrote
it — and its companion mirror — to the device immediately, so the same extent
leaf, the same inode-tree spine, and the same allocation-map pages went out once
per data block and only the last version of each survived. It was the dominant
cost: 64 KiB at a 512-byte block size is 148 data blocks, and **598 of the 746
writes were metadata, 294 of them superseded before the transaction ended** —
each carrying its own HMAC-SHA256 over the whole block, which on a Pi 4 with no
ARMv8 crypto extensions is a CPU cost as well as an I/O one. Staging the sealed
bytes and draining at the commit point leaves **ten** metadata writes for that
case: five mirrored blocks, each written once.

**C2 — one transaction per operation. Closed.** Every mutating operation ended
in `commit()`: a fresh transaction root (2 writes, 1 seal) plus a superblock slot
(2 writes, 1 seal) and a barrier between them — pure per-operation overhead
however small the operation, and it forced each transaction's churn out to the
device at every operation boundary. WB4's scheduler keeps the transaction open
for the next operation to join, so the chunked 4 KiB case costs what the single
call costs: the same 159 blocks and 81 KiB on a 512-byte volume against 335 and
167.5 KiB, the same 26 blocks at 4096 against 160, nothing superseded either
way, and one barrier rather than sixteen.

**C3 — single-block device I/O. Closed.** The drain issued one `write_blocks`
per staged block, so on the Pi's SD card every 512-byte write was a separate
CMD24 plus completion wait where one CMD25 could carry 128 of them — while the
*read* path already gathered a 64 KiB staging window and `drivers/storage/emmc2`
already staged 128 blocks per ADMA2 multi-block transfer. WB2 gathers the
drain's ascending order into physical runs against that same window, hoisted to
one shared bound. A 64 KiB write on a 512-byte volume costs **five** commands
against 158. An empty-file create after a clean mkfs costs four commands for
fifteen blocks: its twelve-block metadata run, the map invalidation, and the
slot pair.

**C4 — no commit barrier (a durability defect). Closed.** `commit()` wrote the
copy-on-write blocks, then the transaction root, then the superblock slot, with
no `Block::flush()` between any of them. On every device with a volatile write
cache the slot could reach stable media before the tree blocks the root it names
transitively references. `open` does re-validate the root before accepting a
slot, so a *lost root* was survivable; a **lost interior tree node beneath a
durable root** was not — both mirror copies absent, the mount fails closed, and
one power cut at the wrong microsecond loses the volume. Filed as
`plans/OPEN-DEFECTS.md` D63 and fixed in WB1 rather than alone, because a
barrier per unbatched per-operation commit would have added a full device-cache
flush to every VFS operation on a driver amplifying 5.6×–17.8× in single-block
writes. The work that depends on a durable published root — snapshots (spec
stage 20), FEC commit witnesses (stage 21) — is now unblocked.

## 2. Non-negotiable invariants

- **Consistency is never traded, only recency.** Nothing is published until the
  superblock slot is written, so every crash leaves the prior committed state or
  the new one, never a torn one — the existing copy-on-write guarantee
  (`arxfs-spec.md` §14) is unchanged. What the cache trades is *how recent* the
  surviving state is, bounded by §5's window in content **and in time**: the
  driver names each transaction's deadline to the kernel timer that fires it
  (§10), so a volume that falls quiet is published rather than held.
- **One barrier per commit, and it is mandatory.** Every dirty block except the
  superblock slot reaches the device, then `flush()`, then the slot. A commit
  that cannot barrier does not publish.
- **A dirty block is pinned memory, not reclaimable cache.** It cannot be
  dropped, only written, so it is bounded by back-pressure (force a commit) and
  never by eviction. It does not go through `tairix_reclaim::ReclaimCache`,
  whose every class is clean and whose refusal path — serve uncached — has no
  meaning for a block that exists nowhere else.
- **Forward progress is guaranteed, and not by the floor.** A file write's
  staged bytes scale with a caller's argument, so no fixed floor can cover "one
  transaction's working set". The write path yields instead: it always stores at
  least one record, then stops on a record boundary and reports a short count.
  The floor (§6) bounds the cache's *usefulness*, not its correctness.
- **No second anything.** One dirty set serves copy-on-write blocks and the
  allocation map; each ordinary commit and explicit sync has one barrier, one
  staging-window bound is shared with the read path, one integrity/crypto path
  seals blocks before staging, and one
  allocator. A dirty block is bytes already produced by the existing seal path.
- **Read-after-write within a transaction sees the new bytes.** The B-tree
  re-reads nodes it has just written; the dirty set is authoritative for its
  handle for every block it holds.
- **Every capacity is derived, never a hand-picked constant** (`AGENTS.md`
  §24.1): the byte ceiling comes from discovered RAM, the deadline from the
  device class the block seam reports. The staging-window *bound* is a fixed
  transfer bound shared with the read path, not a capacity.
- **The ceiling is the machine's, and its volumes share it.** A figure derived
  per volume is a multiple of the machine as soon as the machine has several
  volumes, and a dirty block is the one kind of memory nothing can reclaim, so
  each mounted volume takes a share of one machine-wide total rather than a
  slab of its own.
- **Fails closed, never panics.** A drain that faults rolls the transaction back
  and reports a typed error; it never retries in a loop and never publishes a
  partial transaction.
- **No new capability and no new ABI.** The cache adds no authority: it changes
  when bytes reach the device, not who may write them. `FilesystemWrite::flush`
  already means "make it durable" and keeps that meaning exactly.
- **No mount option, no knob** (`arxfs-spec.md` §1). Behaviour is derived from
  the device, not configured.

## 3. Where it sits

One module, `drivers/filesystem/arxfs/src/wcache.rs`, entirely beneath the
driver's single existing device-write seam:

```text
VFS operation
  -> ARXFS operation (unchanged)
      -> seal path (header/HMAC, AEAD, integrity trailers — unchanged)
          -> stage_block  <- write_meta / seal_data_block   the one seam
              -> dirty block set          (WB1, landed)
                  -> run coalescer        (WB2, landed)
                      -> Block::write_blocks / Block::flush
```

The non-transactional writes — the clean allocation-map stamp, transient
scratch arrays, an idempotent mirror copy-repair, and the superblock slot at the
commit point — go through `write_device`. Map pages and invalidation use the
dirty set; both paths reach the device through one `write_blocks` call site.

Above ARXFS, `kernel/core::fs::CachedFs` (clean metadata) and the driver's
injected `ClusterCache` (clean transforms) are untouched. Below it,
`kernel/tairix-kernel::block_cache::BlockCache` stays a **read** cache: a
coalesced run arrives as one wide `write_blocks`, which its existing coherence
rule already refreshes in place. There is exactly one dirty layer in the stack
and this is it — which is what `plans/SMARTRAM.md` §6.1 reserves for the
filesystem ("dirty data is not disposable cache and is handled by the
filesystem's write, journal, COW, or flush policy").

Mirroring is unchanged: `write_meta` still inserts both `phys` and
`companion(phys)`. The set simply holds two entries with identical bytes, which
the coalescer then recognises as one adjacent 2-block run.

## 4. Crash consistency

Close ordering, and the whole argument:

```text
drain every dirty block except the superblock slot
flush()                      <- the one mandatory barrier
write the superblock slot
flush()                      <- only when the caller asked for durability
```

One barrier suffices because the transaction root is just another block that
must be durable before the slot that publishes it; a per-step barrier buys
nothing and costs a full cache flush per step. The trailing barrier is issued
only for an explicit `fs_sync`, because that is the only case where the commit
*itself* must have reached media before the call returns.

Exhaustive crash outcomes:

| Crash point | Selected root | State |
|---|---|---|
| before the barrier | previous slot | prior |
| between barrier and slot | previous slot (new root durable but unreferenced) | prior |
| slot write torn | previous slot (the slot fails its authenticator) | prior |
| after the slot | new slot | new, whole |

The window this closes is the one C4 opened: a durable slot naming a root whose
subtree never reached media. That ordering is now impossible — when a commit
returns, the only blocks a device may still hold are the slot's two copies.

The commit point is the *primary* copy of the slot, so the pair is written
companion-first and a half-written pair publishes nothing. A failure of either
write leaves publication unknown, because the device may have taken one: the
handle forces itself read-only and frees nothing, so whichever root the device
holds survives for the next mount.

## 5. The commit scheduler — done

A transaction stays open and the next operation joins it. It closes on the
first of:

- an explicit `FilesystemWrite::flush` (`fs_sync`) — closes and issues the
  trailing barrier;
- the dirty set reaching its byte ceiling (§6) — back-pressure: the writer waits
  for real I/O rather than the set growing;
- the **dirty-age window** expiring, checked at the end of the operation that
  reaches it;
- an operation that needs a barrier for its own correctness: `trim` (discard
  eligibility reads the *committed* allocation map, so a block an open
  transaction reallocated must not be discardable), `grow`, `scrub`, `check`,
  `health`, and any operation that widens the incompatible-feature word (the
  word and the structures it names publish together, and a reader must not wait
  on an unrelated operation to learn of them);
- the volume being handed on (`into_block`, which therefore reports the closing
  commit's failure rather than returning a silently older image).

The offline `rescue` opens its own read-only handle, so it has no transaction to
close. **Drop is not a close point**: it cannot report a failure, and a commit
that failed silently in a destructor is worse than one that never happened, so
the teardown paths close explicitly instead — the kernel's volume detach flushes
the filesystem before it retires the driver (`commit_for_detach`), and a handle
dropped with a transaction still open loses it whole, leaving the last published
root. Every teardown does reach an explicit close: the volume-detach path
flushes the filesystem before the device (`commit_for_detach`) and the
`system_power` syscall syncs every mounted volume before the platform stops.

The window is one policy function over `Block::device_class()`, which the seam
already reported and the driver ignored:

| Declared class | Rationale | Window |
|---|---|---|
| `Removable` (SD, eMMC, USB) | highest per-command cost, the measured bottleneck, and the class that gains most | 30 s |
| `Rotational` | seek-bound; batching converts scattered metadata into sequential runs | 15 s |
| `SolidState`, `Virtual` | already cheap per command; a long window buys little and risks more | 5 s |

Ageing a transaction needs a **monotonic clock** and something that will come
back for it, and the host supplies both together
(`ARXFS::with_writeback_host`, §10). A handle given neither has no window to
measure and publishes at every operation: a host that cannot say how much time
has passed, or cannot be told when to return, does not get to defer
durability.

`close()` does **not** sync: POSIX semantics, and the write-then-close workloads
(image builds, bundle extraction, `cp` of many small files) are exactly the ones
the batching exists for. A program needing durability calls `fs_sync`, as it
must on any other system.

**Two undo scopes**, because a transaction that spans operations can fail at two
granularities. A failed *operation* is undone alone: the dirty set keeps a
savepoint of every block that operation stages over or discards, and the
allocator keeps the blocks it claimed, the private blocks it released, and the
frees it deferred, so replaying those backwards leaves the operations that
already joined the transaction exactly as they were. This is what lets one
metadata block be rewritten in place across a whole batch — the entire batching
win — without a late failure destroying an early operation's work. A failed
*commit*, and a device fault that leaves the allocation map's image ambiguous,
abandon the whole transaction back to the last published root; a handle that had
already reported operations into it forces itself read-only rather than serving
writes it can no longer honour, while a transaction carrying only the failing
operation's own work leaves the handle writable exactly as an unbatched commit
failure always did.

Under memory pressure the `tairix_reclaim::PressureGauge` shortens the window
and lowers the ceiling toward the §6 floor: the response to pressure is *flush
sooner*, never *allocate more* and never *drop*. The gauge is read once per
operation, so the band the window is aged against is the band that operation
saw, and a deepening band re-reports the open transaction's deadline rather than
letting it wait out a window measured when memory was plentiful.

## 6. The bound — done

`WritebackBound` (`wcache.rs`), installed by `ARXFS::with_writeback_bound` and
assembled per mount by `kernel/tairix-kernel::writeback_bound`.

- **Ceiling** derived from discovered RAM (`AGENTS.md` §24.1), not a constant:
  `CacheBudget::from_backing(cache_backing_bytes())`, the same derivation the
  volume's clean caches use. Reaching it forces a commit.
- **Pressure-governed** by the same gauge every other cache reads, halving the
  ceiling per band and the dirty-age window with it. One gauge reading per
  operation, not per staged block: in the kernel that reading is the physical
  frame allocator, so per-block would take the global frame-allocator lock
  hundreds of times for an answer that cannot change inside one operation.
- **Machine-wide, and shared**, so several volumes on a 1 GiB machine hold a
  bounded *total* rather than each taking a slab — the combined floor every
  storage design is held to. The
  derived figure is the *machine's* ceiling, not a volume's, and a volume may
  hold an equal share of it — `tairix_reclaim::PinnedShare`, one instance the
  host installs on every volume's pinned ledger, carrying the live total across
  them and how many are drawing — capped further by what the volumes already
  holding leave, and by the machine-wide reserve floor every consumer obeys
  (`permits_reserve`). A volume holding nothing counts for nothing, so a
  machine whose other volumes are empty leaves the whole ceiling to the one that
  is writing, and a single volume behaves exactly as it did before the share
  existed. The equal share is what bounds a *delete*-heavy machine, whose
  volumes hold almost all of their memory in run bookkeeping rather than in
  blocks the siblings can see.
- **Floor** of one coalesced device transfer (`RUN_BYTES`), which the ceiling
  never falls below. It is where the cache stops paying for itself — under one
  transfer the drain cannot form a full run — so a machine whose ceiling cannot
  reach it refuses the mount rather than accepting it and leaving it to commit
  after almost every record.
- **Forward progress does not depend on the ceiling.** The plan's original floor
  was "one transaction's own working set", which is not a bound: a file write's
  staged bytes scale with a caller's argument, so no fixed floor can hold one.
  The write path is instead the thing that yields — it stores at least one record
  whatever the ceiling, then stops on a record boundary and reports the count,
  as `write(2)` may — and the operation's close publishes, so the next call
  proceeds against an empty set. A caller with an indivisible value (a symlink
  target, bounded by the ABI) asks for the whole of it and the bound bites at
  the operation's end. `FilesystemWrite::write_all` is the one place the resume
  loop lives for every caller that needs the whole value stored.
- **Over the transaction, not only its blocks.** The ceiling counts the
  transaction's run bookkeeping (one `RUN_ENTRY_BYTES` per held run,
  `Allocator::txn_pinned`)
  with its staged blocks, because both are pinned on the same terms and an
  operation that *frees* space holds almost all of its memory in the runs:
  freeing a maximally fragmented file dirties a spine's worth of blocks whatever
  its extent count, so a ceiling over the blocks alone would not have bounded a
  delete at all (`plans/OPEN-DEFECTS.md` D67).
- **Accounted as pinned** through `tairix_reclaim::PinnedLedger`, a row of its
  own in the §16.6 cache-ledger export, carrying the whole figure the ceiling
  governs — staged blocks, undo copies, and run bookkeeping — published by the
  driver at each point a decision is taken against it, which is what the share
  the *other* volumes decide against has to be fresh for. A dirty block is deliberately *not*
  admitted through the `ReclaimCache` classification gate, because that gate's
  contract is droppability — and for the same reason the pinned row is a class
  the per-class reclaim totals drop by construction: counting memory that can
  only be written out as reclaimable headroom would stall `ramzip` waiting for
  memory nothing can free.

## 7. Failure and hygiene

- A drain that faults: report the typed `DriverError`, `rollback()` the
  transaction (restoring the saved roots — the existing path), poison the set so
  the next operation re-derives from the last committed root. Blocks the failed
  transaction allocated are already txn-private and reclaimed by the rollback.
  No retry loop, no partial publish.
- A `flush()` that faults is a failed barrier: the slot is not written, so the
  transaction did not happen.
- Released buffers are volatilely wiped, like every other buffer in the driver.
  Entries hold sealed (encrypted) bytes, so no plaintext is ever retained, and
  the driver declares no `BufferClass` on any write: every block it sends is
  ciphertext or authenticated metadata. A `Sensitive`-class bypass therefore has
  no producer, and implementing one would be speculative surface — the seal path
  is what keeps the set free of secrets, not a class flag.
- A read-only handle can never stage a block: `stage_block` refuses one, as do
  `commit` and the device write itself, so the set stays provably empty and
  costs a read-only mount nothing.

## 8. Staging

Each stage is one session's work, ends with the whole-project gate green
(`AGENTS.md` §7), and updates this file's status plus §22 of the spec before it
is reported.

### WB0 — measurement harness. **done**

`drivers/filesystem/arxfs/tests/write_amplification.rs` is the write path's
device-command ledger: an in-RAM device recording every command it is issued, in
order — each write's start block and run length, and each cache barrier. One
ledger yields the commands a window costs, the blocks they carry, how many of
those a later write in the window supersedes, and the run-length histogram; the
recorded order is also what a barrier is proved by, so WB1 asserts against this
fixture rather than a second one. The device stores only the blocks written, so
one workload can be priced on a small volume and on a volume far larger than the
host's RAM.

The §1 table is that harness's output, asserted row by row — six rows over four
workloads and both block sizes, every figure exact, with the amplification
checked as an integer ratio rather than a float. Two of its assertions record
the present rather than a goal, each a later stage's acceptance hook: every
write carrying exactly one block (WB2 moves that histogram) and the chunked case
costing far more than the single call's commands (WB4). The floor case runs every
workload again on a 100 TiB volume and requires an identical command stream,
which is what makes the table a property of the write path and not of the device
it was measured on.

### WB1 — dirty block set and the commit barrier. **done**

`wcache.rs` holds the physical-block-keyed `DirtySet`: sealed blocks, replaced
on rewrite, dropped unwritten when the transaction frees the block again, wiped
as they leave, and handed to the drain in ascending device order. It performs no
I/O and is host-unit-tested, exactly as `pagecache.rs`. `write_meta` and
`seal_data_block` stage through it; `read_block_run` reads through it, so a
read-after-write inside the transaction sees the staged bytes and a wholly
staged run needs no device request. `commit` drains, barriers once, then writes
the slot pair.

*Measured (the WB0 harness, asserted row by row).* A 64 KiB `write_at` on a
512-byte volume: **746 device writes → 158**, of which 598 metadata writes
become ten, 294 superseded writes become none, and 373 KiB on the device becomes
79 KiB — 5.82× byte amplification down to 1.23×. At 4096: 89 → 25 writes, 5.56×
→ 1.56×. The chunked case, which C2 owns, falls 1183 → 335 and 284 → 160. Every
commit issues exactly one barrier, and the only writes after it are the slot's
two copies.

*Fixed with it, each with a regression test that fails before and passes after.*
Three ordering defects the barrier work exposed, none of them observable on the
strictly-ordered devices the suite used to run on:

1. **A commit that failed after its first slot copy published the transaction
   while the caller rolled it back**, freeing the published root's blocks for
   immediate reuse — whole-volume loss on the next mount from a single
   media error at that LBA. The commit point is now the *primary* copy, written
   last, and a slot-write failure leaves publication unknown, so the handle
   forces itself read-only and frees nothing rather than guessing.
2. **`scrub`, `check`, and `health` propagated a failed `commit()` without
   rolling back**, leaving the handle holding an unpublished transaction that
   the next commit would publish behind the caller's back. `commit` now rolls
   back its own failure, so the property belongs to the primitive rather than to
   fifteen call sites.
3. **The allocation map's clean→dirty stamp was not barriered before the first
   page write**, so a reordering device could keep a page while the
   invalidation sat in its cache; the next mount would adopt a map stamped clean
   at a generation it no longer described, and a page carrying a committed
   transaction's frees would mark live blocks free. One barrier at the
   transition — once per sync period, not per write — closes it. The marker is
   staged with the authoritative commit phase, so the commit's existing barrier
   makes it durable without a second normal-path barrier.

The test surface is the WB0 ledger for the command shape (one barrier per
commit, nothing but the slot pair after it) and a volatile-write-cache
`MemBlock` for the consequence: after a commit the only uncommitted blocks are
that pair, and a power loss keeping any subset of them leaves the prior
committed state or the new one, both whole.

### WB2 — run coalescer. **done**

`DirtySet::drain` gathers its ascending order into physical runs and issues one
`write_blocks` per run. One pass both decides a run's length and lays its bytes
into the gather window, so the blocks the set releases and the bytes the device
is handed cannot disagree; a run stops at the first address the set does not
hold, so it can never name a block outside the transaction or reach past the end
of the device. The bound is the read path's transfer window, hoisted to one
definition (`RUN_BYTES`) with one staging type (`RunWindow`) consumed by both
directions, and the driver's single device write became `write_run`, which
refuses a ragged or empty run rather than leaving the device to interpret it.
The window is a fallible reservation sized to the transaction's *longest* run
rather than to the bound — the same sizing the read path applies to a small
read — and a machine too short of memory to hold one writes block by block from
the set instead, so a commit costs commands rather than failing.

*Measured (the WB0 harness, asserted row by row with the exact run-length
histogram).* Identical bytes, far fewer commands: a 64 KiB `write_at` on a
512-byte volume **158 → 5** (a 128-block run at the bound, a 20-block
remainder, the four mirrored metadata pairs as one 8-block run, and the slot
pair), at 4096 **25 → 5**; a 34-byte append **11 → 4**; an empty-file create
**14 → 4** after a clean mkfs, its metadata one 12-block run plus the map
invalidation and slot pair. The chunked case, which C2 owns, falls
335 → 64 and 160 → 64. The barrier shape is unchanged: one barrier per commit
with nothing but the slot's two single-block copies after it, which stay two
separate commands because the primary *is* the commit point.

### WB3 — fold the allocation map's dirty pages in. **done**

Allocation-map pages enter the same `DirtySet` as copy-on-write blocks, in a
post-invalidation phase. Resident changes stay in the map's bounded page cache
between commits; eviction moves one page into the set and drains it, while
`fs_sync` moves all resident pages into the set, coalesces bounded runs, issues
one barrier, then writes the clean generation stamp. A page leaves the cache
once staged, and the transient gather window stays below the cache footprint.

The clean/dirty marker remains mandatory for the in-place map. The first
mutation after a clean sync stages its invalidation with the commit's
authoritative phase, so the existing pre-slot barrier makes it durable before
any map page may reach the device. Removing that marker would let a volatile
cache persist a page carrying frees while losing the slot that made those frees
lawful; the old clean generation would then adopt a map that can reallocate live
blocks. If a clean map fills its bounded cache before commit, eviction confirms
the invalid marker first rather than weakening this ordering.

Each ordinary commit and `fs_sync` uses one barrier, plus one for the map's
clean-to-dirty transition, paid once per sync period by whichever of the two
first has a page to write. A pass that publishes nothing — a clean `check` or
`scrub` — writes nothing and barriers not at all. A clean sync is adoptable; an
ordinary crash or any partial map-page persistence rebuilds from whichever
transaction root survives. The power-loss tests cover both roots and retained
none/all/even/odd map-page subsets, and the 100 TiB rebuild stays inside the
fixed resident-memory budget.

**A rebuild is a device-fault answer only.** A failed stage, page write, or
barrier — under a sync or under an eviction — poisons and drops both staging and
cache; the next check, write, or grow derives the exact map from the committed
trees before using it. Everything else undoes its own marks: an unpublished
transaction reserves its deferred frees again and releases its still-private
allocations, so a rollback costs the transaction and never the volume. That bound
is load-bearing, not incidental — an operation refused for an ordinary reason
(`create` over a taken name, `remove` of a missing one) changes nothing, and a
caller who may create a name must not be able to make each refusal walk every
tree on the volume. `tests/write_amplification.rs` holds it: a create after a
refused one reads exactly what it reads with no refusal before it, over 64 and
1024 directories alike, and a refusal reads no more than the create it refused.

Unknown slot publication restores the last published tree view, reserves the
union of both candidate roots — the frees the commit had already applied are
reserved back, its own blocks stay reserved — and freezes the handle read-only,
so nothing can erode that reservation before a remount reads it.

### WB4 — commit scheduler. **done**

`CommitScheduler` (`wcache.rs`) holds the open transaction's start reading, the
class window, and the count of operations already reported into it. `begin`
joins or opens; `end_operation` publishes when the window has expired, when the
operation widened the incompatible-feature word, or when there is no clock to
age against; the barrier-requiring operations publish before they run; `fs_sync`
publishes and then persists the map behind its own barrier. The dirty set gained
the per-operation savepoint the multi-operation transaction needs, and the
allocator's rollback record moved from "everything this transaction allocated"
to the two scopes §5 states.

*Measured (the WB0 harness, asserted row by row with the run-length
histograms).* The chunked case converges exactly: 64 KiB in sixteen 4 KiB calls
puts 159 blocks and 79.5 KiB on a 512-byte volume, as one call does, in 7
commands against 6, with nothing superseded and one barrier — where per
operation it cost 64 commands, 335 blocks, 24 of them superseded, and 16
barriers. At 4096 it is 26 blocks and 104 KiB either way, 7 commands against 6.


### WB-D1 — the host's write-back expiry timer. **done**

Found by WB4, and the reason WB4 landed driver-only: the scheduler could only
check its window from inside an operation, so a volume that fell quiet held its
transaction until the next one. The expiry now lives above the driver, as a
kernel task.

`FilesystemWrite::set_writeback_host` hands a mount the host's timer
(`tairix_abi::driver::filesystem::WritebackHost`: a monotonic clock and a
"this volume is due at *T*" report). `CommitScheduler` names its deadline as a
transaction opens and names its absence as one closes — the two places the
transaction state changes, so nothing above can forget — and reads its clock
through the same seam, so a handle can never measure a window without something
that will fire it.

Kernel side (`kernel/core/src/fs/writeback.rs`): the mount registry *is* the
bookkeeping. Each `DriverEntry` carries one `u64` deadline slot the driver
stores into lock-free and the flusher reads without touching the mount, so a
volume mid-operation never delays the scan; `LateFilesystem` implements
`WritebackHost` itself and is installed on every driver by `register` — one
choke point, every port, every mount kind. `publish_due` takes the due set
under the registry lock (consuming each deadline, so a fired one cannot re-arm
in the past), then publishes in deadline order with the lock released. One task
serves every mount: `writeback_service::start`, admitted from the shared
`finish_unlock` tail, parks on `WRITEBACK_WAITQ` with the soonest deadline any
volume published. A driver reports a *sooner* deadline through
`waitq::writeback_wake`, which flags the queue only when the flusher is armed
later than that — so a sync-heavy workload costs no task switch per commit, and
an idle machine arms nothing and takes no wakeup.

**Deferral is armed by the flusher and disarmed with it.** The registry's host
reads no clock until the flusher has proved it can park, and stops reading one
if the flusher ever ends — at which point every driver's next operation
publishes and the flusher publishes what is still held on its way out. So no
transaction is ever deferred against a timer that will not fire: a port with no
storage floor, a service that was not admitted, a scheduler hook that is not
wired all fall back to publishing eagerly.

*Measured (`kernel/tairix-kernel/src/writeback_service_tests.rs`, over a real
ARXFS volume whose device image the test re-opens).* An operation leaves the
name absent from the medium and the deadline at one window; the flusher
publishes it at the window with no operation touching the driver, and the name
is then on the medium. Eight operations move the timer **once** and cost one
commit. Three volumes dirtied a second apart are each published at their own
window, in order. A clean volume costs no barrier.

*Corrected with it:* the uncommitted-write retention journal stated that a
surprise removal can only lose what the *device* had not committed
(`kernel/core/src/fs/retained.rs`). With batching live it loses the open
transaction too — safely, since the last published root stands whole, but it is
a second loss the journal cannot replay, and the module now says so and names
the timer and the teardown flushes that bound it.

### WB5 — the bound and pressure. **done**

The design is §6. The set counts what it holds (staged blocks plus the
savepoint's undo copies, which are as unavailable to the machine as the staged
ones) and publishes it to the host's ledger as it changes, so a missed
publication can leave the row stale but never wrong. `end_operation` publishes
when the set has reached the operation's ceiling; `write_file` stops on a record
boundary when it has, which is where the bound meets the one operation whose
staged bytes scale with a caller's argument.

Two things the item had to add outside the driver. `tairix_reclaim` gained the
pinned side of the model (`PinnedAccounting`, `PinnedLedger`) and the ABI a
pinned class id for the cache-ledger row, because there was no honest way to
report unreclaimable bytes through a reclaim class — and the fold that builds
the per-class totals already drops an unknown class, so the exclusion needed no
new code, only the right id. And `FilesystemWrite` gained `write_all`, because
making a short write reachable makes every caller that needs the whole value
stored a caller that has to loop; four of them were checking the count and
failing instead.

*Measured (the WB0 harness).* A payload 24 transfer windows wide, written whole
through repeated short counts on a machine whose per-volume share is four:
peak pinned bytes stay inside the ceiling plus the one record the write was in
the middle of, the transaction is written out more than once, each forced commit
carries its own barrier, and the bytes read back exactly. A critical band pins
less and publishes more often than an unpressured one. The smallest machine a
volume may be mounted on — one transfer window per volume — completes the same
write, and completes it with a hundred tebibytes attached at both block sizes.
A read-only mount pins nothing.

*Fixed with it, each with a regression test.* The per-entry bookkeeping figure
every bounded pool charges on top of a payload was declared three times over
(`block_cache`, `transform_cache`, `retained`) as three private copies of one
value that is the same by construction; it is now one definition
(`tairix_reclaim::MAP_ENTRY_OVERHEAD`). `plant_nested_file` and three
`tools/mkimage` authoring paths turned a short write into a build failure rather
than resuming it, and the account-database commit turned one into a spurious
`EIO`; all now store whole through `write_all`.

### WB6 — acceptance and docs. **done, save the on-metal figure**

The combined floor now holds for *several* volumes, not one, and the batched
commit shape is crash-injected as the unbatched one always was.

**The combined floor (`AGENTS.md` §26.7).** Four 100 TiB volumes are mounted on
one machine and advance a slice each in turn, so every volume is holding what it
has staged while the others decide what they may stage. The gauge source is the
kernel's own in miniature — a staged block is a real allocation, so the free
reading falls as the volumes pin — and the figure asserted is the machine-wide
high-water mark, taken as the shared total moves rather than summed from
per-volume peaks the volumes never reached together
(`tests/write_amplification.rs`).

*Measured.* On a machine whose ceiling divides into shares well above one
device transfer, the four volumes together peak at **4 195 744** bytes against
a 4 194 304-byte ceiling — the ceiling plus a fraction of one record — where a
per-volume ceiling let them reach **8 670 016**, twice the machine's, with four
volumes and proportionally more with more. On the smallest machine a volume may
be mounted on, where the share is one transfer window, they peak at **263 200**
against 262 144. Every volume's payload reads back byte-exact and every volume
was forced to write out by the bound it shares.

*The defect the case found, and its fix.* The ceiling was derived **per
volume** — a sixteenth of discovered RAM each — so eight mounted volumes could
pin half the machine in memory nothing can reclaim, and the machine-wide
reserve this plan claimed bounded them does not bite until free memory has
already fallen to a sixty-fourth of RAM, which is an emergency floor and not a
bound. Measured before the fix: eight volumes pinning 2.08 MiB of a 4 MiB
machine, each at its own full ceiling. The derived figure is now the
*machine's*, and each volume takes a share of it (§6); the published figure is
the whole of what a transaction pins, so a delete-heavy volume is visible to its
siblings rather than only its staged blocks being; and a mount that goes away
gives its share back where its ledger row, which outlives it, would otherwise
keep claiming it. `tairix_reclaim` gained `PinnedShare` for the shared total.

**Crash injection across the batched shape.** The existing sweeps replayed one
transaction per operation; a batch is one transaction spanning several, which is
a stronger claim and was untested. The write-budget sweep now runs in both
shapes from one body (`Publish`), and two more cases cover the batched commit
itself:

- Every write count during a batch of three operations leaves either all of
  them or none. Where the device refused one of the three, that operation was
  reported failed and undone and the ones after it never ran, so what survives
  is a prefix; where all three succeeded, a partial outcome is a failure. Run
  unbatched, the same assertion fails at budget 6 — the sweep discriminates the
  shape rather than merely passing under it.
- A power loss straight after the commit that publishes a batch, over every
  combination of the slot pair the device's volatile cache may keep: the primary
  copy selects the batch, and when it lands every operation in it is *readable*,
  because every block the batch's root names crossed the one barrier first. The
  batch is closed by its dirty-age window rather than by a sync, since a sync's
  trailing barrier would commit the device cache and leave a power loss nothing
  to drop — and that is also the close a real idle volume gets.
- A batch the ceiling forces out mid-way publishes whole operations: a crash
  straight after keeps no more than the caller was told was written, and every
  published byte is exact.

**Still open: the on-metal Pi 4 SD figure.** It cannot be taken from a host
gate — it needs the board — so it is an on-metal acceptance item like the
others in `plans/PI.md`, with the procedure fixed here so the number is
comparable rather than anecdotal. Nothing in the tree waits on it: what the
write path costs a device is already machine-checked as a command ledger (§1),
and the throughput figure is what confirms the ledger's fewer commands are
fewer seconds on real silicon.

1. Build and flash the debug image at the pre-write-back revision
   **`87b7d3e7`** (the WB0 harness, no dirty set):
   `cargo xtask image --target aarch64-rpi --profile debug`.
2. Boot the Pi 4 from SD, log in, and run one fixed workload:
   `stress --hdd 1 --hdd-bytes 64m --timeout 0`. Record the elapsed seconds
   from its completion line. Repeat three times and keep the median; the run
   writes and verifies, so the figure is a write-then-read-back rate.
3. Repeat both steps on the current tree.
4. Record the two elapsed times and their ratio in this section, and mark the
   §18 stage-17 row of the spec `✓`.

Acceptance: the multi-volume floor case passes with bounded resident dirty
bytes, no panic and no busy-spin (**done**, figures above); the crash sweeps
pass in both commit shapes (**done**); the measured Pi 4 SD improvement is
recorded as a number rather than a claim (**outstanding**, procedure above).

## 9. Explicit non-goals

- A journal. ARXFS is copy-on-write; a write-ahead log would be a second
  crash-consistency mechanism (`AGENTS.md` §2.2).
- A second dirty layer anywhere else in the stack. The kernel block cache stays
  a read cache.
- Any mount option, ioctl, per-file policy, or build feature that changes the
  behaviour (`arxfs-spec.md` §1).
- Weakening the barrier, the mirror rule, the seal path, or the integrity
  trailers for speed (`AGENTS.md` §2.16 order of precedence, §2.17).
- Caching *clean* blocks: that is the kernel block cache (SMART11) and
  `CachedFs`, already built.
- A device-side write-cache-disable. ARXFS assumes a volatile cache exists and
  barriers correctly; it does not ask the device to be slow.

## 10. The write-back expiry timer — done

Where the driver's window is *enforced*. The design and its measurements are
the WB-D1 entry in §8; this section holds the parts of it a reader of §2/§5
needs to know.

**It belongs above the driver, where Linux also puts it.** ARXFS owns no
thread and runs only inside a caller's operation, so it can check its window
but never wait for it. The kernel therefore runs the timer: one task parks on
the soonest deadline any mounted volume has published and calls the ordinary
`FilesystemWrite::flush` on each volume that is due.

**The driver names the instant; the kernel never guesses it.** The alternative
— the mount layer presuming a volume is dirty from the operations passing
through it — would flush a volume that had already published (a wasted device
barrier, since `flush` always forces the cache) and would depend on every
mutating path remembering to mark. Reporting from `CommitScheduler` instead
puts the notification at the one place the transaction state changes, so it
cannot be missed, and the window policy stays the driver's single definition
rather than a second copy the kernel ages against.

**It is event-driven, not a sweep** (`AGENTS.md` §2.23). One timer arm per
batch, no periodic wakeup, nothing armed while no volume is dirty, and a fired
deadline consumed so it cannot re-arm in the past. A driver that reports a
*sooner* deadline than the flusher is armed for flags the queue; a later one
needs no wake, because the flusher recomputes the soonest deadline every time
it runs.

**Deferral exists only while something can fire it.** The host reads no clock
until the flusher has parked once, and stops reading one if it ever ends — so
every fallback (no storage floor, a service not admitted, an unwired scheduler
hook) is *eager publication*, never a deferred transaction with no timer. That
is the trade `AGENTS.md` §2.17 forbids making the other way round.

**One flusher, in deadline order.** A task per mount would cost a kernel stack
per attached volume and a spawn/teardown per hotplug, for a saving only a
machine with several simultaneously-dirty volumes would notice. It blocks on
each mount's lock rather than skipping a busy one: skipping would drop a
consumed deadline, and an operation that *fails* rolls back leaving the
transaction open, so a skipped volume could keep its transaction with nothing
left to fire.

## 11. Adjacent findings this plan does not own

Recorded here because the measurement surfaced them, each to be raised on its
own rather than folded in silently (`AGENTS.md` §2.18):

- **Filesystem block size is pinned to the device's logical block size.**
  `bootstrap` takes `block_size` straight from `geometry()`, so a 512-byte SD
  card gives ARXFS 512-byte blocks: 443 bytes of usable content per block (73
  bytes of per-block trailers), 384 bytes of B-tree node payload, and therefore
  far deeper trees and ~8× the extent records of a 4 KiB volume. Decoupling the
  filesystem block size from the device's — formatting at 4 KiB (or wider on
  flash) over a 512-byte device — is a larger, separate on-disk change and is
  **not** in this plan. It compounds with C1–C3 and is the next thing to
  measure after WB6. Owned as spec stage 19 by
  `plans/IMPLEMENT-OUTSTANDING-ARXFS.md` §5.
- **`scrub`, `check`, `trim`, `health`, and `rescue` have no production
  caller.** They are implemented, tested, and capability-gated, but nothing in
  the kernel, a service, or a command app invokes them, so on a live system TRIM
  never issues, scrub never runs, and the health baseline never advances past
  mkfs. There is no `arxfs` command app. Now owned by
  `plans/ARXFS-MAINTENANCE.md` (spec stage 18), which sequenced behind WB1
  because a background writer on a barrier-less commit path would have
  multiplied C4's exposure across every maintenance pass. WB1 is done, so that
  dependency is met.
