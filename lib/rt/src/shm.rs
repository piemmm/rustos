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
//!
//! [`SharedRegion`] is a region this task **created**; [`MappedGrant`] is one
//! another task granted it. Both own a mapping and unmap on drop; they differ
//! only in where the mapping came from, and a grant carries no region id
//! because the holder is not the owner and may not re-grant it.

/// The address a kernel-reported mapping base can be read as, or `None`
/// when it is not one a slice may be built over.
///
/// Zero is refused rather than assumed unreachable: a slice over a null
/// pointer is undefined behaviour even when it is empty, and this is the one
/// place both mappings rule it out.
fn mapped_at(base: u64) -> Option<usize> {
    usize::try_from(base).ok().filter(|addr| *addr != 0)
}

/// A shared-memory region this task created and mapped, unmapped on drop.
pub struct SharedRegion {
    base: usize,
    len: usize,
    id: u64,
}

impl SharedRegion {
    /// Create and map a `len`-byte region.
    ///
    /// `None` if the kernel refused it, or if the base it returned is not
    /// one this target can build a slice over — never a partially-
    /// established region.
    #[must_use]
    pub fn create(len: usize) -> Option<Self> {
        let mut id: u64 = 0;
        let base = crate::shm_create(len, &mut id);
        if base < 0 {
            return None;
        }
        #[allow(clippy::cast_sign_loss)] // `base >= 0` checked above; it is a user VA.
        let addr = mapped_at(base as u64)?;
        Some(Self {
            base: addr,
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

/// A shared-memory region another task granted to this one, mapped
/// read/write and unmapped on drop.
///
/// The length is the **kernel's** own record of the region, reported by
/// `shm_map` — never the granting task's claim about it — so a server
/// writing into a client's region writes inside what the kernel actually
/// mapped.
pub struct MappedGrant {
    base: usize,
    len: usize,
}

impl MappedGrant {
    /// Map the granted region `handle`, requiring at least `least` bytes.
    ///
    /// # Errors
    ///
    /// * The kernel's own refusal — [`tairix_abi::Errno::NotFound`] for a
    ///   handle naming no grant to this task (its owner check at
    ///   `shm_map`).
    /// * [`tairix_abi::Errno::LengthOutOfRange`] for a region smaller than
    ///   `least`, or one whose base is not an address a slice may be built
    ///   over.
    ///
    /// A refusal maps nothing: a region established and then found too
    /// small is released before this returns.
    pub fn map(handle: u64, least: usize) -> Result<Self, tairix_abi::Errno> {
        let mut raw_len: u64 = 0;
        let ret = crate::shm_map(handle, &mut raw_len);
        if ret < 0 {
            return Err(tairix_abi::Errno::from_syscall(ret));
        }
        #[allow(clippy::cast_sign_loss)] // `ret >= 0` checked above; it is a user VA.
        let base = ret as u64;
        let (Some(addr), Ok(len)) = (mapped_at(base), usize::try_from(raw_len)) else {
            // A base or length this target cannot build a slice over names
            // no reachable mapping; release it (the kernel resolves an unmap
            // by its base) and refuse rather than truncate.
            let _ = crate::shm_unmap(base, 0);
            return Err(tairix_abi::Errno::LengthOutOfRange);
        };
        // Constructed before the size check so every refusal below — and any
        // later drop — releases the mapping.
        let region = Self { base: addr, len };
        if len < least {
            return Err(tairix_abi::Errno::LengthOutOfRange);
        }
        Ok(region)
    }

    /// The mapping's byte length, as the kernel recorded it.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping holds no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The mapped bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the kernel mapped exactly `len` bytes read/write at `base`
        // — its own record of the region size — and the mapping lives
        // exactly as long as `self`, which owns it and unmaps only on drop.
        // `&self` excludes every mutable reference to these bytes on this
        // side; the granting task's own access is the owning protocol's to
        // serialise, and at worst tears pixel values rather than breaking
        // this borrow.
        unsafe { core::slice::from_raw_parts(self.base as *const u8, self.len) }
    }

    /// The mapped bytes, for writing.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, with `&mut self` excluding every other
        // reference on this side.
        unsafe { core::slice::from_raw_parts_mut(self.base as *mut u8, self.len) }
    }
}

impl Drop for MappedGrant {
    fn drop(&mut self) {
        let _ = crate::shm_unmap(self.base as u64, self.len);
    }
}

#[cfg(test)]
mod tests {
    use super::{mapped_at, MappedGrant, SharedRegion};

    /// A slice over a null pointer is undefined behaviour even when it is
    /// empty, so a zero base is not a mapping however the kernel reported
    /// it.
    #[test]
    fn a_zero_base_is_not_a_mapping() {
        assert_eq!(mapped_at(0), None);
        assert_eq!(mapped_at(0x1000), Some(0x1000));
    }

    /// On a host there is no kernel behind the trap, so neither mapping is
    /// ever established — which is why no slice is ever built over an
    /// address this build fabricated, and why the interpreter has nothing
    /// of this module to look at.
    #[test]
    fn neither_mapping_is_established_without_a_kernel() {
        assert!(MappedGrant::map(1, 0).is_err());
        assert!(SharedRegion::create(4096).is_none());
    }
}
