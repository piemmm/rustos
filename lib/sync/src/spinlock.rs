//! Spinlocks.
//!
//! Two flavours are provided:
//!
//! - [`SpinLock<T>`] — the canonical test-and-test-and-set spinlock. The
//!   simplest mutual-exclusion primitive in the crate. Use it for short
//!   critical sections that never run from interrupt context.
//! - [`IrqSafeSpinLock<T, I>`] — a spinlock that additionally disables
//!   interrupts on the current CPU while held. Use it for *any* state
//!   that may be touched from a hardware interrupt handler.
//!
//! # When to use
//!
//! - Critical section is bounded in time (does *not* call into the
//!   scheduler, the allocator, IPC, or any blocking API).
//! - Contention is expected to be low. Under high contention switch to
//!   [`McsLock`](crate::McsLock).
//!
//! # When *not* to use
//!
//! - Inside any code path that may sleep, take a page fault on protected
//!   data, or be preempted while holding the lock. The kernel scheduler
//!   may not be invoked while a spinlock is held; doing so is a deadlock
//!   bug.
//! - Across an interrupt boundary unless using [`IrqSafeSpinLock`].
//!
//! # Ordering guarantees
//!
//! `lock`/`try_lock` perform an [`Acquire`] read of the lock word. `unlock`
//! (via the guard's `Drop`) performs a [`Release`] write. This pairs every
//! release with the next acquire, so writes performed inside the critical
//! section are visible to the next holder.
//!
//! # IRQ level
//!
//! - [`SpinLock`] is safe at *process* / *kernel-thread* level only.
//!   Acquiring it from an interrupt handler will deadlock if the
//!   interrupted thread already holds the lock.
//! - [`IrqSafeSpinLock`] is safe at any IRQ level supported by the
//!   plugged-in [`InterruptControl`] implementation.
//!
//! [`Acquire`]: core::sync::atomic::Ordering::Acquire
//! [`Release`]: core::sync::atomic::Ordering::Release

use core::fmt;
use core::ops::{Deref, DerefMut};

use crate::irq::{InterruptControl, NopInterruptControl};
use crate::loom_compat::{spin_loop, AtomicBool, Ordering, SyncUnsafeCell};

/// A test-and-test-and-set spinlock.
///
/// See the [module docs](self) for use cases, ordering, and IRQ guarantees.
pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    /// Owner stamp for the lockup watchdog: `0` while unheld, else the
    /// holding CPU's dense id plus one. A spinner publishes what it reads
    /// here, so a wedged core's report names the core holding the lock
    /// against it instead of leaving the pairing to be guessed.
    #[cfg(feature = "lock-diagnostics")]
    owner: core::sync::atomic::AtomicU32,
    data: SyncUnsafeCell<T>,
}

