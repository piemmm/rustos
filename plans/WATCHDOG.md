# CPU-lockup watchdog

Binding plan for TAIRiX's first-class CPU-lockup watchdog: detect, diagnose,
and best-effort recover from **soft** and **hard** CPU lockups, loudly enough
to explain *why*, without perturbing normal execution.

This plan is the **CPU/core** watchdog — a hardware liveness monitor for the
execution units themselves. It is distinct from, and complementary to, the
service-manager's **process liveness watchdog** (the `WatchdogSec` analogue in
`plans/NEW-SERVICEMANAGER.md` SVC-8): that one watches a *user-space service or
driver process* for a missed heartbeat and recovers it through the restart
policy (the "the driver, not the disk, is the problem" tie-in of
`plans/FIX-IO.md` IO5), whereas this one watches a *CPU* for a stalled
scheduler or a stopped interrupt sample. A wedged user-space driver that still
takes interrupts is not a CPU lockup and is the service watchdog's to catch; a
core that has stopped executing is this watchdog's. Where a wedged *kernel*
task also hard-locks a CPU, this watchdog is the last line.

A third detector sits beside both: the task-latency watchdog
(`plans/FIX-STALLTRACE.md`) watches an *interactive thread* against a frame
budget it declared, and reports the call that spent it. A stalled core is
this plan's; a service that stops answering is the service manager's; a
desktop that paused and then recovered is that one's.

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

Two further hard-lockup fields make the diagnosis honest and actionable. A
hard lockup's recorded pc/pstate/context are, by definition, the last sample
taken *before* the CPU went silent (~`stalled_ms` old) — they name the
innocent code the CPU last returned to, not the wedge — so the record carries
`sampled=pre_silence` to say so (a soft lockup's sample is live and carries no
such marker). Because that stale sample cannot name what is wedging the core
*now*, the observer reads the interrupt controller's globally-shared state
live and reports `stuck_irq=<id>` together with `stuck_state`: the lowest
shared line stuck **active** (handler in flight, never completed) in
preference to merely **pending**. The read is a new Arch-HAL query,
`WatchdogArch::stuck_interrupt`, returning a `StuckInterrupt {intid, active}`
(default `None`; a port with no globally-observable controller state reports
nothing rather than guessing).

Crucially, **only a line that could actually be delivered is ever reported**:
a masked line cannot be signalled to any CPU, so it can never be the cause of
a lockup, and the observer skips it rather than blaming an innocent line. The
scan reports a line only when it is **active** (which is only possible on a
line that was delivered) or **pending _and_ still enabled**; a masked-pending
line is passed over and the scan continues to the next candidate. So
`stuck_state=<active|pending>` distinguishes a live storm (`active`) from an
enabled line asserted but not yet taken (`pending`) — both genuine, deliverable
suspects. Only shared lines are observable this way — aarch64 GICv2 SPIs
(id ≥ 32); per-CPU banked SGIs/PPIs are not, since the observer cannot read
another CPU's banked state. `stuck_irq` is omitted when no deliverable line is
stuck (a pure in-kernel spin with IRQs masked, not a storm).

A banked line is therefore reported by the **victim**, not the observer:
`WatchdogArch::in_flight_interrupt` reads back what that CPU published into its
own per-CPU slot when it acknowledged an interrupt (recorded at the acknowledge,
cleared at the end-of-interrupt — two relaxed stores, off any lock, no change to
delivery or completion). The detail renders it as `in_flight` beside
`stuck_irq`, so a core that never completed an SGI or PPI names that interrupt
instead of the innocent pending SPI the shared scan falls through to.
`InFlightInterrupt` distinguishes "nothing in flight", "in flight, intid N", and
"no reading taken", so the record never implies an observation it did not make.

Reporting undeliverable lines was a real defect, not a cosmetic one: before
this, the fallback returned *any* pending line regardless of its enable bit,
so a masked, unowned, contained line (the recurring `stuck_irq=111` that was
neither the PCIe-MSI line nor any device in the pinned Pi 4 DTB — the lowest
latched-but-masked SPI on the QEMU `virt` / Pi 4 GICv2) was blamed for a hard
lockup it physically could not have caused. Skipping masked lines removes that
false lead at the source; the `enabled`/`masked` state no longer needs
recording because a reported line is always deliverable. The report attributes
the (now always deliverable) id **two ways** to say *whose* line it is. First
against the live kernel IRQ table: the observer resolves the stuck id through
`IrqTable::owner_of_line` (a read-only, owner-agnostic lookup) via the
arch-neutral `watchdog::StuckOwnerResolver` seam the boot path installs over
`&KernelState.irq`, rendering `stuck_owner=<task>` for a line a driver bound.
A line with no task binding is then offered to the port's kernel-internal
line-name resolver (`watchdog::KernelInternalLines`, installed from
`KernelArch::watchdog_line_names`), so a line the kernel services *itself*
through a chained/bespoke handler — which by construction has no `irq_wait`
binding — is **named** instead of dismissed. aarch64 names its two such lines
against the interrupt numbers discovered from the device tree (never board
constants): the BCM2711 PCIe root-complex MSI multiplexer's shared SPI is
`stuck_owner=pcie-msi` (the USB/PCIe MSI line a wedged CPU could not service —
this replaced a misleading bare `unbound` for exactly that line, the observed
`stuck_irq=153` boot report), and the console UART receive line is
`stuck_owner=console-uart`. Only a line neither a driver nor the kernel owns is
`stuck_owner=unbound` — a genuinely spurious/contained line, so the wedge is
elsewhere. Because the GIC scan only reports real SPIs (≤ `MAX_INTID`) and a
directly-bound SPI uses `line == INTID` in the table (MSI virtual lines live
*above* `MAX_INTID` and are never scanned), the lookup by id is exact.
`stuck_owner` is omitted when no owner resolver is installed, so a record never
claims an attribution it could not make. Mapping host-tested via the pure
`resolve_stuck_owner_with` (task-binding wins over a kernel name; a
kernel-internal line is named; a line neither owns is `unbound`); the render
(task/name/`unbound`/omitted) is host-tested against the recording sink.

