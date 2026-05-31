//! Filesystem driver class (`drivers/filesystem/*`).
//!
//! A filesystem driver attaches a block-backed image to a mount point
//! and detaches it again. Path resolution, permission enforcement,
//! and the VFS itself live in `kernel/core` (Stage 5); this trait
//! exposes only the operations the host needs in order to wire a
//! driver into the mount table.
//!
//! The block-device side of the mount lives behind the
//! [`block::Block`](crate::driver::block::Block) trait; this module
//! does not duplicate it.

use super::DriverError;

/// Mount-flag bitmap, frozen at `abi-v1`.
///
/// Unknown bits must be zero; a non-zero unknown bit causes
/// [`DriverError::OutOfRange`] at mount time.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct MountFlags(u32);

impl MountFlags {
    /// Mount the filesystem read-only.
    pub const READ_ONLY: Self = Self(1 << 0);
    /// Reject `setuid` / `setgid` bits on this mount (`AGENTS.md` §5.3).
    pub const NOSUID: Self = Self(1 << 1);
    /// Reject device-special files on this mount.
    pub const NODEV: Self = Self(1 << 2);
    /// Reject executable mappings of files on this mount.
    pub const NOEXEC: Self = Self(1 << 3);

    /// Bitwise OR of every flag defined in `abi-v1`. Any bit outside
    /// this mask is reserved and rejected on the wire.
    pub const KNOWN_MASK: Self =
        Self(Self::READ_ONLY.0 | Self::NOSUID.0 | Self::NODEV.0 | Self::NOEXEC.0);

    /// Raw bitmap.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Construct a [`MountFlags`] from a raw bitmap.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::OutOfRange`] if `raw` has bits set
    /// outside [`Self::KNOWN_MASK`].
    ///
    /// # Capabilities
    ///
    /// None.
    pub const fn from_bits(raw: u32) -> Result<Self, DriverError> {
        if raw & !Self::KNOWN_MASK.0 != 0 {
            return Err(DriverError::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Returns `true` iff every bit in `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The set of flags present in either `self` or `other`.
    ///
    /// Both operands are already-validated [`MountFlags`], so the union
    /// stays within [`Self::KNOWN_MASK`] and needs no re-validation. Used
    /// to build composite mount policies such as the `nosuid,nodev,noexec`
    /// default for `/System/Logs` (`AGENTS.md` §16.2).
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Trait every filesystem driver implements.
///
/// # Capabilities
///
/// The trait's `mount` / `unmount` methods are gated by
/// [`CapabilityId::FS_MOUNT`](crate::CapabilityId::FS_MOUNT) at the
/// dispatch site, on top of the load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD) check.
pub trait Filesystem {
    /// Attach a backing image and bring the filesystem online.
    ///
    /// `source_block_handle` is the kernel-issued
    /// [`DriverHandle`](crate::driver::DriverHandle) for the
    /// underlying [`block::Block`](crate::driver::block::Block) device,
    /// not a path — path resolution belongs to the VFS, not to this
    /// trait.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the dispatcher has not
    ///   verified `CAP_FS_MOUNT`.
    /// * [`DriverError::BadMagic`] if the on-disk filesystem
    ///   superblock fails validation.
    /// * [`DriverError::DeviceFault`] if the block device reports an
    ///   unrecoverable read error.
    /// * [`DriverError::Busy`] if the filesystem is already mounted.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`](crate::CapabilityId::FS_MOUNT).
    fn mount(
        &mut self,
        source_block_handle: crate::driver::DriverHandle,
        flags: MountFlags,
    ) -> Result<(), DriverError>;

    /// Detach the filesystem.
    ///
    /// Implementations must flush any in-flight write before
    /// returning success.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the dispatcher has not
    ///   verified `CAP_FS_MOUNT`.
    /// * [`DriverError::Busy`] if the filesystem still has open
    ///   files.
    /// * [`DriverError::NotFound`] if the filesystem is not mounted.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`](crate::CapabilityId::FS_MOUNT).
    fn unmount(&mut self) -> Result<(), DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::DriverHandle;

    #[test]
    fn known_mask_covers_every_named_flag() {
        let all = MountFlags::READ_ONLY.bits()
            | MountFlags::NOSUID.bits()
            | MountFlags::NODEV.bits()
            | MountFlags::NOEXEC.bits();
        assert_eq!(MountFlags::KNOWN_MASK.bits(), all);
    }

    #[test]
    fn from_bits_rejects_unknown() {
        assert_eq!(MountFlags::from_bits(1 << 31), Err(DriverError::OutOfRange));
    }

    #[test]
    fn union_combines_known_flags() {
        let combined = MountFlags::NOSUID
            .union(MountFlags::NODEV)
            .union(MountFlags::NOEXEC);
        assert!(combined.contains(MountFlags::NOSUID));
        assert!(combined.contains(MountFlags::NODEV));
        assert!(combined.contains(MountFlags::NOEXEC));
        assert!(!combined.contains(MountFlags::READ_ONLY));
        // The union of known flags is itself within the known mask.
        assert_eq!(combined.bits() & !MountFlags::KNOWN_MASK.bits(), 0);
    }

    #[test]
    fn contains_is_bitwise_subset() {
        let Ok(f) = MountFlags::from_bits(MountFlags::NOSUID.bits() | MountFlags::NODEV.bits())
        else {
            unreachable!("known bits")
        };
        assert!(f.contains(MountFlags::NOSUID));
        assert!(!f.contains(MountFlags::READ_ONLY));
    }

    struct MockFs {
        mounted: bool,
    }

    impl Filesystem for MockFs {
        fn mount(&mut self, _src: DriverHandle, _flags: MountFlags) -> Result<(), DriverError> {
            if self.mounted {
                return Err(DriverError::Busy);
            }
            self.mounted = true;
            Ok(())
        }

        fn unmount(&mut self) -> Result<(), DriverError> {
            if !self.mounted {
                return Err(DriverError::NotFound);
            }
            self.mounted = false;
            Ok(())
        }
    }

    #[test]
    fn mock_round_trip() {
        let Ok(handle) = DriverHandle::from_raw(1) else {
            unreachable!("1 is non-zero")
        };
        let mut fs = MockFs { mounted: false };
        assert!(fs.mount(handle, MountFlags::READ_ONLY).is_ok());
        assert_eq!(
            fs.mount(handle, MountFlags::READ_ONLY),
            Err(DriverError::Busy)
        );
        assert!(fs.unmount().is_ok());
        assert_eq!(fs.unmount(), Err(DriverError::NotFound));
    }
}
