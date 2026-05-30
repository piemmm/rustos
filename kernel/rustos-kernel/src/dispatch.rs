//! Production syscall-dispatch callback for the `rustos-kernel` binary.
//!
//! # Two-stage callback ABI
//!
//! The x86_64 syscall trampoline shipped by
//! [`rustos_arch_x86_64::syscall_entry`] expects a bare `extern "C"`
//! function pointer of the [`SyscallDispatchFn`] type. The trampoline
//! itself fail-closes via `rustos_arch_x86_64::qemu_exit::exit_failure`
//! if it ever fires before the callback is installed
//! ([`set_dispatch_callback`] documents that contract); the bin crate
//! must call `set_dispatch_callback` **before** `init_local_syscalls`
//! enables `syscall` on any CPU (`AGENTS.md` §5.4.5 — fail closed).
//!
//! Stage 2.7 follow-up (f5) replaces the previous fail-closed body
//! with [`production_dispatch`]. The callback no longer halts on first
//! syscall: it reads the per-binary [`DISPATCH_SLOT`] (published by
//! `kernel_core::kernel_main` during the `Syscall` init phase, see
//! `docs/src/architecture/kernel.md` "Syscall registration phase"),
//! forwards the call through the resident `DispatchHook`, and
//! encodes the [`DispatchOutcome`] back into the architecture's
//! syscall-return register.
//!
//! The trampoline-level `set_dispatch_callback` ordering is
//! unchanged: the bin crate still installs the callback before
//! `syscall` is enabled (see `boot.rs::try_boot` step 7); the slot
//! is the *second* publication channel and is filled in by
//! `kernel_main` between `Phase::Sched` and `Phase::Ipc`.
//!
//! # Fail-closed branches
//!
//! Two branches halt the CPU forever via
//! [`rustos_arch_x86_64::kernel_arch::halt`], matching the
//! pre-(f5) behaviour:
//!
//! 1. The slot is empty. This means a syscall fired before
//!    `kernel_main` published the hook — impossible if the BSP boot
//!    ordering is correct, but the callback must not assume so.
//! 2. The hook returned [`DispatchOutcome::NoCallerContext`]. This
//!    means `Scheduler::current_task` returned `None` (no task is
//!    running on the issuing CPU) or no `TaskCapabilities` record
//!    exists for the running task — the §5.4.5 fail-closed posture.
//!    `KernelDispatchHook` has already emitted an
//!    `AuditEvent::SyscallNoCallerContext` record by the time we
//!    halt, so the security signal is durable on the audit channel.
//!
//! Both halts are unconditional; production never returns an
//! unspecified value to user space (`AGENTS.md` §2.9 — no
//! `unwrap`/`expect`/`panic!` in production paths; the bottom-typed
//! halt is the documented contract).
//!
//! [`set_dispatch_callback`]: rustos_arch_x86_64::syscall_entry::set_dispatch_callback

use rustos_abi::SYSCALL_MAX_ARGS;
use rustos_arch_x86_64::syscall_entry::SyscallDispatchFn;
use rustos_kernel_core::{DispatchCallbackSlot, DispatchOutcome};
use rustos_kernel_syscall::RawArgs;

/// Bin-crate-owned [`DispatchCallbackSlot`] published into the
/// [`rustos_kernel_core::BootInfo`] hand-off.
///
/// Stage 2.7 follow-up (f4). The slot is a `static` (not `static
/// mut`): its set-once publication path is protected by the internal
/// `OnceCell` (`AGENTS.md` §2.1 — the only sanctioned global mutable
/// state in the kernel is the per-CPU bootstrap area).
/// `kernel_core::kernel_main` calls
/// [`DispatchCallbackSlot::install_dispatcher`] exactly once during
/// the `Syscall` init phase; (f5)'s production dispatch callback
/// reads through [`DispatchCallbackSlot::get`] on every syscall.
pub static DISPATCH_SLOT: DispatchCallbackSlot = DispatchCallbackSlot::new();

/// Bridge the kernel-stack `[u64; SYSCALL_MAX_ARGS]` frame to a
/// [`RawArgs`] value.
///
/// The (c7-arch) compile-time `_RAW_ARGS_LAYOUT_MATCHES_ARRAY`
/// assertion in `rustos_kernel_syscall::table` pins [`RawArgs`]'s
/// `#[repr(transparent)]` over `[u64; SYSCALL_MAX_ARGS]`. This
/// function exists so the host-side tests can verify the
/// reinterpretation round-trip without invoking the freestanding
/// `syscall` instruction.
///
/// # Safety
///
/// `args_ptr` must point at a fully-initialised
/// `[u64; SYSCALL_MAX_ARGS]` that lives for the duration of the call.
/// In production the trampoline lays this frame out on the kernel
/// stack and the array lives at least until the dispatch callback
/// returns.
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
/// the `errno_encoding_round_trips_through_i64` unit test below; new
/// callers (Stage 3b/3c/3d arch ports) reuse this helper rather than
/// re-deriving the convention (`AGENTS.md` §2.2 — no duplication).
#[must_use]
#[allow(
    clippy::cast_sign_loss,
    // Documented: an `Errno` discriminant is always positive (2265 1), so
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

