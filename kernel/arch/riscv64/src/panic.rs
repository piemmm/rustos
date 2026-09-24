//! The port's own fatal reports, for the boot binaries that link no kernel
//! core and for a fault taken before one installed its handler.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each binary declares
//! its own one-liner that forwards to [`handle_panic_via_serial`]. Both
//! reports are the shared [`tairix_arch_api::fatal`] shape, written through
//! the synchronous SBI console, and both park the hart: never a silent
//! reset. Their closing record is what ends a QEMU run at once rather than
//! on its inactivity budget.
//!
//! The *production* kernel routes a panic and a kernel fault through
//! `tairix_kernel_core`'s post-mortem (a register snapshot and a bounded
//! backtrace) via the bin-crate bridge; these are the paths below it.

use core::panic::PanicInfo;

use tairix_arch_api::fatal::{KernelFault, Reporter};
use tairix_arch_api::{BootStackGuard, CpuStateCapture as _};

use crate::kernel_arch::halt_current_hart;
use crate::serial::SbiWriter;

const REPORTER: Reporter = Reporter {
    port: "riscv64",
    unit: "hart",
};

/// Shared `#[panic_handler]` body for the riscv64 boot binaries: report the
/// panic on the SBI console and park the hart.
pub fn handle_panic_via_serial(info: &PanicInfo<'_>) -> ! {
    let (hart, guard) = prologue();
    REPORTER.panic(&mut SbiWriter, hart, info, info.location(), guard);
    halt_current_hart()
}

/// Report a fatal trap no fault handler claimed and park the hart.
pub(crate) fn report_unclaimed_fault(scause: u64, stval: u64, sepc: u64) -> ! {
    let (hart, guard) = prologue();
    REPORTER.fault(
        &mut SbiWriter,
        hart,
        KernelFault {
            syndrome: scause,
            address: stval,
            pc: sepc,
        },
        format_args!("scause {scause:#x}, stval {stval:#x}, sepc {sepc:#x}"),
        guard,
    );
    halt_current_hart()
}

/// The running hart's id and the boot-stack guard's verdict the report names.
fn prologue() -> (u32, Option<BootStackGuard>) {
    let bt = crate::backtrace::Backtracer::new();
    let sp = bt.capture().sp;
    (
        crate::smp::current_hartid(),
        bt.boot_stack_guard().map(|guard| guard.assess(sp)),
    )
}
