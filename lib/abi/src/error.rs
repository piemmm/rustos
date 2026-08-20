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
/// for a TAIRiX [`Errno`].
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
    /// example converting a `Time64` to a narrower on-disk timestamp encoding. The conversion is always checked; this errno is the
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
    /// Emitted only by the random API and only when the
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
    /// registration: the existing live port is never overwritten, and the caller's freshly-created port is
    /// handed back so it can be torn down. Also emitted by
    /// `display_acquire` when the caller already holds the seat lease it
    /// is asking for: a double acquire is a caller bug, surfaced rather
    /// than silently succeeding (`plans/DISPLAY.md`).
    AlreadyExists = 17,
    /// A user-space pointer handed to a syscall does not name memory the
    /// caller may access in the direction the call requires.
    ///
    /// The TAIRiX equivalent of POSIX `EFAULT`. Emitted by any syscall
    /// that copies through the kernel's `copy_from_user` / `copy_to_user`
    /// boundary when the user buffer is null, runs off
    /// the end of the address space, is unmapped, is not a user page, or
    /// lacks the read/write permission the copy direction needs (the
    /// W^X guard refuses writing an executable page). The kernel
    /// returns this one code for every such failure rather than reporting
    /// *which* invariant broke, so a faulting pointer cannot be used as an
    /// oracle to probe the kernel's memory layout (fail
    /// closed;). It is also the fail-closed result when the caller
    /// has no registered address space at all (e.g. a kernel task).
    BadAddress = 18,
    /// A non-blocking operation has nothing to return right now and
    /// would have to block to make progress.
    ///
    /// The TAIRiX equivalent of POSIX `EAGAIN` / `EWOULDBLOCK`. Emitted
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
    /// The TAIRiX equivalent of POSIX `ENOMEM`. Emitted by the anonymous
    /// `mem_map` syscall (`plans/SPAWN.md` SP5) when the kernel cannot map
    /// a fresh region into the caller's address space because physical
    /// frames are exhausted. It is the deterministic, fail-closed result
    /// of out-of-memory: allocation failure is always a `Result`, never a
    /// panic. It is distinct from
    /// [`NoSpace`](Self::NoSpace), which is a *storage* backend running out
    /// of on-disk space.
    OutOfMemory = 20,
    /// A rename names a source and destination on different mounted volumes.
    ///
    /// The TAIRiX equivalent of POSIX `EXDEV`. Emitted by the `fs_rename`
    /// syscall when the two paths do not resolve under the same mounted
    /// volume: a rename preserves the node's identity, which cannot span two
    /// independent backings. It is deliberately distinct from the generic
    /// path-validation errnos so a mover (`mv`) can fall back to the POSIX
    /// copy-then-remove relocation on exactly this condition and no other.
    CrossVolume = 21,
    /// A path names a non-directory where a directory is required.
    ///
    /// The TAIRiX equivalent of POSIX `ENOTDIR`. Emitted by the filesystem
    /// syscalls when an operation that only applies to a directory reaches a
    /// file — a directory-only `fs_unlink`
    /// ([`UnlinkFlags::DIRECTORY`](crate::UnlinkFlags::DIRECTORY), the
    /// `rmdir` guarantee) naming a file, or a path that uses a file as an
    /// intermediate component. It is deliberately distinct from the generic
    /// path-validation errno so `rmdir` can report the GNU "Not a
    /// directory" diagnostic truthfully, never by guessing.
    NotADirectory = 22,
    /// A directory cannot be removed because it still has entries.
    ///
    /// The TAIRiX equivalent of POSIX `ENOTEMPTY`. Emitted by `fs_unlink`
    /// when the named directory is not empty: removal never recurses
    /// implicitly, so a populated directory fails closed with this code. It
    /// is deliberately distinct from the generic path-validation errnos so
    /// `rmdir --ignore-fail-on-non-empty` can tolerate exactly this
    /// condition and no other.
    NotEmpty = 23,
    /// Another task holds the seat (the display with its keyboard and
    /// pointer), so the requested acquire is refused rather than
    /// displacing the holder (`plans/DISPLAY.md`).
    ///
    /// Emitted by `display_acquire` when the seat's recorded owner is a
    /// different task: ownership is exclusive and never stolen, even
    /// between two principals that both hold `CAP_DISPLAY`. The refused
    /// caller may retry after the holder releases.
    SeatBusy = 24,
    /// The caller is not the recorded owner of the seat it tried to
    /// operate on (`plans/DISPLAY.md`).
    ///
    /// Emitted by `display_release` and the owner-gated seat paths (the
    /// desktop keyboard drain `keyboard_read`) when the kernel-attested
    /// caller does not hold the seat's live lease — the seat is unowned,
    /// or another task holds it. A release is owner-checked, never a
    /// global "anyone may flip it back" switch.
    SeatNotOwner = 25,
    /// The caller's seat lease was forcibly revoked by an administrator
    /// (`plans/DISPLAY.md`).
    ///
    /// The distinct refusal an evicted seat owner sees on its next
    /// owner-gated call, so a well-behaved compositor learns it lost the
    /// seat rather than scribbling over the new foreground. Deliberately
    /// distinct from [`SeatNotOwner`](Self::SeatNotOwner): the lease did
    /// not lapse by the caller's own action. A fresh `display_acquire`
    /// (including the evicted task's explicit reacquire) clears the
    /// condition.
    SeatRevoked = 26,
    /// The caller is not the controlling (foreground) owner of the text
    /// console it tried to control (`plans/DISPLAY.md`).
    ///
    /// Emitted by the foreground-gated console paths — `stream_read`,
    /// `stream_input_mode`, and a `console_foreground` transition attempted
    /// by a task that is neither the console's recorded owner nor the task
    /// that granted the current ownership. Only the foreground owner drains
    /// a console's input queue or changes its line discipline; a background
    /// reader is refused with this code instead of being stopped by an
    /// asynchronous signal, so there is no `SIGTTIN`-style race to exploit.
    NotForeground = 27,
    /// A write was issued to a pipe whose every read end is closed.
    ///
    /// The TAIRiX equivalent of POSIX `EPIPE`. Emitted by a write to a
    /// pipe end (`plans/SPAWN.md` SP10) when no reader remains: the bytes
    /// can never be consumed, so the writer learns to stop — this is how a
    /// `yes | head` pipeline terminates its producer. Deliberately distinct
    /// from [`NotFound`](Self::NotFound) (the descriptor itself is still
    /// open and owned by the caller) and from
    /// [`WouldBlock`](Self::WouldBlock) (retrying can never succeed).
    BrokenPipe = 28,
    /// A device endpoint answered the transfer with a protocol STALL.
    ///
    /// Emitted by the USB URB transport (`crate::usb_urb`) when the device
    /// halts the addressed endpoint instead of moving data — for a
    /// mass-storage device this is an in-band protocol signal (USB BOT uses
    /// a bulk-IN STALL to reject a phase), not a hardware failure, so a
    /// class driver must be able to tell it apart from
    /// [`DriverError::DeviceFault`](crate::DriverError::DeviceFault)'s
    /// unrecoverable fault and run its own recovery. The host-controller
    /// driver has already recovered the endpoint (cleared the halt and
    /// repositioned the ring) by the time this code is delivered; the
    /// caller may submit fresh transfers immediately.
    EndpointStalled = 29,
    /// The device behind the requested operation faulted.
    ///
    /// The client-visible form of
    /// [`DriverError::DeviceFault`](crate::DriverError::DeviceFault): the
    /// hardware (or its driver) rejected an operation that was otherwise
    /// well-formed and authorised — a display service's scan-out engine
    /// refusing a present, a controller failing mid-transfer. Deliberately
    /// distinct from [`WouldBlock`](Self::WouldBlock) (a retry can
    /// legitimately succeed once the device is idle) and from
    /// [`NotImplemented`](Self::NotImplemented) (the operation itself is
    /// supported); a caller treats it as the device being unhealthy, not
    /// as a protocol error of its own making.
    DeviceFault = 30,
    /// The object exists but carries no data under the requested name.
    ///
    /// Semantically equivalent to POSIX `ENODATA`. Emitted by `fs_attr_get`
    /// when the file or directory at the path exists (and the caller may
    /// read it) but no extended attribute with the requested key is stored
    /// on it — deliberately distinct from [`NotFound`](Self::NotFound),
    /// which reports that the *path itself* does not resolve. A value may
    /// legitimately be zero bytes long, so absence is reported as this
    /// errno, never as an empty read.
    NoData = 31,
    /// The operation is well-formed on the ABI but the object's backing
    /// cannot support it.
    ///
    /// Semantically equivalent to POSIX `ENOTSUP`. Emitted by the
    /// `fs_attr_*` syscalls when the mounted filesystem's on-disk format
    /// has nowhere to store extended attributes (FAT32, ext4 today) —
    /// deliberately distinct from
    /// [`NotImplemented`](Self::NotImplemented) (the kernel subsystem is
    /// not wired in at all): the subsystem is live, this particular
    /// backing simply cannot represent the request, and retrying can
    /// never succeed on that mount.
    NotSupported = 32,
    /// A blocking wait was cut short because the calling task has a
    /// pending termination.
    ///
    /// The TAIRiX analogue of POSIX `EINTR`, deliberately narrower: it is
    /// emitted only by the kernel's in-kernel park loops (a pipe or
    /// console read, `waitset_wait`, `irq_wait`, a blocking `wait`, …)
    /// when a `Terminate`/`Kill` was deferred against the parked caller,
    /// so the syscall unwinds — releasing everything it holds — and the
    /// task exits at its syscall boundary. A user program never observes
    /// this code: the kernel lands the pending kill before the result
    /// could return to user space. It exists on the ABI so the decode
    /// table is total and diagnostics stay honest.
    Interrupted = 33,
    /// A local address or port is already in use by another socket.
    ///
    /// The TAIRiX equivalent of POSIX `EADDRINUSE`. Emitted by the socket
    /// service when a `bind` names a local port already held by another
    /// socket of the same principal, or an ephemeral-port draw cannot find a
    /// free port. Fail-closed: the existing binding is never displaced.
    AddressInUse = 34,
    /// A requested local address is not held by any interface.
    ///
    /// The TAIRiX equivalent of POSIX `EADDRNOTAVAIL`. Emitted by the socket
    /// service when a `bind` names a specific local address that no managed
    /// interface owns, so no source address could ever be chosen for it.
    AddressUnavailable = 35,
    /// No route exists to the requested destination.
    ///
    /// The TAIRiX equivalent of POSIX `ENETUNREACH`. Emitted by the socket
    /// service when a datagram's destination has no matching route (no
    /// on-link prefix and no default router), or no usable source address of
    /// the destination's family is configured.
    NetworkUnreachable = 36,
    /// A send omitted a destination on a socket with no connected peer.
    ///
    /// The TAIRiX equivalent of POSIX `EDESTADDRREQ`/`ENOTCONN`. Emitted by
    /// the socket service when a `send` supplies no destination and the
    /// socket has never been `connect`ed to a default peer.
    NotConnected = 37,
    /// A bounded, per-principal resource limit has been reached.
    ///
    /// Emitted when a caller's share of a bounded resource is exhausted —
    /// the socket service refusing to open another socket once the
    /// principal's socket quota is full. Distinct from
    /// [`OutOfMemory`](Self::OutOfMemory) (a genuine allocation failure): the
    /// system is healthy, this principal has simply reached its accounted
    /// ceiling and must release before it may allocate more (fail closed).
    LimitExceeded = 38,
    /// The medium reported a permanent, unrecoverable read/write error.
    ///
    /// The TAIRiX equivalent of a SCSI `MEDIUM ERROR` sense: a bad sector
    /// the device could not read, reallocate, or recover. Distinct from
    /// [`DeviceFault`](Self::DeviceFault) (the whole device faulted) and
    /// from the retryable transient classes: the data at that block is
    /// gone, so the block layer surfaces it as a hard I/O error rather
    /// than reissuing the transfer. Emitted on the block-service
    /// completion seam ([`crate::blkio`]).
    MediumError = 39,
    /// The device is present but unresponsive, or has been surprise-removed.
    ///
    /// The block-service health axis ([`crate::blkio::BlkStatus`]) reports
    /// a device that is offline or removed with this code, distinct from a
    /// transient stall (retryable) and from a permanent medium error (the
    /// device is fine, one block is not): the target itself is gone, so the
    /// block layer surfaces it as a hard I/O error to that device's callers
    /// while leaving every other device untouched.
    DeviceOffline = 40,
    /// A device or resource is busy and the requested operation is refused.
    ///
    /// The TAIRiX equivalent of POSIX `EBUSY`. Emitted when an orderly
    /// `hw_remove_node` is asked to retire a still-present device (stopping
    /// an assembled RAID array) while a volume is still attached on a
    /// block-service endpoint the node declares: turning a live mounted
    /// volume into a surprise-removal event is refused, and nothing is
    /// removed (fail closed). A surprise removal, by contrast, never returns
    /// this code — a vanished device cannot be kept alive by pretending its
    /// volumes are still there.
    Busy = 41,
    /// A path could not be resolved because it traverses too many symbolic
    /// links, or because a link was found where the caller forbade one.
    ///
    /// The TAIRiX equivalent of POSIX `ELOOP`. Emitted when per-component
    /// resolution exceeds its hop budget (a link cycle, or a chain longer
    /// than the bound), when the resolved path would exceed
    /// [`crate::fs::FS_PATH_MAX`], and when an open carrying
    /// [`crate::fs::OpenFlags::NO_FOLLOW`] finds the final component really
    /// is a link and byte access was asked for. Resolution stops and nothing
    /// is opened, read, or written (fail closed) — a cycle is never walked
    /// until the kernel runs out of stack.
    LinkLoop = 42,
}

