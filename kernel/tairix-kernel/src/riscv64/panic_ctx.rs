//! Panic-handler bridge between the riscv64 kernel binary and
//! [`tairix_kernel_core::handle_panic`].
//!
//! # Why this lives in the bin's library half
//!
//! The architecture crate (`tairix_arch_riscv64`) may not depend on
//! `kernel/core` (the layering contract: an arch port names only the Arch
//! HAL and `lib/*`), so the bridge that turns a `#[panic_handler]` into a
//! `kernel_core::handle_panic` call lives here, in the bin crate that
//! already links both — the exact sibling of [`crate::x86_64::panic_ctx`]
//! and [`crate::aarch64::panic_ctx`]. The previous per-arch SBI-banner
//! body in the arch crate is gone (one panic path, not three).
//!
//! # How [`RiscvBinArch`] is reached
//!
//! `handle_panic` needs a `PanicContext` carrying a `&RiscvBinArch` (for
//! `current_cpu` / `halt`) and the audit sink. The arch handle is built
//! partway through boot, so `boot` publishes `Arc::as_ptr(&arc)` into
//! [`PANIC_ARCH_PTR`] before any code that could panic runs. The handler
//! loads the pointer, and:
//!
//! * if non-null, forwards through `kernel_core::handle_panic` with the
//!   port's [`Backtracer`] attached, so the dump carries registers and a
//!   bounded backtrace;
//! * otherwise (a pre-init panic) it emits one best-effort SBI-console
//!   record and parks the hart forever (fail closed, never a silent reset).

use core::fmt::Write as _;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, Ordering};

use tairix_arch_riscv64::backtrace::Backtracer;
use tairix_arch_riscv64::serial::SbiWriter;
use tairix_arch_riscv64::{halt_current_hart, SERIAL_SINK};
use tairix_kernel_core::{handle_panic, PanicContext};

use crate::riscv64::boot::RiscvBinArch;

/// The riscv64 post-mortem CPU-state handle the bridge attaches.
static BACKTRACER: Backtracer = Backtracer::new();

/// `AtomicPtr<RiscvBinArch>` published by `boot` once the architecture
/// handle is constructed.
///
/// The pointer is `Arc::as_ptr(&arc)`, stable for the lifetime of the
/// `Arc`; the `Arc` itself lives for the running kernel's lifetime.
pub static PANIC_ARCH_PTR: AtomicPtr<RiscvBinArch> = AtomicPtr::new(core::ptr::null_mut());

/// Publish `arch` into [`PANIC_ARCH_PTR`].
///
/// Called by `boot` immediately after constructing the
/// `Arc<RiscvBinArch>` and before handing it to `kernel_core`.
///
/// # Safety
///
/// `arch_ptr` must remain valid for the lifetime of the kernel image. The
/// standard caller is `Arc::as_ptr(&arc)`, where `arc` is kept alive by
/// `BootInfo`'s `arch` field.
pub unsafe fn publish_arch(arch_ptr: *const RiscvBinArch) {
    // `Release` so a panic observes the initialised pointer; the matching
    // `Acquire` load is in [`handle_panic_via_kernel_core`].
    PANIC_ARCH_PTR.store(arch_ptr.cast_mut(), Ordering::Release);
}

/// Shared `#[panic_handler]` body for the riscv64 kernel binaries.
///
/// Always returns `!`. Forwards to `tairix_kernel_core::handle_panic`
/// once the arch handle is published; otherwise emits one SBI-console
/// record and parks the hart.
pub fn handle_panic_via_kernel_core(info: &PanicInfo<'_>) -> ! {
    let raw = PANIC_ARCH_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        // Pre-init panic: no arch handle for `current_cpu`.
        let mut w = SbiWriter;
        let _ = writeln!(w, "[tairix-kernel] riscv64 panic before init: {info}");
        halt_current_hart()
    } else {
        // SAFETY: `publish_arch` only ever stores `Arc::as_ptr(&arc)`,
        // where `arc: Arc<RiscvBinArch>` is held by `kernel_core`'s
        // `BootInfo` for the running kernel's lifetime; the pointee
        // therefore outlives every panic that can observe a non-null
        // pointer.
        let arch: &RiscvBinArch = unsafe { &*raw };
        let ctx = PanicContext::new(arch, &SERIAL_SINK).with_backtrace(&BACKTRACER);
        handle_panic(info, &ctx)
    }
}
