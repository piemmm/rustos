//! aarch64 implementation of the Arch HAL "enter user mode" surface
//! ([`rustos_arch_api::EnterUser`], `AGENTS.md` §17.2).
//!
//! Dropping a freshly built process image into EL0 is the `eret`
//! sequence: program `SP_EL0` with the user stack pointer, `ELR_EL1`
//! with the entry point, and `SPSR_EL1` to "EL0t with IRQ unmasked" (so
//! EL0 runs **preemptible** — a generic-timer interrupt taken in user
//! mode drives the P-1 preemptive reschedule, `plans/PI.md` D2b-2b-A
//! P-1), set the first-argument register `x0`, then `eret` — a
//! context-synchronising EL1→EL0 transition. This is the one definition
//! of that sequence (`AGENTS.md` §2.2); the CC2/CC3 QEMU verticals reach
//! it through the HAL rather than copying the `asm!` block.

use rustos_arch_api::{EnterUser, UserEntry};

/// aarch64 implementation of the Arch HAL "enter user mode" surface.
///
/// Zero-sized: the `eret` transition needs no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct UserMode;

impl UserMode {
    /// Construct the aarch64 enter-user handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl EnterUser for UserMode {
    unsafe fn enter_user(&self, regs: UserEntry) -> ! {
        // SAFETY: the caller's `EnterUser::enter_user` contract
        // guarantees `regs.entry` is an EL0-executable VA and
        // `regs.stack_pointer` an EL0-writable stack top in the active
        // address space, and that the EL1 vector table is installed.
        unsafe { enter_el0(regs.entry, regs.stack_pointer, regs.arg0) }
    }
}

/// `SPSR.I` — the IRQ mask bit (bit 7 of `[9:6]` `DAIF` = `D A I F`).
/// Clearing it lets EL0 take IRQs.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const SPSR_DAIF_IRQ: u64 = 1 << 7;

/// `SPSR_EL1` for an `eret` to EL0t (`M[3:0] = 0b0000`) with `DAIF` set
/// to mask Debug/SError/FIQ but leave **IRQ unmasked** (bit 7 clear), so
/// EL0 is preemptible: a generic-timer (or device) interrupt taken in
/// user mode traps to the EL1 `LOWER_IRQ` vector and drives the P-1
/// preemptive reschedule (`crate::exceptions::handle_irq` /
/// `crate::preempt::on_el0_preempt_point`). FIQ and SError stay masked
/// (the kernel routes neither to EL0), and Debug stays masked; only the
/// IRQ unmask is required for preemption (`AGENTS.md` §2.16 — preemption
/// is a first-class scheduling goal; §4 — the kernel itself stays
/// non-preemptible, EL1 ticks never switch away).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const SPSR_EL0T_PREEMPTIBLE: u64 = (0b1111 << 6) & !SPSR_DAIF_IRQ;

/// Drop to EL0 at `entry` with `SP_EL0` = `sp` and `x0` set.
///
/// # Safety
///
/// See [`EnterUser::enter_user`]: `entry` must be a valid EL0-executable
/// virtual address, `sp` a valid EL0-writable stack top, and the EL1
/// vector table must be installed. Diverges via `eret`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe fn enter_el0(entry: u64, sp: u64, x0: u64) -> ! {
    // SAFETY: the §1-sanctioned assembly carve-out (no Rust spelling for
    // `eret` or the EL1 system-register writes). Writing
    // `SP_EL0`/`ELR_EL1`/`SPSR_EL1` loads the EL0 entry state; `eret`
    // performs the documented EL1→EL0 transition (a context-synchronising
    // event). The caller's safety contract guarantees the mapped
    // entry/stack. `options(noreturn)` matches the divergence.
    unsafe {
        core::arch::asm!(
            "msr SP_EL0, {sp}",
            "msr ELR_EL1, {entry}",
            "msr SPSR_EL1, {spsr}",
            "eret",
            sp = in(reg) sp,
            entry = in(reg) entry,
            spsr = in(reg) SPSR_EL0T_PREEMPTIBLE,
            in("x0") x0,
            options(noreturn, nostack),
        );
    }
}

/// Host substitute: the `eret` transition is meaningful only on the
/// bare-metal aarch64 target, so the host build cannot perform it. It is
/// never linked into a kernel image and never reached on the host (the
/// QEMU verticals exercise the real transition).
///
/// # Safety
///
/// Never call on the host; see [`EnterUser::enter_user`] for the
/// bare-metal contract.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
unsafe fn enter_el0(_entry: u64, _sp: u64, _x0: u64) -> ! {
    unreachable!("enter_el0 is only meaningful on the bare-metal aarch64 target")
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
