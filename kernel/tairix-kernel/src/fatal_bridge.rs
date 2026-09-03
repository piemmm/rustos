//! The one bridge from a port's fatal entry points to
//! [`tairix_kernel_core`]'s single fatal-report path.
//!
//! Two things can end a TAIRiX kernel: a Rust `panic!`, and a CPU
//! exception taken in kernel mode that the port's vector has no fix-up
//! for. Both are non-recoverable and both deserve the same post-mortem —
//! the register snapshot, the bounded backtrace, and the audit record
//! `kernel/core`'s [`handle_panic`] / [`fault_dump`] emit.
//!
//! Reaching that path needs four per-port values (the published arch
//! handle, the sink, the register/unwind handle, the console list) and one
//! per-port act (write a line and park, for the window before boot has
//! published an arch handle). Everything else — which sink, which handle,
//! what the null-handle window does, how the context is assembled — is one
//! decision for the whole kernel and lives here, not copied into each
//! port's bridge. A port supplies the values by implementing
//! [`FatalReport`]; a fourth port implements the trait rather than growing
//! a fifth copy of the policy.
//!
//! # Why the bin crate
//!
//! An architecture crate may not depend on `kernel/core` (a port names only
//! the Arch HAL and `lib/*`), and `kernel/core` cannot own a
//! `#[panic_handler]` (a host-test build already has `std`'s). The bin
//! crate is the one layer that links both, so the bridge lives here beside
//! the per-port `panic_ctx` modules that implement the trait.

use core::fmt::Arguments;
use core::panic::PanicInfo;

use tairix_arch_api::backtrace::CpuStateCapture;
use tairix_kernel_core::{
    fault_dump, handle_panic, ConsoleDevice, KernelArch, KernelFault, PanicContext, NO_CONSOLES,
};
use tairix_log::Sink;

/// The per-port values [`report_panic`] and [`report_kernel_fault`] need.
///
/// Implemented once per port, on a private unit type beside that port's
/// published arch pointer.
pub trait FatalReport {
    /// The port's `KernelArch` handle type.
    type Arch: KernelArch + 'static;

    /// The arch handle boot published, or [`None`] in the window before it
    /// did (a fault or panic from inside `boot` itself, or from the
    /// allocator on heap exhaustion).
    fn arch() -> Option<&'static Self::Arch>;

    /// Sink the fatal record is written to.
    ///
    /// This is the port's own unbuffered serial sink, not the audit sink a
    /// boot handed to `kernel_core`: a fatal report must not depend on
    /// anything that could itself be the reason the kernel is dying.
    fn audit_sink() -> &'static (dyn Sink + Sync);

    /// The port's post-mortem register-capture and unwind handle.
    fn backtrace() -> &'static dyn CpuStateCapture;

    /// The installed console list, so the report takes the display surface
    /// back from a graphical session before it writes. A port that wires no
    /// console has none to reclaim.
    #[must_use]
    fn consoles() -> &'static [ConsoleDevice] {
        &NO_CONSOLES
    }

    /// Push whatever the port's console buffers still hold to the wire, so
    /// the lead-up context reaches a serial capture ahead of the report.
    /// A port with an unbuffered console has nothing to do.
    fn flush_console() {}

    /// Write one best-effort line naming `reason` on the port's console and
    /// park the CPU forever.
    ///
    /// Reached only in the null-handle window above, where there is no
    /// `current_cpu` to ask and no structured record to be had. Fails
    /// closed: never a silent reset.
    fn report_before_init(reason: Arguments<'_>) -> !;
}

/// Assemble the port's fatal-report context.
fn context<B: FatalReport>(arch: &B::Arch) -> PanicContext<'_, B::Arch> {
    PanicContext::new(arch, B::audit_sink())
        .with_backtrace(B::backtrace())
        .with_consoles(B::consoles())
}

/// `#[panic_handler]` body for every binary of port `B`.
pub fn report_panic<B: FatalReport>(info: &PanicInfo<'_>) -> ! {
    match B::arch() {
        None => B::report_before_init(format_args!("panic before init: {info}")),
        Some(arch) => {
            B::flush_console();
            handle_panic(info, &context::<B>(arch))
        }
    }
}

/// Fatal kernel-fault handler for port `B` — the `extern "C"` shim its
/// synchronous-exception vector invokes.
///
/// Installed once per boot through the port's `set_fault_handler` slot. The
/// three arguments are the port's syndrome / faulting address / faulting PC
/// triple (`ESR_EL1`/`FAR_EL1`/`ELR_EL1`, the `#PF` error code/address/`RIP`,
/// `scause`/`stval`/`sepc`). Never returns: the kernel has no fix-up for
/// the exception, so resuming would re-trap forever.
pub extern "C" fn report_kernel_fault<B: FatalReport>(syndrome: u64, address: u64, pc: u64) -> ! {
    let fault = KernelFault {
        syndrome,
        address,
        pc,
    };
    match B::arch() {
        None => B::report_before_init(format_args!("kernel fault before init: {fault}")),
        Some(arch) => {
            B::flush_console();
            fault_dump(fault, &context::<B>(arch))
        }
    }
}
