//! Debug-only lock-site observation seam for the lockup watchdog.
//!
//! Compiled in only with the `lock-diagnostics` feature, which a
//! `watchdog-diagnostics` kernel build (the non-shippable `debug` image)
//! turns on. When it is on, each IRQ-masking spinlock reports its
//! acquire/hold/release lifecycle here, tagged with the `#[track_caller]`
//! source `file:line` of the acquiring call. The kernel's lockup watchdog
//! installs an [`ObserverFn`] that records the current site per CPU, so a
//! wedged core's report names the exact spinlock it is stuck spinning on or
//! holding while interrupts are masked — the mechanism behind a GICv2 hard
//! lockup, where the maskable liveness sample can no longer observe the
//! stuck section.
//!
//! A shippable build never compiles this in: the instrumentation, the
//! `track_caller` shim, and this module all vanish, so a production lock is
//! a bare compare-and-swap.
//!
//! # Discipline
//!
//! The seam is a single installed thin function pointer — no allocation, no
//! lock. It runs *inside* the lock primitives, so the observer it forwards
//! to must never recurse into a lock (the kernel observer only reads a
//! CPU-register cpu id and stores into per-CPU atomics). An uninstalled
//! observer is a no-op, so a lock taken before the kernel installs the
//! observer (early boot) simply records nothing (fail-safe).

use core::panic::Location;
use core::sync::atomic::{AtomicUsize, Ordering};

/// A lock-lifecycle transition reported to the installed [`ObserverFn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LockEvent {
    /// A spin-acquire began: the caller is now spinning for the lock at the
    /// reported site (recorded as the CPU's current lock, marked
    /// *acquiring*). If the CPU wedges here the report names the site as the
    /// contended lock it could not take.
    Acquiring = 0,
    /// A spin-acquire succeeded: the CPU's current lock is promoted from
    /// *acquiring* to *held*. Pairs with a preceding [`Self::Acquiring`].
    Acquired = 1,
    /// A non-spinning `try_lock` succeeded (recorded directly as the CPU's
    /// current lock, marked *held* — there was no spin phase).
    TryAcquired = 2,
    /// The CPU's current lock was released (the most recent record is
    /// dropped).
    Released = 3,
}

/// The observer the kernel installs to record lock lifecycle events into
/// its per-CPU lockup-diagnostic state.
///
/// `site_ptr` is the acquiring call's `&'static Location<'static>` reduced
/// to a `usize` (`0` for [`LockEvent::Released`], which carries no site).
/// The kernel observer reconstructs the reference to read `file`/`line`.
pub type ObserverFn = fn(event: LockEvent, site_ptr: usize);

/// The installed observer, as a thin `fn` pointer stored as a `usize`
/// (`0` = none). Relaxed access is sufficient: this is a best-effort
/// diagnostic channel, not a synchronising handshake.
static OBSERVER: AtomicUsize = AtomicUsize::new(0);

/// Install the lock-lifecycle observer. Idempotent; the last writer wins.
pub fn install(observer: ObserverFn) {
    OBSERVER.store(observer as usize, Ordering::Relaxed);
}

/// Forward one event to the installed observer, if any.
#[inline]
fn dispatch(event: LockEvent, site_ptr: usize) {
    let raw = OBSERVER.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: `install` only ever stores a value produced by
        // `ObserverFn as usize`; a non-zero slot is therefore a valid
        // `ObserverFn`, which is a plain `fn` with no captured environment
        // and `'static` validity.
        let f: ObserverFn = unsafe { core::mem::transmute::<usize, ObserverFn>(raw) };
        f(event, site_ptr);
    }
}

/// Report an acquire/hold event for `site` (the `#[track_caller]` location
/// of the acquiring call).
#[inline]
pub fn note(event: LockEvent, site: &'static Location<'static>) {
    dispatch(event, site as *const Location<'static> as usize);
}

/// Report the release (drop) of the CPU's current lock.
#[inline]
pub fn note_release() {
    dispatch(LockEvent::Released, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The observer is a process-global seam a real kernel installs exactly
    /// once at boot, and instrumenting `install` from a unit test would make
    /// *every* parallel test's spinlock forward into it — a race. So the
    /// observer is not installed here: the recording end is exercised
    /// deterministically kernel-side (the `tairix_kernel_core` watchdog
    /// tests drive `lock_push`/`lock_pop`/`lock_snapshot` directly), and the
    /// whole spinlock suite running under this feature smoke-tests the
    /// forwarding path (a broken `dispatch` would panic those locks).
    ///
    /// What this test locks in is the fail-safe contract: with no observer
    /// installed, `note`/`note_release` are cheap no-ops that never panic and
    /// never call through a null slot — so a lock taken before the kernel
    /// installs its observer (early boot) is safe.
    #[test]
    fn note_without_an_observer_is_a_safe_no_op() {
        // No `install(..)`, so `OBSERVER` is the `0` sentinel.
        assert_eq!(OBSERVER.load(Ordering::Relaxed), 0);
        let here = Location::caller();
        note(LockEvent::Acquiring, here);
        note(LockEvent::Acquired, here);
        note(LockEvent::TryAcquired, here);
        note_release();
        // Reaching here without a panic or a call through the null slot is
        // the guarantee; the slot is untouched.
        assert_eq!(OBSERVER.load(Ordering::Relaxed), 0);
    }

    /// The event discriminants are the stable wire the kernel observer
    /// matches on; pin them so a reorder that would silently misclassify a
    /// lock event is caught.
    #[test]
    fn event_discriminants_are_stable() {
        assert_eq!(LockEvent::Acquiring as u8, 0);
        assert_eq!(LockEvent::Acquired as u8, 1);
        assert_eq!(LockEvent::TryAcquired as u8, 2);
        assert_eq!(LockEvent::Released as u8, 3);
    }
}
