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

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Freestanding production bin (`x86_64-unknown-none`) -----------

#[cfg(all(freestanding, kernel_isa = "x86_64"))]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{boot, handle_panic_via_kernel_core, FreeListAllocator, SERIAL_SINK};

    // --- Bump-allocator-backed `#[global_allocator]` ---------------

    /// Static heap for the bump allocator.
    ///
    /// `static mut` because the bump allocator hands out disjoint
    /// slices via an `AtomicUsize` cursor; the storage itself is
    /// otherwise immutable from any other call site.
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
    /// API, so it satisfies the `FreeListAllocator::new` uniqueness
    /// requirement.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Panic handler --------------------------------------------

    /// Forward to the shared bridge in `rustos_kernel::x86_64::panic_ctx`.
    /// See `panic_ctx.rs` for the full handler contract.
    #[panic_handler]
    fn rustos_kernel_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// The arch crate's `entry::rustos_arch_x86_64_main` validates the
    /// boot magic (multiboot2 or PVH), records the protocol, and
    /// forwards the unmodified `boot_info`
    /// pointer here. We hand the rest of the pipeline to
    /// [`rustos_kernel::boot`] with the production COM1-backed log
    /// and audit sinks: in production both go to the same serial
    /// console (the audit sink is intercepted by the QEMU
    /// integration test's bin only — see
    /// `tests/integration/kernel_arch_boot`).
    #[no_mangle]
    pub extern "C" fn kernel_main(boot_info: u64) -> ! {
        boot(boot_info, &SERIAL_SINK, &SERIAL_SINK)
    }
}

// --- Freestanding production bin (`aarch64-unknown-none`, Raspberry
//     Pi 4) -------------------------------------------------------
//
// `plans/PI.md` Stage P1. A thin wrapper around
// [`rustos_kernel::aarch64::boot::boot`]: it supplies the
// `#[global_allocator]`, the `#[panic_handler]`, and the
// `extern "C" fn kernel_main(dtb)` symbol the aarch64 boot trampoline
// (`rustos_arch_aarch64`'s `boot.s` → `entry.rs`) calls, then hands off
// to the boot pipeline with the port's PL011-backed console sink.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_aarch64::{handle_panic_via_serial, SERIAL_SINK};
    use rustos_kernel::aarch64::boot;
    use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::FreeListAllocator;

    /// Static boot heap for the bump allocator.
    ///
    /// `static mut` because the bump allocator hands out disjoint slices
    /// via an `AtomicUsize` cursor; the storage is otherwise never
    /// aliased. It lives in `.bss` (zeroed by the boot trampoline). This
    /// is the boot-heap arena — the one `static mut` the binary needs.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the allocator is constructed from `HEAP`'s base pointer in
    /// `const` context; the pointer is page-aligned (`Heap` is
    /// `#[repr(C, align(4096))]`) and the storage lives for the lifetime
    /// of the binary because `HEAP` is a `static`. The allocator is not
    /// exposed through any other API, satisfying `FreeListAllocator::new`'s
    /// uniqueness requirement.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Forward to the shared aarch64 panic bridge (parks the CPU).
    #[panic_handler]
    fn rustos_kernel_panic_aarch64(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// The symbol the aarch64 boot trampoline calls (via
    /// `rustos_arch_aarch64_main`). Hands the verbatim DTB pointer and
    /// the production PL011-backed log/audit sinks to the boot pipeline.
    /// In production both the log and audit streams go to the same serial
    /// console; the boot-completed QEMU vertical replaces the audit sink
    /// (see `tests/integration/kernel_arch_boot_aarch64`).
    #[no_mangle]
    pub extern "C" fn kernel_main(dtb: u64) -> ! {
        boot::boot(dtb, &SERIAL_SINK, &SERIAL_SINK)
    }
}

// --- Freestanding production bin (`riscv64gc-unknown-none-elf`, QEMU
//     `virt` / SiFive) -------------------------------------------------
//
// `plans/PI.md` RV-P1. A thin wrapper around
// [`rustos_kernel::riscv64::boot::boot`]: it supplies the
// `#[global_allocator]`, the `#[panic_handler]`, and the
// `extern "C" fn kernel_main(hartid, dtb)` symbol the riscv64 boot
// trampoline (`rustos_arch_riscv64`'s `boot.s` → `entry.rs`) calls, then
// hands off to the boot pipeline with the port's SBI-backed console sink.
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_riscv64::{handle_panic_via_serial, SERIAL_SINK};
    use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::riscv64::boot;
    use rustos_kernel::FreeListAllocator;

    /// Static boot heap for the bump allocator.
    ///
    /// `static mut` because the bump allocator hands out disjoint slices
    /// via an `AtomicUsize` cursor; the storage is otherwise never
    /// aliased. It lives in the linker's NOLOAD `.heap` section
    /// (`riscv64-virt.ld`), placed after `__bss_end` so the boot
    /// trampoline neither zeroes nor counts it in the usable
    /// physical-memory map (which starts at `__kernel_end`). This is the
    /// boot-heap arena — the one `static mut` the binary needs.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the allocator is constructed from `HEAP`'s base pointer in
    /// `const` context; the pointer is page-aligned (`Heap` is
    /// `#[repr(C, align(4096))]`) and the storage lives for the lifetime
    /// of the binary because `HEAP` is a `static`. The allocator is not
    /// exposed through any other API, satisfying `FreeListAllocator::new`'s
    /// uniqueness requirement.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Forward to the shared riscv64 panic bridge (parks the hart).
    #[panic_handler]
    fn rustos_kernel_panic_riscv64(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// The symbol the riscv64 boot trampoline calls (via
    /// `rustos_arch_riscv64_main`). Hands the verbatim `a0`=hartid /
    /// `a1`=DTB hand-off values and the production SBI-backed log/audit
    /// sinks to the boot pipeline. In production both the log and audit
    /// streams go to the same serial console; the boot-completed QEMU
    /// vertical replaces the audit sink (see
    /// `tests/integration/kernel_arch_boot_riscv64`).
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot::boot(hartid, dtb, &SERIAL_SINK, &SERIAL_SINK)
    }
}

// --- Host stub -----------------------------------------------------
//
// On host triples (`cargo build --workspace` / `cargo test`) the
// crate's binary half has nothing to run: the freestanding kernel
// builds only for the bare-metal targets. The host stub keeps the
// crate compilable on the host so the workspace `cargo build` /
// `cargo test` invocations the rest of the project does succeed.
#[cfg(not(freestanding))]
fn main() {}

#[cfg(not(freestanding))]
#[allow(dead_code)]
fn _suppress_unused_lib() {
    // Reference the library half from the host build so cargo's
    // dead-code lint stays quiet without an `#[allow]` on the lib
    // itself.
    let _ = rustos_kernel::FreeListAllocator::used;
}
