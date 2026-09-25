//! QEMU integration test: the client's composited frames agree with the
//! reference digest on `x86_64`.
//!
//! ## What this proves that a host test cannot
//!
//! Four crates meet in a frame. The world generator and the figure engine
//! work in `f64`; the ground art is integer throughout and carries no
//! vertical of its own because bit-identity follows from the language
//! there; this client's projection, lattice sampling, shading and figure
//! pass are integer too. Each part agreeing separately does not say the
//! composition does.
//!
//! So two whole frames are drawn here, with figures standing in them —
//! one wide at full quality, one close with every degradation rung shed —
//! and every pixel is folded,
//! with `tairix_wintersun_art::digest::REFERENCE_DIGEST` folded in after
//! them, into a single number that must equal
//! `tairix_wintersun_app::digest::REFERENCE_DIGEST`. That is the
//! constant the host suite and the sibling verticals assert, so
//! agreement between targets follows from each agreeing with it and no
//! target can pass by having never run.
//!
//! It is also what first builds the ground art and the figures for this
//! target at all.
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
    use tairix_wintersun_app::digest::{self, REFERENCE_DIGEST};

    /// Static boot heap. The realm field, the generated chunks, the
    /// synthesised material tiles and the two frame buffers all live
    /// here. `static mut` because the allocator hands out disjoint slices
    /// via an atomic cursor; the storage is otherwise never aliased.
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
        let _ = writeln!(com1, "[client_frame] booted on x86_64");

        let Ok(produced) = digest::reference() else {
            let _ = writeln!(com1, "[client_frame] FAIL: the frames could not be drawn");
            qemu_exit::exit_failure();
        };

        // Both values on the failure path, so a mismatch names the
        // divergence rather than merely announcing one.
        if produced == REFERENCE_DIGEST {
            let _ = writeln!(
                com1,
                "[client_frame] PASS: digest {produced:#018x} matches the reference"
            );
            qemu_exit::exit_success();
        }
        let _ = writeln!(
            com1,
            "[client_frame] FAIL: digest {produced:#018x}, expected {REFERENCE_DIGEST:#018x}"
        );
        qemu_exit::exit_failure();
    }
}

/// The panic handler: nothing here panics, so reaching it is itself the
/// failure the vertical reports.
#[panic_handler]
#[cfg(itest_x86_64)]
fn client_frame_qemu_x86_64_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    tairix_arch_x86_64::panic::handle_panic_via_serial(info)
}

// Host stub. The crate is only meaningful on the bare-metal target; on the
// host a no-op `main` keeps `cargo build` and `cargo test` green without
// the crate having to be special-cased.
#[cfg(not(itest_x86_64))]
fn main() {}