## Debug diagnostics: a compile-time gate, address-safe, off the audit log (done)

The address-bearing developer aids below — the kernel-activity breadcrumb,
the pre-silence backtrace, and the sampled `pc`/`pstate` — are a **debug-only
compile-time facility**, gated behind the `watchdog-diagnostics` Cargo feature
(`kernel/core`, propagated to `kernel/arch/aarch64` and `kernel/tairix-kernel`).
`tools/xtask` (`kernel_diag_feature_args`) turns it on for the non-shippable
`debug` image only and leaves it fully compiled out of the shippable
`installer` image, matching the console UART routing the image profiles
already key on. This is the single selection point; the gate is an explicit
feature rather than `debug_assertions` so CI builds and tests both states
deterministically.

When the feature is **off** (any shippable image) the whole facility vanishes:
the per-CPU `kbc_*`/`wd_bt*` storage, the `KernelBreadcrumb` decode/render, the
`note_kernel_breadcrumb`/`note_watchdog_backtrace` recorders, and the aarch64
stack walk are `#[cfg]`-elided, so the ~12 breadcrumb call sites on the
syscall / scheduler-dispatch / user-fault hot paths inline to nothing — no
atomics, no branch, no strings. Verified against the built binary: the
`installer` (`--release`) kernel contains none of the diagnostic symbols or
tag strings; the `debug` kernel contains them all.

The report is **split into two records**. The always-on **summary**
(`CpuHardLockupDetected` / `CpuStallDetected` / the cleared/recovery events)
goes to the persistent hash-chained audit sink and carries only non-disclosing
state — `cpu`, `observer`, `stalled_ms`, `task` (an id, not an address),
`context` (`kernel`/`user`), `sampled=pre_silence`, and `stuck_irq`/
`stuck_state`/`stuck_owner`. It never carries a kernel address, so the
tamper-evident audit trail records *that* a lockup happened and roughly where
with zero disclosure. The debug-only **detail** (`CpuLockupDiagnostic`, id
4085) carries the address-bearing aids and goes to the *diagnostic* (log/UART)
sink — never the audit trail. Every kernel address in it is rendered
**image-base-relative** (`pc=+0x…`, `k_bt=+0x…,+0x…`), never the absolute
runtime address: the port registers the kernel image base
(`set_kernel_image_base(__kernel_start)`) and the render subtracts it
(`image_relative`), so a capture resolves against the debug kernel ELF with
`llvm-addr2line` without disclosing the (KASLR-relocatable) load base — the
`%pK`/`kptr_restrict` discipline. A pc/frame that does not resolve against the
registered base is omitted, never emitted raw (fail closed). `k_detail`
carries only a syscall number, a faulting VA, or a task id — never a syscall
argument value, buffer contents, key, credential, or capability token.

A third record states the *capability* the other two depend on. Whether the
port's non-maskable self-sample exists at all is a run-time verdict
(`probe_fiq_deliverability` on aarch64), and discarding it made an image whose
sampler never ran indistinguishable in the log from one where it worked — so a
reader could not tell a credible `sampled=pre_silence` record from a
meaningless one. The verdict is now reported once, on the boot CPU, as
`CpuWatchdogSelfSample` (id 4086, `self_sample=live` or
`unsupported`/`pending` with the honesty verdict's own reason text). Debug-only
like the detail, address-free and secret-free, latched so it is emitted exactly
once whichever of "port reports" / "sink installed" happens first.

On a board with no non-maskable interrupt channel (the Raspberry Pi 4's
GICv2 in the non-secure world), a CPU wedged with interrupts masked cannot
be reached by its own watchdog sample *or* a buddy's recovery IPI, so its
`wd_ctx_*` sample is `pre_silence` — the syscall-entry trampoline it last
returned to, never the wedge. The stuck region that produces this is
typically the data-abort/user-fault resolver (which runs IRQ-masked — only
the `svc` path re-enables IRQ) or an `IrqSafeSpinLock` spin.

To make such a wedge diagnosable, each CPU publishes a **breadcrumb** of the
in-kernel region it is entering, itself, as it runs
(`watchdog::note_kernel_breadcrumb` → `CpuState::{kbc_site,kbc_detail,
kbc_seq}`): the scheduler dispatch step (`init::run_dispatch_loop`), a
syscall body before any handler work (`KernelDispatchHook::dispatch`, detail
= syscall number), and every phase of the user-fault resolver
(`resolve_user_fault`, detail = faulting VA). Because it is written on the
way *into* the region, it stays fresh through a wedge, so the buddy's report
names the real region.

The `dispatch` region is partitioned finely, because the scheduler crate
(`kernel/sched/cfq`) cannot itself carry a breadcrumb (§17.4 layering —
it may not depend on `kernel/core`), so a wedge anywhere from CFQ's
`step`/`dispatch` through the task's context switch would otherwise all
report the one coarse `dispatch`. The kernel-core task-body shim
(`kthread::dispatch_step` and its scheduler body closure) stamps the finer
crumbs: CFQ's own pick/steal/prologue keeps `dispatch` (set in
`run_dispatch_loop` before the body closure runs); the shim hand-off
(`pending_upgrade` install and, for a user kthread, the `pre_resume`
address-space reactivation + resume/live publication) is `task_body`
(detail = dispatched task id); the arch context switch into the task and its
execution up to its first trap is `user_switch` (detail `0`, the task id
carried by the preceding `task_body` crumb); the dispatcher-side teardown
*after* `ContextSwitch::switch` returns — retiring the resume handle and,
for a user kthread, parking this CPU's translation off the task's user root
(a translation-register write) and checking the guard, all with device
interrupts still masked — is `switch_return` (detail `0`); and CFQ's
post-run accounting tail — also masked, inherited from the suspending task's
exception entry — is `dispatch_tail` (detail = task id). This is what tells a
genuine CFQ-internal scheduler wedge (`dispatch`) apart from one in the
address-space reactivation (`task_body`), the context switch / early task
(`user_switch`), the post-switch user-root translation park (`switch_return`
— a wedge coming *back* from a task, distinct from `user_switch` going
*into* it), or the masked accounting tail (`dispatch_tail`).

