//! Threads within a process: the mechanism behind `thread_create` /
//! `thread_exit` (`plans/THREADS.md` T3b).
//!
//! A process is a thread group named by its leader's `TaskId` (decision 1).
//! Every thread is a scheduler task of its own with its own kernel stack, user
//! stack, and thread pointer, and they all share one address space, one
//! capability record, one descriptor table, and one set of resource limits —
//! which is what makes a `cap_revoke` by one thread bind its siblings rather
//! than leaving a stale copy behind.
//!
//! # What creating a thread costs, and who owns it
//!
//! The kernel reserves the thread's user stack itself (decision 5a), out of the
//! process's own anonymous window and with an **unreserved guard page below
//! it**, so a stack overrun takes a deterministic fault instead of quietly
//! walking into a neighbouring mapping. The reservation is released when the
//! thread dies, which is only safe from the kernel side: the thread runs on
//! that stack right up to the syscall that ends it, so nothing in user space
//! could unmap it, and a *detached* thread has nobody watching its death.
//!
//! Its kernel stack comes from the same guarded arena a process's first thread
//! uses, and its guard page is re-expressed as unmapped in the process's live
//! root here — the first thread has that done while its root is still inactive,
//! during the image build, and a thread created later has no such moment.
//!
//! # Ordering
//!
//! Creation follows the born-parked discipline `spawn` established: every
//! bound is checked and every input validated before any state changes, the
//! task is admitted **parked**, its per-thread state is installed under the
//! returned id, and only then is it unparked — so no CPU can dispatch a thread
//! before the kernel knows what it is.

use alloc::sync::Arc;

use tairix_abi::{Errno, LimitKind, RLIMIT_INFINITY};
use tairix_arch_api::UserEntry;
use tairix_kernel_mem::PAGE_SIZE;
use tairix_kernel_sched_api::{Priority, SchedulerArch};
use tairix_kernel_sec::{TaskId as SecTaskId, ThreadRegisterError};
use tairix_kernel_syscall::{CallerContext, SyscallResult};

use crate::aspace::{OwnedThreadStack, StackSpan};
use crate::bootinfo::KernelArch;
use crate::procspace::ProcessSpace;
use crate::rlimit::DEFAULT_STACK_LIMIT_BYTES;
use crate::syscalls::KernelSyscallHandlers;

/// Pages of a freshly created thread's stack backed eagerly, at its top.
///
/// The span the growth-fault path bounds must name at least one committed
/// page, and the thread's very first instruction may push, so one page is
/// backed at creation and every page below it faults in on demand exactly as
/// the process's first thread's stack does. This is an eager-commit *depth*,
/// not a capacity: the capacity is the thread's whole stack length, which is
/// derived from the process's `stack-bytes` bound.
const THREAD_STACK_COMMIT_PAGES: u64 = 1;

/// A validated `thread_create` request: the register state the new thread
/// starts with, plus the words the kernel must honour on its behalf.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ThreadRequest {
    /// User virtual address the thread begins executing at.
    pub entry: u64,
    /// Value placed in the thread's first-argument register.
    pub arg: u64,
    /// Bytes of user stack to reserve for it (already resolved from
    /// `THREAD_STACK_DEFAULT` and bounded by the `stack-bytes` limit).
    pub stack_bytes: u64,
    /// The thread's initial thread pointer, `0` for none.
    pub tls_base: u64,
    /// Word the kernel zeroes and futex-wakes when the thread dies, `0` for
    /// none.
    pub clear_on_exit: u64,
}

