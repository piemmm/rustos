//! `plans/OPEN-DEFECTS.md` D146 on x86_64: an exception taken on a stack that
//! cannot be used — what an overrun of the boot stack becomes — is reported
//! rather than triple-faulting into a silent exit.
//!
//! The kernel points `rsp` at memory nothing maps and raises a breakpoint.
//! Delivering it faults, delivering that page fault faults again, and the CPU
//! escalates to `#DF`. The port's boot tables give `#DF` a stack of its own,
//! so the double fault reaches the fatal path, and with no handler installed
//! the port writes its own report. The harness passes the run only if it
//! stopped on that report's record naming vector 8.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod kernel {
    use core::fmt::Write as _;
    use core::panic::PanicInfo;

    use tairix_arch_x86_64::panic::handle_panic_via_serial;
    use tairix_arch_x86_64::{paging, serial};

    /// A stack top every push below which lands on memory nothing maps: a
    /// page above the boot identity window. The window's own last bytes are
    /// not usable for this — they back the firmware ROM, which drops writes
    /// rather than faulting on them.
    const STACK_TOP: u64 = ((paging::BOOT_IDENTITY_GIB as u64) << 30) + 0x1000;

    #[panic_handler]
    fn fatal_double_fault_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_boot_info: u64) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        let _ = writeln!(
            com1,
            "[fatal_double_fault] x86_64: trapping with rsp at {STACK_TOP:#x}"
        );
        // SAFETY: the stack pointer is moved onto memory nothing maps and a
        // breakpoint raised; its delivery escalates to `#DF`, which the boot
        // tables take on their own stack to the fatal path, which parks the
        // CPU, so nothing runs after it.
        unsafe {
            core::arch::asm!("mov rsp, {sp}", "int3", sp = in(reg) STACK_TOP, options(noreturn));
        }
    }
}

// Host stub. The crate is only meaningful on the bare-metal target.
#[cfg(not(itest_x86_64))]
fn main() {}
