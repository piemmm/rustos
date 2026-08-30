//! Shared clock-chip fixture for the QEMU real-time-clock verticals
//! (`plans/TIMESYNC.md` TS-3).
//!
//! QEMU starts every emulated RTC — the aarch64 `virt` board's PL031, the
//! riscv64 `virt` board's Goldfish RTC, and the x86_64 CMOS chip — from the
//! instant `-rtc base=` names, then advances it with real time. Pinning that
//! instant is what turns "a clock was set" into a claim worth asserting: a
//! reading that lands in this window came from the chip's registers decoded
//! correctly, and a byte-swap, a wrong register, a fabricated build date, or
//! a millisecond/second confusion all land outside it while still being
//! *plausible* in the wall clock's sense.
//!
//! The harness passes [`RTC_BASE_SECS`] on the QEMU command line and each
//! freestanding guest checks the applied reading with
//! [`reading_is_from_fixture`], so the two ends share one definition.

#![no_std]
#![deny(missing_docs)]

/// The instant the emulated clock chip starts at, in Unix seconds
/// (2027-03-05T12:00:00Z).
///
/// A round instant well inside the wall clock's plausibility window and well
/// clear of every mis-decode [`reading_is_from_fixture`] must reject.
pub const RTC_BASE_SECS: i64 = 1_804_248_000;

/// How far above [`RTC_BASE_SECS`] a reading may land and still be this
/// fixture's clock, in seconds.
///
/// The chip advances with real time from the moment QEMU starts, so a
/// reading is the base plus however long the guest took to reach its RTC
/// read. An hour is more than an order of magnitude above the longest
/// enrolled budget and still narrow enough to reject every mis-decode. A
/// fixed validation bound, never widened to make a slow run pass.
pub const RTC_BOOT_WINDOW_SECS: i64 = 3_600;

/// Whether `secs` is a reading of the fixture's clock chip rather than a
/// mis-decoded or fabricated instant.
#[must_use]
pub const fn reading_is_from_fixture(secs: i64) -> bool {
    secs >= RTC_BASE_SECS && secs <= RTC_BASE_SECS + RTC_BOOT_WINDOW_SECS
}

#[cfg(test)]
mod tests {
    use super::{reading_is_from_fixture, RTC_BASE_SECS, RTC_BOOT_WINDOW_SECS};
    use tairix_abi::time::{is_plausible_wall_time, Time64};

    #[test]
    fn the_fixture_instant_and_its_whole_window_are_plausible_wall_times() {
        assert!(is_plausible_wall_time(Time64::from_secs(RTC_BASE_SECS)));
        assert!(is_plausible_wall_time(Time64::from_secs(
            RTC_BASE_SECS + RTC_BOOT_WINDOW_SECS
        )));
    }

    #[test]
    fn the_window_admits_the_base_and_a_whole_run_of_boot() {
        assert!(reading_is_from_fixture(RTC_BASE_SECS));
        assert!(reading_is_from_fixture(RTC_BASE_SECS + 300));
        assert!(reading_is_from_fixture(
            RTC_BASE_SECS + RTC_BOOT_WINDOW_SECS
        ));
        assert!(!reading_is_from_fixture(
            RTC_BASE_SECS + RTC_BOOT_WINDOW_SECS + 1
        ));
        assert!(!reading_is_from_fixture(RTC_BASE_SECS - 1));
    }

    // The window earns its keep only if it rejects the mis-decodes a bare
    // plausibility check would wave through, so each is asserted to be
    // plausible *and* refused.
    #[test]
    fn the_window_rejects_mis_decodes_a_plausibility_check_would_admit() {
        let byte_swapped = i64::from(
            u32::try_from(RTC_BASE_SECS)
                .expect("the fixture instant fits the chips' 32-bit counters")
                .swap_bytes(),
        );
        assert!(is_plausible_wall_time(Time64::from_secs(byte_swapped)));
        assert!(!reading_is_from_fixture(byte_swapped));

        // A chip read as BCD that is really binary, or the reverse: the same
        // registers reinterpreted land years away.
        let a_year_late = RTC_BASE_SECS + 365 * 86_400;
        assert!(is_plausible_wall_time(Time64::from_secs(a_year_late)));
        assert!(!reading_is_from_fixture(a_year_late));
    }

    #[test]
    fn the_unix_epoch_and_a_stopped_counter_are_refused() {
        assert!(!reading_is_from_fixture(0));
        assert!(!reading_is_from_fixture(i64::from(u32::MAX)));
    }
}
