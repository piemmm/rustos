# Kernel multitasking and the kthread runtime

This page is the source of truth for how a RustOS scheduler task becomes a
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
(`rustos_arch_api::ContextSwitch`, §17.2) — the same `prepare` / `switch`
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

## Planned: bringing EL0 into the model

The runtime above is architecture-neutral and does not yet touch EL0.
Two further decisions are recorded here for the staged EL0 work
(`plans/SPAWN.md` SP2/SP3):

* **Trap-return reschedule decision point.** After the `kernel/syscall`
  dispatcher runs, if the call yielded / parked / exited the caller, the
  arch syscall-trap path saves the caller's EL0 frame into its task and
  `switch`es to the scheduler kthread instead of returning to user mode; on
  resume the task re-enters EL0 with its saved frame. A non-rescheduling
  syscall returns to the same task exactly as today. This is surfaced
  through an existing core seam, **not** a new HAL trait (§17.2).
* **EL0 save-area layout.** A user task is an SP1 kthread whose trampoline
  enters EL0 via `EnterUser::enter_user`; its per-task address space is
  swapped on resume through the existing `AddressSpace::activate` MMU HAL
  slice, so each task stays hardware-isolated (§4).
