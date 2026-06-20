//! riscv64 implementation of the Arch HAL "enter user mode" surface
//! ([`rustos_arch_api::EnterUser`], `AGENTS.md` §17.2).
//!
//! Dropping a freshly built process image into U-mode is the `sret`
//! sequence: clear `sstatus.SPP` (so `sret` targets U-mode) and
//! `sstatus.SPIE` (so `sstatus.SIE` is `0` once back in U-mode), set
//! `sstatus.SUM` (so the S-mode trap handler that runs after the
//! program's `ecall` may touch the U-bit user stack), load `sepc` with
//! the entry point and `sp`/`a0` with the stack pointer and first
//! argument, then `sret`. This is the one definition of that sequence
//! (`AGENTS.md` §2.2); the CC2/CC3 QEMU verticals reach it through the
//! HAL rather than copying the `asm!` block.
//!
//! # U-mode preemptibility
//!
//! Clearing `SPIE` does **not** make U-mode uninterruptible. A
//! supervisor interrupt is taken whenever the hart runs at a privilege
//! *below* S-mode (the privileged-spec rule: priv `U` < `S`), regardless
//! of `sstatus.SIE`; `SIE`/`SPIE` only gate interrupts taken *in S-mode*.
//! So once the boot path arms the supervisor timer (`sie.STIE` +
//! `crate::preempt::init_local_preempt`), a runaway U-mode task is
//! involuntarily preempted by the timer trap — the riscv64 analogue of
//! aarch64's preemptible-EL0 `SPSR` (`plans/PI.md` D2b-2b-A P-1b).
//! Leaving `SIE` clear keeps the *kernel* non-preemptible (`AGENTS.md`
//! §4): a tick taken while in S-mode never fires the preempt point
//! (`crate::trap` gates it on the saved `SPP`).

use rustos_arch_api::{EnterUser, UserEntry};

/// riscv64 implementation of the Arch HAL "enter user mode" surface.
///
/// Zero-sized: the `sret` transition needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct UserMode;

impl UserMode {
    /// Construct the riscv64 enter-user handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl EnterUser for UserMode {
    unsafe fn enter_user(&self, regs: UserEntry) -> ! {
        // SAFETY: the caller's `EnterUser::enter_user` contract
        // guarantees `regs.entry` is a U-accessible executable VA and
        // `regs.stack_pointer` a U-accessible writable stack top in the
        // active address space, and that the trap vector is installed.
        unsafe { enter_user_mode(regs.entry, regs.stack_pointer, regs.arg0) }
    }
}

/// `sstatus.SUM` — permit S-mode data access to U-bit pages (bit 18).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
const SSTATUS_SUM: u64 = 1 << 18;
/// `sstatus.SPP` (bit 8) | `sstatus.SPIE` (bit 5): cleared so `sret`
/// enters U-mode with `sstatus.SIE == 0`. This does not block
/// preemption — a supervisor-timer interrupt is still taken in U-mode
/// because the hart runs below S-mode (see the module docs); `SIE` only
/// governs interrupts taken in S-mode.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
const SSTATUS_SPP_SPIE: u64 = (1 << 8) | (1 << 5);

/// Drop to U-mode at `entry` with stack pointer `sp` and `a0` set.
///
/// # Safety
///
/// See [`EnterUser::enter_user`]: `entry` must be a valid U-accessible
/// executable virtual address, `sp` a valid U-accessible writable stack
/// top, and the trap vector must be installed. Diverges via `sret`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
unsafe fn enter_user_mode(entry: u64, sp: u64, a0: u64) -> ! {
    // SAFETY: the §1-sanctioned assembly carve-out (no Rust spelling for
    // `sret` or the `sstatus` CSR edits). `csrs`/`csrc` set/clear exactly
    // the named bits; `csrw sscratch, sp` (with `sp` still the kernel
    // stack pointer) arms the per-task kernel-stack top the trap vector
    // swaps to on the next U->S trap (`trap.s`); `csrw sepc` and the
    // `sp`/`a0` moves load the U-mode entry state; `sret` performs the
    // documented S→U transition. The caller's safety contract guarantees
    // the mapped entry/stack. `options(noreturn)` matches the divergence.
    unsafe {
        core::arch::asm!(
            "csrs sstatus, {sum}",
            "csrc sstatus, {clr}",
            "csrw sscratch, sp",
            "csrw sepc, {entry}",
            "mv sp, {sp}",
            "sret",
            sum = in(reg) SSTATUS_SUM,
            clr = in(reg) SSTATUS_SPP_SPIE,
            entry = in(reg) entry,
            sp = in(reg) sp,
            in("a0") a0,
            options(noreturn, nostack),
        );
    }
}

/// Host substitute: the `sret` transition is meaningful only on the
/// bare-metal riscv64 target, so the host build cannot perform it. It is
/// never linked into a kernel image and never reached on the host (the
/// QEMU verticals exercise the real transition).
///
/// # Safety
///
/// Never call on the host; see [`EnterUser::enter_user`] for the
/// bare-metal contract.
#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
unsafe fn enter_user_mode(_entry: u64, _sp: u64, _a0: u64) -> ! {
    unreachable!("enter_user_mode is only meaningful on the bare-metal riscv64 target")
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
