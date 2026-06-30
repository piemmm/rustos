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

use rustos_abi::driver::BufferClass;

// `PoolId`, `SlabFreeFn`, and `DmaSlab` moved into `lib/abi` at Stage
// 4.D Item 0-tail so the host trait (`rustos_abi::DriverHost`) can
// name them without inverting the dependency direction. Their unit tests stay in this module against the
// re-export so they keep exercising the same call sites and continue
// to enjoy `alloc` access (`lib/abi` is no-alloc).
pub use rustos_abi::driver::{DmaSlab, PoolId, SlabFreeFn};

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
    /// Returns `Err(())` if `payload` does not fit. Callers
    /// translate this into the appropriate `DriverError`.
    pub fn stage(&mut self, payload: &[u8]) -> Result<(), ()> {
        if payload.len() > self.slab.len() {
            return Err(());
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
    /// Returns `Err(())` if `n` exceeds the underlying region's
    /// capacity.
    pub fn fill_from_device(&mut self, n: usize) -> Result<&[u8], ()> {
        if n > self.slab.len() {
            return Err(());
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
        unsafe { core::ptr::read(&md.slab) }
    }

    fn scrub_if_sensitive(&mut self) {
        if self.class.is_sensitive() {
            for byte in self.slab.as_bytes_mut() {
                *byte = 0;
            }
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

    /// Build a [`DmaSlab`] backed by a leaked `Vec<u8>`. Tests use
    /// this in place of the production pool to exercise the slab
    /// surface in isolation.
    fn leaked_slab(len: usize, pool_id: PoolId, slot: usize, fill: u8) -> DmaSlab {
        let storage = vec![fill; len].into_boxed_slice();
        let phys = storage.as_ptr() as u64;
        let bytes: &'static mut [u8] = alloc::boxed::Box::leak(storage);
        let ptr = NonNull::new(bytes.as_mut_ptr()).expect("box leak is non-null");
        // SAFETY: `Box::leak` yields a `'static` buffer of exactly
        // `len` bytes; nothing else holds a reference to it.
        unsafe { DmaSlab::from_leaked(phys, ptr, len, pool_id, slot) }
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

        let slab = leaked_slab(64, PoolId::MOCK, 0, 0).with_coherency(rec::record);
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
        let plain = leaked_slab(64, PoolId::MOCK, 1, 0);
        plain.sync_range(0, 16);
        assert_eq!(rec::CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dma_slab_round_trip() {
        let mut slab = leaked_slab(16, PoolId::MOCK, 0, 0);
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
        let mut a = leaked_slab(8, PoolId::MOCK, 0, 0);
        let mut b = leaked_slab(8, PoolId::MOCK, 1, 0);
        let mut c = leaked_slab(8, PoolId::MOCK, 2, 0);
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
        let storage = vec![0u8; 32].into_boxed_slice();
        let phys = storage.as_ptr() as u64;
        let bytes: &'static mut [u8] = alloc::boxed::Box::leak(storage);
        let ptr = NonNull::new(bytes.as_mut_ptr()).unwrap();
        let pool_id = PoolId::fresh();
        {
            // SAFETY: pool_ptr is null but the shim ignores it; the
            // bytes are leaked, so they outlive the slab.
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
        let a = leaked_slab(4, id_a, 0, 0);
        let b = leaked_slab(4, id_b, 0, 0);
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
        let slab = leaked_slab(32, PoolId::MOCK, 0, 0);
        let phys = slab.phys();
        let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
        assert!(bb.stage(&[1, 2, 3, 4]).is_ok());
        assert_eq!(bb.used(), 4);
        assert_eq!(bb.staged(), &[1, 2, 3, 4]);
        assert_eq!(bb.phys(), phys);
    }

    #[test]
    fn bounce_buffer_rejects_overflow() {
        let slab = leaked_slab(4, PoolId::MOCK, 0, 0);
        let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
        assert_eq!(bb.stage(&[0u8; 8]), Err(()));
    }

    #[test]
    fn bounce_buffer_scrubs_on_sensitive_drop() {
        let slab = leaked_slab(16, PoolId::MOCK, 0, 0);
        let phys = slab.phys();
        {
            let mut bb = BounceBuffer::new(slab, BufferClass::Sensitive);
            bb.stage(&[0xAA; 16]).unwrap();
            assert_eq!(bb.staged(), &[0xAA; 16]);
        }
        // SAFETY: the leaked `Box` keeps the bytes at `phys` alive
        // for the duration of the test process.
        let view: &[u8] = unsafe { core::slice::from_raw_parts(phys as *const u8, 16) };
        assert!(view.iter().all(|b| *b == 0));
    }

    #[test]
    fn bounce_buffer_preserves_on_non_sensitive_drop() {
        let slab = leaked_slab(16, PoolId::MOCK, 0, 0);
        let phys = slab.phys();
        {
            let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
            bb.stage(&[0xBB; 16]).unwrap();
        }
        // SAFETY: as above.
        let view: &[u8] = unsafe { core::slice::from_raw_parts(phys as *const u8, 16) };
        assert!(view.iter().all(|b| *b == 0xBB));
    }

    #[test]
    fn bounce_buffer_into_slab_scrubs_on_sensitive() {
        let slab = leaked_slab(12, PoolId::MOCK, 0, 0);
        let mut bb = BounceBuffer::new(slab, BufferClass::Sensitive);
        bb.stage(&[0x77; 12]).unwrap();
        let returned = bb.into_slab();
        assert_eq!(returned.len(), 12);
        assert!(returned.as_bytes().iter().all(|b| *b == 0));
    }

    #[test]
    fn fill_from_device_updates_used() {
        let slab = leaked_slab(32, PoolId::MOCK, 0, 0);
        let mut bb = BounceBuffer::new(slab, BufferClass::NonSensitive);
        bb.full_region_mut()[..5].copy_from_slice(b"hello");
        let view = bb.fill_from_device(5).expect("fit");
        assert_eq!(view, b"hello");
        assert_eq!(bb.used(), 5);
        assert_eq!(bb.fill_from_device(64), Err(()));
    }
}
