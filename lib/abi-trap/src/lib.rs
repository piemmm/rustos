//! `rustos-abi-trap` — the raw `abi-v1` user→kernel syscall trap primitive.
//!
//! This crate is the single home of the architecture-specific syscall trap:
//! the `syscall` (x86_64), `svc` (`AArch64`), and `ecall` (RISC-V)
//! instruction plus the register marshalling the kernel's per-arch entry path
//! reads (`kernel/arch/*/src/syscall_entry.rs`). It is the §1 assembly
//! carve-out that the whole user→kernel transition is built on.
//!
//! It exists so the trap exists **exactly once** (`AGENTS.md` §2.2). Two
//! consumers build on it:
//!
//! * `rustos-abi-sys` — the C-callable `ros_sys_<name>` stub runtime, the
//!   curated *System runtime / C ABI* class a program **not** written in Rust
//!   links (`AGENTS.md` §9, §16.4).
//! * `rustos-rt` — the pure-Rust userland runtime that first-party RustOS
//!   programs link. RustOS code is Rust-only and never routes through the C
//!   ABI meant for third parties (`AGENTS.md` §1).
//!
//! # Not a privileged path
//!
//! [`raw_syscall`] adds **no** authority. Every capability check and every
//! input validation happens kernel-side, on the far side of the trap
//! (`AGENTS.md` §5.4); a caller reaches no syscall it could not reach
//! otherwise. Because the kernel re-validates every argument and fails
//! closed, no argument value passed here can cause undefined behaviour beyond
//! what the trap's own `# Safety` contract already covers.
//!
//! # Targets
//!
//! The trap instruction is compiled in only for the three native Tier-1
//! targets (`x86_64`, `aarch64`, `riscv64`); see the per-arch blocks below,
//! each gated on a build-script-emitted `abi_trap_<arch>` cfg (`build.rs`)
//! rather than a target-architecture predicate, so the instruction-set choice
//! stays out of the source tree the §17.2 `cfg-check` guards. `wasm32` has no
//! trap instruction and is out of scope (`plans/CCOMPAT.md` §1). On the host
//! there is no kernel: [`raw_syscall`] fails closed with [`HOST_NO_TRAP`],
//! optionally routed through the `seam` test scaffolding (the `host-seam`
//! feature).
//!
//! # Calling convention
//!
//! The register assignment matches the kernel entry path on each target
//! (`AGENTS.md` §2.2 — one ABI, no duplication):
//!
//! | Target  | Number | Arguments               | Result |
//! |---------|--------|-------------------------|--------|
//! | x86_64  | `rax`  | `rdi rsi rdx r10 r8 r9` | `rax`  |
//! | aarch64 | `x8`   | `x0 x1 x2 x3 x4 x5`     | `x0`   |
//! | riscv64 | `a7`   | `a0 a1 a2 a3 a4 a5`     | `a0`   |

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// The host trap seam is test scaffolding that needs thread-local storage, and
// thus `std`. It is compiled only on the host path (never a native target) and
// only under the `host-seam` feature, which is reached solely through a
// `dev-dependencies` edge — so a shipping build is unaffected and stays
// `no_std` (`AGENTS.md` §6).
#[cfg(all(not(abi_trap_native), feature = "host-seam"))]
extern crate std;

use rustos_abi::SYSCALL_MAX_ARGS;

/// Issue the raw `abi-v1` syscall trap: place `number` and `args` in the
/// per-architecture syscall registers, execute the user→kernel trap, and
/// return the kernel's raw result register.
///
/// This is the lowest-level primitive both userland runtimes wrap; callers
/// give it the already-marshalled register values.
///
/// # Safety
///
/// The trap performs the architectural privilege transition into the kernel.
/// The kernel validates every argument before acting on it (`AGENTS.md`
/// §5.4), so no register *value* supplied here can violate memory safety;
/// however, a syscall may read or write caller memory described by `args`
/// (e.g. an IPC or console buffer), so the caller must ensure any pointer/len
/// pair it marshals into `args` denotes memory it may legitimately expose for
/// the duration of the call.
#[cfg(abi_trap_x86_64)]
#[inline]
#[must_use]
pub unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
    let ret: u64;
    // SAFETY: `syscall` performs the architectural ring-3→ring-0 transition
    // to the kernel's `IA32_LSTAR` entry stub. It clobbers `rcx` (saved RIP)
    // and `r11` (saved RFLAGS), declared as clobbered `lateout`s; no other
    // register state is assumed preserved by us beyond what the calling
    // convention guarantees. The kernel reads `rax`/`rdi`/`rsi`/`rdx`/`r10`/
    // `r8`/`r9` (`pack_raw_args`) and writes the result back into `rax`; it
    // validates every argument before acting on it (`AGENTS.md` §5.4), so no
    // register value supplied here can violate this function's safety. We do
    // not assert `nomem`/`nostack`: a syscall may legitimately read or write
    // caller memory (e.g. an IPC buffer).
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") number,
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// Issue the raw `abi-v1` syscall trap (aarch64 `svc #0`).
///
/// See the x86_64 overload for the contract.
///
/// # Safety
///
/// See the x86_64 [`raw_syscall`] overload.
#[cfg(abi_trap_aarch64)]
#[inline]
#[must_use]
pub unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
    let ret: u64;
    // SAFETY: `svc #0` raises a Supervisor Call exception into the kernel's
    // EL1 vector; the kernel reads `x8` (number) and `x0`–`x5` (arguments)
    // and writes the result back into `x0`. The AArch64 syscall convention
    // preserves every other register across the call, so only `x0` is
    // declared as written. The kernel validates every argument before acting
    // on it (`AGENTS.md` §5.4); no value supplied here can violate safety.
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") number,
            inout("x0") args[0] => ret,
            in("x1") args[1],
            in("x2") args[2],
            in("x3") args[3],
            in("x4") args[4],
            in("x5") args[5],
        );
    }
    ret
}