/// Resolve the `stack_len` a caller asked for into the byte length the kernel
/// will reserve, or refuse the request.
///
/// [`tairix_abi::THREAD_STACK_DEFAULT`] asks for the caller's own effective
/// `stack-bytes` soft bound — the same policy that sizes the process's first
/// thread — so the default lives in exactly one place and a tightened `ulimit`
/// applies to threads with no second definition of the number. A bound of
/// [`RLIMIT_INFINITY`] imposes no ceiling and therefore names no size, so the
/// default policy's own floor stands in for it.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] for a request that rounds to nothing, and
/// [`Errno::OutOfRange`] for one past the effective `stack-bytes` soft bound or
/// one that overflows on page rounding (fail closed — a thread never gets a
/// stack the process's own limit forbids).
pub fn resolve_stack_bytes(requested: usize, stack_soft: u64) -> Result<u64, Errno> {
    let page = PAGE_SIZE as u64;
    let asked = if requested == tairix_abi::THREAD_STACK_DEFAULT {
        if stack_soft == RLIMIT_INFINITY {
            DEFAULT_STACK_LIMIT_BYTES
        } else {
            stack_soft
        }
    } else {
        requested as u64
    };
    let rounded = asked
        .div_ceil(page)
        .checked_mul(page)
        .ok_or(Errno::OutOfRange)?;
    if rounded == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    if rounded > stack_soft {
        return Err(Errno::OutOfRange);
    }
    Ok(rounded)
}

/// The `[guard | stack]` reservation and the growth span a `stack_bytes`-byte
/// thread stack based at `reserve_base` describes.
///
/// The guard page is the reservation's **lowest** page and is deliberately left
/// out of the returned span: nothing records it as backable, so a fault there
/// resolves through neither the stack-growth nor the anonymous handler and
/// stays fatal. That is the structural overrun defence, not a canary.
///
/// Returns [`None`] when the arithmetic would overflow the address space or the
/// resulting span is malformed — the caller then refuses the thread.
#[must_use]
pub fn thread_stack_span(reserve_base: u64, stack_bytes: u64) -> Option<StackSpan> {
    let page = PAGE_SIZE as u64;
    let stack_base = reserve_base.checked_add(page)?;
    let top = stack_base.checked_add(stack_bytes)?;
    let committed_base = top.checked_sub(THREAD_STACK_COMMIT_PAGES * page)?;
    if committed_base < stack_base {
        return None;
    }
    StackSpan::new(stack_base, committed_base, top)
}

/// Pages a `stack_bytes`-byte thread stack reserves, guard page included.
#[must_use]
pub fn thread_reserve_pages(stack_bytes: u64) -> u64 {
    stack_bytes / PAGE_SIZE as u64 + 1
}

