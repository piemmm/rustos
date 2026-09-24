//! `plans/OPEN-DEFECTS.md` D146 on x86_64, for the per-CPU tables: a page
//! fault taken after the kernel has loaded its own descriptor tables, with no
//! fault handler installed, is reported and the run ends on the report.
//!
//! `percpu::init` replaces the boot tables, so a table it built that routed
//! nothing would park the CPU silently on the first fault from then on. The
//! kernel loads its per-CPU tables and then reads the first byte past the boot
//! identity window, which nothing maps; the harness passes the run only if it
//! stopped on the record naming the vector, the error code and `CR2`.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod kernel {
    use core::fmt::Write as _;
    use core::panic::PanicInfo;

    use tairix_arch_x86_64::panic::handle_panic_via_serial;
    use tairix_arch_x86_64::{paging, percpu, qemu_exit, serial};

    /// The first byte past the boot identity window.
    const UNMAPPED: u64 = 0x1_0000_0000;
    const _: () = assert!(UNMAPPED == (paging::BOOT_IDENTITY_GIB as u64) << 30);

    /// The one CPU's descriptor tables, which `percpu::init` fills.
    static PER_CPU_STORAGE: percpu::PerCpuStorage<1> = percpu::PerCpuStorage::new();

    #[panic_handler]
    fn fatal_fault_percpu_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_boot_info: u64) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        if PER_CPU_STORAGE.register().is_err() {
            let _ = writeln!(com1, "[fatal_fault_percpu] FAIL: PerCpuStorage::register");
            qemu_exit::exit_failure();
        }
        // SAFETY: the boot CPU, once, with interrupts disabled since the
        // trampoline.
        if unsafe { percpu::init(0) }.is_err() {
            let _ = writeln!(com1, "[fatal_fault_percpu] FAIL: percpu::init(0)");
            qemu_exit::exit_failure();
        }
        let _ = writeln!(com1, "[fatal_fault_percpu] x86_64: reading {UNMAPPED:#x}");
        // SAFETY: nothing maps the address, so the read raises a `#PF` the
        // per-CPU table routes to the fatal path, which parks the CPU.
        let byte = unsafe { core::ptr::read_volatile(UNMAPPED as *const u8) };
        let _ = writeln!(
            com1,
            "[fatal_fault_percpu] FAIL: the read returned {byte:#x} instead of faulting"
        );
        qemu_exit::exit_failure()
    }
}

// Host stub. The crate is only meaningful on the bare-metal target.
#[cfg(not(itest_x86_64))]
fn main() {}
