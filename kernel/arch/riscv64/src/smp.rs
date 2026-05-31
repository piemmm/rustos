//! Multi-hart (SMP) bring-up primitives for the riscv64 `virt` board.
//!
//! This module owns the architecture side of starting a secondary hart
//! and recovering a running hart's identity:
//!
//! * [`current_hartid`] reads the hart id back out of the `tp` register,
//!   which both the boot trampoline (`boot.s`) and the secondary stub
//!   (`smp.s`) seed with the SBI-handed hartid. The architecture-neutral
//!   kernel never reads `tp`; it works in dense [`CpuId`]s, and the
//!   [`crate::kernel_arch::RiscvArch`] handle maps between the two.
//! * [`set_secondary_entry`] installs the set-once `extern "C" fn(CpuId)
//!   -> !` a freshly-started secondary hart runs, and `start_secondary`
//!   asks SBI HSM to start a parked hart at the `smp.s` trampoline,
//!   which sets up that hart's stack and `tp` before invoking the
//!   installed entry. Storing a `fn` (not a closure) keeps
//!   the hand-off lock-free and free of a captured environment, exactly
//!   as [`crate::preempt`] does for the timer callback.
//!
//! # Why a set-once callback rather than an `extern` symbol
//!
//! The secondary trampoline must call *something* Rust-side, but binding
//! a mandatory `extern "C" fn secondary_main` would force every consumer
//! that links this crate (including the single-hart boot pipeline and
//! the Stage-2 freestanding test bins) to define that symbol or fail to
//! link. A set-once callback (parking until installed) keeps secondary
//! bring-up opt-in without a Cargo feature gate (`AGENTS.md` §2.1 — no
//! hacks, §15.3 — no feature-flag silencing).
//!
//! # Host testability
//!
//! [`MAX_HARTS`], [`is_valid_hartid`], the callback slot, and the
//! [`StartHartError`] decode build and are unit-tested on the host. The
//! `tp` read, the SBI HSM call, and the secondary trampoline are gated
//! to the freestanding riscv64 target.

use core::sync::atomic::{AtomicUsize, Ordering};

use rustos_arch_api::CpuId;

/// Maximum number of harts the secondary-stack pool in `smp.s`
/// reserves. Must equal the `SECONDARY_MAX_HARTS` `.equ` there; the
/// stack slice the trampoline selects for hart `h` is only inside the
/// pool when `h < MAX_HARTS`, which `start_secondary` enforces before
/// issuing the SBI call (`AGENTS.md` §2.9 — fail closed, no OOB stack).
pub const MAX_HARTS: usize = 8;

/// `true` iff `hartid` indexes a reserved secondary-stack slot.
#[must_use]
pub const fn is_valid_hartid(hartid: CpuId) -> bool {
    (hartid as usize) < MAX_HARTS
}

