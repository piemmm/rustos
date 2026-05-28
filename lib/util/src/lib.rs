//! Strictly justified shared utilities.
//!
//! Per `AGENTS.md` §2.3 every item in this crate must be used by at least
//! two independent callers outside `lib/util` itself. The crate exists in
//! Stage 1 as the *destination* for such items as they are extracted out
//! of subsequent stages — adding speculative helpers ahead of demand is
//! explicitly forbidden.
//!
//! As of Stage 1 no utility has yet earned its place here:
//!
//! * The 256-bit bitset that Stage 1 introduced has two future callers
//!   (`lib/caps` already; `kernel/sched` planned in Stage 2) so it lives
//!   in [`rustos-collections`](../rustos_collections/index.html), not here.
//! * Endianness helpers in `lib/abi` are used only by ABI decoders and so
//!   stay local to that crate.
//!
//! Promoting code into this crate requires a `PLAN.md` note documenting
//! the two-or-more concrete callers.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]
