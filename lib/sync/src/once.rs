//! One-shot initialisation primitives: [`OnceCell`] and [`Once`].
//!
//! Both primitives publish a single value of type `T` exactly once, with
//! no possibility of double-init and **no panics**. If the initialiser
//! returns an error (or if a previous initialiser left the cell in a
//! half-initialised state) the cell becomes *poisoned*: every subsequent
//! access returns [`Err(PoisonError)`](PoisonError) and the partially
//! constructed value (if any) is dropped.
//!
//! # Difference between the two
//!
//! - [`OnceCell<T>`]: low-level "set/get" cell. Use it when the value
//!   is computed eagerly elsewhere and you just need a thread-safe slot
//!   to publish it.
//! - [`Once<T>`]: closure-driven lazy initialiser. Use it for static
//!   data that must be computed on first access (parsing ACPI tables,
//!   reading the platform's machine ID, etc.).
//!
//! # When *not* to use
//!
//! - In code that must run from interrupt context against the *same*
//!   cell as a non-interrupt thread: the busy loop here is unbounded if
//!   the initialiser is running on the interrupted CPU. Use a different
//!   pattern (precomputed table, double-checked init at boot only).
//!
//! # Ordering guarantees
//!
//! Publication uses [`Release`] on the state word; observers
//! [`Acquire`]-load it. Any writes performed by the initialiser happen
//! before any reader's first observation of the published value.
//!
//! # IRQ level
//!
//! Process / kernel-thread context only.
//!
//! [`Acquire`]: core::sync::atomic::Ordering::Acquire
//! [`Release`]: core::sync::atomic::Ordering::Release

use core::fmt;
use core::mem::MaybeUninit;

use crate::loom_compat::{AtomicUsize, Ordering, SyncUnsafeCell};
use crate::spinwait::spin_wait;

// State machine.
const EMPTY: usize = 0;
const RUNNING: usize = 1;
const READY: usize = 2;
const POISONED: usize = 3;

/// Returned when a one-shot cell has been poisoned by a previous failed
/// initialiser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoisonError;

impl fmt::Display for PoisonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("once-cell is poisoned")
    }
}

/// Error returned by [`OnceCell::set`] when the cell is already
/// initialised.
#[derive(Debug)]
pub struct AlreadySetError<T>(pub T);

/// Error returned by [`OnceCell::get_or_try_init`] and [`Once::call_once`]
/// when the initialiser fails or the cell is poisoned.
#[derive(Debug, PartialEq, Eq)]
pub enum InitError<E> {
    /// The cell was poisoned by an earlier failed initialiser.
    Poisoned,
    /// The initialiser returned this error. The cell is now poisoned.
    Init(E),
}

impl<E> From<PoisonError> for InitError<E> {
    fn from(_: PoisonError) -> Self {
        Self::Poisoned
    }
}

/// A thread-safe, set-once cell.
pub struct OnceCell<T> {
    state: AtomicUsize,
    value: SyncUnsafeCell<MaybeUninit<T>>,
}

// SAFETY: After a successful publish only `&T` is exposed and only one
// initialiser ever writes the cell. Send/Sync follow the inner type.
unsafe impl<T: Send> Send for OnceCell<T> {}
// SAFETY: As above.
unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}

