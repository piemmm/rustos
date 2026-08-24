//! The production, `tairix-rt`-backed [`ShmMapper`] (feature `rt`).
//!
//! The one definition of "map a client's granted shared-memory region
//! through the kernel's `shm_map`, sized from the kernel's own record of
//! the region length — never the granting client's claim" — shared by
//! the framebuffer display service's serve loop and the desktop
//! session's window server (`plans/APPWIN.md` AW3), so the mapping
//! discipline cannot drift between the two.
//!
//! Feature-gated because the engine crate itself is I/O-free and
//! forbid-unsafe: only a consumer that really runs on the TAIRiX
//! runtime pulls in the syscall-backed mapping (and, with it, this
//! module's single audited `unsafe` view of the kernel mapping).

use tairix_abi::Errno;

use crate::server::{FrameRegion, ShmMapper};

/// Recover the [`Errno`] a syscall encoded as a negative register
/// (`-ret`); an unrecognised code fails closed as
/// [`Errno::NotImplemented`] rather than being guessed.
fn errno_from(ret: i64) -> Errno {
    i32::try_from(-ret)
        .ok()
        .and_then(Errno::from_i32)
        .unwrap_or(Errno::NotImplemented)
}

/// A client region mapped through `shm_map`, unmapped on drop (a
/// reconfigure, a closed window, or an observed lease loss releases the
/// old mapping).
pub struct RtShmRegion {
    /// Base user virtual address of this process's mapping (the value
    /// `shm_unmap` releases by).
    base: u64,
    /// The mapping's base as a pointer, converted once at map time
    /// through a checked `usize::try_from` so no width-truncating cast
    /// survives to the read path.
    ptr: *const u8,
    /// The region's byte length — the kernel's own record, reported by
    /// `shm_map`, never the granting client's claim.
    len: usize,
}

impl FrameRegion for RtShmRegion {
    // The crate forbids `unsafe` by default; this one block reads the
    // kernel-granted frame mapping, whose extent the kernel itself recorded.
    #[allow(unsafe_code)]
    fn bytes(&self) -> &[u8] {
        // SAFETY: the kernel mapped exactly `len` bytes of the granted
        // region (its own record of the region size) read/write into
        // this process at `ptr`, and the mapping stays live until this
        // region's `Drop` releases it — nothing else in this address
        // space unmaps or aliases it. The granting client maps the same
        // frames, but the protocol serialises access: a presenting
        // client is parked in its call until this server replies, so
        // the presented bytes are not written while the engine reads
        // them, and a stale concurrent write could at worst tear pixel
        // values — never break memory safety of this borrow.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for RtShmRegion {
    fn drop(&mut self) {
        // Releasing the mapping drops this process's reference to the
        // region; the kernel resolves the mapping by its base and frees
        // the frames only at the last reference.
        let _ = tairix_rt::shm_unmap(self.base, self.len);
    }
}

/// The production [`ShmMapper`]: the kernel's `shm_map` of a granted
/// handle carried in-band by a `Configure` or window `Create`. Mapping
/// happens exactly once per region; the present hot path only indexes
/// the mapped bytes.
pub struct RtShmMapper;

impl ShmMapper for RtShmMapper {
    type Region = RtShmRegion;

    fn map(&mut self, handle: u64, min_len: usize) -> Result<RtShmRegion, Errno> {
        let mut raw_len: u64 = 0;
        let ret = tairix_rt::shm_map(handle, &mut raw_len);
        if ret < 0 {
            return Err(errno_from(ret));
        }
        #[allow(clippy::cast_sign_loss)] // `ret >= 0` checked above; it is a user VA.
        let base = ret as u64;
        let Ok(len) = usize::try_from(raw_len) else {
            // A region wider than the address width cannot be exposed
            // as a slice; release the mapping (the kernel resolves an
            // unmap by its base) and refuse rather than truncate.
            let _ = tairix_rt::shm_unmap(base, 0);
            return Err(Errno::LengthOutOfRange);
        };
        let Ok(addr) = usize::try_from(base) else {
            // A base the address width cannot hold names no reachable
            // mapping; release it and refuse rather than truncate.
            let _ = tairix_rt::shm_unmap(base, 0);
            return Err(Errno::LengthOutOfRange);
        };
        // Constructing the region first means every refusal below (and
        // any later drop) releases the mapping — no leak on failure.
        let region = RtShmRegion {
            base,
            ptr: addr as *const u8,
            len,
        };
        if len < min_len {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(region)
    }
}
