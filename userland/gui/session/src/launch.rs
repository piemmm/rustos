//! Operator-facing diagnosis for a launched child that failed to load.
//!
//! Asynchronous process launch (`plans/FIX-DESKTOP.md`) admits a child and
//! returns its PID to the desktop immediately; the load — the VFS read of the
//! bundle, the signature/hash verification, and the address-space build —
//! then runs on the *child's own* task, so the compositor loop never freezes
//! behind it. A refusal therefore no longer comes back as the `spawn` return
//! value; it surfaces as the child's **exit status**, a reserved `LOAD_*`
//! code that `lib/abi` places in a high, loader-only band a normal program's
//! own `exit(code)` never lands in.
//!
//! The desktop reaps every exited child on its `CHILD_TOKEN` wait-set member.
//! This module turns a reaped status into the terse line the session prints
//! on `stderr`, so a refused launch is a loud, non-fatal diagnosis (the
//! charter's fail-loud rule) instead of a window that silently never appears.
//! The reason wording is `lib/abi`'s shared mapping, so the desktop and any
//! other launcher describe the same cause identically.

use alloc::collections::BTreeMap;
use alloc::string::String;

use tairix_abi::{load_failure_reason, WaitStatus};

/// The label used for a reaped child the launcher did not record (it should
/// not happen — every desktop child is recorded at launch — but a diagnosis
/// must never be dropped for want of a name).
const UNKNOWN_LABEL: &str = "an application";

/// The `stderr` line for a reaped child `label` that exited with a reserved
/// loader-failure status, or `None` when the exit is not a load refusal.
///
/// A clean exit (`Exited(0)`), any ordinary non-zero exit outside the
/// reserved `LOAD_*` band, and a stop all return `None` — they are the
/// launched app's own business, not a launch failure the desktop reports.
/// The returned line is newline-terminated and ready to hand to `stderr`.
#[must_use]
pub fn launch_failure_report(label: &str, status: WaitStatus) -> Option<String> {
    let WaitStatus::Exited(code) = status else {
        return None;
    };
    let reason = load_failure_reason(code)?;
    Some(alloc::format!(
        "desktop: {label} failed to launch: {reason}\n"
    ))
}

