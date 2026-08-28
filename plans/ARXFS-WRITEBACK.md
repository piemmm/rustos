# ARXFS-WRITEBACK.md — ARXFS write-back cache, commit batching, and the commit barrier

Status: **WB0–WB3 done** (measurement, the dirty set and commit barrier, run
coalescing, and allocation-map integration); **WB4 next**, WB5–WB6 planned.
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
barrier, coalescer, and allocation-map integration are present; the scheduler
and RAM-derived bound remain.

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
commit issues exactly one barrier. The figures are properties of the write path,
not of the device measured on: the harness reproduces each of them on a 100 TiB
volume, thirteen million times the size, to the command.

Amplification is structural, not granular. It had four separate causes, one
stage each so each win is separately measurable. **C1 and C4 are closed by WB1
and C3 by WB2**; C2 remains.

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

**C2 — one transaction per operation.** Every mutating operation ends in
`commit()`: a fresh transaction root (2 writes, 1 seal) plus a superblock slot
(2 writes, 1 seal), now with a barrier between them. Pure per-operation overhead
however small the operation, and it forces each transaction's churn out to the
device at every operation boundary — which is why the chunked 4 KiB case still
costs 2.1× the single-call command count at a 512-byte block size and 6.4× at
4096, for identical bytes, and is the only case left that supersedes anything.

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
  surviving state is, bounded by §5's deadline.
- **One barrier per commit, and it is mandatory.** Every dirty block except the
  superblock slot reaches the device, then `flush()`, then the slot. A commit
  that cannot barrier does not publish.
- **A dirty block is pinned memory, not reclaimable cache.** It cannot be
  dropped, only written, so it is bounded by back-pressure (force a commit) and
  never by eviction. It does not go through `tairix_reclaim::ReclaimCache`,
  whose every class is clean and whose refusal path — serve uncached — has no
  meaning for a block that exists nowhere else.
- **Forward progress is guaranteed.** The bound has a floor of one
  transaction's own working set. A volume that cannot hold that could not
  commit at all, so the floor is a correctness property, not a tuning choice.
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

## 5. The commit scheduler

A transaction stays open and the next operation joins it. It closes on the
first of:

- an explicit `FilesystemWrite::flush` (`fs_sync`) — closes and issues the
  trailing barrier;
- the dirty set reaching its byte ceiling (§6) — back-pressure: the writer waits
  for real I/O rather than the set growing;
- the **dirty-age deadline** expiring;
- an operation that needs a barrier for its own correctness: `trim` (discard
  eligibility reads the *committed* allocation map, so a block an open
  transaction reallocated must not be discardable), `grow`, `scrub`, `check`,
  `health`, `rescue`, `unmount`, and any operation that widens the
  incompatible-feature word;
- the handle being dropped or the volume handed on.

The deadline is a function of `Block::device_class()`, which the seam already
reports and the driver currently ignores:

| Declared class | Rationale | Window |
|---|---|---|
| `Removable` (SD, eMMC, USB) | highest per-command cost, the measured bottleneck, and the class that gains most | longest |
| `Rotational` | seek-bound; batching converts scattered metadata into sequential runs | middle |
| `SolidState`, `Virtual` | already cheap per command; a long window buys little and risks more | shortest |

Concrete values are fixed in WB4 against measurement, expressed as one policy
function over the class — never a global constant, and never a per-volume knob.
`close()` does **not** sync: POSIX semantics, and the write-then-close workloads
(image builds, bundle extraction, `cp` of many small files) are exactly the ones
the batching exists for. A program needing durability calls `fs_sync`, as it
must on any other system.

Under memory pressure the `tairix_reclaim::PressureGauge` shortens the deadline
and lowers the ceiling toward the §6 floor: the response to pressure is *flush
sooner*, never *allocate more* and never *drop*.

## 6. The bound

- **Ceiling** derived from discovered RAM (`AGENTS.md` §24.1), not a constant.
  Reaching it forces a commit.
- **Floor** of one transaction's own working set, so a transaction can always
  complete. Below the floor the mount fails closed at open rather than wedging
  later.
- **Pressure-governed** between the two, by the same gauge every other cache
  reads.
- **Accounted as pinned**, charged to the reclaim ledger so the memory is
  visible in the §16.6 accounting even though it is not reclaimable. A dirty
  block is deliberately *not* admitted through the `ReclaimCache` classification
  gate, because that gate's contract is droppability.
- **Bounded per mount**, so several volumes on a 1 GiB machine share a bounded
  total rather than each taking a slab (`AGENTS.md` §26.7).

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

Each ordinary commit and `fs_sync` uses one barrier on the normal path. A clean
sync is adoptable; an ordinary crash or any partial map-page persistence rebuilds
from whichever transaction root survives. The power-loss tests cover both roots
and retained none/all/even/odd map-page subsets, and the 100 TiB rebuild stays
inside the fixed resident-memory budget. A failed page write or sync barrier
poisons and drops both staging and cache; the next check, write, or grow derives
the exact map from the committed trees before using it. Unknown slot publication
restores the last published tree view, reserves both candidate roots, and freezes
the handle read-only.

### WB4 — commit scheduler. **next**

Multi-operation transactions, the device-class deadline policy, the
barrier-requiring operation list, and `fs_sync` semantics. Fixes C2.

Acceptance: the chunked-4 KiB case converges on the single-call cost; each
barrier-requiring operation demonstrably closes the transaction first; `fs_sync`
makes prior operations durable and issues the trailing barrier; `close()` does
not sync; the deadline is a pure function of the declared class, with a test per
class.

### WB5 — the bound and pressure. **planned**

RAM-derived ceiling, the forward-progress floor, gauge integration, pinned-
memory accounting, and back-pressure.

Acceptance: a writer that outruns the device is throttled by forced commits, not
by growth; rising pressure shortens the window and lowers the ceiling to the
floor and no further; the floor guarantees a transaction completes on the
smallest supported machine; the ledger reports the pinned bytes.

### WB6 — acceptance, hardware, and docs. **planned**

On-hardware Pi 4 SD measurement against the WB0 baseline; the §26.7 combined
floor (small discovered RAM with several 100 TB volumes mounted and writing at
once); crash-injection across the new commit shape; fuzz unchanged in surface
but re-run; `arxfs-spec.md` §22, `docs/src/filesystem/arxfs.md`, and the driver
`README.md` updated; this file replaced by its done-state summary.

Acceptance: the measured Pi 4 SD write throughput improvement is recorded as a
number, not a claim; the floor test passes with bounded resident dirty bytes, no
panic and no busy-spin.

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

## 10. Adjacent findings this plan does not own

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
