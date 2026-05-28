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

use rustos_abi::{CapabilityId, Errno};
use rustos_kernel_sched::Scheduler;
use rustos_kernel_sec::{CapTable, TaskId as SecTaskId};
use rustos_kernel_sync::RwLock;
use rustos_kernel_syscall::{CallerContext, SyscallHandlers, SyscallResult};
use rustos_log::{Field, Sink};
use rustos_util::fmt::format_hex_u64;

use crate::audit::AuditEvent;
use crate::bootinfo::KernelArch;

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
    ) -> Self {
        Self {
            sched,
            caps,
            arch,
            audit,
        }
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
            Err(rustos_kernel_sched::SchedError::NoSuchTask) => Err(Errno::NotFound),
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
        // Order matters: drop the capability record *before* the
        // scheduler removes the task so a concurrent `cap_query`
        // racing this `exit` can never observe a task that the
        // scheduler still believes exists but whose caps have
        // vanished. The CapTable write lock is held only for the
        // duration of the `remove` call.
        let task = caller.task_id;
        let _ = self.caps.write().remove(task);
        match self.sched.exit(task.0) {
            Ok(()) => Ok(0),
            Err(rustos_kernel_sched::SchedError::NoSuchTask) => Err(Errno::NotFound),
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
        let cpu = rustos_kernel_sched::SchedulerArch::current_cpu(self.arch);
        Ok(self.arch.monotonic_ns(cpu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_arch::TestArch;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use rustos_abi::{CapabilityId, Errno};
    use rustos_caps::CapabilitySet;
    use rustos_kernel_sched::SchedulerConfig;
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
        let caps = make_caps_record(0xDEAD, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(0xDEAD),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);
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

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);
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
        let caps = make_caps_record(1, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(1),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);
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
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);
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
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);
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
        let caps = make_caps_record(4, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);
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

        // Register target task 10 with FS_MOUNT.
        let record = make_caps_record(10, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(record);

        let caller_caps = make_caps_record(5, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(5),
            caps: &caller_caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);

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
        let caps = make_caps_record(6, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(&sched, &table, &arch, sink);
        let a = h.clock_get(&ctx).expect("first read");
        let b = h.clock_get(&ctx).expect("second read");
        assert!(b > a, "expected b > a, got a={a} b={b}");
    }
}
