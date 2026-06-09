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
use rustos_sync::SpinLock;

use crate::dispatch_slot::RescheduleAction;

/// Default per-kthread kernel-stack size, in bytes.
///
/// A **user** kthread's body does not merely set up a suspension point: once
/// it `eret`s into EL0, every syscall the task makes is handled *on this
/// stack* (the EL1 trap runs on the kthread's kernel stack). The deepest such
/// path is a full syscall dispatch — the arch trap prologue, the
/// `KernelDispatchHook` layers, a handler, and the validated user-memory copy
/// boundary ([`rustos_kernel_mem::uaccess`]) with its staging — and an
/// unoptimised debug build spills generously at every frame, so the real
/// working set is far above the "modest" depth a plain kernel kthread reaches.
/// Sixteen KiB was *not* enough: a `wait` handler (reap + `copy_to_user`)
/// overran a 16 KiB stack and silently corrupted the adjacent heap allocation
/// — the next task's frozen address-space snapshot (`plans/PI.md` P6e-3b-ii).
/// 64 KiB clears the deepest dispatch with margin and is a whole number of
/// 4 KiB pages so the guard page below it (see [`BoxStack`]) sits on a clean
/// page boundary.
///
/// This bound is **defence in depth**, not the only line of defence: the
/// [`BoxStack`] places a poison-filled guard page (`AGENTS.md` §4) immediately
/// *below* the usable region, so an overrun runs off the bottom of the stack
/// into the guard instead of straight into the neighbouring heap allocation.
/// A contiguous overrun trips the guard's canary, which `dispatch_step`
/// checks every time the task hands the CPU back, and the task is then failed
/// closed rather than allowed to run on a corrupt stack (`AGENTS.md` §2.9,
/// §2.17). The sizing still matters — the guard absorbs an overrun but a
/// generous stack avoids one in the first place — so this bound must
/// comfortably exceed the deepest syscall-handler call depth.
pub const KTHREAD_STACK_BYTES: usize = 64 * 1024;

/// Width of the [`BoxStack`] guard region, in bytes: one 4 KiB page.
///
/// The guard sits immediately below the usable stack. Sized at one page so
/// it matches the on-hardware form this emulates — a single *unmapped* page
/// below the stack (`AGENTS.md` §4, the same model `kernel/mem`'s slab guard
/// documents) — and absorbs a 4 KiB overrun before it can reach the
/// lower-addressed neighbour. The deployment form that turns the overrun into
/// an immediate hardware fault (unmapping this page in the kernel's own page
/// tables) is staged in `plans/PI.md`; until the page-table split it backs on
/// lands, the poison-byte emulation below is the real, non-deferred defence
/// (`AGENTS.md` §2.17 — a guard now, not "later").
const STACK_GUARD_BYTES: usize = 4096;

/// Byte the [`BoxStack`] guard page is filled with (`0xCC`, x86 `int3`),
/// matching `kernel/mem`'s slab guard: an "obviously wrong" value whose
/// disturbance signals an overrun. On the deployment (unmapped-page) form the
/// guard is never written at all — the access faults — so this byte is purely
/// the host/software-emulation sentinel.
const STACK_GUARD_BYTE: u8 = 0xCC;

/// Bytes at the *top* of the guard region (immediately below the usable
/// stack base) that [`BoxStack::check_guard`] verifies on the hot path.
///
/// A kernel stack grows downward and is written contiguously, so an overrun
/// must cross these bytes first; verifying this small, O(1) window on every
/// switch-back catches a contiguous overrun without scanning the whole guard
/// page on the scheduler hot path (`AGENTS.md` §2.16). The full page still
/// provides the 4 KiB of absorption.
const STACK_GUARD_CANARY_BYTES: usize = 64;

/// A kernel-stack guard violation: the task overran its stack into the
/// [`BoxStack`] guard region.
///
/// Returned by [`KernelStack::check_guard`]. On real hardware the overrun
/// faults on the unmapped guard page; the software emulation surfaces the
/// same condition through this value so `dispatch_step` can fail the task
/// closed identically either way (`AGENTS.md` §2.9, §2.17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackGuardViolation;

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

    /// Check this stack's overrun guard, if it has one.
    ///
    /// Returns [`StackGuardViolation`] if the task has run off the bottom of
    /// its usable stack into the guard region. `dispatch_step` calls this
    /// each time the task switches back to the dispatcher and fails the task
    /// closed on a violation (`AGENTS.md` §2.9, §2.17), so an overrun is
    /// caught at the next reschedule instead of silently corrupting the
    /// lower-addressed neighbour.
    ///
    /// The default returns `Ok(())`: a stack source without a guard (a
    /// slab-backed or static test stack) has nothing to check. [`BoxStack`]
    /// overrides it with the poison-canary check (`AGENTS.md` §4).
    fn check_guard(&self) -> Result<(), StackGuardViolation> {
        Ok(())
    }
}

