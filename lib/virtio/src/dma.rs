//! Owned DMA region abstraction and bounce-buffer wrapper.
//!
//! [`DmaSlab`] is the owned representation of a contiguous, device-
//! visible memory range whose ownership has been transferred to the
//! driver code by the host. It replaces the
//! previously-borrowed `DmaRegion<'a>` API: the driver code in
//! `drivers/storage/virtio_blk` and `drivers/network/virtio_net`
//! holds up to three live DMA regions concurrently (descriptor
//! table + avail ring + used ring in [`crate::SplitQueue`]; header +
//! payload + status in `VirtioBlk::run_request`), which is
//! incompatible with a borrow-of-the-pool API because a single
//! `&mut DmaPool` can lend out only one mutable slice at a time.
//!
//! The owned [`DmaSlab`] carries the disjoint-slot invariant in its
//! [`PoolId`] / `slot` fields and reaches the bytes through a
//! `NonNull<u8>` whose validity is witnessed by the pool's slot
//! bitmap (one slot ↔ one slab).
//!
//! [`BounceBuffer`] is the safe wrapper that holds a caller payload
//! inside a host-allocated [`DmaSlab`] for the lifetime of a single
//! virtio transaction. It implements the "sensitive" contract on
//! drop: when the caller declares the payload sensitive, the wrapper
//! zeroes the staging bytes before the slab is dropped, so the
//! host's per-pool slot reclaim never observes residual credentials
//! (zero-on-free).

use tairix_abi::driver::BufferClass;
use tairix_abi::DriverError;

// `PoolId`, `SlabFreeFn`, and `DmaSlab` moved into `lib/abi` at Stage
// 4.D Item 0-tail so the host trait (`tairix_abi::DriverHost`) can
// name them without inverting the dependency direction. Their unit tests stay in this module against the
// re-export so they keep exercising the same call sites and continue
// to enjoy `alloc` access (`lib/abi` is no-alloc).
pub use tairix_abi::driver::{DmaSlab, PoolId, SlabFreeFn};

/// Zero every byte of `slab`: the one scrub staging that held a sensitive
/// payload gets once the device has handed it back.
pub fn scrub(slab: &mut DmaSlab) {
    slab.as_bytes_mut().fill(0);
}

/// Bounce buffer wrapping an owned [`DmaSlab`] for a single virtio
/// transaction.
///
/// Built by a driver immediately before staging a payload into
/// device-visible memory. On drop the wrapper zeroes the staging
/// bytes iff [`BufferClass::Sensitive`] was declared, satisfying the
/// zero-on-free contract on `Block::*_with_class` /
/// `Net::*_with_class`.
#[derive(Debug)]
pub struct BounceBuffer {
    slab: DmaSlab,
    class: BufferClass,
    used: usize,
}

impl BounceBuffer {
    /// Wrap a [`DmaSlab`] into a bounce buffer.
    #[must_use]
    pub fn new(slab: DmaSlab, class: BufferClass) -> Self {
        Self {
            slab,
            class,
            used: 0,
        }
    }

    /// Sensitivity class declared at construction.
    #[must_use]
    pub fn class(&self) -> BufferClass {
        self.class
    }

    /// Bytes staged so far through [`Self::stage`].
    #[must_use]
    pub fn used(&self) -> usize {
        self.used
    }

    /// Device-visible base address of the underlying region.
    #[must_use]
    pub fn phys(&self) -> u64 {
        self.slab.phys()
    }

