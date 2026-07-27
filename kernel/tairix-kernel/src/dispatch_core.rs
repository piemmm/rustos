//! Architecture-neutral syscall-dispatch helpers shared by every
//! per-architecture dispatch callback (`crate::x86_64::dispatch` on
//! x86_64, `crate::aarch64::dispatch` on aarch64).
//!
//! The per-architecture modules differ only in two arch-specific facts:
//! the `SyscallDispatchFn` typedef the arch port's syscall trampoline
//! expects, and the bottom-typed halt the fail-closed branch jumps to.
//! Everything else — reading the kernel-stack `[u64; SYSCALL_MAX_ARGS]`
//! frame into a [`RawArgs`], encoding a [`tairix_kernel_syscall::SyscallResult`]
//! into the syscall-return register, and forwarding one syscall through
//! a slot's resident `DispatchHook` — is identical across architectures
//! and lives here exactly once (no duplication).
//!
//! The module is host-testable and un-gated: the per-architecture
//! callbacks are thin wrappers that supply the arch `SyscallDispatchFn`
//! coercion and the halt, while the substantive logic is unit-tested
//! here once.

use tairix_abi::SYSCALL_MAX_ARGS;
use tairix_arch_api::backtrace::UserRegisterFrame;
use tairix_kernel_core::{
    reschedule_current, DispatchCallbackSlot, DispatchOutcome, RescheduleAction, UserFaultOutcome,
};
use tairix_kernel_syscall::RawArgs;

/// Bridge the kernel-stack `[u64; SYSCALL_MAX_ARGS]` frame to a
/// [`RawArgs`] value.
///
/// The (c7-arch) compile-time `_RAW_ARGS_LAYOUT_MATCHES_ARRAY`
/// assertion in `tairix_kernel_syscall::table` pins [`RawArgs`]'s
/// `#[repr(transparent)]` over `[u64; SYSCALL_MAX_ARGS]`. This
/// function exists so the host-side tests can verify the
/// reinterpretation round-trip without invoking the freestanding
/// syscall instruction.
///
/// # Safety
///
/// `args_ptr` must point at a fully-initialised
/// `[u64; SYSCALL_MAX_ARGS]` that lives for the duration of the call.
/// In production the arch port's trampoline lays this frame out on the
/// kernel stack and the array lives at least until the dispatch
/// callback returns.
#[must_use]
pub unsafe fn read_raw_args(args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> RawArgs {
    // SAFETY: `args_ptr` is documented to point at a valid frame; the
    // (c7-arch) `_RAW_ARGS_LAYOUT_MATCHES_ARRAY` assertion guarantees
    // the cast is a no-op at the byte level. We deliberately copy by
    // value so the caller can drop the source frame as soon as we
    // return.
    let arr = unsafe { *args_ptr };
    RawArgs(arr)
}

/// Encode a [`tairix_kernel_syscall::SyscallResult`] into the
/// architecture's syscall-return register.
///
/// `Ok(value)` is returned verbatim. `Err(errno)` is encoded as the
/// two's-complement negation of the `Errno` discriminant — the
/// standard userland convention is to check the result as `i64` and,
/// if negative, recover the errno via `(-(result as i64)) as i32`.
///
/// The encoding is part of the user/kernel ABI and is exercised by
/// the `errno_encoding_round_trips_through_i64` unit test below; every
/// arch port reuses this helper rather than re-deriving the convention
/// (no duplication).
#[must_use]
#[allow(
    clippy::cast_sign_loss,
    // Documented: an `Errno` discriminant is always positive (≥ 1), so
    // negating it and storing the bit-pattern as `u64` is precisely the
    // userland-facing convention this function exists to encode.
)]
pub const fn encode_result(result: tairix_kernel_syscall::SyscallResult) -> u64 {
    match result {
        Ok(v) => v,
        Err(e) => {
            // `Errno::as_i32()` returns a positive integer (each
            // discriminant ≥ 1). Cast through `i64` so the negation
            // is well-defined for any future discriminant up to
            // `i32::MAX`; `as u64` then reinterprets the negative
            // `i64` as the bit-pattern user space inspects.
            let n = e.as_i32() as i64;
            (-n) as u64
        }
    }
}

