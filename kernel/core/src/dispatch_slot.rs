//! Bin-crate-owned kernel-side publication point for the production
//! syscall dispatcher.
//!
//! Stage 2.7 follow-up (f4) of `PLAN.md`. The production
//! [`rustos_kernel_syscall::Dispatcher`] cannot be installed at the
//! architecture-port's syscall-trampoline level directly because the
//! trampoline expects a bare `extern "C" fn` callback that takes only
//! a syscall number and an argument frame — there is no room in that
//! ABI for borrowed kernel state. The
//! `rustos_arch_x86_64::syscall_entry::set_dispatch_callback` step
//! therefore stays as it is in (c7-bin): it installs the binary's own
//! `extern "C"` callback **before** `syscall` is enabled (the
//! arch-level fail-closed ordering contract; see
//! `rustos_arch_x86_64::syscall_entry` rustdoc).
//!
//! What was missing until (f4) is the *kernel-side* publication path:
//! a place [`crate::kernel_main`] can hand its [`DispatchHook`] (built
//! from `KernelState`'s scheduler, capability table, arch port, and
//! audit sink) so that the binary's `extern "C"` callback can find it
//! at syscall time. That place is one [`DispatchCallbackSlot`] per
//! `rustos-kernel` binary, owned by the bin crate in a `'static`
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
//! (see `kernel/rustos-kernel::dispatch` rustdoc and
//! "fail closed"). The slot only ever transitions
//! `Empty → Installed`; no re-installation, no mutation after
//! publish. [`rustos_sync::OnceCell`] is exactly that
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

use rustos_kernel_syscall::{RawArgs, SyscallResult};
use rustos_sync::OnceCell;

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
///   record for the running task). The charter mandates the
///   bin crate **fail closed** here, exactly the way the
///   `fail_closed_dispatch` callback did before (f5): emit a
///   security record and halt the CPU forever. The hook has already
///   emitted the audit record before returning this variant, so the
///   bin crate only needs to perform the halt.
///
/// The split exists because the audit-record emission belongs in
/// `kernel/core` (which owns the audit-event catalogue) while the
/// `halt` belongs in the arch-coupled bin crate — neither side can
/// own both responsibilities without bloating its dependency surface.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum DispatchOutcome {
    /// The hook ran to completion and produced a normal
    /// [`SyscallResult`].
    Returned(SyscallResult),
    /// Caller identification failed; the bin crate must halt the CPU.
    NoCallerContext,
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
///   wraps a [`rustos_kernel_syscall::Dispatcher`].
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
    /// `raw_number` is the bottom 16 bits of the architecture's
    /// syscall-number register, exactly as the trampoline received
    /// it (`rustos_kernel_syscall::Dispatcher::dispatch` validates
    /// it). `args` is the caller's register tuple already
    /// reinterpreted as a [`RawArgs`] by the trampoline.
    ///
    /// The implementation:
    ///
    /// 1. Identifies the caller (per-CPU `current_task` + per-task
    ///    capability lookup).
    /// 2. Forwards through [`rustos_kernel_syscall::Dispatcher::dispatch`],
    ///    which performs the remaining four steps.
    ///
    /// On a caller-identification failure (no task currently running
    /// on this CPU; capability record missing for the running task)
    /// the implementation must:
    ///
    /// 1. Emit a stable, security-relevant audit record naming the
    ///    failure (the production implementation uses
    ///    [`crate::AuditEvent::SyscallNoCallerContext`]).
    /// 2. Return [`DispatchOutcome::NoCallerContext`], so the
    ///    bin-crate callback halts the CPU forever (fail closed).
    ///
    /// Never panic, never silently succeed.
    fn dispatch(&self, raw_number: u16, args: RawArgs) -> DispatchOutcome;
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
    use rustos_kernel_syscall::RawArgs;

    /// Minimal hook that records the last `(raw_number, args)` it was
    /// handed. Used to verify the slot returns the registered hook
    /// untouched.
    struct RecordingHook {
        log: rustos_sync::SpinLock<alloc::vec::Vec<(u16, RawArgs)>>,
    }

    impl RecordingHook {
        fn new() -> Self {
            Self {
                log: rustos_sync::SpinLock::new(alloc::vec::Vec::new()),
            }
        }
    }

    impl DispatchHook for RecordingHook {
        fn dispatch(&self, raw_number: u16, args: RawArgs) -> DispatchOutcome {
            self.log.lock().push((raw_number, args));
            // The trait contract permits any `DispatchOutcome`; the
            // tests below only assert against the recorded call.
            DispatchOutcome::Returned(Ok(u64::from(raw_number)))
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