/// Heap-backed kernel stack: the production [`KernelStack`] source.
///
/// The allocation is laid out, from low to high address, as a
/// `STACK_GUARD_BYTES` guard region followed by the [`KTHREAD_STACK_BYTES`]
/// usable stack; [`Self::top`] is the exclusive upper bound of the *usable*
/// region. A kernel stack grows *downward* from `top`, so an overrun runs
/// off the bottom of the usable region into the guard — which is
/// poison-filled and verified ([`Self::check_guard`], `AGENTS.md` §4) —
/// before it can reach the lower-addressed heap neighbour. The backing
/// `Box<[u8]>` has a stable address for the box's lifetime and is freed on
/// drop, reclaiming the stack.
pub struct BoxStack(Box<[u8]>);

/// The widest ABI stack alignment any target requires (`AGENTS.md`
/// §17.2); [`ContextSwitch::prepare`] rejects a misaligned `stack_top`.
const STACK_ALIGN: usize = 16;

/// The canary window must fit inside the guard region, and the guard is a
/// whole number of 4 KiB pages so the staged deployment form (unmapping it,
/// `plans/PI.md`) lands on a clean page boundary.
const _STACK_LAYOUT_OK: () = {
    assert!(STACK_GUARD_CANARY_BYTES <= STACK_GUARD_BYTES);
    assert!(STACK_GUARD_BYTES % 4096 == 0);
};

impl BoxStack {
    /// Allocate a fresh kernel stack on the heap: a poison-filled guard
    /// region below a zeroed usable stack.
    ///
    /// The backing slice is heap-allocated directly (`vec!` →
    /// `into_boxed_slice`), never built through a `[0u8; _]` stack temporary:
    /// a ~68 KiB array literal would itself risk the very stack overflow this
    /// type guards against (`AGENTS.md` §2.16). [`Self::top`] rounds the
    /// exclusive upper bound down to `STACK_ALIGN`, so the heap allocator's
    /// own (byte) alignment is sufficient.
    #[must_use]
    pub fn new() -> Self {
        let mut bytes =
            alloc::vec![0u8; STACK_GUARD_BYTES + KTHREAD_STACK_BYTES].into_boxed_slice();
        bytes[..STACK_GUARD_BYTES].fill(STACK_GUARD_BYTE);
        Self(bytes)
    }
}

impl Default for BoxStack {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `top` returns the heap slice's base plus its full length, rounded
// down to `STACK_ALIGN` — the 16-aligned exclusive upper bound of the usable
// region above the guard. The box owns the storage and frees it on drop, and
// the region is exclusive to its owner.
unsafe impl KernelStack for BoxStack {
    fn top(&self) -> u64 {
        let base = self.0.as_ptr() as u64;
        let top = base + (STACK_GUARD_BYTES + KTHREAD_STACK_BYTES) as u64;
        // Round down to `STACK_ALIGN` so the seed `stack_top`
        // [`ContextSwitch::prepare`] requires is aligned regardless of the
        // allocator's base alignment (it wastes at most `STACK_ALIGN - 1`
        // bytes off the top of the usable region).
        top & !(STACK_ALIGN as u64 - 1)
    }

    fn check_guard(&self) -> Result<(), StackGuardViolation> {
        // Verify the canary: the top `STACK_GUARD_CANARY_BYTES` of the guard,
        // immediately below the usable base, which a contiguous downward
        // overrun crosses first. Checking just this O(1) window keeps the
        // scheduler switch-back path cheap (`AGENTS.md` §2.16) while still
        // catching a stack overflow; the full guard page provides absorption.
        let canary = &self.0[STACK_GUARD_BYTES - STACK_GUARD_CANARY_BYTES..STACK_GUARD_BYTES];
        if canary.iter().all(|&b| b == STACK_GUARD_BYTE) {
            Ok(())
        } else {
            Err(StackGuardViolation)
        }
    }
}

// SAFETY: every method forwards to the boxed `KernelStack`, which upholds
// the trait contract (a mapped, writable, exclusive, `STACK_ALIGN`-aligned
// region whose `top` stays valid for the value's life). Boxing erases the
// concrete stack source — `BoxStack` (the software-canary form) or an
// arch-built arena stack whose guard page is unmapped in the task's own
// root — so an arch spawn seam can hand `kernel/core` a stack of either
// kind without the concrete type leaking into the admission generics
// (`AGENTS.md` §2.2 / §17.4). The box owns its payload and is `Send`, so the
// admitted task may run on any CPU.
unsafe impl KernelStack for Box<dyn KernelStack + Send> {
    fn top(&self) -> u64 {
        (**self).top()
    }

