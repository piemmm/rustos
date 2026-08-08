//! Production syscall-dispatch callback for the riscv64 (QEMU `virt` /
//! SiFive) `tairix-kernel` binary — the riscv64 sibling of
//! [`crate::x86_64::dispatch`] (x86_64) and [`crate::aarch64::dispatch`]
//! (`plans/PI.md` RV-P2).
//!
//! The riscv64 `ecall` trap path shipped by
//! [`tairix_arch_riscv64::syscall_entry`] forwards each syscall to a
//! bare `extern "C"` callback of the
//! [`tairix_arch_riscv64::syscall_entry::SyscallDispatchFn`] type
//! (identical in shape to the x86_64 and aarch64 typedefs — one ABI). [`production_dispatch`] is that callback: it reads
//! the argument frame, forwards through the resident `DispatchHook`
//! published into [`DISPATCH_SLOT`] by `kernel_core::kernel_main`, and
//! encodes the result back into `a0`.
//!
//! The arch-neutral lookup → narrow → forward → encode logic lives in
//! [`crate::dispatch_core`] and is unit-tested there once; this module
//! supplies only the two riscv64-specific facts — the
//! `SyscallDispatchFn` coercion and the bottom-typed
//! [`tairix_arch_riscv64::halt_current_hart`] fail-closed halt.

use tairix_abi::SYSCALL_MAX_ARGS;
use tairix_arch_riscv64::syscall_entry::SyscallDispatchFn;
use tairix_kernel_core::DispatchCallbackSlot;

use crate::dispatch_core::{dispatch_via_slot, read_raw_args};

/// Bin-crate-owned [`DispatchCallbackSlot`] published into the
/// [`tairix_kernel_core::BootInfo`] hand-off.
///
/// `kernel_core::kernel_main` calls
/// [`DispatchCallbackSlot::install_dispatcher`] exactly once during the
/// `Syscall` init phase; [`production_dispatch`] reads through
/// [`DispatchCallbackSlot::get`] on every `ecall`. Set-once via the
/// internal `OnceCell`.
pub static DISPATCH_SLOT: DispatchCallbackSlot = DispatchCallbackSlot::new();

/// Production dispatch callback installed before user space is entered.
///
/// Reads the argument frame, looks up the resident `DispatchHook`
/// through [`DISPATCH_SLOT`], and forwards. The two fail-closed branches
/// (empty slot; `NoCallerContext`) halt the hart forever.
///
/// The `extern "C"` signature is locked at compile time by
/// [`_DISPATCH_SIGNATURE_PINNED`] below.
//
// The function must remain a safe `extern "C" fn` because that is the
// type the arch port's `SyscallDispatchFn` typedef expects. It is only ever invoked from the `ecall` trap
// handler, which carries the SAFETY contract documented on
// `SyscallDispatchFn` and re-asserted on the [`read_raw_args`] call
// site. — every `#[allow]` carries a justification.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[must_use = "the dispatch callback's return value is sent back to user space as a syscall result"]
pub extern "C" fn production_dispatch(
    number: u64,
    args_ptr: *const [u64; SYSCALL_MAX_ARGS],
) -> u64 {
    // SAFETY: the trap handler lays out the frame on the kernel stack
    // and only invokes us with a valid pointer; the array lives at least
    // until we return.
    let args = unsafe { read_raw_args(args_ptr) };
    match dispatch_via_slot(&DISPATCH_SLOT, number, args) {
        Some(value) => value,
        None => halt_fail_closed(),
    }
}

/// Production user-fault resolver callback installed beside the dispatch
/// callback before user space is entered.
///
/// The riscv64 trap handler offers every U-mode load/store page fault
/// here first (`tairix_arch_riscv64::fault::UserFaultResolveFn`), with
/// `write` the store/AMO `scause` verdict; the arch-neutral lookup →
/// resolve → terminate sequence lives in
/// [`crate::dispatch_core::resolve_user_fault_via_slot`] and is
/// unit-tested there once. `true` re-runs the faulting instruction; a
/// task-fatal fault (any write, or an unresolvable read) never returns
/// (the helper suspends the reclaimed task with an exit action);
/// `false` sends the trap handler to its fatal path (fail closed).
///
/// `regs` is the faulting U-mode register frame the trap handler captured
/// from the saved trap frame; it is forwarded so the resolver can record a
/// post-mortem crash record with a backtrace.
// The callback must stay a bare safe `extern "C" fn` to match the arch
// port's `UserFaultResolveFn` type; the raw-pointer deref happens only
// inside the guarded `unsafe` block below, whose SAFETY note carries the
// trap-handler-upheld contract.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[must_use]
pub extern "C" fn production_user_fault(
    stval: u64,
    write: bool,
    regs: *const tairix_arch_api::backtrace::UserRegisterFrame,
) -> bool {
    // SAFETY: the trap handler builds the frame on its own kernel stack and
    // holds it live across this call, or passes null; the slot helper
    // narrows the pointer with `as_ref` and never dereferences null.
    unsafe { crate::dispatch_core::resolve_user_fault_via_slot(&DISPATCH_SLOT, stval, write, regs) }
}

// SAFETY-INVARIANT: [`production_user_fault`] is a valid
// [`tairix_arch_riscv64::fault::UserFaultResolveFn`]. The compile-time
// coercion below fails to type-check if the ABI, parameter list, or
// return type ever drifts, matching `_DISPATCH_SIGNATURE_PINNED`.
const _USER_FAULT_SIGNATURE_PINNED: tairix_arch_riscv64::fault::UserFaultResolveFn =
    production_user_fault;

/// Halt the hart forever (the riscv64 fail-closed branch).
///
/// Wrapped behind a non-test indirection so host tests can replace the
/// production park (which would otherwise wedge the test thread) with a
/// panic the scaffolding can observe. — production
/// halts are bottom-typed; the test variant carries the same `!`.
#[cfg(freestanding)]
fn halt_fail_closed() -> ! {
    tairix_arch_riscv64::halt_current_hart()
}

/// Host-test stand-in for [`tairix_arch_riscv64::halt_current_hart`].
#[cfg(not(freestanding))]
fn halt_fail_closed() -> ! {
    panic!("kernel halted (riscv64 production_dispatch fail-closed branch)")
}

// SAFETY-INVARIANT: [`production_dispatch`] is a valid
// [`SyscallDispatchFn`]. The compile-time coercion below fails to
// type-check if the ABI, parameter list, or return type ever drifts, matching the x86_64 and aarch64 dispatch modules.
const _DISPATCH_SIGNATURE_PINNED: SyscallDispatchFn = production_dispatch;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_dispatch_matches_arch_dispatch_fn_signature() {
        // The compile-time `_DISPATCH_SIGNATURE_PINNED` const assertion
        // already proves this at build time; the runtime re-coercion
        // catches a future regression to a variadic or closure shim.
        // The arch-neutral dispatch logic is unit-tested once in
        // `crate::dispatch_core`.
        let f: SyscallDispatchFn = production_dispatch;
        assert!((f as usize) != 0);
    }
}
