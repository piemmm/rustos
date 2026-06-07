//! Architecture-neutral syscall-dispatch helpers shared by every
//! per-architecture dispatch callback (`crate::dispatch` on x86_64,
//! `crate::dispatch_aarch64` on aarch64).
//!
//! The per-architecture modules differ only in two arch-specific facts:
//! the `SyscallDispatchFn` typedef the arch port's syscall trampoline
//! expects, and the bottom-typed halt the fail-closed branch jumps to.
//! Everything else — reading the kernel-stack `[u64; SYSCALL_MAX_ARGS]`
//! frame into a [`RawArgs`], encoding a [`rustos_kernel_syscall::SyscallResult`]
//! into the syscall-return register, and forwarding one syscall through
//! a slot's resident `DispatchHook` — is identical across architectures
//! and lives here exactly once (`AGENTS.md` §2.2 — no duplication).
//!
//! The module is host-testable and un-gated: the per-architecture
//! callbacks are thin wrappers that supply the arch `SyscallDispatchFn`
//! coercion and the halt, while the substantive logic is unit-tested
//! here once.

use rustos_abi::SYSCALL_MAX_ARGS;
use rustos_kernel_core::{reschedule_current, DispatchCallbackSlot, DispatchOutcome};
use rustos_kernel_syscall::RawArgs;

/// Bridge the kernel-stack `[u64; SYSCALL_MAX_ARGS]` frame to a
/// [`RawArgs`] value.
///
/// The (c7-arch) compile-time `_RAW_ARGS_LAYOUT_MATCHES_ARRAY`
/// assertion in `rustos_kernel_syscall::table` pins [`RawArgs`]'s
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

/// Encode a [`rustos_kernel_syscall::SyscallResult`] into the
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
/// (`AGENTS.md` §2.2 — no duplication).
#[must_use]
#[allow(
    clippy::cast_sign_loss,
    // Documented: an `Errno` discriminant is always positive (≥ 1), so
    // negating it and storing the bit-pattern as `u64` is precisely the
    // userland-facing convention this function exists to encode.
)]
pub const fn encode_result(result: rustos_kernel_syscall::SyscallResult) -> u64 {
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
/// the lookup → narrow → forward → encode sequence has one definition
/// (`AGENTS.md` §2.2).
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
            // rather than an unsound switch (fail closed, `AGENTS.md` §2.9).
            let _ = reschedule_current(cpu, action);
            Some(encode_result(result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use rustos_abi::Errno;
    use rustos_kernel_core::{DispatchHook, RescheduleAction};

    /// Hook that returns a caller-supplied [`DispatchOutcome`] for
    /// each invocation. Used to exercise both production-dispatch
    /// branches (happy path and `NoCallerContext`).
    struct StaticHook {
        outcome: DispatchOutcome,
    }
    impl DispatchHook for StaticHook {
        fn dispatch(&self, _raw_number: u16, _args: RawArgs) -> DispatchOutcome {
            self.outcome
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
        }));
        slot.install_dispatcher(hook as &'static dyn DispatchHook)
            .expect("install");
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO);
        assert_eq!(got, Some(0x7));
    }

    #[test]
    fn dispatch_via_slot_returns_none_on_empty_slot() {
        let slot = DispatchCallbackSlot::new();
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO);
        assert!(got.is_none(), "empty slot must signal halt");
    }
}
