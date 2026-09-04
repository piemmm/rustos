//! MCS (Mellor-Crummey & Scott) queue lock.
//!
//! An [`McsLock<T>`] is a fair queue lock optimised for high contention.
//! Each waiter spins on its **own** [`McsNode`], not on the global lock
//! word, so the contended cache line stays local to one CPU at a time —
//! eliminating the bus traffic storms that wreck plain spinlocks under
//! load.
//!
//! # When to use
//!
//! - Many CPUs frequently contend for the same lock.
//! - Critical section is short; the caller is willing to allocate a
//!   `McsNode` on the stack for the lock duration.
//!
//! # When *not* to use
//!
//! - Low contention: a plain [`SpinLock`](crate::SpinLock) is cheaper.
//! - Reader-heavy workloads: see [`RwLock`](crate::RwLock) or
//!   [`SeqLock`](crate::SeqLock).
//! - Inside interrupt context — MCS locks are unbounded in queue depth
//!   and may not be acquired with interrupts enabled.
//!
//! # Ordering guarantees
//!
//! `lock` performs an [`AcqRel`] swap to install the new tail. `unlock`
//! (via guard `Drop`) performs a [`Release`] store on the successor's
//! `locked` flag, which the successor reads with [`Acquire`].
//!
//! # IRQ level
//!
//! Process / kernel-thread context only.
//!
//! [`AcqRel`]: core::sync::atomic::Ordering::AcqRel
//! [`Acquire`]: core::sync::atomic::Ordering::Acquire
//! [`Release`]: core::sync::atomic::Ordering::Release

use core::marker::{PhantomData, PhantomPinned};
use core::ops::{Deref, DerefMut};
use core::ptr;

use crate::loom_compat::{AtomicBool, AtomicPtr, Ordering, SyncUnsafeCell};
use crate::spinwait::spin_wait;

/// Per-waiter queue node.
///
/// Allocate on the calling thread's stack and pass to [`McsLock::lock`].
/// The node must outlive the returned guard.
pub struct McsNode {
    locked: AtomicBool,
    next: AtomicPtr<McsNode>,
    _pin: PhantomPinned,
}

impl McsNode {
    /// Construct a fresh, unused node.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            next: AtomicPtr::new(ptr::null_mut()),
            _pin: PhantomPinned,
        }
    }

    /// Construct a fresh, unused node (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            next: AtomicPtr::new(ptr::null_mut()),
            _pin: PhantomPinned,
        }
    }
}

#[cfg(not(loom))]
impl Default for McsNode {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: Atomic fields permit aliased reads/writes from any CPU; the
// MCS protocol guarantees that only one CPU at a time treats the node
// as "owned".
unsafe impl Send for McsNode {}
// SAFETY: As above.
unsafe impl Sync for McsNode {}

/// An MCS queue lock.
pub struct McsLock<T: ?Sized> {
    tail: AtomicPtr<McsNode>,
    data: SyncUnsafeCell<T>,
}

// SAFETY: Mutual exclusion is enforced by the MCS queue protocol.
unsafe impl<T: ?Sized + Send> Send for McsLock<T> {}
// SAFETY: Same as `Send`.
unsafe impl<T: ?Sized + Send> Sync for McsLock<T> {}

impl<T> McsLock<T> {
    /// Construct a new lock wrapping `value`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            tail: AtomicPtr::new(ptr::null_mut()),
            data: SyncUnsafeCell::new(value),
        }
    }

    /// Construct a new lock wrapping `value` (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            tail: AtomicPtr::new(ptr::null_mut()),
            data: SyncUnsafeCell::new(value),
        }
    }

    /// Consume the lock and return the protected value.
    pub fn into_inner(self) -> T {
        let this = core::mem::ManuallyDrop::new(self);
        // SAFETY: `self` is consumed and held in `ManuallyDrop`, so the
        // inner cell is not dropped twice and no waiters can be queued
        // (each waiter would hold an outstanding `&self`).
        this.data.with(|p| unsafe { ptr::read(p) })
    }
}

impl<T: ?Sized> McsLock<T> {
    /// Acquire the lock, blocking the caller until it is at the head of
    /// the queue.
    ///
    /// `node` must be a freshly-initialised [`McsNode`] that the caller
    /// keeps alive until the returned guard is dropped. The borrow
    /// checker enforces this through the `'a` lifetime.
    pub fn lock<'a>(&'a self, node: &'a mut McsNode) -> McsGuard<'a, T> {
        // Reset our node before publishing it.
        node.next.store(ptr::null_mut(), Ordering::Relaxed);
        node.locked.store(true, Ordering::Relaxed);

        let node_ptr: *mut McsNode = node;
        let predecessor = self.tail.swap(node_ptr, Ordering::AcqRel);

        if !predecessor.is_null() {
            // There is a queued predecessor; link ourselves as their
            // successor and spin on our local flag.
            // SAFETY: The predecessor pointer was published by another
            // thread's `lock` call. That thread keeps its node alive
            // until *we* clear its `locked` flag at unlock time, which
            // has not happened yet, so the dereference is valid.
            unsafe {
                (*predecessor).next.store(node_ptr, Ordering::Release);
            }
            while node.locked.load(Ordering::Acquire) {
                spin_wait();
            }
        }

        McsGuard {
            lock: self,
            node: node_ptr,
            _marker: PhantomData,
        }
    }

    /// Get a mutable reference to the protected value.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self` guarantees no waiters exist.
        self.data.with_mut(|p| unsafe { &mut *p })
    }
}

impl<T: Default> Default for McsLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// RAII guard returned by [`McsLock::lock`].
#[must_use = "if unused the lock is immediately released"]
pub struct McsGuard<'a, T: ?Sized> {
    lock: &'a McsLock<T>,
    node: *mut McsNode,
    // Tie the guard's lifetime to the `&mut McsNode` borrow that produced
    // it so the borrow checker keeps the node alive.
    _marker: PhantomData<&'a mut McsNode>,
}

impl<T: ?Sized> Deref for McsGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: The guard exists only while we are the queue head, so
        // we have exclusive access to the cell.
        self.lock.data.with(|p| unsafe { &*p })
    }
}

impl<T: ?Sized> DerefMut for McsGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: As above.
        self.lock.data.with_mut(|p| unsafe { &mut *p })
    }
}

impl<T: ?Sized> Drop for McsGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: `self.node` was created from a live `&mut McsNode` and
        // remains live for the guard's lifetime.
        let node = unsafe { &*self.node };
        let successor = node.next.load(Ordering::Acquire);
        if successor.is_null() {
            // No queued successor visible yet. Try to atomically clear
            // the tail.
            if self
                .lock
                .tail
                .compare_exchange(
                    self.node,
                    ptr::null_mut(),
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return;
            }
            // A successor is in the middle of `lock`: spin until it
            // links itself into our `next`.
            let succ = loop {
                let s = node.next.load(Ordering::Acquire);
                if !s.is_null() {
                    break s;
                }
                spin_wait();
            };
            // SAFETY: The successor's node is kept alive by its caller's
            // outstanding `&'a mut McsNode` borrow.
            unsafe { (*succ).locked.store(false, Ordering::Release) };
        } else {
            // SAFETY: Same as above.
            unsafe { (*successor).locked.store(false, Ordering::Release) };
        }
    }
}
