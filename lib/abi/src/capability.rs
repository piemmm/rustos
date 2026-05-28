//! Capability identifiers as carried across the ABI.
//!
//! A [`CapabilityId`] is the wire representation of a kernel capability. The
//! identifier space is dense and bounded by [`CAPABILITY_ID_MAX`] so that
//! capability sets can be represented as fixed-size bitmaps without an
//! allocator.
//!
//! Values defined here are part of the frozen `abi-v1` contract: existing
//! identifiers may not be re-numbered or removed; new capabilities must take
//! the next free integer and bump [`CAPABILITY_ID_MAX`] if necessary.

use crate::Errno;

/// Inclusive upper bound on capability identifiers in `abi-v1`.
///
/// Sized to leave headroom for the capabilities introduced by later stages
/// without forcing a `CapabilitySet` to grow past a single 64-bit word per
/// 64 entries. Increasing this value is a breaking ABI change.
pub const CAPABILITY_ID_MAX: u16 = 255;

/// Stable identifier for a kernel capability.
///
/// The inner integer is the on-wire representation; the wrapper type prevents
/// accidental confusion with other 16-bit ABI values such as syscall numbers.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct CapabilityId(u16);

impl CapabilityId {
    /// Mount and unmount filesystems.
    pub const FS_MOUNT: Self = Self(1);
    /// Open raw network sockets.
    pub const NET_RAW: Self = Self(2);
    /// Load a driver module in user space.
    pub const DRV_LOAD: Self = Self(3);
    /// Load a driver module in kernel space (additional to `DRV_LOAD`).
    pub const DRV_KERNEL: Self = Self(4);
    /// Create, modify, or delete users.
    pub const USER_ADMIN: Self = Self(5);
    /// Adjust the system wall clock.
    pub const TIME_SET: Self = Self(6);
    /// Bind to privileged IPC endpoints.
    pub const IPC_BIND_PRIVILEGED: Self = Self(7);
    /// Read the security audit log.
    pub const AUDIT_READ: Self = Self(8);
    /// Write entries to the security audit log.
    pub const AUDIT_WRITE: Self = Self(9);

    /// Construct a [`CapabilityId`] from its raw value, validating the range.
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` exceeds [`CAPABILITY_ID_MAX`].
    pub const fn from_raw(raw: u16) -> Result<Self, Errno> {
        if raw > CAPABILITY_ID_MAX {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// Position of this capability inside a 256-bit capability set.
    ///
    /// Always less than 256 by construction.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[cfg(test)]
mod tests {
    use super::{CapabilityId, CAPABILITY_ID_MAX};
    use crate::Errno;

    #[test]
    fn well_known_ids_are_frozen() {
        // The numeric values are part of abi-v1; do not renumber.
        assert_eq!(CapabilityId::FS_MOUNT.as_u16(), 1);
        assert_eq!(CapabilityId::NET_RAW.as_u16(), 2);
        assert_eq!(CapabilityId::DRV_LOAD.as_u16(), 3);
        assert_eq!(CapabilityId::DRV_KERNEL.as_u16(), 4);
        assert_eq!(CapabilityId::USER_ADMIN.as_u16(), 5);
        assert_eq!(CapabilityId::TIME_SET.as_u16(), 6);
        assert_eq!(CapabilityId::IPC_BIND_PRIVILEGED.as_u16(), 7);
        assert_eq!(CapabilityId::AUDIT_READ.as_u16(), 8);
        assert_eq!(CapabilityId::AUDIT_WRITE.as_u16(), 9);
    }

    #[test]
    fn from_raw_rejects_out_of_range() {
        assert_eq!(CapabilityId::from_raw(0).map(CapabilityId::as_u16), Ok(0));
        assert_eq!(
            CapabilityId::from_raw(CAPABILITY_ID_MAX).map(CapabilityId::as_u16),
            Ok(CAPABILITY_ID_MAX),
        );
        assert_eq!(
            CapabilityId::from_raw(CAPABILITY_ID_MAX + 1),
            Err(Errno::OutOfRange),
        );
    }

    #[test]
    fn index_is_within_bitset_bounds() {
        assert!(CapabilityId::AUDIT_WRITE.index() < 256);
    }
}