    /// Capacity of the underlying region.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slab.len()
    }

    /// Immutable CPU-side view of the staged bytes.
    #[must_use]
    pub fn staged(&self) -> &[u8] {
        &self.slab.as_bytes()[..self.used]
    }

    /// Mutable CPU-side view of the staged bytes.
    #[must_use]
    pub fn staged_mut(&mut self) -> &mut [u8] {
        &mut self.slab.as_bytes_mut()[..self.used]
    }

    /// Copy `payload` into the bounce buffer and remember the
    /// length.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] if `payload` does not fit the
    /// staging window.
    pub fn stage(&mut self, payload: &[u8]) -> Result<(), DriverError> {
        if payload.len() > self.slab.len() {
            return Err(DriverError::BufferTooSmall);
        }
        self.slab.as_bytes_mut()[..payload.len()].copy_from_slice(payload);
        self.used = payload.len();
        Ok(())
    }

    /// Record that the device wrote `n` bytes into the staging
    /// buffer, and surface them through an immutable view.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] if `n` exceeds the underlying
    /// region's capacity.
    pub fn fill_from_device(&mut self, n: usize) -> Result<&[u8], DriverError> {
        if n > self.slab.len() {
            return Err(DriverError::BufferTooSmall);
        }
        self.used = n;
        Ok(&self.slab.as_bytes()[..n])
    }

    /// Mutable CPU-side view of the *full* underlying region
    /// (not limited to `used`). Used by drivers that pre-grant the
    /// device the maximum frame size.
    #[must_use]
    pub fn full_region_mut(&mut self) -> &mut [u8] {
        self.slab.as_bytes_mut()
    }

    /// Consume the bounce buffer, returning the underlying
    /// [`DmaSlab`] after scrubbing if the declared class was
    /// sensitive.
    #[must_use]
    pub fn into_slab(mut self) -> DmaSlab {
        self.scrub_if_sensitive();
        // Re-classify so the `ManuallyDrop`-extracted slab does not
        // get scrubbed a second time when its own destructor (if
        // any) runs.
        self.class = BufferClass::NonSensitive;
        let md = core::mem::ManuallyDrop::new(self);
        // SAFETY: `md` is consumed and never used again; we move
        // `slab` out by a single byte-copy of the field. The
        // remaining fields (`class`, `used`) are `Copy` and have no
        // destructors that need running.
        unsafe { core::ptr::read(&raw const md.slab) }
    }

    /// Never return the buffer to its pool, nor scrub it: the device may still
    /// be reading it. The kernel zeroes it when the process's memory is
    /// finally reclaimed.
    pub fn withhold(&mut self) {
        self.slab.withhold();
        self.class = BufferClass::NonSensitive;
    }

    /// Consume the bounce buffer once its request has ended, as
    /// [`Self::into_slab`] does — unless `device_holds_it`, the request having
    /// been abandoned with the device still holding it
    /// ([`crate::RequestQueue::is_abandoned`]). Then the slab comes back
    /// unscrubbed: a scrub now could overwrite a payload the device has yet to
    /// read, and a write it has yet to make would land after it, so whoever
    /// takes the buffer back from the device scrubs it then.
    #[must_use]
    pub fn into_slab_after(mut self, device_holds_it: bool) -> DmaSlab {
        if device_holds_it {
            self.class = BufferClass::NonSensitive;
        }
        self.into_slab()
    }

    fn scrub_if_sensitive(&mut self) {
        if self.class.is_sensitive() {
            scrub(&mut self.slab);
            self.used = 0;
        }
    }
}

