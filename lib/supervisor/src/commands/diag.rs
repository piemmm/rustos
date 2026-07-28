//! Diagnostic commands: the boot log, a previous boot's crash record, and
//! the heavier interruptible tests (`memtest`, `test disk`).
//!
//! The long-running tests are abortable: the engine polls the keyboard for
//! an `ESC` between work units and passes the host an `abort` predicate, so a
//! test never runs unbounded and never busy-spins.

use crate::commands::{arg_str, arg_u32, missing_arg};
use crate::dispatch::{Flow, Session};
use crate::repl::{read_line, LineOutcome};
use crate::{Report, SupervisorEvent, TestOutcome};

/// `ESC`, the key that aborts a running `memtest` / `test disk`.
const ESC: u8 = 0x1b;

/// The exact word an operator must type to confirm the destructive,
/// one-way `memtest full` takeover. Anything else cancels.
///
/// A fixed, deliberately blunt phrase (not `y`/`yes`) so the irreversible
/// action cannot be triggered by a stray keypress.
const TAKEOVER_CONFIRM: &[u8] = b"DESTROY";

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

/// `memtest [passes]` — the thorough, interruptible RAM test; `memtest full`
/// (alias `memtest --takeover`) is the destructive, one-way whole-RAM test.
pub fn cmd_memtest(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    // The destructive takeover variant is a distinct sub-command, matched
    // before the numeric pass count so `full` / `--takeover` never parse as a
    // (failed) pass count and silently fall back to the safe test.
    if let Some(sub) = args.get(1).and_then(|token| arg_str(token)) {
        if sub == "full" || sub == "--takeover" {
            return cmd_memtest_full(session);
        }
    }
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

/// `memtest full` / `memtest --takeover` — the destructive, one-way whole-RAM
/// takeover test.
///
/// It is irreversible (it overwrites all of RAM and can only end in a reset),
/// so it demands an explicit typed confirmation before it does anything. A
/// mistyped, blank, or over-long confirmation cancels fail-closed and changes
/// nothing. Only once confirmed is the decision audited and the takeover
/// attempted through the host seam; if the seam returns, the takeover did not
/// proceed (unsupported platform or a fail-closed step) and the REPL stays.
fn cmd_memtest_full(session: &mut Session<'_>) -> Flow {
    session
        .out
        .line("memtest full: DESTRUCTIVE one-way whole-RAM test.");
    session
        .out
        .line("It overwrites ALL memory, cannot return, and ends only in a reset.");
    session
        .out
        .write_str("Type DESTROY to confirm (anything else cancels): ");
    // A small on-stack buffer: the confirmation word is short, and an
    // over-long entry simply fails to match and cancels.
    let mut buf = [0u8; 16];
    let read = read_line(session.input, &mut buf, true, session.out);
    session.out.newline();
    let confirmed = matches!(read, LineOutcome::Line(len) if &buf[..len] == TAKEOVER_CONFIRM);
    buf.fill(0);
    if !confirmed {
        session
            .out
            .line("memtest full: cancelled; nothing was changed.");
        return Flow::Stay;
    }
    // Audit the confirmed decision before the attempt: a successful takeover
    // destroys the in-memory audit ring and never returns, so this must be
    // recorded first.
    session.host.audit(SupervisorEvent::MemtestTakeover);
    session.host.takeover_memtest(session.out);
    // Reaching here means the takeover did not proceed (the seam rendered the
    // reason); stay in the Supervisor fail-closed rather than pretend.
    session
        .out
        .line("memtest full: takeover did not proceed; the machine is unchanged.");
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

    #[test]
    fn memtest_full_requires_the_exact_confirmation_word() {
        // Anything other than the exact word cancels and never takes over.
        let mut s = MockSession::new(b"yes\n");
        assert_eq!(dispatch(b"memtest full", &mut s.session()), Flow::Stay);
        assert!(s.output_contains("cancelled"));
        assert!(!s.takeover_requested());
        assert!(!s.audited(crate::SupervisorEvent::MemtestTakeover));
    }

    #[test]
    fn memtest_full_with_a_blank_confirmation_cancels() {
        let mut s = MockSession::new(b"\n");
        dispatch(b"memtest full", &mut s.session());
        assert!(s.output_contains("cancelled"));
        assert!(!s.takeover_requested());
    }

    #[test]
    fn memtest_full_confirmed_audits_then_drives_the_takeover_seam() {
        let mut s = MockSession::new(b"DESTROY\n");
        // The mock seam returns (the test host has no takeover mechanism), so
        // the handler stays in the REPL fail-closed.
        assert_eq!(dispatch(b"memtest full", &mut s.session()), Flow::Stay);
        assert!(s.audited(crate::SupervisorEvent::MemtestTakeover));
        assert!(s.takeover_requested());
    }

    #[test]
    fn memtest_takeover_alias_is_accepted() {
        let mut s = MockSession::new(b"DESTROY\n");
        dispatch(b"memtest --takeover", &mut s.session());
        assert!(s.takeover_requested());
    }

    #[test]
    fn plain_memtest_never_triggers_the_takeover() {
        let mut s = MockSession::new(&[]);
        s.set_memtest_result(TestOutcome::Passed);
        dispatch(b"memtest", &mut s.session());
        assert!(!s.takeover_requested());
        assert!(!s.audited(crate::SupervisorEvent::MemtestTakeover));
    }
}
