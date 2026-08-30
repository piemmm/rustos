//! Bin-crate-owned kernel-side publication point for the production
//! syscall dispatcher.
//!
//! Stage 2.7 follow-up (f4) of `PLAN.md`. The production
//! [`tairix_kernel_syscall::Dispatcher`] cannot be installed at the
//! architecture-port's syscall-trampoline level directly because the
//! trampoline expects a bare `extern "C" fn` callback that takes only
//! a syscall number and an argument frame — there is no room in that
//! ABI for borrowed kernel state. The
//! `tairix_arch_x86_64::syscall_entry::set_dispatch_callback` step
//! therefore stays as it is in (c7-bin): it installs the binary's own
//! `extern "C"` callback **before** `syscall` is enabled (the
//! arch-level fail-closed ordering contract; see
//! `tairix_arch_x86_64::syscall_entry` rustdoc).
//!
//! What was missing until (f4) is the *kernel-side* publication path:
//! a place [`crate::kernel_main`] can hand its [`DispatchHook`] (built
//! from `KernelState`'s scheduler, capability table, arch port, and
//! audit sink) so that the binary's `extern "C"` callback can find it
//! at syscall time. That place is one [`DispatchCallbackSlot`] per
//! `tairix-kernel` binary, owned by the bin crate in a `'static`
//! storage (not a global *mutable* static), referenced from
//! [`crate::BootInfo`], and published
//! at the new `Phase::Syscall` step `kernel_main` runs between
//! `Phase::Sched` and `Phase::Ipc`.
//!
//! # Why a [`OnceCell`]
//!
//! The slot is installed exactly once, by `kernel_main`, on the BSP,
//! before `syscall` may ever fire — the arch-level
//! `set_dispatch_callback` is invoked before `syscall` is enabled
//! (see `kernel/tairix-kernel::dispatch` rustdoc and
//! "fail closed"). The slot only ever transitions
//! `Empty → Installed`; no re-installation, no mutation after
//! publish. [`tairix_sync::OnceCell`] is exactly that
//! transition with the right memory ordering for cross-CPU
//! observation, no extra primitive needed
//! (no bloat).
//!
//! # Concurrency
//!
//! [`DispatchCallbackSlot`] is [`Sync`]. `install_dispatcher` is
//! called from the BSP exactly once during boot; the `set` it
//! performs is happens-before every subsequent `get` on any CPU
//! because [`OnceCell::set`] is a release store and
//! [`OnceCell::get`] is an acquire load (documented in
//! `kernel/sync::once`). Per-CPU syscalls observe `Some(hook)` from
//! the moment `set` returns.

use tairix_arch_api::backtrace::UserRegisterFrame;
use tairix_kernel_syscall::{RawArgs, SyscallResult};
use tairix_sync::OnceCell;

/// Result of one [`DispatchHook::dispatch`] call.
///
/// The hook's caller (the bin-crate `extern "C"` syscall-dispatch
/// callback) needs to distinguish two failure modes:
///
/// * [`Self::Returned`] — a normal syscall outcome. The bin crate
///   encodes the inner [`SyscallResult`] back into the architecture's
///   syscall-return register and returns to user space.
/// * [`Self::NoCallerContext`] — the hook could not identify the
///   caller (no task currently running on this CPU; no capability
///   record for the running task). The syscall ran nothing and granted
///   nothing, so the fail-closed action is to kill the *offending EL0
///   context* and hand the CPU back to the scheduler — the same
///   disposition a wild user fault gets. Halting the CPU instead would
///   let an unattributable trap strand whatever lock and device IRQ that
///   CPU held, permanently and unrecoverably. The hook has already
///   emitted the audit record, so the bin crate only performs the kill.
///
/// The split exists because the audit-record emission belongs in
/// `kernel/core` (which owns the audit-event catalogue) while the
/// arch-coupled suspend belongs in the bin crate — neither side can
/// own both responsibilities without bloating its dependency surface.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum DispatchOutcome {
    /// The hook ran to completion and produced a normal
    /// [`SyscallResult`].
    Returned(SyscallResult),
    /// Caller identification failed; the bin crate must kill this CPU's
    /// EL0 context and return the CPU to the scheduler.
    ///
    /// `cpu` keys the per-CPU resume handle, which is published against
    /// the *CPU* rather than the scheduler's current-task slot — so the
    /// kill still reaches the running task precisely when that slot is
    /// the thing that has gone missing.
    NoCallerContext {
        /// The CPU whose EL0 context could not be attributed.
        cpu: u32,
    },
    /// The syscall rescheduled its caller (a resumable EL0 task that
    /// yielded, parked, or exited; `plans/SPAWN.md` SP2).
    ///
    /// The caller is a [`crate::kthread`] user task running on its own
    /// kernel stack. Rather than return to user space immediately, the
    /// bin-crate callback must suspend it back to the scheduler — it
    /// calls [`crate::reschedule_current`] with `cpu` and `action`,
    /// which switches to the scheduler's dispatch context and returns
    /// only when this task is next dispatched. *Then* it encodes
    /// `result` into the syscall-return register and resumes user space
    /// (an [`RescheduleAction::Exit`] task is never dispatched again, so
    /// the resume after it never happens; the encode is dead on that
    /// path, which is why `result` is still carried — the bin crate need
    /// not special-case the action).
    ///
    /// `cpu` is the dispatching CPU the hook identified the caller on; it keys the per-CPU resume handle so the
    /// suspend reaches *this* CPU's running task.
    Reschedule {
        /// The syscall-return value to encode once the caller is resumed
        /// (ignored for [`RescheduleAction::Exit`], which never resumes).
        result: SyscallResult,
        /// What the scheduler should do with the caller.
        action: RescheduleAction,
        /// The CPU the caller was identified on; keys the per-CPU resume
        /// handle.
        cpu: u32,
    },
}

