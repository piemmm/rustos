//! Production [`SyscallHandlers`] wiring for `kernel/core`.
//!
//! Stage 2.7 follow-up (f3) of `PLAN.md`. The dispatcher in
//! `kernel/syscall` performs the §5.4 checks (identify caller, check
//! capability, validate arguments, audit) and then forwards the call
//! through the [`SyscallHandlers`] trait. This module ships the one
//! concrete implementation that the production kernel uses; tests of
//! the dispatcher continue to substitute their own mocks.
//!
//! # Surface
//!
//! [`KernelSyscallHandlers<'a, A>`] borrows three pieces of kernel
//! state for the lifetime of one syscall:
//!
//! * `&'a Scheduler<A>` — for `yield_now` and `exit` (and, in the
//!   future, anything else that needs a `TaskId → Task` lookup).
//! * `&'a RwLock<CapTable>` — for `cap_query` (read) and `cap_revoke`
//!   (write). Wrapping `CapTable` in `kernel/sync::RwLock` is the
//!   minimum interior mutability the dispatcher requires; the choice
//!   mirrors `Scheduler::tasks`'s reader-preferring lock and lets
//!   `kernel_main`'s `KernelState` compose the two registries under a
//!   single lock-ordering policy (`AGENTS.md` §2.4).
//! * `&'a A` — the arch port, for `clock_get` via
//!   [`KernelArch::monotonic_ns`].
//!
//! Two deferred-feature branches deliberately return stable errnos
//! plus a [`AuditEvent::SyscallFeatureUnavailable`] record per
//! `AGENTS.md` §15.1 — *announce the deferral, never stub*:
//!
//! | Syscall              | Errno              | Reason |
//! |----------------------|--------------------|--------|
//! | `ipc_send`/`ipc_recv` | `NotFound`         | Named-port registry not landed (Stage 5 prerequisite). |
//! | `cap_delegate`       | `NotImplemented`   | User-memory copy-in not landed (Stage 5 / Stage 6).    |
//!
//! Both branches still go through the dispatcher's standard audit
//! pipeline (the `IPC_SEND` / `CAP_DELEGATE` entries are
//! `spec.audit = true`) and **additionally** emit the
//! `SyscallFeatureUnavailable` record so a downstream consumer can
//! tell apart "handler rejected because the call failed" from
//! "handler rejected because the backing subsystem is intentionally
//! inert".
//!
//! # No ambient authority
//!
//! Nothing in this module reads or writes a global; every input is
//! threaded through [`KernelSyscallHandlers::new`]. `cap_query`,
//! `cap_revoke`, and `clock_get` all consult the caller's already-
//! validated [`CallerContext`] — there is no `uid == 0` shortcut.

use crate::sched::{CpuId, SchedError, Scheduler, SchedulerArch};
use rustos_abi::{CapabilityId, Errno, IrqHandle};
use rustos_kernel_irq::{
    block_until_ready, IrqController, IrqTable, IrqWaitAbort, IrqWaiter, WaitOutcome,
};
use rustos_kernel_sec::{CapTable, TaskId as SecTaskId};
use rustos_kernel_syscall::{CallerContext, Dispatcher, RawArgs, SyscallHandlers, SyscallResult};
use rustos_log::{Field, Sink};
use rustos_sync::RwLock;
use rustos_util::fmt::format_hex_u64;

use crate::audit::AuditEvent;
use crate::bootinfo::KernelArch;
use crate::dispatch_slot::{DispatchHook, DispatchOutcome};

/// Production [`SyscallHandlers`] implementation.
///
/// Construct once at boot, after `KernelState` has assembled the
/// scheduler, the capability table, the arch handle, and the audit
/// sink. The struct holds borrows only, never owns anything; it is
/// designed to live on the stack of a syscall trampoline or inside
/// `KernelState` and be re-used for every syscall on every CPU.
pub struct KernelSyscallHandlers<'a, A>
where
    A: KernelArch + 'static,
{
    sched: &'a Scheduler<A>,
    caps: &'a RwLock<CapTable>,
    arch: &'a A,
    audit: &'a (dyn Sink + Sync),
    irq: &'a IrqTable,
    /// Controller-mask seam consumed by [`IrqTable::fire`] from the
    /// architecture port's trap path. Held here so the per-trap
    /// firing code can reach it from inside the dispatch hook; the
    /// `irq_bind` / `irq_wait` syscall handlers themselves do not
    /// dereference it (mask happens on `fire`, not on `wait`).
    irq_controller: &'a (dyn IrqController + Sync),
}

