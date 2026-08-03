//! Compatibility shim that lets the primitives be model-checked with
//! the `loom` crate.
//!
//! Under normal builds the primitives use `core::sync::atomic` and
//! `core::cell::UnsafeCell`. When the crate is compiled with
//! `RUSTFLAGS="--cfg loom"` the same names resolve to the corresponding
//! `loom` types, which schedule every interleaving for the model checker.
//!
//! The shim is `pub(crate)` only; it is *not* part of the crate's public
//! API. Adding new atomic types here is the only way primitive modules
//! should reach for atomics — going through `core::sync::atomic` directly
//! would silently bypass `loom`.

// Re-exports under `cfg(loom)`.
#[cfg(loom)]
pub(crate) use loom::hint::spin_loop;
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering};

// Re-exports under normal (non-`loom`) builds.
#[cfg(not(loom))]
pub(crate) use core::hint::spin_loop;
#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// A `Sync`-friendly `UnsafeCell` wrapper that mirrors `loom::cell::UnsafeCell`'s
/// `with`/`with_mut` access pattern.
///
/// The `with`/`with_mut` API is what `loom` uses to track pointer accesses
/// for race detection. By going through it unconditionally we get the same
/// access pattern in production code (which is enforced by the borrow
/// checker via the closure) and in `loom` runs (which gets the bookkeeping
/// it needs).
pub(crate) struct SyncUnsafeCell<T: ?Sized> {
    #[cfg(loom)]
    inner: loom::cell::UnsafeCell<T>,
    #[cfg(not(loom))]
    inner: core::cell::UnsafeCell<T>,
}

#[cfg(not(loom))]
impl<T> SyncUnsafeCell<T> {
    /// Constructs a new cell. Available as `const fn` in production builds
    /// so primitives can be used to back `static` globals.
    pub(crate) const fn new(value: T) -> Self {
        Self {
            inner: core::cell::UnsafeCell::new(value),
        }
    }
}

#[cfg(loom)]
impl<T> SyncUnsafeCell<T> {
    /// `loom`'s `UnsafeCell::new` is not `const`, so the `loom` build
    /// exposes the same constructor without the qualifier.
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: loom::cell::UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> SyncUnsafeCell<T> {
    /// Run `f` with a `*const T` pointing at the cell contents.
    #[cfg(not(loom))]
    pub(crate) fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(*const T) -> R,
    {
        f(self.inner.get().cast_const())
    }

    /// Run `f` with a `*const T` pointing at the cell contents (`loom`).
    #[cfg(loom)]
    pub(crate) fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(*const T) -> R,
    {
        self.inner.with(f)
    }

    /// Run `f` with a `*mut T` pointing at the cell contents.
    #[cfg(not(loom))]
    pub(crate) fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(*mut T) -> R,
    {
        f(self.inner.get())
    }

    /// Run `f` with a `*mut T` pointing at the cell contents (`loom`).
    #[cfg(loom)]
    pub(crate) fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(*mut T) -> R,
    {
        self.inner.with_mut(f)
    }
}

// SAFETY: `SyncUnsafeCell` exists to be wrapped by primitives that
// themselves provide the synchronisation. Marking it `Sync` therefore
// transfers the safety obligation to the primitive (spinlock, RW lock,
// MCS lock, seqlock, epoch, once), each of which documents how it
// upholds it.
unsafe impl<T: ?Sized + Send> Sync for SyncUnsafeCell<T> {}
