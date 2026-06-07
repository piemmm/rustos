//! Resumable kernel-thread task runtime (`plans/SPAWN.md` SP1).
//!
//! A `kernel/sched` task is admitted with a body closure
//! `FnMut(&mut TaskContext) -> TaskAction` that the scheduler invokes
//! once per dispatch step (`AGENTS.md` §17.1). That contract alone has no
//! notion of a task that *suspends mid-execution and later resumes*: the
//! body runs to a `TaskAction` and returns every time. Real multitasking
//! — and, ultimately, two EL0 user tasks timesharing a CPU
//! (`plans/SPAWN.md` SP2) — needs a task that owns a kernel stack and can
//! be parked at an arbitrary point and resumed exactly there.
//!
//! This module layers that *on top of* the closure contract without
//! changing it (the §17.1 / §2.4 modularity guarantee): a **kthread** is a
//! resumable kernel thread driven through the Arch HAL context-switch
//! slice ([`rustos_arch_api::ContextSwitch`], §17.2). The body the
//! scheduler sees is a thin **shim** owned here; the task's real work runs
//! as a stackful coroutine on its own kernel stack.
//!
//! # The model
//!
//! Each kthread owns a `ThreadControl` block (heap-allocated for a
//! stable address) holding two [`TaskContext`] save areas — the task's and
//! the dispatcher's — its requested [`TaskAction`], a run-state, the work
//! closure, and its kernel stack. The shim closure handed to
//! [`rustos_kernel_sched_api::SchedulerPolicy::spawn`] does, on each
//! dispatch step:
//!
//! 1. on the **first** step, [`ContextSwitch::prepare`] the task's first
//!    frame so it lands in `trampoline`, then fall through;
//! 2. [`ContextSwitch::switch`] *into* the task (saving the dispatcher's
//!    context in `dispatch_ctx`);
//! 3. the task runs until it cooperatively suspends — [`Yielder::yield_now`]
//!    / [`Yielder::park`] switch back to `dispatch_ctx`, or the work
//!    returns and `trampoline` switches back with [`TaskAction::Exit`];
//! 4. control returns to the shim right after the step-2 switch; it reads
//!    the task's requested [`TaskAction`] and returns it to the scheduler.
//!
//! The scheduler crates (`kernel/sched/*`) are untouched.
//!
//! # Why raw pointers across the switch
//!
//! The shim (dispatcher side) and `trampoline`/[`Yielder`] (task side)
//! both reach the same `ThreadControl`, but **never concurrently**: a
//! cooperative context switch hands the single CPU from one to the other,
//! so they are temporally exclusive. To keep that sound under the aliasing
//! model, neither side holds a reference to the control block *across* a
//! switch — every access is through a raw pointer with a reference whose
//! scope ends before the switch, and the [`ContextSwitch`] handle is
//! copied out (`C: Copy`) rather than borrowed across the boundary.
//!
//! # Host vs. bare-metal
//!
//! [`ContextSwitch::switch`] only transfers control on a bare-metal target
//! (the host build's port `switch` is `unreachable!`), so the full
//! coroutine round-trip is proven by the per-arch QEMU verticals. The host
//! tests here cover the host-observable contract — the shim's state
//! machine, the fail-closed `prepare` rejection, and the stack-reclaim /
//! use-after-free discipline against the `kernel/mem` slab tag check
//! (`AGENTS.md` §19.10) — exactly as [`rustos_arch_api::context::conformance`]
//! tests only the host-testable `prepare`.

use alloc::boxed::Box;
use core::ptr::addr_of_mut;

use rustos_arch_api::{ContextSwitch, TaskContext};
use rustos_kernel_sched_api::{
    CpuId, Priority, SchedResult, SchedulerArch, SchedulerPolicy, TaskAction, TaskId,
};

/// Default per-kthread kernel-stack size, in bytes.
///
/// Sixteen KiB is comfortably above the synthesised initial frame plus the
/// modest call depth a cooperative kthread body reaches before its next
/// suspension point, and is a whole number of 4 KiB pages so a future
/// guard-paged stack source (`AGENTS.md` §4) maps cleanly.
pub const KTHREAD_STACK_BYTES: usize = 16 * 1024;

