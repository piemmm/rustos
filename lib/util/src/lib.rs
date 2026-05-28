//! Strictly justified shared utilities.
//!
//! Per `AGENTS.md` §2.3 every item in this crate must be used by at least
//! two independent callers outside `lib/util` itself. The crate exists in
//! Stage 1 as the *destination* for such items as they are extracted out
//! of subsequent stages — adding speculative helpers ahead of demand is
//! explicitly forbidden.
//!
//! Members:
//!
//! * [`fmt`] — no-allocation numeric formatters for audit-log fields.
//!   Promoted from `kernel/sec` in Stage 2.5 (`kernel/ipc`) once a second
//!   caller materialised; consumed by `kernel/sec` and `kernel/ipc`.
//!
//! Promoting code into this crate requires a `PLAN.md` note documenting
//! the two-or-more concrete callers.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod fmt;