/// What a rescheduling syscall ([`DispatchOutcome::Reschedule`]) asks the
/// scheduler to do with its caller.
///
/// A self-contained mirror of the scheduler's `TaskAction` kept on the
/// dispatch-callback ABI so the bin-crate callback and
/// [`crate::dispatch_slot`] never depend on `kernel/sched`'s vocabulary;
/// [`crate::reschedule_current`] maps it onto the scheduler's own
/// `TaskAction` at the one boundary that needs it (one
/// definition, decoded at the edge).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RescheduleAction {
    /// Re-enqueue the caller at its current priority and run something
    /// else (the `yield` syscall).
    Yield,
    /// Park the caller until an external wake (no yet-shipped syscall
    /// produces this; carried for completeness so a future blocking
    /// syscall needs no ABI change).
    Park,
    /// Terminate the caller; it is never dispatched again (the `exit`
    /// syscall).
    Exit,
}

/// Subsystem hook the binary's `extern "C"` syscall-dispatch callback
/// forwards every syscall through.
///
/// One implementation per kernel build:
///
/// * Production: `KernelDispatchHook` in
///   [`crate::syscalls`], which owns borrows into
///   `KernelState` (scheduler + capability table + arch + audit) and
///   wraps a [`tairix_kernel_syscall::Dispatcher`].
/// * Tests substitute their own implementation against `TestArch`.
///
/// The trait is intentionally minimal: the bin-crate callback's job
/// is to *find* the hook (via [`DispatchCallbackSlot::get`]) and to
/// hand it the architecture-supplied [`RawArgs`]; the hook owns
/// everything from "identify the caller" through to
/// `Dispatcher::dispatch`'s return value. Keeping that split here
/// means the bin crate never reaches into `kernel/sec` / `kernel/sched`
/// / `kernel/syscall` internals on the syscall hot path — those
/// dependencies stay confined to `kernel/core` (no interface creep).
pub trait DispatchHook: Sync {
    /// Run one syscall and return its result.
    ///
    /// `raw_number` is the architecture's syscall-number register whole and
    /// unnarrowed, exactly as the trampoline received it;
    /// `tairix_kernel_syscall::Dispatcher::dispatch` validates the full
    /// value, so no caller-supplied bit is dropped on the way in. `args` is
    /// the caller's register tuple already reinterpreted as a [`RawArgs`] by
    /// the trampoline.
    ///
    /// The implementation:
    ///
    /// 1. Identifies the caller (per-CPU `current_task` + per-task
    ///    capability lookup).
    /// 2. Forwards through [`tairix_kernel_syscall::Dispatcher::dispatch`],
    ///    which performs the remaining four steps.
    ///
    /// On a caller-identification failure (no task currently running
    /// on this CPU; capability record missing for the running task)
    /// the implementation must:
    ///
    /// 1. Emit a stable, security-relevant audit record naming the
    ///    failure (the production implementation uses
    ///    [`crate::AuditEvent::SyscallNoCallerContext`]).
    /// 2. Return [`DispatchOutcome::NoCallerContext`], so the bin-crate
    ///    callback kills the unattributable EL0 context (fail closed)
    ///    and leaves the CPU running.
    ///
    /// Never panic, never silently succeed.
    fn dispatch(&self, raw_number: u64, args: RawArgs) -> DispatchOutcome;

