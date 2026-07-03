//! The per-invocation elevation broker (`plans/CAPABILITY_USE.md` CU5).
//!
//! While a session runs, login serves its console's elevation call endpoint
//! ([`rustos_abi::elevate`]): the session's shell forwards an
//! `elevate <user> <program>` request, and this module decides it —
//! re-authenticating the target account through the **same**
//! [`Authenticator`] the login prompt uses (timing-equalised, refusals
//! indistinguishable), then running the program as that account through the
//! injected [`ElevateLauncher`] and reporting its exit code. The requesting
//! shell's own identity and capability set are never touched: the elevated
//! child's set is derived kernel-side as
//! `its manifest ∩ the target account's ceiling`, exactly as at login.
//!
//! Like the [`Login`](crate::Login) state machine this is pure decision
//! logic over injected seams, so every branch is host-tested; the `Run`
//! binary owns the IPC serve loop (`call_recv` → this → `call_reply`) and
//! the syscall-backed launcher.

use rustos_abi::elevate::{ElevateReply, ElevateRequest};
use rustos_abi::Errno;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::decfmt::DecBuf;
use crate::events;
use crate::session::{Authenticator, Credentials};

/// Runs a re-authenticated elevation command: spawns `program` as `uid` on
/// login's own console and blocks until it exits.
///
/// The `Run` binary backs this with the `spawn` syscall's spawn-as-user
/// switch (login's `CAP_SPAWN_AS_USER`) plus `wait`; tests use in-memory
/// fixtures. Separate from [`crate::SessionLauncher`], which starts a
/// *session* from an [`crate::AuthenticatedUser`] record's shell of choice —
/// an elevation runs one explicit program and must never consult the
/// account's shell.
pub trait ElevateLauncher {
    /// Spawn `program` as `uid`, block until it exits, and return its exit
    /// code.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim when the program
    /// cannot be spawned or reaped (unknown path, spawn refused, …); the
    /// broker reports it to the requester and audits it.
    fn run_as(&self, program: &str, uid: u32) -> Result<i32, Errno>;
}

/// Decide one elevation request, returning the reply to post.
///
/// The checks run strictly in trust order, each failing closed:
///
/// 1. **Placement** — the caller's kernel-attested console (`peer_console`,
///    read from `call_peer_origin`, never claimed) must be login's own
///    console (`own_console`): a caller on another console — or on none —
///    is refused before its bytes are even parsed.
/// 2. **Shape** — the request must decode ([`ElevateRequest::decode`],
///    fail-closed).
/// 3. **Re-authentication** — the offered credentials must verify through
///    `authenticator`; a wrong password, an unknown account, and a locked
///    account are refused indistinguishably
///    ([`Errno::PermissionDenied`] — the cause is audited, never
///    disclosed).
/// 4. **Run** — the program is spawned as the target account and waited
///    for; a spawn refusal is reported verbatim.
///
/// Every grant and every refusal emits its audit event
/// ([`events::ELEVATE_GRANTED`] / [`events::ELEVATE_REFUSED`]). The caller
/// owns the request buffer and zeroises it (it carries the offered
/// password) as soon as this returns.
pub fn handle_elevate_request(
    bytes: &[u8],
    peer_console: u64,
    own_console: u64,
    authenticator: &dyn Authenticator,
    launcher: &dyn ElevateLauncher,
    sink: &dyn Sink,
) -> ElevateReply {
    if peer_console != own_console {
        audit_refused(sink, "foreign console", None, Errno::PermissionDenied);
        return ElevateReply::Refused(Errno::PermissionDenied);
    }
    let request = match ElevateRequest::decode(bytes) {
        Ok(request) => request,
        Err(err) => {
            audit_refused(sink, "malformed request", None, err);
            return ElevateReply::Refused(err);
        }
    };
    let credentials = Credentials {
        username: request.username,
        password: request.password,
    };
    let Ok(user) = authenticator.authenticate(&credentials) else {
        // The cause (wrong password / unknown / locked) is deliberately not
        // recorded beyond the offered username: refusals stay
        // indistinguishable even to an audit-log reader comparing entries.
        audit_refused(
            sink,
            "authentication failed",
            Some(request.username),
            Errno::PermissionDenied,
        );
        return ElevateReply::Refused(Errno::PermissionDenied);
    };
    match launcher.run_as(request.program, user.uid.0) {
        Ok(exit_code) => {
            audit_granted(sink, &request, user.uid.0, exit_code);
            ElevateReply::Completed { exit_code }
        }
        Err(err) => {
            audit_refused(sink, "launch failed", Some(request.username), err);
            ElevateReply::Refused(err)
        }
    }
}

