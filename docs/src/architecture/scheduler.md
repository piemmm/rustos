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
4. If everything local is empty, **work-steals** from a victim CPU
   chosen by the project's shared non-cryptographic `FastRng`
   (`lib/rng`; `AGENTS.md` §2.2 — no second PRNG), scanning every band,
   then walks the CPU list circularly until either a task is found or
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
CPU steals the earliest-deadline task from a victim chosen by the
project's shared non-cryptographic `FastRng` (`lib/rng`; `AGENTS.md`
§2.2 — no second PRNG) and **rebases** the migrated task's `ve`/`vd` onto the stealing
CPU's clock (the EEVDF migration rule — a task carries no lag across
CPUs). The earliest-eligible-deadline scan is `O(n)` in the per-CPU
ready count; a future tree-backed index can replace it behind the
`RunQueue` boundary without changing the policy.

## Heterogeneous CPUs (performance + efficiency cores)

Modern asymmetric CPUs — Intel "hybrid" parts, ARM `big.LITTLE` /
DynamIQ — pair high-throughput **performance** cores with low-power
**efficiency** cores. Both policies place work sensibly across such a
machine; on a homogeneous machine every path below is a strict no-op.

### Detecting the topology

A logical CPU's class is a *static identity* — like its `CpuId` it never
changes for the kernel's lifetime — so it lives in the Arch HAL as
`SchedulerArch::core_class(cpu) -> CoreClass` (`kernel/arch/api`). The
method is **provided**: it defaults to `CoreClass::Performance`, so a
homogeneous machine and any port that has not wired discovery behave
exactly as before, and the surface stays free of dynamic
power-management concerns (frequency scaling, deep sleep) which remain
out of the HAL (`AGENTS.md` §2.4, §17.2). The architecture port
discovers the class during early-boot enumeration (`kernel/arch/x86_64::hybrid`),
records each core's class as it comes online, and serves it through the
override. The host `TestArch` lets a unit test model an asymmetric
machine via `TestArch::set_core_class`.

The two x86_64 vendors expose the per-core class through different CPUID
surfaces, so the port reads the vendor string from CPUID leaf 0 and
dispatches:

* **Intel** — the core type is read from CPUID **leaf 0x1A** (Hybrid
  Information Enumeration). Bits 31:24 of `EAX` carry the type; an Atom
  byte (`0x20`) is an efficiency core, and any other value — including
  the `0` a non-hybrid part reports — is a performance core.
* **AMD** — there is no leaf-0x1A equivalent. The class comes from the
  Extended CPU Topology **leaf 0x80000026**, probed only after bounding
  the maximum extended leaf via leaf 0x80000000. At the Core level
  (`ECX[15:8] == 1`), a part that advertises a heterogeneous topology
  (`EAX[30]`) with an available efficiency ranking (`EAX[29]`) reports a
  per-core power/efficiency ranking in `EBX[23:16]`; the lowest tier is
  an efficiency core and every higher tier is a performance core.

Both decoders **fail conservative**: anything that is not an encoding the
vendor has actually published — an unknown core type, a non-heterogeneous
part, a reserved value — is treated as `CoreClass::Performance`, the safe
homogeneous default. Neither guesses a class from family/model heuristics
or frequency tables. The AMD topology encoding is newer than Intel's and
is still settling across CPU generations, so the AMD decoder recognises
only the published ranking field and defers any future encoding change to
a deliberate, reviewed addition rather than a silent reinterpretation.

### Placing work by class

Each scheduler snapshots the per-CPU classes at construction and keeps a
list of the performance and efficiency CPUs. A task's preferred class
follows its priority band:

* `High` / `Normal` (interactive, throughput-sensitive) → a
  **performance** core;
* `Low` (background / idle) → an **efficiency** core.

