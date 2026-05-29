//! DMA region abstraction and bounce-buffer wrapper.
//!
//! [`DmaRegion`] is the safe representation of a contiguous, device-
//! visible memory range owned by the host. It carries both a CPU-side
//! mutable slice and the device-visible base address (`phys`). The
//! buffer's *backing store* is owned by the host implementation (the
//! kernel's per-process heap on real hardware; an `alloc::vec::Vec`
//! on the unit-test host); this type is only a *view*.
//!
//! [`BounceBuffer`] is the safe wrapper that holds a caller payload
//! inside a host-allocated [`DmaRegion`] for the lifetime of a single
//! virtio transaction. It implements the "sensitive" contract on
//! drop: when the caller declares the payload sensitive, the wrapper
//! zeroes the staging bytes before the backing region returns to the
//! host (`AGENTS.md` §4 "Zero-on-free for any allocation that ever
//! held credentials, keys, or capability tokens").

use rustos_abi::driver::BufferClass;

/// Safe view of a host-allocated DMA region.
///
/// The host (a [`crate::host::VirtioHost`]) is the sole owner of the
/// underlying memory; this struct merely borrows it so that the
/// transport / queue code can stage payload bytes through a CPU
/// slice while handing the device-visible address to the device.
///
/// # Invariants
///
/// * `phys` is the device-visible base address that, when programmed
///   into a virtio descriptor's `addr` field, points at the same
///   bytes as `bytes[0]`. On real hardware the host's allocator
///   guarantees this through identity-mapped DMA pages or an
///   IOMMU-bound contiguous mapping; in mock tests the allocator
///   uses `bytes.as_ptr() as u64`.
/// * `bytes.len()` is the byte length of the region; the queue code
///   never indexes past the slice.
#[derive(Debug)]
pub struct DmaRegion<'a> {
    phys: u64,
    bytes: &'a mut [u8],
}

impl<'a> DmaRegion<'a> {
    /// Construct a [`DmaRegion`] from a host-allocated slice and its
    /// device-visible base address.
    ///
    /// The caller — a [`crate::host::VirtioHost`] implementation —
    /// is responsible for proving the `phys ↔ bytes[0]` invariant.
    /// This constructor is the host-private hand-off point.
    #[must_use]
    pub fn from_parts(phys: u64, bytes: &'a mut [u8]) -> Self {
        Self { phys, bytes }
    }

    /// Device-visible base address of this region.
    #[must_use]
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Byte length of this region.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` iff the region is zero-length.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Immutable byte view of the region (CPU-side).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes
    }

    /// Mutable byte view of the region (CPU-side).
    #[must_use]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        self.bytes
    }
}

/// Bounce buffer wrapping a borrowed DMA region for a single virtio
/// transaction.
///
/// Built by a driver immediately before staging a payload into
/// device-visible memory. On drop the wrapper zeroes the staging
/// bytes iff [`BufferClass::Sensitive`] was declared, satisfying the
/// zero-on-free contract on `Block::*_with_class` /
/// `Net::*_with_class` (`AGENTS.md` §4).
///
/// # Why not `Drop` only?
///
/// A drop-only impl would scrub *every* buffer, which would impose
/// a measurable cost on bulk filesystem traffic and is forbidden by
/// `AGENTS.md` §2.3 ("no bloat"). The class-aware scrub is the
/// contract documented on `BufferClass::Sensitive`.
#[derive(Debug)]
pub struct BounceBuffer<'a> {
    region: DmaRegion<'a>,
    class: BufferClass,
    used: usize,
}

impl<'a> BounceBuffer<'a> {
    /// Wrap a [`DmaRegion`] into a bounce buffer.
    #[must_use]
    pub fn new(region: DmaRegion<'a>, class: BufferClass) -> Self {
        Self {
            region,
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
        self.region.phys()
    }

    /// Capacity of the underlying region.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.region.len()
    }

    /// Immutable CPU-side view of the staged bytes.
    #[must_use]
    pub fn staged(&self) -> &[u8] {
        &self.region.as_bytes()[..self.used]
    }

    /// Mutable CPU-side view of the staged bytes.
    #[must_use]
    pub fn staged_mut(&mut self) -> &mut [u8] {
        &mut self.region.as_bytes_mut()[..self.used]
    }

    /// Copy `payload` into the bounce buffer and remember the length.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if `payload` does not fit. Callers translate
    /// this into the appropriate `DriverError`.
    pub fn stage(&mut self, payload: &[u8]) -> Result<(), ()> {
        if payload.len() > self.region.len() {
            return Err(());
        }
        self.region.as_bytes_mut()[..payload.len()].copy_from_slice(payload);
        self.used = payload.len();
        Ok(())
    }

    /// Mutable CPU-side view of the full underlying region.
    ///
    /// Used by drivers that fill the staging buffer through the
    /// device (e.g. `virtio_net` receive) and then read back the
    /// completion length from the queue's used-ring entry. Updates
    /// the internal `used` cursor.
    pub fn fill_from_device(&mut self, n: usize) -> Result<&[u8], ()> {
        if n > self.region.len() {
            return Err(());
        }
        self.used = n;
        Ok(&self.region.as_bytes()[..n])
    }

    /// Mutable CPU-side view of the *full* underlying region (not
    /// limited to `used`). Used by drivers that pre-grant the device
    /// the maximum frame size.
    #[must_use]
    pub fn full_region_mut(&mut self) -> &mut [u8] {
        self.region.as_bytes_mut()
    }

    /// Consume the bounce buffer, returning the underlying
    /// [`DmaRegion`] to the host **after** scrubbing if the
    /// declared class was sensitive.
    ///
    /// Use this in preference to letting [`Drop`] run when the
    /// driver wants to surface a `Result<DmaRegion, _>` to its host.
    #[must_use]
    pub fn into_region(mut self) -> DmaRegion<'a> {
        self.scrub_if_sensitive();
        // SAFETY: we move `self.region` out and consume `self`, so
        // the `Drop` impl below will not scrub a second time.
        let phys = self.region.phys();
        let bytes_ptr = self.region.bytes as *mut [u8];
        // We cannot `mem::take` a mutable reference, so we manually
        // disassemble. The dropping `self` will not access `region`
        // again because we set `self.used = 0` and the bytes slice
        // is left behind in a known-zero state.
        core::mem::forget(self);
        // SAFETY: `bytes_ptr` originated from a `&'a mut [u8]` and is
        // still uniquely live for `'a` because `self` (the only
        // owner of that borrow) has been forgotten without a
        // destructor running, and no aliasing reference exists.
        let bytes: &'a mut [u8] = unsafe { &mut *bytes_ptr };
        DmaRegion::from_parts(phys, bytes)
    }

    fn scrub_if_sensitive(&mut self) {
        if self.class.is_sensitive() {
            for byte in self.region.as_bytes_mut() {
                *byte = 0;
            }
            self.used = 0;
        }
    }
}