impl Drop for BounceBuffer {
    fn drop(&mut self) {
        self.scrub_if_sensitive();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::ptr::NonNull;

    /// Shared bookkeeping for [`dma_slab_drop_invokes_free_fn`].
    /// Hoisted out of the test body so that `free_shim` does not
    /// appear "after statements" inside the `#[test]` function
    /// (`clippy::items-after-statements`).
    mod drop_test_state {
        use core::sync::atomic::{AtomicUsize, Ordering};
        pub(super) static FREED: AtomicUsize = AtomicUsize::new(0);
        pub(super) static LAST_SLOT: AtomicUsize = AtomicUsize::new(usize::MAX);
        pub(super) static LAST_LEN: AtomicUsize = AtomicUsize::new(0);
        /// `SlabFreeFn`-compatible shim that records its arguments
        /// into the file-scope atomics above.
        ///
        /// # Safety
        ///
        /// Matches [`super::super::SlabFreeFn`]: the caller (only
        /// [`super::super::DmaSlab::drop`]) guarantees the call
        /// happens once with the slab's bookkeeping.
        pub(super) unsafe fn free_shim(
            _pool: *const (),
            _cpu: core::ptr::NonNull<u8>,
            slot: usize,
            len: usize,
        ) {
            FREED.fetch_add(1, Ordering::SeqCst);
            LAST_SLOT.store(slot, Ordering::SeqCst);
            LAST_LEN.store(len, Ordering::SeqCst);
        }
    }

    /// A slab lent over `storage`, whose drop frees nothing, so the
    /// interpreter accounts for every byte a test touches.
    ///
    /// # Safety
    ///
    /// `storage` must outlive the slab and be reached only through it until
    /// the slab is gone.
    unsafe fn lent_slab(storage: &mut [u8], pool_id: PoolId, slot: usize) -> DmaSlab {
        let len = storage.len();
        let phys = storage.as_ptr() as u64;
        // SAFETY: per the function contract.
        unsafe { DmaSlab::from_leaked(phys, NonNull::from(storage).cast(), len, pool_id, slot) }
    }

    /// File-scope recorder for the [`DmaSlab::sync_range`] hook. A
    /// [`super::super::SlabCoherencyFn`] is a bare `fn` pointer (no
    /// capture), so the observed `(base, len)` is published through atomics.
    /// Used by a single test (`dma_slab_sync_range_*`) so no cross-test race
    /// on these statics is possible (no flaky tests).
    mod coherency_test_state {
        use core::sync::atomic::{AtomicUsize, Ordering};
        pub(super) static CALLS: AtomicUsize = AtomicUsize::new(0);
        pub(super) static LAST_BASE: AtomicUsize = AtomicUsize::new(0);
        pub(super) static LAST_LEN: AtomicUsize = AtomicUsize::new(0);

        /// A [`super::super::SlabCoherencyFn`]: record the maintained range.
        pub(super) fn record(base: *const u8, len: usize) {
            CALLS.fetch_add(1, Ordering::SeqCst);
            LAST_BASE.store(base as usize, Ordering::SeqCst);
            LAST_LEN.store(len, Ordering::SeqCst);
        }
    }

    #[test]
    fn dma_slab_sync_range_brackets_only_in_bounds_ranges_through_the_hook() {
        use coherency_test_state as rec;
        use core::sync::atomic::Ordering;

        let mut storage = [0u8; 64];
        let mut plain_storage = [0u8; 64];
        // SAFETY: both stores outlive their slabs, reached only through them.
        let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) }.with_coherency(rec::record);
        let base = slab.as_bytes().as_ptr() as usize;

        // An in-bounds range is maintained at the right address and length.
        slab.sync_range(16, 8);
        assert_eq!(rec::CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(rec::LAST_BASE.load(Ordering::SeqCst), base + 16);
        assert_eq!(rec::LAST_LEN.load(Ordering::SeqCst), 8);

        // A zero-length request and an out-of-range request both fail closed
        // to a no-op: the hook is not invoked again.
        slab.sync_range(0, 0);
        slab.sync_range(60, 8);
        slab.sync_range(usize::MAX, 1);
        assert_eq!(rec::CALLS.load(Ordering::SeqCst), 1);

        // A slab minted without a coherency shim never calls the hook
        // (coherent interconnect / mock host).
        // SAFETY: as above.
        let plain = unsafe { lent_slab(&mut plain_storage, PoolId::MOCK, 1) };
        plain.sync_range(0, 16);
        assert_eq!(rec::CALLS.load(Ordering::SeqCst), 1);
        assert!(slab.needs_cache_maintenance());
        assert!(!plain.needs_cache_maintenance());
    }

    #[test]
    fn a_withheld_bounce_buffer_is_neither_freed_nor_scrubbed() {
        /// Counts a free into the `Cell` its pool pointer names.
        ///
        /// # Safety
        ///
        /// `pool` must point at a live `Cell<usize>`.
        unsafe fn count(pool: *const (), _cpu: NonNull<u8>, _slot: usize, _len: usize) {
            // SAFETY: per the function contract.
            let frees = unsafe { &*pool.cast::<core::cell::Cell<usize>>() };
            frees.set(frees.get() + 1);
        }

        // The device may still be reading it; the kernel zeroes it later.
        let frees = core::cell::Cell::new(0usize);
        let mut storage = vec![0xABu8; 32];
        let ptr = NonNull::new(storage.as_mut_ptr()).expect("non-null");
        // SAFETY: `storage` holds 32 bytes, outlives the slab, and is reached
        // only through it until the slab is gone; `frees` outlives it too.
        let slab = unsafe {
            DmaSlab::from_pool(
                0,
                ptr,
                32,
                PoolId::MOCK,
                7,
                core::ptr::from_ref(&frees).cast(),
                count,
            )
        };
        let mut bounce = BounceBuffer::new(slab, BufferClass::Sensitive);
        bounce.withhold();
        drop(bounce);
        assert_eq!(frees.get(), 0);
        assert!(storage.iter().all(|b| *b == 0xAB));
    }