impl<'a, A> KernelSyscallHandlers<'a, A>
where
    A: KernelArch + 'static,
{
    // The `'a` lifetime appears in the constructor parameters; it is
    // not elidable there even though `clippy::elidable_lifetime_names`
    // would suggest it could be. The methods that follow take `&self`
    // only, so we re-name the borrow as `'_` in their signatures.

    /// Build a new handler set bound to the supplied kernel state.
    ///
    /// All borrows must outlive the dispatcher instance that wraps
    /// this handler. In the production kernel `KernelState` owns the
    /// targets and keeps them alive for the lifetime of the kernel
    /// (`AGENTS.md` §2.1 — no global mutable static).
    #[must_use]
    pub const fn new(
        sched: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        arch: &'a A,
        audit: &'a (dyn Sink + Sync),
        irq: &'a IrqTable,
        irq_controller: &'a (dyn IrqController + Sync),
    ) -> Self {
        Self {
            sched,
            caps,
            arch,
            audit,
            irq,
            irq_controller,
        }
    }

    /// Borrow the [`IrqTable`] this handler set wires `irq_bind` /
    /// `irq_wait` against.
    ///
    /// The kernel-binary trap path obtains the table this way so it
    /// can call [`IrqTable::fire`] from a trap dispatcher without
    /// having to re-borrow `KernelState`. The `irq_controller`
    /// argument [`IrqTable::fire`] requires is exposed through
    /// [`Self::irq_controller`].
    #[must_use]
    pub fn irq_table(&self) -> &IrqTable {
        self.irq
    }

    /// Borrow the [`IrqController`] this handler set wires
    /// [`IrqTable::fire`] against.
    #[must_use]
    pub fn irq_controller(&self) -> &(dyn IrqController + Sync) {
        self.irq_controller
    }

    /// Emit one [`AuditEvent::SyscallFeatureUnavailable`] record.
    ///
    /// The handler's deferred-feature branches call this *before*
    /// returning their stable [`Errno`] so an audit-sink observer
    /// sees the precise reason without having to correlate against
    /// the dispatcher's standard `SyscallHandlerRejected` emission.
    fn audit_feature_unavailable(&self, caller: &CallerContext<'_>, feature: &'static str) {
        let mut task_buf = [0u8; 16];
        let ev = AuditEvent::SyscallFeatureUnavailable;
        rustos_log::log(
            self.audit,
            &rustos_log::Event {
                level: rustos_log::Level::Error,
                id: ev.id(),
                message: ev.message(),
                fields: &[
                    Field {
                        key: "task",
                        value: format_hex_u64(caller.task_id.0, &mut task_buf),
                    },
                    Field {
                        key: "feature",
                        value: feature,
                    },
                ],
            },
        );
    }
}

