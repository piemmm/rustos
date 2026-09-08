# CPU frequency scaling

Status: **done** for the Raspberry Pi (aarch64). No other port has a
frequency mechanism, and none is staged — the kernel half is
architecture-neutral and ready for one.

## Why this exists

On a Raspberry Pi the ARM core clock belongs to the `VideoCore` firmware, not
to any register the ARM cores can reach, and the firmware leaves it wherever
it last put it. After the boot window that is `arm_freq_min` — 600 MHz on a
Pi 4B whose parts are rated at 1.5 GHz. Nothing raises it again unless an OS
driver asks, so before this landed a Pi ran at 40% of its rated speed however
much work it had, and the Switchboard reported exactly that.

The reported symptom also exposed a second defect in the *measurement* path,
fixed here: see "The estimator's idle exclusion" below.

## Shape

Three pieces, in the order a boot reaches them.

### 1. The boot floor (`kernel/arch/aarch64/src/firmware.rs`)

One `GET_MAX_CLOCK_RATE` + `SET_CLOCK_RATE` over the pre-MMU mailbox, before
anything else runs, so mounting the root volume, unlocking it, and reaching a
login are not served at the minimum. A one-shot floor, not a policy: it takes
no view of utilisation and never lowers the clock.

The module also owns the pre-MMU mailbox doorbell and its DMA-visible
property buffer, which the framebuffer boot console borrows through
`with_transport` — one transport construction, two early consumers. Each
exchange is split into a pure half taking a transport (`raise_over`) so the
sequence is host-testable against the `lib/vcmailbox` mock; QEMU models no
`VideoCore`.

### 2. The governor (`kernel/core/src/cpufreq/`)

Policy is kernel-side because it must answer inside the idle transition it is
reacting to; an IPC round trip in front of every wake is the latency the
subsystem exists to remove. It is what Linux's `schedutil` is, with the
mechanism moved out to user space.

