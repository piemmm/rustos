# FIX-SLEEPLOCK — A wake decision applies to the park it was taken about

Status: **done** (S1–S6 landed; gate status in the change that lands them).

Binding under `AGENTS.md`. Two defects, one root cause: a wait-queue row is
identified by task id alone, so a decision taken about one park is applied to
a later one — and a row whose task can never run again is still counted as a
waiter. The first strands a live task on a free lock; the second eats a
counted wake.

Read first: `kernel/core/src/sleeplock.rs`, `kernel/core/src/waitq.rs`,
`kernel/core/src/futex.rs`, `kernel/core/src/threads.rs` (`retire`),
`kernel/sched/api/src/policy.rs`, the three `kernel/sched/*/src/scheduler.rs`
`unpark` / `wake_from_parked` pairs, and `plans/OPEN-DEFECTS.md` D112 — whose
fix introduced the delete this plan makes precise.

## S1 — the strand (`OPEN-DEFECTS` D129)

SMP-only. At `-smp 4` the boot reaches the framebuffer `ARXFS passphrase:`
prompt and stops: no `/Drivers/input/*` bundle loads, so no keyboard works,
and all four cores end in `wait_for_interrupt` with the disk idle. ~50–60% of
runs at `-smp 4`, never at `-smp 1`. Two shapes: the driver-store scan kthread
strands (no store scan, service bundles still load), or an `fs_*` bundle loader
strands *holding* the per-mount `SleepLock` and the rest queue behind it (all
19 scan candidates logged, zero `application bundle loaded`).

`SleepLock::hand_off_oldest` reads, wakes, retracts and deletes across four
separate acquisitions of the wait-queue lock, naming its subject by task id:

```rust
while let Some(task) = self.waiters.oldest_task() {   // lock dropped here
    self.handoff.store(task, Ordering::Release);
    if self.waiters.wake_task(hook, task) { return true; }
    if self.retract_handoff(task) { return true; }
    self.waiters.deregister(task);                    // whatever row is there now
}
```

The reachable interleaving, with contender `T`:

1. Releaser reads `oldest_task()` → `T`, holding registration *A*.
2. Releaser publishes `handoff = T`.
3. `T`, resumed by an unrelated wake, deregisters *A*.
4. `wake_task(T)` finds no row and reports `false`. `unpark` is never called.
5. `retract_handoff(T)` succeeds — `T` has not reached its claim.
6. `T` fails its claim, fails `try_lock` (the handoff left `LOCKED` set),
   registers *B*, and parks.
7. `deregister(T)` deletes ***B***. The queue is now empty, so
   `release_and_recheck` clears `LOCKED` **and** `CONTENDED`.

`CONTENDED` clear means every later release takes the one-compare-exchange
fast path and never consults the queue again. `T` is parked for ever on a free,
uncontended lock. Whichever disk consumer was designated at that moment
strands, and anything queued behind a mount lock it held strands with it.

