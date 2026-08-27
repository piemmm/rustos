# ARXFS-WRITEBACK.md — ARXFS write-back cache, commit batching, and the commit barrier

Status: **planned** (stages WB0–WB6, none landed).
Binding under `AGENTS.md` and listed in its §15.18 jump-sheet.
Primary code area: `drivers/filesystem/arxfs/`.
Companion spec section: `docs/src/filesystem/arxfs-spec.md` §22.

ARXFS today issues one single-block device write per copy-on-write block, one
transaction per VFS operation, and **no durability barrier at all**. That makes
writes both slow — measured 5.6×–20.1× byte amplification and, on a 512-byte SD
card, one device command per 512 bytes — and unsafe on any device with a
volatile write cache. This plan fixes both with one mechanism: a
transaction-scoped dirty block set, a run coalescer, a commit scheduler, and the
single barrier that becomes affordable once commits are batched.

It adds **no** capability, **no** ABI surface, **no** second data path, and no
mount option: the behaviour is the one production profile
(`docs/src/filesystem/arxfs-spec.md` §1), tuned from the device class the block
seam already reports.

---

## 1. The measured problem

Measured with the driver's in-memory device write counter, incompressible
payload, one file per case, counting `Block::write_blocks` calls:

| Workload | fs block size | device writes | bytes to device | byte amplification |
|---|---|---|---|---|
| 64 KiB in one `write_at` | 512 | 746 | 373 KiB | 5.8× |
| 64 KiB as 16 × 4 KiB `write_at` | 512 | 2571 | 1.26 MiB | 20.1× |
| 64 KiB in one `write_at` | 4096 | 89 | 356 KiB | 5.6× |
| 64 KiB as 16 × 4 KiB `write_at` | 4096 | 305 | 1.19 MiB | 19.0× |
| 34-byte append to an existing file | 512 | 13 | 6.5 KiB | 196× |
| create one empty file | 512 | 18 | 9 KiB | — |

Amplification is essentially block-size independent in *bytes*, because it is
structural, not granular. It has three separate causes, and the plan fixes each
one in its own stage so each win is separately measurable.

**C1 — intra-transaction rewrite churn.** Every `store_block` ends in
`extent_assign`, a B-tree insert that copy-on-writes the covering leaf and
writes it — and its companion mirror — to the device immediately. So the same
extent leaf, the same inode-tree spine, and the same allocation-map pages are
rewritten and re-sealed once per data block, and only the last version of each
survives the transaction. This is the dominant cost: 64 KiB at a 512-byte block
size is 148 data blocks, so **598 of the 746 writes are superseded metadata**,
each carrying its own HMAC-SHA256 over the whole block — which on a Pi 4, with
no ARMv8 crypto extensions, is a CPU cost as well as an I/O one.

**C2 — one transaction per operation.** Every mutating operation ends in
`commit()`: a fresh transaction root (2 writes, 1 seal) plus a superblock slot
(2 writes, 1 seal). Four writes of pure per-operation overhead however small the
operation, and it forces C1's churn out to the device at every operation
boundary — which is why the chunked 4 KiB case costs 3.4× the single-call case
for identical bytes.

**C3 — single-block device I/O.** `write_block` is the only device write site in
the driver and always writes exactly one block. `Block::write_blocks` already
accepts a multi-block buffer; the *read* path already coalesces into a 64 KiB
staging window (`RunStage`/`READ_RUN_BYTES`); `drivers/storage/emmc2` already
stages 128 blocks (64 KiB) per ADMA2 multi-block transfer. So on the Pi's SD
card every 512-byte write is a separate CMD24 plus completion wait where one
CMD25 could carry 128 of them. The write path is the only side of the driver
that does not coalesce.

**C4 — no commit barrier (a durability defect, fixed here).** `commit()` writes
the copy-on-write blocks, then the transaction root, then the superblock slot,
with no `Block::flush()` between any of them — while `src/transaction.rs`
documents the opposite ("each step flushed before the next"). On every device
with a volatile write cache the slot may reach stable media before the tree
blocks the root it names transitively references. `open` does re-validate the
root before accepting a slot, so a *lost root* is survivable; a **lost interior
tree node beneath a durable root** is not — both mirror copies are absent and
the mount fails closed. Only `map_persist` (explicit `fs_sync`) issues a barrier
today, so an ordinary commit has none.