The full set: `k_site` (`dispatch`/`task_body`/`user_switch`/`switch_return`/
`dispatch_tail`/`syscall`/`fault_entry`/`fault_reclaim`/`fault_stack`/
`fault_ramzip`/`fault_anon`/`fault_file`/`fault_fatal`), `k_detail`
(syscall number, faulting VA, or dispatched task id per the sites above),
and `k_seq` (a per-CPU sequence so two successive reports separate a
*frozen* breadcrumb — stuck in exactly this region — from an advancing
one). In a diagnostics build the recorder is three relaxed stores plus one
release bump — the same order of cost as the progress/liveness heartbeats
already on those paths — and in a shippable build it and its call sites are
compiled out to nothing (above). No secret; `k_*` is omitted when no region
was recorded (a lone user task). Host-tested in both feature states: the site
round-trip and fail-closed decode (every variant), the publish→snapshot
triple, the out-of-range no-op, the summary carrying no kernel address, and
the detail rendering breadcrumbs image-relative.

## Pre-silence backtrace `k_bt` (done)

The single breadcrumb `k_site` names a *region*, and the stale `pc` names one
ambiguous address — neither pins the exact stuck code when a wedge sits deep
in a call nest (e.g. a `task_body`/`dispatch` sample whose real culprit is a
callee). So the report also carries `k_bt` — the frame-pointer-unwound
return-address chain (`k_bt=<pc0>,<pc1>,…`, innermost first, starting at the
interrupted PC) of the context the CPU's *last* cadence sample interrupted.
It names the whole call nest the CPU was in ~1 s before it went silent.

Each port unwinds its interrupted register frame on every sample; the aarch64
port threads the saved exception `frame` from `exceptions::handle_irq` →
`on_watchdog_interrupt` → the cadence callback, which calls
`watchdog::capture_sample_backtrace(frame, out)` — a bounded, fail-closed
AAPCS64 `x29`-chain walk over the interrupted context's stack (stops at a
null/misaligned frame pointer, a frame pointer not strictly above the
exception frame (the stack floor) or not strictly increasing, a zero return
address, **a return address outside the kernel's executable text**, a full
buffer, or a hard `MAX_BACKTRACE_WALK` step cap). The pure walk core is
`walk_frames`, host-tested with an injected fake stack; the arch wrapper
supplies the real map-probe, frame read, and text-range predicate. The
text-range check (`in_kernel_text` over the `__text_start`/`__text_end`
linker bounds) is what makes the chain *trustworthy*: it rejects a stack
**data** word misread as a caller, the defect that previously produced
chains interleaving unrelated `BTreeMap` instantiations that could not be
trusted to justify an SMP fix. Crucially it also proves each frame-pointer
link is mapped with an `AT S1E1R` read-translation check
(`el1_readable`, `PAR_EL1.F`) **before** dereferencing it, so it can never
fault inside the interrupt handler on *any* stack — not even one whose
interrupted context left a stale/garbage but aligned `x29` (early-boot
assembly, a task entry trampoline, a corrupt stack), the defect that
otherwise faulted during the eMMC boot read and wedged the mount. `PAR_EL1`
is saved/restored around the probe so an in-flight translation is not
clobbered. It never loops. It is captured **only for a kernel-context sample**
(`spsr_in_kernel`) — a hard lockup is always an in-kernel wedge, and confining
the walk to the kernel stack keeps it off an untrusted user stack. The frames
are stored per-CPU by `watchdog::note_watchdog_backtrace` (length published
last, release; capped at `WATCHDOG_BACKTRACE_MAX`) and rendered by the buddy
observer; `k_bt` is omitted when no frames were captured (fail closed — never
a fabricated stack) and, in the detail render, each frame is emitted
image-relative (`+0x…`) via `image_relative`, a frame that does not resolve
against the registered image base being skipped rather than disclosed raw.
Host-tested (feature on): the publish→snapshot round-trip, the depth-cap +
empty-clear + out-of-range no-op, and the image-relative render (present with
a captured stack, omitted without, and fail-closed when a frame does not
rebase).

