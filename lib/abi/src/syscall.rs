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
    /// Bind to a hardware interrupt line.
    ///
    /// Argument: `line: u32` — architecture-defined IRQ identifier
    /// (GSI on x86_64, GIC `IntId` on `AArch64`, PLIC source on
    /// RISC-V; the per-architecture binding is documented in
    /// `docs/src/security/irq.md`). Returns an opaque
    /// [`crate::IrqHandle`] (kernel-issued, unforgeable) bound to the
    /// calling task. Requires [`crate::CapabilityId::IRQ_BIND`].
    pub const IRQ_BIND: Self = Self(8);
    /// Wait for a wake-up on a previously bound interrupt handle.
    ///
    /// Arguments: `handle: IrqHandle`, `timeout_ns: u64`. The kernel
    /// blocks the caller on the handle's wait queue. Returns
    /// `Ok(())` when the interrupt fires (the kernel masks the line
    /// at the controller before resuming the waiter so the same edge
    /// does not stampede the driver), or [`crate::Errno::TimedOut`]
    /// if `timeout_ns` elapses first. Requires
    /// [`crate::CapabilityId::IRQ_BIND`] — the handle is also
    /// re-checked against the calling task's binding to defend
    /// against handle forgery (`AGENTS.md` §5.4).
    pub const IRQ_WAIT: Self = Self(9);
    /// Fill a user buffer with cryptographically secure random bytes.
    ///
    /// Arguments: `buf: *mut u8` (user pointer), `len: usize`,
    /// `flags: u32` ([`crate::RandomFlags`]). Returns the number of
    /// bytes written. The kernel draws from its CSPRNG-backed output
    /// reserve (`AGENTS.md` §22); the call is unprivileged (drawing
    /// randomness needs no capability), but a `len` above
    /// [`crate::RANDOM_REQUEST_MAX_BYTES`] is refused. With
    /// [`crate::RandomFlags::NON_BLOCKING`] set and the kernel RNG not
    /// yet seeded it returns [`crate::Errno::EntropyNotReady`] rather
    /// than blocking.
    pub const RANDOM_GET: Self = Self(10);

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

/// Opaque, kernel-issued handle to a bound hardware interrupt line.
///
/// Returned by the `irq_bind` syscall and consumed by `irq_wait`. The
/// inner `u64` is unforgeable in the sense that the kernel rejects any
/// `irq_wait` whose `handle` was not previously minted for the calling
/// task (`AGENTS.md` §5.2 — capabilities are unforgeable tokens; §5.4 —
/// no trusted-caller shortcuts). The wire representation is the raw
/// `u64`; the wrapper exists so call sites cannot confuse it with
/// arbitrary integer arguments.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IrqHandle(u64);

impl IrqHandle {
    /// Reserved invalid value.
    ///
    /// The kernel must never mint this value; it is reserved so a
    /// caller-zeroed buffer cannot be mistaken for a live handle.
    pub const INVALID: Self = Self(0);

    /// Wrap a raw value as a handle.
    ///
    /// Reserved for the kernel's IRQ allocator. User-space code
    /// receives handles from `irq_bind`; constructing one by hand
    /// gains nothing because the kernel re-checks the handle against
    /// the caller's binding on every `irq_wait`.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{IrqHandle, SyscallNumber, SYSCALL_TABLE_HASH_LEN};
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
        assert_eq!(SyscallNumber::IRQ_BIND.as_u16(), 8);
        assert_eq!(SyscallNumber::IRQ_WAIT.as_u16(), 9);
        assert_eq!(SyscallNumber::RANDOM_GET.as_u16(), 10);
    }

    #[test]
    fn irq_handle_round_trips_and_invalid_is_zero() {
        assert_eq!(IrqHandle::INVALID.as_u64(), 0);
        let h = IrqHandle::from_raw(0xDEAD_BEEF_CAFE_F00D);
        assert_eq!(h.as_u64(), 0xDEAD_BEEF_CAFE_F00D);
        assert_ne!(h, IrqHandle::INVALID);
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