/// Create a thread of the caller's own process, returning its thread id.
///
/// Every bound and every input is checked before the first state change, the
/// task is admitted parked, and it is unparked only once its capability alias,
/// stack span, and owned-stack record exist. Any failure leaves the process
/// exactly as it was.
///
/// # Errors
///
/// * [`Errno::NotImplemented`] — the build wired no live address space, frame
///   allocator, or image builder, so no stack can be reserved (fail closed).
/// * [`Errno::OutOfRange`] — the process is at its `threads` bound, or the
///   requested stack exceeds its `stack-bytes` bound.
/// * [`Errno::BadAddress`] — `entry`, `tls_base`, or `clear_on_exit` does not
///   name readable memory of the caller's own address space.
/// * [`Errno::OutOfMemory`] — the anonymous window, the frame allocator, or the
///   scheduler's run queue cannot admit the thread (deterministic exhaustion,
///   never a panic).
pub fn create<A>(
    handlers: &KernelSyscallHandlers<'_, A>,
    caller: &CallerContext<'_>,
    request: ThreadRequest,
) -> SyscallResult
where
    A: KernelArch + 'static,
{
    let process = caller.process();
    let cpu = SchedulerArch::current_cpu(handlers.arch);

    // The process's shared execution context: its live space (to reserve the
    // stack in), its switch-in hook, and its port's enter-user handle. A build
    // that retained none can neither reserve a stack nor resume a thread, so it
    // fails closed rather than admitting a thread that could never run.
    let space = crate::kthread::current_process_space(cpu).ok_or(Errno::NotImplemented)?;
    let frames = handlers.frames.ok_or(Errno::NotImplemented)?;
    let services =
        crate::spawn_services::installed_spawn_services().ok_or(Errno::NotImplemented)?;

    // Capacity before state: the thread bound, then the stack bound.
    let live_threads = handlers.caps.read().thread_count(process) as u64;
    let limits = handlers.aspaces.read().limits(process);
    if live_threads >= limits.get(LimitKind::Threads).soft {
        return Err(Errno::OutOfRange);
    }
    let stack_bytes = request.stack_bytes;

    // The thread's kernel stack, and its guard page re-expressed as unmapped in
    // the process's live root so an overrun of it faults instead of corrupting
    // the neighbouring arena region. A guard the port cannot express fails the
    // creation closed — a thread never runs on an unguarded kernel stack.
    let (kernel_stack, kernel_guard) = services
        .image_builder()
        .alloc_kernel_stack(frames, handlers.page_table_frames);
    if let Some(guard) = kernel_guard {
        space
            .with(|live| live.unmap_kernel_stack_guard(guard))
            .map_err(|_| Errno::NoSpace)?;
    }

    // Reserve `[guard | stack]` in the process's own anonymous window. Nothing
    // is backed yet except the one page below the top, which the span names as
    // committed so the thread's first push lands on real memory.
    let reserve_pages = thread_reserve_pages(stack_bytes);
    let reserve_base = space
        .with(|live| live.reserve_anonymous(reserve_pages))
        .map_err(|_| Errno::OutOfMemory)?;
    let Some(span) = thread_stack_span(reserve_base, stack_bytes) else {
        release_reservation(&space, reserve_base, reserve_pages);
        return Err(Errno::OutOfRange);
    };
    if space
        .with(|live| live.map_anonymous(span.committed_base(), THREAD_STACK_COMMIT_PAGES))
        .is_err()
    {
        release_reservation(&space, reserve_base, reserve_pages);
        return Err(Errno::OutOfMemory);
    }
    // Publish the freshly backed page into the registry snapshot, so a syscall
    // that copies from the new thread's stack resolves it.
    handlers.publish_region_mapping(process, span.committed_base(), THREAD_STACK_COMMIT_PAGES);

    // Build the thread's own switch-in hook and entry from the process's
    // shared hook and port handle — no per-arch producer is involved
    // (decision 9).
    let pre_resume = space.thread_pre_resume(request.tls_base);
    let entry = space.thread_entry(UserEntry::new(
        request.entry,
        span.top(),
        request.arg,
        request.tls_base,
    ));

    // Born parked: the caller installs the thread's per-thread state under the
    // returned id and only then unparks it.
    let work = move |_yielder: &mut crate::kthread::Yielder<A::Cs>| {
        // SAFETY: the thread is dispatched only after `unpark` below, by which
        // point its switch-in hook has run on the dispatcher's context and
        // activated the process's own root; the trap path was installed at
        // boot. `entry` names the caller-validated entry address and the top of
        // the stack reserved above.
        unsafe { entry.enter() }
    };
    let admitted = crate::kthread::spawn_user_kthread_with_stack_live(
        handlers.sched,
        handlers.arch.context_switch(),
        kernel_stack,
        cpu,
        Priority::Normal,
        pre_resume,
        Arc::clone(&space),
        work,
        true,
    );
    let Ok(task_id) = admitted else {
        release_reservation(&space, reserve_base, reserve_pages);
        return Err(Errno::NoSpace);
    };
    let thread = SecTaskId(task_id);

    // Alias the new thread onto the process's one capability record, so it acts
    // under exactly the same authority and a revocation binds it too. A refusal
    // (an unknown process, or an id already registered) is a kernel invariant
    // violation: retire the task and fail closed rather than run a thread whose
    // authority the dispatcher cannot resolve.
    if let Err(error) = handlers.caps.write().register_thread(thread, process) {
        let _ = handlers.sched.exit(task_id);
        release_reservation(&space, reserve_base, reserve_pages);
        return Err(match error {
            ThreadRegisterError::AlreadyPresent => Errno::AlreadyExists,
            // `UnknownProcess` and any future variant: the caller's own record
            // vanished under us.
            _ => Errno::NotFound,
        });
    }
    handlers.aspaces.write().set_owned_thread_stack(
        process,
        thread,
        span,
        OwnedThreadStack {
            reserve_base,
            reserve_pages,
            clear_on_exit: request.clear_on_exit,
        },
    );

    // Every piece of the thread's state now exists, so it is safe to run. A
    // refused wake on a freshly parked task is a kernel invariant violation:
    // retire it and fail closed.
    if handlers.sched.unpark(task_id).is_err() {
        let _ = handlers.sched.exit(task_id);
        release_thread_state(handlers, &space, thread);
        return Err(Errno::NoSpace);
    }
    Ok(task_id)
}

