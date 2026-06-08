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
    /// A message payload exceeds the maximum the receiver advertised.
    ///
    /// Semantically equivalent to POSIX `EMSGSIZE`. Emitted by `kernel/ipc`
    /// when a sender hands the port a payload larger than the port's
    /// declared `max_payload`, or larger than the global
    /// [`crate::ipc::IPC_MESSAGE_MAX_PAYLOAD_LEN`] cap.
    MessageTooLarge = 11,
    /// The requested operation has no implementation in this kernel build.
    ///
    /// Reserved for code paths whose contract is stable on the ABI but
    /// whose backing subsystem is not yet wired in. Issuing a syscall
    /// that returns this errno is **not** an ABI violation — it is the
    /// kernel announcing that a stable interface is intentionally inert
    /// (e.g. `cap_delegate`'s user-pointer copy-in before user-memory
    /// plumbing lands). The variant is part of `abi-v1` and its
    /// discriminant is frozen alongside the others.
    NotImplemented = 12,
    /// A bounded wait expired before the awaited event occurred.
    ///
    /// Emitted by the `irq_wait` syscall (and future bounded-wait
    /// syscalls) when the caller-supplied `timeout_ns` elapses
    /// before the kernel can wake the caller. Returning this errno
    /// is **not** an error in the IRQ subsystem itself — the line
    /// stays bound, the handle stays valid, and the caller may
    /// re-issue `irq_wait` immediately.
    TimedOut = 13,
    /// An absolute time or duration cannot be represented by the target.
    ///
    /// Emitted by [`crate::time::Time64`] / [`crate::time::Duration64`] when a
    /// value is narrowed to a representation that cannot hold it — for
    /// example converting a `Time64` to a narrower on-disk timestamp encoding
    /// (`AGENTS.md` §21). The conversion is always checked; this errno is the
    /// fail-closed result, never a silent truncation, wrap, or saturation.
    TimestampOutOfRange = 14,
    /// A storage backend cannot satisfy a request because it is full.
    ///
    /// Semantically equivalent to POSIX `ENOSPC`. Emitted by a filesystem
    /// driver when it exhausts its on-disk free space (no free data block or
    /// cluster remains) or its inode/directory-entry budget while servicing
    /// an allocating operation such as `create`, `write_at`, or `truncate`.
    /// It is the fail-closed result of a genuinely full volume, distinct from
    /// [`DeviceFault`](crate::DriverError::DeviceFault)'s unrecoverable
    /// hardware error.
    NoSpace = 15,
    /// The kernel cryptographic RNG has not yet been initialised.
    ///
    /// Emitted only by the random API (`AGENTS.md` §22) and only when the
    /// caller explicitly requested non-blocking behaviour
    /// ([`crate::random::RandomFlags::NON_BLOCKING`]). Before the kernel RNG
    /// is seeded a blocking request waits; a non-blocking request fails
    /// closed with this errno rather than returning weak randomness. After
    /// initialisation the random API never returns it.
    EntropyNotReady = 16,
    /// An object cannot be created because one with the same identity
    /// already exists.
    ///
    /// Emitted by `kernel/ipc`'s named-port registry when a caller tries
    /// to register a [`crate::ipc`] endpoint whose `EndpointId` is
    /// already bound. It is the fail-closed result of a duplicate
    /// registration: the existing live port is never overwritten
    /// (`AGENTS.md` §5.4), and the caller's freshly-created port is
    /// handed back so it can be torn down.
    AlreadyExists = 17,
    /// A user-space pointer handed to a syscall does not name memory the
    /// caller may access in the direction the call requires.
    ///
    /// The RustOS equivalent of POSIX `EFAULT`. Emitted by any syscall
    /// that copies through the kernel's `copy_from_user` / `copy_to_user`
    /// boundary (`AGENTS.md` §5.4) when the user buffer is null, runs off
    /// the end of the address space, is unmapped, is not a user page, or
    /// lacks the read/write permission the copy direction needs (the
    /// §19.2 W^X guard refuses writing an executable page). The kernel
    /// returns this one code for every such failure rather than reporting
    /// *which* invariant broke, so a faulting pointer cannot be used as an
    /// oracle to probe the kernel's memory layout (`AGENTS.md` §5.4 — fail
    /// closed; §19.1). It is also the fail-closed result when the caller
    /// has no registered address space at all (e.g. a kernel task).
    BadAddress = 18,
    /// A non-blocking operation has nothing to return right now and
    /// would have to block to make progress.
    ///
    /// The RustOS equivalent of POSIX `EAGAIN` / `EWOULDBLOCK`. Emitted
    /// by the non-blocking `ipc_recv` syscall when the addressed port's
    /// mailbox is momentarily empty: the endpoint is live and bound, the
    /// caller may simply retry. It is deliberately distinct from
    /// [`NotFound`](Self::NotFound) (the endpoint does not exist) so a
    /// receiver can tell "no message yet" from "no such port" without
    /// the distinction leaking any other state.
    WouldBlock = 19,
    /// A request to allocate or grow memory cannot be satisfied because
    /// no backing physical frame (or page-table frame) is available.
    ///
    /// The RustOS equivalent of POSIX `ENOMEM`. Emitted by the anonymous
    /// `mem_map` syscall (`plans/SPAWN.md` SP5) when the kernel cannot map
    /// a fresh region into the caller's address space because physical
    /// frames are exhausted. It is the deterministic, fail-closed result
    /// of out-of-memory: allocation failure is always a `Result`, never a
    /// panic (`AGENTS.md` §4). It is distinct from
    /// [`NoSpace`](Self::NoSpace), which is a *storage* backend running out
    /// of on-disk space.
    OutOfMemory = 20,
}

