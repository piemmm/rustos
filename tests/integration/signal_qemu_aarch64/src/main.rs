//! SPAWN stage `SP7b` QEMU integration test: prove a parent process can deliver
//! a control signal (`Signal::Terminate`) to its own child under the live
//! scheduler on the aarch64 `virt` board.
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the embedded
//! `virt` device tree (`plans/PI.md` P3/P4), brings up the EL1 vectors + GICv2,
//! and installs a syscall-dispatch callback. It builds **two** hardware-isolated
//! EL0 address spaces — a `child` and a `parent`, each its own stage-1 page-table
//! hierarchy — from the two `rxe` fixture builds through the production
//! capability-checked, audited `rustos_kernel_core::spawn_image` caller. It
//! admits the child first, threads the child's scheduler-assigned PID into the
//! parent's startup arguments, records the parent/child link with a
//! `rustos_kernel_core::KernelProcessWait` producer, installs a
//! `rustos_kernel_core::KernelProcessSignal` producer over that same bookkeeping
//! and the live scheduler, and admits the parent as a resumable user kthread
//! (`spawn_user_kthread`, `plans/SPAWN.md` SP2).
//!
//! Driving the cooperative `step` loop interleaves the two runnable tasks. The
//! child yields forever (it never exits on its own). The parent's `signal` `svc`
//! is routed through the signal producer, which authorises the target against
//! the parent's own children and terminates it on the scheduler, recording the
//! signal's `128 + n` status; the parent's `wait` `svc` reaps the now-terminated
//! child, and the kernel copies the recorded status out to the parent's
//! `status` pointer. The parent verifies the status and exits 0. The test
//! PASSes once the parent terminated the child, read back the signalled status,
//! and exited 0 — proving the `signal` delivery path end to end (the `SP7b`
//! "done when"). A regression (a wrong status, a missing reap, a never-draining
//! loop) trips a distinct failure finisher or the harness `Outcome::Timeout`.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-signal-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    loop {
        // SAFETY: `wfe` is a well-defined parked-CPU hint on aarch64.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