// SAFETY: A `SpinLock<T>` provides mutual exclusion over `T`. As long as
// `T: Send`, transferring or sharing the lock across threads is sound:
// only one thread at a time observes the `&mut T` exposed by the guard.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}
// SAFETY: Same argument as `Send`. `T: Send` is sufficient because the
// only way to reach `T` through `&SpinLock<T>` is via `lock()`, which
// serialises access.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Create a new spinlock wrapping `value`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: SyncUnsafeCell::new(value),
            #[cfg(feature = "lock-diagnostics")]
            owner: core::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Create a new spinlock wrapping `value` (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: SyncUnsafeCell::new(value),
            #[cfg(feature = "lock-diagnostics")]
            owner: core::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Consume the lock and return the protected value.
    pub fn into_inner(self) -> T {
        // Wrap in `ManuallyDrop` so the inner `UnsafeCell<T>` never
        // drops `T` — we read it out by value below.
        let this = core::mem::ManuallyDrop::new(self);
        // SAFETY: `self` was consumed and is now inside `ManuallyDrop`,
        // so no other reference can possibly exist into `data` and the
        // value will not be dropped twice.
        this.data.with(|p| unsafe { core::ptr::read(p) })
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// The uninstrumented compare-and-swap acquire, shared by the public
    /// [`Self::try_lock`] and [`Self::lock`] so the lock-diagnostics site
    /// note is emitted exactly once per acquisition (never doubled by
    /// `lock` delegating to `try_lock`).
    #[inline]
    fn raw_try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        // Acquire on success establishes the happens-before edge with the
        // previous holder's Release. Relaxed on failure: we observed
        // contention but do not synchronise.
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            #[cfg(feature = "lock-diagnostics")]
            self.owner
                .store(crate::lockwatch::owner_stamp(), Ordering::Relaxed);
            Some(SpinLockGuard { lock: self })
        } else {
            None
        }
    }

    /// Try to acquire the lock without spinning.
    ///
    /// Returns `Some(guard)` on success, `None` if the lock was contended.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let guard = self.raw_try_lock()?;
        // Record the successful non-spinning acquire against the caller's
        // source site so a wedge while holding this lock names it.
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note(
            crate::lockwatch::LockEvent::TryAcquired,
            core::panic::Location::caller(),
        );
        Some(guard)
    }

    /// Acquire the lock, spinning until it is free.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // Publish the acquiring site *before* spinning, so a CPU that
        // wedges spinning for a never-released lock has its report name the
        // contended lock (marked `acquiring`); the successful-acquire note
        // below then promotes it to `held`.
        #[cfg(feature = "lock-diagnostics")]
        let site = core::panic::Location::caller();
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note(crate::lockwatch::LockEvent::Acquiring, site);
        loop {
            if let Some(guard) = self.raw_try_lock() {
                #[cfg(feature = "lock-diagnostics")]
                crate::lockwatch::note(crate::lockwatch::LockEvent::Acquired, site);
                return guard;
            }
            // Republish the holder each round so a core that wedges here
            // names whoever is actually holding the lock against it.
            #[cfg(feature = "lock-diagnostics")]
            crate::lockwatch::note_contended(site, self.owner.load(Ordering::Relaxed));
            // Test-and-test-and-set: spin reading until the lock looks
            // free, then retry the CAS. This avoids hammering the cache
            // line with RMW operations.
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
    }

    /// Returns `true` if the lock is currently held by *some* thread.
    ///
    /// This is informational only: by the time the caller observes the
    /// return value the state may have changed.
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Get a mutable reference to the protected value.
    ///
    /// Because this takes `&mut self`, no synchronisation is required.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self` guarantees exclusive access; no other
        // borrow of `data` can exist.
        self.data.with_mut(|p| unsafe { &mut *p })
    }
}

impl<T: Default> Default for SpinLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(g) => f.debug_struct("SpinLock").field("data", &&*g).finish(),
            None => f
                .debug_struct("SpinLock")
                .field("data", &format_args!("<locked>"))
                .finish(),
        }
    }
}

/// RAII guard returned by [`SpinLock::lock`] and [`SpinLock::try_lock`].
///
/// The guard releases the lock when dropped.
#[must_use = "if unused the lock is immediately released"]
pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: The guard exists only while the lock is held, which
        // gives exclusive access to the contained value.
        self.lock.data.with(|p| unsafe { &*p })
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: The guard exists only while the lock is held, which
        // gives exclusive access to the contained value.
        self.lock.data.with_mut(|p| unsafe { &mut *p })
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // Cleared before the lock bit, so no window shows the lock free but
        // still stamped with a stale owner.
        #[cfg(feature = "lock-diagnostics")]
        self.lock.owner.store(0, Ordering::Relaxed);
        // Release pairs with the next Acquire CAS, publishing every
        // write performed in the critical section.
        self.lock.locked.store(false, Ordering::Release);
        // Drop the lock-diagnostics record this guard's acquisition pushed.
        // Every `SpinLockGuard` corresponds to exactly one acquire note
        // (`try_lock`/`lock`), so the release note balances it one-to-one.
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note_release();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLockGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

// ---------------------------------------------------------------------------
// IRQ-safe spinlock.
// ---------------------------------------------------------------------------

/// A spinlock that disables interrupts on the current CPU while held.
///
/// `I` is the architecture's [`InterruptControl`] implementation; on
/// host test builds the default [`NopInterruptControl`] turns the
/// IRQ-handling into a no-op so the lock is still usable in unit tests.
///
/// See the [module docs](self) for use cases, ordering, and IRQ guarantees.
pub struct IrqSafeSpinLock<T, I: InterruptControl = NopInterruptControl> {
    inner: SpinLock<T>,
    _irq: core::marker::PhantomData<fn() -> I>,
}

