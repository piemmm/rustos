//! aarch64 half of the fatal-report bridge: the port's four values plus
//! its two entry points ([`handle_panic_via_kernel_core`] for a `panic!`,
//! [`install_kernel_fault_handler`] for a fatal EL1 exception).
//!
//! # Why this lives in the bin's library half
//!
//! The architecture crate (`tairix_arch_aarch64`) may not depend on
//! `kernel/core` (the layering contract: an arch port names only the Arch
//! HAL and `lib/*`), so the bridge that turns a `#[panic_handler]` or a
//! trap-vector callback into a `kernel_core` report lives here, in the bin
//! crate that already links both. The shared policy is
//! [`crate::fatal_bridge`]; this module is only the aarch64 values.
//!
//! # How [`Aarch64BinArch`] is reached
//!
//! The report needs a `&Aarch64BinArch` (for `current_cpu` / `halt`). The
//! arch handle is built partway through boot, so it cannot live in a plain
//! `static`; `boot` publishes `Arc::as_ptr(&arc)` into [`PANIC_ARCH_PTR`]
//! before any code that could panic runs. Before that publish the report
//! falls back to one best-effort serial line and parks (fail closed, never
//! a silent reset).

use core::fmt::{Arguments, Write as _};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, Ordering};

use tairix_arch_aarch64::backtrace::Backtracer;
use tairix_arch_aarch64::fault::{set_fault_handler, SetFaultHandlerError};
use tairix_arch_aarch64::serial::{flush_serial_blocking, ConsoleWriter};
use tairix_arch_aarch64::{halt_current_cpu, SERIAL_SINK};
use tairix_arch_api::backtrace::CpuStateCapture;
use tairix_kernel_core::ConsoleDevice;
use tairix_log::Sink;

use crate::aarch64::arch_wrapper::{console_layout, Aarch64BinArch};
use crate::fatal_bridge::{report_kernel_fault, report_panic, FatalReport};

/// The aarch64 post-mortem CPU-state handle the bridge attaches.
static BACKTRACER: Backtracer = Backtracer::new();

/// `AtomicPtr<Aarch64BinArch>` published by `boot` once the architecture
/// handle is constructed.
///
/// The pointer is `Arc::as_ptr(&arc)`, stable for the lifetime of the
/// `Arc`; the `Arc` itself lives for the running kernel's lifetime (held
/// by `BootInfo`'s `arch` field and re-cloned into `kernel_core`'s
/// `KernelState`).
pub static PANIC_ARCH_PTR: AtomicPtr<Aarch64BinArch> = AtomicPtr::new(core::ptr::null_mut());

/// Publish `arch` into [`PANIC_ARCH_PTR`].
///
/// Called by `boot` immediately after constructing the
/// `Arc<Aarch64BinArch>` and before handing it to `kernel_core`.
///
/// # Safety
///
/// `arch_ptr` must remain valid for the lifetime of the kernel image. The
/// standard caller is `Arc::as_ptr(&arc)`, where `arc` is kept alive by
/// `BootInfo`'s `arch` field.
pub unsafe fn publish_arch(arch_ptr: *const Aarch64BinArch) {
    // `Release` so a panic on another CPU observes the initialised
    // pointer; the matching `Acquire` load is in `Aarch64Fatal::arch`.
    PANIC_ARCH_PTR.store(arch_ptr.cast_mut(), Ordering::Release);
}

/// This port's [`FatalReport`] values.
struct Aarch64Fatal;

impl FatalReport for Aarch64Fatal {
    type Arch = Aarch64BinArch;

    fn arch() -> Option<&'static Aarch64BinArch> {
        let raw = PANIC_ARCH_PTR.load(Ordering::Acquire);
        if raw.is_null() {
            return None;
        }
        // SAFETY: `publish_arch` only ever stores `Arc::as_ptr(&arc)`,
        // where `arc: Arc<Aarch64BinArch>` is held by `kernel_core`'s
        // `BootInfo` for the running kernel's lifetime; the pointee
        // therefore outlives every report that can observe a non-null
        // pointer.
        Some(unsafe { &*raw })
    }

    fn audit_sink() -> &'static (dyn Sink + Sync) {
        &SERIAL_SINK
    }

    fn backtrace() -> &'static dyn CpuStateCapture {
        &BACKTRACER
    }

    fn consoles() -> &'static [ConsoleDevice] {
        let (consoles, _) = console_layout();
        consoles
    }

    fn flush_console() {
        flush_serial_blocking();
    }

    fn report_before_init(reason: Arguments<'_>) -> ! {
        flush_serial_blocking();
        let mut w = ConsoleWriter;
        let _ = writeln!(w, "[tairix-kernel] aarch64 {reason}");
        halt_current_cpu()
    }
}

/// Shared `#[panic_handler]` body for the aarch64 kernel binaries.
pub fn handle_panic_via_kernel_core(info: &PanicInfo<'_>) -> ! {
    report_panic::<Aarch64Fatal>(info)
}

/// Install the production fatal-fault handler on this port.
///
/// Called at `boot` entry, before the EL1 vectors can deliver anything, so
/// no kernel-mode exception is ever taken with the slot empty — an empty
/// slot parks the CPU with interrupts masked and prints nothing, which is
/// how a kernel fault used to disappear.
///
/// # Errors
///
/// [`SetFaultHandlerError::AlreadyInstalled`] when something published a
/// handler first. The slot is set-once by design, so the existing handler
/// keeps the machine's fatal policy; only a QEMU vertical that deliberately
/// observes its own faults installs ahead of boot.
pub fn install_kernel_fault_handler() -> Result<(), SetFaultHandlerError> {
    set_fault_handler(report_kernel_fault::<Aarch64Fatal>)
}
