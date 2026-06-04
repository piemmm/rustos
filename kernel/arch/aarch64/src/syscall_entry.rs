//! aarch64 `svc` syscall entry path.
//!
//! Like riscv64's `ecall`, aarch64 raises a *Supervisor Call* (`svc`)
//! synchronous exception when EL0 code performs a syscall; it enters the
//! same EL1 vector ([`crate::exceptions`]) as every other exception from
//! a lower EL. This module owns the syscall-specific slice of that path:
//!
//! * The `ESR_EL1.EC` code for an `svc` from AArch64 ([`EC_SVC_AARCH64`])
//!   the handler matches to distinguish a syscall from a user fault.
//! * Marshalling the register-passed arguments (`x0`–`x5`) and the
//!   syscall number (`x8`) out of the saved register frame into the
//!   architecture-neutral `rustos_abi` `[u64; SYSCALL_MAX_ARGS]` layout —
//!   the same layout the x86_64 and riscv64 ports build (`AGENTS.md`
//!   §2.2 — one ABI, no duplication).
//! * The dispatch callback the syscall path forwards each `svc` to,
//!   mirroring the [`crate::preempt`] timer-callback design. The
//!   architecture-neutral validation / capability / audit dispatcher
//!   lives in `kernel/syscall` and is installed by the downstream boot
//!   binary; the arch port never re-implements it.
//!
//! # Calling convention
//!
//! RustOS follows the established AArch64 Linux register convention: the
//! syscall number is in `x8`, arguments in `x0`–`x5` (six — exactly
//! `SYSCALL_MAX_ARGS`), and the result is returned in `x0`. The PE sets
//! `ELR_EL1` to the instruction *after* the `svc`, so unlike riscv64's
//! `ecall` no manual PC advance is needed.
//!
//! # Host testability
//!
//! The `ESR` decode, the argument packing, the callback storage, and
//! [`dispatch_svc`] all build and are unit-tested on the host
//! ([`SyscallFrame`] is host-constructible).

use core::sync::atomic::{AtomicUsize, Ordering};

use rustos_abi::SYSCALL_MAX_ARGS;

use crate::fault::exception_class;

/// `ESR_EL1.EC` code for an `svc` executed in AArch64 state (ARM ARM
/// Table D17-2). The user-space syscall path.
pub const EC_SVC_AARCH64: u64 = 0b01_0101;

/// `true` iff `esr` denotes an `svc` from AArch64 — the exception class
/// is [`EC_SVC_AARCH64`].
#[must_use]
pub const fn is_svc(esr: u64) -> bool {
    exception_class(esr) == EC_SVC_AARCH64
}

/// The subset of the saved register frame the syscall path reads.
///
/// `x[0..6]` are the argument registers and `x8` carries the syscall
/// number, matching the AArch64 syscall convention. Host-constructible
/// so [`dispatch_svc`] is unit-testable without a frame on the stack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyscallFrame {
    /// Argument registers `x0`–`x5`. `x0` also receives the result.
    pub args: [u64; SYSCALL_MAX_ARGS],
    /// Syscall number register `x8`.
    pub number: u64,
}

/// Pack the six aarch64 syscall argument registers into the canonical
/// `rustos_abi` layout. The order matches the ABI definition pinned in
/// `lib/abi/src/syscalls.rs`, identical to the x86_64 and riscv64 ports
/// (`AGENTS.md` §2.2 — one ABI).
#[must_use]
pub const fn pack_raw_args(
    x0: u64,
    x1: u64,
    x2: u64,
    x3: u64,
    x4: u64,
    x5: u64,
) -> [u64; SYSCALL_MAX_ARGS] {
    [x0, x1, x2, x3, x4, x5]
}

/// Number of general-purpose registers (`x0`–`x30`) the EL1 vector
/// trampoline (`vectors.s`) saves into its register frame, in order. The
/// frame is read as a `[u64; SAVED_GPRS]` to recover the syscall
/// registers (`AGENTS.md` §2.2 — the frame layout has one definition,
/// pinned by `frame_offsets_match_vectors_s`).
pub const SAVED_GPRS: usize = 31;

/// Recover the syscall [`SyscallFrame`] from the saved GP register frame
/// the EL1 vector trampoline builds.
///
/// `saved[i]` is `xi`, so the arguments are `x0`–`x5` (`saved[0..6]`) and
/// the syscall number is `x8` (`saved[8]`), per the AArch64 syscall
/// convention. Pure and host-testable: the freestanding exception
/// handler reads the frame into a `[u64; SAVED_GPRS]` and calls this,
/// keeping the register-extraction logic off the unsafe asm path
/// (mirroring how riscv64's host-constructible `TrapFrame` is read).
#[must_use]
pub fn syscall_frame_from_saved(saved: &[u64; SAVED_GPRS]) -> SyscallFrame {
    SyscallFrame {
        args: pack_raw_args(saved[0], saved[1], saved[2], saved[3], saved[4], saved[5]),
        number: saved[8],
    }
}