## Stuck-lock site `k_lock` (done)

`k_site` names a region and `k_bt` names the call nest, but on a GICv2 hard
lockup the wedge is, by construction, a CPU spinning or holding a lock with
interrupts masked (only an `IrqSafeSpinLock` masks IRQ) — and the maskable
liveness sample cannot observe *inside* that section. So the detail also
carries `k_lock` — the exact spinlock the wedged core is on, named by the
acquiring call's **source `file:line`** (not a runtime address).

Recording lives in `lib/sync`, behind its own `lock-diagnostics` feature that
`kernel/core`'s `watchdog-diagnostics` turns on (so it tracks the same debug
image, the one selection point); the same switch turns on
`tairix-kalloc/lock-diagnostics`, because the global kernel heap lock is an
`IrqSafeSpinLock` over kalloc's installed mask hooks and is the one lock every
subsystem descends into — a core wedged inside it would otherwise be reported
against whatever outer lock it happened to hold. The spinning family is
instrumented at one point: `SpinLock::{lock,try_lock}` (which
`IrqSafeSpinLock` wraps, so both are covered) are `#[track_caller]` in a
diagnostics build and report their
lifecycle — `Acquiring`/`Acquired`/`TryAcquired`/`Released` with the caller's
`Location` — to an installed thin-fn observer (`lockwatch::note`). A shippable
build compiles the `track_caller` shim, the notes, and the module out entirely,
so a production lock is a bare compare-and-swap (verified: the `--release`
kernel has no `k_lock`/`lockwatch` symbol or string). `RwLock`/`McsLock` are
deliberately *not* instrumented: they do not mask interrupts, so a wedge in one
is a soft lockup the live sample's `pc`/`k_bt` already localise — the facility
targets the IRQ-masking family that produces the *hard* lockup.

`kernel/core` installs the observer once on the boot CPU
(`install_lock_diagnostics`); the observer resolves the running core's dense id
through a lock-free banked-register read (`smp::current_cpu_index`, so it never
recurses into a lock) and records into a per-CPU bounded lock-site stack
(`CpuState::{lock_sites,lock_depth,lock_acquiring}`, `LOCK_STACK_MAX = 8`):
`Acquiring` pushes marked *acquiring*, `Acquired` promotes the top to *held*,
`TryAcquired` pushes *held*, `Released` pops. `lock_acquiring` is one bit
**per entry**, not a single top-of-stack flag: a core spinning for a lock with
interrupts enabled takes and releases nested locks inside that spin (every
interrupt handler does), and a shared flag reported the still-spinning outer
entry as `held` from the first nested release onwards — turning a contended
waiter into a phantom wedge and pointing a lockup investigation at the wrong
core. Nesting deeper than the cap still
balances on release (depth counts true nesting) and simply stops recording the
excess (fail-safe — no growth, no fault). The buddy's report renders the
innermost entry as `k_lock=<file>`, `k_lock_line=<n>`, and `k_lock_state`
(`acquiring` = still spinning, contended/deadlocked; `held` = wedged inside the
critical section), plus `k_lock_owner=<cpu>` — **which core holds the lock
against a spinner**. Each `SpinLock` stamps its owning CPU on acquire and
clears it before releasing the lock word; a spinner republishes what it reads
there each failed CAS round, so the report pairs a wedged waiter with its
holder instead of leaving that to be inferred. It also settles the question a
lone `held` record cannot: when the spinner names the core whose own record
claims `held`, that record is live rather than a stale leftover. The CPU-id
resolver is a single seam in `lib/sync` (`lockwatch::install_cpu_id`), read
both by the locks that stamp an owner and by the kernel observer that picks
the per-CPU slot, so the two can never name different cores. Hold *age* is not
stamped separately: the report already carries the wedged CPU's `stalled_ms`,
and reading a clock on every acquire would cost the kernel's hottest path to
restate it. Because the value is a source string, no runtime address —
and so no KASLR base — is disclosed even though `lock_site` is a pointer
internally; `k_lock` is omitted when the core holds no recorded lock.
Host-tested in both feature states: the lockwatch no-op-without-observer and
stable event discriminants (`lib/sync`), and the stack tracking the innermost
lock / surviving past-cap nesting / naming the stuck lock with its state /
disclosing no runtime address (`kernel/core`).

This is the developer aid that turns the remaining "bare hard lockup on a
secondary CPU, `k_site=task_body`, `sampled=pre_silence`" report (a
`stress --cpu N` wedge in the task-shim / address-space-activation path) into a
report that names the precise spinlock the core is stuck on, so the underlying
SMP lock-ordering defect can be fixed with evidence rather than guessed.

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

The watchdog closes this, from **both** per-CPU interrupt paths.

