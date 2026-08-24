//! Finisher-code composition for the freestanding QEMU integration kernels.
//!
//! A test kernel reports its outcome by writing a code to its board's
//! finisher device (`tairix_arch_aarch64::qemu_exit`,
//! `tairix_arch_riscv64::qemu_exit`), which QEMU turns into its own exit
//! status. Each fixture assigns a small distinct code per internal failure
//! point, plus a *base* it adds an observed value to so the reported code
//! names both the step that failed and what it saw — a child's exit code, or
//! the CPU mask a migration test observed.
//!
//! Both boards read a zero code as *success*, so a failure code is a
//! [`NonZeroU16`]: [`fail_point`] mints one from a literal at compile time and
//! [`fail_code`] composes one that cannot reach zero.
//!
//! That composition has to be total: the observed value comes from the program
//! under test, and composing it wrongly turns a real failure into a debug
//! abort, another failure's code, or a pass. A fixture body compiles only for
//! its bare-metal target, where no host test reaches it, so the arithmetic
//! lives here and is proven once.
//!
//! Test scaffolding: nothing in TAIRiX itself links it.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::num::NonZeroU16;

/// The finisher code naming one of a fixture's failure points.
///
/// Rejects zero at compile time — the expansion is an inline `const` block, so
/// there is no runtime path — because a zero code exits QEMU with the status
/// its runner reads as a pass.
///
/// ```
/// use core::num::NonZeroU16;
/// use tairix_itest_finisher::fail_point;
///
/// const FAIL_POOL: NonZeroU16 = fail_point!(1);
/// assert_eq!(FAIL_POOL.get(), 1);
/// ```
#[macro_export]
macro_rules! fail_point {
    ($code:expr) => {
        const {
            match ::core::num::NonZeroU16::new($code) {
                ::core::option::Option::Some(code) => code,
                ::core::option::Option::None => {
                    ::core::panic!("a zero finisher code reports a failing run as a pass")
                }
            }
        }
    };
}

/// Compose the finisher code reporting `observed` under failure `base`.
///
/// The result never wraps: an `observed` the code space cannot carry saturates
/// at its top instead of aliasing onto a smaller code the fixture assigned to
/// a different failure.
///
/// Saturation is lossy at the top by design — the reportable band is far
/// narrower than the values a program could exit with, and "outside the band"
/// is the useful answer. A code no fixture assigns is what a human sees.
#[must_use]
pub fn fail_code<T: TryInto<u16>>(base: NonZeroU16, observed: T) -> NonZeroU16 {
    base.saturating_add(observed.try_into().unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::fail_code;
    use core::num::NonZeroU16;

    const BASE: NonZeroU16 = fail_point!(100);

    #[test]
    fn a_reportable_observation_is_the_base_plus_itself() {
        assert_eq!(fail_code(BASE, 0_i32).get(), 100);
        assert_eq!(fail_code(BASE, 7_i32).get(), 107);
        assert_eq!(fail_code(BASE, 65_435_i32), NonZeroU16::MAX);
    }

    #[test]
    fn an_observation_above_the_report_saturates_rather_than_wrapping() {
        // The pre-seam form (`FAIL_BASE + (code as u16)`) aborted in debug and
        // aliased onto another fixture's code in release.
        assert_eq!(fail_code(BASE, 65_536_i32), NonZeroU16::MAX);
        assert_eq!(fail_code(BASE, i32::MAX), NonZeroU16::MAX);
        assert_eq!(fail_code(NonZeroU16::MAX, 1_i32), NonZeroU16::MAX);
        assert_eq!(fail_code(fail_point!(65_000), 1_000_i32), NonZeroU16::MAX);
        assert_eq!(fail_code(BASE, u32::MAX), NonZeroU16::MAX);
    }

    #[test]
    fn a_negative_observation_saturates_rather_than_reinterpreting() {
        // `as u16` would have reported -1 as 65_535 + base, i.e. wrapped.
        assert_eq!(fail_code(BASE, -1_i32), NonZeroU16::MAX);
        assert_eq!(fail_code(BASE, i32::MIN), NonZeroU16::MAX);
    }

    #[test]
    fn the_composed_code_is_never_the_success_status() {
        // Zero is a *pass* to both finishers. The smallest base with the
        // smallest observation is the only composition that could reach it, and
        // it reports 1; with `base: u16` and no floor it reported 0.
        assert_eq!(fail_code(NonZeroU16::MIN, 0_i32).get(), 1);
    }

    #[test]
    fn observations_inside_the_band_report_distinct_ordered_codes() {
        // The property the encoding exists for. Staying at or above the base is
        // what keeps a report clear of the small codes a fixture assigns to its
        // own failure points.
        assert_eq!(fail_code(BASE, 0_i32), BASE);
        let mut previous = BASE;
        for observed in 1_i32..=64 {
            let code = fail_code(BASE, observed);
            assert!(
                code > previous,
                "distinct observations report distinct codes"
            );
            assert!(code >= BASE, "a composed code never drops below its base");
            previous = code;
        }
    }

    #[test]
    fn a_literal_failure_point_carries_its_own_value() {
        assert_eq!(fail_point!(1), NonZeroU16::MIN);
        assert_eq!(fail_point!(u16::MAX), NonZeroU16::MAX);
        assert_eq!(fail_point!(42).get(), 42);
    }
}
