# Kernel multitasking and the kthread runtime

This page is the source of truth for how a TAIRiX scheduler task becomes a
*resumable kernel thread* — a task that owns a kernel stack and can be
parked at an arbitrary point and resumed exactly there. It records the
design decisions behind the spawn work staged in
[`plans/SPAWN.md`](../../../plans/SPAWN.md) (SP0). Read it together with the
rustdoc on `kernel/core`'s `kthread` module, the [scheduler
page](./scheduler.md), and [`AGENTS.md`](../../../AGENTS.md) §17.1
(pluggable scheduler) and §17.2 (the Arch HAL).

## Why a kthread runtime exists

A `kernel/sched` task is admitted with a body closure
`FnMut(&mut TaskContext) -> TaskAction` that the scheduler invokes once per
dispatch step (`AGENTS.md` §17.1). That contract has no notion of a task
that *suspends mid-execution and later resumes*: the body runs to a
`TaskAction` and returns every time.

Real multitasking — and, ultimately, two EL0 user tasks timesharing a CPU
— needs a task that can be suspended at any point and resumed there. The
kthread runtime in `kernel/core` provides exactly that, **layered on top
of** the closure contract rather than changing it. The scheduler policy
crates (`kernel/sched/*`) are untouched, so the §17.1 modularity contract
and the frozen-once-shipped scheduler interface (§2.4) both stay intact.

## The kthread-shim model (decision)

The body the scheduler sees is a thin **shim** owned by `kernel/core`. The
task's real work runs as a stackful coroutine on its own kernel stack,
driven through the Arch HAL context-switch slice
(`tairix_arch_api::ContextSwitch`, §17.2) — the same `prepare` / `switch`
primitive every bare-metal port already implements and conformance-tests.

Each kthread owns a heap-allocated control block holding two
`TaskContext` save areas — the task's and the dispatcher's — its requested
`TaskAction`, a run-state, the work closure, and its kernel stack. On each
dispatch step the shim:

1. on the **first** step, calls `ContextSwitch::prepare` to seed the
   task's first kernel-stack frame so it lands in the runtime trampoline,
   then falls through;
2. calls `ContextSwitch::switch` *into* the task, saving the dispatcher's
   context in the control block's `dispatch_ctx`;
3. the task runs until it cooperatively suspends — a `Yielder::yield_now`
   / `Yielder::park` switches back to `dispatch_ctx`, or the work returns
   and the trampoline switches back with `TaskAction::Exit`;
4. control returns to the shim right after the step-2 switch; it reads the
   task's requested `TaskAction` and returns it to the scheduler.

This is why `ContextSwitch::switch` — until now exercised only by the W7
`sched_drive_*` verticals — becomes a *production* scheduling path for the
first time.

## Per-task kernel-stack ownership

Every kthread owns exactly one kernel stack, abstracted by the
`KernelStack` trait (`top()` returns the exclusive, 16-byte-aligned upper
bound the first frame is seeded from). The production source is `BoxStack`,
a heap box; tests and future ports may supply guard-paged or slab-backed
stacks (`AGENTS.md` §4).

The stack lives inside the control block, which the shim closure owns.
When the task exits, the scheduler drops the body, which drops the control
block, which drops the stack — reclaiming it. Because an exited task is
terminal (the shim returns `TaskAction::Exit` and the scheduler never
re-invokes the body), nothing ever executes on the stack after it is
freed, so there is no use-after-free. A slab-backed stack makes that
guarantee enforceable: freeing the stack rotates the slot tag, so a stale
handle into a reclaimed kernel stack is rejected as a tag mismatch
(`AGENTS.md` §19.10).

## Aliasing discipline across the switch

The shim (dispatcher side) and the trampoline / `Yielder` (task side) both
reach the same control block, but **never concurrently**: a cooperative
context switch hands the single CPU from one side to the other, so they are
temporally exclusive. To stay sound under the aliasing model, neither side
holds a reference to the control block *across* a switch — every access is
through a raw pointer whose derived reference goes out of scope before the
switch, and the `ContextSwitch` handle is copied out (`C: Copy`) rather
than borrowed across the boundary.

## Fail-closed behaviour

A stack that cannot seed a first frame (`ContextSwitch::prepare` returns an
error) fails the task closed: the shim marks it terminal and returns
`TaskAction::Exit` rather than switching into an unrunnable context
(`AGENTS.md` §2.9 / §5.4). There is no `unwrap`/`expect`/`panic!` on the
spawn or switch path.

