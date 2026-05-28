//! Epoch-based reclamation (RCU-equivalent).
//!
//! This module supplies the minimum primitive set every other RustOS
//! component needs to publish a new version of a data structure while
//! safely deferring destruction of the old version until no thread can
//! observe it.
//!
//! It is **not** a clone of `crossbeam-epoch`; it is the deliberately
//! small subset of an epoch reclamation domain that the kernel actually
//! requires. Users that want lock-free queues etc. can compose this with
//! their own data-structure-specific atomics.
//!
//! # Model
//!
//! - The [`Epoch`] domain owns a monotonically-increasing global epoch
//!   counter and a list of [`Participant`] slots.
//! - A participant declares itself an *active reader* by calling
//!   [`Participant::pin`], which returns a [`Guard`]. The guard records
//!   the current global epoch; as long as the guard exists, no value
//!   pinned in a strictly older epoch may be reclaimed.
//! - Writers replace published data however they wish (typically through
//!   an `AtomicPtr` of their own) and call [`Epoch::defer_free`] with
//!   the previous version, which is dropped only after the next safe
//!   point has been reached via [`Epoch::advance`].
//!
//! # When to use
//!
//! - You have a read-mostly data structure with lock-free readers and
//!   you need to free the old version of an object some time after the
//!   write that replaced it.
//!
//! # When *not* to use
//!
//! - For mutual exclusion: use a lock.
//! - For very small payloads where a [`SeqLock`](crate::SeqLock) is
//!   adequate.
//! - In situations where the [`alloc`] crate is unavailable: this
//!   primitive heap-allocates deferred actions.
//!
//! # Ordering guarantees
//!
//! - Publishing the data and then calling [`defer_free`](Epoch::defer_free)
//!   is sufficient: the deferred-action queue is taken under a [`SpinLock`],
//!   which provides Acquire/Release synchronisation with [`advance`](Epoch::advance).
//! - [`Participant::pin`] performs a [`SeqCst`] update so that subsequent
//!   reads happen-after the global-epoch update they observed.
//!
//! # IRQ level
//!
//! Process / kernel-thread context only.
//!
//! [`SpinLock`]: crate::SpinLock
//! [`SeqCst`]: core::sync::atomic::Ordering::SeqCst

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::loom_compat::{AtomicBool, AtomicU64, Ordering};
use crate::spinlock::SpinLock;

const NO_EPOCH: u64 = u64::MAX;

struct Slot {
    /// Is this slot currently owned by a [`Participant`]?
    in_use: AtomicBool,
    /// Set while the participant holds a [`Guard`].
    active: AtomicBool,
    /// The global epoch at which the participant pinned; `NO_EPOCH` when
    /// inactive.
    pinned_epoch: AtomicU64,
}

impl Slot {
    fn new() -> Self {
        Self {
            in_use: AtomicBool::new(false),
            active: AtomicBool::new(false),
            pinned_epoch: AtomicU64::new(NO_EPOCH),
        }
    }
}

struct Deferred {
    epoch: u64,
    action: Box<dyn FnOnce() + Send>,
}

/// An epoch-based reclamation domain.
///
/// Construct one per logically independent data structure. Multiple
/// domains do not synchronise with each other.
pub struct Epoch {
    global: AtomicU64,
    // `Box<Slot>` is required, not redundant: participants hold a
    // `NonNull<Slot>` into the slot's allocation, and reallocating the
    // `Vec`'s buffer must not invalidate those pointers. The `Box`
    // gives each slot a stable address.
    #[allow(clippy::vec_box)]
    slots: SpinLock<Vec<Box<Slot>>>,
    deferred: SpinLock<Vec<Deferred>>,
}

// SAFETY: All shared state goes through atomics or `SpinLock`; the only
// raw-pointer-aliased field, the slot list, is owned exclusively by the
// `Vec` and only borrowed via the participants' `NonNull<Slot>` which
// point into the `Box`es (stable addresses).
unsafe impl Send for Epoch {}
// SAFETY: As above.
unsafe impl Sync for Epoch {}

impl Epoch {
    /// Construct an empty reclamation domain.
    #[must_use]
    pub fn new() -> Self {
        Self {
            global: AtomicU64::new(0),
            slots: SpinLock::new(Vec::new()),
            deferred: SpinLock::new(Vec::new()),
        }
    }

    /// Returns the current global epoch (informational only).
    pub fn current(&self) -> u64 {
        self.global.load(Ordering::Acquire)
    }

