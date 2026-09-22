//! The boot-stack guard's on-target checks, shared by the per-port
//! verticals.
//!
//! What only a real boot can prove is that the linker reserved the guard,
//! that the boot stub poisoned it *after* the `.bss` clear that spans it,
//! and that the port's handle names that same reservation. The judgement
//! itself is `kernel/arch/api`'s and is unit-tested on the host, so the
//! ports share these checks rather than each restating them.

#![no_std]

use core::num::NonZeroU16;

use tairix_arch_api::{BootStackGuard, BootStackGuardRegion};
use tairix_memguard::{CANARY_BYTES, GUARD_BYTE};

/// A way the guard can be wrong on real hardware.
///
/// Distinct per check so a failing transcript names the step, and non-zero
/// so it can carry straight into a `SiFive`-Test exit code.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GuardDefect {
    /// The port reports no guard: the linker reserved none, or the handle
    /// does not know about it.
    NotReserved,
    /// The port's region does not end at the stack's lowest byte, so the
    /// handle and the linker script disagree about where the guard is.
    WrongStackBottom,
    /// The reservation is smaller than the window that gets checked.
    TooSmall,
    /// A byte of the reservation is not the sentinel: either the stub
    /// never filled it, or the `.bss` clear ran afterwards and erased it.
    NotPoisoned,
    /// A freshly booted, untouched guard did not read as intact.
    FreshNotIntact,
    /// A byte written through the canary was not detected.
    DisturbanceMissed,
    /// The guard did not read as intact again after the test restored it.
    RestoreNotIntact,
    /// A stack pointer below the stack's lowest byte was not reported as
    /// an overrun.
    BelowStackMissed,
}

impl GuardDefect {
    /// Stable code for the QEMU exit status, distinct per check.
    ///
    /// Non-zero by construction, so a port whose finisher takes a failure
    /// code needs no fallible conversion at the call site.
    #[must_use]
    pub const fn code(self) -> NonZeroU16 {
        let raw = match self {
            Self::NotReserved => 1,
            Self::WrongStackBottom => 2,
            Self::TooSmall => 3,
            Self::NotPoisoned => 4,
            Self::FreshNotIntact => 5,
            Self::DisturbanceMissed => 6,
            Self::RestoreNotIntact => 7,
            Self::BelowStackMissed => 8,
        };
        match NonZeroU16::new(raw) {
            Some(code) => code,
            // Unreachable: every arm above is a non-zero literal.
            None => NonZeroU16::MIN,
        }
    }

    /// Terse description for the run's transcript.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NotReserved => "the port reserves no boot-stack guard",
            Self::WrongStackBottom => "the guard does not end at the stack's lowest byte",
            Self::TooSmall => "the guard is shorter than the checked window",
            Self::NotPoisoned => "a guard byte is not the sentinel",
            Self::FreshNotIntact => "a freshly booted guard did not read as intact",
            Self::DisturbanceMissed => "a write through the canary went undetected",
            Self::RestoreNotIntact => "the restored guard did not read as intact",
            Self::BelowStackMissed => "a stack pointer below the stack was not an overrun",
        }
    }
}

/// Whether every byte of `[low, high)` still holds the sentinel.
///
/// The borrow is confined to this call so no shared view of the guard is
/// live across the write [`check`] makes.
///
/// # Safety
///
/// `[low, high)` must be the reserved guard: mapped, and claimed by
/// nothing else.
unsafe fn wholly_poisoned(low: u64, high: u64) -> bool {
    let Ok(base) = usize::try_from(low) else {
        return false;
    };
    let Ok(len) = usize::try_from(high - low) else {
        return false;
    };
    // SAFETY: the caller vouched the span is the reserved guard, so it is
    // mapped for the life of the kernel and nothing else writes it.
    let bytes =
        unsafe { core::slice::from_raw_parts(core::ptr::with_exposed_provenance::<u8>(base), len) };
    bytes.iter().all(|&byte| byte == GUARD_BYTE)
}

/// Write `byte` into the guard's topmost address — where a downward
/// overrun arrives first.
///
/// # Safety
///
/// `stack_bottom` must be one past the reserved guard's last byte, and
/// nothing else may be using those bytes.
unsafe fn poke_canary_top(stack_bottom: u64, byte: u8) {
    let Ok(top) = usize::try_from(stack_bottom - 1) else {
        return;
    };
    // SAFETY: the caller vouched the byte below `stack_bottom` is the
    // guard's own, which nothing else reads or writes.
    unsafe { core::ptr::with_exposed_provenance_mut::<u8>(top).write_volatile(byte) };
}

/// Run every on-target check against the port's guard, leaving it poisoned
/// again however the run ends.
///
/// `sp` must be the caller's live stack pointer, which is on the boot stack
/// and so above the guard; the fault-injection leg needs a verdict that is
/// decided by the canary rather than short-circuited by the stack pointer.
///
/// # Errors
///
/// The first [`GuardDefect`] a check finds.
///
/// # Safety
///
/// `guard_low` and `stack_bottom` must be the port's own linker symbols for
/// the reservation `region` names, and no other code may be using the guard
/// (this writes a byte of it and restores it).
pub unsafe fn check(
    region: Option<BootStackGuardRegion>,
    guard_low: u64,
    stack_bottom: u64,
    sp: u64,
) -> Result<(), GuardDefect> {
    let Some(region) = region else {
        return Err(GuardDefect::NotReserved);
    };
    if region.stack_bottom_addr() != stack_bottom {
        return Err(GuardDefect::WrongStackBottom);
    }
    if guard_low >= stack_bottom || stack_bottom - guard_low < CANARY_BYTES as u64 {
        return Err(GuardDefect::TooSmall);
    }
    // The stub's fill must have covered the whole reservation, alignment
    // padding included, and must have outlived the `.bss` clear.
    // SAFETY: the caller vouched the span is the port's reservation.
    if !unsafe { wholly_poisoned(guard_low, stack_bottom) } {
        return Err(GuardDefect::NotPoisoned);
    }
    if region.assess(sp) != BootStackGuard::Intact {
        return Err(GuardDefect::FreshNotIntact);
    }

    // Fault injection: a zero is what both a real overrun and a guard the
    // stub never poisoned leave behind, so it must not read as intact.
    // SAFETY: as above; the byte is the guard's own and is restored below.
    unsafe { poke_canary_top(stack_bottom, 0) };
    let disturbed = region.assess(sp);
    // SAFETY: as above. Restored before the verdict is acted on, so the
    // guard is left armed whatever this check returns.
    unsafe { poke_canary_top(stack_bottom, GUARD_BYTE) };
    if disturbed != BootStackGuard::Disturbed {
        return Err(GuardDefect::DisturbanceMissed);
    }
    if region.assess(sp) != BootStackGuard::Intact {
        return Err(GuardDefect::RestoreNotIntact);
    }

    // A frame larger than the guard steps over it without writing a byte,
    // so the stack pointer has to be decisive on its own.
    let below = stack_bottom - 8;
    if region.assess(below)
        != (BootStackGuard::BelowStack {
            sp: below,
            bytes: 8,
        })
    {
        return Err(GuardDefect::BelowStackMissed);
    }
    Ok(())
}
