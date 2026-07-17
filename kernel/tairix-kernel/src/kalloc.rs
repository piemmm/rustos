//! Kernel heap allocator for the `tairix-kernel` binaries.
//!
//! The allocator itself lives in the shared [`tairix_kalloc`] crate
//! (`lib/kalloc`) so the production binary, every
//! `tests/integration/*` QEMU bin, and the architecture ports' boot
//! harnesses all register the identical [`FreeListAllocator`]
//! (no duplication — shared code lives in
//! `lib/`). This module re-exports it so a consumer that depends on
//! `tairix-kernel` reaches the heap types through `tairix_kernel::kalloc`
//! without also naming the `lib/kalloc` crate directly.

pub use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
