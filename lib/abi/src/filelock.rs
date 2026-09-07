//! Advisory byte-range file locks: the `abi-v1` vocabulary the
//! [`FS_LOCK`](crate::SyscallNumber::FS_LOCK) and
//! [`FS_LOCK_QUERY`](crate::SyscallNumber::FS_LOCK_QUERY) syscalls speak.
//!
//! # What a lock is, and what it is not
//!
//! A lock is a **coordination** primitive between processes that agree to
//! use it. It confers no authority and withholds none: the access-control
//! boundary is the per-inode owner/mode/ACL/capability model, and a
//! principal that may write a file may still write a range another owner has
//! locked. What the lock guarantees is that two participants who both take
//! one never believe they hold the same range at once.
//!
//! Enforcing locks against non-participants — POSIX mandatory locking —
//! is deliberately absent. It would hand any principal with write permission
//! a way to stall every other reader of a file indefinitely, which is
//! authority the file's own permissions never granted.
//!
//! # The owner is the open file description
//!
//! A lock belongs to the *open file description* behind the descriptor that
//! took it, not to the process and not to the descriptor number. So:
//!
//! * Two descriptors on one description — a spawn-inherited standard stream,
//!   a duplicated handle — share the description's locks, and neither
//!   conflicts with the other.
//! * Two separate opens of the same file are two owners and *do* conflict,
//!   even within one process or one thread.
//! * The locks release when the last descriptor on the description closes,
//!   which a process exit does for every description it still holds.
//!
//! This is the model POSIX record locks got wrong: theirs are owned by the
//! process, so closing *any* descriptor for the file drops them all, and two
//! threads coordinating through one file cannot use them at all.

use crate::Errno;

/// The `len` spelling meaning "from `start` to the end of the address
/// space", so a range that grows with the file needs no relocking.
pub const LOCK_LEN_TO_END: u64 = 0;

/// The `timeout_ns` spelling meaning "wait indefinitely".
pub const LOCK_WAIT_FOREVER: u64 = u64::MAX;

/// What a request asks of a range.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum LockMode {
    /// Shared: coexists with other owners' shared locks, excludes every
    /// other owner's exclusive lock. The reader's mode.
    Shared = 0,
    /// Exclusive: excludes every other owner's lock of either mode. The
    /// writer's mode.
    Exclusive = 1,
    /// Release whatever this owner holds over the range, whole or in part.
    Unlock = 2,
}

impl LockMode {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a raw discriminant, failing closed on an unassigned value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `raw` names no mode.
    pub const fn from_u32(raw: u32) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Shared),
            1 => Ok(Self::Exclusive),
            2 => Ok(Self::Unlock),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Whether two owners may both hold these modes over one byte.
    ///
    /// The one definition of the conflict rule, so the kernel's acquire
    /// path and its query path can never disagree about what conflicts.
    #[must_use]
    pub const fn compatible_with(self, other: Self) -> bool {
        matches!((self, other), (Self::Shared, Self::Shared))
    }
}

/// A half-open byte range of a file, `[start, start + len)`, with
/// [`LOCK_LEN_TO_END`] for "to the end of the address space".
///
/// Held internally as `start` plus an **inclusive** end so the unbounded
/// form is an ordinary value (`u64::MAX`) rather than a special case every
/// comparison has to remember, and so no overlap test can overflow.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LockRange {
    start: u64,
    end_inclusive: u64,
}

impl LockRange {
    /// The whole file, now and however far it later grows.
    pub const WHOLE: Self = Self {
        start: 0,
        end_inclusive: u64::MAX,
    };

    /// The range `[start, start + len)`, where `len` of
    /// [`LOCK_LEN_TO_END`] runs to the end of the address space.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if a bounded range would end past `u64::MAX`.
    pub const fn new(start: u64, len: u64) -> Result<Self, Errno> {
        if len == LOCK_LEN_TO_END {
            return Ok(Self {
                start,
                end_inclusive: u64::MAX,
            });
        }
        match start.checked_add(len - 1) {
            Some(end_inclusive) => Ok(Self {
                start,
                end_inclusive,
            }),
            None => Err(Errno::OutOfRange),
        }
    }

