//! `plans/OPEN-DEFECTS.md` D146 on aarch64: a CPU fault in a kernel that links
//! only the port and installs no fault handler is reported, and the run ends
//! on the report.
//!
//! The kernel branches to the `f64` bit pattern of `1.0` — where the defect's
//! corrupted vtable slot sent the CPU. The port's boot entry armed the EL1
//! vectors, so the Instruction Abort reaches the fatal path, and with no
//! handler installed the port writes its own report. The harness passes the
//! run only if it stopped on that report's record naming exactly this
//! syndrome, address and PC, which it ends the run on the moment it lands.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod kernel {
    use core::fmt::Write as _;
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::handle_panic_via_serial;
    use tairix_arch_aarch64::serial::ConsoleWriter;

    /// Where the defect's corrupted vtable slot sent the CPU: nothing is
    /// mapped there, so fetching from it raises an Instruction Abort.
    const WILD_TARGET: u64 = 0x3ff0_0000_0000_0000;

    #[panic_handler]
    fn fatal_fault_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let _ = writeln!(
            ConsoleWriter,
            "[fatal_fault] aarch64: branching to {WILD_TARGET:#x}"
        );
        // SAFETY: the branch leaves the image for an address nothing maps;
        // the abort it raises is taken by the armed vectors to the fatal
        // path, which parks the CPU, so nothing runs after it.
        unsafe {
            core::arch::asm!("br {target}", target = in(reg) WILD_TARGET, options(noreturn));
        }
    }
}

// Host stub. The crate is only meaningful on the bare-metal target.
#[cfg(not(itest_aarch64))]
fn main() {}
