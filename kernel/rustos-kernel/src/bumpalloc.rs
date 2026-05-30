//! Boot-heap bump allocator for the `rustos-kernel` binaries.
//!
//! The allocator itself lives in the shared [`rustos_bumpalloc`] crate
//! (`lib/bumpalloc`) so the production binary, every
//! `tests/integration/*` QEMU bin, and the architecture ports' boot
//! harnesses all register the identical implementation
//! (`AGENTS.md` §2.2 — no duplication, §6 — shared code lives in
//! `lib/`). This module re-exports it under the historical
//! `rustos_kernel::bumpalloc` path so existing call sites
//! (`rustos_kernel::bumpalloc::{Heap, HEAP_BYTES}`,
//! `rustos_kernel::BumpAllocator`) keep their imports unchanged.

pub use rustos_bumpalloc::{BumpAllocator, Heap, HEAP_BYTES};
