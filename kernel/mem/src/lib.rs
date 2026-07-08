//! RustOS kernel memory subsystem (Stage 2.2 of `PLAN.md`).
//!
//! This crate is **architecture-neutral**. Anything that touches a real
//! page table, a TLB, or a CPU control register lives in `kernel/arch/*`
//! and is plugged in through the Arch HAL page-table surface
//! (`rustos_arch_api::mmu::AddressSpace` + `rustos_arch_api::tlb::TlbShootdown`),
//! re-exported here behind the [`PageTable`] bound alias.
//!
//! The four public layers, top to bottom:
//!
//! 1. [`sensitive`] — zero-on-free buffers for credentials, keys, and
//!    capability tokens, backed by the audited `zeroize` crate
//!    (: "Zero-on-free for any allocation that ever held
//!    credentials, keys, or capability tokens").
//! 2. [`slab`] — fixed-size kernel object allocator with guard pages on
//!    both sides of every slab (: "Guard pages around
//!    kernel slabs").
//! 3. [`vmm`] — per-process [`AddressSpace`], generic over a
//!    [`PageTable`] backend (a port's HAL page-table implementation).
//!    The architecture crates supply the real implementation; a
//!    `HostPageTable` test double is provided here, gated behind
//!    `#[cfg(test)]`, so this crate is fully host-testable.
//! 4. [`frame`] — physical [`FrameAllocator`], a buddy/bitmap hybrid that
//!    respects bootloader-supplied reserve regions described by a
//!    [`BootMemoryMap`].
//!
//! # Allocation contract
//!
//! Every allocator entry point returns
//! `Result<_, `[`AllocError`]`>`. No path panics on out-of-memory
//! (: *"Deterministic OOM behaviour: allocation failure
//! is a `Result`, never a panic."*).
//!
//! # Unsafe and pointer arithmetic
//!
//! Every `unsafe` block carries a `// SAFETY:` rationale. Raw pointer arithmetic only happens inside the bounds-checked
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

pub mod anon;
pub mod anon_window;
pub mod bootinfo;
pub mod dma;
pub mod error;
pub mod frame;
pub mod live;
pub mod loader;
pub mod mmio;
pub mod pagetables;
pub mod phys;
pub mod pressure;
pub mod ptr;
pub mod reclaim;
pub mod sensitive;
pub mod slab;
pub mod spawn;
pub mod swap;
pub mod uaccess;
pub mod vmm;

pub use anon::{map_anonymous, page_count_for, unmap_anonymous, AnonError, ANON_FLAGS};
pub use anon_window::AnonWindowMap;
pub use bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
pub use dma::{DmaBuffer, DmaError, DmaPool, DmaWindowMap};
pub use error::AllocError;
pub use frame::{Frame, FrameAllocator, FrameCount, PhysAddr, MAX_ORDER, PAGE_SHIFT, PAGE_SIZE};
pub use live::{DmaMapping, LiveSpace, LiveSpaceError, LiveUserSpace};
pub use loader::{map_flags_for, map_image, LoadError};
pub use mmio::{MmioError, MmioMap, MmioRegion, MmioWindowMap};
pub use pagetables::FrameTableSource;
pub use phys::{DirectPhysMap, PhysMap};
pub use pressure::{
    escalation, ramzip_handoff, shrink_target, EscalationStep, FreeMemorySource, MemoryPressure,
    PressureBand, PressureThresholds, RamzipHandoff,
};
pub use reclaim::{
    AccountingError, AdmissionRefusal, CacheAccounting, CacheBudget, CacheCandidate, CachePolicy,
    InvalidationSource, RebuildCost, ReclaimClass, ReclaimOwner, ReclaimRule, Sensitivity,
    MAX_ENTRY_METADATA,
};
pub use sensitive::SensitiveBuffer;
pub use slab::{Slab, SlabError, SlabHandle, SoftwareTagCheck};
pub use spawn::{build_process_image, ProcessImage, SpawnError, UserStack};
pub use swap::{
    EncryptedSwap, EntropySource, SwapBackend, SwapError, SwapKey, SwapPage, SWAP_RECORD_LEN,
};
pub use uaccess::{copy_in, copy_out, UaccessError};
pub use vmm::{
    AddressSpace, FrozenAddressSpace, MapFlags, Page, PageTable, PageTableError, UserAddressSpace,
    VirtAddr,
};

#[cfg(any(test, feature = "host-tests"))]
pub use phys::SimPhysMap;
#[cfg(any(test, feature = "host-tests"))]
pub use vmm::HostPageTable;
