//! The heap-backed `no_std` containers that `core` and `alloc` do not
//! provide.
//!
//! `alloc` already gives TAIRiX `Vec`, `String`, `BTreeMap`, `BTreeSet`,
//! `VecDeque`, and `BinaryHeap`, and this crate does not re-implement any of
//! them. It holds the containers a kernel runs on that `alloc` has no answer
//! for, each with a caller in the tree (`plans/COLLECTIONS.md` is the ledger
//! of what is landed and what is still to come).
//!
//! The containers that allocate *nothing* live in `tairix-inline` instead —
//! a crate that links neither this one nor `alloc`, so the layer running
//! before a heap exists can use them. This crate depends on it for the inline
//! half of its [`SmallVec`].
//!
//! ## Inventory
//!
//! * [`HashMap`] / [`HashSet`] — expected constant-time unordered lookup, open
//!   addressed over control-byte groups. What `BTreeMap` was standing in for
//!   wherever a key order was never wanted.
//! * [`SmallVec`] — inline while it is small, spilling to the heap beyond
//!   that, for the paths that carry a handful of elements and pay an
//!   allocation for it.
//!
//! ## Rules every container here obeys
//!
//! * **Nothing that can fail panics.** Every allocating operation has a
//!   fallible form returning [`TryReserveError`]; no map has an `Index`
//!   implementation, because a subscript that panics on a missing key has no
//!   place in a kernel.
//! * **No allocation on a read path.** Lookup, iteration, and removal allocate
//!   nothing; growth is amortised.
//! * **No fixed capacity ceiling.** A container here grows on demand and fails
//!   closed only on genuine exhaustion. A caller-chosen compile-time bound is
//!   the other crate's business.
//! * **Order is unspecified unless the container says otherwise.** A hash
//!   container's iteration order varies with the hash key and the insertion
//!   history; anything compared, logged, or reproduced wants an ordered
//!   container.
//! * **Secret hygiene is the holder's job.** A container does not scrub the
//!   slots it frees — reuse inside one address space is not a security
//!   boundary — so a holder of a key, credential, or capability token stores a
//!   value type that zeroes itself on drop, exactly as `lib/rt`'s heap
//!   already requires.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

use core::fmt;

pub mod group;
pub mod map;
mod raw;
pub mod set;
pub mod smallvec;

pub use map::HashMap;
pub use set::HashSet;
pub use smallvec::SmallVec;

/// Why an allocating container operation could not go ahead.
///
/// `alloc`'s own `TryReserveError` cannot be constructed outside `alloc`, so
/// the fallible surface here carries its own — the two say the same thing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TryReserveError {
    /// The allocator could not supply the block.
    AllocFailed,
    /// The requested capacity has no representable layout.
    CapacityOverflow,
}

impl fmt::Display for TryReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AllocFailed => "allocation failed",
            Self::CapacityOverflow => "requested capacity is not representable",
        })
    }
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod map_tests;
