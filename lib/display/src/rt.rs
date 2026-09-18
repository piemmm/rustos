//! The production, `tairix-rt`-backed [`ShmMapper`] (feature `rt`).
//!
//! The one definition of "map a client's granted shared-memory region
//! through the kernel's `shm_map`, sized from the kernel's own record of
//! the region length — never the granting client's claim" — shared by
//! the framebuffer display service's serve loop and the desktop
//! session's window server (`plans/APPWIN.md` AW3), so the mapping
//! discipline cannot drift between the two.
//!
//! The mapping itself is the runtime's [`MappedGrant`]; this narrows it to
//! the read-only view a frame region is, so nothing reached through a
//! [`FrameRegion`] can write into a client's frames.

use tairix_abi::Errno;
use tairix_rt::shm::MappedGrant;

use crate::server::{FrameRegion, ShmMapper};

/// A client region mapped through `shm_map`, unmapped on drop (a
/// reconfigure, a closed window, or an observed lease loss releases the
/// old mapping).
pub struct RtShmRegion(MappedGrant);

impl FrameRegion for RtShmRegion {
    fn bytes(&self) -> &[u8] {
        self.0.bytes()
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
        MappedGrant::map(handle, min_len).map(RtShmRegion)
    }
}