## Bringing EL0 into the model

A user task is an SP1 kthread whose work diverges into EL0 via
`EnterUser::enter_user`. The key realisation is that an EL0 `svc` traps
onto *that task's own* EL1 kernel stack — which is its kthread stack — so
**the kthread's kernel stack already is the EL0 save area**: there is no
separate per-task EL0 frame to copy. The trap path suspends the task with
the same `ContextSwitch::switch` the cooperative `Yielder` uses, and the
task resumes exactly there (re-entering EL0) on its next dispatch. No new
HAL trait is needed (§17.2).

### Core reschedule machinery (SP2a — implemented)

The arch-neutral half lives in `kernel/core` and is host-proven:

* **`DispatchOutcome::Reschedule { result, action, cpu }`** — the dispatch
  callback's signal that the syscall rescheduled its caller, carrying a
  self-contained `RescheduleAction` (`Yield` / `Park` / `Exit`) mapped onto
  the scheduler's `TaskAction` at one boundary (§2.2).
* **`reschedule_current(cpu, action) -> bool`** — the entry point the arch
  trap path *and* in-kernel blocking primitives (`SleepLock` contention, a
  block-device completion wait) call. It looks up a per-CPU *resume
  handle*, lifts it out from under its lock *before* the suspending
  `switch` (so no lock is held across the hand-off), and suspends the
  running kthread back to the dispatcher. It **fails closed** (returns
  `false`) when no kthread is published for the CPU — the pre-dispatch
  boot flow or a host test — so the caller falls back (an ordinary syscall
  return, or a bounded CPU park) rather than an unsound switch (§2.9,
  §5.4).
* **Discovered-sized per-CPU state + `pre_resume` hook.** Scheduler
  initialization fallibly allocates one `cpu_state` slot per validated
  discovered CPU; there is no compile-time CPU ceiling. Each slot owns the
  resume handle, current live address-space pointer, preemption latch, and
  preemption counter. A set-once cell publishes the owned slice as one
  immutable object, giving interrupt paths O(1) lookup without allocation or
  a global lock on the hot path. `dispatch_step`
  publishes a resume handle for *every* kthread it is about to switch
  into — user and kernel alike — and clears it the instant the task
  switches back, so a handle is valid exactly while that CPU runs the task
  (in EL0, in one of its syscall traps, or in a kernel kthread's body). A
  user task's handle carries the *syscall* suspend thunk (which brackets
  the suspend with the port's cooperative-park convention hook); a kernel
  kthread's carries the *body* thunk (no bracket — a kthread never entered
  the port's privilege-entry convention). Kernel kthreads being
  suspendable is load-bearing: a kthread contending on a `SleepLock` whose
  holder is parked across a device wait must park too — an in-kernel spin
  would monopolise the CPU and starve the dispatch loop. A `pre_resume`
  hook (carried by `spawn_user_kthread`) runs on the dispatcher side
  before every switch-in; its presence is what marks a task as a *user*
  kthread and what reactivates the task's address space so it `eret`s
  under the correct translation regime (§4). The dispatcher passes the
  hook the task's **own kernel-stack top** (`KernelStack::top`): a port
  whose syscall entry does not implicitly resume on the running task's
  kernel stack uses it to repoint its per-CPU entry stack (see x86_64,
  below); a port that does (aarch64) ignores it.

### EL0 wiring (SP2b — implemented)

* **Per-arch address-space reactivation.** The `pre_resume` hook captures
  only the user page-table root (a `u64`, keeping the runtime `Send`) and
  calls a small per-arch `activate_user_root` primitive (on aarch64:
  reprogram `TTBR0_EL1` + `tlbi`/`isb`, MMU already on; on x86_64: reload
  `CR3`), so each task stays hardware-isolated (§4).
* **Per-arch syscall-entry stack (x86_64, `plans/PI.md` X1).** Where an EL1
  trap implicitly reuses the running kthread's `SP_EL1` (aarch64), the
  x86_64 `syscall` stub instead loads its kernel stack from a per-CPU slot,
  so the x86_64 `pre_resume` hook additionally feeds the kernel-stack top
  the dispatcher hands it to `syscall_entry::set_kernel_rsp0`, repointing
  that slot at *this* task's own kernel stack before the switch-in — without
  it two tasks' syscall handlers would collide on one stack (a correctness
  *and* isolation defect, §4). See [the x86_64 platform
  page](../platform/x86_64.md) ("Resumable ring-3 user kthread").
