//! Test-only counting global allocator for host-side leak soaks.
//!
//! The `plans/APPS.md` I3 reproduction needs to observe "kernel-heap
//! occupancy" on the host: a syscall-cycle soak asserts that a long run of
//! the `top -d0` refresh cycle retains no kernel memory per iteration. The
//! host test binary's heap is the process heap, so the measurement lives
//! here: a thin wrapper around [`std::alloc::System`] that keeps net
//! live-byte balances.
//!
//! Unit tests run in parallel threads inside one process, so one shared
//! counter would be perturbed by unrelated tests (including their
//! deliberately `Box::leak`ed fixtures) and make any assertion flaky. Each
//! measurement therefore owns its own [`LiveBytes`] balance: a soak opts
//! exactly the threads it owns (its caller and its server) into its own
//! counter ([`opt_in_current_thread`]), so the balance it reads is
//! deterministic regardless of what the rest of the test binary is doing.
//!
//! Compiled only under `cfg(test)`: production builds and the integration
//! test binaries never link this allocator.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicIsize, Ordering};

use std::alloc::System;
use std::cell::Cell;

/// One measurement's net allocated-but-not-freed byte balance.
///
/// A test leaks one of these (`Box::leak`, the established fixture pattern)
/// and opts its own threads into it; no other test can touch it.
#[derive(Debug, Default)]
pub struct LiveBytes(AtomicIsize);

impl LiveBytes {
    /// A zeroed balance.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicIsize::new(0))
    }

    /// Net bytes currently allocated-but-not-freed by the threads opted
    /// into this balance.
    ///
    /// A soak samples this once after a warm-up (the baseline) and once
    /// after the measured iterations; a stable cycle keeps the two within a
    /// small, stated slack.
    #[must_use]
    pub fn net(&self) -> isize {
        self.0.load(Ordering::SeqCst)
    }
}

std::thread_local! {
    /// The balance the current thread's allocations and frees feed, if any.
    ///
    /// `const`-initialised so reading it never allocates (a lazily-initialised
    /// TLS slot could re-enter the allocator); a `Cell` of a `Copy` reference
    /// has no destructor to register either.
    static PARTICIPATING: Cell<Option<&'static LiveBytes>> = const { Cell::new(None) };
}

/// Feed the current thread's allocations and frees into `counter` from now
/// on.
///
/// A soak calls this on every thread it owns *after* building its fixtures,
/// so one-time set-up allocations never enter the balance.
pub fn opt_in_current_thread(counter: &'static LiveBytes) {
    PARTICIPATING.with(|slot| slot.set(Some(counter)));
}

/// Stop counting the current thread's allocations and frees.
///
/// A soak that opted in the shared test-runner thread calls this after its
/// final sample so later tests on the same thread never feed the balance.
pub fn opt_out_current_thread() {
    PARTICIPATING.with(|slot| slot.set(None));
}

/// Apply `delta` to the current thread's balance, if it opted into one.
///
/// `try_with` (not `with`) so a free that runs during thread teardown, after
/// the TLS slot is gone, is simply not counted instead of aborting.
fn record(delta: isize) {
    if let Some(counter) = PARTICIPATING.try_with(Cell::get).ok().flatten() {
        counter.0.fetch_add(delta, Ordering::SeqCst);
    }
}

/// Widen a layout size to the signed counter domain.
///
/// A [`Layout`] size never exceeds `isize::MAX` by the `Layout` contract, so
/// the conversion is total; saturate defensively rather than wrap.
fn size_delta(size: usize) -> isize {
    isize::try_from(size).unwrap_or(isize::MAX)
}

/// [`System`] wrapper that keeps the opted-in net live-byte balances.
struct CountingAlloc;

// SAFETY: every method delegates verbatim to `System`, which upholds the
// `GlobalAlloc` contract; the counting is side bookkeeping that touches no
// allocator state and never re-enters the allocator (the TLS slot is
// `const`-initialised).
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded verbatim; the caller upholds the layout contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(size_delta(layout.size()));
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded verbatim; the caller upholds the layout contract.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(size_delta(layout.size()));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim; the caller upholds the layout contract.
        unsafe { System.dealloc(ptr, layout) };
        record(-size_delta(layout.size()));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded verbatim; the caller upholds the layout contract.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            record(size_delta(new_size) - size_delta(layout.size()));
        }
        new_ptr
    }
}

/// The unit-test binary's global allocator: [`System`] plus the opt-in
/// balances.
#[global_allocator]
static COUNTING_ALLOC: CountingAlloc = CountingAlloc;

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::boxed::Box;
    use alloc::vec;

    /// A thread that never opted in moves no balance; an opted-in thread's
    /// transient allocation balances to zero and a retained one is visible
    /// as at least its size until freed.
    #[test]
    fn balance_tracks_only_opted_in_threads() {
        let counter: &'static LiveBytes = Box::leak(Box::new(LiveBytes::new()));

        let outside = std::thread::spawn(|| {
            let buffer = vec![0u8; 4096];
            drop(buffer);
        });
        outside.join().unwrap();
        assert_eq!(counter.net(), 0, "a non-opted-in thread is not counted");

        // Run on a dedicated thread so the opt-in never taints the shared
        // test-runner thread.
        let inside = std::thread::spawn(move || {
            opt_in_current_thread(counter);
            let base = counter.net();
            let buffer = vec![0u8; 4096];
            let held = counter.net() - base;
            drop(buffer);
            let after = counter.net() - base;
            (held, after)
        });
        let (held, after) = inside.join().unwrap();
        assert!(held >= 4096, "retained allocation is visible: {held}");
        assert_eq!(after, 0, "transient allocation balances to zero");
    }
}
