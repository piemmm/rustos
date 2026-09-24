//! The port's own fatal reports, for the boot binaries that link no kernel
//! core and for a fault taken before one installed its handler.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each binary declares
//! its own one-liner that forwards to [`handle_panic_via_serial`]. Both
//! reports are the shared [`tairix_arch_api::fatal`] shape, written to COM1
//! as it stands rather than re-initialised, and both park the CPU: never a
//! silent reset, and never through QEMU's debug-exit port. Their closing
//! record is what ends a QEMU run at once rather than on its inactivity
//! budget.
//!
//! The *production* kernel routes a panic and a kernel fault through
//! `tairix_kernel_core`'s post-mortem (a register snapshot and a bounded
//! backtrace) via the bin-crate bridge; these are the paths below it.

use core::panic::PanicInfo;

use tairix_arch_api::fatal::{KernelFault, Reporter};
use tairix_arch_api::{BootStackGuard, CpuStateCapture as _};

use crate::fault::{exception_syndrome, PAGE_FAULT_VECTOR};
use crate::serial::{Serial, COM1_BASE};

const REPORTER: Reporter = Reporter {
    port: "x86_64",
    unit: "CPU",
};

/// Shared `#[panic_handler]` body for the x86_64 boot binaries: report the
/// panic on COM1 and park the CPU.
pub fn handle_panic_via_serial(info: &PanicInfo<'_>) -> ! {
    let bt = crate::backtrace::Backtracer;
    let guard = guard_verdict(bt.capture().sp);
    REPORTER.panic(
        &mut Serial::at(COM1_BASE),
        current_cpu(),
        info,
        info.location(),
        guard,
    );
    crate::reset::park_cpu()
}

/// Report a fatal exception no fault handler claimed and park the CPU.
///
/// `address` is `CR2` for a page fault and `0` for every other vector: no
/// other exception supplies a faulting address, and `CR2` would name
/// whichever page fault happened last.
pub(crate) fn report_unclaimed_fault(
    vector: u8,
    error_code: u64,
    from_user: bool,
    address: u64,
    rip: u64,
) -> ! {
    // A vector delivered on an IST runs on a stack of its own, which says
    // nothing about where the interrupted code's stack pointer was, so the
    // guard is judged on its canary alone.
    let sp = if crate::percpu::ist_for_vector(vector) == 0 {
        crate::backtrace::Backtracer.capture().sp
    } else {
        0
    };
    let fault = KernelFault {
        syndrome: exception_syndrome(vector, error_code, from_user),
        address,
        pc: rip,
    };
    let ring = if from_user { 3 } else { 0 };
    let (cpu, guard) = (current_cpu(), guard_verdict(sp));
    let mut com1 = Serial::at(COM1_BASE);
    if vector == PAGE_FAULT_VECTOR {
        REPORTER.fault(
            &mut com1,
            cpu,
            fault,
            format_args!(
                "vector {vector}, error code {error_code:#x}, ring {ring}, CR2 {address:#x}, RIP {rip:#x}"
            ),
            guard,
        );
    } else {
        REPORTER.fault(
            &mut com1,
            cpu,
            fault,
            format_args!("vector {vector}, error code {error_code:#x}, ring {ring}, RIP {rip:#x}"),
            guard,
        );
    }
    crate::reset::park_cpu()
}

/// The boot-stack guard's verdict for a CPU whose stack pointer is `sp`.
fn guard_verdict(sp: u64) -> Option<BootStackGuard> {
    crate::backtrace::Backtracer
        .boot_stack_guard()
        .map(|guard| guard.assess(sp))
}

/// The running CPU's dense id where the kernel has mapped it, else its
/// initial APIC id — read through `CPUID` rather than the local APIC, which a
/// report taken before the APIC is mapped would fault on.
fn current_cpu() -> u32 {
    let apic = u8::try_from(core::arch::x86_64::__cpuid(1).ebx >> 24).unwrap_or(u8::MAX);
    match crate::preempt::cpu_id_for_lapic(apic) {
        u32::MAX => u32::from(apic),
        dense => dense,
    }
}
