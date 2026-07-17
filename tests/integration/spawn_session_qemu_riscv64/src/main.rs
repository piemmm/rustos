//! `plans/PI.md` stage RV-X3 QEMU integration test (riscv64 sibling of
//! `spawn_session_qemu_aarch64` / `_x86_64`): prove the riscv64 runtime
//! `spawn` **concurrent producer** on the `virt` board.
//!
//! On boot the test reads the generic-timer rate from the firmware device
//! tree, installs the trap vector + a syscall-dispatch callback, and builds the
//! **parent** a hardware-isolated Sv39 U-mode address space from the `rxe`
//! fixture program through the production capability-checked, audited
//! `tairix_kernel_core::spawn_image` caller, admitting it as a resumable user
//! kthread via `tairix_kernel_core::spawn_user_kthread`. The cooperative
//! `Scheduler::step` loop drives it. The parent issues a real
//! `CAP_PROC_SPAWN`-gated `spawn` `ecall`; the dispatch callback routes it to a
//! riscv64 `ProcessSpawn` producer that builds the **child** (session) a fresh,
//! hardware-isolated Sv39 space **through the identity window without switching
//! the running parent's `satp`** and admits it **Ready** concurrently (the
//! cross-port equal of the `aarch64`/`x86_64` producers). The callback maps each
//! `yield`/`exit` `ecall` to `tairix_kernel_core::reschedule_current`, so the
//! parent and child timeshare the hart on their own kernel stacks (the RV1
//! park-safe path).
//!
//! PASS once the producer built the child and both the parent and the child
//! ran to `exit`. A failed spawn, an unexpected syscall, a wrong drain count,
//! or a stall trips a distinct `SiFive` Test failure finisher or times out, so
//! the run fails loudly — by a failure code or by the harness
//! `Outcome::Timeout`.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-spawn-session-qemu-riscv64: the `test-hooks` Cargo feature is a \
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