/// A kthread's owned kernel stack: a stable, `STACK_ALIGN`-aligned region
/// whose exclusive top (`Self::top`) seeds the task's first frame and
/// whose storage is reclaimed when the value is dropped.
///
/// The runtime owns one per task inside its control block; when the
/// task exits the scheduler drops the body, which drops the control block,
/// which drops the stack — reclaiming it. Because an exited task is never
/// switched into again (the shim returns [`TaskAction::Exit`] and the
/// scheduler never re-invokes the body), nothing executes on the stack
/// after it is freed, so there is no use-after-free (`AGENTS.md` §19.10).
///
/// # Safety
///
/// [`Self::top`] must return the exclusive upper bound (one past the last
/// addressable byte) of a region that is mapped, writable, exclusive to
/// the task, `STACK_ALIGN`-aligned, and stays valid for as long as the
/// stack value lives.
pub unsafe trait KernelStack {
    /// Exclusive upper bound of the stack (one past its last byte),
    /// aligned to `STACK_ALIGN`.
    fn top(&self) -> u64;
}

/// Heap-backed kernel stack: the production [`KernelStack`] source.
///
/// A `STACK_ALIGN`-aligned, [`KTHREAD_STACK_BYTES`]-sized heap box. The
/// allocation has a stable address for the box's lifetime and is freed on
/// drop, reclaiming the stack.
pub struct BoxStack(Box<StackBytes>);

#[repr(C, align(16))]
struct StackBytes([u8; KTHREAD_STACK_BYTES]);

/// The widest ABI stack alignment any target requires (`AGENTS.md`
/// §17.2); [`ContextSwitch::prepare`] rejects a misaligned `stack_top`.
const STACK_ALIGN: usize = 16;

/// [`StackBytes`] must be at least [`STACK_ALIGN`]-aligned so its `top` is a
/// valid `stack_top` for [`ContextSwitch::prepare`], and its size a whole
/// number of [`STACK_ALIGN`] units so the top stays aligned.
const _STACK_LAYOUT_OK: () = {
    assert!(core::mem::align_of::<StackBytes>() >= STACK_ALIGN);
    assert!(KTHREAD_STACK_BYTES % STACK_ALIGN == 0);
};

impl BoxStack {
    /// Allocate a fresh zeroed kernel stack on the heap.
    #[must_use]
    pub fn new() -> Self {
        Self(Box::new(StackBytes([0u8; KTHREAD_STACK_BYTES])))
    }
}

impl Default for BoxStack {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `top` returns `base + KTHREAD_STACK_BYTES`, the exclusive upper
// bound of the heap box's storage. `StackBytes` is `align(16)`, so `base`
// — and therefore `top` — is 16-aligned. The box owns the storage and
// frees it on drop, and the region is exclusive to its owner.
unsafe impl KernelStack for BoxStack {
    fn top(&self) -> u64 {
        let base = core::ptr::addr_of!(*self.0) as u64;
        let top = base + KTHREAD_STACK_BYTES as u64;
        // Round down to `STACK_ALIGN`. Given the layout const-assert this is
        // a no-op, but it makes the alignment `ContextSwitch::prepare`
        // requires total rather than merely an invariant we rely on.
        top & !(STACK_ALIGN as u64 - 1)
    }
}

/// Where a kthread is in its lifecycle, from the shim's point of view.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RunState {
    /// The task's first frame has not been seeded yet; the next dispatch
    /// step calls [`ContextSwitch::prepare`].
    NotStarted,
    /// The task has been entered at least once and is suspended at a
    /// cooperative reschedule point, ready to resume.
    Running,
    /// The work has returned (or could not be started); the task is
    /// terminal and must never be switched into again.
    Finished,
}