/// End the calling thread, releasing what it alone owns.
///
/// The **last** thread of a process to end is a process exit: its status is `0`
/// (falling off the end of `main` gives the same), and the whole process's
/// kernel state is reclaimed. Any earlier thread releases only its own stack,
/// span, and capability alias; the process's address space, descriptors, and
/// limits belong to the group and its remaining threads still need them.
///
/// Either way the thread's clear-on-exit word is zeroed and futex-woken first,
/// so a joiner is released before the thread's memory goes away.
///
/// Returns `Ok(0)`; the dispatch boundary turns the `thread_exit` syscall
/// number into the scheduler `Exit` that actually reaps the task, exactly as it
/// does for `exit`.
pub fn exit<A>(handlers: &KernelSyscallHandlers<'_, A>, caller: &CallerContext<'_>) -> SyscallResult
where
    A: KernelArch + 'static,
{
    let process = caller.process();
    let thread = caller.task_id;
    let last = handlers.caps.read().thread_count(process) <= 1;

    // Release a joiner before anything is torn down: it observes the zeroed
    // word and may then unmap the thread control block it was watching.
    notify_thread_death(handlers, caller);

    if last {
        // The group is empty: this is the process's exit. Record the status the
        // parent's `wait` reaps, then run the one shared reclamation the `exit`
        // syscall and the signal-terminate path both drive.
        handlers.process_wait.record_exit(process, 0);
        handlers.reclaim_process_resources(process);
        return Ok(0);
    }

    // A sibling remains: release only what this thread owns.
    let cpu = SchedulerArch::current_cpu(handlers.arch);
    if let Some(space) = crate::kthread::current_process_space(cpu) {
        release_thread_state(handlers, &space, thread);
    } else {
        // No live space to return the reservation to (a build that retained
        // none never reserved one either); the per-thread bookkeeping still
        // goes.
        drop_thread_bookkeeping(handlers, thread);
    }
    Ok(0)
}

/// Zero the calling thread's clear-on-exit word and wake whoever is blocked on
/// it, so a `join` completes when the thread dies (decision 5).
///
/// Best-effort by design and never fatal: a thread that named no word has
/// nothing to notify, and a word whose page cannot be reached is a defect in
/// the *dying* thread's own bookkeeping, not a reason to leave the process in a
/// half-torn-down state. The wake is issued regardless of whether the store
/// landed, because a joiner re-tests the word and a spurious wake is harmless.
fn notify_thread_death<A>(handlers: &KernelSyscallHandlers<'_, A>, caller: &CallerContext<'_>)
where
    A: KernelArch + 'static,
{
    let Some(owned) = handlers.aspaces.read().owned_thread_stack(caller.task_id) else {
        return;
    };
    if owned.clear_on_exit == 0 {
        return;
    }
    let _ = handlers.copy_out_user(caller, owned.clear_on_exit, &0u32.to_le_bytes());
    // How many joiners were waiting is not a decision this path can act on: a
    // thread that named a clear-on-exit word may legitimately have nobody
    // joining it (a detached thread), and one that does re-tests the word.
    let _ = crate::futex::wake_installed(caller.process(), owned.clear_on_exit, usize::MAX);
}

/// Return `thread`'s stack reservation to the process's anonymous window and
/// drop its per-thread kernel bookkeeping.
fn release_thread_state<A>(
    handlers: &KernelSyscallHandlers<'_, A>,
    space: &Arc<ProcessSpace>,
    thread: SecTaskId,
) where
    A: KernelArch + 'static,
{
    if let Some(owned) = handlers.aspaces.read().owned_thread_stack(thread) {
        release_reservation(space, owned.reserve_base, owned.reserve_pages);
    }
    drop_thread_bookkeeping(handlers, thread);
}

