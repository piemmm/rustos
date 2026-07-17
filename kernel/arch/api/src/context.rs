//! Context-switch surface of the Arch HAL ("context
//! switch").
//!
//! Suspending the running task and resuming another on a CPU is a
//! privilege-neutral but deeply architecture-specific operation: it
//! saves the outgoing task's callee-saved registers onto its kernel
//! stack, records the resulting stack pointer, loads the inbound task's
//! stack pointer, and restores its registers with the port's native
//! prologue/epilogue assembly. The charter makes the architecture surface a
//! closed set of traits on the HAL; this module is the "context switch"
//! member of that set, so the per-task save area and the switch
//! primitive live behind one vocabulary instead of being re-described
//! at every call site. The parallel per-arch
//! implementations of this one trait are the deliberate shape of
//! modularity, never collapsed behind `cfg` (carve-out).
//!
//! # What lives here
//!
//! * [`TaskContext`] — the architecture-neutral per-task save area the
//!   scheduler parks in its task table. Every bare-metal port persists
//!   exactly one word across a switch — the kernel-stack pointer at
//!   suspension — and keeps the callee-saved registers *on the stack
//!   itself* in a fixed frame owned by the switch assembly. So the
//!   neutral save area is a single `#[repr(C)]` `u64`, layout-identical
//!   to each port's native `TaskCtx` (one definition).
//! * [`TaskEntry`] — the entry point a freshly prepared task first runs.
//!   A plain `unsafe extern "C" fn(usize) -> !` (not a closure): it is
//!   reached via the port's resume assembly, which has no Rust frame to
//!   drop a captured environment in, and a task body never returns to
//!   its synthesised frame.
//! * [`PrepareError`] — the fail-closed result of seeding a task's
//!   initial frame ([`ContextSwitch::prepare`]). A bad stack is rejected,
//!   never silently truncated.
//! * [`ContextSwitch`] — the per-port handle the kernel reaches through.
//!   It seeds a never-run task's first frame ([`ContextSwitch::prepare`],
//!   host-testable pointer/layout math) and performs the bare-metal
//!   switch ([`ContextSwitch::switch`], the port's assembly).
//! * [`conformance`] — the conformance vertical: a host-run
//!   [`conformance::run_all`] check every bare-metal port runs over its
//!   [`ContextSwitch`] handle, proving the `prepare` contract (an empty
//!   context is not runnable, a bad stack is rejected fail-closed, and a
//!   good stack yields a runnable in-bounds frame).
//!
//! # Why `prepare` is host-tested but `switch` is not
//!
//! [`ContextSwitch::prepare`] is pure pointer/layout arithmetic over a
//! caller-supplied stack buffer, so it runs and is asserted on the host
//! exactly like the [`crate::timer::conformance`] vertical. The switch
//! itself is only meaningful on the bare-metal target — it returns into
//! a *different* task's stack and cannot be observed from `cargo test` —
//! so, like [`crate::EnterUser::enter_user`], it carries no host
//! conformance check; it is proven end-to-end by each port's QEMU
//! scheduler-drive vertical (a real task switch round-trips). Inventing
//! a host stub that "switches" would be a fake primitive.

/// The entry point a freshly prepared [`TaskContext`] first runs.
///
/// `unsafe extern "C" fn(usize) -> !` rather than a closure: the port's
/// resume assembly jumps to it with the first-argument register set to
/// the task's argument, and a task body never returns to its synthesised
/// frame, so there is no captured environment to drop and nothing for
/// the frame's return address to land on.
pub type TaskEntry = unsafe extern "C" fn(usize) -> !;

/// The architecture-neutral per-task register-save area.
///
/// One [`TaskContext`] per scheduler task. The only persisted field is
/// the kernel-stack pointer at the moment the task was last suspended;
/// the callee-saved registers live on the task's own stack in a fixed
/// frame the port's switch assembly owns. The layout is identical on
/// every bare-metal port (a single `#[repr(C)]` `u64`), so the neutral
/// type *is* the port's `TaskCtx` rather than a parallel definition.
///
/// A freshly constructed [`TaskContext`] has [`stack_pointer`] zero and
/// is **not runnable** ([`Self::is_runnable`]): callers must seed an
/// initial frame with [`ContextSwitch::prepare`] before the first
/// switch into it.
///
/// [`stack_pointer`]: Self::stack_pointer
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskContext {
    /// Kernel-stack pointer at suspension. Read by the resume half of
    /// [`ContextSwitch::switch`]; written by the suspend half and by
    /// [`ContextSwitch::prepare`]. Zero on a never-prepared task.
    pub stack_pointer: u64,
}

impl TaskContext {
    /// Build an empty context. [`Self::stack_pointer`] is zero; the task
    /// is not runnable until [`ContextSwitch::prepare`] seeds a frame.
    #[must_use]
    pub const fn empty() -> Self {
        Self { stack_pointer: 0 }
    }