* The cadence: when `on_watchdog_tick` samples an `Active` CPU running a
  **user** task (`!in_kernel`) whose progress heartbeat is stale past
  `DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS` (1 s — well below the 10 s soft/hard
  thresholds), it calls `preempt::request_forced_yield`. Kernel code is never
  force-yielded on this path (the kernel is non-preemptible), and the reading
  is sound because the very sample calling in produced it.
* The maskable timer tick: `check_stall` requests the same yield whenever
  `progress_overdue` holds. This is the path that matters on a wedged core,
  because the guard above rides a cadence that has *stopped* there — and its
  `in_kernel` input is refreshed only by that cadence, so it rots at whatever
  the last sample read and suppresses the guard exactly when it is needed.
  `progress_overdue` therefore takes no context argument at all;
  `monopolises_cpu` is `!in_kernel && progress_overdue(…)`, one definition of
  the predicate. The request is read **unlatched**, not behind the
  once-per-episode soft-lockup latch, so a core that stays out of the dispatch
  loop is pushed back at every tick rather than once.

Either preemption point honours it — the port's return-to-user callback
(`preempt_current`) and the in-kernel boundary (`yield_if_owed`) share one
`honour_latches` decision — suspending the task back to the dispatcher
**unconditionally** (the forced-yield latch is *not* competitor-gated, unlike
`note_preempt_tick`). Both are needed: a task wedged in EL1 never reaches a
return-to-user point. The dispatch loop then runs one iteration — re-stamping
progress, draining the deferred wakes and the console transmit — before
re-dispatching the task. Crucially this arms **no new timer**: it rides an
interrupt already firing on the CPU, so the tickless invariant is preserved.
Latch: `CpuState::force_yield`, cleared by the dispatcher's
`clear_preempt_pending` so the incoming task earns a fresh guard window.
Host-tested (the `progress_overdue`/`monopolises_cpu` predicates, including
that a rotted `in_kernel` reading does not suppress the tick guard;
`on_watchdog_tick` sets the latch for a monopolising user CPU only;
`check_stall_at` sets it for an overdue CPU; both preemption points honour a
forced yield with no competitor and no latched tick).

## The in-kernel boundary: a burst of never-waiting operations (done)

The guard above covers a monopolising *user* task. In-kernel work needs a
different mechanism, because it cannot be suspended at an arbitrary
instruction: an in-kernel body that issues one bounded operation after another
— a service kthread draining its request queue, a filesystem read walking a
large file span by span — holds its CPU for the whole burst whenever the device
is fast enough that no operation has to wait. `virtio_blk::submit_and_wait`
polls the completion ring *before* waiting, so on an emulated queue (or an NVMe
namespace whose completion is already in the ring) the park that would have
returned control to the dispatcher never happens, and the dispatch loop's
housekeeping and heartbeats stop for the burst's duration. That is what the
soft-lockup detector reports as `context=kernel` with no spin anywhere: the
report is honest and the defect is the missing boundary, not the sample.

`preempt::yield_if_owed` is that boundary. In-kernel code calls it *between*
units of work; it consumes the same two latches and applies the same decision
the return-to-user point applies (one shared `honour_latches`), so a burst
gives the CPU up at most one unit after its quantum expires — or immediately on
a forced yield, which a body wedged in EL1 can be reached by nowhere else — and
costs two atomic swaps when nothing is owed. Placement is the caller's
obligation — only where no spin lock is held; a point that can
already park on a slow device is sound by construction. Call sites: the storage
funnel every in-kernel device operation passes through
(`SharedBlockHandle::with_device`, before the shared device's sleeping lock is
taken) and the in-kernel `/System` store server's between-requests boundary.
Host-tested (free when no tick is latched; consumed exactly once at the
boundary; per-CPU; fails closed for an unknown CPU).

The diagnostic that made this defect read as a *user*-space problem is fixed
with it: the dispatcher stamped one `k_site=user_switch` crumb for both a user
task's EL0 run and a kernel kthread's whole body run, so a kernel-context stall
pointed a reader at a misbehaving program. A kernel kthread body now stamps
`k_site=kernel_body`.

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
neutral `WatchdogSample`, and for a kernel-context sample captures the
pre-silence `k_bt` backtrace from the forwarded `frame`) and arms the cadence
on every online CPU. `handle_irq` forwards the saved register `frame` to
`on_watchdog_interrupt` (the callback signature carries it) so the sample can
unwind the interrupted context.

`WatchdogArch::stuck_interrupt` (aarch64) is `gic::stuck_spi()`: the observer
scans the distributor's `GICD_ISACTIVER` then `GICD_ISPENDR` over the SPI
range for the lowest **deliverable** stuck line, returning its id plus
`active` (which bank matched). An active line is reported unconditionally
(being active proves it was delivered); a pending line is reported only when
its `GICD_ISENABLER` bit is set, and a masked-pending line is skipped so the
scan continues to the next candidate rather than blaming a line that cannot
reach a CPU. A pure read of globally-shared state, safe from any CPU, so a
core hard-wedged on a device SPI is named in the report even though its own
sample is stale, and the `active` flag tells a live storm apart from an
enabled line asserted but not yet taken. The scan logic is host-tested
against the mock distributor (including that a masked-pending line is skipped
in favour of a higher enabled one); the live MMIO read is metal-only (`None`
off metal).

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

## Debug-only FIQ masked-section self-sample (done, aarch64; metal delivery pending)