/// Drop `thread`'s capability alias and stack span (the per-thread halves of
/// the process's state), leaving every process-scoped record for its siblings.
fn drop_thread_bookkeeping<A>(handlers: &KernelSyscallHandlers<'_, A>, thread: SecTaskId)
where
    A: KernelArch + 'static,
{
    handlers.aspaces.write().withdraw_thread(thread);
    handlers.caps.write().remove_thread(thread);
}

/// Release a `[guard | stack]` reservation, unmapping whatever pages the thread
/// made resident (each frame zeroed on the way out) and returning the range to
/// the process's anonymous window.
///
/// A refusal is a bookkeeping mismatch, not something a caller can act on: the
/// thread is dying either way, and the reservation is reclaimed wholesale when
/// the process's space is dropped.
fn release_reservation(space: &Arc<ProcessSpace>, reserve_base: u64, reserve_pages: u64) {
    let _ = space.with(|live| live.unmap_anonymous(reserve_base, reserve_pages));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default request is the caller's own effective bound, so a tightened
    /// `ulimit` sizes its threads too with no second definition of the number.
    #[test]
    fn the_default_stack_is_the_callers_own_stack_bytes_bound() {
        assert_eq!(
            resolve_stack_bytes(tairix_abi::THREAD_STACK_DEFAULT, 64 * 1024),
            Ok(64 * 1024)
        );
        // An unbounded `stack-bytes` names no size, so the default policy's
        // own floor stands in for it.
        assert_eq!(
            resolve_stack_bytes(tairix_abi::THREAD_STACK_DEFAULT, RLIMIT_INFINITY),
            Ok(DEFAULT_STACK_LIMIT_BYTES)
        );
    }

    #[test]
    fn an_explicit_request_is_page_rounded_and_bounded_by_the_limit() {
        let page = PAGE_SIZE as u64;
        assert_eq!(resolve_stack_bytes(1, 64 * 1024), Ok(page));
        assert_eq!(resolve_stack_bytes(PAGE_SIZE + 1, 64 * 1024), Ok(2 * page));
        // Past the soft bound: refused, never silently clamped.
        assert_eq!(
            resolve_stack_bytes(128 * 1024, 64 * 1024),
            Err(Errno::OutOfRange)
        );
        // Exactly at the bound is allowed.
        assert_eq!(resolve_stack_bytes(64 * 1024, 64 * 1024), Ok(64 * 1024));
    }

    #[test]
    fn a_stack_request_that_rounds_to_nothing_is_refused() {
        assert_eq!(resolve_stack_bytes(0, 0), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn the_span_leaves_the_lowest_reserved_page_as_an_unbacked_guard() {
        let page = PAGE_SIZE as u64;
        let base = 0x4000_0000;
        let span = thread_stack_span(base, 8 * page).expect("well-formed");
        assert_eq!(
            span.reserve_base(),
            base + page,
            "the stack starts one page above the reservation: that page is the guard"
        );
        assert_eq!(span.top(), base + page + 8 * page);
        assert!(
            !span.in_growth_room(base),
            "a fault in the guard page must not resolve as stack growth"
        );
        assert!(
            !span.in_growth_room(base + page - 1),
            "nor anywhere else in the guard page"
        );
        assert!(span.in_growth_room(base + page), "the stack's own low page");
    }

    #[test]
    fn the_span_commits_exactly_the_top_pages_and_grows_below_them() {
        let page = PAGE_SIZE as u64;
        let span = thread_stack_span(0x4000_0000, 8 * page).expect("well-formed");
        assert_eq!(span.committed_bytes(), THREAD_STACK_COMMIT_PAGES * page);
        assert_eq!(span.committed_base(), span.top() - page);
        // The committed page is not growth room; everything below it is.
        assert!(!span.in_growth_room(span.committed_base()));
        assert!(span.in_growth_room(span.committed_base() - 1));
    }

    #[test]
    fn a_stack_too_small_to_hold_its_committed_pages_is_refused() {
        // A stack shorter than the eager commit would leave a span whose
        // committed base is below its own reserve base.
        assert!(thread_stack_span(0x4000_0000, 0).is_none());
    }

    #[test]
    fn the_reservation_is_the_stack_plus_one_guard_page() {
        let page = PAGE_SIZE as u64;
        assert_eq!(thread_reserve_pages(8 * page), 9);
        assert_eq!(thread_reserve_pages(page), 2);
    }

    #[test]
    fn a_span_at_the_top_of_the_address_space_is_refused_rather_than_wrapping() {
        assert!(thread_stack_span(u64::MAX - 0xFFF, 1 << 20).is_none());
    }
}
