# Kernel scheduler

`kernel/sched` ships the architecture-neutral half of RustOS' SMP
scheduler. It owns the run queues and the dispatch policy; the
architecture ports (Stage 3 of [`PLAN.md`](../../../PLAN.md)) plug in the
real IPI and timer surfaces through the [`SchedulerArch`] trait.

`SchedulerArch` (and `CpuId`) are defined in the Arch HAL crate
`kernel/arch/api` (`AGENTS.md` §17.2) and re-exported from `kernel/sched`
so the architecture-neutral kernel keeps a single canonical definition
while the ports implement the trait without depending on `kernel/sched`
(§17.4). See [Modularity contracts](./modularity.md#the-arch-hal-kernelarchapi).

This page is the source of truth for the *behaviour* the implementation
must preserve. Read it together with the rustdoc on `kernel/sched`'s
public items, and with [`AGENTS.md`](../../../AGENTS.md) §4 ("SMP from
day one") and §2 ("Non-negotiable rules").

## Scheduler policies

The dispatch *policy* is pluggable (`AGENTS.md` §17.1). Two concrete
policies ship today, each in its own `kernel/sched/<impl>` crate, and
both implement the same [`SchedulerPolicy`] contract so the rest of the
kernel is agnostic to which one an image links:

* **EEVDF** (`kernel/sched/eevdf`, feature `scheduler-eevdf`) — the
  **default**. A fully *tickless* Earliest-Eligible-Virtual-Deadline-First
  policy (see [EEVDF policy](#eevdf-policy-scheduler-eevdf-default)).
* **MLFQ** (`kernel/sched/mlfq`, feature `scheduler-mlfq`) — a
  Multi-Level Feedback Queue with periodic priority boosting (see
  [MLFQ policy](#mlfq-policy-scheduler-mlfq)).

`kernel/core` is the single build-time selection point and enforces that
**exactly one** `scheduler-*` feature is active per image. Everything
below the two policy sections — the IPI hook, the timer entry point, the
current-task slot, and the invariants — is shared by both policies
because it lives in the contract, not the policy.

## MLFQ policy (`scheduler-mlfq`)

This policy is a **Multi-Level Feedback Queue (MLFQ)** with periodic
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

## EEVDF policy (`scheduler-eevdf`, default)

The default policy is **EEVDF — Earliest Eligible Virtual Deadline
First** (Stoica & Abdel-Wahab, 1995; the same family Linux adopted for
its fair scheduler in 6.6). It is dispatched via the same per-CPU
work-stealing structure as MLFQ, but orders tasks by a continuous
*virtual deadline* instead of discrete priority bands.

### Virtual time, weight, eligibility, deadline

Each CPU keeps its own fixed-point **virtual time** `V`. Each task has a
**weight** derived from its [`Priority`] band (`High`:`Normal`:`Low` =
`4`:`2`:`1`) and two virtual-time markers:

* an **eligible time** `ve` — the task may run only once `V >= ve`;
* a **deadline** `vd = ve + request / weight`, where `request` is one
  dispatch's worth of service.

Admission (spawn / unpark / migration) sets `ve = V` (zero initial lag)
and `vd = ve + request/weight`. On each dispatch the CPU runs the
**eligible** task with the **earliest** `vd` (ties broken by `TaskId`
for determinism); the task's fulfilled request then rolls `ve` forward
to its old `vd` and computes the next `vd`. `V` advances by
`service / total_weight` of the tasks competing on that CPU, so a task
accrues virtual time inversely to its weight and receives a CPU share
proportional to it. No task is ever starved: every eligible task has a
finite, monotonically increasing deadline.

### Fully tickless

Fairness, eligibility, and preemption are driven **entirely by virtual
time advanced as work is dispatched** — never by a periodic timer tick.
`Scheduler::on_timer_tick` is a pure observation counter for this
policy; no scheduling decision reads it. This is what makes the policy
tickless: a real arch port can run its timer in one-shot / `NO_HZ` mode,
arming it only for the next virtual deadline rather than at a fixed
frequency. The `tickless_weight_proportional_fairness` unit test proves
fairness holds while `ticks_now()` never moves and `on_timer_tick` is
never called.

### Per-CPU queues + work-stealing

Each CPU owns one virtual-time `RunQueue` with its own clock. An idle
CPU steals the earliest-deadline task from a pseudo-randomly selected
victim and **rebases** the migrated task's `ve`/`vd` onto the stealing
CPU's clock (the EEVDF migration rule — a task carries no lag across
CPUs). The earliest-eligible-deadline scan is `O(n)` in the per-CPU
ready count; a future tree-backed index can replace it behind the
`RunQueue` boundary without changing the policy.

## IPI-based preemption hook

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

## Timer-driven preemption entry point

The inverse direction — *arch driving the scheduler* on every timer
tick — is `Scheduler::on_timer_tick(cpu)`. The arch port's timer ISR
(the LAPIC-timer ISR on x86_64; the CNTV/EL1 handler on aarch64; the
CLINT trap on riscv64; the host worker's quantum tick on wasm32)
calls this once per fire, *after* it has acknowledged the device-
level interrupt source (EOI on the LAPIC, etc.). The scheduler
itself never reaches for a timer register; the arch port owns that.

Under the default tickless EEVDF policy a periodic tick is **not
required** for correctness (see [EEVDF policy](#eevdf-policy-scheduler-eevdf-default));
the entry point exists so a port that does run a periodic timer can
record that it fired. Under MLFQ the same tick also bounds the priority
boost interval. In both policies `on_timer_tick` increments the per-CPU
preemption counter and returns; it does **not** call `Scheduler::step`.
The counter is
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
are explicitly forbidden from interrupt context by `lib/sync`
(see `lib/sync/src/rwlock.rs` module docs: "Process /
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
`lib/sync`, plus an IRQ-safe `RwLock`) can extend
`on_timer_tick` to drive a real preemption step without changing
its public signature.

#### Trait neutrality

The trait `SchedulerArch` deliberately gains *no* new method for
this path: `send_ipi` already documents the scheduler-asks-arch
direction, and the ISR-into-scheduler call is, by construction, a
method on the scheduler itself rather than on the arch trait
(`AGENTS.md` §2.4 — no interface creep).

## Current-task slot

`Scheduler<A>` owns a per-CPU **current-task slot** — one
`AtomicU64` per CPU, sentinel `0` meaning "no task currently
running on this CPU". The slot is the publication point the
syscall entry path reads to recover the caller's `TaskId`
without trusting any caller-supplied value
(`AGENTS.md` §5.4 step 1 — identify the caller; kernel-provided,
not caller-supplied).

### Lifecycle

| event                            | slot effect                              |
| -------------------------------- | ---------------------------------------- |
| `Scheduler::dispatch` (entry)    | publishes the about-to-run task's id     |
| `Scheduler::dispatch` (exit)     | clears the slot, every branch            |
| `Scheduler::park(id)`            | clears every CPU's slot whose entry == id|
| `Scheduler::exit(id)`            | clears every CPU's slot whose entry == id|
| `Scheduler::yield_current(id)`   | re-enqueues `id` Ready, then clears slot |

The slot is exposed read-only through
`Scheduler::current_task(cpu) -> Option<TaskId>`. The setter and
the clear-by-id helpers are private: only the scheduler itself
mutates the slot, so the lifecycle table above is the entire
ground truth.

### Concurrency rules

* The slot is read in **process context** on the issuing CPU only.
  Interrupt-context reads are forbidden — they would race with a
  same-CPU `dispatch` set/clear pair under
  `lib/sync::RwLock`'s process-only contract
  (`AGENTS.md` §1).
* The clear-by-id helper used by `park` / `exit` /
  `yield_current` is a per-slot compare-exchange; a concurrent
  `dispatch` of a *different* task on a sibling CPU is therefore
  untouched.
* `current_task(cpu)` returns `None` for an out-of-range `cpu`,
  not an error. This matches the policy that an unknown CPU has,
  by definition, no current task — and lets the syscall entry
  path probe the slot without having to validate
  `current_cpu()` first (the syscall trampoline always supplies
  a valid CPU id; the `None` return is the defence-in-depth
  fail-closed branch).

### Why this lives on `Scheduler<A>` and not on `SchedulerArch`

Adding a method on the arch trait would create both a duplicate
storage site (every arch port would have to hold a copy) and an
interface widening for a value the scheduler already publishes
internally (`AGENTS.md` §2.4 — no interface creep). The slot is
mutated only by the scheduler's own `dispatch` path, so the
authoritative copy lives where the writer lives.

### `yield_current` vs body-returned `TaskAction::Yield`

`Scheduler::yield_current(task_id)` models a **voluntary syscall
yield**: the task is currently in `TaskState::Running` on its
CPU, the syscall handler wants to relinquish the rest of its
quantum, and the scheduler must re-Ready the task and clear the
slot before the syscall returns to user space.

`TaskAction::Yield` returned by a task body is the
**body-loop yield**: it is processed by `dispatch` along with
MLFQ demotion bookkeeping (`yields_at_band` /
`yields_before_demotion`). The two notions are deliberately
distinct so the syscall handler is not on the hook for demotion
policy, which would be interface creep into the syscall layer.

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

## Crate layout (§17.1)

The scheduler is split per `AGENTS.md` §17.1 into a contract crate and
one policy crate per implementation:

* `kernel/sched/api` (`rustos-kernel-sched-api`) — the
  `SchedulerPolicy` trait, the policy-neutral lifecycle vocabulary
  (`Priority`, `TaskState`, `TaskAction`, `TaskContext`, `TaskId`,
  `SchedError`, `StepOutcome`, `SchedulerConfig`), the re-exported
  Arch HAL surface (`CpuId`, `SchedulerArch`), the host `TestArch`
  double, and the shared `conformance` suite.
* `kernel/sched/eevdf` (`rustos-kernel-sched-eevdf`) — the EEVDF policy
  described above, implementing `SchedulerPolicy`. This is the default.
* `kernel/sched/mlfq` (`rustos-kernel-sched-mlfq`) — the MLFQ policy
  described above, implementing `SchedulerPolicy`. The two are siblings
  (`AGENTS.md` §2.2 carve-out — parallel policies are deliberate, not
  duplication); adding another policy means adding a sibling crate,
  never editing an existing one.
* `kernel/core` is the single build-time selection point: exactly one
  `scheduler-*` feature is active per image (`scheduler-eevdf` by
  default, `scheduler-mlfq` with `--no-default-features --features
  scheduler-mlfq`). It re-exports the chosen policy as
  `crate::sched::Scheduler`; `compile_error!` guards reject the
  zero-policy and two-policy configurations. No other crate names a
  concrete policy — they depend on `kernel/sched/api`.

## Test surface

The scheduler's behaviour is covered by:

* The shared conformance suite `kernel/sched/api/src/conformance.rs`
  (`AGENTS.md` §17.1): generic over `SchedulerPolicy`, it asserts
  correct spawn/dispatch/block/wake and yield semantics, starvation-
  freedom, fairness across bands, and a deadlock-free, lossless,
  bounded-latency stress of 10 000 tasks across 4 simulated cores.
  Every concrete policy must pass it.
* `kernel/sched/api/tests/conformance.rs` runs the suite against the
  in-tree MLFQ policy; `kernel/sched/eevdf/tests/conformance.rs` runs
  the identical suite against EEVDF — proving the two policies are
  interchangeable behind the contract.
* `kernel/sched/eevdf/src/scheduler.rs` `#[cfg(test)] mod tests` —
  EEVDF-specific coverage: tickless weight-proportional fairness
  (asserting no tick is ever used), even sharing of equal-weight tasks,
  work-stealing, park/unpark, the current-task slot, and that
  `on_timer_tick` is observation-only.
* `kernel/sched/mlfq/tests/scheduler.rs` — MLFQ-specific integration
  tests (fairness across ≥ 4 cores, work-stealing balance, IPI-based
  preemption, cancellation safety, error surface).
* `kernel/sched/mlfq/tests/loom.rs` — Loom model check over the
  run-queue's lock-free fast path (single producer racing a single
  stealer for the last element). Compiled into an empty binary when
  `--cfg loom` is not set so default `cargo test` stays fast.

In addition, every module carries `#[cfg(test)] mod tests` with
focused unit coverage. The tests use the in-memory [`TestArch`]
(gated behind the `test-arch` Cargo feature) — production builds
never link the mock.

## Debugging

* **Stuck CPU?** Check `Scheduler::live_task_count`. A non-zero count
  with no progress points at a missed IPI (`SchedulerArch::send_ipi`
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