impl<T> OnceCell<T> {
    /// Construct an empty cell.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(EMPTY),
            value: SyncUnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Construct an empty cell (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: AtomicUsize::new(EMPTY),
            value: SyncUnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Returns `true` if the cell is poisoned.
    pub fn is_poisoned(&self) -> bool {
        self.state.load(Ordering::Acquire) == POISONED
    }

    /// Returns `true` if the cell holds a value.
    pub fn is_initialised(&self) -> bool {
        self.state.load(Ordering::Acquire) == READY
    }

    /// Get a reference to the contained value.
    ///
    /// - `Ok(Some(&T))` — initialised.
    /// - `Ok(None)`     — still empty (or another thread is initialising).
    /// - `Err(_)`       — poisoned.
    pub fn get(&self) -> Result<Option<&T>, PoisonError> {
        match self.state.load(Ordering::Acquire) {
            READY => {
                // SAFETY: state==READY means `value` was initialised by
                // the publishing thread before its Release; our Acquire
                // load synchronises with that.
                Ok(Some(unsafe { self.value.with(|p| (*p).assume_init_ref()) }))
            }
            POISONED => Err(PoisonError),
            _ => Ok(None),
        }
    }

    /// Publish `value`. Fails if the cell is already initialised or
    /// poisoned (the latter is reported by returning `AlreadySetError`
    /// so the caller can recover the value).
    pub fn set(&self, value: T) -> Result<(), AlreadySetError<T>> {
        match self
            .state
            .compare_exchange(EMPTY, RUNNING, Ordering::Acquire, Ordering::Acquire)
        {
            Ok(_) => {
                // SAFETY: We hold the unique RUNNING transition, so no
                // other thread can be writing the cell concurrently.
                self.value.with_mut(|p| unsafe { (*p).write(value) });
                self.state.store(READY, Ordering::Release);
                Ok(())
            }
            Err(_) => Err(AlreadySetError(value)),
        }
    }

    /// Get the value, initialising it with `f` if necessary.
    ///
    /// If `f` returns `Err` the cell is poisoned permanently and the
    /// error is forwarded to the caller.
    pub fn get_or_try_init<F, E>(&self, f: F) -> Result<&T, InitError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        loop {
            match self.state.load(Ordering::Acquire) {
                READY => {
                    // SAFETY: state==READY: value initialised.
                    return Ok(unsafe {
                        self.value.with(|p| (*p).assume_init_ref())
                    });
                }
                POISONED => return Err(InitError::Poisoned),
                EMPTY => {
                    if self
                        .state
                        .compare_exchange(
                            EMPTY,
                            RUNNING,
                            Ordering::Acquire,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        match f() {
                            Ok(v) => {
                                // SAFETY: We are the unique writer.
                                self.value
                                    .with_mut(|p| unsafe { (*p).write(v) });
                                self.state.store(READY, Ordering::Release);
                                // SAFETY: just stored.
                                return Ok(unsafe {
                                    self.value
                                        .with(|p| (*p).assume_init_ref())
                                });
                            }
                            Err(e) => {
                                self.state.store(POISONED, Ordering::Release);
                                return Err(InitError::Init(e));
                            }
                        }
                    }
                    // CAS lost; loop and re-inspect.
                }
                _ /* RUNNING */ => {
                    // Wait for the other initialiser.
                    spin_wait();
                }
            }
        }
    }

    /// Take the value out of the cell, leaving it empty.
    ///
    /// Returns `Err(PoisonError)` if the cell is poisoned, `Ok(None)`
    /// if it is empty, `Ok(Some(value))` otherwise. Requires `&mut self`,
    /// so no synchronisation is necessary.
    pub fn take(&mut self) -> Result<Option<T>, PoisonError> {
        let state = *self.state.get_mut();
        match state {
            READY => {
                *self.state.get_mut() = EMPTY;
                // SAFETY: state was READY and we have exclusive access;
                // we move the value out and reset the state.
                let v = self.value.with_mut(|p| unsafe { (*p).assume_init_read() });
                Ok(Some(v))
            }
            POISONED => Err(PoisonError),
            _ => Ok(None),
        }
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for OnceCell<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == READY {
            // SAFETY: state was READY; drop the contained value exactly
            // once.
            self.value.with_mut(|p| unsafe { (*p).assume_init_drop() });
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.get() {
            Ok(Some(v)) => f.debug_tuple("OnceCell").field(v).finish(),
            Ok(None) => f.write_str("OnceCell(<empty>)"),
            Err(_) => f.write_str("OnceCell(<poisoned>)"),
        }
    }
}

// ---------------------------------------------------------------------------
// Once<T> — closure-driven flavour.
// ---------------------------------------------------------------------------

/// A closure-driven, one-shot initialiser.
///
/// `Once<T>` wraps a [`OnceCell<T>`] and exposes only the lazy-init API:
/// callers pass a closure that produces the value the first time it is
/// needed, and every subsequent caller observes the same value (or the
/// same poisoning, if the original closure failed).
pub struct Once<T>(OnceCell<T>);

impl<T> Once<T> {
    /// Construct a fresh `Once`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new() -> Self {
        Self(OnceCell::new())
    }

    /// Construct a fresh `Once` (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new() -> Self {
        Self(OnceCell::new())
    }

    /// Returns the contained value if already initialised.
    pub fn get(&self) -> Result<Option<&T>, PoisonError> {
        self.0.get()
    }

    /// Returns `true` if the underlying cell is poisoned.
    pub fn is_poisoned(&self) -> bool {
        self.0.is_poisoned()
    }

    /// Initialise the cell with `f` on the first call.
    ///
    /// On success returns `Ok(&T)`. If `f` returns `Err`, the cell is
    /// poisoned and `Err(InitError::Init(e))` is returned. Subsequent
    /// calls observe `Err(InitError::Poisoned)`.
    pub fn call_once<F, E>(&self, f: F) -> Result<&T, InitError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        self.0.get_or_try_init(f)
    }

    /// Convenience variant of [`call_once`](Self::call_once) for
    /// infallible initialisers.
    pub fn call_once_infallible<F>(&self, f: F) -> Result<&T, PoisonError>
    where
        F: FnOnce() -> T,
    {
        match self
            .0
            .get_or_try_init::<_, core::convert::Infallible>(|| Ok(f()))
        {
            Ok(v) => Ok(v),
            Err(InitError::Poisoned) => Err(PoisonError),
            Err(InitError::Init(e)) => match e {},
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for Once<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Once").field(&self.0).finish()
    }
}