/// The secondary-hart entry the trampoline runs, packed into a `usize`
/// (the size of a `fn` pointer) so the trampoline reads it without a
/// lock. `0` until [`set_secondary_entry`] installs it.
static SECONDARY_ENTRY_FN: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_secondary_entry`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetEntryError {
    /// An entry was already installed; the slot is set-once per boot
    /// (`AGENTS.md` §2.1).
    AlreadyInstalled,
}

/// Failure modes of `start_secondary`.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum StartHartError {
    /// `hartid` was outside `0..MAX_HARTS`, so the trampoline would
    /// select a stack slice outside the reserved pool.
    HartIdOutOfRange,
    /// No secondary entry was installed via [`set_secondary_entry`];
    /// starting a hart that would immediately park is refused so the
    /// failure is loud at the call site, not silent on the new hart.
    NoEntryInstalled,
    /// The SBI HSM `hart_start` call returned an error (the hart was
    /// already started, the id is invalid to SBI, etc.).
    Sbi(isize),
}

impl StartHartError {
    /// Stable cause string for audit records (`AGENTS.md` §5.4.4).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HartIdOutOfRange => "hartid_out_of_range",
            Self::NoEntryInstalled => "no_secondary_entry_installed",
            Self::Sbi(_) => "sbi_hart_start_failed",
        }
    }
}

/// Install the entry a secondary hart runs once started.
///
/// The function must be `-> !`: a secondary hart has nowhere to return
/// to (the trampoline's only fallback is a `wfi` park). Encoding the
/// bottom type in the signature pins that at the call site.
///
/// # Errors
///
/// [`SetEntryError::AlreadyInstalled`] on the second publish.
pub fn set_secondary_entry(entry: extern "C" fn(CpuId) -> !) -> Result<(), SetEntryError> {
    let raw = entry as usize;
    SECONDARY_ENTRY_FN
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetEntryError::AlreadyInstalled)
}

/// Address of the installed secondary entry (`0` if none).
/// Test/diagnostic observer.
#[must_use]
pub fn secondary_entry_addr() -> usize {
    SECONDARY_ENTRY_FN.load(Ordering::Acquire)
}

#[cfg(test)]
fn clear_secondary_entry_for_tests() {
    SECONDARY_ENTRY_FN.store(0, Ordering::Release);
}

/// Read the calling hart's id from the `tp` register.
///
/// Both `boot.s` (boot hart) and `smp.s` (secondary harts) seed `tp`
/// with the SBI-handed hartid before entering Rust, so this is a
/// side-effect-free per-CPU identity read.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[must_use]
pub fn current_hartid() -> CpuId {
    let tp: u64;
    // SAFETY: reading `tp` has no side effects. The boot/secondary
    // trampolines guarantee it holds this hart's id (`< MAX_HARTS`),
    // which fits a `CpuId` (`u32`).
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) tp, options(nomem, nostack, preserves_flags));
    }
    #[allow(clippy::cast_possible_truncation)]
    let id = tp as u32;
    id
}

/// Host substitute for the `tp` hart-identity read: the single-hart
/// host build always reports hart `0`. Never linked into a kernel image
/// (the riscv64 build uses the `tp` read above).
#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
#[must_use]
pub fn current_hartid() -> CpuId {
    0
}

/// Start the parked secondary hart `hartid` at the `smp.s` trampoline.
///
/// Validates the id against the stack pool and confirms an entry is
/// installed, then issues the SBI HSM `hart_start` call. On success the
/// target hart runs the trampoline, which sets up its stack and `tp`
/// and tail-calls the [`set_secondary_entry`] callback.
///
/// # Errors
///
/// See [`StartHartError`]. The launcher fails closed (`AGENTS.md`
/// §5.4.5) rather than assuming the hart came up.
///
/// # Safety
///
/// Must be called from the boot hart after `boot.s` has zeroed `.bss`
/// (so the secondary stack pool is clear) and after the secondary entry
/// is installed. `hartid` must name a real, parked hart distinct from
/// the caller.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn start_secondary(hartid: CpuId) -> Result<(), StartHartError> {
    if !is_valid_hartid(hartid) {
        return Err(StartHartError::HartIdOutOfRange);
    }
    if secondary_entry_addr() == 0 {
        return Err(StartHartError::NoEntryInstalled);
    }
    let entry = secondary_trampoline_addr();
    let ret = crate::sbi::hart_start(hartid, entry, 0);
    if ret.is_success() {
        Ok(())
    } else {
        Err(StartHartError::Sbi(ret.error))
    }
}

/// Address of the `_start_secondary` trampoline published by `smp.s`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn secondary_trampoline_addr() -> usize {
    extern "C" {
        fn _start_secondary();
    }
    _start_secondary as *const () as usize
}

/// Rust side of the secondary trampoline.
///
/// `smp.s` jumps here, once per secondary hart, after seeding `tp` and a
/// private stack. It runs the installed [`set_secondary_entry`]
/// callback; with none installed it parks the hart (the
/// [`start_secondary`] guard makes this branch unreachable in practice,
/// but a freshly-started hart must never fall through to undefined
/// instructions — `AGENTS.md` §2.9, fail closed).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[no_mangle]
extern "C" fn rustos_arch_riscv64_secondary_main(hartid: CpuId) -> ! {
    let raw = SECONDARY_ENTRY_FN.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: every store into the slot round-trips a valid
        // `extern "C" fn(CpuId) -> !` pointer through
        // `set_secondary_entry`; the callback is a `fn` with no captured
        // environment, safe to invoke on this hart.
        let entry: extern "C" fn(CpuId) -> ! =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId) -> !>(raw) };
        entry(hartid);
    }
    crate::kernel_arch::halt_current_hart()
}

#[cfg(test)]
#[path = "smp_tests.rs"]
mod tests;