The buddy detector above is the **shippable, complete** design and is
unchanged. For the **debug** image only (`watchdog-diagnostics`), a
non-maskable **FIQ self-sample** is added beside it to observe a core wedged
in a `DAIF.I`-masked section that the maskable IRQ cadence cannot see (the D13
`stress --cpu N` class: an untracked IRQ-masked busy-spin inside a task body,
`plans/OPEN-DEFECTS.md` D13). It routes the `WATCHDOG_PPI` cadence to GIC
**Group 0** so it is signalled as FIQ, adds a FIQ arm to
`tairix_aarch64_trap_handler` that runs the same `on_watchdog_interrupt`
self-sample + `walk_frames` backtrace, and enables it only through an
**empirical, fail-closed delivery probe** — if an FIQ is not actually taken in
a deliberately `DAIF.I`-masked test window (metal armstub routing, GIC group
semantics), it reverts to the IRQ cadence + buddy detector with no broken
channel (fail closed).

**B1 — `DAIF.F`-clear execution discipline — DONE (QEMU-validated), now
runtime-gated on the probe (fail-closed).** Exception entry masks `DAIF.F` in
hardware, `enable_irq` clears I-only, and `IrqSafeSpinLock`'s `DaifIrqControl`
masks I+F, so a wedge lives with FIQ masked and a Group-0 cadence could never
reach it. In a `watchdog-diagnostics` build the port clears `DAIF.F` where the
wedge lives — **but only when the boot probe (`fiq_cadence_enabled()`) proved a
non-maskable FIQ is genuinely deliverable to the non-secure kernel.** The lock
critical-section base mask is unconditionally I+F (the shippable discipline);
`DaifIrqControl::disable` re-clears F *inside* the section,
`exceptions::enable_fiq_delivery()` clears F on the `svc`/fault sync-handler
entry, and the EL0 entry `SPSR` leaves F clear (B5), **only** under that
runtime predicate. `halt_current_cpu` (`#0xf`) stays F-masked, and the FIQ trap
arm never re-clears F (both genuinely nested-FIQ-unsafe).

*Defect fixed here (Pi 4 boot lockup):* the discipline was originally gated on
the **compile-time** feature alone (`daif::critical_section_mask(cfg!(...))`,
I-only in debug; an unconditional `enable_fiq_delivery()` on every sync entry),
so on a two-Security-state GIC-400 — where the probe returns `Unsupported`
because Group 0 belongs to the secure world — the debug kernel still ran with
`DAIF.F` clear everywhere. That is fail-**open**: it exposes the non-secure
kernel to secure-world Group-0 FIQs it cannot service, with no self-sample
benefit (none is delivered there). Both sites now consult the runtime probe and
fail closed (keep FIQ masked, exactly as a shippable build) when it is not
`Supported`; the obsolete compile-time `critical_section_mask` helper is
removed. The leading suspect for the near-every-boot Pi 4 masked-section wedge.

**B2 — Group-0 routing + FIQ dispatcher + empirical probe — DONE (host-tested,
both target builds clean).** The mechanism and its fail-closed capability:

- **GIC Group-0 register layer (`gic.rs`).** `GICD_IGROUPR` (base 0x080,
  banked `IGROUPR0` for the SGIs/PPIs), `GICC_CTLR_ENABLE_GRP0`/
  `GICC_CTLR_ENABLE_GRP1`/`GICC_CTLR_ACKCTL`/`GICC_CTLR_FIQEN` (single-
  Security-state bits 0/1/2/3), the `igroupr_offset` helper, and
  `Gicv2::{set_group0,set_group1,route_selfsample_fiq,read/write_gicc_ctlr,
  read/write_gicd_ctlr}` with feature-gated freestanding wrappers. Host-tested
  against the mock distributor.
- **Single-line FIQ routing (`Gicv2::route_selfsample_fiq`).** `GICC_CTLR.FIQEn`
  is a *global* switch — it routes **every** Group-0 interrupt to the FIQ
  signal, and this board resets every interrupt to Group 0. Enabling `FIQEn`
  alone therefore delivers the preemption-timer PPI (INTID 30) and device SPIs
  as FIQs the `handle_fiq` arm does not service, so they re-fire unbounded — a
  timer-PPI FIQ storm that pegs every core and wedges the boot. To FIQ a single
  line, `route_selfsample_fiq` moves **every interrupt but the cadence PPI**
  into Group 1 (bounded by `GICD_TYPER.ITLinesNumber`), leaves the cadence PPI
  in Group 0, and enables both groups + `AckCtl` + `FIQEn`; `AckCtl` keeps the
  Group-1 IRQs acknowledgeable through the one `GICC_IAR`/`GICC_EOIR` path the
  IRQ handler uses (without it a Group-1 `GICC_IAR` read returns the reserved id
  1022, which itself storms). The shippable `init` stays single-group
  (everything an ordinary IRQ); only the debug watchdog reaches this split.
- **FIQ dispatcher arm (`exceptions.rs`).** `is_fiq(kind)` (vector kinds
  2/6/10, disjoint from IRQ/sync) and a `watchdog-diagnostics`-gated
  `handle_fiq` arm that acknowledges Group 0 through the same `GICC_IAR`/
  `GICC_EOIR` full-cookie handshake, records the delivery (`note_fiq_taken`),
  and for the cadence PPI runs `on_watchdog_interrupt`. It is purely
  observational: it never clears `DAIF.F` (nested FIQ unsafe) and never
  preempts.
