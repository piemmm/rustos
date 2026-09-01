# Kernel scheduler

`kernel/sched` ships the architecture-neutral half of TAIRiX' SMP
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

The dispatch *policy* is pluggable (`AGENTS.md` §17.1). Three concrete
policies ship today, each in its own `kernel/sched/<impl>` crate, all
implementing the same [`SchedulerPolicy`] contract so the rest of the
kernel is agnostic to which one an image links:

* **CFQ** (`kernel/sched/cfq`, feature `scheduler-cfq`) — the
  **default**. A *non-tickless*, Linux-CFS-like Completely-Fair-Queuing
  policy (see [CFQ policy](#cfq-policy-scheduler-cfq-default)).
* **EEVDF** (`kernel/sched/eevdf`, feature `scheduler-eevdf`) — a fully
  *tickless* Earliest-Eligible-Virtual-Deadline-First policy (see
  [EEVDF policy](#eevdf-policy-scheduler-eevdf)).
* **MLFQ** (`kernel/sched/mlfq`, feature `scheduler-mlfq`) — a
  Multi-Level Feedback Queue with periodic priority boosting (see
  [MLFQ policy](#mlfq-policy-scheduler-mlfq)).

`kernel/core` is the single build-time selection point and enforces that
**exactly one** `scheduler-*` feature is active per image. Everything
below the policy sections — the IPI hook, the timer entry point, the
current-task slot, and the invariants — is shared by every policy
because it lives in the contract, not the policy.

## CFQ policy (`scheduler-cfq`, default)

The default policy is **CFQ — Completely Fair Queuing**, modelled on
Linux's Completely Fair Scheduler (Molnar, 2007). Each task carries a
virtual runtime `vruntime`; on each CPU the ready task with the
*smallest* `vruntime` is dispatched next — the leftmost node of Linux
CFS's red-black tree, here an ordered `BTreeSet<(vruntime, TaskId)>`
(`O(log n)` pick/insert/remove). A dispatch charges the running task
`elapsed_ticks * SCALE / weight` of virtual runtime. Equal-weight tasks
therefore receive equal CPU time even when an interrupt-driven task runs
briefly and parks while a CPU-bound task consumes a full quantum; a
heavier-weighted task's `vruntime` rises more slowly for the same elapsed
service. The result is proportional CPU-time share, with no band ever
starved (every `vruntime` advances monotonically). The three `Priority`
bands map to a 4:2:1 weight ratio (the CFS "nice level" analog). A per-CPU
monotonic `min_vruntime` floor places a joining or migrated task one
`SLEEPER_CREDIT` (a single unit of service) *ahead* of the front of the
CPU's timeline — the CFS `place_entity` sleeper credit. The credit is what
makes a woken task sort **strictly** before the population that has been
running: placing it merely level with the leftmost ready entry leaves the
`(vruntime, TaskId)` tie-break to settle the pick, which hands the CPU to
the lower id, so a task that wakes among CPU-bound tasks it was spawned
after loses a full scheduling round on *every* wake and an I/O-bound task
pays that round per round trip. Because the floor advances only to a
*picked* task's `vruntime` and every dispatch charges at least one credit
back, the head start stays bounded by that one unit however long the task
slept: it cannot leap the running population, and a stolen task carries no
lag across CPUs. Per-CPU run queues, work-stealing, class-based placement, the
overflow list, and the park/unpark lost-wakeup token are all shared with
the sibling policies' mechanics.

### Non-tickless — the tickless carve-out

CFQ is the **one scheduler the charter permits to be non-tickless**
(`AGENTS.md` §17.1). Where the tickless policies arm their preemption
one-shot only when a CPU is *contended* and disarm for a sole runnable
task (so a quiet core takes no timer interrupts), CFQ keeps a
fixed-frequency periodic quantum tick armed for *any* running task —
including a lone CPU-bound one — so the timer interrupt fires at a steady
`HZ` cadence exactly like Linux's scheduler tick. Concretely
`Scheduler::dispatch` calls [`SchedulerArch::set_preemption`]`(true)`
unconditionally while a task runs; only a genuinely idle CPU (nothing
runnable at all) disarms, in `Scheduler::step`.

A fired tick does not blindly context-switch, though. As in Linux
(`check_preempt_tick`), the kernel's return-to-user preempt point
(`kernel/core`'s `preempt_current`) reschedules only when the switch would
change what runs — there is another runnable task on the CPU, or pending
interrupt-context work (a device-IRQ deferred wake, an elapsed timed
deadline, a queued foreground signal) to drain. A **lone** runnable task's
tick therefore does its periodic accounting and returns to that same task
without a switch-to-self: switching to itself has no scheduling effect and
would only churn the per-dispatch user-address-space reactivation (and, on
an emulated target, its full TLB flush), starving the task's own progress.
This gate lives in the policy-neutral preempt path, keyed off a
scheduler-backed "runnable competitor?" query, so it holds for every
policy — but only CFQ ever exercises it, because only CFQ arms a lone
task's tick. This deliberate violation of the tickless mandate is granted
to CFQ **alone**; no other policy may take it.

## MLFQ policy (`scheduler-mlfq`)

This policy is a **Multi-Level Feedback Queue (MLFQ)** with periodic
priority boosting, dispatched via **per-CPU work-stealing queues**. The
policy is the classical MLFQ as described in:

> Arpaci-Dusseau, R. H. & Arpaci-Dusseau, A. C. *Operating Systems:
> Three Easy Pieces*, ch. 8. <https://pages.cs.wisc.edu/~remzi/OSTEP/>

The run-queue is a bounded variant of the Chase–Lev work-stealing
deque (Chase & Lev, SPAA '05) with two TAIRiX-specific simplifications:

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

## EEVDF policy (`scheduler-eevdf`)

EEVDF — **Earliest Eligible Virtual Deadline First** (Stoica &
Abdel-Wahab, 1995; the same family Linux adopted for its fair scheduler
in 6.6) is a fully tickless sibling policy. It is dispatched via the same per-CPU
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

`spawn` and `unpark` place a task on the **least-loaded** CPU of its
preferred class (by competing weight, preferring the caller's hint on an
equal-load tie). An idle CPU competes with weight `0`, so new and woken
work fills sleeping cores — whose placement IPI pulls them out of their
idle park — instead of piling onto the spawning CPU; and because each
admission adds the placed task's weight, a burst of spawns spreads
across equally-idle CPUs. The dispatch re-enqueue path is deliberately
stickier: a yielding task stays on its current CPU unless its *class* is
wrong (re-placing on every yield would migrate a task whenever another
CPU dipped below its home's load, thrashing caches for no fairness
gain); only a class mismatch — e.g. a `Low` task work-stealing parked on
a performance core — migrates it, to the least-loaded CPU of the right
class. EEVDF carries the task's competing weight with it across a class
migration (the same no-lag-across-CPUs rebase that work-stealing uses).

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

## Real-time scheduling class

`Priority` (`High`/`Normal`/`Low`) is a *nice level* within the fair band —
it tunes weight and core-class placement but never lets a task escape fair
competition. Orthogonal to it is the **scheduling class**, `SchedClass`
(`kernel/sched/api`), with two values:

* `SchedClass::TimeShared` — the default (and every newly spawned task): the
  fair band governed by the selected policy (CFQ/EEVDF/MLFQ) exactly as
  described above.
* `SchedClass::Realtime` — a strict-priority band that sits **above** the
  entire fair band on every CPU.

The contract every policy enforces is strict:

* a ready real-time task is dispatched before **any** time-shared task on its
  CPU, regardless of the time-shared task's accumulated virtual runtime,
  priority, or wait time;
* a running real-time task is **never** preempted in favour of a time-shared
  task — only another real-time task, a voluntary block/yield, or termination
  takes the CPU from it;
* real-time peers on one CPU are ordered FIFO and re-enqueued at the back on
  yield, so equal peers share the CPU round-robin (the `SCHED_RR` shape) and
  none starves another.

This is the microkernel analogue of a threaded-IRQ / `SCHED_FIFO` grant: an
interrupt-serving user-space driver woken by its device IRQ must run *now*,
ahead of any CPU-bound workload, so it can service the device before the
hardware ring it polls drains. The xHCI USB host controller is the first user
(`plans/USB.md`): under `stress --cpu N` its IRQ-woken report pump would
otherwise merely compete with the CPU hogs and could be scheduled too late to
re-arm the interrupt-IN endpoints, dropping input reports; in the real-time
class the IRQ wake preempts the hogs and the pump runs within
interrupt-return + context-switch latency.

A task enters or leaves the class with the `sched_set_realtime` syscall
(`SyscallNumber::SCHED_SET_REALTIME`, wrapped by `tairix_rt::sched_set_realtime`),
which is **self-only** — a task can reclass only itself, keyed by the
kernel-trusted caller id — and gated in both directions by the dedicated
capability `CAP_SCHED_REALTIME`. Because scheduling class is per-task state
and the capability is static (a signed manifest request intersected with the
user's grants), only a holder is ever real-time and only a holder ever needs
to leave the class, so gating both directions denies a legitimate caller
nothing while keeping the privileged direction firmly closed. A real-time task
that never blocks would monopolise its CPU against time-shared work; that is
inherent to strict priority and is bounded by making the class a guarded
capability granted only to trusted, IRQ-driven drivers. `set_sched_class`
records the class; the task adopts it at its next enqueue (its next wake or
yield), which for the usual caller — a driver that elevates itself once at
start-up and then blocks on its device IRQ — means every subsequent wake is
strict-priority.

The class is a property of the *kernel*, not of any one policy: it is honoured
identically by CFQ, EEVDF, and MLFQ (each keeps a separate strict-priority
band its `step` consults before the fair band), and the shared
`kernel/sched/api` conformance suite pins the guarantee for every policy
(`realtime_is_dispatched_ahead_of_a_full_time_shared_queue`,
`realtime_is_not_preempted_by_time_shared_then_releases_on_park`,
`realtime_peers_share_the_cpu_round_robin`,
`sched_class_reports_and_fails_closed_on_unknown`).

## Changing a live task's priority

The *nice level* itself is mutable after admission through the
`SchedulerPolicy::set_priority` / `priority` pair — the contract behind the
`sched_set_priority` syscall (`plans/NEW-TASKBAR.md` T12; the target rule,
the `CAP_PROC_CONTROL` raise gate, and the audit record are the syscall
page's, [`syscalls.md`](./syscalls.md)). The contract is deliberately the
same shape as `set_sched_class`:

* the new level is **recorded at once** and governs the task's **next
  enqueue** onward — a task sitting ready in a run queue adopts the new
  weight or band at its next dispatch rather than being surgically moved,
  so the observable behaviour is identical across policies (the Chase–Lev
  deques support no arbitrary removal, and no policy needs one);
* re-stating the level a task already holds is an **idempotent success**;
* an unknown id fails closed with `NoSuchTask` and a terminal task with
  `InvalidState` — never a fabricated change;
* the recorded value is the task's *time-shared* service level: a
  `Realtime`-class task keeps it for when it returns to the fair band, and
  the strict-priority band is unaffected by it.

Per policy: **CFQ** and **EEVDF** re-derive their 4:2:1 fair-share weight
from the stored level on every enqueue, so the change simply takes effect
lastingly from the next dispatch. **MLFQ** treats the recorded level as the
task's *current band* with fresh yield residency — its demotion rule and
anti-starvation boost keep adjusting the band afterwards, exactly as they
do for every other task, so an externally lowered task is still boosted
out of starvation on the boost cadence; the starvation guarantee is never
suspended to pin a task low. `priority` reports the band the task holds
*right now*, which under a decay policy is the truthful reading — it feeds
the System Information process record so a task manager can render an
already-lowered process as such. The shared conformance suite pins the
whole contract for every policy
(`priority_reports_changes_and_fails_closed`).

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

## Deferred admission: `spawn_parked`

`spawn` makes a task runnable the instant it returns — it is enqueued and
its home CPU is IPI'd, so on an SMP machine another core can dispatch the
task (and take its first syscall) before the caller's next instruction.
That is wrong for a process whose per-task kernel state — its capability
record, address space, standard streams, resource limits, and device
grants — is installed *after* the scheduler mints its id: a Ready
admission races those installs, and the racing core's first syscall finds
no capability record.

`spawn_parked` is the birth form that closes this race. It registers the
task and returns its id but leaves it `Parked`, off every run queue, with
**no** wake IPI; no CPU can dispatch it. The process-admit path installs
all per-task state under the returned id and only then calls `unpark`,
which performs the placement, enqueue, and IPI exactly as `spawn` would.
Both the `spawn` syscall path and PID 1's admission use it, so a freshly
spawned process is never dispatchable before it is fully constructed. The
`SchedulerPolicy` conformance suite pins the guarantee
(`spawn_parked_stays_parked_until_unpark`).

## Timer-driven preemption entry point

The inverse direction — *arch driving the scheduler* on every timer
tick — is `Scheduler::on_timer_tick(cpu)`. The arch port's timer ISR
(the LAPIC-timer ISR on x86_64; the CNTV/EL1 handler on aarch64; the
CLINT trap on riscv64; the host worker's quantum tick on wasm32)
calls this once per fire, *after* it has acknowledged the device-
level interrupt source (EOI on the LAPIC, etc.). The scheduler
itself never reaches for a timer register; the arch port owns that.

TAIRiX is a **tickless (NO_HZ)** kernel (`AGENTS.md` §17.1) under every
policy but the one sanctioned exception, **CFQ** (the default). Under a
tickless policy no CPU is driven by a fixed-frequency periodic timer
interrupt: the timer is armed **one-shot**, to the next event the
scheduler actually needs (the running task's preemption deadline or the
nearest timed wakeup), and is left unarmed when a CPU is idle or runs a
single runnable task. Under the tickless EEVDF policy a periodic tick is
**not** required for correctness at all (see
[EEVDF policy](#eevdf-policy-scheduler-eevdf)).

**The default CFQ policy is deliberately non-tickless** — the charter's
one sanctioned exception (see [CFQ policy](#cfq-policy-scheduler-cfq-default)):
it passes `armed = true` to `set_preemption` for *any* running task,
including a lone CPU-bound one, so the port keeps re-arming the quantum
one-shot and the effect is a fixed-frequency `HZ`-style periodic tick.
Only a genuinely idle CFQ CPU (nothing runnable) disarms. A fired tick
still only *context-switches* when the switch would change what runs (the
`preempt_current` gate below) — a lone task's tick does its accounting and
returns to that same task — so CFQ never needlessly switch-to-self churns.
Every consumed CFQ tick that does not reach a dispatch re-arms the periodic
deadline: both the lone-task path and a failed immediate suspension restore
the next quantum before returning. A busy task therefore cannot resume with
its fired one-shot cleared and no future preemption interrupt.
Everything below describes the shared mechanism; only the per-dispatch
arm/disarm *decision* differs between the tickless policies and CFQ.

A narrower §17.1 carve-out is a policy that needs periodic wakeups — MLFQ's
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
[`SchedulerArch::set_preemption(armed)`] hook. Under a tickless policy
`armed` is "this CPU still has a ready competitor"; under the default CFQ
policy `armed` is `true` for *any* running task (the non-tickless
carve-out). The port programs (or stops) its per-CPU timer (the LAPIC
one-shot count, `CNTP_TVAL_EL0`, an SBI `set_timer`). The per-CPU quantum
the one-shot is armed to is the shared [`DEFAULT_PREEMPT_QUANTUM_HZ`]
(aarch64/riscv64) or the LAPIC calibration period (x86_64); a fired timer
never re-arms itself, so under a tickless policy a CPU running a sole
runnable task takes no timer interrupts at all (PLAN P-4 retired the
P-1 100 Hz periodic arming), while CFQ re-arms it on the next dispatch to
sustain its periodic tick.

The *nearest timed wakeup* half of the one-shot is the provided
[`SchedulerArch::set_wakeup(deadline_ns)`] hook: a blocking wait with a
finite timeout (the [blocking wait-queue](#blocking-wait-queue-and-the-wake-pending-token)
below) records the soonest waiter deadline across **every** timed
wait-queue through it (`nearest_timed_deadline` — never a single queue's
own view, which would silently drop another queue's pending wake off the
shared one-shot), so the port programs
its single physical one-shot to the *earlier* of the quantum arming and the
wakeup, and a parked waiter fires on time even on an otherwise-idle CPU
that has no task to preempt (`AGENTS.md` §17.1).

Each port realises this with a small per-CPU **deadline combiner**
alongside its preemption state: `set_preemption` records the running
task's quantum deadline (now + one quantum) and `set_wakeup` records the
nearest waiter deadline, both as absolute ticks of the port's free-running
counter (`CNTPCT_EL0` on aarch64, the `time` CSR on riscv64, the TSC on
x86_64); a shared `reprogram` arms the single one-shot to the earlier of
the two via the host-tested `tairix_arch_api::wakeup::earliest` helper, or
disarms when neither is pending. The conversion from monotonic-ns deadline
to counter ticks, and (on x86_64) the rebase of the chosen TSC duration
onto the LAPIC count, use the same calibrated frequency `monotonic_ns`
reads the other way (`AGENTS.md` §2.4). Each port's per-tick timer
callback latches the fired tick as the CPU's **pending preemption**
(`kernel/core::note_preempt_tick`, below) and runs the blocking-wait
**timed-wake sweep** (`kernel/core::timed_wake_sweep`), so every tick —
including one taken on an otherwise-idle CPU armed solely for a wakeup —
releases any elapsed waiter
and re-arms the one-shot to the next deadline. `set_wakeup` defaults to a
no-op, so a non-preemptive port inherits the explicit-wake path only; the
host `TestArch` records each call so the wait syscalls' re-arm epilogues
are asserted directly.

The scheduler also feeds the first-class **CPU-lockup watchdog**
(`kernel/core::watchdog`), which catches two distinct failures and makes each
loud enough to explain *why*. It keeps two per-CPU heartbeats plus an activity
class (Offline / Idle / Active, published by the dispatch loop so only a CPU
that *owes* progress is judged — fail closed). See `plans/WATCHDOG.md`.

- A **soft lockup** is a CPU that keeps taking interrupts but stops returning
  to the scheduler. The dispatch loop stamps a **progress** heartbeat once per
  iteration (`watchdog::note_progress`, the tickless analogue of a Linux
  watchdog thread being scheduled); the armed preemption tick samples it
  (`watchdog::check_stall`) — it only fires on a *contended* CPU, so a lone,
  preemptible task is never falsely flagged — and a heartbeat older than
  `watchdog::DEFAULT_SOFT_LOCKUP_THRESHOLD_NS` (10 s) reports the stall once.

- A **hard lockup** is a CPU that has stopped taking even interrupts (spinning
  with IRQs masked, wedged, an interrupt storm). Its own tick never fires, so
  only another CPU can see it. A port arms a non-maskable ~1 Hz cadence sample
  (the Arch HAL watchdog surface, `tairix_arch_api::watchdog`) that stamps a
  **liveness** heartbeat and runs a **cross-CPU scan** (`on_watchdog_tick`); a
  buddy whose liveness is stale past `DEFAULT_HARD_LOCKUP_THRESHOLD_NS` (10 s)
  while Active is reported hard-locked. On aarch64 the cadence is the virtual
  generic timer (`CNTV`, PPI 27) delivered as an ordinary IRQ — the correct
  cross-CPU *buddy* detector for a GICv2 non-secure kernel, where FIQ (the
  secure pseudo-NMI) is unavailable.

Each cadence sample records what its CPU interrupted (PC, processor state,
kernel-vs-user), so a detected lockup carries fresh "why" context: the locked
CPU, the observer, how long it has been silent, the last-known PC and PSTATE,
and a `context` field naming whether that sample was in **kernel** code or a
**user** task — the single most decisive clue for a wedge's "why" (an
in-kernel spin versus a spinning user task).
A detection then asks the port for a best-effort recovery
(`WatchdogArch::request_recovery` — a reschedule for a soft lockup, a directed
attention signal for a hard one), recorded with its honest outcome. Detection,
reporting, and recovery are lock-free and allocation-free (safe on the
non-maskable path even while a target CPU holds locks), report each episode
once and self-close on recovery, and stay silent before the clock/sink hooks
are installed or on a never-armed CPU (fail closed). The audit catalogue
documents the `CPU_STALL_DETECTED` / `CPU_STALL_CLEARED`,
`CPU_HARD_LOCKUP_DETECTED` / `CPU_HARD_LOCKUP_CLEARED`, and
`CPU_LOCKUP_RECOVERY` records (`docs/src/architecture/kernel.md`).

The watchdog cadence also enforces a **monopoly guard**: a lone CPU-bound
*user* task is not a lockup (its own cadence sample keeps liveness fresh and a
lone user task owes no scheduler progress), but because the preemption tick is
competitor-gated it would otherwise never be forced back to the dispatch loop —
withholding the CPU from housekeeping and the progress heartbeat, pegging a
core (the monopolisation `AGENTS.md` §17.1 forbids). When `on_watchdog_tick`
samples an Active CPU running a user task whose progress heartbeat is stale
past `watchdog::DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS` (1 s), it requests a
`preempt::request_forced_yield`; the return-to-user preempt point this same
interrupt runs honours it in `preempt_current` **unconditionally** (the
forced-yield latch bypasses the competitor gate), returning the task to the
dispatcher for one housekeeping iteration before it resumes. Kernel code is
never *force*-yielded — the kernel is non-preemptible, so it cannot be
suspended at an arbitrary instruction; long in-kernel work instead gives the
CPU up voluntarily, at its own safe boundary (the in-kernel boundary below).
No new timer is armed — the guard rides the always-on watchdog cadence, so the
tickless invariant holds.

The dispatch loop runs with **device interrupts enabled** — TAIRiX is a
fully preemptive kernel (`AGENTS.md` §17.1). It calls
[`KernelArch::set_device_irqs(true)`] before steady-state dispatching **and
again whenever a scheduler step returns**. The second edge is required
because a timer exception masks interrupt delivery before it preempts a user
task, and the per-CPU mask is not part of the task context: switching directly
from that handler back to the dispatcher otherwise leaves the dispatcher
masked permanently. Restoration occurs only after the step has returned, when
no task or scheduler critical section is in flight. Every in-kernel task and
kthread therefore runs with interrupts deliverable: a long in-kernel operation
cannot mask interrupts for its whole span and starve the preemption one-shot,
the buffered-serial transmit drain (§20), or an interrupt-driven waiter. The kernel stays
**non-preemptible** (§4): a device IRQ taken while an in-kernel task runs
services its source and returns to the *same* task; only a timer tick taken
from EL0/U-mode/ring 3 context-switches *immediately* (each port gates the
preempt callback on the interrupted privilege). The
`preempt_inkernel_qemu_aarch64` integration
vertical proves both halves directly: a busy in-kernel kthread that issues
no `yield` and no syscall still takes the generic-timer IRQ *during* its
span (the EL1 tick callback fires), yet the EL0-preemption callback fires
zero times and the kthread runs to its voluntary completion — under the old
cooperative loop (device IRQs masked across the whole task run) no tick
would be taken and it would spin forever.

The host dispatch-loop regression models an exception return by masking
device IRQs inside one task step and proves the dispatcher performs the
post-step restore before shutdown. The four-core
`stress_qemu_aarch64` vertical covers the production consequence: sustained
EL0 preemption cannot leave every CPU masked and freeze service startup,
device completion, and timer wakeups.

A tick the non-preemptible kernel cannot act on is **never lost**: the
per-tick callback latches it as the CPU's pending preemption
(`kernel/core::preempt` — one lock-free per-CPU flag), and the syscall
dispatch hook consumes the latch when the interrupted syscall completes,
suspending the caller back to the scheduler exactly as a `yield` syscall
would (`DispatchOutcome::Reschedule { action: Yield }`) instead of
returning to user mode with the expired quantum forgotten. The
dispatcher clears the latch immediately before switching a task in, so a
user-mode tick — which already preempted immediately — never doubles into
a spurious yield on the resumed task's next syscall. A task's quantum
overrun is therefore bounded by the remainder of one syscall (each
syscall's in-kernel work is itself bounded, e.g. `console_write`'s 4 KiB
clamp); without the latch, a task whose quantum expired mid-syscall
returned to user mode with no timer armed and could starve every
competitor until its next voluntary yield — cooperative scheduling in
preemptive clothing. If the safe-boundary suspension cannot proceed because
no resumable user handle is published, the policy's no-dispatch hook restores
CFQ's periodic deadline; tickless policies leave the hook inert.

### The in-kernel boundary

Both latches above are consumed on the way back to **user** mode, so on their
own they bound only how long a *user* task withholds a CPU. In-kernel work has
no such return, and it does not have to spin to hold a core: a kernel loop that
issues one bounded operation after another — an in-kernel service kthread
draining its request queue, a filesystem read walking a large file span by
span — stays inside a single dispatched body for as long as its work lasts.
Each operation blocks correctly when it must, so a *slow* device parks the body
and the dispatcher runs; but when the device is **fast** — an emulated virtio
queue, an NVMe namespace whose completion is already in the ring at the
driver's first poll — no operation ever waits, and the whole burst runs without
one return to the dispatch loop. Its housekeeping and heartbeats stop for the
duration and every other runnable task on that CPU waits behind the burst,
which the lockup watchdog reports as an in-kernel stall (`context=kernel`,
`k_site=kernel_body`) once the burst outlasts the soft-lockup threshold.

`kernel/core::preempt::yield_if_owed` is the boundary that bounds it. In-kernel
code calls it *between* units of work; it consumes the same latch and applies
the same competitor-gated decision the return-to-user point applies (both share
one `honour_latched_tick` definition), so a burst gives the CPU up at most one
unit after the quantum expires and costs a single atomic read when nothing is
owed. It suspends nothing before the scheduler hook exists (early boot) and
nothing where no resumable task is published, restoring CFQ's periodic deadline
in that case exactly as the user-mode path does.

Placement is the caller's obligation: the boundary suspends the body, so it may
only sit where no spin lock is held. A point on a path that can *already* park
waiting for a slow device is sound by construction, because that park suspends
the same body in the same place. The two call sites are the storage funnel every
in-kernel device operation passes through (`SharedBlockHandle`'s `with_device`,
which offers the turn before taking the shared device's sleeping lock — see
`docs/src/drivers/block.md`) and the in-kernel `/System` store server's
between-requests boundary, where nothing of the server's own is held.

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
masks device interrupts, drains any pending wake once more, and rechecks
scheduler readiness before sleeping: both its local run queue and the global
overflow awaiting re-homing. The readiness recheck covers scheduler work whose
placement IPI arrived and was consumed after the preceding idle verdict but
before interrupt masking; an IPI arriving after masking remains pending and
wakes the architecture wait. If neither deferred nor scheduler work is ready,
the loop tops up the buffered console transmit one last time and parks the CPU
on the port's race-free idle wait (`wfi` on
aarch64/riscv64, `sti; hlt; cli` on x86_64) rather than halting, then
re-enables interrupts; the armed wakeup one-shot or a device IRQ wakes a
waiter and the loop re-steps and dispatches it. Masking across the park,
draining deferred events, and rechecking scheduler readiness before the
`wfi`/`hlt` close both wake-publication races, so no edge is lost. PID 1 `init`
now launches the perpetual
`/System/Services/devmgr.app/Run` service (a `service` directive in its startup
config, supervised alongside the per-console login sessions), which reads
the discovered hardware tree and parks in `hw_tree_wait` for the life of
the system — the first production caller of this blocking-wait path. The
remaining production-launch work — the reactive bus-driver chain that emits
the nodes `devmgr` reacts to — is staged in `plans/PI.md`
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
`tairix_kernel_irq::IrqTable::fire`. The real `unpark` — which reads the
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
`RescheduleAction::Park`; it is woken either by an **explicit event** or,
with a deadline, by the **timed sweep** (`WaitQueue::sweep`). An explicit
event that is **addressed** — a request posted to one endpoint's server, a
reply for one caller's ticket — wakes exactly its target
(`WaitQueue::wake_task`, the wake-one discipline of `AGENTS.md` §27): the
`ipc_call` post wakes the endpoint's recorded server task and a
`call_reply` wakes the ticket's recorded poster, so unrelated parked
servers and callers stay parked (a broadcast there is a thundering herd
that keeps spuriously-woken tasks runnable and floors the idle load
average at ~1). A **condition broadcast** — endpoint destruction, console
input, a child exit, a hardware-tree bump — still wakes every registered
waiter (`WaitQueue::wake_all`), each of which re-checks its own condition
and re-parks on a miss.

One queue that holds waiters of *many independent objects* keys each
registration instead (`WakeKey`, `WaitQueue::register_keyed`), and its
events wake one key's waiters alone (`WaitQueue::wake_key`) — an O(log n +
woken) range over the key-major waiter index. That is what `STREAM_WAITQ`
does for every pipe and pseudo-terminal on the machine: each ring's two
sides (bytes, space) are minted their own identity
(`kernel/core::pipe::RingWaits`), so a chunk moved on one stream leaves
every other stream's reader and writer parked, while all of them still
share the single deadline index the timed sweep and
`nearest_timed_deadline` fold over. Broadcasting to that queue instead
made every 64 KiB moved anywhere on the machine unpark every stream
waiter on it (`plans/OPEN-DEFECTS.md` D62).

An interrupt-reachable wake never touches the
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

There is a second valid ordering: `park()` may publish `Parked` while the
stackful task body is still switching back. If `unpark()` then changes the
task to `Ready` and enqueues it, the old body's eventual `Park` result is
stale. Dispatch preserves the waker-owned `Ready` transition without
re-enqueuing it; otherwise the stale result could undo the wake while leaving
an unusable queue entry. CFQ's
`a_wake_after_park_publication_survives_the_stale_body_return` regression pins
that transition.

`SleepLock` builds fair mutex contention on the same wait queue. Contenders
retain FIFO registration order and release wakes only the oldest waiter,
avoiding a thundering herd and preventing a long-waiting storage operation
from being displaced indefinitely by newer app-load reads.

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
| `Scheduler::park(id)`            | clears the slot **only** once the body lock proves no CPU is running `id`; otherwise IPIs the running CPU, which clears its own slot on dispatch exit |
| `Scheduler::exit(id)`            | same proof, same fallback: clears the slot only when it holds the body lock, else defers and IPIs |
| `Scheduler::yield_current(id)`   | re-enqueues `id` Ready, then clears slot. **No production caller** — the `yield` syscall reaches the scheduler through `TaskAction::Yield` at dispatch exit instead |

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
* **A slot outlives any remote request to clear it while its task is
  still executing.** The slot is the identity every syscall from that
  task is attributed through, so clearing it from another CPU while the
  task runs in user mode would leave its next trap unattributable.
  `park` and `exit` therefore clear it only while holding the task's
  body lock — which no CPU can hold while dispatching that task — and
  otherwise leave the clear to the running CPU's own dispatch exit,
  sending an IPI so it gets there promptly.
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

### Per-task CPU-time accounting

Each dispatch brackets the task body with two `SchedulerArch::ticks_now`
reads and accumulates the span on the task (`run_ticks`), so cumulative
on-CPU time advances exactly as work happens — tickless, no periodic
sampling, and the hot path pays one subtraction, never a unit
conversion. The figure is exposed read-only through
`SchedulerPolicy::cpu_ticks_of(id)` in raw port ticks; the reader (the
System Information process feed in `kernel/core`) converts to
nanoseconds at observation time through `KernelArch::ticks_to_ns`, the
same calibrated frequency the port's `monotonic_ns` uses, so the two
clocks can never diverge. A drained (reaped) task reports
`SchedError::NoSuchTask`, never a fabricated zero; both policies
implement the same contract and the shared conformance suite pins it
(`cpu_time_is_accounted_per_dispatch`).

The span of a run that has **started but not yet returned** to the
dispatch loop is included live: the reported figure adds the elapsed
time since the current run's `last_started` whenever the task is still
the current task on its home CPU. Without this a tickless, CPU-bound
task that never yields — correctly left unpreempted with its one-shot
disarmed (a sole runnable task takes no timer interrupts) — would
contribute nothing to the accounting until it finally yielded, so a
utilisation sample would read it as idle between dispatch returns and
then spike when the whole span settled at once. The current-task slot
is cleared *before* `settle_run_accounting` credits the completed span,
so a span is counted either as in-flight or as settled, never both.

The same dispatch bracket also accumulates the span on the dispatching
CPU (`busy_ticks`), exposed read-only through
`SchedulerPolicy::cpu_busy_ticks(cpu)`. The per-CPU total survives task
exit — a reaped task's time stays in its CPU's total — so it is the
truthful cumulative "busy" half of the System Information busy/idle
utilisation split (`CPU_TIME_STATS`); the introspect reader derives idle
as the remainder of the same monotonic sample. It too includes the
in-flight span of the task currently dispatching on the CPU, so a core
running a sole never-yielding task reads as busy moment to moment rather
than idle-then-spiking. An out-of-range CPU reports
`SchedError::NoSuchCpu`, never a fabricated zero; the same conformance
case pins that the CPU total equals the sum of the work dispatched on
it.

The same bracket counts one context switch per dispatched body, exposed
read-only through `SchedulerPolicy::cpu_switches(cpu)`, and
`SchedulerPolicy::queue_depth(cpu)` samples the runnable tasks queued on
a CPU (excluding the running task, which sits in the current slot).
Together with `preemption_count(cpu)` these are the per-CPU figures the
System Information `CPU_LOAD` query reports (`plans/STRESSTEST.md` ST1);
the busy/idle time split stays in `CPU_TIME_STATS`, so no figure is
served twice. Both fail closed on an out-of-range CPU, and the shared
conformance suite pins the behaviour
(`load_observations_track_dispatch`).

### `yield_current` vs body-returned `TaskAction::Yield`

`Scheduler::yield_current(task_id)` models a **voluntary syscall
yield**: the task is `TaskState::Running` on its CPU, the caller wants
to relinquish the rest of its quantum, and the scheduler re-Readies the
task and clears the slot.

It has **no production caller**, and adding one needs care. It clears
the current-task slot but suspends nothing, so it is only ever sound
when the caller suspends immediately afterwards. A blocking wait that
called it as a fallback and then returned to user space left the task
running as a caller the next syscall could not attribute — which halted
the CPU outright. The `yield` syscall does not use it: the dispatch hook
returns `Reschedule { action: Yield }` and the scheduler re-enqueues
from the `TaskAction::Yield` the kthread reports at dispatch exit.

`TaskAction::Yield` returned by a task body is the
**body-loop yield**: it is processed by `dispatch` along with
MLFQ demotion bookkeeping (`yields_at_band` /
`yields_before_demotion`). The two notions are deliberately
distinct so the syscall handler is not on the hook for demotion
policy, which would be interface creep into the syscall layer.

### Parking a waiter on its *live* CPU

A syscall that blocks (`irq_wait`, `hw_tree_wait`, `ipc_call`,
`call_recv`, `waitset_wait`, the users-DB / app-store waits) parks its
task off the run queue and re-polls each time it is woken. Between two
polls the task is `Parked`, so it can be woken and re-dispatched
(work-stolen) onto a **different** CPU than it parked on. The suspend
mechanism — `reschedule_current(cpu, Park)` — is keyed to a specific
CPU's resume handle, so it must be told the CPU the task occupies *right
now*, never a CPU id captured once before the loop: a stale id selects the
handle of whichever task now runs on that core and suspends **that** task
instead, writing the caller's continuation into its save area and switching
to its dispatcher — the two then resume each other's kernel contexts under
the wrong page-table root and the innocent task dies on a fault it never
took (`plans/OPEN-DEFECTS.md` D44). Every wait loop therefore reads the live
CPU inside the loop, at each park, so a mid-wait migration is always
handled; `dispatch_step` additionally refuses to switch into a suspension
point that is not on the task's own kernel stack, so a mispairing fails
closed instead of corrupting.

The **same live-CPU rule binds the syscall completion path**, not only
the park loop. The dispatch hook reads the caller's CPU once at entry to
identify the caller (correct — the task is running on that CPU then), but
a blocking handler parks and can resume (work-stolen) on a **different**
CPU. When the handler returns, the completion path hands a CPU to the
port's `reschedule_current` — which runs on the core the task is on
*now*. It must therefore re-read the live CPU after the handler returns,
never reuse the entry CPU: passing the stale entry CPU drives
`reschedule_current` against a different core's resume handle, switching
that core through another task's saved context and corrupting both — a
wild fault that kills an unrelated task. The re-read is safe because the
kernel is non-preemptible and nothing between it and the
`reschedule_current` call parks, so the task cannot migrate again in that
window.

## Secondary-CPU bring-up barrier

Secondary CPUs are brought online **one at a time**, behind a per-core
acknowledgement barrier, *before* the boot CPU spawns PID 1. The boot CPU
releases a secondary, then waits (bounded, fail-loud) for that core to
publish an online acknowledgement — the edge `run_secondary` sets, on the
core itself, only after the arch port's secondary entry has adopted the
kernel translation regime and armed the core's per-CPU interrupt state —
before it releases the next core and before it returns to mutate shared
kernel state.

This serialisation is a correctness requirement, not a nicety. A
secondary released last must finish adopting the shared kernel
translation regime before the boot CPU begins mutating shared kernel
structures (spawning PID 1, allocating page tables); otherwise that core
can fault mid-bring-up on real hardware — a cache/coherency hazard a
cacheless emulator never exhibits, observed as the highest dense id
deterministically never coming online (present in the topology, zero
context switches). A core that fails to check in within the budget is
audited (`no_online_ack`) and the boot proceeds on the cores that did,
rather than wedging or silently running a half-brought-up core.

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
   | `doomed` (§5)  | not `Park`    | `Exit`           |
   | `Exited`       | *anything*    | `Exit`           |
   | *anything*     | `Exit`        | `Exit`           |
   | `Parked`       | *anything*    | `Park`           |
   | *anything*     | `Park`        | `Park`           |
   | otherwise      | `Yield`       | `Yield`          |

5. **SMP quiescence and reclamation ownership.** `exit(id)` returns an
   `ExitDisposition` so its caller can reclaim a task's resources
   *safely* on SMP. Reclaiming a task's address space (or anything its
   user code can still reach) while another CPU is still executing that
   task turns a legitimate access into a wild fault, so `exit` never
   reports a still-running task as reclaimable. The task's **body lock**
   is held for the entire time a dispatch executes the task (its
   user-mode run and any syscall handler nested inside it), so acquiring
   it in `exit` *proves* no CPU is executing the task:

   * `try_lock` **succeeds** → the task is quiescent: `exit` retires it
     (drops the body, marks `Exited`) and returns
     `ExitDisposition::Quiesced`. The caller owns teardown and reclaims
     now.
   * `try_lock` **fails** → a dispatch owns the task. `exit` marks it
     `doomed` (a first-wins flag), IPIs the running CPU to force a prompt
     reschedule, and returns `ExitDisposition::Deferred`. It does **not**
     mark the task `Exited` — the owning dispatch performs that final
     transition itself when its body returns (invariant 4), so no policy
     ever exposes an `Exited` task that is still running. The caller must
     **not** reclaim; the deferred teardown is landed by the kernel/core
     dispatch loop (`land_running_kill`) once the task is quiescent.
   * The task was already terminal (or a prior termination owns its
     teardown) → `ExitDisposition::AlreadyExited`; reclaim runs exactly
     once no matter how many kills arrive.

   A `doomed` task that returns `Park` (it blocked mid-handler holding
   kernel state) is **not** force-exited: its kill is landed at the
   syscall boundary once the handler unwinds, so handler state is never
   reclaimed under. `kernel/core` (`procsignal`) drives the caller side —
   the signal-terminate path and the driver-unload path both branch on the
   disposition; see the kernel signals doc.

6. **Task identity is stable.** `TaskId` values are never recycled
   within a single scheduler instance. Stale references therefore
   produce `SchedError::NoSuchTask` rather than waking the wrong task.

## Crate layout (§17.1)

The scheduler is split per `AGENTS.md` §17.1 into a contract crate and
one policy crate per implementation:

* `kernel/sched/api` (`tairix-kernel-sched-api`) — the
  `SchedulerPolicy` trait, the policy-neutral lifecycle vocabulary
  (`Priority`, `TaskState`, `TaskAction`, `TaskContext`, `TaskId`,
  `SchedError`, `StepOutcome`, `SchedulerConfig`), the re-exported
  Arch HAL surface (`CpuId`, `SchedulerArch`), the host `TestArch`
  double, and the shared `conformance` suite.
* `kernel/sched/cfq` (`tairix-kernel-sched-cfq`) — the CFQ policy
  described above, implementing `SchedulerPolicy`. This is the default.
* `kernel/sched/eevdf` (`tairix-kernel-sched-eevdf`) — the EEVDF policy
  described above, implementing `SchedulerPolicy`.
* `kernel/sched/mlfq` (`tairix-kernel-sched-mlfq`) — the MLFQ policy
  described above, implementing `SchedulerPolicy`. The three are siblings
  (`AGENTS.md` §2.2 carve-out — parallel policies are deliberate, not
  duplication); adding another policy means adding a sibling crate,
  never editing an existing one.
* `kernel/core` is the single build-time selection point: exactly one
  `scheduler-*` feature is active per image (`scheduler-cfq` by
  default, `scheduler-eevdf` or `scheduler-mlfq` with
  `--no-default-features --features scheduler-<impl>`). It re-exports the
  chosen policy as
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
  in-tree MLFQ policy; `kernel/sched/eevdf/tests/conformance.rs` and
  `kernel/sched/cfq/tests/conformance.rs` run the identical suite against
  EEVDF and CFQ — proving the policies are interchangeable behind the
  contract.
* `kernel/sched/cfq/src/scheduler.rs` `#[cfg(test)] mod tests` —
  CFQ-specific coverage: that a sole runnable task keeps the periodic
  tick armed (the non-tickless carve-out) while an idle CPU disarms,
  weight-proportional dispatch, work-stealing, park/unpark, and the
  `on_timer_tick` preemption counter.
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
