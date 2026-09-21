//! integration test: the figure engine agrees with the reference digest on
//! `x86_64`.
//!
//! ## What this proves that a host test cannot
//!
//! A figure is `f64` throughout — clip sampling and easing, overlay
//! summing, the orthonormal resolve, the skinning blend, the projection and
//! its foreshortening, the two-bone planting solve, the shadow's
//! singular-value decomposition. All of it is IEEE-754 basic operations
//! over `lib/util::mathf`, whose transcendentals are first-party polynomial
//! kernels rather than a platform libm, so bit-identity *follows* from the
//! language.
//!
//! "Follows" is not evidence. This vertical is the evidence: the whole
//! pipeline runs under a different compiler backend on a different ISA, and
//! the digest it folds must equal
//! `tairix_wintersun_figure::digest::REFERENCE_DIGEST` — the same constant
//! the host suite and the sibling targets assert. Agreement between targets
//! follows from each agreeing with the constant, so no target can pass by
//! having never run.
//!
//! The one hazard the language does not foreclose is a backend fusing a
//! multiply and an add into a single rounded operation. Rust does not
//! enable that; this is what says so rather than assuming it.
//!
//! Any other outcome is a closed failure reported to QEMU.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod kernel {
    use core::fmt::Write as _;

    use tairix_arch_x86_64::{qemu_exit, serial};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_wintersun_figure::digest::{self, REFERENCE_DIGEST};

    /// Static boot heap. The figure path itself allocates nothing; the
    /// heap is here because the shared rasteriser the figure crate links
    /// names `alloc`. `static mut` because the
    /// allocator hands out disjoint slices via an atomic cursor; the
    /// storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Entry point for the freestanding kernel. Called by
    /// `tairix_arch_x86_64`'s boot trampoline after the multiboot magic
    /// has been validated.
    #[no_mangle]
    pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        let _ = writeln!(com1, "[figure_determinism] booted on x86_64");

        let Ok(produced) = digest::reference() else {
            let _ = writeln!(com1, "[figure_determinism] FAIL: the grid did not place");
            qemu_exit::exit_failure();
        };

        // Both values on the failure path, so a mismatch names the
        // divergence rather than merely announcing one.
        if produced == REFERENCE_DIGEST {
            let _ = writeln!(
                com1,
                "[figure_determinism] PASS: digest {produced:#018x} matches the reference"
            );
            qemu_exit::exit_success();
        }
        let _ = writeln!(
            com1,
            "[figure_determinism] FAIL: digest {produced:#018x}, expected {REFERENCE_DIGEST:#018x}"
        );
        qemu_exit::exit_failure();
    }
}

/// The panic handler: nothing here panics, so reaching it is itself the
/// failure the vertical reports.
#[panic_handler]
#[cfg(itest_x86_64)]
fn figure_determinism_qemu_x86_64_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write as _;
    use tairix_arch_x86_64::{qemu_exit, serial};

    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[figure_determinism] panic: {info}");
    qemu_exit::exit_failure();
}

// Host stub. The crate is only meaningful on the bare-metal target; on the
// host a no-op `main` keeps `cargo build` and `cargo test` green without
// the crate having to be special-cased.
#[cfg(not(itest_x86_64))]
fn main() {}