    /// Handle a user-mode data abort at `fault_va` on the calling CPU:
    /// resolve it as demand-paged file backing (a `file_map` region) or
    /// terminate the faulting task.
    ///
    /// `write` is the port-attested access direction (aarch64 `ESR.WnR`,
    /// riscv64 store/AMO `scause`, x86_64 `#PF` error-code `W/R`). File
    /// mappings are read-only, so a write is never resolved — it is fatal
    /// to the task — but it still flows through this one seam so a store
    /// to a read-only mapping (or any wild write) kills the *task* and
    /// never the CPU.
    ///
    /// The architecture port's user-fault path calls this before treating
    /// the abort as fatal, and acts on the returned
    /// [`UserFaultOutcome`]:
    ///
    /// * [`UserFaultOutcome::Resolved`] — the faulting page was made
    ///   resident (or already was, a benign concurrent resolution): the
    ///   port returns to the task and the retried access succeeds.
    /// * [`UserFaultOutcome::Terminated`] — the address is not
    ///   demand-paged backing of the current task's (a wild access, a page
    ///   at/past end-of-file — the `SIGBUS` analogue — an unresolvable
    ///   read/OOM, or **any write**): the hook has already recorded the crash exit code and
    ///   reclaimed the task's kernel resources, and the port suspends the
    ///   task with [`crate::reschedule_current`] and
    ///   [`RescheduleAction::Exit`] on the carried `cpu` — the task is
    ///   reaped and never runs again, and the rest of the system is
    ///   untouched (fail closed without collateral damage).
    /// * [`UserFaultOutcome::Unhandled`] — the fault cannot even be
    ///   attributed to a task (no current task on this CPU): the port
    ///   falls back to its fatal path (halt), exactly as before.
    ///
    /// `regs` is the faulting *user* register frame the architecture port
    /// captured at trap entry, threaded through so the hook can record a
    /// post-mortem crash record (identity, load-relative backtrace, register
    /// snapshot) for a killed task. It is `None` on a port that does not (yet)
    /// save the frame, or on a fault the port cannot attribute; the resolver
    /// then still classifies and terminates, just without a backtrace.
    ///
    /// The default refuses every fault as [`UserFaultOutcome::Unhandled`],
    /// so a hook built without a file-mapping resolver can never fabricate
    /// memory.
    fn resolve_user_fault(
        &self,
        _fault_va: u64,
        _write: bool,
        _regs: Option<&UserRegisterFrame>,
    ) -> UserFaultOutcome {
        UserFaultOutcome::Unhandled
    }

    /// Terminate the task currently running on the calling CPU because it
    /// took an exception the architecture port cannot resolve and must not
    /// retry — an illegal/unallocated instruction (aarch64 `EC=0`), a PC/SP
    /// alignment fault, or any other synchronous EL0 exception that is
    /// neither a syscall nor a demand-pageable abort.
    ///
    /// Unlike [`resolve_user_fault`](Self::resolve_user_fault) this makes
    /// **no** resolution attempt: retrying the faulting instruction would
    /// re-take the same exception forever (the instruction is genuinely
    /// invalid, or its page is intentionally non-executable), so the only
    /// correct action is to kill the task. The implementation records the
    /// crash exit (identity, `fault_pc`-relative backtrace, register
    /// snapshot from `regs`) and reclaims the task's kernel resources
    /// exactly as the fatal branch of `resolve_user_fault` does.
    ///
    /// This exists so a user task's own bad instruction costs only that
    /// task, never the whole CPU: without it the port's fatal fallthrough
    /// parks the core forever (interrupts masked), turning one task's fault
    /// into a system-wide hard lockup.
    ///
    /// * [`UserFaultOutcome::Terminated`] — the task's exit is recorded and
    ///   its resources reclaimed; the port suspends it with
    ///   [`RescheduleAction::Exit`] on the carried `cpu`.
    /// * [`UserFaultOutcome::Unhandled`] — no task could be attributed (no
    ///   current task on this CPU); the port falls back to its fatal path.
    ///
    /// The default refuses (returns [`UserFaultOutcome::Unhandled`]), so a
    /// hook built without the termination path can never silently succeed.
    fn terminate_user_fault(
        &self,
        _fault_pc: u64,
        _regs: Option<&UserRegisterFrame>,
    ) -> UserFaultOutcome {
        UserFaultOutcome::Unhandled
    }
}

