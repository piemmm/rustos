//! The auto-trait bounds every lock guard must carry.
//!
//! A guard's `Send`/`Sync` are inferred from its fields unless the type says
//! otherwise, and the inference is wrong for both in ways safe code can
//! exploit:
//!
//! * `SpinLock<T>` is `Sync` for `T: Send`, so a guard holding only
//!   `&SpinLock<T>` inferred `Sync` for `T: Send` too — while sharing a guard
//!   hands `&T` to several threads at once and so needs `T: Sync`. A
//!   `SpinLock<Cell<u32>>` could therefore be raced from safe code.
//! * A guard is bound to the CPU that took the lock. For the IRQ-masking
//!   flavour that is a hardware contract: `InterruptControl::restore` must run
//!   on the CPU that masked, so an inferred `Send` let safe code drop the
//!   guard on another core and leave the first with interrupts masked for
//!   ever.
//!
//! Every assertion below is a fact about a type, so it is checked at build
//! time in a `const` block: a regression fails the compile rather than waiting
//! for someone to run the test. The `#[test]` wrappers exist to name and group
//! the contracts.
//!
//! Both are absences, so they are asserted by probing for the trait rather
//! than by naming it: an inherent associated const is preferred over a trait's
//! default, so `Probe::<T>` reports the inherent `true` exactly when `T`
//! implements the trait and falls back to the blanket `false` when it does
//! not. This is the only way to state a negative bound on stable Rust; a
//! plain `fn needs_sync<T: Sync>()` can assert presence but never absence.

use core::cell::Cell;
use core::marker::PhantomData;

use tairix_sync::{
    IrqSafeSpinLockGuard, McsGuard, NopInterruptControl, RwLockReadGuard, RwLockWriteGuard,
    SpinLockGuard,
};

struct SyncProbe<T: ?Sized>(PhantomData<T>);
trait SyncFallback {
    const SYNC: bool = false;
}
impl<T: ?Sized> SyncFallback for SyncProbe<T> {}
impl<T: ?Sized + Sync> SyncProbe<T> {
    const SYNC: bool = true;
}

struct SendProbe<T: ?Sized>(PhantomData<T>);
trait SendFallback {
    const SEND: bool = false;
}
impl<T: ?Sized> SendFallback for SendProbe<T> {}
impl<T: ?Sized + Send> SendProbe<T> {
    const SEND: bool = true;
}

/// `Cell<u32>` is the witness type throughout: `Send` but not `Sync`, so it is
/// legal inside every lock here yet must never be reachable from two threads
/// at once.
type NotSync = Cell<u32>;

type IrqGuard<'a, T> = IrqSafeSpinLockGuard<'a, T, NopInterruptControl>;

#[test]
fn the_probes_discriminate() {
    const { assert!(SyncProbe::<u32>::SYNC) };
    const { assert!(!SyncProbe::<NotSync>::SYNC) };
    const { assert!(SendProbe::<u32>::SEND) };
    const { assert!(!SendProbe::<*const u32>::SEND) };
}

/// Sharing a guard hands `&T` to every thread that holds the shared
/// reference, so a guard over a `!Sync` payload must not itself be `Sync`.
/// Without this, `thread::scope` plus two `&guard` borrows races a `Cell`
/// from entirely safe code.
#[test]
fn a_guard_over_a_non_sync_payload_is_not_sync() {
    const { assert!(!SyncProbe::<SpinLockGuard<'static, NotSync>>::SYNC) };
    const { assert!(!SyncProbe::<IrqGuard<'static, NotSync>>::SYNC) };
    const { assert!(!SyncProbe::<RwLockReadGuard<'static, NotSync>>::SYNC) };
    const { assert!(!SyncProbe::<RwLockWriteGuard<'static, NotSync>>::SYNC) };
    const { assert!(!SyncProbe::<McsGuard<'static, NotSync>>::SYNC) };
}

/// The bound is a restriction, not a ban: a `Sync` payload still shares.
#[test]
fn a_guard_over_a_sync_payload_is_sync() {
    const { assert!(SyncProbe::<SpinLockGuard<'static, u32>>::SYNC) };
    const { assert!(SyncProbe::<IrqGuard<'static, u32>>::SYNC) };
    const { assert!(SyncProbe::<RwLockReadGuard<'static, u32>>::SYNC) };
    const { assert!(SyncProbe::<RwLockWriteGuard<'static, u32>>::SYNC) };
}

/// A guard names a critical section one CPU is inside. Releasing it from
/// another core unlocks on behalf of a core that never acquired and, under
/// lock diagnostics, pops a per-CPU record it never pushed.
///
/// For the IRQ-masking guard the consequence is hardware-visible rather than
/// merely untidy: `InterruptControl::restore` is documented to take a state
/// its *own* CPU produced, so a guard dropped elsewhere both breaks that
/// contract from safe code and leaves the acquiring core masked for ever.
#[test]
fn a_guard_cannot_be_moved_to_another_cpu() {
    const { assert!(!SendProbe::<SpinLockGuard<'static, u32>>::SEND) };
    const { assert!(!SendProbe::<IrqGuard<'static, u32>>::SEND) };
    const { assert!(!SendProbe::<RwLockReadGuard<'static, u32>>::SEND) };
    const { assert!(!SendProbe::<RwLockWriteGuard<'static, u32>>::SEND) };
    const { assert!(!SendProbe::<McsGuard<'static, u32>>::SEND) };
}

/// The locks themselves stay freely shareable — the restriction is on the
/// guard, not on putting a lock in a `static` or an `Arc`.
#[test]
fn the_locks_themselves_are_still_send_and_sync() {
    use tairix_sync::{IrqSafeSpinLock, McsLock, RwLock, SeqLock, SpinLock};

    const { assert!(SendProbe::<SpinLock<NotSync>>::SEND) };
    const { assert!(SyncProbe::<SpinLock<NotSync>>::SYNC) };
    const { assert!(SendProbe::<IrqSafeSpinLock<NotSync, NopInterruptControl>>::SEND) };
    const { assert!(SyncProbe::<IrqSafeSpinLock<NotSync, NopInterruptControl>>::SYNC) };
    const { assert!(SendProbe::<McsLock<NotSync>>::SEND) };
    const { assert!(SyncProbe::<McsLock<NotSync>>::SYNC) };
    const { assert!(SendProbe::<SeqLock<u32>>::SEND) };
    const { assert!(SyncProbe::<SeqLock<u32>>::SYNC) };
    // `RwLock` hands out `&T` to concurrent readers, so unlike the mutual
    // exclusion above it needs `T: Sync` to be shared at all.
    const { assert!(SyncProbe::<RwLock<u32>>::SYNC) };
    const { assert!(!SyncProbe::<RwLock<NotSync>>::SYNC) };
}
