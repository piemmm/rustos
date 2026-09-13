//! A published function pointer every callback seam installs through.
//!
//! A kernel is full of slots a port fills once at boot and a hot path reads
//! on every trap: the timer tick, the syscall dispatcher, the fault handler,
//! the user-entry observer. Each was a hand-written `AtomicUsize` holding
//! `f as usize`, read back with `transmute::<usize, F>`.
//!
//! That round trip through an integer discards the pointer's provenance.
//! Calling what comes back is undefined under the strict model the UB oracle
//! checks, and the permissive model only tolerates it because the store side
//! happened to expose the pointer first — a property no reader of the load
//! site can see. Holding the pointer *as* a pointer keeps the provenance and
//! needs no such argument.
//!
//! The duplication mattered as much as the unsafety: the same twenty lines,
//! and the same `unsafe`, sat in every arch port. Here the `unsafe` is one
//! block, and [`FnPtr`] makes a slot for anything that is not a function
//! pointer a compile error rather than a review question.

use core::fmt;
use core::marker::{FnPtr, PhantomData};
use core::mem::{size_of, transmute_copy};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

/// A slot holding one `F`, or nothing.
///
/// Deliberately a `core` atomic rather than the crate's `loom` shim: a slot
/// is written once during boot and never during a model run, so letting the
/// checker explore it would multiply every interleaving for a location that
/// cannot race.
pub struct FnCell<F: FnPtr + Copy> {
    slot: AtomicPtr<()>,
    _fn: PhantomData<F>,
}

impl<F: FnPtr + Copy> FnCell<F> {
    /// A function pointer is thin, so it round-trips through a data pointer
    /// exactly. Stated as a build failure rather than trusted.
    const IS_THIN: () = assert!(
        size_of::<F>() == size_of::<*mut ()>(),
        "FnCell holds only thin function pointers"
    );

    /// An empty slot. `const`, so a seam can place one in a `static`.
    #[must_use]
    pub const fn empty() -> Self {
        let () = Self::IS_THIN;
        Self {
            slot: AtomicPtr::new(ptr::null_mut()),
            _fn: PhantomData,
        }
    }

    /// Publish `f`, replacing whatever was there. The last writer wins.
    pub fn install(&self, f: F) {
        self.slot.store(f.addr().cast_mut(), Ordering::Release);
    }

    /// Claim an empty slot for `f`, reporting whether this call is the one
    /// that took it.
    ///
    /// For a caller that must know it went first — one publishing a second,
    /// dependent slot only if it owns the pair, so a loser cannot overwrite
    /// half of a winner's work.
    ///
    /// A repeat of the same `f` reports `false`, and there is deliberately no
    /// variant that waves it through as already-satisfied: deciding that
    /// would mean comparing the held pointer against the offered one, and two
    /// coercions of one `fn` item are not guaranteed to produce equal
    /// addresses. Miri hands out a fresh address per coercion and shows the
    /// comparison answering "different routine" for the same function, so a
    /// slot built on it would fail open exactly where it claimed to fail
    /// closed. A caller that must install from several CPUs elects one to
    /// claim rather than asking the slot to recognise a routine.
    pub fn claim(&self, f: F) -> bool {
        self.slot
            .compare_exchange(
                ptr::null_mut(),
                f.addr().cast_mut(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Empty the slot, so a later [`load`](Self::load) reports nothing
    /// installed.
    ///
    /// The unregister half of a seam whose provider can go away — a port
    /// tearing a dispatch vector down, a test restoring the slot it
    /// borrowed. A reader that already loaded the pointer keeps calling it,
    /// so a live provider is retired by quiescing its callers first, never
    /// by clearing the slot under them.
    pub fn clear(&self) {
        self.slot.store(ptr::null_mut(), Ordering::Release);
    }

    /// The installed function's address, or null while the slot is empty.
    ///
    /// For a consumer that wants the address rather than a callable — an
    /// SMP trampoline stamping an entry point into a boot slot the hardware
    /// jumps to. Keeps the unstable [`FnPtr`] bound inside this module
    /// instead of every caller that needs one address.
    #[must_use]
    pub fn addr(&self) -> *const () {
        self.slot.load(Ordering::Acquire).cast_const()
    }

    /// The installed function, or [`None`] while the slot is empty.
    #[must_use]
    pub fn load(&self) -> Option<F> {
        let raw = self.slot.load(Ordering::Acquire);
        if raw.is_null() {
            return None;
        }
        // SAFETY: the slot only ever holds `F::addr()` of an `F` handed to
        // an install, whose provenance the pointer preserved,
        // published with `Release` against this `Acquire` load. `F` is a thin
        // function pointer by `IS_THIN`, so the sizes match exactly.
        Some(unsafe { transmute_copy::<*mut (), F>(&raw) })
    }

    /// Whether a function is installed, without reconstructing it.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        !self.slot.load(Ordering::Acquire).is_null()
    }
}

/// The address, not the signature: a slot's identity for a reader is which
/// function is in it.
impl<F: FnPtr + Copy> fmt::Debug for FnCell<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FnCell").field(&self.addr()).finish()
    }
}

