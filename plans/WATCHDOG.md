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
  watchdog interrupt". Hard-lockup basis.
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
task, and the recovery kind + outcome.

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
