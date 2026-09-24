//! `plans/OPEN-DEFECTS.md` D146 on x86_64: a page fault in a kernel that
//! links only the port and installs no fault handler is reported, and the
//! run ends on the report.
//!
//! The kernel reads the first byte past the boot identity window, which
//! nothing maps. The port's boot entry installed its exception tables, so
//! the `#PF` reaches the fatal path instead of triple-faulting through the
//! invalid IDTR `boot.s` leaves, and with no handler installed the port
//! writes its own report. The harness passes the run only if it stopped on
//! that report's record naming the vector, the error code and `CR2`, which
//! it ends the run on the moment it lands.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod kernel {
    use core::fmt::Write as _;
    use core::panic::PanicInfo;

    use tairix_arch_x86_64::panic::handle_panic_via_serial;
    use tairix_arch_x86_64::{paging, qemu_exit, serial};

    /// The first byte past the boot identity window. The harness expects
    /// this address in the record, so a change to the window is a change to
    /// the expectation.
    const UNMAPPED: u64 = 0x1_0000_0000;
    const _: () = assert!(UNMAPPED == (paging::BOOT_IDENTITY_GIB as u64) << 30);

    #[panic_handler]
    fn fatal_fault_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_boot_info: u64) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        let _ = writeln!(com1, "[fatal_fault] x86_64: reading {UNMAPPED:#x}");
        // SAFETY: nothing maps the address, so the read raises a `#PF` the
        // boot tables route to the fatal path, which parks the CPU.
        let byte = unsafe { core::ptr::read_volatile(UNMAPPED as *const u8) };
        let _ = writeln!(
            com1,
            "[fatal_fault] FAIL: the read returned {byte:#x} instead of faulting"
        );
        qemu_exit::exit_failure()
    }
}

// Host stub. The crate is only meaningful on the bare-metal target.
#[cfg(not(itest_x86_64))]
fn main() {}