    /// Register a participant in the domain.
    pub fn register(&self) -> Participant<'_> {
        let mut slots = self.slots.lock();
        // Re-use a free slot if one exists.
        for boxed in slots.iter_mut() {
            if !boxed.in_use.load(Ordering::Relaxed) {
                boxed.in_use.store(true, Ordering::Relaxed);
                boxed.active.store(false, Ordering::Relaxed);
                boxed.pinned_epoch.store(NO_EPOCH, Ordering::Relaxed);
                let ptr = NonNull::from(&**boxed);
                return Participant {
                    domain: self,
                    slot: ptr,
                    _not_send: PhantomData,
                };
            }
        }
        // Allocate a fresh one.
        let boxed = Box::new(Slot::new());
        boxed.in_use.store(true, Ordering::Relaxed);
        let ptr = NonNull::from(&*boxed);
        slots.push(boxed);
        Participant {
            domain: self,
            slot: ptr,
            _not_send: PhantomData,
        }
    }

    /// Schedule `value` to be dropped at a later epoch.
    pub fn defer_free<T: Send + 'static>(&self, value: T) {
        let epoch = self.global.load(Ordering::Acquire);
        let action: Box<dyn FnOnce() + Send> = Box::new(move || drop(value));
        self.deferred.lock().push(Deferred { epoch, action });
    }

    /// Schedule an arbitrary `FnOnce` to be invoked at a later epoch.
    pub fn defer<F: FnOnce() + Send + 'static>(&self, f: F) {
        let epoch = self.global.load(Ordering::Acquire);
        self.deferred.lock().push(Deferred {
            epoch,
            action: Box::new(f),
        });
    }

    /// Advance the global epoch and run any deferred actions whose
    /// safety horizon has passed.
    ///
    /// Returns the number of deferred actions actually executed.
    pub fn advance(&self) -> usize {
        // Bump the global epoch; readers that pin afterwards observe
        // the new value.
        let next = self.global.fetch_add(1, Ordering::AcqRel) + 1;

        // Compute the smallest pinned epoch across active participants.
        // If none are active, every deferred action is safe to run.
        let min_pinned = {
            let slots = self.slots.lock();
            let mut min: Option<u64> = None;
            for boxed in slots.iter() {
                if boxed.active.load(Ordering::Acquire) {
                    let e = boxed.pinned_epoch.load(Ordering::Acquire);
                    if e != NO_EPOCH {
                        min = Some(match min {
                            None => e,
                            Some(m) => core::cmp::min(m, e),
                        });
                    }
                }
            }
            min
        };

        // Anything pinned strictly before `min_pinned` (or anything at
        // all, if no participant is active) is safe to free.
        let mut to_run: Vec<Deferred> = Vec::new();
        {
            let mut deferred = self.deferred.lock();
            let safe_horizon = min_pinned.unwrap_or(next);
            let mut i = 0;
            while i < deferred.len() {
                if deferred[i].epoch < safe_horizon {
                    to_run.push(deferred.swap_remove(i));
                } else {
                    i += 1;
                }
            }
        }
        let n = to_run.len();
        for d in to_run {
            (d.action)();
        }
        n
    }
}

impl Default for Epoch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Epoch {
    fn drop(&mut self) {
        // Run any remaining deferred actions. `&mut self` means no
        // readers can exist by definition.
        let pending = core::mem::take(&mut *self.deferred.lock());
        for d in pending {
            (d.action)();
        }
    }
}

/// A registered participant in an [`Epoch`] domain.
///
/// Holding a `Participant` is cheap; pinning produces a [`Guard`] that
/// records the current epoch.
pub struct Participant<'a> {
    domain: &'a Epoch,
    slot: NonNull<Slot>,
    // `Participant` is intentionally `!Send`: a slot is tied to the
    // logical execution context that owns it. (`Sync` is also not
    // needed; there's no public way to share `&Participant` either.)
    _not_send: PhantomData<*const ()>,
}

impl<'a> Participant<'a> {
    /// Pin the current global epoch; returns a [`Guard`] that releases
    /// the pin when dropped.
    pub fn pin(&self) -> Guard<'_> {
        // SAFETY: `self.slot` was published into `domain.slots` by
        // `register` and remains live for `'a`.
        let slot = unsafe { self.slot.as_ref() };
        let epoch = self.domain.global.load(Ordering::SeqCst);
        slot.pinned_epoch.store(epoch, Ordering::Relaxed);
        slot.active.store(true, Ordering::SeqCst);
        Guard {
            participant: self,
            _marker: PhantomData,
        }
    }

    /// The reclamation domain this participant belongs to.
    #[must_use]
    pub fn domain(&self) -> &'a Epoch {
        self.domain
    }
}

impl Drop for Participant<'_> {
    fn drop(&mut self) {
        // SAFETY: slot pointer still valid (see `pin`).
        let slot = unsafe { self.slot.as_ref() };
        slot.active.store(false, Ordering::Release);
        slot.pinned_epoch.store(NO_EPOCH, Ordering::Relaxed);
        slot.in_use.store(false, Ordering::Release);
    }
}

/// RAII guard returned by [`Participant::pin`]; while it lives, no
/// deferred action queued *before* the pinned epoch may run.
#[must_use = "if unused the participant is immediately unpinned"]
pub struct Guard<'a> {
    participant: &'a Participant<'a>,
    _marker: PhantomData<&'a ()>,
}

impl<'a> Guard<'a> {
    /// The epoch this guard captured.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        // SAFETY: slot pointer still valid (see `pin`).
        let slot = unsafe { self.participant.slot.as_ref() };
        slot.pinned_epoch.load(Ordering::Relaxed)
    }

    /// The domain this guard pins.
    #[must_use]
    pub fn domain(&self) -> &'a Epoch {
        self.participant.domain
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        // SAFETY: slot pointer still valid (see `pin`).
        let slot = unsafe { self.participant.slot.as_ref() };
        slot.active.store(false, Ordering::Release);
        // Keep `pinned_epoch` so debuggers can see the last pin.
    }
}
