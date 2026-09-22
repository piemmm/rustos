//! QEMU integration test: the aarch64 boot stack's poison guard.
//!
//! The boot stack is in use before the MMU is on, so an overrun cannot be
//! caught by unmapping a page below it. The guard is poison instead, and
//! only a real boot can show that the linker reserved it, that the stub's
//! fill survived the `.bss` clear that spans it, and that the port's
//! post-mortem handle names that same reservation. The checks themselves
//! are `tairix_itest_bootguard`'s, shared with the sibling ports; the
//! judgement they exercise is unit-tested on the host.
//!
//! Any failure is reported to QEMU as a distinct, non-zero exit code.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod kernel {
    use core::fmt::Write as _;
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::backtrace::Backtracer;
    use tairix_arch_aarch64::serial::ConsoleWriter;
    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit};
    use tairix_arch_api::CpuStateCapture as _;
    use tairix_itest_bootguard::check;

    extern "C" {
        /// Lowest address of the boot stack's poison guard (linker script).
        static __boot_stack_guard_bottom: u8;
        /// The boot stack's lowest byte — one past the guard's last.
        static __boot_stack_bottom: u8;
    }

    #[panic_handler]
    fn bootguard_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let mut w = ConsoleWriter;
        let bt = Backtracer::new();
        let sp = bt.capture().sp;
        // SAFETY: taking the address of the extern guard symbols is a
        // link-time constant; they are never dereferenced here.
        let low = core::ptr::addr_of!(__boot_stack_guard_bottom) as u64;
        let bottom = core::ptr::addr_of!(__boot_stack_bottom) as u64;
        // SAFETY: the linker script reserves `[low, bottom)` as this CPU's
        // boot-stack guard and nothing else claims it; this kernel is the
        // only thing running, so the checks' write-and-restore is unraced.
        match unsafe { check(bt.boot_stack_guard(), low, bottom, sp) } {
            Ok(()) => {
                let _ = writeln!(w, "[bootguard] aarch64 boot-stack guard verified");
                qemu_exit::exit_success()
            }
            Err(defect) => {
                let _ = writeln!(w, "[bootguard] aarch64 FAILED: {}", defect.describe());
                qemu_exit::exit_failure(defect.code())
            }
        }
    }
}

// Host stub. The crate is *only* meaningful on the bare-metal aarch64
// target; on the host a no-op `main` keeps `cargo build` / `cargo test`
// against the host triple working for IDE indexing and `cargo xtask ci`.
#[cfg(not(itest_aarch64))]
fn main() {}
