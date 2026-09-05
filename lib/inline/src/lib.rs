//! Allocation-free containers whose storage lives inline in what the caller
//! already owns.
//!
//! Linking this crate **never requires a global allocator**: it depends on
//! nothing, not even `alloc`. That is what it is for. The layer that runs
//! before a heap exists — an architecture port at CPU reset, the boot console,
//! the early-boot audit ring, a firmware loader — needs containers, and a
//! container crate that links `alloc` would force an allocator requirement on
//! every one of its consumers, whether or not they ever allocate. The
//! heap-backed containers live next door in `tairix-collections`, which
//! depends on this crate for the inline half of its `SmallVec`.
//!
//! ## Inventory
//!
//! * [`ArrayVec`] / [`ArrayString`] — a vector and a UTF-8 string of a
//!   caller-chosen compile-time bound. What an ad-hoc `[T; N]` paired with a
//!   length field was standing in for.
//! * [`RingBuf`] — a fixed-capacity circular queue, constant time at both
//!   ends. What every hand-rolled type-ahead buffer, diagnostic tail, and
//!   driver hand-off ring was standing in for. [`SecretRing`] is the same
//!   queue for one a credential transits, scrubbing each slot it vacates.
//! * [`IntrusiveList`] — a doubly-linked list whose links live in nodes the
//!   caller owns, so an arbitrary node leaves it in constant time with no
//!   search. What every hand-written `next`/`prev` index pair was standing in
//!   for.
//! * [`BitSet256`] — a fixed 256-bit set, holding a process's capability
//!   membership (`lib/caps`).
//!
//! ## Rules every container here obeys
//!
//! * **Nothing allocates, ever.** A capacity is the caller's own bound, chosen
//!   at the use site, and reaching it is answered with [`CapacityError`]
//!   holding the refused value — never a panic and never a heap block.
//! * **A capacity fault answers with [`Result`], an index fault with
//!   [`Option`], and no operation carries both.** That is why the
//!   fixed-capacity vectors offer no positional insert: its two failure modes
//!   — no room, and an index past the end — are unrelated, and no single error
//!   spells them honestly. `push`, `pop`, `remove`, `swap_remove`, `truncate`,
//!   and `retain` cover every in-tree need without the ambiguity.
//! * **Secret hygiene is the holder's job.** A container does not scrub the
//!   slots it frees — reuse inside one address space is not a security
//!   boundary — so a holder of a key, credential, or capability token stores a
//!   value type that zeroes itself on drop, exactly as `lib/rt`'s heap already
//!   requires. The one exception is a value that merely *transits* a
//!   long-lived kernel buffer, which belongs in a [`SecretRing`].
//! * **Usable from interrupt context.** No allocation and no lock means no
//!   path here can block or re-enter an allocator, which is what lets an ISR
//!   hand off through one.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use core::fmt;

pub mod arraystring;
pub mod arrayvec;
pub mod bitset;
pub mod intrusive;
pub mod ringbuf;

pub use arraystring::ArrayString;
pub use arrayvec::ArrayVec;
pub use bitset::{BitSet256, BitSet256Iter};
pub use intrusive::{IntrusiveList, Link, LinkError, LinkStore};
pub use ringbuf::{RingBuf, SecretRing};

/// A fixed-capacity container refused a value, handing it back.
///
/// Nothing failed to allocate — nothing here allocates — the bound the caller
/// chose at the use site is simply reached. The refused value travels in the
/// error so a fallible push never destroys what it would not take; the
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
