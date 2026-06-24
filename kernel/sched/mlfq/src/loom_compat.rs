//! Compatibility shim that lets the scheduler be model-checked with the
//! `loom` crate.
//!
//! Under normal builds the primitives use `core::sync::atomic`. When the
//! crate is compiled with `RUSTFLAGS="--cfg loom"` the same names resolve
//! to the corresponding `loom` types so the model checker can drive every
//! interleaving.
//!
//! Mirrors the pattern established in `kernel/sync/src/loom_compat.rs`
//! (no duplication: the scheduler maintains its own
//! shim because `kernel/sync`'s is `pub(crate)` and intentionally not part
//! of its public API).

// Re-exports under `cfg(loom)`.
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{
    fence, AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering,
};

// Re-exports under normal (non-`loom`) builds.
#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{
    fence, AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