    /// `true` once an initial frame has been seeded (a non-zero stack
    /// pointer). A switch into a non-runnable context is a kernel bug
    /// the port's `switch` contract forbids.
    #[must_use]
    pub const fn is_runnable(&self) -> bool {
        self.stack_pointer != 0
    }
}

/// The fail-closed result of seeding a task's first frame
/// ([`ContextSwitch::prepare`]).
///
/// A stack that cannot hold a valid initial frame is rejected, never
/// silently truncated or wrapped. The variants
/// are the architecture-neutral union every port reports; a port maps
/// its primitive's error onto them at the HAL boundary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PrepareError {
    /// `stack_top` was zero — there is no stack to seed a frame in.
    NullStack,
    /// `stack_top` was not aligned to the port's ABI stack alignment.
    Misaligned,
    /// `stack_top` had no room for the port's initial frame.
    TooSmall,
}

/// The context-switch handle an architecture port exposes.
///
/// The kernel seeds a never-run task's first frame once with
/// [`Self::prepare`], then suspends/resumes tasks with [`Self::switch`]
/// at every yield/preemption point. Both operate on the neutral
/// [`TaskContext`]; the port forwards them to its native `TaskCtx` save
/// area and switch assembly (the layouts are identical, so the forward
/// is a reinterpretation, not a copy).
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU's scheduler path. A port's handle is typically
/// zero-sized — the per-task state lives in the [`TaskContext`] the
/// caller owns, not in the handle — exactly like the [`crate::Timer`]
/// and [`crate::EnterUser`] handles.
pub trait ContextSwitch: Send + Sync {
    /// Seed `ctx`'s initial frame so the first [`Self::switch`] *into* it
    /// lands at `entry` with the first-argument register set to `arg`.
    ///
    /// `stack_top` is the *exclusive* upper bound of the task's kernel
    /// stack (one byte past the last addressable byte). On success
    /// `ctx.stack_pointer` points at the bottom of the synthesised frame
    /// and [`TaskContext::is_runnable`] becomes `true`.
    ///
    /// # Errors
    ///
    /// Returns a [`PrepareError`] (and leaves `ctx` unchanged) if
    /// `stack_top` is zero, misaligned for the port's ABI, or too small
    /// to hold the port's initial frame. The port fails closed rather
    /// than seed a corrupt frame.
    fn prepare(
        &self,
        ctx: &mut TaskContext,
        stack_top: u64,
        entry: TaskEntry,
        arg: usize,
    ) -> Result<(), PrepareError>;

    /// Switch from `prev` to `next` on the calling CPU.
    ///
    /// Saves the running task's callee-saved registers onto its stack,
    /// records the resulting stack pointer in `*prev`, loads
    /// `(*next).stack_pointer`, and restores the inbound task's
    /// registers. Control returns to the call site of the inbound task's
    /// previous switch — or, for a never-run task seeded by
    /// [`Self::prepare`], to that task's `entry`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that:
    ///
    /// * `prev` and `next` are non-null, aligned [`TaskContext`] pointers
    ///   the kernel owns;
    /// * `prev` belongs to the currently-running task and is exclusive to
    ///   this CPU;
    /// * `(*next)` is runnable ([`TaskContext::is_runnable`]) — its
    ///   `stack_pointer` is a value [`Self::prepare`] or a prior
    ///   [`Self::switch`] wrote for the inbound task;
    /// * the inbound kernel stack is mapped and exclusive to this CPU.
    ///
    /// Switching into a non-runnable `next` is a kernel bug.
    unsafe fn switch(&self, prev: *mut TaskContext, next: *mut TaskContext);

    /// Restore this CPU's *between-handler* privilege-entry convention
    /// immediately **before** a user task cooperatively parks
    /// mid-syscall-handler (the [`Self::leave_cooperative_park`] partner
    /// runs the instant the task is switched back in).
    ///
    /// A resumable user kthread can suspend itself from inside its own
    /// syscall handler (a `yield`/`wait` that reschedules), so the kernel
    /// switches *away* from a CPU that is mid-handler and may later switch a
    /// *different* task in. On ports whose syscall entry leaves a per-CPU
    /// register convention flipped for the duration of the handler — x86_64,
    /// where `swapgs` makes `%gs` the kernel TLS only between entry and exit
    /// — that flipped state must be balanced back to the convention the
    /// dispatcher and the next user-entry path expect before the park, or the
    /// next ring-3 entry of another task would observe an unbalanced `swapgs`
    /// and fault (`plans/PI.md` X2). This pair brackets exactly the park so
    /// the two `swapgs` always pair on the *same* task's control flow.
    ///
    /// The default is a no-op: ports with no such per-handler convention
    /// (aarch64 saves `SP_EL0`/`ELR_EL1`/`SPSR_EL1` in the trap frame;
    /// riscv64 has no cooperative mid-handler park yet) need nothing here.
    /// Only the cooperative-park path
    /// ([`reschedule_current`](../../tairix_kernel_core/index.html)) calls it;
    /// the first trampoline→user entry never does, so it stays balanced.
    ///
    /// # Safety
    ///
    /// Must be called only from the running user task's own syscall-handler
    /// control flow, exactly once before each cooperative park, paired with
    /// [`Self::leave_cooperative_park`] on resume. Calling it elsewhere would
    /// leave the CPU's privilege-entry convention unbalanced.
    unsafe fn enter_cooperative_park(&self) {}

