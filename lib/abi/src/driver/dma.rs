//! Owned DMA region ABI types (`abi-v1`).
//!
//! These types are the host↔driver ABI seam for DMA-able memory.
//! They live in `lib/abi` (rather than in `drivers/bus/virtio`)
//! because every driver-class trait that a host implements — and the
//! [`DriverHost::virtio_host`] accessor on the host trait itself —
//! has to be able to name them without pulling in `drivers/bus/*`.
//! That would invert the dependency direction and violate.
//!
//! The [`PoolId`], [`SlabFreeFn`], and [`DmaSlab`] surface is
//! identical to the surface previously exposed from
//! `drivers/bus/virtio::dma`; that crate now re-exports these
//! definitions for source compatibility. The `BounceBuffer` wrapper
//! is virtio-specific and stays in the virtio crate.
//!
//! No allocation: the crate-wide `no_std` and no-allocation
//! invariants documented in `lib/abi`'s crate-root rustdoc are
//! preserved. The unit tests for these types live in
//! `drivers/bus/virtio/src/dma.rs` (which is permitted to depend on
//! `alloc`) and exercise the surface through the re-export.
//!
//! [`DriverHost::virtio_host`]: super::DriverHost::virtio_host

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::DriverError;

/// Host-side facility that mints owned, device-visible DMA regions.
///
/// This is the bus-neutral DMA-allocation seam, a sibling of
/// [`MmioMapper`](super::mmio::MmioMapper): any driver that has to hand the
/// hardware a physically-addressable buffer — a bus driver staging an xHCI
/// device-context / transfer ring, a virtio driver staging a split
/// virtqueue — obtains a [`DmaSlab`] through it. It lives in `lib/abi` so the
/// host accessor [`DriverHost::dma_host`](super::DriverHost::dma_host) can
/// name it without inverting the dependency direction, and
/// it is *separate from* virtio so a non-virtio bus driver never has to reach
/// through a virtio-shaped trait to allocate DMA. [`VirtioHost`] extends it
/// (`VirtioHost: DmaHost`) so a virtio host is also a DMA host without
/// duplicating the allocation contract.
///
/// [`VirtioHost`]: super::virtio::VirtioHost
pub trait DmaHost {
    /// Allocate a contiguous, device-visible, zero-initialised DMA region.
    ///
    /// The returned [`DmaSlab`] is owned by the caller; it carries the pool
    /// id and slot it was minted from so the host's drop path can reclaim
    /// the slot. The bytes are zero-initialised so a driver can publish the
    /// slab to a device without first clearing leftover bytes from another
    /// transaction (defence in depth zero-on-free).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `size == 0`.
    /// * [`DriverError::LengthOutOfRange`] if the host exhausts its DMA pool.
    /// * [`DriverError::PermissionDenied`] if the calling task is missing the
    ///   capability the host enforces at allocation time (the kernel host
    ///   gates on [`CapabilityId::MEM_DMA`](crate::CapabilityId::MEM_DMA)).
    ///
    /// # Capabilities
    ///
    /// None directly at the trait level; the host enforces its own DMA-pool
    /// quota and per-task capability check at allocation time ("per-process heaps" + "fail closed").
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError>;
}

/// Stable identifier of a DMA pool.
///
/// The driver code is opaque to pool internals; the identifier
/// exists so a [`DmaSlab`] can be tied to the pool that minted it.
/// [`PoolId::MOCK`] is reserved for the in-process mock host shipped
/// by the virtio bus crate's test harness.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub struct PoolId(u64);

impl PoolId {
    /// Reserved identifier for the in-process mock host shipped by
    /// `drivers/bus/virtio::MockHost`.
    pub const MOCK: Self = Self(0);

    /// Construct an identifier from its raw `u64`.
    ///
    /// Intended for the kernel host wiring (Stage 4.D Item 0); the
    /// value must be globally unique within the running system and
    /// must not be zero ([`PoolId::MOCK`] is reserved).
    #[must_use]
    pub const fn from_raw(id: u64) -> Self {
        Self(id)
    }

