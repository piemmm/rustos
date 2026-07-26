//! Control commands: the ones that change boot state.
//!
//! Each audits its intent through [`SupervisorHost::audit`] before acting and
//! fails closed. `mount` performs the *real* passphrase unlock — there is no
//! command that bypasses the passphrase or reveals key material.

use crate::dispatch::{Flow, Session};
use crate::repl::{read_line, LineOutcome};
use crate::{MountOutcome, SupervisorEvent, SupervisorExit};

/// The longest passphrase the Supervisor `mount` prompt accepts, in bytes.
///
/// The same generous fixed operator-input bound the normal unlock prompt
/// uses; the read buffer is a fixed on-stack array of this size that is
/// zeroed the instant the attempt resolves, so the secret never lingers.
const MOUNT_PASSPHRASE_MAX: usize = 256;

/// `continue` / `boot` — leave the Supervisor and resume the normal boot.
pub fn cmd_continue(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.audit(SupervisorEvent::ContinueBoot);
    Flow::Exit(SupervisorExit::ContinueBoot)
}

/// `mount` — prompt for the ARXFS passphrase and perform the real root
/// unlock now. On success the boot continues without a second prompt.
pub fn cmd_mount(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.audit(SupervisorEvent::MountAttempt);
    session.out.write_str("ARXFS passphrase: ");

    // A fixed, on-stack buffer wiped the instant the attempt resolves: the
    // typed secret never reaches a heap and never lingers. The read is
    // un-echoed so the passphrase does not render.
    let mut passphrase = [0u8; MOUNT_PASSPHRASE_MAX];
    let read = read_line(session.input, &mut passphrase, false, session.out);
    session.out.newline();
    let outcome = match read {
        LineOutcome::Line(len) => session.host.mount(&passphrase[..len], session.out),
        LineOutcome::TooLong => {
            passphrase.fill(0);
            session.out.line("Passphrase too long; not attempted.");
            session.host.audit(SupervisorEvent::MountFailed);
            return Flow::Stay;
        }
        LineOutcome::Eof => {
            passphrase.fill(0);
            session.out.line("No passphrase entered; mount cancelled.");
            session.host.audit(SupervisorEvent::MountFailed);
            return Flow::Stay;
        }
    };
    passphrase.fill(0);

    match outcome {
        MountOutcome::Mounted => {
            session.out.line("Root mounted; continuing boot.");
            session.host.audit(SupervisorEvent::MountOk);
            Flow::Exit(SupervisorExit::Mounted)
        }
        MountOutcome::WrongPassphrase => {
            session.out.line("Incorrect passphrase.");
            session.host.audit(SupervisorEvent::MountFailed);
            Flow::Stay
        }
        MountOutcome::Failed => {
            session
                .out
                .line("Mount failed (the root volume could not be read).");
            session.host.audit(SupervisorEvent::MountFailed);
            Flow::Stay
        }
    }
}

/// `reboot` — restart the machine. If the platform cannot reboot the call
/// returns and the failure is reported; otherwise it never returns.
pub fn cmd_reboot(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.audit(SupervisorEvent::Reboot);
    session.out.line("Rebooting...");
    session.host.reboot();
    // Reaching here means the platform reset primitive returned: report it
    // fail-safe and stay in the Supervisor rather than pretend it worked.
    session.out.line("reboot: the platform did not restart.");
    Flow::Stay
}

/// `poweroff` / `halt` — power the machine off. Reports and stays if the
/// platform has no power-off; otherwise never returns.
pub fn cmd_poweroff(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.audit(SupervisorEvent::Poweroff);
    session.out.line("Powering off...");
    session.host.poweroff();
    session
        .out
        .line("poweroff: not supported on this platform.");
    Flow::Stay
}

#[cfg(test)]
mod tests {
    use crate::commands::test_support::MockSession;
    use crate::dispatch::{dispatch, Flow};
    use crate::{MountOutcome, SupervisorEvent, SupervisorExit};

    #[test]
    fn continue_exits_the_repl_and_is_audited() {
        let mut s = MockSession::new(&[]);
        let flow = dispatch(b"continue", &mut s.session());
        assert_eq!(flow, Flow::Exit(SupervisorExit::ContinueBoot));
        assert!(s.audited(SupervisorEvent::ContinueBoot));
    }

    #[test]
    fn boot_is_an_alias_for_continue() {
        let mut s = MockSession::new(&[]);
        assert_eq!(
            dispatch(b"boot", &mut s.session()),
            Flow::Exit(SupervisorExit::ContinueBoot)
        );
    }

    #[test]
    fn reboot_calls_the_host_and_is_audited() {
        let mut s = MockSession::new(&[]);
        // The mock reboot returns (does not diverge), so the handler stays.
        assert_eq!(dispatch(b"reboot", &mut s.session()), Flow::Stay);
        assert!(s.rebooted());
        assert!(s.audited(SupervisorEvent::Reboot));
    }

    #[test]
    fn poweroff_and_halt_call_the_host() {
        let mut s = MockSession::new(&[]);
        dispatch(b"halt", &mut s.session());
        assert!(s.powered_off());
        assert!(s.audited(SupervisorEvent::Poweroff));
    }

    #[test]
    fn mount_with_the_correct_passphrase_mounts_and_exits() {
        // Scripted keyboard bytes: the passphrase then Enter.
        let mut s = MockSession::new(b"opensesame\n");
        s.set_mount_result(MountOutcome::Mounted);
        let flow = dispatch(b"mount", &mut s.session());
        assert_eq!(flow, Flow::Exit(SupervisorExit::Mounted));
        assert!(s.audited(SupervisorEvent::MountAttempt));
        assert!(s.audited(SupervisorEvent::MountOk));
        // The passphrase must never appear in the rendered output.
        assert!(!s.output_contains("opensesame"));
    }

    #[test]
    fn mount_with_a_wrong_passphrase_stays_and_reports() {
        let mut s = MockSession::new(b"wrong\n");
        s.set_mount_result(MountOutcome::WrongPassphrase);
        let flow = dispatch(b"mount", &mut s.session());
        assert_eq!(flow, Flow::Stay);
        assert!(s.output_contains("Incorrect passphrase"));
        assert!(s.audited(SupervisorEvent::MountFailed));
    }

    #[test]
    fn mount_never_reveals_the_typed_passphrase() {
        let mut s = MockSession::new(b"topsecret\n");
        s.set_mount_result(MountOutcome::Failed);
        dispatch(b"mount", &mut s.session());
        assert!(!s.output_contains("topsecret"));
    }
}
