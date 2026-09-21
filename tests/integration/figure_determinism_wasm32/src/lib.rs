//! wasm32 leg of the figure engine's cross-architecture determinism
//! vertical.
//!
//! ## Why this one is not a browser test
//!
//! The sibling wasm32 verticals boot the port in headless Chrome because
//! what they test *is* the browser: a canvas, a worker, a host import.
//! This one tests arithmetic. A WebAssembly engine is all it needs, and
//! asking for a browser would make the fourth Tier-1 leg runnable in
//! fewer places than the claim it defends applies to.
//!
//! So the module exports one function, `figure_digest`, and the harness
//! (`web/harness.mjs`) instantiates it and compares the result against
//! `tairix_wintersun_figure::digest::REFERENCE_DIGEST` — the same
//! constant the host suite and the three QEMU verticals assert.
//!
//! wasm32 is the leg most likely to catch a real divergence: it is the
//! only Tier-1 target with a 32-bit `usize`, so an index or a length that
//! had quietly become part of an answer would show up here and nowhere
//! else.
//!
//! On a host build (`itest_wasm32` off) this compiles to an inert empty
//! `cdylib`, so `cargo build --workspace` stays green without the wasm
//! toolchain.

#![cfg_attr(itest_wasm32, no_std)]
#![deny(missing_docs)]

#[cfg(itest_wasm32)]
mod vertical {
    use core::panic::PanicInfo;

    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_wintersun_figure::digest;

    /// Static heap, the same one the bare-metal siblings use. The figure
    /// path itself allocates nothing; it is here because the shared
    /// rasteriser the figure crate links names `alloc`.
    ///
    /// `static mut` because the allocator hands out disjoint slices via an
    /// atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the module and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Nothing here panics; reaching this is the failure the harness
    /// reports, and trapping is how a wasm module says so.
    #[panic_handler]
    fn figure_determinism_wasm32_panic(_: &PanicInfo<'_>) -> ! {
        core::arch::wasm32::unreachable()
    }

    /// The digest of the reference grid, or `0` if it could not be
    /// placed — which the harness reports as a failure rather than
    /// mistaking for an answer.
    #[no_mangle]
    pub extern "C" fn figure_digest() -> u64 {
        digest::reference().unwrap_or(0)
    }

    /// The constant the digest must equal, read back from the crate so
    /// the harness cannot compare against a stale copy of it.
    #[no_mangle]
    pub extern "C" fn reference_digest() -> u64 {
        digest::REFERENCE_DIGEST
    }
}
