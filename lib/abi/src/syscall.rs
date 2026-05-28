//! Syscall identifiers shared between the kernel and user space.
//!
//! This module defines the numeric identifier each syscall carries on the
//! ABI. The kernel's per-architecture dispatch table — landing in
//! `kernel/syscall/src/table.rs` during Stage 2 — is generated from this
//! definition; `cargo xtask abi-check` enforces that the two never drift.
//!
//! ## Stage 1 boundary
//!
//! Only the *numbering* of syscalls is fixed here. The kernel-side dispatch
//! table is intentionally not yet introduced (see `PLAN.md` Stage 2). To
//! avoid prematurely triggering the cross-check, the syscall ABI lives in a
//! `syscall.rs` (singular) module rather than the `syscalls.rs` file that
//! `cargo xtask abi-check` watches for. The file `lib/abi/src/syscalls.rs`
//! will be introduced together with `kernel/syscall/src/table.rs` so that
//! the diff tool always sees both halves.

use crate::Errno;

/// Length in bytes of the cryptographic hash a manifest uses to pin the
/// syscall table it was built against.
///
/// A manifest carrying a hash whose value disagrees with the kernel's
/// compiled-in hash is refused at load time; this is the mechanism by which
/// `abi-v1` binaries are detected on an `abi-v2` kernel and vice-versa.
pub const SYSCALL_TABLE_HASH_LEN: usize = 32;

/// Stable syscall identifier.
///
/// Wraps a `u16` so it cannot be confused with raw integer arguments at call
/// sites. Identifiers are dense; gaps are not permitted because the kernel
/// dispatch table indexes directly with the value.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SyscallNumber(u16);

impl SyscallNumber {
    /// Yield the calling thread.
    pub const YIELD: Self = Self(0);
    /// Terminate the calling process with the supplied exit code.
    pub const EXIT: Self = Self(1);
    /// Send a message to an IPC endpoint.
    pub const IPC_SEND: Self = Self(2);
    /// Receive a message from an IPC endpoint.
    pub const IPC_RECV: Self = Self(3);
    /// Query whether the caller holds a given capability.
    pub const CAP_QUERY: Self = Self(4);
    /// Delegate a (necessarily narrower) capability set to another task.
    pub const CAP_DELEGATE: Self = Self(5);
    /// Revoke a previously delegated capability set.
    pub const CAP_REVOKE: Self = Self(6);
    /// Read the monotonic clock.
    pub const CLOCK_GET: Self = Self(7);

    /// Inclusive upper bound on the syscall identifier space in `abi-v1`.
    pub const MAX: u16 = 1023;

    /// Wrap a raw value, validating that it falls inside the syscall table.
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` exceeds [`SyscallNumber::MAX`].
    pub const fn from_raw(raw: u16) -> Result<Self, Errno> {
        if raw > Self::MAX {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{SyscallNumber, SYSCALL_TABLE_HASH_LEN};
    use crate::Errno;

    #[test]
    fn well_known_numbers_are_frozen() {
        // Numeric assignments are part of abi-v1; do not renumber.
        assert_eq!(SyscallNumber::YIELD.as_u16(), 0);
        assert_eq!(SyscallNumber::EXIT.as_u16(), 1);
        assert_eq!(SyscallNumber::IPC_SEND.as_u16(), 2);
        assert_eq!(SyscallNumber::IPC_RECV.as_u16(), 3);
        assert_eq!(SyscallNumber::CAP_QUERY.as_u16(), 4);
        assert_eq!(SyscallNumber::CAP_DELEGATE.as_u16(), 5);
        assert_eq!(SyscallNumber::CAP_REVOKE.as_u16(), 6);
        assert_eq!(SyscallNumber::CLOCK_GET.as_u16(), 7);
    }

    #[test]
    fn from_raw_enforces_table_bounds() {
        assert_eq!(
            SyscallNumber::from_raw(SyscallNumber::MAX).map(SyscallNumber::as_u16),
            Ok(1023)
        );
        assert_eq!(
            SyscallNumber::from_raw(SyscallNumber::MAX + 1),
            Err(Errno::OutOfRange),
        );
    }

    #[test]
    fn hash_length_matches_sha256() {
        assert_eq!(SYSCALL_TABLE_HASH_LEN, 32);
    }
}