    fn check_guard(&self) -> Result<(), StackGuardViolation> {
        (**self).check_guard()
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

/// Upper bound on the number of CPUs the per-CPU EL0 resume table is
/// sized for.
///
/// `kernel/core` cannot name an architecture's `MAX_CPUS` (it is a
/// concrete-port constant, `AGENTS.md` §17.4); this is the core-owned
/// bound for the EL0-task resume seam. It is comfortably above every
/// Tier-1 port's own `MAX_CPUS` (x86_64 = 16, aarch64 = 8); a `cpu`
/// index at or beyond it makes [`reschedule_current`] fail closed rather
/// than index out of bounds (`AGENTS.md` §2.9, §5.4.5).
pub const KTHREAD_MAX_CPUS: usize = 64;

/// A published handle to the EL0 user kthread currently switched in on a
/// CPU, through which its syscall trap path suspends it back to the
/// scheduler ([`reschedule_current`]).
///
/// `data` is the address of the running task's `ThreadControl<C, S>` and
/// `thunk` is the `C, S`-monomorphised [`suspend_thunk`] that knows how to
/// reinterpret it; the pair is `Copy` so [`reschedule_current`] can lift it
/// out from under the per-CPU lock *before* performing the (suspending)
/// switch, never holding the lock across the hand-off.
#[derive(Copy, Clone)]
struct UserResumeHandle {
    data: usize,
    thunk: unsafe fn(usize, TaskAction),
}

/// Per-CPU EL0 resume table: slot `cpu` holds the handle for the user
/// kthread currently switched in on that CPU, or `None` when no user task
/// is running there.
///
/// [`dispatch_step`] publishes a slot immediately before switching into a
/// user kthread and clears it the instant the task switches back, so a slot
/// is `Some` exactly while that CPU is executing the task (in EL0 or in one
/// of its syscall traps). The arch trap path reaches it only through
/// [`reschedule_current`]. Each CPU touches only its own slot, so the
/// `SpinLock` never contends across CPUs — it is the minimum interior
/// mutability + memory-ordering primitive for the publish/observe, not a
/// contention point (`AGENTS.md` §2.3).
static USER_RESUME: [SpinLock<Option<UserResumeHandle>>; KTHREAD_MAX_CPUS] =
    [const { SpinLock::new(None) }; KTHREAD_MAX_CPUS];

/// Map the dispatch-callback ABI's [`RescheduleAction`] onto the
/// scheduler's own `TaskAction` at the one boundary that needs it
/// (`AGENTS.md` §2.2 — the two vocabularies meet here, nowhere else).
const fn to_task_action(action: RescheduleAction) -> TaskAction {
    match action {
        RescheduleAction::Yield => TaskAction::Yield,
        RescheduleAction::Park => TaskAction::Park,
        RescheduleAction::Exit => TaskAction::Exit,
    }
}

/// Suspend the `ThreadControl` at `data` with `action` and switch back to
/// its dispatcher, returning when the task is next resumed.
///
/// The `C, S`-monomorphised function pointer a [`UserResumeHandle`] carries:
/// it reconstructs the task's [`Yielder`] from the control block and reuses
/// [`Yielder::suspend`] so the switch-back invoke has exactly one definition
/// (`AGENTS.md` §2.2).
///
/// # Safety
///
/// `data` must be the address of the live, boxed `ThreadControl<C, S>` the
/// publishing [`dispatch_step`] passed, monomorphised over the *same* `C, S`.
/// The caller must run between that `dispatch_step`'s switch-into-task and
/// the task's switch-back — i.e. from the task's own syscall trap — so the
/// CPU exclusively owns the control block (the kthread raw-pointer protocol,
/// see the module docs).
unsafe fn suspend_thunk<C, S>(data: usize, action: TaskAction)
where
    C: ContextSwitch + Copy,
    S: KernelStack,
{
    let ctl = data as *mut ThreadControl<C, S>;
    // SAFETY: `ctl` is the live control block per this function's contract;
    // `cs` is `Copy`, and the three fields are distinct and live.
    let (cs, mut yielder) = unsafe {
        let cs = (*ctl).cs;
        let yielder = Yielder {
            cs,
            task_ctx: addr_of_mut!((*ctl).task_ctx),
            dispatch_ctx: addr_of_mut!((*ctl).dispatch_ctx),
            action: addr_of_mut!((*ctl).action),
        };
        (cs, yielder)
    };
    // Bracket the suspend with the port's cooperative-park hook so a port
    // that flips a per-CPU privilege-entry convention inside its syscall
    // handler (x86_64's entry `swapgs`) balances it across the park: this is
    // the user-kthread mid-handler park path (the syscall trap reaches it via
    // `reschedule_current`), the one place the imbalance arises (`plans/PI.md`
    // X2). The pair is a no-op on ports that need nothing (aarch64/riscv64).
    // SAFETY: we run on the parking user task's own syscall-handler control
    // flow (the kthread raw-pointer protocol, this function's contract); the
    // two calls bracket exactly one `Yielder::suspend`, so `enter`/`leave`
    // pair on this task. `Exit` never returns from `suspend`, leaving the CPU
    // in the balanced between-handler convention `enter` restored — correct,
    // since the task never resumes.
    unsafe {
        cs.enter_cooperative_park();
        yielder.suspend(action);
        cs.leave_cooperative_park();
    }
}

/// Suspend the EL0 user kthread currently switched in on `cpu` with
/// `action`, returning when the scheduler next dispatches it (never, for
/// [`RescheduleAction::Exit`]).
///
/// The bin-crate syscall-dispatch callback calls this on a
/// [`DispatchOutcome::Reschedule`](crate::DispatchOutcome::Reschedule): a
/// resumable user task that yielded, parked, or exited must be suspended
/// back to the scheduler rather than returned to immediately. The suspend
/// switches to the dispatcher's saved context; control returns here — and
/// then to the callback, which encodes the syscall result and resumes user
/// space — only when this task is dispatched again.
///
/// Returns `true` if a user kthread was running on `cpu` and was suspended;
/// `false` if no resume handle is published for `cpu`. A `false` is the
/// fail-closed signal that the caller was **not** a resumable user task (or
/// `cpu` is out of range): the callback then treats the syscall as an
/// ordinary return rather than perform an unsound switch (`AGENTS.md` §2.9,
/// §5.4.5).
#[must_use = "a false return means no user task was suspended; the caller must fall back to an ordinary syscall return"]
pub fn reschedule_current(cpu: CpuId, action: RescheduleAction) -> bool {
    let Ok(idx) = usize::try_from(cpu) else {
        return false;
    };
    let Some(slot) = USER_RESUME.get(idx) else {
        return false;
    };
    // Lift the handle out from under the lock and release it *before*
    // switching: the switch suspends this task, and holding the slot lock
    // across it would deadlock the dispatcher-side clear that runs when the
    // task resumes (`AGENTS.md` §2.1 — no lock held across a hand-off).
    let handle = *slot.lock();
    let Some(handle) = handle else {
        return false;
    };
    // SAFETY: a published handle's `data`/`thunk` were installed by
    // `dispatch_step` for the task currently switched in on this CPU,
    // monomorphised over the matching `C, S`; this call runs from that
    // task's syscall trap, so the control block is live and exclusively
    // owned (the kthread raw-pointer protocol).
    unsafe {
        (handle.thunk)(handle.data, to_task_action(action));
    }
    true
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
    /// Optional hook run on the dispatcher side immediately before each
    /// switch into the task (`plans/SPAWN.md` SP2).
    ///
    /// `Some` marks this as a **user** kthread: the hook reactivates the
    /// task's user address space (its arch page-table root) so the trap
    /// path `eret`s back into EL0 with the correct translation regime, and
    /// its presence is also what makes [`dispatch_step`] publish a
    /// [`UserResumeHandle`] for the trap path. A plain kernel kthread
    /// leaves this `None` and is never published. It runs on the
    /// dispatcher's context, where the kernel mapping is identical across
    /// every user space, so switching the user root mid-step is sound.
    pre_resume: Option<PreResume>,
}

/// A user kthread's pre-resume hook: see [`ThreadControl::pre_resume`].
///
/// The dispatcher passes the task's own kernel-stack top (the value
/// [`KernelStack::top`] returns for this task's stack) so a port whose
/// syscall entry does not implicitly land on the running task's kernel
/// stack can repoint its per-CPU entry stack at it before the switch-in.
/// aarch64 reuses `SP_EL1` implicitly and ignores the argument; x86_64
/// uses it to set the per-CPU `SyscallTls.kernel_rsp0` (`plans/PI.md` §X).
type PreResume = Box<dyn FnMut(u64) + Send + 'static>;

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
    spawn_control(scheduler, home_cpu, priority, cs, stack, work, None)
}

/// Admit a resumable **user** (EL0) kthread onto `scheduler`, giving it a
/// fresh heap-backed kernel stack ([`BoxStack`]).
///
/// Identical to [`spawn_kthread`] but carries a `pre_resume` hook the
/// dispatcher runs immediately before every switch into the task
/// (`plans/SPAWN.md` SP2). The hook reactivates the task's user address
/// space — its arch page-table root — so the task `eret`s back into EL0
/// under the correct translation regime and stays isolated from its
/// siblings (`AGENTS.md` §4). Its presence also enrols the task in the
/// per-CPU resume table ([`reschedule_current`]), so its syscall trap path
/// can suspend it back to the scheduler.
///
/// `work` typically diverges into EL0 via the arch `EnterUser` HAL; the
/// reschedule machinery brings control back to the dispatcher on each
/// rescheduling syscall.
///
/// # Errors
///
/// As [`spawn_kthread`].
pub fn spawn_user_kthread<C, A, P, R, W>(
    scheduler: &P,
    cs: C,
    home_cpu: CpuId,
    priority: Priority,
    pre_resume: R,
    work: W,
) -> SchedResult<TaskId>
where
    C: ContextSwitch + Copy + Send + 'static,
    A: SchedulerArch,
    P: SchedulerPolicy<A>,
    R: FnMut(u64) + Send + 'static,
    W: FnMut(&mut Yielder<C>) + Send + 'static,
{
    spawn_user_kthread_with_stack(
        scheduler,
        cs,
        BoxStack::new(),
        home_cpu,
        priority,
        pre_resume,
        work,
    )
}

/// Admit a resumable user (EL0) kthread onto `scheduler` over a
/// caller-supplied kernel stack `stack`.
///
/// The stack-owning counterpart of [`spawn_user_kthread`], in the same
/// relation [`spawn_kthread_with_stack`] holds to [`spawn_kthread`].
///
/// # Errors
///
/// As [`spawn_kthread`].
pub fn spawn_user_kthread_with_stack<C, A, P, S, R, W>(
    scheduler: &P,
    cs: C,
    stack: S,
    home_cpu: CpuId,
    priority: Priority,
    pre_resume: R,
    work: W,
) -> SchedResult<TaskId>
where
    C: ContextSwitch + Copy + Send + 'static,
    A: SchedulerArch,
    P: SchedulerPolicy<A>,
    S: KernelStack + Send + 'static,
    R: FnMut(u64) + Send + 'static,
    W: FnMut(&mut Yielder<C>) + Send + 'static,
{
    spawn_control(
        scheduler,
        home_cpu,
        priority,
        cs,
        stack,
        work,
        Some(Box::new(pre_resume)),
    )
}

/// Shared admission path for [`spawn_kthread_with_stack`] and
/// [`spawn_user_kthread_with_stack`]: build the boxed [`ThreadControl`]
/// (kernel or user, per `pre_resume`) and hand the scheduler the
/// owning shim closure (`AGENTS.md` §2.2 — one admission path).
fn spawn_control<C, A, P, S, W>(
    scheduler: &P,
    home_cpu: CpuId,
    priority: Priority,
    cs: C,
    stack: S,
    work: W,
    pre_resume: Option<PreResume>,
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
        pre_resume,
    });