/// The work-closure type a kthread runs: a `Send` coroutine body that
/// drives a [`Yielder`] to suspend cooperatively and returns when the task
/// is done. Boxed and type-erased so [`ThreadControl`] does not carry the
/// concrete closure type (which would otherwise leak into every consumer's
/// generics).
type Work<C> = Box<dyn FnMut(&mut Yielder<C>) + Send + 'static>;

/// The suspension handle a kthread's work closure uses to cooperatively
/// yield the CPU back to the scheduler.
///
/// A `Yielder` borrows the three fields of its task's control block it
/// needs as raw pointers, plus a copy of the port's [`ContextSwitch`]
/// handle (`C: Copy`, so nothing is borrowed across the switch). Calling
/// [`Self::yield_now`] or [`Self::park`] records the requested
/// [`TaskAction`] and switches back to the dispatcher; the call returns
/// when the scheduler next dispatches this task and the shim switches back
/// in, so the work resumes exactly where it suspended.
pub struct Yielder<C: ContextSwitch + Copy> {
    cs: C,
    task_ctx: *mut TaskContext,
    dispatch_ctx: *mut TaskContext,
    action: *mut TaskAction,
}

impl<C: ContextSwitch + Copy> Yielder<C> {
    /// Cooperatively yield: re-enqueue this task at its current priority
    /// and run something else, resuming here on the next dispatch.
    pub fn yield_now(&mut self) {
        self.suspend(TaskAction::Yield);
    }

    /// Park this task: do not re-enqueue it until an external `unpark`
    /// wakes it, then resume here.
    pub fn park(&mut self) {
        self.suspend(TaskAction::Park);
    }

    /// Record `action` and switch back to the dispatcher's saved context.
    fn suspend(&mut self, action: TaskAction) {
        // SAFETY: `action`, `task_ctx`, and `dispatch_ctx` point at live,
        // disjoint fields of this task's `ThreadControl`, which outlives
        // the running work (it owns the closure). The CPU is exclusively
        // running this task during the work, so writing `*self.action`
        // and switching are race-free. `task_ctx` is the running task's
        // context (where `switch` saves our suspension point) and
        // `dispatch_ctx` was made runnable by the shim's switch into us,
        // satisfying `ContextSwitch::switch`'s contract.
        unsafe {
            *self.action = action;
            self.cs.switch(self.task_ctx, self.dispatch_ctx);
        }
    }
}

/// The per-kthread control block: everything the dispatcher-side shim and
/// the task-side [`trampoline`]/[`Yielder`] share.
///
/// Heap-allocated (boxed by the shim) so its address is stable while both
/// sides reach it through raw pointers. It is reached from exactly one
/// side at a time — a cooperative context switch hands the CPU between
/// them — so the raw-pointer accesses never alias a live reference across
/// a switch (see the module docs).
struct ThreadControl<C: ContextSwitch + Copy, S: KernelStack> {
    /// The port's context-switch handle, copied into each [`Yielder`].
    cs: C,
    /// The task's saved kernel-stack pointer (its suspension point).
    task_ctx: TaskContext,
    /// The dispatcher's saved context, recorded when the shim switches in.
    dispatch_ctx: TaskContext,
    /// The action the task last requested of the scheduler.
    action: TaskAction,
    /// Lifecycle state from the shim's perspective.
    state: RunState,
    /// The task's owned kernel stack (reclaimed on drop).
    stack: S,
    /// The work to run, taken by the trampoline on first entry. `None`
    /// once taken; a never-started task that fails `prepare` leaves it
    /// `Some` and drops it with the control block.
    work: Option<Work<C>>,
}