* **The producer.** `KernelDispatchHook` maps the `yield` / `exit`
  syscalls to `DispatchOutcome::Reschedule`, and the arch trap callback
  acts on it by calling `reschedule_current` instead of returning to user
  mode. The kthread `TaskAction` returned by `dispatch_step` then becomes
  the single authority for the scheduler re-enqueue/reap (the handlers stop
  driving it directly). A non-rescheduling syscall returns to the same task
  exactly as today.

### Two EL0 tasks timeshare a CPU (SP2c — proven)

The model is proven end to end on the aarch64 `virt` board by
`tests/integration/spawn_el0_timeshare_qemu_aarch64`: it builds **two**
hardware-isolated EL0 address spaces from a pure-Rust fixture program (which
yields then exits through the `tairix_rt::yield_now` / `exit` wrappers),
admits each as a resumable user kthread via `spawn_user_kthread`, and drives
the cooperative `step` loop. Each task's `yield` / `exit` `svc` traps to a
dispatch callback that calls `reschedule_current`, suspending the running
task back to the dispatcher so the scheduler interleaves the two through real
EL0→EL0 context switches — each switching back into its own page-table root
via its `pre_resume` hook. The run passes once both tasks have yielded their
full count and exited, with no task left live.

The syscall-completion boundary has a separate aarch64 proof in
`tests/integration/syscall_resume_qemu_aarch64`. A real EL0 parent completes
an ordinary syscall with a pending reschedule; its successful result remains
on the parent kernel stack while a second EL0 task enters its own address
space and parks from a blocking syscall. The parent then resumes the suspended
handler, receives the original result, and exits cleanly. A CFQ host regression
adds the SMP half: after the parent yields and the child parks on another CPU,
an idle sibling steals and resumes the parent with its closure state intact.
These tests pin that a runnable continuation always has exactly one scheduler
owner across park and migration.

The production-dispatch control/migration siblings extend that proof through
real synchronous IPC: the one-vCPU run is the control, while the four-vCPU run
uses dispatcher handshakes to pause each observed source only after the caller
blocks and force a remote steal. Sixty-four replies cross at least four CPU
transitions while callee-saved integer/FP registers, stack, control flow, and
address-space-local state remain intact. The store/filesystem `SleepLock` also
hands ownership directly to its oldest FIFO waiter while remaining closed to
fresh contenders; wake-one without this reservation permits barging and can
starve the very continuation the scheduler correctly readied.

### Two ring-3 tasks timeshare a CPU on x86_64 (`plans/PI.md` X2)

The same two-task timeshare on x86_64 needs one extra, x86_64-specific piece
of the kthread runtime, because the x86_64 `syscall` entry flips a per-CPU
register convention (`swapgs`) that stays flipped for the duration of a
handler. When a user kthread parks *mid-handler*, the dispatcher may enter a
**different** task whose own `enter_user` path does no `swapgs`, so that
task's next `syscall` would observe an unbalanced GS-swap and fault. The
arch-neutral fix is a cooperative-park hook pair on
`tairix_arch_api::ContextSwitch` — `enter_cooperative_park` /
`leave_cooperative_park`, both **default no-op** — that the kthread runtime
calls in the *syscall* suspend thunk around the suspend switch (the
user-kthread mid-handler park path; a kernel kthread's *body* suspend
skips the bracket — it never entered the convention). They are the exact
analogue of the `pre_resume`
stack-top argument: a seam ports that need nothing (aarch64 saves its return
state in the trap frame; riscv64 has no cooperative mid-handler park yet)
leave at the default, and only x86_64 overrides — there with a `swapgs` that
balances the GS convention across the park. The x86_64 durable user-`%rsp`
save also moves onto each task's own kernel-stack frame so a parked task's
saved user stack pointer is not clobbered by another task's syscall through
the shared per-CPU slot. Both fixes are detailed on [the x86_64 platform
page](../platform/x86_64.md) ("Two-task ring-3 timeshare") and proven by
`tests/integration/spawn_el0_timeshare_qemu_x86_64`, the x86_64 sibling of
the aarch64 vertical above.

## Asynchronous process launch (admit-then-load)