/// Forward one syscall through a slot's resident hook.
///
/// Returns `Some(value)` for the encoded syscall-return register, or
/// `None` if the dispatcher cannot complete (empty slot or
/// `NoCallerContext`) and the caller must halt.
///
/// Shared by every architecture's `production_dispatch` callback so
/// the lookup → narrow → forward → encode sequence has one definition.
pub fn dispatch_via_slot(slot: &DispatchCallbackSlot, number: u64, args: RawArgs) -> Option<u64> {
    let hook = slot.get()?;
    // Narrow the syscall-number register to the bottom 16 bits the
    // dispatcher inspects. `Dispatcher::dispatch` re-validates the
    // value against `SyscallNumber::MAX`; truncating here is the
    // documented ABI step (the upper bits are reserved). A value
    // above `u16::MAX` is rejected by the dispatcher and surfaced
    // as `Errno::OutOfRange` — fail-closed via a normal Errno
    // return, not a halt.
    #[allow(clippy::cast_possible_truncation)]
    let raw_number = number as u16;
    match hook.dispatch(raw_number, args) {
        DispatchOutcome::Returned(result) => Some(encode_result(result)),
        DispatchOutcome::NoCallerContext => None,
        DispatchOutcome::Reschedule {
            result,
            action,
            cpu,
        } => {
            // The caller is a resumable user kthread that yielded, parked,
            // or exited (`plans/SPAWN.md` SP2). Suspend it back to the
            // scheduler; control returns here only when it is next
            // dispatched (never, for `Exit`). A `false` means no user task
            // was running on `cpu` — the syscall is then an ordinary return
            // rather than an unsound switch (fail closed).
            let _ = reschedule_current(cpu, action);
            Some(encode_result(result))
        }
    }
}

/// Forward one user-mode data abort through a slot's resident hook.
///
/// `write` is the port-attested access direction; a write is never
/// resolved (file mappings are read-only) and is always fatal to the
/// faulting task, through the same lookup → terminate sequence.
///
/// Returns `true` when the faulting page is now resident and the arch
/// port should simply return to the task (the retried access succeeds).
/// A fault fatal to the task alone never returns from this call: the
/// hook has recorded the crash exit and reclaimed the task's resources,
/// and the `Exit` suspension hands the CPU back to the scheduler. Every
/// other case — empty slot, no attributable task, or (degenerately) no
/// published user kthread to suspend — returns `false`, sending the arch
/// port to its fatal path (fail closed).
///
/// Shared by every architecture's user-fault callback so the lookup →
/// resolve → terminate sequence has one definition.
///
/// `regs` is a raw pointer to the faulting *user* register frame the
/// architecture port captured at trap entry (or null on a port that does
/// not capture one). It is narrowed to `Option<&UserRegisterFrame>` here —
/// the single site that turns the trap-ABI raw pointer into a borrow — so
/// the hook records a post-mortem crash record with a backtrace. A null
/// pointer becomes `None` and the resolver still classifies and terminates,
/// just without a backtrace.
///
/// # Safety
///
/// `regs`, when non-null, must point to a valid [`UserRegisterFrame`] that
/// lives for the duration of the call (the arch port builds it on its own
/// trap stack and holds it across this call).
pub unsafe fn resolve_user_fault_via_slot(
    slot: &DispatchCallbackSlot,
    fault_va: u64,
    write: bool,
    regs: *const UserRegisterFrame,
) -> bool {
    let Some(hook) = slot.get() else {
        return false;
    };
    // SAFETY: the caller guarantees `regs` is null or a valid frame live for
    // this call; `as_ref` yields `None` for null and never dereferences it.
    let regs = unsafe { regs.as_ref() };
    match hook.resolve_user_fault(fault_va, write, regs) {
        UserFaultOutcome::Resolved => true,
        UserFaultOutcome::Terminated { cpu } => {
            // The task is dead (exit recorded, resources reclaimed):
            // suspend it with an `Exit` action — control never returns
            // for the reclaimed task. A `false` return means no user
            // kthread is published on `cpu`; the task cannot be resumed
            // over reclaimed state, so fall through to the fatal path.
            let _ = reschedule_current(cpu, RescheduleAction::Exit);
            false
        }
        UserFaultOutcome::Unhandled => false,
    }
}

