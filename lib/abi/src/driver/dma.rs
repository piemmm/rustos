//! Owned DMA region ABI types (`abi-v1`).
//!
//! These types are the host↔driver ABI seam for DMA-able memory.
//! They live in `lib/abi` (rather than in `drivers/bus/virtio`)
//! because every driver-class trait that a host implements — and the
//! [`DriverHost::virtio_host`] accessor on the host trait itself —
//! has to be able to name them without pulling in `drivers/bus/*`.
//! That would invert the dependency direction and violate
//! `AGENTS.md` §3.
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

/// Type-erased free shim called from [`DmaSlab::drop`].
///
/// * `pool` is the opaque pointer the slab was built with;
/// * `slot` and `len` are the slab's bookkeeping.
///
/// # Safety
///
/// The shim is `unsafe` because [`DmaSlab::drop`] (the only caller)
/// must guarantee `pool` still points at the originating pool, which
/// the pool enforces by outliving every slab it minted
/// (`AGENTS.md` §4).
pub type SlabFreeFn = unsafe fn(pool: *const (), slot: usize, len: usize);

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
}

// SAFETY: A `DmaSlab` is a tagged pointer to a disjoint range of
// pool storage. `NonNull<u8>` and `*const ()` are `!Send` by default
// because the compiler does not know whether the pointee permits
// cross-thread access. We assert `Send` because (i) the pool's slot
// bitmap guarantees only this slab observes its byte range; (ii) the
// pool implementations in `rustos-kernel-mem` are themselves `Send`
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
        }
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
            // pool's bitmap. `Drop::drop` runs exactly once.
            unsafe { f(self.pool_ptr, self.slot, self.len) }
        }
    }
}
