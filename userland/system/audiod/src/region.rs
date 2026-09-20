//! The shared-PCM-region seam (`plans/SOUND.md`).
//!
//! The service holds a mapping of every ring it moves frames through, and
//! those rings arrive from two opposite directions: a *client* mints its own
//! and grants it inward, while the service mints a *device* ring and grants
//! it outward to the driver. [`RegionHost`] spells both, so neither can be
//! mistaken for the other, and abstracts the `shm_*` syscalls so the whole
//! engine is exercised on the host against an in-process double.

use tairix_abi::Errno;

/// One shared PCM region this service holds a mapping of.
///
/// Opaque, and only ever minted by a [`RegionHost`]: a stale or invented id
/// resolves to nothing rather than to somebody else's frames.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RegionId(pub u32);

/// The shared-memory facility the service's rings live in.
///
/// # Why one region may be borrowed at a time
///
/// [`bytes`](Self::bytes) takes `&mut self`, so the per-period path reads a
/// client ring into that stream's own scratch and *then* writes the device
/// ring — it can never hold two mappings live at once. That is the discipline
/// the real-time path needs stated in the type rather than remembered.
pub trait RegionHost {
    /// Create a region of exactly `len` bytes this service owns and may grant
    /// away — the **device** ring, which the driver maps.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal, or [`Errno::OutOfMemory`] when the
    /// service's own bookkeeping cannot record it.
    fn create(&mut self, len: usize) -> Result<RegionId, Errno>;

    /// Adopt the region a **client** granted to this service's serving
    /// endpoint, requiring at least `len` mapped bytes.
    ///
    /// The length check is the service's own: a grant shorter than the
    /// geometry it issued is refused before a frame moves, so a client cannot
    /// shrink its ring behind the mixer's back.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] for a grant shorter than `len`, or the
    /// kernel's typed refusal of the map.
    fn adopt(&mut self, grant: u64, len: usize) -> Result<RegionId, Errno>;

    /// Mint a grant of `region` directed at `endpoint` — how a device ring
    /// reaches the driver that will DMA out of it.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] for an unknown region, or the kernel's refusal.
    fn grant(&mut self, region: RegionId, endpoint: u64) -> Result<u64, Errno>;

    /// Borrow `region`'s mapped bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when `region` is not one this host holds.
    fn bytes(&mut self, region: RegionId) -> Result<&mut [u8], Errno>;

    /// Drop `region`'s mapping. A region that is not held is already
    /// released, so this is idempotent.
    fn release(&mut self, region: RegionId);
}
