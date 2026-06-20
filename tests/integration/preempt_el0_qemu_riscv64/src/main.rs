//! `plans/PI.md` D2b-2b-A P-1b QEMU integration test (riscv64 sibling of the
//! aarch64 `preempt_el0_qemu_aarch64` vertical): prove the production
//! supervisor-timer interrupt **involuntarily preempts** a runaway U-mode task
//! on the riscv64 `virt` board.
//!
//! On boot the test reads the `timebase-frequency` from the firmware device
//! tree (the `a1` DTB pointer OpenSBI hands the boot hart), installs the S-mode
//! trap vector via `trap::install_trap_vector` — deliberately NOT `init_traps`,
//! so `sstatus.SIE` stays clear and the kernel itself is never preempted — and
//! installs a syscall-dispatch callback. It builds one hardware-isolated Sv39
//! U-mode address space from the `el0_spinner` `rxe` fixture through the
//! production capability-checked, audited `rustos_kernel_core::spawn_image`
//! caller and admits it as a resumable user kthread (`spawn_user_kthread`).
//!
//! It then arms the **production** preemption path verbatim (`AGENTS.md` §2.2 —
//! the same `rustos_arch_riscv64::preempt` surface the bin crate's
//! `arm_preemption` uses): registers a per-hart `PreemptStorage`, installs a
//! U-mode-preemption callback that suspends the running task back to the
//! scheduler via `reschedule_current(_, Yield)`, and starts the periodic SBI
//! timer (`init_local_preempt` sets `sie.STIE`). A supervisor-timer interrupt
//! is taken while the spinner runs in U-mode because the hart runs below S-mode
//! (the privilege rule U < S), regardless of `sstatus.SIE`; the trap handler
//! gates the preempt point on the saved `SPP` so only a U-mode tick preempts.
//!
//! Driving the `step` loop dispatches the spinner into U-mode, where it
//! busy-loops issuing **no** syscall. Because the loop never traps, the *only*
//! thing that can return control to the dispatcher before the spinner's final
//! `exit` is an involuntary timer preemption. The test PASSes once (a) the
//! preempt callback fired at least once **and** (b) the task — correctly
//! resumed mid-loop after each preemption — still completed its spin and
//! exited. A preemption that never fires (the `step` never returns) or a
//! botched resume (the task never exits) stalls the drain, so the run fails
//! loudly — by a failure code or by the harness `Outcome::Timeout`
//! (`AGENTS.md` §7).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-preempt-el0-qemu-riscv64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_riscv64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_riscv64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_riscv64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
    loop {
        // SAFETY: `wfi` is a well-defined parked-hart hint on riscv64.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_riscv64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
