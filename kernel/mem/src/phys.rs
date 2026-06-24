//! Direct physical-memory map: turning a device-visible physical
//! address into a CPU-dereferenceable pointer.
//!
//! `kernel/mem`'s DMA pool ([`crate::dma::DmaPool`]) and MMIO mapper
//! ([`crate::mmio::MmioMap`]) both hand a *device* a physical address
//! (a DMA buffer's frame, a register block's BAR) and then need the
//! CPU to read and write the **same bytes** the device touches. On
//! real hardware those bytes are reachable because the kernel keeps a
//! direct map of physical memory: a fixed virtual window in the active
//! page table where a physical address `p` is reachable at `p +
//! offset`. On the `x86_64` boot path that window is the identity map
//! the trampoline installs over the low 4 GiB (`kernel/arch/x86_64`
//! `boot.s`, SAFETY-INVARIANT 4), so `offset == 0`.
//!
//! This module is the seam between "a `PhysAddr` a device understands"
//! and "a `NonNull<u8>` the CPU can dereference". Production wires a
//! [`DirectPhysMap`] describing the boot direct map; host unit tests
//! wire a `SimPhysMap` that owns a real allocation standing in for
//! physical RAM, so the very pointer a test writes "as the device"
//! aliases the pointer the pool hands the driver (one model exercised in both worlds).
//!
//! The translation is the only place a physical address becomes a
//! pointer; callers route every dereference through the returned
//! [`NonNull`] and the bounds-checked helpers in [`crate::ptr`]
//! (no raw pointer arithmetic without a
//! bounds-checked wrapper).

use core::ptr::NonNull;

use crate::frame::PhysAddr;

/// Translates a device-visible [`PhysAddr`] into a CPU pointer valid
/// for `len` bytes within the kernel's direct physical map.
///
/// Returning [`None`] means `[phys, phys + len)` lies outside the
/// direct map; callers fail closed rather than synthesising a pointer
/// of their own.
pub trait PhysMap {
    /// Map `[phys, phys + len)` to a CPU pointer, or [`None`] if the
    /// range is not covered by the direct map.
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>>;
}

/// The kernel's direct physical map: physical `p` is reachable at the
/// virtual address `p + offset`, for every `p` in `[0, limit)`.
///
/// `offset == 0` describes an identity map (the `x86_64` boot
/// trampoline's low-4-GiB window); a non-zero `offset` describes a
/// higher-half direct map a later boot path may install.
#[derive(Debug, Clone, Copy)]
pub struct DirectPhysMap {
    offset: u64,
    limit: u64,
}

impl DirectPhysMap {
    /// Build a direct map where physical `p` is reachable at
    /// `p + offset`, valid for physical addresses below `limit`.
    #[must_use]
    pub const fn new(offset: u64, limit: u64) -> Self {
        Self { offset, limit }
    }

    /// Build an identity direct map (`offset == 0`) covering
    /// `[0, limit)` — the shape the `x86_64` boot trampoline installs.
    #[must_use]
    pub const fn identity(limit: u64) -> Self {
        Self::new(0, limit)
    }
}

impl PhysMap for DirectPhysMap {
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>> {
        let base = phys.as_u64();
        let len_u64 = u64::try_from(len).ok()?;
        let end = base.checked_add(len_u64)?;
        if end > self.limit {
            return None;
        }
        let virt = base.checked_add(self.offset)?;
        let addr = usize::try_from(virt).ok()?;
        NonNull::new(addr as *mut u8)
    }
}

/// Host-test stand-in for physical RAM.
///
/// Owns a single contiguous, page-aligned allocation representing the
/// physical range `[base, base + len)`. [`translate`](Self::translate)
/// returns a pointer into that allocation, so a unit test can write
/// bytes "as the device" at a physical address and observe them
/// through the same pool accessor a driver would use — the property
/// that makes the DMA/MMIO model hardware-faithful on the host.
#[cfg(any(test, feature = "host-tests"))]
pub struct SimPhysMap {
    base: u64,
    len: usize,
    storage: alloc::vec::Vec<u8>,
    align_pad: usize,
}

