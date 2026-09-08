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
| A program is launched | the maximum, across the load and start |
| The busiest CPU past half busy | the maximum, outright |
| Load with utilisation `u` | `1.25 × max × u`, rounded up to a step |
| Sustained load | climbs as the filter fills, reaching the maximum |
| Just reached the maximum | held there for at least 600 ms |
| Every CPU idle for a window | walks down a step per window to the minimum |
| Settled at the minimum, nothing running | unchanged, and no wakeup is armed |

The quarter of headroom is `schedutil`'s: a CPU saturated at its current rate
cannot report how much faster it wanted to go, so the target overshoots to
find out. With that headroom and the clamp, anything under about a third of a
core already asks for the floor on a 600–1500 MHz part.

### Past half busy, and the hold at the top

Two operator decisions shape the top of the range, both trading some power for
throughput and latency.

The proportional rate reaches the ceiling on its own only at four fifths of a
core, which leaves a genuinely busy machine climbing through rates it will not
stay at. **Past half a core the ceiling is asked for outright.**

And **once the ceiling is asked for it is held for at least 600 ms.** Arriving
at the top and dropping straight off again costs a mechanism round trip in each
direction and serves the part of the burst that mattered at the lower rate; six
tenths of a second spans several bursts of a workload that is intermittent at
the filter's own granularity, so the rate stops flapping there. The hold runs
from the instant the ceiling was reached and is never refreshed by a machine
that simply stays there, and it never delays a *rise* — it exists to stop the
rate coming off the top, not to slow it getting there. While it stands nothing
else can move the published rate, so the waiter parks on its expiry.

Together they are deliberately biased toward the top of the range: exceeding
half a core takes just over 50 ms of work inside the filter's 100 ms window,
and that alone buys 600 ms at the ceiling. A workload that bursts that hard
every half second will sit at the maximum more or less continuously. That is
the intended trade — responsiveness over power — and it is the first place to
look if a board runs hotter than expected.

### Leaving idle is not a reason to go fast

A CPU that wakes to do a millisecond of work and parks again has not earned
the top rate, and only the filter can tell that apart from real work, because
only the filter measures it. So a wake grants no rate: it merely tells the
governor to start looking again.

An earlier revision did grant the maximum for a window on every wake, and
because each wake pushed the deadline further out, any machine waking more
than ten times a second — an idle desktop with a compositor and a clock — sat
at its ceiling permanently at one percent load, warm enough for the firmware
to start soft-throttling. Utilisation never got a say.

### Why work that keeps running still speeds up

Work that never stops produces no transition to observe, so the rise has to be
looked for rather than waited for. While any CPU is active the waiter revisits
the target four times per window, which is the coarsest cadence that still
shows the ramp: it bounds a climb to a handful of mechanism round trips
instead of one per step, and it arms nothing at all on a quiet machine.

### Utilisation without a tick

The dispatch loop brackets *work*: a dispatch that ran a task body opens a
CPU's busy span and one that found nothing to run closes it, so every span
between transitions is wholly busy or wholly idle. Each transition folds its
own CPU's span into a per-CPU filter weighted by how long it lasted, and
reading the filter folds the span since the last commit at the moment somebody
asks. So an idle CPU's utilisation decays with nothing armed to make it
happen, and a busy one's rises as it runs, with no periodic timer behind
either.

The filter carries the duty cycle across idle: a task that runs 2 ms in every
100 ms holds its CPU near 2%, so a low-duty background service does not read
as a busy machine.

### A dispatcher looking for work is not a CPU doing any

Utilisation for a rate decision must be the same quantity the system reports
as busy time — `Scheduler::cpu_busy_ticks`, the time spent inside task bodies,
which is what `top` and the Switchboard render — or the two contradict each
other and one of them is lying.

An earlier revision opened the bracket at the top of every dispatch-loop
iteration and closed it only where the loop committed to its `wfi`. But the
loop declines to park whenever it has anything at all to look at: a deferred
wake it has just drained, ready work homed on this CPU, a fired one-shot. Each
of those re-steps without running a task body, and folding them as work let a
CPU doing a couple of percent of it saturate its filter. The busiest CPU sets
the rate for the whole machine, so one such CPU pinned a Raspberry Pi to its
ceiling — reported as a core stuck at 1.4 GHz next to a scheduler truthfully
reporting 2% busy. The bracket now opens only for a dispatched task body.

### The launch boost

A program launch is the one case measurement cannot answer. It is
latency-critical before it has run an instruction, and most of what follows
waits on the volume the executable is read from — during which every CPU can
be idle and no utilisation accrues at all. So the kernel commits to the
maximum for one window at the point it starts an executable.

There is exactly one `spawn` syscall, so every launch goes through it: the
shell, the taskbar, the file manager, `appmgr` starting a bundle, and `devmgr`
autoloading a driver. The stamp sits behind *both* of the spawn path's
authority checks, so a caller the kernel refused cannot raise the machine's
clock by asking — and it grants nothing a caller did not already have, since a
principal that may start a program may equally pin the clock by running work
that genuinely deserves it.

### Cost when nothing is bound

One relaxed load per dispatch step, and nothing at all beyond that. Every port
but the Raspberry Pi has no frequency mechanism today, and pays exactly that.

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

The park is therefore bracketed on both sides: the baseline is discarded on
the way in and re-seeded from the live counters on the way out, so every
published figure spans running time only. **Both halves are needed, and the
discard is the load-bearing one.** A port calls its per-CPU sampling point
from the timer one-shot *and* from a device interrupt that requested a wake —
which is exactly the interrupt that ends an idle park. That sample runs in
interrupt context, before the dispatch loop regains control, and on a port
whose wait unmasks across the halt it runs before the wait even returns; so
re-seeding on resumption cannot come early enough on its own. Without the
discard, a mostly-idle core published `frequency × duty cycle` on every wake,
which is what made a machine pinned at its ceiling look as though three of its
four cores were dutifully clocking down.

The baseline is a pair of per-CPU atomics that means one instant, and the
sampler runs in interrupt context on the same CPU, so the re-seed clears the
reference slot before writing and republishes it last: a sample landing
mid-write reads "no prior sample" and re-seeds rather than dividing a fresh
core reading by a stale reference one.

A window too short to divide accurately publishes nothing rather than a noisy
figure — `0` and a clear `CPU_INFO_FLAG_FREQ_MEASURED` are the honest unknown
a reader falls back from, never a fabricated rate.
