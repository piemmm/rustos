//! Strictly justified shared utilities.
//!
//! Per every item in this crate must be used by at least
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
//! * [`size`] — GNU coreutils-style block-size parsing and human-readable
//!   size formatting, consumed by the `du` and `df` command apps
//!   (`plans/APPS.md`), so the size grammar and ceiling-rounding rules
//!   exist in exactly one place.
//!
//! The flat device-tree (FDT) parser that once lived here has been folded
//! into the single shared `lib/fdt` reader, which now owns the generic
//! node-iteration API every device-tree consumer uses.
//!
//! Promoting code into this crate requires a `PLAN.md` note documenting
//! the two-or-more concrete callers.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod fmt;
pub mod size;