C4 is a defect that exists now, filed as `plans/OPEN-DEFECTS.md` D63 and owned
by this plan. It is fixed in WB1 rather than alone, because a barrier per
unbatched per-operation commit would add a full device-cache flush to every VFS
operation on a driver already amplifying 5.6×–20.1× in single-block writes; WB1
lands the batching mechanism and the barrier in the same change. No ARXFS work
that depends on a durable published root — snapshots (spec stage 20), FEC commit
witnesses (stage 21) — may land before it.

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
- **No second anything.** One dirty set (the allocation map's private dirty-page
  set folds into it), one barrier, one staging-window bound shared with the read
  path, one integrity/crypto path (blocks enter the set already sealed), one
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

One new module, `drivers/filesystem/arxfs/src/wcache.rs`, entirely beneath the
driver's single existing device-write seam:

```text
VFS operation
  -> ARXFS operation (unchanged)
      -> seal path (header/HMAC, AEAD, integrity trailers — unchanged)
          -> write_block / write_meta  ..............  the one seam
              -> dirty block set          (WB1)
                  -> run coalescer        (WB2)
                      -> Block::write_blocks / Block::flush
```

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
| before the barrier | previous slot | prior — unchanged from today |
| between barrier and slot | previous slot (new root durable but unreferenced) | prior |
| slot write torn | previous slot (the slot fails its authenticator) | prior |
| after the slot | new slot | new, whole |

The window this closes is the one C4 opens: a durable slot naming a root whose
subtree never reached media. After WB1 that ordering is impossible.

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
  Entries hold sealed (encrypted) bytes, so no plaintext is retained; a
  `BufferClass::Sensitive` write bypasses the set entirely and invalidates any
  entry for its range, matching the block cache's rule.
- A read-only handle has no dirty set at all.

## 8. Staging

Each stage is one session's work, ends with the whole-project gate green
(`AGENTS.md` §7), and updates this file's status plus §22 of the spec before it
is reported.

### WB0 — measurement harness. **planned**

Tests only; no behaviour change. Land the write-amplification and
device-command-count regression tests as a reusable fixture (a counting device
recording call count, block count, and per-call run length), asserting the §1
numbers as the baseline every later stage measures against. Add the
run-length histogram assertion the coalescer will move.

Acceptance: the §1 table is reproduced by a test, not by a note.

### WB1 — dirty block set and the commit barrier. **planned**

`wcache.rs`: the physical-block-keyed dirty set, replacement on rewrite,
read-through for read-after-write, drain at commit, and the single pre-slot
barrier that closes C4. `write_block`/`read_block`/`write_meta` route through
it; nothing else in the driver changes.

Acceptance: C1 gone (the 148-block case writes each metadata block once, proved
by the WB0 counters); C4 gone (a device that records ordering proves no slot is
written before the barrier that precedes it); crash-replay across every commit
step still leaves prior-or-new at every write budget; a faulting drain rolls
back and publishes nothing.

### WB2 — run coalescer. **planned**

Drain in ascending physical order, gather adjacent blocks into runs, issue one
`write_blocks` per run, bounded by the staging window the read path already
uses — hoisted to one shared definition consumed by both directions rather than
a second constant.

Acceptance: C3 gone (device *command* count falls to the run count; the mirror
pair is one 2-block command); a short run still writes correctly; the bound is
respected; no run crosses a device end.

### WB3 — fold the allocation map's dirty pages in. **planned**

`allocmap`'s private dirty-page set and `map_persist`'s own `flush()` are the
same problem solved twice. Fold the pages into the one dirty set and the barrier
into the one barrier; the clean stamp still lands after it, for the reason it
does today (losing the stamp costs a rebuild, never correctness).

Acceptance: one dirty set and one barrier remain in the driver; the map is still
adopted after a clean sync and still rebuilt after a crash; the existing map
tests pass unchanged.

### WB4 — commit scheduler. **planned**

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
  measure after WB6.
- **`scrub`, `check`, `trim`, `health`, and `rescue` have no production
  caller.** They are implemented, tested, and capability-gated, but nothing in
  the kernel, a service, or a command app invokes them, so on a live system TRIM
  never issues, scrub never runs, and the health baseline never advances past
  mkfs. There is no `arxfs` command app. Tracked in
  `docs/src/filesystem/arxfs-spec.md` §18 (stage 16) rather than here.
