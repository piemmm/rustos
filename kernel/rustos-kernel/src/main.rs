//! `rustos-kernel` — the production freestanding x86_64 kernel binary.
//!
//! See `kernel/rustos-kernel/README.md` for the full design rationale.
//!
//! The binary is a thin wrapper around [`rustos_kernel::boot`]: it
//! supplies the `#[global_allocator]`, the `#[panic_handler]`, the
//! `extern "C" fn kernel_main` symbol the arch trampoline calls, and
//! the production COM1-backed log/audit sinks. Everything else — the
//! Multiboot2/ACPI parse, the `X86_64Arch` construction, per-CPU
//! init, the fail-closed syscall dispatch callback — lives in the
//! `rustos_kernel` library half so the QEMU integration test bin
//! (`tests/integration/kernel_arch_boot`) can re-use the same boot
//! pipeline with a different audit sink.

#![cfg_attr(all(target_arch = "x86_64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "x86_64", target_os = "none"), no_main)]
#![deny(missing_docs)]

// --- Freestanding production bin (`x86_64-unknown-none`) -----------

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{boot, handle_panic_via_kernel_core, BumpAllocator, SERIAL_SINK};

    // --- Bump-allocator-backed `#[global_allocator]` ---------------

    /// Static heap for the bump allocator.
    ///
    /// `static mut` because the bump allocator hands out disjoint
    /// slices via an `AtomicUsize` cursor; the storage itself is
    /// otherwise immutable from any other call site. `AGENTS.md` §2
    /// — the *one* `static mut` the binary needs, justified in
    /// `README.md` as the boot-heap arena.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the allocator is constructed from `HEAP`'s base
    /// pointer in `const` context; the pointer is page-aligned (the
    /// `Heap` type is `#[repr(C, align(4096))]`) and the storage
    /// lives for the lifetime of the binary because `HEAP` is a
    /// `static`. The allocator is not exposed through any other
    /// API, so it satisfies the `BumpAllocator::new` uniqueness
    /// requirement.
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Panic handler --------------------------------------------

    /// Forward to the shared bridge in `rustos_kernel::panic_ctx`.
    /// See `panic_ctx.rs` for the full handler contract.
    #[panic_handler]
    fn rustos_kernel_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// The arch crate's `entry::rustos_arch_x86_64_main` validates the
    /// Multiboot2 magic and forwards the unmodified `multiboot_info`
    /// pointer here. We hand the rest of the pipeline to
    /// [`rustos_kernel::boot`] with the production COM1-backed log
    /// and audit sinks: in production both go to the same serial
    /// console (the audit sink is intercepted by the QEMU
    /// integration test's bin only — see
    /// `tests/integration/kernel_arch_boot`).
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(multiboot_info, &SERIAL_SINK, &SERIAL_SINK)
    }
}

// --- Host stub -----------------------------------------------------
//
// On host triples (`cargo build --workspace` / `cargo test`) the
// crate's binary half has nothing to run: the freestanding kernel
// builds only for `x86_64-unknown-none`. The host stub keeps the
// crate compilable on the host so the workspace `cargo build` /
// `cargo test` invocations the rest of the project does succeed.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn main() {}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
#[allow(dead_code)]
fn _suppress_unused_lib() {
    // Reference the library half from the host build so cargo's
    // dead-code lint stays quiet without an `#[allow]` on the lib
    // itself.
    let _ = rustos_kernel::BumpAllocator::used;
}