#[cfg(any(test, feature = "host-tests"))]
impl SimPhysMap {
    /// Allocate `len` bytes of simulated physical RAM mapped at
    /// physical `base`. The logical window is page-aligned so a
    /// [`crate::mmio::MmioMap`] register window minted over it meets
    /// its word-access alignment contract.
    ///
    /// # Panics
    ///
    /// Panics (test-only) if `len` is zero; a simulator covering no
    /// bytes is a test bug.
    #[must_use]
    pub fn new(base: PhysAddr, len: usize) -> Self {
        assert!(len != 0, "SimPhysMap needs a non-empty window");
        let raw = len + crate::frame::PAGE_SIZE;
        let storage = alloc::vec![0u8; raw];
        let align_pad = storage.as_ptr().align_offset(crate::frame::PAGE_SIZE);
        assert!(
            align_pad <= crate::frame::PAGE_SIZE,
            "over-allocation guarantees an aligned base"
        );
        Self {
            base: base.as_u64(),
            len,
            storage,
            align_pad,
        }
    }
}

#[cfg(any(test, feature = "host-tests"))]
impl PhysMap for SimPhysMap {
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>> {
        let p = phys.as_u64();
        if p < self.base {
            return None;
        }
        let rel = usize::try_from(p - self.base).ok()?;
        let end = rel.checked_add(len)?;
        if end > self.len {
            return None;
        }
        let off = rel.checked_add(self.align_pad)?;
        // Mirror the host-model pointer pattern used by `DmaPool` and
        // `MmioMap`: the backing `Vec` is allocated once and never
        // resized, so a pointer into it stays valid for the
        // simulator's lifetime. Writes through it are sound because
        // the pool's slot bitmap proves the covered bytes alias
        // nothing else live.
        let ptr = self.storage.as_ptr().wrapping_add(off).cast_mut();
        NonNull::new(ptr)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::frame::PAGE_SIZE;

    #[test]
    fn direct_identity_round_trips_low_address() {
        let map = DirectPhysMap::identity(0x1_0000_0000);
        let p = map
            .translate(PhysAddr::new(0x1000), 0x1000)
            .expect("mapped");
        assert_eq!(p.as_ptr() as u64, 0x1000);
    }

    #[test]
    fn direct_rejects_range_past_limit() {
        let map = DirectPhysMap::identity(0x2000);
        assert!(map.translate(PhysAddr::new(0x1000), 0x1001).is_none());
        assert!(map.translate(PhysAddr::new(0x2000), 1).is_none());
    }

    #[test]
    fn direct_offset_shifts_pointer() {
        let map = DirectPhysMap::new(0x1_0000_0000, 0x2_0000_0000);
        let p = map.translate(PhysAddr::new(0x4000), 8).expect("mapped");
        assert_eq!(p.as_ptr() as u64, 0x1_0000_4000);
    }

    #[test]
    fn sim_aliases_writes_at_a_physical_address() {
        let base = PhysAddr::new(PAGE_SIZE as u64 * 16);
        let sim = SimPhysMap::new(base, 4 * PAGE_SIZE);
        let target = PhysAddr::new(base.as_u64() + PAGE_SIZE as u64);
        let a = sim.translate(target, 4).expect("a");
        let b = sim.translate(target, 4).expect("b");
        // SAFETY: both pointers name the same in-bounds simulated
        // frame; the simulator outlives this test body.
        unsafe {
            a.as_ptr().write(0xAB);
            assert_eq!(b.as_ptr().read(), 0xAB);
        }
    }

    #[test]
    fn sim_is_page_aligned_at_base() {
        let base = PhysAddr::new(0xFEBD_0000);
        let sim = SimPhysMap::new(base, PAGE_SIZE);
        let p = sim.translate(base, 4).expect("mapped");
        assert_eq!(p.as_ptr() as usize % PAGE_SIZE, 0);
    }

    #[test]
    fn sim_rejects_below_base_and_past_end() {
        let base = PhysAddr::new(PAGE_SIZE as u64 * 16);
        let sim = SimPhysMap::new(base, PAGE_SIZE);
        assert!(sim.translate(PhysAddr::new(0), 1).is_none());
        assert!(sim
            .translate(PhysAddr::new(base.as_u64()), PAGE_SIZE + 1)
            .is_none());
    }
}