impl<F: FnPtr + Copy> Default for FnCell<F> {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::FnCell;

    fn seven() -> u32 {
        7
    }
    fn nine() -> u32 {
        9
    }
    extern "C" fn takes_args(a: u32, b: u32) -> u32 {
        a + b
    }

    #[test]
    fn an_empty_cell_holds_nothing() {
        let cell: FnCell<fn() -> u32> = FnCell::empty();
        assert!(!cell.is_installed());
        assert!(cell.load().is_none());
    }

    #[test]
    fn an_installed_function_is_the_one_that_comes_back() {
        let cell: FnCell<fn() -> u32> = FnCell::empty();
        cell.install(seven);
        assert!(cell.is_installed());
        assert_eq!(cell.load().expect("installed")(), 7);
    }

    #[test]
    fn install_replaces_whatever_was_there() {
        let cell: FnCell<fn() -> u32> = FnCell::empty();
        cell.install(seven);
        cell.install(nine);
        assert_eq!(cell.load().expect("installed")(), 9);
    }

    /// `claim` answers "did I go first", so the second caller loses even when
    /// it offers the same function.
    #[test]
    fn claim_reports_only_the_winner() {
        let cell: FnCell<fn() -> u32> = FnCell::empty();
        assert!(cell.claim(seven), "an empty slot is claimable");
        assert!(
            !cell.claim(seven),
            "a repeat of the same function still lost"
        );
        assert!(!cell.claim(nine), "a different function lost too");
        assert_eq!(cell.load().expect("installed")(), 7);
    }

    #[test]
    fn clear_empties_the_slot_and_reopens_the_claim() {
        let cell: FnCell<fn() -> u32> = FnCell::empty();
        cell.install(seven);
        cell.clear();
        assert!(!cell.is_installed());
        assert!(cell.load().is_none());
        assert!(cell.claim(nine), "a cleared slot is claimable");
    }

    /// Null-ness is the whole guarantee `addr` can offer: a production
    /// caller tests it against zero, and comparing it to a fresh coercion of
    /// the same function is not something Rust promises.
    #[test]
    fn addr_is_null_exactly_while_the_slot_is_empty() {
        let cell: FnCell<fn() -> u32> = FnCell::empty();
        assert!(cell.addr().is_null());
        cell.install(seven);
        assert!(!cell.addr().is_null());
        cell.clear();
        assert!(cell.addr().is_null());
    }

    /// The `extern "C"` signatures the arch ports install are the point of
    /// the cell, so one is exercised rather than only Rust-ABI pointers.
    #[test]
    fn an_extern_c_pointer_round_trips_and_calls() {
        let cell: FnCell<extern "C" fn(u32, u32) -> u32> = FnCell::empty();
        cell.install(takes_args);
        assert_eq!(cell.load().expect("installed")(2, 3), 5);
    }

    /// A cell in a `static` is what every seam actually uses.
    #[test]
    fn a_static_cell_is_const_constructible() {
        static CELL: FnCell<fn() -> u32> = FnCell::empty();
        assert!(CELL.load().is_none() || CELL.load().is_some());
        CELL.install(seven);
        assert_eq!(CELL.load().expect("installed")(), 7);
    }
}