`spawn`, `unpark`, and the dispatch re-enqueue path route a task to a CPU
of its preferred class (round-robin within the class). If the task's
current home is already of the right class it stays put — which is why
the path is a no-op on a homogeneous machine, where the efficiency pool
is empty and all work is `Performance`-class. EEVDF carries the task's
competing weight with it across a class migration (the same
no-lag-across-CPUs rebase that work-stealing uses).

### Promotion and demotion

Under **MLFQ** this single rule produces the promote-then-demote
behaviour an idle-but-occasionally-busy task needs *for free*: a `Low`
background task lives on an efficiency core; when the periodic priority
boost lifts it to `High` to avoid starvation it migrates **up** to a
performance core on its next turn, and when it is demoted back to `Low`
once it settles it migrates **down** to an efficiency core again. Under
**EEVDF** priority is static, so placement is by band; work-stealing may
temporarily land a `Low` task on a performance core, and the task
migrates back down to an efficiency core on its next yield.

Liveness is guaranteed regardless of placement: the shared conformance
suite's `heterogeneous_topology_preserves_liveness` test runs a mixed
`High`/`Low` population to completion across performance and efficiency
cores, asserting no task is stranded, lost, or duplicated. Each policy's
own unit tests assert *where* tasks land.

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

RustOS is a **tickless (NO_HZ)** kernel (`AGENTS.md` §17.1): no CPU is
driven by a fixed-frequency periodic timer interrupt. The timer is armed
**one-shot**, to the next event the scheduler actually needs (the running
task's preemption deadline or the nearest timed wakeup), and is left
unarmed when a CPU is idle or runs a single runnable task. Under the
default EEVDF policy a periodic tick is **not** required for correctness
at all (see [EEVDF policy](#eevdf-policy-scheduler-eevdf-default)). The
sole §17.1 carve-out is a policy that needs periodic wakeups — MLFQ's
anti-starvation priority boost. There is exactly one per-CPU timer, and
the boost interval is far longer than one scheduling quantum, so the
boost rides the same on-demand one-shots the preemption path already
arms: those fire **only while a CPU is contended** (which is precisely
when starvation is possible), so `step` — and with it MLFQ's
`maybe_priority_boost` — runs at the quantum cadence and the boost fires
once `boost_interval_ticks` of virtual/wall time elapse. A CPU running a
sole runnable task disarms (no starvation is possible, so no boost is
needed), and **no global fixed-frequency tick is ever reintroduced** —
the §17.1 mandate the carve-out protects.

The one-shot is armed through the Arch HAL timer surface
([`Timer::arm_oneshot`] / [`Timer::disarm`], `kernel/arch/api`): the
scheduler decides *whether* to arm on each dispatch — via the provided
[`SchedulerArch::set_preemption(armed)`] hook, where `armed` is "this CPU
still has a ready competitor" — and the port programs (or stops) its
per-CPU timer (the LAPIC one-shot count, `CNTP_TVAL_EL0`, an SBI
`set_timer`). The per-CPU quantum the one-shot is armed to is the shared
[`DEFAULT_PREEMPT_QUANTUM_HZ`] (aarch64/riscv64) or the LAPIC calibration
period (x86_64); a fired timer never re-arms itself, so a CPU running a
sole runnable task takes no timer interrupts at all (PLAN P-4 retired the
P-1 100 Hz periodic arming).

The *nearest timed wakeup* half of the one-shot is the provided
[`SchedulerArch::set_wakeup(deadline_ns)`] hook: a blocking wait with a
finite timeout (the [blocking wait-queue](#blocking-wait-queue-and-the-wake-pending-token)
below) records its soonest waiter deadline through it, so the port programs
its single physical one-shot to the *earlier* of the quantum arming and the
wakeup, and a parked waiter fires on time even on an otherwise-idle CPU
that has no task to preempt (`AGENTS.md` §17.1).

Each port realises this with a small per-CPU **deadline combiner**
alongside its preemption state: `set_preemption` records the running
task's quantum deadline (now + one quantum) and `set_wakeup` records the
nearest waiter deadline, both as absolute ticks of the port's free-running
counter (`CNTPCT_EL0` on aarch64, the `time` CSR on riscv64, the TSC on
x86_64); a shared `reprogram` arms the single one-shot to the earlier of
the two via the host-tested `rustos_arch_api::wakeup::earliest` helper, or
disarms when neither is pending. The conversion from monotonic-ns deadline
to counter ticks, and (on x86_64) the rebase of the chosen TSC duration
onto the LAPIC count, use the same calibrated frequency `monotonic_ns`
reads the other way (`AGENTS.md` §2.4). Each port installs the
blocking-wait **timed-wake sweep** (`kernel/core::timed_wake_sweep`) as its
per-tick timer callback, so every tick — including one taken on an
otherwise-idle CPU armed solely for a wakeup — releases any elapsed waiter
and re-arms the one-shot to the next deadline. `set_wakeup` defaults to a
no-op, so the host `TestArch` and any non-preemptive port inherit the
explicit-wake path only.

The dispatch loop runs with **device interrupts enabled** — RustOS is a
fully preemptive kernel (`AGENTS.md` §17.1). `admit_init` calls
[`KernelArch::set_device_irqs(true)`] once before steady-state dispatching,
so every in-kernel task and kthread it runs executes with interrupts
deliverable: a long in-kernel operation can no longer mask interrupts for
its whole span and starve the preemption one-shot, the buffered-serial
transmit drain (§20), or an interrupt-driven waiter. The kernel stays
**non-preemptible** (§4): a device IRQ taken while an in-kernel task runs
services its source and returns to the *same* task; only a timer tick taken
from EL0/U-mode/ring 3 reschedules (each port gates preemption on the
interrupted privilege). The `preempt_inkernel_qemu_aarch64` integration
vertical proves both halves directly: a busy in-kernel kthread that issues
no `yield` and no syscall still takes the generic-timer IRQ *during* its
span (the EL1 tick callback fires), yet the EL0-preemption callback fires
zero times and the kthread runs to its voluntary completion — under the old
cooperative loop (device IRQs masked across the whole task run) no tick
would be taken and it would spin forever.

A port whose console transmit is buffered (the aarch64 PL011 — §20) keeps
its in-memory transmit ring draining through
[`KernelArch::pump_console_tx`], a non-blocking top-up the dispatch loop
calls on **every** iteration: after each successful dispatch (in
`service_between_dispatches`, alongside the deferred-wake drain) and again
just before the idle park. This is what keeps the log flowing even while a
perpetually-runnable in-kernel kthread (any service that yields every pass
but never parks) holds the loop off its idle branch
forever — an idle-only drain would freeze the log the instant such a kthread
exists, and the transmit-FIFO "has-room" interrupt cannot be relied on to
self-sustain the drain on real silicon (the Raspberry Pi 4's flow-blocked
UART). Output therefore flows at the loop's dispatch rate, independent of
idle and of the transmit interrupt. The seam defaults to a no-op, so ports
with synchronous console output (riscv64 SBI, x86_64 COM1) inherit nothing.

The idle CPU itself sleeps through [`KernelArch::wait_for_interrupt`]: when
the dispatch loop finds no runnable task but a live task is still parked
(e.g. a perpetual service blocked in a blocking-wait syscall), the loop
masks device interrupts, drains any pending wake once more, tops up the
buffered console transmit one last time, and — if nothing became runnable —
parks the CPU on the port's race-free idle wait (`wfi` on
aarch64/riscv64, `sti; hlt; cli` on x86_64) rather than halting, then
re-enables interrupts; the armed wakeup one-shot or a device IRQ wakes a
waiter and the loop re-steps and dispatches it. Masking across the park and
draining before the `wfi`/`hlt` closes the park/wake race, so no edge is
lost. PID 1 `init` now launches the perpetual
`/System/Services/devmgr` service (a `service` directive in its startup
config, supervised alongside the per-console login sessions), which reads
the discovered hardware tree and parks in `hw_tree_wait` for the life of
the system — the first production caller of this blocking-wait path. The
remaining production-launch work — the reactive bus-driver chain that emits
the nodes `devmgr` reacts to — is staged in `.junie/next-pi-prompt.md`
(Design D D3–D5).

In both policies `on_timer_tick` increments the per-CPU
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

The dispatcher (running in kernel-thread context with device interrupts
enabled) remains the only writer of run-queue state. An interrupt handler
**never** takes a scheduler lock: it does its work lock-free and, when it
needs to wake a waiter, only sets an atomic flag
(`WaitQueue::request_wake` / the `timed_wake_sweep` pending bit), mirroring
`rustos_kernel_irq::IrqTable::fire`. The real `unpark` — which reads the
registry `RwLock` and the run-queue lock — runs at the next
dispatcher-context safe point (`waitq::drain_pending_wakes`, between steps
and before idle), so the scheduler's locks are never acquired with
interrupts disabled and an ISR can never deadlock against an in-progress
`spawn` / `dispatch` on the interrupted task. A woken task cannot run until
the current in-kernel task yields anyway (the kernel is non-preemptible),
so deferring the unpark to that point costs no responsiveness. Involuntary
preemption of a *user* task is the separate privilege-gated path: a timer
tick taken from EL0/U-mode/ring 3 suspends the running user task back to the
scheduler via the port's preempt callback. The integration test's per-CPU
`preemption_count >= N` assertion proves the timer is firing on every CPU.

#### Trait neutrality

The trait `SchedulerArch` deliberately gains *no* new method for
this path: `send_ipi` already documents the scheduler-asks-arch
direction, and the ISR-into-scheduler call is, by construction, a
method on the scheduler itself rather than on the arch trait
(`AGENTS.md` §2.4 — no interface creep).

## Blocking wait-queue and the wake-pending token

A task that must wait for an event it cannot make progress on **parks** off
the run queue rather than busy-yielding (`AGENTS.md` §2.1). The reusable
primitive is `kernel/core::waitq::WaitQueue`: a waiter registers (with an
optional absolute monotonic-ns deadline), then suspends with
`RescheduleAction::Park`; it is woken either by an **explicit event**
(`WaitQueue::wake_all`) or, with a deadline, by the **timed sweep**
(`WaitQueue::sweep`). An interrupt-reachable wake never touches the
wait-queue or scheduler locks: the device-IRQ dispatcher and the timer
one-shot only flag a pending wake (`WaitQueue::request_wake` /
`timed_wake_sweep`), and the actual `wake_all` / deadline sweep + `unpark`
runs at the next dispatcher-context `waitq::drain_pending_wakes` (between
scheduler steps and before idle). The first consumer is the `hw_tree_wait`
syscall, whose waiters `HW_TREE_WAITQ` holds and the discovered-hardware
store wakes on every generation bump (`AGENTS.md` §18.4). Waking a parked
waiter, reading the clock, and arming the one-shot all route through one
boot-installed `WaitQueueArch` adapter over the live `Scheduler<A>` + arch,
so the global wait-queue never names either concrete type (`AGENTS.md`
§17.4 / §2.2).

### No lost wake-ups

The park/unpark race — a wake delivered after the waiter last checked its
condition but before it commits to park — is closed in the scheduler
itself by a **wake-pending token** (mirroring Rust's `Thread`
park/unpark). `Scheduler::unpark` of a task that has *not* yet committed to
park (it is `Ready`/`Running`) cannot move a non-parked task, so instead of
no-oping the wake away it sets the token; the dispatch loop's `Park` commit
consumes the token and re-readies the task rather than sleeping it. A
waiter therefore only ever sleeps through a wake it has not yet observed,
and always re-checks its condition after each wake, so a finished or
timed-out wait returns rather than parking forever. The shared
`SchedulerPolicy` conformance suite's `unpark_before_park_is_not_lost`
case asserts this for every policy.

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