/// Signature of the Rust callback the syscall path forwards each syscall
/// to. `number` is the user's `x8`; `args_ptr` points at a
/// `[u64; SYSCALL_MAX_ARGS]` the handler built on its stack. The return
/// value is written back into the frame's `x0`. Identical to the x86_64
/// and riscv64 dispatch signatures so the binary installs one shim
/// shape.
pub type SyscallDispatchFn =
    extern "C" fn(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64;

/// Atomically-stored dispatch callback (`0` = none installed).
static SYSCALL_DISPATCH_CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// Install the per-binary dispatch callback. Called once during boot,
/// before user space is entered. Storing a `fn` (not a closure) keeps it
/// safe to invoke from exception context.
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

/// Dispatch an `svc` captured in `frame` to the installed callback.
///
/// Reads the syscall number from `frame.number` (`x8`) and the arguments
/// from `frame.args` (`x0`–`x5`), forwards them to the dispatch
/// callback, and writes the result back into `frame.args[0]` (`x0`).
/// Returns `false` (and leaves `frame` untouched) when no callback is
/// installed — the exception handler treats that as a fail-closed
/// condition (`AGENTS.md` §5.4.5), exactly as the other ports do.
#[must_use]
pub fn dispatch_svc(frame: &mut SyscallFrame) -> bool {
    let Some(cb) = dispatch_callback() else {
        return false;
    };
    let args = frame.args;
    let ret = cb(frame.number, &args);
    frame.args[0] = ret;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault::ESR_EC_SHIFT;
    use core::sync::atomic::AtomicU64;

    #[test]
    fn svc_class_is_recognised() {
        assert!(is_svc(EC_SVC_AARCH64 << ESR_EC_SHIFT));
        // A data abort is not an svc.
        assert!(!is_svc(crate::fault::EC_DATA_ABORT_SAME << ESR_EC_SHIFT));
    }

    #[test]
    fn pack_preserves_argument_order() {
        assert_eq!(pack_raw_args(1, 2, 3, 4, 5, 6), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn saved_frame_extracts_args_and_number() {
        // `saved[i]` is `xi`: arguments x0–x5, number x8. Distinct
        // sentinels so a swapped index is caught.
        let mut saved = [0u64; SAVED_GPRS];
        for (i, slot) in saved.iter_mut().enumerate() {
            *slot = 0x100 + i as u64;
        }
        let frame = syscall_frame_from_saved(&saved);
        assert_eq!(frame.args, [0x100, 0x101, 0x102, 0x103, 0x104, 0x105]);
        assert_eq!(frame.number, 0x108);
        // x6, x7 (between the args and the number) are not read.
        assert_eq!(SAVED_GPRS, 31);
    }

    #[test]
    fn ec_svc_matches_arm_arm() {
        assert_eq!(EC_SVC_AARCH64, 0x15);
    }

    static LAST_NUMBER: AtomicU64 = AtomicU64::new(0);
    static LAST_ARG0: AtomicU64 = AtomicU64::new(0);

    extern "C" fn host_dispatch(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
        // SAFETY: `args_ptr` points at the live `args` array `dispatch_svc`
        // built from the frame; reading it for the duration of the call
        // is sound.
        let args = unsafe { &*args_ptr };
        LAST_NUMBER.store(number, Ordering::Relaxed);
        LAST_ARG0.store(args[0], Ordering::Relaxed);
        0x1234
    }

    #[test]
    fn dispatch_forwards_and_writes_result() {
        clear_dispatch_for_tests();
        // No callback installed → fail closed, frame untouched.
        let mut frame = SyscallFrame {
            args: [9, 0, 0, 0, 0, 0],
            number: 42,
        };
        assert!(!dispatch_svc(&mut frame));
        assert_eq!(frame.args[0], 9);

        set_dispatch_callback(host_dispatch);
        assert!(dispatch_svc(&mut frame));
        assert_eq!(LAST_NUMBER.load(Ordering::Relaxed), 42);
        assert_eq!(LAST_ARG0.load(Ordering::Relaxed), 9);
        // Result written back into x0.
        assert_eq!(frame.args[0], 0x1234);
        clear_dispatch_for_tests();
    }
}