* `governor.rs` — pure, host-tested policy: the utilisation filter (`fold`), a
  25% headroom over the measured rate (`schedutil`'s ratio), quantisation up
  to the mechanism's step, and the clamp.
* `domain.rs` — the one binding, the published `{ seq, target_hz }`, and the
  publish-and-park loop.
* `estimate.rs` — the live-clock estimator (unchanged in role; see below).

**Behaviour.** Work arriving on an idle CPU raises the rate to the maximum at
once. Sustained partial load settles proportionally. A program launch holds
the maximum across the load and start, which is mostly spent waiting on a
volume rather than accruing utilisation. A machine that falls quiet walks back
to the minimum a step per response window and then takes no wakeup at all.

**Nothing arms a timer.** The dispatch loop already brackets idle exactly, so
both hooks ride transitions it was making anyway, and the filter is advanced
lazily when read. On a machine with no frequency driver the whole cost is a
per-CPU slot lookup and one relaxed load per dispatch step.

**Load-bearing decisions.**

* *The edge, not the step.* `note_active` runs at the top of every
  dispatch-loop iteration, so it detects the idle→active *edge* rather than
  treating each iteration as a resumption. Both halves depend on that: the
  estimator would otherwise restart its sampling window every dispatch, leaving
  each one too short to divide and publishing nothing at all. The edge marker
  lives in `CpuState` and is maintained whether or not a mechanism is bound,
  since the estimator needs it on every port.
* *Idle brackets, not scheduler ticks.* Utilisation comes from
  `kernel/core/src/init.rs`'s idle park, deliberately **not** from
  `SchedulerPolicy::cpu_busy_ticks` — whose `in_flight_ticks` takes an
  `RwLock`, and a lock shared with a timer interrupt is a self-deadlock.
* *O(1) on the hot path.* Surveying every CPU is O(cores) and happens in the
  waiter, at most once per window. A wake can only ever *raise* the rate, and
  it raises it to the declared maximum, which needs no survey. A wake is
  flagged only when the previous boost had lapsed, bounding wakes to one per
  window however often the machine idles.
* *One lock, and the pair it protects.* The survey, the decision, and the
  publication happen in one critical section, so the sequence and the rate can
  never be read out of step and neither is copied anywhere. Nothing an
  interrupt handler runs touches that lock.
* *A sequence, not the rate, is what a driver waits on.* Firmware clamps and
  rounds, so "block until target equals what I applied" would spin forever.
* *One policy constant.* `RESPONSE_WINDOW_NS` (100 ms) is the filter's time
  constant, the boost window, and the step-down pacing — the same question
  asked three ways.
* *The bind seeds a clean slate plus a boost.* A filter dated time zero would
  read the whole boot as idle and ask for the *minimum* on a machine that is
  demonstrably busy launching the driver; and on a re-bind a CPU recorded
  active under the previous binding would count as busy forever.
* *Boost-on-wake does not degenerate here.* On a kernel whose CPUs wake for
  housekeeping every stray tick would re-stamp the boost. TAIRiX is tickless,
  so a wake really is work arriving.

**The filter is not composable.** The blend is linear in the span's length,
so the same duty cycle switched coarsely and finely settle to slightly
different values. Both are monotone in the duty cycle and land within a step
or two after quantisation, which is all a rate decision needs — but nothing
may assume granularity independence. What the design does rely on, and what
holds exactly, is that a lazy read equals a commit at the same instant.

### 3. The mechanism (`drivers/cpufreq/rpi/`)

A user-space driver `devmgr` autoloads on a discovered
`raspberrypi,firmware-clocks` node. It maps no MMIO and takes no interrupt:
its only path to the clock is the `vcmailbox` service. It reports the range
the firmware declares — never a board constant, so an overclocked board is
driven over its own range — takes the mechanism role, and parks in the kernel
applying targets.

It does **not** report the applied rate back. The firmware clamps and rounds,
so the request is not the truth, and the kernel has a better witness: the
per-CPU estimator measures the live core clock from the silicon's own
counters, so a rate that never took effect shows up as a measured frequency
that does not match the target.

## The seam

| | |
|---|---|
| `cpufreq_bind(limits) -> handle` | syscall 122, `CAP_CPUFREQ`, audited |
| `cpufreq_wait(handle, last_seq, out)` | syscall 123, `CAP_CPUFREQ`, not audited |
| `CpuFreqLimits { min_hz, max_hz, step_hz }` | `lib/abi/src/cpufreq.rs` |
| `CpuFreqTarget { seq, target_hz }` | as above |

`CAP_CPUFREQ` (id 46) guards the whole DVFS surface: a holder that pins the
minimum starves the machine of throughput and one that pins the maximum drives
a passively-cooled board into firmware thermal throttling, neither of which
any per-process limit bounds. Granted to the autoloaded driver alone.

There is no timeout argument: the kernel wakes the waiter both on a demand
change and at the point its own decay would next move the rate, so the wait is
bounded without the caller pacing it. The binding is released by the shared
task-reclaim path, so a driver that dies frees the role.

## The estimator's idle exclusion

The live-frequency estimator divides a core-clock counter delta by a
fixed-rate reference delta. That is only a *frequency* if both counters stop
together when the core does:

| port | core counter | reference | halts together? |
|---|---|---|---|
| x86_64 | `IA32_APERF` | `IA32_MPERF` | yes |
| aarch64 | `PMCCNTR_EL0` (gated in `wfi`) | `CNTVCT_EL0` | **no** |
| riscv64 | `cycle` CSR (gated in `wfi`) | `time` CSR | **no** |

So a window containing idle reported `frequency × duty_cycle`: a core flat out
at 1.5 GHz for 40% of a window read as 600 MHz — the same figure the firmware
defect produced, which is what made the two hard to tell apart. The estimator
now re-seeds its baseline as a CPU leaves its idle park (`estimate::rebase`,
called from the same hook the governor uses), so every published figure spans
running time only. One arch-neutral definition fixes aarch64 and riscv64 and
is a no-op for x86_64's already-correct pair.

A window too short to divide accurately (under 1024 reference ticks) publishes
nothing rather than a noisy figure.

## Not done

* **No second frequency domain.** The Pi's four cores share one ARM clock, so
  the kernel carries one binding. A part with per-cluster clocks needs more
  than one, which is an in-place change to `domain.rs` when such a port
  arrives — not staged, because nothing would hold the second binding.
* **No `HwDeviceClass` variant and no driver class trait.** The driver binds by
  `compatible` and serves no endpoint, so both would be surface with no
  reader.
* **The System Information API reports the measured frequency only**, not the
  governor's target. The measurement is the honest answer to "how fast is this
  core running"; a target would only tell a reader what was asked for.
* **On-metal acceptance.** QEMU models no `VideoCore`, so the firmware
  exchanges are proven against the mock. The live path — boot, idle, load,
  launch — is verified on a Pi 4B.