impl<A> SyscallHandlers for KernelSyscallHandlers<'_, A>
where
    A: KernelArch + 'static,
{
    fn yield_now(&self, caller: &CallerContext<'_>) -> SyscallResult {
        // The dispatcher hands us the caller's `sec::TaskId`; the
        // scheduler's own `TaskId` is a transparent `u64`, so the
        // bridge is one field access. The `Scheduler::yield_current`
        // contract maps every error to a stable `Errno`:
        //
        // * `NoSuchTask` — the dispatcher walked a stale `CallerContext`
        //   (shouldn't happen: `KernelState` removes the caps record
        //   only after `Scheduler::exit`). Surface as `NotFound`.
        // * `InvalidState` — the task was not `Running`; this can
        //   legitimately happen if a sibling CPU parked it between
        //   `current_task` and `yield_current`. Surface as `OutOfRange`
        //   — the closest stable variant that means "the operation
        //   was inapplicable to the current state" without
        //   over-promising `PermissionDenied`.
        match self.sched.yield_current(caller.task_id.0) {
            Ok(()) => Ok(0),
            Err(crate::sched::SchedError::NoSuchTask) => Err(Errno::NotFound),
            Err(_) => Err(Errno::OutOfRange),
        }
    }

    fn exit(&self, caller: &CallerContext<'_>, _code: i32) -> SyscallResult {
        // The exit code is already captured in the dispatcher's
        // `SyscallInvoked` audit record (the `EXIT` spec sets
        // `audit = true`). We deliberately do **not** invent a new
        // field on `Task` or on `CapTable` to remember it — that
        // would be interface creep without a consumer
        // (`AGENTS.md` §2.4).
        //
        // Order matters:
        //
        //   1. Release every IRQ binding the exiting task held
        //      (`docs/src/security/irq.md` — the kernel unmasks no
        //      lines on exit; a freshly created task that wants the
        //      same line must re-issue `irq_bind`).
        //   2. Drop the capability record so a concurrent
        //      `cap_query` racing this `exit` cannot observe a task
        //      that the scheduler still believes exists but whose
        //      caps have vanished.
        //   3. Mark the task exited in the scheduler.
        //
        // Each step is idempotent; the call ordering matters for
        // the *security* observer (no caller can hold an audited
        // capability bit after the IRQ subsystem has released the
        // task's bindings).
        let task = caller.task_id;
        let _ = self.irq.release_for(task);
        let _ = self.caps.write().remove(task);
        match self.sched.exit(task.0) {
            Ok(()) => Ok(0),
            Err(SchedError::NoSuchTask) => Err(Errno::NotFound),
            Err(_) => Err(Errno::OutOfRange),
        }
    }

    fn ipc_send(
        &self,
        caller: &CallerContext<'_>,
        _endpoint: u64,
        _ptr: u64,
        _len: usize,
    ) -> SyscallResult {
        self.audit_feature_unavailable(caller, "ipc_named_ports");
        Err(Errno::NotFound)
    }

    fn ipc_recv(
        &self,
        caller: &CallerContext<'_>,
        _endpoint: u64,
        _ptr: u64,
        _len: usize,
    ) -> SyscallResult {
        self.audit_feature_unavailable(caller, "ipc_named_ports");
        Err(Errno::NotFound)
    }

    fn cap_query(&self, caller: &CallerContext<'_>, cap: CapabilityId) -> SyscallResult {
        // The caller's effective caps are already in `CallerContext`.
        // Going through the CapTable would re-validate the same set
        // and add a lock acquisition for no extra information; the
        // dispatcher guarantees `caller.caps` is the authoritative
        // record `KernelState` registered for `caller.task_id`.
        Ok(u64::from(caller.caps.has(cap)))
    }

    fn cap_delegate(
        &self,
        caller: &CallerContext<'_>,
        _target: u64,
        _set_ptr: u64,
    ) -> SyscallResult {
        // `set_ptr` is a user-space pointer naming a `CapabilitySet`.
        // Reading it requires the user-memory copy-in path, which has
        // not yet landed (`PLAN.md` Stage 5 / Stage 6). Until that
        // arrives the handler announces the deferral and refuses.
        self.audit_feature_unavailable(caller, "user_memory_copyin");
        Err(Errno::NotImplemented)
    }

    fn cap_revoke(
        &self,
        _caller: &CallerContext<'_>,
        target: u64,
        cap: CapabilityId,
    ) -> SyscallResult {
        // The target task is named by raw `TaskId`. `caps_for_mut`
        // returns `None` for an unknown task; that is a stable
        // condition (`Errno::NotFound`) rather than a kernel bug.
        let mut guard = self.caps.write();
        let entry = guard.caps_for_mut(SecTaskId(target));
        match entry {
            Some(record) => {
                // `revoke` is idempotent — it returns `false` if the
                // capability was not held, but the audit record it
                // emits via the underlying `TaskCapabilities::revoke`
                // is the security-relevant signal (the *attempt* is
                // the event). The boolean is intentionally discarded
                // (`AGENTS.md` §5.4.4).
                let _ = record.revoke(cap, self.audit);
                Ok(0)
            }
            None => Err(Errno::NotFound),
        }
    }

    fn clock_get(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        // `monotonic_ns` is documented as monotonically non-decreasing
        // per CPU; the dispatcher invokes us on the issuing CPU's
        // process context (`AGENTS.md` §5.4 step 1), so reading from
        // `self.arch.current_cpu()` is the natural source. We do not
        // accept a caller-supplied CPU id — there is no syscall
        // argument for one, and a kernel-trusted lookup is the only
        // sanctioned source.
        let cpu = crate::sched::SchedulerArch::current_cpu(self.arch);
        Ok(self.arch.monotonic_ns(cpu))
    }

    fn irq_bind(&self, caller: &CallerContext<'_>, line: u32) -> SyscallResult {
        // Capability gate has already been enforced by the
        // dispatcher (the syscall spec carries the `CAP_IRQ_BIND`
        // requirement and the dispatcher's per-call check rejects
        // any caller without it before reaching this handler —
        // `kernel/syscall::Dispatcher::dispatch`). We re-bind the
        // table key against `caller.task_id` (kernel-trusted, not
        // caller-supplied) so the resulting [`IrqHandle`] is
        // unforgeable in the strong sense: it can only be waited on
        // by the task that bound it.
        match self.irq.bind(line, caller.task_id) {
            Ok(out) => Ok(out.handle.as_u64()),
            Err(e) => Err(e.to_errno()),
        }
    }

    fn irq_wait(
        &self,
        caller: &CallerContext<'_>,
        handle: IrqHandle,
        timeout_ns: u64,
    ) -> SyscallResult {
        // The poll-and-yield loop itself lives in
        // `rustos_kernel_irq::block_until_ready` so the in-kernel
        // `KernelVirtioHost::notify_wait` path can drive the same
        // implementation without a second copy (`AGENTS.md` §2.2).
        // This handler supplies the scheduler + arch seam through
        // `SyscallIrqWaiter` and translates the terminal outcome to
        // the documented stable `Errno`.
        let waiter = SyscallIrqWaiter {
            sched: self.sched,
            arch: self.arch,
            // The CPU is captured once: `monotonic_ns` is documented
            // as non-decreasing per CPU, and the handler never
            // migrates mid-wait, so every clock read inside the loop
            // observes the same monotone source
            // (`docs/src/security/irq.md`).
            cpu: SchedulerArch::current_cpu(self.arch),
            task: caller.task_id,
        };
        match block_until_ready(self.irq, handle, caller.task_id, timeout_ns, &waiter) {
            WaitOutcome::Ready => Ok(0),
            WaitOutcome::TimedOut => Err(Errno::TimedOut),
            // A forged / released handle and a vanished task both map
            // to `Errno::NotFound`: `NoSuchTask` cannot happen here
            // (`CallerContext` is built from the live scheduler
            // current-task slot) but is mapped for symmetry with
            // `yield_now` (`AGENTS.md` §5.4.5).
            WaitOutcome::NotFound | WaitOutcome::Aborted(IrqWaitAbort::TaskVanished) => {
                Err(Errno::NotFound)
            }
            // Any other scheduler error fails closed to
            // `Errno::OutOfRange`.
            WaitOutcome::Aborted(IrqWaitAbort::SchedulerError) => Err(Errno::OutOfRange),
        }
    }
}

