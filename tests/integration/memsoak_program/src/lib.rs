//! Pure verdict and report logic of the `memsoak` memory-stability fixture
//! (`plans/APPS.md` "Immediate work" I2/I3).
//!
//! The consuming `Run` binary (`src/run.rs`) drives repeated spawn/wait
//! cycles plus the `top`-refresh-shaped work on a live QEMU boot and samples
//! [`KernelMemoryStats`] through the System Information API before and
//! after. This library owns the parts of that fixture a host test can pin
//! with no kernel: the cycle budgets, the sampled quantity, the strict
//! baseline/final verdict, and the exact report lines the consuming
//! vertical's serial script keys on. Keeping them here means the program,
//! the vertical's script marker, and the unit tests all read one definition
//! and cannot drift.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use tairix_abi::sysinfo::KernelMemoryStats;

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

/// The soak's sample: free physical memory **plus** every byte resident in a
/// user address space.
///
/// `free_bytes` alone is not a quantity a byte-exact verdict can judge. It
/// falls whenever *any* process on the machine allocates, so an unrelated
/// service waking inside the measured window fails a soak the cycle under
/// test passed — `timed`'s NTP retry, which permanently gains its address
/// space two pages, is the observed case, and it is driven by wall time, so
/// no warmup length excludes it. A page moving between the free pool and a
/// user address space leaves this sum unchanged, while kernel memory the
/// cycle failed to return still lowers it. The verdict therefore measures
/// kernel-side retention and nothing else.
///
/// Saturating, so a malformed reply can only under-report rather than wrap
/// into a figure that would read as a stable soak.
#[must_use]
pub fn sample_bytes(stats: &KernelMemoryStats) -> u64 {
    stats.free_bytes.saturating_add(stats.user_resident_bytes)
}

/// The soak's outcome: the strict comparison of the final sample against the
/// baseline.
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

/// Judge the soak: the final [`sample_bytes`] must equal the baseline byte
/// for byte.
///
/// Strict equality is deliberate. The measured window starts only after the
/// warmup cycles have paid every once-per-boot cost, and each cycle ends
/// with the child fully reaped and its whole footprint reclaimed (the
/// teardown the I2 host tests pin as exact), so a steady state is the
/// specified behaviour — tolerating "small" drift would let a slow leak
/// pass N cycles and fail N+M. It is judgeable at all because the sample is
/// immune to what other processes allocate meanwhile ([`sample_bytes`]).
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

    fn stats(free_bytes: u64, user_resident_bytes: u64) -> KernelMemoryStats {
        KernelMemoryStats {
            total_bytes: 1 << 30,
            free_bytes,
            kernel_heap_bytes: 0,
            user_resident_bytes,
            page_size: 4096,
            reserved: 0,
        }
    }

    /// The property the sample exists for: a page leaving the free pool for
    /// *some* user address space is not retention, and must not move the
    /// sample. Without it an unrelated service allocating inside the measured
    /// window fails the soak.
    #[test]
    fn a_page_moving_into_a_user_address_space_leaves_the_sample_unchanged() {
        let before = sample_bytes(&stats(8192, 0));
        let after = sample_bytes(&stats(4096, 4096));
        assert_eq!(before, after);
        assert_eq!(verdict(before, after), Verdict::Stable);
    }

    /// A page that leaves the free pool without becoming user-resident is
    /// exactly what the soak hunts, and still lowers the sample.
    #[test]
    fn a_page_retained_kernel_side_lowers_the_sample() {
        let before = sample_bytes(&stats(8192, 0));
        let after = sample_bytes(&stats(4096, 0));
        assert_eq!(before - after, 4096);
        assert_eq!(verdict(before, after), Verdict::Drifted);
    }

    /// A malformed reply can only under-report; it never wraps into a figure
    /// that would read as a stable soak.
    #[test]
    fn the_sample_saturates_rather_than_wrapping() {
        assert_eq!(sample_bytes(&stats(u64::MAX, 4096)), u64::MAX);
    }

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
