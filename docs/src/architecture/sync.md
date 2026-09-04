# `lib/sync` — synchronisation primitives

`lib/sync` is delivered by **Stage 2.1** of [`PLAN.md`](../../../PLAN.md). It
provides the synchronisation primitives every other kernel crate depends
on, so they land first and never need to be retrofitted (`AGENTS.md`
§2.4).

The crate needs only `core`, never `alloc`. That is deliberate and
load-bearing: a `no_std` binary whose crate graph includes `alloc` must supply
a `#[global_allocator]`, so a single allocating primitive here would force a
heap onto the freestanding boot binaries that deliberately have none — and
push them into hand-rolling their own lock instead. A primitive that must
allocate does not belong in this crate.

## Primitive catalogue

| Primitive | When to use | When **not** to use | Ordering | Safe at IRQ level |
| --- | --- | --- | --- | --- |
| `SpinLock<T>` | Short critical sections; low contention; no interrupts. | Anything that can sleep, or be touched from a hardware interrupt. | `Acquire`/`Release` on the lock word. | Process / kernel-thread context only. |
| `IrqSafeSpinLock<T, I>` | Same as `SpinLock`, but the data may also be touched from an interrupt handler on the *same CPU*. | Across an interrupt boundary on a different CPU without the arch's `InterruptControl` impl wired up. | Same as `SpinLock` plus arch-supplied save/restore. | Any IRQ level supported by the plugged-in `InterruptControl`. |
| `RwLock<T>` (writer-preference) | Read-mostly data with occasional writers. Writers are never starved. | Interrupt handlers (use `IrqSafeSpinLock` or `SeqLock`). | `Acquire`/`Release` on a packed state word. | Process / kernel-thread only. |
| `McsLock<T>` | High-contention critical sections; need FIFO fairness and no cache-line storms. | Low contention (`SpinLock` is cheaper); interrupt handlers (queue is unbounded). | `AcqRel` swap on tail; `Release` store on successor flag. | Process / kernel-thread only. |
| `SeqLock<T>` | Read-mostly `T: Copy` data where readers must never block (vDSO time, statistics counters). | Multiple concurrent writers; large payloads. | Sequence-counter validation with `Acquire` re-sample. | Readers safe at any IRQ; writers must serialise themselves. |
| `OnceCell<T>` / `Once<T>` | Set-once or lazy-init data. **No panic on poison** — the API returns `Result`. | Inside interrupt context against the same cell as the initialiser (busy loop). | `Release` publication; `Acquire` observation. | Process / kernel-thread only. |

### When a critical section may *sleep* — `SleepLock` (kernel-side)

Every primitive above **spins** on contention, which is correct only when
the holder never gives up the CPU. A critical section that may **park** — most
importantly one held across a block-device completion-IRQ wait
(`Block::read_blocks` parks the calling task on the controller interrupt) —
must not use a spin lock: a second contender on the same CPU would deadlock,
and on another CPU it would busy-spin on a sleeping holder (forbidden
busy-waiting, `AGENTS.md` §2.23).

For that case the kernel provides `tairix_kernel_core::SleepLock<T>`: a
scheduler-blocking mutex whose contenders **park off the run queue** and are
woken on release. It cannot live in `lib/sync` because parking/waking is the
scheduler's job and the layering forbids a `lib/*` crate from depending on the
kernel; it reaches the scheduler through the installed `WaitQueueArch` hook and
the kernel's `reschedule_current` park primitive, reusing the same
register-before-re-test lost-wakeup discipline as the other kernel waiters. Use
it for the per-mount filesystem lock and any other mutual-exclusion region that
must be held across a park; keep using the `lib/sync` spin locks for short,
non-sleeping critical sections.

Its uncontended acquire and release are one compare-exchange each, and the
wait queue is not touched. That matters because the lock also serialises
*every* block-device operation on a shared disk, so a filesystem read walking
a file pays one acquire/release per device operation — and an operation served
from the block cache above the disk is a memcpy, not a park. What makes it
possible is that contention lives **in the lock word**: a contender sets a
`CONTENDED` bit there before it parks, so the releaser's single
`LOCKED -> 0` compare-exchange fails precisely when a wake is owed. Flag and
lock bit share one location, so their modification order is total and no
store/load fence is needed — a contender that publishes before the release
makes that release take the wake path, and one that publishes after it
observes the lock already free and never parks. Holding "is anyone waiting?"
anywhere else would have cost the wait-queue lock on every release just to
learn that nobody was. The bit may linger over a handoff or over a contender
that found the lock free; that costs one release which looks at the queue,
finds it empty, and clears the word.

That reasoning holds for the fast path, which is itself a read-modify-write
of the word and so cannot miss a bit set before it. It does **not** extend to
the slow path deciding "the queue is empty, so drop the word", and treating it
as if it did cost a silent boot hang: a contender registering after that queue
scan set `CONTENDED` in the word the scan's release then wiped, having already
read `LOCKED` as set and committed to park, after which every release took the
fast path and never looked at the queue again. The slow path therefore
releases `LOCKED` *first*, keeping `CONTENDED`, and reads the queue once more
afterwards — the mirror of the contender's register-then-test, so whichever of
the two read-modify-writes runs second observes the first. A contender found
by that second look has the lock retaken for it, so the FIFO handoff below is
unaffected; only when the second look is also empty is `CONTENDED` dropped.