    #[test]
    fn dma_slab_round_trip() {
        let mut storage = [0u8; 16];
        // SAFETY: `storage` outlives the slab, reached only through it.
        let mut slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
        assert_eq!(slab.len(), 16);
        assert!(!slab.is_empty());
        assert_eq!(slab.pool_id(), PoolId::MOCK);
        assert_eq!(slab.slot(), 0);
        slab.as_bytes_mut()[0] = 0xAA;
        assert_eq!(slab.as_bytes()[0], 0xAA);
    }

    #[test]
    fn dma_slab_three_simultaneous_disjoint_writes() {
        // Disjointness invariant: three slabs minted from the same
        // logical pool, each with a distinct slot, can be held with
        // three simultaneously-live `&mut [u8]`s. We write a
        // distinct pattern to each and observe no cross-talk.
        let (mut a_storage, mut b_storage, mut c_storage) = ([0u8; 8], [0u8; 8], [0u8; 8]);
        // SAFETY: each store outlives its slab, reached only through it.
        let (mut a, mut b, mut c) = unsafe {
            (
                lent_slab(&mut a_storage, PoolId::MOCK, 0),
                lent_slab(&mut b_storage, PoolId::MOCK, 1),
                lent_slab(&mut c_storage, PoolId::MOCK, 2),
            )
        };
        let a_bytes = a.as_bytes_mut();
        let b_bytes = b.as_bytes_mut();
        let c_bytes = c.as_bytes_mut();
        a_bytes.copy_from_slice(&[0xAAu8; 8]);
        b_bytes.copy_from_slice(&[0xBBu8; 8]);
        c_bytes.copy_from_slice(&[0xCCu8; 8]);
        assert_eq!(a_bytes, &[0xAAu8; 8]);
        assert_eq!(b_bytes, &[0xBBu8; 8]);
        assert_eq!(c_bytes, &[0xCCu8; 8]);
    }

