//! RustOS kernel memory subsystem (Stage 2.2 of `PLAN.md`).
//!
//! This crate is **architecture-neutral**. Anything that touches a real
//! page table, a TLB, or a CPU control register lives in `kernel/arch/*`
//! and is plugged in through the [`PageTableOps`] trait.
//!
//! The four public layers, top to bottom:
//!
//! 1. [`sensitive`] — zero-on-free buffers for credentials, keys, and
//!    capability tokens, backed by the audited `zeroize` crate
//!    (`AGENTS.md` §4: "Zero-on-free for any allocation that ever held
//!    credentials, keys, or capability tokens").
//! 2. [`slab`] — fixed-size kernel object allocator with guard pages on
//!    both sides of every slab (`AGENTS.md` §4: "Guard pages around
//!    kernel slabs").
//! 3. [`vmm`] — per-process [`AddressSpace`], generic over a
//!    [`PageTableOps`] implementation. The architecture crates supply the
//!    real implementation in Stage 3; a `HostPageTable` test double is
//!    provided here, gated behind `#[cfg(test)]`, so this crate is fully
//!    host-testable.
//! 4. [`frame`] — physical [`FrameAllocator`], a buddy/bitmap hybrid that
//!    respects bootloader-supplied reserve regions described by a
//!    [`BootMemoryMap`].
//!
//! # Allocation contract
//!
//! Every allocator entry point returns
//! `Result<_, `[`AllocError`]`>`. No path panics on out-of-memory
//! (`AGENTS.md` §4: *"Deterministic OOM behaviour: allocation failure
//! is a `Result`, never a panic."*).
//!
//! # Unsafe and pointer arithmetic
//!
//! Every `unsafe` block carries a `// SAFETY:` rationale per `AGENTS.md`
//! §2.10. Raw pointer arithmetic only happens inside the bounds-checked
//! helpers in [`ptr`]; no other module is allowed to call
//! `<*mut _>::add` / `<*mut _>::offset` directly.
//!
//! # Documentation
//!
//! See `docs/src/architecture/memory.md` for the architecture-level
//! description.

#![no_std]
#![cfg_attr(loom, allow(dead_code))]

extern crate alloc;

pub mod bootinfo;
pub mod dma;
pub mod error;
pub mod frame;
pub mod loader;
pub mod mmio;
pub mod phys;
pub mod ptr;
pub mod sensitive;
pub mod slab;
pub mod spawn;
pub mod swap;
pub mod uaccess;
pub mod vmm;

pub use bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
pub use dma::{DmaBuffer, DmaError, DmaPool};
pub use error::AllocError;
pub use frame::{Frame, FrameAllocator, FrameCount, PhysAddr, MAX_ORDER, PAGE_SHIFT, PAGE_SIZE};
pub use loader::{map_flags_for, map_image, LoadError};
pub use mmio::{MmioError, MmioMap, MmioRegion};
pub use phys::{DirectPhysMap, PhysMap};
pub use sensitive::SensitiveBuffer;
pub use slab::{Slab, SlabError, SlabHandle, SoftwareTagCheck};
pub use spawn::{build_process_image, ProcessImage, SpawnError, UserStack};
pub use swap::{
    EncryptedSwap, EntropySource, SwapBackend, SwapError, SwapKey, SwapPage, SWAP_RECORD_LEN,
};
pub use uaccess::{copy_in, copy_out, UaccessError};
pub use vmm::{
    AddressSpace, MapFlags, Page, PageTableError, PageTableOps, UserAddressSpace, VirtAddr,
};

#[cfg(any(test, feature = "host-tests"))]
pub use phys::SimPhysMap;
#[cfg(any(test, feature = "host-tests"))]
pub use vmm::HostPageTable;