/// The entry point a freshly prepared kthread first runs.
///
/// Reached via [`ContextSwitch::switch`] into the frame
/// [`ContextSwitch::prepare`] seeded with `arg` = the task's
/// `*mut ThreadControl<C, S>`. It takes the work out of the control block,
/// runs it to completion (the work drives its [`Yielder`] to suspend and
/// resume in between), then marks the task terminal and switches back to
/// the dispatcher, never to be resumed.
///
/// # Safety
///
/// `arg` must be the `usize`-cast address of a live, boxed
/// `ThreadControl<C, S>` whose `task_ctx` was seeded by
/// [`ContextSwitch::prepare`] with this function as the entry. The
/// scheduler/shim upholds this: it is the only caller of `prepare`, and it
/// passes exactly that address.
unsafe extern "C" fn trampoline<C, S>(arg: usize) -> !
where
    C: ContextSwitch + Copy,
    S: KernelStack,
{
    let ctl = arg as *mut ThreadControl<C, S>;

    // Take the work out (a transient borrow of the `Option` field, dropped
    // before the work runs). `None` only if the task was somehow entered
    // twice — impossible on the shim's path — so a missing body simply
    // falls through to the terminal switch-back (fail closed, §2.9).
    // SAFETY: `ctl` is the live control block per this function's contract.
    let work = unsafe { (*ctl).work.take() };
    if let Some(mut work) = work {
        // SAFETY: `ctl` is live; `cs` is `Copy`.
        let cs = unsafe { (*ctl).cs };
        let mut yielder = Yielder {
            cs,
            // SAFETY: these address distinct, live fields of `*ctl`.
            task_ctx: unsafe { addr_of_mut!((*ctl).task_ctx) },
            dispatch_ctx: unsafe { addr_of_mut!((*ctl).dispatch_ctx) },
            action: unsafe { addr_of_mut!((*ctl).action) },
        };
        work(&mut yielder);
    }

    // The work returned: the task is terminal. Record `Exit` so the shim
    // reports it to the scheduler.
    // SAFETY: `ctl` is live.
    unsafe {
        (*ctl).action = TaskAction::Exit;
        (*ctl).state = RunState::Finished;
    }
    // SAFETY: `ctl` is live; `cs` is `Copy`.
    let cs = unsafe { (*ctl).cs };
    loop {
        // Switch back to the dispatcher. The scheduler observes `Exit` and
        // never dispatches this terminal task again, so control never
        // returns here; the loop is a fail-closed guard against an
        // erroneous resume (`AGENTS.md` §2.9), not an expected path.
        // SAFETY: `task_ctx`/`dispatch_ctx` are live, disjoint fields of
        // `*ctl`; `dispatch_ctx` holds the dispatcher's runnable context.
        unsafe {
            cs.switch(
                addr_of_mut!((*ctl).task_ctx),
                addr_of_mut!((*ctl).dispatch_ctx),
            );
        }
    }
}

/// Admit a resumable kthread onto `scheduler`, giving it a fresh
/// heap-backed kernel stack ([`BoxStack`]).
///
/// `work` is the coroutine body: it runs on the kthread's own kernel stack
/// and uses its [`Yielder`] to cooperatively suspend
/// ([`Yielder::yield_now`] / [`Yielder::park`]); returning from `work`
/// exits the task. The call returns the new [`TaskId`].
///
/// The scheduler's closure-body contract (`AGENTS.md` §17.1) is preserved:
/// the body it receives is a thin shim owned here that drives `work`
/// through the [`ContextSwitch`] HAL.
///
/// # Errors
///
/// Propagates [`SchedulerPolicy::spawn`]'s error (e.g.
/// [`rustos_kernel_sched_api::SchedError::NoSuchCpu`] for an out-of-range
/// `home_cpu`).
pub fn spawn_kthread<C, A, P, W>(
    scheduler: &P,
    cs: C,
    home_cpu: CpuId,
    priority: Priority,
    work: W,
) -> SchedResult<TaskId>
where
    C: ContextSwitch + Copy + Send + 'static,
    A: SchedulerArch,
    P: SchedulerPolicy<A>,
    W: FnMut(&mut Yielder<C>) + Send + 'static,
{
    spawn_kthread_with_stack(scheduler, cs, BoxStack::new(), home_cpu, priority, work)
}

