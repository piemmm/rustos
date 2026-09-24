//! The port's own fatal reports, for the boot binaries that link no kernel
//! core and for a fault taken before one installed its handler.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each binary declares
//! its own one-liner that forwards to [`handle_panic_via_serial`]. Both
//! reports are the shared [`tairix_arch_api::fatal`] shape, written on the
//! console directly — never through the queued sink, whose lock the dying
//! code may hold — and both park the CPU: never a silent reset. Their
//! closing record is what ends a QEMU run at once rather than on its
//! inactivity budget.
//!
//! The *production* kernel routes a panic and a kernel fault through
//! `tairix_kernel_core`'s post-mortem (a register snapshot and a bounded
//! backtrace) via the bin-crate bridge; these are the paths below it.

use core::panic::PanicInfo;

use tairix_arch_api::fatal::{KernelFault, Reporter};
use tairix_arch_api::{BootStackGuard, CpuStateCapture as _};

use crate::kernel_arch::halt_current_cpu;
use crate::serial::ConsoleWriter;

const REPORTER: Reporter = Reporter {
    port: "aarch64",
    unit: "CPU",
};

/// Shared `#[panic_handler]` body for the aarch64 boot binaries: report the
/// panic on the console and park the CPU.
pub fn handle_panic_via_serial(info: &PanicInfo<'_>) -> ! {
    let (cpu, guard) = prologue();
    REPORTER.panic(&mut ConsoleWriter, cpu, info, info.location(), guard);
    halt_current_cpu()
}

/// Report a fatal exception no fault handler claimed and park the CPU.
pub(crate) fn report_unclaimed_fault(esr: u64, far: u64, elr: u64) -> ! {
    let (cpu, guard) = prologue();
    let class = crate::fault::exception_class(esr);
    REPORTER.fault(
        &mut ConsoleWriter,
        cpu,
        KernelFault {
            syndrome: esr,
            address: far,
            pc: elr,
        },
        format_args!("ESR_EL1 {esr:#x} (EC {class:#04x}), FAR_EL1 {far:#x}, ELR_EL1 {elr:#x}"),
        guard,
    );
    halt_current_cpu()
}

/// Push the buffered lead-up context to the wire ahead of the report, and
/// read the dense CPU id and the boot-stack guard's verdict it names.
fn prologue() -> (u32, Option<BootStackGuard>) {
    crate::serial::flush_serial_blocking();
    let cpu = crate::smp::current_cpu_index();
    let bt = crate::backtrace::Backtracer::new();
    let sp = bt.capture().sp;
    (cpu, bt.boot_stack_guard().map(|guard| guard.assess(sp)))
}