/// Issue the raw `abi-v1` syscall trap (riscv64 `ecall`).
///
/// See the x86_64 overload for the contract.
///
/// # Safety
///
/// See the x86_64 [`raw_syscall`] overload.
#[cfg(abi_trap_riscv64)]
#[inline]
#[must_use]
pub unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
    let ret: u64;
    // SAFETY: `ecall` raises an Environment Call exception into the kernel's
    // S-mode trap vector; the kernel reads `a7` (number) and `a0`–`a5`
    // (arguments) and writes the result back into `a0`. The RISC-V syscall
    // convention preserves every other register across the call, so only
    // `a0` is declared as written. The kernel validates every argument
    // before acting on it (`AGENTS.md` §5.4); no value supplied here can
    // violate safety.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") number,
            inout("a0") args[0] => ret,
            in("a1") args[1],
            in("a2") args[2],
            in("a3") args[3],
            in("a4") args[4],
            in("a5") args[5],
        );
    }
    ret
}

// --- Host trap seam -------------------------------------------------------
//
// On the host (and any target that is not one of the three native Tier-1
// targets) there is no kernel to trap into. `raw_syscall` still builds and
// links — its callers' marshalling is host-tested — but routes through a
// test-injectable seam (the `host-seam` feature) instead of an instruction.
// This mirrors the kernel side, whose pure marshalling is host-tested while
// the trap instruction itself is exercised only under QEMU
// (`kernel/arch/*/src/syscall_entry.rs`).

/// Sentinel returned by [`raw_syscall`] on the host when no trap seam is
/// installed. There is no kernel to service the call, so the primitive fails
/// closed with an all-ones value rather than fabricating a plausible result
/// (`AGENTS.md` §5.4.5). Production builds never reach this path: the trap
/// instruction above is compiled in for the three native targets.
#[cfg(not(abi_trap_native))]
pub const HOST_NO_TRAP: u64 = u64::MAX;

/// Issue the raw syscall trap (host fallback — no kernel present).
///
/// Returns [`HOST_NO_TRAP`], or the value armed in the `seam` module when the
/// `host-seam` feature is enabled.
///
/// # Safety
///
/// Trivially safe on the host (it performs no trap), but kept `unsafe` so the
/// signature matches the native overloads and callers need not branch.
#[cfg(not(abi_trap_native))]
#[inline]
#[must_use]
pub unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
    #[cfg(feature = "host-seam")]
    if let Some(result) = seam::dispatch(number, &args) {
        return result;
    }
    let _ = (number, args);
    HOST_NO_TRAP
}

/// Host-only test scaffolding (`host-seam` feature): a per-thread injectable
/// replacement for the real trap instruction, used to assert the marshalling
/// and return-decoding of a syscall wrapper on the host without a kernel
/// (`plans/CCOMPAT.md` CC2 "trap injected behind a seam"). A test arms the
/// seam with the value the "kernel" should return, calls a wrapper, then
/// inspects the recorded `(number, args)`.
#[cfg(all(not(abi_trap_native), feature = "host-seam"))]
pub mod seam {
    use rustos_abi::SYSCALL_MAX_ARGS;
    use std::cell::Cell;

    std::thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static RETURN_VALUE: Cell<u64> = const { Cell::new(0) };
        static LAST_CALL: Cell<Option<(u64, [u64; SYSCALL_MAX_ARGS])>> = const { Cell::new(None) };
    }

    /// Arm the seam for the current thread: the next trap returns `value` and
    /// its `(number, args)` are recorded for inspection.
    pub fn arm(value: u64) {
        RETURN_VALUE.with(|v| v.set(value));
        LAST_CALL.with(|c| c.set(None));
        ARMED.with(|a| a.set(true));
    }

    /// The `(number, args)` of the most recent trap on this thread, or `None`
    /// if no trap has been issued since [`arm`].
    #[must_use]
    pub fn last_call() -> Option<(u64, [u64; SYSCALL_MAX_ARGS])> {
        LAST_CALL.with(Cell::get)
    }

    /// Called by `raw_syscall` on the host. Records the call and returns the
    /// armed value, or `None` when not armed (so the fail-closed sentinel
    /// path stays reachable).
    pub(crate) fn dispatch(number: u64, args: &[u64; SYSCALL_MAX_ARGS]) -> Option<u64> {
        if !ARMED.with(Cell::get) {
            return None;
        }
        LAST_CALL.with(|c| c.set(Some((number, *args))));
        Some(RETURN_VALUE.with(Cell::get))
    }
}

#[cfg(all(test, not(abi_trap_native)))]
mod tests {
    use super::*;

    #[test]
    fn host_trap_fails_closed_when_unarmed() {
        // With no seam armed, the host fallback must return the fail-closed
        // sentinel rather than fabricating a result (`AGENTS.md` §5.4.5).
        // SAFETY: the host overload performs no trap.
        let ret = unsafe { raw_syscall(0, [0; SYSCALL_MAX_ARGS]) };
        assert_eq!(ret, HOST_NO_TRAP);
    }
}