    /// Re-establish this CPU's *in-handler* privilege-entry convention
    /// immediately **after** a parked user task is switched back in, undoing
    /// [`Self::enter_cooperative_park`].
    ///
    /// The default is a no-op. See [`Self::enter_cooperative_park`] for the
    /// full contract; this is its exact inverse and the two must be paired.
    ///
    /// # Safety
    ///
    /// Must be called only on resume from a cooperative park, paired with a
    /// prior [`Self::enter_cooperative_park`] on the same task's control flow.
    unsafe fn leave_cooperative_park(&self) {}
}

/// The context-switch conformance vertical.
///
/// Every bare-metal architecture port runs [`conformance::run_all`]
/// against its [`ContextSwitch`] handle. The suite is portable — it
/// names only the trait — and runs on the host, exactly like the sibling
/// [`crate::timer::conformance`] and [`crate::percpu::conformance`]
/// verticals. It exercises only [`ContextSwitch::prepare`] (pure
/// pointer/layout math); [`ContextSwitch::switch`] is proven by each
/// port's QEMU scheduler-drive vertical (see the module docs).
///
/// It is driven per port (not folded into [`crate::conformance::run_all`])
/// because the suite seeds a frame into a caller-supplied stack and runs
/// over the port's real handle in that port's crate, the same precedent
/// as [`crate::irq::conformance`] and [`crate::timer::conformance`].
pub mod conformance {
    use super::{ContextSwitch, PrepareError, TaskContext, TaskEntry};

    /// A divergent host entry used only for its address. The conformance
    /// suite never switches into a prepared frame on the host (that is
    /// the bare-metal-only operation), so this is never invoked; it
    /// exists so [`ContextSwitch::prepare`] has a valid [`TaskEntry`] to
    /// encode. The body diverges via `panic!` rather than `loop {}` so
    /// clippy's `empty_loop` lint does not fire on the host build.
    unsafe extern "C" fn probe_entry(_arg: usize) -> ! {
        panic!("probe_entry is address-only; never invoked by the conformance suite")
    }

    /// A stack buffer large enough for any port's initial frame, aligned
    /// to the widest ABI stack alignment the targets require (16 bytes).
    /// Sized at 512 bytes — comfortably above every port's frame — so the
    /// success case has a valid, in-bounds top to seed.
    #[repr(C, align(16))]
    struct ConformanceStack([u8; 512]);

    /// Run the entire [`ContextSwitch`] conformance suite against `cs`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if an empty context reports runnable, if
    /// a null/misaligned/too-small stack is *not* rejected fail-closed,
    /// or if a good stack does not yield a runnable, in-bounds frame.
    pub fn run_all<C: ContextSwitch + ?Sized>(cs: &C) {
        empty_context_is_not_runnable();
        rejects_null_stack(cs);
        rejects_misaligned_stack(cs);
        rejects_too_small_stack(cs);
        prepares_a_runnable_in_bounds_frame(cs);
    }

    /// A freshly built context carries a zero stack pointer and is not
    /// runnable until a frame is seeded.
    fn empty_context_is_not_runnable() {
        let ctx = TaskContext::empty();
        assert_eq!(ctx.stack_pointer, 0, "an empty context must be zeroed");
        assert!(
            !ctx.is_runnable(),
            "an empty context must not be runnable before prepare"
        );
        assert_eq!(
            TaskContext::default(),
            TaskContext::empty(),
            "the derived default must agree with empty()"
        );
    }

    /// A zero `stack_top` is rejected, and the context is left untouched.
    fn rejects_null_stack<C: ContextSwitch + ?Sized>(cs: &C) {
        let mut ctx = TaskContext::empty();
        let entry: TaskEntry = probe_entry;
        assert_eq!(
            cs.prepare(&mut ctx, 0, entry, 0),
            Err(PrepareError::NullStack),
            "a null stack_top must be rejected"
        );
        assert!(
            !ctx.is_runnable(),
            "a rejected prepare must leave the context non-runnable"
        );
    }