/// Terminate the task that took an **unrecoverable** EL0 exception through
/// a slot's resident hook — an illegal/unallocated instruction, an
/// alignment fault, or any synchronous lower-EL exception the port could
/// neither treat as a syscall nor resolve as a demand-paged abort.
///
/// This is the counterpart of [`resolve_user_fault_via_slot`] for
/// exceptions that must **not** be retried: no resolution is attempted, so
/// the faulting instruction is never re-run (which would re-take the
/// exception forever). The hook records the crash exit and reclaims the
/// task; a task-fatal termination never returns from this call — the `Exit`
/// suspension hands the CPU back to the scheduler, so the CPU stays alive
/// and only the offending task dies.
///
/// Returns `false` — sending the port to its fatal path (halt) — only when
/// the exception cannot be attributed to a running task (no current task,
/// or no published user kthread to suspend); that is a genuine kernel-level
/// failure, not a user one.
///
/// # Safety
///
/// `regs`, when non-null, must point to a valid [`UserRegisterFrame`] that
/// lives for the duration of the call (the arch port builds it on its own
/// trap stack and holds it across this call).
pub unsafe fn terminate_user_fault_via_slot(
    slot: &DispatchCallbackSlot,
    fault_pc: u64,
    regs: *const UserRegisterFrame,
) -> bool {
    let Some(hook) = slot.get() else {
        return false;
    };
    // SAFETY: the caller guarantees `regs` is null or a valid frame live for
    // this call; `as_ref` yields `None` for null and never dereferences it.
    let regs = unsafe { regs.as_ref() };
    match hook.terminate_user_fault(fault_pc, regs) {
        UserFaultOutcome::Terminated { cpu } => {
            // The task is dead (exit recorded, resources reclaimed): suspend
            // it with `Exit`; control never returns for the reclaimed task. A
            // `false` return means no user kthread is published on `cpu`, so
            // the port falls through to its fatal path.
            let _ = reschedule_current(cpu, RescheduleAction::Exit);
            false
        }
        // The `Resolved` variant is meaningless for a termination request;
        // treat it (and `Unhandled`) as "could not terminate" so the port
        // fails closed to its fatal path rather than returning to re-run the
        // faulting instruction.
        UserFaultOutcome::Resolved | UserFaultOutcome::Unhandled => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use tairix_abi::Errno;
    use tairix_kernel_core::{DispatchHook, RescheduleAction};

    /// Hook that returns a caller-supplied [`DispatchOutcome`] for
    /// each invocation — and, for the user-fault path, a caller-supplied
    /// [`UserFaultOutcome`]. Used to exercise every production-dispatch
    /// branch (happy path, `NoCallerContext`, and the fault dispositions).
    struct StaticHook {
        outcome: DispatchOutcome,
        fault_outcome: UserFaultOutcome,
    }
    impl DispatchHook for StaticHook {
        fn dispatch(&self, _raw_number: u16, _args: RawArgs) -> DispatchOutcome {
            self.outcome
        }
        fn resolve_user_fault(
            &self,
            _fault_va: u64,
            write: bool,
            _regs: Option<&UserRegisterFrame>,
        ) -> UserFaultOutcome {
            // Mirror the production hook's invariant: a write is never
            // resolved, so a `Resolved` disposition under `write` is a
            // scaffolding bug worth failing loudly on.
            assert!(
                !(write && self.fault_outcome == UserFaultOutcome::Resolved),
                "a write fault must never resolve"
            );
            self.fault_outcome
        }
        fn terminate_user_fault(
            &self,
            _fault_pc: u64,
            _regs: Option<&UserRegisterFrame>,
        ) -> UserFaultOutcome {
            // Reuse the single configurable disposition: the terminate path
            // is exercised by constructing the hook with the outcome under
            // test (`Terminated` for the reclaim path, `Resolved`/`Unhandled`
            // to prove the helper never returns "retry").
            self.fault_outcome
        }
    }

    #[test]
    fn read_raw_args_reinterprets_frame_in_pack_raw_args_order() {
        let frame: [u64; SYSCALL_MAX_ARGS] = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60];
        // SAFETY: `frame` lives for the duration of the call.
        let args = unsafe { read_raw_args(core::ptr::addr_of!(frame)) };
        assert_eq!(args.as_array(), &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    }

    #[test]
    fn read_raw_args_round_trips_through_extern_c_shim() {
        extern "C" fn shim(
            _number: u64,
            args_ptr: *const [u64; SYSCALL_MAX_ARGS],
            out: *mut [u64; SYSCALL_MAX_ARGS],
        ) {
            // SAFETY: both pointers address disjoint host-side
            // storage that lives for the duration of the call.
            let args = unsafe { read_raw_args(args_ptr) };
            unsafe { core::ptr::write(out, *args.as_array()) };
        }
        let frame: [u64; SYSCALL_MAX_ARGS] = [9, 8, 7, 6, 5, 4];
        let mut out: [u64; SYSCALL_MAX_ARGS] = [0; SYSCALL_MAX_ARGS];
        shim(
            0xAB,
            core::ptr::addr_of!(frame),
            core::ptr::addr_of_mut!(out),
        );
        assert_eq!(out, frame);
    }

    #[test]
    fn encode_result_ok_returns_inner_value() {
        assert_eq!(encode_result(Ok(0)), 0);
        assert_eq!(
            encode_result(Ok(0xDEAD_BEEF_F00D_BEEF)),
            0xDEAD_BEEF_F00D_BEEF
        );
    }

    #[test]
    fn encode_result_err_encodes_as_negative_i64() {
        // Round-trip through `i64` to confirm a userland-style decode
        // recovers the original errno discriminant.
        for e in [
            Errno::BufferTooSmall,
            Errno::PermissionDenied,
            Errno::NotFound,
            Errno::NotImplemented,
        ] {
            let encoded = encode_result(Err(e));
            #[allow(clippy::cast_possible_wrap)]
            let signed = encoded as i64;
            assert!(
                signed < 0,
                "expected negative encoding for {e:?}, got {signed}"
            );
            #[allow(clippy::cast_possible_truncation)]
            let recovered = (-signed) as i32;
            assert_eq!(recovered, e.as_i32());
        }
    }

    #[test]
    fn dispatch_via_slot_returns_encoded_ok_when_hook_returns_ok() {
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::Returned(Ok(0x42)),
            fault_outcome: UserFaultOutcome::Unhandled,
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO);
        assert_eq!(got, Some(0x42));
    }

    #[test]
    fn dispatch_via_slot_returns_encoded_err_when_hook_returns_errno() {
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::Returned(Err(Errno::PermissionDenied)),
            fault_outcome: UserFaultOutcome::Unhandled,
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO).expect("Some on Returned");
        #[allow(clippy::cast_possible_wrap)]
        let signed = got as i64;
        assert!(signed < 0);
        #[allow(clippy::cast_possible_truncation)]
        let recovered = (-signed) as i32;
        assert_eq!(recovered, Errno::PermissionDenied.as_i32());
    }

    #[test]
    fn dispatch_via_slot_returns_none_on_no_caller_context() {
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::NoCallerContext,
            fault_outcome: UserFaultOutcome::Unhandled,
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO);
        assert!(got.is_none(), "NoCallerContext must signal halt");
    }

    #[test]
    fn dispatch_via_slot_encodes_result_on_reschedule_with_no_user_task() {
        // A `Reschedule` outcome on a CPU with no published user kthread
        // (the host has none) falls back to an ordinary encoded return.
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::Reschedule {
                result: Ok(0x7),
                action: RescheduleAction::Yield,
                cpu: 50,
            },
            fault_outcome: UserFaultOutcome::Unhandled,
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO);
        assert_eq!(got, Some(0x7));
    }

    #[test]
    fn resolve_user_fault_via_slot_reports_a_resolved_fault() {
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::NoCallerContext,
            fault_outcome: UserFaultOutcome::Resolved,
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        // SAFETY: a null frame pointer is the documented "no captured
        // frame" case; `resolve_user_fault_via_slot` narrows it to `None`.
        assert!(unsafe {
            resolve_user_fault_via_slot(&slot, 0xF000_1000, false, core::ptr::null())
        });
    }

    #[test]
    fn resolve_user_fault_via_slot_fails_closed_without_a_hook_or_task() {
        // An empty slot resolves nothing (the arch port takes its fatal
        // path), and an `Unhandled` disposition does the same — for reads
        // and writes alike.
        let empty = DispatchCallbackSlot::new();
        // SAFETY: null frame pointer (the "no captured frame" case).
        assert!(!unsafe {
            resolve_user_fault_via_slot(&empty, 0xF000_1000, false, core::ptr::null())
        });
        assert!(!unsafe {
            resolve_user_fault_via_slot(&empty, 0xF000_1000, true, core::ptr::null())
        });

        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::NoCallerContext,
            fault_outcome: UserFaultOutcome::Unhandled,
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        // SAFETY: null frame pointer (the "no captured frame" case).
        assert!(!unsafe {
            resolve_user_fault_via_slot(&slot, 0xF000_1000, false, core::ptr::null())
        });
    }

    #[test]
    fn resolve_user_fault_via_slot_terminated_without_a_kthread_fails_closed() {
        // A `Terminated` disposition on a CPU with no published user
        // kthread (the host has none) cannot resume the reclaimed task:
        // the helper reports unresolved so the arch port halts.
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::NoCallerContext,
            fault_outcome: UserFaultOutcome::Terminated { cpu: 51 },
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        // SAFETY: null frame pointer (the "no captured frame" case).
        assert!(!unsafe {
            resolve_user_fault_via_slot(&slot, 0xF000_1000, false, core::ptr::null())
        });
    }

    #[test]
    fn resolve_user_fault_via_slot_write_fault_is_terminated_never_resolved() {
        // Regression (the M1 file-map vertical's `store` role): a write
        // fault flows through the same seam with `write = true` and comes
        // back `Terminated` — the task dies; the CPU is never halted just
        // because the access was a store. With no published kthread on the
        // host the helper still reports unresolved (the port's fatal path),
        // but the disposition it acted on was the task-fatal one.
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::NoCallerContext,
            fault_outcome: UserFaultOutcome::Terminated { cpu: 52 },
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        // SAFETY: null frame pointer (the "no captured frame" case).
        assert!(!unsafe {
            resolve_user_fault_via_slot(&slot, 0xF000_2000, true, core::ptr::null())
        });
    }

    #[test]
    fn dispatch_via_slot_returns_none_on_empty_slot() {
        let slot = DispatchCallbackSlot::new();
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO);
        assert!(got.is_none(), "empty slot must signal halt");
    }

    #[test]
    fn terminate_user_fault_via_slot_fails_closed_without_a_hook() {
        // No hook installed: the unrecoverable EL0 exception cannot be
        // attributed, so the port takes its fatal path (halt).
        let empty = DispatchCallbackSlot::new();
        // SAFETY: null frame pointer (the "no captured frame" case).
        assert!(!unsafe { terminate_user_fault_via_slot(&empty, 0xC000_1000, core::ptr::null()) });
    }

    #[test]
    fn terminate_user_fault_via_slot_terminated_without_a_kthread_fails_closed() {
        // A `Terminated` disposition on a CPU with no published user kthread
        // (the host has none) cannot suspend the reclaimed task, so the
        // helper reports "could not terminate" and the port halts. The
        // disposition it acted on was the task-fatal one — the CPU is never
        // halted while a task could have been killed instead.
        let slot = DispatchCallbackSlot::new();
        let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
            outcome: DispatchOutcome::NoCallerContext,
            fault_outcome: UserFaultOutcome::Terminated { cpu: 53 },
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        // SAFETY: null frame pointer (the "no captured frame" case).
        assert!(!unsafe { terminate_user_fault_via_slot(&slot, 0xC000_1000, core::ptr::null()) });
    }

    #[test]
    fn terminate_user_fault_via_slot_never_retries_the_faulting_instruction() {
        // The safety invariant of the terminate path: it must NEVER return
        // `true` (which would send the arch port back to `eret` and re-run
        // the unrecoverable instruction forever). Even a hook that
        // degenerately reports `Resolved` or `Unhandled` is treated as
        // "could not terminate" (false → fatal path), never as "retry".
        for outcome in [UserFaultOutcome::Resolved, UserFaultOutcome::Unhandled] {
            let slot = DispatchCallbackSlot::new();
            let hook: &'static StaticHook = Box::leak(Box::new(StaticHook {
                outcome: DispatchOutcome::NoCallerContext,
                fault_outcome: outcome,
            }));
            slot.install_dispatcher(hook as &'static dyn DispatchHook)
                .expect("install");
            // SAFETY: null frame pointer (the "no captured frame" case).
            assert!(
                !unsafe { terminate_user_fault_via_slot(&slot, 0xC000_1000, core::ptr::null()) },
                "terminate must never return retry for {outcome:?}"
            );
        }
    }
}