    #[test]
    fn dma_slab_drop_invokes_free_fn() {
        // drop-frees-pool: a slab built with a real `free_fn`
        // reaches its pool exactly once on drop. We use file-scope
        // `AtomicUsize`s as the test-only "pool".
        use core::sync::atomic::Ordering;
        // Reset between runs.
        drop_test_state::FREED.store(0, Ordering::SeqCst);
        drop_test_state::LAST_SLOT.store(usize::MAX, Ordering::SeqCst);
        drop_test_state::LAST_LEN.store(0, Ordering::SeqCst);
        let mut storage = [0u8; 32];
        let phys = storage.as_ptr() as u64;
        let ptr = NonNull::from(&mut storage).cast::<u8>();
        let pool_id = PoolId::fresh();
        {
            // SAFETY: pool_ptr is null but the shim ignores it; `storage`
            // outlives the slab and is reached only through it meanwhile.
            let slab = unsafe {
                DmaSlab::from_pool(
                    phys,
                    ptr,
                    32,
                    pool_id,
                    7,
                    core::ptr::null(),
                    drop_test_state::free_shim,
                )
            };
            assert_eq!(drop_test_state::FREED.load(Ordering::SeqCst), 0);
            drop(slab);
        }
        assert_eq!(drop_test_state::FREED.load(Ordering::SeqCst), 1);
        assert_eq!(drop_test_state::LAST_SLOT.load(Ordering::SeqCst), 7);
        assert_eq!(drop_test_state::LAST_LEN.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn dma_slab_pool_id_distinguishes_pools() {
        // pool-id rejection across pools: two pools mint slabs with
        // overlapping slot numbers; the `pool_id` field is the
        // discriminator a future bookkeeping consumer relies on.
        let id_a = PoolId::fresh();
        let id_b = PoolId::fresh();
        assert_ne!(id_a, id_b);
        assert_ne!(id_a, PoolId::MOCK);
        let (mut a_storage, mut b_storage) = ([0u8; 4], [0u8; 4]);
        // SAFETY: each store outlives its slab, reached only through it.
        let (a, b) = unsafe {
            (
                lent_slab(&mut a_storage, id_a, 0),
                lent_slab(&mut b_storage, id_b, 0),
            )
        };
        assert_ne!(a.pool_id(), b.pool_id());
        // Same slot index, different pool — disambiguated only by
        // the `pool_id` field. A consumer that mistakenly tries to
        // return slab `a` to pool `id_b` (the canonical "wrong
        // pool" mistake) is expected to compare
        // `slab.pool_id() == pool.id()` before calling its free
        // path. The check is a single equality comparison.
        assert_eq!(a.slot(), b.slot());
        assert_ne!(a.pool_id(), id_b);
        assert_ne!(b.pool_id(), id_a);
    }

    #[test]
    fn bounce_buffer_stages_payload() {
        let mut storage = [0u8; 32];
        // SAFETY: `storage` outlives the slab, reached only through it.
        let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
        let phys = slab.phys();
        let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
        assert!(bb.stage(&[1, 2, 3, 4]).is_ok());
        assert_eq!(bb.used(), 4);
        assert_eq!(bb.staged(), &[1, 2, 3, 4]);
        assert_eq!(bb.phys(), phys);
    }

    #[test]
    fn bounce_buffer_rejects_overflow() {
        let mut storage = [0u8; 4];
        // SAFETY: `storage` outlives the slab, reached only through it.
        let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
        let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
        assert_eq!(bb.stage(&[0u8; 8]), Err(DriverError::BufferTooSmall));
    }

    #[test]
    fn bounce_buffer_scrubs_on_sensitive_drop() {
        let mut storage = [0u8; 16];
        {
            // SAFETY: `storage` outlives the slab, reached only through it.
            let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
            let mut bb = BounceBuffer::new(slab, BufferClass::Sensitive);
            bb.stage(&[0xAA; 16]).unwrap();
            assert_eq!(bb.staged(), &[0xAA; 16]);
        }
        assert!(storage.iter().all(|b| *b == 0));
    }

    #[test]
    fn bounce_buffer_preserves_on_non_sensitive_drop() {
        let mut storage = [0u8; 16];
        {
            // SAFETY: `storage` outlives the slab, reached only through it.
            let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
            let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
            bb.stage(&[0xBB; 16]).unwrap();
        }
        assert!(storage.iter().all(|b| *b == 0xBB));
    }

    #[test]
    fn bounce_buffer_into_slab_scrubs_on_sensitive() {
        let mut storage = [0u8; 12];
        // SAFETY: `storage` outlives the slab, reached only through it.
        let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
        let mut bb = BounceBuffer::new(slab, BufferClass::Sensitive);
        bb.stage(&[0x77; 12]).unwrap();
        let returned = bb.into_slab();
        assert_eq!(returned.len(), 12);
        assert!(returned.as_bytes().iter().all(|b| *b == 0));
    }

    #[test]
    fn a_buffer_the_device_still_holds_comes_back_unscrubbed() {
        let mut storage = [0u8; 8];
        // SAFETY: `storage` outlives the slab, reached only through it.
        let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
        let mut bb = BounceBuffer::new(slab, BufferClass::Sensitive);
        bb.stage(&[0x5A; 8]).unwrap();
        let held = bb.into_slab_after(true);
        assert!(
            held.as_bytes().iter().all(|b| *b == 0x5A),
            "the device may yet read it"
        );
        let mut bb = BounceBuffer::new(held, BufferClass::Sensitive);
        bb.stage(&[0x5A; 8]).unwrap();
        let returned = bb.into_slab_after(false);
        assert!(returned.as_bytes().iter().all(|b| *b == 0));
    }

    #[test]
    fn scrub_zeroes_the_whole_slab() {
        let mut storage = [0xEEu8; 8];
        // SAFETY: `storage` outlives the slab, reached only through it.
        let mut slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
        scrub(&mut slab);
        assert!(slab.as_bytes().iter().all(|b| *b == 0));
    }

    #[test]
    fn fill_from_device_updates_used() {
        let mut storage = [0u8; 32];
        // SAFETY: `storage` outlives the slab, reached only through it.
        let slab = unsafe { lent_slab(&mut storage, PoolId::MOCK, 0) };
        let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
        bb.full_region_mut()[..5].copy_from_slice(b"hello");
        let view = bb.fill_from_device(5).expect("fit");
        assert_eq!(view, b"hello");
        assert_eq!(bb.used(), 5);
        assert_eq!(bb.fill_from_device(64), Err(DriverError::BufferTooSmall));
    }
}
