# CPU frequency scaling

TAIRiX runs each CPU at the rate its work justifies: work arriving on an idle
core is served at full speed immediately, sustained partial load settles
proportionally, and a machine that falls quiet walks back to its minimum. The
policy is in the kernel and the mechanism is a user-space driver.

The staged design and its decisions are `plans/CPUFREQ.md`.

## Two questions, two answers

Frequency splits into questions that must not be confused:

* **What is this core running at?** Measured, from the silicon's own counters,
  by the per-CPU estimator (`kernel/core/src/cpufreq/estimate.rs`) over the
  Arch HAL `coreclock` slice. Reported by the System Information API as a
  CPU record's `current_freq_hz`. It never asks for a rate and never trusts
  one it was told.
* **What *should* it be running at?** Decided by the governor
  (`kernel/core/src/cpufreq/governor.rs`) and published to whichever driver
  holds the mechanism role. It never reports a target as though it were
  achieved.

Keeping them apart is what makes the pair honest: a mechanism whose rate
silently failed shows up as a measured frequency that does not match the
target, rather than as a target read back to itself.

## Where the policy lives, and why

In the kernel. The governor has to answer inside the idle transition it is
reacting to — a target computed an IPC round trip after the work appeared
arrives after the latency it exists to avoid. This is what Linux's `schedutil`
is, and for the same reason.

The *mechanism* is in user space, which is where TAIRiX differs: applying a
rate is a device operation. On a Raspberry Pi it is a `VideoCore` firmware
property exchange; elsewhere it might be a register write or a
power-controller transaction. So the driver runs unprivileged, reaching its
hardware through the same capability-gated paths any driver uses.

## What the governor does

| Situation | Rate asked for |
|---|---|
| A CPU leaves idle | the maximum, at once |
| A program is launched | the maximum, across the load and start |
| Sustained load, utilisation `u` | `1.25 × max × u`, rounded up to a step |
| Every CPU idle for a window | walks down a step per window to the minimum |
| Settled at the minimum | unchanged, and no wakeup is armed |

The quarter of headroom is `schedutil`'s: a CPU saturated at its current rate
cannot report how much faster it wanted to go, so the target overshoots to
find out.

### Utilisation without a tick

The dispatch loop already brackets idle exactly — it parks in one place and
resumes in one place — so every span between transitions is wholly busy or
wholly idle. Each transition folds its own CPU's span into a per-CPU filter
weighted by how long it lasted, and reading the filter folds the span since
the last commit at the moment somebody asks. So an idle CPU's utilisation
decays with nothing armed to make it happen, and the kernel stays tickless.

The filter carries the duty cycle across idle: a task that runs 2 ms in every
100 ms holds its CPU near 2%, so a low-duty background service does not read
as a busy machine.

### The launch boost

A program launch is latency-sensitive before it has done anything measurable,
and much of it is spent waiting on the volume the bundle is read from — during
which every CPU can be idle and no utilisation accrues at all. The `spawn`
path therefore stamps the boost directly, after the authority check so a
refused caller cannot raise the machine's clock by asking.

### Cost when nothing is bound

A per-CPU slot lookup and one relaxed load per dispatch step, to tell an idle
resumption from an ordinary dispatch — and nothing at all beyond that. Every
port but the Raspberry Pi has no frequency mechanism today, and pays exactly
that.

## The mechanism seam

A driver declares the range it can deliver and then blocks for targets:

```text
cpufreq_bind(&CpuFreqLimits { min_hz, max_hz, step_hz }) -> handle
cpufreq_wait(handle, last_seq, &mut CpuFreqTarget)       -> 0
```

Both need `CAP_CPUFREQ`, which guards the whole DVFS surface — a holder that
pins the minimum starves the machine of throughput, and one that pins the
maximum drives a passively-cooled board into firmware thermal throttling.
It is granted to the autoloaded frequency driver and to nothing else.

A driver waits on the target's **sequence**, not its rate: firmware clamps a
request to the clock's range and rounds it to a rate the PLL can synthesise,
so "block until the target equals what I applied" would spin forever. Targets
coalesce latest-wins, so a driver that was busy applying one rate observes
only the newest.

There is no timeout to pass. The kernel wakes the waiter both on a demand
change and at the point its own decay would next move the rate, so the wait is
bounded without the driver pacing it — and a machine settled at its minimum
arms nothing at all.

Exactly one mechanism serves a machine; a second bind is refused rather than
arbitrated. The binding is released by the shared task-reclaim path, so a
driver that exits, faults, or is killed frees the role for a replacement.

## Per-target support

| Target | Mechanism |
|---|---|
| `aarch64` (Raspberry Pi) | `drivers/cpufreq/rpi` — the `VideoCore` firmware ARM clock, plus a boot-time floor before the driver loads |
| `aarch64` (QEMU `virt`) | none; the board exposes no frequency control |
| `x86_64`, `riscv64`, `wasm32` | none |

A target with no mechanism leaves the governor inert: it publishes nothing,
folds nothing, and costs one relaxed load per dispatch step.

### The Raspberry Pi, and why the board needs this at all

The ARM clock belongs to the firmware, which leaves it wherever it last put
it — after the boot window, `arm_freq_min`, 600 MHz on a Pi 4B rated at
1.5 GHz. Nothing raises it unless an OS driver asks, so a board with no
frequency driver runs at 40% of its rated speed however much work it has.

Two things close that. The aarch64 port asks the firmware for full speed
during its pre-MMU discovery (`kernel/arch/aarch64/src/firmware.rs`), so
mounting the root volume, unlocking it, and reaching a login are not served at
the minimum; and `devmgr` then autoloads the frequency driver, whose governor
targets take over. Both read the ceiling from the firmware rather than
assuming one, so a board whose `config.txt` raises `arm_freq` is driven over
its own range.

## Measuring the live clock honestly

The estimator divides a core-clock counter delta by a fixed-rate reference
delta. That is only a *frequency* if both counters stop together when the core
does — and only x86_64's `APERF`/`MPERF` pair does. On aarch64
(`PMCCNTR_EL0` over `CNTVCT_EL0`) and riscv64 (the `cycle` CSR over the `time`
CSR) the core counter is gated while the PE sits in `wfi` while the reference
keeps running, so a window containing idle would report `frequency × duty
cycle` — a core flat out at 1.5 GHz for 40% of a window reading as 600 MHz.

The estimator therefore re-seeds its baseline as a CPU leaves its idle park,
so every published figure spans running time only. A window too short to
divide accurately publishes nothing rather than a noisy figure — `0` and a
clear `CPU_INFO_FLAG_FREQ_MEASURED` are the honest unknown a reader falls back
from, never a fabricated rate.