    // The `move` closure owns the boxed control block, so its heap address
    // stays stable for the raw-pointer protocol; `&mut control` derefs to
    // the `&mut ThreadControl` the shim step takes. `step.cpu` keys the
    // per-CPU resume table for a user kthread.
    scheduler.spawn(home_cpu, priority, move |step| {
        dispatch_step(&mut control, step.cpu)
    })
}

/// Run one dispatch step of the kthread whose control block is `control`.
///
/// This is the shim's per-step logic, factored out so the host tests drive
/// it directly. It seeds the first frame on the first step, switches into
/// the task, and returns the [`TaskAction`] the task requested when it
/// switched back.
fn dispatch_step<C, S>(control: &mut ThreadControl<C, S>, cpu: CpuId) -> TaskAction
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

    // A user kthread (one with a `pre_resume` hook) reactivates its user
    // address space and publishes a resume handle so its syscall trap path
    // can suspend it back to us. Both run on the dispatcher's context,
    // where the kernel mapping is identical across every user space, so
    // switching the user root here is sound (`plans/SPAWN.md` SP2).
    // SAFETY: exclusive dispatcher-side access to `*ctl` (see above).
    let is_user = unsafe { (*ctl).pre_resume.is_some() };
    if is_user {
        // The task's own kernel-stack top: a port whose syscall entry does
        // not implicitly resume on the running task's kernel stack (x86_64)
        // repoints its per-CPU entry stack at this before the switch-in
        // (`plans/PI.md` §X). SAFETY: exclusive dispatcher-side access.
        let stack_top = unsafe { (*ctl).stack.top() };
        // SAFETY: `pre_resume` is `Some`; the field is exclusively ours
        // between switches, so the `&mut` borrow does not alias.
        if let Some(pre) = unsafe { (*ctl).pre_resume.as_mut() } {
            pre(stack_top);
        }
        publish_resume::<C, S>(cpu, ctl);
    }

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

    // The task switched back to us. Retire the resume handle immediately:
    // the task is no longer the one running on `cpu` (it yielded, parked,
    // or exited), so its trap path must no longer reach this control block.
    if is_user {
        clear_resume(cpu);
    }

    // The task ran on its kernel stack; verify it did not run off the bottom
    // into the guard region before we trust it again. A violation means a
    // stack overrun — on real hardware the unmapped guard page would already
    // have faulted; the software emulation catches it here. Fail the task
    // closed: mark it terminal and report `Exit` so the scheduler never
    // switches into its corrupted context again (`AGENTS.md` §2.9, §2.17).
    // SAFETY: exclusive dispatcher-side access to `*ctl` (see above).
    if unsafe { (*ctl).stack.check_guard() }.is_err() {
        unsafe {
            (*ctl).state = RunState::Finished;
        }
        return TaskAction::Exit;
    }

    // Report the action the task requested.
    unsafe { (*ctl).action }
}

