//! Fail-closed syscall-dispatch callback for the boot path.
//!
//! # Why fail-closed
//!
//! The x86_64 syscall trampoline shipped by
//! [`rustos_arch_x86_64::syscall_entry`] requires a dispatch callback
//! to be installed via [`set_dispatch_callback`] *before* `syscall` is
//! enabled on any CPU. The trampoline itself fail-closes via
//! `rustos_arch_x86_64::qemu_exit::exit_failure` if it fires before a
//! callback is installed — an "open by default" failure is forbidden
//! by `AGENTS.md` §5.4.5 / §7.
//!
//! Stage 3a (c7-bin) needs a callback that satisfies the ordering
//! contract (callback installed before `syscall` is enabled) but the
//! supporting infrastructure for actually forwarding to
//! [`rustos_kernel_syscall::Dispatcher::dispatch`] — a production
//! [`SyscallHandlers`] impl and a per-CPU current-task → caller-context
//! plumbing — *has not landed yet*. `kernel_core::kernel_main`'s own
//! rustdoc says so: *"Stage 2.7 will extend `kernel_main` with a
//! syscall-registration phase"*.
//!
//! Until that lands, the callback installed by `crate::boot::boot`
//! is the fail-closed one in this module: it parks the CPU forever via
//! [`rustos_arch_x86_64::kernel_arch::halt`]. The (c7-bin) boot test
//! never enters user space, so the callback is never actually called;
//! installing it is purely defensive.
//!
//! `AGENTS.md` §15.10 — every `#[allow(...)]` / shortcut carries a
//! justifying comment. This module's *entire reason for existing* is
//! the comment block above; the next stage replaces the body with a
//! real forwarder.
//!
//! [`SyscallHandlers`]: rustos_kernel_syscall::SyscallHandlers
//! [`set_dispatch_callback`]: rustos_arch_x86_64::syscall_entry::set_dispatch_callback

use rustos_abi::SYSCALL_MAX_ARGS;
use rustos_arch_x86_64::syscall_entry::SyscallDispatchFn;
use rustos_kernel_syscall::RawArgs;

/// Bridge the kernel-stack `[u64; SYSCALL_MAX_ARGS]` frame to a
/// [`RawArgs`] value.
///
/// The (c7-arch) compile-time `_RAW_ARGS_LAYOUT_MATCHES_ARRAY`
/// assertion in `rustos_kernel_syscall::table` pins
/// [`RawArgs`]'s `#[repr(transparent)]` over
/// `[u64; SYSCALL_MAX_ARGS]`. This function exists so the host-side
/// tests can verify the reinterpretation round-trip without invoking
/// the freestanding `syscall` instruction.
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

/// Fail-closed dispatch callback.
///
/// Installed by `crate::boot::boot` before `syscall` is enabled on
/// any CPU. If the trampoline ever forwards a real syscall here
/// (which the (c7-bin) boot path never does — there is no user space
/// yet), the callback parks the CPU forever via
/// [`rustos_arch_x86_64::kernel_arch::halt`]. The `extern "C"` ABI is
/// pinned to match [`SyscallDispatchFn`]; the [`SYSCALL_MAX_ARGS`]
/// argument array is reinterpreted as a [`RawArgs`] purely so the
/// (c7-arch) layout assertion is exercised in the host-side test
/// suite, even though no real dispatch happens here yet.
///
/// # Stage 2.7 follow-up
///
/// The body of this function is the documented hook for the
/// syscall-registration phase. When `kernel/core::kernel_main` gains
/// the registration step, replace this body with a forwarder to
/// `rustos_kernel_syscall::Dispatcher::dispatch` built against the
/// then-available `SyscallHandlers` impl and per-CPU
/// `CallerContext` plumbing. The signature is locked at compile time
/// by `_DISPATCH_SIGNATURE_PINNED` below.
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
pub extern "C" fn fail_closed_dispatch(
    _number: u64,
    args_ptr: *const [u64; SYSCALL_MAX_ARGS],
) -> u64 {
    // Read the frame purely to exercise the layout invariant. The
    // `RawArgs` value is then dropped before we halt.
    //
    // SAFETY: the trampoline lays out the frame on the kernel stack
    // and only invokes us with a valid pointer. The local `_args` is
    // discarded immediately; we do not retain it across the halt.
    let _args = unsafe { read_raw_args(args_ptr) };
    rustos_arch_x86_64::kernel_arch::halt()
}

// SAFETY-INVARIANT: [`fail_closed_dispatch`] is a valid
// [`SyscallDispatchFn`]. The compile-time coercion below fails to
// type-check if the ABI, parameter list, or return type ever drifts —
// the same pattern the arch crate uses for its `pack_raw_args` ABI
// width test (`AGENTS.md` §2.4).
const _DISPATCH_SIGNATURE_PINNED: SyscallDispatchFn = fail_closed_dispatch;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_raw_args_reinterprets_frame_in_pack_raw_args_order() {
        // Mirror the ordering documented in
        // `rustos_arch_x86_64::syscall_entry::pack_raw_args`:
        // [rdi, rsi, rdx, r10, r8, r9].
        let frame: [u64; SYSCALL_MAX_ARGS] = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60];
        // SAFETY: `frame` lives for the duration of the call; the
        // pointer dereference is well-defined.
        let args = unsafe { read_raw_args(core::ptr::addr_of!(frame)) };
        assert_eq!(args.as_array(), &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    }

    #[test]
    fn read_raw_args_round_trips_through_extern_c_shim() {
        // Stand-in for the freestanding trampoline: an `extern "C"` fn
        // that takes the same `*const [u64; SYSCALL_MAX_ARGS]` the
        // production callback receives and copies the result into a
        // caller-visible slot. Exercises the ABI shape end-to-end on
        // the host build, exactly as `AGENTS.md` §15.2 / §10 require
        // for an `unsafe` extern-FFI bridge.
        // The shim takes an out-pointer for the result rather than
        // returning the array by value, because `extern "C" fn` may
        // not return `[u64; N]` by value on every ABI we target
        // (rustc lints `improper_ctypes_definitions`). The trampoline
        // in production uses the same convention — `RawArgs` is
        // copied out through the `*const [u64; SYSCALL_MAX_ARGS]`
        // argument, never as a return value.
        extern "C" fn shim(
            _number: u64,
            args_ptr: *const [u64; SYSCALL_MAX_ARGS],
            out: *mut [u64; SYSCALL_MAX_ARGS],
        ) {
            // SAFETY: `args_ptr` is the borrowed input frame and
            // `out` is the caller-supplied result slot; both live
            // for the duration of the call. The two pointers
            // address disjoint storage in this host test.
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
    fn fail_closed_dispatch_matches_arch_dispatch_fn_signature() {
        // The compile-time `_DISPATCH_SIGNATURE_PINNED` const assertion
        // already proves this at build time; the runtime test
        // re-exercises the coercion so a future change that introduces
        // a runtime-only ABI shim (e.g. variadics) does not slip
        // through unnoticed.
        let f: SyscallDispatchFn = fail_closed_dispatch;
        // Coerce to a pointer-sized integer so the binding survives
        // clippy's `no_effect_underscore_binding` lint without an
        // `#[allow]`.
        assert!((f as usize) != 0);
    }
}
