//! aarch64 implementation of the Arch HAL "enter user mode" surface
//! ([`tairix_arch_api::EnterUser`]).
//!
//! Dropping a freshly built process image into EL0 is the `eret`
//! sequence: mask the asynchronous exceptions, program `SP_EL0` with the
//! user stack pointer, `ELR_EL1` with the entry point, and `SPSR_EL1` to
//! "EL0t with IRQ unmasked" (so EL0 runs **preemptible** — a
//! generic-timer interrupt taken in user mode drives the P-1 preemptive
//! reschedule, `plans/PI.md` D2b-2b-A P-1), set the first-argument
//! register `x0`, then `eret` — a context-synchronising EL1→EL0
//! transition. This is the one definition of that sequence; the CC2/CC3
//! QEMU verticals reach it through the HAL rather than copying the
//! `asm!` block.
//!
//! [`crate::userentry::el0_spsr`] is that `SPSR`, and a debug image whose
//! watchdog cadence is delivered as a non-maskable FIQ additionally leaves
//! `DAIF.F` clear so the cadence can sample a core *while it runs user code*
//! (`plans/WATCHDOG.md`).
//! Every later return to EL0 restores the `SPSR` this entry established, from
//! the frame `vectors.s` saved, so this is the single place the EL0 mask state
//! is decided.

use tairix_arch_api::{EnterUser, UserEntry};

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

/// `SPSR.F` — the FIQ mask bit (bit 6 of `[9:6]` `DAIF` = `D A I F`).
/// Clearing it lets EL0 take a Group-0/FIQ interrupt.
const SPSR_DAIF_FIQ: u64 = 1 << 6;

/// `SPSR.I` — the IRQ mask bit (bit 7 of `[9:6]` `DAIF` = `D A I F`).
/// Clearing it lets EL0 take IRQs.
const SPSR_DAIF_IRQ: u64 = 1 << 7;

/// `SPSR_EL1` for an `eret` to `EL0t` (`M[3:0] = 0b0000`) with `DAIF` set
/// to mask Debug/SError/FIQ but leave **IRQ unmasked** (bit 7 clear), so
/// EL0 is preemptible: a generic-timer (or device) interrupt taken in
/// user mode traps to the EL1 `LOWER_IRQ` vector and drives the P-1
/// preemptive reschedule (`crate::exceptions::handle_irq` /
/// `crate::preempt::on_el0_preempt_point`). `SError` and Debug stay masked,
/// and FIQ stays masked in a shippable image (which routes no FIQ at all);
/// only the IRQ unmask is required for preemption — the kernel itself
/// stays non-preemptible, so an EL1 tick never switches away.
const SPSR_EL0T_PREEMPTIBLE: u64 = (0b1111 << 6) & !SPSR_DAIF_IRQ;

/// The EL0 `SPSR` this image enters user mode with: `SPSR_EL0T_PREEMPTIBLE`,
/// and with `DAIF.F` *also* clear when `fiq_cadence` says the watchdog's ~1 Hz
/// liveness cadence is delivered as a non-maskable Group-0 FIQ
/// (`crate::watchdog::fiq_cadence_enabled` — a debug build whose boot probe
/// proved FIQ reaches this kernel).
///
/// A user task must not be able to hide from the cadence that judges its core
/// alive. With `DAIF.F` masked in EL0 the FIQ-routed cadence can only ever fire
/// during a kernel entry, so a core running a CPU-bound user task is never
/// sampled: its liveness heartbeat rots, a buddy core reports it hard-locked
/// though it is demonstrably healthy (measured: a `stress --cpu` spinner took
/// one sample where its idle siblings took ~46, while taking thousands of
/// IRQs), and the forced-yield guard for a task that withholds the CPU — which
/// fires only on a *user*-context sample — can never trigger either. Clearing
/// `DAIF.F` here is what makes the "non-maskable" cadence actually
/// non-maskable, and is why `crate::exceptions::kind::LOWER_FIQ` exists.
///
/// Nothing is nested by this: the FIQ vector runs on `SP_EL1` with `DAIF.F`
/// re-masked by the PE, the FIQ arm never re-clears it, and both `eret`
/// sequences mask every asynchronous exception before they program
/// `ELR_EL1`/`SPSR_EL1`. Interrupted user code holds no kernel lock, so a
/// sample taken in EL0 is strictly safer than one taken in a kernel section.
/// Where the probe found FIQ undeliverable (a two-Security-state GIC-400, a
/// Raspberry Pi 4) and in every shippable image, `fiq_cadence` is `false` and
/// EL0 keeps FIQ masked exactly as before (fail closed).
#[must_use]
pub const fn el0_spsr(fiq_cadence: bool) -> u64 {
    if fiq_cadence {
        SPSR_EL0T_PREEMPTIBLE & !SPSR_DAIF_FIQ
    } else {
        SPSR_EL0T_PREEMPTIBLE
    }
}

