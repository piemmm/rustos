//! `plans/PI.md` stage X1 QEMU integration test (x86_64 port): prove a single
//! ring-3 task can be admitted as a **resumable user kthread** and cooperatively
//! park/resume under the live scheduler on x86_64 — the cross-port sibling of
//! the aarch64 `SP2c` `spawn_el0_timeshare`, scoped to one task (the two-task
//! `gs:8` durable-save hazard is stage X2).
//!
//! Like the `mem_map` x86_64 sibling, the ring-3 transition needs the GDT
//! ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry installed, so
//! this test boots the production `rustos-kernel` pipeline. On
//! `AuditEvent::BootCompleted` it enables `IA32_EFER.NXE`, builds one
//! hardware-isolated user address space from the `rxe` fixture program through
//! the production capability-checked, audited `rustos_kernel_core::spawn_image`
//! caller, then admits it as a resumable user kthread via
//! `rustos_kernel_core::spawn_user_kthread`. That task's `pre_resume` hook
//! reloads CR3 (`rustos_arch_x86_64::paging::activate_user_root`) and repoints
//! the per-CPU syscall entry stack at the task's **own** kernel stack
//! (`rustos_arch_x86_64::syscall_entry::set_kernel_rsp0`, the X1 primitive). The
//! test then drives the cooperative `Scheduler::step` loop; the dispatch
//! callback maps the task's `yield`/`exit` to `reschedule_current`, so the task
//! parks back to the dispatcher on each yield and is reaped on exit.
//!
//! The program yields a build-time-pinned number of times then exits 0. The
//! test PASSes once the task yielded its full count and exited. Any shortfall
//! (an unexpected syscall, a wrong drain count, or a deadlock) flips
//! `qemu_exit::exit_failure` or times out, so the run fails loudly
//! (`AGENTS.md` §7).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-spawn-el0-resume-qemu-x86-64: the `test-hooks` Cargo feature \
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
