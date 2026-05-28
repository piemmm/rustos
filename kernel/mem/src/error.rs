//! Allocation error type shared by every allocator in this crate.
//!
//! Per `AGENTS.md` §4 ("Deterministic OOM behaviour: allocation failure
//! is a `Result`, never a panic.") every fallible memory operation
//! returns [`Result<_, AllocError>`]. Callers handle the error explicitly;
//! there is no `unwrap` or `expect` on hot paths.

use core::fmt;

/// Reason an allocation failed.
///
/// The variants are mutually exclusive and stable: kernel code and
/// audit-log writers may match on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AllocError {
    /// No free block of the requested size exists.
    ///
    /// For the frame allocator: no buddy free-list at any order ≥ the
    /// requested order has a block. For the slab allocator: every slab
    /// of the requested class is full.
    OutOfMemory,

    /// The caller asked for a block larger than the allocator supports.
    ///
    /// E.g. requesting `order > MAX_ORDER` from the frame allocator.
    SizeUnsupported,

    /// A zero-sized allocation was requested.
    ///
    /// Zero-sized requests are rejected on purpose: they are almost
    /// always a bug at the call site and a successful return value would
    /// be indistinguishable from a non-zero allocation.
    ZeroSize,

    /// The caller passed an address, frame, or page outside the range
    /// the allocator is responsible for.
    OutOfRange,

    /// The allocator's own backing metadata could not itself be
    /// allocated. This is distinct from [`AllocError::OutOfMemory`]: an
    /// OOM means *user* allocations cannot be satisfied, while
    /// [`AllocError::MetadataAllocFailed`] means the allocator could not
    /// be constructed in the first place.
    MetadataAllocFailed,

    /// The operation would violate an invariant detected by the
    /// allocator (e.g. freeing a frame that is already free, or freeing
    /// a frame that overlaps a reserved region).
    InvariantViolation,
}

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::OutOfMemory => "out of memory",
            Self::SizeUnsupported => "requested size is not supported by this allocator",
            Self::ZeroSize => "zero-sized allocation is not permitted",
            Self::OutOfRange => "address or frame is outside the allocator's range",
            Self::MetadataAllocFailed => "allocator metadata could not be allocated",
            Self::InvariantViolation => "operation would violate an allocator invariant",
        };
        f.write_str(msg)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    extern crate std;
    use std::format;

    #[test]
    fn display_is_stable_and_human_readable() {
        assert_eq!(format!("{}", AllocError::OutOfMemory), "out of memory");
        assert_eq!(
            format!("{}", AllocError::ZeroSize),
            "zero-sized allocation is not permitted"
        );
        assert_eq!(
            format!("{}", AllocError::OutOfRange),
            "address or frame is outside the allocator's range"
        );
        assert!(format!("{}", AllocError::SizeUnsupported).contains("supported"));
        assert!(format!("{}", AllocError::MetadataAllocFailed).contains("metadata"));
        assert!(format!("{}", AllocError::InvariantViolation).contains("invariant"));
    }

    #[test]
    fn copy_and_eq() {
        let a = AllocError::OutOfMemory;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(a, AllocError::ZeroSize);
    }
}
