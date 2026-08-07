//! Panic-handler bridge between the aarch64 kernel binary and
//! [`tairix_kernel_core::handle_panic`].
//!
//! # Why this lives in the bin's library half
//!
//! The architecture crate (`tairix_arch_aarch64`) may not depend on
//! `kernel/core` (the layering contract: an arch port names only the Arch
//! HAL and `lib/*`), so the bridge that turns a `#[panic_handler]` into a
//! `kernel_core::handle_panic` call lives here, in the bin crate that
//! already links both. This is the exact sibling of
//! [`crate::x86_64::panic_ctx`]; the previous per-arch serial-banner body
//! in the arch crate is gone (one panic path, not three).
//!
//! # How [`Aarch64BinArch`] is reached
//!
//! `handle_panic` needs a `PanicContext` carrying a `&Aarch64BinArch`
//! (for `current_cpu` / `halt`) and the audit sink. The arch handle is
//! built partway through boot, so it cannot live in a plain `static`;
//! `boot` publishes `Arc::as_ptr(&arc)` into [`PANIC_ARCH_PTR`] before any
//! code that could panic runs. The handler loads the pointer, and:
//!
//! * if non-null, forwards through `kernel_core::handle_panic` with the
//!   port's [`Backtracer`] attached, so the dump carries registers and a
//!   bounded backtrace;
//! * otherwise (a pre-init panic — from inside `boot` itself, or the
//!   allocator on heap exhaustion) it flushes the buffered serial ring,
//!   emits one best-effort record on the console, and parks the CPU
//!   forever (fail closed, never a silent reset).

use core::fmt::Write as _;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, Ordering};

use tairix_arch_aarch64::backtrace::Backtracer;
use tairix_arch_aarch64::serial::{flush_serial_blocking, ConsoleWriter};
use tairix_arch_aarch64::{halt_current_cpu, SERIAL_SINK};
use tairix_kernel_core::{handle_panic, PanicContext};

use crate::aarch64::arch_wrapper::{console_layout, Aarch64BinArch};

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
    // pointer; the matching `Acquire` load is in
    // [`handle_panic_via_kernel_core`].
    PANIC_ARCH_PTR.store(arch_ptr.cast_mut(), Ordering::Release);
}

/// Shared `#[panic_handler]` body for the aarch64 kernel binaries.
///
/// Always returns `!`. Forwards to `tairix_kernel_core::handle_panic`
/// once the arch handle is published; otherwise emits one console record
/// and parks the CPU.
pub fn handle_panic_via_kernel_core(info: &PanicInfo<'_>) -> ! {
    let raw = PANIC_ARCH_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        // Pre-init panic: no arch handle for `current_cpu`. Flush the
        // buffered serial ring so the lead-up context reaches a capture,
        // emit one best-effort line, and park.
        flush_serial_blocking();
        let mut w = ConsoleWriter;
        let _ = writeln!(w, "[tairix-kernel] aarch64 panic before init: {info}");
        halt_current_cpu()
    } else {
        // Flush buffered serial output before the structured dump so the
        // pre-panic diagnostic context reaches a serial capture.
        flush_serial_blocking();
        // SAFETY: `publish_arch` only ever stores `Arc::as_ptr(&arc)`,
        // where `arc: Arc<Aarch64BinArch>` is held by `kernel_core`'s
        // `BootInfo` for the running kernel's lifetime; the pointee
        // therefore outlives every panic that can observe a non-null
        // pointer.
        let arch: &Aarch64BinArch = unsafe { &*raw };
        // The console list lets the dump take the framebuffer surface back
        // from a graphical session first: a panic behind a desktop's last
        // frame would otherwise be invisible on the screen.
        let (consoles, _) = console_layout();
        let ctx = PanicContext::new(arch, &SERIAL_SINK)
            .with_backtrace(&BACKTRACER)
            .with_consoles(consoles);
        handle_panic(info, &ctx)
    }
}
