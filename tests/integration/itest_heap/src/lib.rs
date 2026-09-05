//! The `#[global_allocator]` a freestanding integration binary links when its
//! crate graph reaches `alloc` but its own test path never allocates.
//!
//! A `no_std` binary whose dependency graph links `alloc` must name a global
//! allocator, and the graph of every bare-metal vertical reaches it through
//! its architecture port: the port logs, `lib/log` frames its early-boot
//! records over `lib/collections`' ring, and that container crate links
//! `alloc` for its heap-backed tier. Cargo resolves features per build, and
//! the QEMU stage builds each target's whole enrolled set in one invocation
//! alongside verticals that link the full kernel, so the heap-backed tier is
//! compiled in whatever a single vertical needs.
//!
//! Most verticals already carry a heap because they link `tairix-kernel` and
//! genuinely allocate. The few that exercise only an architecture primitive —
//! a page-table isolation proof, a referenced-bit probe, a TLB shootdown —
//! allocate nothing, and would otherwise each restate the same arena and
//! attribute. They link this crate instead.
//!
//! The arena is deliberately one page: nothing on those verticals' paths asks
//! for memory, so a request that arrives is a defect rather than a capacity
//! problem, and the allocator answers it with null — which `alloc` turns into
//! the binary's own panic handler and the vertical's QEMU failure exit. A
//! larger arena would hide that, and would push the image past the identity
//! window a paging vertical maps.
//!
//! A vertical that *does* allocate installs its own allocator over a real
//! heap; two global allocators in one binary do not link, which is what keeps
//! the two cases from being confused.
//!
//! On a host build the crate is empty, so its own test binary keeps the
//! standard library's allocator instead of a one-page arena.

#![no_std]

#[cfg(freestanding)]
mod arena {
    use tairix_kalloc::FreeListAllocator;

    /// Bytes of arena. One page, for the reason the crate docs give.
    const HEAP_BYTES: usize = 4096;

    /// Page-aligned so the arena's address satisfies the allocator's
    /// two-machine-word alignment requirement with room to spare.
    #[repr(C, align(4096))]
    struct Arena([u8; HEAP_BYTES]);

    static mut ARENA: Arena = Arena([0; HEAP_BYTES]);

    // SAFETY: `ARENA` is a `static` that outlives the binary, is page-aligned,
    // and this allocator is its only reader or writer — nothing outside this
    // module names it.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(ARENA) as *mut u8, HEAP_BYTES) };
}