/// Drain every currently-exited child, reporting each load refusal and
/// handing every reaped PID back to the caller for its own teardown.
///
/// This is the whole of the desktop's `CHILD_TOKEN` handling, factored out of
/// the freestanding `Run` loop so it is exercised on the host by real tests
/// rather than only in a booted image. The three seams are injected so the
/// pure control flow — drain until empty, drop the in-flight entry, report a
/// reserved-status refusal, forward the PID — is identical in production and
/// under test:
///
/// * `reap` performs one non-blocking reap, returning `Some((pid, status))`
///   for a reaped zombie or `None` when none remains. In production it wraps
///   the kernel `wait`; in tests it replays a scripted sequence.
/// * `report` receives each `stderr` diagnosis line (already newline-
///   terminated). In production it writes `stderr`; in tests it records the
///   lines.
/// * `teardown` receives every reaped PID so the caller can tear that child's
///   windows down. It runs for *every* reaped child, load-failure or not — a
///   child that launched successfully and later exited is torn down here too.
///
/// Reaping drains fully: a non-blocking reap that returns `None` ends the
/// drain, so a burst of exits is handled in one wake and nothing is left for
/// a later poll (`AGENTS.md` §2.23 — no busy-wait, one drain per wake).
pub fn reap_launched<R, P, T>(
    in_flight: &mut BTreeMap<u64, &'static str>,
    mut reap: R,
    mut report: P,
    mut teardown: T,
) where
    R: FnMut() -> Option<(u64, WaitStatus)>,
    P: FnMut(&str),
    T: FnMut(u64),
{
    while let Some((pid, status)) = reap() {
        let label = in_flight.remove(&pid).unwrap_or(UNKNOWN_LABEL);
        if let Some(line) = launch_failure_report(label, status) {
            report(&line);
        }
        teardown(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::{launch_failure_report, reap_launched, UNKNOWN_LABEL};
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::{
        Signal, WaitStatus, LOAD_MALFORMED, LOAD_NOT_FOUND, LOAD_OOM, LOAD_UNVERIFIED,
    };

    #[test]
    fn every_reserved_status_reports_its_reason_named_by_label() {
        for code in [LOAD_NOT_FOUND, LOAD_UNVERIFIED, LOAD_MALFORMED, LOAD_OOM] {
            let line = launch_failure_report("Files", WaitStatus::Exited(code))
                .expect("a reserved load status must be reported");
            assert!(line.starts_with("desktop: Files failed to launch: "));
            assert!(line.ends_with('\n'));
            // The reason is the shared `lib/abi` wording, never fabricated.
            let reason = tairix_abi::load_failure_reason(code).unwrap();
            assert!(line.contains(reason));
        }
    }

    #[test]
    fn a_clean_exit_is_not_a_launch_failure() {
        assert_eq!(launch_failure_report("Files", WaitStatus::Exited(0)), None);
    }

    #[test]
    fn an_ordinary_nonzero_exit_is_not_a_launch_failure() {
        // A program that ran and chose its own non-zero code is not a load
        // refusal; the desktop must not misreport it as one.
        assert_eq!(
            launch_failure_report("Terminal", WaitStatus::Exited(1)),
            None
        );
        assert_eq!(
            launch_failure_report("Terminal", WaitStatus::Exited(90)),
            None
        );
    }

    #[test]
    fn a_stop_is_not_a_launch_failure() {
        assert_eq!(
            launch_failure_report("Viewer", WaitStatus::Stopped(Signal::Stop)),
            None
        );
    }

    /// A scripted drain: two children exit in one wake — a `Files` launch
    /// that was refused (reserved `LOAD_UNVERIFIED`) and a `Terminal` that
    /// ran and exited cleanly. The refusal is reported (named), the clean
    /// exit is not, both in-flight entries are dropped, and *both* PIDs are
    /// handed to teardown. The drain stops when the reap yields `None`.
    #[test]
    fn reap_drains_and_reports_only_the_refused_launch() {
        let mut in_flight: BTreeMap<u64, &'static str> = BTreeMap::new();
        in_flight.insert(10, "Files");
        in_flight.insert(11, "Terminal");

        let mut script = alloc::vec![
            (10u64, WaitStatus::Exited(LOAD_UNVERIFIED)),
            (11u64, WaitStatus::Exited(0)),
        ]
        .into_iter();
        let mut reported: Vec<String> = Vec::new();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut in_flight,
            || script.next(),
            |line| reported.push(String::from(line)),
            |pid| torn_down.push(pid),
        );

        assert!(in_flight.is_empty(), "every reaped child is dropped");
        assert_eq!(
            torn_down,
            alloc::vec![10, 11],
            "every reaped pid is torn down"
        );
        assert_eq!(reported.len(), 1, "only the refused launch is reported");
        assert!(reported[0].starts_with("desktop: Files failed to launch: "));
        assert!(reported[0].contains(tairix_abi::load_failure_reason(LOAD_UNVERIFIED).unwrap()));
    }

    /// A refused child the launcher never recorded (an impossible-in-practice
    /// case the desktop still must not drop silently) is reported under the
    /// fallback label rather than not at all.
    #[test]
    fn an_unrecorded_refused_child_is_still_reported() {
        let mut in_flight: BTreeMap<u64, &'static str> = BTreeMap::new();
        let mut script = alloc::vec![(7u64, WaitStatus::Exited(LOAD_OOM))].into_iter();
        let mut reported: Vec<String> = Vec::new();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut in_flight,
            || script.next(),
            |line| reported.push(String::from(line)),
            |pid| torn_down.push(pid),
        );

        assert_eq!(torn_down, alloc::vec![7]);
        assert_eq!(reported.len(), 1);
        assert!(reported[0].contains(UNKNOWN_LABEL));
    }

    /// No exited child: nothing is reaped, reported, or torn down.
    #[test]
    fn reap_with_no_zombies_does_nothing() {
        let mut in_flight: BTreeMap<u64, &'static str> = BTreeMap::new();
        in_flight.insert(3, "Files");
        let mut reported: Vec<String> = Vec::new();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut in_flight,
            || None,
            |line| reported.push(String::from(line)),
            |pid| torn_down.push(pid),
        );

        assert_eq!(in_flight.len(), 1, "an unexited child stays in flight");
        assert!(reported.is_empty());
        assert!(torn_down.is_empty());
    }
}
