//! x86_64 implementation of the Arch HAL "enter user mode" surface
//! ([`rustos_arch_api::EnterUser`], `AGENTS.md` §17.2).
//!
//! Dropping a freshly built process image into ring 3 is the `iretq`
//! sequence: build the interrupt-return frame the CPU pops on `iretq`
//! (`SS`, user `RSP`, `RFLAGS`, `CS`, `RIP` — from the top of the
//! kernel stack down) from the ring-3 GDT selectors
//! ([`crate::gdt::USER_CS_INDEX`] / [`crate::gdt::USER_DS_INDEX`], both
//! at RPL 3), place the first-argument value in `rdi` (the System V
//! AMD64 first integer register), and `iretq`. This is the one
//! definition of that sequence (`AGENTS.md` §2.2); the CC2/CC3 QEMU
//! verticals reach it through the HAL rather than copying the `asm!`
//! block.
//!
//! # Interrupt and GS state on entry
//!
//! `RFLAGS` is built with `IF` clear, so ring 3 runs with interrupts
//! masked — matching the riscv64 (`SSTATUS.SPIE` clear) and aarch64
//! (`DAIF` masked) ports.
//!
//! `iretq` does **not** swap `GS`. The production syscall entry stub
//! (`crate::syscall_entry::syscall_entry_stub`) `swapgs`es on entry and
//! again on exit, so during normal ring-0 execution the per-CPU
//! [`SyscallTls`](crate::syscall_entry::SyscallTls) block lives in
//! `IA32_KERNEL_GS_BASE` (programmed by
//! `syscall_entry::init_local_syscalls`, freestanding-only)
//! while the active `GS` base holds the user value. Entering ring 3
//! therefore leaves `IA32_KERNEL_GS_BASE` untouched and already correct:
//! the next `syscall`'s `swapgs` recovers the kernel TLS exactly as it
//! does for a ring-3 program that the kernel never re-entered. This
//! port adds no `swapgs` of its own.

use rustos_arch_api::{EnterUser, UserEntry};

/// x86_64 implementation of the Arch HAL "enter user mode" surface.
///
/// Zero-sized: the `iretq` transition needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct UserMode;

impl UserMode {
    /// Construct the x86_64 enter-user handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl EnterUser for UserMode {
    unsafe fn enter_user(&self, regs: UserEntry) -> ! {
        // SAFETY: the caller's `EnterUser::enter_user` contract
        // guarantees `regs.entry` is a ring-3-executable VA and
        // `regs.stack_pointer` a ring-3-writable stack top in the active
        // address space, and that the syscall/exception entry path is
        // installed.
        unsafe { enter_ring3(regs.entry, regs.stack_pointer, regs.arg0) }
    }
}

/// Ring-3 user code selector — `(USER_CS_INDEX << 3) | RPL 3`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const USER_CS: u64 = ((crate::gdt::USER_CS_INDEX << 3) | 3) as u64;
/// Ring-3 stack/data selector — `(USER_DS_INDEX << 3) | RPL 3`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const USER_SS: u64 = ((crate::gdt::USER_DS_INDEX << 3) | 3) as u64;
/// `RFLAGS` for the `iretq` frame: bit 1 is the architecturally
/// reserved-one bit; `IF` (bit 9) is left clear so ring 3 starts with
/// interrupts masked (parity with the riscv64/aarch64 ports).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const USER_RFLAGS: u64 = 1 << 1;

/// Drop to ring 3 at `entry` with stack pointer `sp` and `rdi` set.
///
/// # Safety
///
/// See [`EnterUser::enter_user`]: `entry` must be a valid
/// ring-3-executable virtual address, `sp` a valid ring-3-writable
/// stack top, the GDT must carry the user code/data descriptors at
/// [`crate::gdt::USER_CS_INDEX`] / [`crate::gdt::USER_DS_INDEX`], and
/// the TSS `RSP0` plus the syscall/exception entry path must be
/// installed. Diverges via `iretq`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe fn enter_ring3(entry: u64, sp: u64, arg0: u64) -> ! {
    // SAFETY: the §1-sanctioned assembly carve-out (no Rust spelling for
    // `iretq` or the interrupt-return frame). The five `push`es build
    // the long-mode `iretq` frame on the kernel stack in the order the
    // CPU pops it (RIP last-pushed/first-popped, then CS, RFLAGS, RSP,
    // SS — SDM Vol 3A §6.14.3): pushing SS, RSP, RFLAGS, CS, RIP places
    // them at the correct offsets. `rdi` carries the first-argument
    // value (System V AMD64). `iretq` performs the documented ring-0 →
    // ring-3 transition. The caller's safety contract guarantees the
    // mapped entry/stack and the installed selectors/TSS.
    // `options(noreturn)` matches the divergence.
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {sp}",
            "push {rflags}",
            "push {cs}",
            "push {entry}",
            "iretq",
            ss = in(reg) USER_SS,
            sp = in(reg) sp,
            rflags = in(reg) USER_RFLAGS,
            cs = in(reg) USER_CS,
            entry = in(reg) entry,
            in("rdi") arg0,
            options(noreturn),
        );
    }
}

/// Host substitute: the `iretq` transition is meaningful only on the
/// bare-metal x86_64 target, so the host build cannot perform it. It is
/// never linked into a kernel image and never reached on the host (the
/// QEMU verticals exercise the real transition).
///
/// # Safety
///
/// Never call on the host; see [`EnterUser::enter_user`] for the
/// bare-metal contract.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
unsafe fn enter_ring3(_entry: u64, _sp: u64, _arg0: u64) -> ! {
    unreachable!("enter_ring3 is only meaningful on the bare-metal x86_64 target")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_mode_handle_is_object_safe() {
        let port = UserMode::new();
        let _: &dyn EnterUser = &port;
    }
}
