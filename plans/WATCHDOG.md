# CPU-lockup watchdog

Binding plan for TAIRiX's first-class CPU-lockup watchdog: detect, diagnose,
and best-effort recover from **soft** and **hard** CPU lockups, loudly enough
to explain *why*, without perturbing normal execution.

## Model (done)

Two per-CPU heartbeats plus an activity class, all in
`kernel/core::cpu_state::CpuState`:

- **progress** (`last_progress_ns`) — stamped once per dispatch-loop iteration
  (`watchdog::note_progress`): "the scheduler ran here". Soft-lockup basis.
- **liveness** (`last_seen_ns`) — stamped by the port's non-maskable ~1 Hz
  cadence sample (`watchdog::on_watchdog_tick`): "this CPU still takes the
  watchdog interrupt", *and* once per dispatch-loop iteration
  (`watchdog::note_alive`): reaching the dispatcher is itself proof the CPU is
  alive and taking interrupts (it either just woke from `wfi` by taking one, or
  is running continuously). Hard-lockup basis. Both are needed: the cadence
  sample covers a lone user task that never returns to the dispatcher, and the
  dispatch-loop stamp restarts the liveness window when a CPU resumes work
  after an idle park — without it a CPU returning to `Active` would carry the
  stale heartbeat from *before* the park (its sample is not taken while parked)
  and be falsely hard-locked the instant it republishes `Active`.
- **activity** (`wd_activity`: Offline / Idle / Active) — published by the
  dispatch loop so only a CPU that *owes* progress is judged; a parked (idle)
  or not-yet-online CPU is never flagged (fail closed).

Detection (`kernel/core/src/watchdog.rs`, host-tested):

- **Soft lockup** — an Active CPU whose progress heartbeat is stale past
  `DEFAULT_SOFT_LOCKUP_THRESHOLD_NS` (10 s) while it is still taking its
  watchdog sample. Caught same-CPU by the armed preemption tick (`check_stall`,
  contended CPUs only — no lone-task false positive) *and* cross-CPU by the
  buddy scan, but only when the CPU was last seen **in the kernel** (a lone,
  preemptible user task owes no scheduler progress).
- **Hard lockup** — an Active CPU whose liveness heartbeat is stale past
  `DEFAULT_HARD_LOCKUP_THRESHOLD_NS` (10 s): it has stopped taking even the
  non-maskable sample. Only observable by *another* CPU's `on_watchdog_tick`
  scan.