- **Fail-closed capability (`watchdog.rs`).** `probe_fiq_deliverability`
  applies the single-line FIQ split (`gic::route_selfsample_fiq`), arms a ~1 ms
  one-shot, masks `DAIF.I` (leaving `DAIF.F` clear), and waits a bounded
  interval (a `CNTPCT_EL0` deadline backed by a hard iteration cap) for an FIQ
  to actually be taken. On success it stays routed; on failure it restores the
  perturbed group/enable registers *verbatim* from values saved before the
  probe (so the ordinary-IRQ enable bit is preserved on any GIC Security
  configuration) and reports `FeatureSupport::Unsupported`, leaving the buddy
  detector in place. The pure decision `fiq_support_from_probe` is
  always-compiled and host-tested; the boot path calls the probe once on the
  boot CPU (`arm_preemption`), and `init_local_watchdog` applies the
  `route_selfsample_fiq` split on each online CPU only when the capability is
  Supported.

The probe is `Supported` on a **single-Security-state** GIC and `Unsupported`
on a **two-Security-state** GIC. Measured under QEMU: the `virt` default
(`secure=off`, the board and test-runner configuration) is single-Security-
state, so Group 0 / FIQ reaches non-secure EL1 and the probe returns
`Supported` — the debug image self-samples via FIQ there (this corrects an
earlier assumption that QEMU `virt` was `Unsupported`; it was never measured).
A two-Security-state GIC (QEMU `virt,secure=on`, or a real Pi 4 GIC-400 whose
Group 0 belongs to the secure world) returns `Unsupported`, and the debug
image falls back to the complete cross-CPU buddy detector with no broken
channel (fail closed).

**B3 — QEMU masked-section vertical — DONE.** `tests/integration/
fiq_selfsample_qemu_aarch64` (enrolled in `cargo xtask test --qemu`) boots the
`virt` board, runs `probe_fiq_deliverability` (asserts `Supported`), installs
the production cadence callback, arms a short Group-0 (FIQ) cadence,
deliberately masks `DAIF.I`, and busy-spins in an `#[inline(never)]` marker.
The FIQ fires *through* the `DAIF.I` mask and the self-sample captures a
**live** snapshot: the vertical asserts the interrupted `SPSR_EL1.I` was
masked, the sample was kernel-context, and the sampled PC *and*
`capture_sample_backtrace` top land inside the marker — the proof the
masked-section sampler names the section it is stuck in (`sampled=live`, not
the stale `pre_silence` a buddy sees).

**B4 — the sampler's own re-entrancy cost — DONE.** Delivering the cadence as
a genuine FIQ makes it the one asynchronous exception that *can* interrupt a
`DAIF.I`-masked kernel section, so every kernel window that is unsafe against
a nested exception became reachable for the first time. One such window was
live: the aarch64 trap trampoline's exception-return epilogue programmed the
single-copy `ELR_EL1`/`SPSR_EL1` pair and then ran ~40 further instructions
(the `SP_EL0`, FPCR/FPSR, `q0`–`q31` and GP restores) before its `eret`. An FIQ
taken in that window overwrote both registers, and the sampler's own return
restored *its* saved pair, so the interrupted `eret` re-entered the epilogue at
EL1 with the frame already popped — climbing `sp` one 816-byte frame per turn
off the kernel stack until the loads faulted, then faulting recursively with
`DAIF` masked: a silent, unrecoverable wedge with no panic output, reported
only as `CPU_HARD_LOCKUP_DETECTED` with a `pre_silence` PC inside that
epilogue. Both of the port's `eret` sequences (the trampoline epilogue and the
`userentry` EL0 entry) now mask every asynchronous exception before they
program the return state; `eret` reloads PSTATE from `SPSR_EL1`, so the mask
never reaches the resumed context. Pinned by
`kernel/arch/aarch64::exceptions::eret_tests`.

The FIQ sampler is therefore blind to the ~45-instruction restore tail of an
exception return, by design: it is straight-line, lock-free and MMIO-free, so
it cannot wedge, and no diagnostic value is lost — the handler body, the span
worth observing, stays fully sampled. A `pre_silence` PC *inside* that tail is
now a signature worth reading as "the resume state was destroyed", not as "the
CPU was innocently returning".

**B5 — the cadence must reach a core running *user* code — DONE.** A core is
just as unsampleable while it runs a CPU-bound EL0 task as inside a masked
kernel section, and for the same reason: exception entry to EL0 masks `DAIF.F`
in hardware, so an EL0 `SPSR` that keeps F set makes the FIQ-routed cadence
undeliverable for as long as the task stays in user mode. B1 originally kept the
EL0 `SPSR` F-masked as "nested-FIQ-unsafe", over-generalising from the two
windows where that is real (`halt_current_cpu`, the FIQ arm itself): EL0 is not
inside an FIQ handler, the FIQ vector runs on `SP_EL1` with F re-masked by the
PE, and both `eret` sequences already mask every asynchronous exception before
programming `ELR_EL1`/`SPSR_EL1` (B4). Interrupted user code holds no kernel
lock, so an EL0 sample is strictly safer than a kernel-section one.