fn emit(sink: &dyn Sink, level: Level, id: EventId, message: &str, fields: &[Field<'_>]) {
    log(
        sink,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}

fn audit_granted(sink: &dyn Sink, request: &ElevateRequest<'_>, uid: u32, exit_code: i32) {
    let mut uid_buf = DecBuf::new();
    let mut code_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::ELEVATE_GRANTED,
        "elevation granted",
        &[
            Field {
                key: "user",
                value: rustos_log::FieldValue::Str(request.username),
            },
            Field {
                key: "uid",
                value: rustos_log::FieldValue::Str(uid_buf.format(i128::from(uid))),
            },
            Field {
                key: "program",
                value: rustos_log::FieldValue::Str(request.program),
            },
            Field {
                key: "exit_code",
                value: rustos_log::FieldValue::Str(code_buf.format(i128::from(exit_code))),
            },
        ],
    );
}

fn audit_refused(sink: &dyn Sink, cause: &str, username: Option<&str>, err: Errno) {
    let mut errno_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::ELEVATE_REFUSED,
        "elevation refused",
        &[
            Field {
                key: "cause",
                value: rustos_log::FieldValue::Str(cause),
            },
            Field {
                key: "user",
                value: rustos_log::FieldValue::Str(username.unwrap_or("-")),
            },
            Field {
                key: "errno",
                value: rustos_log::FieldValue::Str(errno_buf.format(i128::from(err.as_i32()))),
            },
        ],
    );
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{handle_elevate_request, ElevateLauncher};
    use crate::events;
    use crate::session::{AuthenticatedUser, Authenticator, Credentials, Gid, Uid};
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::elevate::{ElevateReply, ElevateRequest, ELEVATE_MAX_REQUEST};
    use rustos_abi::Errno;
    use rustos_caps::CapabilitySet;
    use rustos_log::{Event, EventId, Sink};

    /// Authenticator accepting exactly `root`/`correct` as uid 0.
    struct FixedAuth;

    impl Authenticator for FixedAuth {
        fn authenticate(&self, credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
            // Unknown accounts, wrong passwords, and locked accounts are one
            // indistinguishable refusal, exactly as the production
            // `UsersAuthenticator` behaves.
            if credentials.username == "root" && credentials.password == "correct" {
                Ok(AuthenticatedUser {
                    uid: Uid(0),
                    primary_gid: Gid(0),
                    supplementary_gids: Vec::new(),
                    capabilities: CapabilitySet::empty(),
                    shell: "/System/Services/shell".to_string(),
                })
            } else {
                Err(Errno::PermissionDenied)
            }
        }
    }

    /// Launcher recording each run and returning a scripted outcome.
    struct MockLauncher {
        outcome: Result<i32, Errno>,
        runs: RefCell<Vec<(alloc::string::String, u32)>>,
    }

    impl MockLauncher {
        fn new(outcome: Result<i32, Errno>) -> Self {
            Self {
                outcome,
                runs: RefCell::new(Vec::new()),
            }
        }
    }

    impl ElevateLauncher for MockLauncher {
        fn run_as(&self, program: &str, uid: u32) -> Result<i32, Errno> {
            self.runs.borrow_mut().push((program.to_string(), uid));
            self.outcome
        }
    }

    /// Sink counting events by id.
    #[derive(Default)]
    struct CountSink {
        seen: RefCell<Vec<EventId>>,
    }

    impl CountSink {
        fn count(&self, id: EventId) -> usize {
            self.seen.borrow().iter().filter(|e| **e == id).count()
        }
    }

    impl Sink for CountSink {
        fn write_event(&self, event: &Event<'_>) {
            self.seen.borrow_mut().push(event.id);
        }
    }

    fn encoded(
        username: &str,
        password: &str,
        program: &str,
    ) -> ([u8; ELEVATE_MAX_REQUEST], usize) {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = ElevateRequest {
            username,
            password,
            program,
        }
        .encode(&mut buf)
        .expect("encodes");
        (buf, len)
    }

    #[test]
    fn correct_password_runs_the_program_as_the_account_and_audits() {
        let (buf, len) = encoded("root", "correct", "/System/Apps/users.app/Run");
        let launcher = MockLauncher::new(Ok(7));
        let sink = CountSink::default();
        let reply = handle_elevate_request(&buf[..len], 1, 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Completed { exit_code: 7 });
        assert_eq!(
            launcher.runs.borrow().as_slice(),
            &[("/System/Apps/users.app/Run".to_string(), 0)]
        );
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 1);
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 0);
    }

    #[test]
    fn wrong_password_and_unknown_account_are_refused_indistinguishably() {
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let (wrong, wrong_len) = encoded("root", "wrong", "/System/Apps/users.app/Run");
        let (unknown, unknown_len) = encoded("mallory", "correct", "/System/Apps/users.app/Run");
        let refused_wrong =
            handle_elevate_request(&wrong[..wrong_len], 1, 1, &FixedAuth, &launcher, &sink);
        let refused_unknown =
            handle_elevate_request(&unknown[..unknown_len], 1, 1, &FixedAuth, &launcher, &sink);
        // One reply for both causes: the requester learns nothing about
        // which part of the credentials failed.
        assert_eq!(
            refused_wrong,
            ElevateReply::Refused(Errno::PermissionDenied)
        );
        assert_eq!(refused_wrong, refused_unknown);
        assert!(launcher.runs.borrow().is_empty(), "nothing was spawned");
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 2);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 0);
    }

    #[test]
    fn a_caller_on_another_console_is_refused_before_parsing() {
        let (buf, len) = encoded("root", "correct", "/System/Apps/users.app/Run");
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply = handle_elevate_request(&buf[..len], 2, 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
    }

    #[test]
    fn a_malformed_request_is_refused_without_authentication() {
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply = handle_elevate_request(&[0xFF; 10], 1, 1, &FixedAuth, &launcher, &sink);
        assert!(matches!(reply, ElevateReply::Refused(_)));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
    }

    #[test]
    fn a_spawn_refusal_is_reported_verbatim_and_audited() {
        let (buf, len) = encoded("root", "correct", "/missing");
        let launcher = MockLauncher::new(Err(Errno::NotFound));
        let sink = CountSink::default();
        let reply = handle_elevate_request(&buf[..len], 1, 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::NotFound));
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 0);
    }
}