/// Drop to EL0 at `entry` with `SP_EL0` = `sp` and `x0` set.
///
/// The sequence opens by masking every asynchronous exception, because
/// `ELR_EL1`/`SPSR_EL1` are single-copy registers: an interrupt taken
/// once they hold the EL0 entry state overwrites both in hardware, and
/// the handler's own return restores *its* saved pair, so this `eret`
/// would jump back to the interrupted `eret` at EL1 and spin there
/// instead of ever reaching the process. `eret` reloads PSTATE from
/// `SPSR_EL1`, so the mask never reaches EL0 — the entered task still
/// runs preemptible. The same reasoning governs the exception-return
/// epilogue in `vectors.s`.
///
/// # Safety
///
/// See [`EnterUser::enter_user`]: `entry` must be a valid EL0-executable
/// virtual address, `sp` a valid EL0-writable stack top, and the EL1
/// vector table must be installed. Diverges via `eret`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe fn enter_el0(entry: u64, sp: u64, x0: u64) -> ! {
    // Whether EL0 keeps `DAIF.F` clear is a run-time property of the
    // hardware, not of the build, so it is read from the boot probe rather
    // than a compile-time constant.
    #[cfg(feature = "watchdog-diagnostics")]
    let spsr = el0_spsr(crate::watchdog::fiq_cadence_enabled());
    #[cfg(not(feature = "watchdog-diagnostics"))]
    let spsr = el0_spsr(false);
    // SAFETY: the-sanctioned assembly carve-out (no Rust spelling for
    // `eret` or the EL1 system-register writes). Masking `DAIF` only
    // changes this CPU's exception masks, which the `eret` then replaces
    // from `SPSR_EL1`. Writing `SP_EL0`/`ELR_EL1`/`SPSR_EL1` loads the EL0
    // entry state; `eret` performs the documented EL1→EL0 transition (a
    // context-synchronising event). The caller's safety contract
    // guarantees the mapped entry/stack. `options(noreturn)` matches the
    // divergence.
    unsafe {
        core::arch::asm!(
            "msr DAIFSet, #0xf",
            "msr SP_EL0, {sp}",
            "msr ELR_EL1, {entry}",
            "msr SPSR_EL1, {spsr}",
            "eret",
            sp = in(reg) sp,
            entry = in(reg) entry,
            spsr = in(reg) spsr,
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

    /// `SPSR.M[3:0]`: the exception level and stack selector the `eret`
    /// returns to. `0b0000` is `EL0t`.
    const SPSR_MODE_MASK: u64 = 0b1111;

    #[test]
    fn user_mode_handle_is_object_safe() {
        let port = UserMode::new();
        let _: &dyn EnterUser = &port;
    }

    #[test]
    fn el0_always_returns_to_el0t_preemptible() {
        for fiq_cadence in [false, true] {
            let spsr = el0_spsr(fiq_cadence);
            assert_eq!(spsr & SPSR_MODE_MASK, 0, "EL0t");
            assert_eq!(spsr & SPSR_DAIF_IRQ, 0, "IRQ unmasked: EL0 is preemptible");
        }
    }

    #[test]
    fn a_shippable_or_unprobed_image_keeps_fiq_masked_in_el0() {
        let spsr = el0_spsr(false);
        assert_eq!(spsr & SPSR_DAIF_FIQ, SPSR_DAIF_FIQ);
        assert_eq!(spsr, SPSR_EL0T_PREEMPTIBLE);
    }

    #[test]
    fn a_probed_fiq_cadence_leaves_fiq_deliverable_in_el0() {
        // Without this a core running a CPU-bound user task is never
        // sampled by the cadence that judges it alive.
        assert_eq!(el0_spsr(true) & SPSR_DAIF_FIQ, 0);
    }

    #[test]
    fn the_fiq_cadence_changes_only_the_fiq_mask() {
        assert_eq!(el0_spsr(true) ^ el0_spsr(false), SPSR_DAIF_FIQ);
    }

    #[test]
    fn serror_and_debug_stay_masked_in_el0_either_way() {
        const SPSR_DAIF_SERROR: u64 = 1 << 8;
        const SPSR_DAIF_DEBUG: u64 = 1 << 9;
        for fiq_cadence in [false, true] {
            let spsr = el0_spsr(fiq_cadence);
            assert_eq!(spsr & SPSR_DAIF_SERROR, SPSR_DAIF_SERROR);
            assert_eq!(spsr & SPSR_DAIF_DEBUG, SPSR_DAIF_DEBUG);
        }
    }
}