A second, independent hole makes the same delete unsound even when the row
identity is right: `Scheduler::unpark` returns `Err` when
`wake_from_parked`'s `Parked → Ready` compare-exchange loses to a concurrent
waker — a **live, runnable** task. `SchedWaitQueueArch::unpark`
(`kernel/core/src/init.rs`) documents the opposite ("an error means the task
can never run again"), and `hand_off_oldest` acts on that reading.

## S2 — a retired task's rows (`OPEN-DEFECTS` D130)

Nothing deregisters a task from a wait queue on its behalf: every waiter
removes its own row on its own way out. A task killed while parked therefore
leaves a row in every queue it was registered on, and scheduler ids are drawn
at random and never reused, so the rows accumulate for the life of the boot.

That is not only a leak. `wake_in_arrival_order` (`wake_one` / `wake_n`) and
`wake_key` count the unparks they *issue* and discard each result:

```rust
woken += batch.len();
for &id in &batch { arch.unpark(id); }
```

So `wake_n(arch, 1)` over a queue whose FIFO head is a retired task's row
reports `1` and wakes **nobody**. On the futex path — where `FUTEX_WAKE(n)`
releases a caller-chosen count — that is a lost wake-up: a live waiter stays
parked because a corpse spent its wake. `futex::deregister` drops a queue once
it is empty, so the corpse also pins its bucket entry alive.

## What the fix guarantees

1. **A decision about a registration applies to that registration.** A row is
   named by `(key, task, seq)`, so a waiter that resumed, deregistered and
   parked again is a *different* registration and inherits nothing.
2. **A row is removed only by its own waiter or after the scheduler says its
   task can never run.** Nothing else deletes another task's row.
3. **`unpark` reports `Err` only for a task that can never run again** — it is
   terminal, or the id names none. A wake that another waker's wake already
   satisfied is success.
4. **A counted wake counts landed wakes.** `wake_n(arch, n)` releases `n` live
   waiters or exhausts the queue; it never spends a slot on a row it could not
   wake.
5. **A retired thread leaves no rows anywhere.** Retirement drops them, and a
   queue the kernel cannot name self-cleans on its next wake.
6. **D112 stays closed.** A genuinely retired row is still reaped rather than
   left at the head of the queue, and a handoff is never published to a task
   that cannot claim it.

## S3 — registration identity in `WaitQueue`

`WaitSet` already mints exactly the generation this needs and does not expose
it: `next_seq` is monotonic and never reused, a re-`register` of a *present*
row keeps its `seq` (so a re-arming handler keeps its FIFO place), and a
`deregister` followed by a `register` mints a fresh one. So `seq` *is* the
registration identity; only the API is missing.

- `Registration { key, task, seq }` — opaque, `pub(crate)`, with a `task()`
  accessor for the handoff publication.
- `oldest_registration() -> Option<Registration>` **replaces** `oldest_task()`,
  whose only caller is the sleeplock (delete it).
- `wake_registration(arch, &reg) -> bool`: under the lock, require the present
  row's `seq` to equal `reg.seq` — a mismatch or an absent row returns `false`
  having touched nothing, because the designated waiter has resumed and is
  either about to claim or about to re-park. Otherwise drop the lock, `unpark`,
  and on a refusal re-take the lock, re-check the `seq`, and reap the row.
- Factor the three-index removal into `WaitSet::remove(id) -> Option<Waiter>`
  so `deregister_keyed` and the reap share one definition.

`hand_off_oldest` becomes a scan over rounds, and one round — `offer_to` —
publishes ownership, wakes, and withdraws on a miss. It removes nothing:

```rust
while let Some(reg) = self.waiters.oldest_registration() {
    if self.offer_to(hook, &reg) { return true; }
}
```

The round is its own function because it is also the only seam a test can
drive with a *stale* designation: nothing else can place a re-park between
reading the queue and acting on what was read. `retract_handoff` now reports
whether it withdrew (it used to return `true` for "did not withdraw", which is
how a predicate named for its action came to mean the opposite).

**It terminates.** Every round either returns or changes the head: the row is
reaped, or its waiter resumed, or a newer registration stands in its place.
No row can appear below the current minimum `seq` — a fresh `register` takes
`next_seq`, which only rises — so the head's `seq` strictly increases across
rounds and the scan cannot revisit one.

## S4 — one park/unpark handshake, with an honest error contract

All three policies carry a byte-identical `unpark`: `lookup` → state match →
wake-pending token → `SeqCst` fence → re-check `Parked`. That is the park state
machine, not scheduling policy, so it is hoisted rather than fixed three times.
Only the *placement* differs, and placement is policy.

- `kernel/sched/api` gains the handshake over the shared `TaskState` vocabulary
  — `unpark_task` and the `step` Park-commit's `commit_park` — parameterised on
  the policy's own "admit this woken task" step, plus the small trait over the
  per-policy `TaskInner` it needs (`load_state` / `cas_state` /
  `set_wake_pending` / `take_wake_pending`). Three live implementors and one
  live consumer, in this change.
- Each policy keeps `placement_for` / `admit_fresh_on` (cfq, eevdf) and
  `preferred_home` / `push_class` (mlfq), and nothing else of the handshake.
- The compare-exchange-loss arm re-reads the state: `Exited` is
  `Err(InvalidState)`; anything else is `Ok(())`, because the task is runnable,
  which is what the wake asked for. Bounded — no retry loop.
- **Delete the `cpu_state(target)?` that sits after the state commit** in all
  three. It is the only other way `unpark` can report `Err` for a live task,
  and in mlfq it additionally *loses* the task: `Ready`, unqueued, and an error
  returned. `admit_fresh_on` already routes an out-of-range CPU to the overflow
  list, which is the fail-safe that check was reaching for.
- `SchedulerPolicy::unpark`'s doc states the contract the callers rely on: a
  wake of an already-runnable task is `Ok`.

## S5 — no rows outlive their thread

`threads::retire` is documented as the per-thread half of **every** death, and
already clears the signal intake and the kill gate there. Dropping the thread's
wait-queue registrations belongs beside them, before the scheduler releases the
id.

- `WaitSet` gains `by_task: BTreeSet<(TaskId, WakeKey)>`, so one task's rows
  are a contiguous range and `WaitQueue::deregister_task(task)` is
  O(log n + rows) rather than a scan of `by_waiter`. It is the fourth
  cross-index, maintained with the other three and for the same reason.
- `futex::deregister_task(thread)` walks the fixed bucket table, calls
  `deregister_task` on each live key's queue, and drops the ones it empties.
  A bounded walk over a fixed table, paid once per thread death.
- **One list of queues, not three.** There is today no list naming every
  global queue: `TIMED_QUEUES` names nine and `DEFERRED_WAKE_QUEUES` five,
  and a third hand-maintained list would be a third thing to forget a queue
  in. Replace both with one `ALL_QUEUES` of `{ queue, timed, deferred }`
  entries; `nearest_timed_deadline` and `drain_pending_wakes` filter it on
  their own flag, and retirement walks all of it. `WaitQueue::new` stays
  `const`, so the statics are untouched.
- Every wake path reaps a row whose `unpark` reports the task can never run —
  `wake_in_arrival_order`, `wake_key`, `wake_waiter`, `sweep`, and
  `wake_registration`. This is what covers a queue retirement cannot name: a
  `SleepLock`'s private queue is embedded in the mount or device that owns it,
  reachable from no registry, and is dropped with its owner.
- `wake_in_arrival_order` and `wake_key` count a waiter only once its `unpark`
  has landed, and continue past one that did not, so a counted wake cannot be
  spent on a corpse (guarantee 4).

## S6 — the SMP coverage the gate does not have

`autoload-input-qemu-aarch64` is enrolled with `cpus: 1`
(`tools/xtask/src/commands/qemu_tests.rs`), so the whole
unlock → driver-store scan → autoload chain is only ever proven single-CPU —
which is why nothing in the gate has ever seen this. That vertical is *not*
the place to fix it: its 300 s TCG budget already covers boot, a bounded
PBKDF2, autoload, typed login, two app spawns and a pty round trip, and it
carries open defect D15 (an intermittent single-CPU freeze at its Ctrl-C
stage), so an SMP failure there would be ambiguous and a 4-vCPU budget miss
would be the load-dependent timeout §7 forbids.

The dedicated `cpus: 4` row is a **second enrolment of an existing guest**,
`tairix-test-netstack-autoload-qemu-aarch64` — unlock → store scan → autoload
a signed driver into its own user process → `devmgr` binds it to `netstack`.
Multiple enrolments of one binary are a designed-for feature of the table
(`sidecar_path` disambiguates each enrolment's planted image and serial log by
its `TESTS` index, and `sidecar_paths_never_collide_across_enrolments_or_replicas`
pins it), so the SMP coverage costs no new guest and duplicates nothing. That
chain rather than the graphical one because it reaches the same store scan and
user-space driver spawn without the desktop and pty stages, so a failure there
cannot be confused with D15.

The budget needs no headroom for the extra vCPUs: `timeout` is the
inactivity window, not a runtime deadline, and a strand shows up as silence.

## Tests

- **`sleeplock.rs`** — `a_designation_the_waiter_outran_deletes_nothing`
  drives `offer_to` with a designation its waiter has already left and
  re-parked behind, and asserts the live row survives and is handed off on the
  next round; `a_release_never_clears_the_word_over_a_live_waiter` asserts the
  invariant over a whole release; `a_reap_only_removes_the_registration_it_was_taken_about`
  covers the reap's identity check against a wake that reports "did not land"
  for a live task. The D112 tests still pass — a genuinely unwakeable row is
  still reaped and a live successor still reached.
- **`waitq.rs`** — `a_counted_wake_never_spends_itself_on_a_row_it_could_not_wake`
  (the S2 lost wake-up) and `a_retiring_task_leaves_no_row_on_any_key`.
  `an_addressed_wake_reports_the_landing_not_the_registration` additionally
  asserts the reap.
- **`kernel/sched/api`** — `park.rs`'s own model tests pin the handshake,
  including the lost-claim arm that is only reachable concurrently
  (`a_wake_whose_claim_lost_to_another_waker_still_reports_success` is the S4
  regression test); the `unpark_errs_only_for_a_task_that_can_never_run`
  conformance vertical pins the contract for all three policies through
  `run_all`.
- Each of the four defect tests fails against the pre-fix behaviour and passes
  after; verified by restoring that behaviour and re-running.

The test the plan expected in `futex.rs` is not written: the futex half is the
same `WaitQueue` code the `waitq.rs` pair already covers, and a third copy of
it addressed through `futex::deregister_task` would assert the bucket
bookkeeping, not the defect. The bucket half is covered by that function's own
`retain` dropping an emptied key.

## The oracles

- **miri.** No `unsafe` is added or changed: the sleeplock's `unsafe` is its
  `Send`/`Sync` impls and the guard's `Deref`, and every edit here is safe
  code over `BTreeMap`/`BTreeSet` and atomics. `kernel/core` is not enrolled
  (`OPEN-DEFECTS` D123, blocked on D128) and this change neither needs nor
  alters that.
- **loom cannot reach this crate, and the reason is worth recording rather
  than waving at.** `--cfg loom` does not compile `kernel/core` at all: loom's
  atomics and cell have no `const` constructor, and `WaitQueue::new` /
  `SleepLock::new` are `const fn` precisely so the sixteen global queues and
  the per-mount locks can be `static`s. Enrolling it means removing the `const`
  construction of every one of those, which is its own staged piece of work,
  not something to smuggle in here. The interleavings this plan turns on are
  therefore driven deterministically by the tests above, at the exact windows,
  rather than searched — and that gap is recorded as D131.

## Decisions, and what is deliberately not taken

- **The corpse reap stays.** D112's wedge was a handoff published to a task
  that could never claim it, and the reap is what keeps a dead row off the head
  of the queue. The fix is to make it precise, not to remove it.
- **A stale designation is retried, not repurposed.** Waking a task whose
  designated registration is gone would often work — it is still a waiter on
  this lock — but it conflates two parks again. The rescan designates its new
  registration on the next round and terminates (S3).
- **No per-task list of queues.** A back-pointer from a task to a
  `SleepLock`'s embedded queue would have to outlive a mount it cannot; the
  `by_task` index plus the one `ALL_QUEUES` walk covers every queue the kernel
  can name, and the wake-path reap covers the rest.
- **`Registration` is `pub(crate)`.** It is the sleeplock's ownership-transfer
  contract with the queue, not ABI, and nothing outside the crate designates a
  waiter.
