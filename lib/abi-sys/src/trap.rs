//! The architecture-specific syscall trap — the §1 assembly carve-out.
//!
//! # Why this file contains assembly (`AGENTS.md` §1 justification)
//!
//! RustOS is Rust-only; assembly is permitted only where the architecture
//! *strictly* requires it. Issuing the user→kernel transition is exactly
//! such a case: the `syscall` (x86_64), `svc` (`AArch64`), and `ecall`
//! (RISC-V) instructions have no Rust spelling, and placing the syscall
//! number and arguments in the exact registers the kernel's per-arch entry
//! path reads (`kernel/arch/*/src/syscall_entry.rs`) cannot be expressed
//! in safe Rust. This module is the minimal primitive the whole crate
//! exists to wrap; every byte of assembly here is one trap instruction plus
//! its register marshalling, encapsulated behind the safe-to-misuse
//! [`raw_syscall`] boundary (the kernel re-validates every argument on the
//! far side of the trap, `AGENTS.md` §5.4 / `plans/CCOMPAT.md` §4).
//!
//! Each per-architecture block is gated on a build-script-emitted cfg
//! (`abi_sys_trap_x86_64` / `_aarch64` / `_riscv64`, see `build.rs`) rather
//! than a target-architecture predicate, so the instruction-set choice stays
//! out of the source tree the §17.2 `cfg-check` guards.
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

use rustos_abi::SYSCALL_MAX_ARGS;

#[cfg(abi_sys_trap_x86_64)]
#[inline]
pub(crate) unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
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

#[cfg(abi_sys_trap_aarch64)]
#[inline]
pub(crate) unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
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

#[cfg(abi_sys_trap_riscv64)]
#[inline]
pub(crate) unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
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
// targets) there is no kernel to trap into. The stub functions still build
// and link — the C type surface is valid everywhere (`plans/CCOMPAT.md` §1)
// — but `raw_syscall` routes through a test-injectable seam instead of an
// instruction. This mirrors the kernel side, whose pure marshalling is
// host-tested while the trap instruction itself is exercised only under
// QEMU (`kernel/arch/*/src/syscall_entry.rs`).

/// Sentinel returned by [`raw_syscall`] on the host when no trap seam is
/// installed. There is no kernel to service the call, so the runtime fails
/// closed with an all-ones value rather than fabricating a plausible result
/// (`AGENTS.md` §5.4.5). Production builds never reach this path: the crate
/// only ships for the three native targets, where `raw_syscall` is the real
/// trap instruction above.
#[cfg(not(any(abi_sys_trap_x86_64, abi_sys_trap_aarch64, abi_sys_trap_riscv64)))]
pub(crate) const HOST_NO_TRAP: u64 = u64::MAX;

#[cfg(not(any(abi_sys_trap_x86_64, abi_sys_trap_aarch64, abi_sys_trap_riscv64)))]
#[inline]
pub(crate) unsafe fn raw_syscall(number: u64, args: [u64; SYSCALL_MAX_ARGS]) -> u64 {
    #[cfg(test)]
    if let Some(result) = crate::seam::dispatch(number, &args) {
        return result;
    }
    let _ = (number, args);
    HOST_NO_TRAP
}