/// [`IrqWaiter`] adapter wiring the `irq_wait` syscall handler's
/// scheduler + architecture borrows into the shared
/// [`block_until_ready`] loop.
///
/// Holds only borrows and a captured CPU id; constructed fresh per
/// `irq_wait` call on the issuing CPU's process context.
struct SyscallIrqWaiter<'a, A>
where
    A: KernelArch + 'static,
{
    sched: &'a Scheduler<A>,
    arch: &'a A,
    cpu: CpuId,
    task: SecTaskId,
}

impl<A> IrqWaiter for SyscallIrqWaiter<'_, A>
where
    A: KernelArch + 'static,
{
    fn now_ns(&self) -> u64 {
        self.arch.monotonic_ns(self.cpu)
    }

    fn yield_now(&self) -> Result<(), IrqWaitAbort> {
        // A successful yield re-enters the run queue; `InvalidState`
        // happens in tests where the calling task is not marked
        // Running (e.g. the host-side handler tests that do not
        // drive a real dispatch loop) and is treated as a benign
        // continue — the loop still terminates because
        // `monotonic_ns` is strictly monotonic on every supported
        // arch port.
        match self.sched.yield_current(self.task.0) {
            Ok(()) | Err(SchedError::InvalidState) => Ok(()),
            Err(SchedError::NoSuchTask) => Err(IrqWaitAbort::TaskVanished),
            Err(_) => Err(IrqWaitAbort::SchedulerError),
        }
    }
}