Every CPU's cadence sample continuously refreshes its own last-known context
(`wd_ctx_pc`/`wd_ctx_task`/`wd_ctx_aux`/`wd_ctx_in_kernel`), so a buddy that
detects a lockup already has fresh "why" data. Reports are lock-free,
allocation-free, once-per-episode (latched), and self-closing (a recovery
record on clear). Audit events: `CPU_STALL_DETECTED` (4080) / `_CLEARED`
(4081), `CPU_HARD_LOCKUP_DETECTED` (4082) / `_CLEARED` (4083),
`CPU_LOCKUP_RECOVERY` (4084) — carrying cpu, observer, stalled_ms, pc, pstate,
task, `context` (whether the last-known sample was in **kernel** code or a
**user** task — the most decisive clue for a wedge's "why"), and the recovery
kind + outcome.

## Monopoly guard: a lone CPU-bound user task (done)

The ordinary preemption tick is competitor-gated (`preempt::preempt_current`):
a *lone* runnable user task with no competitor is deliberately left running,
because rescheduling to the same sole task only churns the address-space/TLB
switch. Correct for scheduling — but a CPU-bound user task that never issues a
syscall then never returns to the dispatch loop, so its per-dispatch
housekeeping (deferred-wake and console-transmit drains) and its progress
heartbeat stop and it withholds the CPU indefinitely (the monopolisation the
charter forbids). It is *not* a lockup — its own ~1 Hz cadence sample keeps
liveness fresh, and a lone user task owes no scheduler progress — so neither
detector fires; it simply pegs a core.

The watchdog cadence closes this. When `on_watchdog_tick` samples an `Active`
CPU running a **user** task (`!in_kernel`) whose progress heartbeat is stale
past `DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS` (1 s — well below the 10 s
soft/hard thresholds), it calls `preempt::request_forced_yield`. The
return-to-user preempt point this same interrupt runs honours the request in
`preempt_current`, suspending the task back to the dispatcher **unconditionally**
(the forced-yield latch is *not* competitor-gated, unlike `note_preempt_tick`).
The dispatch loop then runs one iteration — re-stamping progress, draining
housekeeping — before re-dispatching the task. Kernel code is never
force-yielded (the kernel is non-preemptible). Crucially this arms **no new
timer**: it rides the watchdog cadence already firing on the CPU, so the
tickless invariant is preserved. Latch: `CpuState::force_yield`, cleared by the
dispatcher's `clear_preempt_pending` so the incoming task earns a fresh guard
window. Host-tested (`monopolises_cpu` predicate; `on_watchdog_tick` sets the
latch for a monopolising user CPU only; `preempt_current` honours a forced
yield with no competitor).

Recovery is best-effort through the Arch HAL `WatchdogArch::request_recovery`
(`kernel/arch/api/src/watchdog.rs`): a soft lockup → reschedule the offending
CPU; a hard lockup → a directed attention signal; a port with no channel →
honest `unsupported` (the detection is still loud). Cost: one relaxed atomic
store per dispatch and per cadence sample; the scan is O(online CPUs) off any
lock.

## aarch64 delivery (done, metal validation pending)

The kernel runs at EL1 **non-secure** on a **GICv2** (QEMU `virt`, RPi4
GIC-400). The cadence sample is the EL1 **virtual** generic timer
(`CNTV_*_EL0`, GIC PPI 27), armed ~1 Hz via the relative `CNTV_TVAL_EL0`
(independent of the physical-timer preemption one-shot; no `CNTVOFF`
dependency), delivered as an ordinary **IRQ**. So hard-lockup detection is the
cross-CPU *buddy* kind: a CPU that stops taking its watchdog IRQ is seen by a
healthy CPU that still takes its own. `kernel/arch/aarch64/src/watchdog.rs`
owns the timer + the `WatchdogArch` recovery (directed reschedule/attention SGI
via `gic::send_sgi`); `exceptions::handle_irq` dispatches PPI 27; the bin
(`gic_irq.rs`) installs the callback (reads `ELR_EL1`/`SPSR_EL1`, builds the
neutral `WatchdogSample`) and arms the cadence on every online CPU.

Limitation (hardware, not a defect): on GICv2 non-secure there is no
non-maskable channel, so a CPU wedged with IRQs masked cannot be *interrupted*
for a live remote register dump or forced recovery — it is detected and
reported loudly from last-known context, and recovery is best-effort. This is
inherent to a GICv2 non-secure kernel, where the only non-maskable channel
(FIQ / Group 0) belongs to the secure world; the watchdog's cross-CPU buddy
detection is the correct and complete design for that hardware. A board that
*does* expose a non-maskable channel — a GICv3 core with `ICC_PMR` priority
masking — can deliver the cadence sample as a true pseudo-NMI (interrupting an
IRQ-masked core for a live remote dump and forced recovery) behind this same
unchanged `WatchdogArch` / `WatchdogSample` surface, with no `kernel/core`
change; that is a per-port delivery detail for such a board, not a pending
upgrade to the design here.

## Other architectures (staged)

x86_64 / riscv64 / wasm32 keep the soft detector (`check_stall` on their tick
paths) and inherit hard-lockup detection when each wires its own
`on_watchdog_tick` cadence + `WatchdogArch` (NMI/LAPIC-deadline on x86_64, the
higher-privilege timer on riscv64). The architecture-neutral core needs no
change — the HAL surface is delivery-agnostic.
