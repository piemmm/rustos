# Kernel scheduler

`kernel/sched` ships the architecture-neutral half of RustOS' SMP
scheduler. It owns the run queues and the dispatch policy; the
architecture ports (Stage 3 of [`PLAN.md`](../../../PLAN.md)) plug in the
real IPI and timer surfaces through the [`SchedulerArch`] trait.

This page is the source of truth for the *behaviour* the implementation
must preserve. Read it together with the rustdoc on `kernel/sched`'s
public items, and with [`AGENTS.md`](../../../AGENTS.md) §4 ("SMP from
day one") and §2 ("Non-negotiable rules").

## Algorithm

The scheduler is a **Multi-Level Feedback Queue (MLFQ)** with periodic
priority boosting, dispatched via **per-CPU work-stealing queues**. The
policy is the classical MLFQ as described in:

> Arpaci-Dusseau, R. H. & Arpaci-Dusseau, A. C. *Operating Systems:
> Three Easy Pieces*, ch. 8. <https://pages.cs.wisc.edu/~remzi/OSTEP/>

The run-queue is a bounded variant of the Chase–Lev work-stealing
deque (Chase & Lev, SPAA '05) with two RustOS-specific simplifications:

1. The buffer is bounded, so a misbehaving task source cannot
   amplify into a kernel-wide DoS (`AGENTS.md` §5).
2. The slot payload is the `Copy` `TaskId` (a `u64`), not a typed
   pointer; lost-CAS races therefore cannot leak or double-free.

### Bands and demotion

Three priority bands, in decreasing order of urgency:
`Priority::High`, `Priority::Normal`, `Priority::Low`. New tasks default
to `Normal`. A task that yields voluntarily
`yields_before_demotion` times at its current band is demoted by one
level (saturating at `Low`). A task that parks or exits does *not*
count as a yield.

### Periodic priority boost

To bound the worst-case starvation latency, every
`boost_interval_ticks` (measured against
[`SchedulerArch::ticks_now`]) the scheduler promotes every non-exited
task back to `Priority::High` and resets its yields-at-band counter.
The boost is fired by whichever CPU notices the threshold first; the
CAS on `last_boost_tick` makes it idempotent across cores. The boost
does *not* migrate tasks between queues — instead, the dispatch path
always reads the *task's* current priority, so the change is visible
from the next time the task is consumed.

This is what gives the starvation-freedom property exercised by the
`starvation_freedom_via_priority_boost` integration test:

> For any non-exited task `T`, the wall-time between two successive
> runs of `T` is bounded by
>
> ```text
> boost_interval_ticks · (1 + total_higher_priority_work)
> ```

### Per-CPU run queues + work-stealing

Each CPU owns three [`RunDeque`]s (one per band). A CPU's
[`step`]:

1. Performs a priority-boost check.
2. Drains the global overflow list (tasks that could not be enqueued on
   their home CPU's queue because it was full) into its local queues.
3. Consumes the highest-priority band that is non-empty.
4. If everything local is empty, **work-steals** from a
   pseudo-randomly selected victim CPU, scanning every band, then
   walks the CPU list circularly until either a task is found or
   every other CPU has been probed.

Both the local consume and the steal use the same `RunDeque::steal`
entry point — the queue is a single-end FIFO consumer, so MLFQ
fairness within a band is preserved. Push is wait-free; consume
is lock-free with a bounded retry loop on `Steal::Retry`.

### IPI-based preemption hook

The scheduler never sleeps or busy-waits on its own. It signals
"there's work for you" to a CPU through
[`SchedulerArch::send_ipi`], which:

* `spawn` calls after enqueuing a fresh task.
* `unpark` calls after re-enqueueing a previously parked task.

The arch port decides whether that IPI raises a hardware interrupt
immediately, schedules a deferred reschedule, or — on
`wasm32-unknown-unknown` — drops a message into a
`MessageChannel`. The scheduler does **not** assume any latency bound
on the IPI; correctness only requires that the target CPU eventually
calls [`step`].

### Timer-driven preemption entry point

The inverse direction — *arch driving the scheduler* on every timer
tick — is `Scheduler::on_timer_tick(cpu)`. The arch port's timer ISR
(the LAPIC-timer ISR on x86_64; the CNTV/EL1 handler on aarch64; the
CLINT trap on riscv64; the host worker's quantum tick on wasm32)
calls this once per fire, *after* it has acknowledged the device-
level interrupt source (EOI on the LAPIC, etc.). The scheduler
itself never reaches for a timer register; the arch port owns that.

`on_timer_tick` increments the per-CPU preemption counter and
returns; it does **not** call `Scheduler::step`. The counter is
observable from `Scheduler::preemption_count(cpu)` and
`Scheduler::total_preemption_count()`. These exist so integration
tests can assert that preemption is actually firing — a silent
regression to cooperative scheduling would otherwise pass the
workload-correctness tests while breaking the security model
(`AGENTS.md` §5: a runaway task on one CPU must not be able to
indefinitely block another).

#### Why the entry point does *not* dispatch

`step` reads the task registry through an `RwLock` and locks the
overflow `SpinLock`; `spawn` writes the registry. Both lock kinds
are explicitly forbidden from interrupt context by `kernel/sync`
(see `kernel/sync/src/rwlock.rs` module docs: "Process /
kernel-thread context only. Never from an interrupt handler.").
An ISR-driven `step` would deadlock against the same CPU's in-
progress `spawn` (writer-held registry lock) or a mid-
`drain_overflow_to` (held overflow lock).

The cooperative `step` loop — driven by every CPU's kernel-thread
context — remains the only writer of run-queue state. The ISR's
job today is purely observational: bump the counter, return, EOI.
The integration test's per-CPU `preemption_count >= N` assertion
is what proves the timer is actually firing on every CPU.

A future commit that lands IRQ-safe locks (an `irq::SpinLock` from
`kernel/sync`, plus an IRQ-safe `RwLock`) can extend
`on_timer_tick` to drive a real preemption step without changing
its public signature.

#### Trait neutrality

The trait `SchedulerArch` deliberately gains *no* new method for
this path: `send_ipi` already documents the scheduler-asks-arch
direction, and the ISR-into-scheduler call is, by construction, a
method on the scheduler itself rather than on the arch trait
(`AGENTS.md` §2.4 — no interface creep).

## Invariants

These hold at every API boundary:

1. **No global mutable state outside per-CPU areas.** The
   `Scheduler<A>` owns a `Box<[CpuState]>` and a single
   `RwLock<BTreeMap<TaskId, Arc<TaskInner>>>` registry, plus a
   `SpinLock<Vec<TaskId>>` overflow list. Each is documented at its
   field. Static / `lazy_static` mutable state is forbidden
   (`AGENTS.md` §2.1).
2. **Bounded queues.** Every `RunDeque` has its capacity fixed at
   construction. Push returns `Err(task)` on overflow; the scheduler
   routes the task into the overflow list rather than panicking.
3. **No `panic!`, `unwrap`, `expect` in the dispatch path.** Every
   reachable failure produces a typed [`SchedError`].
4. **Cancellation safety.** `park`, `unpark`, `exit` can race with the
   task's own body. The scheduler re-resolves the task's state after
   the body returns:

   | observed state | body returned | effective action |
   | -------------- | ------------- | ---------------- |
   | `Exited`       | *anything*    | `Exit`           |
   | *anything*     | `Exit`        | `Exit`           |
   | `Parked`       | *anything*    | `Park`           |
   | *anything*     | `Park`        | `Park`           |
   | otherwise      | `Yield`       | `Yield`          |

5. **Task identity is stable.** `TaskId` values are never recycled
   within a single scheduler instance. Stale references therefore
   produce `SchedError::NoSuchTask` rather than waking the wrong task.

## Test surface

The scheduler ships with three host-side test binaries:

* `kernel/sched/tests/scheduler.rs` — fairness across ≥ 4 cores,
  work-stealing, IPI-based preemption, starvation-freedom via
  priority boost, cancellation safety, error surface.
* `kernel/sched/tests/stress.rs` — 10 000 tasks across 4 cores;
  asserts no deadlock, exact run counts, and bounded first-run
  latency.
* `kernel/sched/tests/loom.rs` — Loom model check over the
  run-queue's lock-free fast path (single producer racing a single
  stealer for the last element). Compiled into an empty binary when
  `--cfg loom` is not set so default `cargo test` stays fast.

In addition, every module carries `#[cfg(test)] mod tests` with
focused unit coverage. The host test binary uses the in-memory
[`TestArch`] (gated behind the `test-arch` Cargo feature, enabled by
`kernel/sched`'s dev-dependency self-reference) — production builds
never link the mock.

## Debugging

* **Stuck CPU?** Check `Scheduler::live_task_count` and the per-band
  approximate length via `RunDeque::len_approx`. A non-zero count with
  no progress points at a missed IPI (`SchedulerArch::send_ipi`
  delivered to a CPU that never calls `step`).
* **Task never runs?** Confirm its `home_cpu` matches a CPU that is
  actually being driven, and that the task is not stuck in
  `TaskState::Parked` — `Scheduler::state_of` reports the last
  observed state.
* **Steal storm?** `Steal::Retry` is reported when a victim's `top`
  CAS lost; bounded retries (capped in `CpuState::pop_highest`)
  prevent spinning. Excess retries usually indicate too few tasks
  spread across too many CPUs; profile work generation, not the
  scheduler.

## Out of scope (Stage 2.3)

* Real hardware IPIs and timers — provided by Stage 3 architecture
  ports.
* Capability checks, audit logging, and syscall dispatch (Stages 2.4
  / 2.5 / 2.6).
* Memory accounting beyond the global heap (Stage 2.7).

Symbols in `inline-code` link to their rustdoc page from a generated
docs build; this file deliberately does not hard-code rustdoc paths
because they are not reachable from the in-tree link checker. Run
`cargo xtask docs-check` and follow the rustdoc cross-reference there
for the authoritative API surface.