impl Errno {
    /// Numeric value carried on the ABI.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Recover an [`Errno`] from its ABI numeric value, or `None` if `value`
    /// is not a known discriminant.
    ///
    /// The inverse of [`as_i32`](Self::as_i32) and the single place the
    /// numeric → variant mapping lives (`AGENTS.md` §2.2): a caller decoding
    /// a syscall's signed result (a negative register is `-errno`, the
    /// standard `abi-v1` convention) recovers the `Errno` here rather than
    /// re-listing the discriminants.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::BufferTooSmall),
            2 => Some(Self::BadAlignment),
            3 => Some(Self::BadMagic),
            4 => Some(Self::LengthOutOfRange),
            5 => Some(Self::OutOfRange),
            6 => Some(Self::PermissionDenied),
            7 => Some(Self::NotFound),
            8 => Some(Self::DelegationWiden),
            9 => Some(Self::SignatureInvalid),
            10 => Some(Self::AbiVersionUnsupported),
            11 => Some(Self::MessageTooLarge),
            12 => Some(Self::NotImplemented),
            13 => Some(Self::TimedOut),
            14 => Some(Self::TimestampOutOfRange),
            15 => Some(Self::NoSpace),
            16 => Some(Self::EntropyNotReady),
            17 => Some(Self::AlreadyExists),
            18 => Some(Self::BadAddress),
            19 => Some(Self::WouldBlock),
            20 => Some(Self::OutOfMemory),
            _ => None,
        }
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
            Self::MessageTooLarge => "message too large",
            Self::NotImplemented => "operation not implemented",
            Self::TimedOut => "operation timed out",
            Self::TimestampOutOfRange => "timestamp out of range",
            Self::NoSpace => "no space left on device",
            Self::EntropyNotReady => "entropy not ready",
            Self::AlreadyExists => "object already exists",
            Self::BadAddress => "bad user-space address",
            Self::WouldBlock => "operation would block",
            Self::OutOfMemory => "out of memory",
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
        assert_eq!(Errno::MessageTooLarge.as_i32(), 11);
        assert_eq!(Errno::NotImplemented.as_i32(), 12);
        assert_eq!(Errno::TimedOut.as_i32(), 13);
        assert_eq!(Errno::TimestampOutOfRange.as_i32(), 14);
        assert_eq!(Errno::NoSpace.as_i32(), 15);
        assert_eq!(Errno::EntropyNotReady.as_i32(), 16);
        assert_eq!(Errno::AlreadyExists.as_i32(), 17);
        assert_eq!(Errno::BadAddress.as_i32(), 18);
        assert_eq!(Errno::WouldBlock.as_i32(), 19);
        assert_eq!(Errno::OutOfMemory.as_i32(), 20);
    }

    #[test]
    fn from_i32_round_trips_every_variant() {
        // Every known discriminant decodes back to its variant, and an
        // unknown value (0 / out of range) is rejected rather than guessed.
        for errno in [
            Errno::BufferTooSmall,
            Errno::BadAlignment,
            Errno::BadMagic,
            Errno::LengthOutOfRange,
            Errno::OutOfRange,
            Errno::PermissionDenied,
            Errno::NotFound,
            Errno::DelegationWiden,
            Errno::SignatureInvalid,
            Errno::AbiVersionUnsupported,
            Errno::MessageTooLarge,
            Errno::NotImplemented,
            Errno::TimedOut,
            Errno::TimestampOutOfRange,
            Errno::NoSpace,
            Errno::EntropyNotReady,
            Errno::AlreadyExists,
            Errno::BadAddress,
            Errno::WouldBlock,
            Errno::OutOfMemory,
        ] {
            assert_eq!(Errno::from_i32(errno.as_i32()), Some(errno));
        }
        assert_eq!(Errno::from_i32(0), None);
        assert_eq!(Errno::from_i32(21), None);
        assert_eq!(Errno::from_i32(-1), None);
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