/// Disposition of one [`DispatchHook::resolve_user_fault`] call — what the
/// architecture port must do with the faulting task (see the method docs).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum UserFaultOutcome {
    /// The faulting page is now resident; return to the task and retry.
    Resolved,
    /// The fault was fatal to the task: its exit is recorded and its
    /// kernel resources reclaimed; suspend it with
    /// [`RescheduleAction::Exit`] on `cpu`.
    Terminated {
        /// The CPU the task was identified on; keys the per-CPU resume
        /// handle exactly as [`DispatchOutcome::Reschedule`] does.
        cpu: u32,
    },
    /// No task could be attributed; the port falls back to its fatal path.
    Unhandled,
}

/// Kernel-side publication point for the production [`DispatchHook`].
///
/// Owned by the bin crate at a `'static` storage. `kernel_main`
/// receives a `&'static DispatchCallbackSlot` through [`crate::BootInfo`]
/// and calls [`Self::install_dispatcher`] exactly once during the
/// `Syscall` init phase; the bin crate's `extern "C"` dispatch
/// callback calls [`Self::get`] on every syscall and forwards through
/// the returned hook.
///
/// # Lifecycle
///
/// `Empty` ← `new` / construction. `Installed` ← exactly one
/// successful [`Self::install_dispatcher`] call. There is no
/// transition out of `Installed` — the kernel never tears the
/// dispatcher down for the lifetime of the running kernel.
pub struct DispatchCallbackSlot {
    hook: OnceCell<&'static (dyn DispatchHook + 'static)>,
}

impl DispatchCallbackSlot {
    /// Construct an empty slot.
    ///
    /// `const fn`, so the bin crate can declare its own slot as a
    /// `static` without resorting to lazy initialisation
    /// (no global mutable static; the slot is
    /// immutable at the type level, with a controlled set-once
    /// publish through [`Self::install_dispatcher`]).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hook: OnceCell::new(),
        }
    }

    /// Publish `hook` as the production dispatch implementation.
    ///
    /// Called by [`crate::kernel_main`] in the `Syscall` init phase,
    /// strictly between `Phase::Sched` and `Phase::Ipc`. The slot is
    /// designed to accept a hook exactly once per boot; a second
    /// successful call is a programmer error and the boot path
    /// surfaces it as [`crate::InitError::DispatcherAlreadyInstalled`].
    ///
    /// # Errors
    ///
    /// Returns [`AlreadyInstalledError`] if the slot has already
    /// accepted a hook. The caller is responsible for converting that
    /// into the appropriate boot-phase failure and halting — no
    /// silent retry (fail closed).
    pub fn install_dispatcher(
        &self,
        hook: &'static (dyn DispatchHook + 'static),
    ) -> Result<(), AlreadyInstalledError> {
        // `OnceCell::set` returns `Err(AlreadySetError(value))` if a
        // value was already published. We deliberately *do not*
        // surface the rejected `value` back to the caller — the
        // bin-crate slot's `hook` is type-erased behind a trait
        // object and an external consumer could not act on it
        // meaningfully. Reporting "already installed" is the security
        // signal; the rejected hook reference is dropped.
        self.hook.set(hook).map_err(|_| AlreadyInstalledError)
    }

    /// Return the installed hook, or `None` if the slot is still
    /// empty.
    ///
    /// The bin-crate `extern "C"` dispatch callback calls this on
    /// every syscall. An empty slot means a syscall fired before
    /// `kernel_main` finished the `Syscall` init phase — which the
    /// arch-level `set_dispatch_callback` ordering contract makes
    /// impossible in a correctly-ordered boot, but the callback is
    /// still required to handle it.
    ///
    /// The returned reference is `'static`: hooks live for the
    /// lifetime of the running kernel by construction in
    /// `kernel_main` (the kernel never returns from `kernel_main`'s
    /// halt).
    #[must_use]
    pub fn get(&self) -> Option<&'static (dyn DispatchHook + 'static)> {
        // `OnceCell::get` returns `Ok(Some(&T))`, `Ok(None)` (still
        // empty), or `Err(PoisonError)` (initialiser previously
        // failed). The slot never uses the failing initialiser path,
        // so poisoning is structurally impossible here; we fold
        // `Err` and `Ok(None)` into the same `None` so callers see a
        // single fail-closed branch (no
        // `unwrap`/`expect`).
        match self.hook.get() {
            Ok(Some(hook)) => Some(*hook),
            Ok(None) | Err(_) => None,
        }
    }

    /// `true` once [`Self::install_dispatcher`] has succeeded.
    ///
    /// Host tests use this to assert the registration-ordering
    /// invariant (the `Syscall` phase installs the hook before the
    /// `BootCompleted` audit record fires).
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.hook.is_initialised()
    }
}

