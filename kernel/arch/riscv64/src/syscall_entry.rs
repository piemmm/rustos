//! riscv64 `ecall` syscall entry path.
//!
//! Unlike x86_64 (a dedicated `syscall`/`sysret` instruction pair with
//! its own MSR-programmed entry), riscv64 raises an *Environment call*
//! synchronous exception when user code executes `ecall`. It enters the
//! same S-mode trap vector as every other trap; this module owns the
//! syscall-specific slice of that path:
//!
//! * The `scause` codes for an environment call ([`SCAUSE_ECALL_FROM_U`]
//!   / [`SCAUSE_ECALL_FROM_S`]) the trap handler matches.
//! * Marshalling the register-passed arguments (`a0`–`a5`) and the
//!   syscall number (`a7`) out of the saved [`crate::trap::TrapFrame`]
//!   into the architecture-neutral `rustos_abi` `[u64; SYSCALL_MAX_ARGS]`
//!   layout — the same layout `kernel/arch/x86_64` builds (one ABI, no duplication).
//! * The dispatch callback the trap path forwards each `ecall` to,
//!   mirroring the [`crate::preempt`] timer-callback design. The
//!   architecture-neutral validation / capability / audit dispatcher
//!   lives in `kernel/syscall` and is installed by the downstream boot
//!   binary; the arch port never re-implements it.
//!
//! # Calling convention
//!
//! RustOS follows the established RISC-V Linux register convention: the
//! syscall number is in `a7`, arguments in `a0`–`a5` (six — exactly
//! `SYSCALL_MAX_ARGS`), and the result is returned in `a0`. After the
//! dispatch the handler advances `sepc` past the 4-byte `ecall`
//! instruction so `sret` resumes at the following instruction rather
//! than re-executing the trap.
//!
//! # Host testability
//!
//! The `scause` decode, the argument packing, the callback storage, and
//! [`dispatch_ecall`] all build and are unit-tested on the host
//! (`dispatch_ecall` takes a `&mut TrapFrame`, which is host-
//! constructible). The `sepc` advance and `sret` live in the
//! freestanding trap handler.

use core::sync::atomic::{AtomicUsize, Ordering};

use rustos_abi::SYSCALL_MAX_ARGS;

use crate::trap::{TrapFrame, SCAUSE_INTERRUPT_BIT};

/// `scause` cause code for an Environment call from U-mode (privileged
/// spec table 4.2) — the user-space syscall path.
pub const SCAUSE_ECALL_FROM_U: u64 = 8;

/// `scause` cause code for an Environment call from S-mode. Not used by
/// the user syscall path; matched only so the handler can distinguish
/// it from an unexpected fault.
pub const SCAUSE_ECALL_FROM_S: u64 = 9;

/// Byte length of the `ecall` instruction. `sepc` is advanced by this
/// after dispatch so `sret` resumes past the trap.
pub const ECALL_INSTR_LEN: u64 = 4;

/// `true` iff `scause` denotes an environment call from U-mode (the
/// interrupt bit is clear and the cause code is [`SCAUSE_ECALL_FROM_U`]).
#[must_use]
pub const fn is_ecall_from_user(scause: u64) -> bool {
    (scause & SCAUSE_INTERRUPT_BIT) == 0 && scause == SCAUSE_ECALL_FROM_U
}

/// Pack the six riscv64 syscall argument registers into the canonical
/// `rustos_abi` layout. The order matches the ABI definition pinned in
/// `lib/abi/src/syscalls.rs`, identical to the x86_64 port's
/// `pack_raw_args` (one ABI).
#[must_use]
pub const fn pack_raw_args(
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
) -> [u64; SYSCALL_MAX_ARGS] {
    [a0, a1, a2, a3, a4, a5]
}

/// Signature of the Rust callback the ecall path forwards each syscall
/// to. `number` is the user's `a7`; `args_ptr` points at a
/// `[u64; SYSCALL_MAX_ARGS]` the handler built on its stack. The return
/// value is written back into the trap frame's `a0`. Identical to the
/// x86_64 `SyscallDispatchFn` so the binary installs one shim shape.
pub type SyscallDispatchFn =
    extern "C" fn(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64;

/// Atomically-stored dispatch callback (`0` = none installed).
static SYSCALL_DISPATCH_CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// Install the per-binary dispatch callback. Called once during boot,
/// before user space is entered. Storing a `fn` (not a closure) keeps
/// it safe to invoke from trap context.
pub fn set_dispatch_callback(cb: SyscallDispatchFn) {
    SYSCALL_DISPATCH_CALLBACK.store(cb as usize, Ordering::Release);
}

/// Read back the installed dispatch callback, if any. Test/diagnostic.
#[must_use]
pub fn dispatch_callback() -> Option<SyscallDispatchFn> {
    let raw = SYSCALL_DISPATCH_CALLBACK.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `SYSCALL_DISPATCH_CALLBACK`
        // round-trips a valid `SyscallDispatchFn` through
        // `set_dispatch_callback`.
        Some(unsafe { core::mem::transmute::<usize, SyscallDispatchFn>(raw) })
    }
}

#[cfg(test)]
fn clear_dispatch_for_tests() {
    SYSCALL_DISPATCH_CALLBACK.store(0, Ordering::Release);
}

/// Dispatch an `ecall` captured in `frame` to the installed callback.
///
/// Reads the syscall number from `frame.a7` and the arguments from
/// `a0`–`a5`, forwards them to the dispatch callback, and writes the
/// result back into `frame.a0`. Returns `false` (and leaves `frame`
/// untouched) when no callback is installed — the freestanding trap
/// handler treats that as a fail-closed condition,
/// exactly as the x86_64 trampoline does.
///
/// `sepc` is **not** advanced here: that is a CSR operation owned by the
/// freestanding handler, which calls this function and then steps
/// `sepc` past the `ecall` on success.
#[must_use]
pub fn dispatch_ecall(frame: &mut TrapFrame) -> bool {
    let Some(cb) = dispatch_callback() else {
        return false;
    };
    let args = pack_raw_args(frame.a0, frame.a1, frame.a2, frame.a3, frame.a4, frame.a5);
    let ret = cb(frame.a7, &args);
    frame.a0 = ret;
    true
}

#[cfg(test)]
#[path = "syscall_entry_tests.rs"]
mod tests;