    /// Raw `u64` representation.
    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// Allocate a fresh, process-unique [`PoolId`] that is **not**
    /// [`PoolId::MOCK`].
    ///
    /// Intended for the kernel host wiring (Stage 4.D Item 0) and
    /// for unit tests that exercise multiple independent pools.
    #[must_use]
    pub fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Cache-maintenance shim a [`DmaSlab`] invokes to keep a **non-coherent**
/// DMA master and the CPU caches in sync.
///
/// `base` is the CPU-virtual address of the affected sub-range and `len`
/// its byte length. The shim must clean **and** invalidate that range to
/// the point of coherency (a `dc civac`-class operation plus a barrier),
/// so that bytes the CPU just wrote are visible to the device and bytes
/// the device just wrote are visible to the CPU. It performs cache
/// maintenance only — it never dereferences the range — so it is a safe
/// `fn`. A slab minted without one (coherent interconnect, or the
/// in-process mock host) skips maintenance entirely.
pub type SlabCoherencyFn = fn(base: *const u8, len: usize);

/// Type-erased free shim called from [`DmaSlab::drop`].
///
/// * `pool` is the opaque pointer the slab was built with;
/// * `cpu` is the slab's CPU base pointer (the user virtual base the
///   allocator returned) — the key a syscall-backed pool (the user-space
///   `RtDriverHost`) frees the buffer by;
/// * `slot` and `len` are the slab's bookkeeping (a slot-bitmap pool such as
///   the in-kernel host frees by `slot`, ignoring `cpu`).
///
/// # Safety
///
/// The shim is `unsafe` because [`DmaSlab::drop`] (the only caller)
/// must guarantee `pool` still points at the originating pool, which
/// the pool enforces by outliving every slab it minted.
pub type SlabFreeFn = unsafe fn(pool: *const (), cpu: NonNull<u8>, slot: usize, len: usize);

/// Owned, device-visible DMA region.
///
/// # Invariants
///
/// * `phys` is the device-visible base address that, when programmed
///   into a virtio descriptor's `addr` field, points at the same
///   bytes as `ptr[0]`.
/// * `ptr` is a non-null, aligned pointer to a buffer of exactly
///   `len` bytes. Disjointness with every other live slab from the
///   same pool is witnessed by the pool's slot bitmap (one slot ↔
///   one slab).
/// * If `free_fn` is `Some`, dropping the slab calls
///   `free_fn(pool_ptr, slot, len)` exactly once; the pool reclaims
///   the slot. If `free_fn` is `None` (the mock-host case), drop is
///   a no-op and the bytes leak (the leak contract).
#[derive(Debug)]
pub struct DmaSlab {
    phys: u64,
    ptr: NonNull<u8>,
    len: usize,
    pool_id: PoolId,
    slot: usize,
    pool_ptr: *const (),
    free_fn: Option<SlabFreeFn>,
    coherency: Option<SlabCoherencyFn>,
}

// SAFETY: A `DmaSlab` is a tagged pointer to a disjoint range of
// pool storage. `NonNull<u8>` and `*const ()` are `!Send` by default
// because the compiler does not know whether the pointee permits
// cross-thread access. We assert `Send` because (i) the pool's slot
// bitmap guarantees only this slab observes its byte range; (ii) the
// pool implementations in `tairix-kernel-mem` are themselves `Send`
// (their internal storage is behind a per-process address space);
// and (iii) the in-process mock-host mint uses `Box::leak`, which
// yields `'static` storage safe to send between test threads.
// No `Sync`: the inner bytes are mutably aliased through
// `as_bytes_mut` and concurrent access through two threads would
// race.
unsafe impl Send for DmaSlab {}

impl DmaSlab {
    /// Construct a [`DmaSlab`] whose drop is a no-op.
    ///
    /// Used by the in-process mock host (which keeps its
    /// `Box::leak` storage strategy) and by unit tests that wrap a
    /// borrowed `&mut [u8]` for the duration of the test function.
    ///
    /// # Safety
    ///
    /// * `ptr` must point at a buffer of exactly `len` bytes that
    ///   remains valid for the entire lifetime of the returned slab
    ///   and is not aliased by any other live reference.
    /// * `phys` must be the device-visible base address of `ptr[0]`.
    #[must_use]
    pub unsafe fn from_leaked(
        phys: u64,
        ptr: NonNull<u8>,
        len: usize,
        pool_id: PoolId,
        slot: usize,
    ) -> Self {
        Self {
            phys,
            ptr,
            len,
            pool_id,
            slot,
            pool_ptr: core::ptr::null(),
            free_fn: None,
            coherency: None,
        }
    }