/// Production [`DispatchHook`] wiring `KernelSyscallHandlers` to the
/// bin-crate dispatch callback.
///
/// Owns the same borrows as [`KernelSyscallHandlers`] plus a
/// [`Dispatcher`] cell built on top of them. The bin-crate
/// `extern "C"` syscall-dispatch callback ((f5)) calls
/// [`Self::dispatch`] once per syscall; this method runs the §5.4
/// sequence (identify caller → forward to [`Dispatcher::dispatch`] →
/// translate result) and returns a [`DispatchOutcome`] the bin crate
/// can encode back into the architecture's syscall-return register
/// or, on caller-identification failure, fail-close by halting the
/// CPU.
///
/// # Caller identification
///
/// The hook reads the per-CPU current-task slot from
/// `Scheduler::current_task` ((f1)) and looks up the per-task
/// capability record through the `CapTable` ((f2)). Both lookups are
/// fallible:
///
/// * `current_task` returns `None` when no task is currently running
///   on the issuing CPU. That cannot happen once the scheduler is
///   live, but the trampoline must not assume so (`AGENTS.md` §5.4.5).
/// * `caps_for` returns `None` when the running task has no
///   capability record — also impossible during normal operation
///   (`KernelState` populates the record before scheduling any task),
///   but treated as a security failure on the same grounds.
///
/// Either failure emits one [`AuditEvent::SyscallNoCallerContext`]
/// record (carrying a stable `cause` field naming which lookup
/// failed) and returns [`DispatchOutcome::NoCallerContext`]; the
/// bin-crate callback halts the CPU forever in response.
pub struct KernelDispatchHook<'a, A>
where
    A: KernelArch + 'static,
{
    handlers: KernelSyscallHandlers<'a, A>,
    sched: &'a Scheduler<A>,
    caps: &'a RwLock<CapTable>,
    arch: &'a A,
    audit: &'a (dyn Sink + Sync),
}

impl<'a, A> KernelDispatchHook<'a, A>
where
    A: KernelArch + 'static,
{
    /// Build a new dispatch hook bound to the supplied kernel state.
    ///
    /// All borrows must outlive the slot the hook is published into.
    /// `KernelState` (constructed by [`crate::kernel_main`]) holds the
    /// targets for the lifetime of the running kernel; the hook is
    /// `Box::leak`'d alongside it so the published `'static dyn
    /// DispatchHook` is sound (`AGENTS.md` §2.1 — no global mutable
    /// static; the leak is a one-shot, immutable publish).
    #[must_use]
    pub fn new(
        sched: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        arch: &'a A,
        audit: &'a (dyn Sink + Sync),
        irq: &'a IrqTable,
        irq_controller: &'a (dyn IrqController + Sync),
    ) -> Self {
        Self {
            handlers: KernelSyscallHandlers::new(sched, caps, arch, audit, irq, irq_controller),
            sched,
            caps,
            arch,
            audit,
        }
    }

    /// Borrow the [`KernelSyscallHandlers`] this hook owns.
    ///
    /// Used by the arch-port trap path to reach the [`IrqTable`] +
    /// [`IrqController`] pair through
    /// [`KernelSyscallHandlers::irq_table`] /
    /// [`KernelSyscallHandlers::irq_controller`] without re-borrowing
    /// `KernelState`.
    #[must_use]
    pub fn handlers(&self) -> &KernelSyscallHandlers<'a, A> {
        &self.handlers
    }

    /// Emit one [`AuditEvent::SyscallNoCallerContext`] record.
    fn audit_no_caller_context(&self, cpu: u32, cause: &'static str) {
        let mut cpu_buf = [0u8; 16];
        let ev = AuditEvent::SyscallNoCallerContext;
        rustos_log::log(
            self.audit,
            &rustos_log::Event {
                level: rustos_log::Level::Error,
                id: ev.id(),
                message: ev.message(),
                fields: &[
                    Field {
                        key: "cpu",
                        value: format_hex_u64(u64::from(cpu), &mut cpu_buf),
                    },
                    Field {
                        key: "cause",
                        value: cause,
                    },
                ],
            },
        );
    }
}