    /// The range spanning `[start, end_inclusive]` directly.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `end_inclusive` precedes `start`.
    pub const fn between(start: u64, end_inclusive: u64) -> Result<Self, Errno> {
        if end_inclusive < start {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            start,
            end_inclusive,
        })
    }

    /// First byte covered.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Last byte covered.
    #[must_use]
    pub const fn end_inclusive(self) -> u64 {
        self.end_inclusive
    }

    /// The `len` spelling this range reports on the wire
    /// ([`LOCK_LEN_TO_END`] when unbounded).
    #[must_use]
    pub const fn wire_len(self) -> u64 {
        if self.end_inclusive == u64::MAX {
            LOCK_LEN_TO_END
        } else {
            self.end_inclusive - self.start + 1
        }
    }

    /// Whether the two ranges share at least one byte.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.start <= other.end_inclusive && other.start <= self.end_inclusive
    }

    /// Whether the two ranges meet end to end with no byte between them, so
    /// one record could cover both.
    #[must_use]
    pub const fn abuts(self, other: Self) -> bool {
        match self.end_inclusive.checked_add(1) {
            Some(next) if next == other.start => true,
            _ => matches!(other.end_inclusive.checked_add(1), Some(next) if next == self.start),
        }
    }

    /// The smallest range covering both, which is only a coherent answer
    /// when they overlap or abut.
    #[must_use]
    pub const fn joined(self, other: Self) -> Self {
        let start = if self.start < other.start {
            self.start
        } else {
            other.start
        };
        let end_inclusive = if self.end_inclusive > other.end_inclusive {
            self.end_inclusive
        } else {
            other.end_inclusive
        };
        Self {
            start,
            end_inclusive,
        }
    }
}

/// Behaviour flags a lock request carries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LockFlags(u32);

impl LockFlags {
    /// Refuse rather than wait when the range is already held
    /// incompatibly, reporting [`Errno::WouldBlock`].
    pub const NONBLOCK: Self = Self(1 << 0);

    /// Every assigned bit, so an unknown bit can be refused.
    const ALL: u32 = Self::NONBLOCK.0;

    /// No flags: a blocking request.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw bits as carried on the ABI.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Decode raw bits, failing closed on any unassigned bit.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `bits` sets a bit this version does not
    /// define.
    pub const fn from_bits(bits: u32) -> Result<Self, Errno> {
        if bits & !Self::ALL != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(bits))
    }

    /// Whether `other`'s bits are all set here.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the request must not wait.
    #[must_use]
    pub const fn is_nonblock(self) -> bool {
        self.contains(Self::NONBLOCK)
    }
}

/// One lock standing in the way of a request, as
/// [`FS_LOCK_QUERY`](crate::SyscallNumber::FS_LOCK_QUERY) reports it.
///
/// `pid` is the process that acquired the lock. It is the whole point of
/// the query — "waiting on pid 412" is actionable where "busy" is not — and
/// it discloses nothing the caller could not already learn, since it must
/// hold an authorised descriptor on the file to ask. A description shared
/// across processes reports the one that took the lock, which is a fact
/// rather than a guess about who may release it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LockConflict {
    /// The mode the conflicting owner holds, never [`LockMode::Unlock`].
    pub mode: LockMode,
    /// First byte the conflicting lock covers.
    pub start: u64,
    /// Its length, [`LOCK_LEN_TO_END`] when it runs to the end.
    pub len: u64,
    /// The process that acquired it.
    pub pid: u64,
}

impl LockConflict {
    /// Byte length on the wire.
    pub const WIRE_LEN: usize = 32;

