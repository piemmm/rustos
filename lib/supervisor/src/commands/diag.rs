//! Diagnostic commands: the boot log, a previous boot's crash record, the
//! one-way whole-RAM `memtest`, and the read-only `test disk` scan.
//!
//! `test disk` is abortable: the engine polls the keyboard for an `ESC`
//! between work units and passes the host an `abort` predicate, so the scan
//! never runs unbounded and never busy-spins. `memtest` is the one-way
//! whole-RAM takeover test — once a supporting platform accepts it there is
//! nothing to abort, so it takes no abort predicate.

use crate::commands::{arg_str, arg_u32, missing_arg};
use crate::dispatch::{Flow, Session};
use crate::{Report, SupervisorEvent, TestOutcome};

/// `ESC`, the key that aborts a running `test disk`.
const ESC: u8 = 0x1b;

/// `log [n]` — show the boot audit-log entries (the last `n` when given).
pub fn cmd_log(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    let count = args
        .get(1)
        .and_then(|token| arg_u32(token).map(|n| n as usize));
    session.host.log_tail(count, session.out);
    Flow::Stay
}

/// `panic-log` / `last` — show a previous boot's panic / lockup record.
pub fn cmd_panic_log(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.panic_log(session.out);
    Flow::Stay
}

/// `memtest` — the one-way whole-RAM takeover test.
///
/// There is only one memory test and it is the full one: invoking `memtest`
/// stops every other CPU, overwrites **all** of RAM (including the running
/// kernel), and can only end in a reset. It takes no arguments and asks for no
/// confirmation — running it *is* the decision to tear the machine down.
///
/// The decision is audited **before** the attempt, because a successful
/// takeover destroys the in-memory audit ring and never returns. The takeover
/// is then driven through the host seam; if that seam returns, the takeover
/// did not proceed (an unsupported platform, or a step that failed closed) and
/// the REPL stays, the reason already rendered by the seam.
pub fn cmd_memtest(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.audit(SupervisorEvent::MemtestTakeover);
    session.host.takeover_memtest(session.out);
    // Reaching here means the takeover did not proceed (the seam rendered the
    // reason); stay in the Supervisor fail-closed rather than pretend.
    session
        .out
        .line("memtest: takeover did not proceed; the machine is unchanged.");
    Flow::Stay
}

/// `test disk <device>` — a bounded, read-only, interruptible surface scan.
pub fn cmd_test(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    match args.get(1).and_then(|token| arg_str(token)) {
        Some("disk") => {}
        Some(other) => {
            session.out.write_str("test: unknown subject: ");
            session.out.write_str(other);
            session.out.newline();
            return Flow::Stay;
        }
        None => {
            missing_arg(session.out, "test disk <device>");
            return Flow::Stay;
        }
    }
    let Some(device) = args.get(2).and_then(|token| arg_str(token)) else {
        missing_arg(session.out, "test disk <device>");
        return Flow::Stay;
    };
    let out = &mut *session.out;
    let input = &mut *session.input;
    let host = &mut *session.host;
    let mut abort = || matches!(input.poll_byte(), Some(ESC));
    let outcome = host.scan_disk(device, &mut *out, &mut abort);
    report_outcome(out, "test disk", outcome);
    Flow::Stay
}

/// Render the terminal outcome of a long-running test in a uniform way.
fn report_outcome(out: &mut dyn Report, what: &str, outcome: TestOutcome) {
    out.write_str(what);
    match outcome {
        TestOutcome::Passed => out.line(": passed."),
        TestOutcome::Failed => out.line(": FAILED (see the report above)."),
        TestOutcome::Aborted => out.line(": aborted."),
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::test_support::MockSession;
    use crate::dispatch::{dispatch, Flow};
    use crate::TestOutcome;

    #[test]
    fn log_without_a_count_shows_the_whole_log() {
        let mut s = MockSession::new(&[]);
        dispatch(b"log", &mut s.session());
        assert!(s.output_contains("boot-log"));
    }

    #[test]
    fn panic_log_is_rendered() {
        let mut s = MockSession::new(&[]);
        dispatch(b"panic-log", &mut s.session());
        assert!(s.output_contains("panic"));
    }

    #[test]
    fn memtest_audits_then_drives_the_takeover_seam_with_no_confirmation() {
        // There is only the one whole-RAM test: a bare `memtest` audits
        // the decision and drives the takeover seam immediately, with no
        // prompt. The mock seam returns (the test host has no takeover
        // mechanism), so the handler stays in the REPL fail-closed.
        let mut s = MockSession::new(&[]);
        assert_eq!(dispatch(b"memtest", &mut s.session()), Flow::Stay);
        assert!(s.audited(crate::SupervisorEvent::MemtestTakeover));
        assert!(s.takeover_requested());
        assert!(s.output_contains("did not proceed"));
    }

    #[test]
    fn memtest_ignores_trailing_arguments() {
        // No `full`/`--takeover` subcommand and no pass count survive: any
        // trailing words are ignored and the single whole-RAM test runs.
        let mut s = MockSession::new(&[]);
        dispatch(b"memtest full please", &mut s.session());
        assert!(s.takeover_requested());
        assert!(s.audited(crate::SupervisorEvent::MemtestTakeover));
    }

    #[test]
    fn test_disk_requires_the_disk_subject_and_a_device() {
        let mut s = MockSession::new(&[]);
        dispatch(b"test", &mut s.session());
        assert!(s.output_contains("usage"));
        let mut s2 = MockSession::new(&[]);
        dispatch(b"test disk", &mut s2.session());
        assert!(s2.output_contains("usage"));
    }

    #[test]
    fn test_disk_scans_the_named_device() {
        let mut s = MockSession::new(&[]);
        s.set_scan_result(TestOutcome::Passed);
        dispatch(b"test disk disk0", &mut s.session());
        assert_eq!(s.last_scan_device().as_deref(), Some("disk0"));
        assert!(s.output_contains("test disk: passed"));
    }
}
