//! Fault-guarded user-memory span copy — the hardware backstop of the
//! kernel's `copy_from_user` / `copy_to_user` boundary
//! (`tests/SECURITY.md` §5).
//!
//! The architecture-neutral copy path (`kernel/mem`'s `uaccess`) proves
//! every page of a user range mapped and permissioned *before* it moves
//! a single byte, and it moves the bytes through the kernel's own
//! direct physical map while the caller's address-space registry guard
//! is held — so under correct operation the copy can never touch an
//! unmapped address. This module is the backstop for when that proof is
//! violated anyway (a kernel defect, a corrupted table, a stale direct-map
//! window): the byte move itself runs inside a per-architecture **fault
//! window**, a short span of instructions the port's trap handler knows
//! by address. A synchronous data fault whose program counter lies inside
//! the window is redirected to the window's fix-up: the copy returns an
//! error and the kernel keeps running, instead of the fault falling
//! through to the port's fatal halt. The caller observes a failed copy
//! (`kernel/mem` maps it onto its fail-closed error), never a partial
//! result treated as valid, and never a halted machine.
//!
//! # The slot
//!
//! The windowed copy routine is target-specific naked assembly (the
//! window bounds are *instruction addresses*), so it lives in each
//! architecture port. The port publishes it here through a set-once
//! slot at trap-vector initialisation — the same install-before-first-
//! fault discipline every port's fault-handler slot follows — and the
//! architecture-neutral copy path reaches it through [`copy_user_span`]
//! without naming any concrete port.
//!
//! With no routine installed [`copy_user_span`] performs a plain
//! forward copy. That is the honest implementation on targets with no
//! synchronous-fault source to guard against (`wasm32`, where the host
//! sandbox owns memory faults, and the host test build), and on a
//! bare-metal target it is exactly the pre-backstop behaviour: the copy
//! still works, and a mid-copy fault falls through to the port's
//! fail-closed fatal path. Installing the routine only ever *adds* the
//! recovery; forgetting it can never widen authority.
//!
//! # Contract of a guarded copy routine
//!
//! A [`GuardedCopyFn`] behaves as `memcpy` over non-overlapping regions
//! and returns `0` on success. When a hardware data fault interrupts the
//! move, the port's trap handler resumes the routine at its fix-up and
//! it returns non-zero instead; the destination bytes are then
//! unspecified and the caller must discard them. The routine never
//! panics, takes no lock, and touches no memory outside
//! `[dst, dst + len)` / `[src, src + len)`.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Signature of a port's fault-windowed span copy.
///
/// Copies `len` bytes from `src` to `dst` (forward, `memcpy` semantics —
/// the regions must not overlap) and returns `0`. Returns non-zero when
/// a synchronous data fault interrupted the move and the port's trap
/// handler resumed the routine at its fix-up; the destination contents
/// are then unspecified and must be discarded.
///
/// # Safety
///
/// The caller must guarantee `dst` is valid for `len` writable bytes and
/// that the regions do not overlap. `src` need only be a *plausibly*
/// readable range: a fault raised by reading (or writing) it mid-copy is
/// exactly what the window absorbs and reports as the non-zero return.
pub type GuardedCopyFn = unsafe extern "C" fn(dst: *mut u8, src: *const u8, len: usize) -> usize;

/// Slot holding the installed guarded copy routine as a raw function
/// pointer (`0` = none installed).
static GUARDED_COPY: AtomicUsize = AtomicUsize::new(0);

/// Failure mode of [`install_guarded_copy`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum InstallGuardedCopyError {
    /// A *different* routine was already published; the slot is set-once
    /// per boot. Re-installing the same routine is idempotent and
    /// succeeds, so a per-CPU init path may call the install
    /// unconditionally.
    AlreadyInstalled,
}

/// Publish the port's fault-windowed copy routine.
///
/// Called by the port's trap-vector initialisation, before user space
/// (and therefore any syscall copy) can run. Idempotent for the same
/// routine so a per-CPU init path may install unconditionally; a
/// *conflicting* second install fails closed.
///
/// # Errors
///
/// [`InstallGuardedCopyError::AlreadyInstalled`] when a different
/// routine is already published.
pub fn install_guarded_copy(cb: GuardedCopyFn) -> Result<(), InstallGuardedCopyError> {
    let raw = cb as usize;
    match GUARDED_COPY.compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(()),
        Err(existing) if existing == raw => Ok(()),
        Err(_) => Err(InstallGuardedCopyError::AlreadyInstalled),
    }
}

