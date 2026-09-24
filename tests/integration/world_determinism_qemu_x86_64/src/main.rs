//! QEMU integration test: the world generator agrees with the reference
//! digest on `x86_64`.
//!
//! ## What this proves that a host test cannot
//!
//! `WinterSun`'s world is a pure function of its seed, and the realm server
//! and every connected client generate it independently. If two of them
//! disagree by one sub-unit, a player walks through a hill on one machine
//! and around it on another, and no amount of netcode recovers from that.
//!
//! The arithmetic is written to be target-independent — IEEE-754 basic
//! operations, `lib/util::mathf` rather than a platform libm, quantised
//! integer outputs, total orders on every sort — but "written to be" is
//! not evidence. This vertical is the evidence: the whole pipeline runs
//! on a freestanding target with no platform libm at all, and the digest
//! it folds must equal
//! `tairix_wintersun_world::digest::REFERENCE_DIGEST` — the same
//! constant the host suite and the sibling `aarch64`, `riscv64` and
//! `wasm32` verticals assert. Agreement between targets follows from each
//! agreeing with the constant, so no target can pass by having never run.
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
    use tairix_wintersun_world::digest::{self, REFERENCE_DIGEST};

    /// Static boot heap. The generator's coarse field, its flood queues
    /// and the chunk arrays all live here. `static mut` because the
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
        let _ = writeln!(com1, "[world_determinism] booted on x86_64");

        let Ok(produced) = digest::world(digest::reference_params()) else {
            let _ = writeln!(com1, "[world_determinism] FAIL: the realm did not solve");
            qemu_exit::exit_failure();
        };

        // Both values on the failure path, so a mismatch names the
        // divergence rather than merely announcing one.
        if produced == REFERENCE_DIGEST {
            let _ = writeln!(
                com1,
                "[world_determinism] PASS: digest {produced:#018x} matches the reference"
            );
            qemu_exit::exit_success();
        }
        let _ = writeln!(
            com1,
            "[world_determinism] FAIL: digest {produced:#018x}, expected {REFERENCE_DIGEST:#018x}"
        );
        qemu_exit::exit_failure();
    }
}

/// The panic handler: nothing here panics, so reaching it is itself the
/// failure the vertical reports.
#[panic_handler]
#[cfg(itest_x86_64)]
fn world_determinism_qemu_x86_64_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    tairix_arch_x86_64::panic::handle_panic_via_serial(info)
}

// Host stub. The crate is only meaningful on the bare-metal target; on the
// host a no-op `main` keeps `cargo build` and `cargo test` green without
// the crate having to be special-cased.
#[cfg(not(itest_x86_64))]
fn main() {}
