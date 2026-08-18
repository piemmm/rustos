//! riscv64 implementation of the Arch HAL "enter user mode" surface
//! ([`tairix_arch_api::EnterUser`]).
//!
//! Dropping a freshly built process image into U-mode is the `sret`
//! sequence: set `sstatus.SUM` (so the S-mode trap handler that runs after
//! the program's `ecall` may touch the U-bit user stack), clear
//! `sstatus.SPP`/`SPIE` (so `sret` targets U-mode with `sstatus.SIE == 0`
//! there) and `sstatus.SIE` (so no S-mode interrupt can be taken across the
//! rest of the sequence — the `sscratch` arm below depends on it), arm
//! `sscratch` with the kernel-stack top, load `sepc` with the entry point and
//! `sp`/`a0` with the stack pointer and first argument, then `sret`. This is
//! the one definition of that sequence; the CC2/CC3 QEMU verticals reach it
//! through the HAL rather than copying the `asm!` block.
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
//! Leaving `SIE` clear keeps the *kernel* non-preemptible: a tick taken while in S-mode never fires the preempt point
//! (`crate::trap` gates it on the saved `SPP`).

use tairix_arch_api::{EnterUser, UserEntry};

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

/// `sstatus.SIE` — S-mode interrupt enable (bit 1).
const SSTATUS_SIE: u64 = 1 << 1;
/// `sstatus.SPIE` — the S-mode interrupt enable `sret` restores (bit 5).
const SSTATUS_SPIE: u64 = 1 << 5;
/// `sstatus.SPP` — the privilege `sret` returns to; clear means U (bit 8).
const SSTATUS_SPP: u64 = 1 << 8;
/// `sstatus.SUM` — permit S-mode data access to U-bit pages (bit 18).
const SSTATUS_SUM: u64 = 1 << 18;

/// The `sstatus` bits [`enter_user_mode`] sets before `sret`.
const USER_ENTRY_SSTATUS_SET: u64 = SSTATUS_SUM;

/// The `sstatus` bits [`enter_user_mode`] clears before `sret`: `SPP` and
/// `SPIE` aim the transition at U-mode, and `SIE` masks S-mode interrupts
/// for the rest of the sequence.
///
/// `SIE` is what makes arming `sscratch` safe. The trap vector reads a
/// non-zero `sscratch` as "this trap came from U-mode", so an interrupt
/// taken in S-mode while it is armed is misclassified: it overwrites the
/// caller's frame and returns through the S-mode path, which leaves
/// `sscratch` zero, and the task then runs in U-mode with no kernel stack
/// armed — every later trap of that task builds its frame on the task's own
/// *user* stack. The aarch64 sibling masks `DAIF` for the same reason.
///
/// The mask never reaches the entered task: `sret` restores U-mode's
/// interrupt state from `SPIE`, and U-mode is preemptible regardless
/// because the hart runs below S-mode (see the module docs).
const USER_ENTRY_SSTATUS_CLEAR: u64 = SSTATUS_SPP | SSTATUS_SPIE | SSTATUS_SIE;

// Masking `SIE` is load-bearing, and the two masks must not fight: a bit in
// both would leave the order of the `csrs`/`csrc` pair deciding the outcome.
const _: () = assert!(USER_ENTRY_SSTATUS_CLEAR & SSTATUS_SIE != 0);
const _: () = assert!(USER_ENTRY_SSTATUS_SET & USER_ENTRY_SSTATUS_CLEAR == 0);

/// Drop to U-mode at `entry` with stack pointer `sp` and `a0` set.
///
/// # Safety
///
/// See [`EnterUser::enter_user`]: `entry` must be a valid U-accessible
/// executable virtual address, `sp` a valid U-accessible writable stack
/// top, and the trap vector must be installed. Diverges via `sret`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
unsafe fn enter_user_mode(entry: u64, sp: u64, a0: u64) -> ! {
    // SAFETY: the-sanctioned assembly carve-out (no Rust spelling for
    // `sret` or the `sstatus` CSR edits). `csrs`/`csrc` set/clear exactly
    // the named bits, and the `csrc` — which masks `SIE` — precedes the
    // `csrw sscratch` it protects; `csrw sscratch, sp` (with `sp` still the
    // kernel stack pointer) arms the per-task kernel-stack top the trap
    // vector swaps to on the next U->S trap (`trap.s`); `csrw sepc` and the
    // `sp`/`a0` moves load the U-mode entry state; `sret` performs the
    // documented S→U transition. The caller's safety contract guarantees
    // the mapped entry/stack. `options(noreturn)` matches the divergence.
    unsafe {
        core::arch::asm!(
            "csrs sstatus, {set}",
            "csrc sstatus, {clr}",
            "csrw sscratch, sp",
            "csrw sepc, {entry}",
            "mv sp, {sp}",
            "sret",
            set = in(reg) USER_ENTRY_SSTATUS_SET,
            clr = in(reg) USER_ENTRY_SSTATUS_CLEAR,
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

    /// The entry sequence arms `sscratch`, which the trap vector reads as
    /// "this trap came from U-mode". Leaving `sstatus.SIE` set across that
    /// arm lets an S-mode interrupt be misclassified, and the entered task
    /// then runs with no kernel stack armed.
    #[test]
    fn user_entry_masks_supervisor_interrupts_before_arming_sscratch() {
        assert_ne!(USER_ENTRY_SSTATUS_CLEAR & SSTATUS_SIE, 0);
    }

    #[test]
    fn user_entry_targets_u_mode_with_interrupts_restored_from_spie() {
        assert_ne!(USER_ENTRY_SSTATUS_CLEAR & SSTATUS_SPP, 0);
        assert_ne!(USER_ENTRY_SSTATUS_CLEAR & SSTATUS_SPIE, 0);
    }

    #[test]
    fn user_entry_lets_the_kernel_reach_the_u_bit_user_stack() {
        assert_eq!(USER_ENTRY_SSTATUS_SET, SSTATUS_SUM);
    }

    #[test]
    fn user_entry_set_and_clear_masks_are_disjoint() {
        assert_eq!(USER_ENTRY_SSTATUS_SET & USER_ENTRY_SSTATUS_CLEAR, 0);
    }
}