/// Read back the installed guarded copy routine, if any. A test /
/// diagnostic observer; [`copy_user_span`] is the consuming path.
#[must_use]
pub fn guarded_copy() -> Option<GuardedCopyFn> {
    let raw = GUARDED_COPY.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every value stored into the slot round-trips a valid
        // `GuardedCopyFn` through `install_guarded_copy`; function
        // pointers are `usize`-sized so the transmute is lossless.
        Some(unsafe { core::mem::transmute::<usize, GuardedCopyFn>(raw) })
    }
}

#[cfg(test)]
fn clear_guarded_copy_for_tests() {
    // Test-only: lets a host test exercise both the default and the
    // installed dispatch without poisoning its siblings. Production code
    // never clears the slot.
    GUARDED_COPY.store(0, Ordering::Release);
}

/// `true` iff `pc` lies inside the half-open fault window
/// `[begin, end)` — the one containment predicate every port's trap
/// handler applies to decide whether a kernel-mode data fault belongs
/// to its guarded copy routine.
#[must_use]
pub const fn pc_in_window(pc: u64, begin: u64, end: u64) -> bool {
    begin <= pc && pc < end
}

/// A hardware data fault interrupted a guarded span copy.
///
/// The destination bytes are unspecified; the caller discards them and
/// fails its operation closed.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct CopySpanFault;