    /// The little-endian wire encoding.
    #[must_use]
    pub fn to_le_bytes(self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.mode.as_u32().to_le_bytes());
        // Bytes 4..8 are reserved padding, so the `u64`s that follow sit at
        // their natural alignment in the caller's buffer.
        out[8..16].copy_from_slice(&self.start.to_le_bytes());
        out[16..24].copy_from_slice(&self.len.to_le_bytes());
        out[24..32].copy_from_slice(&self.pid.to_le_bytes());
        out
    }

    /// Decode the wire encoding.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a mode discriminant this version does not
    /// define, or reserved padding a sender did not zero.
    pub fn from_le_bytes(raw: &[u8; Self::WIRE_LEN]) -> Result<Self, Errno> {
        let word = |at: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&raw[at..at + 8]);
            u64::from_le_bytes(bytes)
        };
        let mut mode_bytes = [0u8; 4];
        mode_bytes.copy_from_slice(&raw[0..4]);
        let mut pad = [0u8; 4];
        pad.copy_from_slice(&raw[4..8]);
        if pad != [0u8; 4] {
            return Err(Errno::OutOfRange);
        }
        let mode = LockMode::from_u32(u32::from_le_bytes(mode_bytes))?;
        if matches!(mode, LockMode::Unlock) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            mode,
            start: word(8),
            len: word(16),
            pid: word(24),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_discriminants_round_trip_and_reject_the_unassigned() {
        for mode in [LockMode::Shared, LockMode::Exclusive, LockMode::Unlock] {
            assert_eq!(LockMode::from_u32(mode.as_u32()), Ok(mode));
        }
        assert_eq!(LockMode::from_u32(3), Err(Errno::OutOfRange));
        assert_eq!(LockMode::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn only_two_shared_holders_are_compatible() {
        assert!(LockMode::Shared.compatible_with(LockMode::Shared));
        assert!(!LockMode::Shared.compatible_with(LockMode::Exclusive));
        assert!(!LockMode::Exclusive.compatible_with(LockMode::Shared));
        assert!(!LockMode::Exclusive.compatible_with(LockMode::Exclusive));
    }

    #[test]
    fn a_zero_length_runs_to_the_end_of_the_address_space() {
        let range = LockRange::new(4096, LOCK_LEN_TO_END).expect("unbounded");
        assert_eq!(range.start(), 4096);
        assert_eq!(range.end_inclusive(), u64::MAX);
        assert_eq!(range.wire_len(), LOCK_LEN_TO_END);
        assert_eq!(LockRange::WHOLE.start(), 0);
        assert_eq!(LockRange::WHOLE.end_inclusive(), u64::MAX);
    }

    #[test]
    fn a_bounded_range_keeps_its_length_across_the_wire_spelling() {
        let range = LockRange::new(10, 5).expect("bounded");
        assert_eq!(range.start(), 10);
        assert_eq!(range.end_inclusive(), 14);
        assert_eq!(range.wire_len(), 5);
    }

    #[test]
    fn the_last_byte_of_the_address_space_is_lockable_but_past_it_is_not() {
        let last = LockRange::new(u64::MAX, 1).expect("the final byte is a real range");
        assert_eq!(last.start(), u64::MAX);
        assert_eq!(last.end_inclusive(), u64::MAX);
        assert_eq!(
            LockRange::new(u64::MAX, 2),
            Err(Errno::OutOfRange),
            "a range that would end past the address space is refused, not wrapped"
        );
        assert_eq!(LockRange::new(2, u64::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn between_refuses_an_inverted_span() {
        assert_eq!(LockRange::between(5, 4), Err(Errno::OutOfRange));
        assert_eq!(
            LockRange::between(5, 5).map(LockRange::wire_len),
            Ok(1),
            "a single byte is a legitimate span"
        );
    }

    #[test]
    fn overlap_containment_and_abutment_agree_at_the_boundaries() {
        let a = LockRange::new(0, 10).expect("a");
        let b = LockRange::new(10, 10).expect("b");
        let inner = LockRange::new(2, 3).expect("inner");
        assert!(!a.overlaps(b), "half-open ranges that meet do not overlap");
        assert!(a.abuts(b), "but they do abut");
        assert!(b.abuts(a), "abutment is symmetric");
        assert!(a.overlaps(inner));
        assert_eq!(a.joined(b), LockRange::new(0, 20).expect("joined"));
    }

    #[test]
    fn an_unbounded_range_overlaps_everything_at_or_past_its_start() {
        let tail = LockRange::new(100, LOCK_LEN_TO_END).expect("tail");
        assert!(tail.overlaps(LockRange::new(0, 200).expect("straddles")));
        assert!(!tail.overlaps(LockRange::new(0, 100).expect("ends just before")));
        assert!(tail.overlaps(LockRange::new(u64::MAX, 1).expect("final byte")));
        assert!(!tail.abuts(LockRange::new(0, 99).expect("gap of one")));
        assert!(tail.abuts(LockRange::new(0, 100).expect("meets exactly")));
    }

    #[test]
    fn flag_bits_round_trip_and_an_unknown_bit_fails_closed() {
        assert_eq!(LockFlags::from_bits(0), Ok(LockFlags::empty()));
        assert_eq!(
            LockFlags::from_bits(LockFlags::NONBLOCK.bits()),
            Ok(LockFlags::NONBLOCK)
        );
        assert!(LockFlags::NONBLOCK.is_nonblock());
        assert!(!LockFlags::empty().is_nonblock());
        assert_eq!(LockFlags::from_bits(1 << 1), Err(Errno::OutOfRange));
        assert_eq!(LockFlags::from_bits(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn conflict_records_round_trip_the_wire() {
        let conflict = LockConflict {
            mode: LockMode::Exclusive,
            start: 4096,
            len: 8192,
            pid: 412,
        };
        let raw = conflict.to_le_bytes();
        assert_eq!(LockConflict::from_le_bytes(&raw), Ok(conflict));
    }

    #[test]
    fn a_conflict_decode_refuses_unlock_a_bad_mode_and_dirty_padding() {
        let mut raw = LockConflict {
            mode: LockMode::Shared,
            start: 0,
            len: LOCK_LEN_TO_END,
            pid: 1,
        }
        .to_le_bytes();
        assert!(LockConflict::from_le_bytes(&raw).is_ok());

        raw[0..4].copy_from_slice(&LockMode::Unlock.as_u32().to_le_bytes());
        assert_eq!(
            LockConflict::from_le_bytes(&raw),
            Err(Errno::OutOfRange),
            "`Unlock` names no holder, so it can never describe a conflict"
        );

        raw[0..4].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(LockConflict::from_le_bytes(&raw), Err(Errno::OutOfRange));

        raw[0..4].copy_from_slice(&LockMode::Shared.as_u32().to_le_bytes());
        raw[5] = 1;
        assert_eq!(
            LockConflict::from_le_bytes(&raw),
            Err(Errno::OutOfRange),
            "reserved padding a sender did not zero is refused rather than ignored"
        );
    }
}
