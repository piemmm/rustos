# `lib/sync` — synchronisation primitives

`lib/sync` is delivered by **Stage 2.1** of [`PLAN.md`](../../../PLAN.md). It
provides the synchronisation primitives every other kernel crate depends
on, so they land first and never need to be retrofitted (`AGENTS.md`
§2.4).

The crate is `no_std`. Only the [`Epoch`](#epoch--guard--deferfree)
reclamation primitive uses the `alloc` crate (for its deferred-action
queue); the rest is pure `core`.

## Primitive catalogue

| Primitive | When to use | When **not** to use | Ordering | Safe at IRQ level |
| --- | --- | --- | --- | --- |
| `SpinLock<T>` | Short critical sections; low contention; no interrupts. | Anything that can sleep, or be touched from a hardware interrupt. | `Acquire`/`Release` on the lock word. | Process / kernel-thread context only. |
| `IrqSafeSpinLock<T, I>` | Same as `SpinLock`, but the data may also be touched from an interrupt handler on the *same CPU*. | Across an interrupt boundary on a different CPU without the arch's `InterruptControl` impl wired up. | Same as `SpinLock` plus arch-supplied save/restore. | Any IRQ level supported by the plugged-in `InterruptControl`. |
| `RwLock<T>` (writer-preference) | Read-mostly data with occasional writers. Writers are never starved. | Interrupt handlers (use `IrqSafeSpinLock` or `SeqLock`). | `Acquire`/`Release` on a packed state word. | Process / kernel-thread only. |
| `McsLock<T>` | High-contention critical sections; need FIFO fairness and no cache-line storms. | Low contention (`SpinLock` is cheaper); interrupt handlers (queue is unbounded). | `AcqRel` swap on tail; `Release` store on successor flag. | Process / kernel-thread only. |
| `SeqLock<T>` | Read-mostly `T: Copy` data where readers must never block (vDSO time, statistics counters). | Multiple concurrent writers; large payloads. | Sequence-counter validation with `Acquire` re-sample. | Readers safe at any IRQ; writers must serialise themselves. |
| `Epoch` / `Guard` / `defer_free` | Lock-free / RCU-style structures where the old version of an object must be freed once no reader can observe it. | Mutual exclusion; small payloads (use `SeqLock`); no-`alloc` contexts. | `SeqCst` pin; `Acquire/Release` on the deferred queue. | Process / kernel-thread only. |
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

       Replacing an entire data structure
       and freeing the old version only after
       all readers have moved on?
                |
            Epoch / Guard / defer_free
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
- **`Epoch`** — `Participant::pin` performs a `SeqCst` load of the
  global epoch and a `SeqCst` store of `active = true`, so any
  subsequent loads happen-after the global-epoch update they observed.
  `defer_free` and `advance` synchronise through the shared `SpinLock`
  on the deferred queue.
- **`OnceCell` / `Once`** — initial publication is a `Release` store on
  the state word; every observer pairs it with an `Acquire` load.

## Testing

- **Unit tests** live next to the code (`lib/sync/src/lib.rs`,
  `#[cfg(test)] mod tests`).
- **Property tests** for the `RwLock` writer-preference fairness
  invariant live in
  [`lib/sync/tests/rwlock_fairness.rs`](../../../lib/sync/tests/rwlock_fairness.rs).
- **Loom-based concurrency tests** for `SpinLock`, `RwLock`, `McsLock`,
  `SeqLock`, `OnceCell` and `Epoch` live in
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