    /// A `stack_top` that is not 16-byte aligned is rejected. Every
    /// target's ABI requires at least 16-byte stack alignment, so an odd
    /// value is misaligned on all of them.
    fn rejects_misaligned_stack<C: ContextSwitch + ?Sized>(cs: &C) {
        let mut ctx = TaskContext::empty();
        let entry: TaskEntry = probe_entry;
        assert_eq!(
            cs.prepare(&mut ctx, 0x1_0001, entry, 0),
            Err(PrepareError::Misaligned),
            "a misaligned stack_top must be rejected"
        );
        assert!(!ctx.is_runnable());
    }

    /// A `stack_top` that is aligned and non-zero but far too small to
    /// hold any port's initial frame is rejected. Sixteen bytes is below
    /// every port's frame size yet 16-byte aligned and non-zero.
    fn rejects_too_small_stack<C: ContextSwitch + ?Sized>(cs: &C) {
        let mut ctx = TaskContext::empty();
        let entry: TaskEntry = probe_entry;
        assert_eq!(
            cs.prepare(&mut ctx, 0x10, entry, 0),
            Err(PrepareError::TooSmall),
            "a too-small stack_top must be rejected"
        );
        assert!(!ctx.is_runnable());
    }

    /// A good `stack_top` yields a runnable context whose seeded stack
    /// pointer lies strictly inside the supplied stack (below the top,
    /// at or above the base).
    fn prepares_a_runnable_in_bounds_frame<C: ContextSwitch + ?Sized>(cs: &C) {
        let mut stack = ConformanceStack([0; 512]);
        let base = core::ptr::addr_of_mut!(stack.0) as u64;
        // The buffer is 16-byte aligned and 512 bytes long, so `top` is
        // 16-byte aligned and there is room for any port's frame.
        let top = base + 512;
        let mut ctx = TaskContext::empty();
        let entry: TaskEntry = probe_entry;
        cs.prepare(&mut ctx, top, entry, 0x00C0_FFEE)
            .expect("a 512-byte aligned stack must seed a frame");
        assert!(ctx.is_runnable(), "a prepared context must be runnable");
        assert!(
            ctx.stack_pointer < top,
            "the seeded stack pointer must lie below the stack top"
        );
        assert!(
            ctx.stack_pointer >= base,
            "the seeded stack pointer must lie within the supplied stack"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{ContextSwitch, PrepareError, TaskContext, TaskEntry};
        use super::run_all;

        /// A faithful host double: it implements `prepare` with the same
        /// fail-closed contract a real port owes and seeds a plausible
        /// frame. `switch` is never exercised on the host, so its body is
        /// empty (the suite calls only `prepare`).
        struct CellContextSwitch;

        /// A frame size below the 512-byte conformance stack but above the
        /// 16-byte too-small probe.
        const DOUBLE_FRAME: u64 = 64;

        impl ContextSwitch for CellContextSwitch {
            fn prepare(
                &self,
                ctx: &mut TaskContext,
                stack_top: u64,
                _entry: TaskEntry,
                _arg: usize,
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
                ctx.stack_pointer = stack_top - DOUBLE_FRAME;
                Ok(())
            }

            unsafe fn switch(&self, _prev: *mut TaskContext, _next: *mut TaskContext) {}
        }

        #[test]
        fn suite_accepts_a_faithful_context_switch() {
            let cs = CellContextSwitch;
            run_all(&cs);
            let dynamic: &dyn ContextSwitch = &cs;
            run_all(dynamic);
        }

        /// A broken `prepare` that accepts a null stack must be rejected
        /// by the fail-closed check.
        struct LenientContextSwitch;

        impl ContextSwitch for LenientContextSwitch {
            fn prepare(
                &self,
                ctx: &mut TaskContext,
                stack_top: u64,
                _entry: TaskEntry,
                _arg: usize,
            ) -> Result<(), PrepareError> {
                // Bug: never validates the stack.
                ctx.stack_pointer = stack_top.wrapping_sub(DOUBLE_FRAME);
                Ok(())
            }

            unsafe fn switch(&self, _prev: *mut TaskContext, _next: *mut TaskContext) {}
        }

        #[test]
        #[should_panic(expected = "a null stack_top must be rejected")]
        fn suite_rejects_a_context_switch_that_accepts_a_null_stack() {
            run_all(&LenientContextSwitch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_context_layout_is_one_word() {
        assert_eq!(core::mem::size_of::<TaskContext>(), 8);
        assert_eq!(core::mem::align_of::<TaskContext>(), 8);
        assert_eq!(core::mem::offset_of!(TaskContext, stack_pointer), 0);
    }

    #[test]
    fn empty_context_is_zero_and_not_runnable() {
        let ctx = TaskContext::empty();
        assert_eq!(ctx.stack_pointer, 0);
        assert!(!ctx.is_runnable());
        let runnable = TaskContext {
            stack_pointer: 0x8000,
        };
        assert!(runnable.is_runnable());
    }
}
