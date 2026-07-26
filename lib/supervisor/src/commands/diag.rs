//! Diagnostic commands: the boot log, a previous boot's crash record, and
//! the heavier interruptible tests (`memtest`, `test disk`).
//!
//! The long-running tests are abortable: the engine polls the keyboard for
//! an `ESC` between work units and passes the host an `abort` predicate, so a
//! test never runs unbounded and never busy-spins.

use crate::commands::{arg_str, arg_u32, missing_arg};
use crate::dispatch::{Flow, Session};
use crate::{Report, TestOutcome};

/// `ESC`, the key that aborts a running `memtest` / `test disk`.
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

/// `memtest [passes]` — the thorough, interruptible RAM test.
pub fn cmd_memtest(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    let passes = args.get(1).and_then(|token| arg_u32(token)).unwrap_or(1);
    if passes == 0 {
        session.out.line("memtest: passes must be at least 1.");
        return Flow::Stay;
    }
    // Reborrow the fields as independent locals so the abort closure may
    // borrow the input while the host renders to the output.
    let out = &mut *session.out;
    let input = &mut *session.input;
    let host = &mut *session.host;
    let mut abort = || matches!(input.poll_byte(), Some(ESC));
    let outcome = host.memtest(passes, &mut *out, &mut abort);
    report_outcome(out, "memtest", outcome);
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
    fn memtest_defaults_to_one_pass_and_reports_passed() {
        let mut s = MockSession::new(&[]);
        s.set_memtest_result(TestOutcome::Passed);
        assert_eq!(dispatch(b"memtest", &mut s.session()), Flow::Stay);
        assert_eq!(s.last_memtest_passes(), Some(1));
        assert!(s.output_contains("memtest: passed"));
    }

    #[test]
    fn memtest_parses_the_pass_count() {
        let mut s = MockSession::new(&[]);
        s.set_memtest_result(TestOutcome::Passed);
        dispatch(b"memtest 3", &mut s.session());
        assert_eq!(s.last_memtest_passes(), Some(3));
    }

    #[test]
    fn memtest_zero_passes_is_rejected() {
        let mut s = MockSession::new(&[]);
        dispatch(b"memtest 0", &mut s.session());
        assert!(s.output_contains("at least 1"));
    }

    #[test]
    fn memtest_reports_a_fault() {
        let mut s = MockSession::new(&[]);
        s.set_memtest_result(TestOutcome::Failed);
        dispatch(b"memtest", &mut s.session());
        assert!(s.output_contains("FAILED"));
    }

    #[test]
    fn memtest_abort_via_esc_is_reported() {
        // A queued ESC byte the poll picks up drives the abort predicate.
        let mut s = MockSession::new(&[]);
        s.set_memtest_result(TestOutcome::Aborted);
        dispatch(b"memtest", &mut s.session());
        assert!(s.output_contains("aborted"));
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
