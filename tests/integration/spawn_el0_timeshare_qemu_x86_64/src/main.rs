//! `plans/PI.md` stage X2 QEMU integration test (x86_64 port): prove **two**
//! hardware-isolated ring-3 tasks timeshare one CPU as resumable user kthreads
//! under the live scheduler on x86_64 — the cross-port sibling of the aarch64
//! `SP2c` `spawn_el0_timeshare`, and the exerciser for the two X2 structural
//! fixes a concurrent mid-handler park needs:
//!
//! 1. The **durable user-`%rsp` save** now lives on each task's own
//!    kernel-stack frame (the entry stub `pushq %gs:8`s the user `%rsp` onto the
//!    frame, restoring it with a single `popq %rsp`), so a task parked
//!    mid-handler does not have its saved user stack pointer clobbered by a
//!    *different* task's syscall through the shared per-CPU `gs:8` slot.
//! 2. The **cooperative-park `swapgs` balance**
//!    (`tairix_arch_api::ContextSwitch::enter`/`leave_cooperative_park`, a no-op
//!    on aarch64/riscv64) brackets the suspend in `kernel/core`'s kthread
//!    runtime, so a parked task's entry `swapgs` is balanced back to the
//!    between-handler GS convention before the dispatcher enters another task —
//!    without it the next ring-3 entry observes an unbalanced GS-swap, reads a
//!    null kernel stack, and `#DF`s.
//!
//! Like the `mem_map` x86_64 sibling, the ring-3 transition needs the GDT
//! ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry installed, so
//! this test boots the production `tairix-kernel` pipeline. On
//! `AuditEvent::BootCompleted` it enables `IA32_EFER.NXE`, builds **two**
//! hardware-isolated user address spaces (two PML4s, one shared frame pool)
//! from the `rxe` fixture program through the production capability-checked,
//! audited `tairix_kernel_core::spawn_image` caller, then admits each as a
//! resumable user kthread via `tairix_kernel_core::spawn_user_kthread`. Each
//! task's `pre_resume` hook reloads CR3
//! (`tairix_arch_x86_64::paging::activate_user_root`) and repoints the per-CPU
//! syscall entry stack at the task's **own** kernel stack
//! (`tairix_arch_x86_64::syscall_entry::set_kernel_rsp0`). The test then drives
//! the cooperative `Scheduler::step` loop; the dispatch callback maps each
//! task's `yield`/`exit` to `reschedule_current`, so the two tasks ping-pong
//! through real ring-3↔kernel context switches.
//!
//! Each task yields a build-time-pinned number of times then exits 0. The test
//! PASSes once both tasks yielded their full count and exited. Any shortfall
//! (an unexpected syscall, a wrong drain count, or a deadlock) flips
//! `qemu_exit::exit_failure` or times out, so the run fails loudly.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-spawn-el0-timeshare-qemu-x86-64: the `test-hooks` Cargo feature \
     is a debug-only test affordance and must not be enabled in release \
     builds. See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    loop {
        // SAFETY: `cli; hlt` is a well-defined parked-CPU sequence on x86_64. Looping defends against spurious wake-ups.
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