impl Default for DispatchCallbackSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for DispatchCallbackSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DispatchCallbackSlot")
            .field("installed", &self.is_installed())
            .finish()
    }
}

/// [`DispatchCallbackSlot::install_dispatcher`] rejected a second
/// publish.
///
/// The slot is set-once per boot; a second call indicates a
/// programmer error (double `kernel_main` entry, double registration
/// from test glue). The boot path converts this into
/// [`crate::InitError::DispatcherAlreadyInstalled`] and halts — no
/// silent recovery.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct AlreadyInstalledError;

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_kernel_syscall::RawArgs;

    /// Minimal hook that records the last `(raw_number, args)` it was
    /// handed. Used to verify the slot returns the registered hook
    /// untouched.
    struct RecordingHook {
        log: tairix_sync::SpinLock<alloc::vec::Vec<(u64, RawArgs)>>,
    }

    impl RecordingHook {
        fn new() -> Self {
            Self {
                log: tairix_sync::SpinLock::new(alloc::vec::Vec::new()),
            }
        }
    }

    impl DispatchHook for RecordingHook {
        fn dispatch(&self, raw_number: u64, args: RawArgs) -> DispatchOutcome {
            self.log.lock().push((raw_number, args));
            // The trait contract permits any `DispatchOutcome`; the
            // tests below only assert against the recorded call.
            DispatchOutcome::Returned(Ok(raw_number))
        }
    }

    #[test]
    fn new_slot_is_empty() {
        let slot = DispatchCallbackSlot::new();
        assert!(!slot.is_installed());
        assert!(slot.get().is_none());
    }

    #[test]
    fn install_then_get_returns_same_hook_ref() {
        let hook: &'static RecordingHook =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(RecordingHook::new()));
        let slot = DispatchCallbackSlot::new();
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("first install succeeds");
        assert!(slot.is_installed());
        let got = slot.get().expect("hook visible after install");
        // Forward a call through the trait object and confirm the
        // recording hook saw it.
        let r = got.dispatch(0x42, RawArgs::ZERO);
        assert_eq!(r, DispatchOutcome::Returned(Ok(0x42)));
        let logged = hook.log.lock().clone();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].0, 0x42);
    }

    #[test]
    fn install_twice_returns_already_installed_error() {
        let h1: &'static RecordingHook =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(RecordingHook::new()));
        let h2: &'static RecordingHook =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(RecordingHook::new()));
        let slot = DispatchCallbackSlot::new();
        slot.install_dispatcher(h1 as &'static dyn DispatchHook)
            .expect("first install succeeds");
        let err = slot
            .install_dispatcher(h2 as &'static dyn DispatchHook)
            .expect_err("second install must fail");
        assert_eq!(err, AlreadyInstalledError);
        // The originally-installed hook is still observable; the
        // second publish never overwrote it.
        let got = slot.get().expect("hook still installed");
        let _ = got.dispatch(7, RawArgs::ZERO);
        assert_eq!(h1.log.lock().len(), 1);
        assert!(h2.log.lock().is_empty());
    }

    #[test]
    fn default_is_empty() {
        let slot: DispatchCallbackSlot = DispatchCallbackSlot::default();
        assert!(!slot.is_installed());
    }

    #[test]
    fn debug_reports_installation_state() {
        let slot = DispatchCallbackSlot::new();
        let s = alloc::format!("{slot:?}");
        assert!(s.contains("installed: false"), "got: {s}");
        let hook: &'static RecordingHook =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(RecordingHook::new()));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        let s = alloc::format!("{slot:?}");
        assert!(s.contains("installed: true"), "got: {s}");
    }
}
