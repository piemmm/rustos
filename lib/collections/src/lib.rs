//! Shared `no_std` collections that are not in `core` or `alloc`.
//!
//! `alloc` already gives TAIRiX `Vec`, `String`, `BTreeMap`, `BTreeSet`,
//! `VecDeque`, and `BinaryHeap`, and this crate does not re-implement any of
//! them. It holds the containers a kernel runs on that `alloc` has no answer
//! for, each with a caller in the tree (`plans/COLLECTIONS.md` is the ledger
//! of what is landed and what is still to come).
//!
//! ## Inventory
//!
//! * [`HashMap`] / [`HashSet`] — expected constant-time unordered lookup, open
//!   addressed over control-byte groups. What `BTreeMap` was standing in for
//!   wherever a key order was never wanted.
//! * [`ArrayVec`] / [`ArrayString`] — a vector and a UTF-8 string of a
//!   caller-chosen compile-time bound, allocating nothing. What an ad-hoc
//!   `[T; N]` paired with a length field was standing in for.
//! * [`SmallVec`] — inline while it is small, spilling to the heap beyond
//!   that, for the paths that carry a handful of elements and pay an
//!   allocation for it.
//! * [`RingBuf`] — a fixed-capacity circular queue, constant time at both
//!   ends. What every hand-rolled type-ahead buffer, diagnostic tail, and
//!   driver hand-off ring was standing in for. [`SecretRing`] is the same
//!   queue for one a credential transits, scrubbing each slot it vacates.
//! * [`BitSet256`] — a fixed 256-bit set, holding a process's capability
//!   membership (`lib/caps`).
//!
//! ## Rules every container here obeys
//!
//! * **Nothing that can fail panics.** Every allocating operation has a
//!   fallible form returning [`TryReserveError`]; no map has an `Index`
//!   implementation, because a subscript that panics on a missing key has no
//!   place in a kernel.
//! * **No allocation on a read path.** Lookup, iteration, and removal allocate
//!   nothing; growth is amortised.
//! * **No fixed capacity ceiling.** A heap-backed container grows on demand
//!   and fails closed only on genuine exhaustion. A const-generic capacity
//!   appears only where the container is deliberately allocation-free and the
//!   bound is the caller's own ([`BitSet256`]).
//! * **Order is unspecified unless the container says otherwise.** A hash
//!   container's iteration order varies with the hash key and the insertion
//!   history; anything compared, logged, or reproduced wants an ordered
//!   container.
//! * **Secret hygiene is the holder's job.** A container does not scrub the
//!   slots it frees — reuse inside one address space is not a security
//!   boundary — so a holder of a key, credential, or capability token stores a
//!   value type that zeroes itself on drop, exactly as `lib/rt`'s heap
//!   already requires. The one exception is a value that merely *transits* a
//!   long-lived kernel buffer, which belongs in a [`SecretRing`].
//! * **A capacity fault answers with [`Result`], an index fault with
//!   [`Option`], and no operation carries both.** That is why the
//!   fixed-capacity vectors offer no positional insert: its two failure modes
//!   — no room, and an index past the end — are unrelated, and no single error
//!   spells them honestly. `push`, `pop`, `remove`, `swap_remove`,
//!   `truncate`, and `retain` cover every in-tree need without the
//!   ambiguity.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

use core::fmt;

pub mod arraystring;
pub mod arrayvec;
pub mod bitset;
pub mod group;
pub mod map;
mod raw;
pub mod ringbuf;
pub mod set;
pub mod smallvec;

pub use arraystring::ArrayString;
pub use arrayvec::ArrayVec;
pub use bitset::{BitSet256, BitSet256Iter};
pub use map::HashMap;
pub use ringbuf::{RingBuf, SecretRing};
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

/// A fixed-capacity container refused a value, handing it back.
///
/// Distinct from [`TryReserveError`]: nothing failed to allocate, the bound the
/// caller chose at the use site is simply reached. The refused value travels in
/// the error so a fallible push never destroys what it would not take; the
/// `T = ()` form is for the bulk pushes, where the caller still holds the
/// slice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CapacityError<T = ()>(T);

impl<T> CapacityError<T> {
    /// Wrap the value the container would not take.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Take back the refused value.
    #[must_use]
    pub fn into_value(self) -> T {
        self.0
    }

    /// Borrow the refused value.
    #[must_use]
    pub const fn value(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Display for CapacityError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fixed capacity reached")
    }
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
mod test_support;

#[cfg(test)]
#[path = "map_tests.rs"]
mod map_tests;