    /// Construct a [`DmaSlab`] that reclaims its slot via `free_fn`
    /// on drop.
    ///
    /// Intended for the kernel host (Stage 4.D Item 0).
    ///
    /// # Safety
    ///
    /// * `ptr` must point at a buffer of exactly `len` bytes whose
    ///   disjointness is witnessed by the pool's slot bitmap (one
    ///   slot ↔ one slab).
    /// * `pool_ptr` must remain valid until `free_fn` is invoked
    ///   from [`Self::drop`]; the pool must outlive the slab.
    /// * `phys` must be the device-visible base address of `ptr[0]`.
    #[must_use]
    pub unsafe fn from_pool(
        phys: u64,
        ptr: NonNull<u8>,
        len: usize,
        pool_id: PoolId,
        slot: usize,
        pool_ptr: *const (),
        free_fn: SlabFreeFn,
    ) -> Self {
        Self {
            phys,
            ptr,
            len,
            pool_id,
            slot,
            pool_ptr,
            free_fn: Some(free_fn),
            coherency: None,
        }
    }

    /// Attach a [`SlabCoherencyFn`] for a **non-coherent** DMA master.
    ///
    /// On an interconnect where the device does not snoop the CPU caches
    /// (e.g. the BCM2711 PCIe root complex), the host wires the arch cache
    /// clean/invalidate primitive here so [`Self::sync_range`] can bracket
    /// every CPU-side publish/consume. Without it [`Self::sync_range`] is a
    /// no-op, the correct behaviour for a coherent interconnect or the
    /// in-process mock host.
    #[must_use]
    pub fn with_coherency(mut self, coherency: SlabCoherencyFn) -> Self {
        self.coherency = Some(coherency);
        self
    }

    /// Clean **and** invalidate the byte range `[offset, offset + len)` to
    /// the point of coherency through the attached [`SlabCoherencyFn`].
    ///
    /// The owner of DMA publication ordering on a non-coherent
    /// interconnect calls this **after** writing bytes the device will
    /// read (so they reach memory before the doorbell) and **before**
    /// reading bytes the device wrote (so the CPU does not see a stale
    /// cached copy). It is a no-op when no shim is attached, when `len` is
    /// zero, or — failing closed — when the range falls
    /// outside the region.
    pub fn sync_range(&self, offset: usize, len: usize) {
        let Some(maintain) = self.coherency else {
            return;
        };
        if len == 0 {
            return;
        }
        let Some(end) = offset.checked_add(len) else {
            return;
        };
        if end > self.len {
            return;
        }
        // SAFETY: `ptr` points at exactly `self.len` valid bytes by the
        // construction invariants, and `offset + len <= self.len`, so
        // `ptr + offset` addresses exactly `len` in-bounds bytes. The shim
        // performs cache maintenance over that range only; it never reads
        // or writes the bytes, so no aliasing rule is involved.
        let base = unsafe { self.ptr.as_ptr().add(offset).cast_const() };
        maintain(base, len);
    }

    /// Device-visible base address of this region.
    #[must_use]
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Byte length of this region.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` iff the region is zero-length.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Identifier of the pool that minted this slab.
    #[must_use]
    pub fn pool_id(&self) -> PoolId {
        self.pool_id
    }

    /// Slot index within the originating pool.
    #[must_use]
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// Immutable byte view of the region (CPU-side).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is non-null and points at exactly `len`
        // bytes by the construction invariants. Disjointness with
        // every other live slab from the same pool is witnessed by
        // the pool's slot bitmap (one slot ↔ one slab), so no
        // other live reference aliases these bytes.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr().cast_const(), self.len) }
    }

    /// Mutable byte view of the region (CPU-side).
    #[must_use]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in [`Self::as_bytes`]. The `&mut self` borrow
        // upgrades exclusivity from "no other live `&` to these
        // bytes" (slot-bitmap disjointness across slabs) to "no
        // other live reference to these bytes" (this is the only
        // slab carrying this slot, and Rust's borrow checker
        // serialises every `&mut [u8]` derived from a given slab).
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for DmaSlab {
    fn drop(&mut self) {
        if let Some(f) = self.free_fn {
            // SAFETY: at construction the caller of `from_pool`
            // proved that `pool_ptr` outlives this slab and that
            // `(slot, len)` is the slab's exclusive slot in the
            // pool's bitmap. `self.ptr` is this slab's CPU base, the
            // key a syscall-backed pool frees by. `Drop::drop` runs
            // exactly once.
            unsafe { f(self.pool_ptr, self.ptr, self.slot, self.len) }
        }
    }
}
