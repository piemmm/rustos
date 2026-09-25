//! What a member agent's offer names once its device's transport is delegated.

use tairix_abi::raid_ipc::MemberOffer;
use tairix_abi::Errno;

/// The two resources a member agent delegates, as its matched node declared
/// them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Transport {
    /// The device's block-service call endpoint id.
    pub endpoint: u64,
    /// The region id of the device's shared data window, which is what
    /// `shm_grant` delegates by. The agent never maps it: it has no reason to
    /// look at the device's bytes.
    pub window: u64,
}

impl Transport {
    /// The offer naming this transport to the composer, given `shm_grant`'s
    /// raw result for the window.
    ///
    /// The window travels as the handle that delegation minted the composer,
    /// never as the region id: the composer maps it by its own handle, and a
    /// region id read as one of the composer's handles names nothing, or
    /// another region.
    ///
    /// # Errors
    ///
    /// The errno `shm_grant` returned, or [`Errno::NotImplemented`] for a
    /// result that is neither a handle nor an errno.
    pub fn offer(&self, window_grant: i64, node: u32) -> Result<MemberOffer, Errno> {
        match u64::try_from(window_grant) {
            Ok(handle) if handle != 0 => Ok(MemberOffer {
                endpoint: self.endpoint,
                window_grant: handle,
                node,
            }),
            _ => Err(Errno::from_syscall(window_grant)),
        }
    }
}

#[cfg(test)]
mod tests;