`SyscallNumber::SPAWN` never blocks the calling task on the heavy work of
starting a program. The freeze it used to cause — a desktop, shell, or any
interactive loop stalled for the whole of a disk read, signature
verification, and address-space build — is removed by splitting the launch
into a small synchronous half on the caller and a deferred half on the new
child's own first scheduled slice. The design is staged in
[`plans/FIX-DESKTOP.md`](../../../plans/FIX-DESKTOP.md) (DESK-1); this
section is the source of truth for the task model.

**Synchronous on the caller (fail-fast, fail-closed).** The `spawn`
handler does only manifest-independent work whose cost is bounded and does
not touch the program's bytes: the `CAP_PROC_SPAWN` check, the path
copy-in and `@self` substitution, attach/standard-stream resolution, the
kernel-attested credential (`resolve_spawn_credential`), the process
identity (`proc_id`, name, spawn path), and the **syntactic** resolution
of the path to either a boot-floor program or a well-formed
`<Name>.app/Run` store bundle. An unresolvable path or a malformed
attach/strings block fails closed here with an errno, admitting nothing.
It then builds a `LoadPlan` — `Prebuilt` (an embedded/driver image whose
`'static`/owned bytes and manifest request are already known) or `Bundle`
(a store bundle the child will read itself) — and calls
`KernelSpawnCtx::admit_loading`, which returns the minted PID at once. The
caller keeps running: no repaint, cursor, or input loop is stalled.

**Deferred on the child.** `admit_loading` registers a **parked plain
kernel kthread** carrying only the manifest-independent admit state: a
*placeholder empty-capability* record (derived through the shared
`ChildRecordSeed`/`derive_task_record` so it and the later effective
record can never diverge), the resolved standard streams and wired open
entries, inherited resource limits and working directory, any device
grants and matched node, and the parent/child wait link. Its loading body
then runs on the child's *own* task: it parks for the app store if needed
(`body_wait_app_store`, via `reschedule_current` — never a busy-spin),
obtains the verified image (a prebuilt image directly, or a bundle read
and verified under the child's *own* `(uid, ceiling)` credential through
the shared load gate), derives and installs the effective capability set
(`ceiling ∩ manifest`) **replacing** the placeholder, builds the isolated
address space through `ArchImageBuilder::build`, registers the frozen
space and user-stack span, and finally upgrades itself into a user task
via `Yielder::become_user`. No unverified byte is ever mapped, and the
child is never dispatchable under the placeholder authority: the effective
record and the frozen space are installed strictly before `become_user`.

**Failure is loud, never silent (`AGENTS.md` §24).** A load that fails on
the child's own slice does not surface as a `spawn` errno — the caller
already has the PID. Instead the child audits the refusal
(`emit_load_refusal`, a `ProcessSpawnDenied` event attributed to the child
with the reserved status) and exits with a reserved load-failure status,
so the parent reaps a code it can turn into a `stderr` diagnosis. The
status band is one `lib/abi` definition both sides share:
`load_failure_status(Errno) -> i32` maps every load-path error onto
`LOAD_NOT_FOUND` / `LOAD_UNVERIFIED` / `LOAD_MALFORMED` / `LOAD_OOM`, and
`load_failure_reason(i32) -> Option<&str>` is the reverse map the parent
reap uses. A refused load leaves nothing half-installed: no address space
is registered and the admit-time bookkeeping is reclaimed.

### The parent reports the refusal (DESK-2)

Because a load refusal now arrives as the child's *exit status* rather than
the `spawn` return, a launcher must inspect what it reaps. The desktop
session is the worked example (`userland/gui/session`): every app started
from the taskbar's launchers or its program-library popup is admitted
immediately and its PID remembered under its display label, and the
session's `CHILD_TOKEN` wait-set member reaps
every exited child. On each reap it maps the status through the shared
`launch_failure_report`, which consults `load_failure_reason`: a reserved
`LOAD_*` code becomes a terse, named line on `stderr` (e.g. `desktop: Files
failed to launch: signature or hash verification failed`), while a clean or
ordinary exit reports nothing. A refused launch is therefore a loud,
non-fatal diagnosis — the desktop keeps presenting and handling input,
never freezing and never letting a failed app vanish without explanation
(`AGENTS.md` §24.1, fail loud / degrade gracefully). Reusing
`load_failure_reason` keeps the wording identical to every other launcher's
diagnosis (`AGENTS.md` §2.2), so the shell and the desktop describe the same
cause the same way.
