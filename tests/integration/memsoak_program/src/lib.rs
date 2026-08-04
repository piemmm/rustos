//! Pure verdict and report logic of the `memsoak` memory-stability fixture
//! (`plans/APPS.md` "Immediate work" I2/I3).
//!
//! The consuming `Run` binary (`src/run.rs`) drives repeated spawn/wait
//! cycles plus the `top`-refresh-shaped work on a live QEMU boot and samples
//! `KernelMemoryStats.free_bytes` through the System Information API before
//! and after. This library owns the parts of that fixture a host test can
//! pin with no kernel: the cycle budgets, the strict baseline/final verdict,
//! and the exact report lines the consuming vertical's serial script keys
//! on. Keeping them here means the program, the vertical's script marker,
//! and the unit tests all read one definition and cannot drift.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;

/// The fixture's command word — the `AppInfo.toml` bundle name, the word
/// the consuming vertical's script types at the shell, and the `comm` the
/// kernel attests on the fixture's audited syscalls (the spawn path names a
/// bundle process by its command stem). One definition, pinned against the
/// manifest source by a host test, so the program, the manifest, and the
/// vertical's PASS keying cannot drift.
pub const COMMAND: &str = "memsoak";

/// Store path of the child the fixture spawns and reaps each cycle: the
/// `true` command app, which exits `0` immediately, so every cycle executes
/// the full spawn → run → exit → reap → teardown path and nothing else.
pub const CHILD_PATH: &[u8] = b"/System/Commands/true.app/Run";

/// Cycles driven before the baseline sample, so every once-per-boot cost is
/// paid off the measured window: the command store's per-bundle
/// verification cache admits `true.app`, sysinfod's heap reaches the query
/// working set, and the kernel's per-first-use paths (wait bookkeeping, IPC
/// endpoint state) settle. A leak would survive any warmup; only one-time
/// growth is excluded.
pub const WARMUP_CYCLES: u32 = 4;

/// Cycles driven between the baseline and final samples. Sized so a
/// per-cycle leak of even one page moves `free_bytes` well past any
/// sampling coincidence, while the whole soak stays a small fraction of the
/// vertical's QEMU budget.
pub const MEASURED_CYCLES: u32 = 32;

/// Nanoseconds of the per-cycle timed `stream_read` bound — the same shape
/// as `top -d0`'s refresh park (a bound that elapses with no input). Kept
/// short so the soak's wall-clock cost is dominated by real work, not
/// sleeping.
pub const CYCLE_PARK_NANOS: u64 = 1_000_000;

/// Leading marker of the success report line. The consuming vertical's
/// serial script waits for this exact prefix before typing the shell `exit`
/// that completes the PASS chain, so it lives here beside the render that
/// emits it.
pub const PASS_MARKER: &str = "MEMSOAK PASS";

/// Leading marker of the failure report line, distinct from
/// [`PASS_MARKER`] so a transcript is unambiguous at a glance.
pub const FAIL_MARKER: &str = "MEMSOAK FAIL";

/// The soak's outcome: the strict comparison of the final free-memory
/// sample against the baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Every byte the measured cycles consumed was returned: the final
    /// sample equals the baseline exactly.
    Stable,
    /// The samples differ — retained memory (final below baseline) or an
    /// accounting anomaly (final above baseline). Both are defects: a
    /// steady-state cycle must return the allocator to exactly where it
    /// started, so any drift fails the soak.
    Drifted,
}

/// Judge the soak: the final sample must equal the baseline byte for byte.
///
/// Strict equality is deliberate. The measured window starts only after the
/// warmup cycles have paid every once-per-boot cost, and each cycle ends
/// with the child fully reaped and its whole footprint reclaimed (the
/// teardown the I2 host tests pin as exact), so a steady state is the
/// specified behaviour — tolerating "small" drift would let a slow leak
/// pass N cycles and fail N+M.
#[must_use]
pub fn verdict(baseline_free_bytes: u64, final_free_bytes: u64) -> Verdict {
    if baseline_free_bytes == final_free_bytes {
        Verdict::Stable
    } else {
        Verdict::Drifted
    }
}

/// Render the one report line the fixture writes: the verdict marker, both
/// samples, and the measured cycle count, terminated by a newline. The
/// numbers make a failing transcript diagnosable (how much drifted, in
/// which direction) without re-running.
#[must_use]
pub fn report_line(verdict: Verdict, baseline_free_bytes: u64, final_free_bytes: u64) -> String {
    let marker = match verdict {
        Verdict::Stable => PASS_MARKER,
        Verdict::Drifted => FAIL_MARKER,
    };
    format!(
        "{marker} baseline={baseline_free_bytes} final={final_free_bytes} cycles={MEASURED_CYCLES}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_samples_are_stable() {
        assert_eq!(verdict(4096, 4096), Verdict::Stable);
        assert_eq!(verdict(0, 0), Verdict::Stable);
        assert_eq!(verdict(u64::MAX, u64::MAX), Verdict::Stable);
    }

    #[test]
    fn any_drift_fails_in_either_direction() {
        // Retained memory: the final sample fell below the baseline.
        assert_eq!(verdict(4096, 0), Verdict::Drifted);
        // Accounting anomaly: the final sample rose above the baseline.
        assert_eq!(verdict(0, 4096), Verdict::Drifted);
        // One byte of drift is already a defect — no tolerance band.
        assert_eq!(verdict(1000, 999), Verdict::Drifted);
        assert_eq!(verdict(999, 1000), Verdict::Drifted);
    }

    #[test]
    fn report_lines_carry_the_marker_the_vertical_scripts_on() {
        let pass = report_line(Verdict::Stable, 7, 7);
        assert!(pass.starts_with(PASS_MARKER));
        assert_eq!(pass, "MEMSOAK PASS baseline=7 final=7 cycles=32\n");

        let fail = report_line(Verdict::Drifted, 8, 7);
        assert!(fail.starts_with(FAIL_MARKER));
        assert_eq!(fail, "MEMSOAK FAIL baseline=8 final=7 cycles=32\n");
    }

    /// The two markers must stay distinct prefixes: a transcript matcher
    /// waiting for the PASS marker must never fire on a FAIL line.
    #[test]
    fn fail_line_never_matches_the_pass_marker() {
        let fail = report_line(Verdict::Drifted, 1, 2);
        assert!(!fail.contains(PASS_MARKER));
    }

    /// [`COMMAND`] is the manifest source's `name`: the vertical's script
    /// and PASS keying read the constant, the composer reads the manifest,
    /// and this pin keeps the two spellings one.
    #[test]
    fn command_matches_the_manifest_source_name() {
        let manifest = include_str!("../AppInfo.toml");
        let named = manifest.lines().any(|line| {
            line.split_once('=').is_some_and(|(key, value)| {
                key.trim() == "name" && value.trim() == alloc::format!("{COMMAND:?}")
            })
        });
        assert!(named, "AppInfo.toml `name` must be {COMMAND:?}");
    }
}
