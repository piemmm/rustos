//! A mapped shared-memory region, owned.
//!
//! [`shm_create`](crate::shm_create) hands back a raw base address and a
//! length, and every consumer that wants the bytes has to build the same
//! slice over them and unmap on the same paths. This is that, once: one
//! `unsafe` with one proof, and a `Drop` that cannot be forgotten.
//!
//! It is deliberately not window-, driver-, or protocol-specific: what a
//! region is *for* — which endpoint it is granted to, what is written into it
//! — belongs to the crate that knows, and this owns only the mapping.

/// A shared-memory region this task created and mapped, unmapped on drop.
pub struct SharedRegion {
    base: usize,
    len: usize,
    id: u64,
}

impl SharedRegion {
    /// Create and map a `len`-byte region.
    ///
    /// `None` if the kernel refused it, or if the base it returned does not
    /// fit this target's address width — never a partially-established
    /// region.
    #[must_use]
    pub fn create(len: usize) -> Option<Self> {
        let mut id: u64 = 0;
        let base = crate::shm_create(len, &mut id);
        if base < 0 {
            return None;
        }
        Some(Self {
            base: usize::try_from(base).ok()?,
            len,
            id,
        })
    }

    /// The region's kernel id, for granting it to an endpoint
    /// ([`shm_grant`](crate::shm_grant)).
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Bytes of the mapping.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the region maps nothing, which only a zero-length create
    /// produces.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The region as a mutable byte slice.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the kernel mapped exactly `len` zeroed bytes read/write at
        // `base` — `shm_create` maps the length it was asked for — and the
        // mapping lives exactly as long as `self`, which owns it and unmaps
        // only on drop. `&mut self` is what excludes every other reference to
        // these bytes on this side; a peer that was *granted* the region
        // reads it under the owning protocol's own serialisation.
        unsafe { core::slice::from_raw_parts_mut(self.base as *mut u8, self.len) }
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        let _ = crate::shm_unmap(self.base as u64, self.len);
    }
}