impl Errno {
    /// Numeric value carried on the ABI.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Recover the [`Errno`] a syscall encoded as a negative signed result
    /// (`-errno`, the standard `abi-v1` convention).
    ///
    /// The one definition of that decode, so no caller re-derives it. A
    /// non-negative `ret` is not an error and an unknown code cannot be
    /// guessed at: both fail closed as
    /// [`NotImplemented`](Self::NotImplemented).
    #[must_use]
    pub fn from_syscall(ret: i64) -> Self {
        i32::try_from(-ret)
            .ok()
            .and_then(Self::from_i32)
            .unwrap_or(Self::NotImplemented)
    }

    /// Recover an [`Errno`] from its ABI numeric value, or `None` if `value`
    /// is not a known discriminant.
    ///
    /// The inverse of [`as_i32`](Self::as_i32) and the single place the
    /// numeric → variant mapping lives: a caller decoding
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
            21 => Some(Self::CrossVolume),
            22 => Some(Self::NotADirectory),
            23 => Some(Self::NotEmpty),
            24 => Some(Self::SeatBusy),
            25 => Some(Self::SeatNotOwner),
            26 => Some(Self::SeatRevoked),
            27 => Some(Self::NotForeground),
            28 => Some(Self::BrokenPipe),
            29 => Some(Self::EndpointStalled),
            30 => Some(Self::DeviceFault),
            31 => Some(Self::NoData),
            32 => Some(Self::NotSupported),
            33 => Some(Self::Interrupted),
            34 => Some(Self::AddressInUse),
            35 => Some(Self::AddressUnavailable),
            36 => Some(Self::NetworkUnreachable),
            37 => Some(Self::NotConnected),
            38 => Some(Self::LimitExceeded),
            39 => Some(Self::MediumError),
            40 => Some(Self::DeviceOffline),
            41 => Some(Self::Busy),
            42 => Some(Self::LinkLoop),
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
            Self::CrossVolume => "paths on different volumes",
            Self::NotADirectory => "not a directory",
            Self::NotEmpty => "directory not empty",
            Self::SeatBusy => "seat held by another task",
            Self::SeatNotOwner => "not the seat owner",
            Self::SeatRevoked => "seat lease revoked",
            Self::NotForeground => "not the console foreground owner",
            Self::BrokenPipe => "broken pipe",
            Self::EndpointStalled => "endpoint stalled",
            Self::DeviceFault => "device fault",
            Self::NoData => "no such attribute",
            Self::NotSupported => "not supported by the backing",
            Self::Interrupted => "wait interrupted by pending termination",
            Self::AddressInUse => "address already in use",
            Self::AddressUnavailable => "address not available",
            Self::NetworkUnreachable => "network unreachable",
            Self::NotConnected => "socket not connected",
            Self::LimitExceeded => "resource limit exceeded",
            Self::MediumError => "permanent medium error",
            Self::DeviceOffline => "device offline or removed",
            Self::Busy => "device or resource busy",
            Self::LinkLoop => "too many symbolic links in path resolution",
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
        assert_eq!(Errno::CrossVolume.as_i32(), 21);
        assert_eq!(Errno::NotADirectory.as_i32(), 22);
        assert_eq!(Errno::NotEmpty.as_i32(), 23);
        assert_eq!(Errno::SeatBusy.as_i32(), 24);
        assert_eq!(Errno::SeatNotOwner.as_i32(), 25);
        assert_eq!(Errno::SeatRevoked.as_i32(), 26);
        assert_eq!(Errno::NotForeground.as_i32(), 27);
        assert_eq!(Errno::BrokenPipe.as_i32(), 28);
        assert_eq!(Errno::EndpointStalled.as_i32(), 29);
        assert_eq!(Errno::DeviceFault.as_i32(), 30);
        assert_eq!(Errno::NoData.as_i32(), 31);
        assert_eq!(Errno::NotSupported.as_i32(), 32);
        assert_eq!(Errno::Interrupted.as_i32(), 33);
        assert_eq!(Errno::AddressInUse.as_i32(), 34);
        assert_eq!(Errno::AddressUnavailable.as_i32(), 35);
        assert_eq!(Errno::NetworkUnreachable.as_i32(), 36);
        assert_eq!(Errno::NotConnected.as_i32(), 37);
        assert_eq!(Errno::LimitExceeded.as_i32(), 38);
        assert_eq!(Errno::MediumError.as_i32(), 39);
        assert_eq!(Errno::DeviceOffline.as_i32(), 40);
        assert_eq!(Errno::Busy.as_i32(), 41);
        assert_eq!(Errno::LinkLoop.as_i32(), 42);
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
            Errno::CrossVolume,
            Errno::NotADirectory,
            Errno::NotEmpty,
            Errno::SeatBusy,
            Errno::SeatNotOwner,
            Errno::SeatRevoked,
            Errno::NotForeground,
            Errno::BrokenPipe,
            Errno::EndpointStalled,
            Errno::DeviceFault,
            Errno::NoData,
            Errno::NotSupported,
            Errno::Interrupted,
            Errno::AddressInUse,
            Errno::AddressUnavailable,
            Errno::NetworkUnreachable,
            Errno::NotConnected,
            Errno::LimitExceeded,
            Errno::MediumError,
            Errno::DeviceOffline,
            Errno::Busy,
            Errno::LinkLoop,
        ] {
            assert_eq!(Errno::from_i32(errno.as_i32()), Some(errno));
        }
        assert_eq!(Errno::from_i32(0), None);
        assert_eq!(Errno::from_i32(43), None);
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
