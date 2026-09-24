//! `plans/OPEN-DEFECTS.md` D146 on riscv64: a trap in a kernel that links only
//! the port and installs no fault handler is reported, and the run ends on
//! the report.
//!
//! The kernel executes `ebreak`, a trap OpenSBI hands to S-mode. The port's
//! boot entry armed `stvec`, so the breakpoint reaches the fatal path, and
//! with no handler installed the port writes its own report. The harness
//! passes the run only if it stopped on that report's record naming the
//! breakpoint cause, which it ends the run on the moment it lands.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_riscv64)]
mod kernel {
    use core::fmt::Write as _;
    use core::panic::PanicInfo;

    use tairix_arch_riscv64::handle_panic_via_serial;
    use tairix_arch_riscv64::serial::SbiWriter;

    #[panic_handler]
    fn fatal_fault_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
        let _ = writeln!(SbiWriter, "[fatal_fault] riscv64: executing ebreak");
        // SAFETY: `ebreak` raises a breakpoint the armed trap vector takes to
        // the fatal path, which parks the hart, so nothing runs after it.
        unsafe {
            core::arch::asm!("ebreak", options(noreturn));
        }
    }
}

// Host stub. The crate is only meaningful on the bare-metal target.
#[cfg(not(itest_riscv64))]
fn main() {}
