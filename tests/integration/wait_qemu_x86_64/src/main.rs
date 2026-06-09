//! `plans/PI.md` stage X4 QEMU integration test (x86_64 port): prove a parent
//! ring-3 process can **block on, reap, and read back the exit code** of its
//! own child under the live scheduler on x86_64 — the cross-port sibling of the
//! aarch64 `wait_qemu_aarch64` (`plans/SPAWN.md` `SP6b` / `plans/PI.md` X4).
//!
//! It is the exerciser for the **resume-after-cooperative-park** return-state
//! path on the x86_64 trap: the parent parks *inside* the `wait` handler (the
//! `KernelProcessWait` producer calls `reschedule_current` while a child is
//! still running), the child runs and exits, and only then is the parent
//! re-dispatched — it must resume on its **own** kernel-stack frame, restore
//! its saved user `%rsp`, copy the reaped exit code out to its `status`
//! pointer, and `sysret` back to ring 3 to verify it and exit 0. This is the
//! `wait`-then-reap-then-resume analogue of the two X2 cooperative-park fixes
//! (the durable per-task user-`%rsp` save and the `swapgs` balance).
//!
//! Like the `mem_map` / timeshare x86_64 siblings, the ring-3 transition needs
//! the GDT ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry
//! installed, so this test boots the production `rustos-kernel` pipeline. On
//! `AuditEvent::BootCompleted` it enables `IA32_EFER.NXE`, builds a **child**
//! and a **parent** hardware-isolated ring-3 address space (two PML4s, one
//! shared frame pool) from the `rxe` fixture program through the production
//! capability-checked, audited `rustos_kernel_core::spawn_image` caller,
//! installs a `rustos_kernel_core::KernelProcessWait` producer, records the
//! parent/child link, and admits each as a resumable user kthread via
//! `rustos_kernel_core::spawn_user_kthread`. Each task's `pre_resume` hook
//! reloads CR3 (`rustos_arch_x86_64::paging::activate_user_root`) and repoints
//! the per-CPU syscall entry stack at the task's **own** kernel stack
//! (`rustos_arch_x86_64::syscall_entry::set_kernel_rsp0`). The test then drives
//! the cooperative `Scheduler::step` loop; the dispatch callback routes the
//! child's `exit` and the parent's `wait`/`exit` through the producer +
//! `reschedule_current`.
//!
//! The child exits with a build-time-pinned code; the parent waits for it,
//! reaps it, reads the code back, and exits 0. The test PASSes once the parent
//! reaped the child, read back the agreed code, and exited 0. Any shortfall
//! (an unexpected syscall, a wrong code, a missing reap, or a deadlock) flips
//! `qemu_exit::exit_failure` or times out, so the run fails loudly
//! (`AGENTS.md` §7).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-wait-qemu-x86-64: the `test-hooks` Cargo feature \
     is a debug-only test affordance and must not be enabled in release \
     builds. See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
extern crate alloc;

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
