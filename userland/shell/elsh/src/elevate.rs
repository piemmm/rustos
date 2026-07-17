//! The `elevate` builtin: run one program as another account
//! (`plans/CAPABILITY_USE.md` CU5).
//!
//! `elevate <user> <program>` prompts for `<user>`'s password (terminal echo
//! off), posts the request to this console's login supervisor through the
//! injected [`Elevator`](crate::host::Elevator) seam, and blocks until the
//! re-authenticated command has run to completion as that account; its exit
//! code becomes the builtin's status (`$?`). The shell holds **no**
//! elevation authority of its own: the supervisor re-authenticates the
//! offered credentials and the kernel derives the elevated child's
//! capability set exactly as at login, so the builtin only asks — it can
//! never grant. A refusal is indistinguishable between a wrong password, an
//! unknown account, and a locked account (the supervisor audits the cause,
//! never discloses it).
//!
//! It is a builtin, not an external program, because the password prompt
//! must own the shell's controlling terminal (echo off on the shell's own
//! streams) and the result must land in `$?` without a job-table detour —
//! the same reason `ulimit` lives in-process.

use alloc::format;
use alloc::string::String;

use crate::builtin::BuiltinContext;

/// Status returned when the builtin is used incorrectly or refused.
const USAGE_ERROR: i32 = 1;

/// Hard bound on an offered password's byte length: matches the login
/// prompt's own line budget, and far below the request bound the wire
/// format enforces (a fail-closed memory bound, not a policy).
const MAX_PASSWORD: usize = 256;

/// Run `elevate <user> <program>`.
///
/// The password buffer is stack-held and zeroed on **every** path out — it
/// carries a credential, and secret hygiene is the holder's job.
pub(crate) fn elevate(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    let [username, program] = args else {
        ctx.console
            .write_stderr("usage: elevate <user> <program>\n");
        return USAGE_ERROR;
    };
    if username.is_empty() || program.is_empty() {
        ctx.console
            .write_stderr("usage: elevate <user> <program>\n");
        return USAGE_ERROR;
    }

    ctx.console
        .write_stdout(&format!("Password for {username}: "));
    let mut secret = [0u8; MAX_PASSWORD];
    let result = match ctx.elevator.read_secret(&mut secret) {
        Ok(len) => match core::str::from_utf8(&secret[..len]) {
            // An empty password is offered as-is: whether it verifies is the
            // supervisor's decision, not a spelling the shell pre-judges.
            Ok(password) => ctx.elevator.elevate(username, password, program),
            // Refuse malformed input locally without ever sending it — the
            // same errno the wire decoder gives a bad encoding.
            Err(_) => Err(tairix_abi::Errno::LengthOutOfRange),
        },
        Err(err) => Err(err),
    };
    secret.fill(0);

    match result {
        Ok(exit_code) => exit_code,
        Err(err) => {
            ctx.console.write_stderr(&format!("elevate: {err}\n"));
            USAGE_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::host::Elevator;
    use crate::test_support::Fixture;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::Errno;

    /// An in-memory [`Elevator`]: scripted secret input, recorded calls,
    /// scripted outcome.
    struct ScriptedElevator {
        secret: &'static str,
        secret_result: Result<(), Errno>,
        outcome: Result<i32, Errno>,
        calls: RefCell<Vec<(String, String, String)>>,
    }

    impl ScriptedElevator {
        fn new(secret: &'static str, outcome: Result<i32, Errno>) -> Self {
            Self {
                secret,
                secret_result: Ok(()),
                outcome,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn failing_read(err: Errno) -> Self {
            Self {
                secret: "",
                secret_result: Err(err),
                outcome: Ok(0),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Elevator for ScriptedElevator {
        fn read_secret(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            self.secret_result?;
            let bytes = self.secret.as_bytes();
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }

        fn elevate(&self, username: &str, password: &str, program: &str) -> Result<i32, Errno> {
            self.calls.borrow_mut().push((
                username.to_string(),
                password.to_string(),
                program.to_string(),
            ));
            self.outcome
        }
    }

    #[test]
    fn elevate_prompts_posts_and_returns_the_exit_code() {
        let elevator = ScriptedElevator::new("hunter2", Ok(42));
        let mut fixture = Fixture::with_elevator(&elevator);
        let status = fixture.run(&["elevate", "root", "/Apps/Tool.app/Run"]);
        assert_eq!(status, Some(42));
        assert!(fixture.console.stdout().contains("Password for root: "));
        assert_eq!(
            elevator.calls.borrow().as_slice(),
            &[(
                "root".to_string(),
                "hunter2".to_string(),
                "/Apps/Tool.app/Run".to_string()
            )]
        );
    }

    #[test]
    fn a_refusal_is_reported_and_fails_the_builtin() {
        let elevator = ScriptedElevator::new("wrong", Err(Errno::PermissionDenied));
        let mut fixture = Fixture::with_elevator(&elevator);
        let status = fixture.run(&["elevate", "root", "/Apps/Tool.app/Run"]);
        assert_eq!(status, Some(1));
        assert!(fixture.console.stderr().contains("elevate:"));
    }

    #[test]
    fn a_failed_secret_read_never_posts() {
        let elevator = ScriptedElevator::failing_read(Errno::PermissionDenied);
        let mut fixture = Fixture::with_elevator(&elevator);
        let status = fixture.run(&["elevate", "root", "/Apps/Tool.app/Run"]);
        assert_eq!(status, Some(1));
        assert!(elevator.calls.borrow().is_empty());
    }

    #[test]
    fn usage_errors_fail_closed_without_prompting() {
        let elevator = ScriptedElevator::new("unused", Ok(0));
        let mut fixture = Fixture::with_elevator(&elevator);
        assert_eq!(fixture.run(&["elevate"]), Some(1));
        assert_eq!(fixture.run(&["elevate", "root"]), Some(1));
        assert_eq!(fixture.run(&["elevate", "", "/p"]), Some(1));
        assert_eq!(fixture.run(&["elevate", "root", ""]), Some(1));
        assert_eq!(fixture.run(&["elevate", "root", "/p", "extra"]), Some(1));
        assert!(elevator.calls.borrow().is_empty());
    }

    #[test]
    fn the_default_seam_fails_closed() {
        let mut fixture = Fixture::new();
        let status = fixture.run(&["elevate", "root", "/p"]);
        assert_eq!(status, Some(1));
        assert!(fixture.console.stderr().contains("elevate:"));
    }
}
