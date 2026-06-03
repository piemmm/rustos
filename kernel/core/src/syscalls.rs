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
//! * `&'a RwLock<PortRegistry>` — the named-port registry, for
//!   `ipc_send` / `ipc_recv` endpoint resolution. Wrapped in the same
//!   reader-preferring lock as `CapTable` so the IPC hot path takes
//!   only a shared lock.
//! * `&'a RwLock<AddressSpaceRegistry>` — the per-task address-space
//!   registry, so a handler can resolve `caller.task_id` to the user
//!   [`AddressSpace`](rustos_kernel_mem::AddressSpace) +
//!   [`PhysMap`] pair the
//!   [`rustos_kernel_mem::uaccess`] copy path walks
//!   ([`KernelSyscallHandlers::with_caller_aspace`], increment C of
//!   `PLAN.md` Stage 7). Reaching it here keeps the copy bridge inside
//!   `kernel/core` so the decoupled dispatcher (`kernel/syscall`)
//!   never gains a `kernel/mem` dependency (`AGENTS.md` §17.4).
//!   Wrapped in the same reader-preferring lock as the other two.
//!
//! `ipc_send` / `ipc_recv` now resolve the destination endpoint
//! against that live registry: an endpoint that is not currently
//! bound fails closed with `NotFound` (a real lookup miss; the
//! dispatcher's standard pipeline audits it). For a *bound* endpoint
//! the message body still needs the user-memory copy-in/out path,
//! which has not landed (Stage 5 / Stage 6); that one remaining branch
//! returns a stable errno plus a [`AuditEvent::SyscallFeatureUnavailable`]
//! record per `AGENTS.md` §15.1 — *announce the deferral, never stub*:
//!
//! | Syscall              | Condition            | Errno            | Reason |
//! |----------------------|----------------------|------------------|--------|
//! | `ipc_send`/`ipc_recv` | endpoint unbound     | `NotFound`       | No port is bound to the endpoint in the [`PortRegistry`]; a real lookup miss. |
//! | `ipc_send`/`ipc_recv` | endpoint bound       | `NotImplemented` | The port resolves, but copying the payload to/from the caller's address space needs the user-memory copy-in path (Stage 5 / Stage 6). |
//! | `cap_delegate`       | always               | `NotImplemented` | User-memory copy-in not landed (Stage 5 / Stage 6).    |
//!
//! The deferred-feature branches still go through the dispatcher's
//! standard audit pipeline (the `IPC_SEND` / `CAP_DELEGATE` entries are
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
use rustos_abi::{CapabilityId, Errno, IrqHandle, RandomFlags, RANDOM_REQUEST_MAX_BYTES};
use rustos_kernel_ipc::{EndpointId, PortRegistry};
use rustos_kernel_irq::{
    block_until_ready, IrqController, IrqTable, IrqWaitAbort, IrqWaiter, WaitOutcome,
};
use rustos_kernel_mem::{PhysMap, UserAddressSpace};
use rustos_kernel_sec::{CapTable, TaskId as SecTaskId};
use rustos_kernel_syscall::{CallerContext, Dispatcher, RawArgs, SyscallHandlers, SyscallResult};
use rustos_log::{Field, Sink};
use rustos_sync::RwLock;
use rustos_util::fmt::format_hex_u64;