/// Move `len` bytes from `src` to `dst` under the port's fault window.
///
/// Dispatches to the installed [`GuardedCopyFn`]; with none installed it
/// performs the plain forward copy (the honest implementation where no
/// synchronous-fault source exists — `wasm32` and the host test build).
///
/// # Errors
///
/// [`CopySpanFault`] when a hardware fault interrupted the move. The
/// destination contents are then unspecified and must be discarded.
///
/// # Safety
///
/// `dst` must be valid for `len` writable bytes, `src` for `len`
/// readable bytes *as far as the caller's software validation can
/// prove*, and the regions must not overlap. A residual fault taken
/// mid-copy — the case the validation could not see — is absorbed by
/// the window and reported as the error, never propagated as a crash.
pub unsafe fn copy_user_span(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> Result<(), CopySpanFault> {
    if let Some(copy) = guarded_copy() {
        // SAFETY: the caller upholds the validity and non-overlap
        // contract; the routine's window absorbs a residual fault.
        let not_copied = unsafe { copy(dst, src, len) };
        if not_copied == 0 {
            Ok(())
        } else {
            Err(CopySpanFault)
        }
    } else {
        // SAFETY: the caller upholds the validity and non-overlap
        // contract; with no fault source this plain copy is total.
        unsafe { core::ptr::copy_nonoverlapping(src, dst, len) };
        Ok(())
    }
}

/// Shared conformance checks every MMU port's fault-window QEMU vertical
/// runs, so the proof is one definition instead of three re-derivations.
pub mod conformance {
    use super::{copy_user_span, guarded_copy, CopySpanFault};

    /// Outcome of [`run`], distinguishing each broken invariant so a
    /// failing vertical's exit status pinpoints it.
    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    pub enum Verdict {
        /// Every check held.
        Pass,
        /// No guarded copy routine was installed before the check ran.
        NotInstalled,
        /// The positive-control copy failed or corrupted its bytes.
        IntactCopyBroken,
        /// The copy over the unmapped page did not report a fault — the
        /// window never absorbed the hardware fault (or the page was
        /// still mapped).
        FaultNotReported,
    }

    /// Run the shared checks.
    ///
    /// `unmapped_va` must be the base of a 4 KiB virtual page with no
    /// mapping in the active translation regime (a page that was never
    /// mapped, or one genuinely unmapped and TLB-flushed); `scratch` is a
    /// small mapped buffer for the positive control.
    ///
    /// Checks, in order: a routine is installed; a copy between mapped
    /// buffers succeeds byte-exactly; a copy whose *source* lies in the
    /// unmapped page returns the fault error and execution continues;
    /// a copy whose *destination* lies in the unmapped page returns the
    /// fault error and execution continues.
    ///
    /// # Safety
    ///
    /// `unmapped_va` must satisfy the contract stated above: the base of
    /// a 4 KiB virtual page with no mapping in the active translation
    /// regime. The checks deliberately access it, relying on the guarded
    /// copy window to absorb the resulting hardware fault; a still-mapped
    /// page turns those accesses into stray reads and writes of live
    /// memory instead of contained faults.
    #[must_use]
    pub unsafe fn run(unmapped_va: *mut u8, scratch: &mut [u8; 64]) -> Verdict {
        if guarded_copy().is_none() {
            return Verdict::NotInstalled;
        }

        // Positive control: an intact copy moves its bytes exactly.
        let mut src = [0u8; 64];
        for (i, b) in (0u8..).zip(src.iter_mut()) {
            *b = i ^ 0x5A;
        }
        scratch.fill(0);
        // SAFETY: `src` and `scratch` are distinct live stack/caller
        // buffers of the stated length.
        let intact = unsafe { copy_user_span(scratch.as_mut_ptr(), src.as_ptr(), src.len()) };
        if intact != Ok(()) || scratch != &src {
            return Verdict::IntactCopyBroken;
        }

        // A read of the unmapped page must fault, be absorbed by the
        // window, and surface as the error — with execution continuing
        // here rather than the port's fatal path.
        // SAFETY: `scratch` is valid for writes; the unmapped source is
        // exactly the residual-fault case the window contract absorbs.
        let read_faulted =
            unsafe { copy_user_span(scratch.as_mut_ptr(), unmapped_va.cast_const(), 8) };
        if read_faulted != Err(CopySpanFault) {
            return Verdict::FaultNotReported;
        }

        // A write to the unmapped page must likewise surface as the
        // error (the store side of the window).
        // SAFETY: `src` is valid for reads; the unmapped destination is
        // the residual-fault case the window contract absorbs.
        let write_faulted = unsafe { copy_user_span(unmapped_va, src.as_ptr(), 8) };
        if write_faulted != Err(CopySpanFault) {
            return Verdict::FaultNotReported;
        }

        Verdict::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The install/dispatch assertions all mutate the single process-wide
    // `GUARDED_COPY` slot, so they live in one test: cargo runs `#[test]`s
    // in parallel threads and two of them clearing and reinstalling the
    // same static would race.
    #[test]
    fn slot_dispatch_default_and_set_once_semantics() {
        // A routine whose non-zero return models a mid-copy fault.
        unsafe extern "C" fn always_faults(_dst: *mut u8, _src: *const u8, len: usize) -> usize {
            len.max(1)
        }
        // A routine that completes the copy and reports success.
        unsafe extern "C" fn other(dst: *mut u8, src: *const u8, len: usize) -> usize {
            // SAFETY: forwarded contract from the caller.
            unsafe { core::ptr::copy_nonoverlapping(src, dst, len) };
            0
        }

        clear_guarded_copy_for_tests();
        assert!(guarded_copy().is_none());

        // Default path: a plain copy moves the bytes.
        let src = [1u8, 2, 3, 4];
        let mut dst = [0u8; 4];
        // SAFETY: distinct live buffers of the stated length.
        let outcome = unsafe { copy_user_span(dst.as_mut_ptr(), src.as_ptr(), src.len()) };
        assert_eq!(outcome, Ok(()));
        assert_eq!(dst, src);

        // Installed path: the routine is consulted and its non-zero
        // return surfaces as the fault error.
        install_guarded_copy(always_faults).expect("first install");
        assert_eq!(
            guarded_copy().map(|f| f as usize),
            Some(always_faults as GuardedCopyFn as usize)
        );
        let mut dst2 = [0u8; 4];
        // SAFETY: distinct live buffers of the stated length.
        let faulted = unsafe { copy_user_span(dst2.as_mut_ptr(), src.as_ptr(), src.len()) };
        assert_eq!(faulted, Err(CopySpanFault));

        // Re-installing the same routine is idempotent; a conflicting
        // routine fails closed.
        assert_eq!(install_guarded_copy(always_faults), Ok(()));
        assert_eq!(
            install_guarded_copy(other),
            Err(InstallGuardedCopyError::AlreadyInstalled)
        );

        // A successful (zero) return from the installed routine is Ok.
        clear_guarded_copy_for_tests();
        install_guarded_copy(other).expect("reinstall after clear");
        let mut dst3 = [0u8; 4];
        // SAFETY: distinct live buffers of the stated length.
        let ok = unsafe { copy_user_span(dst3.as_mut_ptr(), src.as_ptr(), src.len()) };
        assert_eq!(ok, Ok(()));
        assert_eq!(dst3, src);
        clear_guarded_copy_for_tests();
    }

    #[test]
    fn pc_window_containment_is_half_open() {
        assert!(pc_in_window(0x1000, 0x1000, 0x1010));
        assert!(pc_in_window(0x100F, 0x1000, 0x1010));
        assert!(!pc_in_window(0x1010, 0x1000, 0x1010));
        assert!(!pc_in_window(0x0FFF, 0x1000, 0x1010));
        // An empty window contains nothing.
        assert!(!pc_in_window(0x1000, 0x1000, 0x1000));
    }
}
