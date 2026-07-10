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
//! enables `syscall` on any CPU (fail closed).
//!
//! Stage 2.7 follow-up (f5) replaces the previous fail-closed body
//! with [`production_dispatch`]. The callback no longer halts on first
//! syscall: it reads the per-binary [`DISPATCH_SLOT`] (published by
//! `kernel_core::kernel_main` during the `Syscall` init phase, see
//! `docs/src/architecture/kernel.md` "Syscall registration phase"),
//! forwards the call through the resident `DispatchHook`, and
//! encodes the [`DispatchOutcome`](rustos_kernel_core::DispatchOutcome) back into the architecture's
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
//! 2. The hook returned [`DispatchOutcome::NoCallerContext`](rustos_kernel_core::DispatchOutcome::NoCallerContext). This
//!    means `Scheduler::current_task` returned `None` (no task is
//!    running on the issuing CPU) or no `TaskCapabilities` record
//!    exists for the running task — the fail-closed posture.
//!    `KernelDispatchHook` has already emitted an
//!    `AuditEvent::SyscallNoCallerContext` record by the time we
//!    halt, so the security signal is durable on the audit channel.
//!
//! Both halts are unconditional; production never returns an
//! unspecified value to user space (no
//! `unwrap`/`expect`/`panic!` in production paths; the bottom-typed
//! halt is the documented contract).
//!
//! [`set_dispatch_callback`]: rustos_arch_x86_64::syscall_entry::set_dispatch_callback

use rustos_abi::SYSCALL_MAX_ARGS;
use rustos_arch_x86_64::syscall_entry::SyscallDispatchFn;
use rustos_kernel_core::DispatchCallbackSlot;

use crate::dispatch_core::{dispatch_via_slot, read_raw_args};

/// Bin-crate-owned [`DispatchCallbackSlot`] published into the
/// [`rustos_kernel_core::BootInfo`] hand-off.
///
/// Stage 2.7 follow-up (f4). The slot is a `static` (not `static
/// mut`): its set-once publication path is protected by the internal
/// `OnceCell` (the only sanctioned global mutable
/// state in the kernel is the per-CPU bootstrap area).
/// `kernel_core::kernel_main` calls
/// [`DispatchCallbackSlot::install_dispatcher`] exactly once during
/// the `Syscall` init phase; (f5)'s production dispatch callback
/// reads through [`DispatchCallbackSlot::get`] on every syscall.
pub static DISPATCH_SLOT: DispatchCallbackSlot = DispatchCallbackSlot::new();

/// Production dispatch callback installed before `syscall` is
/// enabled on any CPU.
///
/// Reads the per-CPU [`RawArgs`](rustos_kernel_syscall::RawArgs) frame, looks up the resident
/// `DispatchHook` through [`DISPATCH_SLOT`], and forwards. The two
/// halt branches (empty slot; `NoCallerContext`) match the pre-(f5)
/// fail-closed posture exactly.
///
/// The `extern "C"` signature is locked at compile time by
/// `_DISPATCH_SIGNATURE_PINNED` below.
//
// The function must remain a safe `extern "C" fn` because that is
// the type the architecture port's `SyscallDispatchFn` typedef
// expects (no invented APIs). The callback is
// only ever invoked from the syscall trampoline, which carries the
// SAFETY contract documented on `SyscallDispatchFn` and re-asserted
// on the [`read_raw_args`] call site. — every
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

/// Production user-fault resolver callback installed beside the dispatch
/// callback before user space is entered.
///
/// The dedicated `#PF` entry offers every ring-3 data fault here first
/// ([`rustos_arch_x86_64::fault::UserFaultResolveFn`]), with `write` the
/// `#PF` error-code `W/R` verdict; the arch-neutral lookup → resolve →
/// terminate sequence lives in
/// [`crate::dispatch_core::resolve_user_fault_via_slot`] and is
/// unit-tested there once. `true` re-runs the faulting instruction; a
/// task-fatal fault (any write, or an unresolvable read) never returns
/// (the helper suspends the reclaimed task with an exit action);
/// `false` sends the `#PF` entry to its fatal path (fail closed).
#[must_use]
pub extern "C" fn production_user_fault(faulting_addr: u64, write: bool) -> bool {
    crate::dispatch_core::resolve_user_fault_via_slot(&DISPATCH_SLOT, faulting_addr, write)
}

// SAFETY-INVARIANT: [`production_user_fault`] is a valid
// [`rustos_arch_x86_64::fault::UserFaultResolveFn`]. The compile-time
// coercion below fails to type-check if the ABI, parameter list, or
// return type ever drifts, matching `_DISPATCH_SIGNATURE_PINNED`.
const _USER_FAULT_SIGNATURE_PINNED: rustos_arch_x86_64::fault::UserFaultResolveFn =
    production_user_fault;

/// Halt the CPU forever.
///
/// Wrapped behind a non-test indirection so host tests can replace
/// the production halt (which would unwind under `catch_unwind` via
/// the test harness, see `kernel/core::test_arch`) with a panic that
/// the test scaffolding can observe. — production
/// halts are bottom-typed; the test variant carries the same `!`
/// return type.
#[cfg(freestanding)]
fn halt_fail_closed() -> ! {
    rustos_arch_x86_64::kernel_arch::halt()
}

/// Host-test stand-in for [`rustos_arch_x86_64::kernel_arch::halt`].
///
/// `panic!` is the canonical bottom-typed marker on the host build
/// (the charter permits `panic!` in tests). The message string
/// matches `kernel_core::test_arch::HALT_SENTINEL` so the existing
/// `kernel_arch_boot`-style integration tests can re-use the same
/// detection logic.
#[cfg(not(freestanding))]
fn halt_fail_closed() -> ! {
    panic!("kernel halted (production_dispatch fail-closed branch)")
}

// SAFETY-INVARIANT: [`production_dispatch`] is a valid
// [`SyscallDispatchFn`]. The compile-time coercion below fails to
// type-check if the ABI, parameter list, or return type ever drifts —
// the same pattern the arch crate uses for its `pack_raw_args` ABI
// width test.
const _DISPATCH_SIGNATURE_PINNED: SyscallDispatchFn = production_dispatch;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_dispatch_matches_arch_dispatch_fn_signature() {
        // The compile-time `_DISPATCH_SIGNATURE_PINNED` const
        // assertion already proves this at build time; the runtime
        // re-coercion catches a future regression to a variadic or
        // closure shim. The arch-neutral dispatch logic
        // (`read_raw_args`, `encode_result`, `dispatch_via_slot`) is
        // unit-tested once in `crate::dispatch_core`.
        let f: SyscallDispatchFn = production_dispatch;
        assert!((f as usize) != 0);
    }
}