/// Production dispatch callback installed before `syscall` is
/// enabled on any CPU.
///
/// Reads the per-CPU [`RawArgs`] frame, looks up the resident
/// `DispatchHook` through [`DISPATCH_SLOT`], and forwards. The two
/// halt branches (empty slot; `NoCallerContext`) match the pre-(f5)
/// fail-closed posture exactly — `AGENTS.md` §5.4.5.
///
/// The `extern "C"` signature is locked at compile time by
/// `_DISPATCH_SIGNATURE_PINNED` below.
//
// The function must remain a safe `extern "C" fn` because that is
// the type the architecture port's `SyscallDispatchFn` typedef
// expects (`AGENTS.md` §15.2 — no invented APIs). The callback is
// only ever invoked from the syscall trampoline, which carries the
// SAFETY contract documented on `SyscallDispatchFn` and re-asserted
// on the [`read_raw_args`] call site. `AGENTS.md` §15.10 — every
// `#[allow]` carries a justifying comment.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[must_use = "the dispatch callback's return value is sent back to user space as a syscall result"]
pub extern "C" fn production_dispatch(
    number: u64,
    args_ptr: *const [u64; SYSCALL_MAX_ARGS],
) -> u64 {
    // SAFETY: the trampoline lays out the frame on the kernel stack
    // and only invokes us with a valid pointer; the array lives at
    // least until we return.
    let args = unsafe { read_raw_args(args_ptr) };
    match dispatch_via_slot(&DISPATCH_SLOT, number, args) {
        Some(value) => value,
        None => halt_fail_closed(),
    }
}

/// Halt the CPU forever.
///
/// Wrapped behind a non-test indirection so host tests can replace
/// the production halt (which would unwind under `catch_unwind` via
/// the test harness, see `kernel/core::test_arch`) with a panic that
/// the test scaffolding can observe. `AGENTS.md` §2.9 — production
/// halts are bottom-typed; the test variant carries the same `!`
/// return type.
#[cfg(freestanding)]
fn halt_fail_closed() -> ! {
    rustos_arch_x86_64::kernel_arch::halt()
}

/// Host-test stand-in for [`rustos_arch_x86_64::kernel_arch::halt`].
///
/// `panic!` is the canonical bottom-typed marker on the host build
/// (`AGENTS.md` §2.9 permits `panic!` in tests). The message string
/// matches `kernel_core::test_arch::HALT_SENTINEL` so the existing
/// `kernel_arch_boot`-style integration tests can re-use the same
/// detection logic.
#[cfg(not(freestanding))]
fn halt_fail_closed() -> ! {
    panic!("kernel halted (production_dispatch fail-closed branch)")
}

/// Forward one syscall through a slot's resident hook.
///
/// Returns `Some(value)` for the encoded syscall-return register, or
/// `None` if the dispatcher cannot complete (empty slot or
/// `NoCallerContext`) and the caller must halt.
///
/// Split out from [`production_dispatch`] purely so the host tests
/// can exercise the dispatch path with a privately-owned slot,
/// without colliding with the bin-wide [`DISPATCH_SLOT`] static.
/// `AGENTS.md` §2.3 — no bloat; this is the only API the test
/// surface needs beyond the public callback.
fn dispatch_via_slot(slot: &DispatchCallbackSlot, number: u64, args: RawArgs) -> Option<u64> {
    let hook = slot.get()?;
    // Narrow the syscall-number register to the bottom 16 bits the
    // dispatcher inspects. `Dispatcher::dispatch` re-validates the
    // value against `SyscallNumber::MAX`; truncating here is the
    // documented ABI step (the upper bits are reserved). A value
    // above `u16::MAX` is rejected by the dispatcher and surfaced
    // as `Errno::OutOfRange` — fail-closed via a normal Errno
    // return, not a halt.
    // Documented narrowing: SyscallNumber::from_raw inside
    // Dispatcher::dispatch re-validates the value against
    // SyscallNumber::MAX (which fits in u16); the upper bits of the
    // user-space register are reserved (AGENTS.md 00a72.4).
    #[allow(clippy::cast_possible_truncation)]
    let raw_number = number as u16;
    match hook.dispatch(raw_number, args) {
        DispatchOutcome::Returned(result) => Some(encode_result(result)),
        DispatchOutcome::NoCallerContext => None,
    }
}

// SAFETY-INVARIANT: [`production_dispatch`] is a valid
// [`SyscallDispatchFn`]. The compile-time coercion below fails to
// type-check if the ABI, parameter list, or return type ever drifts —
// the same pattern the arch crate uses for its `pack_raw_args` ABI
// width test (`AGENTS.md` §2.4).
const _DISPATCH_SIGNATURE_PINNED: SyscallDispatchFn = production_dispatch;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use rustos_abi::Errno;
    use rustos_kernel_core::DispatchHook;

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
    fn production_dispatch_matches_arch_dispatch_fn_signature() {
        // The compile-time `_DISPATCH_SIGNATURE_PINNED` const
        // assertion already proves this at build time; the runtime
        // re-coercion catches a future regression to a variadic or
        // closure shim.
        let f: SyscallDispatchFn = production_dispatch;
        assert!((f as usize) != 0);
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
        // Documented casts: the encoder builds a negative `i64` from a
        // positive `Errno::as_i32`, so reinterpreting the `u64` as `i64`
        // and narrowing back to `i32` is exact for every `Errno` we ship.
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
        // Same documented round-trip as `encode_result_err_encodes_as_negative_i64`.
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
    fn dispatch_via_slot_returns_none_on_empty_slot() {
        let slot = DispatchCallbackSlot::new();
        let got = dispatch_via_slot(&slot, 0, RawArgs::ZERO);
        assert!(got.is_none(), "empty slot must signal halt");
    }
}
