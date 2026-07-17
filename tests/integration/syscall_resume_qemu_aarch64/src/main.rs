//! Deterministic aarch64 syscall-continuation regression vertical.
//!
//! A real EL0 parent completes an ordinary syscall with a pending reschedule,
//! yields to a competing child, and resumes only after that child parks. The
//! parent must receive the original syscall result and exit successfully.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!("rustos-test-syscall-resume-qemu-aarch64: test hooks are debug-only");

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
mod kernel;

#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    loop {
        // SAFETY: `wfe` is a defined parked-CPU hint on aarch64.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) };
    }
}

#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: `wfe` is a defined parked-CPU hint on aarch64.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) };
    }
}

#[cfg(not(itest_aarch64))]
fn main() {}