/// Admit a resumable kthread onto `scheduler` over a caller-supplied
/// kernel stack `stack`.
///
/// Identical to [`spawn_kthread`] but lets the caller own the stack source
/// — a guard-paged stack on a real port, a slab-backed stack the
/// use-after-free tag check covers (`AGENTS.md` §19.10), or a static stack
/// in a freestanding test. [`spawn_kthread`] is the common case
/// ([`BoxStack`]).
///
/// # Errors
///
/// As [`spawn_kthread`].
pub fn spawn_kthread_with_stack<C, A, P, S, W>(
    scheduler: &P,
    cs: C,
    stack: S,
    home_cpu: CpuId,
    priority: Priority,
    work: W,
) -> SchedResult<TaskId>
where
    C: ContextSwitch + Copy + Send + 'static,
    A: SchedulerArch,
    P: SchedulerPolicy<A>,
    S: KernelStack + Send + 'static,
    W: FnMut(&mut Yielder<C>) + Send + 'static,
{
    let mut control: Box<ThreadControl<C, S>> = Box::new(ThreadControl {
        cs,
        task_ctx: TaskContext::empty(),
        dispatch_ctx: TaskContext::empty(),
        action: TaskAction::Yield,
        state: RunState::NotStarted,
        stack,
        work: Some(Box::new(work)),
    });

    // The `move` closure owns the boxed control block, so its heap address
    // stays stable for the raw-pointer protocol; `&mut control` derefs to
    // the `&mut ThreadControl` the shim step takes.
    scheduler.spawn(home_cpu, priority, move |_step| dispatch_step(&mut control))
}

