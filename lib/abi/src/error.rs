//! Stable error codes returned across the user/kernel ABI.
//!
//! Errors are represented by [`Errno`], a `#[repr(i32)]` enum whose numeric
//! values are part of the frozen `abi-v1` surface. New variants may only be
//! appended; existing values must never be re-numbered or removed.

use core::fmt;

/// Stable kernel-to-user error code.
///
/// Numeric values are part of the frozen ABI: kernel and user space agree
/// on the exact integer for each variant. The discriminants are deliberately
/// disjoint from POSIX `errno` so a mis-routed POSIX value cannot be confused
/// for a RustOS [`Errno`].
#[repr(i32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Errno {
    /// A supplied buffer is shorter than the structure it must contain.
    BufferTooSmall = 1,
    /// A supplied buffer or field has an alignment the ABI requires it to meet
    /// and does not.
    BadAlignment = 2,
    /// A magic number, version tag, or reserved field does not match the ABI.
    BadMagic = 3,
    /// A length, count, or offset field exceeds its ABI-mandated maximum.
    LengthOutOfRange = 4,
    /// A capability identifier or syscall number is outside the table.
    OutOfRange = 5,
    /// A required capability is not held by the caller.
    PermissionDenied = 6,
    /// The requested object does not exist.
    NotFound = 7,
    /// The caller attempted to widen a delegated capability set.
    DelegationWiden = 8,
    /// A signature failed verification.
    SignatureInvalid = 9,
    /// The ABI version stored in a manifest is not supported by this kernel.
    AbiVersionUnsupported = 10,
}

impl Errno {
    /// Numeric value carried on the ABI.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::BufferTooSmall => "buffer too small",
            Self::BadAlignment => "bad alignment",
            Self::BadMagic => "bad magic",
            Self::LengthOutOfRange => "length out of range",
            Self::OutOfRange => "value out of range",
            Self::PermissionDenied => "permission denied",
            Self::NotFound => "not found",
            Self::DelegationWiden => "delegation would widen authority",
            Self::SignatureInvalid => "signature invalid",
            Self::AbiVersionUnsupported => "abi version unsupported",
        };
        f.write_str(message)
    }
}

#[cfg(test)]
mod tests {
    use super::Errno;

    #[test]
    fn discriminants_are_frozen() {
        // These values are part of the abi-v1 contract.
        assert_eq!(Errno::BufferTooSmall.as_i32(), 1);
        assert_eq!(Errno::BadAlignment.as_i32(), 2);
        assert_eq!(Errno::BadMagic.as_i32(), 3);
        assert_eq!(Errno::LengthOutOfRange.as_i32(), 4);
        assert_eq!(Errno::OutOfRange.as_i32(), 5);
        assert_eq!(Errno::PermissionDenied.as_i32(), 6);
        assert_eq!(Errno::NotFound.as_i32(), 7);
        assert_eq!(Errno::DelegationWiden.as_i32(), 8);
        assert_eq!(Errno::SignatureInvalid.as_i32(), 9);
        assert_eq!(Errno::AbiVersionUnsupported.as_i32(), 10);
    }

    #[test]
    fn display_is_stable() {
        // `Display` text is consumed by `lib/log` event records: keep it stable.
        assert_eq!(
            alloc::format!("{}", Errno::PermissionDenied).as_str(),
            "permission denied",
        );
    }

    extern crate alloc;
}