// SAFETY: Same reasoning as `SpinLock<T>`; `I` is zero-sized phantom.
unsafe impl<T: Send, I: InterruptControl> Send for IrqSafeSpinLock<T, I> {}
// SAFETY: Same reasoning as `SpinLock<T>`.
unsafe impl<T: Send, I: InterruptControl> Sync for IrqSafeSpinLock<T, I> {}

impl<T, I: InterruptControl> IrqSafeSpinLock<T, I> {
    /// Create a new IRQ-safe spinlock wrapping `value`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            inner: SpinLock::new(value),
            _irq: core::marker::PhantomData,
        }
    }

    /// Create a new IRQ-safe spinlock wrapping `value` (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            inner: SpinLock::new(value),
            _irq: core::marker::PhantomData,
        }
    }

    /// Consume the lock and return the protected value.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T, I: InterruptControl> IrqSafeSpinLock<T, I> {
    // T is Sized because the inner SpinLock holds it by value.
    /// Try to acquire the lock without spinning.
    ///
    /// Interrupts are disabled on success and re-enabled when the guard
    /// is dropped. On failure the interrupt state is unchanged.
    ///
    /// `#[track_caller]` (lock-diagnostics builds only) so the inner
    /// [`SpinLock`]'s site note records *this* caller's source location,
    /// not a fixed line inside this wrapper.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn try_lock(&self) -> Option<IrqSafeSpinLockGuard<'_, T, I>> {
        let state = I::disable();
        if let Some(inner) = self.inner.try_lock() {
            Some(IrqSafeSpinLockGuard {
                inner: core::mem::ManuallyDrop::new(inner),
                state: core::mem::ManuallyDrop::new(state),
            })
        } else {
            // SAFETY: `state` was produced by the `disable` call above and
            // has not been restored yet.
            unsafe { I::restore(state) };
            None
        }
    }

    /// Acquire the lock, spinning until it is free. Disables interrupts
    /// for the duration of the critical section.
    ///
    /// `#[track_caller]` (lock-diagnostics builds only) so the inner
    /// [`SpinLock`]'s site note records *this* caller's source location.
    /// This is the IRQ-masking spinlock whose held/contended section a
    /// GICv2 hard lockup wedges in, so naming it is the point of the
    /// facility.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn lock(&self) -> IrqSafeSpinLockGuard<'_, T, I> {
        let state = I::disable();
        let inner = self.inner.lock();
        IrqSafeSpinLockGuard {
            inner: core::mem::ManuallyDrop::new(inner),
            state: core::mem::ManuallyDrop::new(state),
        }
    }

    /// Get a mutable reference to the protected value.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

impl<T: Default, I: InterruptControl> Default for IrqSafeSpinLock<T, I> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// RAII guard returned by [`IrqSafeSpinLock`]'s `lock`/`try_lock`.
#[must_use = "if unused the lock is immediately released and interrupts re-enabled"]
pub struct IrqSafeSpinLockGuard<'a, T, I: InterruptControl> {
    inner: core::mem::ManuallyDrop<SpinLockGuard<'a, T>>,
    state: core::mem::ManuallyDrop<I::State>,
}

impl<T, I: InterruptControl> Deref for IrqSafeSpinLockGuard<'_, T, I> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T, I: InterruptControl> DerefMut for IrqSafeSpinLockGuard<'_, T, I> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T, I: InterruptControl> Drop for IrqSafeSpinLockGuard<'_, T, I> {
    fn drop(&mut self) {
        // Drop ordering: release the spinlock first, *then* restore
        // interrupts. Reversing this would let an interrupt take the
        // lock recursively before we released it.
        // SAFETY: `inner` and `state` are dropped exactly once because
        // the surrounding `ManuallyDrop` fields are never accessed
        // after this block.
        unsafe {
            core::mem::ManuallyDrop::drop(&mut self.inner);
            let state = core::mem::ManuallyDrop::take(&mut self.state);
            I::restore(state);
        }
    }
}
