//! Guard-region poison: the sentinel a guarded memory region is filled with,
//! and the window a hot path verifies it through.
//!
//! A *guard region* sits below a region that must not be overrun — a kernel
//! stack, a slab's object array — so a contiguous overrun lands in the guard
//! instead of in the neighbour. The deployment form is an **unmapped** page,
//! where the overrun faults and nothing is ever written; where no page-table
//! split is available the region is filled with a sentinel instead and a
//! disturbance of it is the overrun's signature. The early-boot stack is the
//! second case by necessity: it is in use before the MMU is on, so no mapping
//! exists to take away.
//!
//! Both forms are the same defence, and every guarded region in the kernel is
//! read by the same post-mortem, so the sentinel and the verified window are
//! defined once here rather than copied into each guard.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Byte a guarded region is filled with: `0xCC`, the x86 `int3` trap.
///
/// An "obviously wrong" value in every role it can be misread as — as code it
/// traps, as a pointer it is non-canonical, as a length it is absurd — so a
/// disturbance is unambiguous and a stale guard byte that escapes into live
/// data is loud rather than plausible.
pub const GUARD_BYTE: u8 = 0xCC;

/// Bytes at the top of a guard region (immediately below the region it
/// protects) that a hot path verifies.
///
/// A stack grows downward and is written contiguously, so an overrun crosses
/// this window first: checking it is O(1) and catches the overrun without
/// scanning the whole guard. The rest of the guard is absorption, not
/// detection.
pub const CANARY_BYTES: usize = 64;

/// The window at the top of `guard` — the [`CANARY_BYTES`] immediately below
/// the region it protects — that [`canary_intact`] is asked about.
///
/// A guard shorter than the window yields the whole guard rather than
/// nothing, so a short guard is still checked over every byte it has.
#[must_use]
pub fn canary_window(guard: &[u8]) -> &[u8] {
    &guard[guard.len().saturating_sub(CANARY_BYTES)..]
}

/// Whether every byte of `window` still holds [`GUARD_BYTE`].
///
/// `false` is an overrun: something wrote through the guard. An empty window
/// is also `false` — a guard that reserved no bytes has detection switched
/// off by construction, and saying so is the fail-closed answer; reporting
/// the vacuous `true` would hand back a guarantee no byte is evidence for.
#[must_use]
pub fn canary_intact(window: &[u8]) -> bool {
    !window.is_empty() && window.iter().all(|&byte| byte == GUARD_BYTE)
}

#[cfg(test)]
mod tests {
    use super::{canary_intact, canary_window, CANARY_BYTES, GUARD_BYTE};

    #[test]
    fn the_window_is_the_top_of_the_guard_where_an_overrun_arrives() {
        let mut guard = [GUARD_BYTE; CANARY_BYTES * 4];
        // A downward overrun crosses the guard's highest addresses first.
        let top = guard.len() - 1;
        guard[top] = 0;
        assert!(!canary_intact(canary_window(&guard)));
    }

    #[test]
    fn a_disturbance_below_the_window_is_absorption_not_detection() {
        let mut guard = [GUARD_BYTE; CANARY_BYTES * 4];
        guard[0] = 0;
        assert!(canary_intact(canary_window(&guard)));
    }

    #[test]
    fn a_guard_shorter_than_the_window_yields_the_whole_guard() {
        let guard = [GUARD_BYTE; CANARY_BYTES / 2];
        assert_eq!(canary_window(&guard).len(), guard.len());
        assert!(!canary_intact(canary_window(&[0; CANARY_BYTES / 2])));
    }

    #[test]
    fn a_freshly_poisoned_window_is_intact() {
        let window = [GUARD_BYTE; CANARY_BYTES];
        assert!(canary_intact(&window));
    }

    #[test]
    fn a_single_disturbed_byte_anywhere_trips_the_canary() {
        for at in 0..CANARY_BYTES {
            let mut window = [GUARD_BYTE; CANARY_BYTES];
            window[at] = 0;
            assert!(
                !canary_intact(&window),
                "a write at offset {at} must not read as intact"
            );
        }
    }

    #[test]
    fn a_zeroed_window_trips_the_canary() {
        // The case the boot stack actually presents: `.bss` is zeroed before
        // the canary is installed, so "never poisoned" must not read as
        // intact.
        assert!(!canary_intact(&[0; CANARY_BYTES]));
    }

    #[test]
    fn an_empty_window_cannot_vouch_for_a_guard_and_so_is_not_intact() {
        assert!(!canary_intact(&[]));
    }
}