impl<A> DispatchHook for KernelDispatchHook<'_, A>
where
    A: KernelArch + 'static,
{
    fn dispatch(&self, raw_number: u16, args: RawArgs) -> DispatchOutcome {
        // Step 1 (AGENTS.md §5.4.1) — identify the caller. The
        // scheduler's per-CPU current-task slot is the only sanctioned
        // source; no caller-supplied identity is accepted.
        let cpu = SchedulerArch::current_cpu(self.arch);
        let Some(sched_task_id) = self.sched.current_task(cpu) else {
            self.audit_no_caller_context(cpu, "no_current_task");
            return DispatchOutcome::NoCallerContext;
        };

        // The capability registry is read-locked for the entire
        // dispatch so the `&TaskCapabilities` we hand to
        // `CallerContext` remains valid for the duration of the call.
        // The lock is reader-preferring (`kernel/sync::RwLock`); a
        // concurrent `cap_revoke` waits behind us. AGENTS.md §5.4
        // step 1 ("Identify the caller") explicitly forbids
        // re-locking mid-flight to avoid a TOCTOU window between
        // capability check and use.
        let guard = self.caps.read();
        let task_id = SecTaskId(sched_task_id);
        let Some(caps_record) = guard.caps_for(task_id) else {
            // Drop the guard before emitting the audit record so the
            // sink write does not hold the read lock unnecessarily.
            drop(guard);
            self.audit_no_caller_context(cpu, "no_capability_record");
            return DispatchOutcome::NoCallerContext;
        };

        let caller = CallerContext {
            task_id,
            caps: caps_record,
        };

        // Steps 2–5: hand off to the dispatcher, which performs the
        // capability check, argument validation, handler dispatch,
        // and audit emission.
        let dispatcher = Dispatcher::new(&self.handlers, self.audit);
        DispatchOutcome::Returned(dispatcher.dispatch(&caller, raw_number, args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::SchedulerConfig;
    use crate::test_arch::TestArch;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use rustos_abi::{CapabilityId, Errno};
    use rustos_caps::CapabilitySet;
    use rustos_kernel_irq::{IrqTable, UnsupportedController};
    use rustos_kernel_sec::{TaskCapabilities, UserId};
    use rustos_log::{set_max_level, Level};

    fn install_trace_filter() {
        // The global log filter defaults to `Error`; `SyscallFeatureUnavailable`
        // is emitted at `Error`, but `set_max_level(Trace)` keeps the
        // tests robust against a future raise of `SyscallFeatureUnavailable`'s
        // severity or against other dispatcher events flowing through
        // the same sink (`AGENTS.md` §7 — no flaky tests).
        set_max_level(Level::Trace);
    }

    fn make_sink() -> &'static TestSink {
        Box::leak(Box::new(TestSink::new()))
    }

    fn caps_with(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    fn make_caps_record(
        task: u64,
        items: &[CapabilityId],
        sink: &(dyn Sink + Sync),
    ) -> TaskCapabilities {
        let set = caps_with(items);
        TaskCapabilities::derive(SecTaskId(task), UserId(1000), set, set, sink)
    }

    fn make_sched(arch: Arc<TestArch>) -> Scheduler<TestArch> {
        let cfg = SchedulerConfig::defaults_for(1);
        Scheduler::new(cfg, arch).expect("scheduler builds")
    }

    /// `yield_now` against an unknown task surfaces `NotFound`.
    #[test]
    fn yield_now_unknown_task_returns_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(0xDEAD, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(0xDEAD),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        assert_eq!(h.yield_now(&ctx), Err(Errno::NotFound));
    }

    /// `exit` removes the capability record and forwards to scheduler.
    #[test]
    fn exit_clears_caps_record_and_returns_not_found_on_unknown_task() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        // Register a record so we can confirm `exit` evicts it even
        // though the scheduler half fails.
        let record = make_caps_record(7, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(record);
        assert_eq!(table.read().len(), 1);

        let caps = make_caps_record(7, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        let r = h.exit(&ctx, 0);
        // Scheduler half returns `NoSuchTask` → `NotFound`.
        assert_eq!(r, Err(Errno::NotFound));
        // The capability record was evicted regardless.
        assert!(table.read().is_empty());
    }

    /// `cap_query` returns 1 for a held capability and 0 otherwise.
    #[test]
    fn cap_query_matches_caller_caps() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(1, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(1),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        assert_eq!(h.cap_query(&ctx, CapabilityId::FS_MOUNT), Ok(1));
        assert_eq!(h.cap_query(&ctx, CapabilityId::DRV_KERNEL), Ok(0));
    }

    /// `ipc_send` is intentionally inert and announces the deferral.
    #[test]
    fn ipc_send_returns_not_found_and_audits_feature_unavailable() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        sink.clear();
        assert_eq!(h.ipc_send(&ctx, 1, 0x1000, 4), Err(Errno::NotFound));
        // Exactly one SyscallFeatureUnavailable record.
        let ids = sink.event_ids();
        assert_eq!(
            ids,
            alloc::vec![AuditEvent::SyscallFeatureUnavailable.id().0]
        );
    }

    /// `ipc_recv` mirrors `ipc_send`'s deferral.
    #[test]
    fn ipc_recv_returns_not_found_and_audits_feature_unavailable() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        sink.clear();
        assert_eq!(h.ipc_recv(&ctx, 1, 0x2000, 8), Err(Errno::NotFound));
        assert_eq!(
            sink.event_ids(),
            alloc::vec![AuditEvent::SyscallFeatureUnavailable.id().0]
        );
    }

    /// `cap_delegate` defers user-memory copy-in.
    #[test]
    fn cap_delegate_returns_not_implemented_and_audits_feature_unavailable() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(4, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        sink.clear();
        assert_eq!(h.cap_delegate(&ctx, 5, 0x3000), Err(Errno::NotImplemented));
        assert_eq!(
            sink.event_ids(),
            alloc::vec![AuditEvent::SyscallFeatureUnavailable.id().0]
        );
    }

    /// `cap_revoke` against a known task succeeds; unknown target is
    /// `NotFound`.
    #[test]
    fn cap_revoke_hits_known_task_and_misses_unknown() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        // Register target task 10 with FS_MOUNT.
        let record = make_caps_record(10, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(record);

        let caller_caps = make_caps_record(5, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(5),
            caps: &caller_caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);

        // Hit: revoke FS_MOUNT from task 10.
        assert_eq!(h.cap_revoke(&ctx, 10, CapabilityId::FS_MOUNT), Ok(0));
        // The record now lacks FS_MOUNT.
        assert!(!table
            .read()
            .caps_for(SecTaskId(10))
            .expect("still present")
            .has(CapabilityId::FS_MOUNT));

        // Miss: revoke from a non-existent task.
        assert_eq!(
            h.cap_revoke(&ctx, 999, CapabilityId::FS_MOUNT),
            Err(Errno::NotFound)
        );
    }

    /// `irq_bind` succeeds for an in-range line, mints a non-zero
    /// handle, and records the binding against the caller's task id.
    /// The dispatcher's `SyscallInvoked` audit record is emitted by
    /// the outer dispatcher and is therefore not asserted here
    /// (`kernel/syscall::Dispatcher` covers it); this test asserts
    /// the handler's *behaviour* — a fresh handle returned, the
    /// table populated, no `SyscallFeatureUnavailable` emission.
    #[test]
    fn irq_bind_mints_handle_and_records_owner_against_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        sink.clear();
        let raw = h.irq_bind(&ctx, 5).expect("bind succeeds");
        assert_ne!(raw, 0, "fresh handle must not be IrqHandle::INVALID");
        let entry = irq
            .lookup(IrqHandle::from_raw(raw))
            .expect("binding present");
        assert_eq!(entry.line, 5);
        assert_eq!(entry.owner, SecTaskId(7));
        // No `SyscallFeatureUnavailable` audit emission — the
        // subsystem is now wired (`docs/src/security/irq.md`
        // failure-mode table: a successful bind is audited by the
        // dispatcher's `SyscallInvoked`, not by the handler).
        assert!(
            !sink
                .event_ids()
                .contains(&AuditEvent::SyscallFeatureUnavailable.id().0),
            "deferred-feature audit must no longer fire"
        );
    }

    /// `irq_bind` rejects a line outside the configured `max_line`
    /// with `Errno::OutOfRange`. The dispatcher emits
    /// `SyscallHandlerRejected` for the failure; this test focuses
    /// on the handler's errno mapping.
    #[test]
    fn irq_bind_returns_out_of_range_for_line_above_max() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        assert_eq!(h.irq_bind(&ctx, 100), Err(Errno::OutOfRange));
    }

    /// `irq_bind` rejects a duplicate binding for the same line
    /// with `Errno::OutOfRange` (the closest stable variant for
    /// "operation inapplicable to current state").
    #[test]
    fn irq_bind_rejects_duplicate_line() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        let _ = h.irq_bind(&ctx, 5).expect("first bind ok");
        assert_eq!(h.irq_bind(&ctx, 5), Err(Errno::OutOfRange));
    }

    /// `irq_wait` against a forged handle (one not minted for the
    /// calling task) returns `Errno::NotFound`. The dispatcher
    /// emits the `SyscallHandlerRejected` audit record.
    #[test]
    fn irq_wait_returns_not_found_on_forged_handle() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        assert_eq!(
            h.irq_wait(&ctx, IrqHandle::from_raw(0xDEAD_BEEF), 0),
            Err(Errno::NotFound)
        );
    }

    /// `irq_wait` with a zero-duration timeout returns
    /// `Errno::TimedOut` when the bound line has not fired.
    #[test]
    fn irq_wait_returns_timed_out_when_no_fire_within_zero_timeout() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        let raw = h.irq_bind(&ctx, 5).expect("bind");
        assert_eq!(
            h.irq_wait(&ctx, IrqHandle::from_raw(raw), 0),
            Err(Errno::TimedOut)
        );
    }

    /// Permissive [`rustos_kernel_irq::IrqController`] for the
    /// pre-fired-ready test below. Accepts every line; the
    /// in-crate `UnsupportedController` would reject the test's
    /// `IrqTable::fire` call before the table could set the ready
    /// flag (`UnsupportedController::mask` always returns
    /// `MaskError::Unsupported`).
    struct PermissiveController;
    impl rustos_kernel_irq::IrqController for PermissiveController {
        fn mask(&self, _line: u32) -> Result<(), rustos_kernel_irq::MaskError> {
            Ok(())
        }
    }

    /// `irq_wait` returns `Ok(0)` when the binding has been fired
    /// before the call (the ready flag is set and the handler
    /// consumes it on the first iteration).
    #[test]
    fn irq_wait_returns_ok_when_binding_pre_fired() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController; // syscall handler does not invoke `fire`
        let permissive = PermissiveController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        let raw = h.irq_bind(&ctx, 5).expect("bind");
        // Fire externally against the permissive controller (the
        // arch-port's trap path uses the controller borrowed by
        // `KernelSyscallHandlers`; this test exercises the
        // wait-side handler in isolation).
        irq.fire(5, &permissive).expect("fire");
        // Even with `timeout_ns = 0`, the pre-existing ready flag
        // is consumed on the first iteration (per the
        // ordering contract: ready beats timeout in a tie).
        assert_eq!(h.irq_wait(&ctx, IrqHandle::from_raw(raw), 0), Ok(0));
    }

    /// `exit` releases every IRQ binding the exiting task held.
    #[test]
    fn exit_releases_every_irq_binding_owned_by_task() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(9, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(9),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        let _ = h.irq_bind(&ctx, 5).expect("bind 5");
        let _ = h.irq_bind(&ctx, 6).expect("bind 6");
        assert_eq!(irq.len(), 2);
        // `exit` against an unknown scheduler task returns
        // `Errno::NotFound`, but the IRQ release still happens
        // (the ordering documented in the handler's source).
        let _ = h.exit(&ctx, 0);
        assert!(irq.is_empty(), "exit must drop every binding the task held");
    }

    /// `clock_get` reads `KernelArch::monotonic_ns` and returns
    /// strictly-increasing values across consecutive calls (the
    /// `TestArch` impl is strictly monotonic).
    #[test]
    fn clock_get_returns_monotonic_ns_from_arch() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(6, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl);
        let a = h.clock_get(&ctx).expect("first read");
        let b = h.clock_get(&ctx).expect("second read");
        assert!(b > a, "expected b > a, got a={a} b={b}");
    }
}
