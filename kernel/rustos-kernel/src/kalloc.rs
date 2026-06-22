//! Kernel heap allocator for the `rustos-kernel` binaries.
//!
//! The allocator itself lives in the shared [`rustos_kalloc`] crate
//! (`lib/kalloc`) so the production binary, every
//! `tests/integration/*` QEMU bin, and the architecture ports' boot
//! harnesses all register the identical [`FreeListAllocator`]
//! (`AGENTS.md` §2.2 — no duplication, §6 — shared code lives in
//! `lib/`). This module re-exports it so a consumer that depends on
//! `rustos-kernel` reaches the heap types through `rustos_kernel::kalloc`
//! without also naming the `lib/kalloc` crate directly.

pub use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