impl Drop for BounceBuffer<'_> {
    fn drop(&mut self) {
        self.scrub_if_sensitive();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_region_view_round_trip() {
        let mut buf = [0u8; 16];
        let buf_ptr = buf.as_ptr() as u64;
        let mut r = DmaRegion::from_parts(buf_ptr, &mut buf);
        assert_eq!(r.phys(), buf_ptr);
        assert_eq!(r.len(), 16);
        assert!(!r.is_empty());
        r.as_bytes_mut()[0] = 0xAA;
        assert_eq!(r.as_bytes()[0], 0xAA);
    }

    #[test]
    fn bounce_buffer_stages_payload() {
        let mut backing = [0u8; 32];
        let phys = backing.as_ptr() as u64;
        let region = DmaRegion::from_parts(phys, &mut backing);
        let mut bb = BounceBuffer::new(region, BufferClass::NonSensitive);
        assert!(bb.stage(&[1, 2, 3, 4]).is_ok());
        assert_eq!(bb.used(), 4);
        assert_eq!(bb.staged(), &[1, 2, 3, 4]);
        assert_eq!(bb.phys(), phys);
    }

    #[test]
    fn bounce_buffer_rejects_overflow() {
        let mut backing = [0u8; 4];
        let region = DmaRegion::from_parts(0xCAFE, &mut backing);
        let mut bb = BounceBuffer::new(region, BufferClass::NonSensitive);
        assert_eq!(bb.stage(&[0u8; 8]), Err(()));
    }

    #[test]
    fn bounce_buffer_scrubs_on_sensitive_drop() {
        let mut backing = [0u8; 16];
        {
            let region = DmaRegion::from_parts(0x1000, &mut backing);
            let mut bb = BounceBuffer::new(region, BufferClass::Sensitive);
            bb.stage(&[0xAA; 16]).unwrap();
            assert_eq!(bb.staged(), &[0xAA; 16]);
            // bb dropped here.
        }
        assert!(backing.iter().all(|b| *b == 0));
    }

    #[test]
    fn bounce_buffer_preserves_on_non_sensitive_drop() {
        let mut backing = [0u8; 16];
        {
            let region = DmaRegion::from_parts(0x2000, &mut backing);
            let mut bb = BounceBuffer::new(region, BufferClass::NonSensitive);
            bb.stage(&[0xBB; 16]).unwrap();
        }
        assert!(backing.iter().all(|b| *b == 0xBB));
    }

    #[test]
    fn bounce_buffer_into_region_scrubs_on_sensitive() {
        let mut backing = [0u8; 12];
        let region = DmaRegion::from_parts(0x3000, &mut backing);
        let mut bb = BounceBuffer::new(region, BufferClass::Sensitive);
        bb.stage(&[0x77; 12]).unwrap();
        let returned = bb.into_region();
        assert_eq!(returned.len(), 12);
        assert!(returned.as_bytes().iter().all(|b| *b == 0));
    }

    #[test]
    fn fill_from_device_updates_used() {
        let mut backing = [0u8; 32];
        let region = DmaRegion::from_parts(0x4000, &mut backing);
        let mut bb = BounceBuffer::new(region, BufferClass::NonSensitive);
        bb.full_region_mut()[..5].copy_from_slice(b"hello");
        let view = bb.fill_from_device(5).expect("fit");
        assert_eq!(view, b"hello");
        assert_eq!(bb.used(), 5);
        assert_eq!(bb.fill_from_device(64), Err(()));
    }
}