use crate::aspace::AddressSpaceRegistry;
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
    /// Named-port registry consulted by `ipc_send` / `ipc_recv` to
    /// resolve the endpoint carried in the syscall to a live, kernel-
    /// owned [`rustos_kernel_ipc::Port`]. Borrowed under the same
    /// reader-preferring lock `KernelState` wraps it in; the handlers
    /// take only a read guard (`AGENTS.md` §2.1 — no global mutable
    /// static; the registry owns no lock of its own).
    ipc: &'a RwLock<PortRegistry>,
    /// Per-task address-space registry consulted to resolve the
    /// caller's [`rustos_kernel_sec::TaskId`] to the user
    /// [`AddressSpace`](rustos_kernel_mem::AddressSpace) and the
    /// [`PhysMap`] backing it — the pair the
    /// [`rustos_kernel_mem::uaccess`] copy path walks (`AGENTS.md`
    /// §5.4). Borrowed under the same reader-preferring lock as `caps`
    /// / `ipc`; [`Self::with_caller_aspace`] takes only a read guard.
    /// Threading it here (increment C, `PLAN.md` Stage 7) lets a
    /// handler reach the caller's mappings without coupling the
    /// decoupled dispatcher (`kernel/syscall`) to `kernel/mem`
    /// (`AGENTS.md` §17.4); increment D wires the deferred `ipc_send` /
    /// `ipc_recv` / `cap_delegate` / `random_get` copies through it.
    aspaces: &'a RwLock<AddressSpaceRegistry>,
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
    // Each argument is a *distinct* piece of kernel state the handler
    // borrows explicitly — there is no global mutable static and no
    // ambient authority to reach them through (`AGENTS.md` §2.1 / §4),
    // so they are threaded one-by-one exactly as `BootInfo::new`
    // mirrors its fields. Bundling them behind a wrapper purely to
    // satisfy the arg-count lint would be the one-use wrapper type
    // `AGENTS.md` §2.3 forbids; the explicit list is the clearer shape.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        sched: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        arch: &'a A,
        audit: &'a (dyn Sink + Sync),
        irq: &'a IrqTable,
        irq_controller: &'a (dyn IrqController + Sync),
        ipc: &'a RwLock<PortRegistry>,
        aspaces: &'a RwLock<AddressSpaceRegistry>,
    ) -> Self {
        Self {
            sched,
            caps,
            arch,
            audit,
            irq,
            irq_controller,
            ipc,
            aspaces,
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

    /// Resolve the caller's user address space and the physical map
    /// backing it, then run `f` with the borrowed pair while the
    /// per-task registry's read guard is held.
    ///
    /// This is the bridge increment C (`PLAN.md` Stage 7) adds so a
    /// syscall handler can reach the bytes of the calling task's user
    /// memory: the registry maps `caller.task_id` to the
    /// `(&dyn UserAddressSpace, &dyn PhysMap)` pair the
    /// [`rustos_kernel_mem::uaccess`] copy path walks (`AGENTS.md`
    /// §5.4). The closure shape keeps the read guard alive for exactly
    /// the span the borrowed references are used and never hands a
    /// caller's mappings out past it; the registry exposes only
    /// `translate`, so the copy path can read but never mutate them
    /// (`AGENTS.md` §2.4).
    ///
    /// Returns `None` (fail closed, `AGENTS.md` §5.4) when no address
    /// space is registered for the caller — e.g. a kernel task that
    /// never had user mappings, or a `CallerContext` whose task has
    /// already exited and been withdrawn. A handler maps that `None`
    /// to its own stable [`Errno`]; increment D consumes this to drive
    /// `copy_in` / `copy_out` for the deferred `ipc_send` / `ipc_recv`
    /// / `cap_delegate` / `random_get` payloads.
    pub fn with_caller_aspace<R>(
        &self,
        caller: &CallerContext<'_>,
        f: impl FnOnce(&dyn UserAddressSpace, &dyn PhysMap) -> R,
    ) -> Option<R> {
        let registry = self.aspaces.read();
        let (space, physmap) = registry.resolve(caller.task_id)?;
        Some(f(space, physmap))
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
        endpoint: u64,
        _ptr: u64,
        _len: usize,
    ) -> SyscallResult {
        // §5.4: resolve the destination endpoint against the live
        // named-port registry before doing anything else. An endpoint
        // that is not currently bound fails closed with `NotFound` — a
        // real lookup miss now, not a blanket stub; the dispatcher's
        // standard pipeline audits the rejection at this boundary
        // (`PortRegistry::lookup` deliberately does not). For a *bound*
        // endpoint the message body must still be copied in from the
        // caller's address space (`ptr`/`len`); that user-memory
        // copy-in path has not yet landed (`PLAN.md` Stage 5 / Stage
        // 6), so the handler announces the deferral and refuses rather
        // than fabricating a transfer (`AGENTS.md` §15.1 — announce,
        // never stub).
        if !self.ipc.read().contains(EndpointId(endpoint)) {
            return Err(Errno::NotFound);
        }
        self.audit_feature_unavailable(caller, "user_memory_copyin");
        Err(Errno::NotImplemented)
    }

    fn ipc_recv(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        _ptr: u64,
        _len: usize,
    ) -> SyscallResult {
        // Mirror `ipc_send`: resolve the endpoint against the live
        // registry (unbound → `NotFound`), then announce the deferred
        // user-memory copy-out path for a bound endpoint. The receive
        // side needs the same copy primitive to deliver a drained
        // [`rustos_kernel_ipc::Message`] payload into the caller's
        // buffer.
        if !self.ipc.read().contains(EndpointId(endpoint)) {
            return Err(Errno::NotFound);
        }
        self.audit_feature_unavailable(caller, "user_memory_copyin");
        Err(Errno::NotImplemented)
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

    fn clock_get(&self, caller: &CallerContext<'_>) -> SyscallResult {
        // `monotonic_ns` is documented as monotonically non-decreasing
        // per CPU; the dispatcher invokes us on the issuing CPU's
        // process context (`AGENTS.md` §5.4 step 1), so reading from
        // `self.arch.current_cpu()` is the natural source. We do not
        // accept a caller-supplied CPU id — there is no syscall
        // argument for one, and a kernel-trusted lookup is the only
        // sanctioned source.
        let cpu = crate::sched::SchedulerArch::current_cpu(self.arch);
        let ns = self.arch.monotonic_ns(cpu);
        // A full-resolution timer is a side-channel primitive
        // (`AGENTS.md` §19.1). Only a principal explicitly trusted with
        // `CAP_TIME_HIRES` reads the raw nanosecond value; every other
        // caller — including the §19.5 parser sandboxes and untrusted
        // apps — sees the reading floored to
        // `COARSE_CLOCK_GRANULARITY_NS` (security by default,
        // `AGENTS.md` §5.7). Coarsening is value-only: the `clock_get`
        // ABI signature is unchanged, and `coarsen_clock_ns` preserves
        // the per-CPU monotonic-non-decreasing contract the `irq_wait`
        // timeout loop relies on.
        if caller.caps.has(CapabilityId::TIME_HIRES) {
            Ok(ns)
        } else {
            Ok(rustos_abi::coarsen_clock_ns(ns))
        }
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

    fn random_get(
        &self,
        caller: &CallerContext<'_>,
        _buf: u64,
        len: usize,
        _flags: RandomFlags,
    ) -> SyscallResult {
        // Bound the work one call may request (AGENTS.md §22 — a caller
        // needing more issues further requests). This part of the
        // contract is enforceable today, before the reserve is wired.
        if len > RANDOM_REQUEST_MAX_BYTES {
            return Err(Errno::LengthOutOfRange);
        }
        // The kernel output reserve (`rustos_rng::OutputReserve`, §22) and
        // the user-memory copy-out path it would write through are not yet
        // wired into `KernelState` (the per-CPU reserve and its entropy
        // seam land alongside the Stage 6 user-memory work, the same
        // prerequisite `cap_delegate` defers on). Announce the deferral
        // rather than stub randomness (AGENTS.md §15.1) — never return
        // weak or zero bytes (§5.4 fail closed).
        self.audit_feature_unavailable(caller, "random_output_reserve");
        Err(Errno::NotImplemented)
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
    // Mirrors `KernelSyscallHandlers::new`: the same distinct kernel-
    // state borrows threaded explicitly (`AGENTS.md` §2.1 / §4), not a
    // one-use wrapper type (§2.3).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sched: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        arch: &'a A,
        audit: &'a (dyn Sink + Sync),
        irq: &'a IrqTable,
        irq_controller: &'a (dyn IrqController + Sync),
        ipc: &'a RwLock<PortRegistry>,
        aspaces: &'a RwLock<AddressSpaceRegistry>,
    ) -> Self {
        Self {
            handlers: KernelSyscallHandlers::new(
                sched,
                caps,
                arch,
                audit,
                irq,
                irq_controller,
                ipc,
                aspaces,
            ),
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
    use rustos_kernel_ipc::Port;
    use rustos_kernel_irq::{IrqTable, UnsupportedController};
    use rustos_kernel_mem::{
        AddressSpace, Frame, HostPageTable, MapFlags, Page, PhysAddr, SimPhysMap, VirtAddr,
        PAGE_SIZE,
    };
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

    /// Bind an unrestricted port at `endpoint` into `registry`.
    ///
    /// The port accepts any sender (empty `required_send_caps`, so no
    /// `IPC_BIND_PRIVILEGED` is needed) and any receiver, which is all
    /// the `ipc_send` / `ipc_recv` *endpoint-resolution* path under
    /// test cares about; the per-send capability check lives on
    /// `Port::send` and is exercised by `kernel/ipc`'s own tests.
    fn register_port(registry: &RwLock<PortRegistry>, endpoint: u64, sink: &(dyn Sink + Sync)) {
        let creator = make_caps_record(0xB1, &[], sink);
        let port = Port::create(
            EndpointId(endpoint),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            64,
            4,
            sink,
        )
        .expect("unrestricted port creation succeeds");
        // `register`'s error half is `(Box<Port>, Errno)`, which is not
        // `Debug`, so assert on the `Ok` discriminant rather than
        // `.expect()`ing the value.
        assert!(
            registry.write().register(port, sink).is_ok(),
            "first registration of a fresh endpoint succeeds"
        );
    }

    /// `yield_now` against an unknown task surfaces `NotFound`.
    #[test]
    fn yield_now_unknown_task_returns_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(0xDEAD, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(0xDEAD),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
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

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(1, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(1),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        assert_eq!(h.cap_query(&ctx, CapabilityId::FS_MOUNT), Ok(1));
        assert_eq!(h.cap_query(&ctx, CapabilityId::DRV_KERNEL), Ok(0));
    }

    /// `ipc_send` to an endpoint that is not bound in the registry
    /// fails closed with `NotFound` — a real lookup miss. The deferral
    /// audit is *not* emitted: the call never reached the copy-in
    /// branch, so there is no `SyscallFeatureUnavailable` record.
    #[test]
    fn ipc_send_to_unbound_endpoint_is_not_found_without_deferral_audit() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        sink.clear();
        assert_eq!(h.ipc_send(&ctx, 1, 0x1000, 4), Err(Errno::NotFound));
        // The endpoint never resolved, so the handler did not announce
        // the copy-in deferral.
        assert!(sink.event_ids().is_empty());
    }

    /// `ipc_send` to a *bound* endpoint resolves the live port and then
    /// announces the (unlanded) user-memory copy-in path, returning
    /// `NotImplemented` and exactly one `SyscallFeatureUnavailable`
    /// record (`AGENTS.md` §15.1 — announce, never stub).
    #[test]
    fn ipc_send_to_bound_endpoint_defers_copy_in() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        sink.clear();
        assert_eq!(h.ipc_send(&ctx, 1, 0x1000, 4), Err(Errno::NotImplemented));
        assert_eq!(
            sink.event_ids(),
            alloc::vec![AuditEvent::SyscallFeatureUnavailable.id().0]
        );
    }

    /// `ipc_recv` from an unbound endpoint mirrors `ipc_send`: a real
    /// lookup miss is `NotFound` with no deferral audit.
    #[test]
    fn ipc_recv_from_unbound_endpoint_is_not_found_without_deferral_audit() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        sink.clear();
        assert_eq!(h.ipc_recv(&ctx, 1, 0x2000, 8), Err(Errno::NotFound));
        assert!(sink.event_ids().is_empty());
    }

    /// `ipc_recv` from a *bound* endpoint resolves the port and defers
    /// the user-memory copy-out exactly like the send side.
    #[test]
    fn ipc_recv_from_bound_endpoint_defers_copy_out() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        sink.clear();
        assert_eq!(h.ipc_recv(&ctx, 1, 0x2000, 8), Err(Errno::NotImplemented));
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(4, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
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

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);

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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController; // syscall handler does not invoke `fire`
        let permissive = PermissiveController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
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
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(9, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(9),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        let _ = h.irq_bind(&ctx, 5).expect("bind 5");
        let _ = h.irq_bind(&ctx, 6).expect("bind 6");
        assert_eq!(irq.len(), 2);
        // `exit` against an unknown scheduler task returns
        // `Errno::NotFound`, but the IRQ release still happens
        // (the ordering documented in the handler's source).
        let _ = h.exit(&ctx, 0);
        assert!(irq.is_empty(), "exit must drop every binding the task held");
    }

    /// A caller holding `CAP_TIME_HIRES` reads `KernelArch::monotonic_ns`
    /// at full resolution and observes strictly-increasing values across
    /// consecutive calls (the `TestArch` impl is strictly monotonic).
    #[test]
    fn clock_get_hires_returns_raw_monotonic_ns_from_arch() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(6, &[CapabilityId::TIME_HIRES], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        let a = h.clock_get(&ctx).expect("first read");
        let b = h.clock_get(&ctx).expect("second read");
        // Full resolution: consecutive single-tick reads are distinct.
        assert!(b > a, "expected b > a, got a={a} b={b}");
    }

    /// A caller *without* `CAP_TIME_HIRES` reads the monotonic clock
    /// floored to `COARSE_CLOCK_GRANULARITY_NS`, so sub-granularity
    /// detail is hidden (`AGENTS.md` §19.1) while the reading stays
    /// monotonically non-decreasing.
    #[test]
    fn clock_get_without_hires_is_coarsened() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(6, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        let g = rustos_abi::COARSE_CLOCK_GRANULARITY_NS;

        // Stage a known raw reading; the next `monotonic_ns` returns
        // `value + 1`, so a raw of `12_345` must floor to `12_000`.
        arch.set_monotonic_ns(12_344);
        let coarse = h.clock_get(&ctx).expect("coarsened read");
        assert_eq!(coarse, 12_000, "raw 12_345 must floor to 12_000");

        // Across many sub-granularity ticks the value never decreases
        // and is always a multiple of the granularity.
        arch.set_monotonic_ns(0);
        let mut last = 0;
        for _ in 0..(3 * g) {
            let v = h.clock_get(&ctx).expect("coarsened read");
            assert_eq!(v % g, 0, "coarsened reading must be a multiple of {g}");
            assert!(v >= last, "coarsened reading must not decrease");
            last = v;
        }
        assert!(last >= g, "after >{g} ticks at least one boundary crossed");
    }

    /// The same underlying instant is hidden from an untrusted caller
    /// but visible to a `CAP_TIME_HIRES` holder.
    #[test]
    fn clock_get_hires_sees_more_than_coarsened_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let hires = make_caps_record(6, &[CapabilityId::TIME_HIRES], sink);
        let plain = make_caps_record(7, &[], sink);
        let hires_ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &hires,
        };
        let plain_ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &plain,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);

        arch.set_monotonic_ns(7_000);
        let raw = h.clock_get(&hires_ctx).expect("hires read"); // 7_001
        arch.set_monotonic_ns(7_000);
        let coarse = h.clock_get(&plain_ctx).expect("coarse read"); // 7_000
        assert_eq!(raw, 7_001);
        assert_eq!(coarse, 7_000);
        assert!(raw > coarse, "hires caller resolves the sub-µs detail");
    }

    /// `random_get` refuses an over-large request up front with
    /// `LengthOutOfRange`, before any deferral audit is emitted.
    #[test]
    fn random_get_rejects_request_above_cap() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(11, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(11),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        sink.clear();
        assert_eq!(
            h.random_get(
                &ctx,
                0x4000,
                rustos_abi::RANDOM_REQUEST_MAX_BYTES + 1,
                RandomFlags::empty()
            ),
            Err(Errno::LengthOutOfRange)
        );
        // Refused before reaching the deferred-feature branch.
        assert!(sink.event_ids().is_empty());
    }

    /// An in-range `random_get` defers the (unlanded) output-reserve /
    /// user-memory copy-out, announcing the deferral exactly like
    /// `cap_delegate` rather than returning weak or zero bytes
    /// (`AGENTS.md` §15.1, §22).
    #[test]
    fn random_get_defers_reserve_and_audits_feature_unavailable() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(12, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(12),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        sink.clear();
        assert_eq!(
            h.random_get(&ctx, 0x4000, 32, RandomFlags::NON_BLOCKING),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            sink.event_ids(),
            alloc::vec![AuditEvent::SyscallFeatureUnavailable.id().0]
        );
    }

    /// Page `n`'s base virtual address, as a [`Page`].
    fn page(n: u64) -> Page {
        Page::from_addr(VirtAddr::new(n * PAGE_SIZE as u64)).expect("aligned page")
    }

    /// A boxed user address space with page `n` → frame `frame` mapped
    /// `USER | READ`, behind the object-safe trait the registry stores.
    fn user_space(n: u64, frame: usize) -> Box<dyn UserAddressSpace + Send + Sync> {
        let mut space = AddressSpace::new(HostPageTable::new());
        space
            .map(page(n), Frame(frame), MapFlags::READ | MapFlags::USER)
            .expect("mapped");
        Box::new(space)
    }

    /// A boxed single-page direct physical map for the registry entry.
    fn sim_map() -> Box<dyn PhysMap + Send + Sync> {
        Box::new(SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE))
    }

    /// `with_caller_aspace` resolves a registered caller to its address
    /// space and runs the closure against the borrowed pair — the
    /// increment-C bridge from `caller.task_id` to the user mappings the
    /// copy path walks.
    #[test]
    fn with_caller_aspace_runs_closure_against_registered_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        aspaces
            .write()
            .register(SecTaskId(5), user_space(1, 9), sim_map())
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(5, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(5),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        // The closure sees the caller's own address space: page 1
        // resolves to frame 9 with the flags it was mapped with.
        let resolved = h.with_caller_aspace(&ctx, |space, _physmap| space.translate(page(1)));
        assert_eq!(
            resolved,
            Some(Some((Frame(9), MapFlags::READ | MapFlags::USER)))
        );
    }

    /// `with_caller_aspace` fails closed with `None` (never invoking the
    /// closure) when the caller has no registered address space — a
    /// kernel task, or a task already withdrawn on `exit` (`AGENTS.md`
    /// §5.4).
    #[test]
    fn with_caller_aspace_returns_none_for_unregistered_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(6, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        let mut ran = false;
        let resolved = h.with_caller_aspace(&ctx, |_space, _physmap| {
            ran = true;
            0u8
        });
        assert_eq!(resolved, None);
        assert!(!ran, "the closure must not run when no entry resolves");
    }

    /// Each caller resolves to *its own* address space: the bridge keys
    /// strictly on `caller.task_id`, never leaking one task's mappings
    /// to another.
    #[test]
    fn with_caller_aspace_resolves_only_the_calling_task() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        aspaces
            .write()
            .register(SecTaskId(1), user_space(1, 100), sim_map())
            .expect("task 1 registers");
        aspaces
            .write()
            .register(SecTaskId(2), user_space(1, 200), sim_map())
            .expect("task 2 registers");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps1 = make_caps_record(1, &[], sink);
        let caps2 = make_caps_record(2, &[], sink);
        let ctx1 = CallerContext {
            task_id: SecTaskId(1),
            caps: &caps1,
        };
        let ctx2 = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps2,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces);
        let frame1 = h
            .with_caller_aspace(&ctx1, |space, _| space.translate(page(1)).map(|(f, _)| f))
            .expect("task 1 resolves");
        let frame2 = h
            .with_caller_aspace(&ctx2, |space, _| space.translate(page(1)).map(|(f, _)| f))
            .expect("task 2 resolves");
        assert_eq!(frame1, Some(Frame(100)));
        assert_eq!(frame2, Some(Frame(200)));
    }
}
