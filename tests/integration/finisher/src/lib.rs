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
//! That addition has to be total: the observed value comes from the program
//! under test, and composing it wrongly turns a real failure into a debug
//! abort, another failure's code, or a pass. A fixture body compiles only for
//! its bare-metal target, where no host test reaches it, so [`fail_code`]
//! lives here and is proven once.
//!
//! Test scaffolding: nothing in TAIRiX itself links it.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Compose the finisher code reporting `observed` under failure `base`.
///
/// The result is at least `1` and never wraps: an `observed` the code space
/// cannot carry saturates at its top instead of aliasing onto a smaller code
/// the fixture assigned to a different failure, and the floor holds because
/// both boards' finishers read a zero code as *success*.
///
/// Saturation is lossy at the top by design — the reportable band is far
/// narrower than the values a program could exit with, and "outside the band"
/// is the useful answer. A code no fixture assigns is what a human sees.
#[must_use]
pub fn fail_code<T: TryInto<u16>>(base: u16, observed: T) -> u16 {
    let observed = observed.try_into().unwrap_or(u16::MAX);
    base.saturating_add(observed).max(1)
}

#[cfg(test)]
mod tests {
    use super::fail_code;

    #[test]
    fn a_reportable_observation_is_the_base_plus_itself() {
        assert_eq!(fail_code(100, 0_i32), 100);
        assert_eq!(fail_code(100, 7_i32), 107);
        assert_eq!(fail_code(100, 65_435_i32), u16::MAX);
    }

    #[test]
    fn an_observation_above_the_report_saturates_rather_than_wrapping() {
        // The pre-seam form (`FAIL_BASE + (code as u16)`) aborted in debug and
        // aliased onto another fixture's code in release.
        assert_eq!(fail_code(100, 65_536_i32), u16::MAX);
        assert_eq!(fail_code(100, i32::MAX), u16::MAX);
        assert_eq!(fail_code(65_535, 1_i32), u16::MAX);
        assert_eq!(fail_code(65_000, 1_000_i32), u16::MAX);
        assert_eq!(fail_code(100, u32::MAX), u16::MAX);
    }

    #[test]
    fn a_negative_observation_saturates_rather_than_reinterpreting() {
        // `as u16` would have reported -1 as 65_535 + base, i.e. wrapped.
        assert_eq!(fail_code(100, -1_i32), u16::MAX);
        assert_eq!(fail_code(100, i32::MIN), u16::MAX);
    }

    #[test]
    fn the_composed_code_is_never_the_success_status() {
        // Zero is a *pass* to both finishers, so no composition may reach it.
        assert_eq!(fail_code(0, 0_i32), 1);
        assert_ne!(fail_code(0, 0_u32), 0);
    }

    #[test]
    fn observations_inside_the_band_report_distinct_ordered_codes() {
        // The property the encoding exists for. Staying at or above the base is
        // what keeps a report clear of the small codes a fixture assigns to its
        // own failure points.
        let base = 100_u16;
        assert_eq!(fail_code(base, 0_i32), base);
        let mut previous = base;
        for observed in 1_i32..=64 {
            let code = fail_code(base, observed);
            assert!(
                code > previous,
                "distinct observations report distinct codes"
            );
            assert!(code >= base, "a composed code never drops below its base");
            previous = code;
        }
    }
}