The consequence was a **false hard lockup on a healthy core**, and the reported
shape is indistinguishable from a real wedge: the last liveness stamp is
whatever kernel entry was sampled before the task settled in user mode, it then
rots for the whole run, and a buddy reports `id=4082 … context=kernel
sampled=pre_silence` with a stale `k_site`/`k_bt`/`k_lock` from that unrelated
entry — while the core is demonstrably alive and taking thousands of IRQs. The
soft detector mis-fires the same way (`classify` only reports a stall for a CPU
*last seen in the kernel*, and a rotting kernel-context flag satisfies that),
and `monopolises_cpu` — which fires only on a *user*-context sample — could
never trigger at all, so the guard against a task withholding the CPU was dead
code on exactly the configuration that has the sampler.

The trigger is a **lone** runnable user task, which is what made it look
intermittent: being tickless, the scheduler disarms the preemption one-shot for a
sole runnable task, so its core takes no kernel entry at all and the pending
cadence FIQ has no window to land in. Several runnable tasks on one core keep
letting it through at each preemption entry — measured, `stress --cpu 20` is
clean even *before* the fix while `stress --cpu 1` reports 4/4 — so it is the
idle-ish desktop (one busy app, everything else parked) that shows it.

Fixed by deciding the EL0 entry `SPSR` in one place from the boot probe:
`userentry::el0_spsr(fiq_cadence)` clears `DAIF.F` when
`watchdog::fiq_cadence_enabled()` is true and is otherwise the unchanged
F-masked value, so a shippable image and a board whose probe answered
`Unsupported` behave exactly as before (fail closed). Every later return to EL0
restores the `SPSR` this entry established from the frame `vectors.s` saved, so
there is one definition. Measured on the `virt` debug image (4 vCPU, `stress
--cpu 1`): a spinner took **1** sample against its idle siblings' ~46 before the
fix while thousands of IRQs still reached it, and the false
`4080`/`4082`/`4084`/`4085` set reproduced 4/4; after the fix the same workload is
clean 10/10 and a spinner is sampled in EL0 at the full cadence. Regression
cover: `userentry`'s host tests pin the two `SPSR` values and that only the F bit
differs, and the B3 vertical additionally asserts on the real board that a
`Supported` probe leaves the EL0 entry state F-clear. Recorded as
`plans/OPEN-DEFECTS.md` D29.

Metal delivery on a real Pi 4B stays a boot-time hardware capability
(`plans/FIX-HARDWARE-FEATURES.md`) — there the probe returns `Unsupported` and
the buddy detector runs — and is not claimed until a Pi 4B confirms it.

## CoreSight external-debug (EDPCSR) cross-core PC sample (done, metal enablement pending)

The FIQ self-sample above needs Group 0 / FIQ to reach non-secure EL1, which a
**two-Security-state GIC** (the real Pi 4 GIC-400) denies — there the probe is
`Unsupported` and the masked-section wedge shows only a stale
`sampled=pre_silence` PC. The observer for *that* case is the ARMv8
**external-debug PC Sample Register** (`EDPCSR`, DDI 0487 H9): one core reads
another's sampled PC over the memory-mapped debug interface **without halting
it** and over a channel `DAIF` cannot mask.

Surface (arch-neutral): `WatchdogArch::remote_pc_sample(target) ->
RemotePcSample` (`Sampled{pc,context}` / `Unavailable(reason)` /
`Unsupported(reason)`), default `Unsupported` + conformance, alongside
`stuck_interrupt` (device "why") — this is the *code* "why". The hard-lockup
`scan` reads it and the debug detail renders a fresh `live_pc=+0x…`
(image-relative) + `live_ctx` beside — never replacing — the stale `pc`.

aarch64 (`coresight.rs`): host-tested pure `sample_from` — EDLAR unlock →
EDDEVID capability → EDPRSR validity → EDPCSR **capture-first** (low word
latches high/context) → assemble — over a `DebugMmio` seam; a scale-sized
set-once per-cpu debug-base registry; the freestanding `VolatileDebugMmio`;
`Watchdog::remote_pc_sample` delegates. Discovery (`fdt::debug_component_bases`,
host-tested): the Linux `arm,coresight-cpu-debug` binding (translated `reg` +
`cpu`-phandle → dense id), installed at boot **only** for a base whose gigapage
is already Device-mapped, so a read on the lockup path can never fault; a
component elsewhere, or a tree with no debug nodes (QEMU `virt`, the stock Pi 4
firmware DTB), installs nothing → `Unsupported`, buddy detector unchanged.

Metal enablement: the live `EDPCSR` read is confirmable only on hardware (QEMU
models no EDPCSR), and the stock Pi 4 firmware DTB carries no
`arm,coresight-cpu-debug` nodes, so **firing it on a Pi 4 needs those nodes
supplied in the DTB (or an overlay)** — a provisioning step, not a code change.
Until then the code path is exercised by the fail-closed (`Unsupported`)
vertical, mirroring the FIQ-probe precedent.

## Other architectures (staged)

x86_64 / riscv64 / wasm32 keep the soft detector (`check_stall` on their tick
paths) and inherit hard-lockup detection when each wires its own
`on_watchdog_tick` cadence + `WatchdogArch` (NMI/LAPIC-deadline on x86_64, the
higher-privilege timer on riscv64). The architecture-neutral core needs no
change — the HAL surface is delivery-agnostic.
