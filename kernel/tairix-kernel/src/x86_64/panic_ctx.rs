//! Panic-handler bridge between the bin crates and
//! [`tairix_kernel_core::handle_panic`].
//!
//! # Why this lives in the library half
//!
//! Each binary (`src/main.rs` and the QEMU integration test bin) must
//! declare its own `#[panic_handler]` — Rust forbids library-defined
//! panic handlers. The handler bodies themselves are identical, so we
//! factor the shared logic into [`handle_panic_via_kernel_core`] and
//! the bins call it from their one-line `#[panic_handler]`.
//!
//! # How [`X86_64Arch`] is reached
//!
//! [`tairix_kernel_core::handle_panic`] needs a `PanicContext` carrying
//! a `&BinArch` for `current_cpu` and an `audit_sink`. The arch handle
//! does not exist until after [`crate::x86_64::boot::boot`] has parsed the
//! ACPI MADT, so we cannot place it in a plain `static`. Instead the
//! bin's `boot()` call publishes `Arc::as_ptr(&arc)` into the
//! lib-exported [`PANIC_ARCH_PTR`] (an [`AtomicPtr`]) before any code
//! that could panic runs. The handler loads the pointer, and:
//!
//! * If it is non-null, forwards through `kernel_core::handle_panic`
//!   with a `PanicContext { arch: &*ptr, audit_sink: &SERIAL_SINK }`.
//! * Otherwise (pre-init panic — e.g. from inside `boot()` itself, or
//!   from the global allocator on heap exhaustion) it logs a single
//!   `"panic before init"` record to COM1 and parks the CPU via
//!   [`tairix_arch_x86_64::kernel_arch::halt`]
//!   (fail closed, never silently reset).
//!
//! The handler is **identical** in production and in the integration
//! test, so both bins share this bridge verbatim.

use core::fmt::Write as _;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, Ordering};

use tairix_arch_x86_64::kernel_arch::halt as arch_halt;
use tairix_arch_x86_64::serial::{Serial, COM1_BASE};
use tairix_kernel_core::{handle_panic, PanicContext};

use crate::x86_64::arch_wrapper::BinArch;
use crate::x86_64::serial_sink::SERIAL_SINK;

/// `AtomicPtr<BinArch>` published by [`crate::x86_64::boot::boot`] once the
/// architecture handle is constructed.
///
/// The pointer is `Arc::as_ptr(&arc)`, which is stable for the
/// lifetime of the `Arc`; the `Arc` itself lives for the lifetime of
/// the running kernel (it is held by `BootInfo` and re-cloned into
/// `kernel_core`'s internal `KernelState`).
pub static PANIC_ARCH_PTR: AtomicPtr<BinArch> = AtomicPtr::new(core::ptr::null_mut());

/// Publish `arch` into [`PANIC_ARCH_PTR`].
///
/// Called by [`crate::x86_64::boot::boot`] immediately after constructing the
/// `Arc<BinArch>` (and before installing the syscall dispatch
/// callback / arming the LAPIC timer).
///
/// # Safety
///
/// `arch_ptr` must remain valid for the lifetime of the kernel image.
/// The standard caller is `Arc::as_ptr(&arc) as *mut BinArch`, where
/// `arc` is kept alive by `BootInfo`'s `arch` field.
pub unsafe fn publish_arch(arch_ptr: *const BinArch) {
    // `Release` so a panic that follows on another CPU observes the
    // initialised pointer; the matching `Acquire` load lives in
    // [`handle_panic_via_kernel_core`].
    PANIC_ARCH_PTR.store(arch_ptr.cast_mut(), Ordering::Release);
}

/// Shared `#[panic_handler]` body used by both bin crates.
///
/// Always returns `!`. Forwards to `tairix_kernel_core::handle_panic`
/// when the bin has reached the post-arch-construction phase of boot;
/// otherwise emits one COM1 record and halts.
pub fn handle_panic_via_kernel_core(info: &PanicInfo<'_>) -> ! {
    let raw = PANIC_ARCH_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        // Pre-init panic. We have no arch handle to ask for
        // `current_cpu`; emit a single best-effort record and halt.
        let mut s = Serial::init(COM1_BASE);
        let _ = writeln!(s, "[tairix-kernel] panic before init: {info}");
        arch_halt()
    } else {
        // SAFETY: `publish_arch` only ever stores the result of
        // `Arc::as_ptr(&arc)`, where `arc: Arc<BinArch>` is held by
        // `kernel_core::BootInfo` for the lifetime of the running
        // kernel; the pointee therefore outlives every panic that can
        // observe a non-null `PANIC_ARCH_PTR`.
        let arch: &BinArch = unsafe { &*raw };
        let ctx = PanicContext::new(arch, &SERIAL_SINK);
        handle_panic(info, &ctx)
    }
}