Release keeps its FIFO discipline: ownership is published directly to the
oldest waiter with `LOCKED` still set, so a fresh contender cannot barge ahead
of it. A waiter that has vanished between observation and wake is passed over
for the next-oldest rather than unlocking with the queue still occupied —
doing that would clear the word with it, so no later release would owe the
remaining contenders a wake and they would park for good.

## What a spin round does — the port's spin service

Every primitive above spins on contention, and a spinning CPU is by
definition waiting on another CPU to make progress. Where that other CPU is
simultaneously waiting on *this* one, only this CPU can break the wait — so a
spin round is the point at which it discharges whatever a peer is owed.

`spinwait::spin_wait()` is the one place a spin round is spelled. It runs the
service the port installed (`spinwait::install_service`, set-once, before
interrupts are first enabled and before any secondary CPU starts) and then
hints the CPU. Every primitive in the crate spins through it, which is what
makes the property total rather than a list of audited locks: it holds for a
primitive added later and for a caller no registry names.

The case that needs it is a cross-CPU TLB shootdown whose targets must
acknowledge in software. x86_64 has no broadcast invalidation, so the
initiator IPIs each target and waits — and a target inside
`IrqSafeSpinLock::lock` has masked its own interrupts for the whole acquire
spin, so the acknowledge cannot arrive by interrupt. If the lock it is
spinning for is one the initiator holds (the kernel heap's lock is exactly
that), both spin for ever. The x86_64 port therefore installs its
`tlb_shootdown::serve_pending` as the spin service; see
[the x86_64 platform page](../platform/x86_64.md). aarch64 (`tlbi …is`
broadcasts in hardware) and riscv64 (the SBI RFENCE is firmware-served) need
no acknowledge, install nothing, and pay one load and a branch per round.

The service runs on the calling CPU from an arbitrary spin round, so it must
be reentrant against its own interrupt handler, take no lock, and not spin.
The slot it lives in is a `core` atomic rather than the `loom_compat` shim on
purpose: it is written once at boot and never during a model run, so letting
`loom` explore it would multiply every interleaving for a location that
cannot race.

## Decision tree

```text
                       Need to share data between
                       multiple threads / CPUs?
                                |
              +-----------------+----------------------+
              |                                        |
        Yes, read-only after init                Yes, mutable
              |                                        |
        Once<T> / OnceCell<T>                Touched from an IRQ
                                            handler on the same CPU?
                                                  |
                              +-------------------+-----------------------+
                              |                                           |
                            Yes                                          No
                              |                                           |
                     IrqSafeSpinLock<T, I>                  Reader-heavy / Copy payload?
                                                                          |
                                              +---------------------------+-------------------+
                                              |                          |                    |
                                          Read-mostly Copy        Many readers,         Mutual exclusion
                                          (vDSO, counters)        rare writer                only?
                                              |                          |                    |
                                          SeqLock<T>                 RwLock<T>      Heavy contention? Need FIFO?
                                                                                              |
                                                                                  +-----------+------------+
                                                                                  |                        |
                                                                               Yes                       No
                                                                                  |                        |
                                                                              McsLock<T>             SpinLock<T>
```

## Ordering guarantees in detail

- **`SpinLock` / `IrqSafeSpinLock`** — `lock` performs a CAS with
  `Acquire` ordering; the guard's `Drop` performs a `Release` store.
  Every release-acquire edge orders the prior critical section's writes
  before the next holder's reads.
- **`RwLock`** — reader entry CAS is `Acquire`; reader exit is a
  `Release` decrement. Writer entry CAS is `Acquire` and writer exit
  clears the writer bit with `Release`. The lock is **writer-preference**:
  once `pending_writers > 0` no reader observes a successful `try_read`
  until the next writer has completed.
- **`McsLock`** — `lock` is an `AcqRel` swap on the tail pointer; the
  guard's `Drop` performs a `Release` store on the successor's `locked`
  flag, which the successor reads with `Acquire`.
- **`SeqLock`** — readers sample the sequence counter twice with
  `Acquire` plus an explicit `Acquire` fence; writers bump the counter
  to an odd value with `Release` before mutating and back to even with
  `Release` on commit.
- **`OnceCell` / `Once`** — initial publication is a `Release` store on
  the state word; every observer pairs it with an `Acquire` load.

## Testing

- **Unit tests** live next to the code (`lib/sync/src/lib.rs`,
  `#[cfg(test)] mod tests`).
- **Property tests** for the `RwLock` writer-preference fairness
  invariant live in
  [`lib/sync/tests/rwlock_fairness.rs`](../../../lib/sync/tests/rwlock_fairness.rs).
- **Loom-based concurrency tests** for `SpinLock`, `RwLock`, `McsLock`,
  `SeqLock` and `OnceCell` live in
  [`lib/sync/tests/loom.rs`](../../../lib/sync/tests/loom.rs).
  They are gated behind `#[cfg(loom)]`:

  ```text
  RUSTFLAGS="--cfg loom" cargo test --test loom \
      -p tairix-kernel-sync --release
  ```

- `cargo xtask test` runs everything except the loom suite on the
  default toolchain (loom requires `std`, which the rest of the kernel
  pipeline does not link).

## `unsafe` discipline

Every `unsafe` block in the crate carries a `// SAFETY:` comment per
`AGENTS.md` §2.10. The `SyncUnsafeCell` wrapper in
`lib/sync/src/loom_compat.rs` is the *only* place that re-exports the
underlying `core::cell::UnsafeCell` (or `loom::cell::UnsafeCell`) so that
the loom interleaving instrumentation is the single source of truth for
all primitives in the crate.