/// Publish the per-CPU resume handle for the user kthread `ctl`, about to
/// be switched in on `cpu` (the dispatcher side of [`reschedule_current`]).
///
/// Out-of-range or unconfigured `cpu` is a silent no-op: the task simply
/// cannot be rescheduled from its trap and falls closed there, which is the
/// same outcome [`reschedule_current`] gives (`AGENTS.md` §2.9).
fn publish_resume<C, S>(cpu: CpuId, ctl: *mut ThreadControl<C, S>)
where
    C: ContextSwitch + Copy,
    S: KernelStack,
{
    if let Ok(idx) = usize::try_from(cpu) {
        if let Some(slot) = USER_RESUME.get(idx) {
            *slot.lock() = Some(UserResumeHandle {
                data: ctl as usize,
                thunk: suspend_thunk::<C, S>,
            });
        }
    }
}

/// Clear the per-CPU resume handle for `cpu` once its user kthread has
/// switched back to the dispatcher (the counterpart of [`publish_resume`]).
fn clear_resume(cpu: CpuId) {
    if let Ok(idx) = usize::try_from(cpu) {
        if let Some(slot) = USER_RESUME.get(idx) {
            *slot.lock() = None;
        }
    }
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
            pre_resume: None,
        })
    }

    /// Like [`control_with`] but a **user** kthread: it carries a
    /// `pre_resume` hook that increments `hits` on every switch-in, so a
    /// test can prove the hook fires and the resume handle is published.
    #[allow(clippy::unnecessary_box_returns)]
    fn user_control_with<C: ContextSwitch + Copy, S: KernelStack>(
        cs: C,
        stack: S,
        hits: &'static AtomicUsize,
    ) -> Box<ThreadControl<C, S>> {
        Box::new(ThreadControl {
            cs,
            task_ctx: TaskContext::empty(),
            dispatch_ctx: TaskContext::empty(),
            action: TaskAction::Yield,
            state: RunState::NotStarted,
            stack,
            work: Some(Box::new(|_y: &mut Yielder<C>| {})),
            pre_resume: Some(Box::new(move |_stack_top: u64| {
                hits.fetch_add(1, Ordering::SeqCst);
            })),
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

        let action = dispatch_step(&mut control, 0);

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

        let _ = dispatch_step(&mut control, 0);
        let _ = dispatch_step(&mut control, 0);

        // Prepare happens once; each step switches in.
        assert_eq!(rec.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(rec.switches.load(Ordering::SeqCst), 2);
        assert_eq!(control.state, RunState::Running);
    }

    #[test]
    fn failed_prepare_exits_without_switching() {
        let mut control = control_with(FailingCs, BoxStack::new());

        let action = dispatch_step(&mut control, 0);

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

        let action = dispatch_step(&mut control, 0);

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

    // --- EL0 reschedule seam (plans/SPAWN.md SP2) ----------------------
    //
    // Each test uses a distinct CPU index into the shared `USER_RESUME`
    // table so the parallel host test threads never collide, and clears
    // any handle it publishes before returning.

    /// Leak a fresh, zeroed counter with a `'static` lifetime, for a
    /// user kthread's `pre_resume` hook to tick.
    fn leak_counter() -> &'static AtomicUsize {
        StdBox::leak(StdBox::new(AtomicUsize::new(0)))
    }

    #[test]
    fn reschedule_current_without_a_published_handle_is_false() {
        // No user task is running on CPU 63, so the trap path is told to
        // fall back to an ordinary syscall return (fail closed, §2.9).
        assert!(!reschedule_current(63, RescheduleAction::Yield));
        assert!(!reschedule_current(63, RescheduleAction::Exit));
    }

    #[test]
    fn reschedule_current_out_of_range_cpu_is_false() {
        // A CPU index far beyond the table bound never indexes out of
        // bounds; it fails closed like an unpublished slot.
        assert!(KTHREAD_MAX_CPUS < CpuId::MAX as usize);
        assert!(!reschedule_current(CpuId::MAX, RescheduleAction::Yield));
    }

    #[test]
    fn reschedule_current_suspends_a_published_user_task() {
        let rec = recorder();
        let cs = RecordingCs(rec);
        let mut control = control_with(cs, BoxStack::new());
        let ctl: *mut ThreadControl<RecordingCs, BoxStack> = addr_of_mut!(*control);
        let cpu: CpuId = 60;

        // Model `dispatch_step`'s publish, then drive the trap-path entry
        // point directly. The handle's thunk reconstructs the task's
        // Yielder and suspends it: one switch, task_ctx -> dispatch_ctx,
        // with the requested action recorded.
        publish_resume::<RecordingCs, BoxStack>(cpu, ctl);
        assert!(reschedule_current(cpu, RescheduleAction::Exit));

        assert_eq!(rec.switches.load(Ordering::SeqCst), 1);
        assert_eq!(control.action, TaskAction::Exit);
        assert_eq!(
            rec.last_prev.load(Ordering::SeqCst),
            unsafe { addr_of_mut!((*ctl).task_ctx) } as u64
        );
        assert_eq!(rec.last_next.load(Ordering::SeqCst), unsafe {
            addr_of_mut!((*ctl).dispatch_ctx)
        } as u64);

        // After the dispatcher retires the handle, the slot is empty again.
        clear_resume(cpu);
        assert!(!reschedule_current(cpu, RescheduleAction::Yield));
    }

    #[test]
    fn reschedule_action_maps_onto_task_action() {
        assert_eq!(to_task_action(RescheduleAction::Yield), TaskAction::Yield);
        assert_eq!(to_task_action(RescheduleAction::Park), TaskAction::Park);
        assert_eq!(to_task_action(RescheduleAction::Exit), TaskAction::Exit);
    }

    #[test]
    fn user_dispatch_step_runs_pre_resume_and_publishes_then_clears() {
        let rec = recorder();
        let hits = leak_counter();
        let cpu: CpuId = 61;
        let mut control = user_control_with(RecordingCs(rec), BoxStack::new(), hits);

        // The host switch is a no-op that returns immediately, so a step
        // publishes the handle, runs `pre_resume`, switches in, and clears
        // the handle before returning.
        let _ = dispatch_step(&mut control, cpu);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Handle retired: nothing to reschedule on this CPU now.
        assert!(!reschedule_current(cpu, RescheduleAction::Yield));

        // `pre_resume` runs again on the next switch-in (every step
        // reactivates the user address space).
        let _ = dispatch_step(&mut control, cpu);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn kernel_dispatch_step_never_publishes_a_handle() {
        let rec = recorder();
        let cpu: CpuId = 62;
        // A plain kernel kthread (no `pre_resume`) is never enrolled in the
        // resume table, so its CPU stays unschedulable from a trap path.
        let mut control = control_with(RecordingCs(rec), BoxStack::new());
        let _ = dispatch_step(&mut control, cpu);
        assert!(!reschedule_current(cpu, RescheduleAction::Yield));
    }

    // --- Stack guard page (AGENTS.md §4 / §2.17) -----------------------

    /// A guardless [`KernelStack`] host double, to prove the default
    /// [`KernelStack::check_guard`] is vacuously `Ok`. The host `switch` is a
    /// no-op, so its `top` is never dereferenced.
    #[derive(Copy, Clone)]
    struct GuardlessStack;

    // SAFETY: a host test double whose `top` is a plausible 16-aligned value;
    // the host `ContextSwitch::switch` never transfers control, so nothing
    // executes on this stack.
    unsafe impl KernelStack for GuardlessStack {
        fn top(&self) -> u64 {
            0x1_0000
        }
    }

    /// A [`KernelStack`] over a real [`BoxStack`] whose guard check can be
    /// forced to report a violation, to drive [`dispatch_step`]'s fail-closed
    /// path without an actual (host-impossible) stack overrun.
    struct GuardDouble {
        inner: BoxStack,
        violated: bool,
    }

    // SAFETY: `top` delegates to a real, owned, aligned `BoxStack` region;
    // `check_guard` reports a violation on demand. The host `switch` is a
    // no-op, so nothing executes on the stack.
    unsafe impl KernelStack for GuardDouble {
        fn top(&self) -> u64 {
            self.inner.top()
        }

        fn check_guard(&self) -> Result<(), StackGuardViolation> {
            if self.violated {
                Err(StackGuardViolation)
            } else {
                self.inner.check_guard()
            }
        }
    }

    #[test]
    fn box_stack_guard_is_poisoned_and_usable_top_sits_above_it() {
        let stack = BoxStack::new();
        let base = stack.0.as_ptr() as u64;

        // The guard region (low) is poison-filled and the usable region
        // (high) is zeroed; `top` is the exclusive upper bound of the usable
        // region, above the guard.
        assert!(stack.0[..STACK_GUARD_BYTES]
            .iter()
            .all(|&b| b == STACK_GUARD_BYTE));
        assert!(stack.0[STACK_GUARD_BYTES..].iter().all(|&b| b == 0));
        // The allocator's base is byte-aligned, so the usable top is the
        // (16-aligned) round-down of base + total.
        assert_eq!(
            stack.top(),
            (base + (STACK_GUARD_BYTES + KTHREAD_STACK_BYTES) as u64) & !(STACK_ALIGN as u64 - 1)
        );
        assert!(stack.check_guard().is_ok());
    }

    #[test]
    fn box_stack_check_guard_detects_an_overrun_at_the_usable_base() {
        // The topmost guard byte sits immediately below the usable base — the
        // first byte a contiguous downward overrun crosses.
        let mut stack = BoxStack::new();
        stack.0[STACK_GUARD_BYTES - 1] = 0;
        assert_eq!(stack.check_guard(), Err(StackGuardViolation));
    }

    #[test]
    fn box_stack_check_guard_detects_an_overrun_at_the_canary_floor() {
        // The deepest byte the canary covers is still detected.
        let mut stack = BoxStack::new();
        stack.0[STACK_GUARD_BYTES - STACK_GUARD_CANARY_BYTES] = 0;
        assert_eq!(stack.check_guard(), Err(StackGuardViolation));
    }

    #[test]
    fn default_check_guard_is_ok_for_a_guardless_stack() {
        assert!(GuardlessStack.check_guard().is_ok());
    }

    #[test]
    fn dispatch_step_fails_closed_on_a_guard_violation() {
        let rec = recorder();
        let stack = GuardDouble {
            inner: BoxStack::new(),
            violated: true,
        };
        let mut control = control_with(RecordingCs(rec), stack);

        // The first step prepares the frame and switches in (a host no-op),
        // then the switch-back guard check trips: the task is failed closed
        // (terminal + `Exit`) rather than trusted on a corrupt stack.
        assert_eq!(dispatch_step(&mut control, 0), TaskAction::Exit);
        assert_eq!(control.state, RunState::Finished);

        // It stays terminal and is never switched into again.
        let before = rec.switches.load(Ordering::SeqCst);
        assert_eq!(dispatch_step(&mut control, 0), TaskAction::Exit);
        assert_eq!(rec.switches.load(Ordering::SeqCst), before);
    }

    #[test]
    fn dispatch_step_reports_the_action_when_the_guard_is_intact() {
        let rec = recorder();
        let stack = GuardDouble {
            inner: BoxStack::new(),
            violated: false,
        };
        let mut control = control_with(RecordingCs(rec), stack);

        // With the guard intact the shim reports the task's requested action
        // (the default `Yield`) and the task stays runnable.
        assert_eq!(dispatch_step(&mut control, 0), TaskAction::Yield);
        assert_eq!(control.state, RunState::Running);
    }
}
