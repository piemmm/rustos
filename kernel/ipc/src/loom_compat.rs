//! Loom-aware re-exports for the atomic primitives used in this crate.
//!
//! Mirrors the pattern established by `kernel/sync/src/loom_compat.rs`:
//! the production build uses `core::sync::atomic`; under
//! `--cfg loom` the same names resolve to `loom::sync::atomic` so the
//! model checker can interleave them.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