/// Run one dispatch step of the kthread whose control block is `control`.
///
/// This is the shim's per-step logic, factored out so the host tests drive
/// it directly. It seeds the first frame on the first step, switches into
/// the task, and returns the [`TaskAction`] the task requested when it
/// switched back.
fn dispatch_step<C, S>(control: &mut ThreadControl<C, S>) -> TaskAction
where
    C: ContextSwitch + Copy,
    S: KernelStack,
{
    let ctl: *mut ThreadControl<C, S> = addr_of_mut!(*control);

    // SAFETY (all `*ctl` accesses below): `ctl` is the address of the live
    // boxed control block `control` owns; no other reference to it is live
    // while this runs, and the task side only runs *between* our switch
    // calls (cooperative hand-off), never concurrently.
    match unsafe { (*ctl).state } {
        // A task that has already exited reports `Exit` every time the
        // scheduler asks again, and is never switched into.
        RunState::Finished => return TaskAction::Exit,
        RunState::NotStarted => {
            let cs = unsafe { (*ctl).cs };
            let top = unsafe { (*ctl).stack.top() };
            // Seed the first frame at `trampoline`, passing the control
            // block address as the entry argument.
            let prepared = cs.prepare(
                unsafe { &mut *addr_of_mut!((*ctl).task_ctx) },
                top,
                trampoline::<C, S>,
                ctl as usize,
            );
            if prepared.is_err() {
                // A stack that cannot seed a frame fails the task closed:
                // mark it terminal and exit rather than switch into an
                // unrunnable context (`AGENTS.md` §2.9 / §5.4).
                unsafe {
                    (*ctl).state = RunState::Finished;
                }
                return TaskAction::Exit;
            }
            unsafe {
                (*ctl).state = RunState::Running;
            }
        }
        RunState::Running => {}
    }

    let cs = unsafe { (*ctl).cs };
    // SAFETY: switch into the task. `dispatch_ctx` saves our (the
    // dispatcher's) context; `task_ctx` was made runnable by `prepare`
    // (first step) or a prior `Yielder` suspension (later steps), so it
    // satisfies `ContextSwitch::switch`'s runnable-`next` contract.
    unsafe {
        cs.switch(
            addr_of_mut!((*ctl).dispatch_ctx),
            addr_of_mut!((*ctl).task_ctx),
        );
    }

    // The task switched back to us; report the action it requested.
    unsafe { (*ctl).action }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::boxed::Box as StdBox;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use rustos_arch_api::{PrepareError, TaskEntry};
    use rustos_kernel_mem::{Slab, SlabError, SlabHandle};
    use rustos_kernel_sched_api::{SchedulerConfig, TaskState};

    use crate::sched::Scheduler;
    use crate::test_arch::TestArch;

    /// What a [`RecordingCs`] saw — one recorder per test, so parallel
    /// test threads never share state.
    #[derive(Default)]
    struct Recorder {
        prepares: AtomicUsize,
        switches: AtomicUsize,
        last_stack_top: AtomicU64,
        last_arg: AtomicU64,
        last_entry: AtomicU64,
        last_prev: AtomicU64,
        last_next: AtomicU64,
    }

    /// A faithful host [`ContextSwitch`] double. `prepare` seeds a
    /// plausible in-bounds frame and records its arguments; `switch` is a
    /// no-op (real control transfer is bare-metal only) that records the
    /// pointers it was handed. Carries a `&'static Recorder` so it stays
    /// `Copy + Send + Sync`.
    #[derive(Copy, Clone)]
    struct RecordingCs(&'static Recorder);

    /// Frame the double reserves below `stack_top`; below the 512-byte
    /// conformance stack, above the 16-byte too-small probe.
    const DOUBLE_FRAME: u64 = 64;

    impl ContextSwitch for RecordingCs {
        fn prepare(
            &self,
            ctx: &mut TaskContext,
            stack_top: u64,
            entry: TaskEntry,
            arg: usize,
        ) -> Result<(), PrepareError> {
            if stack_top == 0 {
                return Err(PrepareError::NullStack);
            }
            if stack_top % 16 != 0 {
                return Err(PrepareError::Misaligned);
            }
            if stack_top < DOUBLE_FRAME {
                return Err(PrepareError::TooSmall);
            }
            self.0.prepares.fetch_add(1, Ordering::SeqCst);
            self.0.last_stack_top.store(stack_top, Ordering::SeqCst);
            self.0.last_arg.store(arg as u64, Ordering::SeqCst);
            self.0
                .last_entry
                .store(entry as usize as u64, Ordering::SeqCst);
            ctx.stack_pointer = stack_top - DOUBLE_FRAME;
            Ok(())
        }

        unsafe fn switch(&self, prev: *mut TaskContext, next: *mut TaskContext) {
            self.0.switches.fetch_add(1, Ordering::SeqCst);
            self.0.last_prev.store(prev as u64, Ordering::SeqCst);
            self.0.last_next.store(next as u64, Ordering::SeqCst);
            // No control transfer on the host (see the module docs); the
            // real switch is proven by the per-arch QEMU verticals.
        }
    }

    /// A `ContextSwitch` double whose `prepare` always fails closed, used
    /// to prove the shim turns an unrunnable stack into a clean `Exit`.
    #[derive(Copy, Clone)]
    struct FailingCs;

    impl ContextSwitch for FailingCs {
        fn prepare(
            &self,
            _ctx: &mut TaskContext,
            _stack_top: u64,
            _entry: TaskEntry,
            _arg: usize,
        ) -> Result<(), PrepareError> {
            Err(PrepareError::TooSmall)
        }

        unsafe fn switch(&self, _prev: *mut TaskContext, _next: *mut TaskContext) {
            unreachable!("FailingCs never reaches a runnable task")
        }
    }

    fn recorder() -> &'static Recorder {
        StdBox::leak(StdBox::new(Recorder::default()))
    }

    /// Build a boxed control block directly (bypassing the scheduler) so a
    /// test can drive [`dispatch_step`] and inspect the shim's state
    /// machine without a real context switch.
    ///
    /// Boxed for the stable heap address the raw-pointer protocol needs
    /// (the production `spawn` path boxes for the same reason).
    #[allow(clippy::unnecessary_box_returns)]
    fn control_with<C: ContextSwitch + Copy, S: KernelStack>(
        cs: C,
        stack: S,
    ) -> Box<ThreadControl<C, S>> {
        Box::new(ThreadControl {
            cs,
            task_ctx: TaskContext::empty(),
            dispatch_ctx: TaskContext::empty(),
            action: TaskAction::Yield,
            state: RunState::NotStarted,
            stack,
            work: Some(Box::new(|_y: &mut Yielder<C>| {})),
        })
    }

    #[test]
    fn first_dispatch_step_prepares_then_switches_in() {
        let rec = recorder();
        let cs = RecordingCs(rec);
        let stack = BoxStack::new();
        let top = stack.top();
        let mut control = control_with(cs, stack);
        let ctl_addr = addr_of_mut!(*control) as u64;

        let action = dispatch_step(&mut control);

        // One prepare, with the stack's top, the trampoline entry, and the
        // control block address as the entry argument.
        assert_eq!(rec.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(rec.last_stack_top.load(Ordering::SeqCst), top);
        assert_eq!(rec.last_arg.load(Ordering::SeqCst), ctl_addr);
        assert_eq!(
            rec.last_entry.load(Ordering::SeqCst),
            trampoline::<RecordingCs, BoxStack> as *const () as u64
        );
        // Then exactly one switch, dispatch_ctx -> task_ctx.
        assert_eq!(rec.switches.load(Ordering::SeqCst), 1);
        assert_eq!(control.state, RunState::Running);
        // The no-op double leaves the action at its initial value.
        assert_eq!(action, TaskAction::Yield);
    }

    #[test]
    fn second_dispatch_step_skips_prepare() {
        let rec = recorder();
        let mut control = control_with(RecordingCs(rec), BoxStack::new());

        let _ = dispatch_step(&mut control);
        let _ = dispatch_step(&mut control);

        // Prepare happens once; each step switches in.
        assert_eq!(rec.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(rec.switches.load(Ordering::SeqCst), 2);
        assert_eq!(control.state, RunState::Running);
    }

    #[test]
    fn failed_prepare_exits_without_switching() {
        let mut control = control_with(FailingCs, BoxStack::new());

        let action = dispatch_step(&mut control);

        // Fail closed: report Exit, mark terminal, never switch into an
        // unrunnable context.
        assert_eq!(action, TaskAction::Exit);
        assert_eq!(control.state, RunState::Finished);
    }

    #[test]
    fn finished_task_reports_exit_without_touching_the_port() {
        let rec = recorder();
        let mut control = control_with(RecordingCs(rec), BoxStack::new());
        control.state = RunState::Finished;

        let action = dispatch_step(&mut control);

        assert_eq!(action, TaskAction::Exit);
        // A terminal task is never prepared or switched into again.
        assert_eq!(rec.prepares.load(Ordering::SeqCst), 0);
        assert_eq!(rec.switches.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn yielder_yield_now_records_action_and_switches_back() {
        let rec = recorder();
        let cs = RecordingCs(rec);
        let mut control = control_with(cs, BoxStack::new());
        let ctl: *mut ThreadControl<RecordingCs, BoxStack> = addr_of_mut!(*control);

        let mut yielder = Yielder {
            cs,
            task_ctx: unsafe { addr_of_mut!((*ctl).task_ctx) },
            dispatch_ctx: unsafe { addr_of_mut!((*ctl).dispatch_ctx) },
            action: unsafe { addr_of_mut!((*ctl).action) },
        };

        yielder.yield_now();
        assert_eq!(control.action, TaskAction::Yield);
        yielder.park();
        assert_eq!(control.action, TaskAction::Park);

        // Each suspension switches task_ctx -> dispatch_ctx.
        assert_eq!(rec.switches.load(Ordering::SeqCst), 2);
        assert_eq!(
            rec.last_prev.load(Ordering::SeqCst),
            unsafe { addr_of_mut!((*ctl).task_ctx) } as u64
        );
        assert_eq!(rec.last_next.load(Ordering::SeqCst), unsafe {
            addr_of_mut!((*ctl).dispatch_ctx)
        } as u64);
    }

    #[test]
    fn spawn_kthread_admits_a_task_on_a_live_scheduler() {
        let arch = Arc::new(TestArch::with_cpus(1));
        let scheduler = Scheduler::new(SchedulerConfig::defaults_for(1), Arc::clone(&arch))
            .expect("scheduler builds");
        let rec = recorder();

        let id = spawn_kthread(&scheduler, RecordingCs(rec), 0, Priority::Normal, |_y| {})
            .expect("kthread admitted");
        assert_eq!(scheduler.live_task_count(), 1);

        // One dispatch step runs the shim body, which (with the host's
        // no-op switch) reports Yield, so the task is re-enqueued and stays
        // live. The real coroutine run-to-exit is proven under QEMU.
        let _ = scheduler.step(0);
        assert!(scheduler.run_count(id).expect("known task") >= 1);
        assert_ne!(scheduler.state_of(id), TaskState::Exited);
    }

    // --- Stack reclaim + use-after-free (AGENTS.md §19.10) -------------

    /// A kernel stack carved from a [`Slab`] slot so the slab's software
    /// use-after-free tag check (`AGENTS.md` §19.10) covers it: freeing the
    /// stack rotates the slot tag, so the stale [`SlabHandle`] the freed
    /// stack held is rejected as a [`SlabError::TagMismatch`].
    struct SlabStack {
        slab: Rc<RefCell<Slab>>,
        handle: SlabHandle,
        top: u64,
    }

    impl SlabStack {
        /// Reserve a slot and align a kthread stack inside it.
        fn new(slab: &Rc<RefCell<Slab>>) -> (Self, SlabHandle) {
            let handle = slab.borrow_mut().alloc().expect("slab slot");
            let mut guard = slab.borrow_mut();
            let slot = guard.slot_mut(handle).expect("live slot");
            let base = slot.as_mut_ptr() as u64;
            // Align the usable base up to STACK_ALIGN within the slot; the
            // slot is oversized by STACK_ALIGN so the aligned stack fits.
            let aligned = (base + (STACK_ALIGN as u64 - 1)) & !(STACK_ALIGN as u64 - 1);
            let top = aligned + KTHREAD_STACK_BYTES as u64;
            drop(guard);
            (
                Self {
                    slab: Rc::clone(slab),
                    handle,
                    top,
                },
                handle,
            )
        }
    }

    // SAFETY: `top` is `aligned + KTHREAD_STACK_BYTES`, the exclusive upper
    // bound of a 16-aligned region inside the slab slot (oversized by
    // STACK_ALIGN so it fits). The slot stays valid until `Drop` frees it,
    // which is when the stack value itself is dropped.
    unsafe impl KernelStack for SlabStack {
        fn top(&self) -> u64 {
            self.top
        }
    }

    impl Drop for SlabStack {
        fn drop(&mut self) {
            // Reclaim the slab slot; the tag rotation on the next alloc
            // makes the handle this stack held a detectable UAF.
            let _ = self.slab.borrow_mut().free(self.handle);
        }
    }

    #[test]
    fn exiting_kthread_reclaims_its_stack_and_a_stale_handle_is_a_uaf() {
        let slab = Rc::new(RefCell::new(
            // One slot, oversized so a 16-aligned stack fits inside it.
            Slab::new(KTHREAD_STACK_BYTES + STACK_ALIGN, 1).expect("slab"),
        ));
        let (stack, stale) = SlabStack::new(&slab);
        assert_eq!(slab.borrow().live(), 1);

        // The control block owns the stack, exactly as a spawned kthread's
        // shim body does. Dropping it models the scheduler dropping the
        // body when the task exits.
        let control = control_with(RecordingCs(recorder()), stack);
        drop(control);

        // The stack was reclaimed.
        assert_eq!(slab.borrow().live(), 0);

        // Re-allocating the slot rotates its tag, so the stale handle the
        // freed stack held is now a use-after-free the slab rejects
        // (`AGENTS.md` §19.10) — there is no silent reuse of a dangling
        // kernel stack.
        let _fresh = slab.borrow_mut().alloc().expect("slot reused");
        assert_eq!(
            slab.borrow_mut().slot_mut(stale).err(),
            Some(SlabError::TagMismatch)
        );
    }
}
