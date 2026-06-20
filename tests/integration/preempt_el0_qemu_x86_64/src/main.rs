//! `plans/PI.md` D2b-2b-A P-1c QEMU integration test (x86_64 port): prove the
//! production LAPIC-timer IRQ **involuntarily preempts** a runaway ring-3 task
//! on x86_64 — the cross-port sibling of the aarch64 / riscv64
//! `preempt_el0_qemu_*` verticals.
//!
//! Like the x86_64 `spawn_el0_timeshare` sibling, the ring-3 transition needs
//! the GDT ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry
//! installed, so this test boots the production `rustos-kernel` pipeline (which
//! also programs the periodic LAPIC timer). On `AuditEvent::BootCompleted` it
//! enables `IA32_EFER.NXE`, builds **one** hardware-isolated ring-3 address
//! space from the `el0_spinner` `rxe` fixture through the production
//! capability-checked, audited `rustos_kernel_core::spawn_image` caller, and
//! admits it as a resumable user kthread via
//! `rustos_kernel_core::spawn_user_kthread`. Its `pre_resume` hook reloads CR3
//! (`rustos_arch_x86_64::paging::activate_user_root`) and repoints **both** the
//! per-CPU `syscall` entry stack (`syscall_entry::set_kernel_rsp0`) and the
//! `TSS.RSP0` trap stack (`percpu::install_tss_rsp0`) at the task's own kernel
//! stack — the latter because an involuntary timer IRQ taken from ring 3 is
//! delivered through the IDT interrupt gate, for which the CPU reads
//! `TSS.RSP0`.
//!
//! It then arms the **production** ring-3-preemption path verbatim
//! (`AGENTS.md` §2.2 — the same `rustos_arch_x86_64::preempt` surface the bin
//! crate's `install_irq_dispatch` uses): it installs a ring-3-preemption
//! callback that suspends the running task back to the scheduler via
//! `reschedule_current(_, Yield)`. Ring 3 already runs preemptible
//! (`userentry`'s `IF`-set `RFLAGS`), so a timer tick taken while the spinner
//! is in ring 3 lands on the timer ISR and drives the preempt point.
//!
//! Driving the `step` loop dispatches the spinner into ring 3, where it
//! busy-loops issuing **no** syscall. Because the loop never traps, the *only*
//! thing that can return control to the dispatcher before the spinner's final
//! `exit` is an involuntary timer preemption. The test PASSes once (a) the
//! preempt callback fired at least once **and** (b) the task — correctly
//! resumed mid-loop after each preemption — still completed its spin and
//! exited. A preemption that never fires (the `step` never returns) or a
//! botched resume (the task never exits) stalls the drain, so the run fails
//! loudly — by a failure code or by the harness `Outcome::Timeout`
//! (`AGENTS.md` §7).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-preempt-el0-qemu-x86-64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    loop {
        // SAFETY: `cli; hlt` is a well-defined parked-CPU sequence on x86_64
        // (`AGENTS.md` §2.9). Looping defends against spurious wake-ups.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
